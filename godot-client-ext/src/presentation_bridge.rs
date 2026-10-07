//! `game_core` 纯数据类型 ↔ Godot `Variant` 的**机械转换**。
//!
//! 本文件里不允许出现任何业务判断：
//!
//! - 「动作名 → 通道位」的查表在 `game_core::input::raw::RawInputFrame::from_names`；
//! - 「组件 → 载荷」的翻译在 `game_core::presentation::payload::ToPresentation`；
//! - 「该不该显示」的过滤在 `game_core::presentation::sync`。
//!
//! 这里只做 `VarDictionary` / `Array<VarDictionary>` / `Packed*Array` 的组装与拆解。
//!
//! 注意（godot-rust 0.5 的强类型集合）：
//! - `Dictionary` 是 `Dictionary<K, V>`，跨 FFI 一律用别名 `VarDictionary`；
//! - `Array<Dictionary>` **非法**（`Dictionary` 缺两个泛型参数），数组里放字典
//!   只有 `Array<VarDictionary>` 这一种合法形式；
//! - `Variant` / `Array` / `Dictionary` 是 ByRef 类型，传引用（`&x`）；
//! - `u64` 没有 `ToGodot`，跨 FFI 一律用 `i64`。

use godot::builtin::{
    Array, PackedByteArray, PackedFloat32Array, PackedInt32Array, PackedInt64Array,
    PackedStringArray, VarDictionary,
};
use godot::prelude::*;

use game_core::input::control::InputSourceId;
use game_core::input::raw::RawInputFrame;
use game_core::presentation::event::{PresentationEvent, TimedPresentationEvent};
use game_core::presentation::packed;
use game_core::presentation::payload::{ExtValue, SyncPayload};
use game_core::presentation::semantics;
use game_core::presentation::{PresentationCommand, PresentationFrame};
use game_core::static_data::item;
use game_core::static_data::prototype;

/// 动作表 → `Array[Dictionary]`，每项 `{ name, bit }`。
///
/// 按键（默认绑定 / 改键）由 Godot 侧配置，game-core 不持有物理键。
pub fn action_table_to_variant() -> Array<VarDictionary> {
    let mut out: Array<VarDictionary> = Array::new();
    for (name, bit) in game_core::input::action_table_snapshot() {
        let mut row = VarDictionary::new();
        row.set("name", name);
        row.set("bit", i64::from(bit));
        out.push(&row);
    }
    out
}

/// 原型表 → `Array[Dictionary]`，每项 `{ prototype_id, name, version }`。
pub fn prototype_table_to_variant() -> Array<VarDictionary> {
    let mut out: Array<VarDictionary> = Array::new();
    for (id, name, version) in prototype::prototype_table_snapshot() {
        let mut row = VarDictionary::new();
        row.set("prototype_id", i64::from(id));
        row.set("name", name);
        row.set("version", i64::from(version));
        out.push(&row);
    }
    out
}

/// 物品定义表 → Array[Dictionary]，每项
/// { item_id, name, category, category_name, sub_category, tags, max_stack, prototype_id, version }。
///
/// tags 由逻辑层统一升序输出；Godot 只做集合判断，不判断业务规则。
pub fn item_table_to_variant() -> Array<VarDictionary> {
    let mut out: Array<VarDictionary> = Array::new();
    for row in item::item_table_snapshot() {
        let mut dict = VarDictionary::new();
        dict.set("item_id", i64::from(row.item_id));
        dict.set("name", row.name);
        dict.set("category", i64::from(row.category));
        dict.set("category_name", row.category_name);
        dict.set("sub_category", i64::from(row.sub_category));
        let tags: Vec<i32> = row.tags.iter().map(|tag| *tag as i32).collect();
        dict.set("tags", &PackedInt32Array::from(&tags[..]));
        dict.set("max_stack", i64::from(row.max_stack));
        dict.set("prototype_id", i64::from(row.prototype_id));
        dict.set("version", i64::from(row.version));
        out.push(&dict);
    }
    out
}

