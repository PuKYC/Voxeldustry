//! replicon 网络适配：AOI 可见性位与 AOI 进出视野事件 -> ClientVisibility 的桥。
//!
//! 仅在 feature = "net" 下编译；引擎默认不依赖 bevy_replicon。

use bevy::prelude::*;
use bevy_replicon::{
    server::visibility::{
        client_visibility::ClientVisibility, filters_mask::FilterBit, registry::FilterRegistry,
    },
    shared::replication::{registry::ReplicationRegistry, visibility::ScopeLifetime},
};

use crate::aoi::{AoiSystems, EntityEntered, EntityLeft};

/// AOI 在 replicon 里占用的可见性位。
///
/// 使用 Newtype 模式包装 `FilterBit`，提供严格的类型安全，
/// 防止在复杂的网络同步逻辑中与其他 Filter 的 Bit 混淆。
///
/// 注：经源码核实，`bevy_replicon` 中的 `FilterBit` 底层为 `u8` 且派生了 `Copy`，
/// 因此这里派生 `Copy` 是安全的，且在系统循环中按值传递不会产生 move 错误。
#[derive(Resource, Clone, Copy, Debug)]
pub struct AoiVisibilityBit(pub FilterBit);

impl AoiVisibilityBit {
    /// 获取底层的 `FilterBit`。
    #[inline]
    pub fn bit(self) -> FilterBit {
        self.0
    }
}

/// 核心机制：通过 `FromWorld` 在资源初始化阶段动态申请 Bit。
///
/// 这保证了只要 C/S 两端使用相同的 plugin 添加顺序和相同的代码版本，
/// 就能从 `FilterRegistry` 拿到严格一致的 Bit Index，避免因注册时机不同而错位。
/// （注：跨进程一致性依赖于两端构建产物和插件顺序的绝对一致，本协议层代码
/// 负责保证"同进程内分配时机的确定性"，从而为跨进程一致提供基础。）
impl FromWorld for AoiVisibilityBit {
    fn from_world(world: &mut World) -> Self {
        // 使用 remove_resource 避免同时借用两个 Mut 资源导致编译报错
        let mut filter_registry = world.remove_resource::<FilterRegistry>().expect(
            "AoiReplicationBridgePlugin 必须在 RepliconPlugins (或初始化 FilterRegistry 的插件) 之后添加，\
             否则 FilterRegistry 尚未注册，AOI 的 FilterBit 无法分配",
        );
        let mut replication_registry = world
            .remove_resource::<ReplicationRegistry>()
            .expect("ReplicationRegistry 尚未注册，请确保 RepliconPlugins 已添加");

        let bit = filter_registry.register_scope::<Entity>(
            world,
            &mut replication_registry,
            ScopeLifetime::WhileVisible,
        );

        // 将资源放回 World
        world.insert_resource(filter_registry);
        world.insert_resource(replication_registry);

        Self(bit)
    }
}

/// 把 AOI 可见性位注册进 replicon，并在 PostUpdate 桥接 Enter/Leave 事件。
pub struct AoiReplicationBridgePlugin;

impl Plugin for AoiReplicationBridgePlugin {
    fn name(&self) -> &str {
        "AoiReplicationBridgePlugin"
    }

    fn build(&self, app: &mut App) {
        // 使用 init_resource 触发 FromWorld。
        // 必须在 RepliconPlugins 之后添加，否则 FromWorld 内的 expect 会 panic。
        app.init_resource::<AoiVisibilityBit>().add_systems(
            PostUpdate,
            apply_aoi_to_client_visibility
                // 必须在 aoi.rs 产出本帧 Enter/Leave 事件之后、
                // replicon 的发送系统 (`ServerSystems::Send`) 之前执行。
                .after(AoiSystems)
                .before(bevy_replicon::server::ServerSystems::Send),
        );
    }
}

/// 把 aoi.rs 算出来的 Enter/Leave 事件，翻译成 replicon 的可见性位更新。
///
/// 这一步是 AOI 与网络层之间唯一的耦合点：aoi.rs 本身完全不知道
/// replicon 的存在，只管算"谁该看见谁"；网络语义（Spawn/Despawn、
/// 增量重发）全部交给 replicon 内建处理，不再自己拼包。
fn apply_aoi_to_client_visibility(
    aoi_bit: Res<AoiVisibilityBit>,
    mut entered: MessageReader<EntityEntered>,
    mut left: MessageReader<EntityLeft>,
    mut clients: Query<&mut ClientVisibility>,
) {
    // FilterBit 实现了 Copy，这里按值提取并在后续两个循环中安全复用。
    let bit = aoi_bit.bit();

    for ev in entered.read() {
        if let Ok(mut visibility) = clients.get_mut(ev.observer) {
            visibility.set(ev.entity, bit, true);
        }
    }

    for ev in left.read() {
        if let Ok(mut visibility) = clients.get_mut(ev.observer) {
            visibility.set(ev.entity, bit, false);
        }
    }
}
