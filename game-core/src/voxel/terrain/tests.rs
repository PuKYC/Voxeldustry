use std::collections::{BTreeMap, BTreeSet};

use bevy::prelude::*;
use game_engine::identity::{StableEntityId, StableEntityIndex, StableIdAllocator, StableIdPlugin};
use game_engine::math::Vec3F;
use game_engine::presentation::voxel::{
    raw_halo_has_solid_interior, WrappedBlockCache, RAW_VOXELS,
};
use game_engine::presentation::PresentedTransform;
use game_engine::spatial::Size;
use game_engine::voxel::{
    chunk_content_hash, chunk_key, count_body_nodes, lod_block_origin, release_body, ChunkKey, Lod,
    MaxDepth, MeshBlock, VoxOpsBulkWrite, VoxOpsRead, VoxOpsWrite, VoxTree, VoxVolume,
    VoxelInterner, CHUNK_DEPTH,
};

use crate::presentation::voxel_mesh::VoxChunkGroupRaw;
use crate::static_data::voxel::DEFAULT_VOXEL_SIZE;
use crate::voxel::lod::{VoxelLodConfig, MAX_LOD};
use crate::voxel::terrain::generation::WorldGenerator;
use crate::world::{IslandId, IslandRegistry};

use super::cache::invalidate_chunk_caches;
use super::extract::extract_block_raw;
use super::group_diff::group_neighbour_signature;
use super::resources::{TerrainCaches, TerrainGroupMembers, TerrainStreamState};
use super::systems::stream_terrain_lod;
use super::*;

/// 把一个基础子块在本块缓存对里**所有 LOD** 预热：
/// LOD0 走包装缓存，LOD>0 走按 LOD 生成的缓存（与生产路径一致）。
fn warm_chunk_all_lods(
    interner: &mut VoxelInterner,
    volume: &VoxVolume,
    wrapped: &mut WrappedBlockCache,
    generated: &mut GeneratedBlockCache,
    generator: &WorldGenerator,
    key: ChunkKey,
) {
    for raw_lod in 0..=MAX_LOD {
        let lod = Lod::new(raw_lod);
        let block = MeshBlock::new(lod_block_origin(key, lod), lod);
        let _ = wrapped.get_or_wrap(&volume.chunks, interner.inner_mut(), block);
        if raw_lod > 0 {
            let _ = generated.get_or_generate(interner.inner_mut(), generator, block);
        }
    }
}

/// 把 `key` 在 terrain **四份缓存实例**里所有 LOD 预热一遍。
///
/// LOD0 只进两份 wrapped 缓存（生产路径如此）；LOD>0 同时进 wrapped 与
/// generated，用来验证失效口径覆盖四份实例。
fn warm_chunk_all_four_caches(
    interner: &mut VoxelInterner,
    volume: &VoxVolume,
    wrapped: &mut WrappedBlockCache,
    neighbors: &mut TerrainNeighborBlocks,
    generated: &mut TerrainGeneratedBlocks,
    generator: &WorldGenerator,
    key: ChunkKey,
) {
    for raw_lod in 0..=MAX_LOD {
        let lod = Lod::new(raw_lod);
        let block = MeshBlock::new(lod_block_origin(key, lod), lod);
        let _ = wrapped.get_or_wrap(&volume.chunks, interner.inner_mut(), block);
        let _ = neighbors
            .0
            .get_or_wrap(&volume.chunks, interner.inner_mut(), block);
        if raw_lod > 0 {
            let _ = generated
                .own
                .get_or_generate(interner.inner_mut(), generator, block);
            let _ = generated
                .neighbors
                .get_or_generate(interner.inner_mut(), generator, block);
        }
    }
}

/// 半径 0（1 个 XZ 单元）：debug 下生成最便宜，单元测试够用。
///
/// 装配方插入 [TerrainConfig] 后由 [TerrainPlugin] 注册生成系统。
fn terrain_app(radius_blocks: i32, max_lod: u8) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(StableIdPlugin);
    app.insert_resource(TerrainConfig {
        radius_blocks,
        max_lod,
        ..TerrainConfig::default()
    });
    app.add_plugins(crate::voxel::GameVoxelPlugin);
    app.add_plugins(TerrainPlugin);
    // 只用 app.update() 的测试必须显式 finish，Avian 在 Plugin::finish 注册诊断资源。
    app.finish();
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

