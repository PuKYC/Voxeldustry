//! Occupancy extraction for the greedy mesher.
//!
//! This module is the parameterised port of vendor/voxelis/src/utils/mesh.rs
//! (MIT OR Apache-2.0).  The two differences from the upstream original are:
//!
//! 1. the plane scratch is sized from the meshed block's voxels_per_axis
//!    (see crate::mesh::voxels_per_axis) instead of the hard-coded
//!    MAX_VOXELS_PER_AXIS = 64, and
//! 2. the tree traversal is generic over a tiny NodeAccess trait so that
//!    both the real VoxInterner and the temporary local wrapper tree used by
//!    crate::mesh::extract_block can feed the same mesher.
//!
//! Nothing in here produces bytes: the occupancy masks are pure integer
//! bitmasks over a voxels_per_axis^3 grid.

use std::collections::BTreeMap;

use glam::{IVec3, UVec2, UVec3};

use crate::store::{BlockId, ChunkKey, MaxDepth, VoxInterner, VoxTree, VoxelTrait};

/// The three axis-aligned plane families.
///
/// * YZ  - normal along X, row = y, col = z
/// * XZ  - normal along Y, row = z, col = x
/// * XY  - normal along Z, row = y, col = x
///
/// The row/col roles match the upstream fill_masks_for_region index
/// arithmetic exactly, so they are part of the integer contract of
/// crate::mesh::RectInstance.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Plane {
    /// Plane normal to X.
    YZ,
    /// Plane normal to Y.
    XZ,
    /// Plane normal to Z.
    XY,
}

impl Plane {
    /// Stable integer tag used by crate::mesh::RectInstance::plane.
    #[must_use]
    pub const fn index(self) -> u8 {
        match self {
            Plane::YZ => crate::mesh::PLANE_YZ,
            Plane::XZ => crate::mesh::PLANE_XZ,
            Plane::XY => crate::mesh::PLANE_XY,
        }
    }

    /// Base index (in units of one plane) into the global scratch vector.
    #[must_use]
    pub const fn plane_index(self) -> usize {
        match self {
            Plane::YZ => 0,
            Plane::XZ => 1,
            Plane::XY => 2,
        }
    }
}

/// Face orientation along the plane normal.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Dir {
    /// Face on the positive side of the normal (+X / +Y / +Z).
    Pos,
    /// Face on the negative side of the normal (-X / -Y / -Z).
    Neg,
}

impl Dir {
    /// Stable integer tag used by crate::mesh::RectInstance::dir.
    #[must_use]
    pub const fn index(self) -> u8 {
        match self {
            Dir::Pos => crate::mesh::DIR_POS,
            Dir::Neg => crate::mesh::DIR_NEG,
        }
    }
}

/// The six block faces that can receive external (cross-body) occupancy.
///
/// The discriminant order is the public index order of the
/// external: [Option<&VoxTree<u8>>; 6] parameter of crate::mesh::extract_block:
/// [YZ+, YZ-, XZ+, XZ-, XY+, XY-].
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum ExternalPlane {
    /// Positive X face.
    YZPos,
    /// Negative X face.
    YZNeg,
    /// Positive Y face.
    XZPos,
    /// Negative Y face.
    XZNeg,
    /// Positive Z face.
    XYPos,
    /// Negative Z face.
    XYNeg,
}

impl ExternalPlane {
    /// All six planes in discriminant order.
    pub const ALL: [ExternalPlane; 6] = [
        ExternalPlane::YZPos,
        ExternalPlane::YZNeg,
        ExternalPlane::XZPos,
        ExternalPlane::XZNeg,
        ExternalPlane::XYPos,
        ExternalPlane::XYNeg,
    ];

    /// Converts a raw index in 0..6 into a plane.
    #[must_use]
    pub const fn from_index(index: usize) -> ExternalPlane {
        match index {
            0 => ExternalPlane::YZPos,
            1 => ExternalPlane::YZNeg,
            2 => ExternalPlane::XZPos,
            3 => ExternalPlane::XZNeg,
            4 => ExternalPlane::XYPos,
            _ => ExternalPlane::XYNeg,
        }
    }
}

