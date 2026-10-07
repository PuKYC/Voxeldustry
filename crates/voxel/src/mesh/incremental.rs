//! Slice-level incremental greedy meshing.
//!
//! Reuses the full extractor's merge code: every slice is produced by the same
//! greedy_slice the full path calls, so bit-for-bit equivalence is a
//! construction property, not something two implementations have to agree on.
//!
//! A slice is the integer depth bit fed to generate_greedy_faces_for_slice
//! (range 0..n).  The frozen output order (plane, dir, material, slice, row,
//! col) is the order of the SliceKey BTreeMap followed by the (row, col)
//! order greedy already emits, so batch never sorts.
//!
//! Determinism (L3): every map/set here is a BTreeMap/BTreeSet; no hash
//! iteration decides output order.

use std::collections::{BTreeMap, BTreeSet};

use crate::mesh::greedy::{global_face_masks_for_plane, greedy_slice};
use crate::mesh::occupancy::{
    full_mask, AxisOccupancy, Dir, ExternalPlane, OccupancyData, OccupancyDataBuilder, Plane,
    PLANES,
};
use crate::mesh::{AoRectBatch, RectInstance};
use crate::store::{ChunkKey, Lod};

/// Frozen output-order prefix key: plane, dir, material, slice.
///
/// The field order is the sort order and matches the plane / dir / material /
/// slice fields of RectInstance.  slice is the depth bit in 0..n-1 (not the
/// RectInstance::slice boundary, which can be n for a positive face).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SliceKey {
    /// PLANE_YZ / PLANE_XZ / PLANE_XY.
    pub plane: u8,
    /// DIR_POS / DIR_NEG.
    pub dir: u8,
    /// Material id.
    pub material: u8,
    /// Depth bit in 0..n-1.
    pub slice: u32,
}

/// One slice's greedy product; rects and aos are the same length and in
/// (row, col) order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SliceFaces {
    /// Rectangles of this slice.
    pub rects: Vec<RectInstance>,
    /// AO codes, one per rect.
    pub aos: Vec<[u8; 4]>,
}

impl SliceFaces {
    /// True when the slice produces no geometry.
    #[must_use]
    fn is_empty(&self) -> bool {
        self.rects.is_empty() && self.aos.is_empty()
    }
}

/// Slice-level incremental mesh.
///
/// Holds only integer values (u64 / Vec / BTreeMap): no BlockId and no interner
/// reference, so it can safely outlive the wrapped trees it was built from
/// (spec iron law 2.3/2.4).
pub struct IncrementalMesh {
    occupancy: OccupancyData,
    slices: BTreeMap<SliceKey, SliceFaces>,
}

/// Rebuild report, for assertions and benchmarks (not an output contract).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SliceRebuildReport {
    /// Number of slices cached after the rebuild.
    pub total_slices: usize,
    /// Number of slice keys judged dirty this rebuild.
    pub dirty_slices: usize,
    /// Number of previous slices that were not touched at all.
    pub reused_slices: usize,
    /// Number of dirty slices whose recomputed SliceFaces differ from the old.
    pub changed_slices: usize,
    /// The dirty slice keys (ascending), for range assertions.
    pub dirty: BTreeSet<SliceKey>,
}

impl SliceRebuildReport {
    /// Whether the output actually changed.  Dirty != changed: conservative
    /// marking can leave a dirty slice byte-identical.
    #[must_use]
    pub fn changed(&self) -> bool {
        self.changed_slices > 0
    }
}

/// Global face masks for the three plane families, split by direction.
struct GlobalFaceMasks {
    pos: [Vec<u64>; 3],
    neg: [Vec<u64>; 3],
}

impl GlobalFaceMasks {
    fn compute(occupancy: &OccupancyData) -> Self {
        let n = occupancy.voxels_per_axis as usize;
        let plane_len = n * n;
        let mut pos = [
            vec![0u64; plane_len],
            vec![0u64; plane_len],
            vec![0u64; plane_len],
        ];
        let mut neg = [
            vec![0u64; plane_len],
            vec![0u64; plane_len],
            vec![0u64; plane_len],
        ];
        for plane_data in &PLANES {
            let family = plane_data.plane.plane_index();
            global_face_masks_for_plane(occupancy, plane_data, &mut pos[family], &mut neg[family]);
        }
        Self { pos, neg }
    }

    fn dir(&self, plane: Plane, dir: Dir) -> &[u64] {
        let family = plane.plane_index();
        match dir {
            Dir::Pos => &self.pos[family],
            Dir::Neg => &self.neg[family],
        }
    }
}

fn plane_from_tag(tag: u8) -> Plane {
    match tag {
        crate::mesh::PLANE_XZ => Plane::XZ,
        crate::mesh::PLANE_XY => Plane::XY,
        _ => Plane::YZ,
    }
}

fn dir_from_tag(tag: u8) -> Dir {
    if tag == crate::mesh::DIR_POS {
        Dir::Pos
    } else {
        Dir::Neg
    }
}

/// Looks up the per-material bitmap by material id (never by Vec index).
fn per_material_for(occupancy: &OccupancyData, material: u8) -> Option<&[u64]> {
    occupancy
        .materials
        .binary_search_by_key(&(material as usize), |&(id, _)| id)
        .ok()
        .map(|idx| occupancy.per_material[idx].as_slice())
}

/// Changes the {k-1, k, k+1} intersect [0, n-1] layers of a per-layer mask.
fn expand_layers(layers: u64, n: usize) -> u64 {
    let mut out = layers | (layers << 1) | (layers >> 1);
    if n < 64 {
        out &= (1u64 << n) - 1;
    }
    out
}

