//! Engine-side incremental mesh cache and entry point.
//!
//! IncrementalMeshCache maps (block origin, lod) to a
//! voxel::mesh::IncrementalMesh (the slice-level incremental greedy cache) and
//! does a byte-budgeted LRU on top (default 32 MiB).
//!
//! Invalidation boundary:
//! * WrappedBlockCache::invalidate_covered only drops wrapped trees and MUST
//!   NOT remove IncrementalMeshCache entries.  After an edit the block is
//!   re-wrapped to a fresh occupancy, then rebuilt incrementally; removing the
//!   entry here would make every step a cold start.
//! * Only whole-body unload / clear / release_body / LOD eviction (a block no
//!   longer covered by any CoverPolicy) call remove / remove_covered / clear.
//!
//! Reference counting: IncrementalMesh holds only u64 / Vec / BTreeMap, no
//! BlockId and no interner reference, so it may safely outlive the wrapped
//! trees it was built from and eviction only drops values (iron law 2.3/2.4).
//!
//! Determinism : meshes is a BTreeMap; LRU eviction picks the smallest
//! (stamp, key) pair, so equal stamps break ties by ascending key.  No HashMap
//! iteration decides anything.

use std::collections::BTreeMap;

use bevy::prelude::*;
use voxel::mesh::{
    build_tree_occupancy, AoRectBatch, ExternalPlaneKind as ExternalPlane, IncrementalMesh,
    MeshBlock, MeshOccupancyData as OccupancyData, SliceRebuildReport,
};
use voxel::store::{ChunkKey, Lod, MaxDepth, VoxInterner, VoxTree};

use super::dirty::MeshBlockDirty;
use super::external::ExternalMaskCache;
use super::key::lod_block_origin;
use super::wrapped::WrappedBlockCache;

/// Default incremental cache byte budget (32 MiB).
pub const DEFAULT_INCREMENTAL_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// One cache entry: the incremental mesh plus its LRU stamp and byte size.
struct Entry {
    mesh: IncrementalMesh,
    stamp: u64,
    bytes: usize,
}

/// Engine incremental mesh cache (Bevy resource).
///
/// Owned by the app; see the module docs for the invalidation contract.
#[derive(Resource)]
pub struct IncrementalMeshCache {
    meshes: BTreeMap<(ChunkKey, Lod), Entry>,
    bytes: usize,
    max_bytes: usize,
    clock: u64,
    internal_rebuilds: u64,
    external_rebuilds: u64,
    last_report: Option<SliceRebuildReport>,
}

impl Default for IncrementalMeshCache {
    fn default() -> Self {
        Self::new(DEFAULT_INCREMENTAL_CACHE_BYTES)
    }
}

impl IncrementalMeshCache {
    /// Creates a cache with the given byte budget.
    #[must_use]
    pub fn new(max_bytes: usize) -> Self {
        Self {
            meshes: BTreeMap::new(),
            bytes: 0,
            max_bytes,
            clock: 0,
            internal_rebuilds: 0,
            external_rebuilds: 0,
            last_report: None,
        }
    }

