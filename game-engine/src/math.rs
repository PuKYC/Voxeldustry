//! 确定性定点数数学。
//!
//! 所有参与确定性路径（位置、速度、伤害、碰撞……）的数值一律定点数，
//! 不用 f32/f64。底层是整数运算，跨平台位级一致。

use fixed::types::I40F24;
use serde::{Deserialize, Serialize};
use std::ops::{Add, AddAssign, Mul, Sub, SubAssign};

/// 定点数类型：Q24（40 整数位 + 24 小数位），底层 i64。
///
/// 命名为 `FixedPoint` 以避开 bevy 自带的时间步标记类型 `bevy::prelude::Fixed`。
pub type FixedPoint = I40F24;

/// 定点三维向量（逻辑层专用；渲染侧在边界处转 f32/glam）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vec3F {
    pub x: FixedPoint,
    pub y: FixedPoint,
    pub z: FixedPoint,
}

impl Vec3F {
    pub const ZERO: Self = Self::new(
        FixedPoint::from_bits(0),
        FixedPoint::from_bits(0),
        FixedPoint::from_bits(0),
    );

    pub const fn new(x: FixedPoint, y: FixedPoint, z: FixedPoint) -> Self {
        Self { x, y, z }
    }
}

impl Add for Vec3F {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self::new(self.x + rhs.x, self.y + rhs.y, self.z + rhs.z)
    }
}

impl AddAssign for Vec3F {
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}

impl Sub for Vec3F {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self::new(self.x - rhs.x, self.y - rhs.y, self.z - rhs.z)
    }
}

impl SubAssign for Vec3F {
    fn sub_assign(&mut self, rhs: Self) {
        *self = *self - rhs;
    }
}

impl Mul<FixedPoint> for Vec3F {
    type Output = Self;
    fn mul(self, rhs: FixedPoint) -> Self {
        Self::new(self.x * rhs, self.y * rhs, self.z * rhs)
    }
}

// 网络序列化：按原始整数位（i64）传输，无损、不转浮点。
impl Serialize for Vec3F {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeTuple;
        let mut tuple = serializer.serialize_tuple(3)?;
        tuple.serialize_element(&self.x.to_bits())?;
        tuple.serialize_element(&self.y.to_bits())?;
        tuple.serialize_element(&self.z.to_bits())?;
        tuple.end()
    }
}

impl<'de> Deserialize<'de> for Vec3F {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (x, y, z) = <(i64, i64, i64)>::deserialize(deserializer)?;
        Ok(Vec3F::new(
            FixedPoint::from_bits(x),
            FixedPoint::from_bits(y),
            FixedPoint::from_bits(z),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_math_is_exact_for_representable_values() {
        // 0.5 与 2 都是精确可表示的：0.5 * 2 == 1 精确（无浮点误差）。
        let half = FixedPoint::from_num(1) / FixedPoint::from_num(2);
        assert_eq!(half * FixedPoint::from_num(2), FixedPoint::from_num(1));
    }

    #[test]
    fn vec3_serde_roundtrip_preserves_bits() {
        // 序列化后应逐位还原（原始整数位传输，不转浮点）。
        let v = Vec3F::new(
            FixedPoint::from_num(1) / FixedPoint::from_num(3),
            FixedPoint::from_num(2),
            FixedPoint::from_num(-3),
        );
        let json = serde_json::to_string(&v).unwrap();
        let back: Vec3F = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
        assert_eq!(
            json,
            format!("[{},{},{}]", v.x.to_bits(), v.y.to_bits(), v.z.to_bits())
        );
    }

    #[test]
    fn vec3_arithmetic() {
        let a = Vec3F::new(
            FixedPoint::from_num(1),
            FixedPoint::from_num(2),
            FixedPoint::from_num(3),
        );
        let b = Vec3F::new(
            FixedPoint::from_num(10),
            FixedPoint::from_num(20),
            FixedPoint::from_num(30),
        );
        let sum = a + b;
        assert_eq!(sum.x, FixedPoint::from_num(11));
        assert_eq!(sum.y, FixedPoint::from_num(22));
        assert_eq!(sum.z, FixedPoint::from_num(33));

        let scaled = a * FixedPoint::from_num(2);
        assert_eq!(scaled.x, FixedPoint::from_num(2));
        assert_eq!(scaled.y, FixedPoint::from_num(4));
        assert_eq!(scaled.z, FixedPoint::from_num(6));
    }
}
