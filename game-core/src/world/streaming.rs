//! 流式世界装配：把「驻留 → 流式 → 世界区块 →（可选）存档」接进 App。
//!
//! ## 为什么由本插件集中定序
//!
//! 三个插件过去各自只声明自己集合**内部**的顺序（ResidencyPlugin 的
//! ResidencySet、VoxelStreamPlugin 的链、WorldChunkPlugin 的 WorldChunkSet），
//! 全仓没有任何 configure_sets 把 [GameTickSet] 的五个变体串起来。于是
//! 「Streaming 在 Residency 之后」只是文档约定，实际执行顺序由调度器决定。
//!
//! 本插件把全局顺序固定成一条单向流水线：
//!
//! 1. [GameTickSet::Residency]：收集 Real/Observer 请求、算目标档位，发
//!    ResidencyChanged；
//! 2. [GameTickSet::Streaming]：消费变化，装块 / 生成 / 等落盘确认后释放，
//!    发 ChunkReached / ChunkWillUnload / ChunkEntities。**卸载消息由这一层发出**，
//!    所以它必须先于世界区块；
//! 3. [GameTickSet::WorldChunk]：按上面的消息维护 region 计数并**销毁实体**。
//!    必须在流式之后，否则同一 tick 的卸载消息要等到下一 tick 才看到，
//!    region 计数与实体归属会滞后一拍；
//! 4. [GameTickSet::Gameplay]：实体归属稳定后才跑 AI / 机器等玩法逻辑；
//! 5. [GameTickSet::Save]：**落盘放在最后**，才能看到本 tick 产生的全部脏块
//!    与卸载删除，不会把刚写进内存的内容漏掉。
//!
//! ## 存档是可选的
//!
//! 有 [SaveRoot] 资源且 App 里还没有 SaveStore 时，安装引擎的 SavePlugin 与
//! game-core 的 WorldSavePlugin；没有 SaveRoot 就什么都不装：ChunkPersistence
//! 保持默认的 NoStore，流式层照常装卸，只是不落盘（headless / 单测）。
//! 已有 SaveStore（例如测试或上层已经装过 SavePlugin）时跳过，避免重复注册
//! 系统与后台线程。
//!
//! ## 装配前提
//!
//! 必须先装 crate::voxel::GameVoxelPlugin；TerrainMode::Streaming 时地形插件
//! 会产出一个空体素 body 作为流式层的容器（Eager 地形也能接，只是 Startup
//! 已经把块生成好，流式层会走「块已在容器」的快路径）。
//!
//! ## 本地玩家 = 观察者（过渡）
//!
//! 玩家实体不是「体」：[LocalPlayer] 只是输入 / 表现标记，它没有自己的
//! [VoxVolume]。因此 [attach_local_player_streaming] 给玩家挂 [Observer]
//! （请求 `Render` 档）和 [BodyRef]（指向唯一地形体），而不是 [Real]：
//! `Real` 表示「需要真实模拟的体」，驻留层会按实体自己的稳定 ID 去找容器，
//! 而玩家没有容器。
//!
//! 位置来源仍是 [LogicPos]（过渡，单位是**体局部米**）：`Transform` 是渲染侧
//! f32 米坐标，不能参与块归属（确定性 L3），[logic_pos_for_transform] 在**体位于
//! 原点**时把它转成定点米并吸附到体素网格。这不是权威位姿：船体 / 多岛落地后
//! 必须改成 `BodyPose` + 世界→体变换，当前实现只支持原点地形体。
//!
//! 渲染半径来自 [StreamingPlayerConfig::render_radius_chunks]；缺
//! [ChunkAnchor] 由 [Observer] 的 `#[require(ChunkAnchor)]` 自动补，本插件
//! 不手写锚点。`IslandRegistry` 不是恰好一个岛时每个玩家只 warn 一次并
//! 跳过，避免把玩家锚到错误的体上。

use std::collections::BTreeSet;

use bevy::prelude::*;
use game_engine::identity::StableEntityId;
use game_engine::math::{FixedPoint, Vec3F};
use game_engine::residency::{
    BodyRef, GameTickSet, LogicPos, Observer, ResidencyPlugin, ResidencySet,
};
use game_engine::save::{SaveConfig, SavePlugin, SaveStore};