pub(crate) struct PlaneData {
    pub(crate) plane: Plane,
    pub(crate) offset_factor: usize,
    pub(crate) global_active_idx: usize,
    pub(crate) pos: ExternalPlane,
    pub(crate) neg: ExternalPlane,
}

pub(crate) const PLANES: [PlaneData; 3] = [
    PlaneData {
        plane: Plane::YZ,
        offset_factor: 0,
        global_active_idx: 0,
        pos: ExternalPlane::YZPos,
        neg: ExternalPlane::YZNeg,
    },
    PlaneData {
        plane: Plane::XZ,
        offset_factor: 1,
        global_active_idx: 2,
        pos: ExternalPlane::XZPos,
        neg: ExternalPlane::XZNeg,
    },
    PlaneData {
        plane: Plane::XY,
        offset_factor: 2,
        global_active_idx: 4,
        pos: ExternalPlane::XYPos,
        neg: ExternalPlane::XYNeg,
    },
];

/// Returns the low-n-bit mask (u64::MAX when n >= 64).
#[must_use]
#[inline(always)]
pub(crate) fn full_mask(n: usize) -> u64 {
    if n >= 64 {
        u64::MAX
    } else {
        (1u64 << n) - 1
    }
}

/// Per-axis active extents of an occupancy mask.
#[derive(Debug, Clone, Copy)]
pub struct AxisOccupancy {
    /// The bitmask itself.
    pub active: u64,
    /// First set bit (0 when empty).
    pub min: usize,
    /// One past the last set bit (0 when empty).
    pub max: usize,
}

impl AxisOccupancy {
    /// Builds the extents of active.
    #[must_use]
    pub fn new(active: u64) -> Self {
        if active == 0 {
            Self {
                active,
                min: 0,
                max: 0,
            }
        } else {
            let min = active.trailing_zeros() as usize;
            let max = (64 - active.leading_zeros()) as usize;
            Self { active, min, max }
        }
    }
}

/// Immutable occupancy result consumed by the greedy rectangle extractor.
#[derive(Debug, Clone)]
pub struct OccupancyData {
    /// Grid resolution per axis (depth 5 blocks -> 32).
    pub voxels_per_axis: u32,
    /// The three n * n global occupancy masks (one u64 mask per plane cell,
    /// bits index the normal axis).
    pub global: Vec<u64>,
    /// Active row/col extents per plane, in global_active order
    /// [YZ rows, YZ cols, XZ rows, XZ cols, XY rows, XY cols].
    pub global_active: [AxisOccupancy; 6],
    /// External occupancy masks, 6 * n entries.
    pub external: Vec<u64>,
    /// Whether an external side was explicitly provided.
    pub external_exists: [bool; 6],
    /// Per-material occupancy masks, sorted by material id (deterministic).
    pub per_material: Vec<Vec<u64>>,
    /// (material_id, voxel_count), sorted by material id.
    pub materials: Vec<(usize, usize)>,
}

/// Mutable builder for OccupancyData.
///
/// Create with OccupancyDataBuilder::new using the meshed block's
/// voxels_per_axis.  The upstream 64-wide scratch is replaced by a
/// voxels_per_axis-wide one, which is 56 KiB instead of 224 KiB for the v1
/// depth 5 (32^3) blocks.
pub struct OccupancyDataBuilder {
    /// Grid resolution per axis.
    pub voxels_per_axis: u32,
    /// Global occupancy masks for the three planes.
    pub global: Vec<u64>,
    /// Global active rows/cols bitmasks (see OccupancyData::global_active).
    pub global_active: [u64; 6],
    /// External occupancy masks, 6 * voxels_per_axis entries.
    pub external: Vec<u64>,
    /// Flags for explicitly provided external sides.
    pub external_exists: [bool; 6],
    /// Per-material masks.  A BTreeMap keeps iteration deterministic.
    pub per_material: BTreeMap<usize, Vec<u64>>,
    /// Material voxel counts.
    pub materials: BTreeMap<usize, usize>,
}

