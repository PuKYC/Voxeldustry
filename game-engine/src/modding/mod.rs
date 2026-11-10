//! Mod 体系：编译期 native mod（Mod + ModContext）与运行期数据注册表。
//!
//! 修改阶梯（M0-M7）：
//!
//! | 级 | 修改对象 | 机制 |
//! |---|---|---|
//! | M0 | 数值 / 概率 | DefinitionPatch / 数据补丁 |
//! | M1 | 内容定义 | Generic Registry<T>（冲突拒绝 + 升序遍历） |
//! | M2 | 新组件 + 表现翻译 | 注册制（不注册 = 不同步） |
//! | M3 | 新增系统到某阶段 | ModContext::add_system(EngineSet, ..) |
//! | M4 | 子阶段 / 排序 | insert_set_before + configure_sets |
//! | M5 | 服务 / 策略 | Strategy trait + set_strategy |
//! | M6 | 整块子系统 | GameModule（见后续期次） |
//!
//! 关键判断：Bevy 调度不支持运行期删除 / 替换已注册系统，因此「替换行为」
//! 一律抽成 Strategy trait 换资源，而不是从调度里抠系统。

pub mod params;
pub mod registry;

pub use params::{
    ModParamsPlugin, ParamEntry, ParamError, ParamScope, ParamSpec, ParamTable, ParamType,
    VisibleIf, WorldParamsFrozen,
};
pub use registry::{NamedEntry, OverridePriority, Registry, RegistryEntry, RegistryError};

use std::collections::HashMap;
use std::marker::PhantomData;

use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::system::ScheduleSystem;
use bevy::prelude::*;

use crate::ids::fnv1a32;
use crate::sim::EngineSet;
use crate::spec::GameSpec;

/// mod 清单（稳定、带命名空间）。
pub struct ModManifest {
    pub id: &'static str,
    pub version: u32,
    /// 目标引擎 API 版本（semver 简化版：整数，只增不改）。
    pub engine_api: u32,
    /// 硬依赖：必须存在，且加载在自己之前。
    pub load_after: &'static [&'static str],
    /// 可选依赖：存在时加载在自己之前；缺失不报错。
    pub load_after_optional: &'static [&'static str],
    /// 与本 mod 冲突的 mod id。
    pub conflicts: &'static [&'static str],
    /// 互斥组：同一组内最多加载一个 mod。
    pub exclusive_groups: &'static [&'static str],
    /// 插件参数 schema；key 在 mod 内局部，落盘时拼 `<mod_id>.<key>`。
    pub params: &'static [ParamSpec],
}

/// 一层 mod 静态数据补丁（原始 JSONC 文本，解析由消费方负责）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectLayer {
    pub mod_id: &'static str,
    /// 拓扑排序后的加载序号；同号时按 `mod_id` 稳定排序。
    pub load_order: u32,
    pub src: &'static str,
}

/// mod 登记的静态数据补丁层，按确定性顺序重放。
#[derive(Resource, Default, Debug)]
pub struct ObjectLayers {
    layers: Vec<ObjectLayer>,
}

impl ObjectLayers {
    pub fn add(&mut self, layer: ObjectLayer) {
        self.layers.push(layer);
        self.layers
            .sort_by(|a, b| (a.load_order, a.mod_id).cmp(&(b.load_order, b.mod_id)));
    }

    pub fn iter(&self) -> impl Iterator<Item = &ObjectLayer> {
        self.layers.iter()
    }

    pub fn len(&self) -> usize {
        self.layers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }
}

/// 可被 mod 替换的引擎策略（M5）。
///
/// 策略方法不得读取时钟 / 线程 / 全局可变状态，否则确定性破功。
pub trait Strategy<S: GameSpec>: Resource + Send + Sync + 'static {}

