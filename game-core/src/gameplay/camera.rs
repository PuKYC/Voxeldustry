//! 朝向与相机。
//!
//! 三个角色，各司其职：
//!
//! | 组件 | 角色 | 谁写 |
//! | --- | --- | --- |
//! | [`Facing`] | **朝向状态**（yaw / pitch，弧度） | [`facing_system`]：消费 `ResolvedInput.look_*` |
//! | [`LocalPlayer`] | **玩家标志位**（相机跟谁） | 装配时挂 |
//! | [`CameraFlag`] | **相机标志位**（这个实体是相机） | 装配时挂 |
//!
//! [`camera_system`] 读玩家的 `Facing` + 位置，写到带 `CameraFlag` 的实体上：
//! `Transform`（位置 + 朝向旋转）与 [`CameraView`]。`CameraView` 经
//! `synced_components` 变成 `Camera` 载荷（见 `presentation::payload::PresentedCamera`）
//! 下发给 Godot，Godot 的 `CameraRig` 只做机械搬运，不做任何视角决策。
//!
//! ## 约定
//!
//! - yaw 绕 +Y，0 = 朝 -Z（与 Godot `Camera3D` 默认朝向一致）；
//! - pitch 绕 +X，正 = 抬头；
//! - 欧拉顺序 YXZ（与 Godot `Node3D.rotation` 一致）；
//! - `look_*` 是输入增量：鼠标右移 → `look_yaw > 0` → 向右转 → yaw **减小**；
//!   鼠标下移 → `look_pitch > 0` → 向下看 → pitch **减小**。
//!
//! ## 阶段
//!
//! `facing_system` 在 `Decision`，`camera_system` 在 `Reaction`：后者要读到
//! `Motion` 推进后的玩家位置，否则相机永远慢一个 tick。
//!
//! ## 已知限制
//!
//! 预测 `Snapshot` 只带 `yaw`（移动方向由它决定，`Snapshot::advance` 按同一公式累加），
//! 不带 `pitch`（不影响位移）。局域网 reconciliation 落地时，权威端的 `Facing` 需要一并下发。

use std::f32::consts::{FRAC_PI_2, PI, TAU};

use bevy::prelude::*;

use game_engine::identity::{StableEntityId, StableIdWorldExt};

use crate::input::control::LocalPlayer;
use crate::input::snapshot::ResolvedInput;
use crate::presentation::PresentedTransform;
use crate::privacy::CoreRequiredPerception;

use super::GameplaySet;

/// 视角灵敏度：弧度 / `look_*` 单位。
///
/// `look_*` 单位 = 1000 个鼠标像素（见 `staging::LOOK_SCALE_DENOM`），
/// 3.0 即 0.003 rad/px（约 0.17°/px），是常见 FPS 的量级。
pub const LOOK_SENSITIVITY: f32 = 3.0;

/// pitch 上下限（弧度），留 0.01 余量避免万向节锁。
pub const PITCH_LIMIT: f32 = FRAC_PI_2 - 0.01;

/// 相机相对玩家脚底的眼高（世界单位）。
pub const CAMERA_EYE_HEIGHT: f32 = 1.6;

/// 朝向状态（弧度）。
///
/// 是**状态**不是输入：输入里只有增量（`PlayerInput::look_*`），
/// 绝对朝向由 [`facing_system`] 累加得到。
#[derive(Component, Clone, Copy, Debug, PartialEq, Default)]
pub struct Facing {
    pub yaw: f32,
    pub pitch: f32,
}

impl Facing {
    /// 叠加一个角度增量：yaw 折回 `[-π, π)`，pitch 夹在 ±[`PITCH_LIMIT`]。
    pub fn turned(self, d_yaw: f32, d_pitch: f32) -> Self {
        Self {
            yaw: wrap_pi(self.yaw + d_yaw),
            pitch: (self.pitch + d_pitch).clamp(-PITCH_LIMIT, PITCH_LIMIT),
        }
    }

    /// 对应的旋转（欧拉 YXZ，roll = 0）。
    pub fn rotation(self) -> Quat {
        Quat::from_euler(EulerRot::YXZ, self.yaw, self.pitch, 0.0)
    }
}

/// 把角度折回 `[-π, π)`。
fn wrap_pi(angle: f32) -> f32 {
    (angle + PI).rem_euclid(TAU) - PI
}