    /// Configured byte budget.
    #[must_use]
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Current tracked bytes (see IncrementalMesh::memory_bytes).
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        self.bytes
    }

    /// Number of cached blocks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.meshes.len()
    }

    /// Whether the cache holds no blocks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.meshes.is_empty()
    }

    /// Whether the block has a cached incremental mesh.
    #[must_use]
    pub fn contains(&self, block: MeshBlock) -> bool {
        self.meshes.contains_key(&(block.origin, block.lod))
    }

    /// Read-only access to a cached incremental mesh.
    #[must_use]
    pub fn get(&self, block: MeshBlock) -> Option<&IncrementalMesh> {
        self.meshes
            .get(&(block.origin, block.lod))
            .map(|entry| &entry.mesh)
    }

    /// The report of the most recent internal or external apply.
    ///
    /// Instrumentation for tests/assertions (dirty slice range, changed count);
    /// it is not part of the mesh output contract.
    #[must_use]
    pub fn last_report(&self) -> Option<&SliceRebuildReport> {
        self.last_report.as_ref()
    }

    /// Number of internal (occupancy) rebuilds applied through this cache.
    #[must_use]
    pub fn internal_rebuilds(&self) -> u64 {
        self.internal_rebuilds
    }

    /// Number of external-only rebuilds applied through this cache.
    #[must_use]
    pub fn external_rebuilds(&self) -> u64 {
        self.external_rebuilds
    }

    /// Drops every entry.  Values only; no interner work is needed.
    pub fn clear(&mut self) {
        self.meshes.clear();
        self.bytes = 0;
        self.last_report = None;
    }

    /// Removes one block.  Returns true when it was cached.
    pub fn remove(&mut self, block: MeshBlock) -> bool {
        self.remove_key((block.origin, block.lod))
    }

    /// Removes the incremental mesh of every LOD block covering key.
    ///
    /// Call this on unload / release_body / LOD eviction.  Never call it from
    /// the per-edit wrapped invalidation path.
    pub fn remove_covered(&mut self, key: ChunkKey) {
        for lod in 0..=voxel::mesh::MAX_LOD {
            let lod = Lod::new(lod);
            let origin = lod_block_origin(key, lod);
            self.remove_key((origin, lod));
        }
    }

    /// Monotonic LRU clock.
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn remove_key(&mut self, key: (ChunkKey, Lod)) -> bool {
        match self.meshes.remove(&key) {
            Some(entry) => {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
                true
            }
            None => false,
        }
    }

    /// Evicts least-recently-used entries until the budget is met.
    ///
    /// The entry for protect is never chosen, so a single oversized block still
    /// yields a batch.  Ties on stamp fall back to ascending key (L3).
    fn evict_except(&mut self, protect: (ChunkKey, Lod)) {
        while self.bytes > self.max_bytes {
            let victim = self
                .meshes
                .iter()
                .filter(|(key, _)| **key != protect)
                .min_by_key(|(key, entry)| (entry.stamp, **key))
                .map(|(key, _)| *key);
            match victim {
                Some(key) => {
                    self.remove_key(key);
                }
                None => break,
            }
        }
    }

    /// Full first build or incremental rebuild from a fresh occupancy.
    ///
    /// On the very first call for a block this establishes the entry via
    /// IncrementalMesh::empty followed by rebuild, which returns a proper
    /// SliceRebuildReport for the initial fill.  Subsequent calls run the real
    /// slice-level diff.
    fn apply_internal(&mut self, block: MeshBlock, occupancy: OccupancyData) -> SliceRebuildReport {
        self.internal_rebuilds += 1;
        let key = (block.origin, block.lod);
        let stamp = self.tick();
        let n = occupancy.voxels_per_axis;
        let report = {
            let entry = self.meshes.entry(key).or_insert_with(|| {
                let mesh = IncrementalMesh::empty(n);
                let bytes = mesh.memory_bytes();
                Entry { mesh, stamp, bytes }
            });
            let report = entry.mesh.rebuild(occupancy);
            entry.bytes = entry.mesh.memory_bytes();
            entry.stamp = stamp;
            report
        };
        self.bytes = self.meshes.values().map(|entry| entry.bytes).sum();
        self.evict_except(key);
        self.last_report = Some(report.clone());
        report
    }

    /// External-only rebuild: replace external masks on the cached occupancy
    /// without rebuilding occupancy.  Returns None when the block has no cached
    /// mesh (the external path cannot create one on its own).
    fn apply_external(
        &mut self,
        block: MeshBlock,
        changed: &[(ExternalPlane, Box<[u64]>)],
    ) -> Option<SliceRebuildReport> {
        self.external_rebuilds += 1;
        let key = (block.origin, block.lod);
        let stamp = self.tick();
        let report = {
            let entry = self.meshes.get_mut(&key)?;
            let report = entry.mesh.rebuild_external(changed);
            entry.bytes = entry.mesh.memory_bytes();
            entry.stamp = stamp;
            report
        };
        self.bytes = self.meshes.values().map(|entry| entry.bytes).sum();
        self.evict_except(key);
        self.last_report = Some(report.clone());
        Some(report)
    }

    fn batch(&self, block: MeshBlock) -> Option<AoRectBatch> {
        self.meshes
            .get(&(block.origin, block.lod))
            .map(|entry| entry.mesh.batch(block.origin, block.lod))
    }
}

fn empty_batch(block: MeshBlock) -> AoRectBatch {
    AoRectBatch {
        origin: block.origin,
        lod: block.lod,
        rects: Vec::new(),
        ao: Vec::new(),
    }
}

