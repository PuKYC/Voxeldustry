//! 体素碰撞派生：每个 32³ chunk 一个 parry Voxels Collider。
//!
//! 本模块只在 physics feature 下编译（见 mod.rs）。机制：
//!
//! - collect_chunk_solid_voxels / build_chunk_voxels 是纯函数，输入输出完全确定：
//!   块内按 x -> y -> z 字典序枚举，绝不用 HashMap / 并行 collect；
//! - sync_voxel_collision_chunks 在 VoxelSet::Collision 阶段按 per-chunk 结构哈希
//!   做增量同步（查询用 Added/Changed 过滤，避免每 tick 全量算 hash）。changed 集合
//!   C_set 含：内容哈希 / voxel_size 变化的现有 chunk、新出现的 chunk、以及「被删 chunk
//!   的 in-volume 6 邻」（否则邻居会保留指向空洞的陈旧邻接位）；affected = C_set ∪
//!   N(C_set) 的这些块会被重建 Collider；
//! - cleanup_voxel_colliders 在 VoxelCollider 被移除（含 despawn）时同帧撤销该
//!   body 的全部 chunk 碰撞实体；
//! - VoxelCollisionPlugin 负责装配（可选地补齐 avian 的 PhysicsPlugins）。
//!
//! 坐标约定：VoxVolume 的 chunk 坐标是 body 局部 32³ 分块；每个 chunk 的 Collider
//! 只含该块的局部体素坐标（0..32），实体 Transform 负责把它摆到 body 局部 / 世界
//! 位置。parry 的体素中心为 (k + 0.5) * voxel_size，因此块内 (0,0,0) 覆盖该块
//! 局部 [0, voxel_size]^3。只有构造 Collider 时才把定点 voxel_size 转 f32（物理出口）。
//!
//! 跨块边界：affected = C_set ∪ N(C_set)。相邻两块若都在 affected，用
//! combine_voxel_states（origin_shift = 块偏移 * 32）互相 OR 邻接位；affected 内非空的
//! 块若与「affected 外但有现存实体」的邻块相邻，则用该邻块 Collider 里 stored Voxels 的
//! 克隆参与 combine，把重建时丢掉的邻接位补回来。正确性依据：x ∈ affected 且 y 邻接 x
//! 且 y ∉ affected ⇒ x ∉ C_set（否则 y ∈ N(C_set) ⊆ affected，矛盾），故 x 内容未变，
//! y 现存 shape 关于 x 的位仍有效；y 不写回。这样既避免高速物体在分块接缝处撞上内部边，
//! 也避免反复编辑导致的邻接位退化。组合只走 +x/+y/+z 单向，配合 BTreeMap 升序与
//! neighbors6 固定顺序，保证每对只处理一次且顺序确定。
//!
//! 实体布局（dual path）：
//! - root 带 RigidBody：chunk 实体挂 ChildOf(root)，Transform 为块局部偏移；avian
//!   的 ColliderHierarchyPlugin 会自动补 ColliderOf / ColliderTransform；
//! - root 不带 RigidBody：chunk 实体是顶层静态碰撞体（不挂 ChildOf），Transform 取
//!   root GlobalTransform 变换后的世界位置。
//!
//! 实心 vs 外壳：collect_solid_voxels / collect_surface_voxels / build_voxel_collider
//! 是整 body 版本的纯函数，保留给 bench 对比；引擎系统不再使用整 body 版本。
//! 注意 parry 0.27 的 Voxels 与 Compound 都不可变，任何修改都只能整体重建。
//!
//! 确定性边界：enhanced-determinism 会把 libm 数学与保序哈希传给 parry，使
//! “固定枚举顺序 -> 固定 shape 构造顺序”成立；但这不等于跨平台浮点数值一致，
//! 物理的跨平台可复现性不由本模块保证。

use std::collections::BTreeMap;

use ::voxel::store::{ChunkKey, VoxInterner, VoxOpsRead};
use avian3d::math::Vector;
use avian3d::parry::shape::{SharedShape, Voxels};
use avian3d::prelude::{Collider, PhysicsPlugins, PhysicsSchedulePlugin, RigidBody};
use bevy::prelude::*;

