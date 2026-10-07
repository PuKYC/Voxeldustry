//! 体素地形接入（路径 B）：Startup 生成一圈地形块，产出带
//! [VoxelMeshBlock] 的 mesh 块实体。
//!
//! 体素容器 [VoxVolume] **只作为组件**挂在地形根实体上
//! （「一岛 / 一船 / 一结构 = 一个 VoxVolume 组件」），不作为 Resource；
//! 本模块不持有任何全局体素单例。
//!
//! ## 数据流
//!
//! 1. `Startup`：按 [TerrainConfig] 的半径 / max_lod 用 `WorldGenerator` 生成
//!    基础子块（x/z 多生成一圈 margin，让边缘块的 external 邻块有数据）；
//! 2. 逐块走引擎生产管线：`WrappedBlockCache::get_or_wrap` +
//!    `mesh_block_incremental`（内部 = `build_tree_occupancy` + 切片级增量
//!    greedy），再 pack_rect_batch_with_ao 得到 39 bit 矩形流 + 4 角 AO；
//! 3. 每个块一个 stable id 实体：`VoxelMeshBlock{lod, words}` +
//!    `Transform` / `PresentedTransform`（块原点，米）+ `Size(None)`；
//! 4. 体素容器 `VoxVolume` 本地构建、生成完一次性 spawn 成**组件**。
//!
//! ## 边界
//!
//! - 块原点（米）= `origin * 32 * voxel_size`，**不乘 2^lod**（见
//!   [blocks::block_origin_meters]）；
//! - 矩形流是位置无关的整数描述符，LOD 缩放由渲染侧按 payload 的 lod 处理；
//! - 本模块不依赖 godot，也不改 crates/voxel 或 game-engine。

pub mod blocks;

use std::collections::BTreeMap;

use bevy::prelude::*;
use game_engine::identity::{StableEntityIndex, StableIdAllocator};
use game_engine::presentation::RenderTransform;
use game_engine::spatial::Size;
use game_engine::voxel::{
    mesh_block_incremental, pack_rect_batch_with_ao, ChunkKey, ExternalMaskCache, FaceMask,
    IncrementalMeshCache, Lod, MeshBlock, MeshBlockDirty, VoxTree, VoxVolume, VoxelInterner,
    WrappedBlockCache,
};

use crate::presentation::voxel_mesh::VoxelMeshBlock;
use crate::presentation::PresentedTransform;
use crate::static_data::voxel::{BiomeId, DEFAULT_BIOME, DEFAULT_VOXEL_SIZE};
use crate::world::generation::{WorldGenerator, WorldSeed};

pub use blocks::{
    block_origin_meters, block_span, chunk_keys_to_generate, lod_blocks, neighbor_origins,
    y_band_layers, TerrainBlock, TERRAIN_MARGIN_CHUNKS,
};

/// 地形生成参数（世界层资源）。装配方在 `add_plugins(WorldPlugin)` **之前**
/// `insert_resource` 后，[WorldPlugin] 就会注册地形生成系统；不插入则不生成。
///
/// 这里**没有**"地形插件"：地形不是独立子系统。体素本身只以组件存在——
/// `VoxVolume`（容器）+ `VoxelMeshBlock`（每块的 39 bit 矩形流）；本结构只是
/// 生成参数，不是体素数据。
///
/// `radius_blocks` 是 x/z 上围绕世界原点的基础子块半径（1 个基础子块 = 32
/// 体素 ≈ 14.4 m）。debug 构建生成很慢（实测约 150 ms/子块），所以默认值刻意小：
/// - [TerrainConfig::default]：radius 2 / max_lod 1（7×7×4 ≈ 196 子块；debug ≈ 30 s，
///   release ≈ 2–4 s）；
/// - [TerrainConfig::demo]：radius 0 / max_lod 0（36 子块；debug ≈ 5 s）。
///
/// LOD 合并要半径 ≥ 5 才会越过 32 m 的第一档阈值，所以想看 LOD 切换要用
/// release 构建并调大半径（FFI：`start_bevy_voxel_perf(fixed_hz, runner_hz, radius, max_lod)`）。
///
/// [WorldPlugin]: crate::world::WorldPlugin
#[derive(Resource, Clone, Copy, Debug)]
pub struct TerrainConfig {
    /// 水平方向围绕世界原点的 mesh 块半径（基础子块坐标）。
    pub radius_blocks: i32,
    /// 最高 LOD（0..=3，见 `world::MAX_LOD`）。
    pub max_lod: u8,
    /// 世界生成种子。
    pub seed: u64,
    /// 生物群系（决定调色板与高度基线）。
    pub biome: BiomeId,
}

