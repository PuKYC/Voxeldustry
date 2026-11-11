//! 存档文件 IO 层：目录与命名（§1）、原子写与崩溃一致性（§5）。
//!
//! 本层**只做文件**：字节由 `save::codec` 生成/校验，这里负责“写到哪、
//! 怎么原子地写、读失败怎么办”。不注册 Plugin、不开线程、不加依赖，只用 `std::fs`。
//!
//! # 目录（§1）
//!
//! ```text
//! <root>/world.meta
//! <root>/<body.0 十进制>/region_<rx>_<ry>_<rz>
//! <root>/<body.0 十进制>/region_<rx>_<ry>_<rz>.tmp        # 写入中，崩溃残留
//! <root>/<body.0 十进制>/region_<rx>_<ry>_<rz>.corrupt    # 首次读失败的原字节备份
//! <root>/<body.0 十进制>/region_<rx>_<ry>_<rz>.recovered  # 之后唯一的写目标
//! ```
//!
//! region 键 = 逐轴 `div_euclid(4)`（负坐标不是截断除法），文件名是有符号十进制、
//! 无前导零：`ChunkKey(-1, 0, 8)` → `region_-1_0_2`。每块的 `ChunkKey` 必须满足
//! `region_of(key) == 文件名`，所以 [`SavePaths::region`] 一律先做 [`region_origin`]。
//!
//! # 写（§5.1）
//!
//! 目标路径**只**由 [`SavePaths::write_target`] 决定，调用方不要自己判断：
//! 存在 `.recovered`（粘性，§9）或本会话主文件读失败 → 写 `.recovered`，否则写主文件。
//! 所有文件都是 `tmp + fsync + rename + fsync 目录`，见 [`write_atomic`]。
//!
//! # 读失败（§5.3）
//!
//! 优先级 **`.recovered` > 主文件 > `Miss`**（`Ok(None)`）：
//!
//! - `.recovered` 存在且能解码 → 用它（即使主文件也可读，避免主/恢复分叉）；
//! - `.recovered` 存在但解码失败 → **保留它不动**，回退主文件，并 quarantine 该文件的写入；
//! - 主文件解码失败 → 首次把主文件逐字节复制成 `.corrupt`（只留第一份，§5.3），
//!   然后返回 `Err`：数据按 `Miss` 处理，错误就是调用方要报的
//!   `SaveFailed { path, reason }`，**主文件一个字节都不改**；
//! - 复制失败（磁盘满 / 权限）→ quarantine，之后拒绝写该文件（返回 `Err`，§5.3）。
//!
//! `world.meta` 是唯一不能降级为 `Miss` 的文件（§2）：解码失败一律返回 `Err`，
//! `Ok(None)` 只表示“文件不存在”（= 新世界）。
//!
//! 本层不发 `SaveFailed` 事件、不打日志（纯 `std::fs`）：调用方拿到 `Err` 后
//! 自己告警；以 `.recovered` 载入成功也必须由调用方提示“世界已从 .recovered 恢复”。

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ::voxel::ChunkKey;

use super::codec::{decode_region, decode_world_meta, encode_region, encode_world_meta};
use super::format::{
    region_of, RegionFile, WorldMeta, BODY_RECORD_LEN, DIGEST_LEN, MAX_BODY_COUNT,
    MAX_REGION_FILE_LEN, WORLD_HEADER_LEN,
};
use super::fs_atomic::{self, io_error};
use super::{SaveError, SaveErrorKind};
use crate::body_key::BodyKey;
use crate::identity::StableEntityId;

pub use super::fs_atomic::write_atomic;

/// `world.meta` 的读取上限：48 B 头 + 65536 × 54 B body + 8 B 摘要（§2）。
const MAX_WORLD_META_FILE_LEN: u64 =
    WORLD_HEADER_LEN as u64 + MAX_BODY_COUNT as u64 * BODY_RECORD_LEN as u64 + DIGEST_LEN as u64;

/// 存档目录布局 + 本会话的读失败状态。
///
/// 只持有 `root` 与“哪些文件要写 `.recovered` / 哪些拒写”的集合；不缓存文件内容、
/// 不开线程、不注册 Plugin。`Mutex` 只是为了让 `SavePaths` 能作为资源共享
/// （`Send + Sync`），锁内只有几次 `BTreeSet` 查找，不碰 IO。
#[derive(Debug)]
pub struct SavePaths {
    root: PathBuf,
    state: Mutex<SessionState>,
}

/// 本会话的读失败状态（§5.3）。进程重启后清空，`.recovered` 的存在跨会话生效。
#[derive(Debug, Default)]
struct SessionState {
    /// 本会话主文件读失败过的文件：之后写它必须走 `.recovered`（避免主/恢复分叉）。
    recovered_targets: BTreeSet<PathBuf>,
    /// 拒写（`.corrupt` 复制失败，或 `.recovered` 解不开）：玩家处理前不下笔。
    quarantined: BTreeSet<PathBuf>,
}

