//! 体素碰撞几何构建基准：全体素 vs 外壳。
//!
//! 运行（需 physics + voxel）：
//!   cargo bench -p game-engine --features voxel,physics --bench voxel_collider
//!
//! 关注：collect_* 的枚举耗时、构造 Collider 的耗时，以及临时 Vec 的长度
//! （12 B/体素 -> 内存）。parry Voxels 本体内存是位域，两种模式存下来差别不大，
//! 真正的内存差在调用方 materialize 的 Vec。

use avian3d::math::Vector;
use bevy::math::IVec3;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use game_engine::math::FixedPoint;
use game_engine::voxel::{
    build_chunk_voxels, build_voxel_collider, collect_chunk_solid_voxels, collect_solid_voxels,
    collect_surface_voxels, voxel_size_meters_f32, ChunkKey, Collider, MaxDepth, VoxOpsWrite,
    VoxTree, VoxVolume, VoxelInterner, CHUNK_DEPTH,
};

/// 3 个 chunk 的确定性“地形”（高度函数），既有内部体素也有暴露面。
fn terrain_volume(interner: &mut VoxelInterner) -> VoxVolume {
    let mut volume = VoxVolume::new(FixedPoint::from_num(1));
    let c = 32;
    for key in [
        ChunkKey::new(0, 0, 0),
        ChunkKey::new(1, 0, 0),
        ChunkKey::new(0, 0, 1),
    ] {
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
        for x in 0..c {
            for z in 0..c {
                let h = 8 + ((x * 7 + z * 13) % 12);
                for y in 0..h {
                    let _ = tree.set(interner.inner_mut(), IVec3::new(x, y, z), 1);
                }
            }
        }
        volume.insert_chunk(key, tree);
    }
    volume
}

fn bench_voxel_collider(c: &mut Criterion) {
    let mut interner = VoxelInterner::new(64 * 1024 * 1024);
    let volume = terrain_volume(&mut interner);

    let solid = collect_solid_voxels(&volume, interner.inner()).len();
    let surface = collect_surface_voxels(&volume, interner.inner()).len();
    println!(
        "voxels: solid={solid} surface={surface} | solid_vec_bytes={} surface_vec_bytes={}",
        solid * 12,
        surface * 12
    );

    c.bench_function("collect_solid_voxels", |b| {
        b.iter(|| black_box(collect_solid_voxels(&volume, interner.inner())))
    });
    c.bench_function("collect_surface_voxels", |b| {
        b.iter(|| black_box(collect_surface_voxels(&volume, interner.inner())))
    });
    c.bench_function("collider_solid", |b| {
        b.iter(|| black_box(build_voxel_collider(&volume, interner.inner())))
    });
    c.bench_function("collider_surface", |b| {
        b.iter(|| {
            let coords = collect_surface_voxels(&volume, interner.inner());
            let scale = voxel_size_meters_f32(volume.voxel_size);
            black_box(Collider::voxels(Vector::splat(scale), &coords))
        })
    });

    // 引擎实际路径：逐 chunk 收集 + 构建 parry Voxels（不经过整 body Collider）。
    let chunk_keys: Vec<ChunkKey> = volume.chunks.keys().copied().collect();
    let per_chunk_solid: usize = chunk_keys
        .iter()
        .map(|&key| collect_chunk_solid_voxels(&volume, interner.inner(), key).len())
        .sum();
    println!(
        "voxels: per_chunk_solid={per_chunk_solid} chunks={}",
        chunk_keys.len()
    );

    c.bench_function("collect_chunk_solid_voxels", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for &key in chunk_keys.iter() {
                total +=
                    black_box(collect_chunk_solid_voxels(&volume, interner.inner(), key)).len();
            }
            black_box(total);
        })
    });
    c.bench_function("build_chunk_voxels", |b| {
        b.iter(|| {
            for &key in chunk_keys.iter() {
                let voxels = black_box(build_chunk_voxels(&volume, interner.inner(), key));
                black_box(voxels.is_some());
            }
        })
    });
}

criterion_group!(benches, bench_voxel_collider);
criterion_main!(benches);
