//! CoreGame：game-core 的默认装配（GameModule<CoreSpec>）。
//!
//! 局域网 / 客户端 / 服务端是不同的 GameModule 组合；mod 可在其之上追加。

use bevy::prelude::*;

use game_engine::backend::GameModule;

use crate::gameplay::{DeathPlugin, GameplayPlugin, MovementPlugin};
use crate::input::GameInputPlugin;
use crate::prediction::PredictionPlugin;
use crate::presentation::{register_synced_components, PresentationPlugin};
use crate::spec::CoreSpec;
use crate::static_data::StaticDataPlugin;
use crate::world::WorldPlugin;
use game_engine::aoi::AoIPlugin;
use game_engine::identity::StableIdPlugin;
use game_engine::spatial::{Size, SpatialPlugin};

/// 本游戏的默认插件组合。
pub struct CoreGame;

impl GameModule<CoreSpec> for CoreGame {
    fn build(&self, app: &mut App) {
        app.add_plugins(StaticDataPlugin);
        app.add_plugins(GameInputPlugin);
        app.add_plugins(PresentationPlugin);
        // 逻辑层基础：稳定 ID / 空间索引 / AOI。生产同样需要。
        app.add_plugins((
            StableIdPlugin,
            // 空间/AOI 一律用世界位置（GlobalTransform），逻辑位置写在 Transform 上。
            SpatialPlugin::<GlobalTransform, Size>::new(32.0),
            AoIPlugin::<GlobalTransform>::default(),
        ));
        // 玩法层装配。
        app.add_plugins((GameplayPlugin, MovementPlugin, DeathPlugin));
        // 体素世界层：游戏语义 / 数值 / 生成。v1 无执行。
        app.add_plugins(WorldPlugin);
        // 预测 / 回滚绑定（单人：全部 Predicted、delay 0、无重演）。
        app.add_plugins(PredictionPlugin);
        register_synced_components(app);
    }
}
