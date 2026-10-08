//! 各业务载荷的 GPF1 字节布局（实现引擎的 PackedPayload）。
//!
//! 字节布局知识在引擎的 packed 模块提供原语；这里只把「字段 -> 字节」接上。

use game_engine::presentation::packed::{
    put_bool, put_f32, put_u32, put_u8, PackedError, PackedPayload, Reader, MAX_TAGS,
};

use crate::input::actions::ActionId;

use super::{
    ExtValue, ExtensionPayload, InteractionHint, PresentationState, PresentedHealth,
    PresentedPrototype, PresentedVisibility, RawVoxelPayload, MAX_EXT_FIELDS, MAX_EXT_TAGS,
};

impl PackedPayload for PresentationState {
    fn put_body(&self, out: &mut Vec<u8>) {
        put_u32(out, self.locomotion_state);
        put_u32(out, self.action_state);
        put_u32(out, self.overlay_tags.len() as u32);
        for tag in &self.overlay_tags {
            put_u32(out, *tag);
        }
    }

    fn write_pools(&self, _f: &mut Vec<f32>, i: &mut Vec<i64>) {
        i.push(i64::from(self.locomotion_state));
        i.push(i64::from(self.action_state));
        i.push(self.overlay_tags.len() as i64);
        for tag in &self.overlay_tags {
            i.push(i64::from(*tag));
        }
    }

    fn read_body(
        r: &mut Reader<'_>,
        _f: &mut Vec<f32>,
        i: &mut Vec<i64>,
    ) -> Result<(), PackedError> {
        let locomotion = r.u32()?;
        let action = r.u32()?;
        let count = r.u32()?;
        if count > MAX_TAGS {
            return Err(PackedError::TooManyTags);
        }
        i.push(i64::from(locomotion));
        i.push(i64::from(action));
        i.push(i64::from(count));
        for _ in 0..count {
            i.push(i64::from(r.u32()?));
        }
        Ok(())
    }

    fn from_pools(_f: &[f32], i: &[i64], _fo: usize, io: usize) -> Self {
        let count = i[io + 2] as usize;
        PresentationState {
            locomotion_state: i[io] as u32,
            action_state: i[io + 1] as u32,
            overlay_tags: i[io + 3..io + 3 + count]
                .iter()
                .map(|tag| *tag as u32)
                .collect(),
        }
    }
}

impl PackedPayload for PresentedHealth {
    fn put_body(&self, out: &mut Vec<u8>) {
        put_f32(out, self.current);
        put_f32(out, self.max);
    }

    fn write_pools(&self, f: &mut Vec<f32>, _i: &mut Vec<i64>) {
        f.push(self.current);
        f.push(self.max);
    }

    fn read_body(
        r: &mut Reader<'_>,
        f: &mut Vec<f32>,
        _i: &mut Vec<i64>,
    ) -> Result<(), PackedError> {
        f.push(r.f32()?);
        f.push(r.f32()?);
        Ok(())
    }

    fn from_pools(f: &[f32], _i: &[i64], fo: usize, _io: usize) -> Self {
        PresentedHealth {
            current: f[fo],
            max: f[fo + 1],
        }
    }
}

impl PackedPayload for PresentedVisibility {
    fn put_body(&self, out: &mut Vec<u8>) {
        put_bool(out, self.visible);
    }

    fn write_pools(&self, _f: &mut Vec<f32>, i: &mut Vec<i64>) {
        i.push(i64::from(self.visible as u8));
    }

    fn read_body(
        r: &mut Reader<'_>,
        _f: &mut Vec<f32>,
        i: &mut Vec<i64>,
    ) -> Result<(), PackedError> {
        i.push(i64::from(r.bool()? as u8));
        Ok(())
    }

    fn from_pools(_f: &[f32], i: &[i64], _io: usize, io: usize) -> Self {
        PresentedVisibility {
            visible: i[io] != 0,
        }
    }
}

