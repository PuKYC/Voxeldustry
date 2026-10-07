//! 感知掩码的通用位运算机制（引擎层，默认不依赖网络）。
//!
//! 网络路径（replicon 的 `VisibilityFilter`）与单人内存直通路径共用同一份
//! 位运算真值 [perception_allows]，避免「联机隐身、单机全透明」的分裂。
//! replicon 适配集中在 `perception::net`（feature = "net"），引擎默认不依赖
//! bevy_replicon。
//!
//! 【分层边界】组件级隐私的**机制**全部归属于本模块：
//!
//! - [PerceptionMask] / [perception_allows]：与具体组件无关的位运算；
//! - [RequiredPerception]：泛型可见性需求组件，`S` 是 replicon 的可见性 Scope；
//! - `impl VisibilityFilter for RequiredPerception<S>`：只在 `feature = "net"` 下存在；
//! - [memory_path_component_visible]：单人内存直通真值（与网络路径共用同一份真值）；
//! - [VisibilityCommandsExt] / [PerceptionMaskQueryExt] / [RequiredPerceptionQueryExt]：
//!   Commands / Query 的增量修改辅助；
//! - [VisibilityViolations] / [privacy_component_violation] / [log_privacy_violation]
//!   / [log_privacy_removal_violation]：Debug 自检；
//! - [define_privacy_components!]：业务层一次性声明「哪些组件是隐私」。
//!
//! game-core 只保留三个具体隐私组件与一个元组类型别名，不再拥有任何
//! `impl VisibilityFilter`（因此也不会踩孤儿规则）。

use core::marker::PhantomData;

use bevy::prelude::*;

#[cfg(feature = "net")]
use bevy_replicon::prelude::{FilterScope, VisibilityFilter};

#[cfg(feature = "net")]
pub mod net;

/// 观察者能感知的位掩码。bit 含义由游戏自定义：
/// - bit0：己方阵营
/// - bit1：同公会
/// - bit2：具备「看穿隐身」权限
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[component(immutable)]
pub struct PerceptionMask(pub u32);

impl PerceptionMask {
    /// 空掩码：不具备任何感知能力。
    pub const EMPTY: Self = Self(0);

    /// 全掩码：具备所有感知能力（仅 Debug 暴露，防止生产误用）。
    #[cfg(debug_assertions)]
    pub const ALL: Self = Self(u32::MAX);

    /// 任意一位命中。
    #[inline]
    pub fn intersects(self, bits: u32) -> bool {
        self.0 & bits != 0
    }

    /// 全部命中。
    #[inline]
    pub fn contains(self, bits: u32) -> bool {
        self.0 & bits == bits
    }

    /// 授予指定位。
    #[inline]
    pub fn insert(&mut self, bits: u32) {
        self.0 |= bits;
    }

    /// 撤销指定位。
    #[inline]
    pub fn remove(&mut self, bits: u32) {
        self.0 &= !bits;
    }

    /// 切换指定位。
    #[inline]
    pub fn toggle(&mut self, bits: u32) {
        self.0 ^= bits;
    }

    /// 设置指定位的开关状态。
    #[inline]
    pub fn set(&mut self, bits: u32, enabled: bool) {
        if enabled {
            self.insert(bits);
        } else {
            self.remove(bits);
        }
    }
}

/// 纯函数：只做位运算判断，不碰 ECS / 网络。
///
/// required_bits == 0 表示公开可见，直接短路返回 true。
/// 网络路径与单人内存直通路径都必须调用这一份，禁止各自重新实现。
#[inline]
pub fn perception_allows(perception: PerceptionMask, required_bits: u32) -> bool {
    required_bits == 0 || perception.0 & required_bits != 0
}

// ─────────────────── 泛型可见性需求组件 ───────────────────

