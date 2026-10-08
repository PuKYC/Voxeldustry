//! 世界生成种子（Bevy Resource）。
//!
//! 种子是「一个世界必须有的确定性状态」，与具体体素机制无关，因此留在 world。

use bevy::prelude::*;

/// 世界生成种子（Bevy Resource）。
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorldSeed(pub u64);

impl WorldSeed {
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_roundtrips_and_defaults_to_zero() {
        assert_eq!(WorldSeed::new(7).0, 7);
        assert_eq!(WorldSeed::default(), WorldSeed(0));
    }
}
