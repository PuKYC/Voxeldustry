//! 世界档插件注册表与 profile 解析。
//!
//! 装配不再是写死的插件清单：每个插件有一个稳定 id、声明它依赖的 id，
//! profile 只给出一组 id；解析器按 cargo feature 的方式自动补齐依赖并按
//! 注册表顺序做确定性拓扑排序，最后逐个调用 `build`。
//!
//! 依赖图必须覆盖代码里的隐式前提（`assert!(app.is_plugin_added::<...>())`、
//! 系统参数需要的资源、`.after(...)` 的目标系统）。注册表本身是唯一来源，
//! 新增插件时同时补 `requires`，不要只在 `add_default_plugins` 里加一行。

use std::collections::{BTreeSet, HashMap, HashSet};

use bevy::prelude::*;

use game_engine::aoi::AoIPlugin;
use game_engine::identity::StableIdPlugin;
use game_engine::spatial::{Size, SpatialPlugin};

use crate::gameplay::{BlockEditPlugin, CameraPlugin, DeathPlugin, GameplayPlugin, MovementPlugin};
use crate::input::GameInputPlugin;
use crate::prediction::PredictionPlugin;
use crate::presentation::voxel_mesh::VoxelMeshRefreshPlugin;
use crate::presentation::{register_synced_components, PresentationPlugin};
use crate::voxel::terrain::{TerrainMode, TerrainPlugin};
use crate::voxel::GameVoxelPlugin;
use crate::world::WorldPlugin;

/// 一个可装配插件的构造器条目。
///
/// `id` 是 profile 里的稳定名字；`requires` 是硬依赖（自动补齐）；
/// `default_on` 决定它是否进入默认 profile；`build` 就是原来
/// `add_default_plugins` 里对应的那几行 `add_plugins`。
///
/// `build` 可以保留运行期条件（例如 streaming 只在 `TerrainMode::Streaming`
/// 时真正装管线）；解析器只负责依赖顺序，不替插件判断是否该装配。
#[derive(Debug, Clone, Copy)]
pub struct PluginEntry {
    pub id: &'static str,
    pub requires: &'static [&'static str],
    pub default_on: bool,
    pub build: fn(&mut App),
}

