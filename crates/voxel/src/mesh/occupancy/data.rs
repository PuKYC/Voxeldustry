//! The immutable occupancy result and its mutable builder.

use std::collections::BTreeMap;

use super::full_mask;
use super::plane::ExternalPlane;

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
    /// Optional padded occupancy halo for AO sampling.
    ///
    /// When present its length is `(n + 2)^3` (n = voxels_per_axis) and it is
    /// indexed x-major: `(x + 1) + (n + 2) * ((y + 1) + (n + 2) * (z + 1))`
    /// for block-relative `(x, y, z)` in `-1..=n`; a non-zero byte means
    /// solid.  The raw-voxel mesher fills this from its 34^3 byte buffer so AO
    /// reads face, edge and corner neighbours uniformly.  `None` keeps the
    /// legacy `global` + six face-mask sampling used by the tree and
    /// incremental paths.
    pub halo: Option<Box<[u8]>>,
    /// Per-material occupancy masks, sorted by material id (deterministic).
    pub per_material: Vec<Vec<u64>>,
    /// (material_id, voxel_count), sorted by material id.
    pub materials: Vec<(usize, usize)>,
}

impl OccupancyData {
    /// Padded halo dimension per axis (n + 2); only meaningful when
    /// [`OccupancyData::halo`] is present.
    #[must_use]
    pub fn halo_dim(&self) -> usize {
        self.voxels_per_axis as usize + 2
    }
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
    /// Optional padded halo, carried through to [`OccupancyData::halo`].
    pub halo: Option<Box<[u8]>>,
    /// Per-material masks.  A BTreeMap keeps iteration deterministic.
    pub per_material: BTreeMap<usize, Vec<u64>>,
    /// Material voxel counts.
    pub materials: BTreeMap<usize, usize>,
}

impl Default for OccupancyDataBuilder {
    /// 默认构建器：32 体素宽（v1 的 depth 5 块）。
    fn default() -> Self {
        Self::new(32)
    }
}

impl OccupancyDataBuilder {
    /// Creates an empty builder for a voxels_per_axis-wide grid.
    ///
    /// # 参数
    ///
    /// `voxels_per_axis` 是每轴体素数 `n`，同时也决定了缓冲区的固定长度：
    /// `global` 为 `3 * n * n`、`external` 为 `6 * n`，与占用提取器读取
    /// 的网格分辨率一致。
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
            halo: None,
            per_material: BTreeMap::new(),
            materials: BTreeMap::new(),
        }
    }

    /// Materialises the immutable OccupancyData, sorting materials by id.
    ///
    /// # 返回
    ///
    /// 一个可直接交给 greedy 提取器的快照。`materials` 按材料 id 升序，
    /// `per_material` 与之一一对应；调用了 fill_external_side 的侧面会带上
    /// 对应的 external_exists 标记。
    ///
    /// # Panics
    ///
    /// 某个材料登记了体素数却没有写入任何 per_material 平面时 panic，
    /// 这属于构建方漏调 fill_masks_for_region 的内部错误。
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
            halo: self.halo,
            per_material,
            materials,
        }
    }

    /// Marks an external side as fully occupied (all faces on that side culled).
    ///
    /// # 参数
    ///
    /// `external_plane` 是六个面之一；写入该面在 `external` 里的 n 个全 1
    /// 掩码，并置位 external_exists，使 build 后 OccupancyData 能区分
    /// 「显式填满」与「未提供」。
    pub fn fill_external_side(&mut self, external_plane: ExternalPlane) {
        let n = self.voxels_per_axis as usize;
        let start = external_plane as usize * n;
        self.external[start..start + n].fill(full_mask(n));
        self.external_exists[external_plane as usize] = true;
    }

    /// Clears an external side (no external occlusion).
    ///
    /// 与 fill_external_side 相反：清零该面的 n 个掩码并复位
    /// external_exists，使该侧重新按「没有外部遮挡」处理。
    pub fn clear_external_side(&mut self, external_plane: ExternalPlane) {
        let n = self.voxels_per_axis as usize;
        let start = external_plane as usize * n;
        self.external[start..start + n].fill(0);
        self.external_exists[external_plane as usize] = false;
    }
}
