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

use std::borrow::Cow;
use std::sync::RwLock;

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

// ── 数据驱动方块对象（JSONC）─────────────────────────────────────────────

/// JSONC 中的方块对象定义。
///
/// 这是**数据层**的方块条目；BLOCK_TABLE 是核心 0..=7 的编译期契约。
/// 核心 id 0..=7 必须与 BLOCK_TABLE 同名同 id；mod 条目按名字哈希分配
/// id（>= 10000），加载时再映射到 8..=255 的空闲 u8 槽位。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoxelObjectDef {
    pub id: u32,
    pub name: String,
    #[serde(default = "default_true")]
    pub solid: bool,
    /// 可被替换（草、雪层等）。
    #[serde(default)]
    pub replaceable: bool,
    /// 可作为支撑面（可放火把 / 实体）。
    #[serde(default = "default_true", alias = "supports", alias = "support_face")]
    pub support: bool,
    /// 是否参与碰撞。
    #[serde(default = "default_true")]
    pub collision: bool,
    /// 世界体素值使用的 u8 槽位；加载时由 assign_block_slots 分配。
    #[serde(skip)]
    pub slot: u8,
}

fn default_true() -> bool {
    true
}

impl VoxelObjectDef {
    /// 世界体素值使用的 u8 槽位（加载时已分配）。
    pub fn block_id(&self) -> Option<u8> {
        Some(self.slot)
    }
}

/// 给方块表分配 u8 调色板槽位：核心 id 0..=7 固定用同值槽位，其余条目
/// 按表顺序取 8..=255 的最小空闲槽。超过 256 个方块直接报错。
pub fn assign_block_slots(blocks: &mut [VoxelObjectDef]) -> Result<(), String> {
    let mut used = [false; 256];
    for block in blocks.iter_mut() {
        if block.id <= u32::from(u8::MAX) {
            let slot = block.id as u8;
            if used[usize::from(slot)] {
                return Err(format!("方块槽位重复：id {}", block.id));
            }
            used[usize::from(slot)] = true;
            block.slot = slot;
        }
    }
    let mut next_free = 8usize;
    for block in blocks.iter_mut() {
        if block.id <= u32::from(u8::MAX) {
            continue;
        }
        while next_free < used.len() && used[next_free] {
            next_free += 1;
        }
        if next_free >= used.len() {
            return Err("方块调色板只有 256 个槽位，mod 方块过多".to_string());
        }
        used[next_free] = true;
        block.slot = next_free as u8;
        next_free += 1;
    }
    Ok(())
}

impl super::JsoncTable for VoxelObjectDef {
    type Def = Self;
    const FILE: &'static str = "voxel_objects.jsonc";
    const ROOT_KEY: &'static str = "blocks";

    fn key(def: &Self::Def) -> &str {
        &def.name
    }

    fn validate(defs: &[Self::Def]) -> Result<(), String> {
        if defs.is_empty() {
            return Err("方块对象表为空".to_string());
        }
        let mut previous_id = None;
        let mut names = std::collections::HashSet::new();
        for def in defs {
            if let Some(previous) = previous_id {
                if def.id <= previous {
                    return Err(format!(
                        "方块对象必须按 id 严格升序：{}（id {}）出现在 id {} 之后",
                        def.name, def.id, previous
                    ));
                }
            }
            previous_id = Some(def.id);
            if def.name.is_empty() {
                return Err(format!("id {} 的方块名字为空", def.id));
            }
            if !names.insert(def.name.as_str()) {
                return Err(format!("方块名字重复：{}", def.name));
            }
            if def.name.contains(':') {
                let expected = game_engine::ids::mod_hash_id(&def.name);
                if def.id != expected {
                    return Err(format!(
                        "mod 方块 {} 的 id 必须是 mod_hash_id = {}，实际 {}",
                        def.name, expected, def.id
                    ));
                }
            } else if def.id > game_engine::ids::CORE_ID_MAX {
                return Err(format!(
                    "核心方块 {} 的 id 越界：{}（非命名空间 id 必须 <= {}）",
                    def.name,
                    def.id,
                    game_engine::ids::CORE_ID_MAX
                ));
            }
            if def.id == u32::from(AIR_BLOCK) && (def.solid || def.support || def.collision) {
                return Err("air 必须是 solid=false / support=false / collision=false".to_string());
            }
        }

        // 核心 0..=7 的 id / 名字是导出契约，必须与 BLOCK_TABLE 一致。
        for block in BLOCK_TABLE {
            let found = defs
                .iter()
                .find(|def| def.id == u32::from(block.id))
                .ok_or_else(|| format!("缺少核心方块 id {}（{}）", block.id, block.name))?;
            if found.name != block.name {
                return Err(format!(
                    "核心方块 id {} 名字必须是 {}，实际 {}",
                    block.id, block.name, found.name
                ));
            }
        }
        Ok(())
    }
}

