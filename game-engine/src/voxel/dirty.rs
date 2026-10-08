//! 子块级脏标记：记录「哪些体素变了」的纯世界状态。
//!
//! 本模块是纯世界数据：`VoxelBox`（块内整数体素范围）+ `DirtyChunk`（块坐标 +
//! 编辑范围）+ `VoxelDirtySet`（按 ChunkKey 升序的脏集合），由世界层的
//! `apply_voxel_changes` 写入。把脏集变成 mesh 重建计划的 `rebuild_plan` 属于
//! 表现层网格规划（见 `crate::presentation::voxel::plan`），世界层不反向依赖它。
//!
//! 确定性：所有会决定顺序的容器都是 BTreeMap / 排序 Vec。

use std::collections::BTreeMap;

use bevy::prelude::*;
use voxel::store::ChunkKey;

/// chunk 内的整数体素范围；半开区间，元素 ∈ 0..=32。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VoxelBox {
    /// 每轴下界（含）。
    pub min: [u8; 3],
    /// 每轴上界（不含）。
    pub max: [u8; 3],
}

impl VoxelBox {
    /// 整个子块 [0,32)^3。
    pub const FULL: VoxelBox = VoxelBox {
        min: [0; 3],
        max: [32; 3],
    };

    /// 单个体素 [p, p+1)。
    #[must_use]
    pub fn of_voxel(p: [u8; 3]) -> Self {
        VoxelBox {
            min: p,
            max: [p[0] + 1, p[1] + 1, p[2] + 1],
        }
    }

    /// 保守并集（可能高估，安全）。
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        let mut out = self;
        for i in 0..3 {
            out.min[i] = out.min[i].min(other.min[i]);
            out.max[i] = out.max[i].max(other.max[i]);
        }
        out
    }
}

/// 一个被编辑的子块及其块内体素范围。
#[derive(Clone, Copy, Debug)]
pub struct DirtyChunk {
    /// 子块坐标。
    pub key: ChunkKey,
    /// 块内被改动的体素范围（半开）。
    pub edited: VoxelBox,
}

/// ChunkKey 级脏集合，同时记录每个脏块的编辑体素范围。
///
/// dirty 用 BTreeMap：take_edits() 输出按 ChunkKey 升序（L3）。
#[derive(Resource, Default)]
pub struct VoxelDirtySet {
    dirty: BTreeMap<ChunkKey, VoxelBox>,
}

impl VoxelDirtySet {
    /// 空集合。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 标记 key 内 edited 范围被改动；同 key 重复标记取保守并集。
    pub fn mark_edited(&mut self, key: ChunkKey, edited: VoxelBox) {
        self.dirty
            .entry(key)
            .and_modify(|b| *b = b.union(edited))
            .or_insert(edited);
    }

    /// 旧语义：整块脏。
    pub fn mark_dirty(&mut self, key: ChunkKey) {
        self.mark_edited(key, VoxelBox::FULL);
    }

    /// 批量整块标记。
    pub fn mark_many<I: IntoIterator<Item = ChunkKey>>(&mut self, keys: I) {
        for key in keys {
            self.mark_dirty(key);
        }
    }

    /// key 是否脏。
    #[must_use]
    pub fn is_dirty(&self, key: &ChunkKey) -> bool {
        self.dirty.contains_key(key)
    }

    /// 脏块数量。
    #[must_use]
    pub fn len(&self) -> usize {
        self.dirty.len()
    }

    /// 是否无脏块。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dirty.is_empty()
    }

    /// 清空。
    pub fn clear(&mut self) {
        self.dirty.clear();
    }

    /// 取出全部编辑（ChunkKey 升序）并清空。
    #[must_use]
    pub fn take_edits(&mut self) -> Vec<DirtyChunk> {
        std::mem::take(&mut self.dirty)
            .into_iter()
            .map(|(key, edited)| DirtyChunk { key, edited })
            .collect()
    }

    /// 只读快照（ChunkKey 升序），不清空。
    #[must_use]
    pub fn peek_edits(&self) -> Vec<DirtyChunk> {
        self.dirty
            .iter()
            .map(|(&key, &edited)| DirtyChunk { key, edited })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voxel::chunk_key;

    #[test]
    fn voxel_box_helpers() {
        assert_eq!(VoxelBox::of_voxel([1, 2, 3]).min, [1, 2, 3]);
        assert_eq!(VoxelBox::of_voxel([1, 2, 3]).max, [2, 3, 4]);
        let a = VoxelBox {
            min: [4, 4, 4],
            max: [8, 8, 8],
        };
        let b = VoxelBox {
            min: [0, 6, 7],
            max: [5, 9, 12],
        };
        let u = a.union(b);
        assert_eq!(u.min, [0, 4, 4]);
        assert_eq!(u.max, [8, 9, 12]);
        assert_eq!(
            VoxelBox::FULL,
            VoxelBox {
                min: [0; 3],
                max: [32; 3]
            }
        );
    }

    /// 同 key 重复标记取并集；take_edits / peek_edits 按 ChunkKey 升序。
    #[test]
    fn dirty_merges_and_is_sorted() {
        let mut dirty = VoxelDirtySet::new();
        dirty.mark_edited(
            chunk_key(1, 0, 0),
            VoxelBox {
                min: [0, 0, 0],
                max: [4, 4, 4],
            },
        );
        dirty.mark_dirty(chunk_key(1, 0, 0));
        dirty.mark_dirty(chunk_key(-2, 0, 0));
        assert_eq!(dirty.len(), 2);
        assert!(dirty.is_dirty(&chunk_key(1, 0, 0)));

        let peek = dirty.peek_edits();
        assert_eq!(peek[0].key, chunk_key(-2, 0, 0));
        assert_eq!(peek[1].key, chunk_key(1, 0, 0));
        assert_eq!(
            peek[1].edited,
            VoxelBox::FULL,
            "mark_dirty must union to FULL"
        );

        let edits = dirty.take_edits();
        assert!(dirty.is_empty());
        let keys: Vec<ChunkKey> = edits.iter().map(|e| e.key).collect();
        assert_eq!(keys, vec![chunk_key(-2, 0, 0), chunk_key(1, 0, 0)]);
    }
}
