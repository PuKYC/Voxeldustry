//! SaveNow 世界存盘接线：把「玩家 / 指令触发的全量快照」接到
//! [crate::world::save] 的采集（[capture_world]）与落盘（[write_capture]）。
//!
//! 本模块只做三件事：判断是否触发、按引擎的存档根目录构造 [SavePaths]、
//! 把结果写进 [WorldSaveStatus]。字节格式、region/meta 顺序、原子写都在
//! game_engine::save 与 [crate::world::save]，这里不重复实现、也不绕过。
//!
//! # 触发条件
//!
//! - **手动**：本 tick 读到至少一条 [SaveNow] 消息（玩家 / 指令）。
//! - **自动**：autosave_interval_ticks 到点，且本 tick 还没有存过
//!   （WorldSaveStatus::last_world_tick != Some(当前 tick)）。
//!
//! 一个 tick **最多存一次**：同 tick 同时有手动与自动时只存一次（手动优先）。
//! 手动消息在没有 SaveStore 时会被消费并记进 last_error，不会留到下个 tick
//! 悄悄重放。
//!
//! # autosave 边界语义
//!
//! tick 取 [SaveStore::tick]：由 SaveSet::Flush 在每个 FixedUpdate 先 +1，
//! 所以**第一个 tick 是 1，不是 0**。
//!
//! | 配置 | 行为 |
//! | --- | --- |
//! | None | 关闭自动存档，只响应 [SaveNow] |
//! | Some(0) / Some(1) | **每个 tick 都存**（0 与 1 等价，不会做 tick % 0） |
//! | Some(n)（n >= 2） | tick % n == 0 时存，即 n、2n、3n…… |
//!
//! # 失败
//!
//! 采集 / 写盘失败只 warn! 并写 WorldSaveStatus::last_error：**不 panic**。
//! [write_capture] 先写全部 region、最后写 world.meta，所以失败最多留下
//! 孤儿 region（下次覆盖），不会出现「meta 指向不存在的 region」。
//! 失败时 saves / last_world_tick 都不前进。
//!
//! # 无触发零 IO
//!
//! 没有触发时只读资源（tick / 状态），不建目录、不碰文件、不 stat 路径。
//!
//! # 装配
//!
//!     app.add_plugins(SavePlugin { config: SaveConfig { root, ..Default::default() } });
//!     app.add_plugins(WorldSavePlugin {
//!         config: WorldSaveConfig { generator_version: 1, autosave_interval_ticks: Some(600) },
//!     });
//!
//! WorldSavePlugin 依赖 SaveConfig / SaveStore（game-engine 的 SavePlugin 注册）
//! 与采集所需的 PersistRegistry / OpaqueComponents / IslandRegistry / WorldSeed /
//! VoxelInterner / StableIdAllocator / StableEntityIndex。
//! 建议先装引擎的 SavePlugin 再装本插件：这样 WorldSavePlugin::build 就能拿到
//! 配置好的根目录；即使顺序反了，系统第一次真正存档前也会按 SaveConfig::root
//! 重建 [WorldSavePaths]（只在根目录变化时重建，不丢本会话的 .recovered 状态）。

use bevy::ecs::message::MessageCursor;
use bevy::prelude::*;

use game_engine::residency::GameTickSet;
use game_engine::save::{SaveConfig, SavePaths, SaveSet, SaveStore};

use crate::world::save::{capture_world, write_capture};

/// 世界存盘配置（由装配方插入，也可作为 [WorldSavePlugin] 的构造参数）。
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub struct WorldSaveConfig {
    /// 写进每个 ChunkRecord.generator_version 的生成器版本号。
    pub generator_version: u32,
    /// 自动存档周期（tick）；None = 关闭。Some(0) / Some(1) = 每个 tick。
    ///
    /// tick 来自 [SaveStore::tick]（每个 FixedUpdate +1，首 tick 为 1），
    /// 即 Some(n) 在 n、2n、3n…… 落盘。
    pub autosave_interval_ticks: Option<u64>,
}

impl Default for WorldSaveConfig {
    fn default() -> Self {
        Self {
            generator_version: 1,
            autosave_interval_ticks: None,
        }
    }
}

/// 玩家 / 指令触发的**全量世界快照**请求。
///
/// 每条消息最多触发一次存档；同一 tick 的多条消息合并成一次。
#[derive(Message, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SaveNow;

/// 世界存盘的运行期状态（只由本插件写）。
#[derive(Resource, Debug, Default, PartialEq, Eq)]
pub struct WorldSaveStatus {
    /// 成功落盘的次数。
    pub saves: u64,
    /// 最近一次**成功**落盘时 SaveStore::tick() 的值。
    pub last_world_tick: Option<u64>,
    /// 最近一次失败的原因；成功后清空。
    pub last_error: Option<String>,
}