/// **相机标志位**：挂了它的实体由 [`camera_system`] 驱动。
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct CameraFlag;

/// 相机当前视角（要同步给客户端的那份）。
///
/// 与 `Facing` 同形但是**独立组件**：同步表只认它，于是「哪些朝向会下发」
/// 由「谁挂了 `CameraView`」决定，而不是所有带 `Facing` 的实体都下发。
#[derive(Component, Clone, Copy, Debug, PartialEq, Default)]
pub struct CameraView {
    pub yaw: f32,
    pub pitch: f32,
}

/// 系统：把本 tick 的视角增量累加进 `Facing`。
///
/// 任何有 `ResolvedInput` 的被控实体都会转向（附身 / 驾驶同理），
/// 不限于 `LocalPlayer`。
pub fn facing_system(mut query: Query<(&ResolvedInput, &mut Facing)>) {
    for (input, mut facing) in &mut query {
        let d_yaw = input.0.look_yaw.to_num::<f32>();
        let d_pitch = input.0.look_pitch.to_num::<f32>();
        if d_yaw == 0.0 && d_pitch == 0.0 {
            continue; // 不碰 Facing，避免无谓的 Changed
        }
        *facing = facing.turned(-d_yaw * LOOK_SENSITIVITY, -d_pitch * LOOK_SENSITIVITY);
    }
}

/// 系统：相机（`CameraFlag`）读玩家（`LocalPlayer`）的朝向与位置，设置并同步相机。
///
/// 值没变就不写，避免每 tick 都触发 `Changed<CameraView>` / `Changed<Transform>`
/// 导致无意义的重发。
pub fn camera_system(
    players: Query<(&Transform, &Facing), (With<LocalPlayer>, Without<CameraFlag>)>,
    mut cameras: Query<(&mut Transform, &mut CameraView), With<CameraFlag>>,
) {
    // 单人路径：只有一个本地玩家。多个时取第一个（确定性由 Query 迭代序保证不了，
    // 局域网下需要显式的 "相机跟随目标" 组件，届时替换这里）。
    let Some((player_transform, facing)) = players.iter().next() else {
        return;
    };

    let view = CameraView {
        yaw: facing.yaw,
        pitch: facing.pitch,
    };
    let translation = player_transform.translation + Vec3::Y * CAMERA_EYE_HEIGHT;
    let rotation = facing.rotation();

    for (mut transform, mut camera_view) in &mut cameras {
        if transform.translation != translation {
            transform.translation = translation;
        }
        if transform.rotation != rotation {
            transform.rotation = rotation;
        }
        if *camera_view != view {
            *camera_view = view;
        }
    }
}

/// 系统（`Update`）：给每个 `LocalPlayer` 补 `Facing`。
///
/// 与 `ensure_input_components` 同一思路：装配方（demo / perf / 将来的存档加载）
/// 不必各自记得挂 `Facing`。
pub fn ensure_facing(
    mut commands: Commands,
    query: Query<Entity, (With<LocalPlayer>, Without<Facing>)>,
) {
    for entity in &query {
        commands.entity(entity).insert(Facing::default());
    }
}

/// 生成一个相机实体（稳定 ID + 相机标志位 + 视角 + 表现端点）。
///
/// 需要 `Size` 才进空间索引（AOI 可见性）、需要 `PresentedTransform` 才会被
/// `Transform` 载荷带位置——与游荡实体同一套最小装配。
pub fn spawn_camera_rig(world: &mut World) -> (Entity, StableEntityId) {
    world.spawn_stable((
        CameraFlag,
        CameraView::default(),
        Transform::from_translation(Vec3::Y * CAMERA_EYE_HEIGHT),
        game_engine::spatial::Size(None),
        CoreRequiredPerception::PUBLIC,
        PresentedTransform::default(),
    ))
}

/// 朝向 + 相机装配。
pub struct CameraPlugin;

