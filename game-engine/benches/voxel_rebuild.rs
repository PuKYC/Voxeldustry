//! Criterion benchmarks for the game-engine voxel rebuild path.
//!
//! Items 3 and 4 also assert the boundary-filtered rebuild_plan shape before
//! timing it, so a regression fails the bench run instead of silently
//! measuring the wrong plan.
//!
//! Run:
//!   cargo bench -p game-engine --features voxel --bench voxel_rebuild

use std::collections::BTreeMap;
use std::time::Duration;

use bevy::math::IVec3;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use game_engine::math::FixedPoint;
use game_engine::presentation::voxel::{
    floor_to_lod_policy, mesh_block_incremental, mesh_block_wrapped, pack_rect_stream,
    rebuild_plan, ExternalMaskCache, FaceMask, IncrementalMeshCache, MeshBlockDirty,
    WrappedBlockCache,
};
use game_engine::voxel::{
    chunk_key, ChunkKey, Lod, MaxDepth, MeshBlock, VoxInterner, VoxOpsBulkWrite, VoxOpsWrite,
    VoxTree, VoxVolume, VoxelBox, VoxelDirtySet, VoxelInterner, CHUNK_DEPTH,
};

/// Base subchunks per island axis (8^3 = 512, exactly one LOD3 mesh block).
const ISLAND_CHUNKS: i32 = 8;
/// Voxels per base subchunk axis (2^CHUNK_DEPTH == 32).
const VOXELS: i32 = 1 << CHUNK_DEPTH;
/// Deterministically carved voxels per base subchunk.
const HOLES_PER_CHUNK: u32 = 96;

#[inline]
fn lcg(state: &mut u64) -> u32 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*state >> 33) as u32
}

fn build_chunk(interner: &mut VoxelInterner, cx: i32, cy: i32, cz: i32) -> VoxTree<u8> {
    let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
    let base = 1 + (cx * 3 + cy * 5 + cz * 7).rem_euclid(3) as u8;
    tree.fill(interner.inner_mut(), base);

    let mut state =
        0x9E37_79B9_7F4A_7C15u64 ^ ((cx as u64) << 42) ^ ((cy as u64) << 21) ^ (cz as u64);
    for _ in 0..HOLES_PER_CHUNK {
        let x = (lcg(&mut state) % VOXELS as u32) as i32;
        let y = (lcg(&mut state) % VOXELS as u32) as i32;
        let z = (lcg(&mut state) % VOXELS as u32) as i32;
        let v = match lcg(&mut state) % 4 {
            0 => 0u8,
            n => base + n as u8,
        };
        let _ = tree.set(interner.inner_mut(), IVec3::new(x, y, z), v);
    }
    tree
}

fn build_island() -> (VoxelInterner, VoxVolume) {
    let mut interner = VoxelInterner::new(16 * 1024 * 1024);
    let mut volume = VoxVolume::new(FixedPoint::from_num(1));
    for cz in 0..ISLAND_CHUNKS {
        for cy in 0..ISLAND_CHUNKS {
            for cx in 0..ISLAND_CHUNKS {
                volume.insert_chunk(
                    chunk_key(cx, cy, cz),
                    build_chunk(&mut interner, cx, cy, cz),
                );
            }
        }
    }
    (interner, volume)
}

/// Items 1 + 2: single-edit end-to-end new path vs old LocalTree path.
fn bench_edit_pipeline(c: &mut Criterion) {
    let (mut interner, volume) = build_island();
    let key = chunk_key(0, 0, 0);
    let block = MeshBlock::new(key, Lod::new(0));
    let mut cache = WrappedBlockCache::default();

    let mut group = c.benchmark_group("single_edit");
    group
        .sample_size(50)
        .measurement_time(Duration::from_secs(8));

    // Item 1: invalidate the covered wrapped blocks, re-wrap, extract, pack.
    group.bench_function("new_wrapped_e2e", |b| {
        b.iter(|| {
            cache.invalidate_covered(interner.inner_mut(), key);
            let batch = mesh_block_wrapped(
                &volume.chunks,
                interner.inner_mut(),
                &mut cache,
                block,
                [None; 6],
            );
            let words = pack_rect_stream(&[batch.into_batch()]);
            black_box(words.len())
        });
    });

    // Item 2: old path control - build the LocalTree and extract, then pack.
    group.bench_function("old_local_e2e", |b| {
        b.iter(|| {
            let batch = game_engine::voxel::extract_block(
                &volume.chunks,
                interner.inner(),
                block,
                [None; 6],
            );
            let words = pack_rect_stream(&[batch]);
            black_box(words.len())
        });
    });

    group.finish();
}

