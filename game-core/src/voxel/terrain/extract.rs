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

/// 按 LOD 选择块的树来源并预热进对应缓存（source 提供 interner / 生成器 / 容器）。
///
/// `lod == 0` 走 [WrappedBlockCache]（VoxVolume 的基础子块包装，逐位不变）；
/// `lod > 0` 走按 LOD 直接生成的 [GeneratedBlockCache]。
fn prime_block_tree(
    source: &mut BlockSource<'_>,
    wrapped: &mut WrappedBlockCache,
    generated: &mut GeneratedBlockCache,
    origin: ChunkKey,
    lod: Lod,
) {
    let block = MeshBlock::new(origin, lod);
    if lod.lod() == 0 {
        wrapped.get_or_wrap(&source.volume.chunks, source.interner.inner_mut(), block);
    } else {
        generated.get_or_generate(source.interner.inner_mut(), source.generator, block);
    }
}

/// 预热邻块树所需的世界数据：interner、生成器与体素容器。
struct BlockSource<'a> {
    interner: &'a mut VoxelInterner,
    generator: &'a WorldGenerator,
    volume: &'a VoxVolume,
}

/// 已预热的邻块树引用：LOD0 读 [WrappedBlockCache]，LOD>0 读 [GeneratedBlockCache]。
#[derive(Clone, Copy)]
struct NeighbourTrees<'a> {
    wrapped: &'a WrappedBlockCache,
    generated: &'a GeneratedBlockCache,
}

impl<'a> NeighbourTrees<'a> {
    /// 取某邻块的树；未预热 / 缺失返回 None（按空气处理）。
    fn tree(&self, origin: ChunkKey, lod: Lod) -> Option<&'a VoxTree<u8>> {
        if lod.lod() == 0 {
            self.wrapped.get(origin, lod)
        } else {
            self.generated.get(origin, lod)
        }
    }
}

/// 26 邻偏移（不含原点），顺序 = dx 外层、dy、dz 内层（与逐层 for 一致）。
fn neighbour_offsets() -> impl Iterator<Item = (i32, i32, i32)> {
    (-1i32..=1)
        .flat_map(|dx| (-1i32..=1).flat_map(move |dy| (-1i32..=1).map(move |dz| (dx, dy, dz))))
        .filter(|&(dx, dy, dz)| dx != 0 || dy != 0 || dz != 0)
}

/// 预热 26 邻：lod 为 0 走包装缓存，大于 0 走按 LOD 生成的缓存。
fn prime_neighbourhood(
    source: &mut BlockSource<'_>,
    mesh_block: MeshBlock,
    lod: Lod,
    span: i32,
    wrapped_neighbours: &mut WrappedBlockCache,
    generated_neighbours: &mut GeneratedBlockCache,
) {
    for (dx, dy, dz) in neighbour_offsets() {
        let neighbour = ChunkKey::new(
            mesh_block.origin.x + dx * span,
            mesh_block.origin.y + dy * span,
            mesh_block.origin.z + dz * span,
        );
        prime_block_tree(
            source,
            wrapped_neighbours,
            generated_neighbours,
            neighbour,
            lod,
        );
    }
}

/// 按 26 邻树采样 [Neighbourhood]：棱 / 角邻块须被面邻居报告支持才纳入。
fn sample_neighbourhood<'a>(
    trees: &'a NeighbourTrees<'a>,
    mesh_block: MeshBlock,
    lod: Lod,
    span: i32,
    plan: &[TerrainFaceNeighbor; 6],
) -> Neighbourhood<'a> {
    let mut out = Neighbourhood::none();
    for (dx, dy, dz) in neighbour_offsets() {
        // 棱 / 角邻块必须与面邻居报告一致：只要它所依赖的任一面邻居是 Missing
        //（落在 mesh 区域之外，面上按空气处理），就不能再从 margin 块里读到实心，
        // 否则外墙面的 AO 会在块边界处被误遮挡（块间一格宽的渐变暗线）。
        if !edge_corner_supported(plan, dx, dy, dz) {
            continue;
        }
        let neighbour = ChunkKey::new(
            mesh_block.origin.x + dx * span,
            mesh_block.origin.y + dy * span,
            mesh_block.origin.z + dz * span,
        );
        if let Some(tree) = trees.tree(neighbour, lod) {
            out.set(dx as i8, dy as i8, dz as i8, tree);
        }
    }
    out
}

