//! 性能测试 demo：把「大量实体 + 每 tick 全量变化」的负载灌进表现管线。
//!
//! 目的不是玩法，而是**测量**：模拟 → 收集 → 编码 → 发布 各段耗时，
//! 以及旧 Dictionary 通道与 GPF1 SoA 快通道的差距。
//!
//! 挂载方式：
//! - Godot 侧调用 BevyAppManager.start_bevy_perf_test(fixed_hz, runner_hz, count)；
//! - 或直接 add_plugins(PerfDemoPlugin { entity_count })，见本文件单测。
//!
//! 与 `crate::dev::demo` 一样，生产项目应删除/替换本模块。

use bevy::prelude::*;

use super::spawn_local_player;
use crate::gameplay::GameplaySet;
use crate::input::control::{InputSourceId, LocalPlayer};
use crate::input::SimulationTick;
use crate::presentation::payload::PresentationState;
use crate::presentation::PresentedTransform;
use crate::static_data::prototype::Prototype;
use game_engine::identity::StableIdWorldExt;
use game_engine::spatial::Size;

use crate::voxel::terrain::{TerrainConfig, TerrainStats, VoxelLodObserver};

/// 性能测试用的输入源 ID。
pub const PERF_SOURCE: InputSourceId = InputSourceId(1);

/// 每个性能实体沿对角线来回运动（位置每 tick 必变，最大化 Changed 命中）。
#[derive(Component, Clone, Copy, Debug)]
pub struct PerfMotion {
    pub base: Vec3,
    pub velocity: Vec3,
    pub span: f32,
}

/// 性能测试场景插件。
pub struct PerfDemoPlugin {
    pub entity_count: usize,
}

#[derive(Resource)]
struct PerfEntityCount(usize);

impl Plugin for PerfDemoPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(PerfEntityCount(self.entity_count.max(1)))
            .add_systems(Startup, spawn_perf_world)
            .add_systems(FixedUpdate, perf_move_entities.in_set(GameplaySet::Motion));
    }
}

/// 建场景：1 个本地玩家（大半径 AOI 观察者）+ N 个网格分布的移动实体。
fn spawn_perf_world(world: &mut World) {
    let count = world.resource::<PerfEntityCount>().0;
    let cols = (count as f64).sqrt().ceil() as i32;
    let spacing: i32 = 4;
    let half = cols / 2;

    // 玩家 = AOI 观察者。半径覆盖整个网格，保证「最坏情况：全部可见」。
    let radius = (cols as f32) * (spacing as f32) + 50.0;
    let (_player_entity, _player_id) = spawn_local_player!(
        world,
        position = Vec3::ZERO,
        source = PERF_SOURCE,
        observer = radius,
        extra = (),
    );

    for i in 0..count as i32 {
        let gx = i % cols;
        let gz = i / cols;
        let x = (gx - half) * spacing;
        let z = (gz - half) * spacing;

        let base = Vec3::new(x as f32, 0.0, z as f32);
        let velocity = Vec3::new((1 + (i % 3)) as f32, 0.0, (1 + (i % 2)) as f32);

        world.spawn_stable((
            Transform::from_xyz(x as f32, 0.0, z as f32),
            // 没有 `Size` 就进不了空间索引，会永远落在玩家 AOI 之外。
            Size(None),
            Prototype::new(2),
            PresentationState::idle().with_locomotion(1),
            PresentedTransform::default(),
            PerfMotion {
                base,
                velocity,
                span: 40.0,
            },
        ));
    }
}

/// 每 tick 推进所有性能实体（位置只由 tick 决定）。
fn perf_move_entities(
    tick: Res<SimulationTick>,
    mut query: Query<(&mut Transform, &PerfMotion), Without<LocalPlayer>>,
) {
    let now = tick.0 as f32;
    for (mut transform, motion) in &mut query {
        let position = Vec3::new(
            motion.base.x + wrap_delta(motion.velocity.x * now, motion.span),
            motion.base.y,
            motion.base.z + wrap_delta(motion.velocity.z * now, motion.span),
        );
        transform.translation = position;
    }
}