// ── 生物群系表 ────────────────────────────────────────────────────────────

/// 生物群系 id（游戏语义 world::BiomeId 即对本类型的重导出）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct BiomeId(pub u8);

/// 默认生物群系（Island 缺省值）。
pub const DEFAULT_BIOME: BiomeId = BiomeId(0);

/// 生物群系定义。高度参数以**体素**为单位，供生成函数做定点噪声基线。
#[derive(Clone, Debug)]
pub struct BiomeDef {
    pub id: BiomeId,
    pub name: Cow<'static, str>,
    /// 地表基线高度（世界体素 Y）。
    pub base_height: i32,
    /// 噪声振幅（±体素数）。
    pub amplitude: i32,
    pub surface_block: u8,
    pub subsurface_block: u8,
    pub stone_block: u8,
}

impl BiomeDef {
    /// 生成器只需要的纯数据切片（不含名字）。
    pub fn shape(&self) -> BiomeShape {
        BiomeShape::from(self)
    }
}

/// 生成器使用的生物群系形状；Copy，避免每列查表。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BiomeShape {
    pub id: BiomeId,
    pub base_height: i32,
    pub amplitude: i32,
    pub surface_block: u8,
    pub subsurface_block: u8,
    pub stone_block: u8,
}

impl BiomeShape {
    /// 未知生物群系 id 的兜底形状，与 meadow 一致。
    pub const FALLBACK: Self = Self {
        id: BiomeId(0),
        base_height: 64,
        amplitude: 8,
        surface_block: GRASS_BLOCK,
        subsurface_block: DIRT_BLOCK,
        stone_block: STONE_BLOCK,
    };
}

impl From<&BiomeDef> for BiomeShape {
    fn from(def: &BiomeDef) -> Self {
        Self {
            id: def.id,
            base_height: def.base_height,
            amplitude: def.amplitude,
            surface_block: def.surface_block,
            subsurface_block: def.subsurface_block,
            stone_block: def.stone_block,
        }
    }
}

