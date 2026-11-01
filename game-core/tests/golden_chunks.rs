//! 黄金哈希语料：固定 seed 下 100 个 LOD0 子块的 chunk_content_hash
//! （docs/待办/12-流式体素与区块生成.md §11 阶段 0）。
//!
//! 只用公开 API 走最小路径：
//!
//!     WorldGenerator::from_seed(seed)             // DEFAULT_BIOME = meadow
//!         .generate_chunk(&mut interner, key)     // LOD0，块内 32³ 体素
//!         -> game_engine::voxel::chunk_content_hash(&interner, &tree)
//!
//! 语料落在 tests/golden/chunk_hashes.txt，每行 "seed x y z hash"：
//! seed 十进制；x/y/z 是 LOD0 子块坐标（generate_chunk 的入参）；
//! hash 是 16 位十六进制（chunk_content_hash 返回的 u64）。
//!
//! 基线由当前代码生成一次后落盘；此后任何重构都必须让本文件的测试继续通过，
//! 从而证明「行为未变」。只有确认生成行为被有意改变时才重写基线：
//!
//!     WRITE_GOLDEN=1 cargo test -p game-core --test golden_chunks \
//!         rewrite_corpus_from_current_code -- --exact
//!
//! 默认 cargo test 只跑便宜的 golden_corpus_is_well_formed 与
//! golden_chunk_smoke_matches_corpus（12 块）。两条全量测试
//! （golden_chunk_hashes_match_corpus / golden_chunk_hashes_are_order_independent）
//! 标了 #[ignore]：全量 300 块，debug 约 4-7 分钟；需要时跑：
//!
//!     cargo test --release -p game-core --test golden_chunks -- --ignored
//!
//! 约束：判定不涉及浮点；seed、坐标、乱序序列全部写死或由 splitmix64 决定，
//! 容器只用 BTreeMap（不依赖 HashMap 的迭代顺序）。

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use game_core::voxel::terrain::generation::{new_interner, WorldGenerator, CHUNK_SIZE};
use game_engine::voxel::{chunk_content_hash, dec_ref_recursive, ChunkKey, VoxInterner};

/// 语料覆盖的 3 个 seed：0（边界值）、0xC0FFEE（既有测试常用值）、
/// 0xDEADBEEFCAFEF00D（接近 u64 上限的大数，压高位）。
const SEEDS: [u64; 3] = [0, 0x00C0_FFEE, 0xDEAD_BEEF_CAFE_F00D];