/// 品类表 → Array[Dictionary]，每项 { id, name }。封闭枚举，mod 不得新增。
pub fn item_category_table_to_variant() -> Array<VarDictionary> {
    let mut out: Array<VarDictionary> = Array::new();
    for (id, name) in item::item_category_table_snapshot() {
        let mut dict = VarDictionary::new();
        dict.set("id", i64::from(id));
        dict.set("name", name);
        out.push(&dict);
    }
    out
}

/// 标签表 → Array[Dictionary]，每项 { id, name, partition, source }。
pub fn item_tag_table_to_variant() -> Array<VarDictionary> {
    let mut out: Array<VarDictionary> = Array::new();
    for (id, name, partition, source) in item::item_tag_table_snapshot() {
        let mut dict = VarDictionary::new();
        dict.set("id", i64::from(id));
        dict.set("name", name);
        dict.set("partition", partition);
        dict.set("source", source);
        out.push(&dict);
    }
    out
}

/// 子品类表 → Array[Dictionary]，每项 { id, name, parent, parent_name, partition, source }。
///
/// 本期核心表为空，只固定数据形状（mod 注册入口随 ItemRegistry 后置）。
pub fn item_subcategory_table_to_variant() -> Array<VarDictionary> {
    let mut out: Array<VarDictionary> = Array::new();
    for (id, name, parent, parent_name, partition, source) in
        item::item_subcategory_table_snapshot()
    {
        let mut dict = VarDictionary::new();
        dict.set("id", i64::from(id));
        dict.set("name", name);
        dict.set("parent", i64::from(parent));
        dict.set("parent_name", parent_name);
        dict.set("partition", partition);
        dict.set("source", source);
        out.push(&dict);
    }
    out
}

/// 语义表 → `{ domain: Array[{ id, name, partition, source }] }`（mod 基石）。
///
/// 合并 `game-core` 核心表与 [`semantics::SemanticRegistry`] 的运行时注册：
/// 运行时同 id 覆盖核心名，运行时新增追加在该域末尾，mod 自定义域追加在最后。
/// `source` 为 `"core"` 或 `"mod"`；名字只用于表现层，逻辑 / 协议只认 ID。
pub fn semantic_tables_to_variant(registry: &semantics::SemanticRegistry) -> VarDictionary {
    let mut groups: Vec<(String, Array<VarDictionary>)> = Vec::new();
    for (domain, id, name, source) in registry.merged_entries() {
        let index = match groups.iter().position(|(existing, _)| *existing == domain) {
            Some(index) => index,
            None => {
                groups.push((domain, Array::new()));
                groups.len() - 1
            }
        };
        let mut row = VarDictionary::new();
        row.set("id", i64::from(id));
        row.set("name", name.as_str());
        row.set(
            "partition",
            semantics::partition_name(semantics::partition_of(id)),
        );
        row.set("source", source);
        groups[index].1.push(&row);
    }

    let mut out = VarDictionary::new();
    for (domain, rows) in groups.iter() {
        out.set(domain.as_str(), rows);
    }
    out
}

/// 体素调色板表（路径 B GPU 渲染启动时拉一次）。
///
/// 返回 `{ voxel_size, palette, material_count }`：
/// - `voxel_size`：单个体素边长（米，f32）；
/// - `palette`：256 × RGBA8 字节，按**方块 id** 索引（与 RectInstance.material 一致），
///   未知 id 为品红；颜色直接取 static_data::voxel::MATERIAL_TABLE 的线性 RGBA8 原值，
///   渲染侧不再做任何换算（边界铁律：查表在 game-core，搬运在 godot-client-ext）。
pub fn voxel_table_to_variant() -> VarDictionary {
    use game_core::static_data::voxel::{
        block_def, material_def, MATERIAL_TABLE, VOXEL_SIZE_METERS,
    };

    let mut palette = vec![0u8; 256 * 4];
    for (index, px) in palette.chunks_exact_mut(4).enumerate() {
        let color = block_def(index as u8)
            .and_then(|block| material_def(block.material))
            .map(|material| material.color)
            .unwrap_or([255, 0, 255, 255]);
        px.copy_from_slice(&color);
    }

    let mut out = VarDictionary::new();
    out.set("voxel_size", VOXEL_SIZE_METERS as f32);
    out.set("palette", &PackedByteArray::from(&palette[..]));
    out.set("material_count", MATERIAL_TABLE.len() as i64);
    out
}

