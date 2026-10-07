//! 表现载荷的机制层：载荷集合由游戏侧用 define_payloads! 实例化。
//!
//! **决策记录：不用 Box<dyn Any> + downcast。** 载荷是显式 enum，热路径
//! 是 match（零成本、可调试、可 serde）。引擎只认识 [PresentationPayload]、  
//! [PayloadKindTrait] 两个契约，不写死任何具体载荷。

use serde::{Deserialize, Serialize};

use super::packed::{PackedError, Reader};

/// 载荷种类契约：字节码、稳定标签、门控标记、全量集合。
///
/// 实现由 [define_payloads!] 生成。
pub trait PayloadKindTrait:
    Copy + Eq + core::hash::Hash + core::fmt::Debug + Send + Sync + 'static
{
    /// 全部种类的稳定顺序表。
    const ALL: &'static [Self];

    /// 稳定字符串标签（给 Godot 侧 match 用，不依赖变体顺序）。
    fn as_str(self) -> &'static str;

    /// GPF1 字节码 / Godot 端 ABI 值。
    fn code(self) -> u8;

    /// 字节码 -> 种类；未知码返回 None（绝不 panic）。
    fn from_code(code: u8) -> Option<Self>;

    /// 是否受组件级可见性门控。
    fn is_perception_gated(self) -> bool;
}

/// 一个完整的表现载荷集合。
///
/// 所有会下发给 Godot 的组件，最终都翻译成实现本 trait 的某个 enum。
/// clean 热路径：kind/put/read/from_pools 全是静态分发。
pub trait PresentationPayload:
    Clone + core::fmt::Debug + PartialEq + Serialize + for<'de> Deserialize<'de> + Send + Sync + 'static
{
    /// 对应的种类枚举。
    type Kind: PayloadKindTrait;

    fn kind(&self) -> Self::Kind;

    /// 写入线格式（含 kind 码）。
    fn put(&self, out: &mut Vec<u8>);

    /// 解码快通道：直接写 SoA 池，不构造中间产物。
    fn read_into(
        reader: &mut Reader<'_>,
        f: &mut Vec<f32>,
        i: &mut Vec<i64>,
    ) -> Result<Self::Kind, PackedError>;

    /// Dictionary 兼容通道：从 SoA 池还原。
    fn from_pools(kind: Self::Kind, f: &[f32], i: &[i64], fo: usize, io: usize) -> Self;

    /// 直接把载荷写进 SoA 池（不经过字节）。见 `PackedPayload::write_pools`。
    fn write_pools(&self, f: &mut Vec<f32>, i: &mut Vec<i64>);
}

// ─────────────────── 宏内部辅助（按 token 有无生成字面量） ───────────────────

/// 内部辅助：有 `private: <Ty>` 子句时生成 `true`，否则生成 `false`。
///
/// 供 [define_payloads!] 展开 `is_perception_gated` 用；不面向业务。
#[doc(hidden)]
#[macro_export]
macro_rules! __payload_is_private {
    () => {
        false
    };
    ($private:ty) => {
        true
    };
}

/// 内部辅助：有 `private: <Ty>` 子句时生成 `Some(stringify!(Ty))`，否则 `None`。
///
/// 供 [define_payload_privacy!] 生成隐私清单用；不面向业务。
#[doc(hidden)]
#[macro_export]
macro_rules! __payload_private_name {
    () => {
        None
    };
    ($private:ty) => {
        Some(stringify!($private))
    };
}

/// 只生成隐私清单 `PRIVACY_COMPONENTS`，不展开任何载荷定义。
///
/// 调用形如：
///
/// ```ignore
/// define_payload_privacy! {
///     RenderTransformSample;
///     PresentationState;
///     PresentedHealth, private: ExactHealth;
///     PresentedVisibility;
///     InteractionHint;
/// }
/// ```
///
/// 每条是「载荷类型名」，可选的 `, private: <ComponentTy>` 表示该载荷受该隐私
/// 组件门控。需要完整载荷定义时用 [define_payloads!]；本宏供只想消费隐私清单、
/// 不想重复展开载荷定义的调用方使用。
///
/// 注意：**privacy-only 组件（无独立载荷）不在此表内**，需由业务层另行声明；
/// 且本宏已由 [define_payloads!] 内含调用，二者不要在同一模块同时展开，否则
/// `PRIVACY_COMPONENTS` 会重复定义。
#[macro_export]
macro_rules! define_payload_privacy {
    (
        $(
            $payload:ty $(, private: $private:ty)? ;
        )*
    ) => {
        /// 载荷 -> 隐私组件映射清单（由 `define_payloads!` 生成）。
        ///
        /// 每项为 `(载荷类型名, 该载荷的隐私门控组件)`；第二个为 `None` 表示
        /// 该载荷不受组件级可见性门控。供业务层与 `FilterScope`
        /// 等隐私清单做契约对齐，防止门控清单漂移。
        pub const PRIVACY_COMPONENTS: &[(&str, Option<&str>)] = &[
            $(
                (
                    stringify!($payload),
                    $crate::__payload_private_name!( $( $private )? ),
                ),
            )*
        ];
    };
}

// ─────────────────── 载荷定义宏 ───────────────────

