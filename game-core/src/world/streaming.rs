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
//! ## 启动引导：有档不生成
//!
//! Startup 先跑 [bootstrap_streaming_world]，按 [SaveRoot] 决定世界从哪来：
//!
//! - 有 root 且 [load_world] 读到存档：恢复 body / 块 / 实体后**直接返回**，
//!   不再 spawn 空容器。这是「有档不生成」在世界级的唯一落点，读档成功与
//!   新建世界由同一条 match/return 分支保证互斥，不靠标志位猜；
//! - 没有 root、或 world.meta 不存在：spawn 一个空 VoxVolume，块交给后续
//!   驻留 / 流式管线按请求装；
//! - 读档失败分两类：[SaveErrorKind::is_world_rejection] 为真的**身份不符**
//!   （mod / 插件 / 动作表 / 参数 / 版本）走拒绝路径 —— 不建世界、不落盘
//!   （[SaveBlocked]），写 [WorldLoadRejection] 并把 `save.*` 错误码回传
//!   Godot，然后请求退出；损坏 / 截断 / IO 仍然 error! 后按新世界降级，
//!   空容器照建，绝不让坏档把启动卡死。
//!
//! 引导与 [attach_terrain_body] 用 chain 串起来，保证「先恢复或建容器，再挂
//! body 身份」；用 chain 而不是依赖插件添加顺序，是因为 CoreGame 里
//! TerrainPlugin 先于本插件，而 Startup 的系统顺序由插入顺序 + chain 决定。
//!
//! ## 存档是可选的
//!
//! 有 [SaveRoot] 资源且 App 里还没有 SaveStore 时，安装引擎的 SavePlugin 与
//! game-core 的 WorldSavePlugin；没有 SaveRoot 就什么都不装：ChunkPersistence
//! 保持默认的 NoStore，流式层照常装卸，只是不落盘（headless / 单测）。
//! 已有 SaveStore（例如测试或上层已经装过 SavePlugin）时跳过，避免重复注册
//! 系统与后台线程。读档 / 落盘依赖的 PersistPlugin 则由本插件无条件补装
//! （判重），所以即使没有 SaveRoot，读档所需的 PersistRegistry 也在。
//! 装了 SavePlugin 后，流式层的读路径直接用 SaveStore（见 `voxel::stream`
//! 的「持久化来源」）：卸载后重新请求的块从磁盘 / 内存缓冲装回，而不是重新
//! 生成；ChunkPersistence 只在没有 SaveStore 时（headless / 单测 / 测试注入）
//! 才被查询。
//!
//! ## 装配前提
//!
//! 必须先装 crate::voxel::GameVoxelPlugin 与 crate::voxel::terrain::TerrainPlugin
//! （TerrainMode::Streaming）：地形体由本插件的引导系统创建（读档恢复或空容器），
//! TerrainPlugin 不再注册 Startup。Eager 地形也能接，只是 Startup 已经把块
//! 生成好，流式层会走「块已在容器」的快路径。
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
//!
//! ## 世界物体（feature）
//!
//! 本插件补装 [FeaturePlugin]（判重）并注册 [TreeFeature]。注册前用
//! [WorldFeatureRegistry::contains] 判重：`register_world_feature` 撞同一
//! TYPE_ID 会 panic，而树是「世界的默认内容」，重复装配属于调用方错误。
//! 也可由上层先装 FeaturePlugin 并注册自己的物体，本插件只补默认那一棵。

use std::collections::BTreeSet;

use bevy::prelude::*;
use game_engine::backend::{BackendChannels, BevyLifecycleMsg, FromBevy};
use game_engine::identity::StableEntityId;
use game_engine::math::{FixedPoint, Vec3F};
use game_engine::residency::{
    BodyRef, GameTickSet, LogicPos, Observer, ResidencyPlugin, ResidencySet,
};
use game_engine::save::{PersistPlugin, SaveConfig, SaveError, SavePaths, SavePlugin, SaveStore};

use crate::gameplay::Velocity;
use crate::input::control::LocalPlayer;
use crate::input::ReadOnlySession;
use crate::spec::CoreSpec;
use crate::static_data::voxel::DEFAULT_VOXEL_SIZE;
use crate::voxel::body::BodyKey;
use crate::voxel::stream::{VoxelStreamConfig, VoxelStreamPlugin};
use crate::voxel::terrain::{attach_terrain_body, TerrainConfig, TerrainStats, VoxelLodHandle};
use crate::voxel::{
    FeaturePlugin, TreeFeature, WorldFeature, WorldFeatureAppExt, WorldFeatureRegistry,
};
use crate::voxel::{GameVoxelPlugin, VoxVolume};
use crate::world::chunk::WorldChunkPlugin;
use crate::world::load::{
    load_world_with_mode, LoadMode, WorldLoadRejection, WorldLoadRejectionHandle,
};
use crate::world::save::SaveBlocked;
use crate::world::save_plugin::{OrphanPolicy, SaveRoot, WorldSaveConfig, WorldSavePlugin};
use crate::world::seed::WorldSeed;
use crate::world::IslandRegistry;

/// 流式世界的生产自动存档周期（按 [SaveStore] 的固定 tick 计，60 Hz 下 30 s）。
///
/// 这里存的是 **tick 数** 而不是时长：改 `fixed_hz`（Time<Fixed>）会同步改变
/// 实际秒数。生产默认开启自动存档，崩服最多丢一个周期内的编辑。
pub const STREAMING_AUTOSAVE_TICKS: u64 = 1800;

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

