//! 全局 ID 分区工具（**中立模块**）。
//!
//! 本模块被逻辑层（`static_data`）与表现层（`presentation::semantics`）**共同**依赖，
//! 因此必须放在两者都能依赖的中立位置，否则会出现「逻辑 -> 表现」的依赖倒挂。
//!
//! ID 分区约定（唯一权威）：
//!
//! | 区间 | 用途 | 谁分配 | 生成方式 |
//! |---|---|---|---|
//! | `0` | 保留（none / 空） | 核心 | 常量 |
//! | `1..=999` | 核心（内置） | 核心 | 手工常量，只增不改 |
//! | `1000..=9999` | mod 显式分配 | mod 作者 | 手工挑选 |
//! | `>= 10000` | mod 名字哈希 | 自动 | [`mod_hash_id`] |
//!
//! **命名空间独立、分区规则共享**：`ActionId` / `PrototypeId` / `ItemTypeId` /
//! `ItemTagId` / `ItemSubCategoryId` / 表现语义 ID 各自是一个命名空间，互不比较、
//! 互不合并；共享的只有这张分区表、[`mod_hash_id`] 实现，以及「名字只在表现层，
//! 逻辑只认 ID」的规则。

/// 一条「id + 稳定名字」的中立表项。
///
/// 表现层的 `SemanticEntry` 与逻辑层的物品标签表 `ITEM_TAG_TABLE` 都复用这个形状。
/// 名字只用于表现层；逻辑 / 存档 / 网络永远只看 `id`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdEntry {
    pub id: u32,
    pub name: &'static str,
}

/// ID 分区。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdPartition {
    /// `0`：保留的「无」。
    None,
    /// `1..=999`：核心。
    Core,
    /// `1000..=9999`：mod 显式分配。
    Mod,
    /// `>= 10000`：mod 名字哈希。
    Hashed,
}

pub const CORE_ID_MAX: u32 = 999;
pub const MOD_ID_MIN: u32 = 1_000;
pub const MOD_ID_MAX: u32 = 9_999;
pub const HASH_ID_MIN: u32 = 10_000;

/// ID 落在哪个分区。
#[inline]
pub const fn partition_of(id: u32) -> IdPartition {
    match id {
        0 => IdPartition::None,
        1..=CORE_ID_MAX => IdPartition::Core,
        MOD_ID_MIN..=MOD_ID_MAX => IdPartition::Mod,
        _ => IdPartition::Hashed,
    }
}

/// 分区名（跨 FFI 用）。
pub fn partition_name(partition: IdPartition) -> &'static str {
    match partition {
        IdPartition::None => "none",
        IdPartition::Core => "core",
        IdPartition::Mod => "mod",
        IdPartition::Hashed => "hashed",
    }
}

/// FNV-1a 32 位哈希（跨平台、确定性、无随机盐）。
pub const fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u32;
        hash = hash.wrapping_mul(0x0100_0193);
        index += 1;
    }
    hash
}

/// mod 名字 -> 确定性 ID（`>= HASH_ID_MIN`，永不落入核心 / mod 显式区间）。
///
/// 约定 `namespaced_name` 带 mod 前缀（如 `"mymod:burning"`），避免不同 mod 撞名。
pub fn mod_hash_id(namespaced_name: &str) -> u32 {
    let span = u32::MAX - HASH_ID_MIN + 1;
    HASH_ID_MIN + (fnv1a32(namespaced_name.as_bytes()) % span)
}

/// 检测同一命名空间内的 ID 冲突。
///
/// [`mod_hash_id`] 只有约 42.9 亿个取值，**不保证无碰撞**；显式 mod 区也由作者
/// 自行协调。任何「名字 -> ID」的注册入口在注册时都必须调用本函数做检测：
///
/// - 同 id **不同名** -> 拒绝后注册者并报错（日志含两个名字），**不得静默覆盖**；
/// - 同 id **同名** -> 视为重复注册。
///
/// 返回按 id 排序后相邻的同 id 条目对。const 表单测与运行时注册共用。
pub fn detect_id_collisions(entries: &[IdEntry]) -> Vec<(IdEntry, IdEntry)> {
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|entry| entry.id);
    let mut out = Vec::new();
    for window in sorted.windows(2) {
        if window[0].id == window[1].id {
            out.push((window[0], window[1]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T1：分区边界（0 / 999 / 1000 / 9999 / 10000）。
    #[test]
    fn id_partition_boundaries() {
        assert_eq!(partition_of(0), IdPartition::None);
        assert_eq!(partition_of(1), IdPartition::Core);
        assert_eq!(partition_of(CORE_ID_MAX), IdPartition::Core);
        assert_eq!(partition_of(MOD_ID_MIN), IdPartition::Mod);
        assert_eq!(partition_of(MOD_ID_MAX), IdPartition::Mod);
        assert_eq!(partition_of(HASH_ID_MIN), IdPartition::Hashed);
        assert_eq!(partition_name(IdPartition::Hashed), "hashed");
    }

    /// T2：mod 哈希确定性且落在 hashed 分区。
    #[test]
    fn mod_hash_is_deterministic_and_above_core() {
        let a = mod_hash_id("mymod:burning");
        let b = mod_hash_id("mymod:burning");
        assert_eq!(a, b, "同一名字必须得到同一 ID");
        assert!(a >= HASH_ID_MIN, "mod 哈希必须落在 hashed 分区");
        assert_eq!(partition_of(a), IdPartition::Hashed);
        assert_ne!(
            mod_hash_id("mymod:burning"),
            mod_hash_id("othermod:burning"),
            "不同 mod 前缀不应撞 ID"
        );
    }

    /// T18：包含重复 id 的条目列表应被检测出冲突对。
    #[test]
    fn id_collision_detected() {
        let entries = [
            IdEntry { id: 1, name: "a" },
            IdEntry { id: 2, name: "b" },
            IdEntry { id: 1, name: "c" },
        ];
        let collisions = detect_id_collisions(&entries);
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].0.id, 1);
        assert_eq!(collisions[0].1.id, 1);
        assert_ne!(collisions[0].0.name, collisions[0].1.name);

        // 无重复则无冲突。
        assert!(detect_id_collisions(&entries[..2]).is_empty());
        // 空列表安全。
        assert!(detect_id_collisions(&[]).is_empty());
    }
}
