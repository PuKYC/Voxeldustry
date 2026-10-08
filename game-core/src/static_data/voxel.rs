//! 体素世界静态数据。
//!
//! **游戏数值不是引擎常量**：体素边长、方块调色板、材质表、生物群系表、
//! LOD 距离阈值全部落在本模块，由 crate::voxel / crate::world 消费。
//!
//! 依赖方向：本模块只依赖 game_engine；**不依赖 presentation / godot**。
//! Game 数值一律以 FixedPoint（I40F24）或整数表达，避免 f32 进入逻辑层。

use bevy::prelude::*;
use game_engine::math::FixedPoint;

// ── 方块 id（u8 调色板，0 = 空气 = VoxTree 的 T::default()）─────────────────

/// 空气：u8 默认值，也是 VoxTree<u8> 的“空”值。
pub const AIR_BLOCK: u8 = 0;
pub const STONE_BLOCK: u8 = 1;
pub const DIRT_BLOCK: u8 = 2;
pub const GRASS_BLOCK: u8 = 3;
pub const SAND_BLOCK: u8 = 4;
pub const SNOW_BLOCK: u8 = 5;
pub const WOOD_BLOCK: u8 = 6;
pub const LEAF_BLOCK: u8 = 7;

// ── 材质表 ────────────────────────────────────────────────────────────────

/// 材质 id（渲染侧查表；逻辑层只传递整数）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MaterialId(pub u8);

/// 材质定义。color 是线性 RGBA8，仅作静态数据；逻辑层不参与混合。
#[derive(Clone, Copy, Debug)]
pub struct MaterialDef {
    pub id: MaterialId,
    pub name: &'static str,
    pub color: [u8; 4],
    /// 0 = 镜面，255 = 完全粗糙（渲染侧启发式）。
    pub roughness: u8,
}

pub const MATERIAL_TABLE: &[MaterialDef] = &[
    MaterialDef {
        id: MaterialId(0),
        name: "stone",
        color: [122, 122, 126, 255],
        roughness: 220,
    },
    MaterialDef {
        id: MaterialId(1),
        name: "dirt",
        color: [110, 78, 50, 255],
        roughness: 255,
    },
    MaterialDef {
        id: MaterialId(2),
        name: "grass",
        color: [86, 148, 62, 255],
        roughness: 255,
    },
    MaterialDef {
        id: MaterialId(3),
        name: "sand",
        color: [214, 198, 140, 255],
        roughness: 255,
    },
    MaterialDef {
        id: MaterialId(4),
        name: "snow",
        color: [236, 240, 246, 255],
        roughness: 200,
    },
    MaterialDef {
        id: MaterialId(5),
        name: "wood",
        color: [122, 88, 52, 255],
        roughness: 230,
    },
    MaterialDef {
        id: MaterialId(6),
        name: "leaf",
        color: [64, 122, 54, 255],
        roughness: 255,
    },
    MaterialDef {
        id: MaterialId(7),
        name: "air",
        color: [0, 0, 0, 0],
        roughness: 255,
    },
];

/// 按 id 查材质（线性扫描；表 < 16 项，确定性且无 HashMap 顺序依赖）。
pub fn material_def(id: MaterialId) -> Option<&'static MaterialDef> {
    MATERIAL_TABLE.iter().find(|m| m.id == id)
}

// ── 方块调色板表：u8 方块 id -> 材质 / 视觉数据 ───────────────────────────

/// 方块定义（u8 调色板条目）。
#[derive(Clone, Copy, Debug)]
pub struct BlockDef {
    pub id: u8,
    pub name: &'static str,
    pub material: MaterialId,
    /// 是否阻挡 / 可站立；空气为 false。
    pub solid: bool,
}

