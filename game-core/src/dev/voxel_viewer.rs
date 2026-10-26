//! 体素 demo 的本地观察者：给 `voxel_demo` 一个可编辑的玩家 + 相机。
//!
//! `start_bevy_voxel_demo` 只生成地形，不挂 demo / gameplay_test，因此默认没有
//! LocalPlayer，方块编辑系统拿不到相机与玩家。本插件在 `PostStartup`（地形已生成）
//! 按种子算出 (0,0) 列的地表高度，把玩家放在地表上方 1m，带动态刚体 + 胶囊碰撞体，
//! 让玩家真实落到地形上；相机由 `CameraPlugin` 跟随。
//!
//! 只用于开发演示（与 `dev::demo` / `dev::perf` / `dev::gameplay_test` 同一约定）。

use bevy::prelude::*;
use game_engine::identity::StableIdWorldExt;
use game_engine::voxel::{
    CoefficientCombine, Collider, Friction, LinearVelocity, LockedAxes, Restitution, RigidBody,
    SleepingDisabled,
};

use crate::dev::spawn_local_player;
use crate::gameplay::{spawn_camera_rig, Facing};
use crate::input::control::InputSourceId;
use crate::static_data::voxel::{DEFAULT_BIOME, DEFAULT_VOXEL_SIZE};
use crate::voxel::terrain::generation::WorldGenerator;
use crate::voxel::terrain::TerrainConfig;
use crate::world::WorldSeed;

/// 体素 demo 观察者的输入源 ID。
pub const VOXEL_VIEWER_SOURCE: InputSourceId = InputSourceId(4);

/// 玩家胶囊半径 / 长度（与 gameplay_test 一致：总高 1.7m）。
const PLAYER_CAPSULE_RADIUS: f32 = 0.35;
const PLAYER_CAPSULE_LENGTH: f32 = 1.0;
const PLAYER_CAPSULE_CENTER_Y: f32 = 0.85;

/// 体素 demo 观察者插件。
pub struct VoxelViewerPlugin;

impl Plugin for VoxelViewerPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostStartup, spawn_voxel_viewer);
    }
}

fn spawn_voxel_viewer(world: &mut World) {
    let Some(config) = world.get_resource::<TerrainConfig>().copied() else {
        return;
    };

    let generator = WorldGenerator::new(WorldSeed(config.seed), DEFAULT_BIOME);
    let surface = generator.column_surface(0, 0).max(1);
    let voxel_m = DEFAULT_VOXEL_SIZE.to_num::<f32>();
    // 站在 (0,0) 列地表上方 1m，让重力把玩家落到地形上。
    let feet = Vec3::new(0.5 * voxel_m, surface as f32 * voxel_m + 1.0, 0.5 * voxel_m);

    let (player, _) = spawn_local_player!(
        world,
        position = feet,
        source = VOXEL_VIEWER_SOURCE,
        observer = 80.0,
        extra = (
            RigidBody::Dynamic,
            LockedAxes::ROTATION_LOCKED,
            SleepingDisabled,
            LinearVelocity::ZERO,
        ),
    );

    // 俯视地表：相机在玩家脚下原点 + 眼高，pitch 为负表示低头。
    world.entity_mut(player).insert(Facing {
        yaw: 0.0,
        pitch: -0.45,
    });

    world.spawn((
        Collider::capsule(PLAYER_CAPSULE_RADIUS, PLAYER_CAPSULE_LENGTH),
        Friction::ZERO.with_combine_rule(CoefficientCombine::Min),
        Restitution::ZERO.with_combine_rule(CoefficientCombine::Min),
        Transform::from_xyz(0.0, PLAYER_CAPSULE_CENTER_Y, 0.0),
        ChildOf(player),
    ));

    spawn_camera_rig(world);
}
