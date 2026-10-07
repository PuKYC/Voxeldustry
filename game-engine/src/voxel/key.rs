//! 整数体素寻址工具。
//!
//! 集中构造 ChunkKey，避免字段访问散落多处；上游可见性若改变只需改本文件。

use voxel::store::{ChunkKey, Lod, CHUNK_DEPTH};

/// 构造一个子块坐标。
#[inline]
pub fn chunk_key(x: i32, y: i32, z: i32) -> ChunkKey {
    ChunkKey::new(x, y, z)
}

/// 一个 base 子块每轴的体素数（2^CHUNK_DEPTH；v1 = 32）。
#[inline]
pub fn chunk_voxels_per_axis() -> i32 {
    1i32 << CHUNK_DEPTH
}

/// 6 邻（+x/-x/+y/-y/+z/-z），顺序固定 -> 确定性。
#[inline]
pub fn neighbors6(key: ChunkKey) -> [ChunkKey; 6] {
    [
        chunk_key(key.x + 1, key.y, key.z),
        chunk_key(key.x - 1, key.y, key.z),
        chunk_key(key.x, key.y + 1, key.z),
        chunk_key(key.x, key.y - 1, key.z),
        chunk_key(key.x, key.y, key.z + 1),
        chunk_key(key.x, key.y, key.z - 1),
    ]
}

/// 整数 floor 除法（向下取整，负数也正确）。
#[inline]
pub fn floor_div(a: i32, b: i32) -> i32 {
    a.div_euclid(b)
}

/// LOD lod 的 mesh 块每轴子块数 = 2^lod。
#[inline]
pub fn lod_block_span(lod: Lod) -> i32 {
    1i32 << lod.lod()
}

/// 覆盖 key 的、LOD lod 的 mesh 块原点（各轴向下取整到 2^lod 的倍数）。
#[inline]
pub fn lod_block_origin(key: ChunkKey, lod: Lod) -> ChunkKey {
    let s = lod_block_span(lod);
    chunk_key(
        floor_div(key.x, s) * s,
        floor_div(key.y, s) * s,
        floor_div(key.z, s) * s,
    )
}