/// Per plane-family set of changed layers (bit k set when some cell's bit k
/// differs), diffing global and per_material by material id.
///
/// materials counts and external_exists deliberately do not participate.
fn diff_layers(old: &OccupancyData, next: &OccupancyData) -> [u64; 3] {
    let n = next.voxels_per_axis as usize;
    let plane_len = n * n;
    let mut layers = [0u64; 3];

    for (family, (old_plane, new_plane)) in old
        .global
        .chunks_exact(plane_len)
        .zip(next.global.chunks_exact(plane_len))
        .enumerate()
    {
        for (&a, &b) in old_plane.iter().zip(new_plane.iter()) {
            layers[family] |= a ^ b;
        }
    }

    let old_map: BTreeMap<usize, &[u64]> = old
        .materials
        .iter()
        .enumerate()
        .map(|(i, &(id, _))| (id, old.per_material[i].as_slice()))
        .collect();
    let new_map: BTreeMap<usize, &[u64]> = next
        .materials
        .iter()
        .enumerate()
        .map(|(i, &(id, _))| (id, next.per_material[i].as_slice()))
        .collect();
    let ids: BTreeSet<usize> = old_map.keys().chain(new_map.keys()).copied().collect();

    for id in ids {
        let old_bits = old_map.get(&id).copied();
        let new_bits = new_map.get(&id).copied();
        for (family, layer) in layers.iter_mut().enumerate() {
            let start = family * plane_len;
            let end = start + plane_len;
            match (old_bits, new_bits) {
                (Some(a), Some(b)) => {
                    for (&av, &bv) in a[start..end].iter().zip(&b[start..end]) {
                        *layer |= av ^ bv;
                    }
                }
                (Some(a), None) => {
                    for &av in &a[start..end] {
                        *layer |= av;
                    }
                }
                (None, Some(b)) => {
                    for &bv in &b[start..end] {
                        *layer |= bv;
                    }
                }
                (None, None) => {}
            }
        }
    }

    layers
}

/// Which external sides changed.
fn external_changed(old: &OccupancyData, next: &OccupancyData) -> [bool; 6] {
    let n = next.voxels_per_axis as usize;
    let mut changed = [false; 6];
    for (p, changed_p) in changed.iter_mut().enumerate() {
        let start = p * n;
        *changed_p = old.external[start..start + n] != next.external[start..start + n];
    }
    changed
}

/// Expands changed layers plus external changes into the dirty slice-key set.
///
/// Any cell change in a plane family marks {k-1, k, k+1} for ALL materials of
/// that family (conservative: a material change alters the global union and
/// therefore other materials' face masks and AO).  An external change reruns
/// every slice of just that (plane, dir).
fn build_dirty(
    old: &OccupancyData,
    next: &OccupancyData,
    layers: [u64; 3],
    ext_changed: [bool; 6],
) -> BTreeSet<SliceKey> {
    let n = next.voxels_per_axis as usize;
    let ids: BTreeSet<usize> = old
        .materials
        .iter()
        .map(|&(id, _)| id)
        .chain(next.materials.iter().map(|&(id, _)| id))
        .collect();

    let mut dirty = BTreeSet::new();
    for plane_data in &PLANES {
        let family = plane_data.plane.plane_index();
        for (dir, ext) in [(Dir::Pos, plane_data.pos), (Dir::Neg, plane_data.neg)] {
            let mut bits = expand_layers(layers[family], n);
            if ext_changed[ext as usize] {
                bits = full_mask(n);
            }
            if bits == 0 {
                continue;
            }
            for &id in &ids {
                let material = id as u8;
                let mut remaining = bits;
                while remaining != 0 {
                    let slice = remaining.trailing_zeros();
                    remaining &= remaining - 1;
                    dirty.insert(SliceKey {
                        plane: plane_data.plane.index(),
                        dir: dir.index(),
                        material,
                        slice,
                    });
                }
            }
        }
    }
    dirty
}

