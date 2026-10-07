//! 静态数据（逻辑层）。
//!
//! 本模块是「对象原型」与「物品定义」两类静态数据的**真相源**；
//! 表现层只消费快照（经 godot-client-ext/src/presentation_bridge.rs 搬运给 Godot）。
//!
//! 依赖方向：game_engine::ids <- static_data/*；static_data <- presentation。
//! **禁止** static_data 依赖 presentation 或 godot。

pub mod item;
pub mod prototype;
/// 体素世界数值：方块调色板 / 材质 / 生物群系 / 体素边长 / LOD 阈值。
pub mod voxel;

use bevy::prelude::*;

use item::ItemTagIndex;

/// 静态数据插件：Startup 时构建只读反查索引（无渲染、无网络依赖）。
pub struct StaticDataPlugin;

impl Plugin for StaticDataPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ItemTagIndex>()
            .add_systems(Startup, build_item_tag_index);
    }
}

/// 从核心表构建 ItemTagIndex（确定性：组内升序、去重）。
fn build_item_tag_index(mut index: ResMut<ItemTagIndex>) {
    *index = ItemTagIndex::build(item::ITEM_TABLE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use item::ITEM_TAG_STACKABLE;

    #[test]
    fn static_data_plugin_builds_tag_index() {
        let mut app = App::new();
        app.add_plugins(StaticDataPlugin);
        app.world_mut().run_schedule(Startup);

        let index = app.world().resource::<ItemTagIndex>();
        assert!(index.tag_count() > 0, "标签索引应非空");
        assert_eq!(index.items_with(ITEM_TAG_STACKABLE).len(), 3);
    }
}
