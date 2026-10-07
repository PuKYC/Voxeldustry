//! 运行时语义注册表（Godot 主线程注册、Bevy 线程读取）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use bevy::prelude::Resource;

use game_engine::modding::{NamedEntry, OverridePriority, Registry};

use super::mod_hash_id;
use super::tables::{domain_table, is_core_domain, SEMANTIC_DOMAINS};

#[derive(Default)]
struct RegistryInner {
    /// domain -> 运行期注册表（对外按 id 升序）。
    domains: HashMap<String, Registry<NamedEntry>>,
}

/// 运行时语义注册表。
///
/// `Arc<Mutex<..>>` 内部共享 + `Clone`：Godot 主线程注册、Bevy 线程读取，
/// 同时作为 Bevy `Resource` 供系统消费。运行时条目优先于核心表同名 id；
/// 未注册的 id 回退到核心表。
#[derive(Resource, Clone, Default)]
pub struct SemanticRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

impl SemanticRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 对 poisoning 采取「继续用」而不是 panic：语义名丢失不应该让游戏崩掉。
    fn lock(&self) -> MutexGuard<'_, RegistryInner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 注册 / 覆盖一条运行时条目：同 `(domain, id)` 已存在则更新名字。
    ///
    /// 返回 `true` 表示新增运行时条目，`false` 表示覆盖已有运行时条目。
    /// 注意：核心表不在运行时表内，因此覆盖核心 id 也算「新增」——
    /// 效果是运行时名字遮蔽核心名（见 [`Self::name_of`]）。
    /// 任意域名都接受（mod 可自定义域）。
    pub fn register(&self, domain: &str, id: u32, name: &str) -> bool {
        let mut inner = self.lock();
        let registry = inner.domains.entry(domain.to_string()).or_default();
        let existed = registry.contains(id);
        // 显式覆盖：同域内后注册者覆盖先注册者（LoadOrder 相同即后者胜）。
        let _ = registry.override_entry(NamedEntry::new(id, name), OverridePriority::LoadOrder(0));
        !existed
    }

    /// 用名字哈希注册（mod 推荐入口），返回生成的 ID。
    pub fn register_named(&self, domain: &str, namespaced_name: &str) -> u32 {
        let id = mod_hash_id(namespaced_name);
        self.register(domain, id, namespaced_name);
        id
    }

    /// 运行时注册的名字（不含核心表）。
    pub fn runtime_name(&self, domain: &str, id: u32) -> Option<String> {
        self.lock()
            .domains
            .get(domain)
            .and_then(|registry| registry.get(id))
            .map(|entry| entry.name.clone())
    }

    /// 某个域运行时注册的全部条目（按 id 升序，确定性）。
    pub fn runtime_entries(&self, domain: &str) -> Vec<(u32, String)> {
        self.lock()
            .domains
            .get(domain)
            .map(|registry| {
                registry
                    .sorted()
                    .into_iter()
                    .map(|entry| (entry.id, entry.name.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 有运行时条目的所有域名（字典序，稳定）。
    pub fn runtime_domains(&self) -> Vec<String> {
        let mut domains: Vec<String> = self.lock().domains.keys().cloned().collect();
        domains.sort();
        domains
    }

    /// 最终名字：运行时优先，其次核心表。
    pub fn name_of(&self, domain: &str, id: u32) -> Option<String> {
        self.runtime_name(domain, id).or_else(|| {
            domain_table(domain)
                .and_then(|table| table.iter().find(|entry| entry.id == id))
                .map(|entry| entry.name.to_string())
        })
    }

    /// 合并核心表 + 运行时注册的完整视图。
    ///
    /// 每项 `(domain, id, name, source)`；`source` 为 `"core"` 或 `"mod"`。
    /// 顺序：核心域按 `SEMANTIC_DOMAINS`，域内先核心后运行时新增；
    /// 非核心域（mod 自定义）追加在最后。
    pub fn merged_entries(&self) -> Vec<(String, u32, String, &'static str)> {
        let mut out: Vec<(String, u32, String, &'static str)> = Vec::new();

        for (domain, table) in SEMANTIC_DOMAINS {
            for entry in table.iter() {
                let name = self
                    .runtime_name(domain, entry.id)
                    .unwrap_or_else(|| entry.name.to_string());
                out.push(((*domain).to_string(), entry.id, name, "core"));
            }
            for (id, name) in self.runtime_entries(domain) {
                if table.iter().any(|entry| entry.id == id) {
                    continue; // 已作为核心 id 的覆盖处理
                }
                out.push(((*domain).to_string(), id, name, "mod"));
            }
        }

        for domain in self
            .runtime_domains()
            .into_iter()
            .filter(|domain| !is_core_domain(domain))
        {
            for (id, name) in self.runtime_entries(&domain) {
                out.push((domain.clone(), id, name, "mod"));
            }
        }

        out
    }

    /// 清空全部运行时注册（核心表不受影响）。
    pub fn clear(&self) {
        self.lock().domains.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::super::HASH_ID_MIN;
    use super::*;

    #[test]
    fn runtime_registration_overrides_and_extends() {
        let registry = SemanticRegistry::new();
        // 核心名可查。
        assert_eq!(registry.name_of("locomotion", 1).as_deref(), Some("walk"));
        // 覆盖核心 id：核心表不在运行时表里，因此算新增，名字立即遮蔽核心名。
        assert!(registry.register("locomotion", 1, "jog"));
        assert_eq!(registry.name_of("locomotion", 1).as_deref(), Some("jog"));
        // 再注册同一个运行时 id 才是覆盖。
        assert!(!registry.register("locomotion", 1, "sprint"));
        assert_eq!(registry.name_of("locomotion", 1).as_deref(), Some("sprint"));
        // 新增 id。
        assert!(registry.register("locomotion", 4242, "dodge"));
        assert_eq!(
            registry.name_of("locomotion", 4242).as_deref(),
            Some("dodge")
        );
    }

    #[test]
    fn register_named_is_hashed_and_idempotent() {
        let registry = SemanticRegistry::new();
        let a = registry.register_named("overlay_tag", "mymod:wet");
        let b = registry.register_named("overlay_tag", "mymod:wet");
        assert_eq!(a, b);
        assert!(a >= HASH_ID_MIN);
        assert_eq!(
            registry.name_of("overlay_tag", a).as_deref(),
            Some("mymod:wet")
        );
    }

    #[test]
    fn merged_view_marks_source_and_keeps_core_order() {
        let registry = SemanticRegistry::new();
        registry.register("locomotion", 1, "jog");
        let id = registry.register_named("mymod", "mymod:custom");
        let merged = registry.merged_entries();

        // 核心域第一个条目 (locomotion, 0) 仍是 core，且覆盖后的名字生效。
        assert_eq!(merged[0].0, "locomotion");
        assert_eq!(merged[0].1, 0);
        assert_eq!(merged[0].3, "core");
        assert!(merged
            .iter()
            .any(|(d, i, n, s)| d == "locomotion" && *i == 1 && n == "jog" && *s == "core"));
        // mod 自定义域追加在最后。
        assert!(merged
            .iter()
            .any(|(d, i, n, s)| d == "mymod" && *i == id && n == "mymod:custom" && *s == "mod"));
    }
}