/// Recomputes and replaces the dirty slices, returning changed_slices.
///
/// Mirrors the full extractor's activity filter.  Dirty keys are visited in
/// sorted order, so every (plane, dir, material) group is contiguous: the
/// combined `per_material & global_dir` mask is computed once per group, and a
/// dirty slice whose bit is absent from that group's union emits nothing and
/// skips `greedy_slice` entirely.  Without this, the intentionally coarse
/// dirty set (all materials x {k-1, k, k+1} x both dirs x 3 families) would
/// call `greedy_slice` several times more often than the full rebuild calls
/// it, making the "incremental" path slower than the full one on large edits.
fn apply_dirty(
    slices: &mut BTreeMap<SliceKey, SliceFaces>,
    occupancy: &OccupancyData,
    masks: &GlobalFaceMasks,
    dirty: &BTreeSet<SliceKey>,
) -> usize {
    if dirty.is_empty() {
        return 0;
    }
    let n = occupancy.voxels_per_axis as usize;
    let plane_len = n * n;
    // extract_rects short-circuits to empty when all six external sides are
    // full.  Mirror that here so the incremental path stays bit-identical.
    let external_all_full = occupancy.external.iter().all(|&mask| mask == full_mask(n));

    let mut changed = 0usize;
    let mut scratch = vec![0u64; plane_len];
    // Union of the current group's combined mask; a zero bit means this slice
    // has no faces at all and greedy_slice would return empty.
    let mut active_depth = 0u64;
    let mut group: Option<(u8, u8, u8)> = None;

    for key in dirty {
        let tag = (key.plane, key.dir, key.material);
        let new_faces = if external_all_full {
            SliceFaces::default()
        } else {
            if group != Some(tag) {
                group = Some(tag);
                active_depth = 0;
                if let Some(per_material) = per_material_for(occupancy, key.material) {
                    let plane = plane_from_tag(key.plane);
                    let dir = dir_from_tag(key.dir);
                    let base = plane.plane_index() * plane_len;
                    let src = &per_material[base..base + plane_len];
                    let global_dir = masks.dir(plane, dir);
                    for (dst, (&a, &b)) in scratch.iter_mut().zip(src.iter().zip(global_dir.iter()))
                    {
                        *dst = a & b;
                        active_depth |= *dst;
                    }
                }
            }
            if (active_depth >> key.slice) & 1 == 0 {
                SliceFaces::default()
            } else {
                let plane = plane_from_tag(key.plane);
                let dir = dir_from_tag(key.dir);
                let mut faces = SliceFaces::default();
                greedy_slice(
                    occupancy,
                    &scratch,
                    plane,
                    dir,
                    key.material,
                    key.slice as usize,
                    &mut faces,
                );
                faces
            }
        };

        let is_changed = match slices.get(key) {
            Some(old) => old != &new_faces,
            None => !new_faces.is_empty(),
        };
        if is_changed {
            changed += 1;
            if new_faces.is_empty() {
                slices.remove(key);
            } else {
                slices.insert(*key, new_faces);
            }
        }
    }
    changed
}

/// Full greedy build into a fresh slice map.
fn build_full_into(occupancy: &OccupancyData, slices: &mut BTreeMap<SliceKey, SliceFaces>) {
    slices.clear();
    let n = occupancy.voxels_per_axis as usize;
    let plane_len = n * n;
    if occupancy.external.iter().all(|&mask| mask == full_mask(n)) {
        return;
    }
    let masks = GlobalFaceMasks::compute(occupancy);
    let mut scratch = vec![0u64; plane_len];

    for plane_data in &PLANES {
        let plane = plane_data.plane;
        let base = plane_data.offset_factor * plane_len;
        for (material_idx, &(material_id, _)) in occupancy.materials.iter().enumerate() {
            let per_material = &occupancy.per_material[material_idx];
            let material = material_id as u8;
            for dir in [Dir::Pos, Dir::Neg] {
                let src = &per_material[base..base + plane_len];
                let global_dir = masks.dir(plane, dir);
                for (dst, (&a, &b)) in scratch.iter_mut().zip(src.iter().zip(global_dir.iter())) {
                    *dst = a & b;
                }
                let mut active_depth = 0u64;
                for &mask in &scratch {
                    active_depth |= mask;
                }
                if active_depth == 0 {
                    continue;
                }
                let depth_occ = AxisOccupancy::new(active_depth);
                for slice_bit in depth_occ.min..depth_occ.max {
                    if (depth_occ.active >> slice_bit) & 1 == 0 {
                        continue;
                    }
                    let mut faces = SliceFaces::default();
                    greedy_slice(
                        occupancy, &scratch, plane, dir, material, slice_bit, &mut faces,
                    );
                    if !faces.is_empty() {
                        slices.insert(
                            SliceKey {
                                plane: plane.index(),
                                dir: dir.index(),
                                material,
                                slice: slice_bit as u32,
                            },
                            faces,
                        );
                    }
                }
            }
        }
    }
}

impl IncrementalMesh {
    /// Empty mesh at resolution voxels_per_axis; call rebuild once to fill.
    #[must_use]
    pub fn empty(voxels_per_axis: u32) -> Self {
        Self {
            occupancy: OccupancyDataBuilder::new(voxels_per_axis).build(),
            slices: BTreeMap::new(),
        }
    }

    /// Full build: equivalent to empty followed by one rebuild.
    #[must_use]
    pub fn new(occupancy: OccupancyData) -> Self {
        let mut slices = BTreeMap::new();
        build_full_into(&occupancy, &mut slices);
        Self { occupancy, slices }
    }

    /// Incremental rebuild: diff, rerun only dirty slices, reuse the rest.
    ///
    /// changed is decided by comparing each dirty slice's old and new
    /// SliceFaces, never by dirty_slices > 0.
    pub fn rebuild(&mut self, next: OccupancyData) -> SliceRebuildReport {
        let IncrementalMesh { occupancy, slices } = self;

        if occupancy.voxels_per_axis != next.voxels_per_axis {
            build_full_into(&next, slices);
            let dirty: BTreeSet<SliceKey> = slices.keys().copied().collect();
            let report = SliceRebuildReport {
                total_slices: slices.len(),
                dirty_slices: dirty.len(),
                reused_slices: 0,
                changed_slices: slices.len(),
                dirty,
            };
            *occupancy = next;
            return report;
        }

        let layers = diff_layers(occupancy, &next);
        let ext_changed = external_changed(occupancy, &next);
        let dirty = build_dirty(occupancy, &next, layers, ext_changed);
        let masks = GlobalFaceMasks::compute(&next);
        let changed_slices = apply_dirty(slices, &next, &masks, &dirty);

        let reported = slices.keys().filter(|key| dirty.contains(*key)).count();
        let report = SliceRebuildReport {
            total_slices: slices.len(),
            dirty_slices: dirty.len(),
            reused_slices: slices.len() - reported,
            changed_slices,
            dirty,
        };
        *occupancy = next;
        report
    }

