//! 体素地形生成。
//!
//! 三条硬约束：
//! 1. **确定性（L3）**：种子 + ChunkKey -> 逐位一致的树；噪声 / 高度全部走
//!    FixedPoint 定点整数运算，不用 f32。
//! 2. **惰性生成**：以 ChunkKey 为单位按需生成；生成结果与生成顺序无关
//!    （同一 interner 下，先建哪个子块不影响某个子块的内容）。
//! 3. **实心塌陷**：整块位于实心内部时用 VoxTree::fill（just_fill 路径）塌成
//!    单个叶节点；只有地表附近的子块才逐体素 set。
//!
//! 生成的方块值来自 static_data 调色板；0 = 空气 = T::default()。

use std::collections::BTreeMap;

use bevy::prelude::*;
use game_engine::math::FixedPoint;
use game_engine::voxel::{
    chunk_content_hash, ChunkGenerator, ChunkKey, LazyChunks, MaxDepth, VoxInterner,
    VoxOpsBulkWrite, VoxOpsWrite, VoxTree, CHUNK_DEPTH, MAX_ALLOWED_DEPTH,
};

use crate::static_data::voxel::{
    biome_def, BiomeId, AIR_BLOCK, DEFAULT_BIOME, DIRT_BLOCK, GRASS_BLOCK, STONE_BLOCK,
};

/// 子块每轴体素数（CHUNK_DEPTH = 5 -> 32）。
pub const CHUNK_SIZE: i32 = 1 << (CHUNK_DEPTH as u32);

/// LOD 3 需要 depth 8；MAX_ALLOWED_DEPTH 必须 >= 9，否则编译期报错。
const _: () = assert!((CHUNK_DEPTH as usize) + 3 < MAX_ALLOWED_DEPTH as usize);

/// 默认 interner 预算（引擎可覆盖；增长路径兜底）。
pub const DEFAULT_INTERNER_BUDGET_BYTES: usize = 16 * 1024 * 1024;

/// 建一个缺省预算的共享 interner。
pub fn new_interner() -> VoxInterner<u8> {
    VoxInterner::<u8>::with_memory_budget(DEFAULT_INTERNER_BUDGET_BYTES)
}

/// 世界生成种子（Bevy Resource）。
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorldSeed(pub u64);

impl WorldSeed {
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }
}

/// 单岛屿的确定性生成器：种子 + 生物群系。
#[derive(Clone, Copy, Debug)]
pub struct WorldGenerator {
    pub seed: WorldSeed,
    pub biome: BiomeId,
}

impl WorldGenerator {
    pub const fn new(seed: WorldSeed, biome: BiomeId) -> Self {
        Self { seed, biome }
    }

    pub const fn from_seed(seed: u64) -> Self {
        Self {
            seed: WorldSeed(seed),
            biome: DEFAULT_BIOME,
        }
    }

    pub const fn with_biome(mut self, biome: BiomeId) -> Self {
        self.biome = biome;
        self
    }