use super::change::VoxelSet;
use super::hash::chunk_content_hash;
use super::interner::VoxelInterner;
use super::key::{chunk_voxels_per_axis, neighbors6};
use super::volume::VoxVolume;
use crate::math::FixedPoint;

/// 标记组件：实体需要从它的 VoxVolume 派生 per-chunk 体素 Collider。
///
/// - 存在即派生，移除即撤销（cleanup_voxel_colliders 同帧处理）；
/// - #[require(Transform)]：约定 Transform 就是 body 局部原点，
///   chunk 的 grid_coordinates 用块局部体素坐标（0..32）；
/// - 具体派生状态记账在每个 VoxelCollisionChunk 实体上，不再有整 body 记账。
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[require(Transform)]
pub struct VoxelCollider;

/// 一个 32³ chunk 的碰撞实体：记录它属于哪个 root body、是哪个 chunk，
/// 以及上一次同步时的内容哈希 / voxel_size 位。
///
/// sync_voxel_collision_chunks 用 content_hash 判断该 chunk 是否需要重建；
/// voxel_size_bits 单独记录，因为 content_hash 只覆盖体素结构、不含 voxel_size。
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoxelCollisionChunk {
    /// 该 chunk Collider 所属的 root body 实体。
    pub body: Entity,
    /// body 局部的 chunk 坐标。
    pub key: ChunkKey,
    /// 上一次构建时的块结构哈希。
    pub content_hash: u64,
    /// 上一次构建时的 voxel_size 原始定点位（FixedPoint::to_bits()）。
    pub voxel_size_bits: i64,
}

/// 体素边长的 f32 表现出口（物理 / 渲染边界才转浮点）。
#[inline]
pub fn voxel_size_meters_f32(voxel_size: FixedPoint) -> f32 {
    voxel_size.to_num::<f32>()
}

/// 收集单个 chunk 的实心体素局部坐标（块内 0..32）。
///
/// 顺序确定：块内按 x -> y -> z 字典序枚举 local 0..chunk_voxels_per_axis()；
/// get 返回 None（默认 / 空）的体素跳过。chunk 不存在或为空返回空 Vec。
pub fn collect_chunk_solid_voxels(
    volume: &VoxVolume,
    interner: &VoxInterner<u8>,
    key: ChunkKey,
) -> Vec<bevy::math::IVec3> {
    let c = chunk_voxels_per_axis();
    let mut out = Vec::new();
    let Some(tree) = volume.get_chunk(&key) else {
        return out;
    };
    for x in 0..c {
        for y in 0..c {
            for z in 0..c {
                let local = bevy::math::IVec3::new(x, y, z);
                if tree.get(interner, local).is_some() {
                    out.push(local);
                }
            }
        }
    }
    out
}

/// 由单个 chunk 构建 parry Voxels（局部坐标 0..32）；空 chunk 或非法 voxel_size
/// 返回 None。
///
/// 非法 voxel_size 会 warn + debug_assert 后返回 None —— 此时调用方会把它当作
/// 「该 chunk 应为空」。voxel_size 的合法性由上层保证。
pub fn build_chunk_voxels(
    volume: &VoxVolume,
    interner: &VoxInterner<u8>,
    key: ChunkKey,
) -> Option<Voxels> {
    let coords = collect_chunk_solid_voxels(volume, interner, key);
    if coords.is_empty() {
        return None;
    }
    let scale = voxel_size_meters_f32(volume.voxel_size);
    if !(scale.is_finite() && scale > 0.0) {
        warn!(
            "voxel chunk collider skipped: key={key:?} invalid voxel_size={scale} voxels={}",
            coords.len()
        );
        debug_assert!(
            scale > 0.0,
            "voxel_size must be positive to build a chunk collider"
        );
        return None;
    }
    Some(Voxels::new(Vector::splat(scale), &coords))
}

/// 收集 volume 的全部实心体素全局坐标（body_voxel 语义）。整 body 版本，保留给 bench。
///
/// 顺序确定：chunk 按 BTreeMap（ChunkKey 升序）遍历；块内按 x -> y -> z 字典序
/// 枚举 local 0..chunk_voxels_per_axis()；get 返回 None（默认 / 空）的体素跳过。
pub fn collect_solid_voxels(
    volume: &VoxVolume,
    interner: &VoxInterner<u8>,
) -> Vec<bevy::math::IVec3> {
    let c = chunk_voxels_per_axis();
    let mut out = Vec::new();
    for (key, tree) in volume.chunks.iter() {
        for x in 0..c {
            for y in 0..c {
                for z in 0..c {
                    let local = bevy::math::IVec3::new(x, y, z);
                    if tree.get(interner, local).is_some() {
                        out.push(bevy::math::IVec3::new(
                            key.x * c + x,
                            key.y * c + y,
                            key.z * c + z,
                        ));
                    }
                }
            }
        }
    }
    out
}

