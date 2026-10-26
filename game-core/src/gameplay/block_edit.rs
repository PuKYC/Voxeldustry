//! 方块编辑玩法：破坏 / 放置（最小闭环）。
//!
//! ## 数据流
//!
//! 1. 每 tick 从相机取一条射线（`VoxelRay::from_transform`）；
//! 2. [`pick_nearest`] 在触及距离内对所有带 `VoxVolume` 的 body 做 DDA；
//! 3. 把结果写进本玩家的 [`InteractionHint`]：命中 = `target BLOCK` 且
//!    `enabled = true`，否则 `NONE / false`（Godot 主按钮据此把 attack 解析成
//!    break_block）；
//! 4. 若本 tick 有 `break_block` / `place_block` 按下边沿，且冷却已过：
//!    - 破坏：命中格写空气；
//!    - 放置：命中面外一格写当前方块，并拒绝与玩家碰撞体重叠的格子。
//!
//! 写入统一走 `VoxelChangeBuffer`，由引擎 `VoxelSet::Apply` 应用到
//! `VoxVolume`、标脏并驱动碰撞 / 网格刷新。本模块不直接改体素树，也不复制
//! chunk 映射逻辑。
//!
//! ## 范围
//!
//! 单机本地。没有预测回滚、掉落物、背包；当前方块先是资源里的一个写死值
//! （[`SelectedBlock`]），以后接物品系统时替换读取来源即可。

use bevy::prelude::*;
use game_engine::input::actions::ActionId;
use game_engine::input::SimulationTick;
use game_engine::voxel::{
    chunk_voxels_per_axis, ChunkKey, VoxVolume, VoxelChangeBuffer, VoxelInterner, VoxelSet,
};

use crate::gameplay::{camera_system, CameraFlag, GameplaySet};
use crate::input::control::LocalPlayer;
use crate::input::snapshot::{PlayerInput, ResolvedInput};
use crate::presentation::payload::{
    InteractionHint, INTERACTION_TARGET_BLOCK, INTERACTION_TARGET_NONE,
};
use crate::static_data::voxel::{AIR_BLOCK, GRASS_BLOCK};
use crate::voxel::pick::{pick_nearest, VoxelRay, DEFAULT_PICK_DISTANCE_M};

/// `break_block` 的通道位（见 `input::actions::ACTION_TABLE`）。
pub const BREAK_BLOCK_BIT: u64 = 1 << 9;
/// `place_block` 的通道位。
pub const PLACE_BLOCK_BIT: u64 = 1 << 14;

/// `attack` 的 ActionId（无方块目标时的提示）。
const ATTACK_ACTION: ActionId = ActionId(9);
/// `break_block` 的 ActionId（有方块目标时的提示）。
const BREAK_ACTION: ActionId = ActionId(10);

/// 方块编辑运行参数。
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct BlockEditConfig {
    /// 触及距离（米），也是 [`InteractionHint`] 的 `enabled` 判据。
    pub reach_m: f32,
    /// 两次编辑之间的最小逻辑 tick 数（0 = 不限制）。
    pub cooldown_ticks: u32,
    /// 放置时的玩家水平半径（米），用于拒绝与玩家重叠的格子。
    pub player_radius_m: f32,
    /// 放置时的玩家高度（米）。
    pub player_height_m: f32,
}

impl Default for BlockEditConfig {
    fn default() -> Self {
        Self {
            reach_m: DEFAULT_PICK_DISTANCE_M,
            // 6 tick @60Hz = 100ms；足以吸收同一帧的重复按下 / 连点抖动。
            cooldown_ticks: 6,
            player_radius_m: 0.35,
            player_height_m: 1.7,
        }
    }
}

/// 当前放置方块。先写死为草方块；接物品系统后由选中槽位更新这个资源。
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectedBlock(pub u8);

impl Default for SelectedBlock {
    fn default() -> Self {
        Self(GRASS_BLOCK)
    }
}

/// 编辑冷却状态。
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockEditCooldown {
    last_tick: Option<u32>,
}

