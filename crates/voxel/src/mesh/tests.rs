//! Tests for the integer mesh layer (design sections 3.2, 9.5, 9.6, 9.7).

use std::collections::BTreeMap;

use glam::{IVec3, UVec2, UVec3};

use crate::mesh::occupancy::OccupancyDataBuilder;
use crate::mesh::{
    compute_face_ao, extract_block, extract_block_tree, extract_block_with_ao,
    generate_occupancy_masks, voxels_per_axis, wrap_block, AoRectBatch, Dir, MeshBlock, Plane,
    RectInstance, BASE_DEPTH, DIR_NEG, DIR_POS, MAX_LOD, PLANE_XY, PLANE_XZ, PLANE_YZ,
};
use crate::store::{
    BlockId, ChunkKey, Lod, MaxDepth, VoxInterner, VoxOpsBulkWrite, VoxOpsRead, VoxOpsWrite,
    VoxTree,
};

/// A canonical exposed 1x1 face: (plane, dir, slice, row, col).
type FaceKey = (u8, u8, u8, u8, u8);

fn key(x: i32, y: i32, z: i32) -> ChunkKey {
    ChunkKey { x, y, z }
}

fn new_interner() -> VoxInterner<u8> {
    // Small on purpose: the interner auto-grows, and meshing tests should not
    // allocate hundreds of MB up front.
    VoxInterner::with_memory_budget(16 * 1024 * 1024)
}

fn solid_chunk(interner: &mut VoxInterner<u8>) -> VoxTree<u8> {
    let mut tree = VoxTree::new(MaxDepth::new(BASE_DEPTH));
    tree.fill(interner, 1);
    tree
}

fn chunk_from_fn(interner: &mut VoxInterner<u8>, f: impl Fn(i32, i32, i32) -> u8) -> VoxTree<u8> {
    let depth = BASE_DEPTH;
    let n = 1i32 << depth;
    let mut tree = VoxTree::new(MaxDepth::new(depth));
    for y in 0..n {
        for z in 0..n {
            for x in 0..n {
                let value = f(x, y, z);
                if value != 0 {
                    tree.set(interner, IVec3::new(x, y, z), value);
                }
            }
        }
    }
    tree
}

fn dense(tree: &VoxTree<u8>, interner: &VoxInterner<u8>, depth: u8) -> Vec<u8> {
    let n = 1usize << depth;
    let mut data = vec![0u8; n * n * n];
    for y in 0..n {
        for z in 0..n {
            for x in 0..n {
                let value = tree
                    .get(interner, IVec3::new(x as i32, y as i32, z as i32))
                    .unwrap_or(0);
                data[y * n * n + z * n + x] = value;
            }
        }
    }
    data
}

/// Enumerates every exposed face of a dense material grid at 1x1 extents.
fn reference_faces(data: &[u8], n: usize) -> BTreeMap<FaceKey, u8> {
    let idx = |x: usize, y: usize, z: usize| y * n * n + z * n + x;
    let mut faces = BTreeMap::new();
    for y in 0..n {
        for z in 0..n {
            for x in 0..n {
                let m = data[idx(x, y, z)];
                if m == 0 {
                    continue;
                }
                if x + 1 >= n || data[idx(x + 1, y, z)] == 0 {
                    faces.insert((PLANE_YZ, DIR_POS, (x + 1) as u8, y as u8, z as u8), m);
                }
                if x == 0 || data[idx(x - 1, y, z)] == 0 {
                    faces.insert((PLANE_YZ, DIR_NEG, x as u8, y as u8, z as u8), m);
                }
                if y + 1 >= n || data[idx(x, y + 1, z)] == 0 {
                    faces.insert((PLANE_XZ, DIR_POS, (y + 1) as u8, z as u8, x as u8), m);
                }
                if y == 0 || data[idx(x, y - 1, z)] == 0 {
                    faces.insert((PLANE_XZ, DIR_NEG, y as u8, z as u8, x as u8), m);
                }
                if z + 1 >= n || data[idx(x, y, z + 1)] == 0 {
                    faces.insert((PLANE_XY, DIR_POS, (z + 1) as u8, y as u8, x as u8), m);
                }
                if z == 0 || data[idx(x, y, z - 1)] == 0 {
                    faces.insert((PLANE_XY, DIR_NEG, z as u8, y as u8, x as u8), m);
                }
            }
        }
    }
    faces
}

