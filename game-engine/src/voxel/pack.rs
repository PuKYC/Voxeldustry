//! 矩形实例打包（路径 B）。
//!
//! ## 8 B/矩形 位布局（小端 u64）
//!
//! bit  [0,3)   orientation = plane * 2 + dir   (3 bit, 0..5)
//! bit  [3,9)   slice                           (6 bit, 0..32)
//! bit  [9,14)  row                             (5 bit, 0..31)
//! bit [14,19)  col                             (5 bit, 0..31)
//! bit [19,25)  w                               (6 bit, 1..32)
//! bit [25,31)  h                               (6 bit, 1..32)
//! bit [31,39)  material                        (8 bit)
//! bit [39,47)  ao                              (4 角 × 2 bit, 0..=3)
//! bit [47,64)  0（保留）
//!
//! ## AO 扩展位（bit [39,47)）
//!
//! 39 bit 描述符本身不变：AO 只用原本保留的高位 [39,47)。旧生产端写 0，旧消费端
//! 只读 [0,39)，所以这是向后兼容的纯扩展，不是格式变更。
//! 每角 2 bit、范围 0..=3（3 = 全亮）：bit 39 + 2*i。角序 i 与 voxel::mesh 的
//! compute_face_ao 以及 game-core presentation::voxel_mesh::rect_corner / 着色器 UV
//! 完全一致：i = cx | (cy << 1)，cx 沿 col（w）方向、cy 沿 row（h）方向。
//!
//! orientation 只把 plane({0,1,2}) 与 dir({0,1}) 折成 3 bit；矩形是位置无关的
//! 整数描述符，不含 body/island 变换，也不含 voxel_size。顺序由
//! pack_rect_stream 固定为 (ChunkKey, plane, dir, material, slice, row, col)
//! （确定性）。

use voxel::mesh::{AoRectBatch, RectBatch, RectInstance};

/// 各字段位宽（bit）。
pub const ORIENTATION_BITS: u32 = 3;
pub const SLICE_BITS: u32 = 6;
pub const ROW_BITS: u32 = 5;
pub const COL_BITS: u32 = 5;
pub const W_BITS: u32 = 6;
pub const H_BITS: u32 = 6;
pub const MATERIAL_BITS: u32 = 8;
/// 矩形描述符总位数（不含保留位）= 39。
pub const RECT_BITS: u32 =
    ORIENTATION_BITS + SLICE_BITS + ROW_BITS + COL_BITS + W_BITS + H_BITS + MATERIAL_BITS;

const ORIENTATION_SHIFT: u32 = 0;
const SLICE_SHIFT: u32 = ORIENTATION_SHIFT + ORIENTATION_BITS;
const ROW_SHIFT: u32 = SLICE_SHIFT + SLICE_BITS;
const COL_SHIFT: u32 = ROW_SHIFT + ROW_BITS;
const W_SHIFT: u32 = COL_SHIFT + COL_BITS;
const H_SHIFT: u32 = W_SHIFT + W_BITS;
const MATERIAL_SHIFT: u32 = H_SHIFT + H_BITS;

/// AO 起始位：紧跟 39 bit 描述符（bit 39）。
pub const AO_SHIFT: u32 = RECT_BITS;
/// 每角 AO 的位宽（2 bit，值 0..=3）。
pub const AO_CORNER_BITS: u32 = 2;
/// 四角 AO 总位宽（8 bit）。
pub const AO_BITS: u32 = 4 * AO_CORNER_BITS;
/// 完整字（39 bit 矩形 + 4 角 AO）的位数 = 47。
pub const WORD_BITS: u32 = RECT_BITS + AO_BITS;

/// orientation = plane * 2 + dir（3 bit，0..5）。
#[inline]
pub fn orientation_of(rect: &RectInstance) -> u64 {
    ((rect.plane as u64) & 0b11) * 2 + ((rect.dir as u64) & 0b1)
}

