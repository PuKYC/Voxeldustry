//! 网络适配入口（feature = "net"）。
//!
//! 单人不编译本模块。局域网 / 专用服务器按以下顺序装配：
//!
//! RepliconPlugins（ServerPlugin 或 ClientPlugin + 传输）
//!   -> CoreNetSharedPlugin        // 两端都要
//!   -> CoreNetServerAoiPlugin     // 仅服务端
//!
//! 本模块只做两件事：
//! 1. 注册组件级可见性 filter（CoreRequiredPerception）——两端顺序必须一致；
//! 2. 服务端把 AOI 的 Enter/Leave 翻译成 replicon 的可见性位。
//!
//! AOI 桥必须在 RepliconPlugins 之后添加，否则 AoiVisibilityBit 的
//! FromWorld 拿不到 FilterRegistry / ReplicationRegistry。

use bevy::prelude::*;

use crate::privacy::register_component_visibility;
use game_engine::perception::net::AoiReplicationBridgePlugin;

/// 两端共用：注册 CoreRequiredPerception 的 VisibilityFilter。
pub struct CoreNetSharedPlugin;

impl Plugin for CoreNetSharedPlugin {
    fn build(&self, app: &mut App) {
        register_component_visibility(app);
    }
}

/// 仅服务端：把 AOI 进出视野翻译成 replicon 可见性位。
pub struct CoreNetServerAoiPlugin;

impl Plugin for CoreNetServerAoiPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(AoiReplicationBridgePlugin);
    }
}

/// 服务端便捷组合（共享 filter + AOI 桥）。
pub struct CoreNetServerPlugin;

impl Plugin for CoreNetServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((CoreNetSharedPlugin, CoreNetServerAoiPlugin));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在已初始化 replicon 注册表的 App 上，服务端组合应能完成插件构建
    /// （AoiVisibilityBit::from_world 会分配 FilterBit）。
    #[test]
    fn server_net_plugin_builds_after_registries() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.init_resource::<bevy_replicon::server::visibility::registry::FilterRegistry>();
        app.init_resource::<bevy_replicon::shared::replication::registry::ReplicationRegistry>();
        app.add_plugins(CoreNetServerPlugin);
        app.finish();
        app.cleanup();
    }
}