    /// External-only rebuild: replace external[p] on the cached occupancy and
    /// rerun only the affected (plane, dir) slices.  Does not rebuild
    /// occupancy.
    pub fn rebuild_external(
        &mut self,
        changed: &[(ExternalPlane, Box<[u64]>)],
    ) -> SliceRebuildReport {
        let IncrementalMesh { occupancy, slices } = self;
        let n = occupancy.voxels_per_axis as usize;
        let mut ext_changed = [false; 6];
        for (plane, mask) in changed {
            let plane_index = *plane as usize;
            let start = plane_index * n;
            debug_assert_eq!(
                mask.len(),
                n,
                "external mask length must equal voxels_per_axis"
            );
            occupancy.external[start..start + n].copy_from_slice(mask);
            occupancy.external_exists[plane_index] = true;
            ext_changed[plane_index] = true;
        }

        // External changes never change the material set, so diffing the
        // occupancy against itself yields only the external-driven dirty keys.
        let dirty = build_dirty(occupancy, occupancy, [0u64; 3], ext_changed);
        let masks = GlobalFaceMasks::compute(occupancy);
        let changed_slices = apply_dirty(slices, occupancy, &masks, &dirty);

        let reported = slices.keys().filter(|key| dirty.contains(*key)).count();
        SliceRebuildReport {
            total_slices: slices.len(),
            dirty_slices: dirty.len(),
            reused_slices: slices.len() - reported,
            changed_slices,
            dirty,
        }
    }

    /// Frozen-order batch (SliceKey order, then per-slice (row, col) order).
    /// Never sorts.
    #[must_use]
    pub fn batch(&self, origin: ChunkKey, lod: Lod) -> AoRectBatch {
        let mut rects = Vec::new();
        let mut ao = Vec::new();
        for faces in self.slices.values() {
            rects.extend_from_slice(&faces.rects);
            ao.extend_from_slice(&faces.aos);
        }
        AoRectBatch {
            origin,
            lod,
            rects,
            ao,
        }
    }

    /// The cached occupancy.
    #[must_use]
    pub fn occupancy(&self) -> &OccupancyData {
        &self.occupancy
    }

    /// Approximate cached byte size (for LRU accounting).
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        let mut bytes = std::mem::size_of::<Self>();
        let occupancy = &self.occupancy;
        bytes += occupancy.global.capacity() * std::mem::size_of::<u64>();
        bytes += occupancy.external.capacity() * std::mem::size_of::<u64>();
        for mask in &occupancy.per_material {
            bytes += mask.capacity() * std::mem::size_of::<u64>();
        }
        bytes += occupancy.materials.capacity() * std::mem::size_of::<(usize, usize)>();
        for faces in self.slices.values() {
            bytes += faces.rects.capacity() * std::mem::size_of::<RectInstance>();
            bytes += faces.aos.capacity() * std::mem::size_of::<[u8; 4]>();
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use crate::mesh::greedy::extract_rects;
    use crate::mesh::occupancy::{AxisOccupancy, OccupancyData};
    use crate::mesh::{
        rect_key, AoRectBatch, ExternalPlane, RectInstance, DIR_NEG, DIR_POS, PLANE_XY, PLANE_XZ,
        PLANE_YZ,
    };
    use crate::store::{ChunkKey, Lod};

    use super::{IncrementalMesh, SliceKey};

    /// Deterministic LCG (no external rand dependency).
    fn lcg(state: &mut u64) -> u32 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (*state >> 33) as u32
    }

    /// Dense n^3 material grid, indexed y*n*n + z*n + x.
    struct Grid {
        n: usize,
        voxels: Vec<u8>,
    }

    impl Grid {
        fn new(n: usize) -> Self {
            Self {
                n,
                voxels: vec![0; n * n * n],
            }
        }

        fn get(&self, x: usize, y: usize, z: usize) -> u8 {
            self.voxels[y * self.n * self.n + z * self.n + x]
        }

        fn set(&mut self, x: usize, y: usize, z: usize, value: u8) {
            self.voxels[y * self.n * self.n + z * self.n + x] = value;
        }

        fn occupancy(&self, external: Vec<u64>, external_exists: [bool; 6]) -> OccupancyData {
            let n = self.n;
            let plane_len = n * n;
            let mut global = vec![0u64; 3 * plane_len];
            let mut per: BTreeMap<u8, Vec<u64>> = BTreeMap::new();
            let mut counts: BTreeMap<u8, usize> = BTreeMap::new();

            for y in 0..n {
                for z in 0..n {
                    for x in 0..n {
                        let material = self.get(x, y, z);
                        if material == 0 {
                            continue;
                        }
                        let entry = per
                            .entry(material)
                            .or_insert_with(|| vec![0u64; 3 * plane_len]);
                        let xb = 1u64 << x;
                        let yb = 1u64 << y;
                        let zb = 1u64 << z;
                        // YZ: normal x, row y, col z.
                        global[y * n + z] |= xb;
                        entry[y * n + z] |= xb;
                        // XZ: normal y, row z, col x.
                        global[plane_len + z * n + x] |= yb;
                        entry[plane_len + z * n + x] |= yb;
                        // XY: normal z, row y, col x.
                        global[2 * plane_len + y * n + x] |= zb;
                        entry[2 * plane_len + y * n + x] |= zb;
                        *counts.entry(material).or_insert(0) += 1;
                    }
                }
            }

            let global_active = active_axes(&global, n);
            let materials: Vec<(usize, usize)> = counts
                .iter()
                .map(|(&id, &count)| (id as usize, count))
                .collect();
            let per_material: Vec<Vec<u64>> = materials
                .iter()
                .map(|&(id, _)| per.remove(&(id as u8)).expect("material bitmap registered"))
                .collect();

            OccupancyData {
                voxels_per_axis: n as u32,
                global,
                global_active,
                external,
                external_exists,
                per_material,
                materials,
            }
        }
    }

