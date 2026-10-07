//! 核心语义表、域查询与表快照（编译期常量，只增不改）。

use super::{partition_name, partition_of, SemanticEntry};

macro_rules! semantic_table {
    ($table:ident, $name_fn:ident, [$(($id:expr, $name:expr)),+ $(,)?]) => {
        /// 语义表（ID 与名字都唯一）。
        pub const $table: &[SemanticEntry] = &[
            $(SemanticEntry { id: $id, name: $name }),+
        ];

        /// ID -> 名字；未知返回 `None`（前向兼容：老客户端遇到新 ID 不崩）。
        pub fn $name_fn(id: u32) -> Option<&'static str> {
            $table
                .iter()
                .find(|entry| entry.id == id)
                .map(|entry| entry.name)
        }
    };
}

semantic_table!(
    LOCOMOTION_TABLE,
    locomotion_name,
    [
        (0, "idle"),
        (1, "walk"),
        (2, "run"),
        (3, "crouch"),
        (4, "fall"),
        (5, "swim"),
        (6, "fly"),
    ]
);

semantic_table!(
    ACTION_STATE_TABLE,
    action_state_name,
    [
        (0, "none"),
        (1, "attack"),
        (2, "cast"),
        (3, "block"),
        (4, "reload"),
        (5, "use"),
    ]
);

semantic_table!(
    OVERLAY_TAG_TABLE,
    overlay_tag_name,
    [
        (1, "burning"),
        (2, "stunned"),
        (3, "rooted"),
        (4, "slowed"),
        (5, "shielded"),
        (6, "poisoned"),
    ]
);

semantic_table!(
    SOUND_TABLE,
    sound_name,
    [
        (1, "footstep"),
        (2, "jump"),
        (3, "land"),
        (4, "hit"),
        (5, "swing"),
        (6, "ui_click"),
    ]
);

semantic_table!(
    ANIM_TABLE,
    anim_name,
    [
        (1, "idle"),
        (2, "walk"),
        (3, "run"),
        (4, "jump"),
        (5, "fall"),
        (6, "attack"),
        (7, "hit"),
        (8, "death"),
    ]
);

semantic_table!(
    VFX_TABLE,
    vfx_name,
    [
        (1, "hit_spark"),
        (2, "muzzle_flash"),
        (3, "dust"),
        (4, "heal"),
        (5, "explosion"),
    ]
);

/// 全部核心语义域：`(域名, 表)`。
pub const SEMANTIC_DOMAINS: &[(&str, &[SemanticEntry])] = &[
    ("locomotion", LOCOMOTION_TABLE),
    ("action_state", ACTION_STATE_TABLE),
    ("overlay_tag", OVERLAY_TAG_TABLE),
    ("sound", SOUND_TABLE),
    ("anim", ANIM_TABLE),
    ("vfx", VFX_TABLE),
];

/// 域名 -> 核心表；非核心域返回 `None`。
pub fn domain_table(domain: &str) -> Option<&'static [SemanticEntry]> {
    SEMANTIC_DOMAINS
        .iter()
        .find(|(name, _)| *name == domain)
        .map(|(_, table)| *table)
}

/// 是否是核心域。
pub fn is_core_domain(domain: &str) -> bool {
    domain_table(domain).is_some()
}

/// 表快照：`(id, name, partition)`。
pub fn table_snapshot(table: &[SemanticEntry]) -> Vec<(u32, &'static str, &'static str)> {
    table
        .iter()
        .map(|entry| (entry.id, entry.name, partition_name(partition_of(entry.id))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::CORE_ID_MAX;
    use super::*;

    #[test]
    fn tables_are_unique_and_partitioned() {
        for (domain, table) in SEMANTIC_DOMAINS {
            let mut ids: Vec<u32> = table.iter().map(|e| e.id).collect();
            let before = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(before, ids.len(), "{domain} 有重复 ID");

            let mut names: Vec<&str> = table.iter().map(|e| e.name).collect();
            let before = names.len();
            names.sort_unstable();
            names.dedup();
            assert_eq!(before, names.len(), "{domain} 有重复名字");

            for entry in table.iter() {
                assert!(
                    entry.id <= CORE_ID_MAX,
                    "{domain} 的核心表 ID {} 越界（mod 区间应留给外部）",
                    entry.id
                );
                assert!(!entry.name.is_empty(), "{domain} 存在空名字");
            }
        }
    }

    #[test]
    fn lookup_unknown_is_none() {
        assert_eq!(locomotion_name(1), Some("walk"));
        assert_eq!(locomotion_name(4242), None);
        assert_eq!(sound_name(4), Some("hit"));
    }
}
