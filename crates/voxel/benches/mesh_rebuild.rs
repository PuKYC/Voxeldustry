//! Criterion benchmarks for the deterministic voxel meshing pipeline.
//!
//! The input is a fixed, deterministic 8x8x8 group of base subchunks (a
//! synthetic island).  Every carved voxel is produced by an integer LCG
//! seeded from the chunk coordinate, so two runs measure exactly the same
//! tree (L3 determinism: no HashMap iteration feeds the workload).
//!
//! Run (release is implicit for cargo bench):
//!   cargo bench -p voxel --bench mesh_rebuild

use std::collections::BTreeMap;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use glam::{IVec3, UVec2, UVec3};

use voxel::mesh::{
    extract_block, extract_block_tree, extract_block_tree_with_ao, extract_rects_into,
    generate_external_occupancy_mask, generate_external_occupancy_mask_slow,
    generate_occupancy_masks, wrap_block, ExternalPlaneKind, IncrementalMesh, MeshBlock,
    MeshOccupancyData, OccupancyDataBuilder, RectInstance,
};
use voxel::store::{
    ChunkKey, Lod, MaxDepth, VoxInterner, VoxOpsBulkWrite, VoxOpsWrite, VoxTree, CHUNK_DEPTH,
};

/// Base subchunks per island axis (8^3 = 512, exactly one LOD3 mesh block).
const ISLAND_CHUNKS: i32 = 8;
/// Voxels per base subchunk axis (2^CHUNK_DEPTH == 32).
const VOXELS: i32 = 1 << CHUNK_DEPTH;
/// Deterministically carved voxels per base subchunk.
const HOLES_PER_CHUNK: u32 = 96;

/// Deterministic integer PRNG (same LCG as the crate's unit tests).
#[inline]
fn lcg(state: &mut u64) -> u32 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*state >> 33) as u32
}

/// Builds one base subchunk: a solid block with deterministic holes so the
/// tree is a branch (not a single leaf) in every run.
fn build_chunk(interner: &mut VoxInterner<u8>, cx: i32, cy: i32, cz: i32) -> VoxTree<u8> {
    let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
    let base = 1 + (cx * 3 + cy * 5 + cz * 7).rem_euclid(3) as u8;
    tree.fill(interner, base);

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
        let _ = tree.set(interner, IVec3::new(x, y, z), v);
    }
    tree
}

/// Builds the deterministic island used by every bench item.
fn build_island() -> (VoxInterner<u8>, BTreeMap<ChunkKey, VoxTree<u8>>) {
    let mut interner = VoxInterner::<u8>::with_memory_budget(16 * 1024 * 1024);
    let mut chunks: BTreeMap<ChunkKey, VoxTree<u8>> = BTreeMap::new();
    for cz in 0..ISLAND_CHUNKS {
        for cy in 0..ISLAND_CHUNKS {
            for cx in 0..ISLAND_CHUNKS {
                chunks.insert(
                    ChunkKey::new(cx, cy, cz),
                    build_chunk(&mut interner, cx, cy, cz),
                );
            }
        }
    }
    (interner, chunks)
}

/// Terrain-like base subchunk: a heightfield over mostly-flat 8x8 plateaus
/// (heights 22..24) with layered materials.  Contrasts the fragmented
/// solid+holes island, because greedy merge cost depends on how fragmented the
/// exposed surface is.
fn build_terrain_chunk(interner: &mut VoxInterner<u8>, cx: i32, cy: i32, cz: i32) -> VoxTree<u8> {
    let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
    tree.fill(interner, 1);
    for z in 0..VOXELS {
        for x in 0..VOXELS {
            let wx = cx * VOXELS + x;
            let wz = cz * VOXELS + z;
            let h = 22 + (wx / 8 + wz / 8 + cy) % 3;
            for y in (h + 1)..VOXELS {
                let _ = tree.set(interner, IVec3::new(x, y, z), 0);
            }
            let _ = tree.set(interner, IVec3::new(x, h, z), 2);
        }
    }
    tree
}