impl Plugin for CameraPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, ensure_facing)
            .add_systems(FixedUpdate, facing_system.in_set(GameplaySet::Decision))
            .add_systems(FixedUpdate, camera_system.in_set(GameplaySet::Reaction));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::snapshot::PlayerInput;
    use game_engine::math::FixedPoint;

    #[test]
    fn turned_wraps_yaw_into_half_open_range() {
        let f = Facing::default().turned(PI + 0.5, 0.0);
        assert!(f.yaw >= -PI && f.yaw < PI, "yaw 应折回 [-π, π)：{}", f.yaw);
        assert!((f.yaw - (-PI + 0.5)).abs() < 1e-5);

        let g = Facing::default().turned(-PI - 0.5, 0.0);
        assert!(g.yaw >= -PI && g.yaw < PI, "yaw 应折回 [-π, π)：{}", g.yaw);
        assert!((g.yaw - (PI - 0.5)).abs() < 1e-5);
    }

    #[test]
    fn turned_clamps_pitch() {
        assert_eq!(Facing::default().turned(0.0, 10.0).pitch, PITCH_LIMIT);
        assert_eq!(Facing::default().turned(0.0, -10.0).pitch, -PITCH_LIMIT);
    }

    /// yaw = 0 必须朝 -Z（Godot Camera3D 默认朝向）；yaw = +90° 朝 -X（逆时针，从上往下看）。
    #[test]
    fn yaw_zero_looks_down_negative_z() {
        let forward = Facing::default().rotation() * Vec3::NEG_Z;
        assert!((forward - Vec3::NEG_Z).length() < 1e-5);

        let left = Facing {
            yaw: FRAC_PI_2,
            pitch: 0.0,
        }
        .rotation()
            * Vec3::NEG_Z;
        assert!((left - Vec3::NEG_X).length() < 1e-5);
    }

    #[test]
    fn positive_pitch_looks_up() {
        let forward = Facing {
            yaw: 0.0,
            pitch: 0.5,
        }
        .rotation()
            * Vec3::NEG_Z;
        assert!(forward.y > 0.0, "pitch > 0 应抬头：{forward:?}");
    }

    fn app_with_camera() -> (App, Entity, Entity) {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(Time::<Fixed>::from_hz(60.0));
        app.add_plugins((super::super::GameplayPlugin, CameraPlugin));

        let player = app
            .world_mut()
            .spawn((
                LocalPlayer,
                Transform::from_xyz(3.0, 0.0, -4.0),
                Facing::default(),
                ResolvedInput::default(),
            ))
            .id();
        let camera = app
            .world_mut()
            .spawn((CameraFlag, CameraView::default(), Transform::default()))
            .id();
        app.finish();
        app.cleanup();
        (app, player, camera)
    }

    #[test]
    fn mouse_right_turns_right_and_mouse_down_looks_down() {
        let (mut app, player, _camera) = app_with_camera();
        app.world_mut()
            .entity_mut(player)
            .insert(ResolvedInput(PlayerInput {
                look_yaw: FixedPoint::from_num(0.1),
                look_pitch: FixedPoint::from_num(0.1),
                ..PlayerInput::NONE
            }));
        app.world_mut().run_schedule(FixedUpdate);

        let facing = *app.world().entity(player).get::<Facing>().unwrap();
        assert!(facing.yaw < 0.0, "鼠标右移应使 yaw 减小：{}", facing.yaw);
        assert!(
            facing.pitch < 0.0,
            "鼠标下移应使 pitch 减小：{}",
            facing.pitch
        );
        assert!((facing.yaw + 0.1 * LOOK_SENSITIVITY).abs() < 1e-3);
    }

    #[test]
    fn camera_copies_player_facing_and_follows_position() {
        let (mut app, player, camera) = app_with_camera();
        app.world_mut()
            .entity_mut(player)
            .insert(ResolvedInput(PlayerInput {
                look_yaw: FixedPoint::from_num(0.2),
                ..PlayerInput::NONE
            }));
        app.world_mut().run_schedule(FixedUpdate);

        let facing = *app.world().entity(player).get::<Facing>().unwrap();
        let view = *app.world().entity(camera).get::<CameraView>().unwrap();
        assert_eq!(view.yaw, facing.yaw);
        assert_eq!(view.pitch, facing.pitch);

        let transform = app.world().entity(camera).get::<Transform>().unwrap();
        assert_eq!(
            transform.translation,
            Vec3::new(3.0, CAMERA_EYE_HEIGHT, -4.0)
        );
        assert!(transform.rotation.angle_between(facing.rotation()) < 1e-5);
    }

    #[test]
    fn ensure_facing_adds_missing_component() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_systems(Update, ensure_facing);
        let player = app.world_mut().spawn(LocalPlayer).id();
        app.update();
        assert!(app.world().entity(player).get::<Facing>().is_some());
    }
}