    /// 生成一个子块。
    ///
    /// - 整块在实心内部 -> 单个叶节点（STONE）；
    /// - 整块在地表之上 -> 空树（无节点）；
    /// - 否则逐体素 set 地表附近的三层材料。
    pub fn generate_chunk(&self, interner: &mut VoxInterner<u8>, key: ChunkKey) -> VoxTree<u8> {
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH as u8));

        let base_x = key.x * CHUNK_SIZE;
        let base_y = key.y * CHUNK_SIZE;
        let base_z = key.z * CHUNK_SIZE;

        let palette = column_palette(self.biome);

        // 先扫描整块的表面高度范围，决定能否整体塌陷 / 是否全空气。
        let mut min_surface = i32::MAX;
        let mut max_surface = i32::MIN;
        for local_z in 0..CHUNK_SIZE {
            for local_x in 0..CHUNK_SIZE {
                let surface = self.column_surface(base_x + local_x, base_z + local_z);
                min_surface = min_surface.min(surface);
                max_surface = max_surface.max(surface);
            }
        }

        // 实心内部：最高体素也在地表以下 SUBSURFACE_DEPTH 层 -> 全石。
        if min_surface >= base_y + CHUNK_SIZE + SUBSURFACE_DEPTH {
            tree.fill(interner, palette.stone);
            return tree;
        }

        // 地表之上：最低体素已在地表之上 -> 空气。
        if base_y >= max_surface {
            return tree;
        }

        for local_z in 0..CHUNK_SIZE {
            for local_x in 0..CHUNK_SIZE {
                let surface = self.column_surface(base_x + local_x, base_z + local_z);
                for local_y in 0..CHUNK_SIZE {
                    let depth = surface - 1 - (base_y + local_y);
                    let block = if depth < 0 {
                        AIR_BLOCK
                    } else if depth == 0 {
                        palette.surface
                    } else if depth < SUBSURFACE_DEPTH {
                        palette.subsurface
                    } else {
                        palette.stone
                    };
                    if block != AIR_BLOCK {
                        tree.set(interner, IVec3::new(local_x, local_y, local_z), block);
                    }
                }
            }
        }

        tree
    }

    /// 一次性生成多个子块（按传入顺序调用 generate_chunk，结果与顺序无关）。
    pub fn generate_island<I>(
        &self,
        interner: &mut VoxInterner<u8>,
        keys: I,
    ) -> BTreeMap<ChunkKey, VoxTree<u8>>
    where
        I: IntoIterator<Item = ChunkKey>,
    {
        let mut chunks = BTreeMap::new();
        for key in keys {
            chunks.insert(key, self.generate_chunk(interner, key));
        }
        chunks
    }

    /// 惰性补齐一个子块；已存在则不动并返回 false。
    pub fn ensure_chunk(
        &self,
        interner: &mut VoxInterner<u8>,
        chunks: &mut BTreeMap<ChunkKey, VoxTree<u8>>,
        key: ChunkKey,
    ) -> bool {
        if chunks.contains_key(&key) {
            return false;
        }
        let tree = self.generate_chunk(interner, key);
        chunks.insert(key, tree);
        true
    }

    /// 世界体素坐标 (x, z) 的地表高度：第一层空气的 Y。
    pub fn column_surface(&self, x: i32, z: i32) -> i32 {
        column_height(self.seed.0, self.biome, x as i64, z as i64)
    }
}

impl ChunkGenerator for WorldGenerator {
    fn generate(&mut self, key: ChunkKey, interner: &mut VoxInterner<u8>) -> VoxTree<u8> {
        WorldGenerator::generate_chunk(self, interner, key)
    }
}

/// 用引擎的 LazyChunks 按需补齐给定 key（命中缓存的 key 不重复生成）。
pub fn generate_island_lazy<G>(
    generator: &mut G,
    interner: &mut VoxInterner<u8>,
    keys: &[ChunkKey],
) -> LazyChunks
where
    G: ChunkGenerator + ?Sized,
{
    let mut lazy = LazyChunks::new();
    lazy.pregenerate(keys, interner, generator);
    lazy
}

/// 单列材料层配置。
#[derive(Clone, Copy, Debug)]
struct ColumnPalette {
    surface: u8,
    subsurface: u8,
    stone: u8,
}

fn column_palette(biome: BiomeId) -> ColumnPalette {
    match biome_def(biome) {
        Some(def) => ColumnPalette {
            surface: def.surface_block,
            subsurface: def.subsurface_block,
            stone: def.stone_block,
        },
        None => ColumnPalette {
            surface: GRASS_BLOCK,
            subsurface: DIRT_BLOCK,
            stone: STONE_BLOCK,
        },
    }
}

/// 地表以下第 depth 层仍是表层；深度 1..SUBSURFACE_DEPTH 为次表层。
const SUBSURFACE_DEPTH: i32 = 3;

// ── 定点整数噪声 ──────────────────────────────────────────────────────────

/// 噪声格距（体素）。
const NOISE_CELL_COARSE: i64 = 32;
const NOISE_CELL_FINE: i64 = 12;