/// Expands a rect batch into its 1x1 faces, asserting no overlap.
fn expand_rects(rects: &[RectInstance]) -> BTreeMap<FaceKey, u8> {
    let mut faces = BTreeMap::new();
    for rect in rects {
        for dr in 0..rect.h as usize {
            for dc in 0..rect.w as usize {
                let k = (
                    rect.plane,
                    rect.dir,
                    rect.slice,
                    (rect.row as usize + dr) as u8,
                    (rect.col as usize + dc) as u8,
                );
                assert!(
                    faces.insert(k, rect.material).is_none(),
                    "rectangles overlap at {:?}",
                    k
                );
            }
        }
    }
    faces
}

fn plane_of(tag: u8) -> Plane {
    match tag {
        PLANE_YZ => Plane::YZ,
        PLANE_XZ => Plane::XZ,
        _ => Plane::XY,
    }
}

fn dir_of(tag: u8) -> Dir {
    if tag == DIR_POS {
        Dir::Pos
    } else {
        Dir::Neg
    }
}

/// 1. Rect equivalence: rects tile exactly the same surface as 1x1 faces.
#[test]
fn rects_tile_the_same_surface_as_faces() {
    let mut interner = new_interner();
    let mut chunks = BTreeMap::new();

    let blob = chunk_from_fn(&mut interner, |x, y, z| {
        let dx = x - 16;
        let dy = y - 16;
        let dz = z - 16;
        if y < 2 {
            1
        } else if dx * dx + dy * dy + dz * dz < 90 {
            2
        } else {
            0
        }
    });
    chunks.insert(key(0, 0, 0), blob);

    let block = MeshBlock::new(key(0, 0, 0), Lod::new(0));
    let batch = extract_block(&chunks, &interner, block, [None; 6]);

    let data = dense(chunks.get(&key(0, 0, 0)).unwrap(), &interner, BASE_DEPTH);
    let reference = reference_faces(&data, 32);
    let expanded = expand_rects(&batch.rects);

    assert_eq!(
        expanded, reference,
        "rects must cover exactly the exposed surface with the same materials"
    );
    assert!(
        batch.rects.len() <= reference.len(),
        "rect count {} must not exceed face count {}",
        batch.rects.len(),
        reference.len()
    );
    let projected: usize = batch.rects.iter().map(|r| r.area() as usize).sum();
    assert_eq!(projected, reference.len(), "projected area must match");
}

/// 2. Determinism: meshing twice yields identical ordered rects.
#[test]
fn meshing_is_deterministic() {
    let mut interner = new_interner();
    let mut chunks = BTreeMap::new();
    for cz in 0..2 {
        for cx in 0..2 {
            let tree = chunk_from_fn(&mut interner, |x, y, z| {
                if y < 3 && ((x + cx * 5) ^ (z + cz * 3)) & 3 != 0 {
                    1
                } else if y == 3 && (x + z) % 7 == 0 {
                    2
                } else {
                    0
                }
            });
            chunks.insert(key(cx, 0, cz), tree);
        }
    }

    let block = MeshBlock::new(key(0, 0, 0), Lod::new(0));
    let a = extract_block(&chunks, &interner, block, [None; 6]);
    let b = extract_block(&chunks, &interner, block, [None; 6]);
    assert_eq!(
        a.rects, b.rects,
        "same world must produce identical rect order"
    );
    assert_eq!(a, b);
}