/// Mod 加载错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModError {
    MissingDependency { module: String, missing: String },
    CircularDependency { module: String },
    Conflict { a: String, b: String },
    ExclusiveGroup { group: String, a: String, b: String },
    DuplicateId { id: String },
    Param(ParamError),
}

impl std::fmt::Display for ModError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModError::MissingDependency { module, missing } => {
                write!(f, "mod {module} 依赖 {missing}，但它不在 mod 集合里")
            }
            ModError::CircularDependency { module } => {
                write!(f, "mod 加载顺序存在环：{module}")
            }
            ModError::Conflict { a, b } => write!(f, "mod {a} 与 {b} 冲突"),
            ModError::ExclusiveGroup { group, a, b } => {
                write!(f, "mod {a} 与 {b} 同属互斥组 {group}")
            }
            ModError::DuplicateId { id } => write!(f, "mod id 重复：{id}"),
            ModError::Param(error) => write!(f, "mod 参数表错误：{error}"),
        }
    }
}

impl From<ParamError> for ModError {
    fn from(error: ParamError) -> Self {
        ModError::Param(error)
    }
}

impl std::error::Error for ModError {}

/// 运行期数据注册表：按 domain 分组，底层复用通用 Registry。
#[derive(Default, Resource)]
pub struct DataRegistry {
    domains: HashMap<String, Registry<NamedEntry>>,
}

impl DataRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 冲突即拒绝（不静默覆盖）。
    pub fn register(&mut self, domain: &str, id: u32, name: &str) -> Result<(), RegistryError> {
        self.domains
            .entry(domain.to_string())
            .or_default()
            .register(NamedEntry::new(id, name))
    }

    /// 应用一条显式优先级的定义补丁（M0，字段级覆盖）。
    ///
    /// 复用 [`Registry::override_entry`]：优先级不足时保留旧值（`Ok(None)`），
    /// 名字 / id 冲突仍按注册表规则拒绝。`priority` 必须显式给出，
    /// 不依赖加载顺序的偶然结果。
    pub fn patch(
        &mut self,
        patch: DefinitionPatch,
        priority: OverridePriority,
    ) -> Result<Option<NamedEntry>, RegistryError> {
        self.domains
            .entry(patch.domain)
            .or_default()
            .override_entry(NamedEntry::new(patch.id, patch.name), priority)
    }

    pub fn registry(&self, domain: &str) -> Option<&Registry<NamedEntry>> {
        self.domains.get(domain)
    }

    /// 确定性升序视图。
    pub fn sorted(&self, domain: &str) -> Vec<&NamedEntry> {
        self.domains
            .get(domain)
            .map(|registry| registry.sorted())
            .unwrap_or_default()
    }

    /// 全部域名（字典序，稳定）。
    pub fn domains(&self) -> Vec<&str> {
        let mut domains: Vec<&str> = self.domains.keys().map(String::as_str).collect();
        domains.sort_unstable();
        domains
    }
}

/// 编译期 native mod。
pub trait Mod<S: GameSpec>: Send + Sync + 'static {
    fn manifest(&self) -> &'static ModManifest;

    /// M0 / M1：注册或覆盖数据（运行期可见）。默认空实现。
    fn register_data(&self, _data: &mut DataRegistry) {}

    /// M2-M6：编译期装配（加组件注册、加系统、换策略…）。
    fn install(&self, _ctx: &mut ModContext<'_, S>) {}
}

/// mod 装配上下文（冷路径，一次性）。
pub struct ModContext<'a, S: GameSpec> {
    app: &'a mut App,
    data: &'a mut DataRegistry,
    order: u32,
    mod_id: &'static str,
    _spec: PhantomData<S>,
}

impl<'a, S: GameSpec> ModContext<'a, S> {
    pub(crate) fn new(
        app: &'a mut App,
        data: &'a mut DataRegistry,
        order: u32,
        mod_id: &'static str,
    ) -> Self {
        Self {
            app,
            data,
            order,
            mod_id,
            _spec: PhantomData,
        }
    }

