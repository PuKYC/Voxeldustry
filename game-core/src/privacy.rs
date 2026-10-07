//! 隐私组件与可见性清单（game-core 业务侧）。
//!
//! 分工边界：
//!
//! - `game_engine::spatial` / `game_engine::aoi`：负责 "这个实体整体在不在某 client 的空间范围内"，
//!   产出 `EntityEntered` / `EntityLeft` 事件，不变。
//! - 本文件：负责声明「实体已经在视野内，但某些 Component（阵营私有数据、隐身状态）
//!   要不要因为业务规则对特定 client 隐藏」的**业务清单**；判断机制全部在
//!   `game_engine::perception`。
//!
//! 【分层边界（本轮调整后）】replicon 的组件级隐私机制（泛型 `RequiredPerception<S>`、
//! `VisibilityFilter` 实现、内存直通真值、Commands/Query 扩展、Debug 自检）已全部
//! 收回 `game_engine::perception`。本文件只保留：
//!
//! - 三个具体隐私组件：`ExactHealth` / `InventoryContents` / `StealthDetail`；
//! - `CorePrivacyScope`：由 `define_privacy_components!` 生成的组件元组（= replicon Scope）；
//! - `CoreRequiredPerception = RequiredPerception<CorePrivacyScope>`；
//! - `PRIVACY_ONLY_COMPONENTS` 与契约自检测试。
//!
//! 引擎的 `VisibilityFilter` 实现以 `S` 为 Scope 泛型：业务层只给出一个组件元组，
//! 引擎负责注册。因此 game-core 里不再有任何 `impl VisibilityFilter`（孤儿规则消失）。

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

// 复用并重导出引擎的组件级隐私机制：保持 `crate::privacy::X` 路径稳定。
#[cfg(debug_assertions)]
pub use game_engine::perception::VisibilityViolations;
pub use game_engine::perception::{
    log_privacy_removal_violation, log_privacy_violation, memory_path_component_visible,
    perception_allows, privacy_component_violation, PerceptionMask, PerceptionMaskQueryExt,
    RequiredPerception, RequiredPerceptionQueryExt, VisibilityCommandsExt,
};

// ─────────────────── 本游戏具体隐私组件 ───────────────────

/// 本项目唯一的血量组件（逻辑真值 + 隐私门控载体）。
///
/// 为什么放在本模块：血量是要按 `PerceptionMask` 分级下发的隐私数据，
/// 因此它同时注册进 `CorePrivacyScope`，接受组件级可见性过滤。
/// 逻辑侧（`gameplay::death`）直接读写这份整数血量；表现层通过
/// `ToPresentation` 把它投影成 `PresentedHealth`（GPF1 线格式仍是
/// current/max 两个 f32，保持不变）。
///
/// 不变量：`current` / `max` 都是整数，伤害结算是整数饱和减法，保持确定性。
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExactHealth {
    /// 当前血量（<= 0 视为死亡）。
    pub current: i32,
    /// 血量上限。
    pub max: i32,
}

impl ExactHealth {
    /// 满血。
    pub const fn full(max: i32) -> Self {
        Self { current: max, max }
    }
}
#[derive(Component, Serialize, Deserialize)]
pub struct InventoryContents(pub Vec<u32>);
#[derive(Component, Serialize, Deserialize)]
pub struct StealthDetail {
    pub is_hidden: bool,
    pub source_ability: u32,
}

/// **privacy-only 组件清单**：只作为隐私门控、没有独立表现载荷的组件类型名。
///
/// `define_payloads!` 生成的 `PRIVACY_COMPONENTS` 只覆盖「有载荷」的隐私组件
/// （如 `ExactHealth`）；`InventoryContents` / `StealthDetail` 这类不含载荷的
/// 隐私组件由本常量在业务层单独声明，两者合起来才是完整的隐私组件清单。
pub const PRIVACY_ONLY_COMPONENTS: &[&str] = &["InventoryContents", "StealthDetail"];

// ─────────────────── 隐私清单声明 ───────────────────

// 一次性声明隐私组件元组，并由引擎生成：
// - `CorePrivacyScope`（replicon FilterScope）；
// - `register_component_visibility`（net 注册 filter + debug 自检系统）。
game_engine::define_privacy_components! {
    scope: CorePrivacyScope;
    components: [ExactHealth, InventoryContents, StealthDetail];
}

/// 本游戏统一的可见性规则组件特化（Scope = `CorePrivacyScope`）。
pub type CoreRequiredPerception = RequiredPerception<CorePrivacyScope>;

#[cfg(test)]
mod tests {
    use super::*;

    // 补充：直接验证 VisibilityFilter Trait 的 Fail-Closed 行为。
    #[cfg(feature = "net")]
    #[test]
    fn test_visibility_filter_trait_fail_closed() {
        use bevy_replicon::prelude::VisibilityFilter;

        let req = CoreRequiredPerception::new(0b0001);
        // Bevy 0.19 中 Entity::from_bits 接收 u64，是最安全的构造假 ID 的方式
        let client_entity = Entity::from_bits(1);

        // 观察者有权限
        let mask_ok = PerceptionMask(0b0001);
        assert!(req.is_visible(client_entity, Some(&mask_ok)));

        // 观察者无权限 (Fail-Closed)
        let mask_bad = PerceptionMask(0b0010);
        assert!(!req.is_visible(client_entity, Some(&mask_bad)));

        // 观察者完全没有 PerceptionMask 组件 (传入 None，必须 Fail-Closed)
        assert!(
            !req.is_visible(client_entity, None),
            "缺少 PerceptionMask 时必须隐藏隐私数据！"
        );
    }

