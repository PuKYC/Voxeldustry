//! 体素网格的 GPU 几何契约（体素表现唯一通道 = 原始体素 halo）。
//!
//! ## 分层
//!
//! - 世界层 `game-engine::voxel` 提供 `VoxVolume`；表现层机制
//!   `game_engine::presentation::voxel::extract_raw_halo` 产出 34³ halo 缓冲，
//!   Godot 侧再调 gdext mesher（`game_core::presentation::mesh_raw_halo`）现算
//!   8 B/矩形的 39 bit u64 流（低 39 bit 是描述符，bit [39,47) 是 4 角 AO，
//!   见 [ao_corner_index]）；
//! - 本模块把「一个 32³ mesh 块的原始体素缓冲」定义成可由表现管线承载的组件
//!   [VoxChunkRaw]，并给出 rect -> 四边形角点 / 法线的**整数几何契约**；
//! - Godot 侧的 voxel_rect.gdshader 用完全相同的位布局与轴映射逐顶点展开，
//!   本模块的 rect_corner / rect_normal 就是那份着色器数学的 Rust 镜像，
//!   由单测锁死（没有 Godot 可跑时，这是唯一能自动验证的几何真相源）。
//!
//! ## 轴映射（与 crates/voxel 的 RectInstance 语义逐条对应）
//!
//!   plane 0 (YZ): rows=y, cols=z -> 角点 = (slice, row + cy*h, col + cx*w)
//!   plane 1 (XZ): rows=z, cols=x -> 角点 = (col + cx*w, slice, row + cy*h)
//!   plane 2 (XY): rows=y, cols=x -> 角点 = (col + cx*w, row + cy*h, slice)
//!
//! slice **已经是面坐标**（DIR_POS -> layer + 1，DIR_NEG -> layer）；
//! dir = 0 是 DIR_POS（+轴法线），dir = 1 是 DIR_NEG（-轴法线）。
//!
//! 矩形是位置无关的：块原点（米）由节点的 Transform 载荷给出，LOD 缩放
//! voxel_size * 2^lod 由渲染侧按 RawVoxelPayload.lod 计算。

use bevy::prelude::*;
use game_engine::voxel::RectInstance;

/// 一个 mesh 块（32³ 基础跨度）的原始体素缓冲（内部 32³ + halo 层，共 34³）。
///
/// 值 = 方块 id（`static_data::voxel::BLOCK_TABLE` 的 id），0 = 空气。
/// 布局与 `game_engine::presentation::voxel::extract_raw_halo` 一致。
///
/// 这是体素表现的**唯一**通道（RawVoxels 载荷）：Godot 侧调 gdext mesher
/// 现算矩形流后交给 VoxelMeshNode 渲染。
#[derive(Component, Clone, Debug, PartialEq, Eq, Default)]
pub struct VoxChunkRaw {
    /// mesh 块的 LOD 级别（0..=3）。
    pub lod: u8,
    /// `extract_raw_halo` 产出的 34³ 方块 id 缓冲。
    pub blocks: Vec<u8>,
}

/// 一个矩形在块局部体素坐标下的 4 个角点。
///
/// 角点编号 = cx | (cy << 1)（0..4），与顶点 UV (cx, cy) 一一对应：
/// 0=(0,0) 1=(1,0) 2=(0,1) 3=(1,1)。具体顶点顺序无所谓（四边形相同），
/// 但 Rust 单测与着色器必须用同一套编号。
pub fn rect_corner(rect: RectInstance, corner: usize) -> [i32; 3] {
    let cx = (corner & 1) as i32;
    let cy = ((corner >> 1) & 1) as i32;
    let cols = i32::from(rect.col) + cx * i32::from(rect.w);
    let rows = i32::from(rect.row) + cy * i32::from(rect.h);
    let slice = i32::from(rect.slice);
    match rect.plane {
        0 => [slice, rows, cols], // YZ: rows=y, cols=z
        1 => [cols, slice, rows], // XZ: rows=z, cols=x
        _ => [cols, rows, slice], // XY: rows=y, cols=x
    }
}

/// 面法线（整数，单位轴）。dir = 0 是 DIR_POS（+轴），dir = 1 是 DIR_NEG。
pub fn rect_normal(rect: RectInstance) -> [i32; 3] {
    let sign = if rect.dir == 0 { 1 } else { -1 };
    match rect.plane {
        0 => [sign, 0, 0],
        1 => [0, sign, 0],
        _ => [0, 0, sign],
    }
}

/// AO 角点编号（0..4）与 [rect_corner] 的 UV 一一对应：i = cx | (cy << 1)。
///
/// cx 是 UV.x（沿 col / w 方向），cy 是 UV.y（沿 row / h 方向）。这与
/// voxel::mesh::compute_face_ao 的返回顺序、以及 voxel_rect.gdshader 里
/// int(cx) | (int(cy) << 1) 完全一致（由 Rust 单测锁死，见本模块 tests）。
pub const fn ao_corner_index(cx: u32, cy: u32) -> u32 {
    (cx & 1) | ((cy & 1) << 1)
}

