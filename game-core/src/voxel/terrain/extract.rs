//! 单块的原始体素 halo 提取。
//!
//! 流程：按 LOD 预热 26 邻 -> 汇总 [Neighbourhood] -> 把面邻居报告翻译成 mesh 层的
//! [MeshFaceNeighbor] -> 用本块树提取 34³ 原始体素 + 裙边掩码。

use game_engine::presentation::voxel::{
    extract_raw_halo_with_faces, raw_halo_has_solid_interior, FaceNeighbor as MeshFaceNeighbor,
    WrappedBlockCache, RAW_VOXELS,
};
use game_engine::voxel::{
    ChunkKey, Lod, MeshBlock, Neighbourhood, VoxTree, VoxVolume, VoxelInterner,
};

use crate::voxel::terrain::generation::WorldGenerator;

use super::blocks::{
    face_neighbors, lod_mismatch_mask, missing_face_mask, FaceNeighbor as TerrainFaceNeighbor,
    TerrainBlock, TerrainPartition,
};
use super::generated::GeneratedBlockCache;
use super::resources::TerrainCaches;

/// 棱 / 角邻块（符号三元组里至少两个非零）是否被面邻居报告支持：
/// 它涉及的每个轴向面邻居都不是 `Missing` 才可用于 halo 的 AO 采样。
/// 面邻居（`dx/dy/dz` 中恰好一个非零）不受影响。
/// `plan` 顺序 = `[+x, -x, +y, -y, +z, -z]`。
fn edge_corner_supported(plan: &[TerrainFaceNeighbor; 6], dx: i32, dy: i32, dz: i32) -> bool {
    let non_zero = i32::from(dx != 0) + i32::from(dy != 0) + i32::from(dz != 0);
    if non_zero < 2 {
        return true;
    }
    let face_present = |index: usize| !matches!(plan[index], TerrainFaceNeighbor::Missing);
    let x_ok = dx == 0 || face_present(if dx > 0 { 0 } else { 1 });
    let y_ok = dy == 0 || face_present(if dy > 0 { 2 } else { 3 });
    let z_ok = dz == 0 || face_present(if dz > 0 { 4 } else { 5 });
    x_ok && y_ok && z_ok
}

/// 按 LOD 选择块的树来源并预热进对应缓存。
///
/// `lod == 0` 走 [WrappedBlockCache]（VoxVolume 的基础子块包装，逐位不变）；
/// `lod > 0` 走按 LOD 直接生成的 [GeneratedBlockCache]。
fn prime_block_tree(
    interner: &mut VoxelInterner,
    generator: &WorldGenerator,
    volume: &VoxVolume,
    wrapped: &mut WrappedBlockCache,
    generated: &mut GeneratedBlockCache,
    origin: ChunkKey,
    lod: Lod,
) {
    let block = MeshBlock::new(origin, lod);
    if lod.lod() == 0 {
        wrapped.get_or_wrap(&volume.chunks, interner.inner_mut(), block);
    } else {
        generated.get_or_generate(interner.inner_mut(), generator, block);
    }
}