impl SavePaths {
    /// 以存档根目录（`saves/<world>`）构造；不创建目录，纯路径计算。
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            state: Mutex::new(SessionState::default()),
        }
    }

    /// 存档根目录。
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/world.meta`（§1）。
    #[must_use]
    pub fn world_meta(&self) -> PathBuf {
        self.root.join("world.meta")
    }

    /// `<root>/<body.0 十进制>`：无符号、无前导零、无前缀（§1）。
    #[must_use]
    pub fn body_dir(&self, body: BodyKey) -> PathBuf {
        self.root.join(body.as_u64().to_string())
    }

    /// `<root>/<body.0>/region_<rx>_<ry>_<rz>`。
    ///
    /// `rx/ry/rz` 由 [`region_origin`]（逐轴 `div_euclid(4)`）算出，
    /// 所以同 region 内任何 `ChunkKey` 都落到同一个文件。
    #[must_use]
    pub fn region(&self, body: BodyKey, key: ChunkKey) -> PathBuf {
        self.body_dir(body)
            .join(region_file_name(region_origin(key)))
    }

    /// 同目录、追加 `.corrupt` 后缀：`region_0_0_0.corrupt` / `world.meta.corrupt`。
    #[must_use]
    pub fn corrupt(&self, path: &Path) -> PathBuf {
        fs_atomic::with_suffix(path, ".corrupt")
    }

    /// 同目录、追加 `.recovered` 后缀（§5.3 的写目标）。
    #[must_use]
    pub fn recovered(&self, path: &Path) -> PathBuf {
        fs_atomic::with_suffix(path, ".recovered")
    }

    /// 同目录、追加 `.tmp` 后缀（写入中的临时文件，§1）。
    ///
    /// 启动清理可以直接用它扫残留：`.tmp` 从未 rename，不可能是有效档。
    #[must_use]
    pub fn tmp(&self, path: &Path) -> PathBuf {
        fs_atomic::tmp_path(path)
    }

    /// 该文件的**唯一写目标**（§5.3 / §9 的粘性）。
    ///
    /// 满足任一条件就写 `.recovered`：
    ///
    /// - `.recovered` 已经存在（跨会话粘性：主文件后来修好也不回主文件）；
    /// - 本会话里主文件读过且失败过（首次写 `.recovered` 时它还不存在）。
    ///
    /// 回到主文件的唯一路径是**手动删除** `.recovered` 并重新 [`SavePaths::new`]
    /// （§9 最后一条）。
    #[must_use]
    pub fn write_target(&self, path: &Path) -> PathBuf {
        let recovered = self.recovered(path);
        if recovered.exists() || self.state().recovered_targets.contains(path) {
            recovered
        } else {
            path.to_path_buf()
        }
    }

    /// 该文件是否被 quarantine：玩家处理前拒绝写入（§5.3）。
    #[must_use]
    pub fn is_quarantined(&self, path: &Path) -> bool {
        self.state().quarantined.contains(path)
    }

    /// 标记“之后必须写 `.recovered`”（主文件读失败时调用）。
    pub(crate) fn mark_recovered_target(&self, path: &Path) {
        self.state_mut()
            .recovered_targets
            .insert(path.to_path_buf());
    }

    /// 标记“拒写”（`.corrupt` 复制失败、`.recovered` 解码失败时调用）。
    pub(crate) fn quarantine(&self, path: &Path) {
        self.state_mut().quarantined.insert(path.to_path_buf());
    }

    fn state(&self) -> std::sync::MutexGuard<'_, SessionState> {
        // 锁内只做集合查询；poison 只可能是别的线程 panic，恢复内层数据继续用。
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn state_mut(&self) -> std::sync::MutexGuard<'_, SessionState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// region 键：逐轴 `div_euclid(REGION_SIZE)`（§1，负坐标必须用欧几里得除法）。
#[must_use]
pub fn region_origin(key: ChunkKey) -> ChunkKey {
    region_of(key)
}

/// 文件名主体：`region_<x>_<y>_<z>`，有符号十进制、无前导零（`region_-1_0_2`）。
///
/// 规格 §1 只写死 `region_<rx>_<ry>_<rz>` 与 `region_-1_0_2` 这个例子，
/// 没写分隔符/宽度，这里按 `region_{x}_{y}_{z}` 实现（与 `Display for i32` 一致）。
#[must_use]
fn region_file_name(origin: ChunkKey) -> String {
    format!("region_{}_{}_{}", origin.x, origin.y, origin.z)
}

/// 从文件名解析 region 键（接受可选的 `.recovered` 后缀，其它后缀调用方先跳过）。
///
/// 只接受 `region_file_name` 的规范输出：`+1`、前导零、多余分量都必须拒绝，
/// 否则同一个 region 可能对应多个文件，枚举就不再是确定的。
fn parse_region_file_name(name: &str) -> Option<ChunkKey> {
    let base = name.strip_suffix(".recovered").unwrap_or(name);
    let rest = base.strip_prefix("region_")?;
    let mut parts = rest.split('_');
    let x = parts.next()?.parse::<i32>().ok()?;
    let y = parts.next()?.parse::<i32>().ok()?;
    let z = parts.next()?.parse::<i32>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let key = ChunkKey::new(x, y, z);
    (region_file_name(key) == base).then_some(key)
}

/// 文件名不是规范的 `region_<x>_<y>_<z>`：拒绝而不是跳过（未知文件可能是玩家/工具放错）。
fn invalid_region_name(path: &Path) -> SaveError {
    SaveError::new(
        path.display().to_string(),
        0,
        SaveErrorKind::InvalidValue {
            what: "region file name",
            value: 0,
        },
    )
}

/// 缺省写目标被 quarantine 时的错误：拒绝写，不覆盖主文件、不静默降级（§5.3）。
fn quarantined_error(path: &Path) -> SaveError {
    SaveError::new(
        path.display().to_string(),
        0,
        SaveErrorKind::Io {
            stage: fs_atomic::STAGE_QUARANTINE,
            kind: io::ErrorKind::PermissionDenied,
            raw: None,
        },
    )
}

/// 读一个文件的全部字节；不存在返回 `Ok(None)`（= `Miss`），其余 IO 错误带阶段返回。
///
/// 先按 `max_len` 检查长度再读，避免拿一个畸形大文件把内存撑爆（§0 的上限）。
fn read_file(path: &Path, max_len: u64) -> Result<Option<Vec<u8>>, SaveError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io_error(fs_atomic::STAGE_OPEN, path, err)),
    };
    let len = file
        .metadata()
        .map_err(|err| io_error(fs_atomic::STAGE_READ, path, err))?
        .len();
    if len > max_len {
        return Err(SaveError::new(
            path.display().to_string(),
            0,
            SaveErrorKind::TooLarge {
                what: "file length",
                value: len,
                max: max_len,
            },
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    file.read_to_end(&mut bytes)
        .map_err(|err| io_error(fs_atomic::STAGE_READ, path, err))?;
    Ok(Some(bytes))
}

/// `.corrupt` 备份：首次主文件读失败时把主文件原字节逐字节复制过去（§5.3）。
///
/// `.corrupt` 只留第一份：已存在就不再覆盖、不删除。
/// 复制失败（磁盘满 / 权限）→ 本会话 quarantine 该文件并返回错误，**不**覆盖主文件。
fn backup_corrupt(paths: &SavePaths, main: &Path) -> Result<(), SaveError> {
    let corrupt = paths.corrupt(main);
    if corrupt.is_file() {
        return Ok(());
    }
    match fs_atomic::copy_atomic(main, &corrupt) {
        Ok(()) => Ok(()),
        Err(err) => {
            paths.quarantine(main);
            Err(err)
        }
    }
}

/// 通用读取：`.recovered` > 主文件 > `Miss`（§5.3），失败时维护 `.corrupt` 与 quarantine。
fn read_with_recovery<T>(
    paths: &SavePaths,
    main: &Path,
    max_len: u64,
    decode: impl Fn(&[u8]) -> Result<T, SaveError>,
) -> Result<Option<T>, SaveError> {
    let recovered = paths.recovered(main);
    let mut recovered_error = None;

    // 1. .recovered 优先：只要存在且能解码就用它，哪怕主文件也可读。
    if let Some(bytes) = read_file(&recovered, max_len)? {
        match decode(&bytes) {
            Ok(value) => return Ok(Some(value)),
            Err(err) => {
                // 保留 .recovered 不动，回退主文件；玩家处理前 quarantine 该文件的写入。
                paths.quarantine(main);
                recovered_error = Some(err);
            }
        }
    }

    // 2. 主文件。
    match read_file(main, max_len)? {
        Some(bytes) => match decode(&bytes) {
            Ok(value) => Ok(Some(value)),
            Err(err) => {
                // 首次失败：备份原字节；备份失败会 quarantine 并把错误换成复制错误。
                backup_corrupt(paths, main)?;
                // 本会话之后写 .recovered（§5.3），主文件永不改写。
                paths.mark_recovered_target(main);
                Err(err)
            }
        },
        // 主文件不存在：有 .recovered 但解不开时把它的错误带出去，否则就是干净的 Miss。
        None => match recovered_error {
            Some(err) => Err(err),
            None => Ok(None),
        },
    }
}

/// 读 `world.meta`：`Ok(None)` 只表示文件不存在（新世界）。
///
/// 解码失败**不降级**（§2：唯一不能降级为 `Miss` 的文件）：先按 §5.3 备份
/// `.corrupt`，然后返回 `Err`，由调用方拒绝打开世界。
pub fn read_world_meta(paths: &SavePaths) -> Result<Option<WorldMeta>, SaveError> {
    read_with_recovery(
        paths,
        &paths.world_meta(),
        MAX_WORLD_META_FILE_LEN,
        decode_world_meta,
    )
}

/// 读一个 region：`region_of(key)` 决定文件，返回 `Ok(None)` 即 `Miss`。
///
/// 解码失败时：首次生成 `.corrupt` 备份、本会话之后写 `.recovered`，并返回
/// `Err`（= 单块失败不阻塞其他块，`lookup` 立即给 `Miss`，同时把原因
/// 交给调用方发 `SaveFailed { path, reason }`）。
pub fn read_region(
    paths: &SavePaths,
    body: BodyKey,
    key: ChunkKey,
) -> Result<Option<RegionFile>, SaveError> {
    read_with_recovery(
        paths,
        &paths.region(body, key),
        MAX_REGION_FILE_LEN,
        decode_region,
    )
}

/// 列出某个 body 目录里**可用** region 文件的 region 键（升序、去重）。
///
/// 后缀规则（§1 / §5.3）：
///
/// - `.tmp`：写入中的崩溃残留，跳过；
/// - `.corrupt`：读失败时保留的原字节备份，跳过；
/// - `.recovered`：同名 region 的可用数据（读路径优先它），只计一次；
/// - 其余文件名必须严格是 `region_<x>_<y>_<z>`（有符号十进制、无前导零），
///   解析不出就返回 `Err`，不静默跳过。
///
/// 返回的是 **region 键**（逐轴 `div_euclid(REGION_SIZE)` 的那套），不是锚点块键；
/// 调用方要读文件时必须用 [`SavePaths::region`]（或传锚点块键给 [`read_region`]），
/// 它会先做 [`region_origin`]。
///
/// 目录不存在（该 body 没有任何 region）返回空列表；其它 IO 错误返回 `Err`。
pub fn regions_on_disk(paths: &SavePaths, body: BodyKey) -> Result<Vec<ChunkKey>, SaveError> {
    let dir = paths.body_dir(body);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(io_error(fs_atomic::STAGE_READ, &dir, err)),
    };

    let mut regions = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|err| io_error(fs_atomic::STAGE_READ, &dir, err))?;
        let file_name = entry.file_name();
        let path = dir.join(&file_name);
        let Some(name) = file_name.to_str() else {
            return Err(invalid_region_name(&path));
        };
        if name.ends_with(".tmp") || name.ends_with(".corrupt") {
            continue;
        }
        let key = parse_region_file_name(name).ok_or_else(|| invalid_region_name(&path))?;
        regions.insert(key);
    }
    Ok(regions.into_iter().collect())
}

/// 一次回收删掉的东西。用于日志与测试断言。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReclaimStats {
    /// 处理掉的 body 目录数（每个 [`reclaim_body`] 计 1）。
    ///
    /// 正常情况下目录会被删掉；若删完文件后目录又被并发写入、暂时删不掉，
    /// 仍计 1 且不报错（下一次回收会继续清）。
    pub bodies: usize,
    /// 删掉的 region 键数，**按目录里数出来的键去重**：同一个 region 的主文件与
    /// `.recovered` 只算一个；`.tmp` / `.corrupt` 以及其它非 region 文件照样会被
    /// 删掉，但不贡献计数（它们不是任何一个 region 的当前数据）。
    pub regions: usize,
}

/// 列出存档根目录下**不在** `meta.bodies` 里的 body 目录（按 body ID 升序）。
///
/// 孤儿目录是崩溃（body 记录还没写进 `world.meta`，region 已经落盘）、
/// `WipeWorld`（清空 body 表但没删目录）或换存档（复用同一个 root）之后的正常
/// 残留，不是损坏。本函数只枚举，**不删任何东西**；删不删、什么时候删由调用方
/// 决定（[`reclaim_orphans`] 是“全清”的那条路）。
///
/// 只认目录，且目录名必须能 `parse::<u64>()` 成 body ID。规范名是无符号十进制、
/// 无前导零（见 [`SavePaths::body_dir`]），但 `parse` 会接受 `007` 这类前导零
/// 写法并按 7 处理：它们指向的是同一个 body，没必要为了写法差异把目录永远留着。
/// 其它名字（`world.meta` 这类文件、`backup` 目录）静默跳过——它们不归存档
/// 布局管，报错只会让启动流程卡在一个无关文件上。
///
/// 根目录不存在 = 还没有任何存档，返回空列表；其它 IO 错误带阶段返回。
pub fn orphan_bodies(paths: &SavePaths, meta: &WorldMeta) -> Result<Vec<BodyKey>, SaveError> {
    let root = paths.root();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(io_error(fs_atomic::STAGE_READ, root, err)),
    };

    // BTreeSet：去重（`007` 与 `7` 同一 body）并按 BodyKey（= u64）升序输出。
    let mut orphans = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|err| io_error(fs_atomic::STAGE_READ, root, err))?;
        let path = entry.path();
        // 只认目录：`world.meta`、`.corrupt` 之类的普通文件直接跳过。
        let file_type = entry
            .file_type()
            .map_err(|err| io_error(fs_atomic::STAGE_READ, &path, err))?;
        if !file_type.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(id) = name.parse::<u64>() else {
            continue;
        };
        let body = BodyKey::new(StableEntityId(id));
        if meta.bodies.contains_key(&body) {
            continue;
        }
        orphans.insert(body);
    }
    Ok(orphans.into_iter().collect())
}

/// 回收一个 body 目录：删掉目录里的全部文件，空了再把目录也删掉。
///
/// 这个目录整体属于一个 `world.meta` 里不存在的 body（见 [`orphan_bodies`]），
/// 所以删除范围是**目录里的所有普通文件**，包括 `.tmp` 与 `.corrupt`。`.corrupt`
/// 的取证价值只在 body 仍然存在、玩家还可能被告知“这一块坏了”时成立；孤儿目录里
/// 留着它只会让目录永远删不掉。
///
/// 顺序是**先列出、先统计，再做任何删除**：目录里出现子目录属于布局违规（规格只
/// 允许平铺的 region 文件），直接返回语义错误且一个文件都不删；否则删到一半才发现
/// 会留下半清理状态。统计的 `regions` 是“目录里数出来的 region 键数”，不是删成功
/// 的文件数——删除一旦失败就 `Err` 返回，调用方按整次回收失败处理，不需要部分
/// 成功的计数。
///
/// 删完后目录为空才 `remove_dir` 并 fsync 根目录；若还有残余（并发写入）不报错，
/// 目录留在原地下一次再清。目录不存在 = 已经没有可回收的东西，返回全 0。
pub fn reclaim_body(paths: &SavePaths, body: BodyKey) -> Result<ReclaimStats, SaveError> {
    let dir = paths.body_dir(body);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(ReclaimStats::default()),
        Err(err) => return Err(io_error(fs_atomic::STAGE_READ, &dir, err)),
    };

    // 第一遍只收集：子目录要在动手前就发现，避免删到一半才报错。
    let mut files = Vec::new();
    let mut regions = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|err| io_error(fs_atomic::STAGE_READ, &dir, err))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| io_error(fs_atomic::STAGE_READ, &path, err))?;
        if file_type.is_dir() {
            return Err(SaveError::semantic(
                path.display().to_string(),
                SaveErrorKind::InvalidValue {
                    what: "orphan body dir contains a subdirectory",
                    value: 0,
                },
            ));
        }
        // 主文件与 .recovered 解析出同一个键，BTreeSet 自动去重；.tmp/.corrupt 解析
        // 不出键，不计数（但下面照样删）。
        if let Some(name) = entry.file_name().to_str() {
            if let Some(key) = parse_region_file_name(name) {
                regions.insert(key);
            }
        }
        files.push(path);
    }

    // 第二遍才是删除。并发的“别人已经删了”不算错（与 remove_region 一致）。
    for file in &files {
        match std::fs::remove_file(file) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_error(fs_atomic::STAGE_REMOVE, file, err)),
        }
    }

    // 再看一次：真的空了才删目录；有残余不报错，下一次回收接着清。
    let empty = match std::fs::read_dir(&dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => true,
        Err(err) => return Err(io_error(fs_atomic::STAGE_READ, &dir, err)),
    };
    if empty {
        match std::fs::remove_dir(&dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_error(fs_atomic::STAGE_REMOVE, &dir, err)),
        }
        fs_atomic::sync_dir(paths.root())?;
    }

    Ok(ReclaimStats {
        bodies: 1,
        regions: regions.len(),
    })
}

/// 回收**所有**孤儿 body 目录（[`orphan_bodies`] 的判定 + 逐个 [`reclaim_body`]）。
///
/// 统计按 body 累加。任何一个目录回收失败都整体返回 `Err`：宁可让调用方看到
/// “这次没清完”，也不要把半清理状态当成成功。
pub fn reclaim_orphans(paths: &SavePaths, meta: &WorldMeta) -> Result<ReclaimStats, SaveError> {
    let mut total = ReclaimStats::default();
    for body in orphan_bodies(paths, meta)? {
        let stats = reclaim_body(paths, body)?;
        total.bodies += stats.bodies;
        total.regions += stats.regions;
    }
    Ok(total)
}

/// 原子写一个 region 文件，返回**实际**写到的路径（可能是 `.recovered`）。
///
/// 顺序：先编码（内存里完整字节），再按 [`SavePaths::write_target`] 选目标，
/// 最后 [`write_atomic`]（tmp + fsync + rename + fsync 目录）。
///
/// 调用方契约：`file.chunks` 里的每个 `ChunkKey` 都必须满足
/// `region_of(key) == region_origin(key)`（§1）；违反是调用方 bug，debug 下断言。
pub fn write_region(
    paths: &SavePaths,
    body: BodyKey,
    key: ChunkKey,
    file: &RegionFile,
) -> Result<PathBuf, SaveError> {
    let expected = region_origin(key);
    debug_assert!(
        file.chunks
            .keys()
            .all(|chunk| region_of(*chunk) == expected),
        "region 文件里出现了不属于 {expected:?} 的 ChunkKey（§1）"
    );

    let bytes = encode_region(file)?;
    write_encoded(paths, &paths.region(body, key), &bytes)
}

/// 删除一个已经被清空的 region 文件，返回主文件路径。
///
/// 只在墓碑落盘、且删除该块后 `chunks` 为空时调用：这个 region 不再有任何记录，
/// 留着空文件只会让下一次读多一次 IO。`.recovered` 是读路径的首选，只删主文件
/// 会让旧内容在下次读取时“复活”，所以两份都删；`.corrupt` 是读失败的原始字节
/// 备份，按 §5.3 保留不动。
///
/// 文件不存在不算错（可能已被另一个 store 删掉），其余 IO 失败带阶段返回，
/// 调用方保留墓碑下次重试。成功后 fsync 父目录，让删除本身持久化。
pub(crate) fn remove_region(
    paths: &SavePaths,
    body: BodyKey,
    key: ChunkKey,
) -> Result<PathBuf, SaveError> {
    let main = paths.region(body, key);
    let recovered = paths.recovered(&main);
    for file in [&main, &recovered] {
        match std::fs::remove_file(file) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_error(fs_atomic::STAGE_REMOVE, file, err)),
        }
    }
    let dir = paths.body_dir(body);
    if dir.is_dir() {
        fs_atomic::sync_dir(&dir)?;
    }
    Ok(main)
}

/// 删除一个空的 body 目录，返回是否真的删掉了。
///
/// 空目录本身无害：[`write_atomic`] 每次写入都会 `create_dir_all`（fs_atomic.rs:115），
/// 所以删不删都不影响正确性。这里清它只是免得存档树里堆一堆“region 删光了、
/// 目录还在”的空壳，人工排查时误以为这个 body 还占着盘。
///
/// 只有目录确实为空才 `remove_dir` 并 fsync 根目录；非空（还有别的 region，或
/// `.tmp` / `.corrupt` 残留）原样保留、返回 `false`。目录不存在同样返回
/// `false`——没有东西可删，也不算失败。
pub(crate) fn prune_empty_body_dir(paths: &SavePaths, body: BodyKey) -> Result<bool, SaveError> {
    let dir = paths.body_dir(body);
    let mut entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(io_error(fs_atomic::STAGE_READ, &dir, err)),
    };
    if entries.next().is_some() {
        return Ok(false);
    }
    match std::fs::remove_dir(&dir) {
        Ok(()) => {}
        // 并发：目录已经不在了，目的已经达到，不报错。
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        // 并发：读完之后又有人写入，目录不再为空，留给下一次。
        Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => return Ok(false),
        Err(err) => return Err(io_error(fs_atomic::STAGE_REMOVE, &dir, err)),
    }
    fs_atomic::sync_dir(paths.root())?;
    Ok(true)
}

/// 原子写 `world.meta`，返回实际写到的路径（§5.1 的世界级顺序：meta 最后写）。
pub fn write_world_meta(paths: &SavePaths, meta: &WorldMeta) -> Result<PathBuf, SaveError> {
    let bytes = encode_world_meta(meta)?;
    write_encoded(paths, &paths.world_meta(), &bytes)
}

/// 选目标并原子写：quarantine 的文件直接拒绝（§5.3），其余走 [`SavePaths::write_target`]。
fn write_encoded(paths: &SavePaths, main: &Path, bytes: &[u8]) -> Result<PathBuf, SaveError> {
    if paths.is_quarantined(main) {
        return Err(quarantined_error(main));
    }
    let target = paths.write_target(main);
    write_atomic(&target, bytes)?;
    Ok(target)
}
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::identity::StableEntityId;
    use crate::save::codec::encode_region;
    use crate::save::{AttachTag, BodyKind, BodyRecord};
    use crate::streaming::ChunkRecord;

    use super::*;

    /// `std::env::temp_dir()` 下的唯一目录；`Drop` 时整棵删掉（不加 tempfile 依赖）。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let seq = NEXT.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |delta| delta.as_nanos());
            let path = std::env::temp_dir().join(format!(
                "game-engine-save-store-{}-{nanos}-{seq}-{label}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("创建测试临时目录");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn paths(&self) -> SavePaths {
            SavePaths::new(self.path())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn body(id: u64) -> BodyKey {
        BodyKey::new(StableEntityId(id))
    }

    fn key(x: i32, y: i32, z: i32) -> ChunkKey {
        ChunkKey::new(x, y, z)
    }

    /// 只含给定 body 的 meta；其余字段与本组测试无关，填最小值。
    fn meta_with(bodies: &[BodyKey]) -> WorldMeta {
        let mut table = BTreeMap::new();
        for body in bodies {
            table.insert(
                *body,
                BodyRecord {
                    id: *body,
                    kind: BodyKind::Island,
                    attach: AttachTag::World,
                    biome: 0,
                    anchored: 0,
                    voxel_size: 1,
                    drift: [0; 3],
                    attach_to: BodyKey::INVALID,
                },
            );
        }
        WorldMeta {
            seed: 0,
            world_tick: 0,
            next_stable_entity_id: 0,
            bodies: table,
            mods: Vec::new(),
            params: Vec::new(),
            actions: Vec::new(),
        }
    }

    /// 带一块 `content` 的 region；`marker` 让不同版本字节不同。
    fn region(marker: u8) -> RegionFile {
        let mut chunks = BTreeMap::new();
        chunks.insert(
            key(0, 0, 0),
            ChunkRecord {
                generator_version: 3,
                content: Some(vec![marker; 8]),
                proposals: Vec::new(),
                entities: Vec::new(),
            },
        );
        RegionFile { chunks }
    }

    /// 翻掉尾摘要的最后一个字节：结构合法、摘要不符（§1.1 的纠错用例）。
    fn flip_last_byte(path: &Path) {
        let mut bytes = fs::read(path).expect("读文件");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        fs::write(path, bytes).expect("写回文件");
    }

    #[test]
    fn write_atomic_creates_file_and_leaves_no_tmp() {
        let dir = TempDir::new("atomic-create");
        let target = dir.path().join("nested/deeper/world.meta");

        write_atomic(&target, b"hello").expect("原子写必须成功");

        assert_eq!(fs::read(&target).expect("读回"), b"hello");
        assert!(target.is_file(), "父目录必须逐级创建");
        assert!(!dir.paths().tmp(&target).exists(), "写后不能残留 .tmp");
    }

    #[test]
    fn write_atomic_overwrites_existing_file() {
        let dir = TempDir::new("atomic-overwrite");
        let target = dir.path().join("region_0_0_0");

        write_atomic(&target, b"old").expect("首次写");
        write_atomic(&target, b"new-and-longer").expect("覆盖写");

        assert_eq!(fs::read(&target).expect("读回"), b"new-and-longer");
        assert!(!dir.paths().tmp(&target).exists(), "覆盖后不能残留 .tmp");
    }

    #[test]
    fn write_atomic_failure_keeps_existing_file_and_cleans_tmp() {
        let dir = TempDir::new("atomic-fail");
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"keep").expect("占位文件");
        // 父路径是普通文件：create_dir_all 必失败，目标路径不可写。
        let target = blocker.join("region_0_0_0");

        let err = write_atomic(&target, b"data").expect_err("父目录建不出来必须失败");

        assert!(
            matches!(err.kind(), SaveErrorKind::Io { stage, .. } if *stage == "mkdir"),
            "必须是 mkdir 阶段: {err}"
        );
        assert_eq!(fs::read(&blocker).expect("原文件"), b"keep");
        assert!(!dir.paths().tmp(&target).exists(), "失败要清理 tmp");
    }

    #[test]
    fn stale_tmp_from_crash_is_replaced_on_write() {
        let dir = TempDir::new("stale-tmp");
        let paths = dir.paths();
        let target = paths.world_meta();
        // 模拟“写了 tmp、没来得及 rename 就被杀”：陈旧 tmp 必须被重建，不能卡住写入。
        fs::write(paths.tmp(&target), b"half-written garbage").expect("残留 tmp");

        write_atomic(&target, b"complete").expect("残留 tmp 不能阻塞原子写");

        assert_eq!(fs::read(&target).expect("读回"), b"complete");
        assert!(!paths.tmp(&target).exists());
    }

    #[test]
    fn read_region_ignores_stale_tmp() {
        let dir = TempDir::new("tmp-ignored");
        let paths = dir.paths();
        let b = body(3);
        let k = key(0, 0, 0);
        let main = paths.region(b, k);
        fs::create_dir_all(main.parent().expect("body 目录")).expect("建目录");

        // 只有 tmp（从未 rename）：读路径必须当它不存在。
        let pending = encode_region(&region(9)).expect("编码");
        fs::write(paths.tmp(&main), &pending).expect("写 tmp");
        assert!(
            read_region(&paths, b, k).expect("读").is_none(),
            "tmp 不是数据，必须 Miss"
        );

        // 旧主文件 + 更新的 tmp：读到的仍是旧内容，不能把 tmp 当新数据。
        let old = region(1);
        write_region(&paths, b, k, &old).expect("写旧 region");
        fs::write(paths.tmp(&main), &pending).expect("写更新的 tmp");
        assert_eq!(
            read_region(&paths, b, k).expect("读").expect("命中"),
            old,
            "读路径不能把 tmp 当成新版本"
        );
        assert!(
            paths.tmp(&main).exists(),
            "读路径不做清理（启动时才删 tmp）"
        );

        // 残留 tmp 也不阻塞后续原子写。
        let new = region(2);
        write_region(&paths, b, k, &new).expect("覆盖写");
        assert_eq!(read_region(&paths, b, k).expect("读").expect("命中"), new);
        assert!(!paths.tmp(&main).exists(), "写完不能残留 tmp");
    }

    #[test]
    fn corrupt_main_creates_corrupt_copy_and_misses() {
        let dir = TempDir::new("corrupt-backup");
        let paths = dir.paths();
        let b = body(1);
        let k = key(0, 0, 0);
        let main = write_region(&paths, b, k, &region(0xAA)).expect("写 region");
        flip_last_byte(&main);
        let broken = fs::read(&main).expect("读损坏文件");

        let err = read_region(&paths, b, k).expect_err("损坏必须读失败");
        assert!(
            matches!(err.kind(), SaveErrorKind::DigestMismatch { .. }),
            "篡改尾摘要应报 DigestMismatch: {err}"
        );

        let corrupt = paths.corrupt(&main);
        assert!(corrupt.is_file(), "首次读失败必须生成 .corrupt");
        assert_eq!(
            fs::read(&corrupt).expect("读 .corrupt"),
            broken,
            ".corrupt 必须是损坏的原始字节"
        );
        assert_eq!(fs::read(&main).expect("主文件"), broken, "读路径不改主文件");
        assert!(
            !paths.recovered(&main).exists(),
            "只读失败不能写 .recovered"
        );

        // 再读一次：仍然失败，.corrupt 不变。
        assert!(read_region(&paths, b, k).is_err());
        assert_eq!(fs::read(&corrupt).expect("读 .corrupt"), broken);
    }

    #[test]
    fn write_after_corruption_targets_recovered_and_keeps_main() {
        let dir = TempDir::new("recovered-target");
        let paths = dir.paths();
        let b = body(1);
        let k = key(0, 0, 0);
        let main = write_region(&paths, b, k, &region(0xAA)).expect("写 region");
        flip_last_byte(&main);
        let broken = fs::read(&main).expect("读损坏文件");

        assert!(read_region(&paths, b, k).is_err(), "先让主文件读失败");
        assert_eq!(paths.write_target(&main), paths.recovered(&main));

        let fresh = region(0x55);
        let target = write_region(&paths, b, k, &fresh).expect("写 .recovered");

        assert_eq!(target, paths.recovered(&main), "写目标必须是 .recovered");
        assert!(target.is_file());
        assert_eq!(
            fs::read(&main).expect("主文件"),
            broken,
            "主文件字节必须不变"
        );
        assert_eq!(
            read_region(&paths, b, k).expect("读").expect("命中"),
            fresh,
            ".recovered 优先"
        );
        assert!(!paths.tmp(&target).exists(), "写完不能残留 tmp");
    }

    #[test]
    fn replaced_is_sticky_across_sessions_and_survives_main_repair() {
        let dir = TempDir::new("recovered-sticky");
        let paths = dir.paths();
        let b = body(2);
        let k = key(0, 0, 0);
        let main = write_region(&paths, b, k, &region(0xAA)).expect("写 region");
        flip_last_byte(&main);
        assert!(read_region(&paths, b, k).is_err());

        let recovered_content = region(0x55);
        write_region(&paths, b, k, &recovered_content).expect("写 .recovered");

        // 新会话（新的 SavePaths）：.recovered 存在 → 仍然是它。
        let paths = SavePaths::new(dir.path());
        assert_eq!(paths.write_target(&main), paths.recovered(&main));

        // 把主文件“修好”成合法但更旧的内容：读与写都不回主文件。
        let repaired = region(0x11);
        fs::write(&main, encode_region(&repaired).expect("编码")).expect("修好主文件");
        let repaired_bytes = fs::read(&main).expect("读主文件");

        assert_eq!(
            read_region(&paths, b, k).expect("读").expect("命中"),
            recovered_content,
            "粘性：主文件修好也不回主文件"
        );
        let target = write_region(&paths, b, k, &region(0x22)).expect("写");
        assert_eq!(target, paths.recovered(&main));
        assert_eq!(
            fs::read(&main).expect("主文件"),
            repaired_bytes,
            "粘性期间主文件仍不被改写"
        );

        // §9：手动删 .recovered 是回到主文件的唯一路径。
        fs::remove_file(paths.recovered(&main)).expect("删 .recovered");
        let cleaned = SavePaths::new(dir.path());
        assert_eq!(cleaned.write_target(&main), main);
        assert_eq!(
            read_region(&cleaned, b, k).expect("读").expect("命中"),
            repaired
        );
    }

    #[test]
    fn corrupt_copy_keeps_first_snapshot() {
        let dir = TempDir::new("corrupt-first");
        let paths = dir.paths();
        let b = body(4);
        let k = key(0, 0, 0);
        let main = write_region(&paths, b, k, &region(1)).expect("写 region");
        flip_last_byte(&main);
        let first_broken = fs::read(&main).expect("读损坏文件");
        assert!(read_region(&paths, b, k).is_err());
        let corrupt = paths.corrupt(&main);
        assert_eq!(fs::read(&corrupt).expect("读 .corrupt"), first_broken);

        // 第二次损坏（换一个字节）：.corrupt 保持第一份，不覆盖、不删除。
        let mut bytes = fs::read(&main).expect("读主文件");
        bytes[8] ^= 0xFF;
        fs::write(&main, &bytes).expect("二次损坏");
        assert!(read_region(&paths, b, k).is_err());
        assert_eq!(
            fs::read(&corrupt).expect("读 .corrupt"),
            first_broken,
            ".corrupt 只留第一份"
        );
    }

    #[test]
    fn corrupt_copy_failure_quarantines_writes() {
        let dir = TempDir::new("quarantine");
        let paths = dir.paths();
        let b = body(5);
        let k = key(0, 0, 0);
        let main = write_region(&paths, b, k, &region(1)).expect("写 region");
        flip_last_byte(&main);
        let broken = fs::read(&main).expect("读损坏文件");
        // 让 .corrupt 的位置是个目录：复制时的 rename 必失败。
        fs::create_dir_all(paths.corrupt(&main)).expect("占位目录");

        let err = read_region(&paths, b, k).expect_err("复制失败必须报错");
        assert!(
            matches!(err.kind(), SaveErrorKind::Io { .. }),
            "复制失败是 IO 错误: {err}"
        );
        assert!(paths.is_quarantined(&main), "复制失败要 quarantine 该文件");

        let blocked = write_region(&paths, b, k, &region(2)).expect_err("quarantine 期间拒写");
        assert!(
            matches!(blocked.kind(), SaveErrorKind::Io { stage, .. } if *stage == "quarantine"),
            "拒写错误阶段是 quarantine: {blocked}"
        );
        assert_eq!(blocked.path(), main.display().to_string());
        assert_eq!(
            fs::read(&main).expect("主文件"),
            broken,
            "拒写不能覆盖主文件"
        );
        assert!(
            !paths.recovered(&main).exists(),
            "quarantine 时不能悄悄降级写 .recovered"
        );
    }

    #[test]
    fn region_path_layout_is_decimal_body_and_euclidean_region() {
        let paths = SavePaths::new("/saves/w");
        let b = body(42);

        assert_eq!(paths.world_meta(), PathBuf::from("/saves/w/world.meta"));
        assert_eq!(
            paths.region(b, key(1, 2, 3)),
            PathBuf::from("/saves/w/42/region_0_0_0")
        );
        // 负坐标：div_euclid(4)，文件名有符号十进制（§1 的 region_-1_0_2 写法）。
        assert_eq!(
            paths.region(b, key(-1, 0, 8)),
            PathBuf::from("/saves/w/42/region_-1_0_2")
        );
        assert_eq!(paths.body_dir(b).file_name().expect("文件名"), "42");
        assert_eq!(
            paths.body_dir(body(0)).file_name().expect("文件名"),
            "0",
            "body 目录是十进制、无前导零"
        );

        assert_eq!(region_origin(key(-1, 0, 8)), key(-1, 0, 2));
        assert_eq!(region_origin(key(-5, 7, 0)), key(-2, 1, 0));
        assert_eq!(region_origin(key(4, -1, -4)), key(1, -1, -1));
    }

    #[test]
    fn empty_region_round_trip_creates_body_dir() {
        let dir = TempDir::new("empty-region");
        let paths = dir.paths();
        let b = body(7);
        let k = key(0, 0, 0);
        assert!(!paths.body_dir(b).exists(), "写之前不能有 body 目录");

        let target = write_region(&paths, b, k, &RegionFile::default()).expect("写空 region");

        assert_eq!(target, paths.region(b, k));
        assert!(paths.body_dir(b).is_dir(), "body 目录必须自动创建");
        let read = read_region(&paths, b, k)
            .expect("读空 region")
            .expect("chunk_count = 0 是合法文件");
        assert!(read.is_empty());
        assert_eq!(read, RegionFile::default());

        assert!(
            read_region(&paths, b, key(4, 0, 0))
                .expect("读别的 region")
                .is_none(),
            "同 body 别的 region 仍是 Miss"
        );
        assert!(
            read_region(&paths, body(8), k)
                .expect("读空 body")
                .is_none(),
            "没有 body 目录 = Miss"
        );
    }

    #[test]
    fn world_meta_round_trip_and_missing_is_none() {
        let dir = TempDir::new("world-meta");
        let paths = dir.paths();
        assert!(
            read_world_meta(&paths).expect("读不存在的 meta").is_none(),
            "文件不存在 = 新世界"
        );

        let meta = WorldMeta {
            seed: 0x0123_4567_89AB_CDEF,
            world_tick: 99,
            next_stable_entity_id: 7,
            bodies: BTreeMap::new(),
            mods: Vec::new(),
            params: Vec::new(),
            actions: Vec::new(),
        };
        let target = write_world_meta(&paths, &meta).expect("写 world.meta");

        assert_eq!(target, paths.world_meta());
        assert_eq!(read_world_meta(&paths).expect("读回").expect("命中"), meta);
        assert!(!paths.tmp(&target).exists());
    }

    #[test]
    fn corrupt_world_meta_is_never_a_miss() {
        let dir = TempDir::new("world-meta-corrupt");
        let paths = dir.paths();
        let meta = WorldMeta::default();
        write_world_meta(&paths, &meta).expect("写 world.meta");
        let main = paths.world_meta();
        flip_last_byte(&main);
        let broken = fs::read(&main).expect("读损坏 meta");

        let err = read_world_meta(&paths).expect_err("world.meta 损坏不能降级");
        assert!(
            matches!(err.kind(), SaveErrorKind::DigestMismatch { .. }),
            "{err}"
        );
        assert!(paths.corrupt(&main).is_file(), "首次失败要留 .corrupt");
        assert_eq!(fs::read(paths.corrupt(&main)).expect("读 .corrupt"), broken);
        assert!(
            read_world_meta(&paths).is_err(),
            "§2：world.meta 是唯一不能降级为 Miss 的文件"
        );
    }

    #[test]
    fn backup_paths_append_suffix_to_file_name() {
        let paths = SavePaths::new("/saves/w");
        let main = paths.region(body(1), key(0, 0, 0));

        assert_eq!(
            paths.corrupt(&main).file_name().expect("文件名"),
            "region_0_0_0.corrupt"
        );
        assert_eq!(
            paths.recovered(&main).file_name().expect("文件名"),
            "region_0_0_0.recovered"
        );
        assert_eq!(
            paths.tmp(&main).file_name().expect("文件名"),
            "region_0_0_0.tmp"
        );
        assert_eq!(paths.corrupt(&main).parent(), main.parent());
        assert_eq!(
            paths
                .corrupt(&paths.world_meta())
                .file_name()
                .expect("文件名"),
            "world.meta.corrupt",
            "追加后缀，不能替换扩展名"
        );
    }

    #[test]
    fn regions_on_disk_sorts_dedups_and_skips_suffixes() {
        let dir = TempDir::new("regions-list");
        let paths = dir.paths();
        let b = body(9);

        let first = write_region(&paths, b, key(0, 0, 0), &region(1)).expect("写 region 0");
        let second =
            write_region(&paths, b, key(5, 0, 0), &RegionFile::default()).expect("写 region 1");

        // 同 region 的 .recovered：是可用数据，但只算一次。
        let recovered_bytes = encode_region(&region(2)).expect("编码");
        fs::write(paths.recovered(&first), &recovered_bytes).expect("写 .recovered");

        // 只有 .recovered、没有主文件的 region：仍要枚举出来。
        let only_recovered = paths.region(b, key(8, 0, 0));
        fs::create_dir_all(only_recovered.parent().expect("body 目录")).expect("建目录");
        fs::write(
            paths.recovered(&only_recovered),
            encode_region(&RegionFile::default()).expect("编码"),
        )
        .expect("写 .recovered");

        // 崩溃残留与损坏备份：即使文件名主体非法也必须跳过，不能报错。
        fs::write(paths.tmp(&second), b"tmp").expect("写 .tmp");
        fs::write(paths.corrupt(&second), b"corrupt").expect("写 .corrupt");
        fs::write(paths.body_dir(b).join("region_not_a_key.tmp"), b"tmp").expect("写非法 .tmp");
        fs::write(
            paths.body_dir(b).join("region_not_a_key.corrupt"),
            b"corrupt",
        )
        .expect("写非法 .corrupt");

        assert_eq!(
            regions_on_disk(&paths, b).expect("枚举 region"),
            vec![key(0, 0, 0), key(1, 0, 0), key(2, 0, 0)],
            "必须按 region 键升序、去重，并跳过 .tmp/.corrupt"
        );
    }

    #[test]
    fn regions_on_disk_missing_dir_is_empty() {
        let dir = TempDir::new("regions-missing");
        let paths = dir.paths();

        assert!(regions_on_disk(&paths, body(1))
            .expect("缺目录 = 空")
            .is_empty());
    }

    #[test]
    fn regions_on_disk_rejects_malformed_name() {
        for name in [
            "bogus",
            "region_0_0",
            "region_+1_0_0",
            "region_01_0_0",
            "region_0_0_0.recovered.bak",
        ] {
            let dir = TempDir::new("regions-bad-name");
            let paths = dir.paths();
            let b = body(3);
            let body_dir = paths.body_dir(b);
            fs::create_dir_all(&body_dir).expect("建目录");
            fs::write(body_dir.join(name), b"x").expect("写文件");

            let err = regions_on_disk(&paths, b).expect_err("非法文件名必须报错");
            assert!(
                matches!(
                    err.kind(),
                    SaveErrorKind::InvalidValue { what, .. } if *what == "region file name"
                ),
                "{name}: {err}"
            );
        }
    }

    #[test]
    fn orphan_bodies_finds_unknown_dirs() {
        let dir = TempDir::new("orphans-find");
        let paths = dir.paths();
        let kept = body(5);
        // 故意按 20 / 3 的顺序写，断言输出按 body ID 升序。
        let orphan_high = body(20);
        let orphan_low = body(3);
        write_region(&paths, kept, key(0, 0, 0), &region(1)).expect("写 kept");
        write_region(&paths, orphan_high, key(0, 0, 0), &region(2)).expect("写 20");
        write_region(&paths, orphan_low, key(0, 0, 0), &region(3)).expect("写 3");

        assert_eq!(
            orphan_bodies(&paths, &meta_with(&[kept])).expect("枚举孤儿"),
            vec![orphan_low, orphan_high],
            "只返回 meta 里没有的目录，且按 ID 升序"
        );
    }

    #[test]
    fn orphan_bodies_skips_non_numeric_and_files() {
        let dir = TempDir::new("orphans-skip");
        let paths = dir.paths();
        fs::create_dir_all(dir.path().join("backup")).expect("建 backup 目录");
        fs::create_dir_all(dir.path().join("007")).expect("建前导零目录");
        fs::write(paths.world_meta(), b"not a real meta").expect("写 world.meta 文件");
        write_region(&paths, body(7), key(0, 0, 0), &region(1)).expect("写 body 7");

        // 前导零 007 被 parse 成 7：与 7 目录是同一个 body，去重后只出现一次。
        // backup / world.meta 不是数字目录，静默跳过。
        assert_eq!(
            orphan_bodies(&paths, &meta_with(&[])).expect("枚举孤儿"),
            vec![body(7)]
        );

        // 7 已经在 body 表里时，007 也不算孤儿（同一个 body）。
        assert!(orphan_bodies(&paths, &meta_with(&[body(7)]))
            .expect("枚举孤儿")
            .is_empty());
    }

    #[test]
    fn reclaim_body_deletes_everything_and_dir() {
        let dir = TempDir::new("reclaim-all");
        let paths = dir.paths();
        let b = body(3);
        let main = write_region(&paths, b, key(0, 0, 0), &region(1)).expect("写主文件");
        let second =
            write_region(&paths, b, key(8, 0, 0), &RegionFile::default()).expect("写第二个 region");
        // 主文件 + .recovered 同键：regions 只算一个；.tmp / .corrupt 不计数但照样删。
        fs::write(paths.recovered(&main), b"stale recovered").expect("写 .recovered");
        fs::write(paths.tmp(&main), b"tmp").expect("写 .tmp");
        fs::write(paths.corrupt(&main), b"corrupt").expect("写 .corrupt");

        let stats = reclaim_body(&paths, b).expect("回收 body");

        assert_eq!(
            stats,
            ReclaimStats {
                bodies: 1,
                regions: 2,
            }
        );
        assert!(!paths.body_dir(b).exists(), "跑干净后目录必须删掉");
        assert!(!main.exists() && !second.exists(), "region 文件必须全删");
    }

    #[test]
    fn reclaim_body_missing_dir_is_noop() {
        let dir = TempDir::new("reclaim-missing");
        let paths = dir.paths();

        let stats = reclaim_body(&paths, body(99)).expect("缺目录不是错误");

        assert_eq!(stats, ReclaimStats::default());
        assert!(!paths.body_dir(body(99)).exists(), "不能凭空建目录");
    }

    #[test]
    fn reclaim_body_rejects_subdirectory() {
        let dir = TempDir::new("reclaim-subdir");
        let paths = dir.paths();
        let b = body(4);
        let main = write_region(&paths, b, key(0, 0, 0), &region(1)).expect("写主文件");
        fs::create_dir_all(paths.body_dir(b).join("nested")).expect("建子目录");

        let err = reclaim_body(&paths, b).expect_err("子目录必须报错");

        assert!(
            matches!(
                err.kind(),
                SaveErrorKind::InvalidValue { what, .. }
                    if *what == "orphan body dir contains a subdirectory"
            ),
            "{err}"
        );
        assert!(main.is_file(), "先校验后删：主文件必须还在");
        assert_eq!(
            read_region(&paths, b, key(0, 0, 0))
                .expect("读")
                .expect("命中"),
            region(1),
            "不能删掉一半才报错"
        );
    }

    #[test]
    fn reclaim_orphans_keeps_meta_bodies() {
        let dir = TempDir::new("reclaim-orphans");
        let paths = dir.paths();
        let kept = body(5);
        let orphan = body(6);
        let kept_key = key(0, 0, 0);
        write_region(&paths, kept, kept_key, &region(1)).expect("写 kept");
        write_region(&paths, orphan, kept_key, &region(2)).expect("写 orphan");

        let stats = reclaim_orphans(&paths, &meta_with(&[kept])).expect("回收孤儿");

        assert_eq!(
            stats,
            ReclaimStats {
                bodies: 1,
                regions: 1,
            }
        );
        assert!(!paths.body_dir(orphan).exists(), "孤儿目录必须删掉");
        assert!(paths.body_dir(kept).is_dir(), "meta 里的 body 不能动");
        assert_eq!(
            read_region(&paths, kept, kept_key)
                .expect("读 kept")
                .expect("命中"),
            region(1),
            "meta 里的 region 必须原样可读"
        );
    }

    #[test]
    fn prune_empty_body_dir_only_removes_empty() {
        let dir = TempDir::new("prune-empty");
        let paths = dir.paths();
        fs::create_dir_all(paths.body_dir(body(1))).expect("建空目录");
        write_region(&paths, body(2), key(0, 0, 0), &region(1)).expect("写非空目录");

        assert!(prune_empty_body_dir(&paths, body(1)).expect("删空目录"));
        assert!(!paths.body_dir(body(1)).exists());

        assert!(!prune_empty_body_dir(&paths, body(2)).expect("非空不删"));
        assert!(paths.body_dir(body(2)).is_dir(), "非空目录必须留着");

        assert!(
            !prune_empty_body_dir(&paths, body(3)).expect("缺目录不算错"),
            "目录不存在返回 false"
        );
    }
}