use crate::input::control::LocalPlayer;
use crate::static_data::voxel::DEFAULT_VOXEL_SIZE;
use crate::voxel::body::BodyKey;
use crate::voxel::stream::VoxelStreamPlugin;
use crate::voxel::GameVoxelPlugin;
use crate::world::chunk::WorldChunkPlugin;
use crate::world::save_plugin::{SaveRoot, WorldSaveConfig, WorldSavePlugin};
use crate::world::IslandRegistry;

/// 本地玩家作为流式观察者的参数。
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamingPlayerConfig {
    /// 观察者渲染半径（体素块）。默认 2（约 5×5 块、72 m 见方）。
    pub render_radius_chunks: u8,
}

impl Default for StreamingPlayerConfig {
    fn default() -> Self {
        Self {
            render_radius_chunks: 2,
        }
    }
}

/// 把驻留 / 流式 / 世界区块 /（可选）存档管线接进 App。
///
/// 依赖：先装 crate::voxel::GameVoxelPlugin；TerrainMode::Streaming 时由
/// TerrainPlugin 产出空体素 body。
pub struct StreamingWorldPlugin;

impl Plugin for StreamingWorldPlugin {
    fn build(&self, app: &mut App) {
        assert!(
            app.is_plugin_added::<GameVoxelPlugin>(),
            "StreamingWorldPlugin 依赖 GameVoxelPlugin"
        );

        app.init_resource::<StreamingPlayerConfig>();

        // 顺序不能反：WorldChunkPlugin 在 build 里断言 ChunkReached / ChunkWillUnload /
        // ChunkEntities 三条消息已存在，而它们分别由 ResidencyPlugin / VoxelStreamPlugin 注册。
        app.add_plugins(ResidencyPlugin::default());
        app.add_plugins(VoxelStreamPlugin);
        app.add_plugins(WorldChunkPlugin);

        configure_tick_sets(app);

        // 玩家观察者必须在 Anchor 之前补齐 LogicPos / BodyRef / Observer，
        // 否则第一 tick 的 update_anchors 看不到这些组件，请求要滞后一 tick。
        app.add_systems(
            FixedUpdate,
            (attach_local_player_streaming, sync_local_player_logic_pos)
                .chain()
                .in_set(GameTickSet::Residency)
                .before(ResidencySet::Anchor),
        );

        // 存档可选：SaveRoot 是装配方的显式选择；没有它就不碰磁盘。
        let save_root = app
            .world()
            .get_resource::<SaveRoot>()
            .map(|root| root.0.clone());
        let Some(root) = save_root else {
            return;
        };
        if app.world().contains_resource::<SaveStore>() {
            // 已经有存档层（上层或测试自己装的）：再装一遍会重复注册系统与
            // 后台线程，直接跳过。
            debug!("StreamingWorldPlugin：已有 SaveStore，跳过 SavePlugin / WorldSavePlugin");
            return;
        }
        app.add_plugins(SavePlugin {
            config: SaveConfig {
                root,
                ..SaveConfig::default()
            },
        });
        app.add_plugins(WorldSavePlugin {
            config: WorldSaveConfig::default(),
        });
    }
}

/// 固定 [GameTickSet] 的全局顺序：驻留 → 流式 → 世界区块 → 玩法 → 存档。
///
/// 单独抽出来是为了让测试能只装探针系统验证顺序，而不必装配整条管线
/// （整条管线的系统在缺资源时会 panic）。
fn configure_tick_sets(app: &mut App) {
    app.configure_sets(
        FixedUpdate,
        (
            GameTickSet::Residency,
            GameTickSet::Streaming,
            GameTickSet::WorldChunk,
            GameTickSet::Gameplay,
            GameTickSet::Save,
        )
            .chain(),
    );
}

/// 渲染世界坐标（米，f32）→ 体局部 `LogicPos`（定点米）。
///
/// **过渡换算**：当前没有权威 `BodyPose`，只能从 `Transform` 反推；只在「体位于
/// 原点、无旋转 / 缩放」时成立（地形体就是这样），船体 / 多岛必须先做世界→体变换。
///
/// 落点吸附到体素网格（0.45 m）：f32 的 28.8 实际是 28.799999，正好落在块边界
/// （2 × 14.4 m）下方一点，不吸附会被 `chunk_of` 的向下取整算进前一块。f32 只出现
/// 在这一步，块归属仍由 `chunk_of` 用定点整数计算。
fn logic_pos_for_transform(pos: Vec3) -> LogicPos {
    let inv = FixedPoint::from_num(1) / DEFAULT_VOXEL_SIZE;
    let snap = |v: f32| (FixedPoint::from_num(v) * inv).round() * DEFAULT_VOXEL_SIZE;
    LogicPos(Vec3F::new(snap(pos.x), snap(pos.y), snap(pos.z)))
}

