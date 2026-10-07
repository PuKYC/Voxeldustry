//! WP6 v1 acceptance: subchunk AABB, incremental mesh == full mesh and
//! instance-buffer determinism (design section 10).
//!
//! Game-engine voxel tests are feature-gated: run them with
//!   cargo test -p game-engine --features voxel --test voxel_acceptance
//! (game-core enables the feature transitively, so cargo test --workspace also
//! builds them).
#![cfg(feature = "voxel")]

use std::collections::BTreeMap;

use bevy::math::IVec3;
use game_engine::math::FixedPoint;
use game_engine::rng::RngState;
use game_engine::voxel::{
    chunk_key, chunk_voxels_per_axis, count_body_nodes, extract_block, floor_to_lod_policy,
    lod_block_origin, mesh_block_incremental, mesh_block_wrapped, neighbors6, pack_rect_stream,
    rebuild_plan, release_body, release_body_with_cache, ChunkKey, DirtyChunk, ExternalMaskCache,
    FaceMask, IncrementalMeshCache, Lod, MaxDepth, MeshBlock, MeshBlockDirty, VoxOpsBulkWrite,
    VoxOpsState, VoxOpsWrite, VoxTree, VoxVolume, VoxelAabb, VoxelBox, VoxelDirtySet,
    VoxelInterner, WrappedBlockCache, CHUNK_DEPTH,
};
use voxel::mesh::{extract_block_tree_with_ao, extract_block_with_ao, AoRectBatch};
use voxel::store::VoxInterner;

// ---------------------------------------------------------------------------
// 5. Subchunk AABB == chunk bounding box
// ---------------------------------------------------------------------------

/// Local helper: there is no public chunk_aabb() in the engine, so the test
/// constructs the bounding box from the chunk key and the 32-voxel grid.
fn chunk_aabb(key: ChunkKey) -> VoxelAabb {
    let c = chunk_voxels_per_axis();
    VoxelAabb::from_min_size([key.x * c, key.y * c, key.z * c], [c, c, c])
}

