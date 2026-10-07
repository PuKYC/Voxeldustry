//! GameSpec：一个游戏给引擎的类型绑定。
//!
//! 所有「换游戏就需要换」的类型都收在这里；引擎内部只认这些关联类型。
//! 遵守 rule of two：只有出现第二种真实实现、或明确要由 mod 替换的接缝才泛型化。

use bevy::prelude::Resource;

use crate::input::actions::{ActionMap, EmptyActions};
use crate::presentation::event::PresentationEventData;
use crate::presentation::packed::{PackedError, PackedPayload, Reader};
use crate::presentation::payload::{PayloadKindTrait, PresentationPayload};

/// 一个游戏给引擎的类型绑定。
pub trait GameSpec: Send + Sync + 'static {
    /// 输入动作表（名字 -> 通道位）。
    type Actions: ActionMap;
    /// 表现载荷集合（由 define_payloads! 生成）。
    type Payload: PresentationPayload;
    /// 表现事件集合。
    type Event: PresentationEventData;
    /// 运行期语义注册表（mod 基石）。引擎不关心其内部形状。
    type Semantics: Resource + Clone + Send + Sync + 'static;
    /// 逻辑定步频率。
    const FIXED_HZ: u32;
}

/// 空载荷值（默认绑定用）。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NoPayloadValue;

/// 空载荷集合（默认绑定用）。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum NoPayload {
    None(NoPayloadValue),
}

/// 空载荷种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoKind {
    None = 0,
}

impl PayloadKindTrait for NoKind {
    const ALL: &'static [Self] = &[NoKind::None];
    fn as_str(self) -> &'static str {
        "none"
    }
    fn code(self) -> u8 {
        0
    }
    fn from_code(code: u8) -> Option<Self> {
        if code == 0 {
            Some(NoKind::None)
        } else {
            None
        }
    }
    fn is_perception_gated(self) -> bool {
        false
    }
}

impl PackedPayload for NoPayloadValue {
    fn put_body(&self, _out: &mut Vec<u8>) {}
    fn read_body(
        _r: &mut Reader<'_>,
        _f: &mut Vec<f32>,
        _i: &mut Vec<i64>,
    ) -> Result<(), PackedError> {
        Ok(())
    }
    fn write_pools(&self, _f: &mut Vec<f32>, _i: &mut Vec<i64>) {}
    fn from_pools(_f: &[f32], _i: &[i64], _fo: usize, _io: usize) -> Self {
        NoPayloadValue
    }
}

impl PresentationPayload for NoPayload {
    type Kind = NoKind;
    fn kind(&self) -> NoKind {
        NoKind::None
    }
    fn put(&self, out: &mut Vec<u8>) {
        out.push(0);
        NoPayloadValue.put_body(out);
    }
    fn read_into(
        r: &mut Reader<'_>,
        f: &mut Vec<f32>,
        i: &mut Vec<i64>,
    ) -> Result<NoKind, PackedError> {
        let code = r.u8()?;
        let kind = NoKind::from_code(code).ok_or(PackedError::BadPayloadKind(code))?;
        NoPayloadValue::read_body(r, f, i)?;
        Ok(kind)
    }
    fn write_pools(&self, f: &mut Vec<f32>, i: &mut Vec<i64>) {
        NoPayloadValue.write_pools(f, i);
    }
    fn from_pools(_kind: NoKind, _f: &[f32], _i: &[i64], _fo: usize, _io: usize) -> Self {
        NoPayload::None(NoPayloadValue)
    }
}

/// 空语义注册表（默认绑定用）。
#[derive(Resource, Clone, Default, Debug)]
pub struct NoSemantics;

/// 空事件（默认绑定用）。
#[derive(Clone, Debug)]
pub struct NoEvent;

impl PresentationEventData for NoEvent {
    fn kind_str(&self) -> &'static str {
        "none"
    }
    fn entity(&self) -> crate::identity::StableEntityId {
        crate::identity::StableEntityId(0)
    }
}

/// 默认绑定：给不需要替换的最小游戏 / 引擎自测用。
pub struct DefaultSpec;

impl GameSpec for DefaultSpec {
    type Actions = EmptyActions;
    type Payload = NoPayload;
    type Event = NoEvent;
    type Semantics = NoSemantics;
    const FIXED_HZ: u32 = crate::sim::DEFAULT_FIXED_HZ;
}
