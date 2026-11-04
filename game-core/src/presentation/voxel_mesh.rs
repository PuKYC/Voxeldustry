//! 体素网格的 GPU 几何契约（体素表现唯一通道 = 原始体素 halo）。
//!
//! ## 分层
//!
//! - 世界层 `game-engine::voxel` 提供 `VoxVolume`；表现层机制
//!   `game_engine::presentation::voxel::extract_raw_halo` 产出 34³ halo 缓冲，
//!   Godot 侧再调 gdext mesher（`game_core::presentation::mesh_raw_halo`）现算
//!   8 B/矩形的 39 bit u64 流（低 39 bit 是描述符，bit [39,47) 是 4 角 AO，
//!   见 [ao_corner_index]）；
//! - 本模块把「一个 32³ mesh 块的原始体素缓冲」定义成可由表现管线承载的组件
//!   [VoxChunkRaw]，并给出 rect -> 四边形角点 / 法线的**整数几何契约**；
//! - Godot 侧的 voxel_rect.gdshader 用完全相同的位布局与轴映射逐顶点展开，
//!   本模块的 rect_corner / rect_normal 就是那份着色器数学的 Rust 镜像，
//!   由单测锁死（没有 Godot 可跑时，这是唯一能自动验证的几何真相源）。
//!
//! ## 轴映射（与 crates/voxel 的 RectInstance 语义逐条对应）
//!
//!   plane 0 (YZ): rows=y, cols=z -> 角点 = (slice, row + cy*h, col + cx*w)
//!   plane 1 (XZ): rows=z, cols=x -> 角点 = (col + cx*w, slice, row + cy*h)
//!   plane 2 (XY): rows=y, cols=x -> 角点 = (col + cx*w, row + cy*h, slice)
//!
//! slice **已经是面坐标**（DIR_POS -> layer + 1，DIR_NEG -> layer）；
//! dir = 0 是 DIR_POS（+轴法线），dir = 1 是 DIR_NEG（-轴法线）。
//!
//! 矩形是位置无关的：块原点（米）由节点的 Transform 载荷给出，LOD 缩放
//! voxel_size * 2^lod 由渲染侧按 RawVoxelPayload.lod 计算。

use std::collections::{BTreeMap, BTreeSet};

use bevy::prelude::*;
use game_engine::body_key::BodyKey;
use game_engine::identity::StableEntityId;
use game_engine::presentation::voxel::{extract_raw_halo, raw_halo_has_solid_interior};
use game_engine::voxel::{
    ChunkKey, DirtyChunk, Lod, MeshBlock, Neighbourhood, RectInstance, VoxVolume, VoxelBox,
    VoxelDirtySet, VoxelInterner,
};

use crate::static_data::voxel::FACE_COUNT;
use crate::voxel::terrain::blocks::group_key;

/// 一个 mesh 块（32³ 基础跨度）的原始体素缓冲（内部 32³ + halo 层，共 34³）。
///
/// 值 = 方块 id（`static_data::voxel::BLOCK_TABLE` 的 id），0 = 空气。
/// 布局与 `game_engine::presentation::voxel::extract_raw_halo` 一致。
///
/// 这是体素表现的**唯一**通道（RawVoxels 载荷）：Godot 侧调 gdext mesher
/// 现算矩形流后交给 VoxelMeshNode 渲染。
#[derive(Component, Clone, Debug, PartialEq, Eq, Default)]
pub struct VoxChunkRaw {
    /// mesh 块的 LOD 级别（0..=3）。
    pub lod: u8,
    /// `extract_raw_halo` 产出的 34³ 方块 id 缓冲。
    pub blocks: Vec<u8>,
    /// 接缝裙边掩码（0 = 无裙边）。
    ///
    /// bit `face` 对应 ExternalPlane 顺序 `[+x, -x, +y, -y, +z, -z]`，
    /// 取 `lod_mismatch_mask(...) | missing_face_mask(&face_neighbors(...))`：
    /// 跨 LOD 错位面与面外没有同 LOD 邻居的面都置位。当前 extract 路径下这些
    /// 面的面外 halo 是空气，主 mesher 已覆盖边界格，mask 不额外产生几何。
    pub skirt_mask: u8,
}
/// 一组同一 LOD 的 mesh 块（每块 32³ 基础跨度 + halo，共 34³）。
///
/// `chunks` 用 `BTreeMap<ChunkKey, (Vec<u8>, u8)>`：键遍历 = ChunkKey 升序，
/// 保证组载荷 `put_body` 逐字节确定。元组第一项是 34³ 方块 id 缓冲，布局与
/// [VoxChunkRaw::blocks] 相同（`blocks.len() == RAW_VOXELS`，值 = 方块 id，
/// 0 = 空气）；第二项是该块的跨 LOD 接缝裙边掩码（6 bit，ExternalPlane 顺序
/// `[+x, -x, +y, -y, +z, -z]`，0 = 无裙边）。
///
/// 这是 RAWVOXELGROUPS 载荷的源组件：一个实体承载一个 LOD 分组（组内块同 LOD，
/// 键 = `terrain::GroupKey`），Godot 侧物化一个节点、组内每块一个子
/// VoxelMeshNode。**必须逐块携带 mask**：否则切到分组通道会静默关闭裙边。
#[derive(Component, Clone, Debug, PartialEq, Eq, Default)]
pub struct VoxChunkGroupRaw {
    /// 组内所有块的 LOD 级别（0..=3）。
    pub lod: u8,
    /// 块原点 -> (34³ 方块 id 缓冲, 该块跨 LOD 裙边掩码)。
    pub chunks: BTreeMap<ChunkKey, (Vec<u8>, u8)>,
}

/// 分组实体所属的 body（流式地形分组用；Eager 地形分组没有这个组件）。
///
/// 流式层是单块单组，分组实体上没有 [VoxVolume]（容器在 body 根实体上），
/// 所以由这个组件把「这一组属于哪个 body」带回表现层刷新系统。Eager 地形的
/// 分组实体不带它——那里的分组 id 是组自己的稳定 id，不是 body，不能拿它
/// 当 body 用。
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderGroupBody(pub BodyKey);