    fn active_axes(global: &[u64], n: usize) -> [AxisOccupancy; 6] {
        let plane_len = n * n;
        let mut active = [0u64; 6];
        for (family, chunk) in global.chunks_exact(plane_len).enumerate() {
            let (row_idx, col_idx) = match family {
                0 => (0usize, 1usize),
                1 => (2, 3),
                _ => (4, 5),
            };
            for row in 0..n {
                for col in 0..n {
                    if chunk[row * n + col] != 0 {
                        active[row_idx] |= 1u64 << row;
                        active[col_idx] |= 1u64 << col;
                    }
                }
            }
        }
        active.map(AxisOccupancy::new)
    }

    fn blank_occ(n: usize) -> OccupancyData {
        OccupancyData {
            voxels_per_axis: n as u32,
            global: vec![0; 3 * n * n],
            global_active: [AxisOccupancy::new(0); 6],
            external: vec![0; 6 * n],
            external_exists: [false; 6],
            per_material: vec![vec![0; 3 * n * n]],
            materials: vec![(1, 0)],
        }
    }

    fn origin() -> ChunkKey {
        ChunkKey { x: 0, y: 0, z: 0 }
    }

    /// Full reference path: extract_rects materialised by rect_key sort.
    fn full_batch(occ: &OccupancyData) -> AoRectBatch {
        let mut rects = Vec::new();
        let mut aos = Vec::new();
        extract_rects(occ, &mut rects, &mut aos);
        let mut pairs: Vec<(RectInstance, [u8; 4])> = rects.into_iter().zip(aos).collect();
        pairs.sort_by_key(|a| rect_key(&a.0));
        let mut sorted_rects = Vec::with_capacity(pairs.len());
        let mut sorted_ao = Vec::with_capacity(pairs.len());
        for (rect, code) in pairs {
            sorted_rects.push(rect);
            sorted_ao.push(code);
        }
        AoRectBatch {
            origin: origin(),
            lod: Lod::new(0),
            rects: sorted_rects,
            ao: sorted_ao,
        }
    }

    fn random_external(state: &mut u64, n: usize) -> Vec<u64> {
        let mut external = vec![0u64; 6 * n];
        for mask in &mut external {
            *mask = u64::from(lcg(state)) & ((1u64 << n) - 1);
        }
        external
    }

    fn random_xyz(state: &mut u64, n: usize) -> (usize, usize, usize) {
        (
            (lcg(state) % n as u32) as usize,
            (lcg(state) % n as u32) as usize,
            (lcg(state) % n as u32) as usize,
        )
    }

    fn assert_batches_match(mesh: &IncrementalMesh, occ: &OccupancyData, label: &str) {
        let got = mesh.batch(origin(), Lod::new(0));
        let want = full_batch(occ);
        assert_eq!(got.rects, want.rects, "rects mismatch ({label})");
        assert_eq!(got.ao, want.ao, "ao mismatch ({label})");
    }

    // ---- Test 6: incremental == full, bit for bit -------------------------

    #[test]
    fn incremental_matches_full_over_random_edit_sequence() {
        let n = 12usize;
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut grid = Grid::new(n);
        for _ in 0..(n * n * n / 3) {
            let (x, y, z) = random_xyz(&mut state, n);
            let material = 1 + (lcg(&mut state) % 3) as u8;
            grid.set(x, y, z, material);
        }
        let mut external = random_external(&mut state, n);
        let mut exists = [false; 6];
        let first = grid.occupancy(external.clone(), exists);
        let mut mesh = IncrementalMesh::new(first.clone());
        assert_batches_match(&mesh, &first, "seed");

        for step in 0..80 {
            match lcg(&mut state) % 6 {
                0 => {
                    let (x, y, z) = random_xyz(&mut state, n);
                    grid.set(x, y, z, 1 + (lcg(&mut state) % 3) as u8);
                }
                1 => {
                    let (x, y, z) = random_xyz(&mut state, n);
                    grid.set(x, y, z, 0);
                }
                2 => {
                    let (x, y, z) = random_xyz(&mut state, n);
                    grid.set(x, y, z, 1 + (lcg(&mut state) % 4) as u8);
                }
                3 => {
                    let (x, _, _) = random_xyz(&mut state, n);
                    for y in 0..n {
                        for z in 0..n {
                            grid.set(x, y, z, 0);
                        }
                    }
                }
                // Delete a whole material id (add/remove coverage).
                4 => {
                    let target = 1 + (lcg(&mut state) % 4) as u8;
                    for y in 0..n {
                        for z in 0..n {
                            for x in 0..n {
                                if grid.get(x, y, z) == target {
                                    grid.set(x, y, z, 0);
                                }
                            }
                        }
                    }
                }
                _ => {
                    external = random_external(&mut state, n);
                    exists = [true; 6];
                }
            }

            let next = grid.occupancy(external.clone(), exists);
            let report = mesh.rebuild(next.clone());
            assert_batches_match(
                &mesh,
                &next,
                &format!("step {step} (dirty {})", report.dirty_slices),
            );
        }
    }

