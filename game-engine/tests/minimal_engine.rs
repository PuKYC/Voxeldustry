//! 验收：game-engine 可脱离 game-core 单独起一个最小 App。
//!
//! 同时验证 DefaultSpec（空动作表 / 空载荷 / 空事件）可用。

use bevy::prelude::*;

use game_engine::input::actions::EmptyActions;
use game_engine::input::InputPlugin;
use game_engine::presentation::EnginePresentationPlugin;
use game_engine::sim::EngineSetPlugin;
use game_engine::spec::{DefaultSpec, NoEvent, NoPayload};

#[test]
fn minimal_app_runs_without_game_core() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(EngineSetPlugin);
    app.add_plugins(InputPlugin::<EmptyActions>::default());
    app.add_plugins(EnginePresentationPlugin::<NoPayload, NoEvent>::default());

    app.update();

    // DefaultSpec 必须满足 GameSpec（编译期即验证）。
    fn assert_spec<S: game_engine::spec::GameSpec>() {}
    assert_spec::<DefaultSpec>();
}