/// 把一个矩形描述符打包成 39 bit 的 u64。
#[inline]
pub fn pack_rect(rect: &RectInstance) -> u64 {
    (orientation_of(rect) << ORIENTATION_SHIFT)
        | (((rect.slice as u64) & 0b11_1111) << SLICE_SHIFT)
        | (((rect.row as u64) & 0b1_1111) << ROW_SHIFT)
        | (((rect.col as u64) & 0b1_1111) << COL_SHIFT)
        | (((rect.w as u64) & 0b11_1111) << W_SHIFT)
        | (((rect.h as u64) & 0b11_1111) << H_SHIFT)
        | (((rect.material as u64) & 0xFF) << MATERIAL_SHIFT)
}

/// 从 39 bit u64 还原矩形描述符。
#[inline]
pub fn unpack_rect(word: u64) -> RectInstance {
    let orientation = ((word >> ORIENTATION_SHIFT) & 0b111) as u8;
    RectInstance {
        plane: orientation / 2,
        dir: orientation % 2,
        slice: ((word >> SLICE_SHIFT) & 0b11_1111) as u8,
        row: ((word >> ROW_SHIFT) & 0b1_1111) as u8,
        col: ((word >> COL_SHIFT) & 0b1_1111) as u8,
        w: ((word >> W_SHIFT) & 0b11_1111) as u8,
        h: ((word >> H_SHIFT) & 0b11_1111) as u8,
        material: ((word >> MATERIAL_SHIFT) & 0xFF) as u8,
    }
}

/// 把 4 角 AO（每角 0..=3）打包进 bit [39,47)。
///
/// 角序 i = cx | (cy << 1)，与 voxel::mesh::compute_face_ao 的返回顺序一致。
#[inline]
pub fn pack_ao(ao: &[u8; 4]) -> u64 {
    let mut word = 0u64;
    let mut i = 0u32;
    while i < 4 {
        word |= ((ao[i as usize] & 0b11) as u64) << (AO_SHIFT + AO_CORNER_BITS * i);
        i += 1;
    }
    word
}

/// 从 bit [39,47) 取回 4 角 AO（每角 0..=3）。
#[inline]
pub fn unpack_ao(word: u64) -> [u8; 4] {
    let mut ao = [0u8; 4];
    let mut i = 0u32;
    while i < 4 {
        ao[i as usize] = ((word >> (AO_SHIFT + AO_CORNER_BITS * i)) & 0b11) as u8;
        i += 1;
    }
    ao
}

/// 39 bit 矩形 + 4 角 AO 组成一个完整的 8 B 字。
#[inline]
pub fn pack_rect_with_ao(rect: &RectInstance, ao: &[u8; 4]) -> u64 {
    pack_rect(rect) | pack_ao(ao)
}

/// 打包单个 RectBatch，按 (plane, dir, material, slice, row, col) 排序。
pub fn pack_rect_batch(batch: &RectBatch) -> Vec<u64> {
    pack_rect_stream(std::slice::from_ref(batch))
}

/// 把多个 RectBatch 打包成确定的 8 B/矩形 u64 流。
///
/// 全局按 (ChunkKey, plane, dir, material, slice, row, col, packed) 排序：前 7
/// 项是设计要求，最后以 packed 位模式做全序 tie-break，保证出现重复
/// 键时也逐位确定。
pub fn pack_rect_stream(batches: &[RectBatch]) -> Vec<u64> {
    let mut keyed: Vec<(i32, i32, i32, u8, u8, u8, u8, u8, u8, u64)> = Vec::new();
    for batch in batches {
        let o = batch.origin;
        for rect in &batch.rects {
            keyed.push((
                o.x,
                o.y,
                o.z,
                rect.plane,
                rect.dir,
                rect.material,
                rect.slice,
                rect.row,
                rect.col,
                pack_rect(rect),
            ));
        }
    }
    keyed.sort_unstable();
    keyed.into_iter().map(|t| t.9).collect()
}