/// 3. LOD block wrapping: hash sharing and equivalence to a single tree.
#[test]
fn lod_wrapping_shares_and_matches_single_tree() {
    let mut interner = new_interner();
    let mut chunks = BTreeMap::new();

    // A sparse but chunk-spanning pattern over a 2x2x2 group.
    let pattern = |lx: i32, ly: i32, lz: i32, cx: i32, cz: i32| -> u8 {
        if ly < 2 && ((lx + cx * 7) ^ (lz + cz * 5)) & 3 != 0 {
            1
        } else {
            0
        }
    };

    for cy in 0..2 {
        for cz in 0..2 {
            for cx in 0..2 {
                let tree = chunk_from_fn(&mut interner, |x, y, z| pattern(x, y, z, cx, cz));
                chunks.insert(key(cx, cy, cz), tree);
            }
        }
    }

    let block = MeshBlock::new(key(0, 0, 0), Lod::new(1));
    let wrapped_a = wrap_block(&chunks, &mut interner, block);
    let wrapped_b = wrap_block(&chunks, &mut interner, block);
    assert_eq!(
        wrapped_a.get_root_id(),
        wrapped_b.get_root_id(),
        "same 2^lod group must hash-share one BlockId"
    );

    // Equivalent single depth-(5+1) tree built one voxel at a time.
    let depth = BASE_DEPTH + 1;
    let mut big = VoxTree::new(MaxDepth::new(depth));
    let n = 1i32 << depth;
    for y in 0..n {
        for z in 0..n {
            for x in 0..n {
                let (cx, cy, cz) = (x / 32, y / 32, z / 32);
                let (lx, ly, lz) = (x % 32, y % 32, z % 32);
                let value = pattern(lx, ly, lz, cx, cz);
                let _ = cy;
                if value != 0 {
                    big.set(&mut interner, IVec3::new(x, y, z), value);
                }
            }
        }
    }

    let via_local = extract_block(&chunks, &interner, block, [None; 6]);
    let via_wrapped = extract_block_tree(&wrapped_a, &interner, block, [None; 6]);
    let via_big = extract_block_tree(&big, &interner, block, [None; 6]);

    assert_eq!(
        via_local.rects, via_wrapped.rects,
        "local wrapper must equal interned wrapper"
    );
    assert_eq!(
        via_wrapped.rects, via_big.rects,
        "wrapped block must equal an equivalent single depth-(5+L) tree"
    );
}

/// 4. LOD scaling: a rect long edge in base voxels equals the source node edge.
#[test]
fn lod_rect_edges_scale_with_node_edge() {
    for lod in 0..=MAX_LOD {
        let mut interner = new_interner();
        let mut chunks = BTreeMap::new();
        let solid = solid_chunk(&mut interner);
        chunks.insert(key(0, 0, 0), solid);

        // Resolution helper: wrapped depth (5+lod) at lod is always 32.
        assert_eq!(
            voxels_per_axis(MaxDepth::new(BASE_DEPTH + lod), Lod::new(lod)),
            32
        );

        let block = MeshBlock::new(key(0, 0, 0), Lod::new(lod));
        let batch = extract_block(&chunks, &interner, block, [None; 6]);

        let cell = (32u32 >> lod) as u8;
        assert_eq!(
            batch.rects.len(),
            6,
            "one solid subchunk is a cube at lod {}",
            lod
        );
        for rect in &batch.rects {
            assert_eq!(rect.w, cell, "lod {} width", lod);
            assert_eq!(rect.h, cell, "lod {} height", lod);
            assert_eq!(
                (rect.w as u32) << lod,
                32,
                "a rect edge times 2^lod must equal the 32-voxel base node edge (lod {})",
                lod
            );
        }
    }
}