impl Default for OccupancyDataBuilder {
    fn default() -> Self {
        Self::new(32)
    }
}

impl OccupancyDataBuilder {
    /// Creates an empty builder for a voxels_per_axis-wide grid.
    ///
    /// # Panics
    ///
    /// Panics if voxels_per_axis is 0 or greater than 64.
    #[must_use]
    pub fn new(voxels_per_axis: u32) -> Self {
        assert!(
            voxels_per_axis > 0 && voxels_per_axis <= 64,
            "voxels_per_axis must be in 1..=64"
        );
        let n = voxels_per_axis as usize;
        Self {
            voxels_per_axis,
            global: vec![0u64; 3 * n * n],
            global_active: [0u64; 6],
            external: vec![0u64; 6 * n],
            external_exists: [false; 6],
            per_material: BTreeMap::new(),
            materials: BTreeMap::new(),
        }
    }

    /// Materialises the immutable OccupancyData, sorting materials by id.
    #[must_use]
    pub fn build(self) -> OccupancyData {
        let materials: Vec<(usize, usize)> = self.materials.into_iter().collect();
        let mut per_material_map = self.per_material;
        let per_material = materials
            .iter()
            .map(|(material_id, _)| {
                per_material_map
                    .remove(material_id)
                    .expect("material registered without occupancy plane")
            })
            .collect();

        OccupancyData {
            voxels_per_axis: self.voxels_per_axis,
            global: self.global,
            global_active: self.global_active.map(AxisOccupancy::new),
            external: self.external,
            external_exists: self.external_exists,
            per_material,
            materials,
        }
    }

    /// Marks an external side as fully occupied (all faces on that side culled).
    pub fn fill_external_side(&mut self, external_plane: ExternalPlane) {
        let n = self.voxels_per_axis as usize;
        let start = external_plane as usize * n;
        self.external[start..start + n].fill(full_mask(n));
        self.external_exists[external_plane as usize] = true;
    }

    /// Clears an external side (no external occlusion).
    pub fn clear_external_side(&mut self, external_plane: ExternalPlane) {
        let n = self.voxels_per_axis as usize;
        let start = external_plane as usize * n;
        self.external[start..start + n].fill(0);
        self.external_exists[external_plane as usize] = false;
    }
}

/// Splits an ExternalPlane into its plane family and direction.
#[must_use]
#[inline(always)]
pub const fn extract_plane_dir(external_plane: ExternalPlane) -> (Plane, Dir) {
    match external_plane {
        ExternalPlane::YZPos => (Plane::YZ, Dir::Pos),
        ExternalPlane::YZNeg => (Plane::YZ, Dir::Neg),
        ExternalPlane::XZPos => (Plane::XZ, Dir::Pos),
        ExternalPlane::XZNeg => (Plane::XZ, Dir::Neg),
        ExternalPlane::XYPos => (Plane::XY, Dir::Pos),
        ExternalPlane::XYNeg => (Plane::XY, Dir::Neg),
    }
}

/// Read-only octree access used by the occupancy extractors.
///
/// Implemented for VoxInterner (via InternerAccess) and for the temporary
/// LocalTree wrapper built by crate::mesh::extract_block.
pub(crate) trait NodeAccess<T: VoxelTrait> {
    /// Node handle.
    type Id: Copy + Eq;

    /// Returns true when the handle is the empty sentinel.
    fn is_empty(&self, id: Self::Id) -> bool;

    /// Returns true when the handle is a leaf node.
    fn is_leaf(&self, id: Self::Id) -> bool;

    /// Returns the (averaged) value stored at a node.
    fn value(&self, id: Self::Id) -> T;

    /// Returns the child at index (only valid for branches).
    fn child(&self, id: Self::Id, index: usize) -> Self::Id;
}

/// NodeAccess adapter over a VoxInterner.
pub(crate) struct InternerAccess<'a, T: VoxelTrait>(pub &'a VoxInterner<T>);

