//! 静态数据（逻辑层）。
//!
//! 本模块是对象原型与物品定义两类静态数据的真相源；表现层只消费快照。
//! 依赖方向：game_engine::ids <- static_data/*；static_data <- presentation。
//! 禁止 static_data 依赖 presentation 或 godot。
//!
//! 数据格式是 JSONC：基础层嵌入在 data/，Godot 侧可读取覆盖文件，
//! mod 用 merge-patch 分层。解析、校验、重载都在本模块。

pub mod item;
pub mod prototype;
pub mod voxel;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use bevy::prelude::*;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{Map, Value};

use game_engine::ids::mod_hash_id;
use game_engine::modding::{ObjectLayer, ObjectLayers};
use game_engine::sim::EngineSet;
use game_engine::voxel::VoxelSet;

use item::{
    ItemDef, ItemDefFile, ItemTableRow, ItemTagId, ItemTagIndex, ItemTypeId, PlacedObject,
    PlacedObjectRef,
};
use prototype::{EntityObjectDef, PrototypeId};
use voxel::{BiomeDef, BiomeDefFile, BiomeId, VoxelObjectDef};

pub const VOXEL_OBJECTS_JSONC: &str = include_str!("data/voxel_objects.jsonc");
pub const BIOMES_JSONC: &str = include_str!("data/biomes.jsonc");
pub const ENTITY_OBJECTS_JSONC: &str = include_str!("data/entity_objects.jsonc");
pub const ITEM_PLACEMENT_JSONC: &str = include_str!("data/item_placement.jsonc");

// ─────────────────────────── JSONC / 表 trait ───────────────────────────

/// 一类 JSONC 静态表。
pub trait JsoncTable {
    type Def: DeserializeOwned;
    const FILE: &'static str;
    const ROOT_KEY: &'static str;

    /// 补丁定位用的稳定名字。
    fn key(def: &Self::Def) -> &str;

    /// 表内校验（id/name 唯一、升序、字段一致性）。
    fn validate(defs: &[Self::Def]) -> Result<(), String>;
}

/// 去掉 JSONC 的 // 与 /* */ 注释、以及尾逗号。
/// 字符串内的 //、逗号保持原样；换行保留，serde_json 的错误行号才有意义。
pub fn strip_jsonc(input: &str) -> String {
    let mut without_comments = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                without_comments.push(ch);
                while let Some(inner) = chars.next() {
                    without_comments.push(inner);
                    if inner == '\\' {
                        if let Some(escaped) = chars.next() {
                            without_comments.push(escaped);
                        }
                    } else if inner == '"' {
                        break;
                    }
                }
            }
            '/' => match chars.peek().copied() {
                Some('/') => {
                    chars.next();
                    for inner in chars.by_ref() {
                        if inner == '\n' {
                            without_comments.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next();
                    let mut previous = '\0';
                    for inner in chars.by_ref() {
                        if inner == '\n' {
                            without_comments.push('\n');
                        }
                        if previous == '*' && inner == '/' {
                            break;
                        }
                        previous = inner;
                    }
                }
                _ => without_comments.push('/'),
            },
            _ => without_comments.push(ch),
        }
    }

    // 第二遍：删除紧跟 } / ] 的逗号。
    let mut out = String::with_capacity(without_comments.len());
    let mut chars = without_comments.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                out.push(ch);
                while let Some(inner) = chars.next() {
                    out.push(inner);
                    if inner == '\\' {
                        if let Some(escaped) = chars.next() {
                            out.push(escaped);
                        }
                    } else if inner == '"' {
                        break;
                    }
                }
            }
            ',' => {
                let next = chars.clone().find(|candidate| !candidate.is_whitespace());
                if !matches!(next, Some('}') | Some(']')) {
                    out.push(ch);
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

fn parse_jsonc_value(label: &str, text: &str) -> Result<Value, String> {
    let stripped = strip_jsonc(text);
    serde_json::from_str(&stripped)
        .map_err(|error| format!("{label}:{}:{}: {error}", error.line(), error.column()))
}

fn parse_root_array(label: &str, text: &str, kind: ObjectTableKind) -> Result<Value, String> {
    let value = parse_jsonc_value(label, text)?;
    let object = value
        .as_object()
        .ok_or_else(|| format!("{label}: 顶层必须是对象"))?;
    object
        .get(kind.root_key())
        .cloned()
        .ok_or_else(|| format!("{label}: 缺少根数组 {}", kind.root_key()))
}

fn deserialize_table<T: DeserializeOwned>(value: Value, label: &str) -> Result<Vec<T>, String> {
    let array = value
        .as_array()
        .ok_or_else(|| format!("{label}: 根键必须是数组"))?;
    array
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let name = entry
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("#{index}"));
            serde_json::from_value(entry.clone())
                .map_err(|error| format!("{label}: 条目 {name}: {error}"))
        })
        .collect()
}

// ─────────────────────────── 表种类 / 来源 ───────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectTableKind {
    Voxel,
    Biome,
    Entity,
    Item,
}

impl ObjectTableKind {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "voxel" | "voxel_objects" | "blocks" => Some(Self::Voxel),
            "biome" | "biomes" => Some(Self::Biome),
            "entity" | "entity_objects" | "prototypes" => Some(Self::Entity),
            "item" | "item_placement" | "items" => Some(Self::Item),
            _ => None,
        }
    }

    pub const fn root_key(self) -> &'static str {
        match self {
            Self::Voxel => VoxelObjectDef::ROOT_KEY,
            Self::Biome => BiomeDefFile::ROOT_KEY,
            Self::Entity => EntityObjectDef::ROOT_KEY,
            Self::Item => ItemDefFile::ROOT_KEY,
        }
    }

    pub const fn file(self) -> &'static str {
        match self {
            Self::Voxel => VoxelObjectDef::FILE,
            Self::Biome => BiomeDefFile::FILE,
            Self::Entity => EntityObjectDef::FILE,
            Self::Item => ItemDefFile::FILE,
        }
    }

    fn embedded(self) -> &'static str {
        match self {
            Self::Voxel => VOXEL_OBJECTS_JSONC,
            Self::Biome => BIOMES_JSONC,
            Self::Entity => ENTITY_OBJECTS_JSONC,
            Self::Item => ITEM_PLACEMENT_JSONC,
        }
    }
}