/// 挂在 gameplay 实体上：这份隐私向数据需要命中 `PerceptionMask` 的哪些 bit 才可见。
///
/// 泛型参数 `S` 是 replicon 的可见性 Scope（`FilterScope`）。把 Scope 提升为
/// 类型参数后，`VisibilityFilter` 实现由引擎统一提供；业务层只需给出一个组件元组
/// 类型别名，例如 game-core 的
/// `CoreRequiredPerception = RequiredPerception<CorePrivacyScope>`。
///
/// 【为什么结构体只写 `S: 'static`，不写 `S: FilterScope`】
/// `S` 通过 `PhantomData<fn() -> S>` 参与类型，而 Bevy `Component` 要求
/// `Self: 'static`，缺少 `'static` 会触发 E0310。这里**不能**写
/// `S: FilterScope`：`FilterScope` 来自 bevy_replicon，只在
/// `feature = "net"` 下存在，写在这里会让默认构建硬依赖 replicon。
/// `FilterScope` 只加在下面的 net `VisibilityFilter` impl 上。
///
/// 【构造】第二个字段是私有标记，外部不能用元组语法构造；请使用
/// [RequiredPerception::new] / [RequiredPerception::PUBLIC]。
///
/// 【序列化】手写 serde impl，线格式与旧的单字段 `RequiredPerception(u32)` 完全一致，
/// 且不要求 `S: Serialize`。
#[derive(Component)]
#[component(immutable)]
pub struct RequiredPerception<S: 'static>(pub u32, PhantomData<fn() -> S>);

impl<S: 'static> RequiredPerception<S> {
    /// 无需求：任何观察者都能看见（公开可见）。
    pub const PUBLIC: Self = Self(0, PhantomData);

    /// 构造一个带指定位需求的可见性规则。
    #[inline]
    pub const fn new(bits: u32) -> Self {
        Self(bits, PhantomData)
    }

    /// 检查是否为公开可见（无权限需求）。
    #[inline]
    pub fn is_public(self) -> bool {
        self.0 == 0
    }

    /// 检查是否包含指定位需求（任意一位命中）。
    #[inline]
    pub fn intersects(self, bits: u32) -> bool {
        self.0 & bits != 0
    }

    /// 追加指定位需求。
    #[inline]
    pub fn insert(&mut self, bits: u32) {
        self.0 |= bits;
    }

    /// 移除指定位需求。
    #[inline]
    pub fn remove(&mut self, bits: u32) {
        self.0 &= !bits;
    }
}

// 以下标量 trait 全部**手写**，不给 S 加任何 trait 界。
// 原因：业务 Scope 元组里的组件未必实现 Clone/Copy/Debug/PartialEq/Default
// （例如含 Vec<u32> 的组件），若用 #[derive(...)] 会给 S 带上这些界，
// 导致 RequiredPerception<CorePrivacyScope> 失去这些能力。这里只比较/复制 u32。

impl<S: 'static> Clone for RequiredPerception<S> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<S: 'static> Copy for RequiredPerception<S> {}

impl<S: 'static> Default for RequiredPerception<S> {
    #[inline]
    fn default() -> Self {
        Self::PUBLIC
    }
}

impl<S: 'static> core::fmt::Debug for RequiredPerception<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("RequiredPerception").field(&self.0).finish()
    }
}

impl<S: 'static> PartialEq for RequiredPerception<S> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<S: 'static> Eq for RequiredPerception<S> {}

// 手写 serde：只序列化 u32，与旧的 RequiredPerception(u32) 线格式一致，
// 且不要求 S: Serialize / S: Deserialize。
impl<S: 'static> serde::Serialize for RequiredPerception<S> {
    fn serialize<Ser>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: serde::Serializer,
    {
        serde::Serialize::serialize(&self.0, serializer)
    }
}

impl<'de, S: 'static> serde::Deserialize<'de> for RequiredPerception<S> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let bits = <u32 as serde::Deserialize<'de>>::deserialize(deserializer)?;
        Ok(Self::new(bits))
    }
}