    // 契约自检：载荷隐私表 + privacy-only 清单必须与 CorePrivacyScope 对齐。
    // 防止「加了隐私组件却忘了注册门控」。
    #[test]
    fn privacy_lists_align_with_scope() {
        let mut from_payloads: Vec<&str> = crate::presentation::payload::PRIVACY_COMPONENTS
            .iter()
            .filter_map(|(_, private)| *private)
            .collect();
        from_payloads.extend_from_slice(PRIVACY_ONLY_COMPONENTS);
        from_payloads.sort_unstable();

        let mut scope = vec!["ExactHealth", "InventoryContents", "StealthDetail"];
        scope.sort_unstable();

        assert_eq!(
            from_payloads, scope,
            "隐私组件清单与 CorePrivacyScope 不一致：请同步 PRIVACY_COMPONENTS / PRIVACY_ONLY_COMPONENTS"
        );
    }

    // ── 以下用例随 RequiredPerception 一并从 game-engine::perception 迁回 ──

    /// 单机内存直通路径必须与网络路径共用同一份真值。
    #[test]
    fn memory_path_matches_network_path_truth() {
        let cases = [
            (0b0000, 0b0000, true),
            (0b1111, 0b0000, true),
            (0b0000, 0b0100, false),
            (0b1010, 0b0100, false),
            (0b1110, 0b0100, true),
        ];
        for (p, r, expected) in cases {
            let p_mask = PerceptionMask(p);
            let r_req = CoreRequiredPerception::new(r);
            let net_res = perception_allows(p_mask, r_req.0);
            let mem_res = memory_path_component_visible(p_mask, Some(&r_req));
            assert_eq!(
                net_res, expected,
                "network path mismatch for {p:#x} & {r:#x}"
            );
            assert_eq!(
                mem_res, expected,
                "memory path mismatch for {p:#x} & {r:#x}"
            );
        }
    }

    #[test]
    fn memory_path_defaults_visible_without_rule() {
        assert!(memory_path_component_visible::<CorePrivacyScope>(
            PerceptionMask(0),
            None
        ));
    }

    #[test]
    fn required_perception_api_semantics() {
        assert!(CoreRequiredPerception::PUBLIC.is_public());
        assert!(!CoreRequiredPerception::new(0b0001).is_public());

        let mut req = CoreRequiredPerception::new(0b0001);
        assert!(req.intersects(0b0001));
        assert!(!req.intersects(0b0010));
        req.insert(0b0010);
        assert_eq!(req, CoreRequiredPerception::new(0b0011));
        req.remove(0b0001);
        assert_eq!(req, CoreRequiredPerception::new(0b0010));
    }
}

/// 集成测试：验证 Debug 自检系统的 Schedule 调度与 RemovedComponents 边界行为。
#[cfg(all(test, debug_assertions))]
mod integration_tests {
    use super::*;

    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins(bevy::prelude::MinimalPlugins);

        // 初始化 Replicon 必需的 Registry（仅 net 构建）。
        #[cfg(feature = "net")]
        {
            app.init_resource::<bevy_replicon::server::visibility::registry::FilterRegistry>();
            app.init_resource::<bevy_replicon::shared::replication::registry::ReplicationRegistry>(
            );
        }

        register_component_visibility(&mut app);
        app
    }

    #[test]
    fn test_violation_on_missing_required_perception() {
        let mut app = test_app();
        app.world_mut().spawn(ExactHealth::full(100));
        app.update();
        let violations = app.world().resource::<VisibilityViolations>().0;
        assert_eq!(violations, 1, "新增隐私组件缺规则应触发 1 次违规");
    }

    #[test]
    fn test_violation_on_midnight_removal() {
        let mut app = test_app();
        let entity = app
            .world_mut()
            .spawn((ExactHealth::full(100), CoreRequiredPerception::PUBLIC))
            .id();
        app.update();
        assert_eq!(app.world().resource::<VisibilityViolations>().0, 0);

        app.world_mut()
            .entity_mut(entity)
            .remove::<CoreRequiredPerception>();
        app.update();
        let violations = app.world().resource::<VisibilityViolations>().0;
        assert_eq!(violations, 1, "中途 remove 规则应触发 1 次违规");
    }

    #[test]
    fn test_no_violation_on_despawn() {
        let mut app = test_app();
        let entity = app
            .world_mut()
            .spawn((ExactHealth::full(100), CoreRequiredPerception::PUBLIC))
            .id();
        app.update();

        app.world_mut().despawn(entity);
        app.update();
        let violations = app.world().resource::<VisibilityViolations>().0;
        assert_eq!(violations, 0, "实体 despawn 不应触发违规");
    }

    #[test]
    fn test_no_violation_on_make_public() {
        let mut app = test_app();
        let entity = app
            .world_mut()
            .spawn((ExactHealth::full(100), CoreRequiredPerception::new(1)))
            .id();
        app.update();

        app.world_mut()
            .entity_mut(entity)
            .insert(CoreRequiredPerception::PUBLIC);
        app.update();
        let violations = app.world().resource::<VisibilityViolations>().0;
        assert_eq!(violations, 0, "使用合法 API 覆盖值不应触发违规");
    }
}