    #[test]
    fn incremental_matches_full_for_all_full_external() {
        let n = 5usize;
        let mut grid = Grid::new(n);
        grid.set(1, 1, 1, 1);
        grid.set(3, 2, 2, 2);
        let full = (1u64 << n) - 1;
        let occ = grid.occupancy(vec![full; 6 * n], [true; 6]);
        let mesh = IncrementalMesh::new(occ.clone());
        assert!(mesh.batch(origin(), Lod::new(0)).rects.is_empty());
        assert_batches_match(&mesh, &occ, "all-full external");
    }

    // ---- Test 7: dirty slice boundary coverage ----------------------------

    fn set_global_bit(occ: &mut OccupancyData, family: usize, row: usize, col: usize, k: usize) {
        let n = occ.voxels_per_axis as usize;
        occ.global[family * n * n + row * n + col] |= 1u64 << k;
        let (row_idx, col_idx) = match family {
            0 => (0usize, 1usize),
            1 => (2, 3),
            _ => (4, 5),
        };
        occ.global_active[row_idx] =
            AxisOccupancy::new(occ.global_active[row_idx].active | (1u64 << row));
        occ.global_active[col_idx] =
            AxisOccupancy::new(occ.global_active[col_idx].active | (1u64 << col));
    }

    fn expanded_layers(k: usize, n: usize) -> BTreeSet<u32> {
        let mut set = BTreeSet::new();
        for delta in -1i32..=1 {
            let layer = k as i32 + delta;
            if layer >= 0 && (layer as usize) < n {
                set.insert(layer as u32);
            }
        }
        set
    }

    fn expected_dirty_for_family(
        family: usize,
        n: usize,
        k: usize,
        material: u8,
    ) -> BTreeSet<SliceKey> {
        let plane = [PLANE_YZ, PLANE_XZ, PLANE_XY][family];
        let mut set = BTreeSet::new();
        for dir in [DIR_POS, DIR_NEG] {
            for &slice in &expanded_layers(k, n) {
                set.insert(SliceKey {
                    plane,
                    dir,
                    material,
                    slice,
                });
            }
        }
        set
    }

    #[test]
    fn dirty_slices_cover_ao_layers_and_clamp() {
        let n = 8usize;
        for k in 0..n {
            for family in 0..3 {
                let old = blank_occ(n);
                let mut next = blank_occ(n);
                set_global_bit(&mut next, family, 4, 5, k);

                let mut mesh = IncrementalMesh::new(old);
                let report = mesh.rebuild(next);

                let expected = expected_dirty_for_family(family, n, k, 1);
                assert_eq!(
                    report.dirty, expected,
                    "family {family} layer {k} dirty keys"
                );
                let slices: BTreeSet<u32> = report.dirty.iter().map(|key| key.slice).collect();
                assert_eq!(slices, expanded_layers(k, n));
                assert!(report.dirty.iter().all(|key| (key.slice as usize) < n));
            }
        }
    }

    #[test]
    fn whole_voxel_edit_dirties_the_union_of_three_families() {
        let n = 10usize;
        let k = 4usize;
        let old = blank_occ(n);
        let mut next = blank_occ(n);
        for family in 0..3 {
            set_global_bit(&mut next, family, 3, 6, k);
        }

        let mut mesh = IncrementalMesh::new(old);
        let report = mesh.rebuild(next);

        let mut expected = BTreeSet::new();
        for family in 0..3 {
            expected.extend(expected_dirty_for_family(family, n, k, 1));
        }
        assert_eq!(report.dirty, expected);
    }

    // ---- Test 8: output order ---------------------------------------------

    #[test]
    fn batch_is_monotone_and_matches_sorted_full_path() {
        let n = 9usize;
        let mut state = 0xdead_beef_cafe_babeu64;
        let mut grid = Grid::new(n);
        for _ in 0..(n * n * n / 2) {
            let (x, y, z) = random_xyz(&mut state, n);
            grid.set(x, y, z, 1 + (lcg(&mut state) % 3) as u8);
        }
        let occ = grid.occupancy(random_external(&mut state, n), [true; 6]);
        let mesh = IncrementalMesh::new(occ.clone());
        let batch = mesh.batch(origin(), Lod::new(0));

        for pair in batch.rects.windows(2) {
            assert!(
                rect_key(&pair[0]) <= rect_key(&pair[1]),
                "batch rect_key must be monotone"
            );
        }
        assert_batches_match(&mesh, &occ, "ordered");
    }

    // ---- Test 9: material id swap with unchanged global -------------------

    #[test]
    fn material_id_swap_is_caught_without_global_change() {
        let n = 6usize;
        let mut state = 0x0fed_cba9_8765_4321u64;
        let mut old_grid = Grid::new(n);
        for _ in 0..(n * n * n / 3) {
            let (x, y, z) = random_xyz(&mut state, n);
            old_grid.set(x, y, z, 1);
        }
        let external = vec![0u64; 6 * n];
        let old_occ = old_grid.occupancy(external.clone(), [false; 6]);

        let mut new_grid = Grid::new(n);
        for y in 0..n {
            for z in 0..n {
                for x in 0..n {
                    if old_grid.get(x, y, z) != 0 {
                        new_grid.set(x, y, z, 2);
                    }
                }
            }
        }
        let new_occ = new_grid.occupancy(external, [false; 6]);
        assert_eq!(
            old_occ.global, new_occ.global,
            "global union must be unchanged"
        );
        assert_ne!(
            old_occ.materials, new_occ.materials,
            "material ids must differ (bitmaps may be identical)"
        );

        let mut mesh = IncrementalMesh::new(old_occ);
        let report = mesh.rebuild(new_occ.clone());
        assert!(report.dirty_slices > 0, "swap must mark slices dirty");
        assert!(report.changed(), "swap must change the output");
        assert_batches_match(&mesh, &new_occ, "material swap");
    }