impl<T: VoxelTrait> NodeAccess<T> for InternerAccess<'_, T> {
    type Id = BlockId;

    #[inline(always)]
    fn is_empty(&self, id: BlockId) -> bool {
        id.is_empty()
    }

    #[inline(always)]
    fn is_leaf(&self, id: BlockId) -> bool {
        id.is_leaf()
    }

    #[inline(always)]
    fn value(&self, id: BlockId) -> T {
        *self.0.get_value(&id)
    }

    #[inline(always)]
    fn child(&self, id: BlockId, index: usize) -> BlockId {
        self.0.get_child_id(&id, index)
    }
}

/// Empty sentinel of LocalTree.
pub(crate) const LOCAL_EMPTY: u32 = u32::MAX;

#[derive(Clone, Copy)]
pub(crate) enum LocalNode {
    Leaf(u8),
    /// A branch carries the same LOD averaged value the interner stores, so a
    /// traversal cut at max_depth reads the identical value from both trees.
    Branch {
        value: u8,
        children: [u32; 8],
    },
}

/// A temporary, interned-free copy of a wrapped mesh block.
///
/// Used by crate::mesh::extract_block, whose signature takes an immutable
/// VoxInterner but which still has to mesh a 2^lod-wide group of base
/// subchunks.  The DAG is expanded into a tree; every node below a base
/// subchunk root is copied verbatim from the interner.
pub(crate) struct LocalTree {
    nodes: Vec<LocalNode>,
    root: u32,
}

impl LocalTree {
    /// The empty local tree.
    #[must_use]
    pub(crate) fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            root: LOCAL_EMPTY,
        }
    }

    /// Root handle.
    #[must_use]
    pub(crate) fn root(&self) -> u32 {
        self.root
    }

    /// Number of materialised nodes (test/diagnostics helper).
    #[must_use]
    pub(crate) fn node_count(&self) -> usize {
        self.nodes.len()
    }
}

impl NodeAccess<u8> for LocalTree {
    type Id = u32;

    #[inline(always)]
    fn is_empty(&self, id: u32) -> bool {
        id == LOCAL_EMPTY
    }

    #[inline(always)]
    fn is_leaf(&self, id: u32) -> bool {
        matches!(self.nodes[id as usize], LocalNode::Leaf(_))
    }

    #[inline(always)]
    fn value(&self, id: u32) -> u8 {
        match self.nodes[id as usize] {
            LocalNode::Leaf(value) => value,
            LocalNode::Branch { value, .. } => value,
        }
    }

    #[inline(always)]
    fn child(&self, id: u32, index: usize) -> u32 {
        match self.nodes[id as usize] {
            LocalNode::Branch { children, .. } => children[index],
            LocalNode::Leaf(_) => LOCAL_EMPTY,
        }
    }
}

/// Copies an interner subtree into out, returning the local root handle.
///
/// Shared subtrees are expanded (the v1 blocks are small enough that this is
/// cheaper than an interner round-trip).
fn copy_subtree(interner: &VoxInterner<u8>, id: BlockId, out: &mut Vec<LocalNode>) -> u32 {
    if id.is_empty() {
        return LOCAL_EMPTY;
    }
    if id.is_leaf() {
        out.push(LocalNode::Leaf(*interner.get_value(&id)));
        return (out.len() - 1) as u32;
    }
    let value = *interner.get_value(&id);
    let mut children = [LOCAL_EMPTY; 8];
    for (index, slot) in children.iter_mut().enumerate() {
        let child = interner.get_child_id(&id, index);
        *slot = copy_subtree(interner, child, out);
    }
    out.push(LocalNode::Branch { value, children });
    (out.len() - 1) as u32
}