/// (c) 地形根实体是正式 island body：一次 `update()`（Startup 里的
/// `build_terrain` -> `attach_terrain_body`）之后必须已有 StableEntityId 与
/// IslandRegistry 条目；anchored = true、drift = ZERO、biome = 配置值。
///
/// 这条同时是「exclusive system 能看到 build_terrain 的 Commands」的实测：
/// 地形根实体只由 `Commands` 生成，若命令没 flush，这里的查询会是空、
/// 注册表里也不会有任何条目。
#[test]
fn terrain_root_entity_is_a_registered_island_body() {
    let mut app = terrain_app(0, 0);
    app.update();

    let world = app.world_mut();
    let entities: Vec<Entity> = {
        let mut query = world.query_filtered::<Entity, With<VoxVolume>>();
        query.iter(world).collect()
    };
    assert_eq!(entities.len(), 1, "地形根实体必须恰好一个");
    let entity = entities[0];

    let stable = *world
        .get::<StableEntityId>(entity)
        .expect("地形根实体必须有 StableEntityId：attach_terrain_body 必须在 Startup 生效");
    let config_biome = world.resource::<TerrainConfig>().biome;

    // StableEntityIndex 双向可查。
    {
        let index = world.resource::<StableEntityIndex>();
        assert_eq!(index.id(entity), Some(stable));
        assert_eq!(index.entity(stable), Some(entity));
    }
    // 注册表里恰好一条地形岛，语义字段正确。
    {
        let registry = world.resource::<IslandRegistry>();
        assert_eq!(registry.len(), 1, "地形是唯一一条岛注册表记录");
        let island = registry
            .get(IslandId(stable.0))
            .expect("地形根实体必须在 IslandRegistry 里有对应条目");
        assert!(island.anchored, "地形是锚定岛");
        assert_eq!(island.drift, Vec3F::ZERO, "锚定地形 drift 必须为零");
        assert_eq!(
            island.biome, config_biome,
            "biome 必须取 TerrainConfig.biome"
        );
    }

    // 幂等：直接再跑一次系统（等价于 Startup 重复执行），不新增条目、不取号。
    let next_id = world.resource::<StableIdAllocator>().next_id();
    attach_terrain_body(world);
    assert_eq!(world.resource::<IslandRegistry>().len(), 1);
    assert_eq!(world.resource::<StableIdAllocator>().next_id(), next_id);
    assert_eq!(world.get::<StableEntityId>(entity), Some(&stable));

    // 再 update 一次（Update 里的流式系统）也不新增注册表条目 / 不再分配 ID。
    app.update();
    assert_eq!(
        app.world().resource::<IslandRegistry>().len(),
        1,
        "再 update 一次不得新增岛注册表条目"
    );
    assert_eq!(
        app.world().resource::<StableIdAllocator>().next_id(),
        next_id,
        "再 update 一次不得再分配稳定 ID"
    );
    assert_eq!(app.world().get::<StableEntityId>(entity), Some(&stable));
}

#[test]
fn build_terrain_produces_group_entities() {
    let mut app = terrain_app(0, 0);
    app.update();

    let stats = *app.world().resource::<TerrainStats>();
    let layers = y_band_layers(0) as usize;
    // 半径 0 → 1 个 XZ 单元 × y band；天空层（全空气）被跳过。
    assert_eq!(stats.chunks, 3 * 3 * layers, "半径 0 + margin 1");
    // 块级口径：非空块 + 全空块 = 期望块总数。
    assert_eq!(stats.blocks + stats.skipped_empty, layers);
    assert!(stats.blocks > 0, "地表块必须产出几何");
    assert!(stats.skipped_empty > 0, "天空层必须被跳过");
    assert_eq!(stats.lod_blocks[0], stats.blocks);

    let groups = app.world().resource::<TerrainGroups>();
    assert_eq!(groups.len(), stats.groups);
    // 与 build_terrain 相同的静态分区：用来重算每块的期望 skirt_mask。
    let partition = TerrainPartition::new(&lod_blocks(0, 0, voxel_size_meters()));
    assert!(stats.groups > 0 && stats.groups <= layers, "组数应少于块数");
    assert_eq!(stats.lod_groups[0], stats.groups);

    let mut total_chunks = 0usize;
    for (key, entity) in groups.iter() {
        assert_eq!(key.lod, 0);
        let entity_ref = app.world().entity(entity);
        assert!(entity_ref.get::<StableEntityId>().is_some());
        assert!(entity_ref.get::<Size>().is_some());
        assert!(entity_ref.get::<PresentedTransform>().is_some());

        let raw = entity_ref
            .get::<VoxChunkGroupRaw>()
            .expect("mesh 分组实体必须带 VoxChunkGroupRaw");
        assert_eq!(raw.lod, 0);
        assert!(!raw.chunks.is_empty(), "只有含非空块才会 spawn");
        for (origin, (blocks, mask)) in &raw.chunks {
            assert_eq!(group_key(*origin, 0), key, "成员块必须属于该组");
            assert_eq!(blocks.len(), RAW_VOXELS);
            assert!(raw_halo_has_solid_interior(blocks));
            // LOD0 划分没有跨 LOD 面；非零位只来自「面外没有同 LOD 邻居」。
            let block = TerrainBlock {
                origin: *origin,
                lod: 0,
            };
            let expected = lod_mismatch_mask(&block, &partition)
                | missing_face_mask(&face_neighbors(&block, &partition));
            assert_eq!(*mask, expected, "skirt_mask 必须是错位面 | 无同 LOD 邻居面");
            total_chunks += 1;
        }

        // Transform 米坐标 = 组原点 * 32 * voxel_size（不乘 2^lod）。
        let transform = entity_ref
            .get::<Transform>()
            .expect("分组实体必须带 Transform");
        let want = block_origin_meters(group_origin(key), DEFAULT_VOXEL_SIZE.to_num::<f32>());
        assert!((transform.translation.x - want[0]).abs() < 1e-3);
        assert!((transform.translation.y - want[1]).abs() < 1e-3);
        assert!((transform.translation.z - want[2]).abs() < 1e-3);
    }
    assert_eq!(total_chunks, stats.blocks, "组内块数总和 = 非空块数");
}