/// 标记资源：让「LOD 观察者」位置驱动本地玩家的流式锚点。
///
/// 只由 dev 观察者入口（`TerrainMode::Streaming` + `voxel_viewer`，即
/// `start_bevy_streaming`）插入。原因：`voxel_viewer` 只 spawn 玩家，不带任何
/// 输入位移系统，玩家出生后只受重力；没有本系统时「移动相机」最多改 LOD 档位，
/// 改不了驻留锚点，世界流式永远不会卸载块，也就无法从 Godot 侧验证升降档。
///
/// 真实玩法有自己的控制器写 `Transform`，不该被相机瞬移，所以本资源是显式
/// opt-in，而不是流式模式的默认行为（见 [follow_observer_streaming_player]）。
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObserverDrivenStreamingPlayer;

/// 启动引导的读档结果（诊断 / 测试用）。
///
/// 重进世界要看这里，而不是流的 store-hit 计数：[load_world] 已经把存档块**直接
/// 装进容器**，这些块不经过 `SaveStore::lookup`，所以 `StreamStats::
/// loaded_from_store` 在「刚读档启动」时恒为 0。要区分「从档恢复」与「新世界」，
/// 只能看引导系统自己的结果。
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorldLoadReport {
    /// 是否从存档恢复（world.meta 存在且读档成功）。
    pub loaded_from_save: bool,
    /// 读档恢复的 body 数。
    pub bodies: usize,
    /// 读档读到的块记录数（含 content = None，即需要重新生成的块）。
    pub chunks: usize,
    /// 读档应用的实体记录数。
    pub entities: usize,
    /// 因 generator_version 不匹配而丢弃内容的块数。
    pub stale_chunks: usize,
}

/// 把驻留 / 流式 / 世界区块 /（可选）存档管线接进 App。
///
/// 依赖：先装 crate::voxel::GameVoxelPlugin 与 crate::voxel::terrain::TerrainPlugin
/// （TerrainMode::Streaming）。Startup 引导负责读档或建空地形体，见模块文档
/// 「启动引导：有档不生成」。
pub struct StreamingWorldPlugin;