/// 一个矩形在块局部体素坐标下的 4 个角点。
///
/// 角点编号 = cx | (cy << 1)（0..4），与顶点 UV (cx, cy) 一一对应：
/// 0=(0,0) 1=(1,0) 2=(0,1) 3=(1,1)。具体顶点顺序无所谓（四边形相同），
/// 但 Rust 单测与着色器必须用同一套编号。
pub fn rect_corner(rect: RectInstance, corner: usize) -> [i32; 3] {
    let cx = (corner & 1) as i32;
    let cy = ((corner >> 1) & 1) as i32;
    let cols = i32::from(rect.col) + cx * i32::from(rect.w);
    let rows = i32::from(rect.row) + cy * i32::from(rect.h);
    let slice = i32::from(rect.slice);
    match rect.plane {
        0 => [slice, rows, cols], // YZ: rows=y, cols=z
        1 => [cols, slice, rows], // XZ: rows=z, cols=x
        _ => [cols, rows, slice], // XY: rows=y, cols=x
    }
}

/// 面法线（整数，单位轴）。dir = 0 是 DIR_POS（+轴），dir = 1 是 DIR_NEG。
pub fn rect_normal(rect: RectInstance) -> [i32; 3] {
    let sign = if rect.dir == 0 { 1 } else { -1 };
    match rect.plane {
        0 => [sign, 0, 0],
        1 => [0, sign, 0],
        _ => [0, 0, sign],
    }
}

/// AO 角点编号（0..4）与 [rect_corner] 的 UV 一一对应：i = cx | (cy << 1)。
///
/// cx 是 UV.x（沿 col / w 方向），cy 是 UV.y（沿 row / h 方向）。这与
/// voxel::mesh::compute_face_ao 的返回顺序、以及 voxel_rect.gdshader 里
/// int(cx) | (int(cy) << 1) 完全一致（由 Rust 单测锁死，见本模块 tests）。
pub const fn ao_corner_index(cx: u32, cy: u32) -> u32 {
    (cx & 1) | ((cy & 1) << 1)
}

/// 该 LOD 下每个矩形格子单位对应的米数 = voxel_size_m * 2^lod。
pub fn rect_scale_meters(voxel_size_m: f32, lod: u8) -> f32 {
    voxel_size_m * (1u32 << u32::from(lod)) as f32
}

/// 把世界层脏块刷新成新的 RawVoxelGroups 载荷（表现侧唯一通道）。
///
/// 只在 Update 运行，读取 FixedUpdate 里 `apply_voxel_changes` 留下的
/// [`VoxelDirtySet`]，在 [`CollectPresentationSet`] 之前改写
/// [`VoxChunkGroupRaw`]，因此同一渲染帧就能看到新几何。
///
/// 刷新范围：
/// - 编辑块本身；
/// - 当编辑范围贴到 chunk 边界时，halo 受影响的 6 面 / 12 棱 / 8 角邻块
///   （最多 26 个），否则接缝 AO 会保留旧值。
///
/// LOD 限制：只刷新 `lod == 0` 的分组。粗 LOD 块由 terrain 的
/// `GeneratedBlockCache` 按 LOD 直接生成，不读 `VoxVolume` 的编辑；玩家在
/// 触及距离内始终处于 LOD0，因此本闭环不做粗块回写（见 docs 的 LOD 风险）。
pub struct VoxelMeshRefreshPlugin;

impl Plugin for VoxelMeshRefreshPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            refresh_dirty_voxel_meshes.before(game_engine::presentation::CollectPresentationSet),
        );
    }
}

/// 待刷新的分组实体：原始块 + 可选的体积 / 稳定 id / body 归属。
type DirtyVoxelGroupQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut VoxChunkGroupRaw,
        Option<&'static VoxVolume>,
        Option<&'static StableEntityId>,
        Option<&'static RenderGroupBody>,
    ),
>;