/// `PackedStringArray` → `Vec<String>`（Godot → Rust 的第一段搬运）。
///
/// `PackedArray` 没有 `iter_shared()`，只能 `to_vec()` / `as_slice()`。
pub fn packed_strings_to_vec(value: &PackedStringArray) -> Vec<String> {
    value.to_vec().iter().map(String::from).collect()
}

/// 借用视图（写成自由函数而不是闭包，生命周期才能被正确推导）。
fn as_strs(items: &[String]) -> Vec<&str> {
    items.iter().map(String::as_str).collect()
}

/// 组装一帧原始输入。
///
/// 名字查表、未知名字收集、轴值量化全部由
/// [`RawInputFrame::from_names`]（`game_core`）完成。
pub fn build_raw_input_frame(
    held: &PackedStringArray,
    pressed: &PackedStringArray,
    released: &PackedStringArray,
    move_axis: Vector2,
    look_delta: Vector2,
    render_clock_ms: u64,
    seq: u64,
) -> (RawInputFrame, Vec<String>) {
    let held = packed_strings_to_vec(held);
    let pressed = packed_strings_to_vec(pressed);
    let released = packed_strings_to_vec(released);

    RawInputFrame::from_names::<game_core::input::actions::CoreActions>(
        as_strs(&held),
        as_strs(&pressed),
        as_strs(&released),
        (move_axis.x, move_axis.y),
        (look_delta.x, look_delta.y),
        seq,
        render_clock_ms,
    )
}

/// 输入源 ID 包装（Godot 侧只传一个整数）。
pub fn source_id_from_i64(value: i64) -> InputSourceId {
    InputSourceId(u64::try_from(value).unwrap_or(1))
}

/// 表现帧 → `Dictionary`。
///
/// ```text
/// { session, seq, tick, render_clock_ms, timestep_ms, commands: [ { kind, ... } ] }
/// ```
///
/// alpha 不在帧里：Godot 每帧用 `BevyAppManager.render_alpha()` 在真正的
/// 渲染时刻采样（见 `game_core::presentation::sample_alpha`）。
pub fn frame_to_dictionary(frame: &PresentationFrame) -> VarDictionary {
    let mut commands: Array<VarDictionary> = Array::new();
    for command in frame.commands.iter() {
        commands.push(&command_to_dictionary(command));
    }

    let mut out = VarDictionary::new();
    out.set("session", frame.session as i64);
    out.set("seq", frame.seq as i64);
    out.set("tick", i64::from(frame.tick));
    out.set("render_clock_ms", frame.render_clock_ms as i64);
    out.set("timestep_ms", frame.timestep_ms);
    out.set("commands", &commands);
    out
}

fn command_to_dictionary(command: &PresentationCommand) -> VarDictionary {
    let mut out = VarDictionary::new();
    out.set("kind", command.kind_str());

    match command {
        PresentationCommand::Attach { id } => {
            out.set("id", id.0 as i64);
        }
        PresentationCommand::Add { id, payload } | PresentationCommand::Update { id, payload } => {
            out.set("id", id.0 as i64);
            out.set("payload", &payload_to_dictionary(payload));
        }
        PresentationCommand::Remove { id, kind } => {
            out.set("id", id.0 as i64);
            out.set("payload_kind", kind.as_str());
        }
        PresentationCommand::Detach { id } | PresentationCommand::Despawn { id } => {
            out.set("id", id.0 as i64);
        }
    }

    out
}

