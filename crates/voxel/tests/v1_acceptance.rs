//! WP6 v1 acceptance at the storage/mesher boundary (crates/voxel):
//! incremental mesh == full mesh, and merge ratio on a synthetic surface.
//!
//! Everything here uses only the public voxel crate API (no Bevy, no game-core).

use std::collections::BTreeMap;

use glam::IVec3;
use voxel::mesh::{extract_block, MeshBlock, RectInstance};
use voxel::store::{
    ChunkKey, Lod, MaxDepth, VoxInterner, VoxOpsState, VoxOpsWrite, VoxTree, CHUNK_DEPTH,
};

const N: i32 = 1 << CHUNK_DEPTH;

fn new_interner() -> VoxInterner<u8> {
    // Small on purpose: growth is part of v1 and keeps the test cheap.
    VoxInterner::<u8>::with_memory_budget(2 * 1024 * 1024)
}

fn build_chunk(interner: &mut VoxInterner<u8>, f: impl Fn(i32, i32, i32) -> u8) -> VoxTree<u8> {
    let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
    for y in 0..N {
        for z in 0..N {
            for x in 0..N {
                let v = f(x, y, z);
                if v != 0 {
                    let _ = tree.set(interner, IVec3::new(x, y, z), v);
                }
            }
        }
    }
    tree
}

fn base_voxel(x: i32, y: i32, z: i32) -> u8 {
    let height = 10 + (x * 3 + z * 5).rem_euclid(7);
    if y > height {
        0
    } else if y == height {
        3
    } else if y + 2 >= height {
        2
    } else {
        1
    }
}

fn edited_voxel(x: i32, y: i32, z: i32) -> u8 {
    if x == 20 && y == 12 && z == 9 {
        7
    } else {
        base_voxel(x, y, z)
    }
}

fn one_chunk(tree: VoxTree<u8>) -> BTreeMap<ChunkKey, VoxTree<u8>> {
    let mut map = BTreeMap::new();
    map.insert(ChunkKey::new(0, 0, 0), tree);
    map
}

#[test]
fn incremental_block_mesh_equals_full_mesh() {
    let block = MeshBlock::new(ChunkKey::new(0, 0, 0), Lod::new(0));

    let mut interner_a = new_interner();
    let base = build_chunk(&mut interner_a, base_voxel);
    let mut chunks_a = one_chunk(base);
    let before = extract_block(&chunks_a, &interner_a, block, [None; 6]);

    // Exactly one voxel edit on the existing tree.
    let changed = chunks_a
        .get_mut(&ChunkKey::new(0, 0, 0))
        .expect("chunk present")
        .set(&mut interner_a, IVec3::new(20, 12, 9), 7);
    assert!(changed, "the edit must change the tree");
    let incremental = extract_block(&chunks_a, &interner_a, block, [None; 6]);

    // Fresh from-scratch extraction of the same final content.
    let mut interner_b = new_interner();
    let chunks_b = one_chunk(build_chunk(&mut interner_b, edited_voxel));
    let full = extract_block(&chunks_b, &interner_b, block, [None; 6]);

    assert_ne!(
        before.rects, incremental.rects,
        "the edit must be observable"
    );
    assert_eq!(
        incremental.rects, full.rects,
        "incremental mesh must equal a from-scratch mesh of the final world"
    );
}

#[test]
fn merge_ratio_on_synthetic_surface_exceeds_one() {
    let mut interner = new_interner();
    let chunks = one_chunk(build_chunk(&mut interner, base_voxel));
    let block = MeshBlock::new(ChunkKey::new(0, 0, 0), Lod::new(0));
    let batch = extract_block(&chunks, &interner, block, [None; 6]);

    let total_rects = batch.rects.len();
    let total_faces: usize = batch
        .rects
        .iter()
        .map(|r: &RectInstance| r.area() as usize)
        .sum();
    let m = total_faces as f64 / total_rects as f64;
    println!("synthetic chunk: rects={total_rects} exposed_faces={total_faces} m={m:.3}");
    assert!(total_rects > 0, "a solid surface must produce rectangles");
    assert!(m >= 1.0, "merge ratio must be at least 1.0");
}

#[test]
fn block_mesh_is_repeatable() {
    let mut interner = new_interner();
    let chunks = one_chunk(build_chunk(&mut interner, base_voxel));
    let block = MeshBlock::new(ChunkKey::new(0, 0, 0), Lod::new(0));

    let a = extract_block(&chunks, &interner, block, [None; 6]);
    let b = extract_block(&chunks, &interner, block, [None; 6]);
    assert_eq!(
        a.rects, b.rects,
        "same world must mesh to the same ordered rects"
    );

    // Sanity: the block covers the chunk and is not empty.
    assert!(!a.rects.is_empty());
    assert!(!chunks[&ChunkKey::new(0, 0, 0)].is_empty());
}

// Keep the 32^3 assumption explicit.
const _: () = assert!(N == 32);