/// 每个 seed 的 100 个写死坐标（全部是 LOD0 子块，块内 32³ 体素）。
///
/// 选择原则（覆盖生成器的全部分支 / 边界，不做随机）：
/// - y 带：-16/-8/-4 在实心内部（fill 塌陷成单叶），0/1 是地下，2 是地表主带
///   （DEFAULT_BIOME base_height 64 / amplitude 8，地表落在 58..70），
///   3/4/5/8 在地表之上（空树）；
/// - 负坐标：x/z 取 -1 到 -1048576，覆盖 div_euclid / rem_euclid 的负数路径；
/// - 噪声格边界：x/z = ±31/±32/±33（粗格 NOISE_CELL_COARSE = 32）、
///   ±12/±13（细格 NOISE_CELL_FINE = 12），块边界正好压在格边界上；
/// - 远距离：(1024, -2048)、(-1024, 2048)、(65536, 65536)、(-65536, -65536)、
///   (±1048576, ∓1048576)，压 hash_lattice 的大整数格点（约 2^20 块 = 2^25 体素）；
/// - 邻接：x = 2/3/4 与 -2/-3/-4 连续三块，覆盖接缝两侧的坐标；
/// - 混合符号：(11, -11)、(-13, 13)、(-1, 1) 等，覆盖四个象限。
const COORDS: [(i32, i32, i32); 100] = [
    // ── 原点竖带：实心 -> 地下 -> 地表 -> 天空（8）──
    (0, -8, 0),
    (0, -4, 0),
    (0, -1, 0),
    (0, 0, 0),
    (0, 1, 0),
    (0, 2, 0),
    (0, 3, 0),
    (0, 4, 0),
    // ── 粗格 32 边界上的地表列，y = -1/1/2/3（6 列 × 4 = 24）──
    (31, -1, 31),
    (31, 1, 31),
    (31, 2, 31),
    (31, 3, 31),
    (32, -1, 32),
    (32, 1, 32),
    (32, 2, 32),
    (32, 3, 32),
    (33, -1, 33),
    (33, 1, 33),
    (33, 2, 33),
    (33, 3, 33),
    (-31, -1, -31),
    (-31, 1, -31),
    (-31, 2, -31),
    (-31, 3, -31),
    (-32, -1, -32),
    (-32, 1, -32),
    (-32, 2, -32),
    (-32, 3, -32),
    (-33, -1, -33),
    (-33, 1, -33),
    (-33, 2, -33),
    (-33, 3, -33),
    // ── 细格 12 边界与跨 0 边界，y = 1/2（6 列 × 2 = 12）──
    (12, 1, 12),
    (12, 2, 12),
    (-12, 1, -12),
    (-12, 2, -12),
    (-13, 1, 13),
    (-13, 2, 13),
    (11, 1, -11),
    (11, 2, -11),
    (1, 1, -1),
    (1, 2, -1),
    (-1, 1, 1),
    (-1, 2, 1),
    // ── 负坐标深带，y = 0/1/2（5 列 × 3 = 15）──
    (-5, 0, -7),
    (-5, 1, -7),
    (-5, 2, -7),
    (-9, 0, 3),
    (-9, 1, 3),
    (-9, 2, 3),
    (5, 0, -9),
    (5, 1, -9),
    (5, 2, -9),
    (-123, 0, -456),
    (-123, 1, -456),
    (-123, 2, -456),
    (456, 0, -123),
    (456, 1, -123),
    (456, 2, -123),
    // ── 远距离，y = 1/2（4 列 × 2 = 8）──
    (1024, 1, -2048),
    (1024, 2, -2048),
    (-1024, 1, 2048),
    (-1024, 2, 2048),
    (65536, 1, 65536),
    (65536, 2, 65536),
    (-65536, 1, -65536),
    (-65536, 2, -65536),
    // ── 极远（2^20 块），y = -8/1/2/5（2 列 × 4 = 8）──
    (1048576, -8, -1048576),
    (1048576, 1, -1048576),
    (1048576, 2, -1048576),
    (1048576, 5, -1048576),
    (-1048576, -8, 1048576),
    (-1048576, 1, 1048576),
    (-1048576, 2, 1048576),
    (-1048576, 5, 1048576),
    // ── 深地 / 高空补充（4 + 4 + 2 = 10）──
    (7, -16, -11),
    (7, -4, -11),
    (7, 5, -11),
    (7, 8, -11),
    (100, -16, -100),
    (100, -4, -100),
    (100, 5, -100),
    (100, 8, -100),
    (-1, -16, 1),
    (-1, 5, 1),
    // ── 接缝两侧的连续三块（6）──
    (2, 2, 2),
    (3, 2, 2),
    (4, 2, 2),
    (-2, 2, -2),
    (-3, 2, -2),
    (-4, 2, -2),
    // ── 其余混合分布（9）──
    (17, 2, -19),
    (-23, 2, 29),
    (63, 1, -64),
    (64, 1, 64),
    (-64, 2, -63),
    (255, 2, 0),
    (-256, 2, 0),
    (2048, 2, 2048),
    (0, 2, 1048576),
];

