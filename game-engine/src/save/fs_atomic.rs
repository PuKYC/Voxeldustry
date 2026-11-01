//! 单文件原子写与原子复制（《存档格式规格》§5.1、§5.3）。
//!
//! 所有落盘文件同一套步骤：
//!
//! 1. 内存里已是完整字节（编码失败根本不到这一层）；
//! 2. 同目录写 `<final>.tmp`：`create_new` → `write_all` → `sync_all()` → 关闭；
//! 3. `rename(tmp, final)`（同目录同文件系统，Unix 上原子覆盖）；
//! 4. 打开父目录 `sync_all()`，让重命名项持久化。
//!
//! 任何一步失败都尽力删除 tmp，**绝不**在最终路径留下半截文件：崩溃窗口里
//! 只有“旧文件”或“完整新文件”两种状态（§5.2 第 2、3 行）。
//!
//! 只做 Unix 语义（rename 覆盖、目录 fsync）；非 Unix 平台按 §5.1 另写分支。
//! 这里不注册 Plugin、不开线程、不加依赖，只用 `std::fs`。

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use super::{SaveError, SaveErrorKind};

/// 失败阶段标记。全部是小写静态串，日志/断言直接可比。
pub(crate) const STAGE_MKDIR: &str = "mkdir";
pub(crate) const STAGE_OPEN: &str = "open";
pub(crate) const STAGE_READ: &str = "read";
pub(crate) const STAGE_WRITE: &str = "write";
pub(crate) const STAGE_SYNC: &str = "sync";
pub(crate) const STAGE_RENAME: &str = "rename";
pub(crate) const STAGE_COPY: &str = "copy";
pub(crate) const STAGE_REMOVE: &str = "remove";
/// 复制失败后的本会话 quarantine：拒绝写该文件（§5.3）。
pub(crate) const STAGE_QUARANTINE: &str = "quarantine";

/// 把 `std::io::Error` 映射成带**路径**与**阶段**的 `SaveError`。
///
/// 路径进 `SaveError::path`，阶段进 `SaveErrorKind::Io { stage, .. }`；
/// 保留 `ErrorKind` 与原始 errno，磁盘满（ENOSPC）之类的失败不会被抹平。
pub(crate) fn io_error(stage: &'static str, path: &Path, err: io::Error) -> SaveError {
    SaveError::new(
        path.display().to_string(),
        0,
        SaveErrorKind::Io {
            stage,
            kind: err.kind(),
            raw: err.raw_os_error(),
        },
    )
}

/// 同目录、追加后缀：`region_0_0_0` + `.tmp` → `region_0_0_0.tmp`。
///
/// 刻意**不**用 `Path::with_extension`：`world.meta` 的后缀必须是
/// `world.meta.corrupt`，不能被替换成 `world.corrupt`。
#[must_use]
pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = match path.file_name() {
        Some(name) => name.to_os_string(),
        // 极端情况（路径以 `..` 或根结尾）：直接在整条路径后追加。
        None => path.as_os_str().to_os_string(),
    };
    name.push(suffix);
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

/// `<final>.tmp`：写入中的临时文件（§1 的 `region_*.tmp`）。
#[must_use]
pub(crate) fn tmp_path(path: &Path) -> PathBuf {
    with_suffix(path, ".tmp")
}

/// 父目录；没有父目录时用当前目录。
fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// `create_new` 打开 tmp；崩溃残留的旧 tmp 先删再建，绝不覆盖目标文件。
fn create_tmp(tmp: &Path) -> Result<File, SaveError> {
    match OpenOptions::new().write(true).create_new(true).open(tmp) {
        Ok(file) => Ok(file),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            // 从未 rename 过的 tmp 不可能是有效档（§5.1 第 6 条），删掉重写。
            fs::remove_file(tmp).map_err(|err| io_error(STAGE_REMOVE, tmp, err))?;
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(tmp)
                .map_err(|err| io_error(STAGE_OPEN, tmp, err))
        }
        Err(err) => Err(io_error(STAGE_OPEN, tmp, err)),
    }
}

/// 打开父目录并 `sync_all()`，让 rename 项持久化（§5.1 第 4 步）。
fn sync_dir(dir: &Path) -> Result<(), SaveError> {
    let handle = File::open(dir).map_err(|err| io_error(STAGE_OPEN, dir, err))?;
    handle
        .sync_all()
        .map_err(|err| io_error(STAGE_SYNC, dir, err))
}

/// 原子写：`tmp` + 文件 fsync + rename + 目录 fsync（§5.1）。
///
/// 父目录逐级 `create_dir_all`。任何一步失败都清理 tmp，目标路径保持原样
/// （要么旧文件，要么完整新文件，不存在半截文件）。
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), SaveError> {
    use std::io::Write as _;

    let dir = parent_dir(path);
    fs::create_dir_all(&dir).map_err(|err| io_error(STAGE_MKDIR, &dir, err))?;

    let tmp = tmp_path(path);
    let result = (|| -> Result<(), SaveError> {
        let mut file = create_tmp(&tmp)?;
        file.write_all(bytes)
            .map_err(|err| io_error(STAGE_WRITE, &tmp, err))?;
        file.sync_all()
            .map_err(|err| io_error(STAGE_SYNC, &tmp, err))?;
        drop(file);
        fs::rename(&tmp, path).map_err(|err| io_error(STAGE_RENAME, path, err))?;
        sync_dir(&dir)
    })();

    if result.is_err() {
        // 尽力清理；rename 成功后 tmp 已不存在，remove 报 NotFound 属正常。
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// 原子复制：源文件 → `<dst>.tmp` → fsync → rename → 目录 fsync（§5.3 的 `.corrupt` 备份）。
///
/// 用于“首次主文件读失败”的逐字节备份；失败时不留下半截 `.corrupt`。
pub(crate) fn copy_atomic(src: &Path, dst: &Path) -> Result<(), SaveError> {
    let dir = parent_dir(dst);
    fs::create_dir_all(&dir).map_err(|err| io_error(STAGE_MKDIR, &dir, err))?;

    let mut input = File::open(src).map_err(|err| io_error(STAGE_OPEN, src, err))?;
    let tmp = tmp_path(dst);
    let result = (|| -> Result<(), SaveError> {
        let mut output = create_tmp(&tmp)?;
        io::copy(&mut input, &mut output).map_err(|err| io_error(STAGE_COPY, &tmp, err))?;
        output
            .sync_all()
            .map_err(|err| io_error(STAGE_SYNC, &tmp, err))?;
        drop(output);
        fs::rename(&tmp, dst).map_err(|err| io_error(STAGE_RENAME, dst, err))?;
        sync_dir(&dir)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
