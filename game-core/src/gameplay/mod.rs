//! 玩法层：阶段集装配 + movement / death 系统。
//!
//! 玩法系统**不自己声明彼此顺序**，只挂到 [`GameplaySet`] 的某个阶段；
//! [`GameplayPlugin`] 只挂引擎的 `EngineSetPlugin`；阶段顺序由 game-engine
//! 在 `FixedUpdate` 里一次 `.chain()` 声明（见 `game-engine/src/sim.rs`）。

mod death;
mod movement;

pub use death::DeathPlugin;
pub use movement::{MovementPlugin, Velocity};

use bevy::prelude::*;

/// 玩法系统阶段集。
///
/// ```text
/// Input -> Decision -> Motion -> Combat -> Reaction -> Cleanup
/// ```
///
/// 与表现层正交：表现收集仍在 `Update`，玩法推进仍在 `FixedUpdate`。
pub(crate) use game_engine::sim::{EngineSet, EngineSetPlugin};

/// 玩法阶段现在就是引擎的稳定阶段集（mod 也按它排序）。
pub type GameplaySet = EngineSet;

/// 只声明 [`EngineSet`] 的先后顺序，**不注册任何玩法系统**。
pub struct GameplayPlugin;

impl Plugin for GameplayPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(EngineSetPlugin);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 装配自检：阶段集与移动 / 死亡系统能一起挂上并跑一帧（不 panic）。
    ///
    /// bevy_backend::run_bevy_backend 需要真实通道，单测覆盖不到，这里补上
    /// 插件级装配的最小验证。
    #[test]
    fn gameplay_plugins_build_and_tick() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins((GameplayPlugin, MovementPlugin, DeathPlugin));
        app.update();
    }

    /// 确定性金标：固定初始状态跑 1000 个 fixed tick，两遍结果逐位一致。
    ///
    /// 只关心确定性边界（L3）：同样的输入 + 同样的顺序 -> 同样的世界状态。
    #[test]
    fn fixed_tick_simulation_is_bitwise_deterministic() {
        use game_engine::identity::StableEntityId;

        fn build() -> App {
            let mut app = App::new();
            app.add_plugins(MinimalPlugins);
            app.insert_resource(Time::<Fixed>::from_hz(60.0));
            app.add_plugins((GameplayPlugin, MovementPlugin));
            for i in 0..64i32 {
                let x = (i % 8) as f32;
                let z = (i / 8) as f32;
                app.world_mut().spawn((
                    StableEntityId(i as u64 + 1),
                    Transform::from_translation(Vec3::new(x, 0.0, z)),
                    super::movement::Velocity {
                        linear: Vec3::new((1 + (i % 3)) as f32, 0.0, (1 + (i % 2)) as f32),
                    },
                ));
            }
            app.finish();
            app.cleanup();
            app
        }

        fn world_hash(app: &mut App) -> Vec<u8> {
            let world = app.world_mut();
            let mut query = world.query::<(&StableEntityId, &Transform)>();
            let mut rows: Vec<(u64, u32, u32, u32)> = query
                .iter(world)
                .map(|(id, transform)| {
                    (
                        id.0,
                        transform.translation.x.to_bits(),
                        transform.translation.y.to_bits(),
                        transform.translation.z.to_bits(),
                    )
                })
                .collect();
            rows.sort_unstable();
            let mut bytes = Vec::with_capacity(rows.len() * 20);
            for (id, x, y, z) in rows {
                bytes.extend_from_slice(&id.to_le_bytes());
                bytes.extend_from_slice(&x.to_le_bytes());
                bytes.extend_from_slice(&y.to_le_bytes());
                bytes.extend_from_slice(&z.to_le_bytes());
            }
            bytes
        }

        let mut a = build();
        let mut b = build();
        for _ in 0..1000 {
            a.world_mut().run_schedule(FixedUpdate);
            b.world_mut().run_schedule(FixedUpdate);
        }

        let ha = world_hash(&mut a);
        let hb = world_hash(&mut b);
        assert!(!ha.is_empty(), "必须至少有一个实体参与模拟");
        assert_eq!(ha, hb, "固定输入下世界状态必须逐位一致");
    }
}