    /// 本次 mod 的加载序号（决定同集合内的稳定排序）。
    pub fn load_order(&self) -> u32 {
        self.order
    }

    pub fn app(&mut self) -> &mut App {
        self.app
    }

    pub fn data(&mut self) -> &mut DataRegistry {
        self.data
    }

    /// 登记一层静态数据补丁文本（JSONC）。
    ///
    /// 文本在 mod 的 `install` 期间登记；game-core 的静态数据层在重载时
    /// 按 `(load_order, mod_id)` 重放，所以热重载不会丢掉 mod 修改。
    /// 补丁文档的 schema 由消费方定义（game-core 认 `table` + `patch`）。
    pub fn add_object_layer(&mut self, src: &'static str) {
        self.app.init_resource::<ObjectLayers>();
        let mut layers = self.app.world_mut().resource_mut::<ObjectLayers>();
        layers.add(ObjectLayer {
            mod_id: self.mod_id,
            load_order: self.order,
            src,
        });
    }

    /// M3：把一个系统加到引擎的命名阶段。
    pub fn add_system<M>(
        &mut self,
        set: EngineSet,
        system: impl IntoScheduleConfigs<ScheduleSystem, M>,
    ) -> &mut Self {
        self.app.add_systems(FixedUpdate, system.in_set(set));
        self
    }

    /// M4：把 mod 的系统挂到自己插入的集合。
    pub fn add_system_in<M>(
        &mut self,
        set: impl SystemSet,
        system: impl IntoScheduleConfigs<ScheduleSystem, M>,
    ) -> &mut Self {
        self.app.add_systems(FixedUpdate, system.in_set(set));
        self
    }

    /// M4：在锚点集合之前插入一个新的命名集合（mod 自己的阶段）。
    pub fn insert_set_before(&mut self, anchor: EngineSet, new: impl SystemSet) -> &mut Self {
        self.app.configure_sets(FixedUpdate, new.before(anchor));
        self
    }

    /// M5：替换引擎策略（移动模型 / 伤害模型 / 可见性策略…）。
    pub fn set_strategy<St: Strategy<S>>(&mut self, strategy: St) -> &mut Self {
        self.app.insert_resource(strategy);
        self
    }

    /// M1：运行期注册一条定义（冲突拒绝）。
    pub fn register_data(
        &mut self,
        domain: &str,
        id: u32,
        name: &str,
    ) -> Result<(), RegistryError> {
        self.data.register(domain, id, name)
    }

    /// M0：应用一条定义补丁（字段级覆盖）。
    ///
    /// 覆盖优先级取本 mod 的加载序号（拓扑排序后的顺序），因此「后加载者胜」
    /// 且可复现；需要更强的确定性强度时，mod 可绕过本方法直接调用
    /// [`DataRegistry::patch`] 并给出 [`OverridePriority::Explicit`]。
    pub fn patch_definition(
        &mut self,
        patch: DefinitionPatch,
    ) -> Result<Option<NamedEntry>, RegistryError> {
        let priority = OverridePriority::LoadOrder(self.order);
        self.data.patch(patch, priority)
    }
}

/// M0 数据补丁：字段级覆盖（当前只演示名字覆盖）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefinitionPatch {
    pub domain: String,
    pub id: u32,
    pub name: String,
}

/// 启动期一次性的 mod 集合（冷路径，允许 dyn；不进热路径）。
pub struct Mods<S: GameSpec> {
    mods: Vec<Box<dyn Mod<S>>>,
}

impl<S: GameSpec> Default for Mods<S> {
    fn default() -> Self {
        Self { mods: Vec::new() }
    }
}