pub const BLOCK_TABLE: &[BlockDef] = &[
    BlockDef {
        id: AIR_BLOCK,
        name: "air",
        material: MaterialId(7),
        solid: false,
    },
    BlockDef {
        id: STONE_BLOCK,
        name: "stone",
        material: MaterialId(0),
        solid: true,
    },
    BlockDef {
        id: DIRT_BLOCK,
        name: "dirt",
        material: MaterialId(1),
        solid: true,
    },
    BlockDef {
        id: GRASS_BLOCK,
        name: "grass",
        material: MaterialId(2),
        solid: true,
    },
    BlockDef {
        id: SAND_BLOCK,
        name: "sand",
        material: MaterialId(3),
        solid: true,
    },
    BlockDef {
        id: SNOW_BLOCK,
        name: "snow",
        material: MaterialId(4),
        solid: true,
    },
    BlockDef {
        id: WOOD_BLOCK,
        name: "wood",
        material: MaterialId(5),
        solid: true,
    },
    BlockDef {
        id: LEAF_BLOCK,
        name: "leaf",
        material: MaterialId(6),
        solid: true,
    },
];

/// 按 u8 方块 id 查调色板条目。
pub fn block_def(id: u8) -> Option<&'static BlockDef> {
    BLOCK_TABLE.iter().find(|b| b.id == id)
}

/// 方块是否实心（未知 id 视为空气）。
pub fn is_solid_block(id: u8) -> bool {
    block_def(id).is_some_and(|b| b.solid)
}

// ── 生物群系表 ────────────────────────────────────────────────────────────

/// 生物群系 id（游戏语义 world::BiomeId 即对本类型的重导出）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct BiomeId(pub u8);

/// 默认生物群系（Island 缺省值）。
pub const DEFAULT_BIOME: BiomeId = BiomeId(0);

/// 生物群系定义。高度参数以**体素**为单位，供生成函数做定点噪声基线。
#[derive(Clone, Copy, Debug)]
pub struct BiomeDef {
    pub id: BiomeId,
    pub name: &'static str,
    /// 地表基线高度（世界体素 Y）。
    pub base_height: i32,
    /// 噪声振幅（±体素数）。
    pub amplitude: i32,
    pub surface_block: u8,
    pub subsurface_block: u8,
    pub stone_block: u8,
}

pub const BIOME_TABLE: &[BiomeDef] = &[
    BiomeDef {
        id: BiomeId(0),
        name: "meadow",
        base_height: 64,
        amplitude: 8,
        surface_block: GRASS_BLOCK,
        subsurface_block: DIRT_BLOCK,
        stone_block: STONE_BLOCK,
    },
    BiomeDef {
        id: BiomeId(1),
        name: "forest",
        base_height: 68,
        amplitude: 10,
        surface_block: GRASS_BLOCK,
        subsurface_block: DIRT_BLOCK,
        stone_block: STONE_BLOCK,
    },
    BiomeDef {
        id: BiomeId(2),
        name: "desert",
        base_height: 60,
        amplitude: 6,
        surface_block: SAND_BLOCK,
        subsurface_block: SAND_BLOCK,
        stone_block: STONE_BLOCK,
    },
    BiomeDef {
        id: BiomeId(3),
        name: "tundra",
        base_height: 66,
        amplitude: 12,
        surface_block: SNOW_BLOCK,
        subsurface_block: DIRT_BLOCK,
        stone_block: STONE_BLOCK,
    },
];

/// 按 id 查生物群系定义。
pub fn biome_def(id: BiomeId) -> Option<&'static BiomeDef> {
    BIOME_TABLE.iter().find(|b| b.id == id)
}

// ── 体素边长（写进 VoxVolume.voxel_size）──────────────────────────────────

/// 0.45 m 的 I40F24 原始位：round(0.45 * 2^24) = 7_549_747。
///
/// FixedPoint::from_num 不是 const fn，因此以位常量暴露；
/// from_bits 是 const fn，可直接用于常量上下文。
pub const VOXEL_SIZE_BITS: i64 = 7_549_747;

/// 单个体素边长 = 0.45 m（定点）。
pub const DEFAULT_VOXEL_SIZE: FixedPoint = FixedPoint::from_bits(VOXEL_SIZE_BITS);

/// 供文档 / 调试使用的十进制镜像；**逻辑不读这个 f64**。
pub const VOXEL_SIZE_METERS: f64 = 0.45;

// ── LOD 距离阈值──────────────────────────────────────