/// 本次装配实际装上的插件 id，按装配顺序保存。
///
/// 这是注册表与存档之间唯一的桥：存档写入时把它冻结进 `world.meta`，读档时
/// 逐项比对——顺序也是数据，重算会把「注册表变了」降级成静默漂移。
///
/// 直接搭 `App` 的单测没有这个资源：存档侧按空清单落盘，读档侧按空清单放行。
#[derive(Resource, Clone, Debug, PartialEq, Eq, Default)]
pub struct ActivePlugins(Vec<&'static str>);

impl ActivePlugins {
    /// 按装配顺序记录插件 id。
    #[must_use]
    pub fn new(ids: Vec<&'static str>) -> Self {
        Self(ids)
    }

    /// 装配顺序（不是字典序）。
    #[must_use]
    pub fn ids(&self) -> &[&'static str] {
        &self.0
    }
}

const NO_REQUIRES: &[&str] = &[];
const REQUIRES_SPATIAL: &[&str] = &["spatial"];
const REQUIRES_GAMEPLAY: &[&str] = &["gameplay"];
const REQUIRES_BLOCK_EDIT: &[&str] = &["world", "game_voxel", "camera", "gameplay"];
const REQUIRES_GAME_VOXEL_STABLE_ID: &[&str] = &["game_voxel", "stable_id"];
const REQUIRES_STREAMING: &[&str] = &["game_voxel", "terrain", "stable_id", "world"];
const REQUIRES_GAME_VOXEL: &[&str] = &["game_voxel"];
const REQUIRES_PRESENTATION_INPUT: &[&str] = &["presentation", "input"];
const REQUIRES_PRESENTATION: &[&str] = &["presentation"];

/// 全部可装配插件，顺序即同层拓扑排序的 tie-break。
///
/// 顺序刻意保留旧 `add_default_plugins` 的装配次序；解析器只会在依赖允许
/// 的范围内重排，因此同一 profile 每次得到完全相同的插件序列。
pub static PLUGIN_REGISTRY: &[PluginEntry] = &[
    PluginEntry {
        id: "input",
        requires: NO_REQUIRES,
        default_on: true,
        build: build_input,
    },
    PluginEntry {
        id: "presentation",
        requires: NO_REQUIRES,
        default_on: true,
        build: build_presentation,
    },
    PluginEntry {
        id: "stable_id",
        requires: NO_REQUIRES,
        default_on: true,
        build: build_stable_id,
    },
    PluginEntry {
        id: "spatial",
        requires: NO_REQUIRES,
        default_on: true,
        build: build_spatial,
    },
    PluginEntry {
        id: "aoi",
        requires: REQUIRES_SPATIAL,
        default_on: true,
        build: build_aoi,
    },
    PluginEntry {
        id: "gameplay",
        requires: NO_REQUIRES,
        default_on: true,
        build: build_gameplay,
    },
    PluginEntry {
        id: "movement",
        requires: REQUIRES_GAMEPLAY,
        default_on: true,
        build: build_movement,
    },
    PluginEntry {
        id: "death",
        requires: REQUIRES_GAMEPLAY,
        default_on: true,
        build: build_death,
    },
    PluginEntry {
        id: "camera",
        requires: REQUIRES_GAMEPLAY,
        default_on: true,
        build: build_camera,
    },
    PluginEntry {
        id: "block_edit",
        // 系统参数要 VoxelInterner / VoxVolume（game_voxel）；方块编辑的落点
        // 属于世界语义（world）。相机与阶段集是 `.after(camera_system)` 与
        // `GameplaySet::Reaction` 的显式前提。
        requires: REQUIRES_BLOCK_EDIT,
        default_on: true,
        build: build_block_edit,
    },
    PluginEntry {
        id: "world",
        requires: NO_REQUIRES,
        default_on: true,
        build: build_world,
    },
    PluginEntry {
        id: "game_voxel",
        requires: NO_REQUIRES,
        default_on: true,
        build: build_game_voxel,
    },
    PluginEntry {
        id: "terrain",
        // TerrainPlugin 的 build 里有 is_plugin_added::<GameVoxelPlugin>() 断言；
        // Eager 的 Startup 生成还会调用 attach_terrain_body -> attach_island_entity，
        // 后者硬性要求 StableIdAllocator / StableEntityIndex。
        requires: REQUIRES_GAME_VOXEL_STABLE_ID,
        default_on: true,
        build: build_terrain,
    },
    PluginEntry {
        id: "streaming",
        // 文档与 build 断言都要求 GameVoxelPlugin；管线本身依赖 TerrainPlugin
        // 的流式模式，启动引导还要稳定 ID 与 IslandRegistry（WorldPlugin）。
        requires: REQUIRES_STREAMING,
        default_on: true,
        build: build_streaming,
    },
    PluginEntry {
        id: "voxel_mesh_refresh",
        // 刷新系统直接读写 VoxelInterner / VoxDirtySet 与体素分组组件。
        requires: REQUIRES_GAME_VOXEL,
        default_on: true,
        build: build_voxel_mesh_refresh,
    },
    PluginEntry {
        id: "prediction",
        // record_local_history 读 SimulationTick（input）；回滚采样接在表现
        // 管线（presentation）上。
        requires: REQUIRES_PRESENTATION_INPUT,
        default_on: true,
        build: build_prediction,
    },
    PluginEntry {
        id: "sync_register",
        // register_synced_components 往表现管线的 CollectPresentationSet 挂系统，
        // 并依赖 PresentationPlugin 初始化扩展 schema / 资源。
        requires: REQUIRES_PRESENTATION,
        default_on: true,
        build: build_sync_register,
    },
];

// ── 构造器：函数体就是旧 add_default_plugins 里的对应行 ─────────────────────

fn build_input(app: &mut App) {
    app.add_plugins(GameInputPlugin);
}

fn build_presentation(app: &mut App) {
    app.add_plugins(PresentationPlugin);
}

fn build_stable_id(app: &mut App) {
    app.add_plugins(StableIdPlugin);
}

fn build_spatial(app: &mut App) {
    // 空间/AOI 一律用世界位置（GlobalTransform），逻辑位置写在 Transform 上。
    app.add_plugins(SpatialPlugin::<GlobalTransform, Size>::new(32.0));
}

fn build_aoi(app: &mut App) {
    app.add_plugins(AoIPlugin::<GlobalTransform>::default());
}

fn build_gameplay(app: &mut App) {
    app.add_plugins(GameplayPlugin);
}

fn build_movement(app: &mut App) {
    app.add_plugins(MovementPlugin);
}

fn build_death(app: &mut App) {
    app.add_plugins(DeathPlugin);
}

fn build_camera(app: &mut App) {
    app.add_plugins(CameraPlugin);
}

fn build_block_edit(app: &mut App) {
    app.add_plugins(BlockEditPlugin);
}

fn build_world(app: &mut App) {
    app.add_plugins(WorldPlugin);
}

fn build_game_voxel(app: &mut App) {
    app.add_plugins(GameVoxelPlugin);
}

fn build_terrain(app: &mut App) {
    app.add_plugins(TerrainPlugin);
}

fn build_streaming(app: &mut App) {
    // 只有装配方显式选了 TerrainMode::Streaming 才接整条管线；Eager 下
    // 这个 profile 条目仍在序列里，但函数体不装任何插件（旧行为零变化）。
    if app.world().get_resource::<TerrainMode>().copied() == Some(TerrainMode::Streaming) {
        app.add_plugins(crate::world::StreamingWorldPlugin);
    }
}

fn build_voxel_mesh_refresh(app: &mut App) {
    app.add_plugins(VoxelMeshRefreshPlugin);
}

fn build_prediction(app: &mut App) {
    app.add_plugins(PredictionPlugin);
}

fn build_sync_register(app: &mut App) {
    register_synced_components(app);
}

/// profile / 插件依赖解析错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    /// profile 选了注册表里没有的 id。
    UnknownPlugin { id: String },
    /// 某个插件的 `requires` 指向注册表里不存在的 id。
    MissingDependency { plugin: String, dependency: String },
    /// 依赖图有环；`chain` 形如 `a -> b -> a`。
    Cycle { chain: String },
}

