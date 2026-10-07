//! LOD block wrapping and resolution helpers.
//!
//! A MeshBlock at lod L covers 2^L x 2^L x 2^L base subchunks.  The wrapper
//! tree is built bottom-up with VoxInterner::get_or_create_branch, so identical
//! groups collapse to the same hashed parent (O(1), zero extra storage).  The
//! resulting tree has depth BASE_DEPTH + L, and is meshed at lod = L, which
//! yields exactly 32^3 cells regardless of L.

use std::collections::BTreeMap;

use glam::UVec3;

use crate::mesh::{MeshBlock, BASE_DEPTH, MAX_LOD};
use crate::store::{BlockId, ChunkKey, Lod, MaxDepth, VoxInterner, VoxTree};

/// Cells per axis of a tree of depth max_depth meshed at lod.
///
/// Equivalent to 1 << (max_depth - lod).  For a base (depth 5) tree at lod 0,
/// or a wrapped depth-(5+L) tree at lod L, this is always 32.
#[must_use]
pub fn voxels_per_axis(max_depth: MaxDepth, lod: Lod) -> u32 {
    1u32 << max_depth.max().saturating_sub(lod.lod())
}

/// Wraps the 2^lod x 2^lod x 2^lod base subchunks under block.origin into a
/// single VoxTree of depth BASE_DEPTH + lod.
///
/// Missing base subchunks become empty children.  The tree shares structure
/// with every other caller that wraps the same group (same BlockId).
///
/// # Panics
///
/// Panics when block.lod is greater than MAX_LOD (v1: 3).
#[must_use]
pub fn wrap_block(
    chunks: &BTreeMap<ChunkKey, VoxTree<u8>>,
    interner: &mut VoxInterner<u8>,
    block: MeshBlock,
) -> VoxTree<u8> {
    let lod = block.lod.lod();
    assert!(lod <= MAX_LOD, "lod exceeds MAX_LOD");
    let depth = BASE_DEPTH + lod;
    let root = build_interned(chunks, interner, block.origin, UVec3::ZERO, lod as u32);
    // get_or_create_branch returns a root that already owns one reference (or
    // EMPTY, which owns none); from_root transfers that ownership to the tree.
    VoxTree::from_root(MaxDepth::new(depth), root)
}

fn build_interned(
    chunks: &BTreeMap<ChunkKey, VoxTree<u8>>,
    interner: &mut VoxInterner<u8>,
    origin: ChunkKey,
    local: UVec3,
    level: u32,
) -> BlockId {
    if level == 0 {
        let key = ChunkKey {
            x: origin.x + local.x as i32,
            y: origin.y + local.y as i32,
            z: origin.z + local.z as i32,
        };
        match chunks.get(&key) {
            Some(tree) => {
                let root = tree.get_root_id();
                if root.is_empty() {
                    BlockId::EMPTY
                } else {
                    // Own one reference so the parent can transfer it to the branch.
                    interner.inc_ref(&root);
                    root
                }
            }
            None => BlockId::EMPTY,
        }
    } else {
        let half = 1u32 << (level - 1);
        let mut children = [BlockId::EMPTY; 8];
        for (index, slot) in children.iter_mut().enumerate() {
            let dx = (index & 1) as u32;
            let dy = ((index >> 1) & 1) as u32;
            let dz = ((index >> 2) & 1) as u32;
            *slot = build_interned(
                chunks,
                interner,
                origin,
                UVec3::new(
                    local.x + dx * half,
                    local.y + dy * half,
                    local.z + dz * half,
                ),
                level - 1,
            );
        }

        let mut types = 0u8;
        let mut mask = 0u8;
        for (index, child) in children.iter().enumerate() {
            if !child.is_empty() {
                mask |= 1 << index;
                if child.is_leaf() {
                    types |= 1 << index;
                }
            }
        }
        if mask == 0 {
            return BlockId::EMPTY;
        }
        interner.get_or_create_branch(children, types, mask)
    }
}
