//! 地形基准（指令数版）：用 gungraun + Valgrind Callgrind 统计**执行的指令数**。
//!
//! 与 `voxel_terrain.rs`（criterion，墙钟时间）覆盖同一批路径，区别在度量方式：
//! 指令数与机器负载、频率、邻居进程无关，同一份代码/工具链跑出来完全一致，
//! 因此结果可以跨 runner、跨时间直接比较，适合当 CI 门禁。
//!
//! 覆盖：
//!   - `bench_mesh_pack`：LOD0 / LOD3 的 mesh 提取 + 打包
//!   - `bench_pack`：只测打包（输入预先 mesh 好）
//!   - `bench_lod_partition`：LOD 块划分（纯计算）
//!
//! 运行（需要 valgrind）：
//!   cargo bench -p game-core --bench terrain_inst
//!   cargo bench -p game-core --bench terrain_inst -- --baseline=ci
//!
//! 首次带 `--baseline=ci` 会创建基线；之后每次都与它比较，指令数涨幅超过默认
//! 软阈值（Ir +10%）时以退出码 3 失败。
//!
//! 注意：setup 在被测区间之外执行，所以地形生成的成本不会算进基准。

use std::collections::BTreeMap;
use std::hint::black_box;

use gungraun::{library_benchmark, library_benchmark_group, main};

use game_core::static_data::voxel::DEFAULT_BIOME;
use game_core::voxel::terrain::blocks::lod_blocks;
use game_core::voxel::terrain::generation::WorldGenerator;
use game_core::voxel::terrain::neighbor_origins;
use game_core::world::WorldSeed;
use game_engine::presentation::voxel::{mesh_block_wrapped, pack_rect_batch, WrappedBlockCache};
use game_engine::voxel::{
    chunk_key, lod_block_origin, ChunkKey, Lod, MeshBlock, RectBatch, VoxInterner, VoxTree,
};

/// 生成子块的范围（覆盖 lod3 的块 + 一圈 margin），与 criterion 版一致。
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

/// 预热本块缓存（own）+ 6 邻缓存（neighbors）。
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

/// 被测块：包含地表子块 (0, 1, 0) 的那个对齐块，与 criterion 版选择规则一致。
fn surface_block(lod: u8) -> ChunkKey {
    lod_block_origin(chunk_key(0, 1, 0), Lod::new(lod))
}

struct MeshState {
    chunks: BTreeMap<ChunkKey, VoxTree<u8>>,
    interner: VoxInterner<u8>,
    own: WrappedBlockCache,
    neighbors: WrappedBlockCache,
    mesh_block: MeshBlock,
    origin: ChunkKey,
    lod: u8,
}

fn mesh_state(lod: u8) -> MeshState {
    let mut interner = VoxInterner::<u8>::with_memory_budget(16 * 1024 * 1024);
    let chunks = build_chunks(&mut interner);
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
    MeshState {
        chunks,
        interner,
        own,
        neighbors,
        mesh_block: MeshBlock::new(origin, Lod::new(lod)),
        origin,
        lod,
    }
}

#[library_benchmark(setup = mesh_state)]
#[bench::lod0(0u8)]
#[bench::lod3(3u8)]
fn bench_mesh_pack(mut state: MeshState) {
    let external = external_of(&state.neighbors, state.origin, state.lod);
    let batch = mesh_block_wrapped(
        black_box(&state.chunks),
        &mut state.interner,
        &mut state.own,
        state.mesh_block,
        external,
    );
    black_box(pack_rect_batch(&batch.into_batch()));
}

struct PackState {
    batch: RectBatch,
}

fn pack_state(lod: u8) -> PackState {
    let mut state = mesh_state(lod);
    let external = external_of(&state.neighbors, state.origin, state.lod);
    let batch: RectBatch = mesh_block_wrapped(
        &state.chunks,
        &mut state.interner,
        &mut state.own,
        state.mesh_block,
        external,
    )
    .into_batch();
    assert!(!batch.rects.is_empty(), "pack 基准输入必须非空");
    PackState { batch }
}

#[library_benchmark(setup = pack_state)]
#[bench::lod0(0u8)]
#[bench::lod3(3u8)]
fn bench_pack(state: PackState) {
    black_box(pack_rect_batch(black_box(&state.batch)));
}

#[library_benchmark]
#[bench::radius8(8i32)]
fn bench_lod_partition(radius: i32) {
    let voxel_size_m = 0.45;
    black_box(lod_blocks(black_box(radius), 3, black_box(voxel_size_m)));
}

library_benchmark_group!(
    name = terrain_inst,
    benchmarks = [bench_mesh_pack, bench_pack, bench_lod_partition]
);

main!(library_benchmark_groups = terrain_inst);
