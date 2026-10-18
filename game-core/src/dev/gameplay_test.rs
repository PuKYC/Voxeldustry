//! 游戏内容测试场景：可站立的体素平台 + 带碰撞的玩家。
//!
//! 与 [`crate::dev::demo`] 的区别：这里没有游荡实体 / 隐私演示，只验证「游戏内容」
//! 最基本的两件事：
//!
//! - **平台**：`VoxVolume` + `VoxelCollider` + `RigidBody::Static`，既通过
//!   `VoxChunkGroupRaw` 下发可见几何，又参与 avian 物理；
//! - **玩家**：`LocalPlayer` + 动态刚体 + 胶囊碰撞体（碰撞体是脚底原点实体下的
//!   子实体，保持 CameraPlugin / Prediction 使用的「Transform = 脚底」约定），
//!   WASD / Space 驱动水平速度与跳跃，会真实站在平台表面并被平台挡住。
//!
//! 启动：Godot 场景 `test/gameplay_test.tscn`（`mode = gameplay_test`）。
//!
//! 生产项目可以整体删除本模块（与 `dev::demo` / `dev::perf` 同一约定）。

use std::collections::BTreeMap;

use bevy::prelude::*;
use game_engine::identity::StableIdWorldExt;
use game_engine::presentation::voxel::extract_raw_halo;
use game_engine::presentation::RenderTransform;
use game_engine::spatial::Size;
use game_engine::voxel::{
    ChunkKey, Collider, LinearVelocity, LockedAxes, Lod, MaxDepth, MeshBlock, Neighbourhood,
    RigidBody, ShapeCastConfig, SleepingDisabled, SpatialQuery, SpatialQueryFilter, VoxOpsWrite,
    VoxTree, VoxVolume, VoxelCollider, VoxelInterner, CHUNK_DEPTH,
};

use super::spawn_local_player;
use crate::gameplay::{spawn_camera_rig, Facing, GameplaySet, Velocity};
use crate::input::control::{InputSourceId, LocalPlayer};
use crate::input::snapshot::ResolvedInput;
use crate::presentation::voxel_mesh::VoxChunkGroupRaw;
use crate::presentation::PresentedTransform;
use crate::static_data::voxel::{DEFAULT_VOXEL_SIZE, GRASS_BLOCK};

/// 测试场景输入源 ID。
pub const GAMEPLAY_TEST_SOURCE: InputSourceId = InputSourceId(3);

/// 平台：一个 32 x 4 x 32 体素的基础块（约 14.4 m x 1.8 m x 14.4 m）。
pub const PLATFORM_HEIGHT_VOXELS: i32 = 4;

/// 玩家胶囊（总高 = LENGTH + 2 * RADIUS = 1.7 m）。
pub const PLAYER_CAPSULE_RADIUS: f32 = 0.35;
pub const PLAYER_CAPSULE_LENGTH: f32 = 1.0;
/// 胶囊中心相对脚底原点的偏移（= 总高 / 2）。
pub const PLAYER_CAPSULE_CENTER_Y: f32 = 0.85;

/// 玩家水平移动速度（米 / 秒）。
pub const PLAYER_MOVE_SPEED: f32 = 6.5;
/// 水平速度向目标速度靠拢的加速度（米 / 秒²）。
pub const PLAYER_ACCELERATION: f32 = 40.0;
/// 跳跃初速度（米 / 秒）。
pub const PLAYER_JUMP_SPEED: f32 = 5.5;
/// 地面检测向下投射距离（米）。
pub const GROUND_CAST_DISTANCE: f32 = 0.18;

/// `InputAction` 表中 jump 的通道位（见 `input::actions::ACTION_TABLE`）。
const JUMP_CHANNEL_BIT: u64 = 1 << 4;

/// 游戏内容测试场景插件。
pub struct GameplayTestPlugin;

impl Plugin for GameplayTestPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_gameplay_test_world)
            // 与 demo 的玩家移动同阶段：Decision 已更新 Facing，本系统消费它。
            .add_systems(
                FixedUpdate,
                drive_physics_player.in_set(GameplaySet::Motion),
            );
    }
}

/// 玩家胶囊碰撞体所在的子实体。
///
/// 玩家根实体保持「脚底原点」，这样 CameraPlugin 的 `+CAMERA_EYE_HEIGHT` 与
/// 回滚 Snapshot 的语义都不变；物理碰撞形状通过带偏移的子实体挂到同一个
/// RigidBody 上。
#[derive(Component, Clone, Copy, Debug)]
pub struct PlayerCollider(pub Entity);