/// 单个体素是否为实心（跨 chunk 边界安全）。
fn solid_at(volume: &VoxVolume, interner: &VoxInterner<u8>, world: bevy::math::IVec3) -> bool {
    let c = chunk_voxels_per_axis();
    let key = ChunkKey::new(
        world.x.div_euclid(c),
        world.y.div_euclid(c),
        world.z.div_euclid(c),
    );
    let Some(tree) = volume.get_chunk(&key) else {
        return false;
    };
    let local = bevy::math::IVec3::new(
        world.x.rem_euclid(c),
        world.y.rem_euclid(c),
        world.z.rem_euclid(c),
    );
    tree.get(interner, local).is_some()
}

/// 收集「表面」实心体素：仅当六邻里至少有一个非实心时才收录。整 body 版本，保留给 bench。
///
/// 顺序与 collect_solid_voxels 一致（chunk 升序 + x -> y -> z）。
pub fn collect_surface_voxels(
    volume: &VoxVolume,
    interner: &VoxInterner<u8>,
) -> Vec<bevy::math::IVec3> {
    let c = chunk_voxels_per_axis();
    let mut out = Vec::new();
    for (key, tree) in volume.chunks.iter() {
        for x in 0..c {
            for y in 0..c {
                for z in 0..c {
                    let local = bevy::math::IVec3::new(x, y, z);
                    if tree.get(interner, local).is_none() {
                        continue;
                    }
                    let world =
                        bevy::math::IVec3::new(key.x * c + x, key.y * c + y, key.z * c + z);
                    let exposed = [
                        bevy::math::IVec3::X,
                        bevy::math::IVec3::NEG_X,
                        bevy::math::IVec3::Y,
                        bevy::math::IVec3::NEG_Y,
                        bevy::math::IVec3::Z,
                        bevy::math::IVec3::NEG_Z,
                    ]
                    .iter()
                    .any(|&n| !solid_at(volume, interner, world + n));
                    if exposed {
                        out.push(world);
                    }
                }
            }
        }
    }
    out
}

/// 由整个 volume 构建一个 avian 体素 Collider（全体素，保留内部）；空体积或非法
/// voxel_size 返回 None。整 body 版本，保留给 bench 对比。
pub fn build_voxel_collider(volume: &VoxVolume, interner: &VoxInterner<u8>) -> Option<Collider> {
    let coords = collect_solid_voxels(volume, interner);
    if coords.is_empty() {
        return None;
    }
    let scale = voxel_size_meters_f32(volume.voxel_size);
    if !(scale.is_finite() && scale > 0.0) {
        warn!(
            "voxel collider skipped: invalid voxel_size={scale} surface_voxels={}",
            coords.len()
        );
        debug_assert!(scale > 0.0, "voxel_size must be positive to build a collider");
        return None;
    }
    Some(Collider::voxels(Vector::splat(scale), &coords))
}

/// 在一个 chunk 位置生成碰撞实体（dual path）。
///
/// - root 带 RigidBody：挂 ChildOf(root)，Transform 为块局部偏移；avian 会自动补
///   ColliderOf / ColliderTransform 并按 RigidBody 的位姿摆好；
/// - root 不带 RigidBody：作为顶层静态碰撞体，Transform 取 root GlobalTransform
///   变换后的世界位置（avian 的 transform_to_position 会把 GlobalTransform 同步到
///   Position / Rotation）。
fn spawn_chunk_collider(
    commands: &mut Commands,
    root: Entity,
    root_global: &GlobalTransform,
    body: Option<&RigidBody>,
    chunk: VoxelCollisionChunk,
    local_offset: Vec3,
    collider: Collider,
) {
    if body.is_some() {
        commands.spawn((
            collider,
            chunk,
            Transform::from_translation(local_offset),
            ChildOf(root),
        ));
    } else {
        let world = root_global.transform_point(local_offset);
        commands.spawn((collider, chunk, Transform::from_translation(world)));
    }
}

