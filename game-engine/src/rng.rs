//! 确定性 PRNG：种子 + 状态存成 ECS Resource，参与快照/回滚。

use bevy::prelude::*;
use rand_core::{Rng, SeedableRng};
use rand_pcg::Pcg32;

use crate::math::FixedPoint;

/// 确定性 PRNG 状态（ECS Resource）。
#[derive(Resource, Clone, Debug)]
pub struct RngState(pub Pcg32);

impl RngState {
    pub fn from_seed(seed: u64) -> Self {
        Self(Pcg32::seed_from_u64(seed))
    }

    pub fn next_u32(&mut self) -> u32 {
        self.0.next_u32()
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    /// 返回 [0, 1) 的定点数（取 24 位小数）。
    pub fn next_fixed(&mut self) -> FixedPoint {
        let bits = (self.next_u32() & 0x00FF_FFFF) as i64;
        FixedPoint::from_bits(bits)
    }
}

impl Default for RngState {
    fn default() -> Self {
        Self::from_seed(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = RngState::from_seed(42);
        let mut b = RngState::from_seed(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seed_different_sequence() {
        let mut a = RngState::from_seed(1);
        let mut b = RngState::from_seed(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn next_fixed_in_unit_interval() {
        let mut r = RngState::from_seed(7);
        for _ in 0..1000 {
            let v = r.next_fixed();
            assert!(v >= FixedPoint::from_num(0));
            assert!(v < FixedPoint::from_num(1));
        }
    }
}