/// 三张表的覆盖文本；None / 空串表示使用嵌入基础层。
#[derive(Clone, Debug, Default)]
pub struct ObjectTableSources {
    pub voxel: Option<String>,
    pub biome: Option<String>,
    pub entity: Option<String>,
    pub item: Option<String>,
}

impl ObjectTableSources {
    fn text_for(&self, kind: ObjectTableKind) -> &str {
        let override_text = match kind {
            ObjectTableKind::Voxel => self.voxel.as_deref(),
            ObjectTableKind::Biome => self.biome.as_deref(),
            ObjectTableKind::Entity => self.entity.as_deref(),
            ObjectTableKind::Item => self.item.as_deref(),
        };
        override_text
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| kind.embedded())
    }
}

// ─────────────────────────── 补丁文档 ───────────────────────────

fn default_version() -> u32 {
    1
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchDocument {
    #[serde(default = "default_version")]
    version: u32,
    table: String,
    #[serde(default)]
    patch: Vec<PatchEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchEntry {
    name: String,
    #[serde(default)]
    set: Option<Value>,
    #[serde(default)]
    add: Option<Value>,
    #[serde(default)]
    priority: Option<u32>,
}

#[derive(Clone, Debug)]
struct Claim {
    source: String,
    priority: Option<u32>,
}

#[derive(Default)]
struct ConflictTracker {
    claims: HashMap<(ObjectTableKind, String, String), Claim>,
}

impl ConflictTracker {
    /// 返回 true 表示本层赢得这个叶子；false 表示被低优先级剪掉。
    fn claim(
        &mut self,
        kind: ObjectTableKind,
        name: &str,
        path: &str,
        source: &str,
        priority: Option<u32>,
    ) -> Result<bool, String> {
        let key = (kind, name.to_string(), path.to_string());
        let Some(existing) = self.claims.get(&key) else {
            self.claims.insert(
                key,
                Claim {
                    source: source.to_string(),
                    priority,
                },
            );
            return Ok(true);
        };
        if existing.source == source {
            let existing = self.claims.get_mut(&key).expect("just checked");
            existing.priority = priority;
            return Ok(true);
        }
        let wins = match (existing.priority, priority) {
            (None, None) => {
                return Err(format!(
                    "补丁冲突：{} 与 {} 同时修改 {}.{}（未声明 priority）",
                    existing.source, source, name, path
                ))
            }
            (Some(old), Some(new)) if new == old => {
                return Err(format!(
                    "补丁冲突：{} 与 {} 同时以 priority={} 修改 {}.{}",
                    existing.source, source, old, name, path
                ))
            }
            (Some(old), Some(new)) => new > old,
            (None, Some(_)) => true,
            (Some(_), None) => false,
        };
        if wins {
            let existing = self.claims.get_mut(&key).expect("just checked");
            existing.source = source.to_string();
            existing.priority = priority;
        }
        Ok(wins)
    }
}

fn merge_patch(target: &mut Value, patch: Value) {
    match patch {
        Value::Object(patch_object) => {
            if !target.is_object() {
                *target = Value::Object(Map::new());
            }
            let target_object = target.as_object_mut().expect("just made object");
            for (key, value) in patch_object {
                if value.is_null() {
                    target_object.remove(&key);
                } else {
                    merge_patch(target_object.entry(key).or_insert(Value::Null), value);
                }
            }
        }
        other => *target = other,
    }
}

/// 按叶子路径修剪补丁：低优先级字段被剪掉；整个对象被剪空则返回 None。
fn filter_patch_leaves(
    value: &Value,
    prefix: &mut Vec<String>,
    tracker: &mut ConflictTracker,
    kind: ObjectTableKind,
    name: &str,
    source: &str,
    priority: Option<u32>,
) -> Result<Option<Value>, String> {
    match value {
        Value::Object(object) => {
            let mut filtered = Map::new();
            for (key, child) in object {
                prefix.push(key.clone());
                let kept =
                    filter_patch_leaves(child, prefix, tracker, kind, name, source, priority)?;
                prefix.pop();
                if let Some(child) = kept {
                    filtered.insert(key.clone(), child);
                }
            }
            if filtered.is_empty() {
                Ok(None)
            } else {
                Ok(Some(Value::Object(filtered)))
            }
        }
        _ => {
            if prefix.is_empty() {
                return Err(format!("{source}: {name} 的补丁值必须是对象"));
            }
            let path = prefix.join(".");
            if tracker.claim(kind, name, &path, source, priority)? {
                Ok(Some(value.clone()))
            } else {
                Ok(None)
            }
        }
    }
}

fn find_entry_index(root: &Value, name: &str) -> Option<usize> {
    root.as_array()?
        .iter()
        .position(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
}

fn apply_patch_layer(
    root: &mut Value,
    document: PatchDocument,
    kind: ObjectTableKind,
    layer: &ObjectLayer,
    tracker: &mut ConflictTracker,
) -> Result<(), String> {
    if document.version != 1 {
        return Err(format!(
            "mod {}: 不支持的补丁 version {}（只支持 1）",
            layer.mod_id, document.version
        ));
    }
    let source = format!("mod {} (load_order {})", layer.mod_id, layer.load_order);
    if document.table.trim().is_empty() {
        return Err(format!("{source}: 缺少 table"));
    }
    if ObjectTableKind::parse(&document.table) != Some(kind) {
        return Err(format!(
            "{source}: table {} 与目标表 {} 不匹配",
            document.table,
            kind.file()
        ));
    }

    for entry in document.patch {
        match (entry.set, entry.add) {
            (Some(_), Some(_)) => {
                return Err(format!("{source}: {} 同时声明 set 与 add", entry.name))
            }
            (None, None) => return Err(format!("{source}: {} 必须声明 set 或 add", entry.name)),
            (Some(set), None) => {
                if !set.is_object() {
                    return Err(format!("{source}: {} 的 set 必须是对象", entry.name));
                }
                let Some(index) = find_entry_index(root, &entry.name) else {
                    return Err(format!("{source}: 找不到要修改的条目 {}", entry.name));
                };
                let current_name = root[index]
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let current_id = root[index].get("id").and_then(Value::as_u64);
                if let Some(new_name) = set.get("name").and_then(Value::as_str) {
                    if new_name != current_name {
                        return Err(format!(
                            "{source}: 不允许通过 set 改条目名字（{} -> {}）",
                            current_name, new_name
                        ));
                    }
                }
                if let Some(new_id) = set.get("id").and_then(Value::as_u64) {
                    if Some(new_id) != current_id {
                        return Err(format!(
                            "{source}: 不允许通过 set 改条目 id（{}）",
                            entry.name
                        ));
                    }
                }
                let mut prefix = Vec::new();
                let filtered = filter_patch_leaves(
                    &set,
                    &mut prefix,
                    tracker,
                    kind,
                    &entry.name,
                    &source,
                    entry.priority,
                )?;
                if let Some(filtered) = filtered {
                    merge_patch(&mut root[index], filtered);
                }
            }
            (None, Some(add)) => {
                if !entry.name.contains(':') {
                    return Err(format!(
                        "{source}: 新增条目 {} 必须带 mod 命名空间（name:item）",
                        entry.name
                    ));
                }
                if !add.is_object() {
                    return Err(format!("{source}: {} 的 add 必须是对象", entry.name));
                }
                if find_entry_index(root, &entry.name).is_some() {
                    return Err(format!("{source}: 新增条目 {} 已存在", entry.name));
                }
                let expected_id = mod_hash_id(&entry.name);
                let mut add = add;
                let object = add.as_object_mut().expect("checked object");
                if let Some(id) = object.get("id").and_then(Value::as_u64) {
                    if id as u32 != expected_id {
                        return Err(format!(
                            "{source}: 新增条目 {} 的 id 必须是 {}，实际 {}",
                            entry.name, expected_id, id
                        ));
                    }
                } else {
                    object.insert("id".to_string(), Value::from(expected_id));
                }
                if let Some(name) = object.get("name").and_then(Value::as_str) {
                    if name != entry.name {
                        return Err(format!(
                            "{source}: 新增条目 name 与补丁定位名不一致：{} != {}",
                            name, entry.name
                        ));
                    }
                } else {
                    object.insert("name".to_string(), Value::from(entry.name.clone()));
                }
                root.as_array_mut().expect("root array checked").push(add);
            }
        }
    }
    root.as_array_mut()
        .expect("root array checked")
        .sort_by_key(|entry| entry.get("id").and_then(Value::as_u64).unwrap_or(u64::MAX));
    Ok(())
}
// ─────────────────────────── ObjectTables ───────────────────────────

/// 一次加载得到的三张对象表（不可变；重载时整体替换）。
#[derive(Resource, Clone, Debug)]
pub struct ObjectTables {
    inner: Arc<ObjectTablesInner>,
}

#[derive(Debug)]
struct ObjectTablesInner {
    generation: u32,
    blocks: Vec<VoxelObjectDef>,
    biomes: Vec<BiomeDef>,
    entities: Vec<EntityObjectDef>,
    items: Vec<ItemDef>,
    block_by_id: HashMap<u32, usize>,
    block_by_slot: HashMap<u8, usize>,
    block_by_name: HashMap<String, usize>,
    biome_by_id: HashMap<u8, usize>,
    biome_by_name: HashMap<String, usize>,
    entity_by_id: HashMap<u32, usize>,
    entity_by_name: HashMap<String, usize>,
    item_by_id: HashMap<u32, usize>,
    item_by_name: HashMap<String, usize>,
}

impl ObjectTables {
    /// 从三张已解析的表构建并做跨表校验。
    pub fn from_parts(
        blocks: Vec<VoxelObjectDef>,
        biomes: Vec<BiomeDef>,
        entities: Vec<EntityObjectDef>,
        items: Vec<ItemDef>,
        generation: u32,
    ) -> Result<Self, String> {
        VoxelObjectDef::validate(&blocks)?;
        EntityObjectDef::validate(&entities)?;

        let mut block_by_id = HashMap::with_capacity(blocks.len());
        let mut block_by_slot = HashMap::with_capacity(blocks.len());
        let mut block_by_name = HashMap::with_capacity(blocks.len());
        for (index, def) in blocks.iter().enumerate() {
            if block_by_id.insert(def.id, index).is_some() {
                return Err(format!("方块 id 重复：{}", def.id));
            }
            if block_by_slot.insert(def.slot, index).is_some() {
                return Err(format!("方块槽位重复：{}", def.slot));
            }
            if block_by_name.insert(def.name.clone(), index).is_some() {
                return Err(format!("方块名字重复：{}", def.name));
            }
        }

        let mut biome_by_id = HashMap::with_capacity(biomes.len());
        let mut biome_by_name = HashMap::with_capacity(biomes.len());
        for (index, def) in biomes.iter().enumerate() {
            if biome_by_id.insert(def.id.0, index).is_some() {
                return Err(format!("生物群系 id 重复：{}", def.id.0));
            }
            if biome_by_name.insert(def.name.to_string(), index).is_some() {
                return Err(format!("生物群系名字重复：{}", def.name));
            }
            for (role, slot) in [
                ("surface_block", def.surface_block),
                ("subsurface_block", def.subsurface_block),
                ("stone_block", def.stone_block),
            ] {
                if !block_by_slot.contains_key(&slot) {
                    return Err(format!(
                        "{} 的 {}({}) 不在方块对象表里",
                        def.name, role, slot
                    ));
                }
            }
        }

        let mut entity_by_id = HashMap::with_capacity(entities.len());
        let mut entity_by_name = HashMap::with_capacity(entities.len());
        for (index, def) in entities.iter().enumerate() {
            if entity_by_id.insert(def.id, index).is_some() {
                return Err(format!("实体 id 重复：{}", def.id));
            }
            if entity_by_name.insert(def.name.clone(), index).is_some() {
                return Err(format!("实体名字重复：{}", def.name));
            }
        }

        let mut item_by_id = HashMap::with_capacity(items.len());
        let mut item_by_name = HashMap::with_capacity(items.len());
        for (index, def) in items.iter().enumerate() {
            if item_by_id.insert(def.id, index).is_some() {
                return Err(format!("物品 id 重复：{}", def.id));
            }
            if item_by_name.insert(def.name.to_string(), index).is_some() {
                return Err(format!("物品名字重复：{}", def.name));
            }
        }

        // 跨表：world_prototype / places 必须指向实体表或方块表里的条目。
        for item in &items {
            if !entity_by_id.contains_key(&item.world_prototype.0) {
                return Err(format!(
                    "{} 的 world_prototype({}) 不在实体对象表里",
                    item.name, item.world_prototype.0
                ));
            }
            match item.places {
                None => {}
                Some(PlacedObject::Block(id)) if !block_by_slot.contains_key(&id) => {
                    return Err(format!(
                        "{} 的 places.block({}) 不在方块对象表里",
                        item.name, id
                    ));
                }
                Some(PlacedObject::VoxelObject(id)) if !block_by_id.contains_key(&id) => {
                    return Err(format!(
                        "{} 的 places.voxel_object({}) 不在方块对象表里",
                        item.name, id
                    ));
                }
                Some(PlacedObject::Entity(id)) if !entity_by_id.contains_key(&id.0) => {
                    return Err(format!(
                        "{} 的 places.entity({}) 不在实体对象表里",
                        item.name, id.0
                    ));
                }
                _ => {}
            }
        }

        let shapes = biomes.iter().map(BiomeDef::shape).collect();

        Ok(Self {
            inner: Arc::new(ObjectTablesInner {
                generation,
                blocks,
                biomes,
                entities,
                items,
                block_by_id,
                block_by_slot,
                block_by_name,
                biome_by_id,
                biome_by_name,
                entity_by_id,
                entity_by_name,
                item_by_id,
                item_by_name,
            }),
        })
        .inspect(|_| voxel::install_biome_shapes(shapes))
    }

    pub fn generation(&self) -> u32 {
        self.inner.generation
    }

    pub fn blocks(&self) -> &[VoxelObjectDef] {
        &self.inner.blocks
    }

    pub fn biomes(&self) -> &[BiomeDef] {
        &self.inner.biomes
    }

    pub fn entities(&self) -> &[EntityObjectDef] {
        &self.inner.entities
    }

    pub fn items(&self) -> &[ItemDef] {
        &self.inner.items
    }

    pub fn voxel(&self, id: u32) -> Option<&VoxelObjectDef> {
        self.inner
            .block_by_id
            .get(&id)
            .map(|index| &self.inner.blocks[*index])
    }

    /// 按 8 bit 调色板槽位查方块（世界体素值就是槽位，不是逻辑 id）。
    pub fn block(&self, slot: u8) -> Option<&VoxelObjectDef> {
        self.inner
            .block_by_slot
            .get(&slot)
            .map(|index| &self.inner.blocks[*index])
    }

    pub fn voxel_by_name(&self, name: &str) -> Option<&VoxelObjectDef> {
        self.inner
            .block_by_name
            .get(name)
            .map(|index| &self.inner.blocks[*index])
    }

    pub fn biome(&self, id: BiomeId) -> Option<&BiomeDef> {
        self.inner
            .biome_by_id
            .get(&id.0)
            .map(|index| &self.inner.biomes[*index])
    }

    pub fn biome_by_name(&self, name: &str) -> Option<&BiomeDef> {
        self.inner
            .biome_by_name
            .get(name)
            .map(|index| &self.inner.biomes[*index])
    }

    pub fn entity(&self, id: PrototypeId) -> Option<&EntityObjectDef> {
        self.inner
            .entity_by_id
            .get(&id.0)
            .map(|index| &self.inner.entities[*index])
    }

    pub fn entity_by_name(&self, name: &str) -> Option<&EntityObjectDef> {
        self.inner
            .entity_by_name
            .get(name)
            .map(|index| &self.inner.entities[*index])
    }

    pub fn item(&self, id: ItemTypeId) -> Option<&ItemDef> {
        self.inner
            .item_by_id
            .get(&id.0)
            .map(|index| &self.inner.items[*index])
    }

    pub fn item_by_name(&self, name: &str) -> Option<&ItemDef> {
        self.inner
            .item_by_name
            .get(name)
            .map(|index| &self.inner.items[*index])
    }

    /// 原型表快照（供 Godot 对账）；名字借用表内字符串，不额外分配。
    pub fn prototype_table_rows(&self) -> Vec<(u32, &str, u32)> {
        self.inner
            .entities
            .iter()
            .map(|def| (def.id, def.name.as_str(), def.version))
            .collect()
    }

    /// 物品表快照（供 Godot 对账）。
    pub fn item_table_rows(&self) -> Vec<ItemTableRow> {
        self.inner
            .items
            .iter()
            .map(|def| ItemTableRow {
                item_id: def.id,
                name: def.name.clone(),
                category: def.category.as_u8(),
                category_name: def.category.as_str(),
                sub_category: def.sub_category.0,
                tags: def.tags.iter().map(|tag| tag.0).collect(),
                max_stack: def.max_stack(),
                mass_grams: def.mass_grams,
                stack_capacity_grams: def.stack_capacity_grams,
                prototype_id: def.world_prototype.0,
                version: def.version,
            })
            .collect()
    }
}

// ─────────────────────────── 加载流水线 ───────────────────────────

fn load_kind<T>(
    kind: ObjectTableKind,
    sources: &ObjectTableSources,
    layers: &ObjectLayers,
) -> Result<Vec<T>, String>
where
    T: JsoncTable<Def = T> + DeserializeOwned,
{
    let base_label = kind.file();
    let base_text = sources.text_for(kind);
    let label = if std::ptr::eq(base_text, kind.embedded()) {
        base_label.to_string()
    } else {
        format!("覆盖文件 {base_label}")
    };
    let mut root = parse_root_array(&label, base_text, kind)?;

    let mut tracker = ConflictTracker::default();
    for layer in layers.iter() {
        let layer_label = format!("mod {} ({})", layer.mod_id, kind.file());
        let value = parse_jsonc_value(&layer_label, layer.src)?;
        let document: PatchDocument =
            serde_json::from_value(value).map_err(|error| format!("{layer_label}: {error}"))?;
        let Some(document_kind) = ObjectTableKind::parse(&document.table) else {
            return Err(format!(
                "{layer_label}: 未知 table \"{}\"（可用 voxel_objects / entity_objects / item_placement）",
                document.table
            ));
        };
        // 同一个 mod 层会被三张表各扫一遍，只处理目标表的补丁。
        if document_kind != kind {
            continue;
        }
        apply_patch_layer(&mut root, document, kind, layer, &mut tracker)?;
    }

    let defs: Vec<T> = deserialize_table(root, &label)?;
    T::validate(&defs)?;
    Ok(defs)
}

/// 把物品表里的稳定名字引用解析成运行时 id。
fn resolve_items(
    files: Vec<ItemDefFile>,
    blocks: &[VoxelObjectDef],
    entities: &[EntityObjectDef],
) -> Result<Vec<ItemDef>, String> {
    let block_by_name: HashMap<&str, &VoxelObjectDef> =
        blocks.iter().map(|def| (def.name.as_str(), def)).collect();
    let entity_by_name: HashMap<&str, u32> = entities
        .iter()
        .map(|def| (def.name.as_str(), def.id))
        .collect();
    let entity_by_id: HashSet<u32> = entities.iter().map(|def| def.id).collect();

    let mut out = Vec::with_capacity(files.len());
    for file in files {
        let ItemDefFile {
            id,
            name,
            category,
            sub_category,
            tags,
            mass_grams,
            stack_capacity_grams,
            world_prototype,
            version,
            places,
        } = file;

        if !entity_by_id.contains(&world_prototype) {
            return Err(format!(
                "{name} 的 world_prototype({world_prototype}) 不存在"
            ));
        }

        let places = match places {
            None => None,
            Some(PlacedObjectRef::Block { name: target }) => {
                let block = block_by_name
                    .get(target.as_str())
                    .ok_or_else(|| format!("{name} 的 places.block({target}) 不存在"))?;
                // 世界里的体素值是 8 bit 调色板槽位，不是带命名空间的逻辑 id。
                Some(PlacedObject::Block(block.slot))
            }
            Some(PlacedObjectRef::VoxelObject { name: target }) => {
                let voxel = block_by_name
                    .get(target.as_str())
                    .ok_or_else(|| format!("{name} 的 places.voxel_object({target}) 不存在"))?;
                Some(PlacedObject::VoxelObject(voxel.id))
            }
            Some(PlacedObjectRef::Entity { name: target }) => {
                let entity_id = *entity_by_name
                    .get(target.as_str())
                    .ok_or_else(|| format!("{name} 的 places.entity({target}) 不存在"))?;
                Some(PlacedObject::Entity(PrototypeId(entity_id)))
            }
        };

        out.push(ItemDef {
            id,
            name: std::borrow::Cow::Owned(name),
            category,
            sub_category: item::ItemSubCategoryId(sub_category),
            tags: std::borrow::Cow::Owned(tags.into_iter().map(ItemTagId).collect::<Vec<_>>()),
            mass_grams,
            stack_capacity_grams,
            world_prototype: PrototypeId(world_prototype),
            version,
            places,
        });
    }
    Ok(out)
}

/// 走完整流水线：基础层 -> mod 补丁层 -> 反序列化 -> 校验 -> 跨表解析。
pub fn load_object_tables(
    sources: &ObjectTableSources,
    layers: &ObjectLayers,
    generation: u32,
) -> Result<ObjectTables, String> {
    let mut blocks: Vec<VoxelObjectDef> = load_kind(ObjectTableKind::Voxel, sources, layers)?;
    voxel::assign_block_slots(&mut blocks)?;
    let biome_files: Vec<BiomeDefFile> = load_kind(ObjectTableKind::Biome, sources, layers)?;
    let biomes = voxel::resolve_biomes(biome_files, &blocks)?;
    let entities: Vec<EntityObjectDef> = load_kind(ObjectTableKind::Entity, sources, layers)?;
    let item_files: Vec<ItemDefFile> = load_kind(ObjectTableKind::Item, sources, layers)?;
    let items = resolve_items(item_files, &blocks, &entities)?;
    ObjectTables::from_parts(blocks, biomes, entities, items, generation)
}

// ─────────────────────────── 跨线程句柄 / 消息 ───────────────────────────

/// Godot 侧写入、Bevy 在 tick 边界消费的重载请求。
#[derive(Message, Clone, Debug, Default)]
pub struct ReloadObjectTables {
    pub voxel_src: Option<String>,
    pub biome_src: Option<String>,
    pub entity_src: Option<String>,
    pub item_src: Option<String>,
}

impl ReloadObjectTables {
    pub fn from_sources(sources: &ObjectTableSources) -> Self {
        Self {
            voxel_src: sources.voxel.clone(),
            biome_src: sources.biome.clone(),
            entity_src: sources.entity.clone(),
            item_src: sources.item.clone(),
        }
    }

    pub fn to_sources(&self) -> ObjectTableSources {
        ObjectTableSources {
            voxel: self.voxel_src.clone(),
            biome: self.biome_src.clone(),
            entity: self.entity_src.clone(),
            item: self.item_src.clone(),
        }
    }
}

#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectTablesReloaded {
    pub generation: u32,
}

/// 最近一次重载失败的错误文本（成功时清空）；Godot 可读取显示。
#[derive(Resource, Default, Clone, Debug)]
pub struct LastReloadError(pub Option<String>);

/// 跨线程对象表句柄：Godot 线程写 pending，Bevy 线程发布新表。
#[derive(Clone, Debug, Resource)]
pub struct ObjectTablesHandle(Arc<ObjectTablesHandleInner>);

impl Default for ObjectTablesHandle {
    fn default() -> Self {
        Self(Arc::new(ObjectTablesHandleInner {
            // 只有 dev 构建默认允许热重载；联网会话可由外部随时关掉。
            reload_enabled: AtomicBool::new(cfg!(debug_assertions)),
            ..ObjectTablesHandleInner::default()
        }))
    }
}

#[derive(Debug, Default)]
struct ObjectTablesHandleInner {
    current: RwLock<Option<ObjectTables>>,
    pending: Mutex<Option<ReloadObjectTables>>,
    last_error: RwLock<Option<String>>,
    reload_enabled: AtomicBool,
}

impl ObjectTablesHandle {
    pub fn current(&self) -> Option<ObjectTables> {
        self.0.current.read().expect("object tables lock").clone()
    }

    pub fn generation(&self) -> u32 {
        self.current()
            .map(|tables| tables.generation())
            .unwrap_or(0)
    }

    pub fn publish(&self, tables: ObjectTables) {
        *self.0.current.write().expect("object tables lock") = Some(tables);
        *self.0.last_error.write().expect("object tables lock") = None;
    }

    pub fn publish_error(&self, error: String) {
        *self.0.last_error.write().expect("object tables lock") = Some(error);
    }

    pub fn last_error(&self) -> Option<String> {
        self.0
            .last_error
            .read()
            .expect("object tables lock")
            .clone()
    }

    /// 运行期开关：联网会话等场景可关掉热重载。
    pub fn set_reload_enabled(&self, enabled: bool) {
        self.0.reload_enabled.store(enabled, Ordering::Release);
    }

    pub fn reload_enabled(&self) -> bool {
        self.0.reload_enabled.load(Ordering::Acquire)
    }

    /// 只有开关打开时才登记请求；关闭时写入 last_error 并返回 false。
    pub fn queue_reload(&self, request: ReloadObjectTables) -> bool {
        if !self.reload_enabled() {
            self.publish_error("对象表热重载已在运行期禁用".to_string());
            return false;
        }
        *self.0.pending.lock().expect("object tables lock") = Some(request);
        true
    }

    pub fn take_reload(&self) -> Option<ReloadObjectTables> {
        self.0.pending.lock().expect("object tables lock").take()
    }
}
// ─────────────────────────── 插件 ───────────────────────────

/// 静态数据插件：Startup 构建对象表与反查索引；FixedUpdate 在 tick 边界应用重载。
pub struct StaticDataPlugin;

impl Plugin for StaticDataPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ObjectTablesHandle>()
            .init_resource::<ObjectLayers>()
            .init_resource::<LastReloadError>()
            .init_resource::<ItemTagIndex>()
            .init_resource::<voxel::WorldTables>()
            .add_message::<ReloadObjectTables>()
            .add_message::<ObjectTablesReloaded>();
        #[cfg(feature = "physics")]
        app.init_resource::<game_engine::voxel::VoxelSolidity>();
        app.add_systems(
            Startup,
            (
                init_object_tables,
                sync_voxel_solidity,
                build_item_tag_index,
            )
                .chain(),
        )
        // 重载必须在本次 tick 的所有逻辑系统之前落地，否则同一 tick 里
        // 有的系统读到旧表、有的读到新表。
        .add_systems(
            FixedUpdate,
            (apply_object_tables_reload, sync_voxel_solidity)
                .chain()
                .before(EngineSet::Input)
                .before(VoxelSet::Apply),
        );
    }
}

