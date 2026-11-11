//! 语义动作 ↔ 输入通道位的**机制层**。
//!
//! 具体动作表（名字 / id / 通道位）属于游戏内容，由游戏侧通过
//! [`ActionMap`] 提供；引擎只定义形状与查询的默认实现。
//!
//! 两层分离不可混淆：
//!
//! - [`ActionId`]：语义动作标识，**可无限扩展**（mod 用字符串哈希）；
//! - [`ChannelMask`]：输入通道位，**必须定长 64**（性能 + 确定性 + 位运算友好）。
//!
//! 名字只在表现层/跨边界出现；存档 / 协议 / 回滚 / 玩法逻辑一律只看通道位，
//! 因此改键不影响逻辑。

use bevy::prelude::Resource;
use serde::{Deserialize, Serialize};

/// 语义动作 ID（可扩展；mod 用 FNV1a 哈希生成）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ActionId(pub u32);

/// 输入通道位掩码（定长 64）。
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, Serialize, Deserialize)]
pub struct ChannelMask(pub u64);

/// 通道位上限。
pub const MAX_INPUT_CHANNELS: u32 = 64;

/// 一条动作定义。
///
/// `name` 同时是 Godot `InputMap` 里的动作名，也是唯一的跨边界标识。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ActionDef {
    pub name: &'static str,
    pub action: ActionId,
    pub channel_bit: u32,
}

/// 通道位掩码工具。
impl ChannelMask {
    pub const EMPTY: Self = Self(0);

    /// 第 `index` 位的掩码。越界会被截断到 64 位内（不应发生，调用方先查表）。
    #[inline]
    pub fn bit(index: u32) -> Self {
        debug_assert!(index < MAX_INPUT_CHANNELS, "通道位越界: {index}");
        Self(1u64 << (index % MAX_INPUT_CHANNELS))
    }

    #[inline]
    pub fn insert(&mut self, index: u32) {
        self.0 |= Self::bit(index).0;
    }

    #[inline]
    pub fn remove(&mut self, index: u32) {
        self.0 &= !Self::bit(index).0;
    }

    #[inline]
    pub fn contains(self, index: u32) -> bool {
        self.0 & Self::bit(index).0 != 0
    }

    /// 任意一位命中。
    #[inline]
    pub fn intersects(self, mask: u64) -> bool {
        self.0 & mask != 0
    }

    /// 全部命中。
    #[inline]
    pub fn contains_all(self, mask: u64) -> bool {
        self.0 & mask == mask
    }
}

/// 动作表契约：引擎只通过它把「名字」折成「通道位」。
///
/// 用 ZST + 关联函数，编译期单态化；不是 `dyn`，热路径零开销。
pub trait ActionMap: Send + Sync + 'static {
    /// 单一真相源：动作定义表。
    fn table() -> &'static [ActionDef];

    /// 名字查表（Godot 侧唯一入口）。未知名字返回 `None`（前向兼容，不 panic）。
    ///
    /// 默认线性查找：核心表仅几十条，无需二分。
    fn lookup(name: &str) -> Option<&'static ActionDef> {
        Self::table().iter().find(|def| def.name == name)
    }

    /// 名字 → 通道位序号。
    fn channel_bit_of(name: &str) -> Option<u32> {
        Self::lookup(name).map(|def| def.channel_bit)
    }

    /// 名字 → 单一位掩码。
    fn mask_of_name(name: &str) -> Option<u64> {
        Self::channel_bit_of(name).map(|bit| 1u64 << (bit % MAX_INPUT_CHANNELS))
    }

    /// 批量名字 → 掩码（未知名字被忽略）。
    fn mask_of_names<'a>(names: impl IntoIterator<Item = &'a str>) -> u64 {
        names
            .into_iter()
            .filter_map(Self::mask_of_name)
            .fold(0u64, |acc, mask| acc | mask)
    }

    /// 位序号 → 名字。
    fn name_of_bit(bit: u32) -> Option<&'static str> {
        Self::table()
            .iter()
            .find(|def| def.channel_bit == bit)
            .map(|def| def.name)
    }

    /// 位序号 → 语义动作。
    fn action_of_bit(bit: u32) -> Option<ActionId> {
        Self::table()
            .iter()
            .find(|def| def.channel_bit == bit)
            .map(|def| def.action)
    }

    /// 供 Godot 启动时拉取并校验自己的 `InputMap`：返回 `(name, channel_bit)`。
    fn snapshot() -> Vec<(&'static str, u32)> {
        Self::table()
            .iter()
            .map(|def| (def.name, def.channel_bit))
            .collect()
    }
}