/// 提取一个块的原始体素 halo（34³）与接缝裙边掩码；内部无实心体素返回 None。
///
/// 返回 (halo 字节, skirt_mask)：掩码 = 跨 LOD 错位面 | 面外没有同 LOD 邻居的面，
/// 0 = 无裙边。当前 extract 路径下掩码不额外产出裙边几何（见 `append_skirts`）。
///
/// 共享给 Startup 生成与运行期流式重划分，避免两条路径分叉。邻块缓存与本块
/// 包装缓存必须是两个资源（external 只读借用与 &mut 本块缓存冲突）。
pub(crate) fn extract_block_raw(
    block: TerrainBlock,
    volume: &VoxVolume,
    generator: &WorldGenerator,
    partition: &TerrainPartition,
    caches: &mut TerrainCaches<'_>,
) -> Option<(Vec<u8>, u8)> {
    let TerrainCaches {
        interner,
        wrapped,
        generated,
        neighbors,
    } = caches;
    let lod = Lod::new(block.lod);
    let mesh_block = MeshBlock::new(block.origin, lod);
    let span = 1i32 << block.lod;
    // 面邻居报告只保留同 LOD（跨 LOD 已映射成 Missing）：块只看同 LOD 邻居，
    // 缺失按空气处理 -> 每块自己封边，不会再有「两侧都剔除」的穿透洞。
    let plan = face_neighbors(&block, partition);
    // 裙边掩码 = 跨 LOD 错位面 + 面外没有同 LOD 邻居的面（缺失按空气处理）。
    // 注意：自闭合后这些面的面外 halo 是空气，主 mesher 已画出边界格，裙边
    // 当前的覆盖位图会全部跳过 -> 本掩码不额外产生几何，保留为安全网。
    let skirt = lod_mismatch_mask(&block, partition) | missing_face_mask(&plan);
    // 树来源按 LOD 分流（godot_voxel 的做法）：
    // - lod == 0：直接从已生成的 VoxVolume 包装（逐位不变，physics / 近景用）；
    // - lod > 0：按自己的 LOD 直接生成（WorldGenerator::generate_block_at_lod）。
    //   LOD 数据不折叠 LOD0，所以粗格的地表材质不会被投成地下材质、也不会下沉。
    let lod_generated = block.lod > 0;

    // 预热同 LOD 的 26 邻（6 面 + 12 棱 + 8 角），随后只读取回。对角邻块让
    // halo 的棱 / 角也能被填充，从而给出真正的 3D AO；缺失 = 空气。
    for dx in -1i32..=1 {
        for dy in -1i32..=1 {
            for dz in -1i32..=1 {
                if dx == 0 && dy == 0 && dz == 0 {
                    continue;
                }
                let neighbor = ChunkKey::new(
                    mesh_block.origin.x + dx * span,
                    mesh_block.origin.y + dy * span,
                    mesh_block.origin.z + dz * span,
                );
                prime_block_tree(
                    interner,
                    generator,
                    volume,
                    &mut neighbors.0,
                    &mut generated.neighbors,
                    neighbor,
                    lod,
                );
            }
        }
    }

    // 预热跨 LOD 面邻居（粗 / 细各按自己的 lod 选来源；同 LOD 面上面已预热）。
    // 面邻居全是同 LOD（或 Missing），26 邻预热已覆盖；跨 LOD 不再预热、不再采样。
    // 26 邻的树来源与块本身一致：LOD0 走包装缓存，LOD>0 走按 LOD 生成的缓存。
    let neighborhood_tree = |origin: ChunkKey, at_lod: Lod| -> Option<&VoxTree<u8>> {
        if at_lod.lod() == 0 {
            neighbors.0.get(origin, at_lod)
        } else {
            generated.neighbors.get(origin, at_lod)
        }
    };

    let neighbourhood = {
        let mut out = Neighbourhood::none();
        for dx in -1i8..=1 {
            for dy in -1i8..=1 {
                for dz in -1i8..=1 {
                    if dx == 0 && dy == 0 && dz == 0 {
                        continue;
                    }
                    let neighbor = ChunkKey::new(
                        mesh_block.origin.x + i32::from(dx) * span,
                        mesh_block.origin.y + i32::from(dy) * span,
                        mesh_block.origin.z + i32::from(dz) * span,
                    );
                    // 棱 / 角邻块必须与面邻居报告一致：只要它所依赖的任一面邻居
                    // 是 Missing（落在 mesh 区域之外，面上按空气处理），就不能再从
                    // margin 块里读到实心，否则外墙面的 AO 会在块边界处被误遮挡
                    // （表现为块间一格宽的渐变暗线）。
                    if !edge_corner_supported(&plan, i32::from(dx), i32::from(dy), i32::from(dz)) {
                        continue;
                    }
                    if let Some(tree) = neighborhood_tree(neighbor, lod) {
                        out.set(dx, dy, dz, tree);
                    }
                }
            }
        }
        out
    };

    // 把面邻居报告解析成 voxel 层描述符（借用上面已预热的树）。
    let mesh_faces = {
        let mut out = [MeshFaceNeighbor::Missing; 6];
        for (face, plan_face) in plan.iter().enumerate() {
            out[face] = match plan_face {
                TerrainFaceNeighbor::Missing => MeshFaceNeighbor::Missing,
                TerrainFaceNeighbor::Same(origin) => match neighborhood_tree(*origin, lod) {
                    Some(tree) => MeshFaceNeighbor::Same(tree),
                    None => MeshFaceNeighbor::Missing,
                },
                // 跨 LOD 面不会出现在 face_neighbors 里（已映射成 Missing）；
                // 兜底成 Missing，避免枚举扩张时静默漏面。
                TerrainFaceNeighbor::Coarser { .. } | TerrainFaceNeighbor::Finer(_) => {
                    MeshFaceNeighbor::Missing
                }
            };
        }
        out
    };

    // 用已 wrap 的块树 + LOD 感知面邻居 + 26 邻树提取原始体素
    //（内部 32³ + 整圈 halo）。
    let tree = if lod_generated {
        generated
            .own
            .get_or_generate(interner.inner_mut(), generator, mesh_block)
    } else {
        wrapped.get_or_wrap(&volume.chunks, interner.inner_mut(), mesh_block)
    };
    let raw = extract_raw_halo_with_faces(
        tree,
        interner.inner(),
        mesh_block,
        mesh_faces,
        neighbourhood,
    );
    debug_assert_eq!(raw.len(), RAW_VOXELS, "extract_raw_halo 必须产出 34³");
    if raw_halo_has_solid_interior(&raw) {
        Some((raw, skirt))
    } else {
        None
    }
}