/// smoke 测试的 12 个 (seed, x, y, z)：3 个 seed 各 4 个，全部取自 COORDS / 语料。
///
/// 覆盖：地下 fill 塌陷（y=-16/-8）、地表主带（y=1/2）、地表之上空树（y=5/8）、
/// 负坐标混合符号（-13,13）、粗格边界 ±32/±33、邻接接缝（x=3）、
/// 远距离（±65536 与 2^20 档）、跨 0 边界。
const SMOKE_CHUNKS: [(u64, i32, i32, i32); 12] = [
    // seed 0
    (SEEDS[0], -1, -16, 1),           // 地下 fill 塌陷 + 负坐标 + 跨 0 边界
    (SEEDS[0], 32, 1, 32),            // 地表主带 + 粗格 32 边界
    (SEEDS[0], 1048576, 5, -1048576), // 地表之上空树 + 极远 2^20 档
    (SEEDS[0], 63, 1, -64),           // 混合符号 + 接缝附近
    // seed 0xC0FFEE
    (SEEDS[1], 7, 8, -11),       // 高空空树
    (SEEDS[1], -13, 2, 13),      // 负坐标混合符号 + 细格 12 边界 + 地表主带
    (SEEDS[1], -33, 1, -33),     // 负坐标粗格 -33 边界 + 地表
    (SEEDS[1], 65536, 2, 65536), // 远距离 65536
    // seed 0xDEADBEEFCAFEF00D
    (SEEDS[2], 0, -8, 0),             // 地下 fill 塌陷
    (SEEDS[2], 3, 2, 2),              // 邻接接缝 x=3 + 跨 0 边界
    (SEEDS[2], -1048576, 1, 1048576), // 极远负坐标 + 地表主带
    (SEEDS[2], -1, 5, 1),             // 高空空树 + 混合符号
];

/// 语料相对 game-core 的路径（用 CARGO_MANIFEST_DIR 解析，测试可从任意 cwd 跑）。
const CORPUS_RELATIVE_PATH: &str = "tests/golden/chunk_hashes.txt";

/// 一条语料记录。
#[derive(Clone, Copy, Debug)]
struct Record {
    seed: u64,
    x: i32,
    y: i32,
    z: i32,
    hash: u64,
}

fn corpus_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CORPUS_RELATIVE_PATH)
}

/// 读语料；空行与 # 开头的注释行跳过。格式错误带上行号。
fn parse_corpus() -> Vec<Record> {
    let path = corpus_path();
    let text = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "读不到黄金语料 {}：{error}；请按该文件头的命令生成",
            path.display()
        )
    });

    let mut records = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(
            fields.len(),
            5,
            "语料第 {line_number} 行应为 \"seed x y z hash\"，实际：{line:?}"
        );
        let seed = fields[0]
            .parse::<u64>()
            .unwrap_or_else(|error| panic!("第 {line_number} 行 seed 解析失败：{error}"));
        let x = fields[1]
            .parse::<i32>()
            .unwrap_or_else(|error| panic!("第 {line_number} 行 x 解析失败：{error}"));
        let y = fields[2]
            .parse::<i32>()
            .unwrap_or_else(|error| panic!("第 {line_number} 行 y 解析失败：{error}"));
        let z = fields[3]
            .parse::<i32>()
            .unwrap_or_else(|error| panic!("第 {line_number} 行 z 解析失败：{error}"));
        let hash_text = fields[4];
        let hex = hash_text.strip_prefix("0x").unwrap_or_else(|| {
            panic!("第 {line_number} 行 hash 必须是 0x 前缀的十六进制：{hash_text:?}")
        });
        assert_eq!(
            hex.len(),
            16,
            "第 {line_number} 行 hash 必须是 16 位十六进制：{hash_text:?}"
        );
        let hash = u64::from_str_radix(hex, 16)
            .unwrap_or_else(|error| panic!("第 {line_number} 行 hash 解析失败：{error}"));
        records.push(Record {
            seed,
            x,
            y,
            z,
            hash,
        });
    }
    records
}

fn corpus_map(records: &[Record]) -> BTreeMap<(u64, i32, i32, i32), u64> {
    let mut map = BTreeMap::new();
    for record in records {
        map.insert((record.seed, record.x, record.y, record.z), record.hash);
    }
    map
}