/// 最小空表：不需要任何动作的游戏 / 引擎自测用。
pub struct EmptyActions;

impl ActionMap for EmptyActions {
    fn table() -> &'static [ActionDef] {
        &[]
    }
}

/// mod 动作 ID 的起点（含）。
pub const MOD_ACTION_ID_BASE: u32 = 1000;

/// mod 动作 ID 的终点（含）。1000~9999 留给 mod。
pub const MOD_ACTION_ID_END: u32 = 9999;

/// 动作名允许的最大字节数。
pub const ACTION_NAME_MAX_BYTES: usize = 64;

/// 解析后的全局动作表：core 动作原样保留，mod 动作追加在后。
///
/// 这是「建档时解析一次、随存档冻结」的产物：装配期由
/// [Mods::apply](crate::modding::Mods::apply) 插入为 Resource，输入系统与存档
/// 读档校验都以它为准。
///
/// # 顺序
///
/// 前段是 core 动作，按 [channel_bit](ActionDef::channel_bit) 升序；后段是 mod
/// 动作，按 (mod_id, name) 全局升序分配，因此 mod 的注册顺序不影响结果。
#[derive(Resource, Clone, Debug, Default)]
pub struct ResolvedActions {
    defs: Vec<ActionDef>,
}

impl ResolvedActions {
    /// 把 core 表与各 mod 声明的动作名解析成一张全局表。
    ///
    /// - core 的 id 与通道位原样保留（不得占用 1000..=9999 的 mod 区间）；
    /// - mod 动作不得与 core 或彼此重名；
    /// - mod 动作的 id 从 1000 起按 (mod_id, name) 升序分配，通道位从 0 起取
    ///   第一个未被占用的位。
    ///
    /// mod 名非法、重名、id 或通道位耗尽都返回 [ActionResolveError]。
    pub fn resolve(
        core: &'static [ActionDef],
        mods: &[(&'static str, &'static [&'static str])],
    ) -> Result<Self, ActionResolveError> {
        let mut defs: Vec<ActionDef> = core.to_vec();
        defs.sort_by_key(|def| def.channel_bit);
        if defs
            .iter()
            .any(|def| (MOD_ACTION_ID_BASE..=MOD_ACTION_ID_END).contains(&def.action.0))
        {
            return Err(ActionResolveError::ReservedId);
        }

        let mut entries: Vec<(&'static str, &'static str)> = Vec::new();
        for (mod_id, names) in mods {
            for name in *names {
                validate_action_name(name)?;
                entries.push((*mod_id, *name));
            }
        }
        entries.sort_unstable();

        // 重名检测按 name 排序，避免两个 mod 的同名动作被别的名字隔开而漏检；
        // 分配顺序仍按 (mod_id, name)。
        let mut by_name = entries.clone();
        by_name.sort_unstable_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(b.0)));
        for pair in by_name.windows(2) {
            if pair[0].1 == pair[1].1 {
                return Err(ActionResolveError::DuplicateName {
                    name: pair[1].1,
                    first: pair[0].0,
                    second: pair[1].0,
                });
            }
        }
        for (mod_id, name) in &entries {
            if defs.iter().any(|def| def.name == *name) {
                return Err(ActionResolveError::DuplicateName {
                    name,
                    first: "core",
                    second: mod_id,
                });
            }
        }

        let max_mod_actions = (MOD_ACTION_ID_END - MOD_ACTION_ID_BASE + 1) as usize;
        if entries.len() > max_mod_actions {
            return Err(ActionResolveError::Exhausted {
                used: entries.len(),
                max: max_mod_actions,
            });
        }

        let mut occupied = 0u64;
        for def in &defs {
            if def.channel_bit < MAX_INPUT_CHANNELS {
                occupied |= 1u64 << def.channel_bit;
            }
        }
        for (index, (_, name)) in entries.iter().enumerate() {
            let bit = (0..MAX_INPUT_CHANNELS)
                .find(|bit| occupied & (1u64 << bit) == 0)
                .ok_or(ActionResolveError::Exhausted {
                    used: occupied.count_ones() as usize,
                    max: MAX_INPUT_CHANNELS as usize,
                })?;
            occupied |= 1u64 << bit;
            defs.push(ActionDef {
                name,
                action: ActionId(MOD_ACTION_ID_BASE + index as u32),
                channel_bit: bit,
            });
        }

        Ok(Self { defs })
    }

    /// 全局动作表（core 在前按位升序，mod 在后按分配顺序）。
    pub fn table(&self) -> &[ActionDef] {
        &self.defs
    }

    /// 名字查表；未知名字返回 None。
    pub fn lookup(&self, name: &str) -> Option<ActionDef> {
        self.defs.iter().find(|def| def.name == name).copied()
    }

    /// 名字 → 通道位序号。
    pub fn channel_bit_of(&self, name: &str) -> Option<u32> {
        self.lookup(name).map(|def| def.channel_bit)
    }

    /// 名字 → 单一位掩码。
    pub fn mask_of_name(&self, name: &str) -> Option<ChannelMask> {
        self.channel_bit_of(name)
            .map(|bit| ChannelMask::bit(bit % MAX_INPUT_CHANNELS))
    }

    /// 批量名字 → 掩码（未知名字被忽略）。
    pub fn mask_of_names<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> ChannelMask {
        names
            .into_iter()
            .filter_map(|name| self.mask_of_name(name))
            .fold(ChannelMask::EMPTY, |acc, mask| ChannelMask(acc.0 | mask.0))
    }

    /// 位序号 → 名字。
    pub fn name_of_bit(&self, bit: u32) -> Option<&'static str> {
        self.defs
            .iter()
            .find(|def| def.channel_bit == bit)
            .map(|def| def.name)
    }