/// 手搭含跨 LOD 面的划分：extract_block_raw 的 skirt_mask 必须等于
/// 「跨 LOD 错位面 | 面外没有同 LOD 邻居的面」。
#[test]
fn extract_block_raw_reports_cross_lod_skirt_mask() {
    // 被测块 (1,0,0) LOD0；+X 面外是 LOD1 粗块 (2,0,0)（跨 LOD），
    // +Y 面是同 LOD 块 (1,1,0)（不应置位）。
    let block = TerrainBlock {
        origin: chunk_key(1, 0, 0),
        lod: 0,
    };
    let coarse = TerrainBlock {
        origin: chunk_key(2, 0, 0),
        lod: 1,
    };
    let same_y = TerrainBlock {
        origin: chunk_key(1, 1, 0),
        lod: 0,
    };
    let partition = TerrainPartition::new(&[block, coarse, same_y]);
    let faces = face_neighbors(&block, &partition);
    let expected = lod_mismatch_mask(&block, &partition) | missing_face_mask(&faces);
    assert_eq!(
        lod_mismatch_mask(&block, &partition),
        1 << 0,
        "只有 +X 面跨 LOD"
    );
    assert_eq!(face_neighbors(&block, &partition)[0], FaceNeighbor::Missing);
    // +Y 面是同 LOD 块，不进掩码。
    assert_eq!(expected & (1 << 2), 0);

    // 只放被测块自己的实心 base 子块；缺失邻块按空气处理，不影响掩码。
    let mut interner = VoxelInterner::new(4 * 1024 * 1024);
    let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
    let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
    tree.fill(interner.inner_mut(), 1);
    volume.insert_chunk(block.origin, tree);

    let mut wrapped = WrappedBlockCache::default();
    let mut generated = TerrainGeneratedBlocks::default();
    let mut neighbors = TerrainNeighborBlocks::default();
    let generator = WorldGenerator::from_seed(0x00C0_FFEE);
    let mut caches = TerrainCaches {
        interner: &mut interner,
        wrapped: &mut wrapped,
        generated: &mut generated,
        neighbors: &mut neighbors,
    };
    let (raw, mask) = extract_block_raw(block, &volume, &generator, &partition, &mut caches)
        .expect("实心块必须产出 raw halo");

    assert_eq!(raw.len(), RAW_VOXELS);
    assert!(raw_halo_has_solid_interior(&raw));
    assert_eq!(
        mask, expected,
        "extract_block_raw 的掩码必须与纯函数报告一致"
    );
}

/// 确定性快照：组键 + 组内逐块 (原点, halo, mask)，均按键升序。
type GroupSnapshot = Vec<(GroupKey, Vec<(ChunkKey, Vec<u8>, u8)>)>;

#[test]
fn build_terrain_is_deterministic() {
    let mut first = terrain_app(0, 0);
    first.update();
    let mut second = terrain_app(0, 0);
    second.update();

    // 确定性快照：组键 + 组内逐块 (原点, halo, mask)，均按键升序。
    let group_snapshot = |app: &App| -> GroupSnapshot {
        let groups = app.world().resource::<TerrainGroups>();
        let mut out = Vec::new();
        for (key, entity) in groups.iter() {
            let raw = app
                .world()
                .entity(entity)
                .get::<VoxChunkGroupRaw>()
                .expect("VoxChunkGroupRaw");
            let chunks: Vec<(ChunkKey, Vec<u8>, u8)> = raw
                .chunks
                .iter()
                .map(|(origin, (blocks, mask))| (*origin, blocks.clone(), *mask))
                .collect();
            out.push((key, chunks));
        }
        out
    };
    assert_eq!(group_snapshot(&first), group_snapshot(&second));
    assert_eq!(
        *first.world().resource::<TerrainStats>(),
        *second.world().resource::<TerrainStats>()
    );
}