/// replicon 适配：引擎为任意 `FilterScope` 统一实现组件级可见性。
///
/// `Scope` 直接等于 `S`，即业务层声明的隐私组件元组；`ClientComponent` 固定为
/// [PerceptionMask]。默认构建（无 net）下此 impl 不存在。
#[cfg(feature = "net")]
impl<S: FilterScope + 'static> VisibilityFilter for RequiredPerception<S> {
    type ClientComponent = PerceptionMask;

    /// 只对业务层声明的隐私组件生效，Transform / 表现层数据完全不受影响——
    /// 不要把 Scope 扩大到跟隐私无关的组件上，否则调试时很难定位
    /// "为什么这个字段突然收不到了"。
    type Scope = S;

    fn is_visible(&self, _client: Entity, perception: Option<&PerceptionMask>) -> bool {
        // 【源码核实与架构共识】：
        // 1. Replicon 是 Fail-Open 的：实体没挂 RequiredPerception 时直接放行组件。
        // 2. 本函数仅在实体显式挂载了 RequiredPerception 时被调用。
        // 3. 客户端缺 PerceptionMask 时 perception 为 None，is_some_and 返回 false（Fail-Closed）。
        //
        // 【团队共识：不对称设计的语义】：
        // - 实体缺规则 (Fail-Open) = "这是一块普通的石头，没有隐私，所有人都能看"。
        // - 观察者缺权限 (Fail-Closed) = "这是一个没有视觉/权限的瞎子，什么都看不见"。
        perception.is_some_and(|p| perception_allows(*p, self.0))
    }
}

// ─────────────────── 单人内存直通真值 ───────────────────

/// 纯函数：单人模式下判断某个隐私组件是否对该观察者可见。
///
/// `required == None` 表示实体没有挂规则，默认可见（Fail-Open）。这与网络路径
/// （replicon 对没有挂 RequiredPerception 的实体直接放行）语义完全一致，
/// 保证单/联机的隐身/阵营规则永远只有一份真值。
#[inline]
pub fn memory_path_component_visible<S: 'static>(
    local_player_perception: PerceptionMask,
    required: Option<&RequiredPerception<S>>,
) -> bool {
    match required {
        None => true,
        Some(required) => perception_allows(local_player_perception, required.0),
    }
}

// ─────────────────── Debug 自检 ───────────────────

/// 【Debug 统计】：记录本帧发生的可见性契约违规次数。
///
/// 仅在 Debug 模式下存在。除了触发 error! 日志外，还会递增此计数器。
/// 可用于在 Dev UI 中实时监控违规情况，或在集成测试中作为断言信号。
#[cfg(debug_assertions)]
#[derive(Resource, Default, Debug)]
pub struct VisibilityViolations(pub usize);

/// 通用违规计数：新增隐私组件 T 但缺规则的实体数，加上中途移除
/// RequiredPerception<S> 却仍带 T 的实体数；逐条打印 error! 日志。
///
/// 供 [define_privacy_components!] 为每个隐私组件生成的 Debug 自检系统调用。
/// label 是该隐私组件的类型名（stringify!），仅用于日志。
pub fn privacy_component_violation<T: Component, S: 'static>(
    added_private: Query<Entity, (Added<T>, Without<RequiredPerception<S>>)>,
    still_has_private: Query<(), With<T>>,
    mut removed_required: RemovedComponents<RequiredPerception<S>>,
    label: &str,
) -> usize {
    let mut violations = 0;
    // 场景 1：新增隐私组件但缺规则。
    for entity in &added_private {
        log_privacy_violation(entity, label);
        violations += 1;
    }
    // 场景 2：规则被中途移除，但实体身上还带着隐私组件。
    for entity in removed_required.read() {
        // 实体已 despawn 时 get 返回 Err，自动跳过，不会误报。
        if still_has_private.get(entity).is_ok() {
            log_privacy_removal_violation(entity);
            violations += 1;
        }
    }
    violations
}