/// 按 per-chunk 结构哈希把 VoxVolume 同步为一组 chunk 碰撞实体（在 VoxelSet::Collision）。
///
/// 过滤 Added/Changed：新挂 VoxelCollider、新增或编辑 VoxVolume 才进入；
/// 其余 tick 不算 hash。对每个匹配的 root：
/// 1. 收集它现有的 VoxelCollisionChunk 实体（ChunkKey 升序）；
/// 2. 逐 chunk 比对 content_hash / voxel_size_bits，得到 changed_keys；再把「已被删除、
///    但还有 in-volume 邻块」的那些邻块也补进 changed_keys；
/// 3. affected = changed_keys ∪ 其 6 邻（仅限 volume 中存在的 key），并构建 Voxels
///    （空 chunk 记为 None = 应为空）；
/// 4. 对 affected 内相邻块做 combine_voxel_states（单向 +x/+y/+z）；
/// 4b. 对 affected 内非空的块，与其「affected 外但有现存实体」的邻块，用该邻块
///     Collider 里 stored Voxels 的克隆再做一次 combine（补回重建丢掉的邻接位）；
/// 5. 落盘：替换 / 生成 / 撤销 chunk 实体，并 try_despawn 已不在 volume 的旧 chunk。
///
/// 不复用 VoxelDirtySet：脏 key 不带 body 身份，多个 body 可能共用同一 key；
/// Changed<VoxVolume> + per-chunk hash diff 已覆盖增量。
pub fn sync_voxel_collision_chunks(
    mut commands: Commands,
    interner: Res<VoxelInterner>,
    roots: Query<
        (Entity, &VoxVolume, &GlobalTransform, Option<&RigidBody>),
        (
            With<VoxelCollider>,
            Or<(Added<VoxVolume>, Changed<VoxVolume>, Added<VoxelCollider>)>,
        ),
    >,
    mut chunks: Query<(
        Entity,
        &mut VoxelCollisionChunk,
        Option<&mut Collider>,
        Option<&mut Transform>,
    )>,
) {
    let interner = interner.inner();
    let chunk_axis = chunk_voxels_per_axis();
    for (root, volume, root_global, body) in &roots {
        // ── 1. 该 root 现有的 chunk 实体（ChunkKey 升序，确定性）──
        let mut existing: BTreeMap<ChunkKey, (Entity, u64, i64)> = BTreeMap::new();
        for (entity, chunk, _collider, _transform) in chunks.iter() {
            if chunk.body == root {
                existing.insert(
                    chunk.key,
                    (entity, chunk.content_hash, chunk.voxel_size_bits),
                );
            }
        }

        // ── 2. per-chunk 内容哈希 diff ──
        let voxel_size_bits = volume.voxel_size.to_bits();
        let mut changed_keys: Vec<ChunkKey> = Vec::new();
        for (key, tree) in volume.chunks.iter() {
            let hash = chunk_content_hash(interner, tree);
            match existing.get(key) {
                Some((_, old_hash, old_bits))
                    if *old_hash == hash && *old_bits == voxel_size_bits => {}
                _ => changed_keys.push(*key),
            }
        }

        // ── 2b. 被删 chunk 的 in-volume 6 邻补进 changed_keys ──
        // changed_keys 只从 volume 现有 chunk 推出；被删的 key 已不在 volume，其邻居
        // 若不回炉，就会保留指向空洞的陈旧邻接位（combine 只 OR、清不掉）。把这些邻居
        // 也标为 changed，它们会被 fresh 重建，朝空洞的位自然为 0。
        for (key, _cell) in existing.iter() {
            if volume.contains_chunk(key) {
                continue;
            }
            for neighbor in neighbors6(*key) {
                if volume.contains_chunk(&neighbor) && !changed_keys.contains(&neighbor) {
                    changed_keys.push(neighbor);
                }
            }
        }

        // ── 3. affected = changed ∪ 6 邻（仅保留 volume 中存在的 key）；None = 应为空 ──
        let mut affected: BTreeMap<ChunkKey, Option<Voxels>> = BTreeMap::new();
        for key in changed_keys.iter().copied() {
            for candidate in std::iter::once(key).chain(neighbors6(key)) {
                if !volume.contains_chunk(&candidate) {
                    continue;
                }
                if affected.contains_key(&candidate) {
                    continue;
                }
                affected.insert(candidate, build_chunk_voxels(volume, interner, candidate));
            }
        }

        // ── 4. 跨块边界合并：只走 +x/+y/+z，BTreeMap 升序保证每对只处理一次 ──
        let affected_keys: Vec<ChunkKey> = affected.keys().copied().collect();
        for key in affected_keys.iter().copied() {
            for delta in [
                bevy::math::IVec3::X,
                bevy::math::IVec3::Y,
                bevy::math::IVec3::Z,
            ] {
                let neighbor = ChunkKey::new(key.x + delta.x, key.y + delta.y, key.z + delta.z);
                if !affected.contains_key(&neighbor) {
                    continue;
                }
                let a_cell = affected.remove(&key).expect("affected key present");
                let b_cell = affected.remove(&neighbor).expect("affected neighbor present");
                match (a_cell, b_cell) {
                    (Some(mut a), Some(mut b)) => {
                        let shift = bevy::math::IVec3::new(
                            neighbor.x - key.x,
                            neighbor.y - key.y,
                            neighbor.z - key.z,
                        ) * chunk_axis;
                        a.combine_voxel_states(&mut b, shift);
                        affected.insert(key, Some(a));
                        affected.insert(neighbor, Some(b));
                    }
                    (a_cell, b_cell) => {
                        affected.insert(key, a_cell);
                        affected.insert(neighbor, b_cell);
                    }
                }
            }
        }

        // ── 4b. affected 块与「affected 外但有现存实体」的邻块做 stored-shape combine ──
        //
        // 正确性依据：affected = C_set ∪ N(C_set)（C_set = changed_keys）。若 x ∈ affected
        // 且 y 与 x 邻接且 y ∉ affected，则必有 x ∉ C_set —— 否则 y ∈ N(C_set) ⊆ affected，
        // 矛盾；即 x 的 volume 内容未变。因此 y 现存 Collider 的 Voxels 里关于 x 的邻接位
        // 仍然有效：用 y 的 stored shape 参与 combine，就能把 x 因 fresh 重建丢掉的位补回来。
        // y 自身的位没有变化，不需要写回（丢弃克隆即可）。
        //
        // 确定性：x 按 affected_keys 的 BTreeMap 升序、6 邻按 neighbors6 固定顺序。
        // 借用顺序：这里只用 chunks.get（只读 query item），必须在 Step 5b 的
        // chunks.get_mut 之前完成，且 clone 出的 Voxels 不跨语句持有 Collider 借用。
        for key in affected_keys.iter().copied() {
            let Some(cell) = affected.remove(&key) else {
                continue;
            };
            let Some(mut voxels) = cell else {
                // 该 key 应为空：不参与边界合并，原样放回。
                affected.insert(key, None);
                continue;
            };
            for neighbor in neighbors6(key) {
                if affected.contains_key(&neighbor) {
                    // affected 内部的配对已在 Step 4 处理。
                    continue;
                }
                // neighbor 必须仍在 volume：被删的 chunk 实体要到 Step 5a 才 try_despawn，
                // 此刻 existing 里还残留它的旧实体；若拿它的陈旧 shape 回填，会把 Step 2b
                // 刚清掉的、指向空洞的邻接位又 OR 回来。
                if !volume.contains_chunk(&neighbor) {
                    continue;
                }
                let Some((entity, _hash, _bits)) = existing.get(&neighbor).copied() else {
                    // affected 外且无现存实体：本就没派生，跳过。
                    continue;
                };
                // chunks.get 返回只读 query item：Option<&mut Collider> 降为 Option<&Collider>。
                let stored = chunks.get(entity).ok().and_then(
                    |(_entity, _chunk, collider, _transform)| {
                        collider.and_then(|collider| collider.shape().as_voxels().cloned())
                    },
                );
                let Some(mut neighbor_voxels) = stored else {
                    continue;
                };
                let shift = bevy::math::IVec3::new(
                    neighbor.x - key.x,
                    neighbor.y - key.y,
                    neighbor.z - key.z,
                ) * chunk_axis;
                voxels.combine_voxel_states(&mut neighbor_voxels, shift);
                // neighbor_voxels 是克隆，丢弃即可；y 的实体不在本次落盘范围。
            }
            affected.insert(key, Some(voxels));
        }

        // ── 5a. 旧 chunk 已不在 volume：撤销 ──
        for (key, (entity, _hash, _bits)) in existing.iter() {
            if !volume.contains_chunk(key) {
                if let Ok(mut entity_commands) = commands.get_entity(*entity) {
                    entity_commands.try_despawn();
                }
            }
        }

        // ── 5b. 落盘 affected ──
        let size = voxel_size_meters_f32(volume.voxel_size);
        for (key, voxels) in affected {
            let old_entity = existing.get(&key).map(|(entity, _hash, _bits)| *entity);
            let Some(voxels) = voxels else {
                // 该 key 在 volume 中但内容为空：撤销已有实体。
                if let Some(entity) = old_entity {
                    if let Ok(mut entity_commands) = commands.get_entity(entity) {
                        entity_commands.try_despawn();
                    }
                }
                continue;
            };
            let content_hash = volume
                .get_chunk(&key)
                .map(|tree| chunk_content_hash(interner, tree))
                .unwrap_or(0);
            let collider = Collider::from(SharedShape::new(voxels));
            let local_offset = Vec3::new(
                key.x as f32 * chunk_axis as f32 * size,
                key.y as f32 * chunk_axis as f32 * size,
                key.z as f32 * chunk_axis as f32 * size,
            );
            match old_entity {
                Some(entity) => {
                    if let Ok((_entity, mut chunk, collider_slot, transform_slot)) =
                        chunks.get_mut(entity)
                    {
                        *chunk = VoxelCollisionChunk {
                            body: root,
                            key,
                            content_hash,
                            voxel_size_bits,
                        };
                        match collider_slot {
                            // 编辑后整体替换：绝不在 shape_mut() 后再 set_scale。
                            Some(mut slot) => *slot = collider,
                            None => {
                                commands.entity(entity).insert(collider);
                            }
                        }
                        // voxel_size / root 位姿变化时同步块偏移（voxel_size_bits 变化
                        // 也会让该 key 进入 affected）。
                        if let Some(mut transform) = transform_slot {
                            transform.translation = if body.is_some() {
                                local_offset
                            } else {
                                root_global.transform_point(local_offset)
                            };
                        }
                    }
                }
                None => {
                    spawn_chunk_collider(
                        &mut commands,
                        root,
                        root_global,
                        body,
                        VoxelCollisionChunk {
                            body: root,
                            key,
                            content_hash,
                            voxel_size_bits,
                        },
                        local_offset,
                        collider,
                    );
                }
            }
        }
    }
}