/// Items 3 + 4: rebuild_plan boundary filtering shape + end-to-end timing.
fn bench_boundary_scenarios(c: &mut Criterion) {
    let (mut interner, volume) = build_island();
    let key = chunk_key(0, 0, 0);
    let policy = floor_to_lod_policy(vec![Lod::new(0)]);

    // Scenario A: edit [8,24)^3 is farther than one band (2^0 base voxel) from
    // every face -> exactly one internal block, no external-only block.
    let edit_a = VoxelBox {
        min: [8, 8, 8],
        max: [24, 24, 24],
    };
    let mut dirty = VoxelDirtySet::new();
    dirty.mark_edited(key, edit_a);
    let plan_a = rebuild_plan(&dirty.take_edits(), &policy);
    assert!(dirty.is_empty(), "scenario A: dirty set must be drained");
    assert_eq!(plan_a.len(), 1, "scenario A: one planned block");
    assert!(plan_a[0].internal, "scenario A: block must be internal");
    assert_eq!(
        plan_a.iter().filter(|p| !p.internal).count(),
        0,
        "scenario A: no external-only blocks"
    );

    // Scenario B: edit flush against +X -> one internal block plus the +X
    // neighbour with only its opposite (YZNeg) face dirty.
    let edit_b = VoxelBox {
        min: [31, 8, 8],
        max: [32, 24, 24],
    };
    dirty.mark_edited(key, edit_b);
    let plan_b = rebuild_plan(&dirty.take_edits(), &policy);
    assert_eq!(plan_b.len(), 2, "scenario B: internal + one neighbour");
    assert_eq!(
        plan_b.iter().filter(|p| p.internal).count(),
        1,
        "scenario B: exactly one internal block"
    );
    let ext_b = plan_b
        .iter()
        .find(|p| !p.internal)
        .expect("scenario B: one external-only block");
    assert_eq!(
        ext_b.faces,
        FaceMask(1 << 1),
        "scenario B: +X neighbour must carry the YZNeg face bit"
    );
    assert_eq!(ext_b.block.origin, chunk_key(1, 0, 0));

    let mut cache = WrappedBlockCache::default();
    let mut plan: Vec<MeshBlock> = Vec::new();

    let mut group = c.benchmark_group("boundary_scenario");
    group
        .sample_size(50)
        .measurement_time(Duration::from_secs(8));

    plan.clear();
    plan.extend(plan_a.iter().map(|p| p.block));
    group.bench_function("scenario_a_e2e", |b| {
        b.iter(|| {
            let mut batches = Vec::with_capacity(plan.len());
            for &block in plan.iter() {
                let batch = mesh_block_wrapped(
                    &volume.chunks,
                    interner.inner_mut(),
                    &mut cache,
                    block,
                    [None; 6],
                );
                batches.push(batch.into_batch());
            }
            let words = pack_rect_stream(&batches);
            black_box(words.len())
        });
    });

    plan.clear();
    plan.extend(plan_b.iter().map(|p| p.block));
    group.bench_function("scenario_b_e2e", |b| {
        b.iter(|| {
            let mut batches = Vec::with_capacity(plan.len());
            for &block in plan.iter() {
                let batch = mesh_block_wrapped(
                    &volume.chunks,
                    interner.inner_mut(),
                    &mut cache,
                    block,
                    [None; 6],
                );
                batches.push(batch.into_batch());
            }
            let words = pack_rect_stream(&batches);
            black_box(words.len())
        });
    });

    group.finish();
}

/// Deterministic island where chunk (0,0,0) has an side^3 voxel pocket carved
/// out of its +X / south-west corner (flush against the +X block face used by
/// T4 scenario B).  side == 0 returns the unmodified base island.
fn build_island_with_edit(interner: &mut VoxelInterner, side: i32) -> VoxVolume {
    let mut volume = VoxVolume::new(FixedPoint::from_num(1));
    for cz in 0..ISLAND_CHUNKS {
        for cy in 0..ISLAND_CHUNKS {
            for cx in 0..ISLAND_CHUNKS {
                let mut tree = build_chunk(interner, cx, cy, cz);
                if side > 0 && cx == 0 && cy == 0 && cz == 0 {
                    let x0 = VOXELS - side;
                    for z in 0..side {
                        for y in 0..side {
                            for x in 0..side {
                                let _ = tree.set(interner.inner_mut(), IVec3::new(x0 + x, y, z), 0);
                            }
                        }
                    }
                }
                volume.insert_chunk(chunk_key(cx, cy, cz), tree);
            }
        }
    }
    volume
}