/// 建场景：静态体素平台 + 本地玩家 + 相机。
fn spawn_gameplay_test_world(world: &mut World) {
    spawn_platform(world);

    // 平台顶面 = 4 体素 * 0.45 m = 1.8 m。玩家脚底从 2.05 m 落下，
    // 确保第一帧不会和平台穿插。
    let (_player, _player_id) = spawn_local_player!(
        world,
        position = Vec3::new(7.2, 2.05, 7.2),
        source = GAMEPLAY_TEST_SOURCE,
        observer = 80.0,
        extra = (
            RigidBody::Dynamic,
            // 平台 / 玩家都是胶囊，锁定旋转避免被撞倒。
            LockedAxes::ROTATION_LOCKED,
            // 玩家休眠后手动写 LinearVelocity 不会自动唤醒，测试场景保持常醒。
            SleepingDisabled,
            LinearVelocity::ZERO,
        ),
    );

    // 碰撞形状挂在脚底原点之上：总高 1.7 m，中心在脚底 + 0.85 m。
    let collider_entity = world
        .spawn((
            Collider::capsule(PLAYER_CAPSULE_RADIUS, PLAYER_CAPSULE_LENGTH),
            Transform::from_xyz(0.0, PLAYER_CAPSULE_CENTER_Y, 0.0),
            ChildOf(_player),
        ))
        .id();
    world
        .entity_mut(_player)
        .insert(PlayerCollider(collider_entity));

    // 与 demo 一样：相机实体由 CameraPlugin 跟随本地玩家。
    spawn_camera_rig(world);
}

/// 生成体素平台，同时挂碰撞、静态刚体和 RawVoxelGroups 可见几何。
fn spawn_platform(world: &mut World) {
    let key = ChunkKey::new(0, 0, 0);

    let (volume, raw) = {
        let mut interner = world.resource_mut::<VoxelInterner>();
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
        for x in 0..32 {
            for y in 0..PLATFORM_HEIGHT_VOXELS {
                for z in 0..32 {
                    let _ = tree.set(interner.inner_mut(), IVec3::new(x, y, z), GRASS_BLOCK);
                }
            }
        }
        // 单块 halo：块外按空气处理，平台顶面 / 侧面才能正确封边。
        let raw = extract_raw_halo(
            &tree,
            interner.inner(),
            MeshBlock::new(key, Lod::new(0)),
            Neighbourhood::none(),
        );
        let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
        volume.insert_chunk(key, tree);
        (volume, raw)
    };

    let mut chunks = BTreeMap::new();
    chunks.insert(key, (raw, 0u8));

    let mut presented = PresentedTransform::default();
    let origin = Vec3::ZERO;
    presented.set_sampled(
        RenderTransform::from_translation(origin),
        RenderTransform::from_translation(origin),
    );

    world.spawn_stable((
        VoxChunkGroupRaw { lod: 0, chunks },
        volume,
        VoxelCollider,
        RigidBody::Static,
        Transform::from_translation(origin),
        Size(None),
        presented,
    ));
}

