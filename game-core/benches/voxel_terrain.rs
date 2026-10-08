//! 体素地形基准：LOD 划分 / 网格化 + 打包（criterion，无需 Bevy App）。
//!
//! 输入用 game-core 的真实生成器（`WorldGenerator`，固定 seed + margin 一圈），
//! 生成成本在被测闭包之外，所以测的是路径 B 的生产管线本身：
//! `WrappedBlockCache`（wrap_block）+ `mesh_block_wrapped`
//! （extract_block_tree_with_ao）+ `pack_rect_batch`。
//!
//! 手动运行：`cargo bench -p game-core --bench voxel_terrain`

use std::collections::BTreeMap;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use game_engine::presentation::voxel::{mesh_block_wrapped, pack_rect_batch, WrappedBlockCache};
use game_engine::voxel::{
    chunk_key, lod_block_origin, ChunkKey, Lod, MeshBlock, RectBatch, VoxInterner, VoxTree,
};

use game_core::static_data::voxel::DEFAULT_BIOME;
use game_core::voxel::terrain::blocks::lod_blocks;
use game_core::voxel::terrain::generation::WorldGenerator;
use game_core::voxel::terrain::neighbor_origins;
use game_core::world::WorldSeed;

/// 生成子块的范围（覆盖 lod3 的块 + 一圈 margin）。
///
/// y 只到地形带（world y ∈ [-32, 128) 体素）就够了：地表在 y ≈ 56..76。
const BENCH_RADIUS_CHUNKS: i32 = 8;
const BENCH_Y_MIN: i32 = -1;
const BENCH_Y_MAX: i32 = 3; // exclusive

/// 固定 seed 的真实地形子块（确定性）。
fn build_chunks(interner: &mut VoxInterner<u8>) -> BTreeMap<ChunkKey, VoxTree<u8>> {
    let generator = WorldGenerator::new(WorldSeed(0xC0FF_EE), DEFAULT_BIOME);
    let mut chunks = BTreeMap::new();
    for x in -1..=BENCH_RADIUS_CHUNKS {
        for z in -1..=BENCH_RADIUS_CHUNKS {
            for y in BENCH_Y_MIN..BENCH_Y_MAX {
                let key = chunk_key(x, y, z);
                chunks.insert(key, generator.generate_chunk(interner, key));
            }
        }
    }
    chunks
}

/// 预热一个 LOD 块的本块缓存（own）+ 6 邻缓存（neighbors）。
///
/// 必须分两个缓存：`mesh_block_wrapped` 要 `&mut` 本块缓存，而 external
/// 只读借用邻块树（与生产路径一致）。
fn warm(
    chunks: &BTreeMap<ChunkKey, VoxTree<u8>>,
    interner: &mut VoxInterner<u8>,
    own: &mut WrappedBlockCache,
    neighbors: &mut WrappedBlockCache,
    origin: ChunkKey,
    lod: u8,
) {
    let lod = Lod::new(lod);
    own.get_or_wrap(chunks, interner, MeshBlock::new(origin, lod));
    for neighbor in neighbor_origins(origin, lod.lod()) {
        neighbors.get_or_wrap(chunks, interner, MeshBlock::new(neighbor, lod));
    }
}

fn external_of(
    neighbors: &WrappedBlockCache,
    origin: ChunkKey,
    lod: u8,
) -> [Option<&VoxTree<u8>>; 6] {
    let mut out: [Option<&VoxTree<u8>>; 6] = [None; 6];
    for (face, neighbor) in neighbor_origins(origin, lod).iter().enumerate() {
        out[face] = neighbors.get(*neighbor, Lod::new(lod));
    }
    out
}

/// 被测块：包含地表子块 (0, 1, 0)（world y ∈ [32, 64)）的那个对齐块。
///
/// 用 `lod_block_origin` 取，和生产 LOD 划分的选择规则一致；直接用原点块
/// 会取到全实心地下块（0 个暴露面），测不出 meshing。
fn surface_block(lod: u8) -> ChunkKey {
    lod_block_origin(chunk_key(0, 1, 0), Lod::new(lod))
}

fn bench_mesh_pack(c: &mut Criterion) {
    let mut interner = VoxInterner::<u8>::with_memory_budget(16 * 1024 * 1024);
    let chunks = build_chunks(&mut interner);

    let mut group = c.benchmark_group("terrain_mesh_pack");
    for lod in 0..=3u8 {
        let origin = surface_block(lod);
        let mut own = WrappedBlockCache::default();
        let mut neighbors = WrappedBlockCache::default();
        warm(
            &chunks,
            &mut interner,
            &mut own,
            &mut neighbors,
            origin,
            lod,
        );
        let mesh_block = MeshBlock::new(origin, Lod::new(lod));
        let external = external_of(&neighbors, origin, lod);
        group.bench_with_input(BenchmarkId::from_parameter(lod), &lod, |b, _| {
            b.iter(|| {
                let batch = mesh_block_wrapped(
                    black_box(&chunks),
                    &mut interner,
                    &mut own,
                    mesh_block,
                    external,
                );
                black_box(pack_rect_batch(&batch.into_batch()))
            });
        });
    }
    group.finish();
}

fn bench_pack_only(c: &mut Criterion) {
    let mut interner = VoxInterner::<u8>::with_memory_budget(16 * 1024 * 1024);
    let chunks = build_chunks(&mut interner);

    let mut group = c.benchmark_group("terrain_pack");
    for lod in 0..=3u8 {
        let origin = surface_block(lod);
        let mut own = WrappedBlockCache::default();
        let mut neighbors = WrappedBlockCache::default();
        warm(
            &chunks,
            &mut interner,
            &mut own,
            &mut neighbors,
            origin,
            lod,
        );
        let mesh_block = MeshBlock::new(origin, Lod::new(lod));
        let external = external_of(&neighbors, origin, lod);
        let batch: RectBatch =
            mesh_block_wrapped(&chunks, &mut interner, &mut own, mesh_block, external).into_batch();
        debug_assert!(!batch.rects.is_empty(), "pack 基准输入必须非空");
        group.bench_with_input(BenchmarkId::from_parameter(lod), &lod, |b, _| {
            b.iter(|| black_box(pack_rect_batch(black_box(&batch))));
        });
    }
    group.finish();
}

fn bench_lod_partition(c: &mut Criterion) {
    let voxel_size_m = 0.45;
    let mut group = c.benchmark_group("terrain_lod_partition");
    for radius in [2i32, 5, 8] {
        group.bench_with_input(
            BenchmarkId::from_parameter(radius),
            &radius,
            |b, &radius| {
                b.iter(|| black_box(lod_blocks(radius, 3, voxel_size_m)));
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_mesh_pack,
    bench_pack_only,
    bench_lod_partition
);
criterion_main!(benches);