/// 基础层 + 已登记 mod 层 -> ObjectTables。失败时直接 panic：
/// 嵌入数据是编译期常量，加载失败属于程序错误，不应带病启动。
fn init_object_tables(
    handle: Res<ObjectTablesHandle>,
    layers: Res<ObjectLayers>,
    mut commands: Commands,
) {
    let sources = ObjectTableSources::default();
    match load_object_tables(&sources, &layers, 1) {
        Ok(tables) => {
            handle.publish(tables.clone());
            commands.insert_resource(tables);
        }
        Err(error) => panic!("静态数据加载失败: {error}"),
    }
}

/// 把方块 collision 字段同步到物理碰撞的 256 项 LUT。
#[cfg(feature = "physics")]
fn sync_voxel_solidity(
    tables: Res<ObjectTables>,
    mut solidity: ResMut<game_engine::voxel::VoxelSolidity>,
) {
    if !tables.is_changed() {
        return;
    }
    let mut flags = [false; 256];
    for block in tables.blocks() {
        if let Some(slot) = block.block_id() {
            flags[usize::from(slot)] = block.collision;
        }
    }
    *solidity = game_engine::voxel::VoxelSolidity::from_flags(flags);
}

/// 非物理构建里保持系统元组形状一致，不产生任何行为。
#[cfg(not(feature = "physics"))]
fn sync_voxel_solidity() {}

