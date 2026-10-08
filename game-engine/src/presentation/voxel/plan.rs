//! mesh 重建规划：块级脏描述与边界过滤的重建计划。
//!
//! 纯规划机制：读世界层的 [`DirtyChunk`]（「哪些体素变了」的世界记录），
//! 输出 [`MeshBlockDirty`]（「要重建哪些 mesh 块、各自 internal/faces」）。
//! 本模块不持有任何世界状态，也不注册插件 / 系统。
//!
//! 旧接口（ChunkKey 级脏集 + 无条件 6 邻 `rebuild_set`）被替换为
//! `VoxelBox`（块内整数体素范围）+ `MeshBlockDirty`（块级 internal/faces）
//! + `rebuild_plan`（band 相对**全局块边界**过滤）。
//!
//! 确定性：所有会决定顺序的容器都是 BTreeMap / 排序 Vec。

use std::collections::BTreeMap;

use voxel::mesh::MeshBlock;
use voxel::store::{ChunkKey, Lod};

use crate::voxel::{lod_block_origin, lod_block_span, DirtyChunk};

/// 一个 base 子块每轴的体素数（1 << CHUNK_DEPTH == 32）。
const CHUNK_VOXELS: i32 = 32;

/// 6 位面掩码。bit i 对应 neighbors6[i] == ExternalPlane::from_index(i)。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct FaceMask(pub u8);

impl FaceMask {
    /// 全部 6 面。
    pub const ALL: u8 = 0b11_1111;

    /// 位并。
    #[must_use]
    pub fn or(self, other: Self) -> Self {
        FaceMask(self.0 | other.0)
    }

    /// bit i 是否置位。
    #[must_use]
    pub fn has(self, i: usize) -> bool {
        (self.0 >> i) & 1 != 0
    }
}

/// 一个待重建的 mesh 块。
#[derive(Clone, Copy, Debug)]
pub struct MeshBlockDirty {
    /// 待重建的 mesh 块。
    pub block: MeshBlock,
    /// true：块内自身 occupancy 变了 → 内部掩码 + 全部 6 面都要重算。
    pub internal: bool,
    /// 仅外部面变了（internal == false 时有效）→ 只刷这些面的 external 掩码。
    pub faces: FaceMask,
}

/// 覆盖策略：把覆盖 key 的所有 LOD mesh 块写进 out。
pub trait CoverPolicy {
    /// 把覆盖 key 的所有 LOD mesh 块写进 out（可重复，调用方去重）。
    fn covering_blocks(&self, key: ChunkKey, out: &mut Vec<MeshBlock>);
}

impl<F> CoverPolicy for F
where
    F: Fn(ChunkKey, &mut Vec<MeshBlock>),
{
    fn covering_blocks(&self, key: ChunkKey, out: &mut Vec<MeshBlock>) {
        self(key, out);
    }
}

/// 便捷策略：给定 LOD 级别，返回每级向下取整后的 mesh 块。
#[must_use]
pub fn floor_to_lod_policy(levels: Vec<Lod>) -> impl CoverPolicy {
    move |key: ChunkKey, out: &mut Vec<MeshBlock>| {
        for &lod in &levels {
            out.push(MeshBlock::new(lod_block_origin(key, lod), lod));
        }
    }
}

/// neighbors6 / ExternalPlane::ALL 的顺序索引：
/// 0=+X, 1=-X, 2=+Y, 3=-Y, 4=+Z, 5=-Z。
#[inline]
fn unit(f: usize) -> (i32, i32, i32) {
    match f {
        0 => (1, 0, 0),
        1 => (-1, 0, 0),
        2 => (0, 1, 0),
        3 => (0, -1, 0),
        4 => (0, 0, 1),
        _ => (0, 0, -1),
    }
}

/// 对面的 bit 索引：0<->1, 2<->3, 4<->5。
#[inline]
fn opposite(f: usize) -> usize {
    f ^ 1
}