#[test]
fn release_terrain_returns_interner_to_baseline() {
    let mut app = terrain_app(0, 0);
    // 基线：GameVoxelPlugin 刚建好 interner、还没生成任何子块
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
    let volume_nodes = count_body_nodes(app.world().resource::<VoxelInterner>().inner(), volume);
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

/// 整卷释放必须清 terrain 四份缓存；`max_lod > 0` 时 generated（own+neighbors）
/// 不能把粗 LOD 节点钉在 interner 里。
#[test]
fn release_terrain_body_clears_all_caches_at_nonzero_lod() {
    let mut app = streaming_app_resources(2, 1);
    let baseline = app
        .world()
        .resource::<VoxelInterner>()
        .inner()
        .alive_nodes();
    spawn_synthetic_volume(&mut app, 2, 1);

    set_stream_config(
        &mut app,
        VoxelLodConfig {
            max_lod: 1,
            thresholds_m: [1.0, 2.0, 3.0],
        },
    );
    // 远观察者 -> 分区合并到 LOD1，填充 generated.own / neighbors。
    set_stream_observer(&mut app, [10_000.0, 0.0, 0.0]);
    app.update();

    let stats = *app.world().resource::<TerrainStats>();
    assert!(
        stats.lod_blocks[1] > 0,
        "必须产出 LOD1 块才能覆盖 generated 缓存：{:?}",
        stats.lod_blocks
    );

    // 把四份缓存暂时取出，补预热两份 wrapped 缓存（全 LOD），再整卷释放。
    let mut wrapped = app
        .world_mut()
        .remove_resource::<WrappedBlockCache>()
        .expect("WrappedBlockCache");
    let mut neighbors = app
        .world_mut()
        .remove_resource::<TerrainNeighborBlocks>()
        .expect("TerrainNeighborBlocks");
    let mut generated = app
        .world_mut()
        .remove_resource::<TerrainGeneratedBlocks>()
        .expect("TerrainGeneratedBlocks");
    assert!(!generated.own.is_empty() || !generated.neighbors.is_empty());

    let entity = volume_entity(app.world_mut());
    let generator = WorldGenerator::from_seed(0x00C0_FFEE);
    let key = chunk_key(0, 0, 0);
    let world = app.world_mut();
    world.resource_scope::<VoxelInterner, _>(|world, mut interner| {
        let volume = world.get::<VoxVolume>(entity).expect("VoxVolume");
        warm_chunk_all_four_caches(
            &mut interner,
            volume,
            &mut wrapped,
            &mut neighbors,
            &mut generated,
            &generator,
            key,
        );
    });
    assert!(!wrapped.is_empty() && !neighbors.0.is_empty());

    let released = {
        let world = app.world_mut();
        world.resource_scope::<VoxelInterner, _>(|world, mut interner| {
            let entity = volume_entity(world);
            let mut entity_mut = world.entity_mut(entity);
            let mut volume = entity_mut
                .get_mut::<VoxVolume>()
                .expect("地形根实体必须带 VoxVolume 组件");
            release_terrain_body(
                &mut interner,
                &mut volume,
                &mut wrapped,
                &mut generated,
                &mut neighbors,
            )
        })
    };

    assert!(released > 0);
    assert!(wrapped.is_empty(), "WrappedBlockCache 未清空");
    assert!(neighbors.0.is_empty(), "TerrainNeighborBlocks 未清空");
    assert!(generated.own.is_empty(), "generated.own 未清空");
    assert!(generated.neighbors.is_empty(), "generated.neighbors 未清空");
    {
        let volume = app
            .world()
            .entity(entity)
            .get::<VoxVolume>()
            .expect("VoxVolume");
        assert!(volume.is_empty(), "整卷释放后 volume 必须为空");
    }
    assert_eq!(
        app.world()
            .resource::<VoxelInterner>()
            .inner()
            .alive_nodes(),
        baseline,
        "四份缓存 + body 全部释放后，存活节点必须回空 interner 基线"
    );

    app.world_mut().insert_resource(wrapped);
    app.world_mut().insert_resource(neighbors);
    app.world_mut().insert_resource(generated);
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

// ─────────────── 运行期流式重划分（合成体素，不跑 worldgen）───────────────

/// 只装资源 / 系统、**不建 volume** 的流式 App（用于需要先记空 interner 基线的测试）。
fn streaming_app_resources(radius: i32, max_lod: u8) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(StableIdPlugin);
    app.add_plugins(crate::voxel::GameVoxelPlugin);
    app.init_resource::<WrappedBlockCache>()
        .init_resource::<TerrainGeneratedBlocks>()
        .init_resource::<TerrainNeighborBlocks>()
        .init_resource::<TerrainGroups>()
        .init_resource::<TerrainGroupMembers>()
        .init_resource::<TerrainStats>()
        .init_resource::<VoxelLodObserver>()
        .init_resource::<VoxelLodConfig>()
        .init_resource::<TerrainStreamState>();
    app.insert_resource(TerrainConfig {
        radius_blocks: radius,
        max_lod,
        ..TerrainConfig::default()
    });
    app.add_systems(Update, stream_terrain_lod);
    // 只用 app.update() 的测试必须显式 finish，Avian 在 Plugin::finish 注册诊断资源。
    app.finish();
    app
}

/// 给流式 App 塞一份合成实心体素（跳过 worldgen）。
fn spawn_synthetic_volume(app: &mut App, radius: i32, max_lod: u8) {
    let world = app.world_mut();
    let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
    world.resource_scope::<VoxelInterner, _>(|world, mut interner| {
        for key in chunk_keys_to_generate(radius, max_lod) {
            let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
            tree.fill(interner.inner_mut(), 1);
            volume.insert_chunk(key, tree);
        }
        world.spawn(volume);
    });
}

/// 合成实心体素 + 只挂 key 资源的 App：跳过 worldgen，测试快且完全确定。
///
/// 直接调用私有系统 stream_terrain_lod，等于把流式逻辑从生成里隔离出来验证。
fn streaming_app(radius: i32, max_lod: u8) -> App {
    let mut app = streaming_app_resources(radius, max_lod);
    spawn_synthetic_volume(&mut app, radius, max_lod);
    app
}

fn set_stream_observer(app: &mut App, position: [f32; 3]) {
    app.world_mut().resource_mut::<VoxelLodObserver>().position = position;
}

fn set_stream_config(app: &mut App, config: VoxelLodConfig) {
    *app.world_mut().resource_mut::<VoxelLodConfig>() = config;
}

/// 当前分组键（BTreeMap 升序 -> 直接是确定性顺序）。
fn stream_group_keys(app: &App) -> Vec<GroupKey> {
    app.world()
        .resource::<TerrainGroups>()
        .iter()
        .map(|(key, _)| key)
        .collect()
}

#[test]
fn stream_terrain_lod_restreams_around_observer() {
    // 半径 2 半开 [-2,2) 才有对齐的 2×2 母块可合并；半开半径 1 只有 2×2 格，
    // 凑不齐任何对齐母块，永远停在 LOD0（闭区间下靠 x/z=1 的多余格才够）。
    let mut app = streaming_app(2, 1);
    set_stream_config(
        &mut app,
        VoxelLodConfig {
            max_lod: 1,
            thresholds_m: [1.0, 2.0, 3.0],
        },
    );

    // 第一帧：观察者在原点 -> 母块 AABB 都含原点，距离 0，全部 LOD0。
    set_stream_observer(&mut app, [0.0, 0.0, 0.0]);
    app.update();
    let before = *app.world().resource::<TerrainStats>();
    assert!(before.blocks > 0);
    assert_eq!(before.lod_blocks[0], before.blocks, "近观察者必须全 LOD0");
    let stale = app
        .world()
        .resource::<TerrainGroups>()
        .get(group_key(chunk_key(0, 0, 0), 0))
        .expect("(0,0,0) 所在的 LOD0 组必须存在");

    // 观察者移到 +X 极远 -> 完整母块合并成 LOD1。
    set_stream_observer(&mut app, [10_000.0, 0.0, 0.0]);
    app.update();

    let stats = *app.world().resource::<TerrainStats>();
    assert!(
        stats.lod_blocks[1] > 0,
        "远端必须出现 LOD1：{:?}",
        stats.lod_blocks
    );
    assert_eq!(stats.groups, app.world().resource::<TerrainGroups>().len());
    assert_eq!(
        app.world().resource::<StableEntityIndex>().len(),
        stats.groups,
        "实体索引必须与 TerrainGroups 一致"
    );
    assert!(
        app.world().get_entity(stale).is_err(),
        "离开分区的旧组实体必须被 despawn"
    );
    for (key, entity) in app.world().resource::<TerrainGroups>().iter() {
        let entity_ref = app
            .world()
            .get_entity(entity)
            .unwrap_or_else(|_| panic!("分组实体 {key:?} 必须存在"));
        assert!(entity_ref.get::<VoxChunkGroupRaw>().is_some());
    }
}

fn run_observer_sequence() -> (Vec<GroupKey>, TerrainStats) {
    let mut app = streaming_app(1, 1);
    set_stream_config(
        &mut app,
        VoxelLodConfig {
            max_lod: 1,
            thresholds_m: [1.0, 2.0, 3.0],
        },
    );
    for position in [
        [0.0, 0.0, 0.0],
        [100.0, 0.0, 0.0],
        [100.0, 0.0, 200.0],
        [-100.0, 0.0, -50.0],
    ] {
        set_stream_observer(&mut app, position);
        app.update();
    }
    (
        stream_group_keys(&app),
        *app.world().resource::<TerrainStats>(),
    )
}

#[test]
fn stream_terrain_lod_is_deterministic() {
    let first = run_observer_sequence();
    let second = run_observer_sequence();
    assert_eq!(first.0, second.0, "同一观察者序列必须产生相同分区键");
    assert_eq!(first.1, second.1, "统计必须一致");
}

#[test]
fn stream_terrain_lod_keeps_registered_state_consistent() {
    // 走真实 TerrainPlugin 注册路径（radius 0 的 worldgen 便宜）。
    let mut app = terrain_app(0, 0);
    app.update();
    let before = *app.world().resource::<TerrainStats>();

    // radius 0 没有可合并的母块：换阈值 + 远观察者也不改分区。
    *app.world_mut().resource_mut::<VoxelLodConfig>() = VoxelLodConfig {
        max_lod: 0,
        thresholds_m: [1.0, 2.0, 3.0],
    };
    app.world_mut().resource_mut::<VoxelLodObserver>().position = [5_000.0, 0.0, 0.0];
    app.update();

    let after = *app.world().resource::<TerrainStats>();
    assert_eq!(before.blocks, after.blocks);
    assert_eq!(before.groups, after.groups);
    assert_eq!(after.groups, app.world().resource::<TerrainGroups>().len());
    // 分组实体 + 地形根实体（正式 island body，见 attach_terrain_body）。
    assert_eq!(
        app.world().resource::<StableEntityIndex>().len(),
        after.groups + 1
    );
}

/// (a) 合成全实心地形：每个分组实体的块集合 == 该 GroupKey 覆盖的 desired 块。
///
/// 用固定的 thresholds/config 调一次与系统相同的 `lod_blocks_for_observer`，
/// 直接比对，避免依赖具体分区形状。
#[test]
fn group_members_match_desired_blocks() {
    let config = VoxelLodConfig {
        max_lod: 1,
        thresholds_m: [1.0, 2.0, 3.0],
    };
    let mut app = streaming_app(1, 1);
    set_stream_config(&mut app, config);
    set_stream_observer(&mut app, [0.0, 0.0, 0.0]);
    app.update();

    let desired = lod_blocks_for_observer([0.0, 0.0], 1, 1, voxel_size_meters(), &config);
    let mut want: BTreeMap<GroupKey, BTreeSet<ChunkKey>> = BTreeMap::new();
    for block in &desired {
        want.entry(group_key(block.origin, block.lod))
            .or_default()
            .insert(block.origin);
    }

    let groups = app.world().resource::<TerrainGroups>();
    let have: BTreeMap<GroupKey, BTreeSet<ChunkKey>> = groups
        .iter()
        .map(|(key, entity)| {
            let raw = app
                .world()
                .entity(entity)
                .get::<VoxChunkGroupRaw>()
                .expect("VoxChunkGroupRaw");
            assert!(raw.chunks.values().all(|(b, _)| b.len() == RAW_VOXELS));
            (key, raw.chunks.keys().copied().collect())
        })
        .collect();
    // 合成体素全实心 -> 没有空块，实体块集合应逐组等于 desired。
    assert_eq!(
        have, want,
        "组内块集合必须等于该 GroupKey 覆盖的 desired 块"
    );
    assert!(!want.is_empty());
}

/// (b) 只改动一个组的成员集，只重建该组：其它组实体 id 不变，旧实体被 despawn。
///
/// 直接篡改受害者组的成员快照模拟「该组内一块变化」，并把 `last_observer`
/// 置成 NaN 绕过 early-out（观察者/配置本身不变，期望分区完全相同）。
#[test]
fn changing_one_group_membership_only_rebuilds_that_group() {
    let mut app = streaming_app(1, 1);
    set_stream_config(
        &mut app,
        VoxelLodConfig {
            max_lod: 1,
            thresholds_m: [1.0, 2.0, 3.0],
        },
    );
    set_stream_observer(&mut app, [0.0, 0.0, 0.0]);
    app.update();

    let before: BTreeMap<GroupKey, Entity> =
        app.world().resource::<TerrainGroups>().iter().collect();
    assert!(
        before.len() >= 2,
        "测试需要至少两个组：{:?}",
        before.keys().collect::<Vec<_>>()
    );
    let victim = *before.keys().next().expect("非空");

    // 篡改受害者组的成员快照（模拟该组内一块变化），并强制重跑 diff。
    {
        let mut members = app.world_mut().resource_mut::<TerrainGroupMembers>();
        let snapshot = members.0.get_mut(&victim).expect("受害者组必须在快照里");
        snapshot.blocks.insert(chunk_key(12_345, 0, 0));
    }
    app.world_mut()
        .resource_mut::<TerrainStreamState>()
        .last_observer = [f32::NAN, 0.0, 0.0];

    app.update();

    let groups = app.world().resource::<TerrainGroups>();
    let rebuilt = groups.get(victim).expect("受害者组必须重建");
    assert_ne!(rebuilt, before[&victim], "受害者组必须换新实体");
    assert!(
        app.world().get_entity(before[&victim]).is_err(),
        "受害者组旧实体必须 despawn"
    );
    for (key, old_entity) in &before {
        if *key == victim {
            continue;
        }
        assert_eq!(
            groups.get(*key),
            Some(*old_entity),
            "未变化组 {key:?} 的实体 id 不得改变"
        );
    }
    // 篡改只在快照层：重建后组内容回到 desired，不含伪块。
    let raw = app
        .world()
        .entity(rebuilt)
        .get::<VoxChunkGroupRaw>()
        .expect("VoxChunkGroupRaw");
    assert!(
        !raw.chunks.contains_key(&chunk_key(12_345, 0, 0)),
        "重建必须回到 desired 成员"
    );
}

/// (c) 邻块 LOD 变化、但本组**成员集合不变**时也必须重建：
/// 本块 halo 采样与 skirt_mask 依赖 6 面邻居的 LOD，只比成员集合会残留旧裙边。
#[test]
fn neighbour_lod_change_rebuilds_group_without_membership_change() {
    let member = chunk_key(1, 0, 0);
    let members: BTreeSet<ChunkKey> = [member].into_iter().collect();

    // 邻居 (+x) 更粗：本块 +x 面 = Coarser。
    let coarser = [
        TerrainBlock {
            origin: member,
            lod: 0,
        },
        TerrainBlock {
            origin: chunk_key(2, 0, 0),
            lod: 1,
        },
    ];
    let sig_coarser = group_neighbour_signature(&members, 0, &TerrainPartition::new(&coarser));

    // 邻居换成同 LOD：本块 +x 面 = Same，成员集合完全不变。
    let same = [
        TerrainBlock {
            origin: member,
            lod: 0,
        },
        TerrainBlock {
            origin: chunk_key(2, 0, 0),
            lod: 0,
        },
    ];
    let sig_same = group_neighbour_signature(&members, 0, &TerrainPartition::new(&same));

    // 跨 LOD 面在 mesh 报告里已经是 Missing，但错位掩码必须记下来。
    assert_eq!(sig_coarser[&member].0[0], FaceNeighbor::Missing);
    assert_eq!(sig_coarser[&member].1, 1 << 0, "跨 LOD 面必须进裙边掩码");
    assert!(matches!(sig_same[&member].0[0], FaceNeighbor::Same(_)));
    assert_eq!(sig_same[&member].1, 0);
    assert_ne!(sig_coarser, sig_same, "邻块 LOD 变化必须改变组重建签名");
}

// ─────────────── 编辑失效：四份缓存 + LOD0 内容（阶段 -1 补丁）───────────────

/// 编辑失效必须清 terrain 的**四份缓存实例**（own + neighbors × wrapped/generated），
/// 而不是只看 wrapper；LOD0 重新提取的内容必须等于编辑后的 volume。
///
/// 粗 LOD 内容跟随编辑属阶段 3（`generate_block_at_lod` 不读 volume），
/// 这里只锁定“缓存不会返回编辑前旧树”。
#[test]
fn invalidate_chunk_caches_clears_all_four_instances_and_lod0_reflects_edit() {
    let mut interner = VoxelInterner::new(4 * 1024 * 1024);
    let generator = WorldGenerator::from_seed(0x00C0_FFEE);
    let key = chunk_key(0, 0, 0);
    let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
    volume.insert_chunk(key, generator.generate_chunk(interner.inner_mut(), key));

    let mut wrapped = WrappedBlockCache::default();
    let mut neighbors = TerrainNeighborBlocks::default();
    let mut generated = TerrainGeneratedBlocks::default();
    warm_chunk_all_four_caches(
        &mut interner,
        &volume,
        &mut wrapped,
        &mut neighbors,
        &mut generated,
        &generator,
        key,
    );
    // 先确认四份实例都真的预热了（否则“清空”断言没有意义）。
    for raw_lod in 0..=MAX_LOD {
        let lod = Lod::new(raw_lod);
        let origin = lod_block_origin(key, lod);
        assert!(wrapped.get(origin, lod).is_some(), "wrapped LOD{raw_lod}");
        assert!(
            neighbors.0.get(origin, lod).is_some(),
            "neighbors LOD{raw_lod}"
        );
        if raw_lod > 0 {
            assert!(
                generated.own.get(origin, lod).is_some(),
                "generated.own LOD{raw_lod}"
            );
            assert!(
                generated.neighbors.get(origin, lod).is_some(),
                "generated.neighbors LOD{raw_lod}"
            );
        }
    }

    // 编辑一个体素（取一个与当前值不同的值），记录编辑后的 body 哈希。
    let edit = bevy::math::IVec3::new(1, 2, 3);
    let current = volume
        .get_chunk(&key)
        .expect("chunk 必须存在")
        .get(interner.inner(), edit)
        .unwrap_or(0);
    let edited = if current == 9 { 8 } else { 9 };
    assert!(
        volume
            .get_chunk_mut(&key)
            .expect("chunk 必须存在")
            .set(interner.inner_mut(), edit, edited),
        "编辑必须改变树"
    );
    let edited_hash = chunk_content_hash(interner.inner(), volume.get_chunk(&key).unwrap());

    invalidate_chunk_caches(
        &mut interner,
        key,
        &mut wrapped,
        &mut generated,
        &mut neighbors,
    );

    // 四份实例 × 所有 LOD：该 key 覆盖的条目必须全部消失。
    for raw_lod in 0..=MAX_LOD {
        let lod = Lod::new(raw_lod);
        let origin = lod_block_origin(key, lod);
        assert!(
            wrapped.get(origin, lod).is_none(),
            "wrapped LOD{raw_lod} 未失效"
        );
        assert!(
            neighbors.0.get(origin, lod).is_none(),
            "neighbors LOD{raw_lod} 未失效"
        );
        assert!(
            generated.own.get(origin, lod).is_none(),
            "generated.own LOD{raw_lod} 未失效"
        );
        assert!(
            generated.neighbors.get(origin, lod).is_none(),
            "generated.neighbors LOD{raw_lod} 未失效"
        );
    }

    // LOD0 走 wrapped 路径重新提取：hash 与体素都必须等于编辑后的 volume。
    let rewrapped = wrapped.get_or_wrap(
        &volume.chunks,
        interner.inner_mut(),
        MeshBlock::new(key, Lod::new(0)),
    );
    assert_eq!(
        chunk_content_hash(interner.inner(), rewrapped),
        edited_hash,
        "LOD0 重提取必须读到编辑后的内容"
    );
    assert_eq!(rewrapped.get(interner.inner(), edit), Some(edited));
    wrapped.clear(interner.inner_mut());
}

// ─────────────── 唯一卸载入口 unload_chunk（阶段 -1）───────────────

/// 卸载后 body 可达节点回 0，两套「本块」缓存的所有 LOD 条目都被清空，
/// interner 存活节点回到基线；重复卸载幂等。
#[test]
fn unload_chunk_returns_body_and_caches_to_baseline() {
    let mut interner = VoxelInterner::new(4 * 1024 * 1024);
    let generator = WorldGenerator::from_seed(0x00C0_FFEE);
    let keys = [chunk_key(0, 0, 0), chunk_key(1, 0, 0)];
    let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
    let baseline = interner.inner().alive_nodes();
    assert_eq!(count_body_nodes(interner.inner(), &volume), 0);

    let mut wrapped = WrappedBlockCache::default();
    let mut generated = GeneratedBlockCache::default();
    for &key in &keys {
        let tree = generator.generate_chunk(interner.inner_mut(), key);
        volume.insert_chunk(key, tree);
        warm_chunk_all_lods(
            &mut interner,
            &volume,
            &mut wrapped,
            &mut generated,
            &generator,
            key,
        );
    }
    assert!(count_body_nodes(interner.inner(), &volume) > 0);
    // 两个基础子块在 LOD0 各一条，LOD>0 的粗块覆盖两者、每档各一条：
    // 这证明卸载测试确实覆盖了「所有 LOD」，而不是只清 LOD0。
    assert_eq!(
        wrapped.len(),
        keys.len() + usize::from(MAX_LOD),
        "包装缓存必须预热 LOD0 + 每个粗 LOD"
    );
    assert_eq!(
        generated.len(),
        usize::from(MAX_LOD),
        "生成缓存必须预热每个粗 LOD"
    );

    for &key in &keys {
        assert!(
            unload_chunk(
                &mut interner,
                &mut volume,
                &mut wrapped,
                &mut generated,
                key
            ),
            "volume 里有树时必须返回 true"
        );
        assert!(!volume.contains_chunk(&key));
    }
    assert!(volume.is_empty());
    assert_eq!(count_body_nodes(interner.inner(), &volume), 0);
    assert!(wrapped.is_empty(), "LOD0..=MAX_LOD 的包装条目都必须失效");
    assert!(generated.is_empty(), "LOD>0 的生成条目都必须失效");
    assert_eq!(
        interner.inner().alive_nodes(),
        baseline,
        "缓存与 body 的引用都归还后，存活节点必须回基线"
    );

    // 幂等：没有树可取时返回 false，不重复 dec_ref、不 panic。
    assert!(
        !unload_chunk(
            &mut interner,
            &mut volume,
            &mut wrapped,
            &mut generated,
            keys[0]
        ),
        "重复卸载必须返回 false"
    );
}

/// 同一块 load → unload 循环 100 轮，存活节点数不增长。
///
/// 每轮改一个不同体素，避免「内容完全相同 -> hash-cons 复用节点」把泄漏
/// 掩盖成「不增长」。粗 LOD 生成缓存由上一个测试覆盖，这里只走包装缓存，
/// 保证 100 轮在 debug 下也够快。
#[test]
fn unload_chunk_cycle_does_not_grow_nodes() {
    let mut interner = VoxelInterner::new(4 * 1024 * 1024);
    let key = chunk_key(0, 0, 0);
    let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
    let baseline = interner.inner().alive_nodes();

    for cycle in 0..100 {
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
        tree.fill(interner.inner_mut(), 2);
        let _ = tree.set(
            interner.inner_mut(),
            bevy::math::IVec3::new(cycle % 32, 0, 0),
            9,
        );
        volume.insert_chunk(key, tree);
        assert!(count_body_nodes(interner.inner(), &volume) > 0);

        let mut wrapped = WrappedBlockCache::default();
        for raw_lod in 0..=MAX_LOD {
            let lod = Lod::new(raw_lod);
            let block = MeshBlock::new(lod_block_origin(key, lod), lod);
            let _ = wrapped.get_or_wrap(&volume.chunks, interner.inner_mut(), block);
        }
        assert!(!wrapped.is_empty());

        let mut generated = GeneratedBlockCache::default();
        assert!(unload_chunk(
            &mut interner,
            &mut volume,
            &mut wrapped,
            &mut generated,
            key
        ));
        assert!(wrapped.is_empty());
        assert_eq!(count_body_nodes(interner.inner(), &volume), 0);
        assert_eq!(
            interner.inner().alive_nodes(),
            baseline,
            "第 {cycle} 轮 load → unload 后节点数增长（引用泄漏）"
        );
    }
}

/// 编辑过的块卸载后重生成：同一编辑重放，chunk_content_hash 与卸载前一致。
///
/// 编辑不是生成器的输入（当前没有存档层），所以「重生成」= 同一生成器 +
/// 重放同一编辑；这同时验证 unload 的 dec_ref 没有误伤仍被共享的节点。
#[test]
fn unload_reload_of_edited_chunk_keeps_content_hash() {
    let mut interner = VoxelInterner::new(4 * 1024 * 1024);
    let generator = WorldGenerator::from_seed(0x00C0_FFEE);
    let key = chunk_key(0, 0, 0);
    let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
    let mut wrapped = WrappedBlockCache::default();
    let mut generated = GeneratedBlockCache::default();

    let edit = bevy::math::IVec3::new(3, 4, 5);
    let mut tree = generator.generate_chunk(interner.inner_mut(), key);
    let _ = tree.set(interner.inner_mut(), edit, 9);
    volume.insert_chunk(key, tree);
    let hash_before = chunk_content_hash(
        interner.inner(),
        volume.get_chunk(&key).expect("块必须已插入"),
    );
    let nodes_before = count_body_nodes(interner.inner(), &volume);
    assert!(nodes_before > 0);

    warm_chunk_all_lods(
        &mut interner,
        &volume,
        &mut wrapped,
        &mut generated,
        &generator,
        key,
    );
    assert!(unload_chunk(
        &mut interner,
        &mut volume,
        &mut wrapped,
        &mut generated,
        key
    ));
    assert_eq!(count_body_nodes(interner.inner(), &volume), 0);
    assert!(wrapped.is_empty() && generated.is_empty());

    // 重生成 + 重放编辑 -> 相同内容、相同哈希。
    let mut reloaded = generator.generate_chunk(interner.inner_mut(), key);
    let _ = reloaded.set(interner.inner_mut(), edit, 9);
    volume.insert_chunk(key, reloaded);
    assert_eq!(
        chunk_content_hash(
            interner.inner(),
            volume.get_chunk(&key).expect("重生成块必须已插入"),
        ),
        hash_before,
        "卸载/重生成不得改变编辑后块的内容哈希"
    );
    assert_eq!(
        count_body_nodes(interner.inner(), &volume),
        nodes_before,
        "同内容的可达节点数必须一致"
    );

    // 收尾：本测试的缓存已清空，直接整卷释放。
    let _ = release_body(interner.inner_mut(), &mut volume);
    assert_eq!(count_body_nodes(interner.inner(), &volume), 0);
}