impl TerrainConfig {
    /// 默认实体 demo 用的极小地形：1 个 XZ 单元（36 个子块）、无 LOD。
    ///
    /// debug 构建实测约 150 ms/子块（36 子块 ≈ 5 s），所以"默认挂在地形上的
    /// demo"用最小规模；要看更大范围请走 `start_bevy_voxel_perf(..., radius, lod)`。
    #[must_use]
    pub fn demo() -> Self {
        Self {
            radius_blocks: 0,
            max_lod: 0,
            seed: 0xC0FF_EE,
            biome: DEFAULT_BIOME,
        }
    }
}

impl Default for TerrainConfig {
    fn default() -> Self {
        Self {
            radius_blocks: 2,
            max_lod: 1,
            seed: 0xC0FF_EE,
            biome: DEFAULT_BIOME,
        }
    }
}

/// 注册地形生成系统与运行期资源：由 [WorldPlugin](crate::world::WorldPlugin)
/// 在 `TerrainConfig` 资源存在时调用。
///
/// 只做两件事：挂 `Startup` 生成系统、初始化实体索引 / 统计资源；体素数据
/// 本身只以组件存在（生成时把 `VoxVolume` spawn 到实体上）。
pub(crate) fn register_systems(app: &mut App) {
    app.init_resource::<TerrainBlocks>()
        .init_resource::<TerrainNeighborBlocks>()
        .init_resource::<TerrainStats>()
        .add_systems(Startup, build_terrain);
}

/// 已产出的 mesh 块实体索引：`(块原点, lod) -> Entity`（确定性升序）。
#[derive(Resource, Default, Debug)]
pub struct TerrainBlocks(BTreeMap<(ChunkKey, u8), Entity>);

impl TerrainBlocks {
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn get(&self, origin: ChunkKey, lod: u8) -> Option<Entity> {
        self.0.get(&(origin, lod)).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = ((ChunkKey, u8), Entity)> + '_ {
        self.0.iter().map(|(key, entity)| (*key, *entity))
    }
}

/// 邻块包装缓存：`mesh_block_incremental` 的 `external` 只读借用与它自己的
/// `&mut WrappedBlockCache` 不能来自同一个变量（借用冲突），所以邻块单独一份。
#[derive(Resource, Default)]
pub struct TerrainNeighborBlocks(pub WrappedBlockCache);

/// 地形统计（demo / perf / 测试断言用；不进协议）。
#[derive(Resource, Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerrainStats {
    /// 生成的基础子块数。
    pub chunks: usize,
    /// 产出的 mesh 块实体数。
    pub blocks: usize,
    /// 每个 LOD 的块数（下标 = lod，0..=3）。
    pub lod_blocks: [usize; 4],
    /// 矩形总数（39 bit 描述符个数）。
    pub rects: usize,
    /// 因网格为空（全空气或实心内部无暴露面）而跳过的块数。
    pub skipped_empty: usize,
}

/// Setup 用体素边长（米，f64 镜像）。
#[must_use]
pub fn voxel_size_meters() -> f64 {
    DEFAULT_VOXEL_SIZE.to_num::<f64>()
}