impl ProfileError {
    /// 稳定错误码，供 Godot 侧按 code 分支处理（文案可改，code 不可改）。
    pub fn code(&self) -> &'static str {
        match self {
            ProfileError::UnknownPlugin { .. } => "profile.unknown_plugin",
            ProfileError::MissingDependency { .. } => "profile.missing_dependency",
            ProfileError::Cycle { .. } => "profile.cycle",
        }
    }
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProfileError::UnknownPlugin { id } => {
                write!(f, "插件 id {id} 不在插件注册表里")
            }
            ProfileError::MissingDependency { plugin, dependency } => {
                write!(f, "插件 {plugin} 依赖 {dependency}，但注册表里没有这个 id")
            }
            ProfileError::Cycle { chain } => {
                write!(f, "插件依赖存在环：{chain}")
            }
        }
    }
}

impl std::error::Error for ProfileError {}

/// 解析默认 profile：所有 `default_on` 的 id，按注册表顺序。
pub fn default_plugin_selection() -> Vec<&'static str> {
    PLUGIN_REGISTRY
        .iter()
        .filter(|entry| entry.default_on)
        .map(|entry| entry.id)
        .collect()
}

/// 只读查看 profile：默认 profile 去掉会推进玩法状态、产生副作用的插件。
///
/// 只读会话冻结输入与模拟时钟（[crate::input::ReadOnlySession]），但
/// `FixedUpdate` 仍会跑；剔除 gameplay / movement / death / camera / block_edit /
/// prediction 后，世界里不会再有 AI、移动、死亡结算或方块编辑，剩下地形、
/// 流式、表现链负责把存档画出来。
///
/// 依赖不在这里补：调用方把结果交给 [resolve_plugins]，requires 自动补齐。
pub fn readonly_plugin_selection() -> Vec<&'static str> {
    const EXCLUDED: [&str; 6] = [
        "gameplay",
        "movement",
        "death",
        "camera",
        "block_edit",
        "prediction",
    ];
    default_plugin_selection()
        .into_iter()
        .filter(|id| !EXCLUDED.contains(id))
        .collect()
}

