//! 表现帧的二进制打包格式（跨 FFI 快通道）。
//!
//! ## 为什么需要它
//!
//! 旧通道把每一帧翻译成 Array[Dictionary]：N 个实体 × M 个载荷 =
//! 每帧上千次堆分配 + 上千次 Variant 构造，而且**全部发生在 Godot 主线程**。
//! 本模块定义一种自描述、版本化、小端的二进制表示，让整帧只经过一次 FFI
//! 搬运；godot-client-ext 再把它解析成：
//!
//! - **SoA Packed 数组**（[FrameStreams]，主推快通道）：GDScript 直接索引，
//!   每帧分配次数与「载荷种类」同阶，而不是与「实体数」同阶；
//! - **兼容 Dictionary**（经由 [decode_frame]）：用于 A/B 对比与旧脚本回退。
//!
//! ## 边界铁律
//!
//! 字节布局知识**只存在于 game-core**。godot-client-ext 只调用
//! [encode_frame] / [decode_streams] / [decode_frame] / [assemble_frame]，
//! 不自己拼字节、不自己算偏移。
//!
//! ## 格式 v2（全部小端）
//!
//! 偏移  长度  字段
//! 0     4     magic = GPF1
//! 4     2     version = 2
//! 6     2     flags = 0（保留）
//! 8     8     session
//! 16    8     seq
//! 24    4     tick
//! 28    8     render_clock_ms
//! 36    4     timestep_ms (f32)
//! 40    4     command_count
//! 44    ...   command_count 条命令
//!
//! 每条命令固定头部：kind u8 + entity_id u64，随后按 kind 追加负载：
//!
//! kind 0=attach 无附加（只建立数据记录）；1=add / 2=update 附带 payload；
//! 3=remove 附带 payload_kind u8；4=detach / 5=despawn 无附加。
//!
//! payload = payload_kind u8 + 负载体：
//! 0=transform：14 个 f32（prev.pos3, prev.yaw, prev.correction3,
//!             curr.pos3, curr.yaw, curr.correction3）
//! 1=presentation：locomotion u32, action u32, tag_count u32, tags[tag_count] u32
//! 2=health：current f32, max f32
//! 3=visibility：visible u8
//! 4=interaction：action u32, enabled u8
//! 5=extension：count u32，随后 count × (key u32, tag u8, value)；
//!              tag 0=i32(u32), 1=bool(u8), 2=tags(n u32 + n×u32)
//! 6=prototype：u32（静态原型 ID，走普通 i32 池 1 槽）

use crate::identity::StableEntityId;

use super::command::{PresentationCommand, PresentationFrame};
use super::interp::{RenderTransform, RenderTransformSample};
use super::payload::{PayloadKindTrait, PresentationPayload};

/// 魔数（小端字节 GPF1）。
pub const PACKED_MAGIC: [u8; 4] = *b"GPF1";
/// 当前格式版本。
pub const PACKED_VERSION: u16 = 2;
/// 固定头部长度。
pub const HEADER_LEN: usize = 44;

/// 命令 kind 码。
pub mod kind {
    pub const ATTACH: u8 = 0;
    pub const ADD: u8 = 1;
    pub const UPDATE: u8 = 2;
    pub const REMOVE: u8 = 3;
    pub const DETACH: u8 = 4;
    pub const DESPAWN: u8 = 5;
    /// 该命令没有载荷。
    pub const NONE: u8 = 0xFF;
}

/// 「该命令没有载荷」的哨兵码（REMOVE 之外的命令在 SoA 里用它占位）。
///
/// 载荷的实际 code 来自 PayloadKind::code()（payload.rs 的 sync_payloads!
/// 是唯一真值源），这里只保留这个哨兵，不再维护一份平行的常量表。
pub const PAYLOAD_NONE: u8 = 0xFF;

/// transform 载荷的 f32 数量。
pub const TRANSFORM_FLOATS: usize = 14;
/// 防恶意长度：单个 presentation 载荷的 overlay tag 上限。
pub const MAX_TAGS: u32 = 4096;
/// 防恶意长度：command_count 预分配上限。
const MAX_PRECOMPUTE_COMMANDS: usize = 1 << 20;

/// 帧头（与命令体分离，便于只读头部）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PackedHeader {
    pub session: u64,
    pub seq: u64,
    pub tick: u32,
    pub render_clock_ms: u64,
    pub timestep_ms: f32,
    pub command_count: u32,
}