/// 【Debug 告警】：实体新增了隐私组件 label 却没挂 RequiredPerception。
///
/// 这会导致隐私数据完全公开（Fail-Open）。
pub fn log_privacy_violation(entity: Entity, label: &str) {
    error!(
        "【可见性违规】实体 {entity:?} 挂载了隐私 Component `{label}` 但缺少 RequiredPerception！\n\
         这会导致隐私数据完全公开（Fail-Open）。请显式插入 RequiredPerception::PUBLIC 或配置需求位。"
    );
}

/// 【Debug 告警】：实体的 RequiredPerception 被中途原生移除。
///
/// 这会导致隐私数据瞬间完全公开（Fail-Open）。
pub fn log_privacy_removal_violation(entity: Entity) {
    error!(
        "【可见性违规】实体 {entity:?} 的 RequiredPerception 被中途移除了！\n\
         这会导致隐私数据瞬间完全公开（Fail-Open）。\n\
         严禁使用原生的 `.remove::<RequiredPerception>()`，请使用 make_public() 或 set_public() 替代。"
    );
}

// ─────────────────── Commands / Query 扩展 ───────────────────

/// 为 `EntityCommands` 提供可见性控制的链式调用。
///
/// 泛型 S 与 [RequiredPerception] 的 Scope 对齐；调用方通常由参数推断，
/// 例如 `set_required_perception(CoreRequiredPerception::new(0b0001))`。
pub trait VisibilityCommandsExt<S: 'static> {
    /// 设置实体的隐私数据可见性需求。
    fn set_required_perception(&mut self, required: RequiredPerception<S>) -> &mut Self;
    /// 将实体设为公开可见。
    ///
    /// 这里使用 insert(PUBLIC) 而不是 remove::<RequiredPerception>()。
    ///
    /// **主因**：make_public 是**状态操作**（"让实体公开"），不是**结构操作**
    /// （"删掉隐私规则组件"）。前者即便 Replicon 是 Fail-Open、两者网络语义等价，
    /// 组件语义上的差别也值得保留——将来若 Replicon 改为 Fail-Closed，
    /// 状态语义版本自动正确，结构版本则会静默失效。
    ///
    /// **次因**：保持组件始终存在，避免 Archetype 跳转（仅在实体频繁在两种状态间切换时有收益）。
    fn make_public(&mut self) -> &mut Self;
    /// 设置客户端实体的感知掩码（覆盖式）。
    fn set_perception_mask(&mut self, mask: PerceptionMask) -> &mut Self;
}

// Bevy 0.15 EntityCommands 需要显式提供两个生命周期参数
impl<S: 'static> VisibilityCommandsExt<S> for EntityCommands<'_> {
    fn set_required_perception(&mut self, required: RequiredPerception<S>) -> &mut Self {
        self.insert(required)
    }

    fn make_public(&mut self) -> &mut Self {
        self.insert(RequiredPerception::<S>::PUBLIC)
    }
    fn set_perception_mask(&mut self, mask: PerceptionMask) -> &mut Self {
        self.insert(mask)
    }
}

/// 为 `Query<&PerceptionMask>` 提供增量修改的便捷计算。
///
/// 【架构变更注意】：因 Replicon 0.43 强制要求 VisibilityFilter 组件为 Immutable，
/// Bevy 0.15 中无法再通过 `Query<&mut T>` 就地修改。
/// 这些方法现在返回计算后的新组件值，调用者需使用 Commands.entity(e).insert(new_val) 覆盖。
pub trait PerceptionMaskQueryExt {
    fn get_granted(&self, entity: Entity, bits: u32) -> Option<PerceptionMask>;
    fn get_revoked(&self, entity: Entity, bits: u32) -> Option<PerceptionMask>;
    fn intersects(&self, entity: Entity, bits: u32) -> bool;
}

impl PerceptionMaskQueryExt for Query<'_, '_, &PerceptionMask> {
    fn get_granted(&self, entity: Entity, bits: u32) -> Option<PerceptionMask> {
        self.get(entity).ok().copied().map(|mut mask| {
            mask.insert(bits);
            mask
        })
    }