/// Builds the temporary wrapped tree for a 2^lod x 2^lod x 2^lod group of base
/// subchunks.
///
/// The wrapper root sits lod levels above the base subchunk roots, exactly
/// like crate::mesh::wrap_block but without touching the interner.
#[must_use]
pub(crate) fn build_local_block_tree(
    chunks: &BTreeMap<ChunkKey, VoxTree<u8>>,
    interner: &VoxInterner<u8>,
    origin: ChunkKey,
    lod: u8,
) -> LocalTree {
    let mut nodes = Vec::new();
    let root = build_local_node(
        chunks,
        interner,
        origin,
        UVec3::ZERO,
        lod as u32,
        &mut nodes,
    );
    LocalTree { nodes, root }
}

fn build_local_node(
    chunks: &BTreeMap<ChunkKey, VoxTree<u8>>,
    interner: &VoxInterner<u8>,
    origin: ChunkKey,
    local: UVec3,
    level: u32,
    nodes: &mut Vec<LocalNode>,
) -> u32 {
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
                    LOCAL_EMPTY
                } else {
                    copy_subtree(interner, root, nodes)
                }
            }
            None => LOCAL_EMPTY,
        }
    } else {
        let half = 1u32 << (level - 1);
        let mut children = [LOCAL_EMPTY; 8];
        for (index, slot) in children.iter_mut().enumerate() {
            let dx = (index & 1) as u32;
            let dy = ((index >> 1) & 1) as u32;
            let dz = ((index >> 2) & 1) as u32;
            *slot = build_local_node(
                chunks,
                interner,
                origin,
                UVec3::new(
                    local.x + dx * half,
                    local.y + dy * half,
                    local.z + dz * half,
                ),
                level - 1,
                nodes,
            );
        }
        if children.iter().all(|&child| child == LOCAL_EMPTY) {
            return LOCAL_EMPTY;
        }
        let mut values = [0u8; 8];
        for (index, &child) in children.iter().enumerate() {
            if child != LOCAL_EMPTY {
                values[index] = match nodes[child as usize] {
                    LocalNode::Leaf(value) => value,
                    LocalNode::Branch { value, .. } => value,
                };
            }
        }
        let value = <u8 as VoxelTrait>::average(&values);
        nodes.push(LocalNode::Branch { value, children });
        (nodes.len() - 1) as u32
    }
}

/// Returns true when the voxel at position (in the tree's own depth frame) is
/// non-default.
fn occupied_at<T: VoxelTrait, A: NodeAccess<T>>(
    access: &A,
    root: A::Id,
    position: IVec3,
    max_depth: u32,
) -> bool {
    let mut node = root;
    let mut depth = 0u32;
    while !access.is_empty(node) {
        if !access.is_leaf(node) {
            if depth >= max_depth {
                return access.value(node) != T::default();
            }
            let shift = (max_depth - depth - 1) as usize;
            let index = ((position.x as usize >> shift) & 1)
                | (((position.y as usize >> shift) & 1) << 1)
                | (((position.z as usize >> shift) & 1) << 2);
            node = access.child(node, index);
            depth += 1;
        } else {
            return access.value(node) != T::default();
        }
    }
    false
}

fn fill_masks_for_region<T: VoxelTrait, A: NodeAccess<T>>(
    _access: &A,
    builder: &mut OccupancyDataBuilder,
    region_offset: UVec3,
    side: u32,
    material_id: usize,
) {
    let n = builder.voxels_per_axis as usize;
    let side = side as usize;
    let volume = side * side * side;

    builder
        .materials
        .entry(material_id)
        .and_modify(|count| *count += volume)
        .or_insert(volume);

    if side != n {
        let plane_len = 3 * n * n;
        let occupancy = builder
            .per_material
            .entry(material_id)
            .or_insert_with(|| vec![0u64; plane_len]);

        let run_mask = (1u64 << side) - 1;

        let start_x = region_offset.x as usize;
        let x_mask = run_mask << start_x;
        let start_y = region_offset.y as usize;
        let y_mask = run_mask << start_y;
        let start_z = region_offset.z as usize;
        let z_mask = run_mask << start_z;

        // YZ masks
        builder.global_active[0] |= y_mask; // rows
        builder.global_active[1] |= z_mask; // cols
                                            // XZ masks
        builder.global_active[2] |= z_mask; // rows
        builder.global_active[3] |= x_mask; // cols
                                            // XY masks
        builder.global_active[4] |= y_mask; // rows
        builder.global_active[5] |= x_mask; // cols

        for i in 0..side {
            let z = start_z + i;
            let y = start_y + i;

            let index_base_y = n * n + z * n + start_x;
            let index_base_z = 2 * n * n + y * n + start_x;
            let index_base_x = y * n + start_z;

            for j in 0..side {
                let index_y = index_base_y + j;
                builder.global[index_y] |= y_mask;
                occupancy[index_y] |= y_mask;

                let index_z = index_base_z + j;
                builder.global[index_z] |= z_mask;
                occupancy[index_z] |= z_mask;

                let index_x = index_base_x + j;
                builder.global[index_x] |= x_mask;
                occupancy[index_x] |= x_mask;
            }
        }
    } else {
        let plane_len = 3 * n * n;
        let full = full_mask(n);
        builder
            .per_material
            .insert(material_id, vec![full; plane_len]);
        builder.global.fill(full);
        builder.global_active = [full; 6];
    }
}

