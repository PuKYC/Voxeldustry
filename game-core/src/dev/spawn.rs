//! 玩法实体装配样板收口。
//!
//! 「本地玩家」需要十几个组件，且受 Bevy 元组 Bundle 的 15 元素上限约束，
//! 必须分两次 insert；稳定 ID 索引与输入源实体还要各补一步。`dev/demo.rs` 与
//! `dev/perf.rs` 各抄过一份，这里用宏收口：调用方只需给出位置、输入源、
//! 观察者半径与场景特有的额外组件。

/// 生成一个「本地玩家 + 输入源」装配，返回 `(Entity, StableEntityId)`。
///
/// - 第一批：身份 / 空间 / 表现基础组件（`spawn_stable`，当帧建立稳定 ID 映射）；
/// - 第二批：输入能力通道 + 快照缓冲 + 表现插值端点 + `$extra`；
/// - 随后补一个 `InputSource` 指向玩家，并再保险一次稳定 ID 索引。
///
/// `extra` 放场景特有组件；当前调用方都传空元组 `()`。
macro_rules! spawn_local_player {
    (
        $world:expr,
        position = $position:expr,
        source = $source:expr,
        observer = $observer:expr,
        extra = ($($extra:expr),* $(,)?),
    ) => {{
        let position: Vec3 = $position;

        let (entity, id) = $world.spawn_stable((
            Transform::from_translation(position),
            // `Size` 是进空间索引（也就是 AOI 可见性）的必要条件。
            game_engine::spatial::Size(None),
            crate::static_data::prototype::Prototype::new(1),
            crate::presentation::payload::PresentationState::idle(),
            crate::input::control::LocalPlayer,
            crate::privacy::PerceptionMask(0b0001),
            game_engine::aoi::AoiObserver::new($observer),
            crate::input::control::ControlledBy { source: $source },
        ));

        $world.entity_mut(entity).insert((
            crate::input::capability::MoveChannel,
            crate::input::capability::JumpChannel,
            crate::input::capability::InteractChannel::default(),
            crate::input::capability::ToolUseChannel,
            crate::input::snapshot::RawResolved::default(),
            crate::input::snapshot::ResolvedInput::default(),
            crate::input::capability::InputAvailability::default(),
            crate::input::snapshot::InputBuffer::default(),
            // 表现侧：位置插值端点（每 tick 推进，Godot 在渲染时刻采样）。
            crate::presentation::PresentedTransform::default(),
            // 逻辑速度 + 回滚/插值历史（Snapshot 由 record_local_history 每 tick 写入）。
            crate::gameplay::Velocity { linear: Vec3::ZERO },
            game_engine::rollback::RenderHistory::<crate::prediction::Snapshot>::predicted(
                crate::prediction::HISTORY_CAPACITY,
            ),
            $($extra,)*
        ));

        // 输入源实体：把 Godot 的输入通道指向刚建好的玩家。
        $world.spawn((
            crate::input::control::InputSource { id: $source },
            crate::input::control::ControlsEntity { target: id },
        ));

        // spawn_stable 已建立索引；这里再保险一次，避免插件顺序变化影响。
        $world
            .resource_mut::<game_engine::identity::StableEntityIndex>()
            .insert(id, entity);

        (entity, id)
    }};
}

pub(crate) use spawn_local_player;