impl PackedPayload for PresentedPrototype {
    fn put_body(&self, out: &mut Vec<u8>) {
        put_u32(out, self.0);
    }

    fn write_pools(&self, _f: &mut Vec<f32>, i: &mut Vec<i64>) {
        i.push(i64::from(self.0));
    }

    fn read_body(
        r: &mut Reader<'_>,
        _f: &mut Vec<f32>,
        i: &mut Vec<i64>,
    ) -> Result<(), PackedError> {
        i.push(i64::from(r.u32()?));
        Ok(())
    }

    fn from_pools(_f: &[f32], i: &[i64], _fo: usize, io: usize) -> Self {
        // 脏池 / 越界 offset 一律退化为 0，绝不 panic。
        PresentedPrototype(i.get(io).copied().unwrap_or(0) as u32)
    }
}

impl PackedPayload for InteractionHint {
    fn put_body(&self, out: &mut Vec<u8>) {
        put_u32(out, self.action.0);
        put_bool(out, self.enabled);
    }

    fn write_pools(&self, _f: &mut Vec<f32>, i: &mut Vec<i64>) {
        i.push(i64::from(self.action.0));
        i.push(i64::from(self.enabled as u8));
    }

    fn read_body(
        r: &mut Reader<'_>,
        _f: &mut Vec<f32>,
        i: &mut Vec<i64>,
    ) -> Result<(), PackedError> {
        i.push(i64::from(r.u32()?));
        i.push(i64::from(r.bool()? as u8));
        Ok(())
    }

    fn from_pools(_f: &[f32], i: &[i64], _io: usize, io: usize) -> Self {
        InteractionHint {
            action: ActionId(i[io] as u32),
            enabled: i[io + 1] != 0,
        }
    }
}

// ── 原始体素载荷：byte_len u32 LE + lod u8 + byte_len 字节（版本化、确定性）──
//
// 载荷体（不含 kind 码）字节布局：
//   [0,4)            byte_len  u32 LE
//   [4]              lod       u8
//   [5,5+byte_len)   byte_len 个原始体素字节
//
// SoA 池布局（write_pools 与 read_body 必须逐槽一致）：
//   i32 池依次 [lod, byte_len, chunk_count, chunk...]，
//   其中 chunk_count = ceil(byte_len / 8)，每 chunk 是 8 字节小端打包的 u64
//   （尾部补零）再 as i64。绝不 1 byte 占 1 个 i64。
impl PackedPayload for RawVoxelPayload {
    fn put_body(&self, out: &mut Vec<u8>) {
        put_u32(out, self.blocks.len() as u32);
        put_u8(out, self.lod);
        out.extend_from_slice(&self.blocks);
    }