/// Generic occupancy extraction from any NodeAccess tree.
pub(crate) fn generate_occupancy_masks_generic<T: VoxelTrait, A: NodeAccess<T>>(
    access: &A,
    root: A::Id,
    builder: &mut OccupancyDataBuilder,
    max_depth: MaxDepth,
    offset: UVec3,
) {
    if access.is_empty(root) {
        return;
    }

    let max_depth = max_depth.max();
    assert_eq!(
        builder.voxels_per_axis,
        1u32 << max_depth,
        "builder resolution does not match max_depth"
    );

    let default_t = T::default();

    if access.is_leaf(root) {
        let value = access.value(root);
        if value != default_t {
            fill_masks_for_region(
                access,
                builder,
                offset,
                1u32 << max_depth,
                value.material_id(),
            );
        }
        return;
    }

    let mut stack: Vec<(A::Id, UVec3, u32)> = Vec::with_capacity(64);
    stack.push((root, UVec3::ZERO, 0));

    while let Some((node, pos, depth)) = stack.pop() {
        if !access.is_leaf(node) && depth < max_depth as u32 {
            let child_cube_half_side = 1u32 << (max_depth as u32 - depth - 1);
            for index in 0..8usize {
                let child = access.child(node, index);
                if !access.is_empty(child) {
                    let x = (index & 1) as u32 * child_cube_half_side;
                    let y = ((index >> 1) & 1) as u32 * child_cube_half_side;
                    let z = ((index >> 2) & 1) as u32 * child_cube_half_side;
                    stack.push((child, pos + UVec3::new(x, y, z), depth + 1));
                }
            }
        } else {
            let value = access.value(node);
            if value != default_t {
                let cube_side = 1u32 << (max_depth as u32 - depth);
                fill_masks_for_region(
                    access,
                    builder,
                    offset + pos,
                    cube_side,
                    value.material_id(),
                );
            }
        }
    }
}

/// Public interner-backed occupancy extraction (upstream entry point).
pub fn generate_occupancy_masks<T: VoxelTrait>(
    interner: &VoxInterner<T>,
    builder: &mut OccupancyDataBuilder,
    root_id: &BlockId,
    max_depth: MaxDepth,
    offset: UVec3,
) {
    generate_occupancy_masks_generic(
        &InternerAccess(interner),
        *root_id,
        builder,
        max_depth,
        offset,
    );
}