/// 把 value 折叠到 [-span/2, span/2)。
fn wrap_delta(value: f32, span: f32) -> f32 {
    let half = span / 2.0;
    let shifted = value + half;
    let periods = (shifted / span).floor();
    shifted - periods * span - half
}

/// 体素性能场景输入源 ID。
pub const VOXEL_PERF_SOURCE: InputSourceId = InputSourceId(2);

/// 体素性能场景：`TerrainConfig` + 每 tick 沿 X 移动的本地观察者。
///
/// 地形在 Startup 网格化一次（静态）；移动观察者会让 mesh 块实体不停进出
/// 可见集，从而把「空间索引 -> AOI -> 可见性 -> 收集 -> 发布」整条链路的
/// 成本压出来（含 RAWVOXELS 的大载荷进出）。
#[derive(Clone, Copy, Debug)]
pub struct VoxelPerfPlugin {
    pub terrain: TerrainConfig,
    /// 观察者沿 X 的移动速度（米 / 逻辑 tick）。
    pub speed: f32,
}

impl VoxelPerfPlugin {
    #[must_use]
    pub fn new(terrain: TerrainConfig) -> Self {
        Self {
            terrain,
            speed: 0.25,
        }
    }
}

#[derive(Resource, Clone, Copy, Debug)]
struct VoxelPerfConfig {
    speed: f32,
    observer_radius: f32,
}

impl Plugin for VoxelPerfPlugin {
    fn build(&self, app: &mut App) {
        let radius = observer_radius_meters(self.terrain.radius_blocks);
        // 地形参数必须在 GameVoxelPlugin 之前插入才会被注册（见 TerrainPlugin::build）；
        // CoreGame 路径已挂过 GameVoxelPlugin / TerrainPlugin，这里不重复挂。
        app.insert_resource(self.terrain);
        if !app.is_plugin_added::<crate::voxel::GameVoxelPlugin>() {
            app.add_plugins(crate::voxel::GameVoxelPlugin);
        }
        if !app.is_plugin_added::<crate::voxel::terrain::TerrainPlugin>() {
            app.add_plugins(crate::voxel::terrain::TerrainPlugin);
        }
        app.insert_resource(VoxelPerfConfig {
            speed: self.speed,
            observer_radius: radius,
        })
        .add_systems(Startup, spawn_voxel_perf_world)
        .add_systems(FixedUpdate, move_voxel_observer.in_set(GameplaySet::Motion))
        .add_systems(Update, report_terrain_once);
    }
}

/// 观察者半径：覆盖整个地形区域 + 余量（米）。
fn observer_radius_meters(radius_blocks: i32) -> f32 {
    let blocks = (radius_blocks.max(0) + 1) as f32;
    blocks * 32.0 * crate::static_data::voxel::VOXEL_SIZE_METERS as f32 * 1.6 + 40.0
}

fn spawn_voxel_perf_world(world: &mut World) {
    let radius = world.resource::<VoxelPerfConfig>().observer_radius;
    // 地表在 world y ≈ 64 体素（biome base_height = 64）。
    let surface_y = 64.0 * crate::static_data::voxel::VOXEL_SIZE_METERS as f32;
    spawn_local_player!(
        world,
        position = Vec3::new(0.0, surface_y, 0.0),
        source = VOXEL_PERF_SOURCE,
        observer = radius,
        extra = (),
    );
}