/// 移除 VoxelCollider 时（含 despawn）撤销该 body 的全部 chunk 碰撞实体。
///
/// 与 sync_voxel_collision_chunks 组成 (cleanup, rebuild).chain()，同帧完成撤销。
/// RemovedComponents 也覆盖 despawn；无 RigidBody 的顶层 chunk 不会被自动回收，
/// 必须在这里显式 despawn（有 RigidBody 的子实体虽会随 root 自动 despawn，显式处理
/// 也要安全）。用 get_entity 过滤已销毁的实体，避免向不存在的实体排命令。
pub fn cleanup_voxel_colliders(
    mut commands: Commands,
    mut removed: RemovedComponents<VoxelCollider>,
    chunks: Query<(Entity, &VoxelCollisionChunk)>,
) {
    for root in removed.read() {
        for (entity, chunk) in &chunks {
            if chunk.body != root {
                continue;
            }
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_despawn();
            }
        }
    }
}

/// 体素碰撞插件：注册 VoxelSet::Collision 阶段的派生 / 撤销系统。
///
/// ensure_physics = true（默认）时，若 App 里还没装 avian 的 PhysicsPlugins
/// 就补上（用组内公开的 PhysicsSchedulePlugin 判定，因为 PhysicsPlugins
/// 实现的是 PluginGroup 而不是 Plugin，不能直接 is_plugin_added）。
pub struct VoxelCollisionPlugin {
    pub ensure_physics: bool,
}