/// 本插件自己持有的 [SavePaths]（SavePaths 不是 Clone，不从 SaveStore 取）。
///
/// 以 SaveConfig::root 构造；SaveConfig 在 build 时还没出现（装配顺序反了）
/// 就用 SaveConfig::default().root 兜底，系统第一次存档前按真实根目录重建。
#[derive(Resource, Debug)]
pub struct WorldSavePaths(pub SavePaths);

impl WorldSavePaths {
    /// 底层路径对象。
    #[must_use]
    pub fn paths(&self) -> &SavePaths {
        &self.0
    }
}

/// 世界存盘插件：插配置 / 状态 / 路径，注册 [SaveNow] 与 exclusive 存档系统。
#[derive(Clone, Debug, Default)]
pub struct WorldSavePlugin {
    /// 插件配置（同时作为 [WorldSaveConfig] 资源插入）。
    pub config: WorldSaveConfig,
}

impl Plugin for WorldSavePlugin {
    fn build(&self, app: &mut App) {
        let configured_root = app
            .world()
            .get_resource::<SaveConfig>()
            .map(|config| config.root.clone());
        if configured_root.is_none() {
            warn!(
                "WorldSavePlugin：装配时还没有 SaveConfig（建议先 add_plugins(SavePlugin)）；\
                 暂用默认根目录 {:?}，第一次存档前会按 SaveConfig 重建",
                SaveConfig::default().root
            );
        }
        let root = configured_root.unwrap_or_else(|| SaveConfig::default().root);

        app.insert_resource(self.config.clone())
            .init_resource::<WorldSaveStatus>()
            .insert_resource(WorldSavePaths(SavePaths::new(root)))
            .add_message::<SaveNow>()
            // 排在 SaveSet::Poll 之后：先让 Flush 推进 tick、Poll 回收后台结果，
            // 再拿世界做全量采集，避免和存档层的世界访问抢在同一段。
            .add_systems(
                FixedUpdate,
                save_now_system
                    .in_set(GameTickSet::Save)
                    .after(SaveSet::Poll),
            );
    }
}

/// 触发判定：autosave 是否到点（不做 IO）。
fn autosave_due(config: &WorldSaveConfig, tick: u64) -> bool {
    match config.autosave_interval_ticks {
        None => false,
        // 0 与 1 同义：每个 tick；别让 tick % 0 变成 panic。
        Some(interval) if interval <= 1 => true,
        Some(interval) => tick % interval == 0,
    }
}

/// 让 [WorldSavePaths] 跟上 SaveConfig::root。
///
/// 只在根目录变化（或资源被移除）时重建；正常情况下零分配、零 IO。重建会丢掉
/// [SavePaths] 本会话的 .recovered / quarantine 粘性状态，所以只在装配顺序
/// 导致根目录不一致时发生。
fn refresh_paths(world: &mut World) {
    let configured = world
        .get_resource::<SaveConfig>()
        .map(|config| config.root.clone());
    let root = match configured {
        Some(root) => root,
        None => world
            .get_resource::<WorldSavePaths>()
            .map_or_else(
                || SaveConfig::default().root,
                |paths| paths.0.root().to_path_buf(),
            ),
    };
    let stale = world
        .get_resource::<WorldSavePaths>()
        .is_none_or(|paths| paths.0.root() != root.as_path());
    if stale {
        world.insert_resource(WorldSavePaths(SavePaths::new(root)));
    }
}

/// exclusive 存档系统：&mut World 才能跑 [capture_world]。
fn save_now_system(world: &mut World, mut cursor: Local<MessageCursor<SaveNow>>) {
    // 先消费消息：无论后面成功与否，本 tick 的请求都只算一次。
    let requested = {
        let messages = world.resource::<Messages<SaveNow>>();
        cursor.read(messages).count() > 0
    };

    let Some(tick) = world.get_resource::<SaveStore>().map(SaveStore::tick) else {
        if requested {
            record_error(
                world,
                "缺少 SaveStore 资源（game-engine 的 SavePlugin 未装配）",
            );
        }
        return;
    };

    let config = world.resource::<WorldSaveConfig>().clone();
    let last_saved_tick = world
        .get_resource::<WorldSaveStatus>()
        .and_then(|status| status.last_world_tick);
    let autosave_now = autosave_due(&config, tick) && last_saved_tick != Some(tick);
    if !requested && !autosave_now {
        return;
    }

    refresh_paths(world);

    match capture_world(world, tick, config.generator_version) {
        Ok(capture) => {
            let written = {
                let paths = world.resource::<WorldSavePaths>();
                write_capture(&paths.0, &capture)
            };
            match written {
                Ok(()) => record_success(world, tick),
                Err(error) => record_error(world, format!("写盘失败：{error}")),
            }
        }
        Err(error) => record_error(world, format!("采集失败：{error}")),
    }
}

