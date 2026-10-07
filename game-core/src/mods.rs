//! game-core 的 mod 装配示例与默认集合。
//!
//! - CoreDataMod：M1 数据注册示例；
//! - CoreGameplayMod：M3 系统插入示例。

use bevy::prelude::*;
use game_engine::modding::{DataRegistry, Mod, ModContext, ModManifest, Mods};
use game_engine::sim::EngineSet;

use crate::spec::CoreSpec;

/// 本游戏的 mod 集合类型。
pub type CoreMods = Mods<CoreSpec>;

static CORE_DATA: ModManifest = ModManifest {
    id: "core:data",
    version: 1,
    engine_api: 1,
    load_after: &[],
    conflicts: &[],
};

/// 数据 mod 示例：注册一条运行期定义（M1）。
pub struct CoreDataMod;

impl Mod<CoreSpec> for CoreDataMod {
    fn manifest(&self) -> &'static ModManifest {
        &CORE_DATA
    }

    fn register_data(&self, data: &mut DataRegistry) {
        // 未知冲突返回 Err；示例用 mod 命名空间名字。
        let _ = data.register("example", 1000, "mymod:burning");
    }
}

static CORE_GAMEPLAY: ModManifest = ModManifest {
    id: "core:gameplay",
    version: 1,
    engine_api: 1,
    load_after: &[],
    conflicts: &[],
};

/// 记录 mod 系统运行次数（测试用）。
#[derive(Resource, Default)]
pub struct ModTickCount(pub u32);

fn core_mod_tick(mut count: ResMut<ModTickCount>) {
    count.0 += 1;
}

/// 系统 mod 示例：在 EngineSet::Reaction 阶段插入一个系统（M3）。
pub struct CoreGameplayMod;

impl Mod<CoreSpec> for CoreGameplayMod {
    fn manifest(&self) -> &'static ModManifest {
        &CORE_GAMEPLAY
    }

    fn install(&self, context: &mut ModContext<'_, CoreSpec>) {
        context.add_system(EngineSet::Reaction, core_mod_tick);
    }
}

/// 默认 mod 集合（当前为空，保留装配点）。
pub fn default_core_mods() -> CoreMods {
    CoreMods::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_mod_registers_and_system_mod_runs_in_stage() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(game_engine::sim::EngineSetPlugin);
        app.init_resource::<ModTickCount>();

        let mut data = DataRegistry::new();
        let hash = CoreMods::new()
            .push(CoreDataMod)
            .push(CoreGameplayMod)
            .apply(&mut app, &mut data)
            .expect("mod 集合应加载成功");
        assert_ne!(hash, 0, "mod_set_hash 必须非零");

        assert_eq!(data.sorted("example").len(), 1, "M1 数据应已注册");

        app.finish();
        app.cleanup();
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(
            app.world().resource::<ModTickCount>().0,
            1,
            "M3 系统应在 Reaction 阶段运行一次"
        );
    }
}