    #[test]
    fn rebuild_external_only_reruns_affected_slices() {
        let n = 7usize;
        let mut state = 0xabcd_ef01_2345_6789u64;
        let mut grid = Grid::new(n);
        for _ in 0..(n * n * n / 2) {
            let (x, y, z) = random_xyz(&mut state, n);
            grid.set(x, y, z, 1 + (lcg(&mut state) % 2) as u8);
        }
        let occ = grid.occupancy(vec![0u64; 6 * n], [false; 6]);
        let mut mesh = IncrementalMesh::new(occ.clone());

        let mask: Box<[u64]> = (0..n)
            .map(|row| {
                let mut bits = 0u64;
                for col in 0..n {
                    if (row + col) % 2 == 0 {
                        bits |= 1u64 << col;
                    }
                }
                bits
            })
            .collect();
        let report = mesh.rebuild_external(&[(ExternalPlane::YZPos, mask)]);

        for key in &report.dirty {
            assert_eq!(key.plane, PLANE_YZ);
            assert_eq!(key.dir, DIR_POS);
        }
        assert!(report.dirty_slices > 0);

        let mut reference_occ = occ.clone();
        reference_occ.external[0..n].copy_from_slice(&mesh.occupancy().external[0..n]);
        reference_occ.external_exists[0] = true;
        assert_batches_match(&mesh, &reference_occ, "external-only");
    }

    #[test]
    fn empty_then_rebuild_matches_full_build() {
        let n = 7usize;
        let mut state = 0x5555_aaaa_1234_9876u64;
        let mut grid = Grid::new(n);
        for _ in 0..(n * n * n / 3) {
            let (x, y, z) = random_xyz(&mut state, n);
            grid.set(x, y, z, 1 + (lcg(&mut state) % 2) as u8);
        }
        let occ = grid.occupancy(random_external(&mut state, n), [true; 6]);

        let mut incremental = IncrementalMesh::empty(n as u32);
        incremental.rebuild(occ.clone());
        let full = IncrementalMesh::new(occ);
        assert_eq!(
            incremental.batch(origin(), Lod::new(0)),
            full.batch(origin(), Lod::new(0))
        );
    }

    #[test]
    fn resolution_change_triggers_full_rebuild() {
        let mut mesh = IncrementalMesh::empty(4);
        let mut grid = Grid::new(8);
        grid.set(1, 1, 1, 1);
        grid.set(6, 6, 6, 2);
        let occ = grid.occupancy(vec![0u64; 48], [false; 6]);
        let report = mesh.rebuild(occ.clone());
        assert_eq!(report.reused_slices, 0);
        assert_eq!(report.dirty_slices, report.total_slices);
        assert_batches_match(&mesh, &occ, "resolution change");
    }

    #[test]
    fn dirty_but_unchanged_reports_no_change() {
        let n = 6usize;
        let mut grid = Grid::new(n);
        for y in 0..n {
            for z in 0..n {
                for x in 0..n {
                    grid.set(x, y, z, 1);
                }
            }
        }
        let external = vec![0u64; 6 * n];
        let old = grid.occupancy(external.clone(), [false; 6]);

        let mut swapped = grid;
        swapped.set(n / 2, n / 2, n / 2, 2);
        let next = swapped.occupancy(external, [false; 6]);
        assert_eq!(
            old.global, next.global,
            "solid global union is unchanged by an interior swap"
        );

        let mut mesh = IncrementalMesh::new(old);
        let report = mesh.rebuild(next.clone());
        assert!(report.dirty_slices > 0, "conservative marking is dirty");
        assert!(
            !report.changed(),
            "an occluded interior swap must not change the output"
        );
        assert_batches_match(&mesh, &next, "occluded swap");
    }