const DIRS6: [(i32, i32, i32); 6] = [
    (1, 0, 0),
    (-1, 0, 0),
    (0, 1, 0),
    (0, -1, 0),
    (0, 0, 1),
    (0, 0, -1),
];

/// Wraps block's six same-LOD neighbours in a dedicated cache and returns the
/// external array (same wiring as the section 7 test 10 warm_external).  A
/// separate WrappedBlockCache is required because mesh_block_incremental takes
/// its own cache by &mut while external holds shared references.
fn warm_external<'a>(
    cache: &'a mut WrappedBlockCache,
    interner: &mut VoxInterner<u8>,
    chunks: &BTreeMap<ChunkKey, VoxTree<u8>>,
    block: MeshBlock,
) -> [Option<&'a VoxTree<u8>>; 6] {
    let span = block.subchunk_span();
    for (dx, dy, dz) in DIRS6 {
        let neighbour = MeshBlock::new(
            chunk_key(
                block.origin.x + dx * span,
                block.origin.y + dy * span,
                block.origin.z + dz * span,
            ),
            block.lod,
        );
        let _ = cache.get_or_wrap(chunks, interner, neighbour);
    }
    let mut external = [None; 6];
    for (i, (dx, dy, dz)) in DIRS6.iter().enumerate() {
        let origin = chunk_key(
            block.origin.x + dx * span,
            block.origin.y + dy * span,
            block.origin.z + dz * span,
        );
        external[i] = cache.get(origin, block.lod);
    }
    external
}

/// end-to-end mesh_block_incremental vs the full
/// mesh_block_wrapped recompute, for 1^3 / 4^3 / 16^3 edits of chunk (0,0,0).
///
/// Both sides invalidate the wrapped cache every iteration (so occupancy is
/// rebuilt from the current volume, as in production); only the incremental
/// side keeps its IncrementalMeshCache entry.  The routine alternates base and
/// edited content so every timed incremental call is a real slice diff.
fn bench_incremental_internal(c: &mut Criterion) {
    let key = chunk_key(0, 0, 0);
    let block = MeshBlock::new(key, Lod::new(0));
    let plan = MeshBlockDirty {
        block,
        internal: true,
        faces: FaceMask(0),
    };

    let mut group = c.benchmark_group("incremental_internal");
    group
        .sample_size(30)
        .measurement_time(Duration::from_secs(6));

    for side in [1i32, 4, 16] {
        let mut interner = VoxelInterner::new(16 * 1024 * 1024);
        let volume_a = build_island_with_edit(&mut interner, 0);
        let volume_b = build_island_with_edit(&mut interner, side);

        // Seed the incremental entry on the base version and report the cache
        // footprint (stderr, so the raw criterion log keeps it).
        let mut wrapped = WrappedBlockCache::default();
        let mut ext_cache = ExternalMaskCache::default();
        let mut cache = IncrementalMeshCache::default();
        let _ = mesh_block_incremental(
            &volume_a.chunks,
            interner.inner_mut(),
            &mut wrapped,
            &mut ext_cache,
            &mut cache,
            plan,
            [None; 6],
        );
        eprintln!(
            "incremental_internal side={side}^3 cache_memory_bytes={} cached_blocks={}",
            cache.memory_bytes(),
            cache.len(),
        );

        let label = format!("{side}x");
        group.bench_function(BenchmarkId::new("full", &label), |b| {
            b.iter(|| {
                wrapped.invalidate_covered(interner.inner_mut(), key);
                let batch = mesh_block_wrapped(
                    &volume_b.chunks,
                    interner.inner_mut(),
                    &mut wrapped,
                    block,
                    [None; 6],
                );
                black_box(pack_rect_stream(&[batch.into_batch()]).len())
            });
        });

        let mut use_edited = false;
        group.bench_function(BenchmarkId::new("incr", &label), |b| {
            b.iter(|| {
                use_edited = !use_edited;
                let chunks = if use_edited {
                    &volume_b.chunks
                } else {
                    &volume_a.chunks
                };
                wrapped.invalidate_covered(interner.inner_mut(), key);
                let (batch, changed) = mesh_block_incremental(
                    chunks,
                    interner.inner_mut(),
                    &mut wrapped,
                    &mut ext_cache,
                    &mut cache,
                    plan,
                    [None; 6],
                );
                black_box((pack_rect_stream(&[batch.into_batch()]).len(), changed))
            });
        });
    }

    group.finish();
}