/// 消费脏块并增量重建 `VoxChunkGroupRaw` 内受影响的块。
///
/// ## 三种分组形状
///
/// 生产里体素容器**不在**分组实体上（Eager 地形把容器放在 body 根实体上，
/// 流式层也是单块单组），所以先解析「这一组属于哪个 body、用哪个 [VoxVolume]」，
/// 按以下优先级：
///
/// 1. **合并形状**：分组实体自己带 [VoxVolume]（旧单测、`dev/gameplay_test.rs`）。
///    直接用它的容器；body 取该实体自己的 [StableEntityId]（没有就当无 body）。
/// 2. **流式形状**：分组实体带 [RenderGroupBody]。body 来自该组件，容器在 body
///    根实体上——从「[VoxVolume] + [StableEntityId]」实体表里按 body 查，缺失
///    就跳过。
/// 3. **Eager 生产形状**：分组实体既没有容器也没有 owner（组上的
///    [StableEntityId] 是组自己的 id，不是 body），只能反查包含关系：把组内
///    **受影响的**块键拿去问每个 body 的容器 `contains_chunk`，恰好命中一个
///    body 才采用；0 个跳过；多个视为歧义，跳过并 `debug!`——不猜。
///
/// ## 刷新范围与门控
///
/// - 有 body 的编辑只送给该 body 的分组（`by_body_affected[body]`）；
/// - bodyless 编辑（`mark_edited` 旧调用）按**兼容语义**对任何分组生效；
/// - 流式分组只刷新 `chunks` 里已经有的键：它是单块单组、组原点就是该块，
///   把邻块补进来会在错误位置重复渲染。Eager / 合并形状保留旧行为（新变实心
///   的邻块补进组，否则空块里放方块不可见）；
/// - 只处理 `lod == 0`；仍用 `group_key(anchor, 0)` 做 region 门控
///   （anchor = `chunks` 升序首个键），避免把组外的邻块塞进别的分组。
pub fn refresh_dirty_voxel_meshes(
    mut dirty: ResMut<VoxelDirtySet>,
    interner: Res<VoxelInterner>,
    volumes: Query<(&VoxVolume, &StableEntityId), Without<VoxChunkGroupRaw>>,
    mut groups: DirtyVoxelGroupQuery,
) {
    let edits = dirty.take_edits();
    if edits.is_empty() {
        return;
    }

    // bodyless 桶与按 body 分桶：同一个 ChunkKey 在不同 body 下是不同内容，
    // 绝不能合并成一份受影响集合。
    let mut bodyless: Vec<DirtyChunk> = Vec::new();
    let mut by_body: BTreeMap<BodyKey, Vec<DirtyChunk>> = BTreeMap::new();
    for edit in edits {
        match edit.body {
            Some(body) => by_body.entry(body).or_default().push(edit),
            None => bodyless.push(edit),
        }
    }

    let bodyless_affected = affected_chunk_keys(&bodyless);
    let by_body_affected: BTreeMap<BodyKey, BTreeSet<ChunkKey>> = by_body
        .iter()
        .map(|(body, edits)| (*body, affected_chunk_keys(edits)))
        .collect();

    // body 根实体表：同时带容器与稳定 id 的实体（Eager 地形根、流式 body 根）。
    // 同名 body 重复时保留先出现者，只警告不覆盖。
    let mut volumes_by_body: BTreeMap<BodyKey, &VoxVolume> = BTreeMap::new();
    for (volume, id) in &volumes {
        let body = BodyKey::from(*id);
        if volumes_by_body.contains_key(&body) {
            warn!("duplicate VoxVolume owner for body {body:?}; keeping the first");
            continue;
        }
        volumes_by_body.insert(body, volume);
    }

    for (mut raw, own_volume, own_id, owner) in &mut groups {
        // 粗 LOD 的 raw 不是从 VoxVolume 折叠出来的，不能在这里改。
        if raw.lod != 0 {
            continue;
        }
        let Some(anchor) = raw.chunks.keys().next().copied() else {
            continue;
        };
        let region = group_key(anchor, 0);

        let resolved: Option<(&VoxVolume, Option<BodyKey>)> = if let Some(volume) = own_volume {
            // 合并形状：容器就在分组实体上。
            Some((volume, own_id.map(|id| BodyKey::from(*id))))
        } else if let Some(owner) = owner {
            // 流式形状：owner 给出 body，容器在 body 根实体上。
            volumes_by_body
                .get(&owner.0)
                .map(|volume| (*volume, Some(owner.0)))
        } else {
            // Eager 生产形状：按「组内受影响的块键」反查唯一包含它的 body。
            let mut candidates: BTreeSet<BodyKey> = BTreeSet::new();
            for key in raw.chunks.keys() {
                let affected = bodyless_affected.contains(key)
                    || by_body_affected.values().any(|set| set.contains(key));
                if !affected {
                    continue;
                }
                for (body, volume) in &volumes_by_body {
                    if volume.contains_chunk(key) {
                        candidates.insert(*body);
                    }
                }
            }
            let mut it = candidates.iter();
            match (it.next().copied(), it.next()) {
                (Some(body), None) => volumes_by_body
                    .get(&body)
                    .map(|volume| (*volume, Some(body))),
                (None, _) => None,
                (Some(_), Some(_)) => {
                    debug!(
                        "voxel mesh refresh skipped ambiguous group: anchor={anchor:?} candidates={candidates:?}"
                    );
                    None
                }
            }
        };
        let Some((volume, body)) = resolved else {
            continue;
        };

        // 流式分组只更新组内已有的键；Eager / 合并形状可以补进新变实心的邻块。
        let streaming = owner.is_some();
        let mut keys = bodyless_affected.clone();
        if let Some(body) = body {
            if let Some(set) = by_body_affected.get(&body) {
                keys.extend(set.iter().copied());
            }
        }
        for key in &keys {
            if group_key(*key, 0) != region {
                continue;
            }
            if streaming && !raw.chunks.contains_key(key) {
                continue;
            }
            refresh_group_chunk(volume, interner.inner(), &mut raw, *key);
        }
    }
}

/// 编辑块 + 受 halo 影响的邻块（确定性升序）。
fn affected_chunk_keys(edits: &[DirtyChunk]) -> BTreeSet<ChunkKey> {
    let axis = game_engine::voxel::chunk_voxels_per_axis() as u8;
    let mut keys = BTreeSet::new();
    for edit in edits {
        keys.insert(edit.key);
        for dx in -1i32..=1 {
            for dy in -1i32..=1 {
                for dz in -1i32..=1 {
                    if dx == 0 && dy == 0 && dz == 0 {
                        continue;
                    }
                    if !side_touched(edit.edited, dx, 0, axis)
                        || !side_touched(edit.edited, dy, 1, axis)
                        || !side_touched(edit.edited, dz, 2, axis)
                    {
                        continue;
                    }
                    keys.insert(ChunkKey::new(
                        edit.key.x + dx,
                        edit.key.y + dy,
                        edit.key.z + dz,
                    ));
                }
            }
        }
    }
    keys
}

fn side_touched(edited: VoxelBox, delta: i32, axis: usize, chunk_axis: u8) -> bool {
    match delta {
        -1 => edited.min[axis] == 0,
        1 => edited.max[axis] >= chunk_axis,
        _ => true,
    }
}