fn record_success(world: &mut World, tick: u64) {
    if let Some(mut status) = world.get_resource_mut::<WorldSaveStatus>() {
        status.saves += 1;
        status.last_world_tick = Some(tick);
        status.last_error = None;
    }
}

fn record_error(world: &mut World, reason: impl Into<String>) {
    let reason = reason.into();
    warn!("世界存档失败：{reason}");
    if let Some(mut status) = world.get_resource_mut::<WorldSaveStatus>() {
        status.last_error = Some(reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use game_engine::body_key::BodyKey;
    use game_engine::identity::{
        StableEntityId, StableEntityIndex, StableIdAllocator, StableIdPlugin,
    };
    use game_engine::math::{FixedPoint, Vec3F};
    use game_engine::residency::ChunkAnchor;
    use game_engine::save::{
        read_region, read_world_meta, OpaqueComponents, Persist, PersistPlugin, PersistRegistry,
        SavePlugin,
    };
    use game_engine::voxel::{
        ChunkKey, MaxDepth, VoxOpsBulkWrite, VoxTree, VoxVolume, VoxelInterner, CHUNK_DEPTH,
    };

    use crate::static_data::voxel::BiomeId;
    use crate::voxel::body::spawn_island_entity;
    use crate::voxel::GameVoxelPlugin;
    use crate::world::{load_world, IslandRegistry, WorldPlugin, WorldSeed};

    const GENERATOR_VERSION: u32 = 7;

    /// std::env::temp_dir 下的唯一子目录；Drop 时整棵删掉（与 world::save 测试一致）。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("系统时钟早于 UNIX_EPOCH")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "ecs_game_save_plugin_{tag}_{}_{nanos}_{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir_all(&path).expect("创建临时目录");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// 真插件装配（不 mock）：采集 / 读档需要的资源都由插件注册。
    fn test_app(root: &Path, autosave: Option<u64>) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.add_plugins(PersistPlugin);
        app.add_plugins(WorldPlugin);
        app.add_plugins(GameVoxelPlugin);
        // 先装引擎存档插件：WorldSavePlugin::build 才能拿到 root。
        app.add_plugins(SavePlugin {
            config: SaveConfig {
                root: root.to_path_buf(),
                ..SaveConfig::default()
            },
        });
        app.add_plugins(WorldSavePlugin {
            config: WorldSaveConfig {
                generator_version: GENERATOR_VERSION,
                autosave_interval_ticks: autosave,
            },
        });
        app.finish();
        app.cleanup();
        app
    }

    /// 手工装配一个只有资源的空世界（读档 / 回读用）。
    fn bare_world(seed: u64) -> World {
        let mut world = World::new();
        world.insert_resource(WorldSeed(seed));
        world.insert_resource(IslandRegistry::new());
        world.insert_resource(StableIdAllocator::default());
        world.insert_resource(StableEntityIndex::default());
        world.insert_resource(VoxelInterner::new(4 * 1024 * 1024));
        world.insert_resource(PersistRegistry::new());
        world.insert_resource(OpaqueComponents::default());
        world
    }

    /// 跑一个 FixedUpdate（等同于一个存档 tick）；SaveSet::Flush 会先推进 tick。
    fn tick(app: &mut App) {
        app.world_mut().run_schedule(FixedUpdate);
    }

    fn insert_filled_chunk(world: &mut World, volume: &mut VoxVolume, key: ChunkKey, value: u8) {
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
        {
            let mut interner = world.resource_mut::<VoxelInterner>();
            tree.fill(interner.inner_mut(), value);
        }
        volume.insert_chunk(key, tree);
    }

    /// 造一个带一个实心块的岛，返回 (body 键, 块键)。
    fn spawn_island_with_chunk(world: &mut World) -> (BodyKey, ChunkKey) {
        let (entity, island_id) = spawn_island_entity(world, Vec3F::ZERO, true, BiomeId(1));
        let chunk = ChunkKey::new(2, 0, 0);
        let mut volume = VoxVolume::new(FixedPoint::from_num(1));
        insert_filled_chunk(world, &mut volume, chunk, 1);
        world.entity_mut(entity).insert(volume);
        (BodyKey::from(island_id), chunk)
    }

    #[test]
    fn save_now_writes_meta_and_regions_that_round_trip() {
        let dir = TempDir::new("round_trip");
        let mut app = test_app(dir.path(), None);
        let (body, chunk) = spawn_island_with_chunk(app.world_mut());

        app.world_mut().write_message(SaveNow);
        tick(&mut app);

        let paths = SavePaths::new(dir.path());
        assert!(paths.world_meta().exists(), "world.meta 必须落盘");
        assert!(
            paths.region(body, chunk).exists(),
            "对应的 region 文件必须落盘：{:?}",
            paths.region(body, chunk)
        );

        let expected = capture_world(app.world_mut(), 1, GENERATOR_VERSION).expect("原世界可采集");
        assert_eq!(
            read_world_meta(&paths).expect("读 meta"),
            Some(expected.meta.clone()),
            "磁盘 meta 必须与原世界的采集结果逐字段相等"
        );
        assert!(!expected.regions.is_empty());
        for ((body, region_key), file) in &expected.regions {
            let anchor = *file.chunks.keys().next().expect("采集不会产生空 region");
            assert_eq!(*region_key, game_engine::save::region_of(anchor));
            assert_eq!(
                read_region(&paths, *body, anchor)
                    .expect("读 region")
                    .as_ref(),
                Some(file),
                "region 内容必须逐字节相等（等价于块内容哈希一致）"
            );
        }

        // 用 load_world 回读，再采集一次：meta 与 region 都必须和原世界一致。
        let mut loaded = bare_world(0);
        let info = load_world(&mut loaded, &paths)
            .expect("读档成功")
            .expect("world.meta 必须存在");
        assert_eq!(info.world_tick, 1);
        assert_eq!(info.stats.bodies, 1);
        assert_eq!(info.stats.entities, 0);

        let recaptured = capture_world(&mut loaded, 1, GENERATOR_VERSION).expect("回读世界可采集");
        assert_eq!(recaptured.meta, expected.meta, "回读后 meta 必须一致");
        assert_eq!(recaptured.regions, expected.regions, "回读后块内容必须一致");
    }

    #[test]
    fn no_trigger_does_no_io() {
        let dir = TempDir::new("no_trigger");
        let mut app = test_app(dir.path(), None);
        spawn_island_with_chunk(app.world_mut());

        for _ in 0..3 {
            tick(&mut app);
        }

        let paths = SavePaths::new(dir.path());
        assert!(!paths.world_meta().exists(), "没有触发就不该有 world.meta");
        assert_eq!(
            fs::read_dir(dir.path()).expect("临时目录存在").count(),
            0,
            "没有触发时目录里不该有任何文件"
        );
        let status = app.world().resource::<WorldSaveStatus>();
        assert_eq!(status.saves, 0);
        assert_eq!(status.last_world_tick, None);
        assert_eq!(status.last_error, None);
    }

    #[test]
    fn two_save_now_messages_save_twice() {
        let dir = TempDir::new("twice");
        let mut app = test_app(dir.path(), None);
        spawn_island_with_chunk(app.world_mut());

        app.world_mut().write_message(SaveNow);
        tick(&mut app);
        {
            let status = app.world().resource::<WorldSaveStatus>();
            assert_eq!(status.saves, 1);
            assert_eq!(status.last_world_tick, Some(1));
            assert_eq!(status.last_error, None);
        }

        app.world_mut().write_message(SaveNow);
        tick(&mut app);
        {
            let status = app.world().resource::<WorldSaveStatus>();
            assert_eq!(status.saves, 2, "第二次 SaveNow 也必须落盘");
            assert_eq!(
                status.last_world_tick,
                Some(2),
                "last_world_tick 必须是最新 tick"
            );
            assert_eq!(status.last_error, None, "第二次不报错");
        }

        let meta = read_world_meta(&SavePaths::new(dir.path()))
            .expect("读 meta")
            .expect("world.meta 必须存在");
        assert_eq!(meta.world_tick, 2, "meta 里的 tick 必须是最后一次");
    }

    #[test]
    fn autosave_fires_on_interval_boundaries() {
        let dir = TempDir::new("autosave");
        let mut app = test_app(dir.path(), Some(3));
        spawn_island_with_chunk(app.world_mut());
        let paths = SavePaths::new(dir.path());

        for tick_index in 1..=7_u64 {
            tick(&mut app);
            let status = app.world().resource::<WorldSaveStatus>();
            let expected_saves = match tick_index {
                1 | 2 => 0,
                3 | 4 | 5 => 1,
                _ => 2,
            };
            assert_eq!(
                status.saves, expected_saves,
                "第 {tick_index} 个 tick 的存档次数不对"
            );
            if expected_saves == 0 {
                assert_eq!(status.last_world_tick, None);
                assert!(
                    !paths.world_meta().exists(),
                    "第 {tick_index} tick 之前不该落盘"
                );
            } else {
                let last_due = (tick_index / 3) * 3;
                assert_eq!(status.last_world_tick, Some(last_due));
                assert_eq!(status.last_error, None);
            }
        }

        let status = app.world().resource::<WorldSaveStatus>();
        assert_eq!(status.saves, 2, "1..=7 个 tick 只在 3、6 落盘");
        let meta = read_world_meta(&paths)
            .expect("读 meta")
            .expect("world.meta 必须存在");
        assert_eq!(meta.world_tick, 6);
    }

    #[test]
    fn autosave_zero_means_every_tick() {
        let dir = TempDir::new("autosave_zero");
        let mut app = test_app(dir.path(), Some(0));
        spawn_island_with_chunk(app.world_mut());

        for tick_index in 1..=4_u64 {
            tick(&mut app);
            let status = app.world().resource::<WorldSaveStatus>();
            assert_eq!(status.saves, tick_index, "Some(0) 必须每个 tick 都存");
            assert_eq!(status.last_world_tick, Some(tick_index));
        }
        assert_eq!(app.world().resource::<WorldSaveStatus>().saves, 4);
        assert!(SavePaths::new(dir.path()).world_meta().exists());
    }

    #[test]
    fn missing_persist_registry_does_not_panic_and_does_not_write() {
        let dir = TempDir::new("missing_registry");
        let mut app = test_app(dir.path(), None);
        app.world_mut().remove_resource::<PersistRegistry>();

        app.world_mut().write_message(SaveNow);
        tick(&mut app);

        let paths = SavePaths::new(dir.path());
        let status = app.world().resource::<WorldSaveStatus>();
        assert_eq!(status.saves, 0, "失败不能计入成功次数");
        assert_eq!(status.last_world_tick, None);
        let error = status
            .last_error
            .as_deref()
            .expect("失败必须留下 last_error");
        assert!(
            error.contains("PersistRegistry"),
            "last_error 要写清缺的是哪个资源：{error}"
        );
        assert!(!paths.world_meta().exists(), "采集失败时不能写 world.meta");
        assert!(
            !paths
                .body_dir(BodyKey::new(StableEntityId(1)))
                .exists(),
            "采集失败时不该建 body 目录"
        );
    }

    #[test]
    fn persisted_entity_round_trips_into_its_body_and_chunk() {
        let dir = TempDir::new("persist_entity");
        let mut app = test_app(dir.path(), None);

        let (body, chunk) = spawn_island_with_chunk(app.world_mut());
        let stable = app
            .world_mut()
            .resource_mut::<StableIdAllocator>()
            .allocate();
        app.world_mut().spawn((
            stable,
            Persist {
                prototype_type_id: 9,
            },
            ChunkAnchor::new(body, chunk),
        ));

        app.world_mut().write_message(SaveNow);
        tick(&mut app);

        // 直接查磁盘：实体必须落在正确的 (body, chunk) 记录里。
        let paths = SavePaths::new(dir.path());
        let file = read_region(&paths, body, chunk)
            .expect("读 region")
            .expect("region 文件必须存在");
        let record = file.chunks.get(&chunk).expect("块记录必须存在");
        let ids: Vec<u64> = record
            .entities
            .iter()
            .map(|e| e.stable_entity_id)
            .collect();
        assert_eq!(ids, vec![stable.0], "实体必须只出现在它自己的块里");
        assert!(record.content.is_some(), "有体素的块必须带 content");
        assert_eq!(record.generator_version, GENERATOR_VERSION);

        // 回读：实体与体素都要恢复到同一个 body / chunk。
        let mut loaded = bare_world(0);
        load_world(&mut loaded, &paths)
            .expect("读档成功")
            .expect("world.meta 必须存在");
        let loaded_entity = loaded
            .resource::<StableEntityIndex>()
            .entity(stable)
            .expect("Persist 实体必须按稳定 ID 恢复");
        assert!(loaded.get::<StableEntityId>(loaded_entity).is_some());

        let body_entity = loaded
            .resource::<StableEntityIndex>()
            .entity(body.stable_id())
            .expect("body 必须恢复");
        let volume = loaded
            .get::<VoxVolume>(body_entity)
            .expect("body 必须带 VoxVolume");
        assert!(volume.contains_chunk(&chunk), "块必须装回原来的 body");
    }
}