    /// 位序号 → 语义动作。
    pub fn action_of_bit(&self, bit: u32) -> Option<ActionId> {
        self.defs
            .iter()
            .find(|def| def.channel_bit == bit)
            .map(|def| def.action)
    }

    /// 供表现层拉取并校验：返回 (name, channel_bit)。
    pub fn snapshot(&self) -> Vec<(&'static str, u32)> {
        self.defs
            .iter()
            .map(|def| (def.name, def.channel_bit))
            .collect()
    }
}

/// 动作名必须非空、≤ [ACTION_NAME_MAX_BYTES] 字节、且不含控制字符。
fn validate_action_name(name: &'static str) -> Result<(), ActionResolveError> {
    if name.is_empty() || name.len() > ACTION_NAME_MAX_BYTES || name.chars().any(char::is_control) {
        return Err(ActionResolveError::BadName { name });
    }
    Ok(())
}

/// 动作表解析失败的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionResolveError {
    /// 两个来源声明了同一个动作名；first / second 是来源 mod id
    /// （与 core 重名时 first 为 "core"）。
    DuplicateName {
        name: &'static str,
        first: &'static str,
        second: &'static str,
    },
    /// core 动作占用了保留给 mod 的 id 区间 1000..=9999。
    ReservedId,
    /// id 或通道位耗尽。
    Exhausted { used: usize, max: usize },
    /// 动作名非法（空 / 超长 / 含控制字符）。
    BadName { name: &'static str },
}

impl std::fmt::Display for ActionResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateName {
                name,
                first,
                second,
            } => write!(f, "动作名 \"{name}\" 重复（{first} 与 {second}）"),
            Self::ReservedId => write!(
                f,
                "核心动作占用了保留给 mod 的动作 ID 区间 {MOD_ACTION_ID_BASE}..={MOD_ACTION_ID_END}"
            ),
            Self::Exhausted { used, max } => {
                write!(f, "动作表容量耗尽：已用 {used}，上限 {max}")
            }
            Self::BadName { name } => write!(
                f,
                "动作名 \"{name}\" 非法（必须非空、不超过 {ACTION_NAME_MAX_BYTES} 字节、且不含控制字符）"
            ),
        }
    }
}

