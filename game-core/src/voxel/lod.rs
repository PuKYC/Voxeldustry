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

/// 运行期可调的 LOD 配置（Godot 经 FFI 可写）。
///
/// 与 [LodPolicy]（只读、无状态）不同，本资源是**可变配置**：terrain 的运行期
/// 流式系统每 tick 读它；godot-client-ext 经 [crate::voxel::terrain::VoxelLodHandle] 写入。
/// 默认值由 [LOD_DISTANCE_THRESHOLDS] 派生，因此 [VoxelLodConfig::default] 与
/// [lod_for_distance] 数值一致。
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct VoxelLodConfig {
    /// 最高 LOD（[sanitized] 会夹到 0..=[MAX_LOD]）。
    pub max_lod: u8,
    /// 三档距离阈值（米，升序）；见 [sanitized]。
    pub thresholds_m: [f32; 3],
}

impl Default for VoxelLodConfig {
    fn default() -> Self {
        Self {
            max_lod: MAX_LOD,
            thresholds_m: [
                LOD_DISTANCE_THRESHOLDS[0].to_num::<f32>(),
                LOD_DISTANCE_THRESHOLDS[1].to_num::<f32>(),
                LOD_DISTANCE_THRESHOLDS[2].to_num::<f32>(),
            ],
        }
    }
}

impl VoxelLodConfig {
    /// 夹到合法域：max_lod 落在 0..=MAX_LOD；每个阈值有限、>= 1.0 且严格递增
    /// （后一档至少比前一档大 1.0）。确定性纯函数。
    #[must_use]
    pub fn sanitized(self) -> Self {
        let max_lod = self.max_lod.min(MAX_LOD);
        let mut thresholds_m = [1.0f32; 3];
        for index in 0..3 {
            let mut value = self.thresholds_m[index];
            if !value.is_finite() {
                value = 1.0;
            }
            value = value.max(1.0);
            if index > 0 {
                value = value.max(thresholds_m[index - 1] + 1.0);
            }
            thresholds_m[index] = value;
        }
        Self {
            max_lod,
            thresholds_m,
        }
    }

    /// 距离（米）-> LOD：与 [lod_for_distance] 相同的「逐档通过」规则，但阈值
    /// 取自本配置，结果夹到 max_lod。默认配置逐值复现 [lod_for_distance]。
    #[must_use]
    pub fn lod_for_distance_m(&self, distance_m: f64) -> u8 {
        // NaN / 负值一律退化为 0 m（f64::max 会忽略 NaN）。
        let distance = FixedPoint::from_num(distance_m.max(0.0));
        let mut lod = 0u8;
        for threshold in self.thresholds_m {
            if distance >= FixedPoint::from_num(threshold) {
                lod += 1;
            } else {
                break;
            }
        }
        lod.min(self.max_lod)
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

    #[test]
    fn default_voxel_lod_config_matches_lod_for_distance() {
        let config = VoxelLodConfig::default();
        assert_eq!(config.max_lod, MAX_LOD);
        assert_eq!(config.thresholds_m, [32.0, 64.0, 128.0]);
        for meters_value in [0i64, 1, 31, 32, 63, 64, 127, 128, 10_000] {
            let expected = lod_for_distance(meters(meters_value)).lod();
            assert_eq!(
                config.lod_for_distance_m(meters_value as f64),
                expected,
                "{meters_value} m 处默认配置必须复现 lod_for_distance"
            );
        }
    }

    #[test]
    fn sanitized_clamps_bad_thresholds_and_max_lod() {
        let dirty = VoxelLodConfig {
            max_lod: 99,
            thresholds_m: [f32::NAN, -5.0, 0.0],
        }
        .sanitized();
        assert_eq!(dirty.max_lod, MAX_LOD, "max_lod 必须夹到上限");
        assert!(dirty.thresholds_m[0] >= 1.0);
        assert!(dirty.thresholds_m[1] >= dirty.thresholds_m[0] + 1.0);
        assert!(dirty.thresholds_m[2] >= dirty.thresholds_m[1] + 1.0);

        let collapsed = VoxelLodConfig {
            max_lod: 0,
            thresholds_m: [100.0, 50.0, 10.0],
        }
        .sanitized();
        assert_eq!(collapsed.max_lod, 0);
        assert_eq!(collapsed.lod_for_distance_m(10_000.0), 0);

        // NaN 距离退化为 0 m，不 panic。
        assert_eq!(VoxelLodConfig::default().lod_for_distance_m(f64::NAN), 0);
    }
}
