//! 体素世界静态数据。
//!
//! **游戏数值不是引擎常量**：体素边长、方块 id/名字表、生物群系表、
//! LOD 距离阈值全部落在本模块，由 crate::voxel / crate::world 消费。
//!
//! **材质概念完全不在 Rust**：颜色 / PBR / 纹理以及 block -> 面材质绑定
//! 全部由 Godot 侧资源与 VoxelMaterialLibrary 接管；Rust 只保留 block id、
//! 方块名契约与几何。
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

// ── 面序（与 ExternalPlane / RectInstance 一致）────────────────────────────

/// 面索引个数：顺序 `[+x, -x, +y, -y, +z, -z]`（face = plane * 2 + dir）。
///
/// 与 `voxel::mesh::ExternalPlane`、`RectInstance` 的 `plane * 2 + dir` 以及
/// 着色器的 `orientation` 完全一致；Godot 侧 build_face_material_map() 的索引
/// 也按此顺序（Rust 不认识材质，只保留几何面序契约）。
pub const FACE_COUNT: usize = 6;

// ── 方块表：u8 方块 id -> 名字 / 是否实心 ─────────────────────────────────

/// 方块定义（u8 方块 id 条目）。
///
/// **不含任何外观 / 材质字段**：材质绑定由 Godot 侧负责，Rust 只导出
/// id -> name 契约供对账（[`block_name`]）。
#[derive(Clone, Copy, Debug)]
pub struct BlockDef {
    pub id: u8,
    pub name: &'static str,
    /// 是否阻挡 / 可站立；空气为 false。
    pub solid: bool,
}

pub const BLOCK_TABLE: &[BlockDef] = &[
    BlockDef {
        id: AIR_BLOCK,
        name: "air",
        solid: false,
    },
    BlockDef {
        id: STONE_BLOCK,
        name: "stone",
        solid: true,
    },
    BlockDef {
        id: DIRT_BLOCK,
        name: "dirt",
        solid: true,
    },
    BlockDef {
        // 草方块的六面外观差异由 Godot 侧 face material map 决定，Rust 不感知。
        id: GRASS_BLOCK,
        name: "grass",
        solid: true,
    },
    BlockDef {
        id: SAND_BLOCK,
        name: "sand",
        solid: true,
    },
    BlockDef {
        id: SNOW_BLOCK,
        name: "snow",
        solid: true,
    },
    BlockDef {
        id: WOOD_BLOCK,
        name: "wood",
        solid: true,
    },
    BlockDef {
        id: LEAF_BLOCK,
        name: "leaf",
        solid: true,
    },
];

/// 按 u8 方块 id 查表项。
#[must_use]
pub fn block_def(id: u8) -> Option<&'static BlockDef> {
    BLOCK_TABLE.iter().find(|b| b.id == id)
}

/// 按 u8 方块 id 查名字（导出对账 / 调试用）；未知 id 返回 `None`。
#[must_use]
pub fn block_name(id: u8) -> Option<&'static str> {
    block_def(id).map(|block| block.name)
}

/// 方块是否实心（未知 id 视为空气）。
#[must_use]
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
        assert!(tables.biome(DEFAULT_BIOME).is_some());
        assert!(tables.block_is_solid(STONE_BLOCK));
        assert!(!tables.block_is_solid(AIR_BLOCK));
        assert_eq!(tables.voxel_size(), DEFAULT_VOXEL_SIZE);
        assert_eq!(tables.lod_thresholds().len(), 3);
    }

    /// block id 唯一，且 0..7 的名字是导出契约；未知 id 返回 None。
    #[test]
    fn block_ids_unique_and_names_exported() {
        let mut ids: Vec<u8> = BLOCK_TABLE.iter().map(|b| b.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "方块 id 必须唯一");

        let expected = [
            (AIR_BLOCK, "air"),
            (STONE_BLOCK, "stone"),
            (DIRT_BLOCK, "dirt"),
            (GRASS_BLOCK, "grass"),
            (SAND_BLOCK, "sand"),
            (SNOW_BLOCK, "snow"),
            (WOOD_BLOCK, "wood"),
            (LEAF_BLOCK, "leaf"),
        ];
        for (id, name) in expected {
            assert_eq!(block_name(id), Some(name), "block {id} 的名字契约");
        }
        assert_eq!(block_name(8), None, "未知 block id 返回 None");
        assert_eq!(block_name(255), None);

        let mut names: Vec<&str> = BLOCK_TABLE.iter().map(|b| b.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(
            before,
            names.len(),
            "方块名必须唯一（Godot 资源按名字绑定）"
        );
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