#[test]
fn subchunk_aabb_matches_chunk_bounding_box() {
    let c = chunk_voxels_per_axis();
    assert_eq!(
        c,
        1 << CHUNK_DEPTH,
        "one chunk is 2^CHUNK_DEPTH voxels per axis"
    );
    assert_eq!(c, 32);

    for key in [
        chunk_key(0, 0, 0),
        chunk_key(2, -1, 3),
        chunk_key(-4, 5, -2),
    ] {
        let aabb = chunk_aabb(key);
        let min = [key.x * c, key.y * c, key.z * c];
        let max = [(key.x + 1) * c, (key.y + 1) * c, (key.z + 1) * c];

        assert_eq!(aabb, VoxelAabb::new(min, max), "AABB must be the chunk box");
        assert_eq!(
            aabb.size(),
            [c, c, c],
            "AABB must be CHUNK_DEPTH x voxel grid"
        );
        assert!(!aabb.is_empty());

        // Inside (half-open [min, max)): lower corner and an interior point.
        assert!(aabb.contains(min), "lower corner {min:?} must be inside");
        assert!(aabb.contains([min[0] + c / 2, min[1] + c / 2, min[2] + c / 2]));

        // Outside: the upper corner and just below the lower corner.
        assert!(
            !aabb.contains(max),
            "half-open upper corner must be outside"
        );
        assert!(!aabb.contains([min[0] + c, min[1], min[2]]));
        assert!(!aabb.contains([min[0] - 1, min[1], min[2]]));

        // A chunk diagonal neighbour only touches; it must not overlap.
        let next = chunk_aabb(chunk_key(key.x + 1, key.y, key.z));
        assert!(
            !aabb.intersects(&next),
            "adjacent chunk AABBs must not overlap"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Incremental mesh == full mesh
// ---------------------------------------------------------------------------

const DEPTH: u8 = CHUNK_DEPTH;
const N: i32 = 32;

/// Deterministic per-voxel terrain inside a chunk: a small hill of 1/2/3.
fn base_voxel(x: i32, y: i32, z: i32) -> u8 {
    let height = 8 + (x * 7 + z * 13).rem_euclid(9);
    if y > height {
        0
    } else if y == height {
        3
    } else if y + 3 >= height {
        2
    } else {
        1
    }
}

/// Final world = base world with exactly one voxel changed to value 7.
fn edited_voxel(x: i32, y: i32, z: i32) -> u8 {
    if x == 16 && y == 16 && z == 16 {
        7
    } else {
        base_voxel(x, y, z)
    }
}

fn build_chunk(interner: &mut VoxelInterner, f: impl Fn(i32, i32, i32) -> u8) -> VoxTree<u8> {
    let mut tree = VoxTree::<u8>::new(MaxDepth::new(DEPTH));
    for y in 0..N {
        for z in 0..N {
            for x in 0..N {
                let v = f(x, y, z);
                if v != 0 {
                    let _ = tree.set(interner.inner_mut(), IVec3::new(x, y, z), v);
                }
            }
        }
    }
    tree
}

#[test]
fn incremental_mesh_equals_full_mesh() {
    let key = chunk_key(0, 0, 0);
    let block = MeshBlock::new(key, Lod::new(0));
    let voxel_size = FixedPoint::from_num(1);

    // World A: base terrain, meshed once before the edit.
    let mut interner_a = VoxelInterner::new(2 * 1024 * 1024);
    let base = build_chunk(&mut interner_a, base_voxel);
    let mut volume_a = VoxVolume::new(voxel_size);
    volume_a.insert_chunk(key, base);
    let before = extract_block(&volume_a.chunks, interner_a.inner(), block, [None; 6]);

    // Apply exactly one voxel edit through the volume component.
    let changed = volume_a.get_chunk_mut(&key).expect("chunk present").set(
        interner_a.inner_mut(),
        IVec3::new(16, 16, 16),
        7,
    );
    assert!(changed, "the single edit must change the tree");
    let incremental = extract_block(&volume_a.chunks, interner_a.inner(), block, [None; 6]);

    // World B: rebuilt from scratch with the same final content, fresh interner.
    let mut interner_b = VoxelInterner::new(2 * 1024 * 1024);
    let final_tree = build_chunk(&mut interner_b, edited_voxel);
    let mut volume_b = VoxVolume::new(voxel_size);
    volume_b.insert_chunk(key, final_tree);
    let full = extract_block(&volume_b.chunks, interner_b.inner(), block, [None; 6]);

    // Sanity: the edit is observable, and the dirty rebuild matches the full one.
    assert_ne!(
        before.rects, incremental.rects,
        "one voxel edit must change at least one rectangle"
    );
    assert_eq!(
        incremental.rects, full.rects,
        "incremental re-extraction must equal a from-scratch extraction over the final world"
    );
    assert_eq!(incremental.origin, full.origin);
    assert_eq!(incremental.lod, full.lod);
}

// ---------------------------------------------------------------------------
// 6. Instance-buffer determinism (extract twice -> identical packed bytes)
// ---------------------------------------------------------------------------

fn synthetic_voxel(cx: i32, cz: i32, x: i32, y: i32, z: i32) -> u8 {
    let wx = cx * N + x;
    let wz = cz * N + z;
    let height = 6 + (wx * 5 + wz * 11).rem_euclid(7);
    if y > height {
        0
    } else if y == height {
        4
    } else if y + 2 >= height {
        2
    } else {
        1
    }
}

fn mesh_all(volume: &VoxVolume, interner: &VoxelInterner) -> Vec<game_engine::voxel::RectBatch> {
    let mut batches = Vec::new();
    for (key, tree) in volume.chunks.iter() {
        if tree.is_empty() {
            continue;
        }
        batches.push(extract_block(
            &volume.chunks,
            interner.inner(),
            MeshBlock::new(*key, Lod::new(0)),
            [None; 6],
        ));
    }
    batches
}

#[test]
fn instance_buffer_determinism() {
    let mut interner = VoxelInterner::new(4 * 1024 * 1024);
    let mut volume = VoxVolume::new(FixedPoint::from_num(1));

    let mut keys: BTreeMap<ChunkKey, ()> = BTreeMap::new();
    for cz in 0..2 {
        for cx in 0..2 {
            let key = chunk_key(cx, 0, cz);
            keys.insert(key, ());
            let tree = build_chunk(&mut interner, |x, y, z| synthetic_voxel(cx, 0, x, y, z));
            volume.insert_chunk(key, tree);
        }
    }
    let _ = keys;

    let batches_a = mesh_all(&volume, &interner);
    let batches_b = mesh_all(&volume, &interner);
    assert_eq!(
        batches_a, batches_b,
        "two extractions must produce equal rect batches"
    );

    let words_a = pack_rect_stream(&batches_a);
    let words_b = pack_rect_stream(&batches_b);
    println!(
        "instance stream deterministic: rects={} bytes={}",
        words_a.len(),
        words_a.len() * 8
    );
    assert_eq!(
        words_a, words_b,
        "packed instance stream must be byte-identical"
    );
    assert!(
        words_a.iter().all(|w| w >> 39 == 0),
        "all rects must fit 39 bits"
    );
}

// ---------------------------------------------------------------------------
// Test 1 invariant: rebuild_plan is a boundary-filtered subset of the old
// dirty+6-neighbour+covering superset, and never omits a block that really
// changes its mesh (brute-force full rebuild A vs B).
// ---------------------------------------------------------------------------

/// Chunks that make up the small multi-chunk world used by the tests below.
const PLAN_KEYS: [ChunkKey; 4] = [
    ChunkKey::new(0, 0, 0),
    ChunkKey::new(1, 0, 0),
    ChunkKey::new(0, 0, 1),
    ChunkKey::new(1, 0, 1),
];

/// Deterministic terrain for the plan-invariant test (world-space).
fn plan_terrain(cx: i32, cy: i32, cz: i32, x: i32, y: i32, z: i32) -> u8 {
    let wx = cx * N + x;
    let wy = cy * N + y;
    let wz = cz * N + z;
    let height = 9 + (wx * 7 + wz * 13).rem_euclid(9);
    if wy > height {
        0
    } else if wy == height {
        3
    } else if wy + 3 >= height {
        2
    } else {
        1
    }
}

/// Builds one chunk of plan_terrain, overriding the edit box of edit.0
/// with material 7 so that world B differs from world A only inside that box.
fn build_key_chunk(
    interner: &mut VoxelInterner,
    key: ChunkKey,
    edit: Option<(ChunkKey, VoxelBox)>,
) -> VoxTree<u8> {
    build_chunk(interner, |x, y, z| {
        if let Some((ek, eb)) = edit {
            if ek == key
                && x >= eb.min[0] as i32
                && x < eb.max[0] as i32
                && y >= eb.min[1] as i32
                && y < eb.max[1] as i32
                && z >= eb.min[2] as i32
                && z < eb.max[2] as i32
            {
                return 7;
            }
        }
        plan_terrain(key.x, key.y, key.z, x, y, z)
    })
}

fn build_volume(
    interner: &mut VoxelInterner,
    keys: &[ChunkKey],
    edit: Option<(ChunkKey, VoxelBox)>,
) -> VoxVolume {
    let mut volume = VoxVolume::new(FixedPoint::from_num(1));
    for &k in keys {
        volume.insert_chunk(k, build_key_chunk(interner, k, edit));
    }
    volume
}

/// Runs the old LocalTree pipeline and the new wrapped pipeline on block
/// with the six same-LOD neighbours warmed through cache; returns (old, new).
fn both_pipelines(
    chunks: &BTreeMap<ChunkKey, VoxTree<u8>>,
    interner: &mut VoxInterner<u8>,
    cache: &mut WrappedBlockCache,
    block: MeshBlock,
) -> (AoRectBatch, AoRectBatch) {
    const DIRS: [(i32, i32, i32); 6] = [
        (1, 0, 0),
        (-1, 0, 0),
        (0, 1, 0),
        (0, -1, 0),
        (0, 0, 1),
        (0, 0, -1),
    ];
    let span = block.subchunk_span();
    let _ = cache.get_or_wrap(chunks, interner, block);
    for (dx, dy, dz) in DIRS {
        let nb = MeshBlock::new(
            chunk_key(
                block.origin.x + dx * span,
                block.origin.y + dy * span,
                block.origin.z + dz * span,
            ),
            block.lod,
        );
        let _ = cache.get_or_wrap(chunks, interner, nb);
    }
    let own = cache
        .get(block.origin, block.lod)
        .expect("own block wrapped");
    let mut ext: [Option<&VoxTree<u8>>; 6] = [None; 6];
    for (i, (dx, dy, dz)) in DIRS.iter().enumerate() {
        let nb_origin = chunk_key(
            block.origin.x + dx * span,
            block.origin.y + dy * span,
            block.origin.z + dz * span,
        );
        ext[i] = cache.get(nb_origin, block.lod);
    }
    let old = extract_block_with_ao(chunks, interner, block, ext);
    let new = extract_block_tree_with_ao(own, interner, block, ext);
    (old, new)
}

fn mesh_batch(
    chunks: &BTreeMap<ChunkKey, VoxTree<u8>>,
    interner: &mut VoxInterner<u8>,
    cache: &mut WrappedBlockCache,
    block: MeshBlock,
) -> AoRectBatch {
    both_pipelines(chunks, interner, cache, block).0
}

/// For one LOD0 edit, brute-force the whole old dirty+6-neighbour superset and
/// assert that the plan (a) stays inside it, (b) contains every block whose
/// rects/AO really change, and (c) never emits an external-only block with an
/// empty face mask.
fn assert_plan_covers_changes(keys: &[ChunkKey], ekey: ChunkKey, eb: VoxelBox) {
    let policy = floor_to_lod_policy(vec![Lod::new(0)]);
    let mut ia = VoxelInterner::new(8 * 1024 * 1024);
    let va = build_volume(&mut ia, keys, None);
    let mut ib = VoxelInterner::new(8 * 1024 * 1024);
    let vb = build_volume(&mut ib, keys, Some((ekey, eb)));

    let mut superset: Vec<ChunkKey> = vec![ekey];
    superset.extend(neighbors6(ekey));

    let mut ca = WrappedBlockCache::default();
    let mut cb = WrappedBlockCache::default();
    let mut changed: Vec<ChunkKey> = Vec::new();
    for &k in superset.iter() {
        let block = MeshBlock::new(k, Lod::new(0));
        let a = mesh_batch(&va.chunks, ia.inner_mut(), &mut ca, block);
        let b = mesh_batch(&vb.chunks, ib.inner_mut(), &mut cb, block);
        if a != b {
            changed.push(k);
        }
    }
    ca.clear(ia.inner_mut());
    cb.clear(ib.inner_mut());

    let plan = rebuild_plan(
        &[DirtyChunk {
            key: ekey,
            edited: eb,
        }],
        &policy,
    );
    let plan_blocks: Vec<ChunkKey> = plan.iter().map(|p| p.block.origin).collect();

    for k in plan_blocks.iter() {
        assert!(
            superset.contains(k),
            "plan block {k:?} escaped the old dirty+neighbour superset"
        );
    }
    for k in changed.iter() {
        assert!(
            plan_blocks.contains(k),
            "editing {ekey:?} {eb:?} changed the mesh of {k:?} but rebuild_plan omitted it"
        );
    }
    for p in plan.iter() {
        assert!(
            p.internal || p.faces.0 != 0,
            "external-only plan entry without a face mask"
        );
    }
}

#[test]
fn plan_boundary_filter_invariant() {
    let cases = [
        VoxelBox {
            min: [8, 8, 8],
            max: [24, 24, 24],
        },
        VoxelBox {
            min: [31, 8, 8],
            max: [32, 24, 24],
        },
        VoxelBox {
            min: [31, 31, 31],
            max: [32, 32, 32],
        },
        VoxelBox {
            min: [8, 31, 8],
            max: [24, 32, 24],
        },
    ];
    for eb in cases {
        assert_plan_covers_changes(&PLAN_KEYS, chunk_key(0, 0, 0), eb);
    }

    // Deterministic pseudo-random edits (seeded Pcg32; L3-friendly).
    let mut rng = RngState::from_seed(0x5EED_1234);
    for _ in 0..6 {
        let mut m = || (rng.next_u32() % 24) as u8;
        let min = [m(), m(), m()];
        let mut s = || 1 + (rng.next_u32() % 8) as u8;
        let size = [s(), s(), s()];
        let max = [
            (min[0] + size[0]).min(32),
            (min[1] + size[1]).min(32),
            (min[2] + size[2]).min(32),
        ];
        assert_plan_covers_changes(&PLAN_KEYS, chunk_key(0, 0, 0), VoxelBox { min, max });
    }
}

// ---------------------------------------------------------------------------
// Test 2: new wrapped pipeline == old LocalTree pipeline (rects AND AO)
// ---------------------------------------------------------------------------

/// Deterministic terrain for the multi-LOD equivalence test.
fn block_voxel(cx: i32, cy: i32, cz: i32, x: i32, y: i32, z: i32) -> u8 {
    let wx = cx * N + x;
    let wy = cy * N + y;
    let wz = cz * N + z;
    let height = 12 + (wx * 5 + wy * 3 + wz * 11).rem_euclid(7);
    if wy > height {
        0
    } else if wy == height {
        4
    } else if wy + 2 >= height {
        2
    } else {
        1
    }
}

#[test]
fn wrapped_pipeline_bit_equal_to_old_pipeline() {
    let mut interner = VoxelInterner::new(8 * 1024 * 1024);
    let mut volume = VoxVolume::new(FixedPoint::from_num(1));
    for &k in PLAN_KEYS.iter() {
        let t = build_chunk(&mut interner, |x, y, z| block_voxel(k.x, k.y, k.z, x, y, z));
        volume.insert_chunk(k, t);
    }

    let mut cache = WrappedBlockCache::default();
    let mut checked = 0usize;
    for lod in 0..=3u8 {
        let lod = Lod::new(lod);
        let mut blocks: BTreeMap<(i32, i32, i32), MeshBlock> = BTreeMap::new();
        for &k in PLAN_KEYS.iter() {
            let o = lod_block_origin(k, lod);
            blocks.insert((o.x, o.y, o.z), MeshBlock::new(o, lod));
        }
        for block in blocks.into_values() {
            let (old, new) =
                both_pipelines(&volume.chunks, interner.inner_mut(), &mut cache, block);
            assert_eq!(
                old,
                new,
                "old/new pipeline differ at lod {} block {:?}",
                lod.lod(),
                block
            );
            checked += 1;
        }
    }
    // LOD0 gives 4 distinct chunks; every higher LOD collapses them to the
    // single aligned block at the origin, so 4 + 3 = 7 blocks are checked.
    assert!(checked >= 7, "expected all LOD levels to be exercised");
    cache.clear(interner.inner_mut());
}

// ---------------------------------------------------------------------------
// Test 4: reference counting / no pinned nodes
// ---------------------------------------------------------------------------

#[test]
fn wrapped_cache_release_leaks_no_nodes() {
    let mut interner = VoxelInterner::new(8 * 1024 * 1024);
    let keys = [chunk_key(0, 0, 0), chunk_key(1, 0, 0), chunk_key(0, 0, 1)];

    let empty_alive = interner.inner().alive_nodes();
    let mut volume = build_volume(&mut interner, &keys, None);
    let body_alive = interner.inner().alive_nodes();
    assert!(body_alive > empty_alive);
    let body_nodes = count_body_nodes(interner.inner(), &volume);
    assert!(body_nodes > 0);

    // Warm all LODs (LOD>0 creates wrapper branch nodes + refs).
    let mut cache = WrappedBlockCache::default();
    for lod in 0..=3u8 {
        let lod = Lod::new(lod);
        for &k in keys.iter() {
            let block = MeshBlock::new(lod_block_origin(k, lod), lod);
            let _ = cache.get_or_wrap(&volume.chunks, interner.inner_mut(), block);
        }
    }
    assert!(cache.len() >= keys.len());

    // Correct release order: clear the cache, then release the body.
    let released = release_body_with_cache(interner.inner_mut(), &mut volume, &mut cache);
    assert_eq!(released, keys.len());
    assert!(cache.is_empty());
    assert!(volume.chunks.is_empty());
    assert_eq!(count_body_nodes(interner.inner(), &volume), 0);
    assert_eq!(
        interner.inner().alive_nodes(),
        empty_alive,
        "cache.clear must release every wrapped root"
    );

    // Hazard check: releasing without clearing pins the body nodes; clear fixes it.
    let mut volume2 = build_volume(&mut interner, &keys, None);
    let mut cache2 = WrappedBlockCache::default();
    for lod in 0..=3u8 {
        let lod = Lod::new(lod);
        for &k in keys.iter() {
            let block = MeshBlock::new(lod_block_origin(k, lod), lod);
            let _ = cache2.get_or_wrap(&volume2.chunks, interner.inner_mut(), block);
        }
    }
    assert_eq!(release_body(interner.inner_mut(), &mut volume2), keys.len());
    assert!(
        interner.inner().alive_nodes() > empty_alive,
        "uncleared cache must pin wrapper nodes"
    );
    cache2.clear(interner.inner_mut());
    assert_eq!(
        interner.inner().alive_nodes(),
        empty_alive,
        "clear must unpin the wrapper nodes"
    );
}

// ---------------------------------------------------------------------------
// Test 5: two plan -> mesh -> pack runs are byte-identical
// ---------------------------------------------------------------------------

#[test]
fn plan_mesh_pack_is_byte_deterministic() {
    let policy = floor_to_lod_policy(vec![Lod::new(0), Lod::new(1)]);
    let ekey = chunk_key(0, 0, 0);
    let eb = VoxelBox {
        min: [30, 8, 8],
        max: [32, 24, 24],
    };

    let mut interner = VoxelInterner::new(8 * 1024 * 1024);
    let volume = build_volume(&mut interner, &PLAN_KEYS, Some((ekey, eb)));
    let mut cache = WrappedBlockCache::default();

    let run = |cache: &mut WrappedBlockCache,
               interner: &mut VoxelInterner|
     -> (Vec<u64>, Vec<MeshBlock>) {
        let mut dirty = VoxelDirtySet::new();
        dirty.mark_edited(ekey, eb);
        let plan = dirty.take_rebuild_plan(&policy);
        let mut batches = Vec::new();
        for p in plan.iter() {
            let batch = mesh_block_wrapped(
                &volume.chunks,
                interner.inner_mut(),
                cache,
                p.block,
                [None; 6],
            );
            batches.push(batch.into_batch());
        }
        let words = pack_rect_stream(&batches);
        (words, plan.iter().map(|p| p.block).collect())
    };

    let (a, plan_a) = run(&mut cache, &mut interner);
    let (b, plan_b) = run(&mut cache, &mut interner);
    assert_eq!(
        plan_a, plan_b,
        "two rebuild_plans must be identical and ordered"
    );
    assert_eq!(a, b, "two plan->mesh->pack runs must be byte-identical");
    assert!(!a.is_empty());
    assert!(a.iter().all(|w| w >> 39 == 0), "all rects must fit 39 bits");
}

// ---------------------------------------------------------------------------
// Test 10: engine incremental entry + external-only path
// ---------------------------------------------------------------------------

const DIRS6: [(i32, i32, i32); 6] = [
    (1, 0, 0),
    (-1, 0, 0),
    (0, 1, 0),
    (0, -1, 0),
    (0, 0, 1),
    (0, 0, -1),
];

/// Wraps block's six same-LOD neighbours in a *dedicated* neighbour cache and
/// returns the external array.  A separate WrappedBlockCache is required
/// because mesh_block_incremental takes its own cache by &mut while external
/// holds shared references.
fn warm_external<'a>(
    cache: &'a mut WrappedBlockCache,
    interner: &mut VoxInterner<u8>,
    chunks: &BTreeMap<ChunkKey, VoxTree<u8>>,
    block: MeshBlock,
) -> [Option<&'a VoxTree<u8>>; 6] {
    let span = block.subchunk_span();
    for (dx, dy, dz) in DIRS6 {
        let nb = MeshBlock::new(
            chunk_key(
                block.origin.x + dx * span,
                block.origin.y + dy * span,
                block.origin.z + dz * span,
            ),
            block.lod,
        );
        let _ = cache.get_or_wrap(chunks, interner, nb);
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

/// Test 10a: internal incremental entry == mesh_block_wrapped (rects + ao).
#[test]
fn incremental_entry_internal_equals_wrapped() {
    let mut interner = VoxelInterner::new(8 * 1024 * 1024);
    let volume = build_volume(&mut interner, &PLAN_KEYS, None);
    let block = MeshBlock::new(chunk_key(0, 0, 0), Lod::new(0));
    let plan = MeshBlockDirty {
        block,
        internal: true,
        faces: FaceMask(0),
    };

    let mut nbr_cache = WrappedBlockCache::default();
    let external = warm_external(&mut nbr_cache, interner.inner_mut(), &volume.chunks, block);

    let mut own_cache = WrappedBlockCache::default();
    let expected = mesh_block_wrapped(
        &volume.chunks,
        interner.inner_mut(),
        &mut own_cache,
        block,
        external,
    );

    let mut ext_cache = ExternalMaskCache::default();
    let mut cache = IncrementalMeshCache::default();
    let (got, changed) = mesh_block_incremental(
        &volume.chunks,
        interner.inner_mut(),
        &mut own_cache,
        &mut ext_cache,
        &mut cache,
        plan,
        external,
    );
    assert_eq!(
        got, expected,
        "internal incremental path must equal mesh_block_wrapped bit for bit"
    );
    assert!(changed, "the first internal build must report changed");
    assert_eq!(cache.internal_rebuilds(), 1);
    assert_eq!(cache.external_rebuilds(), 0);
    assert!(cache.contains(block));

    // A second run over identical content is a no-op.
    let (again, changed_again) = mesh_block_incremental(
        &volume.chunks,
        interner.inner_mut(),
        &mut own_cache,
        &mut ext_cache,
        &mut cache,
        plan,
        external,
    );
    assert_eq!(again, got);
    assert!(
        !changed_again,
        "identical occupancy must not report changed"
    );
    assert!(
        cache.last_report().is_some_and(|r| !r.changed()),
        "last report must be unchanged"
    );

    cache.clear();
    own_cache.clear(interner.inner_mut());
    nbr_cache.clear(interner.inner_mut());
}

/// World builder for the external-only test: carve an empty pocket into the
/// -X face of chunk (1,0,0) when carve_neighbour is set.
fn build_world(interner: &mut VoxelInterner, carve_neighbour: bool) -> VoxVolume {
    let mut volume = VoxVolume::new(FixedPoint::from_num(1));
    for &k in PLAN_KEYS.iter() {
        let carve = carve_neighbour && k == chunk_key(1, 0, 0);
        let tree = build_chunk(interner, |x, y, z| {
            if carve && x < 2 && y < 4 {
                0
            } else {
                plan_terrain(k.x, k.y, k.z, x, y, z)
            }
        });
        volume.insert_chunk(k, tree);
    }
    volume
}

/// Test 10b: external-only rebuild must skip occupancy and match a full
/// recompute; only neighbour chunks changed, so the block bitmaps are stable.
#[test]
fn incremental_entry_external_only_skips_occupancy() {
    let key = chunk_key(0, 0, 0);
    let neighbour = chunk_key(1, 0, 0);
    let block = MeshBlock::new(key, Lod::new(0));
    let internal_plan = MeshBlockDirty {
        block,
        internal: true,
        faces: FaceMask(0),
    };
    let external_plan = MeshBlockDirty {
        block,
        internal: false,
        faces: FaceMask(1 << 0), // YZPos: the +X face toward the neighbour
    };

    let mut interner = VoxelInterner::new(8 * 1024 * 1024);
    let volume_a = build_world(&mut interner, false);
    let mut nbr_cache = WrappedBlockCache::default();
    let external_a = warm_external(
        &mut nbr_cache,
        interner.inner_mut(),
        &volume_a.chunks,
        block,
    );

    let mut own_cache = WrappedBlockCache::default();
    let mut ext_cache = ExternalMaskCache::default();
    let mut cache = IncrementalMeshCache::default();
    let (before_batch, _) = mesh_block_incremental(
        &volume_a.chunks,
        interner.inner_mut(),
        &mut own_cache,
        &mut ext_cache,
        &mut cache,
        internal_plan,
        external_a,
    );
    assert!(cache.contains(block));

    // Snapshot the occupancy bitmaps (global + per-material + material counts).
    let before_snapshot = {
        let occ = cache.get(block).expect("cached").occupancy();
        (
            occ.global.clone(),
            occ.per_material.clone(),
            occ.materials.clone(),
        )
    };

    // World B: only the +X neighbour changes, right at its -X face.
    let volume_b = build_world(&mut interner, true);
    nbr_cache.invalidate_covered(interner.inner_mut(), neighbour);
    assert!(
        cache.contains(block),
        "wrapped invalidation must not drop the incremental entry"
    );
    let external_b = warm_external(
        &mut nbr_cache,
        interner.inner_mut(),
        &volume_b.chunks,
        block,
    );

    let (after_batch, changed) = mesh_block_incremental(
        &volume_b.chunks,
        interner.inner_mut(),
        &mut own_cache,
        &mut ext_cache,
        &mut cache,
        external_plan,
        external_b,
    );

    // External-only path ran; occupancy construction did not.
    assert_eq!(cache.internal_rebuilds(), 1, "occupancy rebuilt only once");
    assert_eq!(cache.external_rebuilds(), 1, "external-only path ran once");
    let after_snapshot = {
        let occ = cache.get(block).expect("cached").occupancy();
        (
            occ.global.clone(),
            occ.per_material.clone(),
            occ.materials.clone(),
        )
    };
    assert_eq!(
        before_snapshot, after_snapshot,
        "external-only must not touch the occupancy bitmaps"
    );
    assert!(changed, "the neighbour edit must change the block output");
    assert_ne!(after_batch, before_batch);

    // Full recompute over world B with the same external array.
    let mut fresh = WrappedBlockCache::default();
    let full = mesh_block_wrapped(
        &volume_b.chunks,
        interner.inner_mut(),
        &mut fresh,
        block,
        external_b,
    );
    assert_eq!(
        after_batch, full,
        "external-only result must equal a full recompute bit for bit"
    );

    // The dirty set only covers the affected (plane, dir) = YZPos (0, 0).
    let report = cache.last_report().expect("report recorded");
    assert!(!report.dirty.is_empty());
    assert!(
        report.dirty.iter().all(|k| k.plane == 0 && k.dir == 0),
        "a +X external change may only dirty YZPos (plane 0, dir 0) slices"
    );

    cache.clear();
    own_cache.clear(interner.inner_mut());
    nbr_cache.clear(interner.inner_mut());
    fresh.clear(interner.inner_mut());
}

/// First-seen external-only block cannot build a mesh (no occupancy, no wrap).
/// It must be explicit: empty batch, changed == false, cache untouched.
#[test]
fn incremental_entry_external_only_without_internal_is_empty() {
    let block = MeshBlock::new(chunk_key(0, 0, 0), Lod::new(0));
    let mut interner = VoxelInterner::new(4 * 1024 * 1024);
    let volume = build_volume(&mut interner, &PLAN_KEYS, None);
    let mut wrapped = WrappedBlockCache::default();
    let mut ext_cache = ExternalMaskCache::default();
    let mut cache = IncrementalMeshCache::default();

    let (batch, changed) = mesh_block_incremental(
        &volume.chunks,
        interner.inner_mut(),
        &mut wrapped,
        &mut ext_cache,
        &mut cache,
        MeshBlockDirty {
            block,
            internal: false,
            faces: FaceMask(1 << 0),
        },
        [None; 6],
    );
    assert!(batch.rects.is_empty() && batch.ao.is_empty());
    assert_eq!(batch.origin, block.origin);
    assert_eq!(batch.lod, block.lod);
    assert!(!changed);
    assert!(
        cache.is_empty(),
        "external-only must not create a cache entry"
    );

    wrapped.clear(interner.inner_mut());
}

/// Test 10c: changed tracks the output, not the (conservative) dirty set.
#[test]
fn incremental_changed_tracks_output_not_dirty() {
    let key = chunk_key(0, 0, 0);
    let block = MeshBlock::new(key, Lod::new(0));
    let mut interner = VoxelInterner::new(8 * 1024 * 1024);

    // Fully solid chunk: every interior voxel is occluded, so recolouring its
    // centre changes per_material but no emitted face.
    let mut solid = VoxTree::<u8>::new(MaxDepth::new(DEPTH));
    solid.fill(interner.inner_mut(), 1);
    let mut volume = VoxVolume::new(FixedPoint::from_num(1));
    volume.insert_chunk(key, solid);

    let mut wrapped = WrappedBlockCache::default();
    let mut ext_cache = ExternalMaskCache::default();
    let mut cache = IncrementalMeshCache::default();
    let plan = MeshBlockDirty {
        block,
        internal: true,
        faces: FaceMask(0),
    };

    let (before, before_changed) = mesh_block_incremental(
        &volume.chunks,
        interner.inner_mut(),
        &mut wrapped,
        &mut ext_cache,
        &mut cache,
        plan,
        [None; 6],
    );
    assert!(before_changed);

    let edited = volume.get_chunk_mut(&key).expect("chunk present").set(
        interner.inner_mut(),
        IVec3::new(16, 16, 16),
        2,
    );
    assert!(edited);
    wrapped.invalidate_covered(interner.inner_mut(), key);
    assert!(
        cache.contains(block),
        "wrapped invalidation must keep the incremental entry"
    );

    let (after, after_changed) = mesh_block_incremental(
        &volume.chunks,
        interner.inner_mut(),
        &mut wrapped,
        &mut ext_cache,
        &mut cache,
        plan,
        [None; 6],
    );
    assert_eq!(
        before, after,
        "an occluded material swap must not change the mesh"
    );
    assert!(
        !after_changed,
        "dirty but output-identical edit must report changed == false"
    );

    let report = cache.last_report().expect("report recorded");
    assert!(
        !report.dirty.is_empty(),
        "the edit must still dirty some slices"
    );
    assert_eq!(report.changed_slices, 0);

    cache.clear();
    wrapped.clear(interner.inner_mut());
}