/// 生成 + 取内容哈希 + 立即释放树。释放是为了让 interner 复用节点（乱序测试
/// 在同一个 interner 上跑完 300 块也不必扩预算）；chunk_content_hash 只依赖
/// 树形与叶值，与节点复用无关。
fn chunk_hash(generator: &WorldGenerator, interner: &mut VoxInterner<u8>, key: ChunkKey) -> u64 {
    let tree = generator.generate_chunk(interner, key);
    let hash = chunk_content_hash(interner, &tree);
    let root = tree.get_root_id();
    dec_ref_recursive(interner, &root);
    hash
}

/// 一个 seed 下 100 个写死坐标的实际哈希（BTreeMap 按坐标升序）。
fn compute_seed_hashes(seed: u64) -> BTreeMap<(i32, i32, i32), u64> {
    let generator = WorldGenerator::from_seed(seed);
    let mut interner = new_interner();
    let mut hashes = BTreeMap::new();
    for (x, y, z) in COORDS {
        let hash = chunk_hash(&generator, &mut interner, ChunkKey::new(x, y, z));
        hashes.insert((x, y, z), hash);
    }
    hashes
}

/// 乱序排序键：splitmix64（与生成器同族），确定性、无浮点。
fn shuffle_key(seed: u64, x: i32, y: i32, z: i32) -> u64 {
    const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
    fn splitmix64(mut state: u64) -> u64 {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        state = (state ^ (state >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        state = (state ^ (state >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        state ^ (state >> 31)
    }
    let mut hash = seed ^ GAMMA;
    hash = splitmix64(hash ^ (x as i64 as u64));
    hash = splitmix64(hash ^ (y as i64 as u64));
    splitmix64(hash ^ (z as i64 as u64))
}

/// 逐行重算语料：不一致时报告第一处差异（seed / 坐标 / 期望 / 实际）。
///
/// 全量 300 块，默认不跑（见文件头的 ignore 说明）。
#[test]
#[ignore = "全量 300 块，debug 约 4-7 分钟；跑：cargo test --release -p game-core --test golden_chunks -- --ignored"]
fn golden_chunk_hashes_match_corpus() {
    // 语料是 LOD0（块内 32³ 体素）的基线；块尺寸变了必须重写基线。
    assert_eq!(CHUNK_SIZE, 32, "语料按 LOD0（块内 32³ 体素）记录");

    let records = parse_corpus();
    assert_eq!(
        records.len(),
        SEEDS.len() * COORDS.len(),
        "语料应覆盖 {} seed × {} 块",
        SEEDS.len(),
        COORDS.len()
    );
    let corpus = corpus_map(&records);

    for seed in SEEDS {
        let mut actual = compute_seed_hashes(seed);
        for (x, y, z) in COORDS {
            let expected = *corpus
                .get(&(seed, x, y, z))
                .unwrap_or_else(|| panic!("语料缺少 seed={seed} 块 ({x}, {y}, {z})"));
            let got = actual
                .remove(&(x, y, z))
                .expect("compute_seed_hashes 应覆盖 COORDS 全部坐标");
            if got != expected {
                panic!(
                    "黄金语料不一致（第一处差异）：\n  seed     = {seed} (0x{seed:016x})\n  chunk    = ({x}, {y}, {z})\n  expected = 0x{expected:016x}\n  actual   = 0x{got:016x}\n生成结果已改变；若为有意改动，确认影响后按 {} 文件头的命令重写基线。",
                    CORPUS_RELATIVE_PATH
                );
            }
        }
    }
}

/// 同一批块打乱生成顺序（单 interner，300 块混合 3 个 seed），哈希集合与语料一致。
///
/// 全量 300 块，默认不跑（见文件头的 ignore 说明）。
#[test]
#[ignore = "全量 300 块，debug 约 4-7 分钟；跑：cargo test --release -p game-core --test golden_chunks -- --ignored"]
fn golden_chunk_hashes_are_order_independent() {
    let corpus = corpus_map(&parse_corpus());

    let mut keys: Vec<(u64, i32, i32, i32)> = SEEDS
        .iter()
        .flat_map(|&seed| COORDS.iter().map(move |&(x, y, z)| (seed, x, y, z)))
        .collect();
    assert_eq!(keys.len(), SEEDS.len() * COORDS.len());
    keys.sort_by_key(|&(seed, x, y, z)| shuffle_key(seed, x, y, z));
    assert_ne!(
        keys[0],
        (SEEDS[0], COORDS[0].0, COORDS[0].1, COORDS[0].2),
        "乱序序列不应与语料顺序相同，否则没有测到顺序无关"
    );

    // 单个 interner 跑完全部 300 块：节点会被回收复用，内容哈希必须与分配顺序无关。
    let mut interner = new_interner();
    let mut shuffled: BTreeMap<(u64, i32, i32, i32), u64> = BTreeMap::new();
    for &(seed, x, y, z) in &keys {
        let generator = WorldGenerator::from_seed(seed);
        let hash = chunk_hash(&generator, &mut interner, ChunkKey::new(x, y, z));
        shuffled.insert((seed, x, y, z), hash);
    }
    assert_eq!(
        shuffled.len(),
        corpus.len(),
        "乱序结果应覆盖语料全部 (seed, 坐标)"
    );

    for (&key, &expected) in &corpus {
        let (seed, x, y, z) = key;
        let got = *shuffled
            .get(&key)
            .unwrap_or_else(|| panic!("乱序结果缺少 seed={seed} 块 ({x}, {y}, {z})"));
        if got != expected {
            panic!(
                "乱序生成与语料不一致（第一处差异）：\n  seed     = {seed} (0x{seed:016x})\n  chunk    = ({x}, {y}, {z})\n  expected = 0x{expected:016x}\n  actual   = 0x{got:016x}\n生成结果依赖生成顺序，违反 L3 确定性约束。"
            );
        }
    }
}

/// 默认跑的烟测：从语料里挑 12 个 (seed, x, y, z)（3 seed 各 4 个）重算比对。
///
/// expected 直接取语料；取不到说明 SMOKE_CHUNKS 与语料脱节，直接 panic。
#[test]
fn golden_chunk_smoke_matches_corpus() {
    let corpus = corpus_map(&parse_corpus());

    let mut per_seed: BTreeMap<u64, usize> = BTreeMap::new();
    for (seed, _, _, _) in SMOKE_CHUNKS {
        *per_seed.entry(seed).or_default() += 1;
    }
    assert_eq!(
        per_seed.keys().copied().collect::<Vec<u64>>(),
        SEEDS.to_vec(),
        "smoke 必须覆盖全部 3 个 seed"
    );
    for (seed, count) in &per_seed {
        assert_eq!(*count, 4, "smoke 每个 seed 必须恰好 4 块（seed={seed}）");
    }

    let mut interner = new_interner();
    for (seed, x, y, z) in SMOKE_CHUNKS {
        let expected = *corpus
            .get(&(seed, x, y, z))
            .unwrap_or_else(|| panic!("smoke 选取的坐标不在语料中：seed={seed} ({x}, {y}, {z})"));
        let generator = WorldGenerator::from_seed(seed);
        let actual = chunk_hash(&generator, &mut interner, ChunkKey::new(x, y, z));
        if actual != expected {
            panic!(
                "黄金语料不一致（smoke 第一处差异）：\n  seed     = {seed} (0x{seed:016x})\n  chunk    = ({x}, {y}, {z})\n  expected = 0x{expected:016x}\n  actual   = 0x{actual:016x}\n生成结果已改变；若为有意改动，确认影响后按 {} 文件头的命令重写基线。",
                CORPUS_RELATIVE_PATH
            );
        }
    }
}

/// 语料自身结构：3 个 seed × 100 个唯一坐标、行序按 (seed, x, y, z) 升序。
#[test]
fn golden_corpus_is_well_formed() {
    let unique_coords: BTreeMap<(i32, i32, i32), ()> = COORDS.iter().map(|&c| (c, ())).collect();
    assert_eq!(unique_coords.len(), COORDS.len(), "COORDS 不应有重复坐标");

    let records = parse_corpus();
    assert_eq!(records.len(), SEEDS.len() * COORDS.len());

    let mut per_seed: BTreeMap<u64, BTreeMap<(i32, i32, i32), u64>> = BTreeMap::new();
    for record in &records {
        let seed_map = per_seed.entry(record.seed).or_default();
        assert!(
            seed_map
                .insert((record.x, record.y, record.z), record.hash)
                .is_none(),
            "语料有重复行：seed={} ({}, {}, {})",
            record.seed,
            record.x,
            record.y,
            record.z
        );
    }

    let seeds: Vec<u64> = per_seed.keys().copied().collect();
    assert_eq!(seeds, SEEDS.to_vec(), "语料 seed 集合必须与 SEEDS 一致");
    for (seed, seed_map) in &per_seed {
        assert_eq!(seed_map.len(), COORDS.len(), "seed={seed} 的块数不对");
        for coord in seed_map.keys() {
            assert!(
                unique_coords.contains_key(coord),
                "语料含坐标表外的块：seed={seed} {coord:?}"
            );
        }
    }

    // 行序稳定，diff 才能只显示真正变化的行。
    let mut previous: Option<(u64, i32, i32, i32)> = None;
    for record in &records {
        let key = (record.seed, record.x, record.y, record.z);
        if let Some(prev) = previous {
            assert!(
                prev < key,
                "语料行序必须按 (seed, x, y, z) 升序：{prev:?} -> {key:?}"
            );
        }
        previous = Some(key);
    }
}

/// 用当前代码重写基线：只有显式设置 WRITE_GOLDEN=1 时才执行，否则打印提示并跳过。
///
/// 这是工具而不是断言，所以名字里没有 golden、也不标 #[ignore]：保证
/// "cargo test ... golden" 只看到两条全量测试被 ignore，"-- --ignored"
/// 也只跑那两条全量测试（不会误触重写）。
#[test]
fn rewrite_corpus_from_current_code() {
    if env::var("WRITE_GOLDEN").ok().as_deref() != Some("1") {
        println!("未设置 WRITE_GOLDEN=1，跳过语料重写（不会修改 {CORPUS_RELATIVE_PATH}）");
        return;
    }

    let mut text = String::new();
    text.push_str("# 黄金哈希语料（docs/待办/12-流式体素与区块生成.md §11 阶段 0）\n");
    text.push_str(
        "# 每行：seed x y z hash（seed 十进制；x/y/z 为 LOD0 子块坐标；hash 为 16 位十六进制）\n",
    );
    text.push_str("# 生成：WorldGenerator::from_seed(seed).generate_chunk + game_engine::voxel::chunk_content_hash\n");
    text.push_str("# 重写：WRITE_GOLDEN=1 cargo test -p game-core --test golden_chunks rewrite_corpus_from_current_code -- --exact\n");

    let mut lines = 0usize;
    for seed in SEEDS {
        for ((x, y, z), hash) in compute_seed_hashes(seed) {
            text.push_str(&format!("{seed} {x} {y} {z} 0x{hash:016x}\n"));
            lines += 1;
        }
    }

    let path = corpus_path();
    fs::create_dir_all(path.parent().expect("语料路径应有父目录")).expect("创建 tests/golden 失败");
    fs::write(&path, &text).expect("写语料失败");
    println!(
        "已写入 {}：{} 行（{} seed × {} 块）",
        path.display(),
        lines,
        SEEDS.len(),
        COORDS.len()
    );
    assert_eq!(lines, SEEDS.len() * COORDS.len());
}
