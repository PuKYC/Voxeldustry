//! 体素改变缓冲：挂在带 VoxVolume 的实体上的待应用编辑队列。
use bevy::prelude::*;
use voxel::store::{ChunkKey, VoxOpsWrite};

use super::dirty::{VoxelBox, VoxelDirtySet};
use super::interner::VoxelInterner;
use super::key::chunk_voxels_per_axis;
use super::volume::VoxVolume;

/// 一次体素编辑：体素全局坐标（相对该实体 VoxVolume，即 body_voxel）+ 新值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoxelEdit {
    pub voxel: [i32; 3],
    pub value: u8,
}

impl VoxelEdit {
    pub const fn new(voxel: [i32; 3], value: u8) -> Self {
        Self { voxel, value }
    }
}

/// 每实体体素改变缓冲组件。只有同时带 VoxVolume 的实体才有意义。
#[derive(Component, Default, Debug)]
pub struct VoxelChangeBuffer {
    edits: Vec<VoxelEdit>,
}

impl VoxelChangeBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, voxel: [i32; 3], value: u8) {
        self.edits.push(VoxelEdit::new(voxel, value));
    }

    pub fn push_edit(&mut self, edit: VoxelEdit) {
        self.edits.push(edit);
    }

    pub fn extend(&mut self, edits: impl IntoIterator<Item = VoxelEdit>) {
        self.edits.extend(edits);
    }

    pub fn len(&self) -> usize {
        self.edits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }

    pub fn edits(&self) -> &[VoxelEdit] {
        &self.edits
    }

    pub fn drain(&mut self) -> impl Iterator<Item = VoxelEdit> + '_ {
        self.edits.drain(..)
    }

    pub fn clear(&mut self) {
        self.edits.clear();
    }
}

/// 体素机制系统阶段（仿 input::InputSet）。
///
/// 顺序固定为 Apply -> Collision -> Mesh：
/// - Apply：把编辑缓冲写进 VoxVolume；
/// - Collision：按 volume 内容派生 / 撤销物理 Collider；
/// - Mesh：重建网格。
///
/// Collision 变体无论如何都存在，保证阶段顺序在不同 feature 组合下一致。
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VoxelSet {
    Apply,
    Collision,
    Mesh,
}

/// 单点体素写入：把 body 局部整数体素坐标处设为 `value`，并在真正改变时
/// 把所在 chunk 的这一格标记为脏。
///
/// - `Ok(true)`：写入生效；
/// - `Ok(false)`：值本来就相同，树未变、也不标脏；
/// - `Err(key)`：目标 chunk 不存在（不创建 chunk）。
///
/// 这是世界层唯一需要的「单点写入 + 标脏」封装；`VoxTree::set` 本身已经
/// 有结构共享 / interner 去重，本函数只做 chunk 映射与脏标记，不复制节点。
pub fn set_voxel(
    volume: &mut VoxVolume,
    interner: &mut VoxelInterner,
    dirty: &mut VoxelDirtySet,
    voxel: [i32; 3],
    value: u8,
) -> Result<bool, ChunkKey> {
    let c = chunk_voxels_per_axis();
    let key = ChunkKey::new(
        voxel[0].div_euclid(c),
        voxel[1].div_euclid(c),
        voxel[2].div_euclid(c),
    );
    let local = [
        voxel[0].rem_euclid(c) as u8,
        voxel[1].rem_euclid(c) as u8,
        voxel[2].rem_euclid(c) as u8,
    ];
    let Some(tree) = volume.get_chunk_mut(&key) else {
        return Err(key);
    };
    let local_ivec = IVec3::new(
        i32::from(local[0]),
        i32::from(local[1]),
        i32::from(local[2]),
    );
    let changed = tree.set(interner.inner_mut(), local_ivec, value);
    if changed {
        dirty.mark_edited(key, VoxelBox::of_voxel(local));
    }
    Ok(changed)
}