/// Generic external-face occupancy extraction from any NodeAccess tree.
pub(crate) fn generate_external_occupancy_mask_generic<T: VoxelTrait, A: NodeAccess<T>>(
    access: &A,
    builder: &mut OccupancyDataBuilder,
    root_id: A::Id,
    max_depth: MaxDepth,
    external_plane: ExternalPlane,
    offset: UVec2,
) {
    let max_depth = max_depth.max();
    let voxels_per_axis = 1usize << max_depth;
    assert_eq!(
        builder.voxels_per_axis as usize, voxels_per_axis,
        "builder resolution does not match max_depth"
    );

    let start_col = offset.x as usize;
    let start_row = offset.y as usize;

    let (plane, dir) = extract_plane_dir(external_plane);

    let offset_start = external_plane as usize * voxels_per_axis;
    builder.external_exists[external_plane as usize] = true;
    let external_plane_masks = &mut builder.external[offset_start..offset_start + voxels_per_axis];

    if access.is_empty(root_id) {
        return;
    }

    let pos_vox = match dir {
        Dir::Pos => 0,
        Dir::Neg => (voxels_per_axis - 1) as i32,
    };

    if !access.is_leaf(root_id) {
        match plane {
            Plane::YZ => {
                for y in 0..voxels_per_axis {
                    let row = start_row + y;
                    for z in 0..voxels_per_axis {
                        if occupied_at(
                            access,
                            root_id,
                            IVec3::new(pos_vox, y as i32, z as i32),
                            max_depth as u32,
                        ) {
                            external_plane_masks[row] |= 1u64 << (start_col + z);
                        }
                    }
                }
            }
            Plane::XZ => {
                for z in 0..voxels_per_axis {
                    let row = start_row + z;
                    for x in 0..voxels_per_axis {
                        if occupied_at(
                            access,
                            root_id,
                            IVec3::new(x as i32, pos_vox, z as i32),
                            max_depth as u32,
                        ) {
                            external_plane_masks[row] |= 1u64 << (start_col + x);
                        }
                    }
                }
            }
            Plane::XY => {
                for y in 0..voxels_per_axis {
                    let row = start_row + y;
                    for x in 0..voxels_per_axis {
                        if occupied_at(
                            access,
                            root_id,
                            IVec3::new(x as i32, y as i32, pos_vox),
                            max_depth as u32,
                        ) {
                            external_plane_masks[row] |= 1u64 << (start_col + x);
                        }
                    }
                }
            }
        }
    } else {
        let bit_mask = if voxels_per_axis == 64 {
            u64::MAX
        } else {
            ((1u64 << voxels_per_axis) - 1) << start_col
        };
        for r in 0..voxels_per_axis {
            external_plane_masks[start_row + r] |= bit_mask;
        }
    }
}