/// 同 LOD、沿 f 方向相邻的 mesh 块（原点 + unit(f) * span，span = 2^lod 子块）。
#[inline]
fn neighbor_block(b: MeshBlock, f: usize, span: i32) -> MeshBlock {
    let (dx, dy, dz) = unit(f);
    MeshBlock {
        origin: ChunkKey::new(
            b.origin.x + dx * span,
            b.origin.y + dy * span,
            b.origin.z + dz * span,
        ),
        lod: b.lod,
    }
}

/// 编辑 e 是否触及 mesh 块 b 的 f 面。
///
/// band 宽度 = 2^lod base 体素（= resolution 5 下最外层采样格），且必须相对
/// **全局块边界** bmin/bmax 比较，不是 chunk 局部的 0/31。
#[inline]
fn reaches_face(e: &DirtyChunk, b: MeshBlock, f: usize, band: i32, span: i32) -> bool {
    let ekey = [e.key.x, e.key.y, e.key.z];
    let borigin = [b.origin.x, b.origin.y, b.origin.z];
    let axis = f / 2;
    let gmin = ekey[axis] * CHUNK_VOXELS + e.edited.min[axis] as i32;
    let gmax = ekey[axis] * CHUNK_VOXELS + e.edited.max[axis] as i32;
    let bmin = borigin[axis] * CHUNK_VOXELS;
    let bmax = bmin + span * CHUNK_VOXELS;
    if f.is_multiple_of(2) {
        // + 方向：编辑上界越过块的近面 band。
        gmax > bmax - band
    } else {
        // - 方向：编辑下界越过块的近面 band。
        gmin < bmin + band
    }
}