/// 把外部（存档/配置/UI）传来的插件 id 解析成注册表内的静态 id。
///
/// 未知 id 返回 [ProfileError::UnknownPlugin]；只做 id 归一，依赖补齐与排序
/// 交给 [`resolve_plugins`]。重复 id 原样保留（解析器本来就幂等去重）。
pub fn intern_plugin_selection(ids: &[String]) -> Result<Vec<&'static str>, ProfileError> {
    // 注册表只有 17 项，线性查够用：不为它引入 HashMap / 懒加载。
    let mut selection = Vec::with_capacity(ids.len());
    for id in ids {
        let entry = PLUGIN_REGISTRY
            .iter()
            .find(|entry| entry.id == id.as_str())
            .ok_or_else(|| ProfileError::UnknownPlugin { id: id.clone() })?;
        selection.push(entry.id);
    }
    Ok(selection)
}

/// 按 profile 的 id 列表解析出要装配的插件，依赖自动补齐并拓扑排序。
///
/// 约束：
/// - 未知 id 报 [ProfileError::UnknownPlugin]；
/// - `requires` 指向注册表外报 [ProfileError::MissingDependency]；
/// - 有环报 [ProfileError::Cycle]（`chain` 给出闭环）；
/// - 同一层（当前入度为 0）按 [PLUGIN_REGISTRY] 顺序取，结果确定；
/// - profile 里重复写同一个 id 只装一次。
pub fn resolve_plugins(selection: &[&str]) -> Result<Vec<&'static PluginEntry>, ProfileError> {
    resolve_with(PLUGIN_REGISTRY, selection)
}

/// 可注入注册表的解析核心（测试用小型表覆盖环 / 缺依赖 / 重复选择）。
fn resolve_with<'a>(
    reg: &'a [PluginEntry],
    selection: &[&str],
) -> Result<Vec<&'a PluginEntry>, ProfileError> {
    let index: HashMap<&str, usize> = reg
        .iter()
        .enumerate()
        .map(|(i, entry)| (entry.id, i))
        .collect();

    // 1. 先校验 profile 本身，未知 id 直接拒绝（不静默忽略）。
    let mut roots = Vec::with_capacity(selection.len());
    for &id in selection {
        let Some(&i) = index.get(id) else {
            return Err(ProfileError::UnknownPlugin { id: id.to_string() });
        };
        roots.push(i);
    }

    // 2. DFS 收集闭包，同时检测缺失依赖与环。
    let mut state = vec![0u8; reg.len()]; // 0=未访问 1=访问中 2=已完成
    let mut included = vec![false; reg.len()];
    let mut stack = Vec::new();
    for root in roots {
        visit(reg, &index, &mut state, &mut included, &mut stack, root)?;
    }

    // 3. Kahn 拓扑排序；同层取注册表下标最小者，保证确定性。
    let mut indegree = vec![0usize; reg.len()];
    let mut dependents = vec![Vec::new(); reg.len()];
    for i in 0..reg.len() {
        if !included[i] {
            continue;
        }
        let mut seen = HashSet::new();
        for &dependency in reg[i].requires {
            if !seen.insert(dependency) {
                continue;
            }
            let dependency_index = index[dependency];
            indegree[i] += 1;
            dependents[dependency_index].push(i);
        }
    }

    let mut ready: BTreeSet<usize> = (0..reg.len())
        .filter(|&i| included[i] && indegree[i] == 0)
        .collect();
    let mut ordered = Vec::with_capacity(included.iter().filter(|&&yes| yes).count());
    while let Some(&i) = ready.iter().next() {
        ready.remove(&i);
        ordered.push(&reg[i]);
        for &dependent in &dependents[i] {
            indegree[dependent] -= 1;
            if indegree[dependent] == 0 {
                ready.insert(dependent);
            }
        }
    }

    if ordered.len() != included.iter().filter(|&&yes| yes).count() {
        // DFS 已先行检测环，这里只是防御性兜底（例如未来改解析器时漏了）。
        let chain = (0..reg.len())
            .filter(|&i| included[i] && indegree[i] > 0)
            .map(|i| reg[i].id)
            .collect::<Vec<_>>()
            .join(" -> ");
        return Err(ProfileError::Cycle { chain });
    }

    Ok(ordered)
}

