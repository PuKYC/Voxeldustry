//! 各业务载荷的 GPF1 字节布局（实现引擎的 PackedPayload）。
//!
//! 字节布局知识在引擎的 packed 模块提供原语；这里只把「字段 -> 字节」接上。

use game_engine::presentation::packed::{
    put_bool, put_f32, put_u32, put_u64, put_u8, PackedError, PackedPayload, Reader, MAX_TAGS,
};

use crate::input::actions::ActionId;

use super::{
    ExtValue, ExtensionPayload, InteractionHint, PresentationState, PresentedHealth,
    PresentedPrototype, PresentedVisibility, RectListPayload, MAX_EXT_FIELDS, MAX_EXT_TAGS,
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

// ── 矩形实例载荷：count u32 LE + lod u8 + count × u64 LE（版本化、确定性）──
//
// 载荷体（不含 kind 码）字节布局：
//   [0,4)          count  u32 LE   矩形数
//   [4]            lod    u8       mesh 块 LOD
//   [5,5+8*count)  count × u64 LE  39 bit 矩形描述符（位布局见 RectListPayload）
//
// 逐字小端、无填充、无平台相关布局；同一输入 -> 逐字节一致。
// SoA 池布局（write_pools 与 read_body 必须一致）：i32 池依次 [lod, count, word...]。
// 这是**追加**的载荷体，GPF1 头部与 0..6 号既有载荷的布局一概不动。
impl PackedPayload for RectListPayload {
    fn put_body(&self, out: &mut Vec<u8>) {
        let count = self.rect_count();
        put_u32(out, count as u32);
        put_u8(out, self.lod);
        for chunk in self.rects.chunks_exact(RectListPayload::WORD_BYTES) {
            let mut bytes = [0u8; RectListPayload::WORD_BYTES];
            bytes.copy_from_slice(chunk);
            put_u64(out, u64::from_le_bytes(bytes));
        }
    }

    fn write_pools(&self, _f: &mut Vec<f32>, i: &mut Vec<i64>) {
        i.push(i64::from(self.lod));
        i.push(self.rect_count() as i64);
        for chunk in self.rects.chunks_exact(RectListPayload::WORD_BYTES) {
            let mut bytes = [0u8; RectListPayload::WORD_BYTES];
            bytes.copy_from_slice(chunk);
            i.push(u64::from_le_bytes(bytes) as i64);
        }
    }

    fn read_body(
        r: &mut Reader<'_>,
        _f: &mut Vec<f32>,
        i: &mut Vec<i64>,
    ) -> Result<(), PackedError> {
        let count = r.u32()? as usize;
        let lod = r.u8()?;
        i.push(i64::from(lod));
        i.push(count as i64);
        for _ in 0..count {
            i.push(r.u64()? as i64);
        }
        Ok(())
    }

    fn from_pools(_f: &[f32], i: &[i64], _fo: usize, io: usize) -> Self {
        // 脏池 / 越界 offset 一律退化为空载荷，绝不 panic。
        let Some(header_end) = io.checked_add(2) else {
            return Self::default();
        };
        let Some(header) = i.get(io..header_end) else {
            return Self::default();
        };
        let lod = header[0] as u8;
        let count = header[1] as usize;
        let Some(words_end) = header_end.checked_add(count) else {
            return Self::default();
        };
        let Some(words) = i.get(header_end..words_end) else {
            return Self::default();
        };
        let mut rects = Vec::with_capacity(count * RectListPayload::WORD_BYTES);
        for word in words {
            rects.extend_from_slice(&(*word as u64).to_le_bytes());
        }
        Self { lod, rects }
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
    use crate::presentation::payload::{PayloadKind, RectListPayload, SyncPayload};
    use game_engine::presentation::packed::{PackedPayload, Reader};
    use game_engine::voxel::{chunk_key, pack_rect_stream, Lod, RectBatch, RectInstance};

    fn rect(
        plane: u8,
        dir: u8,
        slice: u8,
        row: u8,
        col: u8,
        w: u8,
        h: u8,
        material: u8,
    ) -> RectInstance {
        RectInstance {
            plane,
            dir,
            slice,
            row,
            col,
            w,
            h,
            material,
        }
    }

    /// 用引擎打包器产出一段真实的 39 bit 矩形流。
    fn sample_words() -> Vec<u64> {
        let batch = RectBatch {
            origin: chunk_key(1, -2, 3),
            lod: Lod::new(0),
            rects: vec![
                rect(0, 0, 0, 0, 0, 4, 3, 7),
                rect(2, 1, 32, 31, 31, 32, 5, 255),
            ],
        };
        pack_rect_stream(&[batch])
    }

    fn body_bytes(payload: &RectListPayload) -> Vec<u8> {
        let mut out = Vec::new();
        payload.put_body(&mut out);
        out
    }

    fn decode_body(bytes: &[u8]) -> RectListPayload {
        let mut r = Reader::new(bytes);
        let mut f = Vec::new();
        let mut i = Vec::new();
        RectListPayload::read_body(&mut r, &mut f, &mut i).expect("载荷体必须能解码");
        assert!(r.u8().is_err(), "载荷体必须正好消费完，不能有尾部字节");
        RectListPayload::from_pools(&f, &i, 0, 0)
    }

    /// 非空矩形流经 GPF1 字节 / SoA 池往返后无损。
    #[test]
    fn rect_list_roundtrips_losslessly() {
        let words = sample_words();
        assert_eq!(words.len(), 2, "样例应有两个矩形");
        let payload = RectListPayload::from_stream(2, &words);
        assert_eq!(payload.rect_count(), 2);
        assert!(!payload.is_empty());

        let out = body_bytes(&payload);

        // 版本化布局自检：count u32 LE + lod u8 + count × u64 LE。
        assert_eq!(out.len(), 4 + 1 + words.len() * 8);
        assert_eq!(u32::from_le_bytes(out[0..4].try_into().unwrap()), 2);
        assert_eq!(out[4], 2);
        assert_eq!(u64::from_le_bytes(out[5..13].try_into().unwrap()), words[0]);

        let decoded = decode_body(&out);
        assert_eq!(decoded, payload);
        assert_eq!(decoded.lod, 2);
        assert_eq!(decoded.to_words(), words);
    }

    /// 空列表也必须有稳定布局：count = 0、lod 保留。
    #[test]
    fn empty_rect_list_roundtrips() {
        let payload = RectListPayload::from_stream(0, &[]);
        assert!(payload.is_empty());
        assert_eq!(payload.rect_count(), 0);

        let out = body_bytes(&payload);
        assert_eq!(out, vec![0, 0, 0, 0, 0], "空列表 = count 0 + lod 0");

        let decoded = decode_body(&out);
        assert_eq!(decoded, payload);
        assert_eq!(decoded.to_words(), Vec::<u64>::new());
    }

    /// 同一输入两次编码 -> 逐字节一致；解码后再编码也一致。
    #[test]
    fn rect_list_bytes_are_deterministic() {
        let words = sample_words();
        let a = RectListPayload::from_stream(3, &words);
        let b = RectListPayload::from_stream(3, &words);
        let bytes_a = body_bytes(&a);
        let bytes_b = body_bytes(&b);
        assert_eq!(bytes_a, bytes_b, "同一输入必须产出逐字节一致的载荷");

        let mut again = Vec::new();
        decode_body(&bytes_a).put_body(&mut again);
        assert_eq!(bytes_a, again, "解码 -> 再编码必须逐字节一致");
    }

    /// 宏生成的 SyncPayload 分发也要能往返（kind 码 + 载荷体）。
    #[test]
    fn rect_list_sync_payload_roundtrips() {
        let words = sample_words();
        let payload = SyncPayload::RectList(RectListPayload::from_stream(1, &words));

        let mut out = Vec::new();
        payload.put(&mut out);
        assert_eq!(
            out[0],
            PayloadKind::RectList.code(),
            "载荷体前必须是 kind 码 7"
        );
        assert_eq!(out[0], 7);

        let mut r = Reader::new(&out);
        let mut f = Vec::new();
        let mut i = Vec::new();
        let kind = SyncPayload::read_into(&mut r, &mut f, &mut i).expect("必须能解码");
        assert_eq!(kind, PayloadKind::RectList);
        let decoded = SyncPayload::from_pools(kind, &f, &i, 0, 0);
        assert_eq!(decoded, payload);
    }
}
