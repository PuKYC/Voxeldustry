//! M5：可替换的引擎策略（位移模型等）。
//!
//! 引擎只提供「策略槽 + 契约」，默认实现留在游戏侧，避免把玩法数值写进引擎。
//!
//! 确定性约束：
//! - 策略是 Resource，mod 通过 ModContext::set_strategy 在冷路径整体替换；
//! - 策略方法不得读取时钟 / 线程 / 全局可变状态。

use std::marker::PhantomData;

use bevy::prelude::*;

use crate::modding::Strategy;
use crate::spec::GameSpec;

/// 位移积分模型（M5）。纯函数式。
///
/// 位置/速度统一使用 Bevy 的 `Vec3`（f32）；逻辑位置由 `Transform` 承载。
pub trait MovementModel: Send + Sync + 'static {
    /// 位置积分：给定当前位置与速度，返回本 tick 的新位置。
    fn integrate(&self, translation: Vec3, velocity: Vec3, dt: f32) -> Vec3;
    /// 速度阻尼：给定当前速度，返回本 tick 的新速度。
    fn damp(&self, velocity: Vec3, dt: f32) -> Vec3;
}

/// 运行期位移策略槽。
///
/// 引擎系统只读这个 Resource；mod 用
/// ModContext::set_strategy(MovementStrategy::<S>::new(MyModel))
/// 在启动期整体替换，不需要从调度里抠系统。
#[derive(Resource)]
pub struct MovementStrategy<S: GameSpec> {
    inner: Box<dyn MovementModel>,
    _spec: PhantomData<S>,
}

impl<S: GameSpec> Strategy<S> for MovementStrategy<S> {}

impl<S: GameSpec> MovementStrategy<S> {
    pub fn new<M: MovementModel>(model: M) -> Self {
        Self {
            inner: Box::new(model),
            _spec: PhantomData,
        }
    }

    #[inline]
    pub fn integrate(&self, translation: Vec3, velocity: Vec3, dt: f32) -> Vec3 {
        self.inner.integrate(translation, velocity, dt)
    }

    #[inline]
    pub fn damp(&self, velocity: Vec3, dt: f32) -> Vec3 {
        self.inner.damp(velocity, dt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::DefaultSpec;

    #[derive(Debug, Default)]
    struct FrozenModel;

    impl MovementModel for FrozenModel {
        fn integrate(&self, translation: Vec3, _velocity: Vec3, _dt: f32) -> Vec3 {
            translation
        }
        fn damp(&self, velocity: Vec3, _dt: f32) -> Vec3 {
            velocity
        }
    }

    #[test]
    fn slot_dispatches_to_installed_model() {
        let mut app = App::new();
        app.insert_resource(MovementStrategy::<DefaultSpec>::new(FrozenModel));
        let strategy = app.world().resource::<MovementStrategy<DefaultSpec>>();

        let t = Vec3::new(3.0, 4.0, 5.0);
        let v = Vec3::new(1.0, 1.0, 1.0);
        assert_eq!(strategy.integrate(t, v, 1.0), t);
        assert_eq!(strategy.damp(v, 1.0), v);
    }
}