/// Internal occupancy build only (no greedy, no external), i.e. the first half
/// of the production build_tree_occupancy path.
fn build_occupancy(interner: &VoxInterner<u8>, tree: &VoxTree<u8>) -> MeshOccupancyData {
    let mut builder = OccupancyDataBuilder::new(1u32 << CHUNK_DEPTH);
    generate_occupancy_masks(
        interner,
        &mut builder,
        &tree.get_root_id(),
        MaxDepth::new(CHUNK_DEPTH),
        UVec3::ZERO,
    );
    builder.build()
}

fn block0() -> MeshBlock {
    MeshBlock::new(ChunkKey::new(0, 0, 0), Lod::new(0))
}

fn block3() -> MeshBlock {
    MeshBlock::new(ChunkKey::new(0, 0, 0), Lod::new(3))
}

/// Item 1: wrap_block LOD0 / LOD3 (wrap + explicit release, so the wrapper
/// branch nodes are recycled and the next iteration measures a cold wrap over
/// the already interned base subchunks).
fn bench_wrap_block(c: &mut Criterion) {
    let (mut interner, chunks) = build_island();
    let mut group = c.benchmark_group("wrap_block");
    group
        .sample_size(30)
        .measurement_time(Duration::from_secs(8));
    for lod in [0u8, 3u8] {
        let block = MeshBlock::new(ChunkKey::new(0, 0, 0), Lod::new(lod));
        group.bench_function(BenchmarkId::new("lod", lod), |b| {
            b.iter(|| {
                let tree = wrap_block(&chunks, &mut interner, block);
                let root = tree.get_root_id();
                if !root.is_empty() {
                    interner.dec_ref_recursive(&root);
                }
                black_box(root);
            });
        });
    }
    group.finish();
}

/// Item 2: occupancy build via extract_block_tree_with_ao on the wrapped trees
/// from item 1.
fn bench_occupancy(c: &mut Criterion) {
    let (mut interner, chunks) = build_island();
    let b0 = block0();
    let b3 = block3();
    let tree0 = wrap_block(&chunks, &mut interner, b0);
    let tree3 = wrap_block(&chunks, &mut interner, b3);

    let mut group = c.benchmark_group("extract_block_tree_with_ao");
    group
        .sample_size(20)
        .measurement_time(Duration::from_secs(10));
    group.bench_function("lod0", |b| {
        b.iter(|| {
            let out = extract_block_tree_with_ao(&tree0, &interner, b0, [None; 6]);
            black_box(out.rect_count());
        });
    });
    group.bench_function("lod3", |b| {
        b.iter(|| {
            let out = extract_block_tree_with_ao(&tree3, &interner, b3, [None; 6]);
            black_box(out.rect_count());
        });
    });
    group.finish();

    interner.dec_ref_recursive(&tree0.get_root_id());
    interner.dec_ref_recursive(&tree3.get_root_id());
}