/// 把每实体 VoxelChangeBuffer 应用到它的 VoxVolume 并标脏。
///
/// 查询 `(&mut VoxVolume, &mut VoxelChangeBuffer)` 天然保证「有体素组件才有效」。
/// 目标子块不存在：跳过 + `warn!` + `debug_assert!`。
pub fn apply_voxel_changes(
    mut interner: ResMut<VoxelInterner>,
    mut dirty: ResMut<VoxelDirtySet>,
    mut query: Query<(&mut VoxVolume, &mut VoxelChangeBuffer)>,
) {
    for (mut volume, mut buffer) in &mut query {
        for edit in buffer.drain() {
            match set_voxel(
                &mut volume,
                &mut interner,
                &mut dirty,
                edit.voxel,
                edit.value,
            ) {
                Ok(_) => {}
                Err(key) => {
                    warn!(
                        "voxel edit target chunk missing: key={key:?} voxel={:?}",
                        edit.voxel
                    );
                    debug_assert!(false, "voxel edit target chunk missing: key={key:?}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::FixedPoint;
    use ::voxel::store::{MaxDepth, VoxOpsRead, VoxOpsState, VoxTree, CHUNK_DEPTH};

    #[test]
    fn buffer_records_push_order_and_drains_clean() {
        let mut buffer = VoxelChangeBuffer::new();
        buffer.push([1, 2, 3], 4);
        buffer.push([5, 6, 7], 9);
        assert_eq!(buffer.len(), 2);
        assert!(!buffer.is_empty());
        assert_eq!(
            buffer.edits().to_vec(),
            vec![VoxelEdit::new([1, 2, 3], 4), VoxelEdit::new([5, 6, 7], 9)]
        );

        let drained: Vec<VoxelEdit> = buffer.drain().collect();
        assert_eq!(
            drained,
            vec![VoxelEdit::new([1, 2, 3], 4), VoxelEdit::new([5, 6, 7], 9)]
        );
        assert!(buffer.is_empty());
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn apply_voxel_changes_writes_volume_and_marks_dirty() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(VoxelInterner::new(1024 * 1024))
            .init_resource::<VoxelDirtySet>()
            .add_systems(Update, apply_voxel_changes);

        let key = ChunkKey::new(0, 0, 0);
        let mut volume = VoxVolume::new(FixedPoint::from_num(1));
        volume.insert_chunk(key, VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH)));

        let mut with_volume = VoxelChangeBuffer::new();
        with_volume.push([5, 6, 7], 9);
        let entity = app.world_mut().spawn((volume, with_volume)).id();

        // 没有 VoxVolume 的实体：系统查询不匹配，缓冲保持不变（不触发 missing-chunk）。
        let mut without_volume = VoxelChangeBuffer::new();
        without_volume.push([5, 6, 7], 9);
        let orphan = app.world_mut().spawn(without_volume).id();

        app.update();

        assert!(app
            .world()
            .entity(entity)
            .get::<VoxelChangeBuffer>()
            .unwrap()
            .is_empty());
        assert_eq!(
            app.world()
                .entity(orphan)
                .get::<VoxelChangeBuffer>()
                .unwrap()
                .len(),
            1
        );

        let dirty = app.world().resource::<VoxelDirtySet>();
        assert!(dirty.is_dirty(&key));
        assert_eq!(dirty.len(), 1);
        assert_eq!(
            dirty.peek_edits()[0].edited,
            VoxelBox::of_voxel([5, 6, 7]),
            "单点写入只能标记这一格，不能退化成整块脏"
        );

        let volume = app.world().entity(entity).get::<VoxVolume>().unwrap();
        let tree = volume.get_chunk(&key).unwrap();
        assert!(!tree.is_empty(), "被编辑的体素必须让子块非空");
        assert_eq!(
            tree.get(
                app.world().resource::<VoxelInterner>().inner(),
                IVec3::new(5, 6, 7)
            ),
            Some(9),
            "写入后必须能读回同一个值"
        );
    }

    fn chunk_with_volume() -> (VoxVolume, ChunkKey) {
        let key = ChunkKey::new(0, 0, 0);
        let mut volume = VoxVolume::new(FixedPoint::from_num(1));
        volume.insert_chunk(key, VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH)));
        (volume, key)
    }

    #[test]
    fn set_voxel_roundtrips_and_marks_only_that_voxel() {
        let (mut volume, key) = chunk_with_volume();
        let mut interner = VoxelInterner::new(1024 * 1024);
        let mut dirty = VoxelDirtySet::new();

        assert_eq!(
            set_voxel(&mut volume, &mut interner, &mut dirty, [5, 6, 7], 9),
            Ok(true)
        );
        assert_eq!(
            volume
                .get_chunk(&key)
                .unwrap()
                .get(interner.inner(), IVec3::new(5, 6, 7)),
            Some(9)
        );
        let edits = dirty.peek_edits();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].key, key);
        assert_eq!(edits[0].edited, VoxelBox::of_voxel([5, 6, 7]));

        // 同值写入：树不变，不重复标脏。
        assert_eq!(
            set_voxel(&mut volume, &mut interner, &mut dirty, [5, 6, 7], 9),
            Ok(false)
        );
        assert_eq!(dirty.peek_edits(), edits, "无变化不得扩大脏范围");

        // 清空这一格后读回空气（VoxTree 的默认值）。
        assert_eq!(
            set_voxel(&mut volume, &mut interner, &mut dirty, [5, 6, 7], 0),
            Ok(true)
        );
        assert_eq!(
            volume
                .get_chunk(&key)
                .unwrap()
                .get(interner.inner(), IVec3::new(5, 6, 7)),
            None
        );
    }

    #[test]
    fn set_voxel_missing_chunk_returns_key_and_does_not_mark_dirty() {
        let mut volume = VoxVolume::new(FixedPoint::from_num(1));
        let mut interner = VoxelInterner::new(1024 * 1024);
        let mut dirty = VoxelDirtySet::new();

        let missing = ChunkKey::new(3, 0, 0);
        assert_eq!(
            set_voxel(&mut volume, &mut interner, &mut dirty, [3 * 32, 0, 0], 1),
            Err(missing)
        );
        assert!(dirty.is_empty(), "缺失 chunk 不得产生脏标记");
    }

    /// 单点写入走 VoxTree 的结构共享路径：反复写同值 / 写回原值不得让
    /// interner 的存活节点数持续增长（去重不能被打坏）。
    #[test]
    fn repeated_set_voxel_does_not_leak_interner_nodes() {
        let (mut volume, _) = chunk_with_volume();
        let mut interner = VoxelInterner::new(1024 * 1024);
        let mut dirty = VoxelDirtySet::new();

        assert_eq!(
            set_voxel(&mut volume, &mut interner, &mut dirty, [5, 6, 7], 9),
            Ok(true)
        );
        let baseline = interner.inner().alive_nodes();

        for _ in 0..64 {
            assert_eq!(
                set_voxel(&mut volume, &mut interner, &mut dirty, [5, 6, 7], 9),
                Ok(false)
            );
        }
        assert_eq!(
            interner.inner().alive_nodes(),
            baseline,
            "同值写入不得分配新节点"
        );

        for step in 0..64 {
            let value = if step % 2 == 0 { 0 } else { 9 };
            let _ = set_voxel(&mut volume, &mut interner, &mut dirty, [5, 6, 7], value);
        }
        assert_eq!(
            interner.inner().alive_nodes(),
            baseline,
            "写空再写回必须复用同一批去重节点"
        );
    }
}