/// splitmix64：确定性、无浮点。
fn splitmix64(mut state: u64) -> u64 {
    state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    state = (state ^ (state >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    state = (state ^ (state >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    state ^ (state >> 31)
}

/// 格点哈希。
fn hash_lattice(seed: u64, gx: i64, gz: i64) -> u64 {
    let mut hash = seed;
    hash ^= splitmix64(gx as u64);
    hash = splitmix64(hash);
    hash ^= splitmix64(gz as u64);
    splitmix64(hash)
}

/// 格点取值，范围 [-0.5, 0.5)，Q24 定点。
fn lattice_value(seed: u64, gx: i64, gz: i64) -> FixedPoint {
    let raw = (hash_lattice(seed, gx, gz) & 0x00FF_FFFF) as i64;
    FixedPoint::from_bits(raw - (1 << 23))
}

fn smoothstep(t: FixedPoint) -> FixedPoint {
    let three = FixedPoint::from_bits(3 << 24);
    let two = FixedPoint::from_bits(2 << 24);
    t * t * (three - two * t)
}

fn lerp(a: FixedPoint, b: FixedPoint, t: FixedPoint) -> FixedPoint {
    a + (b - a) * t
}

/// 双线性插值 value noise。
fn value_noise(seed: u64, x: i64, z: i64, cell: i64) -> FixedPoint {
    let gx = x.div_euclid(cell);
    let gz = z.div_euclid(cell);
    let fx = x.rem_euclid(cell);
    let fz = z.rem_euclid(cell);

    let tx = FixedPoint::from_bits((fx << 24) / cell);
    let tz = FixedPoint::from_bits((fz << 24) / cell);

    let v00 = lattice_value(seed, gx, gz);
    let v10 = lattice_value(seed, gx + 1, gz);
    let v01 = lattice_value(seed, gx, gz + 1);
    let v11 = lattice_value(seed, gx + 1, gz + 1);

    let sx = smoothstep(tx);
    let sz = smoothstep(tz);

    lerp(lerp(v00, v10, sx), lerp(v01, v11, sx), sz)
}

/// 单个世界列的定点高度函数（体素整数）。
fn column_height(seed: u64, biome: BiomeId, x: i64, z: i64) -> i32 {
    let (base_height, amplitude) = match biome_def(biome) {
        Some(def) => (def.base_height, def.amplitude),
        None => (64, 8),
    };

    let coarse = value_noise(seed, x, z, NOISE_CELL_COARSE);
    let fine = value_noise(seed ^ 0xA24B_AED4_963E_E407, x, z, NOISE_CELL_FINE);
    let half = FixedPoint::from_bits(1 << 23);
    let noise = coarse + fine * half;

    let base = FixedPoint::from_bits((base_height as i64) << 24);
    let amplitude = FixedPoint::from_bits((amplitude as i64) << 24);
    let height = base + noise * amplitude;

    (height.to_bits() >> 24) as i32
}

// ── 内容哈希（生成确定性测试用）──────────────────────────────────────────

/// 单个子块的结构指纹（委托引擎的 chunk_content_hash）。
///
/// 只遍历子树形状与叶值，不使用 interner 节点索引，因此与构建顺序、
/// interner 实例无关（生成确定性）。
pub fn content_hash(tree: &VoxTree<u8>, interner: &VoxInterner<u8>) -> u64 {
    // 引擎的结构哈希只遍历子树形状与叶值，不用节点索引。
    chunk_content_hash(interner, tree)
}

// ── 自由函数包装（便于测试 / 调用方不持有生成器时使用）───────────────────

pub fn generate_chunk(seed: u64, interner: &mut VoxInterner<u8>, key: ChunkKey) -> VoxTree<u8> {
    WorldGenerator::from_seed(seed).generate_chunk(interner, key)
}

pub fn generate_island<I>(
    seed: u64,
    interner: &mut VoxInterner<u8>,
    keys: I,
) -> BTreeMap<ChunkKey, VoxTree<u8>>
where
    I: IntoIterator<Item = ChunkKey>,
{
    WorldGenerator::from_seed(seed).generate_island(interner, keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_engine::voxel::VoxOpsState;

    const TEST_BUDGET_BYTES: usize = 2 * 1024 * 1024;

    fn test_interner() -> VoxInterner<u8> {
        VoxInterner::<u8>::with_memory_budget(TEST_BUDGET_BYTES)
    }

    fn key(x: i32, y: i32, z: i32) -> ChunkKey {
        ChunkKey { x, y, z }
    }

    #[test]
    fn same_seed_same_chunk_is_bitwise_identical() {
        let generator = WorldGenerator::from_seed(0xC0FF_EE);
        let chunk = key(0, 1, 0);

        let mut first_interner = test_interner();
        let mut second_interner = test_interner();
        let first = generator.generate_chunk(&mut first_interner, chunk);
        let second = generator.generate_chunk(&mut second_interner, chunk);

        assert_eq!(first.get_root_id(), second.get_root_id());
        assert_eq!(
            content_hash(&first, &first_interner),
            content_hash(&second, &second_interner)
        );
    }

    #[test]
    fn different_seed_changes_content() {
        let chunk = key(0, 1, 0);

        let mut first_interner = test_interner();
        let mut second_interner = test_interner();
        let first = WorldGenerator::from_seed(1).generate_chunk(&mut first_interner, chunk);
        let second = WorldGenerator::from_seed(2).generate_chunk(&mut second_interner, chunk);

        assert_ne!(
            content_hash(&first, &first_interner),
            content_hash(&second, &second_interner)
        );
    }

    #[test]
    fn deep_chunk_collapses_to_single_leaf() {
        let generator = WorldGenerator::from_seed(7);
        let mut interner = test_interner();
        let tree = generator.generate_chunk(&mut interner, key(0, -8, 0));

        assert!(!tree.is_empty(), "深地层子块不应为空");
        assert!(tree.is_leaf(), "实心内部应塌陷为单个叶节点");
    }

    #[test]
    fn sky_chunk_is_empty() {
        let generator = WorldGenerator::from_seed(7);
        let mut interner = test_interner();
        let tree = generator.generate_chunk(&mut interner, key(0, 32, 0));
        assert!(tree.is_empty(), "高空子块应为空");
    }

    #[test]
    fn lazy_generation_matches_eager_generation() {
        let generator = WorldGenerator::from_seed(42);
        let keys = vec![
            key(0, 1, 0),
            key(0, 2, 0),
            key(1, 1, 0),
            key(1, 2, 0),
            key(0, 1, 1),
            key(-1, 2, -1),
        ];

        let mut eager_interner = test_interner();
        let eager = generator.generate_island(&mut eager_interner, keys.clone());

        // 反向顺序惰性补齐：内容必须与一次性生成一致。
        let mut lazy_interner = test_interner();
        let mut lazy = BTreeMap::new();
        for chunk in keys.iter().rev() {
            assert!(generator.ensure_chunk(&mut lazy_interner, &mut lazy, *chunk));
            // 重复补齐应为 no-op。
            assert!(!generator.ensure_chunk(&mut lazy_interner, &mut lazy, *chunk));
        }

        assert_eq!(eager.len(), lazy.len());
        for (chunk, eager_tree) in &eager {
            let lazy_tree = lazy.get(chunk).expect("惰性结果应覆盖全部 key");
            assert_eq!(
                content_hash(eager_tree, &eager_interner),
                content_hash(lazy_tree, &lazy_interner),
                "子块 {chunk:?} 惰性生成与一次性生成内容不一致"
            );
        }
    }

    #[test]
    fn content_hash_is_stable_for_repeated_generation() {
        let generator = WorldGenerator::from_seed(1234);
        let chunk = key(0, 1, 0);

        let mut first_interner = test_interner();
        let mut second_interner = test_interner();
        let first = generator.generate_chunk(&mut first_interner, chunk);
        let second = generator.generate_chunk(&mut second_interner, chunk);

        let hash = content_hash(&first, &first_interner);
        assert_eq!(hash, content_hash(&first, &first_interner));
        assert_eq!(hash, content_hash(&second, &second_interner));
    }

    #[test]
    fn engine_lazy_chunks_matches_eager_generation() {
        let keys = vec![key(0, 1, 0), key(0, 2, 0), key(1, 1, 0), key(1, 2, 0)];

        let mut generator = WorldGenerator::from_seed(42);

        let mut eager_interner = test_interner();
        let eager = generator.generate_island(&mut eager_interner, keys.clone());

        let mut lazy_interner = test_interner();
        let mut lazy = LazyChunks::new();
        for &chunk in keys.iter().rev() {
            // 命中缓存时返回已有的树，不重新生成。
            lazy.get_or_generate(chunk, &mut lazy_interner, &mut generator);
            lazy.get_or_generate(chunk, &mut lazy_interner, &mut generator);
        }
        assert_eq!(lazy.len(), keys.len());

        for (chunk, eager_tree) in &eager {
            let lazy_tree = lazy.get(chunk).expect("LazyChunks 应覆盖全部 key");
            assert_eq!(
                content_hash(eager_tree, &eager_interner),
                chunk_content_hash(&lazy_interner, lazy_tree),
                "引擎 LazyChunks 结果应与一次性生成一致"
            );
        }
    }
}