/// 从当前 ObjectTables 构建 ItemTagIndex（确定性：组内升序、去重）。
fn build_item_tag_index(mut index: ResMut<ItemTagIndex>, tables: Res<ObjectTables>) {
    *index = ItemTagIndex::build(tables.items());
}

/// 只在 FixedUpdate（tick 边界）消费重载请求；失败保留旧表。
fn apply_object_tables_reload(
    handle: Res<ObjectTablesHandle>,
    layers: Res<ObjectLayers>,
    mut current: ResMut<ObjectTables>,
    mut last_error: ResMut<LastReloadError>,
    mut tag_index: ResMut<ItemTagIndex>,
    mut reloaded: MessageWriter<ObjectTablesReloaded>,
    mut requests: MessageReader<ReloadObjectTables>,
) {
    // 两条入口：Godot 线程写句柄，游戏内系统发消息。同一 tick 多条时最后一条生效。
    let pending = handle.take_reload();
    let Some(request) = requests.read().last().cloned().or(pending) else {
        return;
    };
    if !handle.reload_enabled() {
        let message = "对象表热重载已在运行期禁用".to_string();
        handle.publish_error(message.clone());
        last_error.0 = Some(message);
        return;
    }
    let generation = current.generation().saturating_add(1);
    match load_object_tables(&request.to_sources(), &layers, generation) {
        Ok(tables) => {
            info!("静态数据重载成功：generation {}", generation);
            *current = tables.clone();
            *tag_index = ItemTagIndex::build(tables.items());
            handle.publish(tables);
            last_error.0 = None;
            reloaded.write(ObjectTablesReloaded { generation });
        }
        Err(error) => {
            error!(
                "静态数据重载失败（保留 generation {}）：{error}",
                current.generation()
            );
            handle.publish_error(error.clone());
            last_error.0 = Some(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_engine::ids::mod_hash_id;

    fn base_layers() -> ObjectLayers {
        ObjectLayers::default()
    }

    static LAYER_MOD_MANIFEST: game_engine::modding::ModManifest =
        game_engine::modding::ModManifest {
            id: "layer_test",
            version: 1,
            engine_api: 1,
            load_after: &[],
            conflicts: &[],
        };

    struct LayerTestMod;

    impl game_engine::modding::Mod<game_engine::spec::DefaultSpec> for LayerTestMod {
        fn manifest(&self) -> &'static game_engine::modding::ModManifest {
            &LAYER_MOD_MANIFEST
        }

        fn install(
            &self,
            context: &mut game_engine::modding::ModContext<'_, game_engine::spec::DefaultSpec>,
        ) {
            context.add_object_layer(
                r#"{
                    "version": 1,
                    "table": "voxel_objects",
                    "patch": [{ "name": "stone", "set": { "replaceable": true } }]
                }"#,
            );
        }
    }

    /// 端到端：Mod 在 install 中登记的补丁层，必须被子系统启动加载重放。
    #[test]
    fn mod_layer_is_replayed_during_startup_load() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StaticDataPlugin);
        let mut data = game_engine::modding::DataRegistry::new();
        game_engine::modding::Mods::<game_engine::spec::DefaultSpec>::new()
            .push(LayerTestMod)
            .apply(&mut app, &mut data)
            .expect("mod apply");
        app.world_mut().run_schedule(Startup);

        let tables = app.world().resource::<ObjectTables>();
        assert!(
            tables.block(1).unwrap().replaceable,
            "mod 层必须在 Startup 加载时重放"
        );
    }

    /// physics 构建下，Startup 必须把方块的 collision 字段同步进碰撞 LUT。
    #[cfg(feature = "physics")]
    #[test]
    fn solidity_follows_the_blocks_collision_field() {
        let patch = r#"{"version":1,"table":"voxel_objects","patch":[{"name":"mymod:ghost","add":{"solid":true,"collision":false}}]}"#;
        let mut layers = ObjectLayers::default();
        layers.add(ObjectLayer {
            mod_id: "mymod",
            load_order: 0,
            src: patch,
        });

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StaticDataPlugin);
        app.insert_resource(layers);
        app.world_mut().run_schedule(Startup);

        let slot = app
            .world()
            .resource::<ObjectTables>()
            .voxel_by_name("mymod:ghost")
            .expect("mod block")
            .slot;
        let solidity = app.world().resource::<game_engine::voxel::VoxelSolidity>();
        assert!(solidity.is_solid(1), "stone 仍然参与碰撞");
        assert!(!solidity.is_solid(slot), "collision=false 的方块不参与碰撞");
    }

    #[test]
    fn static_data_plugin_builds_tag_index() {
        let mut app = App::new();
        app.add_plugins(StaticDataPlugin);
        app.world_mut().run_schedule(Startup);

        let index = app.world().resource::<ItemTagIndex>();
        assert!(index.tag_count() > 0, "标签索引应非空");
        assert_eq!(index.items_with(item::ITEM_TAG_STACKABLE).len(), 3);
        let tables = app.world().resource::<ObjectTables>();
        assert_eq!(tables.generation(), 1);
    }

    #[test]
    fn strip_jsonc_removes_comments_and_trailing_commas() {
        let input = r#"{
            // line
            "url": "http://example.com/a//b",
            /* block */
            "items": [1, 2,],
        }"#;
        let stripped = strip_jsonc(input);
        assert!(stripped.contains("http://example.com/a//b"));
        assert!(!stripped.contains("line"));
        assert!(!stripped.contains("block"));
        let value: Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(value["items"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn embedded_tables_load_and_keep_contracts() {
        let tables = load_object_tables(&ObjectTableSources::default(), &base_layers(), 1)
            .expect("embedded jsonc should load");
        assert_eq!(tables.generation(), 1);
        for (id, name) in [
            (0u8, "air"),
            (1, "stone"),
            (2, "dirt"),
            (3, "grass"),
            (4, "sand"),
            (5, "snow"),
            (6, "wood"),
            (7, "leaf"),
        ] {
            assert_eq!(tables.block(id).map(|def| def.name.as_str()), Some(name));
        }
        let torch = tables.item(ItemTypeId(4)).expect("torch");
        assert_eq!(torch.places, Some(PlacedObject::Entity(PrototypeId(30))));
        assert!(tables.entity(PrototypeId(30)).is_some());
    }

    #[test]
    fn mod_patch_can_set_fields_and_add_namespaced_entries() {
        let patch = r#"{
            "version": 1,
            "table": "voxel_objects",
            "patch": [
                { "name": "stone", "set": { "replaceable": true } },
                { "name": "mymod:glow", "add": { "solid": true } }
            ]
        }"#;
        let mut layers = ObjectLayers::default();
        layers.add(ObjectLayer {
            mod_id: "mymod",
            load_order: 0,
            src: patch,
        });
        let tables = load_object_tables(&ObjectTableSources::default(), &layers, 2).unwrap();
        assert!(tables.block(1).unwrap().replaceable);
        let glow = tables
            .voxel_by_name("mymod:glow")
            .expect("namespaced voxel added");
        assert_eq!(glow.id, mod_hash_id("mymod:glow"));
    }

    #[test]
    fn mod_patch_can_add_namespaced_entity_and_item() {
        let entity_id = mod_hash_id("mymod:lantern");
        let entity_patch = r#"{"version":1,"table":"entity_objects","patch":[{"name":"mymod:lantern","add":{"version":1,"placement":{"anchor":"voxel","footprint":[1,1,1],"requires_support":true}}}]}"#;
        let item_patch = format!(
            r#"{{"version":1,"table":"item_placement","patch":[{{"name":"mymod:lantern_item","add":{{"category":5,"sub_category":0,"tags":[3,10],"mass_grams":1000,"stack_capacity_grams":1000,"world_prototype":{entity_id},"version":1,"places":{{"kind":"entity","name":"mymod:lantern"}}}}}}]}}"#
        );
        let mut layers = ObjectLayers::default();
        layers.add(ObjectLayer {
            mod_id: "mymod",
            load_order: 0,
            src: entity_patch,
        });
        layers.add(ObjectLayer {
            mod_id: "mymod",
            load_order: 0,
            src: Box::leak(item_patch.into_boxed_str()),
        });

        let tables = load_object_tables(&ObjectTableSources::default(), &layers, 2).unwrap();
        let entity = tables
            .entity_by_name("mymod:lantern")
            .expect("namespaced entity added");
        assert_eq!(entity.id, entity_id);
        assert!(entity.placement.requires_support);

        let item = tables
            .item_by_name("mymod:lantern_item")
            .expect("namespaced item added");
        assert_eq!(item.world_prototype, PrototypeId(entity_id));
        assert_eq!(
            item.places,
            Some(crate::static_data::item::PlacedObject::Entity(PrototypeId(
                entity_id
            )))
        );
    }

    #[test]
    fn mod_voxel_gets_palette_slot_and_item_places_by_slot() {
        let block_patch = r#"{"version":1,"table":"voxel_objects","patch":[{"name":"mymod:glow","add":{"solid":true}}]}"#;
        let item_patch = r#"{"version":1,"table":"item_placement","patch":[{"name":"mymod:glow_item","add":{"category":1,"sub_category":0,"tags":[1,3,10],"mass_grams":100,"stack_capacity_grams":6400,"world_prototype":20,"version":1,"places":{"kind":"block","name":"mymod:glow"}}}]}"#;
        let mut layers = ObjectLayers::default();
        layers.add(ObjectLayer {
            mod_id: "mymod",
            load_order: 0,
            src: block_patch,
        });
        layers.add(ObjectLayer {
            mod_id: "mymod",
            load_order: 0,
            src: item_patch,
        });

        let tables = load_object_tables(&ObjectTableSources::default(), &layers, 2).unwrap();
        let glow = tables.voxel_by_name("mymod:glow").expect("mod voxel");
        assert!(glow.slot >= 8, "mod 方块必须分到 8..=255 的槽位");
        assert_eq!(
            tables.block(glow.slot).map(|def| def.name.as_str()),
            Some("mymod:glow"),
            "世界体素值按槽位查表"
        );
        let item = tables.item_by_name("mymod:glow_item").expect("mod item");
        assert_eq!(item.places, Some(PlacedObject::Block(glow.slot)));
    }

    #[test]
    fn two_mods_patching_same_leaf_conflict_by_default() {
        let patch = |value: bool| {
            format!(
                r#"{{"version":1,"table":"voxel_objects","patch":[{{"name":"stone","set":{{"replaceable":{value}}}}}]}}"#
            )
        };
        let first = patch(true);
        let second = patch(false);
        let mut layers = ObjectLayers::default();
        layers.add(ObjectLayer {
            mod_id: "alpha",
            load_order: 0,
            src: Box::leak(first.into_boxed_str()),
        });
        layers.add(ObjectLayer {
            mod_id: "beta",
            load_order: 1,
            src: Box::leak(second.into_boxed_str()),
        });
        let error = load_object_tables(&ObjectTableSources::default(), &layers, 2)
            .expect_err("conflict must be rejected");
        assert!(error.contains("补丁冲突"), "{error}");
    }

    #[test]
    fn unknown_patch_table_is_rejected() {
        let mut layers = ObjectLayers::default();
        layers.add(ObjectLayer {
            mod_id: "typo",
            load_order: 0,
            src: r#"{"version":1,"table":"voxel_object","patch":[]}"#,
        });
        let error = load_object_tables(&ObjectTableSources::default(), &layers, 2)
            .expect_err("unknown table must be rejected");
        assert!(error.contains("未知 table"), "{error}");
    }

    #[test]
    fn higher_priority_wins_and_lower_field_is_pruned() {
        let first = r#"{"version":1,"table":"voxel_objects","patch":[{"name":"stone","priority":1,"set":{"replaceable":true}}]}"#;
        let second = r#"{"version":1,"table":"voxel_objects","patch":[{"name":"stone","priority":2,"set":{"replaceable":false}}]}"#;
        let mut layers = ObjectLayers::default();
        layers.add(ObjectLayer {
            mod_id: "alpha",
            load_order: 0,
            src: first,
        });
        layers.add(ObjectLayer {
            mod_id: "beta",
            load_order: 1,
            src: second,
        });
        let tables = load_object_tables(&ObjectTableSources::default(), &layers, 2).unwrap();
        assert!(!tables.block(1).unwrap().replaceable);
    }

    #[test]
    fn unknown_field_is_rejected_with_source() {
        let patch = r#"{"version":1,"table":"voxel_objects","patch":[{"name":"stone","set":{"replacable":true}}]}"#;
        let mut layers = ObjectLayers::default();
        layers.add(ObjectLayer {
            mod_id: "mymod",
            load_order: 0,
            src: patch,
        });
        let error = load_object_tables(&ObjectTableSources::default(), &layers, 2)
            .expect_err("unknown field must fail");
        assert!(error.contains("unknown field"), "{error}");
    }

    #[test]
    fn reload_applies_on_fixed_update_and_bumps_generation() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StaticDataPlugin);
        app.world_mut().run_schedule(Startup);
        assert_eq!(app.world().resource::<ObjectTables>().generation(), 1);

        let handle = app.world().resource::<ObjectTablesHandle>().clone();
        // 覆盖文件是完整表（不是补丁）；mod 补丁层由 ObjectLayers 重放。
        handle.queue_reload(ReloadObjectTables {
            voxel_src: Some(VOXEL_OBJECTS_JSONC.to_string()),
            biome_src: None,
            entity_src: None,
            item_src: None,
        });
        app.world_mut().run_schedule(FixedUpdate);
        let tables = app.world().resource::<ObjectTables>();
        assert_eq!(
            tables.generation(),
            2,
            "last_error={:?} handle_error={:?}",
            app.world().resource::<LastReloadError>().0,
            handle.last_error()
        );
        assert!(app.world().resource::<LastReloadError>().0.is_none());
    }

    #[test]
    fn reload_message_applies_on_fixed_update() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StaticDataPlugin);
        app.world_mut().run_schedule(Startup);

        app.world_mut().write_message(ReloadObjectTables {
            voxel_src: Some(VOXEL_OBJECTS_JSONC.to_string()),
            biome_src: None,
            entity_src: None,
            item_src: None,
        });
        app.world_mut().run_schedule(FixedUpdate);

        assert_eq!(app.world().resource::<ObjectTables>().generation(), 2);
    }

    #[test]
    fn disabled_reload_is_rejected() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StaticDataPlugin);
        app.world_mut().run_schedule(Startup);

        let handle = app.world().resource::<ObjectTablesHandle>().clone();
        handle.set_reload_enabled(false);
        app.world_mut().write_message(ReloadObjectTables {
            voxel_src: Some(VOXEL_OBJECTS_JSONC.to_string()),
            biome_src: None,
            entity_src: None,
            item_src: None,
        });
        app.world_mut().run_schedule(FixedUpdate);

        assert_eq!(app.world().resource::<ObjectTables>().generation(), 1);
        assert!(handle
            .last_error()
            .is_some_and(|error| error.contains("禁用")));
    }

    #[test]
    fn failed_reload_keeps_old_table_and_records_error() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StaticDataPlugin);
        app.world_mut().run_schedule(Startup);

        let handle = app.world().resource::<ObjectTablesHandle>().clone();
        handle.queue_reload(ReloadObjectTables {
            voxel_src: Some("{ not jsonc".to_string()),
            biome_src: None,
            entity_src: None,
            item_src: None,
        });
        app.world_mut().run_schedule(FixedUpdate);
        let tables = app.world().resource::<ObjectTables>();
        assert_eq!(tables.generation(), 1, "失败必须保留旧表");
        assert!(app.world().resource::<LastReloadError>().0.is_some());
        assert!(handle.last_error().is_some());
    }
}