/// 生成载荷种类枚举、载荷 enum，以及上述 trait 的静态分发实现。
///
/// 调用形如：
///
/// ```ignore
/// define_payloads! {
///     PayloadKind, SyncPayload;
///     Transform = 0, "transform", payload: RenderTransformSample;
///     Health    = 2, "health", private: ExactHealth, payload: PresentedHealth;
/// }
/// ```
///
/// 分号前两个 ident 依次是要生成的「种类枚举」与「载荷 enum」的类型名；宏体内一律
/// 使用传入的 ident，不再硬编码具体类型名，因此同一 crate 可用不同名字实例化。
/// 每个 crate 只应在表现载荷模块调用一次。
///
/// 每条载荷可选的 `private: <ComponentTy>` 子句表示该载荷受组件级可见性门控，
/// 且 `<ComponentTy>` 是本游戏的隐私组件；省略即不受门控。门控清单以
/// `PRIVACY_COMPONENTS` 常量形式与载荷定义同源生成（见 [define_payload_privacy!]）。
///
/// code 只能末尾追加，禁止重排 / 复用 —— 它同时是线格式与 Godot 端 ABI。
#[macro_export]
macro_rules! define_payloads {
    (
        $kind:ident, $payload_enum:ident;
        $(
            // `private:` 子句在 `payload:` 之前，两者共用标签后的那个逗号；
            // 可选组以 `private` 开头、后续以 `payload` 开头，首个 token 不同，
            // 因此不存在宏匹配的局部歧义。
            $variant:ident = $code:literal, $label:literal, $(private: $private:ty,)? payload: $payload:ty;
        )*
    ) => {
        #[repr(u8)]
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
        pub enum $kind { $( $variant = $code, )* }

        impl $kind {
            pub const ALL: &'static [$kind] = &[ $( $kind::$variant, )* ];

            pub const fn as_str(self) -> &'static str {
                match self { $( $kind::$variant => $label, )* }
            }

            pub const fn code(self) -> u8 { self as u8 }

            pub const fn from_code(code: u8) -> Option<Self> {
                match code { $( $code => Some($kind::$variant), )* _ => None }
            }

            /// 是否受组件级可见性门控：带 `private:` 子句的变体为 true。
            pub const fn is_perception_gated(self) -> bool {
                match self {
                    $( $kind::$variant => $crate::__payload_is_private!( $( $private )? ), )*
                }
            }
        }

        #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        pub enum $payload_enum { $( $variant($payload), )* }

        impl $payload_enum {
            pub fn kind(&self) -> $kind {
                match self { $( $payload_enum::$variant(_) => $kind::$variant, )* }
            }

            pub fn type_name(&self) -> &'static str { self.kind().as_str() }

            pub fn put(&self, out: &mut Vec<u8>) {
                out.push($kind::code(self.kind()));
                match self { $( $payload_enum::$variant(v) =>
                    <$payload as $crate::presentation::packed::PackedPayload>::put_body(v, out), )* }
            }

            pub fn read_into(
                r: &mut $crate::presentation::packed::Reader<'_>,
                f: &mut Vec<f32>,
                i: &mut Vec<i64>,
            ) -> Result<$kind, $crate::presentation::packed::PackedError> {
                let code = $crate::presentation::packed::Reader::u8(r)?;
                let kind = $kind::from_code(code)
                    .ok_or($crate::presentation::packed::PackedError::BadPayloadKind(code))?;
                match kind {
                    $( $kind::$variant =>
                        <$payload as $crate::presentation::packed::PackedPayload>::read_body(r, f, i)?, )*
                }
                Ok(kind)
            }

            pub fn write_pools(&self, f: &mut Vec<f32>, i: &mut Vec<i64>) {
                match self { $( $payload_enum::$variant(v) =>
                    <$payload as $crate::presentation::packed::PackedPayload>::write_pools(v, f, i), )* }
            }

            pub fn from_pools(
                kind: $kind, f: &[f32], i: &[i64], fo: usize, io: usize,
            ) -> Self {
                match kind {
                    $( $kind::$variant => $payload_enum::$variant(
                        <$payload as $crate::presentation::packed::PackedPayload>::from_pools(f, i, fo, io)), )*
                }
            }
        }

        impl $crate::presentation::payload::PayloadKindTrait for $kind {
            const ALL: &'static [Self] = $kind::ALL;
            fn as_str(self) -> &'static str { $kind::as_str(self) }
            fn code(self) -> u8 { $kind::code(self) }
            fn from_code(code: u8) -> Option<Self> { $kind::from_code(code) }
            fn is_perception_gated(self) -> bool { $kind::is_perception_gated(self) }
        }

        impl $crate::presentation::payload::PresentationPayload for $payload_enum {
            type Kind = $kind;
            fn kind(&self) -> $kind { $payload_enum::kind(self) }
            fn put(&self, out: &mut Vec<u8>) { $payload_enum::put(self, out) }
            fn read_into(
                r: &mut $crate::presentation::packed::Reader<'_>,
                f: &mut Vec<f32>,
                i: &mut Vec<i64>,
            ) -> Result<$kind, $crate::presentation::packed::PackedError> {
                $payload_enum::read_into(r, f, i)
            }
            fn write_pools(&self, f: &mut Vec<f32>, i: &mut Vec<i64>) {
                $payload_enum::write_pools(self, f, i)
            }
            fn from_pools(kind: $kind, f: &[f32], i: &[i64], fo: usize, io: usize) -> Self {
                $payload_enum::from_pools(kind, f, i, fo, io)
            }
        }

        // 隐私清单与载荷定义同源生成，业务层无需再抄字符串。
        $crate::define_payload_privacy! {
            $(
                $payload $(, private: $private)? ;
            )*
        }
    };
}