    fn get_revoked(&self, entity: Entity, bits: u32) -> Option<PerceptionMask> {
        self.get(entity).ok().copied().map(|mut mask| {
            mask.remove(bits);
            mask
        })
    }

    fn intersects(&self, entity: Entity, bits: u32) -> bool {
        self.get(entity).is_ok_and(|mask| mask.intersects(bits))
    }
}

/// 为 `Query<&RequiredPerception<S>>` 提供增量修改的便捷计算。
///
/// 注意：此处**不使用** `Option::copied()`，因为业务 Scope 元组未必 `Copy`；
/// 直接按 u32 重新构造新组件值（[RequiredPerception::new]）。
pub trait RequiredPerceptionQueryExt<S: 'static> {
    fn get_required(&self, entity: Entity, bits: u32) -> Option<RequiredPerception<S>>;
    fn get_waived(&self, entity: Entity, bits: u32) -> Option<RequiredPerception<S>>;
    /// 状态操作：计算设为公开可见的新组件值。
    fn get_public(&self, entity: Entity) -> Option<RequiredPerception<S>>;
}

impl<S: 'static> RequiredPerceptionQueryExt<S> for Query<'_, '_, &RequiredPerception<S>> {
    fn get_required(&self, entity: Entity, bits: u32) -> Option<RequiredPerception<S>> {
        self.get(entity)
            .ok()
            .map(|req| RequiredPerception::new(req.0 | bits))
    }

    fn get_waived(&self, entity: Entity, bits: u32) -> Option<RequiredPerception<S>> {
        self.get(entity)
            .ok()
            .map(|req| RequiredPerception::new(req.0 & !bits))
    }

    fn get_public(&self, entity: Entity) -> Option<RequiredPerception<S>> {
        self.get(entity)
            .ok()
            .map(|_| RequiredPerception::<S>::PUBLIC)
    }
}

// ─────────────────── 业务层隐私清单声明宏 ───────────────────