/// Fast, face-descending external-face occupancy extraction.
///
/// Bit-for-bit equivalent to generate_external_occupancy_mask_generic (the
/// reference implementation kept for comparison tests), but instead of probing
/// every one of the n^2 face voxels with occupied_at it walks the octree once
/// and only descends the four children that lie on the sampled face.  A leaf,
/// or a branch reached at max_depth, marks its whole projected square at once.
pub(crate) fn generate_external_occupancy_mask_fast<T: VoxelTrait, A: NodeAccess<T>>(
    access: &A,
    builder: &mut OccupancyDataBuilder,
    root_id: A::Id,
    max_depth: MaxDepth,
    external_plane: ExternalPlane,
    offset: UVec2,
) {
    let max_depth = max_depth.max();
    let voxels_per_axis = 1usize << max_depth;
    assert_eq!(
        builder.voxels_per_axis as usize, voxels_per_axis,
        "builder resolution does not match max_depth"
    );

    let start_col = offset.x as usize;
    let start_row = offset.y as usize;

    let (plane, dir) = extract_plane_dir(external_plane);

    let offset_start = external_plane as usize * voxels_per_axis;
    builder.external_exists[external_plane as usize] = true;
    // Same length-n slice as the generic implementation, so a row offset is
    // honoured (and rejected) identically.
    let external_plane_masks = &mut builder.external[offset_start..offset_start + voxels_per_axis];

    if access.is_empty(root_id) {
        return;
    }

    if access.is_leaf(root_id) {
        // The reference implementation fills the whole sampled plane for a leaf
        // root unconditionally, even when that leaf holds the default value.
        let bit_mask = if voxels_per_axis == 64 {
            u64::MAX
        } else {
            ((1u64 << voxels_per_axis) - 1) << start_col
        };
        for r in 0..voxels_per_axis {
            external_plane_masks[start_row + r] |= bit_mask;
        }
        return;
    }

    // Index bit of the plane normal axis: x = bit 0, y = bit 1, z = bit 2.
    let normal_axis = match plane {
        Plane::YZ => 0usize,
        Plane::XZ => 1usize,
        Plane::XY => 2usize,
    };
    // Dir::Pos samples the neighbour's local coordinate 0, Dir::Neg the last
    // one (see the generic implementation's pos_vox); at octree depth d both
    // correspond to index bit d of the normal axis.
    let dir_bit = match dir {
        Dir::Pos => 0usize,
        Dir::Neg => 1usize,
    };

    let default_t = T::default();
    let mut stack: Vec<(A::Id, UVec3, u32)> = Vec::with_capacity(64);
    stack.push((root_id, UVec3::ZERO, 0u32));

    while let Some((node, cube_min, depth)) = stack.pop() {
        if !access.is_leaf(node) && depth < max_depth as u32 {
            let half = 1u32 << (max_depth as u32 - depth - 1);
            for index in 0..8usize {
                if ((index >> normal_axis) & 1) != dir_bit {
                    continue;
                }
                let child = access.child(node, index);
                if access.is_empty(child) {
                    continue;
                }
                let x = (index & 1) as u32 * half;
                let y = ((index >> 1) & 1) as u32 * half;
                let z = ((index >> 2) & 1) as u32 * half;
                stack.push((child, cube_min + UVec3::new(x, y, z), depth + 1));
            }
        } else {
            // Leaf at any depth, or a branch reached at max_depth: occupied_at
            // resolves both to value != default for every voxel in the cube.
            if access.value(node) == default_t {
                continue;
            }
            let side = (1u32 << (max_depth as u32 - depth)) as usize;
            let (row0, col0) = match plane {
                Plane::YZ => (cube_min.y as usize, cube_min.z as usize),
                Plane::XZ => (cube_min.z as usize, cube_min.x as usize),
                Plane::XY => (cube_min.y as usize, cube_min.x as usize),
            };
            let run_mask = if side >= 64 {
                u64::MAX
            } else {
                ((1u64 << side) - 1) << (start_col + col0)
            };
            for r in 0..side {
                external_plane_masks[start_row + row0 + r] |= run_mask;
            }
        }
    }
}

/// Public interner-backed external occupancy extraction (upstream entry point).
///
/// The caller feeds a neighbour body's tree for external_plane; faces whose
/// directly-across voxel is occupied are culled from the mesh, so a seam
/// between two bodies does not emit duplicate geometry.
pub fn generate_external_occupancy_mask<T: VoxelTrait>(
    interner: &VoxInterner<T>,
    builder: &mut OccupancyDataBuilder,
    root_id: &BlockId,
    max_depth: MaxDepth,
    external_plane: ExternalPlane,
    offset: UVec2,
) {
    generate_external_occupancy_mask_fast(
        &InternerAccess(interner),
        builder,
        *root_id,
        max_depth,
        external_plane,
        offset,
    );
}

/// Reference (slow) external occupancy extraction, exposed only for
/// benchmarks that compare it against the public fast default.
///
/// This forwards to `generate_external_occupancy_mask_generic`, which is the
/// n^2 `occupied_at` reference kept for equivalence tests.  Production callers
/// must keep using [`generate_external_occupancy_mask`] (fast): this entry
/// point exists so an external bench target can measure the old cost without
/// the crate-internal reference becoming part of the supported API surface.
#[doc(hidden)]
pub fn generate_external_occupancy_mask_slow<T: VoxelTrait>(
    interner: &VoxInterner<T>,
    builder: &mut OccupancyDataBuilder,
    root_id: &BlockId,
    max_depth: MaxDepth,
    external_plane: ExternalPlane,
    offset: UVec2,
) {
    generate_external_occupancy_mask_generic(
        &InternerAccess(interner),
        builder,
        *root_id,
        max_depth,
        external_plane,
        offset,
    );
}