impl<S: GameSpec> Mods<S> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push<M: Mod<S>>(mut self, module: M) -> Self {
        self.mods.push(Box::new(module));
        self
    }

    pub fn len(&self) -> usize {
        self.mods.len()
    }

    pub fn is_empty(&self) -> bool {
        self.mods.is_empty()
    }

    pub fn manifests(&self) -> Vec<&'static ModManifest> {
        self.mods.iter().map(|module| module.manifest()).collect()
    }

    /// 按 load_after / load_after_optional 做确定性拓扑排序（同层按 id 字典序），
    /// 校验 conflicts 与 exclusive_groups，依次 register_data + install，
    /// 并返回 mod_set_hash。
    pub fn apply(self, app: &mut App, data: &mut DataRegistry) -> Result<u64, ModError> {
        let count = self.mods.len();

        let mut index: HashMap<&'static str, usize> = HashMap::with_capacity(count);
        for (position, module) in self.mods.iter().enumerate() {
            let id = module.manifest().id;
            if index.insert(id, position).is_some() {
                return Err(ModError::DuplicateId { id: id.to_string() });
            }
        }

        let mut group_members: HashMap<&'static str, &'static str> = HashMap::new();
        for module in self.mods.iter() {
            let manifest = module.manifest();
            for other in manifest.conflicts {
                if index.contains_key(other) {
                    return Err(ModError::Conflict {
                        a: manifest.id.to_string(),
                        b: (*other).to_string(),
                    });
                }
            }
            for group in manifest.exclusive_groups {
                if let Some(previous) = group_members.insert(group, manifest.id) {
                    if previous != manifest.id {
                        return Err(ModError::ExclusiveGroup {
                            group: (*group).to_string(),
                            a: previous.to_string(),
                            b: manifest.id.to_string(),
                        });
                    }
                }
            }
            for dependency in manifest.load_after {
                if !index.contains_key(dependency) {
                    return Err(ModError::MissingDependency {
                        module: manifest.id.to_string(),
                        missing: (*dependency).to_string(),
                    });
                }
            }
        }

        // Kahn 拓扑排序；就绪集合按 id 字典序，保证可复现。
        let mut indegree = vec![0usize; count];
        let mut edges: Vec<Vec<usize>> = vec![Vec::new(); count];
        for (position, module) in self.mods.iter().enumerate() {
            for dependency in module.manifest().load_after {
                let dependency_index = index[dependency];
                edges[dependency_index].push(position);
                indegree[position] += 1;
            }
            for dependency in module.manifest().load_after_optional {
                if let Some(&dependency_index) = index.get(dependency) {
                    edges[dependency_index].push(position);
                    indegree[position] += 1;
                }
            }
        }

        let mut ready: Vec<usize> = (0..count).filter(|&i| indegree[i] == 0).collect();
        let mut order: Vec<usize> = Vec::with_capacity(count);
        while !ready.is_empty() {
            ready.sort_by_key(|&i| self.mods[i].manifest().id);
            let next = ready.remove(0);
            order.push(next);
            for &successor in &edges[next] {
                indegree[successor] -= 1;
                if indegree[successor] == 0 {
                    ready.push(successor);
                }
            }
        }

        if order.len() != count {
            let module = self
                .mods
                .iter()
                .find(|m| indegree[index[m.manifest().id]] > 0)
                .map(|m| m.manifest().id.to_string())
                .unwrap_or_default();
            return Err(ModError::CircularDependency { module });
        }

        for (load_order, &position) in order.iter().enumerate() {
            let module = &self.mods[position];
            module.register_data(data);
            let mut context = ModContext::new(app, data, load_order as u32, module.manifest().id);
            module.install(&mut context);
        }

        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for &position in &order {
            let manifest = self.mods[position].manifest();
            let token = format!("{}:{}", manifest.id, manifest.version);
            hash ^= u64::from(fnv1a32(token.as_bytes()));
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Ok(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::DefaultSpec;
    use std::time::Duration;

    #[derive(Resource, Default)]
    struct Counter(u32);

    #[derive(Resource, Default)]
    struct OrderLog(Vec<&'static str>);

    struct SimpleMod(&'static ModManifest);

    impl Mod<DefaultSpec> for SimpleMod {
        fn manifest(&self) -> &'static ModManifest {
            self.0
        }
    }

    fn app_with_sets() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(crate::sim::EngineSetPlugin);
        app.insert_resource(Time::<Fixed>::from_hz(60.0));
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(20),
        ));
        app
    }

    fn bump(mut counter: ResMut<Counter>) {
        counter.0 += 1;
    }

    static BUMP: ModManifest = ModManifest {
        id: "bump",
        version: 1,
        engine_api: 1,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[],
    };

    struct BumpMod;

    impl Mod<DefaultSpec> for BumpMod {
        fn manifest(&self) -> &'static ModManifest {
            &BUMP
        }
        fn install(&self, context: &mut ModContext<'_, DefaultSpec>) {
            context.add_system(EngineSet::Motion, bump);
        }
    }

    static MA: ModManifest = ModManifest {
        id: "a",
        version: 1,
        engine_api: 1,
        load_after: &["b"],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[],
    };
    static MB: ModManifest = ModManifest {
        id: "b",
        version: 1,
        engine_api: 1,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[],
    };

    struct LogMod(&'static ModManifest, &'static str);

    impl Mod<DefaultSpec> for LogMod {
        fn manifest(&self) -> &'static ModManifest {
            self.0
        }
        fn install(&self, context: &mut ModContext<'_, DefaultSpec>) {
            context
                .app()
                .world_mut()
                .resource_mut::<OrderLog>()
                .0
                .push(self.1);
        }
    }

    #[test]
    fn mod_add_system_runs_in_expected_stage() {
        let mut app = app_with_sets();
        app.init_resource::<Counter>();
        let mut data = DataRegistry::new();
        let hash = Mods::<DefaultSpec>::new()
            .push(BumpMod)
            .apply(&mut app, &mut data)
            .unwrap();
        assert_ne!(hash, 0);
        // 直接运行 FixedUpdate，避免依赖墙钟 / 定步累积。
        app.finish();
        app.cleanup();
        app.world_mut().run_schedule(FixedUpdate);
        let count = app.world().resource::<Counter>().0;
        assert_eq!(count, 1, "系统必须在 EngineSet::Motion 阶段运行一次");
    }

    static LAYER_LATE: ModManifest = ModManifest {
        id: "z_layer",
        version: 1,
        engine_api: 1,
        load_after: &["a_layer"],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[],
    };
    static LAYER_EARLY: ModManifest = ModManifest {
        id: "a_layer",
        version: 1,
        engine_api: 1,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[],
    };

    struct LayerMod(&'static ModManifest, &'static str);

    impl Mod<DefaultSpec> for LayerMod {
        fn manifest(&self) -> &'static ModManifest {
            self.0
        }
        fn install(&self, context: &mut ModContext<'_, DefaultSpec>) {
            context.add_object_layer(self.1);
        }
    }

    #[test]
    fn object_layers_record_mod_order_and_sort() {
        let mut app = app_with_sets();
        let mut data = DataRegistry::new();
        Mods::<DefaultSpec>::new()
            .push(LayerMod(&LAYER_LATE, "late"))
            .push(LayerMod(&LAYER_EARLY, "early"))
            .apply(&mut app, &mut data)
            .unwrap();

        let layers = app.world().resource::<ObjectLayers>();
        let entries: Vec<(&str, &str)> = layers
            .iter()
            .map(|layer| (layer.mod_id, layer.src))
            .collect();
        assert_eq!(entries, vec![("a_layer", "early"), ("z_layer", "late")]);
        assert_eq!(layers.iter().next().unwrap().load_order, 0);
        assert_eq!(layers.iter().nth(1).unwrap().load_order, 1);
    }

    #[test]
    fn load_after_is_topologically_sorted_deterministically() {
        let mut app = app_with_sets();
        app.init_resource::<OrderLog>();
        let mut data = DataRegistry::new();
        Mods::<DefaultSpec>::new()
            .push(LogMod(&MA, "a"))
            .push(LogMod(&MB, "b"))
            .apply(&mut app, &mut data)
            .unwrap();
        assert_eq!(app.world().resource::<OrderLog>().0, vec!["b", "a"]);
    }

    static CONFLICT_A: ModManifest = ModManifest {
        id: "ca",
        version: 1,
        engine_api: 1,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &["cb"],
        exclusive_groups: &[],
        params: &[],
    };
    static CONFLICT_B: ModManifest = ModManifest {
        id: "cb",
        version: 1,
        engine_api: 1,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[],
    };

    #[test]
    fn conflicts_are_rejected() {
        let mut app = app_with_sets();
        let mut data = DataRegistry::new();
        let result = Mods::<DefaultSpec>::new()
            .push(SimpleMod(&CONFLICT_A))
            .push(SimpleMod(&CONFLICT_B))
            .apply(&mut app, &mut data);
        assert!(matches!(result, Err(ModError::Conflict { .. })));
    }

    static MISSING: ModManifest = ModManifest {
        id: "missing",
        version: 1,
        engine_api: 1,
        load_after: &["nonexistent"],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[],
    };

    #[test]
    fn missing_dependency_is_rejected() {
        let mut app = app_with_sets();
        let mut data = DataRegistry::new();
        let result = Mods::<DefaultSpec>::new()
            .push(SimpleMod(&MISSING))
            .apply(&mut app, &mut data);
        assert!(matches!(result, Err(ModError::MissingDependency { .. })));
    }

    static OPTIONAL_GUEST: ModManifest = ModManifest {
        id: "opt_guest",
        version: 1,
        engine_api: 1,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[],
    };
    static OPTIONAL_HOST: ModManifest = ModManifest {
        id: "opt_host",
        version: 1,
        engine_api: 1,
        load_after: &[],
        load_after_optional: &["opt_guest"],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[],
    };

    #[test]
    fn optional_dependency_orders_when_present() {
        let mut app = app_with_sets();
        app.init_resource::<OrderLog>();
        let mut data = DataRegistry::new();
        Mods::<DefaultSpec>::new()
            .push(LogMod(&OPTIONAL_HOST, "host"))
            .push(LogMod(&OPTIONAL_GUEST, "guest"))
            .apply(&mut app, &mut data)
            .unwrap();
        assert_eq!(app.world().resource::<OrderLog>().0, vec!["guest", "host"]);
    }

    static OPTIONAL_ORPHAN: ModManifest = ModManifest {
        id: "opt_orphan",
        version: 1,
        engine_api: 1,
        load_after: &[],
        load_after_optional: &["opt_ghost"],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[],
    };

    #[test]
    fn optional_dependency_missing_is_allowed() {
        let mut app = app_with_sets();
        app.init_resource::<OrderLog>();
        let mut data = DataRegistry::new();
        let hash = Mods::<DefaultSpec>::new()
            .push(LogMod(&OPTIONAL_ORPHAN, "orphan"))
            .apply(&mut app, &mut data)
            .unwrap();
        assert_ne!(hash, 0);
        assert_eq!(app.world().resource::<OrderLog>().0, vec!["orphan"]);
    }

    static GROUP_A: ModManifest = ModManifest {
        id: "group_a",
        version: 1,
        engine_api: 1,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &["renderer"],
        params: &[],
    };
    static GROUP_B: ModManifest = ModManifest {
        id: "group_b",
        version: 1,
        engine_api: 1,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &["renderer"],
        params: &[],
    };

    #[test]
    fn exclusive_group_rejects_second_member() {
        let mut app = app_with_sets();
        let mut data = DataRegistry::new();
        let result = Mods::<DefaultSpec>::new()
            .push(SimpleMod(&GROUP_A))
            .push(SimpleMod(&GROUP_B))
            .apply(&mut app, &mut data);
        match result {
            Err(ModError::ExclusiveGroup { group, a, b }) => {
                assert_eq!(group, "renderer");
                assert_eq!((a.as_str(), b.as_str()), ("group_a", "group_b"));
            }
            other => panic!("期望 ExclusiveGroup，实际 {other:?}"),
        }
    }

    #[test]
    fn exclusive_group_with_one_member_is_allowed() {
        let mut app = app_with_sets();
        let mut data = DataRegistry::new();
        let result = Mods::<DefaultSpec>::new()
            .push(SimpleMod(&GROUP_A))
            .apply(&mut app, &mut data);
        assert!(result.is_ok());
    }

    static CYCLE_A: ModManifest = ModManifest {
        id: "cycle_a",
        version: 1,
        engine_api: 1,
        load_after: &["cycle_b"],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[],
    };
    static CYCLE_B: ModManifest = ModManifest {
        id: "cycle_b",
        version: 1,
        engine_api: 1,
        load_after: &["cycle_a"],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[],
    };

    #[test]
    fn cycle_is_rejected() {
        let mut app = app_with_sets();
        let mut data = DataRegistry::new();
        let result = Mods::<DefaultSpec>::new()
            .push(SimpleMod(&CYCLE_A))
            .push(SimpleMod(&CYCLE_B))
            .apply(&mut app, &mut data);
        assert!(matches!(result, Err(ModError::CircularDependency { .. })));
    }

    #[test]
    fn data_registry_rejects_conflicts_and_sorts() {
        let mut data = DataRegistry::new();
        data.register("mymod", 5, "five").unwrap();
        data.register("mymod", 1, "one").unwrap();
        assert!(data.register("mymod", 5, "other").is_err());
        let ids: Vec<u32> = data.sorted("mymod").iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 5]);
    }

    /// M0：补丁只在优先级足够时生效，且结果可复现。
    #[test]
    fn definition_patch_uses_explicit_priority() {
        let mut data = DataRegistry::new();
        data.register("mymod", 5, "five").unwrap();

        let replaced = data
            .patch(
                DefinitionPatch {
                    domain: "mymod".to_string(),
                    id: 5,
                    name: "five_v2".to_string(),
                },
                OverridePriority::LoadOrder(1),
            )
            .unwrap();
        assert!(replaced.is_some());
        assert_eq!(data.sorted("mymod")[0].name, "five_v2");

        // 更低优先级不得覆盖，且返回 None（保留 v2）。
        let replaced = data
            .patch(
                DefinitionPatch {
                    domain: "mymod".to_string(),
                    id: 5,
                    name: "five_v3".to_string(),
                },
                OverridePriority::Core,
            )
            .unwrap();
        assert!(replaced.is_none());
        assert_eq!(data.sorted("mymod")[0].name, "five_v2");
    }

    static PATCHER: ModManifest = ModManifest {
        id: "patcher",
        version: 1,
        engine_api: 1,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[],
    };

    struct PatchMod;

    impl Mod<DefaultSpec> for PatchMod {
        fn manifest(&self) -> &'static ModManifest {
            &PATCHER
        }
        fn install(&self, context: &mut ModContext<'_, DefaultSpec>) {
            context
                .patch_definition(DefinitionPatch {
                    domain: "example".to_string(),
                    id: 1000,
                    name: "patched".to_string(),
                })
                .unwrap();
        }
    }

    /// M0：mod 经 ModContext 应用补丁（优先级取加载序号，后加载者胜）。
    #[test]
    fn mod_context_patch_definition_applies() {
        let mut app = app_with_sets();
        let mut data = DataRegistry::new();
        data.register("example", 1000, "original").unwrap();
        Mods::<DefaultSpec>::new()
            .push(PatchMod)
            .apply(&mut app, &mut data)
            .unwrap();
        assert_eq!(data.sorted("example")[0].name, "patched");
    }
}