    /// Literal copy of the pre-refactor extract_rects body, kept only to assert
    /// the refactor did not change a single output bit or the raw call order.
    #[allow(clippy::needless_range_loop)]
    fn extract_rects_reference(
        occupancy: &OccupancyData,
        rects: &mut Vec<RectInstance>,
        aos: &mut Vec<[u8; 4]>,
    ) {
        use crate::mesh::greedy::{generate_greedy_faces_for_slice, SliceData};
        use crate::mesh::occupancy::{full_mask, Dir, PLANES};

        let n = occupancy.voxels_per_axis as usize;
        let full = full_mask(n);

        if occupancy.external.iter().all(|&mask| mask == full) {
            return;
        }

        let mut global_face_masks_pos = vec![0u64; n * n];
        let mut global_face_masks_neg = vec![0u64; n * n];
        let mut material_face_masks_pos = vec![0u64; n * n];
        let mut material_face_masks_neg = vec![0u64; n * n];
        let mut faces = vec![0u64; n];

        for plane_data in &PLANES {
            let base = plane_data.offset_factor * n * n;
            let ext_pos_start = plane_data.pos as usize * n;
            let external_pos = &occupancy.external[ext_pos_start..ext_pos_start + n];
            let ext_neg_start = plane_data.neg as usize * n;
            let external_neg = &occupancy.external[ext_neg_start..ext_neg_start + n];
            let rows = occupancy.global_active[plane_data.global_active_idx];
            let cols = occupancy.global_active[plane_data.global_active_idx + 1];
            let min_row = rows.min;
            let max_row = rows.max;
            let min_col = cols.min;
            let max_col = cols.max;

            for row in min_row..max_row {
                if (rows.active >> row) & 1 == 0 {
                    continue;
                }
                let base_idx = row * n;
                for col in min_col..max_col {
                    if (cols.active >> col) & 1 == 0 {
                        continue;
                    }
                    let idx = base_idx + col;
                    let mask = occupancy.global[base + idx];
                    let mut global_pos = !(mask >> 1) & mask;
                    let mut global_neg = !(mask << 1) & mask;
                    global_pos &= !(((external_pos[row] >> col) & 1) << (n - 1));
                    global_neg &= !((external_neg[row] >> col) & 1);
                    global_face_masks_pos[idx] = global_pos;
                    global_face_masks_neg[idx] = global_neg;
                }
            }

            for (material_idx, (material_id, _)) in occupancy.materials.iter().enumerate() {
                let occupancy_per_material = &occupancy.per_material[material_idx];
                let mut active_row_pos = 0u64;
                let mut active_col_pos = 0u64;
                let mut active_depth_pos = 0u64;
                let mut count_pos = 0usize;
                let mut active_row_neg = 0u64;
                let mut active_col_neg = 0u64;
                let mut active_depth_neg = 0u64;
                let mut count_neg = 0usize;

                for row in min_row..max_row {
                    let base_idx = row * n;
                    for col in min_col..max_col {
                        let idx = base_idx + col;
                        let mask = occupancy_per_material[base + idx];
                        let mask_pos = mask & global_face_masks_pos[idx];
                        active_depth_pos |= mask_pos;
                        material_face_masks_pos[idx] = mask_pos;
                        count_pos += mask_pos.count_ones() as usize;
                        let pos_active = (mask_pos != 0) as u64;
                        active_row_pos |= pos_active << row;
                        active_col_pos |= pos_active << col;
                        let mask_neg = mask & global_face_masks_neg[idx];
                        active_depth_neg |= mask_neg;
                        material_face_masks_neg[idx] = mask_neg;
                        count_neg += mask_neg.count_ones() as usize;
                        let neg_active = (mask_neg != 0) as u64;
                        active_row_neg |= neg_active << row;
                        active_col_neg |= neg_active << col;
                    }
                }

                if count_pos == 0 && count_neg == 0 {
                    continue;
                }

                let material = *material_id as u8;
                let dirs = [
                    (
                        Dir::Pos,
                        count_pos,
                        active_depth_pos,
                        active_row_pos,
                        active_col_pos,
                        true,
                    ),
                    (
                        Dir::Neg,
                        count_neg,
                        active_depth_neg,
                        active_row_neg,
                        active_col_neg,
                        false,
                    ),
                ];

                for (dir, total, active_depth, active_row, active_col, is_pos) in dirs {
                    if total == 0 {
                        continue;
                    }
                    let masks: &[u64] = if is_pos {
                        &material_face_masks_pos
                    } else {
                        &material_face_masks_neg
                    };
                    let depth_occ = AxisOccupancy::new(active_depth);
                    let row_occ = AxisOccupancy::new(active_row);
                    let col_occ = AxisOccupancy::new(active_col);
                    let mut faces_left = total;

                    for slice_bit in depth_occ.min..depth_occ.max {
                        if (depth_occ.active >> slice_bit) & 1 == 0 {
                            continue;
                        }
                        let current_faces_left = faces_left;
                        for face in faces.iter_mut() {
                            *face = 0;
                        }

                        for row in row_occ.min..row_occ.max {
                            if (row_occ.active >> row) & 1 == 0 {
                                continue;
                            }
                            let base_idx = row * n;
                            for col in col_occ.min..col_occ.max {
                                if (col_occ.active >> col) & 1 == 0 {
                                    continue;
                                }
                                let idx = base_idx + col;
                                if ((masks[idx] >> slice_bit) & 1) != 0 {
                                    faces[row] |= 1u64 << col;
                                    faces_left -= 1;
                                }
                            }
                        }

                        let faces_total = current_faces_left - faces_left;
                        let slice_data = SliceData {
                            min_row: row_occ.min,
                            max_row: row_occ.max,
                            plane: plane_data.plane,
                            dir,
                            material,
                        };

                        generate_greedy_faces_for_slice(
                            occupancy,
                            &slice_data,
                            slice_bit as u32,
                            faces_total,
                            &faces,
                            rects,
                            aos,
                        );

                        if faces_left == 0 {
                            break;
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn refactor_preserves_extract_rects_bit_for_bit() {
        let n = 10usize;
        let mut state = 0x1357_9bdf_2468_ace0u64;
        for round in 0..8 {
            let mut grid = Grid::new(n);
            for _ in 0..(n * n * n / 2) {
                let (x, y, z) = random_xyz(&mut state, n);
                grid.set(x, y, z, 1 + (lcg(&mut state) % 4) as u8);
            }
            let occ = grid.occupancy(random_external(&mut state, n), [true; 6]);

            let mut got_rects = Vec::new();
            let mut got_ao = Vec::new();
            extract_rects(&occ, &mut got_rects, &mut got_ao);

            let mut want_rects = Vec::new();
            let mut want_ao = Vec::new();
            extract_rects_reference(&occ, &mut want_rects, &mut want_ao);

            assert_eq!(
                got_rects, want_rects,
                "round {round}: raw rect order differs"
            );
            assert_eq!(got_ao, want_ao, "round {round}: raw ao order differs");
        }
    }
}