/// 表现载荷 → `VarDictionary`（**兼容通道，不承诺扩展**）。
///
/// 边界（b）：
/// - 主推通道是 GPF1 packed（`FrameStreams` SoA），新载荷**只保证**走那里；
/// - 本 `match` **故意不写 `_ =>` 兜底**：新增载荷时编译器会强制在这里补一个
///   分支。按策略补「显式不支持」（`godot_warn!` / 置空）即可，不必实现真正的
///   `VarDictionary` 组装。
fn payload_to_dictionary(payload: &SyncPayload) -> VarDictionary {
    let mut out = VarDictionary::new();
    out.set("type", payload.type_name());

    match payload {
        SyncPayload::Prototype(value) => {
            out.set("value", i64::from(value.0));
        }

        SyncPayload::RectList(_) => {
            // 体素矩形列表只走 GPF1 紧凑通道；Dictionary 兼容通道不支持。
        }

        SyncPayload::Transform(sample) => {
            // `PackedArray` 是 ByRef 类型，必须传引用（`&x`）。
            // 下发插值两端点，Godot 在渲染时刻按帧上的 `alpha` 采样。
            out.set(
                "prev_position",
                &PackedFloat32Array::from(&sample.prev.position[..]),
            );
            out.set("prev_yaw", sample.prev.yaw);
            out.set(
                "curr_position",
                &PackedFloat32Array::from(&sample.curr.position[..]),
            );
            out.set("curr_yaw", sample.curr.yaw);
            out.set(
                "correction",
                &PackedFloat32Array::from(&sample.curr.correction[..]),
            );
        }
        SyncPayload::Presentation(value) => {
            out.set("locomotion_state", i64::from(value.locomotion_state));
            out.set("action_state", i64::from(value.action_state));
            let tags: Vec<i32> = value.overlay_tags.iter().map(|tag| *tag as i32).collect();
            out.set("overlay_tags", &PackedInt32Array::from(&tags[..]));
        }
        SyncPayload::Health(value) => {
            out.set("current", value.current);
            out.set("max", value.max);
        }
        SyncPayload::Visibility(value) => {
            out.set("visible", value.visible);
        }
        SyncPayload::Interaction(value) => {
            out.set("action", i64::from(value.action.0));
            out.set("enabled", value.enabled);
        }
        SyncPayload::Extension(value) => {
            // 兼容通道：整袋字段 → Array[{ key, tag, value }]；tag 与 GPF1 线格式一致
            // （0=i32、1=bool、2=tags），value 分别装 int / bool / PackedInt32Array。
            let mut fields: Array<VarDictionary> = Array::new();
            for field in value.fields() {
                let mut row = VarDictionary::new();
                row.set("key", i64::from(field.key));
                match &field.value {
                    ExtValue::I32(v) => {
                        row.set("tag", 0i64);
                        row.set("value", i64::from(*v));
                    }
                    ExtValue::Bool(b) => {
                        row.set("tag", 1i64);
                        row.set("value", *b);
                    }
                    ExtValue::Tags(tags) => {
                        row.set("tag", 2i64);
                        let packed: Vec<i32> = tags.iter().map(|tag| *tag as i32).collect();
                        row.set("value", &PackedInt32Array::from(&packed[..]));
                    }
                }
                fields.push(&row);
            }
            out.set("fields", &fields);
        }
    }

    out
}

/// 表现事件 → `Array[Dictionary]`（保序）。
pub fn events_to_array(events: &[TimedPresentationEvent]) -> Array<VarDictionary> {
    let mut out: Array<VarDictionary> = Array::new();
    for timed in events {
        let event = &timed.event;
        let mut row = VarDictionary::new();
        row.set("type", event.kind_str());
        row.set("id", event.entity().0 as i64);
        row.set("tick", i64::from(timed.tick));

        match event {
            PresentationEvent::PlaySound {
                sound, position, ..
            } => {
                row.set("sound", i64::from(sound.0));
                row.set("position", &PackedFloat32Array::from(&position[..]));
            }
            PresentationEvent::TriggerAnim { anim, speed, .. } => {
                row.set("anim", i64::from(anim.0));
                row.set("speed", *speed);
            }
            PresentationEvent::SpawnVfx { vfx, position, .. } => {
                row.set("vfx", i64::from(vfx.0));
                row.set("position", &PackedFloat32Array::from(&position[..]));
            }
            PresentationEvent::DamagePopup { amount, .. } => {
                row.set("amount", i64::from(*amount));
            }
        }

        out.push(&row);
    }
    out
}

// ─────────────── GPF1 二进制打包帧解析（SoA 快通道） ───────────────
//
// 布局知识全部在 game-core::presentation::packed；本文件只做
// 「纯数据 → Godot Packed 数组 / Dictionary」的机械搬运。