impl Default for VoxelCollisionPlugin {
    fn default() -> Self {
        Self {
            ensure_physics: true,
        }
    }
}

impl Plugin for VoxelCollisionPlugin {
    fn build(&self, app: &mut App) {
        if self.ensure_physics && !app.is_plugin_added::<PhysicsSchedulePlugin>() {
            app.add_plugins(PhysicsPlugins::default());
        }
        app.configure_sets(
            FixedUpdate,
            (VoxelSet::Apply, VoxelSet::Collision, VoxelSet::Mesh).chain(),
        )
        .add_systems(
            FixedUpdate,
            (cleanup_voxel_colliders, sync_voxel_collision_chunks)
                .chain()
                .in_set(VoxelSet::Collision),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::voxel::store::{ChunkKey, MaxDepth, VoxOpsBulkWrite, VoxTree, CHUNK_DEPTH};

    fn solid_volume(interner: &mut VoxelInterner) -> VoxVolume {
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
        tree.fill(interner.inner_mut(), 1);
        let mut volume = VoxVolume::new(FixedPoint::from_num(1));
        volume.insert_chunk(ChunkKey::new(0, 0, 0), tree);
        volume
    }

    #[test]
    fn full_chunk_builds_some_voxel_collider() {
        let mut interner = VoxelInterner::new(4 * 1024 * 1024);
        let volume = solid_volume(&mut interner);
        let coords = collect_solid_voxels(&volume, interner.inner());
        assert_eq!(coords.len(), 32 * 32 * 32);
        assert_eq!(
            coords.first().copied(),
            Some(bevy::math::IVec3::new(0, 0, 0))
        );
        assert_eq!(
            coords.last().copied(),
            Some(bevy::math::IVec3::new(31, 31, 31))
        );
        assert!(build_voxel_collider(&volume, interner.inner()).is_some());
    }

    #[test]
    fn chunk_coords_are_local_and_ordered() {
        let mut interner = VoxelInterner::new(4 * 1024 * 1024);
        let volume = solid_volume(&mut interner);
        let key = ChunkKey::new(0, 0, 0);
        let coords = collect_chunk_solid_voxels(&volume, interner.inner(), key);
        assert_eq!(coords.len(), 32 * 32 * 32);
        assert_eq!(coords.first().copied(), Some(bevy::math::IVec3::new(0, 0, 0)));
        assert_eq!(
            coords.last().copied(),
            Some(bevy::math::IVec3::new(31, 31, 31))
        );
        assert!(build_chunk_voxels(&volume, interner.inner(), key).is_some());
        // 不存在的 chunk -> 空 / None。
        assert!(collect_chunk_solid_voxels(&volume, interner.inner(), ChunkKey::new(5, 0, 0))
            .is_empty());
        assert!(build_chunk_voxels(&volume, interner.inner(), ChunkKey::new(5, 0, 0)).is_none());
    }

    #[test]
    fn surface_of_full_chunk_is_a_shell() {
        let mut interner = VoxelInterner::new(4 * 1024 * 1024);
        let volume = solid_volume(&mut interner);
        let coords = collect_surface_voxels(&volume, interner.inner());
        // 32^3 实心块的表层 = 32^3 - 30^3（内部 30^3 被挖空）。
        assert_eq!(coords.len(), 32 * 32 * 32 - 30 * 30 * 30);
        assert!(coords.contains(&bevy::math::IVec3::new(0, 0, 0)));
        assert!(!coords.contains(&bevy::math::IVec3::new(15, 15, 15)));
    }

    #[test]
    fn empty_volume_builds_none() {
        let interner = VoxelInterner::new(1024 * 1024);
        let volume = VoxVolume::new(FixedPoint::from_num(1));
        assert!(collect_solid_voxels(&volume, interner.inner()).is_empty());
        assert!(build_voxel_collider(&volume, interner.inner()).is_none());
        assert!(collect_chunk_solid_voxels(&volume, interner.inner(), ChunkKey::new(0, 0, 0))
            .is_empty());
        assert!(build_chunk_voxels(&volume, interner.inner(), ChunkKey::new(0, 0, 0)).is_none());
    }

    #[test]
    fn voxel_size_is_converted_to_meters() {
        let half = FixedPoint::from_num(1) / FixedPoint::from_num(2);
        assert_eq!(voxel_size_meters_f32(half), 0.5);
    }
}
