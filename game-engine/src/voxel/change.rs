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
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VoxelSet {
    Apply,
    Mesh,
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
    let c = chunk_voxels_per_axis();
    for (mut volume, mut buffer) in &mut query {
        if buffer.is_empty() {
            continue;
        }
        for edit in buffer.drain() {
            let key = ChunkKey::new(
                edit.voxel[0].div_euclid(c),
                edit.voxel[1].div_euclid(c),
                edit.voxel[2].div_euclid(c),
            );
            let local = [
                edit.voxel[0].rem_euclid(c) as u8,
                edit.voxel[1].rem_euclid(c) as u8,
                edit.voxel[2].rem_euclid(c) as u8,
            ];
            let Some(tree) = volume.get_chunk_mut(&key) else {
                warn!(
                    "voxel edit target chunk missing: key={key:?} voxel={:?}",
                    edit.voxel
                );
                debug_assert!(false, "voxel edit target chunk missing: key={key:?}");
                continue;
            };
            let local_ivec = IVec3::new(local[0] as i32, local[1] as i32, local[2] as i32);
            let _ = tree.set(interner.inner_mut(), local_ivec, edit.value);
            dirty.mark_edited(key, VoxelBox::of_voxel(local));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::FixedPoint;
    use ::voxel::store::{MaxDepth, VoxOpsState, VoxTree, CHUNK_DEPTH};

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

        let volume = app.world().entity(entity).get::<VoxVolume>().unwrap();
        let tree = volume.get_chunk(&key).unwrap();
        assert!(!tree.is_empty(), "被编辑的体素必须让子块非空");
    }
}