/// 把 u64 序列转成 Godot 的 PackedInt64Array（gdext 无 u64 数组）。
fn u64_to_packed(values: &[u64]) -> PackedInt64Array {
    let converted: Vec<i64> = values.iter().map(|value| *value as i64).collect();
    PackedInt64Array::from(&converted[..])
}

/// 把 u32 序列转成 Godot 的 PackedInt64Array。
fn u32_to_packed(values: &[u32]) -> PackedInt64Array {
    let converted: Vec<i64> = values.iter().map(|value| i64::from(*value)).collect();
    PackedInt64Array::from(&converted[..])
}

/// 只解析打包帧头部 → 小 Dictionary（O(1)、不构造命令）。
///
/// 键：session / seq / tick / render_clock_ms / timestep_ms / command_count。
pub fn packed_header_to_dictionary(data: &PackedByteArray) -> VarDictionary {
    let mut out = VarDictionary::new();
    match packed::decode_header(data.as_slice()) {
        Ok(header) => {
            out.set("session", header.session as i64);
            out.set("seq", header.seq as i64);
            out.set("tick", i64::from(header.tick));
            out.set("render_clock_ms", header.render_clock_ms as i64);
            out.set("timestep_ms", header.timestep_ms);
            out.set("command_count", i64::from(header.command_count));
        }
        Err(error) => {
            godot_warn!("[presentation_bridge] 打包帧头部解析失败：{error}");
        }
    }
    out
}

/// 解析打包帧 → **SoA 平行数组**（主推快通道）。
///
/// 返回的 Dictionary：
/// - 元数据：session / seq / tick / render_clock_ms / timestep_ms / command_count
/// - 每命令一条：kinds / entity_ids / prototype_ids / payload_kinds
/// - 载荷池：f32_pool / i32_pool，配合 f32_offsets/f32_counts、i32_offsets/i32_counts
///
/// GDScript 一次取出 Packed 数组后按下标索引，**不产生每实体的 Dictionary**。
pub fn packed_frame_to_streams(data: &PackedByteArray) -> VarDictionary {
    let mut out = VarDictionary::new();
    match packed::decode_streams::<SyncPayload>(data.as_slice()) {
        Ok((header, streams)) => {
            out.set("session", header.session as i64);
            out.set("seq", header.seq as i64);
            out.set("tick", i64::from(header.tick));
            out.set("render_clock_ms", header.render_clock_ms as i64);
            out.set("timestep_ms", header.timestep_ms);
            out.set("command_count", i64::from(header.command_count));

            let kinds: Vec<i32> = streams.kinds.iter().map(|k| i32::from(*k)).collect();
            out.set("kinds", &PackedInt32Array::from(&kinds[..]));
            out.set("entity_ids", &u64_to_packed(&streams.entity_ids));
            let payload_kinds: Vec<i32> = streams
                .payload_kinds
                .iter()
                .map(|k| i32::from(*k))
                .collect();
            out.set("payload_kinds", &PackedInt32Array::from(&payload_kinds[..]));

            out.set("f32_offsets", &u32_to_packed(&streams.f32_offsets));
            out.set("f32_counts", &u32_to_packed(&streams.f32_counts));
            out.set("i32_offsets", &u32_to_packed(&streams.i32_offsets));
            out.set("i32_counts", &u32_to_packed(&streams.i32_counts));
            out.set("f32_pool", &PackedFloat32Array::from(&streams.f32_pool[..]));
            out.set("i32_pool", &PackedInt64Array::from(&streams.i32_pool[..]));
        }
        Err(error) => {
            godot_warn!("[presentation_bridge] 打包帧解析失败：{error}");
        }
    }
    out
}

/// 解析打包帧 → 完整 Dictionary（兼容旧通道，逐命令构造）。
///
/// 用于 A/B 对比与旧脚本回退；解析失败返回空 Dictionary 并 warning。
pub fn packed_frame_to_dictionary(data: &PackedByteArray) -> VarDictionary {
    match packed::decode_frame::<SyncPayload>(data.as_slice()) {
        Ok((header, commands)) => {
            let frame = packed::assemble_frame::<SyncPayload>(header, commands);
            frame_to_dictionary(&frame)
        }
        Err(error) => {
            godot_warn!("[presentation_bridge] 打包帧解析失败：{error}");
            VarDictionary::new()
        }
    }
}