/// 解析错误。**绝不 panic**：畸形输入一律返回错误，由调用方 warning 后空帧处理。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackedError {
    /// 输入不足（含截断）。
    Truncated,
    /// 魔数不匹配。
    BadMagic,
    /// 版本不支持。
    UnsupportedVersion(u16),
    /// 未知命令 kind。
    BadCommandKind(u8),
    /// 未知 payload kind。
    BadPayloadKind(u8),
    /// tag 数量越界。
    TooManyTags,
    /// 扩展载荷字段数越界。
    TooManyExtensionFields,
    /// 未知扩展字段 tag（无长度信息，无法安全跳过）。
    BadExtensionTag(u8),
}

impl std::fmt::Display for PackedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackedError::Truncated => write!(f, "打包表现帧被截断"),
            PackedError::BadMagic => write!(f, "魔数不匹配（不是 GPF1 打包帧）"),
            PackedError::UnsupportedVersion(v) => write!(f, "不支持的打包版本 {v}"),
            PackedError::BadCommandKind(k) => write!(f, "未知命令 kind {k}"),
            PackedError::BadPayloadKind(k) => write!(f, "未知载荷 kind {k}"),
            PackedError::TooManyTags => write!(f, "overlay tag 数量越界"),
            PackedError::TooManyExtensionFields => write!(f, "扩展字段数越界"),
            PackedError::BadExtensionTag(tag) => write!(f, "未知扩展字段 tag {tag}"),
        }
    }
}

impl std::error::Error for PackedError {}

/// SoA（结构体数组）解析结果：GDScript 侧按平行数组直接索引。
///
/// 每个命令在 kinds / entity_ids / payload_kinds 中
/// 各占一个位置；f32_* / i32_* 用「偏移 + 数量」指向两个池。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameStreams {
    pub kinds: Vec<u8>,
    pub entity_ids: Vec<u64>,
    pub payload_kinds: Vec<u8>,
    pub f32_offsets: Vec<u32>,
    pub f32_counts: Vec<u32>,
    pub i32_offsets: Vec<u32>,
    pub i32_counts: Vec<u32>,
    pub f32_pool: Vec<f32>,
    pub i32_pool: Vec<i64>,
}

impl FrameStreams {
    pub fn with_capacity(commands: usize) -> Self {
        Self {
            kinds: Vec::with_capacity(commands),
            entity_ids: Vec::with_capacity(commands),
            payload_kinds: Vec::with_capacity(commands),
            f32_offsets: Vec::with_capacity(commands),
            f32_counts: Vec::with_capacity(commands),
            i32_offsets: Vec::with_capacity(commands),
            i32_counts: Vec::with_capacity(commands),
            f32_pool: Vec::new(),
            i32_pool: Vec::new(),
        }
    }

    pub fn command_count(&self) -> usize {
        self.kinds.len()
    }

    fn push_range(&mut self, f32_start: usize, i32_start: usize) {
        self.f32_offsets.push(f32_start as u32);
        self.f32_counts
            .push((self.f32_pool.len() - f32_start) as u32);
        self.i32_offsets.push(i32_start as u32);
        self.i32_counts
            .push((self.i32_pool.len() - i32_start) as u32);
    }

    fn push_empty_ranges(&mut self) {
        let f = self.f32_pool.len();
        let i = self.i32_pool.len();
        self.push_range(f, i);
    }
}

// ───────────────────────────── 写入 ─────────────────────────────

pub fn put_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}
pub fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub fn put_f32(out: &mut Vec<u8>, v: f32) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub fn put_bool(out: &mut Vec<u8>, v: bool) {
    out.push(u8::from(v));
}

// ───────────────────────── 线格式接口（C） ─────────────────────────
//
// 每个载荷的字节布局只写这一处；payload.rs 的 sync_payloads! 只认识本接口，
// 不认识具体布局。三个方法合起来取代了旧版 put / read / 还原三处重复的
// 「按载荷类型分支」。
//
// 全部静态分发：调用点单态化，没有 vtable / 反射 / Any。

/// 一个载荷的 GPF1 字节布局。
pub trait PackedPayload: Sized {
    /// 只写载荷体（**不含** kind 码）。
    fn put_body(&self, out: &mut Vec<u8>);
    /// 从字节读入并**直接推进 SoA 池**（不构造 Self）。
    fn read_body(r: &mut Reader<'_>, f: &mut Vec<f32>, i: &mut Vec<i64>)
        -> Result<(), PackedError>;
    /// 从 SoA 池还原（Dictionary 兼容通道用）。
    fn from_pools(f: &[f32], i: &[i64], fo: usize, io: usize) -> Self;

    /// 直接把载荷写进 SoA 池（不经过字节）。
    ///
    /// 与 [`PackedPayload::read_body`] 写入**完全相同的池布局**；
    /// [`frame_to_streams`] 用它在发布侧一步生成 SoA，跳过
    /// `encode → bytes → decode` 往返。
    fn write_pools(&self, f: &mut Vec<f32>, i: &mut Vec<i64>);
}

impl PackedPayload for RenderTransformSample {
    fn put_body(&self, out: &mut Vec<u8>) {
        put_transform(out, &self.prev);
        put_transform(out, &self.curr);
    }