/// 观察者位置只由 tick 决定（确定性），在 ±span 内来回。
///
/// 同时把位置写进 [VoxelLodObserver]，驱动 terrain 的运行期 LOD 重划分
/// （世界层只看该资源，不引入 terrain -> input 依赖）。
fn move_voxel_observer(
    tick: Res<SimulationTick>,
    config: Res<VoxelPerfConfig>,
    mut query: Query<&mut Transform, With<LocalPlayer>>,
    mut lod_observer: ResMut<VoxelLodObserver>,
) {
    let span = config.observer_radius.max(1.0) * 0.25;
    let x = wrap_delta(tick.0 as f32 * config.speed, span);
    let mut position = None;
    for mut transform in &mut query {
        transform.translation.x = x;
        position = Some(transform.translation.to_array());
    }
    if let Some(position) = position {
        lod_observer.position = position;
    }
}

/// 第一帧打印一次地形统计（Startup 之后），供 Godot 控制台观察。
fn report_terrain_once(stats: Res<TerrainStats>, mut reported: Local<bool>) {
    if *reported {
        return;
    }
    *reported = true;
    println!(
        "[voxel_perf] chunks={} blocks={} lod={:?}",
        stats.chunks, stats.blocks, stats.lod_blocks
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::GameInputPlugin;
    use crate::presentation::{register_synced_components, PresentationPlugin, PresentationSlot};
    use game_engine::aoi::AoIPlugin;
    use game_engine::identity::StableIdPlugin;
    use game_engine::spatial::SpatialPlugin;
    use std::time::{Duration, Instant};

    fn build_app(entities: usize) -> App {
        build_app_with(entities, true)
    }

    /// `with_aoi = false` 时走 refresh_visibility 的退化路径（所有带
    /// StableEntityId 的实体都可见），用于隔离 AOI / 空间索引的耗时。
    fn build_app_with(entities: usize, with_aoi: bool) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(GameInputPlugin);
        app.add_plugins(PresentationPlugin);
        app.add_plugins(StableIdPlugin);
        if with_aoi {
            app.add_plugins((
                SpatialPlugin::<GlobalTransform, Size>::new(32.0),
                AoIPlugin::<GlobalTransform>::default(),
            ));
        }
        app.add_plugins(PerfDemoPlugin {
            entity_count: entities,
        });
        register_synced_components(&mut app);
        app.insert_resource(Time::<Fixed>::from_hz(60.0));
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(16),
        ));
        app
    }

    /// 体素性能场景的最小 App：地形 + 移动观察者 + 完整表现管线。
    fn build_voxel_app(terrain: TerrainConfig) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(GameInputPlugin);
        app.add_plugins(PresentationPlugin);
        app.add_plugins(StableIdPlugin);
        app.add_plugins((
            SpatialPlugin::<GlobalTransform, Size>::new(32.0),
            AoIPlugin::<GlobalTransform>::default(),
        ));
        app.add_plugins(VoxelPerfPlugin::new(terrain));
        register_synced_components(&mut app);
        app.insert_resource(Time::<Fixed>::from_hz(60.0));
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(16),
        ));
        app
    }

    /// 体素场景冒烟 + 计时：Startup 生成/网格化一次，随后移动观察者驱动
    /// 「空间索引 -> AOI -> 可见性 -> 收集 -> 发布」。
    #[test]
    fn bench_voxel_scene() {
        const FRAMES: usize = 30;
        let terrain = TerrainConfig {
            // 冒烟测试用最小地形（debug 生成便宜）；大场景看
            // bench_voxel_scene_lods / FFI start_bevy_voxel_perf。
            radius_blocks: 0,
            max_lod: 0,
            ..TerrainConfig::default()
        };
        let mut app = build_voxel_app(terrain);

        let started = Instant::now();
        app.update();
        let startup_ms = started.elapsed().as_secs_f64() * 1e3;

        let stats = *app.world().resource::<TerrainStats>();
        assert!(stats.chunks > 0 && stats.blocks > 0);
        assert_eq!(
            stats.blocks,
            app.world()
                .resource::<crate::voxel::terrain::TerrainBlocks>()
                .len()
        );

        let mut frames = 0usize;
        let mut commands = 0usize;
        let mut raw_commands = 0usize;
        let started = Instant::now();
        for _ in 0..FRAMES {
            app.update();
            if let Some(entry) = app.world().resource::<PresentationSlot>().take_entry() {
                frames += 1;
                commands += entry.frame.commands.len();
                for command in entry.frame.commands.iter() {
                    if let crate::presentation::PresentationCommand::Add {
                        payload: crate::presentation::SyncPayload::RawVoxels(payload),
                        ..
                    } = command
                    {
                        assert!(!payload.is_empty(), "RAWVOXELS 载荷必须带原始体素");
                        assert_eq!(payload.lod, 0);
                        assert_eq!(
                            payload.blocks.len(),
                            game_engine::presentation::voxel::RAW_VOXELS
                        );
                        raw_commands += 1;
                    }
                }
                assert!(!entry.streams.kinds.is_empty(), "发布时必须带 SoA 流");
            }
        }
        let per_frame = started.elapsed().as_secs_f64() * 1e3 / FRAMES as f64;
        println!(
            "voxel scene [{}]: startup {:.1} ms | chunks={} blocks={} | {:.3} ms/frame x {} | {} frames | {} commands",
            profile_label(),
            startup_ms,
            stats.chunks,
            stats.blocks,
            per_frame,
            FRAMES,
            frames,
            commands
        );
        assert!(frames > 0, "移动观察者必须产生可见性变化并发布帧");
        assert!(raw_commands > 0, "体素场景必须至少下发一次 RAWVOXELS 载荷");
    }

    /// 多 LOD 体素场景（release 手动跑；debug 生成很慢）。
    #[test]
    #[ignore = "release 基准：radius 8 / LOD3 的生成在 debug 下很慢"]
    fn bench_voxel_scene_lods() {
        let terrain = TerrainConfig {
            radius_blocks: 8,
            max_lod: 3,
            ..TerrainConfig::default()
        };
        let mut app = build_voxel_app(terrain);
        let started = Instant::now();
        app.update();
        let startup_ms = started.elapsed().as_secs_f64() * 1e3;
        let stats = *app.world().resource::<TerrainStats>();
        println!(
            "voxel LOD scene [{}]: startup {:.1} ms | chunks={} blocks={} lod={:?}",
            profile_label(),
            startup_ms,
            stats.chunks,
            stats.blocks,
            stats.lod_blocks
        );
        assert!(
            stats.lod_blocks[1] > 0 || stats.lod_blocks[2] > 0 || stats.lod_blocks[3] > 0,
            "半径 8 必须出现粗 LOD 块"
        );
    }

    struct PerfSample {
        ms_per_frame: f64,
        frames_taken: usize,
        total_commands: usize,
    }

    fn measure(app: &mut App, warmup: usize, frames: usize) -> PerfSample {
        for _ in 0..warmup {
            app.update();
        }
        let _ = app.world().resource::<PresentationSlot>().take_entry();

        let start = Instant::now();
        let mut total_commands = 0usize;
        let mut frames_taken = 0usize;
        for _ in 0..frames {
            app.update();
            if let Some(entry) = app.world().resource::<PresentationSlot>().take_entry() {
                total_commands += entry.frame.commands.len();
                assert!(!entry.streams.kinds.is_empty(), "发布时必须带 SoA 流");
                frames_taken += 1;
            }
        }
        PerfSample {
            ms_per_frame: start.elapsed().as_secs_f64() * 1e3 / frames as f64,
            frames_taken,
            total_commands,
        }
    }

    fn profile_label() -> &'static str {
        if cfg!(debug_assertions) {
            "debug(未优化，仅供回归对比)"
        } else {
            "release"
        }
    }

    /// 端到端性能压测：1000 实体、每个实体每 tick 都移动。
    ///
    /// 用 cargo test -p game-core --release dev::perf -- --nocapture 查看输出。
    #[test]
    fn bench_presentation_pipeline_1000_entities() {
        const ENTITIES: usize = 1000;
        const WARMUP: usize = 20;
        const FRAMES: usize = 60;

        let mut app = build_app(ENTITIES);
        let sample = measure(&mut app, WARMUP, FRAMES);

        println!(
            "perf pipeline [{}]: {} entities | {:.3} ms/frame x {} frames | {} frames taken | {} commands (avg {:.1}/frame)",
            profile_label(),
            ENTITIES,
            sample.ms_per_frame,
            FRAMES,
            sample.frames_taken,
            sample.total_commands,
            sample.total_commands as f64 / FRAMES as f64,
        );

        assert!(sample.frames_taken > 0, "表现管线必须产出帧");
        assert!(
            sample.total_commands >= ENTITIES,
            "至少每个实体一条命令，实际 {}",
            sample.total_commands
        );
    }

    /// 同上，但**不挂 AOI / 空间索引**，用于把 AOI 的耗时从总耗时里剥出来。
    #[test]
    fn bench_presentation_pipeline_1000_entities_no_aoi() {
        const ENTITIES: usize = 1000;
        const WARMUP: usize = 20;
        const FRAMES: usize = 60;

        let mut app = build_app_with(ENTITIES, false);
        let sample = measure(&mut app, WARMUP, FRAMES);

        println!(
            "perf pipeline no-AOI [{}]: {} entities | {:.3} ms/frame | {} frames taken | {} commands",
            profile_label(),
            ENTITIES,
            sample.ms_per_frame,
            sample.frames_taken,
            sample.total_commands,
        );
        assert!(sample.frames_taken > 0, "表现管线必须产出帧");
    }

    #[test]
    fn wrap_delta_keeps_value_in_range() {
        let span = 40.0;
        for step in -100..100 {
            let value = (step * 7) as f32;
            let wrapped = wrap_delta(value, span);
            assert!(
                wrapped >= -20.0 && wrapped < 20.0,
                "wrap 结果越界: {wrapped:?}"
            );
        }
    }

    /// 分阶段计时用的最小 App：可分别开关「表现管线 / 组件注册 / AOI」。
    fn build_phase_app(
        entities: usize,
        with_presentation: bool,
        register: bool,
        with_spatial: bool,
        with_aoi: bool,
        single_threaded: bool,
    ) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(GameInputPlugin);
        app.add_plugins(StableIdPlugin);
        if with_presentation {
            app.add_plugins(PresentationPlugin);
        }
        if with_spatial {
            app.add_plugins(SpatialPlugin::<GlobalTransform, Size>::new(32.0));
        }
        if with_aoi {
            app.add_plugins(AoIPlugin::<GlobalTransform>::default());
        }
        app.add_plugins(PerfDemoPlugin {
            entity_count: entities,
        });
        if register {
            register_synced_components(&mut app);
        }
        app.insert_resource(Time::<Fixed>::from_hz(60.0));
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(16),
        ));
        if single_threaded {
            force_single_threaded(&mut app);
        }
        app
    }

    /// 把所有主调度器切到单线程执行器。
    ///
    /// 多线程执行器下「加/减系统的墙钟差」不等于该系统的成本（并行会重叠、
    /// 改变同步点），实测会出现「加了系统反而更快」的假象。单线程下逐系统
    /// 串行执行，差值才可归因。
    fn force_single_threaded(app: &mut App) {
        use bevy::ecs::schedule::{Schedule, SingleThreadedExecutor};
        let mut force = |schedule: &mut Schedule| {
            schedule.set_executor(SingleThreadedExecutor::new());
        };
        app.edit_schedule(First, &mut force);
        app.edit_schedule(PreUpdate, &mut force);
        app.edit_schedule(Update, &mut force);
        app.edit_schedule(PostUpdate, &mut force);
        app.edit_schedule(Last, &mut force);
        app.edit_schedule(FixedPreUpdate, &mut force);
        app.edit_schedule(FixedUpdate, &mut force);
        app.edit_schedule(FixedPostUpdate, &mut force);
    }

    fn time_updates(app: &mut App, warmup: usize, frames: usize) -> f64 {
        for _ in 0..warmup {
            app.update();
        }
        let start = Instant::now();
        for _ in 0..frames {
            app.update();
        }
        start.elapsed().as_secs_f64() * 1e3 / frames as f64
    }

    /// 把 1000 实体压测拆成五段，定位真正的热点。
    ///
    /// 单次采样噪声可达数 ms（多次运行里甚至出现「加了系统反而更快」），
    /// 所以这里**交错多轮 + 每段取最小值**：min 是噪声下对真实成本最稳的
    /// 估计，交错让热漂移均匀作用到每一段。
    ///
    /// 手动运行（不在默认测试集，避免拖慢 debug 套件）：
    /// cargo test -p game-core --release dev::perf::tests::bench_pipeline_phases -- --ignored --nocapture
    #[test]
    #[ignore = "release 基准：手动用 --ignored --release 运行"]
    fn bench_pipeline_phases() {
        const ENTITIES: usize = 1000;
        const WARMUP: usize = 20;
        const FRAMES: usize = 90;
        const ROUNDS: usize = 5;

        let cases: [(&str, bool, bool, bool, bool); 5] = [
            ("sim only", false, false, false, false),
            ("sim + presentation(no reg)", true, false, false, false),
            ("sim + presentation(reg)", true, true, false, false),
            ("sim + presentation(reg) + spatial", true, true, true, false),
            (
                "sim + presentation(reg) + spatial + aoi",
                true,
                true,
                true,
                true,
            ),
        ];

        let mut best = [f64::INFINITY; 5];
        for _round in 0..ROUNDS {
            for (index, &(_, presentation, register, spatial, aoi)) in cases.iter().enumerate() {
                let mut app = build_phase_app(ENTITIES, presentation, register, spatial, aoi, true);
                let ms = time_updates(&mut app, WARMUP, FRAMES);
                if ms < best[index] {
                    best[index] = ms;
                }
            }
        }

        let mut previous = 0.0_f64;
        for (index, &(name, ..)) in cases.iter().enumerate() {
            println!(
                "phase [{}] {:>38}: {:6.3} ms/frame  (+{:.3})",
                profile_label(),
                name,
                best[index],
                best[index] - previous
            );
            previous = best[index];
        }
    }

    /// 受控对比：同一套系统，单线程 vs 多线程执行器。
    ///
    /// 交错多轮 + 取最小值，排除机器状态漂移。若 multi 远大于 single，
    /// 说明墙钟被执行器调度开销主导，而不是被某个业务系统主导。
    ///
    /// 手动运行：
    /// cargo test -p game-core --release dev::perf::tests::bench_executor_overhead -- --ignored --nocapture
    #[test]
    #[ignore = "release 基准：手动用 --ignored --release 运行"]
    fn bench_executor_overhead() {
        const ENTITIES: usize = 1000;
        const WARMUP: usize = 20;
        const FRAMES: usize = 90;
        const ROUNDS: usize = 5;

        let mut single = f64::INFINITY;
        let mut multi = f64::INFINITY;
        for _round in 0..ROUNDS {
            let mut app_single = build_phase_app(ENTITIES, true, true, true, true, true);
            single = single.min(time_updates(&mut app_single, WARMUP, FRAMES));
            let mut app_multi = build_phase_app(ENTITIES, true, true, true, true, false);
            multi = multi.min(time_updates(&mut app_multi, WARMUP, FRAMES));
        }

        println!(
            "executor [{}]: single {:.3} ms/frame | multi {:.3} ms/frame | multi/single = {:.2}x",
            profile_label(),
            single,
            multi,
            multi / single
        );
    }
}
