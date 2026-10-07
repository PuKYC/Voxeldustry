//! 输入源与控制关系（架构：输入篇「输入源实体化 + 控制关系组件化」）。

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::identity::StableEntityId;

/// 输入源标识（单人=本地玩家控制器；联机=`Client_N`）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct InputSourceId(pub u64);

/// 输入源实体。
///
/// 把「谁在输入」实体化，而不是藏在全局变量里。
#[derive(Component, Clone, Copy, Debug)]
pub struct InputSource {
    pub id: InputSourceId,
}

/// 控制关系：输入源控制哪个实体。
///
/// 有了它，「附身元素傀儡」「驾驶载具」「遥控装置」都不需要改输入架构 ——
/// 只改这一个组件指向谁。
#[derive(Component, Clone, Copy, Debug)]
pub struct ControlsEntity {
    pub target: StableEntityId,
}

/// 反向：这个实体被哪个输入源控制。
#[derive(Component, Clone, Copy, Debug)]
pub struct ControlledBy {
    pub source: InputSourceId,
}

/// 标记组件：本地权限主体。
///
/// 表现层的可见性过滤用它定位本地玩家，**不依赖 `AoIManager` 内部状态**
/// （否则「本地玩家」与「AOI 观察者」两个概念会耦合死）。
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct LocalPlayer;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_relation_is_copyable_data() {
        let controls = ControlsEntity {
            target: StableEntityId(7),
        };
        let copy = controls;
        assert_eq!(copy.target.0, 7);
        assert_eq!(copy.target, controls.target);
    }
}