    fn write_pools(&self, f: &mut Vec<f32>, _i: &mut Vec<i64>) {
        for t in [&self.prev, &self.curr] {
            f.extend_from_slice(&t.position);
            f.push(t.yaw);
            f.extend_from_slice(&t.correction);
        }
    }

    fn read_body(
        r: &mut Reader<'_>,
        f: &mut Vec<f32>,
        _i: &mut Vec<i64>,
    ) -> Result<(), PackedError> {
        for _ in 0..TRANSFORM_FLOATS {
            f.push(r.f32()?);
        }
        Ok(())
    }

    fn from_pools(f: &[f32], _i: &[i64], fo: usize, _io: usize) -> Self {
        RenderTransformSample {
            prev: read_transform(f, fo),
            curr: read_transform(f, fo + 7),
        }
    }
}

fn put_payload<P: PresentationPayload>(out: &mut Vec<u8>, payload: &P) {
    payload.put(out);
}

/// 写一个完整 RenderTransform：pos3 + yaw + correction3。
fn put_transform(out: &mut Vec<u8>, transform: &RenderTransform) {
    for v in transform.position {
        put_f32(out, v);
    }
    put_f32(out, transform.yaw);
    for v in transform.correction {
        put_f32(out, v);
    }
}

fn put_command<P: PresentationPayload>(out: &mut Vec<u8>, command: &PresentationCommand<P>) {
    match command {
        PresentationCommand::Attach { id } => {
            put_u8(out, kind::ATTACH);
            put_u64(out, id.0);
        }
        PresentationCommand::Add { id, payload } => {
            put_u8(out, kind::ADD);
            put_u64(out, id.0);
            put_payload(out, payload);
        }
        PresentationCommand::Update { id, payload } => {
            put_u8(out, kind::UPDATE);
            put_u64(out, id.0);
            put_payload(out, payload);
        }
        PresentationCommand::Remove { id, kind } => {
            put_u8(out, kind::REMOVE);
            put_u64(out, id.0);
            put_u8(out, kind.code());
        }
        PresentationCommand::Detach { id } => {
            put_u8(out, kind::DETACH);
            put_u64(out, id.0);
        }
        PresentationCommand::Despawn { id } => {
            put_u8(out, kind::DESPAWN);
            put_u64(out, id.0);
        }
    }
}

/// 把一帧编码进 out（可复用以避免反复分配）。
pub fn encode_into<P: PresentationPayload>(frame: &PresentationFrame<P>, out: &mut Vec<u8>) {
    out.extend_from_slice(&PACKED_MAGIC);
    put_u16(out, PACKED_VERSION);
    put_u16(out, 0);
    put_u64(out, frame.session);
    put_u64(out, frame.seq);
    put_u32(out, frame.tick);
    put_u64(out, frame.render_clock_ms);
    put_f32(out, frame.timestep_ms);
    put_u32(out, frame.commands.len() as u32);
    for command in frame.commands.iter() {
        put_command(out, command);
    }
}

/// 把一帧编码为独立字节缓冲。
pub fn encode_frame<P: PresentationPayload>(frame: &PresentationFrame<P>) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + frame.commands.len() * 16);
    encode_into(frame, &mut out);
    out
}

// ───────────────────────────── 读取 ─────────────────────────────