impl BlockEditCooldown {
    /// 距上次编辑是否已经过了 `cooldown_ticks` 个 tick。
    #[must_use]
    pub fn is_ready(&self, tick: u32, cooldown_ticks: u32) -> bool {
        match self.last_tick {
            None => true,
            Some(last) => tick.wrapping_sub(last) >= cooldown_ticks,
        }
    }

    /// 记录一次编辑发生的 tick。
    pub fn mark(&mut self, tick: u32) {
        self.last_tick = Some(tick);
    }
}

/// 方块编辑插件。
pub struct BlockEditPlugin;

impl Plugin for BlockEditPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<BlockEditConfig>()
            .init_resource::<SelectedBlock>()
            .init_resource::<BlockEditCooldown>()
            .init_resource::<SimulationTick>()
            // 相机在 Reaction 更新；编辑必须先读到本 tick 的相机位姿，再把编辑
            // 缓冲交给同一 tick 的 VoxelSet::Apply。
            .add_systems(
                FixedUpdate,
                block_edit_system
                    .in_set(GameplaySet::Reaction)
                    .after(camera_system),
            )
            .configure_sets(FixedUpdate, GameplaySet::Reaction.before(VoxelSet::Apply));
    }
}

/// 每 tick：拾取 + 写交互提示 + 消费 break/place 边沿。
#[allow(clippy::too_many_arguments)]
pub fn block_edit_system(
    tick: Res<SimulationTick>,
    config: Res<BlockEditConfig>,
    selected: Res<SelectedBlock>,
    mut cooldown: ResMut<BlockEditCooldown>,
    interner: Res<VoxelInterner>,
    cameras: Query<&Transform, With<CameraFlag>>,
    mut players: Query<
        (
            Entity,
            &Transform,
            &ResolvedInput,
            Option<&mut InteractionHint>,
        ),
        With<LocalPlayer>,
    >,
    mut bodies: Query<(Entity, &VoxVolume, &GlobalTransform, &mut VoxelChangeBuffer)>,
    mut commands: Commands,
) {
    let hit = cameras.iter().next().and_then(|camera| {
        let ray = VoxelRay::from_transform(camera)?;
        pick_nearest(
            interner.inner(),
            ray,
            config.reach_m,
            bodies
                .iter()
                .map(|(entity, volume, transform, _)| (entity, volume, transform)),
        )
    });

    // ── 1. 交互提示：每 tick 都写，命中方块 = BLOCK，否则 NONE ──
    let next_hint = InteractionHint {
        action: if hit.is_some() {
            BREAK_ACTION
        } else {
            ATTACK_ACTION
        },
        enabled: hit.is_some(),
        target: if hit.is_some() {
            INTERACTION_TARGET_BLOCK
        } else {
            INTERACTION_TARGET_NONE
        },
    };
    let mut actor: Option<(Vec3, PlayerInput)> = None;
    for (entity, transform, input, hint) in &mut players {
        if actor.is_none() {
            actor = Some((transform.translation, input.0));
        }
        match hint {
            Some(mut hint) => {
                if *hint != next_hint {
                    *hint = next_hint;
                }
            }
            None => {
                // 手搭的本地玩家可能没挂提示；补上，最迟下一 tick 生效。
                commands.entity(entity).insert(next_hint);
            }
        }
    }

    let Some(hit) = hit else {
        return;
    };
    let Some((player_feet, input)) = actor else {
        return;
    };
    if !cooldown.is_ready(tick.0, config.cooldown_ticks) {
        return;
    }

    // ── 2. 破坏：命中格设为空气 ──
    if input.pressed & BREAK_BLOCK_BIT != 0 {
        if push_edit(&mut bodies, hit.body, hit.voxel, AIR_BLOCK) {
            cooldown.mark(tick.0);
        }
        return;
    }

    // ── 3. 放置：命中面外一格写当前方块，且不能和玩家碰撞体重叠 ──
    if input.pressed & PLACE_BLOCK_BIT != 0 {
        if selected.0 == AIR_BLOCK {
            return;
        }
        let target = hit.voxel + hit.normal;
        // 触及距离内一般都有 chunk；若目标落在尚未生成的区域，不要 push 一个
        // 注定缺失的目标（apply 阶段会 debug_assert），直接跳过放置。
        let target_chunk_exists = bodies
            .get(hit.body)
            .ok()
            .is_some_and(|(_, volume, _, _)| chunk_exists(volume, target));
        if !target_chunk_exists {
            return;
        }
        let Some((voxel_size, body_transform)) = body_info(&bodies, hit.body) else {
            return;
        };
        if placement_overlaps_player(
            &body_transform,
            target,
            voxel_size,
            player_feet,
            config.player_radius_m,
            config.player_height_m,
        ) {
            debug!(
                "block placement rejected: target {:?} overlaps player at {:?}",
                target, player_feet
            );
            return;
        }
        if push_edit(&mut bodies, hit.body, target, selected.0) {
            cooldown.mark(tick.0);
        }
    }
}