fn refresh_group_chunk(
    volume: &VoxVolume,
    interner: &game_engine::voxel::VoxInterner<u8>,
    raw: &mut VoxChunkGroupRaw,
    key: ChunkKey,
) {
    let Some(tree) = volume.get_chunk(&key) else {
        return;
    };
    let mut neighbours = Neighbourhood::none();
    for dx in -1i8..=1 {
        for dy in -1i8..=1 {
            for dz in -1i8..=1 {
                if dx == 0 && dy == 0 && dz == 0 {
                    continue;
                }
                let neighbour = ChunkKey::new(
                    key.x + i32::from(dx),
                    key.y + i32::from(dy),
                    key.z + i32::from(dz),
                );
                if let Some(tree) = volume.get_chunk(&neighbour) {
                    neighbours.set(dx, dy, dz, tree);
                }
            }
        }
    }

    let halo = extract_raw_halo(tree, interner, MeshBlock::new(key, Lod::new(0)), neighbours);
    let solid = raw_halo_has_solid_interior(&halo);
    match raw.chunks.get_mut(&key) {
        // 保留原有 skirt_mask：它由 LOD 分区决定，与方块内容无关。
        Some(entry) => entry.0 = halo,
        // 新变成实心的块（例如在空 chunk 里放方块）必须补进组，否则不可见。
        None if solid => {
            raw.chunks.insert(key, (halo, 0));
        }
        None => {}
    }
}

/// Godot 侧 `VoxelMaterialLibrary.build_face_material_map()` 产出的映射表长度：
/// 256 个 block × 6 个面（face = plane*2+dir，顺序 `[+x, -x, +y, -y, +z, -z]`）。
pub const FACE_MATERIAL_MAP_LEN: usize = 256 * FACE_COUNT;