/// World B differs from A only by the
/// neighbour chunk (0,0,0); neighbour block (1,0,0) has an unchanged occupancy
/// bitmap, so plan.internal == false rebuilds only the affected YZNeg slices.
///
/// The external arrays come from two dedicated neighbour caches (borrow
/// separation, exactly like the section 7 test 10 warm_external); the routine
/// alternates A/B so every timed call is a real rebuild_external.
fn bench_incremental_external(c: &mut Criterion) {
    let edited_key = chunk_key(0, 0, 0);
    let neighbour_key = chunk_key(1, 0, 0);
    let edited_block = MeshBlock::new(edited_key, Lod::new(0));
    let neighbour_block = MeshBlock::new(neighbour_key, Lod::new(0));
    let internal_plan = MeshBlockDirty {
        block: edited_block,
        internal: true,
        faces: FaceMask(0),
    };
    // Face bit 1 == YZNeg, i.e. the -X neighbour (chunk 0,0,0).
    let external_plan = MeshBlockDirty {
        block: neighbour_block,
        internal: false,
        faces: FaceMask(1 << 1),
    };

    let mut interner = VoxelInterner::new(16 * 1024 * 1024);
    let volume_a = build_island_with_edit(&mut interner, 0);
    let volume_b = build_island_with_edit(&mut interner, 16);

    let mut wrapped = WrappedBlockCache::default();
    let mut ext_cache = ExternalMaskCache::default();
    let mut cache = IncrementalMeshCache::default();

    // Two independent neighbour caches, one per world; they stay frozen so the
    // external arrays can be swapped without re-wrapping inside the timed loop.
    let mut nbr_a = WrappedBlockCache::default();
    let external_a = warm_external(
        &mut nbr_a,
        interner.inner_mut(),
        &volume_a.chunks,
        neighbour_block,
    );
    let mut nbr_b = WrappedBlockCache::default();
    let external_b = warm_external(
        &mut nbr_b,
        interner.inner_mut(),
        &volume_b.chunks,
        neighbour_block,
    );

    // Seed both incremental entries on the base version.  The neighbour entry
    // gets its full external array so the external-only delta is exactly the
    // one changed side.
    let _ = mesh_block_incremental(
        &volume_a.chunks,
        interner.inner_mut(),
        &mut wrapped,
        &mut ext_cache,
        &mut cache,
        internal_plan,
        [None; 6],
    );
    let _ = mesh_block_incremental(
        &volume_a.chunks,
        interner.inner_mut(),
        &mut wrapped,
        &mut ext_cache,
        &mut cache,
        MeshBlockDirty {
            block: neighbour_block,
            internal: true,
            faces: FaceMask(0),
        },
        external_a,
    );
    eprintln!(
        "incremental_external seeded cache_memory_bytes={} cached_blocks={}",
        cache.memory_bytes(),
        cache.len(),
    );

    let mut group = c.benchmark_group("incremental_external");
    group
        .sample_size(30)
        .measurement_time(Duration::from_secs(6));

    // The extra block in T4 scenario B: only the -X neighbour changed.
    let mut use_edited = false;
    group.bench_function("external_only_e2e", |b| {
        b.iter(|| {
            use_edited = !use_edited;
            let external = if use_edited { external_b } else { external_a };
            let (batch, changed) = mesh_block_incremental(
                &volume_a.chunks,
                interner.inner_mut(),
                &mut wrapped,
                &mut ext_cache,
                &mut cache,
                external_plan,
                external,
            );
            black_box((pack_rect_stream(&[batch.into_batch()]).len(), changed))
        });
    });

    // The same neighbour block recomputed from scratch over world B.
    group.bench_function("neighbour_full_e2e", |b| {
        b.iter(|| {
            wrapped.invalidate_covered(interner.inner_mut(), neighbour_key);
            let batch = mesh_block_wrapped(
                &volume_b.chunks,
                interner.inner_mut(),
                &mut wrapped,
                neighbour_block,
                external_b,
            );
            black_box(pack_rect_stream(&[batch.into_batch()]).len())
        });
    });

    // Scenario A control: the edited block's own internal incremental rebuild.
    let mut use_edited_a = false;
    group.bench_function("scenario_a_incremental_e2e", |b| {
        b.iter(|| {
            use_edited_a = !use_edited_a;
            let chunks = if use_edited_a {
                &volume_b.chunks
            } else {
                &volume_a.chunks
            };
            wrapped.invalidate_covered(interner.inner_mut(), edited_key);
            let (batch, changed) = mesh_block_incremental(
                chunks,
                interner.inner_mut(),
                &mut wrapped,
                &mut ext_cache,
                &mut cache,
                internal_plan,
                [None; 6],
            );
            black_box((pack_rect_stream(&[batch.into_batch()]).len(), changed))
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_edit_pipeline,
    bench_boundary_scenarios,
    bench_incremental_internal,
    bench_incremental_external,
);
criterion_main!(benches);