    fn write_pools(&self, _f: &mut Vec<f32>, i: &mut Vec<i64>) {
        let byte_len = self.blocks.len();
        let chunk_count = byte_len.div_ceil(8);
        i.push(i64::from(self.lod));
        i.push(byte_len as i64);
        i.push(chunk_count as i64);
        for chunk in self.blocks.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            i.push(u64::from_le_bytes(word) as i64);
        }
    }

    fn read_body(
        r: &mut Reader<'_>,
        _f: &mut Vec<f32>,
        i: &mut Vec<i64>,
    ) -> Result<(), PackedError> {
        let byte_len = r.u32()? as usize;
        let lod = r.u8()?;
        let chunk_count = byte_len.div_ceil(8);
        i.push(i64::from(lod));
        i.push(byte_len as i64);
        i.push(chunk_count as i64);
        let mut remaining = byte_len;
        while remaining >= 8 {
            i.push(r.u64()? as i64);
            remaining -= 8;
        }
        if remaining > 0 {
            let mut word = [0u8; 8];
            for byte in word.iter_mut().take(remaining) {
                *byte = r.u8()?;
            }
            i.push(u64::from_le_bytes(word) as i64);
        }
        Ok(())
    }

    fn from_pools(_f: &[f32], i: &[i64], _fo: usize, io: usize) -> Self {
        // 脏池 / 越界 offset 一律退化为空载荷，绝不 panic。
        let (Some(io1), Some(io2), Some(io3)) =
            (io.checked_add(1), io.checked_add(2), io.checked_add(3))
        else {
            return Self::default();
        };
        let (Some(lod_raw), Some(byte_len_raw), Some(chunk_count_raw)) =
            (i.get(io).copied(), i.get(io1).copied(), i.get(io2).copied())
        else {
            return Self::default();
        };
        let (Ok(lod), Ok(byte_len), Ok(chunk_count)) = (
            u8::try_from(lod_raw),
            usize::try_from(byte_len_raw),
            usize::try_from(chunk_count_raw),
        ) else {
            return Self::default();
        };
        if chunk_count != byte_len.div_ceil(8) {
            return Self::default();
        }
        let Some(chunks_end) = io3.checked_add(chunk_count) else {
            return Self::default();
        };
        let Some(chunks) = i.get(io3..chunks_end) else {
            return Self::default();
        };
        let mut blocks = Vec::with_capacity(byte_len);
        for (index, chunk) in chunks.iter().enumerate() {
            let word = (*chunk as u64).to_le_bytes();
            let take = byte_len.saturating_sub(index.saturating_mul(8)).min(8);
            blocks.extend_from_slice(&word[..take]);
        }
        Self { lod, blocks }
    }
}

// ── 扩展载荷：字段袋的 GPF1 布局（原型期全部写 i32 池）──
//
// 线格式：count, 然后每字段 [key(u32), tag(u8), value]；
// 池布局：i.push(count), 每字段 i.push(key), i.push(tag), 然后按 tag 推值。
impl PackedPayload for ExtensionPayload {
    fn put_body(&self, out: &mut Vec<u8>) {
        // 上限的最终防线：即使载荷经 serde / 手工路径构造（绕过 from_fields），
        // 写出的帧也必然落在解码端上限内，不会让整帧被拒收。
        let fields = self.fields();
        debug_assert!(
            fields.len() <= MAX_EXT_FIELDS,
            "扩展字段数 {} 超过上限 {}（from_fields 应已拦截）",
            fields.len(),
            MAX_EXT_FIELDS
        );
        let count = fields.len().min(MAX_EXT_FIELDS);
        put_u32(out, count as u32);
        for field in &fields[..count] {
            put_u32(out, u32::from(field.key));
            match &field.value {
                ExtValue::I32(v) => {
                    put_u8(out, 0);
                    put_u32(out, *v as u32);
                }
                ExtValue::Bool(b) => {
                    put_u8(out, 1);
                    put_u8(out, u8::from(*b));
                }
                ExtValue::Tags(tags) => {
                    debug_assert!(
                        tags.len() <= MAX_EXT_TAGS,
                        "扩展 Tags 数 {} 超过上限 {}（normalize 应已截断）",
                        tags.len(),
                        MAX_EXT_TAGS
                    );
                    let n = tags.len().min(MAX_EXT_TAGS);
                    put_u8(out, 2);
                    put_u32(out, n as u32);
                    for tag in &tags[..n] {
                        put_u32(out, *tag);
                    }
                }
            }
        }
    }

    fn write_pools(&self, _f: &mut Vec<f32>, i: &mut Vec<i64>) {
        // 与 read_body 的池布局逐项一致；上限截断同 put_body。
        let fields = self.fields();
        let count = fields.len().min(MAX_EXT_FIELDS);
        i.push(count as i64);
        for field in &fields[..count] {
            i.push(i64::from(u32::from(field.key)));
            match &field.value {
                ExtValue::I32(v) => {
                    i.push(0);
                    i.push(i64::from(*v));
                }
                ExtValue::Bool(b) => {
                    i.push(1);
                    i.push(i64::from(*b as u8));
                }
                ExtValue::Tags(tags) => {
                    let n = tags.len().min(MAX_EXT_TAGS);
                    i.push(2);
                    i.push(n as i64);
                    for tag in &tags[..n] {
                        i.push(i64::from(*tag));
                    }
                }
            }
        }
    }