/// 渲染入口：34³ raw halo -> 39 bit 矩形流，并按 Godot 提供的 block→面材质
/// 映射重映射每个矩形的 `material` 字段。
///
/// `face_material_map` 由 Godot 的 `VoxelMaterialLibrary.build_face_material_map()`
/// 生成，索引 `block * FACE_COUNT + face`，值就是矩形要写的材质 id。Rust 侧
/// 不持有任何材质表，只做机械重映射。
///
/// **降级路径**：长度不等于 [`FACE_MATERIAL_MAP_LEN`]（例如空 map 表示 Godot
/// 未提供绑定时）退化为 identity —— 矩形 material 保持 block id，不 panic。
#[must_use]
pub fn mesh_raw_halo_for_render(
    lod: u8,
    blocks: &[u8],
    skirt_mask: u8,
    face_material_map: &[u8],
) -> Vec<u64> {
    if face_material_map.len() != FACE_MATERIAL_MAP_LEN {
        return game_engine::presentation::voxel::mesh_raw_halo_with_skirts(
            lod, blocks, skirt_mask,
        );
    }

    game_engine::presentation::voxel::mesh_raw_halo_with_skirts_and_face_materials(
        lod,
        blocks,
        skirt_mask,
        &|block, face| {
            // 正常 mesher 的 face 一定在 0..5；这里防御性夹取，避免 map 越界。
            let face = usize::from(face).min(FACE_COUNT - 1);
            face_material_map[usize::from(block) * FACE_COUNT + face]
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::static_data::voxel::{DEFAULT_VOXEL_SIZE, GRASS_BLOCK};
    use game_engine::presentation::voxel::{
        pack_rect, pack_rect_with_ao, raw_halo_index, unpack_ao, unpack_rect, RAW_VOXELS,
    };
    use game_engine::voxel::{set_voxel_for, MaxDepth, VoxOpsWrite, VoxTree, CHUNK_DEPTH};

    /// 按 `RectInstance` 的字段顺序打包：(plane, dir, slice, row, col, w, h, material)。
    fn rect(v: [u8; 8]) -> RectInstance {
        let [plane, dir, slice, row, col, w, h, material] = v;
        RectInstance {
            plane,
            dir,
            slice,
            row,
            col,
            w,
            h,
            material,
        }
    }

    #[test]
    fn word_roundtrips_through_engine_layout() {
        let cases = [
            rect([0, 0, 0, 0, 0, 4, 3, 7]),
            rect([1, 1, 17, 3, 5, 7, 9, 42]),
            rect([2, 0, 32, 31, 31, 32, 32, 255]),
        ];
        for r in cases {
            assert_eq!(unpack_rect(pack_rect(&r)), r);
        }
    }

    #[test]
    fn yz_face_corners_span_row_and_col() {
        // slice=5 的 +X 面，row=2 h=3（y），col=4 w=6（z）。
        let r = rect([0, 0, 5, 2, 4, 6, 3, 1]);
        let c: Vec<[i32; 3]> = (0..4).map(|i| rect_corner(r, i)).collect();
        assert_eq!(c, vec![[5, 2, 4], [5, 2, 10], [5, 5, 4], [5, 5, 10]]);
        assert_eq!(rect_normal(r), [1, 0, 0]);
    }

    #[test]
    fn xz_face_corners_span_col_and_row() {
        // plane XZ: rows=z(h), cols=x(w)，slice 是 y。
        let r = rect([1, 1, 9, 1, 3, 5, 2, 2]);
        let c: Vec<[i32; 3]> = (0..4).map(|i| rect_corner(r, i)).collect();
        assert_eq!(c, vec![[3, 9, 1], [8, 9, 1], [3, 9, 3], [8, 9, 3]]);
        assert_eq!(rect_normal(r), [0, -1, 0]);
    }

    #[test]
    fn xy_face_corners_span_col_and_row() {
        // plane XY: rows=y(h), cols=x(w)，slice 是 z。
        let r = rect([2, 0, 4, 6, 2, 3, 5, 3]);
        let c: Vec<[i32; 3]> = (0..4).map(|i| rect_corner(r, i)).collect();
        assert_eq!(c, vec![[2, 6, 4], [5, 6, 4], [2, 11, 4], [5, 11, 4]]);
        assert_eq!(rect_normal(r), [0, 0, 1]);
    }

    #[test]
    fn all_six_orientations_have_unit_axis_normals() {
        for plane in 0..3u8 {
            for dir in 0..2u8 {
                let r = rect([plane, dir, 1, 1, 1, 1, 1, 0]);
                let n = rect_normal(r);
                assert_eq!(n.iter().map(|x| x.abs()).sum::<i32>(), 1);
                assert_eq!(n[usize::from(plane)], if dir == 0 { 1 } else { -1 });
                let mut cs: Vec<[i32; 3]> = (0..4).map(|i| rect_corner(r, i)).collect();
                cs.sort_unstable();
                cs.dedup();
                assert_eq!(cs.len(), 4);
            }
        }
    }

    #[test]
    fn lod_scale_doubles_per_level() {
        let vs = DEFAULT_VOXEL_SIZE.to_num::<f32>();
        assert!((rect_scale_meters(vs, 0) - vs).abs() < 1e-6);
        assert!((rect_scale_meters(vs, 1) - vs * 2.0).abs() < 1e-6);
        assert!((rect_scale_meters(vs, 3) - vs * 8.0).abs() < 1e-6);
        assert!((rect_scale_meters(0.45, 0) - 0.45).abs() < 1e-6);
    }

    #[test]
    fn ao_corner_index_matches_rect_corner_uv() {
        // 着色器用 UV (cx, cy) 取 AO：i = cx | (cy << 1)，与 rect_corner 同序。
        for r in [
            rect([0, 0, 5, 2, 4, 6, 3, 1]),
            rect([1, 1, 9, 1, 3, 5, 2, 2]),
            rect([2, 0, 4, 6, 2, 3, 5, 3]),
        ] {
            for cy in 0..2u32 {
                for cx in 0..2u32 {
                    let i = ao_corner_index(cx, cy) as usize;
                    // 手工展开的 UV 角点（与 shader 的 cols/rows 公式一致）。
                    let cols = i32::from(r.col) + cx as i32 * i32::from(r.w);
                    let rows = i32::from(r.row) + cy as i32 * i32::from(r.h);
                    let slice = i32::from(r.slice);
                    let uv_local = match r.plane {
                        0 => [slice, rows, cols],
                        1 => [cols, slice, rows],
                        _ => [cols, rows, slice],
                    };
                    assert_eq!(
                        rect_corner(r, i),
                        uv_local,
                        "UV(cx={cx},cy={cy}) 角点不一致"
                    );
                }
            }
        }
        let mut idx: Vec<u32> = (0..2)
            .flat_map(|cy| (0..2).map(move |cx| ao_corner_index(cx, cy)))
            .collect();
        idx.sort_unstable();
        assert_eq!(idx, vec![0, 1, 2, 3]);
    }

    #[test]
    fn ao_bits_do_not_disturb_the_39_bit_rect() {
        let r = rect([2, 1, 4, 6, 2, 3, 5, 200]);
        let ao = [3u8, 2, 1, 0];
        let word = pack_rect_with_ao(&r, &ao);
        assert_eq!(
            unpack_rect(word),
            unpack_rect(pack_rect(&r)),
            "AO 不得改变矩形"
        );
        assert_eq!(unpack_ao(word), ao);
        // 消费端按 UV 角取 AO：每个角只读自己的 2 bit。
        assert_eq!(unpack_ao(word)[ao_corner_index(0, 0) as usize], 3);
        assert_eq!(unpack_ao(word)[ao_corner_index(1, 1) as usize], 0);
    }

    /// 渲染入口必须按 Godot 提供的 face material map 逐面重映射 material 字段。
    #[test]
    fn render_mesher_applies_face_material_map() {
        use crate::static_data::voxel::{GRASS_BLOCK, STONE_BLOCK};

        // 合成 map：默认 identity（每面 = block id）；草方块六面映射成 10..15。
        let mut map = vec![0u8; FACE_MATERIAL_MAP_LEN];
        for block in 0..=255usize {
            for face in 0..FACE_COUNT {
                map[block * FACE_COUNT + face] = block as u8;
            }
        }
        for face in 0..FACE_COUNT {
            map[usize::from(GRASS_BLOCK) * FACE_COUNT + face] = 10 + face as u8;
        }

        let mut blocks = vec![0u8; RAW_VOXELS];
        // halo 内部坐标 (1,1,1) = 块局部 (0,0,0)。
        blocks[raw_halo_index(1, 1, 1)] = GRASS_BLOCK;
        let words = mesh_raw_halo_for_render(0, &blocks, 0, &map);
        assert_eq!(words.len(), 6, "单个体素 = 6 个面");

        let mut seen = [false; 6];
        for word in &words {
            let r = unpack_rect(*word);
            let face = usize::from(r.plane) * 2 + usize::from(r.dir);
            assert!(!seen[face], "每个朝向只应有一个 1x1 面");
            seen[face] = true;
            assert_eq!(
                r.material,
                10 + face as u8,
                "face {face} 必须取 map[block*6+face]"
            );
        }
        assert!(seen.iter().all(|hit| *hit));

        // 非草方块：map 为 identity，material 保持 block id。
        let mut stone = vec![0u8; RAW_VOXELS];
        stone[raw_halo_index(1, 1, 1)] = STONE_BLOCK;
        let words = mesh_raw_halo_for_render(0, &stone, 0, &map);
        assert!(words
            .iter()
            .all(|w| unpack_rect(*w).material == STONE_BLOCK));

        // 空气块不得产生几何。
        assert!(mesh_raw_halo_for_render(0, &vec![0u8; RAW_VOXELS], 0, &map).is_empty());
    }

    /// 未提供绑定（空 map / 长度错误）时 identity 降级：material 保持 block id，
    /// 几何不变且不 panic。
    #[test]
    fn render_mesher_identity_without_face_material_map() {
        use crate::static_data::voxel::{GRASS_BLOCK, STONE_BLOCK};

        let mut blocks = vec![0u8; RAW_VOXELS];
        blocks[raw_halo_index(1, 1, 1)] = GRASS_BLOCK;

        for map in [
            Vec::new(),
            vec![0u8; 10],
            vec![0u8; FACE_MATERIAL_MAP_LEN * 2],
        ] {
            let words = mesh_raw_halo_for_render(0, &blocks, 0, &map);
            assert_eq!(words.len(), 6, "identity 降级不改变几何");
            assert!(
                words
                    .iter()
                    .all(|w| unpack_rect(*w).material == GRASS_BLOCK),
                "identity 降级时 material 必须保持 block id"
            );
        }

        let mut stone = vec![0u8; RAW_VOXELS];
        stone[raw_halo_index(1, 1, 1)] = STONE_BLOCK;
        assert!(mesh_raw_halo_for_render(0, &stone, 0, &[])
            .iter()
            .all(|w| unpack_rect(*w).material == STONE_BLOCK));
    }

    fn tree_with_voxels(interner: &mut VoxelInterner, voxels: &[(IVec3, u8)]) -> VoxTree<u8> {
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
        for (point, value) in voxels {
            let _ = tree.set(interner.inner_mut(), *point, *value);
        }
        tree
    }

    fn raw_for(interner: &VoxelInterner, tree: &VoxTree<u8>, key: ChunkKey) -> Vec<u8> {
        extract_raw_halo(
            tree,
            interner.inner(),
            MeshBlock::new(key, Lod::new(0)),
            Neighbourhood::none(),
        )
    }

    fn edit_volume(
        app: &mut App,
        entity: Entity,
        dirty: &mut VoxelDirtySet,
        body: Option<BodyKey>,
        voxel: [i32; 3],
        value: u8,
    ) {
        app.world_mut()
            .resource_scope::<VoxelInterner, _>(|world, mut interner| {
                let mut entity_mut = world.entity_mut(entity);
                let mut volume = entity_mut.get_mut::<VoxVolume>().expect("volume");
                assert_eq!(
                    set_voxel_for(&mut volume, &mut interner, dirty, body, voxel, value),
                    Ok(true)
                );
            });
        // resource_scope 结束后把脏集放回 App，供 Update 系统消费。
        app.world_mut().insert_resource(std::mem::take(dirty));
    }

    #[test]
    fn dirty_edit_refreshes_group_raw_halo() {
        let key = ChunkKey::new(0, 0, 0);
        let mut interner = VoxelInterner::new(4 * 1024 * 1024);
        let tree = tree_with_voxels(&mut interner, &[(IVec3::new(5, 0, 0), GRASS_BLOCK)]);
        let raw = raw_for(&interner, &tree, key);
        let index = raw_halo_index(6, 1, 1);
        assert_eq!(raw[index], GRASS_BLOCK);

        let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
        volume.insert_chunk(key, tree);
        let mut chunks = BTreeMap::new();
        chunks.insert(key, (raw, 0));

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(interner);
        app.init_resource::<VoxelDirtySet>();
        app.add_plugins(VoxelMeshRefreshPlugin);
        let entity = app
            .world_mut()
            .spawn((volume, VoxChunkGroupRaw { lod: 0, chunks }))
            .id();

        let mut dirty = VoxelDirtySet::new();
        edit_volume(&mut app, entity, &mut dirty, None, [5, 0, 0], 0);
        app.update();

        let raw = app
            .world()
            .entity(entity)
            .get::<VoxChunkGroupRaw>()
            .expect("group");
        assert_eq!(
            raw.chunks[&key].0[index], 0,
            "breaking a voxel must clear its halo byte in the same update"
        );
    }

    #[test]
    fn dirty_edit_adds_a_newly_solid_chunk_to_the_group() {
        let key0 = ChunkKey::new(0, 0, 0);
        let key1 = ChunkKey::new(1, 0, 0);
        let mut interner = VoxelInterner::new(4 * 1024 * 1024);
        let tree0 = tree_with_voxels(&mut interner, &[(IVec3::new(5, 0, 0), GRASS_BLOCK)]);
        let raw0 = raw_for(&interner, &tree0, key0);
        let tree1 = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));

        let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
        volume.insert_chunk(key0, tree0);
        volume.insert_chunk(key1, tree1);
        let mut chunks = BTreeMap::new();
        chunks.insert(key0, (raw0, 0));

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(interner);
        app.init_resource::<VoxelDirtySet>();
        app.add_plugins(VoxelMeshRefreshPlugin);
        let entity = app
            .world_mut()
            .spawn((volume, VoxChunkGroupRaw { lod: 0, chunks }))
            .id();

        let mut dirty = VoxelDirtySet::new();
        // chunk 1 的局部 (0,0,0) = body 体素 (32,0,0)。
        edit_volume(&mut app, entity, &mut dirty, None, [32, 0, 0], GRASS_BLOCK);
        app.update();

        let raw = app
            .world()
            .entity(entity)
            .get::<VoxChunkGroupRaw>()
            .expect("group");
        let (halo, _) = raw
            .chunks
            .get(&key1)
            .expect("newly solid chunk must be added");
        assert_eq!(halo[raw_halo_index(1, 1, 1)], GRASS_BLOCK);
    }

    #[test]
    fn halo_affected_keys_expand_only_on_touched_boundaries() {
        let key = ChunkKey::new(0, 0, 0);
        let center = DirtyChunk {
            body: None,
            key,
            edited: VoxelBox {
                min: [1, 1, 1],
                max: [2, 2, 2],
            },
        };
        assert_eq!(
            affected_chunk_keys(&[center]),
            BTreeSet::from([key]),
            "a center edit must not dirty neighbours"
        );

        let corner = DirtyChunk {
            body: None,
            key,
            edited: VoxelBox {
                min: [0, 0, 0],
                max: [1, 1, 1],
            },
        };
        let keys = affected_chunk_keys(&[corner]);
        assert_eq!(keys.len(), 8, "corner edit reaches the 8 chunks around it");
        assert!(keys.contains(&ChunkKey::new(-1, -1, -1)));
        assert!(keys.contains(&ChunkKey::new(0, 0, 0)));

        let full = DirtyChunk {
            body: None,
            key,
            edited: VoxelBox::FULL,
        };
        assert_eq!(
            affected_chunk_keys(&[full]).len(),
            27,
            "a whole-chunk edit touches all 26 halo neighbours"
        );
    }

    /// 生产 Eager 形状：body 根实体带容器，分组实体只有 raw（既无 volume 也
    /// 无 owner）。刷新只能靠「组内受影响块 ∈ body 容器」反查出唯一 body。
    #[test]
    fn refresh_uses_separate_body_volume() {
        let key = ChunkKey::new(0, 0, 0);
        let mut interner = VoxelInterner::new(4 * 1024 * 1024);
        let tree = tree_with_voxels(&mut interner, &[(IVec3::new(5, 0, 0), GRASS_BLOCK)]);
        let raw = raw_for(&interner, &tree, key);
        let index = raw_halo_index(6, 1, 1);
        assert_eq!(raw[index], GRASS_BLOCK);

        let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
        volume.insert_chunk(key, tree);
        let mut chunks = BTreeMap::new();
        chunks.insert(key, (raw, 0));

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(interner);
        app.init_resource::<VoxelDirtySet>();
        app.add_plugins(VoxelMeshRefreshPlugin);

        let body_id = StableEntityId(41);
        let body = app.world_mut().spawn((volume, body_id)).id();
        // 分组实体上的 StableEntityId 是组自己的 id，不是 body。
        let group = app
            .world_mut()
            .spawn((StableEntityId(77), VoxChunkGroupRaw { lod: 0, chunks }))
            .id();

        let mut dirty = VoxelDirtySet::new();
        edit_volume(
            &mut app,
            body,
            &mut dirty,
            Some(BodyKey::new(body_id)),
            [5, 0, 0],
            0,
        );
        app.update();

        let raw = app
            .world()
            .entity(group)
            .get::<VoxChunkGroupRaw>()
            .expect("group");
        assert_eq!(
            raw.chunks[&key].0[index], 0,
            "分组没有容器 / owner 时必须通过包含关系找到 body 的 volume"
        );
    }

    /// 两个 body 共用同一个 ChunkKey：编辑必须按 body 精确路由，不能串台。
    #[test]
    fn refresh_matches_edits_by_body() {
        let key = ChunkKey::new(0, 0, 0);
        let index = raw_halo_index(6, 1, 1);
        let mut interner = VoxelInterner::new(8 * 1024 * 1024);
        let tree_a = tree_with_voxels(&mut interner, &[(IVec3::new(5, 0, 0), GRASS_BLOCK)]);
        let tree_b = tree_with_voxels(&mut interner, &[(IVec3::new(5, 0, 0), GRASS_BLOCK)]);
        let raw_a = raw_for(&interner, &tree_a, key);
        let raw_b = raw_for(&interner, &tree_b, key);
        assert_eq!(raw_a[index], GRASS_BLOCK);
        assert_eq!(raw_b[index], GRASS_BLOCK);

        let mut volume_a = VoxVolume::new(DEFAULT_VOXEL_SIZE);
        volume_a.insert_chunk(key, tree_a);
        let mut volume_b = VoxVolume::new(DEFAULT_VOXEL_SIZE);
        volume_b.insert_chunk(key, tree_b);

        let id_a = StableEntityId(101);
        let id_b = StableEntityId(102);

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(interner);
        app.init_resource::<VoxelDirtySet>();
        app.add_plugins(VoxelMeshRefreshPlugin);

        let body_a = app.world_mut().spawn((volume_a, id_a)).id();
        app.world_mut().spawn((volume_b, id_b));

        let group_a = app
            .world_mut()
            .spawn((
                StableEntityId(201),
                VoxChunkGroupRaw {
                    lod: 0,
                    chunks: BTreeMap::from([(key, (raw_a, 0))]),
                },
                RenderGroupBody(BodyKey::new(id_a)),
            ))
            .id();
        let group_b = app
            .world_mut()
            .spawn((
                StableEntityId(202),
                VoxChunkGroupRaw {
                    lod: 0,
                    chunks: BTreeMap::from([(key, (raw_b.clone(), 0))]),
                },
                RenderGroupBody(BodyKey::new(id_b)),
            ))
            .id();

        let mut dirty = VoxelDirtySet::new();
        edit_volume(
            &mut app,
            body_a,
            &mut dirty,
            Some(BodyKey::new(id_a)),
            [5, 0, 0],
            0,
        );
        app.update();

        let group_a_raw = app
            .world()
            .entity(group_a)
            .get::<VoxChunkGroupRaw>()
            .expect("group a");
        let group_b_raw = app
            .world()
            .entity(group_b)
            .get::<VoxChunkGroupRaw>()
            .expect("group b");
        assert_eq!(
            group_a_raw.chunks[&key].0[index], 0,
            "A 的编辑必须刷新 A 的分组"
        );
        assert_eq!(group_b_raw.chunks[&key].0, raw_b, "B 的分组必须逐字节不变");
    }

    /// bodyless 编辑（旧 `mark_edited`）是兼容语义：合并形状与流式 owner 形状
    /// 都要刷新。
    #[test]
    fn refresh_bodyless_edit_reaches_all_shapes() {
        let key = ChunkKey::new(0, 0, 0);
        let index = raw_halo_index(6, 1, 1);
        let mut interner = VoxelInterner::new(8 * 1024 * 1024);
        let merged_tree = tree_with_voxels(&mut interner, &[(IVec3::new(5, 0, 0), GRASS_BLOCK)]);
        let merged_raw = raw_for(&interner, &merged_tree, key);
        let owner_tree = tree_with_voxels(&mut interner, &[(IVec3::new(5, 0, 0), GRASS_BLOCK)]);
        // owner 分组先给一份全空气载荷：刷新后必须变成实心。
        let stale = vec![0u8; RAW_VOXELS];

        let mut merged_volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
        merged_volume.insert_chunk(key, merged_tree);
        let mut owner_volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
        owner_volume.insert_chunk(key, owner_tree);

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(interner);
        app.init_resource::<VoxelDirtySet>();
        app.add_plugins(VoxelMeshRefreshPlugin);

        let merged = app
            .world_mut()
            .spawn((
                merged_volume,
                VoxChunkGroupRaw {
                    lod: 0,
                    chunks: BTreeMap::from([(key, (merged_raw, 0))]),
                },
            ))
            .id();
        let body_id = StableEntityId(301);
        app.world_mut().spawn((owner_volume, body_id));
        let owner_group = app
            .world_mut()
            .spawn((
                StableEntityId(302),
                VoxChunkGroupRaw {
                    lod: 0,
                    chunks: BTreeMap::from([(key, (stale, 0))]),
                },
                RenderGroupBody(BodyKey::new(body_id)),
            ))
            .id();

        let mut dirty = VoxelDirtySet::new();
        // bodyless：只改合并形状的容器，脏标记不带 body。
        edit_volume(&mut app, merged, &mut dirty, None, [5, 0, 0], 0);
        app.update();

        let merged_raw = app
            .world()
            .entity(merged)
            .get::<VoxChunkGroupRaw>()
            .expect("merged group");
        assert_eq!(
            merged_raw.chunks[&key].0[index], 0,
            "合并形状必须被 bodyless 编辑刷新"
        );
        let owner_raw = app
            .world()
            .entity(owner_group)
            .get::<VoxChunkGroupRaw>()
            .expect("owner group");
        assert_eq!(
            owner_raw.chunks[&key].0[index], GRASS_BLOCK,
            "bodyless 编辑按兼容语义对 owner 形状同样生效"
        );
    }

    /// 流式分组是单块单组、组原点就是该块：即使 halo 受影响集合含邻块，也只能
    /// 刷新组内已有的键，否则邻块会在错误位置重复渲染。
    #[test]
    fn streaming_group_keeps_only_its_own_chunk() {
        let key = ChunkKey::new(1, 0, 0);
        let neighbour = ChunkKey::new(0, 0, 0);
        let index = raw_halo_index(1, 1, 1);
        let mut interner = VoxelInterner::new(8 * 1024 * 1024);
        let tree = tree_with_voxels(&mut interner, &[(IVec3::new(0, 0, 0), GRASS_BLOCK)]);
        let raw = raw_for(&interner, &tree, key);
        assert_eq!(raw[index], GRASS_BLOCK);
        // 邻块必须实心，否则「不吸收邻块」的断言会因空块本就进不了组而假通过。
        let neighbour_tree =
            tree_with_voxels(&mut interner, &[(IVec3::new(31, 31, 31), GRASS_BLOCK)]);

        let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
        volume.insert_chunk(key, tree);
        volume.insert_chunk(neighbour, neighbour_tree);

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(interner);
        app.init_resource::<VoxelDirtySet>();
        app.add_plugins(VoxelMeshRefreshPlugin);

        let body_id = StableEntityId(401);
        let body = app.world_mut().spawn((volume, body_id)).id();
        let group = app
            .world_mut()
            .spawn((
                StableEntityId(402),
                VoxChunkGroupRaw {
                    lod: 0,
                    chunks: BTreeMap::from([(key, (raw, 0))]),
                },
                RenderGroupBody(BodyKey::new(body_id)),
            ))
            .id();

        let mut dirty = VoxelDirtySet::new();
        // chunk (1,0,0) 的局部 (0,0,0) = body 体素 (32,0,0)：贴 -x 边界，
        // 受影响集合会含同 region 的邻块 (0,0,0)。
        edit_volume(
            &mut app,
            body,
            &mut dirty,
            Some(BodyKey::new(body_id)),
            [32, 0, 0],
            0,
        );
        app.update();

        let raw = app
            .world()
            .entity(group)
            .get::<VoxChunkGroupRaw>()
            .expect("group");
        assert_eq!(raw.chunks.len(), 1, "流式单块单组不得吸收邻块");
        assert!(!raw.chunks.contains_key(&neighbour));
        assert_eq!(
            raw.chunks[&key].0[index], 0,
            "组内该块的 halo 必须已就地刷新"
        );
    }

    /// 分组既无容器也无 owner，而两个 body 都包含该键：歧义不猜，原样跳过。
    #[test]
    fn refresh_skips_ambiguous_body() {
        let key = ChunkKey::new(0, 0, 0);
        let mut interner = VoxelInterner::new(8 * 1024 * 1024);
        let tree_a = tree_with_voxels(&mut interner, &[(IVec3::new(5, 0, 0), GRASS_BLOCK)]);
        let tree_b = tree_with_voxels(&mut interner, &[(IVec3::new(7, 0, 0), GRASS_BLOCK)]);
        let stale = vec![0u8; RAW_VOXELS];

        let mut volume_a = VoxVolume::new(DEFAULT_VOXEL_SIZE);
        volume_a.insert_chunk(key, tree_a);
        let mut volume_b = VoxVolume::new(DEFAULT_VOXEL_SIZE);
        volume_b.insert_chunk(key, tree_b);

        let id_a = StableEntityId(501);
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(interner);
        app.init_resource::<VoxelDirtySet>();
        app.add_plugins(VoxelMeshRefreshPlugin);

        let body_a = app.world_mut().spawn((volume_a, id_a)).id();
        app.world_mut().spawn((volume_b, StableEntityId(502)));
        let group = app
            .world_mut()
            .spawn((
                StableEntityId(503),
                VoxChunkGroupRaw {
                    lod: 0,
                    chunks: BTreeMap::from([(key, (stale.clone(), 0))]),
                },
            ))
            .id();

        let mut dirty = VoxelDirtySet::new();
        edit_volume(
            &mut app,
            body_a,
            &mut dirty,
            Some(BodyKey::new(id_a)),
            [9, 0, 0],
            GRASS_BLOCK,
        );
        app.update();

        let raw = app
            .world()
            .entity(group)
            .get::<VoxChunkGroupRaw>()
            .expect("group");
        assert_eq!(
            raw.chunks[&key].0, stale,
            "歧义分组必须原样跳过（不猜、不 panic）"
        );
    }
}