pub const BIOME_TABLE: &[BiomeDef] = &[
    BiomeDef {
        id: BiomeId(0),
        name: Cow::Borrowed("meadow"),
        base_height: 64,
        amplitude: 8,
        surface_block: GRASS_BLOCK,
        subsurface_block: DIRT_BLOCK,
        stone_block: STONE_BLOCK,
    },
    BiomeDef {
        id: BiomeId(1),
        name: Cow::Borrowed("forest"),
        base_height: 68,
        amplitude: 10,
        surface_block: GRASS_BLOCK,
        subsurface_block: DIRT_BLOCK,
        stone_block: STONE_BLOCK,
    },
    BiomeDef {
        id: BiomeId(2),
        name: Cow::Borrowed("desert"),
        base_height: 60,
        amplitude: 6,
        surface_block: SAND_BLOCK,
        subsurface_block: SAND_BLOCK,
        stone_block: STONE_BLOCK,
    },
    BiomeDef {
        id: BiomeId(3),
        name: Cow::Borrowed("tundra"),
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

/// 运行期已加载的生物群系形状；生成器在 const 表之外优先读这里。
static BIOME_SHAPES: RwLock<Vec<BiomeShape>> = RwLock::new(Vec::new());

/// 成功加载/重载后安装生物群系形状；失败时保留旧值。
pub fn install_biome_shapes(shapes: Vec<BiomeShape>) {
    *BIOME_SHAPES.write().expect("biome shapes lock") = shapes;
}

/// 按 id 取运行期形状；尚未加载时回退 const 表。
pub fn biome_shape(id: BiomeId) -> Option<BiomeShape> {
    if let Ok(shapes) = BIOME_SHAPES.read() {
        if let Some(shape) = shapes.iter().find(|shape| shape.id == id) {
            return Some(*shape);
        }
    }
    biome_def(id).map(BiomeShape::from)
}

/// JSONC 中的生物群系定义；方块字段写稳定名字，加载时解析成槽位。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BiomeDefFile {
    pub id: u8,
    pub name: String,
    pub base_height: i32,
    pub amplitude: i32,
    pub surface_block: String,
    pub subsurface_block: String,
    pub stone_block: String,
}

impl super::JsoncTable for BiomeDefFile {
    type Def = Self;
    const FILE: &'static str = "biomes.jsonc";
    const ROOT_KEY: &'static str = "biomes";

    fn key(def: &Self::Def) -> &str {
        &def.name
    }

    fn validate(defs: &[Self::Def]) -> Result<(), String> {
        if defs.is_empty() {
            return Err("生物群系表为空".to_string());
        }
        let mut previous_id = None;
        let mut names = std::collections::HashSet::new();
        for def in defs {
            if let Some(previous) = previous_id {
                if def.id <= previous {
                    return Err(format!(
                        "生物群系必须按 id 严格升序：{}（id {}）出现在 id {} 之后",
                        def.name, def.id, previous
                    ));
                }
            }
            previous_id = Some(def.id);
            if def.name.is_empty() {
                return Err(format!("id {} 的生物群系名字为空", def.id));
            }
            if !names.insert(def.name.as_str()) {
                return Err(format!("生物群系名字重复：{}", def.name));
            }
            if def.base_height.abs() > 1_000_000 {
                return Err(format!(
                    "{} 的 base_height 越界：{}",
                    def.name, def.base_height
                ));
            }
            if def.amplitude < 0 {
                return Err(format!("{} 的 amplitude 必须 >= 0", def.name));
            }
            for block in [&def.surface_block, &def.subsurface_block, &def.stone_block] {
                if block.is_empty() {
                    return Err(format!("{} 引用了空方块名字", def.name));
                }
            }
        }
        for biome in BIOME_TABLE {
            let found = defs
                .iter()
                .find(|def| u32::from(def.id) == u32::from(biome.id.0))
                .ok_or_else(|| format!("缺少核心生物群系 id {}（{}）", biome.id.0, biome.name))?;
            if found.name != biome.name {
                return Err(format!(
                    "核心生物群系 id {} 名字必须是 {}，实际 {}",
                    biome.id.0, biome.name, found.name
                ));
            }
        }
        Ok(())
    }
}

/// 把生物群系的方块名字解析成调色板槽位。
pub fn resolve_biomes(
    files: Vec<BiomeDefFile>,
    blocks: &[VoxelObjectDef],
) -> Result<Vec<BiomeDef>, String> {
    let by_name: std::collections::HashMap<&str, u8> = blocks
        .iter()
        .map(|block| (block.name.as_str(), block.block_id().unwrap_or(0)))
        .collect();
    let mut out = Vec::with_capacity(files.len());
    for file in files {
        let surface_block = *by_name.get(file.surface_block.as_str()).ok_or_else(|| {
            format!(
                "{} 的 surface_block({}) 不存在",
                file.name, file.surface_block
            )
        })?;
        let subsurface_block = *by_name.get(file.subsurface_block.as_str()).ok_or_else(|| {
            format!(
                "{} 的 subsurface_block({}) 不存在",
                file.name, file.subsurface_block
            )
        })?;
        let stone_block = *by_name
            .get(file.stone_block.as_str())
            .ok_or_else(|| format!("{} 的 stone_block({}) 不存在", file.name, file.stone_block))?;
        out.push(BiomeDef {
            id: BiomeId(file.id),
            name: Cow::Owned(file.name),
            base_height: file.base_height,
            amplitude: file.amplitude,
            surface_block,
            subsurface_block,
            stone_block,
        });
    }
    Ok(out)
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