/// Meshes one block incrementally.
///
/// * plan.internal == true: wrap the block (hash-consed) and build its
///   occupancy with the same build_tree_occupancy the full path uses, then
///   diff against the cached IncrementalMesh.  changed is
///   SliceRebuildReport::changed(), never dirty_slices > 0.
/// * plan.internal == false: external-only.  The block is NOT re-wrapped and
///   occupancy is NOT built; for each face bit in plan.faces the neighbour mask
///   comes from ExternalMaskCache (fast face-descending extract) and only the
///   affected slices are rerun via IncrementalMesh::rebuild_external.
///
/// external is the same [YZ+, YZ-, XZ+, XZ-, XY+, XY-] neighbour array the
/// full path takes.  The caller should source it from a WrappedBlockCache
/// distinct from the one passed here, because the two are borrowed separately.
///
/// Returns (batch, changed).  changed == false means the block rects+ao are
/// bit-identical to the previous version, so the caller can skip the upload.
///
/// First-seen external-only block: the cache has no IncrementalMesh and,
/// because the external path must not build occupancy, no mesh can be produced.
/// In that case the function returns an empty batch with changed == false and
/// leaves the cache untouched.  The engine must run an internal plan for a
/// block before any external-only plan for it.
#[must_use]
pub fn mesh_block_incremental(
    chunks: &BTreeMap<ChunkKey, VoxTree<u8>>,
    interner: &mut VoxInterner<u8>,
    wrapped: &mut WrappedBlockCache,
    ext_cache: &mut ExternalMaskCache,
    cache: &mut IncrementalMeshCache,
    plan: MeshBlockDirty,
    external: [Option<&VoxTree<u8>>; 6],
) -> (AoRectBatch, bool) {
    let block = plan.block;
    if plan.internal {
        // Wrap the block (or hit the cache), then build its occupancy with the
        // very same constructor the full extractor uses.
        let tree = wrapped.get_or_wrap(chunks, interner, block);
        let occupancy = build_tree_occupancy(tree, interner, block.lod, external, [false; 6]);
        let report = cache.apply_internal(block, occupancy);
        let changed = report.changed();
        let batch = cache.batch(block).unwrap_or_else(|| empty_batch(block));
        (batch, changed)
    } else {
        // External-only: no wrap, no occupancy.  Fetch just the changed faces.
        let depth = MaxDepth::new(voxel::mesh::BASE_DEPTH);
        let n = 1usize << depth.max();
        let mut changed_masks: Vec<(ExternalPlane, Box<[u64]>)> = Vec::new();
        for (p, neighbour) in external.iter().enumerate() {
            if !plan.faces.has(p) {
                continue;
            }
            let plane = ExternalPlane::from_index(p);
            let mask: Box<[u64]> = match neighbour {
                Some(neighbour) => {
                    let raw = ext_cache.get_or_generate_raw(interner, neighbour, plane, depth);
                    if raw.len() == n {
                        raw.to_vec().into_boxed_slice()
                    } else {
                        // Empty root: no occlusion on this side.
                        vec![0u64; n].into_boxed_slice()
                    }
                }
                None => vec![0u64; n].into_boxed_slice(),
            };
            changed_masks.push((plane, mask));
        }

        match cache.apply_external(block, &changed_masks) {
            Some(report) => {
                let changed = report.changed();
                let batch = cache.batch(block).unwrap_or_else(|| empty_batch(block));
                (batch, changed)
            }
            None => (empty_batch(block), false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voxel::chunk_key;

    fn entry(mesh: IncrementalMesh, stamp: u64, bytes: usize) -> Entry {
        Entry { mesh, stamp, bytes }
    }

    /// Equal stamps must evict the smallest (origin, lod) first: determinism.
    #[test]
    fn lru_tie_break_is_ascending_key() {
        let mut cache = IncrementalMeshCache::new(0);
        let a = chunk_key(0, 0, 0);
        let b = chunk_key(1, 0, 0);
        cache
            .meshes
            .insert((b, Lod::new(0)), entry(IncrementalMesh::empty(1), 7, 8));
        cache
            .meshes
            .insert((a, Lod::new(0)), entry(IncrementalMesh::empty(1), 7, 8));
        cache.bytes = 16;
        cache.max_bytes = 8;

        // Protect an unrelated key so the smallest key is eligible.
        cache.evict_except((chunk_key(9, 9, 9), Lod::new(0)));
        assert_eq!(cache.len(), 1);
        assert!(cache.meshes.contains_key(&(b, Lod::new(0))));
        assert!(!cache.meshes.contains_key(&(a, Lod::new(0))));
    }

    /// The entry passed as protect survives even when it alone exceeds budget.
    #[test]
    fn lru_never_evicts_protected_entry() {
        let mut cache = IncrementalMeshCache::new(1);
        let a = chunk_key(0, 0, 0);
        cache
            .meshes
            .insert((a, Lod::new(0)), entry(IncrementalMesh::empty(1), 3, 64));
        cache.bytes = 64;
        cache.evict_except((a, Lod::new(0)));
        assert_eq!(cache.len(), 1);
    }

    /// remove_covered drops every LOD entry for the key and frees bytes.
    #[test]
    fn remove_covered_drops_all_lods() {
        let mut cache = IncrementalMeshCache::new(1024);
        let key = chunk_key(0, 0, 0);
        for lod in 0..=voxel::mesh::MAX_LOD {
            let origin = lod_block_origin(key, Lod::new(lod));
            let mesh = IncrementalMesh::empty(1);
            let bytes = mesh.memory_bytes();
            cache
                .meshes
                .insert((origin, Lod::new(lod)), entry(mesh, lod as u64, bytes));
        }
        cache.bytes = cache.meshes.values().map(|e| e.bytes).sum();
        assert!(!cache.is_empty());
        cache.remove_covered(key);
        assert!(cache.is_empty());
        assert_eq!(cache.memory_bytes(), 0);
    }

    #[test]
    fn default_budget_is_32_mib() {
        assert_eq!(
            IncrementalMeshCache::default().max_bytes(),
            DEFAULT_INCREMENTAL_CACHE_BYTES
        );
        assert_eq!(DEFAULT_INCREMENTAL_CACHE_BYTES, 32 * 1024 * 1024);
    }
}
