//! LOD 选择策略。
//!
//! 引擎只接收 Lod；距离 -> Lod 的阈值全部来自 static_data（游戏数值）。
//! 本模块只做纯函数映射，不读取 Transform / 不碰 Bevy。

use bevy::prelude::*;
use game_engine::math::FixedPoint;
use game_engine::voxel::Lod;

use crate::static_data::voxel::LOD_DISTANCE_THRESHOLDS;

/// v1 支持的最高 LOD（LOD 0..=3，对应 depth 5..=8）。
pub const MAX_LOD: u8 = 3;

/// 距离 -> Lod。
///
/// 阈值见 [LOD_DISTANCE_THRESHOLDS]：
/// - d < 32 m  -> LOD 0
/// - d < 64 m  -> LOD 1
/// - d < 128 m -> LOD 2
/// - 否则       -> LOD 3
///
/// 单调不减：d 增大时 Lod 不会回退。
pub fn lod_for_distance(distance: FixedPoint) -> Lod {
    let mut lod: u8 = 0;
    let mut index = 0usize;
    while index < LOD_DISTANCE_THRESHOLDS.len() {
        if distance >= LOD_DISTANCE_THRESHOLDS[index] {
            lod += 1;
            index += 1;
        } else {
            break;
        }
    }
    Lod::new(lod)
}

/// LOD 策略资源。v1 无状态；做成 Resource 便于将来到处注入 / 替换策略。
#[derive(Resource, Default, Clone, Copy, Debug)]
pub struct LodPolicy;

impl LodPolicy {
    pub fn lod_for_distance(&self, distance: FixedPoint) -> Lod {
        lod_for_distance(distance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meters(value: i64) -> FixedPoint {
        FixedPoint::from_bits(value << 24)
    }

    #[test]
    fn lod_threshold_boundaries() {
        assert_eq!(lod_for_distance(meters(0)).lod(), 0);
        assert_eq!(lod_for_distance(meters(31)).lod(), 0);
        assert_eq!(lod_for_distance(meters(32)).lod(), 1);
        assert_eq!(lod_for_distance(meters(63)).lod(), 1);
        assert_eq!(lod_for_distance(meters(64)).lod(), 2);
        assert_eq!(lod_for_distance(meters(127)).lod(), 2);
        assert_eq!(lod_for_distance(meters(128)).lod(), 3);
        assert_eq!(lod_for_distance(meters(10_000)).lod(), MAX_LOD);
    }

    #[test]
    fn lod_mapping_is_monotonic() {
        let step = meters(1);
        let mut distance = meters(0);
        let mut previous = lod_for_distance(distance).lod();
        for _ in 0..300 {
            distance += step;
            let current = lod_for_distance(distance).lod();
            assert!(
                current >= previous,
                "LOD 必须单调不减：{previous} -> {current}"
            );
            assert!(current <= MAX_LOD);
            previous = current;
        }
    }
}