/// Item 3: extract_rects alone on a prebuilt OccupancyData (the greedy merge
/// is isolated from occupancy extraction).
fn bench_extract_rects(c: &mut Criterion) {
    let (mut interner, chunks) = build_island();
    let b0 = block0();
    let tree0 = wrap_block(&chunks, &mut interner, b0);

    let mut builder = OccupancyDataBuilder::new(1u32 << CHUNK_DEPTH);
    generate_occupancy_masks(
        &interner,
        &mut builder,
        &tree0.get_root_id(),
        MaxDepth::new(CHUNK_DEPTH),
        UVec3::ZERO,
    );
    let occupancy: MeshOccupancyData = builder.build();

    let mut group = c.benchmark_group("extract_rects");
    group
        .sample_size(50)
        .measurement_time(Duration::from_secs(5));
    group.bench_function("prebuilt_lod0", |b| {
        b.iter_batched(
            || {
                (
                    Vec::<RectInstance>::with_capacity(4096),
                    Vec::<[u8; 4]>::with_capacity(4096),
                )
            },
            |(mut rects, mut aos)| {
                extract_rects_into(&occupancy, &mut rects, &mut aos);
                black_box((rects.len(), aos.len()))
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();

    interner.dec_ref_recursive(&tree0.get_root_id());
}

const FACE_NAMES: [&str; 6] = ["YZPos", "YZNeg", "XZPos", "XZNeg", "XYPos", "XYNeg"];

/// Item 4: external face extraction, old n^2 reference vs new face-descending
/// fast path, one benchmark per face.
fn bench_external(c: &mut Criterion) {
    let (mut interner, chunks) = build_island();
    let b0 = block0();
    let neighbour = wrap_block(&chunks, &mut interner, b0);
    let root = neighbour.get_root_id();
    let max_depth = MaxDepth::new(CHUNK_DEPTH);

    let mut group = c.benchmark_group("external_occupancy");
    group
        .sample_size(50)
        .measurement_time(Duration::from_secs(5));
    for (i, plane) in ExternalPlaneKind::ALL.iter().copied().enumerate() {
        let name = FACE_NAMES[i];
        group.bench_function(BenchmarkId::new("slow_generic", name), |b| {
            b.iter_batched(
                || OccupancyDataBuilder::new(1u32 << CHUNK_DEPTH),
                |mut builder| {
                    generate_external_occupancy_mask_slow(
                        &interner,
                        &mut builder,
                        &root,
                        max_depth,
                        plane,
                        UVec2::ZERO,
                    );
                    black_box(builder.external)
                },
                BatchSize::SmallInput,
            );
        });
        group.bench_function(BenchmarkId::new("fast", name), |b| {
            b.iter_batched(
                || OccupancyDataBuilder::new(1u32 << CHUNK_DEPTH),
                |mut builder| {
                    generate_external_occupancy_mask(
                        &interner,
                        &mut builder,
                        &root,
                        max_depth,
                        plane,
                        UVec2::ZERO,
                    );
                    black_box(builder.external)
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();

    interner.dec_ref_recursive(&neighbour.get_root_id());
}

/// Item 5: old LocalTree pipeline (extract_block) vs wrapped pipeline
/// (extract_block_tree), LOD0 / LOD3, to quantify the T1 win.
fn bench_local_vs_wrapped(c: &mut Criterion) {
    let (mut interner, chunks) = build_island();
    let b0 = block0();
    let b3 = block3();
    let tree0 = wrap_block(&chunks, &mut interner, b0);
    let tree3 = wrap_block(&chunks, &mut interner, b3);

    let mut group = c.benchmark_group("extract_local_vs_wrapped");
    group
        .sample_size(20)
        .measurement_time(Duration::from_secs(10));
    group.bench_function("local_lod0", |b| {
        b.iter(|| {
            let out = extract_block(&chunks, &interner, b0, [None; 6]);
            black_box(out.rects.len());
        });
    });
    group.bench_function("wrapped_lod0", |b| {
        b.iter(|| {
            let out = extract_block_tree(&tree0, &interner, b0, [None; 6]);
            black_box(out.rects.len());
        });
    });
    group.bench_function("local_lod3", |b| {
        b.iter(|| {
            let out = extract_block(&chunks, &interner, b3, [None; 6]);
            black_box(out.rects.len());
        });
    });
    group.bench_function("wrapped_lod3", |b| {
        b.iter(|| {
            let out = extract_block_tree(&tree3, &interner, b3, [None; 6]);
            black_box(out.rects.len());
        });
    });
    group.finish();

    interner.dec_ref_recursive(&tree0.get_root_id());
    interner.dec_ref_recursive(&tree3.get_root_id());
}

/// Direct occupancy-vs-greedy breakdown on the 512-chunk island LOD0 block, so
/// the greedy share is measured in one run instead of derived by subtracting
/// two independently noisy full-extraction runs.
fn bench_occupancy_breakdown(c: &mut Criterion) {
    let (mut interner, chunks) = build_island();
    let b0 = block0();
    let tree = wrap_block(&chunks, &mut interner, b0);
    let root = tree.get_root_id();
    let depth = MaxDepth::new(CHUNK_DEPTH);
    let occupancy = build_occupancy(&interner, &tree);

    let mut group = c.benchmark_group("occupancy_breakdown");
    group
        .sample_size(30)
        .measurement_time(Duration::from_secs(5));

    group.bench_function("island/occupancy_only", |b| {
        b.iter(|| {
            let mut builder = OccupancyDataBuilder::new(1u32 << CHUNK_DEPTH);
            generate_occupancy_masks(&interner, &mut builder, &root, depth, UVec3::ZERO);
            black_box(builder.global.len())
        });
    });
    group.bench_function("island/greedy_only", |b| {
        b.iter_batched(
            || {
                (
                    Vec::<RectInstance>::with_capacity(4096),
                    Vec::<[u8; 4]>::with_capacity(4096),
                )
            },
            |(mut rects, mut aos)| {
                extract_rects_into(&occupancy, &mut rects, &mut aos);
                black_box((rects.len(), aos.len()))
            },
            BatchSize::SmallInput,
        );
    });
    group.bench_function("island/full", |b| {
        b.iter(|| {
            let out = extract_block_tree_with_ao(&tree, &interner, b0, [None; 6]);
            black_box(out.rect_count())
        });
    });
    group.finish();

    interner.dec_ref_recursive(&root);
}

/// Same three measurements on a large flat-surface terrain-like chunk and on the
/// fragmented solid+holes chunk, to see how surface fragmentation moves the
/// greedy share (a synthetic stand-in for "retest on real terrain").
fn bench_chunk_shapes(c: &mut Criterion) {
    let mut interner_t = VoxInterner::<u8>::with_memory_budget(16 * 1024 * 1024);
    let tree_t = build_terrain_chunk(&mut interner_t, 0, 0, 0);
    let occ_t = build_occupancy(&interner_t, &tree_t);

    let mut interner_f = VoxInterner::<u8>::with_memory_budget(16 * 1024 * 1024);
    let tree_f = build_chunk(&mut interner_f, 0, 0, 0);
    let occ_f = build_occupancy(&interner_f, &tree_f);

    let b0 = block0();
    let depth = MaxDepth::new(CHUNK_DEPTH);

    let mut group = c.benchmark_group("occupancy_breakdown_chunks");
    group
        .sample_size(50)
        .measurement_time(Duration::from_secs(3));

    group.bench_function("terrain/occupancy_only", |b| {
        b.iter(|| {
            let mut builder = OccupancyDataBuilder::new(1u32 << CHUNK_DEPTH);
            generate_occupancy_masks(
                &interner_t,
                &mut builder,
                &tree_t.get_root_id(),
                depth,
                UVec3::ZERO,
            );
            black_box(builder.global.len())
        });
    });
    group.bench_function("terrain/greedy_only", |b| {
        b.iter_batched(
            || {
                (
                    Vec::<RectInstance>::with_capacity(4096),
                    Vec::<[u8; 4]>::with_capacity(4096),
                )
            },
            |(mut rects, mut aos)| {
                extract_rects_into(&occ_t, &mut rects, &mut aos);
                black_box((rects.len(), aos.len()))
            },
            BatchSize::SmallInput,
        );
    });
    group.bench_function("terrain/full", |b| {
        b.iter(|| {
            let out = extract_block_tree_with_ao(&tree_t, &interner_t, b0, [None; 6]);
            black_box(out.rect_count())
        });
    });

    group.bench_function("fragmented/occupancy_only", |b| {
        b.iter(|| {
            let mut builder = OccupancyDataBuilder::new(1u32 << CHUNK_DEPTH);
            generate_occupancy_masks(
                &interner_f,
                &mut builder,
                &tree_f.get_root_id(),
                depth,
                UVec3::ZERO,
            );
            black_box(builder.global.len())
        });
    });
    group.bench_function("fragmented/greedy_only", |b| {
        b.iter_batched(
            || {
                (
                    Vec::<RectInstance>::with_capacity(4096),
                    Vec::<[u8; 4]>::with_capacity(4096),
                )
            },
            |(mut rects, mut aos)| {
                extract_rects_into(&occ_f, &mut rects, &mut aos);
                black_box((rects.len(), aos.len()))
            },
            BatchSize::SmallInput,
        );
    });
    group.bench_function("fragmented/full", |b| {
        b.iter(|| {
            let out = extract_block_tree_with_ao(&tree_f, &interner_f, b0, [None; 6]);
            black_box(out.rect_count())
        });
    });

    group.finish();
}

/// T6 / Phase 5: same 32^3 terrain occupancy, full `extract_rects` vs
/// `IncrementalMesh::rebuild` from the previous version, for a 1^3 / 4^3 /
/// 16^3 material edit anchored at the +X / south-west corner (so the edit
/// always crosses the exposed surface).
///
/// The rebuild benchmark alternates base -> edited -> base so every timed call
/// is a real slice diff; the previous-version occupancy is produced in the
/// (untimed) `iter_batched` setup, exactly like an editor handing over a fresh
/// occupancy each frame.  dirty/total slice counts come from a one-off probe
/// rebuild and are printed to stderr so the raw criterion tee keeps them.
fn bench_incremental_slices(c: &mut Criterion) {
    let mut group = c.benchmark_group("incremental_slice");
    group
        .sample_size(30)
        .measurement_time(Duration::from_secs(6));

    for shape in ["fragmented", "terrain"] {
        for side in [1i32, 4, 16] {
            let mut interner = VoxInterner::<u8>::with_memory_budget(16 * 1024 * 1024);
            let base = match shape {
                "terrain" => build_terrain_chunk(&mut interner, 0, 0, 0),
                _ => build_chunk(&mut interner, 0, 0, 0),
            };
            let base_occ = build_occupancy(&interner, &base);

            let mut edited = match shape {
                "terrain" => build_terrain_chunk(&mut interner, 0, 0, 0),
                _ => build_chunk(&mut interner, 0, 0, 0),
            };
            // Interior material edit: nothing clamps at 0 / n-1, so every
            // family contributes the full {k-1, k, k+1} AO halo.
            let anchor = 8i32;
            for z in 0..side {
                for y in 0..side {
                    for x in 0..side {
                        let _ = edited.set(
                            &mut interner,
                            IVec3::new(anchor + x, anchor + y, anchor + z),
                            2,
                        );
                    }
                }
            }
            let next_occ = build_occupancy(&interner, &edited);

            // One representative rebuild, for the dirty-slice ratio and the
            // incremental cache footprint (stderr keeps it next to the timings).
            let mut probe = IncrementalMesh::new(base_occ.clone());
            let report = probe.rebuild(next_occ.clone());
            eprintln!(
            "incremental_slice shape={shape} side={side}^3 dirty_slices={} total_slices={} dirty_over_total={:.4} \
             reused_slices={} changed_slices={} memory_bytes={}",
            report.dirty_slices,
            report.total_slices,
            report.dirty_slices as f64 / report.total_slices.max(1) as f64,
            report.reused_slices,
            report.changed_slices,
            probe.memory_bytes(),
        );

            let label = format!("{shape}/{side}x");

            group.bench_function(BenchmarkId::new("full", &label), |b| {
                b.iter_batched(
                    || {
                        (
                            Vec::<RectInstance>::with_capacity(8192),
                            Vec::<[u8; 4]>::with_capacity(8192),
                        )
                    },
                    |(mut rects, mut aos)| {
                        extract_rects_into(&next_occ, &mut rects, &mut aos);
                        black_box((rects.len(), aos.len()))
                    },
                    BatchSize::SmallInput,
                );
            });

            let mut mesh = IncrementalMesh::new(base_occ.clone());
            let mut use_edited = false;
            group.bench_function(BenchmarkId::new("rebuild", &label), |b| {
                b.iter_batched(
                    || {
                        use_edited = !use_edited;
                        if use_edited {
                            next_occ.clone()
                        } else {
                            base_occ.clone()
                        }
                    },
                    |next| {
                        let report = mesh.rebuild(next);
                        black_box(report.dirty_slices)
                    },
                    BatchSize::PerIteration,
                );
            });
        }
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_wrap_block,
    bench_occupancy,
    bench_extract_rects,
    bench_external,
    bench_local_vs_wrapped,
    bench_occupancy_breakdown,
    bench_chunk_shapes,
    bench_incremental_slices,
);
criterion_main!(benches);