/// 一次性声明「本游戏的全部隐私组件」，并生成组件级可见性接线。
///
/// 调用形如：
///
/// ```ignore
/// game_engine::define_privacy_components! {
///     scope: CorePrivacyScope;
///     components: [ExactHealth, InventoryContents, StealthDetail];
/// }
/// ```
///
/// 生成项（全部在**调用方模块**内）：
/// - `pub type <scope> = (A, B, C,);`：喂给 RequiredPerception<scope> 的 FilterScope；
/// - `pub fn register_component_visibility(app: &mut bevy::prelude::App)`：
///   - `feature = "net"` 时注册
///     `app.add_visibility_filter::<RequiredPerception<scope>>()`；
///   - `debug_assertions` 时 init_resource::<VisibilityViolations>() 并为每个组件挂
///     一个 PostUpdate 自检系统（逐组件调用 privacy_component_violation 累加并打日志）；
/// - 组件清单数量需落在 replicon FilterScope 的元组实现区间（2..=10）。
///
/// 宏内所有机制类型都走 `$crate::perception::...` 全路径，业务层只需给出组件名。
#[macro_export]
macro_rules! define_privacy_components {
    (
        scope: $scope:ident;
        components: [ $($component:ident),+ $(,)? ];
    ) => {
        /// 本游戏声明的隐私组件元组（replicon FilterScope）。
        pub type $scope = ( $($component,)+ );

        /// 注册组件级可见性 filter（net）+ Debug 自检系统。
        ///
        /// 客户端与服务端必须以同样的顺序调用，复用「两端注册顺序一致」的约束。
        ///
        /// 非 net 的 release 构建里两个 cfg 块都被裁掉、app 参数无人使用，
        /// 这里显式 allow，避免业务层编译出现 no-op 警告。
        #[allow(unused_variables)]
        pub fn register_component_visibility(app: &mut ::bevy::prelude::App) {
            #[cfg(feature = "net")]
            {
                use ::bevy_replicon::prelude::AppVisibilityExt as _;
                app.add_visibility_filter::<$crate::perception::RequiredPerception<$scope>>();
            }

            // 在 Debug 模式下启用自检系统，防止"忘挂/误删 RequiredPerception 导致隐私裸奔"。
            // 放在 PostUpdate 确保本帧所有 Commands 都已 flush。
            #[cfg(debug_assertions)]
            {
                app.init_resource::<$crate::perception::VisibilityViolations>();

                // 每个组件一个独立的块，块内局部 fn 同名也不会冲突（是各自独立的类型）。
                $(
                    {
                        fn selfcheck(
                            added: ::bevy::prelude::Query<
                                ::bevy::prelude::Entity,
                                (
                                    ::bevy::prelude::Added<$component>,
                                    ::bevy::prelude::Without<$crate::perception::RequiredPerception<$scope>>,
                                ),
                            >,
                            still: ::bevy::prelude::Query<(), ::bevy::prelude::With<$component>>,
                            removed: ::bevy::prelude::RemovedComponents<
                                $crate::perception::RequiredPerception<$scope>,
                            >,
                            mut violations: ::bevy::prelude::ResMut<$crate::perception::VisibilityViolations>,
                        ) {
                            let found = $crate::perception::privacy_component_violation::<$component, $scope>(
                                added,
                                still,
                                removed,
                                stringify!($component),
                            );
                            violations.0 += found;
                        }
                        app.add_systems(::bevy::prelude::PostUpdate, selfcheck);
                    }
                )*
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_requirement_is_always_visible() {
        assert!(perception_allows(PerceptionMask(0), 0));
    }

    #[test]
    fn bit_overlap_is_visible_and_disjoint_is_not() {
        assert!(perception_allows(PerceptionMask(0b0001), 0b0001));
        assert!(perception_allows(PerceptionMask(0b0011), 0b0010));
        assert!(!perception_allows(PerceptionMask(0b0001), 0b0100));
        assert!(!perception_allows(PerceptionMask(0), 0b0001));
    }

    #[test]
    fn mask_helpers_are_consistent() {
        let mut mask = PerceptionMask::EMPTY;
        mask.insert(0b0101);
        assert!(mask.intersects(0b0001));
        assert!(mask.contains(0b0101));
        assert!(!mask.contains(0b0111));
        mask.remove(0b0001);
        assert_eq!(mask, PerceptionMask(0b0100));
        mask.set(0b0010, true);
        assert_eq!(mask, PerceptionMask(0b0110));
        mask.toggle(0b0010);
        assert_eq!(mask, PerceptionMask(0b0100));
    }

    /// 泛型组件在 S 只有 'static 界时也应当具备标量语义（不依赖 S 的 trait）。
    #[test]
    fn generic_required_perception_scalar_semantics() {
        // 用 () 作 Scope：它不实现 replicon FilterScope，但类型本身合法，
        // 证明结构体没有把 replicon 拖进默认构建。
        type Req = RequiredPerception<()>;
        assert!(Req::PUBLIC.is_public());
        assert!(!Req::new(0b0001).is_public());

        let mut req = Req::new(0b0001);
        assert!(req.intersects(0b0001));
        assert!(!req.intersects(0b0010));
        req.insert(0b0010);
        assert_eq!(req, Req::new(0b0011));
        req.remove(0b0001);
        assert_eq!(req, Req::new(0b0010));
        assert_eq!(Req::default(), Req::PUBLIC);
    }

    /// 单机内存直通真值必须与网络路径共用同一份真值。
    #[test]
    fn memory_path_matches_network_path_truth() {
        type Req = RequiredPerception<()>;
        let cases = [
            (0b0000, 0b0000, true),
            (0b1111, 0b0000, true),
            (0b0000, 0b0100, false),
            (0b1010, 0b0100, false),
            (0b1110, 0b0100, true),
        ];
        for (p, r, expected) in cases {
            let p_mask = PerceptionMask(p);
            let r_req = Req::new(r);
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
        assert!(memory_path_component_visible::<()>(PerceptionMask(0), None));
    }
}
