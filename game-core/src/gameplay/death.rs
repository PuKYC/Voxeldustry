//! 死亡：伤害结算 -> 判定 -> 清理。
use bevy::prelude::*;

use crate::privacy::ExactHealth;

#[derive(Resource, Debug, Default)]
pub struct Deaths {
    pub count: u32,
}

pub struct DeathPlugin;

impl Plugin for DeathPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Deaths>()
            .add_message::<DamageEvent>()
            .add_message::<EventDeath>()
            // 阶段顺序由 game-engine 的 EngineSetPlugin 一次 chain：
            // Combat -> Reaction -> Cleanup。
            .add_systems(
                FixedUpdate,
                damage_system.in_set(super::GameplaySet::Combat),
            )
            .add_systems(
                FixedUpdate,
                death_check_system.in_set(super::GameplaySet::Reaction),
            )
            .add_systems(
                FixedUpdate,
                death_system.in_set(super::GameplaySet::Cleanup),
            );
    }
}

fn damage_system(mut events: MessageReader<DamageEvent>, mut query: Query<&mut ExactHealth>) {
    for damage in events.read() {
        if let Ok(mut health) = query.get_mut(damage.entity) {
            health.current = health.current.saturating_sub(damage.amount);
        }
    }
}

fn death_check_system(
    query: Query<(Entity, &ExactHealth), Changed<ExactHealth>>,
    mut deaths: MessageWriter<EventDeath>,
) {
    for (entity, health) in query.iter() {
        if health.current <= 0 {
            deaths.write(EventDeath { entity });
        }
    }
}

fn death_system(
    mut deaths: MessageReader<EventDeath>,
    mut commands: Commands,
    mut counter: ResMut<Deaths>,
) {
    for death in deaths.read() {
        counter.count += 1;
        commands.entity(death.entity).despawn();
    }
}

/// 造成伤害（瞬时消息）。来源只管写 Health，不判定死亡
#[derive(Message)]
pub struct DamageEvent {
    pub entity: Entity,
    pub amount: i32,
}

/// 死亡（一次性事实，用消息表达，而非 `IsDead` 状态字段）。
#[derive(Message)]
pub struct EventDeath {
    pub entity: Entity,
}

// 血量组件已统一为 `crate::privacy::ExactHealth`：它既是唯一的
// 逻辑真值（本模块的 damage/death 系统直接读写），又作为隐私门控载体
// 注册进 `CorePrivacyScope`。不要在这里再新建第二套 Health。
// 类型文档见 `privacy.rs`。