/// Startup：生成子块 -> 逐块网格化 -> 产出 stable id 实体。
///
/// 体素容器在本地构建，生成完成后作为 `VoxVolume` **组件** spawn 到地形
/// 根实体上，不写进任何 Resource。
///
/// 用普通 system（多 `ResMut`）而不是 exclusive system，是为了让
/// interner / wrapped / 邻块缓存 / 增量缓存的借用天然分离；实体 id 由
/// `StableIdAllocator` 立即分配并登记进 `StableEntityIndex`（等价
/// `spawn_stable` 的当帧语义）。
#[allow(clippy::too_many_arguments)]
fn build_terrain(
    mut commands: Commands,
    config: Res<TerrainConfig>,
    mut interner: ResMut<VoxelInterner>,
    mut wrapped: ResMut<WrappedBlockCache>,
    mut neighbors: ResMut<TerrainNeighborBlocks>,
    mut ext_cache: ResMut<ExternalMaskCache>,
    mut mesh_cache: ResMut<IncrementalMeshCache>,
    mut blocks: ResMut<TerrainBlocks>,
    mut stats: ResMut<TerrainStats>,
    mut allocator: ResMut<StableIdAllocator>,
    mut index: ResMut<StableEntityIndex>,
) {
    let voxel_size_m = voxel_size_meters();
    let radius_blocks = config.radius_blocks.max(0);
    let max_lod = config.max_lod.min(crate::world::lod::MAX_LOD);

    // ── 1. 生成基础子块（本地 VoxVolume，最后作为组件挂到地形根实体）──
    let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
    let generator = WorldGenerator::new(WorldSeed(config.seed), config.biome);
    for key in chunk_keys_to_generate(radius_blocks, max_lod) {
        let tree = generator.generate_chunk(interner.inner_mut(), key);
        volume.insert_chunk(key, tree);
    }
    stats.chunks = volume.chunk_count();

    // ── 2. 逐块网格化 + 产出实体 ──
    for block in lod_blocks(radius_blocks, max_lod, voxel_size_m) {
        let lod = Lod::new(block.lod);
        let mesh_block = MeshBlock::new(block.origin, lod);
        let neighbors6 = neighbor_origins(mesh_block.origin, block.lod);

        // 预热同 LOD 的 6 邻（邻块缓存），随后只读取回。
        for neighbor in neighbors6.iter() {
            neighbors.0.get_or_wrap(
                &volume.chunks,
                interner.inner_mut(),
                MeshBlock::new(*neighbor, lod),
            );
        }
        let external: [Option<&VoxTree<u8>>; 6] = {
            let neighbor_cache = &neighbors.0;
            let mut out: [Option<&VoxTree<u8>>; 6] = [None; 6];
            for (face, neighbor) in neighbors6.iter().enumerate() {
                out[face] = neighbor_cache.get(*neighbor, lod);
            }
            out
        };

        let plan = MeshBlockDirty {
            block: mesh_block,
            internal: true,
            faces: FaceMask(0),
        };
        let (batch, _changed) = mesh_block_incremental(
            &volume.chunks,
            interner.inner_mut(),
            &mut wrapped,
            &mut ext_cache,
            &mut mesh_cache,
            plan,
            external,
        );
        let rect_count = batch.rect_count();
        let words = pack_rect_batch_with_ao(&batch);
        if words.is_empty() {
            // 全空气（天空）或实心内部（无暴露面）的块不产生几何：
            // 不建实体，避免每帧下发空 RectList 载荷。
            stats.skipped_empty += 1;
            continue;
        }

        // 块实体。
        let origin_m = block_origin_meters(block.origin, DEFAULT_VOXEL_SIZE.to_num::<f32>());
        let position = Vec3::from_array(origin_m);
        let mut presented = PresentedTransform::default();
        presented.set_sampled(
            RenderTransform::from_translation(position),
            RenderTransform::from_translation(position),
        );
        let id = allocator.allocate();
        let entity = commands
            .spawn((
                id,
                VoxelMeshBlock::new(block.lod, words),
                Transform::from_translation(position),
                Size(None),
                presented,
            ))
            .id();
        index.insert(id, entity);
        blocks.0.insert((block.origin, block.lod), entity);

        stats.blocks += 1;
        stats.lod_blocks[usize::from(block.lod)] += 1;
        stats.rects += rect_count;
    }

    // 地形根实体：体素容器以组件形式挂载
    //（「一岛 / 一船 / 一结构 = 一个 VoxVolume 组件」）；生成完成后一次性 attach。
    commands.spawn(volume);

    info!(
        "terrain: chunks={} blocks={} lod={:?} rects={} seed={:#x}",
        stats.chunks, stats.blocks, stats.lod_blocks, stats.rects, config.seed
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::WorldPlugin;
    use game_engine::identity::{StableEntityId, StableIdPlugin};
    use game_engine::voxel::{count_body_nodes, release_body};

    /// 半径 0（1 个 XZ 单元）：debug 下生成最便宜，单元测试够用。
    ///
    /// 地形不是插件：装配方插入 [TerrainConfig] 后由 [WorldPlugin] 注册系统。
    fn terrain_app(radius_blocks: i32, max_lod: u8) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.insert_resource(TerrainConfig {
            radius_blocks,
            max_lod,
            ..TerrainConfig::default()
        });
        app.add_plugins(WorldPlugin);
        app
    }

    /// 地形根实体：体素容器必须是**组件**，不是 Resource。
    fn volume_entity(world: &mut World) -> Entity {
        let mut query = world.query_filtered::<Entity, With<VoxVolume>>();
        query
            .iter(world)
            .next()
            .expect("地形根实体必须带 VoxVolume 组件")
    }

    /// 锁定边界：体素容器只以组件存在，不能退回全局 Resource。
    #[test]
    fn terrain_volume_is_a_component_not_a_resource() {
        let mut app = terrain_app(0, 0);
        app.update();
        let entity = volume_entity(app.world_mut());
        assert!(
            app.world().entity(entity).get::<VoxVolume>().is_some(),
            "体素容器必须是实体上的 VoxVolume 组件"
        );
    }

    #[test]
    fn build_terrain_produces_block_entities() {
        let mut app = terrain_app(0, 0);
        app.update();

        let stats = *app.world().resource::<TerrainStats>();
        let layers = y_band_layers(0) as usize;
        // 半径 0 → 1 个 XZ 单元 × y band；天空层（全空气）被跳过。
        assert_eq!(stats.chunks, 3 * 3 * layers, "半径 0 + margin 1");
        assert_eq!(stats.blocks + stats.skipped_empty, layers);
        assert!(stats.blocks > 0, "地表块必须产出几何");
        assert!(stats.skipped_empty > 0, "天空层必须被跳过");
        assert_eq!(stats.lod_blocks[0], stats.blocks);
        assert!(stats.rects > 0, "地表必须产生矩形");

        let blocks = app.world().resource::<TerrainBlocks>();
        assert_eq!(blocks.len(), stats.blocks);

        for ((origin, lod), entity) in blocks.iter() {
            assert_eq!(lod, 0);
            let entity_ref = app.world().entity(entity);
            assert!(entity_ref.get::<StableEntityId>().is_some());
            assert!(entity_ref.get::<Size>().is_some());
            assert!(entity_ref.get::<PresentedTransform>().is_some());

            let block = entity_ref
                .get::<VoxelMeshBlock>()
                .expect("mesh 块实体必须带 VoxelMeshBlock");
            assert_eq!(block.lod, 0);
            assert!(!block.is_empty());
            assert!(
                block.words.iter().all(|word| word >> 39 == 0),
                "矩形必须落在 39 bit 布局内"
            );

            // Transform 米坐标 = origin * 32 * voxel_size（不乘 2^lod）。
            let transform = entity_ref
                .get::<Transform>()
                .expect("块实体必须带 Transform");
            let want = block_origin_meters(origin, DEFAULT_VOXEL_SIZE.to_num::<f32>());
            assert!((transform.translation.x - want[0]).abs() < 1e-3);
            assert!((transform.translation.y - want[1]).abs() < 1e-3);
            assert!((transform.translation.z - want[2]).abs() < 1e-3);
        }
    }

    #[test]
    fn build_terrain_is_deterministic() {
        let mut first = terrain_app(0, 0);
        first.update();
        let mut second = terrain_app(0, 0);
        second.update();

        let words = |app: &App| -> Vec<Vec<u64>> {
            let blocks = app.world().resource::<TerrainBlocks>();
            let mut out = Vec::new();
            for (_, entity) in blocks.iter() {
                let block = app
                    .world()
                    .entity(entity)
                    .get::<VoxelMeshBlock>()
                    .expect("VoxelMeshBlock");
                out.push(block.words.clone());
            }
            out
        };
        assert_eq!(words(&first), words(&second));
        assert_eq!(
            *first.world().resource::<TerrainStats>(),
            *second.world().resource::<TerrainStats>()
        );
    }

    #[test]
    fn release_terrain_returns_interner_to_baseline() {
        let mut app = terrain_app(0, 0);
        // 基线：WorldPlugin 刚建好 interner、还没生成任何子块
        //（空哨兵本身可能算一个 node，所以不能断言 0）。
        let baseline = app
            .world()
            .resource::<VoxelInterner>()
            .inner()
            .alive_nodes();

        app.update();

        let alive_before = app
            .world()
            .resource::<VoxelInterner>()
            .inner()
            .alive_nodes();
        assert!(alive_before > baseline, "生成后必须有可达节点");
        let entity = volume_entity(app.world_mut());
        let volume = app
            .world()
            .entity(entity)
            .get::<VoxVolume>()
            .expect("地形根实体必须带 VoxVolume 组件");
        let volume_nodes =
            count_body_nodes(app.world().resource::<VoxelInterner>().inner(), volume);
        assert!(volume_nodes > 0);

        // 先清两个包装缓存（否则 root 引用把节点钉住），再 release_body。
        let world = app.world_mut();
        world.resource_scope::<VoxelInterner, _>(|world, mut interner| {
            {
                let mut wrapped = world.resource_mut::<WrappedBlockCache>();
                wrapped.clear(interner.inner_mut());
            }
            {
                let mut neighbors = world.resource_mut::<TerrainNeighborBlocks>();
                neighbors.0.clear(interner.inner_mut());
            }
            let entity = volume_entity(world);
            let mut entity_mut = world.entity_mut(entity);
            let mut volume = entity_mut
                .get_mut::<VoxVolume>()
                .expect("地形根实体必须带 VoxVolume 组件");
            let released = release_body(interner.inner_mut(), &mut volume);
            assert!(released > 0);
            assert_eq!(count_body_nodes(interner.inner(), &volume), 0);
            assert_eq!(
                interner.inner().alive_nodes(),
                baseline,
                "包装缓存与 body 释放后可达节点必须回到基线"
            );
        });
    }

    /// 大半径 LOD 场景：debug 下生成太慢，只在需要时手动跑。
    #[test]
    #[ignore = "半径 8 / LOD3 的生成在 debug 下很慢；release 手动运行"]
    fn terrain_with_lods_builds_coarse_blocks() {
        let mut app = terrain_app(8, 3);
        app.update();
        let stats = *app.world().resource::<TerrainStats>();
        assert!(stats.lod_blocks[1] > 0 || stats.lod_blocks[2] > 0);
    }
}
