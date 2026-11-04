//! world.meta / region 文件的字节编解码（纯函数，无 IO）。
//!
//! 线格式与校验规则逐条对应《存档格式规格》§2（world.meta）、§3（region 文件与记录体）、
//! §4（实体/组件外层框）、§1.1（摘要）。所有 encode 都是纯函数：同样入参字节相同，
//! 提案/实体在写之前按规格键重排（BTreeMap），重复键直接报错而不是静默覆盖。

mod header;
mod reader;
mod record;
mod region;

pub use header::{decode_world_meta, encode_world_meta};
pub use record::{
    decode_chunk_record, decode_component_table, encode_chunk_record, encode_component_table,
};
pub use region::{decode_region, encode_region};

use crate::save::{SaveError, SaveErrorKind};

use self::reader::field_path;

// ---------------------------------------------------------------------------
// 校验 helper（统一构造 SaveError）
// ---------------------------------------------------------------------------

/// 校验两个值相等；失败报 Inconsistent（头长度、索引偏移、最大实体 id 等）。
pub(crate) fn expect_eq(
    path: &str,
    field: &str,
    offset: usize,
    what: &'static str,
    expected: u64,
    found: u64,
) -> Result<(), SaveError> {
    if expected != found {
        return Err(SaveError::new(
            field_path(path, field),
            offset,
            SaveErrorKind::Inconsistent {
                what,
                expected,
                found,
            },
        ));
    }
    Ok(())
}

/// 校验 `value` 没有置起 `allowed` 之外的标志位；失败报 Unsupported。
pub(crate) fn expect_flags_mask(
    path: &str,
    field: &str,
    offset: usize,
    what: &'static str,
    value: u64,
    allowed: u64,
) -> Result<(), SaveError> {
    if value & !allowed != 0 {
        return Err(SaveError::new(
            field_path(path, field),
            offset,
            SaveErrorKind::Unsupported { what, value },
        ));
    }
    Ok(())
}

/// 校验 `value <= max`；失败报 TooLarge。
pub(crate) fn expect_max(
    path: &str,
    field: &str,
    offset: usize,
    what: &'static str,
    value: u64,
    max: u64,
) -> Result<(), SaveError> {
    if value > max {
        return Err(SaveError::new(
            field_path(path, field),
            offset,
            SaveErrorKind::TooLarge { what, value, max },
        ));
    }
    Ok(())
}

/// 校验保留字段为 0；失败报 NonZeroReserved。
pub(crate) fn expect_reserved_zero(
    path: &str,
    field: &str,
    offset: usize,
    value: u64,
) -> Result<(), SaveError> {
    if value != 0 {
        return Err(SaveError::new(
            field_path(path, field),
            offset,
            SaveErrorKind::NonZeroReserved { value },
        ));
    }
    Ok(())
}

/// 校验 `value != 0`；失败报 InvalidValue。
pub(crate) fn expect_nonzero(
    path: &str,
    field: &str,
    offset: usize,
    what: &'static str,
    value: i64,
) -> Result<(), SaveError> {
    if value == 0 {
        return Err(SaveError::new(
            field_path(path, field),
            offset,
            SaveErrorKind::InvalidValue { what, value },
        ));
    }
    Ok(())
}

/// 校验版本 / 枚举取值必须等于 `expected`；失败报 Unsupported。
pub(crate) fn expect_supported(
    path: &str,
    field: &str,
    offset: usize,
    what: &'static str,
    expected: u64,
    found: u64,
) -> Result<(), SaveError> {
    if found != expected {
        return Err(SaveError::new(
            field_path(path, field),
            offset,
            SaveErrorKind::Unsupported { what, value: found },
        ));
    }
    Ok(())
}

/// 校验键严格升序；重复报 Duplicate，逆序报 NotStrictlyIncreasing。
pub(crate) fn expect_strictly_increasing(
    path: &str,
    field: &str,
    offset: usize,
    what: &'static str,
    previous: u64,
    current: u64,
) -> Result<(), SaveError> {
    if current <= previous {
        let kind = if current == previous {
            SaveErrorKind::Duplicate { what, id: current }
        } else {
            SaveErrorKind::NotStrictlyIncreasing {
                what,
                previous,
                current,
            }
        };
        return Err(SaveError::new(field_path(path, field), offset, kind));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