/// Cross-body seam culling: external occupancy removes the shared faces.
#[test]
fn external_occupancy_culls_seam_faces() {
    let mut interner = new_interner();
    let mut chunks = BTreeMap::new();
    let a = solid_chunk(&mut interner);
    let b = solid_chunk(&mut interner);
    chunks.insert(key(0, 0, 0), a);
    chunks.insert(key(1, 0, 0), b);

    let a_tree = chunks.get(&key(0, 0, 0)).unwrap();
    let b_tree = chunks.get(&key(1, 0, 0)).unwrap();

    let mut external_a: [Option<&VoxTree<u8>>; 6] = [None; 6];
    external_a[0] = Some(b_tree); // YZPos
    let mut external_b: [Option<&VoxTree<u8>>; 6] = [None; 6];
    external_b[1] = Some(a_tree); // YZNeg

    let batch_a = extract_block(
        &chunks,
        &interner,
        MeshBlock::new(key(0, 0, 0), Lod::new(0)),
        external_a,
    );
    let batch_b = extract_block(
        &chunks,
        &interner,
        MeshBlock::new(key(1, 0, 0), Lod::new(0)),
        external_b,
    );

    assert!(
        !batch_a
            .rects
            .iter()
            .any(|r| r.plane == PLANE_YZ && r.dir == DIR_POS),
        "A plus-X seam faces must be culled"
    );
    assert!(
        !batch_b
            .rects
            .iter()
            .any(|r| r.plane == PLANE_YZ && r.dir == DIR_NEG),
        "B minus-X seam faces must be culled"
    );
    assert!(
        batch_a
            .rects
            .iter()
            .any(|r| r.plane == PLANE_YZ && r.dir == DIR_NEG),
        "A minus-X faces must remain"
    );
}

/// 5. Merge ratio: faces / rects must be at least 1.0 (prints the measured m).
#[test]
fn merge_ratio_is_at_least_one() {
    let mut interner = new_interner();
    let mut chunks = BTreeMap::new();
    let terrain = chunk_from_fn(&mut interner, |x, y, z| {
        let height = 2 + (x * 7 + z * 13).rem_euclid(9);
        if y < height {
            1
        } else if y == height && (x + z) % 5 == 0 {
            2
        } else {
            0
        }
    });
    chunks.insert(key(0, 0, 0), terrain);

    let block = MeshBlock::new(key(0, 0, 0), Lod::new(0));
    let batch = extract_block(&chunks, &interner, block, [None; 6]);
    let data = dense(chunks.get(&key(0, 0, 0)).unwrap(), &interner, BASE_DEPTH);
    let faces = reference_faces(&data, 32).len();
    let rects = batch.rects.len();
    assert!(rects > 0, "terrain must produce rectangles");
    let m = faces as f64 / rects as f64;
    println!("faces={} rects={} m={:.3}", faces, rects, m);
    assert!(m >= 1.0, "merge ratio must be >= 1.0, got {}", m);
}

/// 6. AO x merge consistency.
#[test]
fn ao_is_constant_across_merged_rects() {
    let mut interner = new_interner();

    // Flat floor: AO is constant (all level 3) so the top face merges fully.
    let mut flat_chunks = BTreeMap::new();
    let floor = chunk_from_fn(&mut interner, |_, y, _| if y < 2 { 1 } else { 0 });
    flat_chunks.insert(key(0, 0, 0), floor);
    let block = MeshBlock::new(key(0, 0, 0), Lod::new(0));
    let flat = extract_block_with_ao(&flat_chunks, &interner, block, [None; 6]);

    let mut flat_top = flat
        .rects
        .iter()
        .zip(flat.ao.iter())
        .filter(|(r, _)| r.plane == PLANE_XZ && r.dir == DIR_POS && r.slice == 2);
    let first = flat_top.next().expect("flat floor has a top face");
    assert_eq!(first.0.w, 32, "flat AO-constant top face must merge fully");
    assert_eq!(first.0.h, 32, "flat AO-constant top face must merge fully");
    assert_eq!(
        *first.1,
        [3, 3, 3, 3],
        "flat top corners are fully lit (AO level 3)"
    );
    assert!(flat_top.next().is_none(), "only one merged top rectangle");

    // Floor plus an off-plane pillar: AO varies near the pillar.
    let mut bumpy_chunks = BTreeMap::new();
    let pillar = chunk_from_fn(&mut interner, |x, y, z| {
        if y < 2 {
            1
        } else if x == 16 && z == 16 {
            2
        } else {
            0
        }
    });
    bumpy_chunks.insert(key(0, 0, 0), pillar);
    let bumpy = extract_block_with_ao(&bumpy_chunks, &interner, block, [None; 6]);

    // Per-face AO reference, built through the public occupancy entry point.
    let root = bumpy_chunks.get(&key(0, 0, 0)).unwrap().get_root_id();
    let mut builder = OccupancyDataBuilder::new(32);
    generate_occupancy_masks(
        &interner,
        &mut builder,
        &root,
        MaxDepth::new(BASE_DEPTH),
        UVec3::ZERO,
    );
    let occupancy = builder.build();

    let mut saw_single = false;
    for (rect, code) in bumpy.rects.iter().zip(bumpy.ao.iter()) {
        for dr in 0..rect.h as usize {
            for dc in 0..rect.w as usize {
                let row = rect.row as usize + dr;
                let col = rect.col as usize + dc;
                let bit = match rect.dir {
                    DIR_POS => rect.slice as i64 - 1,
                    _ => rect.slice as i64,
                };
                assert!(
                    bit >= 0,
                    "slice/dir combination must yield a non-negative bit"
                );
                let reference = compute_face_ao(
                    &occupancy,
                    plane_of(rect.plane),
                    dir_of(rect.dir),
                    bit as u32,
                    row,
                    col,
                );
                assert_eq!(
                    reference,
                    *code,
                    "merged rect corner AO must equal the per-face reference at {:?}",
                    (row, col)
                );
            }
        }
        if rect.plane == PLANE_XZ
            && rect.dir == DIR_POS
            && rect.slice == 2
            && rect.w == 1
            && rect.h == 1
        {
            saw_single = true;
        }
    }
    assert!(
        saw_single,
        "AO-varying top faces must fall back to 1x1 rectangles"
    );
}

