//! 小端读写器与字段路径工具：encode/decode 共用的底层字节读写。

use crate::save::{SaveError, SaveErrorKind};

// ---------------------------------------------------------------------------
// 小端读写器
// ---------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct Writer {
    pub(crate) buf: Vec<u8>,
}

impl Writer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    pub(crate) fn u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn i32(&mut self, value: i32) {
        self.u32(value as u32);
    }

    pub(crate) fn i64(&mut self, value: i64) {
        self.u64(value as u64);
    }

    pub(crate) fn bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

/// 拼接字段路径：`{path}.{field}`。
///
/// 解码与校验一律经此构造错误路径，避免调用点手写 `format!` 造成文案漂移。
pub(crate) fn field_path(path: &str, field: &str) -> String {
    format!("{path}.{field}")
}

/// 拼接带下标的字段路径：`{path}.{field}[{index}]`。
pub(crate) fn indexed_path(path: &str, field: &str, index: usize) -> String {
    format!("{path}.{field}[{index}]")
}

pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    /// 该切片在文件里的绝对起始偏移，用于把错误偏移报到文件坐标。
    base: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8], base: usize) -> Self {
        Self {
            bytes,
            pos: 0,
            base,
        }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    pub(crate) fn offset(&self) -> usize {
        self.base + self.pos
    }

    pub(crate) fn take(&mut self, len: usize, path: &str) -> Result<&'a [u8], SaveError> {
        if self.remaining() < len {
            return Err(SaveError::new(
                path,
                self.offset(),
                SaveErrorKind::Truncated {
                    need: len,
                    have: self.remaining(),
                },
            ));
        }
        let start = self.pos;
        self.pos += len;
        Ok(&self.bytes[start..self.pos])
    }

    /// 按「分区路径 + 字段名」读取，内部统一拼接错误路径。
    pub(crate) fn take_at(
        &mut self,
        path: &str,
        field: &str,
        len: usize,
    ) -> Result<&'a [u8], SaveError> {
        self.take(len, &field_path(path, field))
    }

    pub(crate) fn read_u16_at(&mut self, path: &str, field: &str) -> Result<u16, SaveError> {
        self.u16(&field_path(path, field))
    }

    pub(crate) fn read_u32_at(&mut self, path: &str, field: &str) -> Result<u32, SaveError> {
        self.u32(&field_path(path, field))
    }

    pub(crate) fn read_u64_at(&mut self, path: &str, field: &str) -> Result<u64, SaveError> {
        self.u64(&field_path(path, field))
    }

    pub(crate) fn read_i32_at(&mut self, path: &str, field: &str) -> Result<i32, SaveError> {
        self.i32(&field_path(path, field))
    }

    pub(crate) fn u8(&mut self, path: &str) -> Result<u8, SaveError> {
        Ok(self.take(1, path)?[0])
    }

    pub(crate) fn u16(&mut self, path: &str) -> Result<u16, SaveError> {
        let bytes = self.take(2, path)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    pub(crate) fn u32(&mut self, path: &str) -> Result<u32, SaveError> {
        let bytes = self.take(4, path)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub(crate) fn u64(&mut self, path: &str) -> Result<u64, SaveError> {
        let bytes = self.take(8, path)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    pub(crate) fn i32(&mut self, path: &str) -> Result<i32, SaveError> {
        self.u32(path).map(|value| value as i32)
    }

    pub(crate) fn i64(&mut self, path: &str) -> Result<i64, SaveError> {
        self.u64(path).map(|value| value as i64)
    }
}

pub(crate) fn read_u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

pub(crate) fn u32_len(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}
