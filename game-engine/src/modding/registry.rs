//! 通用运行期注册表（mod 数据的确定性基石）。
//!
//! 三条确定性铁律：
//! 1. **名字只在表现层**：逻辑 / 存档 / 网络只看 id；
//! 2. **冲突拒绝**：同 id 不同名 -> 报错并拒绝后注册者；同名不同 id -> 同样拒绝；
//! 3. **遍历排序**：对外只提供按 id 升序的视图，禁止依赖插入 / 哈希顺序；
//! 4. **覆盖优先级显式化**：Registry::override_entry 必须携带显式
//!    OverridePriority，而不是只靠加载顺序的偶然结果。

use std::cmp::Ordering;
use std::collections::HashMap;

/// 能进入注册表的条目。
pub trait RegistryEntry: Clone {
    fn id(&self) -> u32;
    fn name(&self) -> &str;
}

/// 一个「id + 拥有所有权的名字」条目（运行期注册用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedEntry {
    pub id: u32,
    pub name: String,
}

impl NamedEntry {
    pub fn new(id: u32, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
        }
    }
}

impl RegistryEntry for NamedEntry {
    fn id(&self) -> u32 {
        self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
}

impl RegistryEntry for crate::ids::IdEntry {
    fn id(&self) -> u32 {
        self.id
    }
    fn name(&self) -> &str {
        self.name
    }
}

/// 注册冲突。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// 同 id 不同名。
    IdCollision {
        id: u32,
        existing: String,
        incoming: String,
    },
    /// 同名不同 id。
    NameCollision {
        name: String,
        existing_id: u32,
        incoming_id: u32,
    },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::IdCollision {
                id,
                existing,
                incoming,
            } => write!(f, "id {id} 冲突：已有 {existing:?}，又来 {incoming:?}"),
            RegistryError::NameCollision {
                name,
                existing_id,
                incoming_id,
            } => write!(
                f,
                "名字 {name:?} 冲突：已有 id {existing_id}，又来 id {incoming_id}"
            ),
        }
    }
}

impl std::error::Error for RegistryError {}

/// 显式覆盖优先级。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverridePriority {
    /// 核心定义。
    Core,
    /// 按加载序号（拓扑排序后的顺序）。
    LoadOrder(u32),
    /// 显式等级（mod 作者自己声明的强度）。
    Explicit(u32),
}

impl OverridePriority {
    fn rank(self) -> u64 {
        match self {
            OverridePriority::Core => 0,
            OverridePriority::LoadOrder(order) => 1_000_000 + u64::from(order),
            OverridePriority::Explicit(level) => 2_000_000 + u64::from(level),
        }
    }
}

impl PartialOrd for OverridePriority {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OverridePriority {
    fn cmp(&self, other: &Self) -> Ordering {
        self.rank().cmp(&other.rank())
    }
}

/// id 命名空间内的运行期注册表。
pub struct Registry<T: RegistryEntry> {
    by_id: HashMap<u32, T>,
    name_to_id: HashMap<String, u32>,
    priority: HashMap<u32, OverridePriority>,
    /// 注册序号：仅用于诊断，不参与遍历顺序。
    seq: u64,
}

impl<T: RegistryEntry> Default for Registry<T> {
    fn default() -> Self {
        Self {
            by_id: HashMap::new(),
            name_to_id: HashMap::new(),
            priority: HashMap::new(),
            seq: 0,
        }
    }
}

impl<T: RegistryEntry> Registry<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// 冲突即拒绝（不静默覆盖）。同 id 同名视为重复，幂等成功。
    pub fn register(&mut self, entry: T) -> Result<(), RegistryError> {
        let id = entry.id();
        let name = entry.name().to_string();

        if let Some(existing) = self.by_id.get(&id) {
            if existing.name() == name {
                return Ok(());
            }
            return Err(RegistryError::IdCollision {
                id,
                existing: existing.name().to_string(),
                incoming: name,
            });
        }
        if let Some(&existing_id) = self.name_to_id.get(&name) {
            if existing_id != id {
                return Err(RegistryError::NameCollision {
                    name,
                    existing_id,
                    incoming_id: id,
                });
            }
        }

        self.name_to_id.insert(name, id);
        self.by_id.insert(id, entry);
        self.priority.insert(id, OverridePriority::Core);
        self.seq += 1;
        Ok(())
    }