impl Plugin for StreamingWorldPlugin {
    fn build(&self, app: &mut App) {
        assert!(
            app.is_plugin_added::<GameVoxelPlugin>(),
            "StreamingWorldPlugin 依赖 GameVoxelPlugin"
        );

        // 读档要求 PersistRegistry / OpaqueComponents，落盘也会用它们；WorldSavePlugin
        // 自己不装 PersistPlugin。重复添加会让 register::<Persist>() 撞同一类型 ID
        // 而 panic，所以先判重（上层测试可能已经装过）。
        if !app.is_plugin_added::<PersistPlugin>() {
            app.add_plugins(PersistPlugin);
        }

        app.init_resource::<StreamingPlayerConfig>();
        // 诊断快照读它：重进世界时「是否从档恢复」只有这里知道。
        app.init_resource::<WorldLoadReport>();

        // 顺序不能反：WorldChunkPlugin 在 build 里断言 ChunkReached / ChunkWillUnload /
        // ChunkEntities 三条消息已存在，而它们分别由 ResidencyPlugin / VoxelStreamPlugin 注册。
        app.add_plugins(ResidencyPlugin::default());
        app.add_plugins(VoxelStreamPlugin);
        app.add_plugins(WorldChunkPlugin);

        // 世界物体：先补插件（判重，重复添加只是再 init 一次资源也无害），
        // 再按 TYPE_ID 判重后注册默认的树。上层若已注册同 ID 的自定义物体，
        // 这里不会撞 panic。
        if !app.is_plugin_added::<FeaturePlugin>() {
            app.add_plugins(FeaturePlugin);
        }
        if !app
            .world()
            .resource::<WorldFeatureRegistry>()
            .contains(TreeFeature::TYPE_ID)
        {
            app.register_world_feature::<TreeFeature>();
        }

        configure_tick_sets(app);

        // Streaming 的地形体创建已从 TerrainPlugin 挪到这里：有档读档、无档建空
        // 容器，再由 attach_terrain_body 挂 body 身份。chain 保证顺序，与插件添加
        // 顺序无关（CoreGame 先加 TerrainPlugin）。
        app.add_systems(
            Startup,
            (bootstrap_streaming_world, attach_terrain_body).chain(),
        );

        // 玩家观察者必须在 Anchor 之前补齐 LogicPos / BodyRef / Observer，
        // 否则第一 tick 的 update_anchors 看不到这些组件，请求要滞后一 tick。
        //
        // follow 夹在 attach 与 sync 之间：先补身份，再把外部观察者位置写成
        // Transform，最后让 sync 在同 tick 内把 LogicPos 跟上（否则驻留会晚一 tick
        // 才看到新位置）。follow 只在 [ObserverDrivenStreamingPlayer] 存在时跑。
        app.add_systems(
            FixedUpdate,
            (
                attach_local_player_streaming,
                follow_observer_streaming_player
                    .run_if(resource_exists::<ObserverDrivenStreamingPlayer>),
                sync_local_player_logic_pos,
            )
                .chain()
                .in_set(GameTickSet::Residency)
                .before(ResidencySet::Anchor),
        );
        // 物理侧的速度清零单独成系统（avian 的 LinearVelocity 只在 physics
        // feature 下存在，不能塞进上面的元组）。与 follow 无关的顺序依赖：
        // 它只写速度，不读 Transform。
        #[cfg(feature = "physics")]
        app.add_systems(
            FixedUpdate,
            freeze_observer_streaming_player_velocity
                .run_if(resource_exists::<ObserverDrivenStreamingPlayer>)
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
        // 两部分分开判断：重复装 SavePlugin 会重复注册系统与后台线程；重复装
        // WorldSavePlugin 会同时跑两套存档系统。只判断 SaveStore 的话，
        //「上层自己装了存档层、却没装世界存档」会让世界存档被静默关掉。
        let has_save_layer =
            app.world().contains_resource::<SaveStore>() || app.is_plugin_added::<SavePlugin>();
        let has_world_save = app.is_plugin_added::<WorldSavePlugin>();
        if has_save_layer && has_world_save {
            debug!("StreamingWorldPlugin：已有存档层与世界存档插件，跳过重复装配");
            return;
        }
        // 生产默认开自动存档：崩服最多丢一个周期内的编辑。WorldSavePlugin::build
        // 会把自己的 config 插入成资源、覆盖已有值，所以必须在 add_plugins 之前
        // 先读上层可能插入的 WorldSaveConfig，再原样传回去（允许上层覆盖默认值）。
        let save_config = app
            .world()
            .get_resource::<WorldSaveConfig>()
            .cloned()
            .unwrap_or(WorldSaveConfig {
                autosave_interval_ticks: Some(STREAMING_AUTOSAVE_TICKS),
            });
        // 只读会话：存档层建只读路径（写操作在 store 层直接拒绝），自动存档仍然
        // 会跑，但每次都被闸门挡下，不会产生任何磁盘写入。
        let read_only = app.world().contains_resource::<ReadOnlySession>();
        if !has_save_layer {
            app.add_plugins(SavePlugin {
                config: SaveConfig {
                    root,
                    read_only,
                    ..SaveConfig::default()
                },
            });
        }
        if !has_world_save {
            app.add_plugins(WorldSavePlugin {
                config: save_config,
            });
        }
    }
}

/// 读档成功后的孤儿 region 回收（[OrphanPolicy::Delete] 才真的删）。
///
/// 失败只 warn：清理失败不该把一个已经读好的世界判成启动失败。
fn reclaim_after_load(world: &World, paths: &SavePaths) {
    if paths.is_read_only() {
        debug!("world: 只读会话跳过孤儿 region 回收");
        return;
    }
    if world
        .get_resource::<OrphanPolicy>()
        .copied()
        .unwrap_or_default()
        != OrphanPolicy::Delete
    {
        return;
    }
    match crate::world::load::reclaim_orphan_regions(paths) {
        Ok(stats) if stats.bodies > 0 => {
            info!(
                "world: 回收孤儿 body {} 个、region {} 个",
                stats.bodies, stats.regions
            );
        }
        Ok(_) => debug!("world: 没有孤儿 region"),
        Err(error) => warn!("world: 孤儿 region 回收失败：{error}"),
    }
}

/// Streaming 模式的 Startup 引导：有存档就读档，没有就建空地形体。
///
/// 读档成功时**直接 return**：世界里已经有存档恢复出来的 body / 块，不能再
/// spawn 空容器，否则会出现一个空地形体外加一个读回来的地形体。无 SaveRoot、
/// world.meta 不存在、**损坏**档三种情况共用下面同一条新建路径；损坏档只降级
/// （error!）不 panic，避免玩家被一份坏档永久卡在启动。身份不符的档则相反：
/// 走 [reject_world_load] 拒绝并返回，既不建新世界也不覆盖原档。
///
/// 这里必须能在没有任何 SaveStore 时运行（无 SaveRoot 的 headless / 单测），
/// 所以只按 SaveRoot 决定是否碰磁盘，不无条件取 SaveStore。
fn bootstrap_streaming_world(world: &mut World) {
    let read_only = world.contains_resource::<ReadOnlySession>();
    let save_root = world.get_resource::<SaveRoot>().map(|root| root.0.clone());
    if let Some(root) = save_root {
        let paths = if read_only {
            SavePaths::read_only(root)
        } else {
            SavePaths::new(root)
        };
        // 生成器版本的唯一来源是流式层配置。这里是 exclusive &mut World，
        // 先把值拷出来再传，免得 load_world 的 &mut 借用与资源借用重叠。
        let generator_version = world.resource::<VoxelStreamConfig>().generator_version;
        let mode = if read_only {
            LoadMode::ReadOnly
        } else {
            LoadMode::Normal
        };
        match load_world_with_mode(world, &paths, generator_version, mode) {
            Ok(Some(loaded)) => {
                info!(
                    "world: 读档完成（tick={} bodies={} chunks={} stale_chunks={} entities={} orphan_regions={}），跳过空地形",
                    loaded.world_tick,
                    loaded.stats.bodies,
                    loaded.stats.chunks,
                    loaded.stats.stale_chunks,
                    loaded.stats.entities,
                    loaded.stats.orphan_regions,
                );
                // 诊断用：重进世界要先看「是否从档恢复、恢复了多少块」。流的
                // store-hit 计数在这里恒为 0（load_world 直接装容器），不能拿它
                // 判断存档有没有生效。
                world.insert_resource(WorldLoadReport {
                    loaded_from_save: true,
                    bodies: loaded.stats.bodies,
                    chunks: loaded.stats.chunks,
                    entities: loaded.stats.entities,
                    stale_chunks: loaded.stats.stale_chunks,
                });
                // 孤儿回收放在读档成功之后：坏档旁边可能还有能手工抢救的目录，
                // 读不动就什么都别删。默认策略是 Keep，删除必须显式开启。
                reclaim_after_load(world, &paths);
                // 有档不生成：load_world 已经把 body 与块装回世界，这里必须结束，
                // 不能再落到下面的空容器分支。
                return;
            }
            Ok(None) => {
                info!("world: 没有存档，按新世界启动");
                if read_only {
                    // 只读入口明确指向一个不存在 / 没有 world.meta 的目录：
                    // 建一个空世界会让人误以为「档是空的」，直接不生成。
                    warn!("world: 只读会话没有找到 world.meta，保持空世界");
                    return;
                }
            }
            Err(error) if read_only || error.kind().is_world_rejection() => {
                // 身份不符（mod / 插件 / 动作表 / 参数 / 版本）：档本身是好的，
                // 只是与当前装配不同。这里既不建新世界也不落盘，原档原样留给玩家。
                // 只读会话对任何读档错误都走这条路：只读绝不降级新建世界。
                reject_world_load(world, &error);
                return;
            }
            Err(error) => {
                // 损坏 / 截断 / IO：玩家往往只能靠删档或让世界重建恢复，保持
                // 原来的降级语义（error! 后落到下面的新建世界路径）。
                error!("world: 读档失败，按新世界启动：{error}");
            }
        }
    }

    // 新世界的种子取自 TerrainConfig.seed：Eager 模式用它生成地形，流式模式的
    // 生成（stream.rs）、世界物体、存档 meta.seed 读的却是 WorldSeed，不同步的话
    // 同一个配置在两种模式下会长出两个不同的世界，而 TerrainConfig.seed 被静默忽略。
    // 读档路径已在上面 return，存档里的种子优先，绝不会被这里覆盖。
    if let Some(seed) = world
        .get_resource::<TerrainConfig>()
        .map(|config| config.seed)
    {
        if world.resource::<WorldSeed>().0 != seed {
            info!("world: 新世界种子 {seed:#x}（来自 TerrainConfig.seed）");
            world.insert_resource(WorldSeed(seed));
        }
    }

    // 新建世界：只出空容器，块由驻留 / 流式管线按请求装；body 身份交给紧随其后的
    // attach_terrain_body。TerrainStats 由 TerrainPlugin 注册；缺资源时跳过，避免
    // 引导系统依赖插件注册的先后细节。
    if let Some(mut stats) = world.get_resource_mut::<TerrainStats>() {
        *stats = TerrainStats::default();
    }
    let volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
    // physics：与 terrain::build_terrain 的 spawn 形状一致（原 Streaming Startup
    // 的空容器也挂 VoxelCollider），块内容到达后由体素/物理层按脏块更新。
    #[cfg(feature = "physics")]
    world.spawn((volume, game_engine::voxel::VoxelCollider));
    #[cfg(not(feature = "physics"))]
    world.spawn(volume);
}

/// 世界身份不符（mod / 插件 / 动作表 / 参数 / 版本）：**拒绝**读档。
///
/// 档本身能读，只是与当前装配不同（少装 / 多装 / 版本不同），属于配置问题而
/// 不是损坏问题：静默新建一个空世界会在第一次自动存档时把玩家还想用的档盖掉。
/// 所以这里只做三件事：留诊断（[WorldLoadRejection] + 跨线程句柄）→ 回传
/// [BevyLifecycleMsg::Error] 给 Godot → 请求退出主循环。不 spawn 体素容器、
/// 不写 [WorldLoadReport]、不覆盖 [WorldSeed]，世界保持「拒绝后的空壳」，
/// 由 [SaveBlocked] 保证它永远不会落盘。
///
/// 退出用 Bevy 的 [AppExit]（error 码 1）：runner 每帧 `update()` 之后
/// `should_exit()` 就能读到，和「窗口关闭」走同一条路径，不必给后端新增资源。
/// headless 单测里没有 `Messages<AppExit>` 时 `write_message` 返回 None，
/// 不会 panic。
fn reject_world_load(world: &mut World, error: &SaveError) {
    let rejection = WorldLoadRejection {
        code: error.kind().code().to_string(),
        path: error.path().to_string(),
        message: error.to_string(),
    };
    error!(
        "world: 读档被拒绝 {} @ {}：{}（保留原档，不建新世界）",
        rejection.code, rejection.path, rejection.message
    );
    if let Some(handle) = world.get_resource::<WorldLoadRejectionHandle>() {
        handle.set(rejection.clone());
    }
    // Godot 侧从后台线程的共享状态读它渲染差异面板；headless 没有这个资源，
    // 拿不到就安静跳过。消息发不出去（接收端已丢）也不影响本地诊断。
    if let Some(channels) = world.get_resource::<BackendChannels<CoreSpec>>() {
        let _ = channels.life_tx.send(FromBevy {
            session: channels.session,
            payload: BevyLifecycleMsg::Error {
                code: rejection.code.clone(),
                message: rejection.message.clone(),
            },
        });
    }
    world.insert_resource(rejection);
    world.insert_resource(SaveBlocked);
    world.write_message(AppExit::error());
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

/// 位姿刚变过的本地玩家：同步 `LogicPos` 的目标集合。
type MovedLocalPlayerQuery<'w, 's> = Query<
    'w,
    's,
    (&'static Transform, &'static mut LogicPos),
    (With<LocalPlayer>, Changed<Transform>),
>;

/// 玩家 `Transform` 变化后同步 `LogicPos`（体素归属只读定点）。
///
/// 与 [attach_local_player_streaming] 分开：新玩家由 attach 负责第一份
/// `LogicPos`，这里只跟随后续移动。
fn sync_local_player_logic_pos(mut players: MovedLocalPlayerQuery) {
    for (transform, mut logic) in &mut players {
        logic.0 = logic_pos_for_transform(transform.translation).0;
    }
}

/// 把 [VoxelLodHandle] 里的观察者位置写成玩家 `Transform`。
///
/// 只在 [ObserverDrivenStreamingPlayer] 存在时注册（显式 opt-in）。observer 为
/// `None`（Godot 还没推过位置）时不动，避免把一个正常走动的玩家吸到原点。
///
/// 写 `Transform` 等价于瞬移：avian 的 `transform_to_position` 会把
/// GlobalTransform 的变化写回 `Position`。同时清掉玩法速度，免得瞬移前后
/// 的重力 / 碰撞速度累积成「起飞」。
fn follow_observer_streaming_player(
    lod: Option<Res<VoxelLodHandle>>,
    mut players: Query<(&mut Transform, Option<&mut Velocity>), With<LocalPlayer>>,
) {
    // 锁只在这一步持有：Godot 主线程写 observer 时会短暂争用，不能跨系统持有。
    let Some([x, y, z]) = lod.as_deref().and_then(observer_position) else {
        return;
    };
    let target = Vec3::new(x, y, z);
    for (mut transform, velocity) in &mut players {
        transform.translation = target;
        if let Some(mut velocity) = velocity {
            velocity.linear = Vec3::ZERO;
        }
    }
}

/// 读当前 observer 覆写值（[VoxelLodHandle] 内部锁，poison 用 into_inner）。
fn observer_position(handle: &VoxelLodHandle) -> Option<[f32; 3]> {
    match handle.0.lock() {
        Ok(runtime) => runtime.observer,
        Err(poisoned) => poisoned.into_inner().observer,
    }
}

/// 物理侧：观察者驱动的玩家不参与重力 / 碰撞速度累积（只清速度，不碰位置）。
///
/// 只在 observer 真的推过位置时清速度：没被观察者钉住的玩家仍要走正常的
/// 重力 / 碰撞（否则玩家会一直浮在出生点上方，连脚下的方块都够不到）。
#[cfg(feature = "physics")]
fn freeze_observer_streaming_player_velocity(
    lod: Option<Res<VoxelLodHandle>>,
    mut players: Query<&mut game_engine::voxel::LinearVelocity, With<LocalPlayer>>,
) {
    if lod.as_deref().and_then(observer_position).is_none() {
        return;
    }
    for mut velocity in &mut players {
        velocity.0 = Vec3::ZERO;
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
    use game_engine::save::{SaveErrorKind, SavePaths, SaveStore};
    use game_engine::streaming::{ChunkPersistence, GenTable, StoreLookup};
    use game_engine::voxel::{
        chunk_key, MaxDepth, VoxOpsBulkWrite, VoxTree, VoxVolume, VoxelBox, VoxelDirtySet,
        VoxelInterner, CHUNK_DEPTH,
    };

    use crate::input::control::LocalPlayer;
    use crate::voxel::stream::{
        ChunkEntities, ChunkWillUnload, PendingUnloads, RenderGroups, StreamStats,
        VoxelStreamConfig,
    };
    use crate::voxel::terrain::{
        TerrainConfig, TerrainGroups, TerrainMode, TerrainPlugin, TerrainStats,
    };
    use crate::world::chunk::RegionIndex;
    use crate::world::save_plugin::{
        OrphanPolicy, SaveNow, SaveRoot, WorldSaveConfig, WorldSavePaths, WorldSaveStatus,
    };
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
        terrain_app_with_root(mode, with_pipeline, None)
    }

    /// 同 terrain_app，但可在 StreamingWorldPlugin 之前插入 SaveRoot。
    ///
    /// root 必须在 add_plugins(StreamingWorldPlugin) 之前进入世界：插件的 build
    /// 只有那时才能决定是否安装 SavePlugin / WorldSavePlugin。
    fn terrain_app_with_root(
        mode: Option<TerrainMode>,
        with_pipeline: bool,
        root: Option<&Path>,
    ) -> App {
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
        if let Some(root) = root {
            app.insert_resource(SaveRoot(root.to_path_buf()));
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
    fn streaming_boot_without_save_creates_empty_terrain() {
        let mut app = terrain_app(Some(TerrainMode::Streaming), true);
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

    /// 真插件装配：Streaming 引导建空容器 -> 手工塞一个实心块 -> SaveNow 落盘。
    ///
    /// 返回第一 App 地形体的稳定 ID，供第二次启动比对。走 SaveNow 的真实路径
    /// （而不是直接 capture_world），同时覆盖「写盘」与「启动读档」两端。
    fn boot_and_save_one_chunk(root: &Path) -> StableEntityId {
        let mut app = terrain_app_with_root(Some(TerrainMode::Streaming), true, Some(root));
        app.update();

        // 实心块不依赖世界生成器的地表高度，块内容一定非空（空树可能被存成 None）。
        let chunk = chunk_key(0, 0, 0);
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
        {
            let mut interner = app.world_mut().resource_mut::<VoxelInterner>();
            tree.fill(interner.inner_mut(), 1);
        }
        let body = {
            let world = app.world_mut();
            let mut query = world.query_filtered::<Entity, With<VoxVolume>>();
            let entity = query.iter(world).next().expect("引导后应有地形体");
            let body = world
                .get::<StableEntityId>(entity)
                .copied()
                .expect("地形体必须有稳定 ID");
            world
                .get_mut::<VoxVolume>(entity)
                .expect("地形体必须带 VoxVolume")
                .insert_chunk(chunk, tree);
            // 夹具绕过 set_voxel_for 直接插块，补标编辑后 SaveNow 才会写 content。
            world.resource_mut::<VoxelDirtySet>().mark_edited_for(
                Some(BodyKey::from(body)),
                chunk,
                VoxelBox::FULL,
            );
            body
        };

        app.world_mut().write_message(SaveNow);
        app.world_mut().run_schedule(FixedUpdate);
        let status = app.world().resource::<WorldSaveStatus>();
        assert!(
            status.last_error.is_none(),
            "SaveNow 落盘失败：{:?}",
            status.last_error
        );
        assert_eq!(status.saves, 1, "SaveNow 必须成功落盘一次");
        assert!(
            SavePaths::new(root).world_meta().exists(),
            "world.meta 必须落盘"
        );

        app.world_mut()
            .resource_mut::<SaveStore>()
            .shutdown_worker();
        body
    }

    #[test]
    fn streaming_boot_seeds_new_world_from_terrain_config() {
        let mut app = terrain_app(Some(TerrainMode::Streaming), true);
        app.insert_resource(TerrainConfig {
            seed: 0x5EED,
            ..TerrainConfig::default()
        });
        app.update();

        assert_eq!(
            app.world().resource::<WorldSeed>().0,
            0x5EED,
            "新世界的种子必须来自 TerrainConfig.seed，否则流式生成与配置脱节"
        );
    }

    #[test]
    fn streaming_boot_keeps_saved_seed_over_config() {
        let dir = TempDir::new("boot_seed");
        // 第一 App 用默认 TerrainConfig.seed（0x00C0_FFEE）落盘，meta.seed 记的就是它。
        let saved_seed = terrain_app(Some(TerrainMode::Streaming), false)
            .world()
            .resource::<TerrainConfig>()
            .seed;
        boot_and_save_one_chunk(dir.path());

        // 第二次启动故意换个配置种子：存档里的 seed 必须优先。
        let mut app = terrain_app_with_root(Some(TerrainMode::Streaming), true, Some(dir.path()));
        app.insert_resource(TerrainConfig {
            seed: 0x1234,
            ..TerrainConfig::default()
        });
        app.update();

        assert_eq!(
            app.world().resource::<WorldSeed>().0,
            saved_seed,
            "读档时 WorldSeed 必须来自 world.meta，不能被 TerrainConfig.seed 覆盖"
        );
        app.world_mut()
            .resource_mut::<SaveStore>()
            .shutdown_worker();
    }

    #[test]
    fn streaming_boot_with_save_loads_world_and_skips_empty_terrain() {
        let dir = TempDir::new("boot_load");
        let saved_body = boot_and_save_one_chunk(dir.path());

        let mut app = terrain_app_with_root(Some(TerrainMode::Streaming), true, Some(dir.path()));
        app.update();

        {
            let world = app.world_mut();
            let mut query = world.query_filtered::<Entity, With<VoxVolume>>();
            let entities: Vec<Entity> = query.iter(world).collect();
            assert_eq!(entities.len(), 1, "读档成功后世界里不能多出一个空地形体");
            let entity = entities[0];
            assert_eq!(
                world.get::<StableEntityId>(entity).copied(),
                Some(saved_body),
                "读档必须恢复存档里的 body 身份"
            );
            assert_eq!(
                world
                    .get::<VoxVolume>(entity)
                    .expect("带 VoxVolume")
                    .chunk_count(),
                1,
                "存档里的块数必须恢复"
            );
            assert_eq!(
                world.resource::<IslandRegistry>().len(),
                1,
                "岛屿注册表必须只有存档里的地形这一个岛"
            );
        }
        app.world_mut()
            .resource_mut::<SaveStore>()
            .shutdown_worker();
    }

    /// 在存档根下造一个 meta 里没有的 body 目录（模拟崩溃残留 / WipeWorld 之前的旧档）。
    fn plant_orphan_body(root: &Path, id: u64) -> PathBuf {
        let dir = SavePaths::new(root).body_dir(BodyKey::new(StableEntityId(id)));
        std::fs::create_dir_all(&dir).expect("创建孤儿目录");
        std::fs::write(dir.join("region_0_0_0"), b"junk").expect("写孤儿 region");
        dir
    }

    #[test]
    fn orphan_body_dir_survives_default_policy() {
        let dir = TempDir::new("orphan_keep");
        let saved = boot_and_save_one_chunk(dir.path());
        let orphan = plant_orphan_body(dir.path(), saved.0 + 100);

        let mut app = terrain_app_with_root(Some(TerrainMode::Streaming), true, Some(dir.path()));
        app.update();

        assert!(
            orphan.exists(),
            "默认策略是 Keep：删除玩家数据必须由装配方显式开启"
        );
        assert!(
            SavePaths::new(dir.path())
                .body_dir(BodyKey::from(saved))
                .exists(),
            "存档里活着的 body 目录当然也要在"
        );
        app.world_mut()
            .resource_mut::<SaveStore>()
            .shutdown_worker();
    }

    #[test]
    fn orphan_policy_delete_reclaims_after_successful_load() {
        let dir = TempDir::new("orphan_delete");
        let saved = boot_and_save_one_chunk(dir.path());
        let orphan = plant_orphan_body(dir.path(), saved.0 + 100);

        let mut app = terrain_app_with_root(Some(TerrainMode::Streaming), true, Some(dir.path()));
        // Startups 在第一次 update 才跑，build 之后再插也来得及。
        app.insert_resource(OrphanPolicy::Delete);
        app.update();

        assert!(!orphan.exists(), "Delete 策略下孤儿 body 目录必须被回收");
        assert!(
            SavePaths::new(dir.path())
                .body_dir(BodyKey::from(saved))
                .exists(),
            "回收只能删 meta 里没有的 body，活着的 body 一个文件都不能丢"
        );
        assert_eq!(only_volume(&mut app).chunk_count(), 1, "读档本身不受影响");
        app.world_mut()
            .resource_mut::<SaveStore>()
            .shutdown_worker();
    }

    #[test]
    fn streaming_boot_with_bumped_version_starts_empty() {
        let dir = TempDir::new("boot_version_bump");
        let saved_body = boot_and_save_one_chunk(dir.path());

        let mut app = terrain_app_with_root(Some(TerrainMode::Streaming), true, Some(dir.path()));
        // 必须在第一次 update（即 Startup）之前生效：读档在 bootstrap 系统里
        // 用这个资源决定是否安装块内容。
        app.insert_resource(VoxelStreamConfig {
            generator_version: 2,
            ..VoxelStreamConfig::default()
        });
        app.update();

        // 旧版本的块内容被丢弃，但记录/实体还在，所以仍只有一个地形体。
        let volume = only_volume(&mut app);
        assert_eq!(
            volume.chunk_count(),
            0,
            "版本不匹配的旧内容不能在启动时被安装"
        );

        {
            let world = app.world_mut();
            let mut query = world.query_filtered::<Entity, With<VoxVolume>>();
            let entities: Vec<Entity> = query.iter(world).collect();
            assert_eq!(entities.len(), 1, "世界里仍应恰有一个地形体素实体");
            assert_eq!(
                world.get::<StableEntityId>(entities[0]).copied(),
                Some(saved_body),
                "实体记录必须保留（丢的是内容不是记录）"
            );
            assert_eq!(
                world.resource::<IslandRegistry>().len(),
                1,
                "岛屿注册表仍只有存档里的地形这一个岛"
            );
        }

        app.world_mut()
            .resource_mut::<SaveStore>()
            .shutdown_worker();
    }

    #[test]
    fn streaming_boot_with_corrupt_save_falls_back_to_empty_terrain() {
        let dir = TempDir::new("boot_corrupt");
        let _ = boot_and_save_one_chunk(dir.path());

        // 尾部翻一位：world.meta 解码失败，read_with_recovery 会先备份 .corrupt，
        // 再把错误交给引导系统降级，而不是当成「没有存档」。
        let meta = SavePaths::new(dir.path()).world_meta();
        let mut bytes = std::fs::read(&meta).expect("读 world.meta");
        assert!(bytes.len() > 2, "world.meta 不应为空");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&meta, bytes).expect("写坏 world.meta");

        let mut app = terrain_app_with_root(Some(TerrainMode::Streaming), true, Some(dir.path()));
        app.update();

        {
            let world = app.world_mut();
            let mut query = world.query_filtered::<Entity, With<VoxVolume>>();
            let entities: Vec<Entity> = query.iter(world).collect();
            assert_eq!(entities.len(), 1, "坏档降级后应恰有一个空地形体");
            assert_eq!(
                world
                    .get::<VoxVolume>(entities[0])
                    .expect("带 VoxVolume")
                    .chunk_count(),
                0,
                "坏档降级必须从空地形开始"
            );
            assert_eq!(
                world.resource::<IslandRegistry>().len(),
                1,
                "坏档降级后岛屿注册表必须有且只有一个地形岛"
            );
        }
        let paths = SavePaths::new(dir.path());
        assert!(
            paths.corrupt(&meta).exists(),
            "坏档必须被备份成 .corrupt：{:?}",
            paths.corrupt(&meta)
        );
        app.world_mut()
            .resource_mut::<SaveStore>()
            .shutdown_worker();
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

        // 世界物体：默认注册了那棵树，注册表必须带它的 TYPE_ID。
        let features = world.resource::<WorldFeatureRegistry>();
        assert!(
            features.contains(TreeFeature::TYPE_ID),
            "StreamingWorldPlugin 应注册默认的 TreeFeature"
        );
        assert_eq!(features.len(), 1, "默认只注册树一个世界物体");
    }

    #[test]
    fn streaming_world_plugin_does_not_reregister_tree() {
        // 上层先装 FeaturePlugin 并注册树，本插件再加一次不能 panic
        // （register_world_feature 撞 TYPE_ID 会 panic，所以必须先判重）。
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.add_plugins(GameVoxelPlugin);
        app.add_plugins(FeaturePlugin);
        app.register_world_feature::<TreeFeature>();
        app.add_plugins(StreamingWorldPlugin);

        let features = app.world().resource::<WorldFeatureRegistry>();
        assert_eq!(features.len(), 1);
        assert!(features.contains(TreeFeature::TYPE_ID));
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

    /// 上层只装了存档层（没有世界存档插件）时，世界存档仍必须补上：
    /// 否则退出/自动存档被静默关掉，编辑只在卸载时才落盘。
    #[test]
    fn streaming_install_adds_world_save_when_only_store_exists() {
        let dir = TempDir::new("save_store_only");

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.add_plugins(GameVoxelPlugin);
        app.insert_resource(SaveRoot(dir.path().to_path_buf()));
        app.add_plugins(SavePlugin {
            config: SaveConfig {
                root: dir.path().to_path_buf(),
                ..SaveConfig::default()
            },
        });
        assert!(!app.is_plugin_added::<WorldSavePlugin>());

        app.add_plugins(StreamingWorldPlugin);

        assert!(
            app.is_plugin_added::<WorldSavePlugin>(),
            "已有存档层但缺世界存档插件时，必须补装 WorldSavePlugin"
        );
        assert_eq!(
            app.world()
                .resource::<WorldSaveConfig>()
                .autosave_interval_ticks,
            Some(STREAMING_AUTOSAVE_TICKS)
        );

        app.world_mut()
            .resource_mut::<SaveStore>()
            .shutdown_worker();
    }

    #[test]
    fn streaming_install_enables_autosave() {
        let dir = TempDir::new("autosave_default");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.add_plugins(GameVoxelPlugin);
        app.insert_resource(SaveRoot(dir.path().to_path_buf()));
        app.add_plugins(StreamingWorldPlugin);

        assert_eq!(
            app.world()
                .resource::<WorldSaveConfig>()
                .autosave_interval_ticks,
            Some(STREAMING_AUTOSAVE_TICKS),
            "生产装配必须默认开启自动存档"
        );

        // 显式停后台线程：App drop 前把 worker join 掉，避免测试进程漏线程。
        app.world_mut()
            .resource_mut::<SaveStore>()
            .shutdown_worker();
    }

    #[test]
    fn streaming_install_keeps_custom_autosave_config() {
        let dir = TempDir::new("autosave_custom");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.add_plugins(GameVoxelPlugin);
        app.insert_resource(SaveRoot(dir.path().to_path_buf()));
        // 上层显式给出的配置必须先于插件 build 进入世界。
        app.insert_resource(WorldSaveConfig {
            autosave_interval_ticks: Some(7),
        });
        app.add_plugins(StreamingWorldPlugin);

        assert_eq!(
            app.world()
                .resource::<WorldSaveConfig>()
                .autosave_interval_ticks,
            Some(7),
            "上层显式配置必须优先于生产默认值"
        );

        app.world_mut()
            .resource_mut::<SaveStore>()
            .shutdown_worker();
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
            groups.0.values().all(|entity| world
                .get::<crate::presentation::VoxChunkGroupRaw>(*entity)
                .is_some()),
            "每个到达 Render 的块都必须有存活的分组实体（全空气块也有 0 矩形载荷）"
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

    #[test]
    fn readonly_boot_rejects_unreadable_save_instead_of_building_new_world() {
        let dir = TempDir::new("readonly_boot");
        std::fs::write(dir.path().join("world.meta"), b"not a world").expect("写坏档");

        let mut app = terrain_app_with_root(Some(TerrainMode::Streaming), true, Some(dir.path()));
        // 装配完成后才插：只读路径必须看资源，而不是看 SavePlugin 有没有先建好。
        app.insert_resource(ReadOnlySession);
        app.update();

        assert!(
            app.world().contains_resource::<SaveBlocked>(),
            "只读读档失败也必须装存档闸门"
        );
        assert!(
            !app.world().resource::<WorldLoadReport>().loaded_from_save,
            "只读读档失败不能写成「已从档恢复」"
        );
        let world = app.world_mut();
        let mut query = world.query_filtered::<Entity, With<VoxVolume>>();
        assert_eq!(
            query.iter(world).count(),
            0,
            "只读会话读档失败绝不能降级新建世界"
        );
    }

    /// 身份不符的档被拒绝：只留诊断 + 闸门 + 退出请求，不建世界。
    ///
    /// 这里直接调 reject_world_load（bootstrap 的 Err 分支只多一层 match）：
    /// 断言拒绝路径不 spawn VoxVolume、不写 WorldLoadReport、不动 WorldSeed，
    /// 且 SaveBlocked 与 AppExit 都就位。没有 BackendChannels 时必须安静跳过
    /// （headless 单测就是这种形态），不能 panic。
    #[test]
    fn world_rejection_keeps_world_empty_and_blocks_saves() {
        let mut world = World::new();
        world.init_resource::<Messages<AppExit>>();
        world.insert_resource(WorldSeed(0x1234));
        let error = SaveError::semantic(
            "load.meta.mods",
            SaveErrorKind::MissingMod {
                id: "core".to_string(),
            },
        );

        reject_world_load(&mut world, &error);

        let rejection = world.resource::<WorldLoadRejection>().clone();
        assert_eq!(rejection.code, "save.missing_mod");
        assert_eq!(rejection.path, "load.meta.mods");
        assert!(
            rejection.message.contains("存档加载过 mod"),
            "message 必须是 SaveError 的 Display 原文：{}",
            rejection.message
        );
        assert!(
            world.contains_resource::<SaveBlocked>(),
            "拒绝后必须装上存档闸门"
        );
        assert!(
            !world.contains_resource::<WorldLoadReport>(),
            "拒绝路径不能写 WorldLoadReport（它会被当成「已从档恢复」）"
        );
        assert_eq!(
            world.resource::<WorldSeed>().0,
            0x1234,
            "拒绝路径不能覆盖 WorldSeed"
        );
        let mut volume_query = world.query_filtered::<Entity, With<VoxVolume>>();
        assert_eq!(
            volume_query.iter(&world).count(),
            0,
            "拒绝路径不能 spawn 空体素容器"
        );

        let exits: Vec<AppExit> = world.resource_mut::<Messages<AppExit>>().drain().collect();
        assert_eq!(exits, vec![AppExit::error()], "拒绝后必须请求退出主循环");
    }
}