fn push_edit(
    bodies: &mut Query<(Entity, &VoxVolume, &GlobalTransform, &mut VoxelChangeBuffer)>,
    body: Entity,
    voxel: IVec3,
    value: u8,
) -> bool {
    match bodies.get_mut(body) {
        Ok((_, _, _, mut buffer)) => {
            buffer.push([voxel.x, voxel.y, voxel.z], value);
            true
        }
        Err(_) => false,
    }
}

fn body_info(
    bodies: &Query<(Entity, &VoxVolume, &GlobalTransform, &mut VoxelChangeBuffer)>,
    body: Entity,
) -> Option<(f32, GlobalTransform)> {
    let (_, volume, transform, _) = bodies.get(body).ok()?;
    Some((volume.voxel_size.to_num::<f32>(), *transform))
}

fn chunk_exists(volume: &VoxVolume, voxel: IVec3) -> bool {
    let axis = chunk_voxels_per_axis();
    volume.contains_chunk(&ChunkKey::new(
        voxel.x.div_euclid(axis),
        voxel.y.div_euclid(axis),
        voxel.z.div_euclid(axis),
    ))
}

/// 目标体素的世界 AABB 是否与玩家（脚底原点 + 圆柱近似 AABB）严格相交。
///
/// 用 8 个角点做保守世界 AABB：body 旋转时可能略微高估，但只会更安全地
/// 拒绝放置，不会把方块放进玩家身体里。
#[must_use]
pub fn placement_overlaps_player(
    body_transform: &GlobalTransform,
    voxel: IVec3,
    voxel_size: f32,
    player_feet: Vec3,
    player_radius: f32,
    player_height: f32,
) -> bool {
    if !voxel_size.is_finite() || voxel_size <= 0.0 {
        return true;
    }
    let min_local = voxel.as_vec3() * voxel_size;
    let max_local = (voxel + IVec3::ONE).as_vec3() * voxel_size;
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for index in 0..8 {
        let x = if index & 1 == 0 {
            min_local.x
        } else {
            max_local.x
        };
        let y = if index & 2 == 0 {
            min_local.y
        } else {
            max_local.y
        };
        let z = if index & 4 == 0 {
            min_local.z
        } else {
            max_local.z
        };
        let world = body_transform.transform_point(Vec3::new(x, y, z));
        min = min.min(world);
        max = max.max(world);
    }

    let player_min = Vec3::new(
        player_feet.x - player_radius,
        player_feet.y,
        player_feet.z - player_radius,
    );
    let player_max = Vec3::new(
        player_feet.x + player_radius,
        player_feet.y + player_height,
        player_feet.z + player_radius,
    );

    min.x < player_max.x
        && max.x > player_min.x
        && min.y < player_max.y
        && max.y > player_min.y
        && min.z < player_max.z
        && max.z > player_min.z
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::actions::mask_of_name;
    use crate::static_data::voxel::DEFAULT_VOXEL_SIZE;
    use game_engine::voxel::{
        ChunkKey, MaxDepth, VoxOpsRead, VoxOpsWrite, VoxTree, VoxelPlugin, CHUNK_DEPTH,
    };
    use std::f32::consts::FRAC_PI_2;

    fn test_interner() -> VoxelInterner {
        VoxelInterner::new(4 * 1024 * 1024)
    }

    fn single_voxel_volume(interner: &mut VoxelInterner, point: IVec3, value: u8) -> VoxVolume {
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
        let _ = tree.set(interner.inner_mut(), point, value);
        let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
        volume.insert_chunk(ChunkKey::new(0, 0, 0), tree);
        volume
    }

    fn run_fixed_update(app: &mut App) {
        let step = app.world().resource::<Time<Fixed>>().timestep();
        app.world_mut()
            .resource_mut::<Time<Fixed>>()
            .accumulate_overstep(step);
        app.update();
    }

    fn test_app(interner: VoxelInterner) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(Time::<Fixed>::from_hz(60.0));
        app.insert_resource(SimulationTick(0));
        app.insert_resource(interner);
        app.add_plugins(VoxelPlugin::default());
        app.insert_resource(BlockEditConfig {
            reach_m: 10.0,
            cooldown_ticks: 1,
            ..BlockEditConfig::default()
        });
        app.add_plugins(BlockEditPlugin);
        app
    }

    fn spawn_body(app: &mut App, volume: VoxVolume, transform: Transform) -> Entity {
        let global = GlobalTransform::from(transform);
        app.world_mut()
            .spawn((volume, VoxelChangeBuffer::new(), transform, global))
            .id()
    }

    fn spawn_player(app: &mut App, feet: Vec3) -> Entity {
        app.world_mut()
            .spawn((
                LocalPlayer,
                Transform::from_translation(feet),
                InteractionHint {
                    action: ATTACK_ACTION,
                    enabled: false,
                    target: INTERACTION_TARGET_NONE,
                },
                ResolvedInput(PlayerInput::NONE),
            ))
            .id()
    }

    fn spawn_camera(app: &mut App, transform: Transform) -> Entity {
        app.world_mut()
            .spawn((CameraFlag, transform, GlobalTransform::from(transform)))
            .id()
    }

    fn set_pressed(app: &mut App, player: Entity, pressed: u64) {
        app.world_mut()
            .entity_mut(player)
            .insert(ResolvedInput(PlayerInput {
                pressed,
                ..PlayerInput::NONE
            }));
    }

    fn advance_tick(app: &mut App) {
        app.world_mut().resource_mut::<SimulationTick>().0 += 1;
    }

    fn read_tree(app: &App, body: Entity, point: IVec3) -> Option<u8> {
        let volume = app.world().entity(body).get::<VoxVolume>().expect("volume");
        let interner = app.world().resource::<VoxelInterner>();
        volume
            .get_chunk(&ChunkKey::new(0, 0, 0))
            .and_then(|tree| tree.get(interner.inner(), point))
    }

    #[test]
    fn action_bits_match_the_action_table() {
        assert_eq!(mask_of_name("break_block"), Some(BREAK_BLOCK_BIT));
        assert_eq!(mask_of_name("place_block"), Some(PLACE_BLOCK_BIT));
        assert_eq!(crate::input::actions::action_of_bit(9), Some(BREAK_ACTION));
        assert_eq!(crate::input::actions::action_of_bit(8), Some(ATTACK_ACTION));
    }

    #[test]
    fn break_removes_the_target_voxel() {
        let mut interner = test_interner();
        let volume = single_voxel_volume(&mut interner, IVec3::new(5, 0, 0), GRASS_BLOCK);
        let mut app = test_app(interner);
        let body = spawn_body(&mut app, volume, Transform::IDENTITY);
        let player = spawn_player(&mut app, Vec3::ZERO);
        let camera_transform =
            Transform::from_xyz(0.1, 0.2, 0.2).with_rotation(Quat::from_rotation_y(-FRAC_PI_2));
        let _camera = spawn_camera(&mut app, camera_transform);

        set_pressed(&mut app, player, BREAK_BLOCK_BIT);
        advance_tick(&mut app);
        run_fixed_update(&mut app);

        assert_eq!(
            read_tree(&app, body, IVec3::new(5, 0, 0)),
            None,
            "break must clear the targeted voxel"
        );
    }

    #[test]
    fn place_writes_the_adjacent_face_voxel() {
        let mut interner = test_interner();
        let volume = single_voxel_volume(&mut interner, IVec3::new(5, 0, 0), GRASS_BLOCK);
        let mut app = test_app(interner);
        let body = spawn_body(&mut app, volume, Transform::IDENTITY);
        let player = spawn_player(&mut app, Vec3::ZERO);
        let _camera = spawn_camera(
            &mut app,
            Transform::from_xyz(0.1, 0.2, 0.2).with_rotation(Quat::from_rotation_y(-FRAC_PI_2)),
        );

        set_pressed(&mut app, player, PLACE_BLOCK_BIT);
        advance_tick(&mut app);
        run_fixed_update(&mut app);

        assert_eq!(
            read_tree(&app, body, IVec3::new(4, 0, 0)),
            Some(GRASS_BLOCK),
            "place must fill the voxel outside the hit face"
        );
        assert_eq!(
            read_tree(&app, body, IVec3::new(5, 0, 0)),
            Some(GRASS_BLOCK),
            "the hit voxel itself must stay solid"
        );
    }

    #[test]
    fn place_into_a_missing_chunk_is_skipped_without_pushing_an_edit() {
        let mut interner = test_interner();
        let volume = single_voxel_volume(&mut interner, IVec3::new(0, 0, 0), GRASS_BLOCK);
        let mut app = test_app(interner);
        let body = spawn_body(&mut app, volume, Transform::IDENTITY);
        let player = spawn_player(&mut app, Vec3::new(10.0, 0.0, 10.0));
        let _camera = spawn_camera(
            &mut app,
            Transform::from_xyz(-1.0, 0.2, 0.2).with_rotation(Quat::from_rotation_y(-FRAC_PI_2)),
        );

        set_pressed(&mut app, player, PLACE_BLOCK_BIT);
        advance_tick(&mut app);
        // 目标 (-1,0,0) 在 chunk (-1,0,0)，本 volume 只有 chunk 0。
        // 若错误地 push 编辑，apply_voxel_changes 的 debug_assert 会在这里失败。
        run_fixed_update(&mut app);

        let volume = app.world().entity(body).get::<VoxVolume>().expect("volume");
        assert_eq!(volume.chunk_count(), 1, "不得凭空创建 chunk");
        assert_eq!(
            read_tree(&app, body, IVec3::new(0, 0, 0)),
            Some(GRASS_BLOCK)
        );
    }

    #[test]
    fn place_is_rejected_when_it_overlaps_the_player() {
        let mut interner = test_interner();
        let volume = single_voxel_volume(&mut interner, IVec3::new(0, 0, 0), GRASS_BLOCK);
        let mut app = test_app(interner);
        // body 下移一个体素：局部 (0,0,0) 顶面正好在玩家脚底 y=0，
        // 面外一格 (0,1,0) 会占世界 y[0,0.45]，与玩家 AABB 相交。
        let body = spawn_body(
            &mut app,
            volume,
            Transform::from_xyz(0.0, -DEFAULT_VOXEL_SIZE.to_num::<f32>(), 0.0),
        );
        let player = spawn_player(&mut app, Vec3::ZERO);
        let _camera = spawn_camera(
            &mut app,
            Transform::from_xyz(0.2, 1.0, 0.2).with_rotation(Quat::from_rotation_x(-FRAC_PI_2)),
        );

        set_pressed(&mut app, player, PLACE_BLOCK_BIT);
        advance_tick(&mut app);
        run_fixed_update(&mut app);

        assert_eq!(
            read_tree(&app, body, IVec3::new(0, 1, 0)),
            None,
            "placement overlapping the player capsule must be rejected"
        );
    }

    #[test]
    fn one_tick_cannot_both_break_and_place() {
        let mut interner = test_interner();
        let volume = single_voxel_volume(&mut interner, IVec3::new(5, 0, 0), GRASS_BLOCK);
        let mut app = test_app(interner);
        let body = spawn_body(&mut app, volume, Transform::IDENTITY);
        let player = spawn_player(&mut app, Vec3::ZERO);
        let _camera = spawn_camera(
            &mut app,
            Transform::from_xyz(0.1, 0.2, 0.2).with_rotation(Quat::from_rotation_y(-FRAC_PI_2)),
        );
        set_pressed(&mut app, player, BREAK_BLOCK_BIT | PLACE_BLOCK_BIT);
        advance_tick(&mut app);
        run_fixed_update(&mut app);

        assert_eq!(read_tree(&app, body, IVec3::new(5, 0, 0)), None);
        assert_eq!(
            read_tree(&app, body, IVec3::new(4, 0, 0)),
            None,
            "break wins the tick; place must not also fire"
        );
    }

    #[test]
    fn cooldown_blocks_back_to_back_edits() {
        let mut cooldown = BlockEditCooldown::default();
        assert!(cooldown.is_ready(10, 6));
        cooldown.mark(10);
        assert!(!cooldown.is_ready(10, 6));
        assert!(!cooldown.is_ready(15, 6));
        assert!(cooldown.is_ready(16, 6));
    }

    #[test]
    fn interaction_hint_tracks_camera_target() {
        let mut interner = test_interner();
        let volume = single_voxel_volume(&mut interner, IVec3::new(5, 0, 0), GRASS_BLOCK);
        let mut app = test_app(interner);
        let _body = spawn_body(&mut app, volume, Transform::IDENTITY);
        let player = spawn_player(&mut app, Vec3::ZERO);
        let camera = spawn_camera(
            &mut app,
            Transform::from_xyz(0.1, 0.2, 0.2).with_rotation(Quat::from_rotation_y(-FRAC_PI_2)),
        );

        advance_tick(&mut app);
        run_fixed_update(&mut app);
        let hint = *app
            .world()
            .entity(player)
            .get::<InteractionHint>()
            .expect("hint must be present");
        assert_eq!(hint.target, INTERACTION_TARGET_BLOCK);
        assert!(hint.enabled);
        assert_eq!(hint.action, BREAK_ACTION);

        // 超出触及距离：方块还在，但提示必须关闭（enabled 反映 reach）。
        app.world_mut().resource_mut::<BlockEditConfig>().reach_m = 1.0;
        advance_tick(&mut app);
        run_fixed_update(&mut app);
        let hint = *app
            .world()
            .entity(player)
            .get::<InteractionHint>()
            .expect("hint must be present");
        assert_eq!(hint.target, INTERACTION_TARGET_NONE);
        assert!(!hint.enabled, "out of reach must disable the hint");
        app.world_mut().resource_mut::<BlockEditConfig>().reach_m = 10.0;

        // 抬头看天：不再命中，主按钮应回到 Attack。
        let up = Transform::from_xyz(0.1, 0.2, 0.2).with_rotation(Quat::from_rotation_x(FRAC_PI_2));
        *app.world_mut()
            .entity_mut(camera)
            .get_mut::<Transform>()
            .expect("camera transform") = up;
        *app.world_mut()
            .entity_mut(camera)
            .get_mut::<GlobalTransform>()
            .expect("camera global") = GlobalTransform::from(up);
        advance_tick(&mut app);
        run_fixed_update(&mut app);

        let hint = *app
            .world()
            .entity(player)
            .get::<InteractionHint>()
            .expect("hint must be present");
        assert_eq!(hint.target, INTERACTION_TARGET_NONE);
        assert!(!hint.enabled);
        assert_eq!(hint.action, ATTACK_ACTION);
    }

    #[test]
    fn placement_overlap_uses_world_aabb() {
        let body = GlobalTransform::IDENTITY;
        // 体素 (0,0,0) 边长 1，玩家脚底 (0.5,0,0.5) 半径 0.35：相交。
        assert!(placement_overlaps_player(
            &body,
            IVec3::ZERO,
            1.0,
            Vec3::new(0.5, 0.0, 0.5),
            0.35,
            1.7
        ));
        // 方块在地面以下，玩家站在其顶面：不严格相交。
        assert!(!placement_overlaps_player(
            &GlobalTransform::from_xyz(0.0, -1.0, 0.0),
            IVec3::ZERO,
            1.0,
            Vec3::new(0.5, 0.0, 0.5),
            0.35,
            1.7
        ));
    }
}