/// LOD 切换阈值（米，定点）：
///
/// | 距离 d (m)  | LOD |
/// |-------------|-----|
/// | [0, 32)     | 0   |
/// | [32, 64)    | 1   |
/// | [64, 128)   | 2   |
/// | [128, +inf) | 3   |
///
/// 依据：mesh 块物理跨度 LOD L = 14.4 * 2^L m，取相邻档的 2 倍作为
/// 切换点，使同一档内 LOD0 粒度（0.45 m）的矩形密度与距离成反比。
pub const LOD_DISTANCE_THRESHOLDS: [FixedPoint; 3] = [
    FixedPoint::from_bits(32 << 24),
    FixedPoint::from_bits(64 << 24),
    FixedPoint::from_bits(128 << 24),
];

// ── 世界静态表资源句柄 ────────────────────────────────────────────────────

/// 世界静态表资源句柄：只读访问本模块中的游戏数值。
///
/// 把静态表做成 Resource 是为了让系统通过注入拿到一致入口，
/// 也方便将来在测试 / mod 中替换数值（v1 仍读 const 表）。
#[derive(Resource, Default, Clone, Copy, Debug)]
pub struct WorldTables;

impl WorldTables {
    pub fn block(&self, id: u8) -> Option<&'static BlockDef> {
        block_def(id)
    }

    pub fn material(&self, id: MaterialId) -> Option<&'static MaterialDef> {
        material_def(id)
    }

    pub fn biome(&self, id: BiomeId) -> Option<&'static BiomeDef> {
        biome_def(id)
    }

    pub fn block_is_solid(&self, id: u8) -> bool {
        is_solid_block(id)
    }

    /// 默认体素边长（写进 VoxVolume.voxel_size）。
    pub fn voxel_size(&self) -> FixedPoint {
        DEFAULT_VOXEL_SIZE
    }

    /// LOD 距离阈值（米，定点）。
    pub fn lod_thresholds(&self) -> &'static [FixedPoint; 3] {
        &LOD_DISTANCE_THRESHOLDS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_tables_delegate_to_static_tables() {
        let tables = WorldTables;
        assert!(tables.block(STONE_BLOCK).is_some());
        assert!(tables.material(MaterialId(0)).is_some());
        assert!(tables.biome(DEFAULT_BIOME).is_some());
        assert!(tables.block_is_solid(STONE_BLOCK));
        assert!(!tables.block_is_solid(AIR_BLOCK));
        assert_eq!(tables.voxel_size(), DEFAULT_VOXEL_SIZE);
        assert_eq!(tables.lod_thresholds().len(), 3);
    }

    #[test]
    fn block_ids_unique_and_materials_exist() {
        let mut ids: Vec<u8> = BLOCK_TABLE.iter().map(|b| b.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "方块 id 必须唯一");

        for block in BLOCK_TABLE {
            assert!(
                material_def(block.material).is_some(),
                "方块 {} 引用了不存在的材质 {:?}",
                block.name,
                block.material
            );
        }
    }

    #[test]
    fn biome_ids_unique_and_blocks_exist() {
        let mut ids: Vec<u8> = BIOME_TABLE.iter().map(|b| b.id.0).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "生物群系 id 必须唯一");

        for biome in BIOME_TABLE {
            for block in [
                biome.surface_block,
                biome.subsurface_block,
                biome.stone_block,
            ] {
                assert!(
                    block_def(block).is_some(),
                    "生物群系 {} 引用了不存在的方块 {}",
                    biome.name,
                    block
                );
            }
        }
    }

    #[test]
    fn default_voxel_size_is_045_meters() {
        let meters = DEFAULT_VOXEL_SIZE.to_num::<f64>();
        assert!(
            (meters - VOXEL_SIZE_METERS).abs() < 1e-6,
            "期望 0.45 m，得到 {meters}"
        );
        assert_eq!(DEFAULT_VOXEL_SIZE, FixedPoint::from_bits(VOXEL_SIZE_BITS));
    }

    #[test]
    fn lod_thresholds_strictly_increasing_and_positive() {
        assert!(LOD_DISTANCE_THRESHOLDS[0] > FixedPoint::from_bits(0));
        assert!(LOD_DISTANCE_THRESHOLDS[0] < LOD_DISTANCE_THRESHOLDS[1]);
        assert!(LOD_DISTANCE_THRESHOLDS[1] < LOD_DISTANCE_THRESHOLDS[2]);
    }
}
