//! 世界状态校验和：确定性验证与回滚等价的公共原语。
//!
//! 校验和只读遍历 World，不写任何状态；同一输入 + 同一 tick 数必须得到同一值。
//! 覆盖范围刻意保守：世界种子、实体稳定 ID、Transform 的位模式。它不覆盖
//! 体素内容、物品栏、脚本状态——那些要走各自的持久化 / 快照路径。
//!
//! 用途：
//! - 验证双跑：同一存档 / 同一输入跑两遍，比对校验和；
//! - 回滚等价：快照恢复后继续跑，比对校验和是否回到快照时的值；
//! - 联机诊断：客户端把校验和放进诊断快照，主机可发现模拟漂移。

use bevy::prelude::*;
use game_engine::identity::StableEntityId;

use crate::world::WorldSeed;

/// 校验和格式版本：覆盖范围或混入顺序变化时必须递增。
pub const CHECKSUM_VERSION: u32 = 1;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// 计算世界状态校验和（FNV-1a 64）。
///
/// 实体按 `(StableEntityId, Transform 位模式)` 排序后混入，因此 spawn 顺序
/// 不影响结果；没有 `Transform` 的实体（纯逻辑实体、体素体）不参与。
pub fn simulation_checksum(world: &mut World) -> u64 {
    let seed = world.get_resource::<WorldSeed>().map_or(0, |seed| seed.0);

    let mut rows: Vec<(u64, [u32; 10])> = {
        let mut query = world.query::<(&StableEntityId, &Transform)>();
        query
            .iter(world)
            .map(|(id, transform)| (id.0, transform_bits(transform)))
            .collect()
    };
    rows.sort_unstable();

    let mut hash = FNV_OFFSET;
    hash = fnv_bytes(hash, &CHECKSUM_VERSION.to_le_bytes());
    hash = fnv_bytes(hash, &seed.to_le_bytes());
    hash = fnv_bytes(hash, &(rows.len() as u64).to_le_bytes());
    for (id, bits) in rows {
        hash = fnv_bytes(hash, &id.to_le_bytes());
        for word in bits {
            hash = fnv_bytes(hash, &word.to_le_bytes());
        }
    }
    hash
}

fn transform_bits(transform: &Transform) -> [u32; 10] {
    [
        transform.translation.x.to_bits(),
        transform.translation.y.to_bits(),
        transform.translation.z.to_bits(),
        transform.rotation.x.to_bits(),
        transform.rotation.y.to_bits(),
        transform.rotation.z.to_bits(),
        transform.rotation.w.to_bits(),
        transform.scale.x.to_bits(),
        transform.scale.y.to_bits(),
        transform.scale.z.to_bits(),
    ]
}

fn fnv_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(rows: &[(u64, Vec3)]) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(WorldSeed(42));
        for (id, translation) in rows {
            app.world_mut().spawn((
                StableEntityId(*id),
                Transform::from_translation(*translation),
            ));
        }
        app
    }

    #[test]
    fn checksum_is_spawn_order_independent() {
        let mut a = app_with(&[(1, Vec3::new(1.0, 2.0, 3.0)), (2, Vec3::ZERO)]);
        let mut b = app_with(&[(2, Vec3::ZERO), (1, Vec3::new(1.0, 2.0, 3.0))]);
        assert_eq!(
            simulation_checksum(a.world_mut()),
            simulation_checksum(b.world_mut())
        );
    }

    #[test]
    fn checksum_changes_with_transform_and_seed() {
        let mut a = app_with(&[(1, Vec3::ZERO)]);
        let base = simulation_checksum(a.world_mut());
        {
            let mut query = a.world_mut().query::<&mut Transform>();
            let mut transform = query.single_mut(a.world_mut()).unwrap();
            transform.translation.x = 1.0;
        }
        assert_ne!(base, simulation_checksum(a.world_mut()));

        let mut b = app_with(&[(1, Vec3::ZERO)]);
        b.world_mut().resource_mut::<WorldSeed>().0 = 43;
        assert_ne!(base, simulation_checksum(b.world_mut()));
    }

    #[test]
    fn checksum_ignores_unidentified_entities() {
        let mut a = app_with(&[(1, Vec3::ZERO)]);
        let base = simulation_checksum(a.world_mut());
        a.world_mut().spawn(Transform::from_translation(Vec3::ONE));
        assert_eq!(base, simulation_checksum(a.world_mut()));
    }
    fn replay_app() -> App {
        use crate::gameplay::{GameplayPlugin, MovementPlugin, Velocity};

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(Time::<Fixed>::from_hz(60.0));
        app.add_plugins((GameplayPlugin, MovementPlugin));
        for i in 0..16u64 {
            app.world_mut().spawn((
                StableEntityId(i + 1),
                Transform::from_translation(Vec3::new(i as f32, 0.0, 0.0)),
                Velocity {
                    linear: Vec3::new(1.0, 0.0, 0.5),
                },
            ));
        }
        app.finish();
        app.cleanup();
        app
    }

    #[test]
    fn replaying_the_same_ticks_produces_the_same_checksum() {
        let mut a = replay_app();
        let mut b = replay_app();
        for _ in 0..120 {
            a.world_mut().run_schedule(FixedUpdate);
            b.world_mut().run_schedule(FixedUpdate);
        }
        assert_eq!(
            simulation_checksum(a.world_mut()),
            simulation_checksum(b.world_mut())
        );
    }

    #[test]
    fn restoring_a_transform_snapshot_restores_the_checksum() {
        let mut app = replay_app();
        let before = simulation_checksum(app.world_mut());

        let snapshot: Vec<(Entity, Transform)> = {
            let mut query = app.world_mut().query::<(Entity, &Transform)>();
            query
                .iter(app.world())
                .map(|(entity, transform)| (entity, *transform))
                .collect()
        };
        for _ in 0..30 {
            app.world_mut().run_schedule(FixedUpdate);
        }
        assert_ne!(before, simulation_checksum(app.world_mut()));

        for (entity, transform) in snapshot {
            *app.world_mut().get_mut::<Transform>(entity).unwrap() = transform;
        }
        assert_eq!(before, simulation_checksum(app.world_mut()));
    }
}