/// 小端字节读取游标。对 crate 内部可见：sync_payloads! 生成的解码分发要和
/// PackedPayload 的实现一起使用它，但**字节布局知识仍只在本模块**。
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], PackedError> {
        let end = self.pos.checked_add(n).ok_or(PackedError::Truncated)?;
        if end > self.buf.len() {
            return Err(PackedError::Truncated);
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    pub fn u8(&mut self) -> Result<u8, PackedError> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16, PackedError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    pub fn u32(&mut self) -> Result<u32, PackedError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn u64(&mut self) -> Result<u64, PackedError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    pub fn f32(&mut self) -> Result<f32, PackedError> {
        let b = self.take(4)?;
        Ok(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn bool(&mut self) -> Result<bool, PackedError> {
        Ok(self.u8()? != 0)
    }
}

fn read_header(r: &mut Reader<'_>) -> Result<PackedHeader, PackedError> {
    let magic = r.take(4)?;
    if magic != PACKED_MAGIC {
        return Err(PackedError::BadMagic);
    }
    let version = r.u16()?;
    if version != PACKED_VERSION {
        return Err(PackedError::UnsupportedVersion(version));
    }
    let _flags = r.u16()?;
    Ok(PackedHeader {
        session: r.u64()?,
        seq: r.u64()?,
        tick: r.u32()?,
        render_clock_ms: r.u64()?,
        timestep_ms: r.f32()?,
        command_count: r.u32()?,
    })
}

/// 只解析固定头部（O(1)、不分配），用于 render_alpha 等热路径。
pub fn decode_header(bytes: &[u8]) -> Result<PackedHeader, PackedError> {
    let mut r = Reader::new(bytes);
    read_header(&mut r)
}

fn read_payload<P: PresentationPayload>(
    r: &mut Reader<'_>,
    s: &mut FrameStreams,
) -> Result<u8, PackedError> {
    let f32_start = s.f32_pool.len();
    let i32_start = s.i32_pool.len();
    // 解码快通道：直接写 SoA 池，不构造中间 SyncPayload。
    let kind = P::read_into(r, &mut s.f32_pool, &mut s.i32_pool)?;
    s.push_range(f32_start, i32_start);
    Ok(kind.code())
}

fn read_command<P: PresentationPayload>(
    r: &mut Reader<'_>,
    s: &mut FrameStreams,
) -> Result<(), PackedError> {
    let command_kind = r.u8()?;
    let entity_id = r.u64()?;

    s.kinds.push(command_kind);
    s.entity_ids.push(entity_id);
    s.payload_kinds.push(PAYLOAD_NONE);

    match command_kind {
        kind::ATTACH => {
            s.push_empty_ranges();
        }
        kind::ADD | kind::UPDATE => {
            let code = read_payload::<P>(r, s)?;
            *s.payload_kinds.last_mut().expect("刚 push 过") = code;
        }
        kind::REMOVE => {
            let code = r.u8()?;
            <P::Kind as PayloadKindTrait>::from_code(code)
                .ok_or(PackedError::BadPayloadKind(code))?;
            *s.payload_kinds.last_mut().expect("刚 push 过") = code;
            s.push_empty_ranges();
        }
        kind::DETACH | kind::DESPAWN => {
            s.push_empty_ranges();
        }
        other => return Err(PackedError::BadCommandKind(other)),
    }
    Ok(())
}

/// 单遍解析成 SoA 流（不构造 Vec<PresentationCommand>）。
pub fn decode_streams<P: PresentationPayload>(
    bytes: &[u8],
) -> Result<(PackedHeader, FrameStreams), PackedError> {
    let mut r = Reader::new(bytes);
    let header = read_header(&mut r)?;
    let capacity = (header.command_count as usize).min(MAX_PRECOMPUTE_COMMANDS);
    let mut streams = FrameStreams::with_capacity(capacity);
    for _ in 0..header.command_count {
        read_command::<P>(&mut r, &mut streams)?;
    }
    Ok((header, streams))
}

/// 从类型化帧**一步**生成 SoA 流（发布侧快通道）。
///
/// 与 `decode_streams(encode_frame(frame))` 语义等价，但跳过
/// `encode → PackedByteArray → Reader` 往返。池布局由各载荷的
/// [`PackedPayload::write_pools`] 定义，必须与 `read_body` 一致。
pub fn frame_to_streams<P: PresentationPayload>(frame: &PresentationFrame<P>) -> FrameStreams {
    let mut s = FrameStreams::with_capacity(frame.commands.len());
    for command in frame.commands.iter() {
        match command {
            PresentationCommand::Attach { id } => {
                s.kinds.push(kind::ATTACH);
                s.entity_ids.push(id.0);
                s.payload_kinds.push(PAYLOAD_NONE);
                s.push_empty_ranges();
            }
            PresentationCommand::Add { id, payload } => {
                push_payload_command(&mut s, kind::ADD, id, payload);
            }
            PresentationCommand::Update { id, payload } => {
                push_payload_command(&mut s, kind::UPDATE, id, payload);
            }
            PresentationCommand::Remove { id, kind: removed } => {
                s.kinds.push(kind::REMOVE);
                s.entity_ids.push(id.0);
                s.payload_kinds.push(removed.code());
                s.push_empty_ranges();
            }
            PresentationCommand::Detach { id } => {
                s.kinds.push(kind::DETACH);
                s.entity_ids.push(id.0);
                s.payload_kinds.push(PAYLOAD_NONE);
                s.push_empty_ranges();
            }
            PresentationCommand::Despawn { id } => {
                s.kinds.push(kind::DESPAWN);
                s.entity_ids.push(id.0);
                s.payload_kinds.push(PAYLOAD_NONE);
                s.push_empty_ranges();
            }
        }
    }
    s
}

fn push_payload_command<P: PresentationPayload>(
    s: &mut FrameStreams,
    command_kind: u8,
    id: &StableEntityId,
    payload: &P,
) {
    s.kinds.push(command_kind);
    s.entity_ids.push(id.0);
    s.payload_kinds.push(payload.kind().code());
    let f0 = s.f32_pool.len();
    let i0 = s.i32_pool.len();
    payload.write_pools(&mut s.f32_pool, &mut s.i32_pool);
    s.push_range(f0, i0);
}

fn read_transform(pool: &[f32], offset: usize) -> RenderTransform {
    RenderTransform {
        position: [pool[offset], pool[offset + 1], pool[offset + 2]],
        yaw: pool[offset + 3],
        correction: [pool[offset + 4], pool[offset + 5], pool[offset + 6]],
    }
}

fn payload_from_streams<P: PresentationPayload>(
    streams: &FrameStreams,
    index: usize,
    code: u8,
) -> Result<P, PackedError> {
    let kind =
        <P::Kind as PayloadKindTrait>::from_code(code).ok_or(PackedError::BadPayloadKind(code))?;

    // 只把本命令自己的池切片交给 from_pools（偏移归零），并在这里判定越界。
    // 这样脏 / 错位的 FrameStreams 不会让 from_pools 读到相邻命令的槽位，
    // 而是退化为「本命令槽不足」——由各 from_pools 的边界守卫处理。
    let f_start = streams.f32_offsets[index] as usize;
    let f_end = f_start.saturating_add(streams.f32_counts[index] as usize);
    let i_start = streams.i32_offsets[index] as usize;
    let i_end = i_start.saturating_add(streams.i32_counts[index] as usize);
    let f = streams.f32_pool.get(f_start..f_end).unwrap_or(&[]);
    let i = streams.i32_pool.get(i_start..i_end).unwrap_or(&[]);

    Ok(P::from_pools(kind, f, i, 0, 0))
}

/// 把 SoA 流还原成命令序列（兼容路径 / 单测用；会分配 enum）。
pub fn commands_from_streams<P: PresentationPayload>(
    streams: &FrameStreams,
) -> Result<Vec<PresentationCommand<P>>, PackedError> {
    let mut out = Vec::with_capacity(streams.kinds.len());
    for index in 0..streams.kinds.len() {
        let id = StableEntityId(streams.entity_ids[index]);
        let command = match streams.kinds[index] {
            kind::ATTACH => PresentationCommand::Attach { id },
            kind::ADD => PresentationCommand::Add {
                id,
                payload: payload_from_streams::<P>(streams, index, streams.payload_kinds[index])?,
            },
            kind::UPDATE => PresentationCommand::Update {
                id,
                payload: payload_from_streams(streams, index, streams.payload_kinds[index])?,
            },
            kind::REMOVE => PresentationCommand::Remove {
                id,
                kind: <P::Kind as PayloadKindTrait>::from_code(streams.payload_kinds[index])
                    .ok_or(PackedError::BadPayloadKind(streams.payload_kinds[index]))?,
            },
            kind::DETACH => PresentationCommand::Detach { id },
            kind::DESPAWN => PresentationCommand::Despawn { id },
            other => return Err(PackedError::BadCommandKind(other)),
        };
        out.push(command);
    }
    Ok(out)
}

/// 解析成完整命令序列（含帧头）。
pub fn decode_frame<P: PresentationPayload>(
    bytes: &[u8],
) -> Result<(PackedHeader, Vec<PresentationCommand<P>>), PackedError> {
    let (header, streams) = decode_streams::<P>(bytes)?;
    let commands = commands_from_streams::<P>(&streams)?;
    Ok((header, commands))
}

/// 用帧头 + 命令序列重组 [PresentationFrame]（godot-client-ext 兼容路径用）。
pub fn assemble_frame<P: PresentationPayload>(
    header: PackedHeader,
    commands: Vec<PresentationCommand<P>>,
) -> PresentationFrame<P> {
    PresentationFrame {
        session: header.session,
        seq: header.seq,
        tick: header.tick,
        render_clock_ms: header.render_clock_ms,
        timestep_ms: header.timestep_ms,
        commands: commands.into(),
    }
}

/// 快速判断一段字节是否是本格式的打包帧。
pub fn is_packed_frame(bytes: &[u8]) -> bool {
    bytes.len() >= HEADER_LEN && bytes[..4] == PACKED_MAGIC
}
