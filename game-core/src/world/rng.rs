//! 世界随机数（确定性）：包装 game-engine 的 RngState。
//!
//! RngState 本身是引擎机制；本模块把它包成世界层 Resource，并提供稳定的
//! 委托入口，避免调用方直接依赖引擎路径。

use bevy::prelude::*;
use game_engine::math::FixedPoint;
use game_engine::rng::RngState;

/// 世界层 PRNG 资源：种子 + 状态，参与快照/回滚。
#[derive(Resource, Clone, Debug)]
pub struct WorldRng(pub RngState);

impl WorldRng {
    pub fn from_seed(seed: u64) -> Self {
        Self(RngState::from_seed(seed))
    }

    pub fn next_u32(&mut self) -> u32 {
        self.0.next_u32()
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    pub fn next_fixed(&mut self) -> FixedPoint {
        self.0.next_fixed()
    }
}

impl Default for WorldRng {
    fn default() -> Self {
        Self::from_seed(0)
    }
}