/// Round-trip of the 39-bit integer packing.
#[test]
fn rect_instance_packing_round_trips() {
    let rect = RectInstance {
        plane: PLANE_XZ,
        dir: DIR_POS,
        slice: 17,
        row: 5,
        col: 31,
        w: 9,
        h: 4,
        material: 200,
    };
    let bits = rect.to_bits();
    assert!(bits < (1u64 << 39), "descriptor must fit in 39 bits");
    assert_eq!(RectInstance::from_bits(bits), rect);
    assert_eq!(rect.orientation(), PLANE_XZ * 2 + DIR_POS);
    assert_eq!(rect.area(), 36);
}

/// AoRectBatch helper sanity.
#[test]
fn ao_batch_into_batch_drops_ao() {
    let batch = AoRectBatch {
        origin: key(0, 0, 0),
        lod: Lod::new(0),
        rects: vec![RectInstance::default()],
        ao: vec![[0, 1, 2, 3]],
    };
    let plain = batch.clone().into_batch();
    assert_eq!(plain.rects.len(), 1);
    assert_eq!(plain.origin, key(0, 0, 0));
    assert_eq!(batch.rect_count(), 1);
}

/// Deterministic LCG used to build the pseudo-random test trees without
/// pulling in an external rand dependency.
fn lcg(state: &mut u64) -> u32 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*state >> 33) as u32
}

/// Test 3 (spec section 7): run the generic external extractor, the
/// face-descending fast extractor and the public entry point into three fresh
/// builders and require the external masks and external_exists flags to be
/// bitwise identical for every ExternalPlane.
fn assert_external_implementations_agree(
    interner: &VoxInterner<u8>,
    label: &str,
    root: BlockId,
    max_depth: MaxDepth,
    offset: UVec2,
) {
    use crate::mesh::occupancy::{
        generate_external_occupancy_mask_fast, generate_external_occupancy_mask_generic,
        ExternalPlane, InternerAccess,
    };

    let access = InternerAccess(interner);
    for plane in ExternalPlane::ALL {
        let mut generic = OccupancyDataBuilder::new(32);
        let mut fast = OccupancyDataBuilder::new(32);
        let mut public = OccupancyDataBuilder::new(32);

        generate_external_occupancy_mask_generic(
            &access,
            &mut generic,
            root,
            max_depth,
            plane,
            offset,
        );
        generate_external_occupancy_mask_fast(&access, &mut fast, root, max_depth, plane, offset);
        crate::mesh::generate_external_occupancy_mask(
            interner,
            &mut public,
            &root,
            max_depth,
            plane,
            offset,
        );

        assert_eq!(
            fast.external, generic.external,
            "external mask mismatch (tree {label}, plane {plane:?}, offset {offset:?})"
        );
        assert_eq!(
            fast.external_exists, generic.external_exists,
            "external_exists mismatch (tree {label}, plane {plane:?}, offset {offset:?})"
        );
        assert_eq!(
            public.external, fast.external,
            "public wrapper is not the fast path (tree {label}, plane {plane:?}, offset {offset:?})"
        );
        assert_eq!(
            public.external_exists, fast.external_exists,
            "public wrapper flags differ (tree {label}, plane {plane:?}, offset {offset:?})"
        );
    }
}

