//! 体素碰撞几何构建（指令数版）：gungraun + Valgrind Callgrind。
//!
//! 与 `voxel_collider.rs`（criterion，墙钟时间）覆盖同一条引擎实际路径：
//! 逐 chunk 收集实心体素 + 构建 parry Voxels。指令数确定性，适合当 CI 门禁。
//!
//! 运行（需要 valgrind）：
//!   cargo bench -p game-engine --features voxel,physics --bench voxel_collider_inst
//!   cargo bench -p game-engine --features voxel,physics --bench voxel_collider_inst -- --baseline=ci

use std::hint::black_box;

use bevy::math::IVec3;
use gungraun::{library_benchmark, library_benchmark_group, main};

use game_engine::math::FixedPoint;
use game_engine::voxel::{
    build_chunk_voxels, collect_chunk_solid_voxels, ChunkKey, MaxDepth, VoxOpsWrite, VoxTree,
    VoxVolume, VoxelInterner, CHUNK_DEPTH,
};

/// 沿 x 方向铺 `chunks` 个 chunk 的确定性「地形」（高度函数），既有内部体素也有暴露面。
fn terrain_volume(interner: &mut VoxelInterner, chunks: usize) -> VoxVolume {
    let mut volume = VoxVolume::new(FixedPoint::from_num(1));
    let c = 32;
    for i in 0..chunks.max(1) {
        let key = ChunkKey::new(i as i32, 0, 0);
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

struct ColliderState {
    volume: VoxVolume,
    interner: VoxelInterner,
    keys: Vec<ChunkKey>,
}

fn collider_state(chunks: usize) -> ColliderState {
    let mut interner = VoxelInterner::new(64 * 1024 * 1024);
    let volume = terrain_volume(&mut interner, chunks);
    let keys: Vec<ChunkKey> = volume.chunks.keys().copied().collect();
    ColliderState {
        volume,
        interner,
        keys,
    }
}

#[library_benchmark(setup = collider_state)]
#[bench::three_chunks(3usize)]
fn bench_collect_chunk_solid_voxels(state: ColliderState) {
    let mut total = 0usize;
    for &key in state.keys.iter() {
        total += black_box(collect_chunk_solid_voxels(
            &state.volume,
            state.interner.inner(),
            key,
        ))
        .len();
    }
    black_box(total);
}

#[library_benchmark(setup = collider_state)]
#[bench::three_chunks(3usize)]
fn bench_build_chunk_voxels(state: ColliderState) {
    for &key in state.keys.iter() {
        let voxels = black_box(build_chunk_voxels(
            &state.volume,
            state.interner.inner(),
            key,
        ));
        black_box(voxels.is_some());
    }
}

library_benchmark_group!(
    name = voxel_collider_inst,
    benchmarks = [bench_collect_chunk_solid_voxels, bench_build_chunk_voxels]
);

main!(library_benchmark_groups = voxel_collider_inst);