/// 把面邻居报告解析成 mesh 层描述符（借用已预热的邻块树）。
fn resolve_mesh_faces<'a>(
    trees: &'a NeighbourTrees<'a>,
    plan: &[TerrainFaceNeighbor; 6],
    lod: Lod,
) -> [MeshFaceNeighbor<'a>; 6] {
    let mut out = [MeshFaceNeighbor::Missing; 6];
    for (face, plan_face) in plan.iter().enumerate() {
        out[face] = match plan_face {
            TerrainFaceNeighbor::Missing => MeshFaceNeighbor::Missing,
            TerrainFaceNeighbor::Same(origin) => match trees.tree(*origin, lod) {
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
}

/// 面邻居报告 + 裙边掩码：两者都只依赖分区与块自身，与块内容无关。
fn block_face_plan(
    block: &TerrainBlock,
    partition: &TerrainPartition,
) -> ([TerrainFaceNeighbor; 6], u8) {
    // 面邻居报告只保留同 LOD（跨 LOD 已映射成 Missing）：块只看同 LOD 邻居，
    // 缺失按空气处理 -> 每块自己封边，不会再有「两侧都剔除」的穿透洞。
    let plan = face_neighbors(block, partition);
    // 裙边掩码 = 跨 LOD 错位面 + 面外没有同 LOD 邻居的面（缺失按空气处理）。
    // 注意：自闭合后这些面的面外 halo 是空气，主 mesher 已画出边界格，裙边
    // 当前的覆盖位图会全部跳过 -> 本掩码不额外产生几何，保留为安全网。
    let skirt = lod_mismatch_mask(block, partition) | missing_face_mask(&plan);
    (plan, skirt)
}

/// 用已预热的本块树 + LOD 感知面邻居 + 26 邻树提取原始体素（32³ + 整圈 halo）。
fn extract_own_raw<'a>(
    source: &mut BlockSource<'_>,
    wrapped: &mut WrappedBlockCache,
    generated: &mut GeneratedBlockCache,
    lod_generated: bool,
    mesh_block: MeshBlock,
    mesh_faces: [MeshFaceNeighbor<'a>; 6],
    neighbourhood: Neighbourhood<'a>,
) -> Vec<u8> {
    let tree = if lod_generated {
        generated.get_or_generate(source.interner.inner_mut(), source.generator, mesh_block)
    } else {
        wrapped.get_or_wrap(
            &source.volume.chunks,
            source.interner.inner_mut(),
            mesh_block,
        )
    };
    let raw = extract_raw_halo_with_faces(
        tree,
        source.interner.inner(),
        mesh_block,
        mesh_faces,
        neighbourhood,
    );
    debug_assert_eq!(raw.len(), RAW_VOXELS, "extract_raw_halo 必须产出 34³");
    raw
}

/// 提取一个块的原始体素 halo（34³）与接缝裙边掩码；内部无实心体素返回 None。
///
/// 返回 (halo 字节, skirt_mask)：掩码 = 跨 LOD 错位面 | 面外没有同 LOD 邻居的面，
/// 0 = 无裙边。当前 extract 路径下掩码不额外产出裙边几何（见 append_skirts）。
///
/// 共享给 Startup 生成与运行期流式重划分，避免两条路径分叉。邻块缓存与本块
/// 包装缓存必须是两个资源（external 只读借用与 &mut 本块缓存冲突）。
///
/// 本体只做装配：block_face_plan -> prime_neighbourhood -> sample_neighbourhood /
/// resolve_mesh_faces -> extract_own_raw（圈复杂度 < 15，行为逐位不变）。
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
    let (plan, skirt) = block_face_plan(&block, partition);
    let lod = Lod::new(block.lod);
    let mesh_block = MeshBlock::new(block.origin, lod);
    let span = 1i32 << block.lod;
    // 树来源按 LOD 分流（godot_voxel 的做法）：
    // - lod == 0：直接从已生成的 VoxVolume 包装（逐位不变，physics / 近景用）；
    // - lod > 0：按自己的 LOD 直接生成（LOD 数据不折叠 LOD0）。
    let lod_generated = block.lod > 0;

    let mut source = BlockSource {
        interner,
        generator,
        volume,
    };
    prime_neighbourhood(
        &mut source,
        mesh_block,
        lod,
        span,
        &mut neighbors.0,
        &mut generated.neighbors,
    );
    let trees = NeighbourTrees {
        wrapped: &neighbors.0,
        generated: &generated.neighbors,
    };
    let neighbourhood = sample_neighbourhood(&trees, mesh_block, lod, span, &plan);
    let mesh_faces = resolve_mesh_faces(&trees, &plan, lod);

    let raw = extract_own_raw(
        &mut source,
        wrapped,
        &mut generated.own,
        lod_generated,
        mesh_block,
        mesh_faces,
        neighbourhood,
    );
    if raw_halo_has_solid_interior(&raw) {
        Some((raw, skirt))
    } else {
        None
    }
}