/// 边界过滤重建计划（严格按 伪代码）：
/// * internal 块：自身 occupancy 变 → 内部 + 全部 6 面；
/// * 邻块：只有当编辑触及本块该面时，才把邻块对面标成 external-only；
/// * 输出从 BTreeMap 收集，按 (origin, lod) 升序。
#[must_use]
pub fn rebuild_plan(edits: &[DirtyChunk], cover: &dyn CoverPolicy) -> Vec<MeshBlockDirty> {
    let mut acc: BTreeMap<(ChunkKey, Lod), MeshBlockDirty> = BTreeMap::new();
    let mut owned: Vec<MeshBlock> = Vec::new();

    for e in edits {
        owned.clear();
        cover.covering_blocks(e.key, &mut owned);

        for &b in owned.iter() {
            // 先标记本块 internal（结束 entry 借用，之后才能再动 acc）。
            {
                let entry = acc.entry((b.origin, b.lod)).or_insert(MeshBlockDirty {
                    block: b,
                    internal: false,
                    faces: FaceMask(0),
                });
                entry.internal = true;
            }

            let span = lod_block_span(b.lod);
            let band = span; // 2^lod base 体素
            for f in 0..6usize {
                if !reaches_face(e, b, f, band, span) {
                    continue;
                }
                let nb = neighbor_block(b, f, span);
                let nentry = acc.entry((nb.origin, nb.lod)).or_insert(MeshBlockDirty {
                    block: nb,
                    internal: false,
                    faces: FaceMask(0),
                });
                // 内部块会重算全部面，external-only 面不需要再叠加。
                if !nentry.internal {
                    nentry.faces = nentry.faces.or(FaceMask(1u8 << opposite(f)));
                }
            }
        }
    }

    acc.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voxel::{chunk_key, neighbors6, VoxelBox, VoxelDirtySet};

    fn lod0() -> impl CoverPolicy {
        floor_to_lod_policy(vec![Lod::new(0)])
    }

    fn plan_ids(plan: &[MeshBlockDirty]) -> Vec<(ChunkKey, Lod)> {
        plan.iter().map(|p| (p.block.origin, p.block.lod)).collect()
    }

    fn find(plan: &[MeshBlockDirty], origin: ChunkKey, lod: Lod) -> Option<&MeshBlockDirty> {
        plan.iter()
            .find(|p| p.block.origin == origin && p.block.lod == lod)
    }

    /// 测试 1 case 1：内部编辑 [8,24)^3 → 只有 1 个 internal 块，0 个纯面块。
    #[test]
    fn plan_interior_edit_has_no_face_blocks() {
        let key = chunk_key(0, 0, 0);
        let edits = [DirtyChunk {
            key,
            edited: VoxelBox {
                min: [8, 8, 8],
                max: [24, 24, 24],
            },
        }];
        let plan = rebuild_plan(&edits, &lod0());
        assert_eq!(plan.len(), 1, "only the covered block, no neighbours");
        assert_eq!(plan_ids(&plan), vec![(key, Lod::new(0))]);
        assert!(plan[0].internal);
        assert_eq!(plan[0].faces, FaceMask(0));
    }

    /// 测试 1 case 2：贴 +X/+Y/+Z 角 → 1 internal + 3 个 faces，且面位是
    /// 对面的 X-/Y-/Z-（bit 1/3/5）。
    #[test]
    fn plan_corner_edit_marks_opposite_faces() {
        let key = chunk_key(0, 0, 0);
        let edits = [DirtyChunk {
            key,
            edited: VoxelBox {
                min: [31, 31, 31],
                max: [32, 32, 32],
            },
        }];
        let plan = rebuild_plan(&edits, &lod0());
        assert_eq!(plan.len(), 4, "1 internal + 3 external-only neighbours");
        let internal = find(&plan, key, Lod::new(0)).expect("internal block present");
        assert!(internal.internal);
        assert_eq!(internal.faces, FaceMask(0));

        let px = find(&plan, chunk_key(1, 0, 0), Lod::new(0)).expect("+X neighbour");
        assert!(!px.internal);
        assert_eq!(px.faces, FaceMask(1 << 1), "X- opposite bit");

        let py = find(&plan, chunk_key(0, 1, 0), Lod::new(0)).expect("+Y neighbour");
        assert!(!py.internal);
        assert_eq!(py.faces, FaceMask(1 << 3), "Y- opposite bit");

        let pz = find(&plan, chunk_key(0, 0, 1), Lod::new(0)).expect("+Z neighbour");
        assert!(!pz.internal);
        assert_eq!(pz.faces, FaceMask(1 << 5), "Z- opposite bit");
    }

    /// 测试 1 case 3：只贴 +X → 1 internal + 1 个 faces == YZNeg 位（bit 1）。
    #[test]
    fn plan_single_pos_x_edit_marks_x_neg_face() {
        let key = chunk_key(0, 0, 0);
        let edits = [DirtyChunk {
            key,
            edited: VoxelBox {
                min: [31, 8, 8],
                max: [32, 24, 24],
            },
        }];
        let plan = rebuild_plan(&edits, &lod0());
        assert_eq!(plan.len(), 2);
        assert!(find(&plan, key, Lod::new(0)).unwrap().internal);
        let nx = find(&plan, chunk_key(1, 0, 0), Lod::new(0)).expect("+X neighbour");
        assert!(!nx.internal);
        assert_eq!(nx.faces, FaceMask(1 << 1));
    }

    /// 测试 1 case 4：LOD1（band = 2 base 体素）。chunk (1,0,0) 的局部
    /// x∈[30,32) 是覆盖块 origin(0,0,0)（跨 chunk 0..1）的外层 2 体素，因此
    /// +X 邻块 origin(2,0,0) 应被标脏；y/z 取内部避免误触。
    #[test]
    fn plan_lod1_edit_reaches_block_face() {
        let key = chunk_key(1, 0, 0);
        let edits = [DirtyChunk {
            key,
            edited: VoxelBox {
                min: [30, 8, 8],
                max: [32, 24, 24],
            },
        }];
        let policy = floor_to_lod_policy(vec![Lod::new(1)]);
        let plan = rebuild_plan(&edits, &policy);
        assert_eq!(plan.len(), 2, "internal cover block + one neighbour");
        let cover = find(&plan, chunk_key(0, 0, 0), Lod::new(1)).expect("cover block");
        assert!(cover.internal);
        let nx = find(&plan, chunk_key(2, 0, 0), Lod::new(1)).expect("+X LOD1 neighbour");
        assert!(!nx.internal);
        assert_eq!(nx.faces, FaceMask(1 << 1));

        // band 相对全局块边界：同一编辑若落在覆盖块内层（chunk 0 的 x∈[30,32)），
        // 距离块面 64 还有 32 体素，不应标 +X 邻块。
        let inner = [DirtyChunk {
            key: chunk_key(0, 0, 0),
            edited: VoxelBox {
                min: [30, 8, 8],
                max: [32, 24, 24],
            },
        }];
        let plan_inner = rebuild_plan(&inner, &policy);
        assert_eq!(plan_inner.len(), 1);
        assert!(plan_inner[0].internal);
    }

    /// 输出按 (origin, lod) 升序，且同 key 重复标记取并集。
    #[test]
    fn plan_is_sorted_and_dirty_merges() {
        let mut dirty = VoxelDirtySet::new();
        dirty.mark_edited(
            chunk_key(1, 0, 0),
            VoxelBox {
                min: [0, 0, 0],
                max: [4, 4, 4],
            },
        );
        dirty.mark_dirty(chunk_key(1, 0, 0));
        dirty.mark_dirty(chunk_key(-2, 0, 0));
        assert_eq!(dirty.len(), 2);
        assert!(dirty.is_dirty(&chunk_key(1, 0, 0)));

        let peek = dirty.peek_edits();
        assert_eq!(peek[0].key, chunk_key(-2, 0, 0));
        assert_eq!(peek[1].key, chunk_key(1, 0, 0));
        assert_eq!(
            peek[1].edited,
            VoxelBox::FULL,
            "mark_dirty must union to FULL"
        );

        let plan = rebuild_plan(&dirty.take_edits(), &lod0());
        assert!(dirty.is_empty());
        let ids: Vec<(ChunkKey, Lod)> =
            plan.iter().map(|p| (p.block.origin, p.block.lod)).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(ids, sorted, "plan must be sorted by (origin, lod)");
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "strictly ascending");
    }

    #[test]
    fn face_mask_helpers() {
        assert!(FaceMask(1 << 2).has(2));
        assert!(!FaceMask(1 << 2).has(3));
        assert_eq!(FaceMask(1).or(FaceMask(4)), FaceMask(5));
        assert_eq!(FaceMask::ALL, 0b11_1111);
    }

    #[test]
    fn neighbor_block_steps_by_span() {
        let b = MeshBlock::new(chunk_key(4, 4, 4), Lod::new(2));
        assert_eq!(neighbor_block(b, 0, 4).origin, chunk_key(8, 4, 4));
        assert_eq!(neighbor_block(b, 1, 4).origin, chunk_key(0, 4, 4));
        assert_eq!(neighbor_block(b, 4, 4).origin, chunk_key(4, 4, 8));
        for f in 0..6 {
            assert_eq!(opposite(opposite(f)), f);
        }
    }

    #[test]
    fn old_neighbours_still_exported() {
        // 保证 neighbors6 顺序与 FaceMask 位序一致。
        let k = chunk_key(0, 0, 0);
        let ns = neighbors6(k);
        assert_eq!(ns[0], chunk_key(1, 0, 0));
        assert_eq!(ns[1], chunk_key(-1, 0, 0));
        assert_eq!(ns[2], chunk_key(0, 1, 0));
        assert_eq!(ns[3], chunk_key(0, -1, 0));
        assert_eq!(ns[4], chunk_key(0, 0, 1));
        assert_eq!(ns[5], chunk_key(0, 0, -1));
    }
}