/// 给本地玩家补上流式观察者身份（幂等）。
///
/// 只在「实体刚添加、`Transform` 变了、`BodyRef` 缺失或刚变、`Observer`
/// 缺失」时进入；进入后仍逐个检查 Option，已经齐全的组件不会重复写。
/// 地形体不唯一时每个玩家只 `warn!` 一次并跳过，避免把玩家锚到错误的体上。
#[allow(clippy::type_complexity)]
fn attach_local_player_streaming(
    mut commands: Commands,
    config: Res<StreamingPlayerConfig>,
    registry: Res<IslandRegistry>,
    mut warned: Local<BTreeSet<Entity>>,
    players: Query<
        (
            Entity,
            &Transform,
            Option<&StableEntityId>,
            Option<&BodyRef>,
            Option<&Observer>,
        ),
        (
            With<LocalPlayer>,
            Or<(
                Added<LocalPlayer>,
                Changed<Transform>,
                // BodyRef 被写成 INVALID 时也要能修回来；有效值不会再触发。
                Changed<BodyRef>,
                Without<BodyRef>,
                Without<Observer>,
            )>,
        ),
    >,
) {
    // 当前世界只有一个原点地形体；多于一个时无法判断玩家站在哪个体上，
    // 宁可不动也不能瞎锚。
    let terrain_body = if registry.len() == 1 {
        registry.ids().next().map(BodyKey::from)
    } else {
        None
    };
    let Some(terrain_body) = terrain_body else {
        for (entity, ..) in &players {
            if warned.insert(entity) {
                warn!(
                    "streaming: IslandRegistry 有 {} 个岛（需要恰好 1 个地形体），玩家 {entity:?} 暂不挂 Observer/BodyRef",
                    registry.len()
                );
            }
        }
        return;
    };

    for (entity, transform, _stable_id, body_ref, observer) in &players {
        if observer.is_none() {
            commands.entity(entity).insert(Observer {
                radius: config.render_radius_chunks,
            });
        }
        if body_ref.is_none_or(|body| !body.0.is_valid()) {
            commands.entity(entity).insert(BodyRef(terrain_body));
        }
        // 新玩家第一个 tick 就要有正确位置，不能等 Changed<Transform>。
        commands
            .entity(entity)
            .insert(logic_pos_for_transform(transform.translation));
    }
}

