//! 停靠记录：v1 只定义类型，不实现玩法。
//!
//! 方案 A（分离体 + 刚体挂接记录）：host 是宿主，guest 是停靠方，
//! offset 必须量化到体素网格（不是子块对齐），rotation 是 90 度的倍数。

use bevy::prelude::*;
use game_engine::identity::StableEntityId;

/// 一次停靠关系。字段全为逻辑权威数据（整数 / 稳定 id）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DockRecord {
    /// 宿主（被停靠的体）。
    pub host: StableEntityId,
    /// 停靠方（飞船 / 结构）。
    pub guest: StableEntityId,
    /// 相对宿主的偏移，单位 = 体素网格。
    pub offset: IVec3,
    /// 朝向，90 度的倍数（0..=3 表示 4 个朝向）。
    pub rotation: u8,
}

impl DockRecord {
    pub const fn new(
        host: StableEntityId,
        guest: StableEntityId,
        offset: IVec3,
        rotation: u8,
    ) -> Self {
        Self {
            host,
            guest,
            offset,
            rotation,
        }
    }

    /// 朝向是否已量化到 90 度倍数（v1 只做校验，不做玩法判定）。
    pub const fn is_rotation_quantized(&self) -> bool {
        self.rotation % 90 == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dock_record_roundtrips_fields() {
        let record = DockRecord::new(
            StableEntityId(1),
            StableEntityId(2),
            IVec3::new(3, -4, 5),
            90,
        );
        assert_eq!(record.host, StableEntityId(1));
        assert_eq!(record.guest, StableEntityId(2));
        assert_eq!(record.offset, IVec3::new(3, -4, 5));
        assert!(record.is_rotation_quantized());
        assert!(
            !DockRecord::new(StableEntityId(1), StableEntityId(2), IVec3::ZERO, 45)
                .is_rotation_quantized()
        );
    }
}