    fn read_body(
        r: &mut Reader<'_>,
        _f: &mut Vec<f32>,
        i: &mut Vec<i64>,
    ) -> Result<(), PackedError> {
        let count = r.u32()?;
        if count as usize > MAX_EXT_FIELDS {
            return Err(PackedError::TooManyExtensionFields);
        }
        i.push(i64::from(count));
        for _ in 0..count {
            let key = r.u32()?;
            let tag = r.u8()?;
            i.push(i64::from(key));
            i.push(i64::from(tag));
            match tag {
                0 => i.push(i64::from(r.u32()? as i32)),
                1 => i.push(i64::from(r.u8()?)),
                2 => {
                    let n = r.u32()?;
                    if n as usize > MAX_EXT_TAGS {
                        return Err(PackedError::TooManyTags);
                    }
                    i.push(i64::from(n));
                    for _ in 0..n {
                        i.push(i64::from(r.u32()?));
                    }
                }
                // 未知 tag 无长度信息，无法安全跳过：整帧拒绝（前向兼容靠尾部追加）。
                other => return Err(PackedError::BadExtensionTag(other)),
            }
        }
        Ok(())
    }

    fn from_pools(_f: &[f32], i: &[i64], _fo: usize, io: usize) -> Self {
        // 公开 API 且无 Result：脏池 / 越界 offset 一律退化为空载荷，绝不 panic。
        if io >= i.len() {
            return ExtensionPayload::default();
        }
        let mut cursor = io;
        let count = i[cursor] as usize;
        cursor += 1;
        if count > MAX_EXT_FIELDS {
            return ExtensionPayload::default();
        }
        let mut fields = Vec::with_capacity(count);
        for _ in 0..count {
            if cursor + 2 > i.len() {
                return ExtensionPayload::default();
            }
            let key = i[cursor] as u32 as u16;
            cursor += 1;
            let tag = i[cursor];
            cursor += 1;
            let value = match tag {
                0 => {
                    if cursor >= i.len() {
                        return ExtensionPayload::default();
                    }
                    let v = i[cursor] as u32 as i32;
                    cursor += 1;
                    ExtValue::I32(v)
                }
                1 => {
                    if cursor >= i.len() {
                        return ExtensionPayload::default();
                    }
                    let b = i[cursor] != 0;
                    cursor += 1;
                    ExtValue::Bool(b)
                }
                2 => {
                    if cursor >= i.len() {
                        return ExtensionPayload::default();
                    }
                    let n = i[cursor] as usize;
                    cursor += 1;
                    if n > MAX_EXT_TAGS || cursor + n > i.len() {
                        return ExtensionPayload::default();
                    }
                    let tags = i[cursor..cursor + n].iter().map(|x| *x as u32).collect();
                    cursor += n;
                    ExtValue::Tags(tags)
                }
                _ => ExtValue::I32(0),
            };
            fields.push(super::ExtField { key, value });
        }
        // 这里若因脏池越界则 from_fields 失败 -> 退化为空载荷，绝不 panic。
        ExtensionPayload::from_fields(fields).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use crate::presentation::payload::RawVoxelPayload;
    use game_engine::presentation::packed::{PackedPayload, Reader};
    use game_engine::presentation::voxel::RAW_VOXELS;

    fn raw_body_bytes(payload: &RawVoxelPayload) -> Vec<u8> {
        let mut out = Vec::new();
        payload.put_body(&mut out);
        out
    }

    /// read_body 推进的池必须与 write_pools 逐槽一致，并能 from_pools 还原。
    fn decode_raw_body(bytes: &[u8]) -> RawVoxelPayload {
        let mut r = Reader::new(bytes);
        let mut f = Vec::new();
        let mut i = Vec::new();
        RawVoxelPayload::read_body(&mut r, &mut f, &mut i).expect("raw 载荷体必须能解码");
        assert!(r.u8().is_err(), "载荷体必须正好消费完，不能有尾部字节");
        let decoded = RawVoxelPayload::from_pools(&f, &i, 0, 0);
        let mut f2 = Vec::new();
        let mut i2 = Vec::new();
        decoded.write_pools(&mut f2, &mut i2);
        assert_eq!(f2, f, "read_body 与 write_pools 的 f32 池必须一致");
        assert_eq!(i2, i, "read_body 与 write_pools 的 i32 池必须逐槽一致");
        decoded
    }

    /// 非空 / 空 / 非 8 倍数长度的往返必须无损。
    #[test]
    fn raw_voxel_payload_roundtrips_losslessly() {
        for lod in [0u8, 3] {
            for len in [0usize, 1, 7, 8, 9, 17, RAW_VOXELS] {
                let blocks: Vec<u8> = (0..len).map(|i| ((i * 7 + 1) % 256) as u8).collect();
                let payload = RawVoxelPayload::from_halo(lod, blocks.clone());
                let out = raw_body_bytes(&payload);

                // 版本化布局自检：byte_len u32 LE + lod u8 + byte_len 字节。
                assert_eq!(out.len(), 4 + 1 + len);
                assert_eq!(
                    u32::from_le_bytes(out[0..4].try_into().unwrap()),
                    len as u32
                );
                assert_eq!(out[4], lod);
                assert_eq!(&out[5..], &blocks[..]);

                let decoded = decode_raw_body(&out);
                assert_eq!(decoded, payload);
                assert_eq!(decoded.blocks, blocks);
            }
        }
    }

    /// 池布局必须是 [lod, byte_len, chunk_count, chunk...]，chunk 数 = ceil(len/8)。
    #[test]
    fn raw_voxel_pool_layout_is_chunked() {
        let payload = RawVoxelPayload::from_halo(2, vec![1, 2, 3, 4, 5]);
        let mut f = Vec::new();
        let mut i = Vec::new();
        payload.write_pools(&mut f, &mut i);
        assert!(f.is_empty(), "raw 载荷不写 f32 池");
        assert_eq!(i.len(), 3 + 1, "5 字节 = 1 个补齐 chunk");
        assert_eq!(i[0], 2);
        assert_eq!(i[1], 5);
        assert_eq!(i[2], 1);
        let expected = u64::from_le_bytes([1, 2, 3, 4, 5, 0, 0, 0]) as i64;
        assert_eq!(i[3], expected);

        let mut f = Vec::new();
        let mut i = Vec::new();
        RawVoxelPayload::from_halo(0, vec![9; RAW_VOXELS]).write_pools(&mut f, &mut i);
        assert_eq!(i[2], RAW_VOXELS.div_ceil(8) as i64);
    }

    /// 脏池 / 越界 offset 必须退化为 default，绝不 panic。
    #[test]
    fn raw_voxel_from_pools_degrades_on_dirty_pool() {
        assert_eq!(
            RawVoxelPayload::from_pools(&[], &[], 0, 0),
            RawVoxelPayload::default()
        );
        assert_eq!(
            RawVoxelPayload::from_pools(&[], &[1], 0, 0),
            RawVoxelPayload::default()
        );
        // chunk_count 与 byte_len 不一致 -> default。
        assert_eq!(
            RawVoxelPayload::from_pools(&[], &[0, 5, 99, 1], 0, 0),
            RawVoxelPayload::default()
        );
        // 越界 io -> default。
        assert_eq!(
            RawVoxelPayload::from_pools(&[], &[0, 0, 0], 0, 99),
            RawVoxelPayload::default()
        );
    }
}