/// 该 LOD 下每个矩形格子单位对应的米数 = voxel_size_m * 2^lod。
pub fn rect_scale_meters(voxel_size_m: f32, lod: u8) -> f32 {
    voxel_size_m * (1u32 << u32::from(lod)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::static_data::voxel::DEFAULT_VOXEL_SIZE;
    use game_engine::presentation::voxel::{pack_rect, pack_rect_with_ao, unpack_ao, unpack_rect};

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

    #[test]
    fn word_roundtrips_through_engine_layout() {
        let cases = [
            rect(0, 0, 0, 0, 0, 4, 3, 7),
            rect(1, 1, 17, 3, 5, 7, 9, 42),
            rect(2, 0, 32, 31, 31, 32, 32, 255),
        ];
        for r in cases {
            assert_eq!(unpack_rect(pack_rect(&r)), r);
        }
    }

    #[test]
    fn yz_face_corners_span_row_and_col() {
        // slice=5 的 +X 面，row=2 h=3（y），col=4 w=6（z）。
        let r = rect(0, 0, 5, 2, 4, 6, 3, 1);
        let c: Vec<[i32; 3]> = (0..4).map(|i| rect_corner(r, i)).collect();
        assert_eq!(c, vec![[5, 2, 4], [5, 2, 10], [5, 5, 4], [5, 5, 10]]);
        assert_eq!(rect_normal(r), [1, 0, 0]);
    }

    #[test]
    fn xz_face_corners_span_col_and_row() {
        // plane XZ: rows=z(h), cols=x(w)，slice 是 y。
        let r = rect(1, 1, 9, 1, 3, 5, 2, 2);
        let c: Vec<[i32; 3]> = (0..4).map(|i| rect_corner(r, i)).collect();
        assert_eq!(c, vec![[3, 9, 1], [8, 9, 1], [3, 9, 3], [8, 9, 3]]);
        assert_eq!(rect_normal(r), [0, -1, 0]);
    }

    #[test]
    fn xy_face_corners_span_col_and_row() {
        // plane XY: rows=y(h), cols=x(w)，slice 是 z。
        let r = rect(2, 0, 4, 6, 2, 3, 5, 3);
        let c: Vec<[i32; 3]> = (0..4).map(|i| rect_corner(r, i)).collect();
        assert_eq!(c, vec![[2, 6, 4], [5, 6, 4], [2, 11, 4], [5, 11, 4]]);
        assert_eq!(rect_normal(r), [0, 0, 1]);
    }

    #[test]
    fn all_six_orientations_have_unit_axis_normals() {
        for plane in 0..3u8 {
            for dir in 0..2u8 {
                let r = rect(plane, dir, 1, 1, 1, 1, 1, 0);
                let n = rect_normal(r);
                assert_eq!(n.iter().map(|x| x.abs()).sum::<i32>(), 1);
                assert_eq!(n[usize::from(plane)], if dir == 0 { 1 } else { -1 });
                let mut cs: Vec<[i32; 3]> = (0..4).map(|i| rect_corner(r, i)).collect();
                cs.sort_unstable();
                cs.dedup();
                assert_eq!(cs.len(), 4);
            }
        }
    }

    #[test]
    fn lod_scale_doubles_per_level() {
        let vs = DEFAULT_VOXEL_SIZE.to_num::<f32>();
        assert!((rect_scale_meters(vs, 0) - vs).abs() < 1e-6);
        assert!((rect_scale_meters(vs, 1) - vs * 2.0).abs() < 1e-6);
        assert!((rect_scale_meters(vs, 3) - vs * 8.0).abs() < 1e-6);
        assert!((rect_scale_meters(0.45, 0) - 0.45).abs() < 1e-6);
    }

    #[test]
    fn ao_corner_index_matches_rect_corner_uv() {
        // 着色器用 UV (cx, cy) 取 AO：i = cx | (cy << 1)，与 rect_corner 同序。
        for r in [
            rect(0, 0, 5, 2, 4, 6, 3, 1),
            rect(1, 1, 9, 1, 3, 5, 2, 2),
            rect(2, 0, 4, 6, 2, 3, 5, 3),
        ] {
            for cy in 0..2u32 {
                for cx in 0..2u32 {
                    let i = ao_corner_index(cx, cy) as usize;
                    // 手工展开的 UV 角点（与 shader 的 cols/rows 公式一致）。
                    let cols = i32::from(r.col) + cx as i32 * i32::from(r.w);
                    let rows = i32::from(r.row) + cy as i32 * i32::from(r.h);
                    let slice = i32::from(r.slice);
                    let uv_local = match r.plane {
                        0 => [slice, rows, cols],
                        1 => [cols, slice, rows],
                        _ => [cols, rows, slice],
                    };
                    assert_eq!(
                        rect_corner(r, i),
                        uv_local,
                        "UV(cx={cx},cy={cy}) 角点不一致"
                    );
                }
            }
        }
        let mut idx: Vec<u32> = (0..2)
            .flat_map(|cy| (0..2).map(move |cx| ao_corner_index(cx, cy)))
            .collect();
        idx.sort_unstable();
        assert_eq!(idx, vec![0, 1, 2, 3]);
    }

    #[test]
    fn ao_bits_do_not_disturb_the_39_bit_rect() {
        let r = rect(2, 1, 4, 6, 2, 3, 5, 200);
        let ao = [3u8, 2, 1, 0];
        let word = pack_rect_with_ao(&r, &ao);
        assert_eq!(
            unpack_rect(word),
            unpack_rect(pack_rect(&r)),
            "AO 不得改变矩形"
        );
        assert_eq!(unpack_ao(word), ao);
        // 消费端按 UV 角取 AO：每个角只读自己的 2 bit。
        assert_eq!(unpack_ao(word)[ao_corner_index(0, 0) as usize], 3);
        assert_eq!(unpack_ao(word)[ao_corner_index(1, 1) as usize], 0);
    }
}