fn visit(
    reg: &[PluginEntry],
    index: &HashMap<&str, usize>,
    state: &mut [u8],
    included: &mut [bool],
    stack: &mut Vec<usize>,
    node: usize,
) -> Result<(), ProfileError> {
    match state[node] {
        1 => {
            let start = stack.iter().position(|&n| n == node).unwrap_or(0);
            let mut chain: Vec<&str> = stack[start..].iter().map(|&i| reg[i].id).collect();
            chain.push(reg[node].id);
            return Err(ProfileError::Cycle {
                chain: chain.join(" -> "),
            });
        }
        2 => return Ok(()),
        _ => {}
    }

    state[node] = 1;
    stack.push(node);
    included[node] = true;

    for &dependency in reg[node].requires {
        let Some(&dependency_index) = index.get(dependency) else {
            return Err(ProfileError::MissingDependency {
                plugin: reg[node].id.to_string(),
                dependency: dependency.to_string(),
            });
        };
        visit(reg, index, state, included, stack, dependency_index)?;
    }

    stack.pop();
    state[node] = 2;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop(_: &mut App) {}

    const TEST_NONE: &[&str] = &[];
    const TEST_A: &[&str] = &["a"];
    const TEST_B: &[&str] = &["b"];
    const TEST_GHOST: &[&str] = &["ghost"];

    static TEST_CYCLE: &[PluginEntry] = &[
        PluginEntry {
            id: "a",
            requires: TEST_B,
            default_on: true,
            build: noop,
        },
        PluginEntry {
            id: "b",
            requires: TEST_A,
            default_on: true,
            build: noop,
        },
    ];

    static TEST_MISSING: &[PluginEntry] = &[PluginEntry {
        id: "a",
        requires: TEST_GHOST,
        default_on: false,
        build: noop,
    }];

    static TEST_ORDER: &[PluginEntry] = &[
        PluginEntry {
            id: "a",
            requires: TEST_NONE,
            default_on: true,
            build: noop,
        },
        PluginEntry {
            id: "b",
            requires: TEST_A,
            default_on: true,
            build: noop,
        },
        PluginEntry {
            id: "c",
            requires: TEST_A,
            default_on: true,
            build: noop,
        },
        PluginEntry {
            id: "d",
            requires: TEST_B,
            default_on: false,
            build: noop,
        },
    ];

    #[test]
    fn registry_ids_are_unique() {
        let mut ids = HashSet::new();
        for entry in PLUGIN_REGISTRY {
            assert!(ids.insert(entry.id), "插件 id 重复：{}", entry.id);
        }
    }

    #[test]
    fn registry_requires_point_to_known_ids() {
        let ids: HashSet<&str> = PLUGIN_REGISTRY.iter().map(|entry| entry.id).collect();
        for entry in PLUGIN_REGISTRY {
            for dependency in entry.requires {
                assert!(
                    ids.contains(dependency),
                    "插件 {} 的 requires 指向不存在的 id：{dependency}",
                    entry.id
                );
            }
        }
    }

    #[test]
    fn plugin_does_not_require_itself() {
        for entry in PLUGIN_REGISTRY {
            assert!(
                !entry.requires.contains(&entry.id),
                "插件 {} 不能依赖自己",
                entry.id
            );
        }
    }

    #[test]
    fn default_selection_is_all_default_on_entries_in_registry_order() {
        let expected: Vec<&str> = PLUGIN_REGISTRY
            .iter()
            .filter(|entry| entry.default_on)
            .map(|entry| entry.id)
            .collect();
        assert_eq!(default_plugin_selection(), expected);
    }

    #[test]
    fn readonly_selection_drops_play_simulation_plugins() {
        let readonly = readonly_plugin_selection();
        for excluded in [
            "gameplay",
            "movement",
            "death",
            "camera",
            "block_edit",
            "prediction",
        ] {
            assert!(
                !readonly.contains(&excluded),
                "只读 profile 不应装 {excluded}"
            );
        }
        for kept in ["input", "presentation", "terrain", "streaming", "world"] {
            assert!(readonly.contains(&kept), "只读 profile 必须保留 {kept}");
        }
        // 依赖由解析器补齐；只读 profile 本身也必须可解析。
        resolve_plugins(&readonly).expect("只读 profile 必须可解析");
    }

    #[test]
    fn default_selection_resolves_and_satisfies_dependencies() {
        let resolved =
            resolve_plugins(&default_plugin_selection()).expect("默认 profile 必须可解析");
        assert_eq!(resolved.len(), default_plugin_selection().len());

        let position: HashMap<&str, usize> = resolved
            .iter()
            .enumerate()
            .map(|(i, entry)| (entry.id, i))
            .collect();
        for (i, entry) in resolved.iter().enumerate() {
            for dependency in entry.requires {
                let dependency_position = position
                    .get(dependency)
                    .unwrap_or_else(|| panic!("依赖 {dependency} 未进入解析结果"));
                assert!(
                    *dependency_position < i,
                    "插件 {} 排在其依赖 {dependency} 之前",
                    entry.id
                );
            }
        }
    }

    #[test]
    fn unknown_plugin_is_rejected() {
        assert_eq!(
            resolve_with(TEST_ORDER, &["does_not_exist"]).unwrap_err(),
            ProfileError::UnknownPlugin {
                id: "does_not_exist".to_string()
            }
        );
    }

    #[test]
    fn missing_dependency_is_rejected() {
        assert_eq!(
            resolve_with(TEST_MISSING, &["a"]).unwrap_err(),
            ProfileError::MissingDependency {
                plugin: "a".to_string(),
                dependency: "ghost".to_string()
            }
        );
    }

    #[test]
    fn cycle_is_rejected_with_closed_chain() {
        assert_eq!(
            resolve_with(TEST_CYCLE, &["a"]).unwrap_err(),
            ProfileError::Cycle {
                chain: "a -> b -> a".to_string()
            }
        );
    }

    #[test]
    fn duplicate_selection_is_deduplicated() {
        let resolved = resolve_with(TEST_ORDER, &["a", "a", "b"]).expect("重复选择必须可解析");
        let ids: Vec<&str> = resolved.iter().map(|entry| entry.id).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn requires_are_added_automatically() {
        let resolved = resolve_with(TEST_ORDER, &["c"]).expect("依赖必须自动补齐");
        let ids: Vec<&str> = resolved.iter().map(|entry| entry.id).collect();
        assert_eq!(ids, vec!["a", "c"]);
    }

    #[test]
    fn same_layer_keeps_registry_order() {
        // 选择顺序反过来也不影响结果：a 先就绪，b/c 同层按注册表顺序。
        let resolved = resolve_with(TEST_ORDER, &["c", "b", "a"]).expect("依赖必须自动补齐");
        let ids: Vec<&str> = resolved.iter().map(|entry| entry.id).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }
}
