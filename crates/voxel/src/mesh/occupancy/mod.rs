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
//!
//! # Layout
//!
//! * `plane`         - Plane / Dir / ExternalPlane tags, the plane table and
//!   the ExternalPlane -> (Plane, Dir) split.
//! * `neighbourhood` - the 26-neighbour sampler used by AO.
//! * `data`          - the immutable OccupancyData result and its builder.
//! * `local_tree`    - the interner-free NodeAccess implementation that lets
//!   the raw-voxel path reuse the same extractors.
//! * `sampling`      - single-voxel / material queries.
//! * `masks`         - the three-plane global occupancy masks.
//! * `external`      - the six external face masks used for seam culling.
//!
//! Every name that was reachable as `crate::mesh::occupancy::X` before the
//! split is re-exported here, so the public path is unchanged.

mod data;
mod external;
mod local_tree;
mod masks;
mod neighbourhood;
mod plane;
mod sampling;

pub use self::data::{AxisOccupancy, OccupancyData, OccupancyDataBuilder};
pub use self::external::{generate_external_occupancy_mask, generate_external_occupancy_mask_slow};
pub(crate) use self::external::{
    generate_external_occupancy_mask_fast, generate_external_occupancy_mask_generic,
};
pub(crate) use self::local_tree::{build_local_block_tree, InternerAccess};
pub use self::masks::generate_occupancy_masks;
pub(crate) use self::masks::generate_occupancy_masks_generic;
pub use self::neighbourhood::{FaceNeighbor, Neighbourhood};
pub use self::plane::{extract_plane_dir, Dir, ExternalPlane, Plane};
pub(crate) use self::plane::{PlaneData, PLANES};
pub use self::sampling::{tree_voxel_is_solid, voxel_material};

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