/// 把过滤后的 `ResolvedInput` 写进 avian 的速度组件。
///
/// - 水平方向：相对 `Facing.yaw` 的目标速度 + 加速度靠拢；
/// - 跳跃：只在向下 shape cast 命中地面时接受 `pressed` 的 jump 位；
/// - 竖直方向：落地后把残余向下速度清零，避免重力在接触面上累积。
///
/// 同步维护 gameplay 层的 `Velocity`，让回滚历史 / 表现采样读到同一份速度。
#[allow(clippy::too_many_arguments)]
fn drive_physics_player(
    time: Res<Time<Fixed>>,
    spatial_query: SpatialQuery,
    mut query: Query<
        (
            Entity,
            &ResolvedInput,
            Option<&Facing>,
            &PlayerCollider,
            &Transform,
            &mut LinearVelocity,
            &mut Velocity,
        ),
        With<LocalPlayer>,
    >,
) {
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }

    let shape = Collider::capsule(PLAYER_CAPSULE_RADIUS, PLAYER_CAPSULE_LENGTH);

    for (entity, input, facing, collider, transform, mut linear, mut logical) in &mut query {
        let yaw = facing.map_or(0.0, |f| f.yaw);
        let (sin, cos) = yaw.sin_cos();
        let forward = Vec3::new(-sin, 0.0, -cos);
        let right = Vec3::new(cos, 0.0, -sin);
        let wish =
            right * input.0.move_x.to_num::<f32>() + forward * input.0.move_y.to_num::<f32>();
        let target = wish.normalize_or_zero() * PLAYER_MOVE_SPEED;

        let blend = (PLAYER_ACCELERATION * dt).clamp(0.0, 1.0);
        linear.x += (target.x - linear.x) * blend;
        linear.z += (target.z - linear.z) * blend;

        let origin = transform.translation + Vec3::Y * PLAYER_CAPSULE_CENTER_Y;
        let ground = spatial_query.cast_shape(
            &shape,
            origin,
            Quat::IDENTITY,
            Dir3::NEG_Y,
            &ShapeCastConfig::from_max_distance(GROUND_CAST_DISTANCE),
            &SpatialQueryFilter::from_excluded_entities([entity, collider.0]),
        );

        if ground.is_some() && linear.y <= 0.1 {
            if input.0.pressed & JUMP_CHANNEL_BIT != 0 {
                linear.y = PLAYER_JUMP_SPEED;
            } else if linear.y < 0.0 {
                // 接触面已阻止下坠：清掉残余负速度，避免下一 tick 继续累积。
                linear.y = 0.0;
            }
        }

        logical.linear = Vec3::new(linear.x, linear.y, linear.z);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_engine::identity::StableIdPlugin;
    use game_engine::voxel::{ColliderOf, VoxelCollisionChunk, VoxelPlugin};

    /// 无头 update 至少推进一个 fixed step（与 game-engine 的碰撞测试同一做法）。
    fn run_fixed_update(app: &mut App) {
        let step = app.world().resource::<Time<Fixed>>().timestep();
        app.world_mut()
            .resource_mut::<Time<Fixed>>()
            .accumulate_overstep(step);
        app.update();
    }

    #[test]
    fn scene_spawns_visible_platform_and_player_with_collision() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.add_plugins(VoxelPlugin::default());
        app.add_plugins(game_engine::voxel::VoxelCollisionPlugin::default());
        app.add_plugins(GameplayTestPlugin);
        // Avian 在 Plugin::finish 注册诊断资源；只跑 update() 的测试必须显式 finish。
        app.finish();
        app.update(); // Startup：建平台 / 玩家 / 相机。

        // 平台：同一实体既可见（RawVoxelGroups 载荷）又带碰撞 / 静态刚体。
        let mut platform_query = app
            .world_mut()
            .query::<(&VoxVolume, &VoxChunkGroupRaw, &RigidBody)>();
        let platforms: Vec<_> = platform_query
            .iter(app.world())
            .filter(|(_, raw, body)| **body == RigidBody::Static && !raw.chunks.is_empty())
            .collect();
        assert_eq!(platforms.len(), 1, "测试场景必须恰有一个可见体素平台");

        // 可见性：平台下发的 34³ 原始体素必须能 mesh 出至少一个矩形。
        let (_, raw, _) = platforms[0];
        let (blocks, skirt_mask) = raw.chunks.values().next().expect("平台必须有体素块");
        let words = game_engine::presentation::voxel::mesh_raw_halo_with_skirts(
            raw.lod,
            blocks.as_slice(),
            *skirt_mask,
        );
        assert!(!words.is_empty(), "平台原始体素必须能 mesh 出可见几何");

        // 玩家：根实体是动态刚体，碰撞体是挂在同一 body 上的子实体。
        let player = {
            let mut query = app
                .world_mut()
                .query_filtered::<Entity, With<LocalPlayer>>();
            query.single(app.world()).expect("测试场景必须有本地玩家")
        };
        let collider_entity = app
            .world()
            .entity(player)
            .get::<PlayerCollider>()
            .expect("玩家必须记录碰撞体实体")
            .0;
        assert!(
            app.world()
                .entity(collider_entity)
                .get::<Collider>()
                .is_some(),
            "玩家碰撞体必须带 avian Collider"
        );
        assert_eq!(
            app.world()
                .entity(collider_entity)
                .get::<ColliderOf>()
                .expect("玩家碰撞体必须挂到玩家刚体上")
                .body,
            player
        );

        // 跑 60 个逻辑 tick：派生平台 chunk 碰撞体，并让玩家落到平台表面。
        for _ in 0..60 {
            run_fixed_update(&mut app);
        }

        let mut chunks = app.world_mut().query::<&VoxelCollisionChunk>();
        assert!(
            chunks.iter(app.world()).count() >= 1,
            "体素平台必须派生出至少一个 chunk 碰撞体"
        );

        let y = app
            .world()
            .entity(player)
            .get::<Transform>()
            .expect("玩家必须有 Transform")
            .translation
            .y;
        assert!(y > 1.7, "玩家不能穿过平台顶面（y={y}）");
        assert!(y < 3.0, "玩家应该落到平台附近（y={y}）");
    }
}