/// 打包单个 AoRectBatch（39 bit + 4 角 AO）。
pub fn pack_rect_batch_with_ao(batch: &AoRectBatch) -> Vec<u64> {
    pack_rect_stream_with_ao(std::slice::from_ref(batch))
}

/// 把多个 AoRectBatch 打包成确定的 8 B/矩形 u64 流（含 AO）。
///
/// 排序主键与 pack_rect_stream 完全一致；主键相同而 AO 不同时用完整 47 bit 字做
/// tie-break。AO 全 0 时输出与 pack_rect_stream 逐位相同（向后兼容）。
pub fn pack_rect_stream_with_ao(batches: &[AoRectBatch]) -> Vec<u64> {
    let mut keyed: Vec<(i32, i32, i32, u8, u8, u8, u8, u8, u8, u64)> = Vec::new();
    for batch in batches {
        let o = batch.origin;
        for (i, rect) in batch.rects.iter().enumerate() {
            let ao = batch.ao.get(i).copied().unwrap_or([0u8; 4]);
            keyed.push((
                o.x,
                o.y,
                o.z,
                rect.plane,
                rect.dir,
                rect.material,
                rect.slice,
                rect.row,
                rect.col,
                pack_rect_with_ao(rect, &ao),
            ));
        }
    }
    keyed.sort_unstable();
    keyed.into_iter().map(|t| t.9).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voxel::key::chunk_key;
    use voxel::store::Lod;

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

    fn fields(r: &RectInstance) -> (u8, u8, u8, u8, u8, u8, u8, u8) {
        (r.plane, r.dir, r.slice, r.row, r.col, r.w, r.h, r.material)
    }

    fn batch(x: i32, rects: Vec<RectInstance>) -> RectBatch {
        RectBatch {
            origin: chunk_key(x, 0, 0),
            lod: Lod::new(0),
            rects,
        }
    }

    #[test]
    fn pack_stream_is_deterministic_and_roundtrips() {
        let batches = vec![
            batch(
                1,
                vec![
                    rect(0, 0, 0, 0, 0, 4, 3, 7),
                    rect(1, 1, 32, 31, 31, 1, 1, 200),
                ],
            ),
            batch(0, vec![rect(2, 0, 5, 2, 3, 32, 1, 9)]),
        ];

        let a = pack_rect_stream(&batches);
        let b = pack_rect_stream(&batches);
        assert_eq!(a, b, "two runs must be byte-identical");
        assert_eq!(a.len(), 3);

        // 每个矩形字段 roundtrip。
        let full = rect(2, 1, 32, 31, 31, 32, 32, 255);
        assert_eq!(fields(&unpack_rect(pack_rect(&full))), fields(&full));

        // 多重集相等（排序顺序可能改变）。
        let mut want: Vec<(u8, u8, u8, u8, u8, u8, u8, u8)> = batches
            .iter()
            .flat_map(|b| b.rects.iter().map(fields))
            .collect();
        let mut got: Vec<(u8, u8, u8, u8, u8, u8, u8, u8)> =
            a.iter().map(|w| fields(&unpack_rect(*w))).collect();
        want.sort_unstable();
        got.sort_unstable();
        assert_eq!(want, got);
    }

    #[test]
    fn pack_matches_mesh_crate_layout() {
        // game-engine 的打包必须与 crates/voxel 的 RectInstance::to_bits 完全一致。
        let cases = [
            rect(0, 0, 0, 0, 0, 1, 1, 0),
            rect(1, 1, 17, 3, 5, 7, 9, 42),
            rect(2, 0, 32, 31, 31, 32, 32, 255),
        ];
        for r in cases.iter() {
            let word = pack_rect(r);
            assert_eq!(word, r.to_bits());
            assert_eq!(unpack_rect(word), RectInstance::from_bits(word));
            assert_eq!(unpack_rect(word), *r);
        }
    }

    #[test]
    fn pack_fits_39_bits() {
        let full = rect(2, 1, 32, 31, 31, 32, 32, 255);
        let w = pack_rect(&full);
        assert_eq!(w >> RECT_BITS, 0, "top bits must be zero");
        assert_eq!(w & !((1u64 << RECT_BITS) - 1), 0);
        // orientation 用满 3 bit（5 = 0b101）；slice 32 用满 6 bit。
        assert_eq!((w >> ORIENTATION_SHIFT) & 0b111, 5);
        assert_eq!((w >> SLICE_SHIFT) & 0b11_1111, 32);
        // 不同矩形不碰撞。
        assert_ne!(
            pack_rect(&full),
            pack_rect(&rect(2, 1, 32, 31, 31, 32, 32, 254))
        );
    }

    #[test]
    fn ao_lives_in_reserved_bits_and_keeps_39_bits_intact() {
        let r = rect(2, 1, 32, 31, 31, 32, 32, 255);
        let base = pack_rect(&r);
        assert_eq!(base >> RECT_BITS, 0, "39 bit 描述符必须不碰高位");
        for ao in [[0u8; 4], [1, 2, 3, 0], [3, 3, 3, 3], [0, 0, 0, 1]] {
            let word = pack_rect_with_ao(&r, &ao);
            assert_eq!(word >> WORD_BITS, 0, "只允许写到 bit [39,47)");
            assert_eq!(
                word & ((1u64 << RECT_BITS) - 1),
                base,
                "39 bit 必须逐位不变"
            );
            assert_eq!(unpack_ao(word), ao, "AO 必须无损还原");
            assert_eq!(fields(&unpack_rect(word)), fields(&r), "解出的矩形不变");
        }
    }

    #[test]
    fn ao_corner_bit_order_is_cx_cy() {
        assert_eq!(pack_ao(&[1, 0, 0, 0]), 1u64 << 39);
        assert_eq!(pack_ao(&[0, 1, 0, 0]), 1u64 << 41);
        assert_eq!(pack_ao(&[0, 0, 1, 0]), 1u64 << 43);
        assert_eq!(pack_ao(&[0, 0, 0, 1]), 1u64 << 45);
        assert_eq!(unpack_ao(pack_ao(&[3, 2, 1, 0])), [3, 2, 1, 0]);
    }

    #[test]
    fn ao_stream_is_deterministic_and_zero_ao_matches_rect_stream() {
        let a = AoRectBatch {
            origin: chunk_key(0, 0, 0),
            lod: Lod::new(0),
            rects: vec![rect(0, 0, 1, 2, 3, 4, 5, 6), rect(0, 0, 1, 2, 3, 4, 5, 6)],
            ao: vec![[3, 3, 3, 3], [0, 1, 2, 3]],
        };
        let words = pack_rect_stream_with_ao(std::slice::from_ref(&a));
        assert_eq!(
            words,
            pack_rect_stream_with_ao(std::slice::from_ref(&a)),
            "两次打包必须逐位一致"
        );
        // 完整 47 bit 字做全序：AO [0,1,2,3] 比全 3 小，所以排在前。
        assert_eq!(unpack_ao(words[0]), [0, 1, 2, 3]);
        assert_eq!(unpack_ao(words[1]), [3, 3, 3, 3]);
        assert!(words[0] < words[1]);
        assert_eq!(unpack_rect(words[0]), unpack_rect(words[1]));

        let zero = AoRectBatch {
            origin: a.origin,
            lod: a.lod,
            rects: a.rects.clone(),
            ao: vec![[0u8; 4]; a.rects.len()],
        };
        let plain = RectBatch {
            origin: a.origin,
            lod: a.lod,
            rects: a.rects.clone(),
        };
        assert_eq!(
            pack_rect_stream_with_ao(std::slice::from_ref(&zero)),
            pack_rect_stream(std::slice::from_ref(&plain)),
            "AO=0 时新流必须与旧流逐位相同"
        );
    }
}