/// 玩家 `Transform` 变化后同步 `LogicPos`（体素归属只读定点）。
///
/// 与 [attach_local_player_streaming] 分开：新玩家由 attach 负责第一份
/// `LogicPos`，这里只跟随后续移动。
fn sync_local_player_logic_pos(
    mut players: Query<(&Transform, &mut LogicPos), (With<LocalPlayer>, Changed<Transform>)>,
) {
    for (transform, mut logic) in &mut players {
        logic.0 = logic_pos_for_transform(transform.translation).0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use game_engine::body_key::BodyKey;
    use game_engine::identity::{StableEntityId, StableIdPlugin, StableIdWorldExt};
    use game_engine::residency::{
        BodyRef, ChunkAnchor, ChunkReached, LogicPos, Observer, ResidencyTable,
    };
    use game_engine::save::SaveStore;
    use game_engine::streaming::{ChunkPersistence, GenTable, StoreLookup};
    use game_engine::voxel::{chunk_key, VoxVolume};

    use crate::input::control::LocalPlayer;
    use crate::voxel::stream::{
        ChunkEntities, ChunkWillUnload, PendingUnloads, RenderGroups, StreamStats,
    };
    use crate::voxel::terrain::{
        TerrainConfig, TerrainGroups, TerrainMode, TerrainPlugin, TerrainStats,
    };
    use crate::world::chunk::RegionIndex;
    use crate::world::save_plugin::WorldSavePaths;
    use crate::world::{IslandRegistry, WorldPlugin};

    /// std::env::temp_dir 下的唯一子目录；Drop 时整棵删掉（与 world::save_plugin
    /// 测试里的写法一致）。
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
                "ecs_game_streaming_world_{tag}_{}_{nanos}_{}",
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

    /// 地形测试 App：半径 0 / max_lod 0（9 个基础子块），debug 下也足够便宜。
    ///
    /// 不插 TerrainMode 等价于 Eager；需要 Streaming 时显式插入。
    fn terrain_app(mode: Option<TerrainMode>, with_pipeline: bool) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.add_plugins(WorldPlugin);
        app.add_plugins(GameVoxelPlugin);
        app.insert_resource(TerrainConfig {
            radius_blocks: 0,
            max_lod: 0,
            ..TerrainConfig::default()
        });
        if let Some(mode) = mode {
            app.insert_resource(mode);
        }
        app.add_plugins(TerrainPlugin);
        if with_pipeline {
            app.add_plugins(StreamingWorldPlugin);
        }
        // 只用 app.update() 的测试必须显式 finish，Avian 在 Plugin::finish 注册诊断资源。
        app.finish();
        app.cleanup();
        app
    }

    /// 世界里唯一带 VoxVolume 的地形根实体。
    fn only_volume(app: &mut App) -> &VoxVolume {
        let world = app.world_mut();
        let mut query = world.query_filtered::<Entity, With<VoxVolume>>();
        let entities: Vec<Entity> = query.iter(world).collect();
        assert_eq!(entities.len(), 1, "世界里应恰有一个地形体素实体");
        world
            .get::<VoxVolume>(entities[0])
            .expect("地形根实体必须带 VoxVolume")
    }

    #[test]
    fn streaming_terrain_starts_empty_with_body_identity() {
        let mut app = terrain_app(Some(TerrainMode::Streaming), false);
        app.update();

        // 先拿实体，再读卷：空容器里一个块都没有。
        let world = app.world_mut();
        let mut query = world.query_filtered::<Entity, With<VoxVolume>>();
        let entities: Vec<Entity> = query.iter(world).collect();
        assert_eq!(entities.len(), 1, "世界里应恰有一个地形体素实体");
        let entity = entities[0];
        assert_eq!(
            world
                .get::<VoxVolume>(entity)
                .expect("带 VoxVolume")
                .chunk_count(),
            0,
            "Streaming 地形 Startup 不能生成任何块"
        );
        assert!(
            world.get::<StableEntityId>(entity).is_some(),
            "地形根实体必须有正式 body 身份"
        );
        assert_eq!(
            world.resource::<IslandRegistry>().len(),
            1,
            "岛屿注册表必须有且只有地形这一个岛"
        );

        let stats = *world.resource::<TerrainStats>();
        assert_eq!(stats.chunks, 0);
        assert_eq!(stats.blocks, 0);
        assert_eq!(stats.groups, 0);
        assert_eq!(stats.lod_blocks, [0; 4]);
        assert_eq!(stats.lod_groups, [0; 4]);
        assert_eq!(stats.skipped_empty, 0);
        assert!(world.resource::<TerrainGroups>().is_empty());
    }

    #[test]
    fn eager_terrain_still_generates() {
        // 不插 TerrainMode：缺省 Eager，行为必须与旧路径一致。
        let mut app = terrain_app(None, false);
        app.update();

        assert!(
            only_volume(&mut app).chunk_count() > 0,
            "Eager 地形 Startup 必须照旧生成全部块"
        );
    }

    #[test]
    fn streaming_world_plugin_registers_messages_and_resources() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.add_plugins(GameVoxelPlugin);
        app.add_plugins(StreamingWorldPlugin);

        let world = app.world();
        assert!(world.contains_resource::<Messages<ChunkReached>>());
        assert!(world.contains_resource::<Messages<ChunkWillUnload>>());
        assert!(world.contains_resource::<Messages<ChunkEntities>>());
        assert!(world.contains_resource::<ResidencyTable>());
        assert!(world.contains_resource::<GenTable>());
        assert!(world.contains_resource::<RegionIndex>());
        assert!(world.contains_resource::<PendingUnloads>());

        // 没有 SaveRoot：不能偷偷落盘。
        assert!(
            !world.contains_resource::<SaveStore>(),
            "没有 SaveRoot 时不应安装存档层"
        );
        let lookup = world
            .resource::<ChunkPersistence>()
            .lookup(BodyKey::new(StableEntityId(1)), chunk_key(0, 0, 0));
        assert_eq!(
            lookup,
            StoreLookup::Miss,
            "默认 ChunkPersistence 必须是 Miss"
        );
    }

    #[test]
    fn streaming_world_plugin_installs_save_plugins_with_root() {
        let dir = TempDir::new("save_root");

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.add_plugins(GameVoxelPlugin);
        app.insert_resource(SaveRoot(dir.path().to_path_buf()));
        app.add_plugins(StreamingWorldPlugin);

        {
            let world = app.world();
            let store = world.resource::<SaveStore>();
            assert!(store.has_worker(), "SaveRoot 存在时必须起后台存档线程");
            let paths = world.resource::<WorldSavePaths>();
            assert_eq!(paths.paths().root(), dir.path());
        }

        // 显式停后台线程：App drop 前把 worker join 掉，避免测试进程漏线程。
        app.world_mut()
            .resource_mut::<SaveStore>()
            .shutdown_worker();
        assert!(!app.world().resource::<SaveStore>().has_worker());
    }

    #[derive(Resource, Default)]
    struct Probe(Vec<&'static str>);

    fn probe_residency(mut probe: ResMut<Probe>) {
        probe.0.push("Residency");
    }

    fn probe_streaming(mut probe: ResMut<Probe>) {
        probe.0.push("Streaming");
    }

    fn probe_world_chunk(mut probe: ResMut<Probe>) {
        probe.0.push("WorldChunk");
    }

    fn probe_gameplay(mut probe: ResMut<Probe>) {
        probe.0.push("Gameplay");
    }

    fn probe_save(mut probe: ResMut<Probe>) {
        probe.0.push("Save");
    }

    #[test]
    fn tick_set_order_is_chained() {
        // 只装探针：不装任何会因缺资源 panic 的插件，只验证 configure_sets 本身。
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.init_resource::<Probe>();
        configure_tick_sets(&mut app);

        app.add_systems(FixedUpdate, probe_residency.in_set(GameTickSet::Residency));
        app.add_systems(FixedUpdate, probe_streaming.in_set(GameTickSet::Streaming));
        app.add_systems(
            FixedUpdate,
            probe_world_chunk.in_set(GameTickSet::WorldChunk),
        );
        app.add_systems(FixedUpdate, probe_gameplay.in_set(GameTickSet::Gameplay));
        app.add_systems(FixedUpdate, probe_save.in_set(GameTickSet::Save));

        app.world_mut().run_schedule(FixedUpdate);

        assert_eq!(
            app.world().resource::<Probe>().0,
            vec!["Residency", "Streaming", "WorldChunk", "Gameplay", "Save"],
            "GameTickSet 必须按 驻留→流式→世界区块→玩法→存档 排序"
        );
    }

    #[test]
    fn streaming_mode_does_not_stream_without_requests() {
        let mut app = terrain_app(Some(TerrainMode::Streaming), true);
        // 没有 Real/Observer，就没有驻留请求；旧 LOD 流式系统也不应注册。
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(
            only_volume(&mut app).chunk_count(),
            0,
            "没有请求时流式管线不应偷偷生成任何块"
        );
    }

    #[test]
    fn player_becomes_observer_anchored_to_terrain_body() {
        let mut app = terrain_app(Some(TerrainMode::Streaming), true);
        app.update();

        let (player, _) = app
            .world_mut()
            .spawn_stable((LocalPlayer, Transform::from_xyz(0.0, 0.0, 0.0)));
        app.world_mut().run_schedule(FixedUpdate);

        let terrain_body = {
            let world = app.world();
            let registry = world.resource::<IslandRegistry>();
            assert_eq!(registry.len(), 1, "测试地形必须恰好是一个岛");
            BodyKey::from(registry.ids().next().expect("唯一岛必须有 id"))
        };

        let world = app.world();
        let observer = world
            .get::<Observer>(player)
            .expect("玩家第一 tick 就必须是 Observer");
        assert_eq!(observer.radius, 2, "默认渲染半径来自 StreamingPlayerConfig");
        assert_eq!(
            world.get::<BodyRef>(player).expect("玩家必须挂 BodyRef").0,
            terrain_body,
            "玩家应锚到唯一地形体"
        );
        let anchor = world
            .get::<ChunkAnchor>(player)
            .expect("Observer 的 #[require] 必须补 ChunkAnchor");
        assert!(anchor.is_valid(), "锚点必须已经解析出 body");
        assert_eq!(anchor.body, terrain_body);
        assert_eq!(anchor.chunk, chunk_key(0, 0, 0), "原点玩家在 (0,0,0) 块");

        // 幂等：第二 tick 不再重复插入，锚点也不漂移。
        app.world_mut().run_schedule(FixedUpdate);
        let world = app.world();
        assert_eq!(
            world
                .get::<Observer>(player)
                .expect("Observer 不能消失")
                .radius,
            2
        );
        assert_eq!(
            world.get::<BodyRef>(player).expect("BodyRef 不能消失").0,
            terrain_body
        );
        assert_eq!(
            world
                .get::<ChunkAnchor>(player)
                .expect("ChunkAnchor 不能消失")
                .chunk,
            chunk_key(0, 0, 0),
            "静止玩家第二 tick 的锚点不能变化"
        );
    }

    #[test]
    fn player_requests_load_render_chunks() {
        let mut app = terrain_app(Some(TerrainMode::Streaming), true);
        app.update();

        app.world_mut()
            .spawn_stable((LocalPlayer, Transform::from_xyz(0.0, 0.0, 0.0)));
        // 半径 2 的球约 33 块，流式预算 8 块/tick；留足余量等全部结算。
        for _ in 0..16 {
            app.world_mut().run_schedule(FixedUpdate);
        }

        assert!(
            only_volume(&mut app).chunk_count() > 0,
            "Observer 必须把 Render 请求送进流式层并装块"
        );

        let world = app.world();
        let stats = world.resource::<StreamStats>();
        assert!(stats.generated > 0, "生成路径必须真的产出区块");
        assert!(stats.render_groups > 0, "统计数据也应记录存活分组");

        let groups = world.resource::<RenderGroups>();
        assert!(!groups.0.is_empty(), "RenderGroups 不能为空");
        assert!(
            groups.0.values().any(Option::is_some),
            "至少一个区块应产出实际分组实体（非全空气）"
        );
    }

    #[test]
    fn logic_pos_follows_transform() {
        let mut app = terrain_app(Some(TerrainMode::Streaming), true);
        app.update();

        let (player, _) = app
            .world_mut()
            .spawn_stable((LocalPlayer, Transform::from_xyz(0.0, 0.0, 0.0)));
        app.world_mut().run_schedule(FixedUpdate);

        app.world_mut()
            .entity_mut(player)
            .get_mut::<Transform>()
            .expect("玩家必须有 Transform")
            .translation = Vec3::new(28.8, 0.0, 0.0);
        app.world_mut().run_schedule(FixedUpdate);

        let world = app.world();
        let anchor = world.get::<ChunkAnchor>(player).expect("玩家必须已锚定");
        assert_eq!(anchor.chunk.x, 2, "28.8 m = 64 体素 = 第 2 块（0 起算）");
        let logic = world.get::<LogicPos>(player).expect("玩家必须有 LogicPos");
        // LogicPos 在本仓库按体局部米存放（chunk_of 的块边 = 0.45 * 32 米），
        // 换算回体素应为 64。
        assert_eq!(
            (logic.0.x / DEFAULT_VOXEL_SIZE).round().to_num::<i64>(),
            64,
            "28.8 m / 0.45 m 每体素 = 64 体素"
        );
    }

    #[test]
    fn player_without_terrain_body_is_not_anchored() {
        let mut app = terrain_app(Some(TerrainMode::Streaming), true);
        app.update();
        app.world_mut().resource_mut::<IslandRegistry>().clear();

        let (player, _) = app
            .world_mut()
            .spawn_stable((LocalPlayer, Transform::from_xyz(0.0, 0.0, 0.0)));
        for _ in 0..3 {
            app.world_mut().run_schedule(FixedUpdate);
        }

        let world = app.world();
        assert!(
            world.get::<Observer>(player).is_none(),
            "没有地形体时不能瞎挂 Observer"
        );
        assert!(
            world.get::<BodyRef>(player).is_none(),
            "没有地形体时不能瞎写 BodyRef"
        );
    }
}