impl std::error::Error for ActionResolveError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// 15 条 core 动作：通道位 0..=14、id 1..=15（与 game-core 的 core 表同形）。
    const CORE: [ActionDef; 15] = [
        ActionDef {
            name: "core_00",
            action: ActionId(1),
            channel_bit: 0,
        },
        ActionDef {
            name: "core_01",
            action: ActionId(2),
            channel_bit: 1,
        },
        ActionDef {
            name: "core_02",
            action: ActionId(3),
            channel_bit: 2,
        },
        ActionDef {
            name: "core_03",
            action: ActionId(4),
            channel_bit: 3,
        },
        ActionDef {
            name: "core_04",
            action: ActionId(5),
            channel_bit: 4,
        },
        ActionDef {
            name: "core_05",
            action: ActionId(6),
            channel_bit: 5,
        },
        ActionDef {
            name: "core_06",
            action: ActionId(7),
            channel_bit: 6,
        },
        ActionDef {
            name: "core_07",
            action: ActionId(8),
            channel_bit: 7,
        },
        ActionDef {
            name: "core_08",
            action: ActionId(9),
            channel_bit: 8,
        },
        ActionDef {
            name: "core_09",
            action: ActionId(10),
            channel_bit: 9,
        },
        ActionDef {
            name: "core_10",
            action: ActionId(11),
            channel_bit: 10,
        },
        ActionDef {
            name: "core_11",
            action: ActionId(12),
            channel_bit: 11,
        },
        ActionDef {
            name: "core_12",
            action: ActionId(13),
            channel_bit: 12,
        },
        ActionDef {
            name: "core_13",
            action: ActionId(14),
            channel_bit: 13,
        },
        ActionDef {
            name: "core_14",
            action: ActionId(15),
            channel_bit: 14,
        },
    ];

    fn triple(defs: &[ActionDef]) -> Vec<(&'static str, u32, u32)> {
        defs.iter()
            .map(|def| (def.name, def.action.0, def.channel_bit))
            .collect()
    }

    /// 运行期泄漏一份名字切片，方便构造「很多动作」的 mod 清单。
    fn leaked(names: Vec<&'static str>) -> &'static [&'static str] {
        Box::leak(names.into_boxed_slice())
    }

    fn leaked_names(prefix: &str, count: usize) -> &'static [&'static str] {
        let names: Vec<&'static str> = (0..count)
            .map(|index| {
                let name: &'static str = Box::leak(format!("{prefix}{index:04}").into_boxed_str());
                name
            })
            .collect();
        leaked(names)
    }

    #[test]
    fn resolve_without_mods_keeps_core_table() {
        let resolved = ResolvedActions::resolve(&CORE, &[]).expect("core 表恒可解析");
        assert_eq!(triple(resolved.table()), triple(&CORE));
        for def in &CORE {
            assert_eq!(resolved.lookup(def.name), Some(*def));
            assert_eq!(resolved.channel_bit_of(def.name), Some(def.channel_bit));
            assert_eq!(resolved.action_of_bit(def.channel_bit), Some(def.action));
            assert_eq!(resolved.name_of_bit(def.channel_bit), Some(def.name));
            assert_eq!(
                resolved.mask_of_name(def.name),
                Some(ChannelMask::bit(def.channel_bit))
            );
        }
        assert_eq!(resolved.lookup("missing"), None);
        assert_eq!(resolved.name_of_bit(63), None);
        assert_eq!(resolved.snapshot().len(), CORE.len());
    }

    #[test]
    fn mod_action_gets_first_free_channel_and_id() {
        let mods: &[(&'static str, &'static [&'static str])] = &[("demo", &["mod.build"])];
        let resolved = ResolvedActions::resolve(&CORE, mods).expect("mod 动作可解析");
        assert_eq!(resolved.table().len(), CORE.len() + 1);

        let def = resolved.lookup("mod.build").expect("mod 动作必须在表里");
        assert_eq!(def.action, ActionId(MOD_ACTION_ID_BASE));
        assert_eq!(def.channel_bit, 15);
        assert_eq!(
            resolved.mask_of_name("mod.build"),
            Some(ChannelMask::bit(15))
        );
        assert_eq!(
            resolved.mask_of_names(["mod.build", "core_00"]),
            ChannelMask(1 | (1 << 15))
        );
        assert_eq!(resolved.name_of_bit(15), Some("mod.build"));
        assert_eq!(resolved.action_of_bit(15), Some(ActionId(1000)));
    }

    #[test]
    fn mod_declaration_order_does_not_change_table() {
        let forward: &[(&'static str, &'static [&'static str])] =
            &[("mod.b", &["beta"]), ("mod.a", &["alpha"])];
        let reversed: &[(&'static str, &'static [&'static str])] =
            &[("mod.a", &["alpha"]), ("mod.b", &["beta"])];

        let first = ResolvedActions::resolve(&CORE, forward).expect("解析成功");
        let second = ResolvedActions::resolve(&CORE, reversed).expect("解析成功");
        assert_eq!(triple(first.table()), triple(second.table()));
        assert_eq!(
            first.lookup("alpha").map(|def| def.action),
            Some(ActionId(1000))
        );
        assert_eq!(
            first.lookup("beta").map(|def| def.action),
            Some(ActionId(1001))
        );
        assert_eq!(first.lookup("alpha").map(|def| def.channel_bit), Some(15));
        assert_eq!(first.lookup("beta").map(|def| def.channel_bit), Some(16));
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let conflict: &[(&'static str, &'static [&'static str])] = &[("demo", &["core_00"])];
        assert_eq!(
            ResolvedActions::resolve(&CORE, conflict).expect_err("与 core 重名必须拒绝"),
            ActionResolveError::DuplicateName {
                name: "core_00",
                first: "core",
                second: "demo",
            }
        );

        // 两个 mod 重名，且中间夹着别的名字：检测不能依赖相邻。
        let conflict: &[(&'static str, &'static [&'static str])] =
            &[("mod.a", &["shared"]), ("mod.b", &["other", "shared"])];
        assert_eq!(
            ResolvedActions::resolve(&CORE, conflict).expect_err("mod 之间重名必须拒绝"),
            ActionResolveError::DuplicateName {
                name: "shared",
                first: "mod.a",
                second: "mod.b",
            }
        );
    }

    #[test]
    fn bad_names_are_rejected() {
        let cases: [&'static str; 3] = ["", "bad\nname", {
            let long: &'static str =
                Box::leak("a".repeat(ACTION_NAME_MAX_BYTES + 1).into_boxed_str());
            long
        }];
        for name in cases {
            let mods = [("demo", leaked(vec![name]))];
            assert_eq!(
                ResolvedActions::resolve(&CORE, &mods).expect_err("非法动作名必须拒绝"),
                ActionResolveError::BadName { name },
                "名字 {name:?} 应被判非法"
            );
        }
    }

    #[test]
    fn core_reserved_id_is_rejected() {
        const BAD_CORE: [ActionDef; 1] = [ActionDef {
            name: "core",
            action: ActionId(MOD_ACTION_ID_BASE),
            channel_bit: 0,
        }];
        assert_eq!(
            ResolvedActions::resolve(&BAD_CORE, &[]).expect_err("core 不得占用 mod id 区间"),
            ActionResolveError::ReservedId
        );
    }

    #[test]
    fn action_id_range_exhaustion_is_rejected() {
        let names = leaked_names("act", 9001);
        let mods = [("demo", names)];
        assert_eq!(
            ResolvedActions::resolve(&CORE, &mods).expect_err("超过 9000 个 mod 动作必须拒绝"),
            ActionResolveError::Exhausted {
                used: 9001,
                max: 9000,
            }
        );
    }

    #[test]
    fn channel_exhaustion_is_rejected() {
        let names = leaked_names("act", 49);
        let mods = [("demo", names)];
        let resolved = ResolvedActions::resolve(&CORE, &mods).expect("15 + 49 = 64 个通道位刚好够");
        assert_eq!(resolved.table().len(), MAX_INPUT_CHANNELS as usize);
        assert_eq!(
            resolved.lookup("act0048").map(|def| def.channel_bit),
            Some(63)
        );

        let names = leaked_names("act", 50);
        let mods = [("demo", names)];
        assert_eq!(
            ResolvedActions::resolve(&CORE, &mods)
                .expect_err("第 50 个 mod 动作必须因通道耗尽被拒绝"),
            ActionResolveError::Exhausted { used: 64, max: 64 }
        );
    }
}