    /// 显式覆盖：只有优先级不低于既有条目时才生效。
    ///
    /// 返回 Ok(Some(old)) 表示已替换，Ok(None) 表示优先级不足而保留旧值。
    pub fn override_entry(
        &mut self,
        entry: T,
        priority: OverridePriority,
    ) -> Result<Option<T>, RegistryError> {
        let id = entry.id();
        let name = entry.name().to_string();

        if let Some(&existing_id) = self.name_to_id.get(&name) {
            if existing_id != id {
                return Err(RegistryError::NameCollision {
                    name,
                    existing_id,
                    incoming_id: id,
                });
            }
        }

        if let Some(old) = self.by_id.get(&id) {
            let old_name = old.name().to_string();
            let old_priority = self
                .priority
                .get(&id)
                .copied()
                .unwrap_or(OverridePriority::Core);
            if priority < old_priority {
                return Ok(None);
            }
            if old_name != name {
                self.name_to_id.remove(&old_name);
            }
        }

        self.name_to_id.insert(name, id);
        self.priority.insert(id, priority);
        self.seq += 1;
        Ok(self.by_id.insert(id, entry))
    }

    pub fn get(&self, id: u32) -> Option<&T> {
        self.by_id.get(&id)
    }

    pub fn get_by_name(&self, name: &str) -> Option<&T> {
        self.name_to_id.get(name).and_then(|id| self.by_id.get(id))
    }

    pub fn contains(&self, id: u32) -> bool {
        self.by_id.contains_key(&id)
    }

    pub fn priority_of(&self, id: u32) -> Option<OverridePriority> {
        self.priority.get(&id).copied()
    }

    /// **确定性遍历**：永远按 id 升序；禁止遍历内部 HashMap。
    pub fn sorted(&self) -> Vec<&T> {
        let mut out: Vec<&T> = self.by_id.values().collect();
        out.sort_by_key(|entry| entry.id());
        out
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub fn clear(&mut self) {
        self.by_id.clear();
        self.name_to_id.clear();
        self.priority.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u32, name: &str) -> NamedEntry {
        NamedEntry::new(id, name)
    }

    #[test]
    fn id_conflict_is_rejected_not_silently_overwritten() {
        let mut registry = Registry::new();
        registry.register(entry(1, "a")).unwrap();
        let err = registry.register(entry(1, "b")).unwrap_err();
        assert!(matches!(err, RegistryError::IdCollision { id: 1, .. }));
        assert!(registry.register(entry(1, "a")).is_ok());
    }

    #[test]
    fn name_conflict_is_rejected() {
        let mut registry = Registry::new();
        registry.register(entry(1, "a")).unwrap();
        let err = registry.register(entry(2, "a")).unwrap_err();
        assert!(matches!(
            err,
            RegistryError::NameCollision {
                existing_id: 1,
                incoming_id: 2,
                ..
            }
        ));
    }

    #[test]
    fn sorted_is_ascending_and_independent_of_insertion_order() {
        let mut a = Registry::new();
        for id in [5u32, 1, 3, 2, 4] {
            a.register(entry(id, format!("n{id}").as_str())).unwrap();
        }
        let mut b = Registry::new();
        for id in [2u32, 4, 1, 5, 3] {
            b.register(entry(id, format!("n{id}").as_str())).unwrap();
        }
        let ids_a: Vec<u32> = a.sorted().iter().map(|e| e.id()).collect();
        let ids_b: Vec<u32> = b.sorted().iter().map(|e| e.id()).collect();
        assert_eq!(ids_a, vec![1, 2, 3, 4, 5]);
        assert_eq!(ids_a, ids_b, "遍历顺序必须只由 id 决定");
    }

    #[test]
    fn override_priority_is_explicit_and_deterministic() {
        let mut registry = Registry::new();
        // 核心定义优先级最低，允许 mod 覆盖。
        registry
            .override_entry(entry(7, "core"), OverridePriority::Core)
            .unwrap();
        let replaced = registry
            .override_entry(entry(7, "mid"), OverridePriority::Explicit(5))
            .unwrap();
        assert!(replaced.is_some());
        // 更低优先级不覆盖。
        let replaced = registry
            .override_entry(entry(7, "low"), OverridePriority::Explicit(1))
            .unwrap();
        assert!(replaced.is_none());
        assert_eq!(registry.get(7).unwrap().name(), "mid");
        // 更高优先级覆盖，且旧名字映射被清理。
        let replaced = registry
            .override_entry(entry(7, "high"), OverridePriority::Explicit(9))
            .unwrap();
        assert!(replaced.is_some());
        assert_eq!(registry.get(7).unwrap().name(), "high");
        assert!(registry.get_by_name("mid").is_none());
        assert!(registry.get_by_name("high").is_some());
    }

    #[test]
    fn three_mods_chained_override_has_reproducible_winner() {
        let mut registry = Registry::new();
        registry
            .override_entry(entry(42, "mod_a"), OverridePriority::LoadOrder(0))
            .unwrap();
        registry
            .override_entry(entry(42, "mod_b"), OverridePriority::LoadOrder(1))
            .unwrap();
        registry
            .override_entry(entry(42, "mod_c"), OverridePriority::LoadOrder(2))
            .unwrap();
        assert_eq!(registry.get(42).unwrap().name(), "mod_c");
        assert_eq!(
            registry.priority_of(42),
            Some(OverridePriority::LoadOrder(2))
        );
    }
}