#[test]
fn external_fast_matches_generic_bit_for_bit() {
    let mut interner = new_interner();
    let max_depth = MaxDepth::new(BASE_DEPTH);
    let zero = UVec2::ZERO;
    // start_col is honoured by both implementations; start_row must stay 0
    // because the generic reference indexes a length-n row slice.
    let shifted = UVec2::new(8, 0);

    // Empty root.
    let empty = VoxTree::new(max_depth);

    // A default ("value 0") fill collapses back to the empty root: VoxTree::fill
    // clears for the default value, so a default leaf cannot be interned.
    let mut default_fill = VoxTree::new(max_depth);
    default_fill.fill(&mut interner, 0);
    assert!(default_fill.get_root_id().is_empty());

    // Single non-zero leaf root.
    let mut leaf = VoxTree::new(max_depth);
    leaf.fill(&mut interner, 7);

    // Multi-level tree: isolated voxels force branches down to the leaves.
    let mut multi = VoxTree::new(max_depth);
    for (x, y, z, v) in [
        (0, 0, 0, 1u8),
        (31, 0, 0, 2),
        (0, 31, 0, 3),
        (0, 0, 31, 4),
        (31, 31, 31, 5),
        (16, 16, 16, 6),
    ] {
        multi.set(&mut interner, IVec3::new(x, y, z), v);
    }

    // Whole-octant leaves: large uniform regions plus a few deeper edits.
    let mut blocky = VoxTree::new(max_depth);
    blocky.fill(&mut interner, 2);
    blocky.set(&mut interner, IVec3::new(3, 3, 3), 5);
    blocky.set(&mut interner, IVec3::new(20, 20, 20), 6);

    // Deterministic pseudo-random tree (LCG, no external rand dependency).
    let mut random = VoxTree::new(max_depth);
    let mut state = 0x1234_5678_9abc_def0u64;
    for _ in 0..512 {
        let x = (lcg(&mut state) % 32) as i32;
        let y = (lcg(&mut state) % 32) as i32;
        let z = (lcg(&mut state) % 32) as i32;
        let v = 1 + (lcg(&mut state) % 3) as u8;
        random.set(&mut interner, IVec3::new(x, y, z), v);
    }

    // A deeper tree (declared depth 6) sampled at depth 5: this reaches
    // branches at exactly max_depth, which occupied_at reads as a value.
    let mut deep = VoxTree::new(MaxDepth::new(BASE_DEPTH + 1));
    let mut deep_state = 0x0fed_cba9_8765_4321u64;
    for _ in 0..1024 {
        let x = (lcg(&mut deep_state) % 64) as i32;
        let y = (lcg(&mut deep_state) % 64) as i32;
        let z = (lcg(&mut deep_state) % 64) as i32;
        let v = 1 + (lcg(&mut deep_state) % 2) as u8;
        deep.set(&mut interner, IVec3::new(x, y, z), v);
    }

    let trees: [(&str, &VoxTree<u8>); 7] = [
        ("empty", &empty),
        ("default_fill", &default_fill),
        ("leaf", &leaf),
        ("multi", &multi),
        ("blocky", &blocky),
        ("random", &random),
        ("deep", &deep),
    ];
    for (label, tree) in trees {
        for offset in [zero, shifted] {
            assert_external_implementations_agree(
                &interner,
                label,
                tree.get_root_id(),
                max_depth,
                offset,
            );
        }
    }
}
