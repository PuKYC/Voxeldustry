//! 子块级脏标记：记录「哪个 body 的哪些体素变了」的纯世界状态。
//!
//! 本模块是纯世界数据：`VoxelBox`（块内整数体素范围）+ `DirtyChunk`（body +
//! 块坐标 + 编辑范围）+ `VoxelDirtySet`（按 `(body, ChunkKey)` 升序的脏集合），
//! 由世界层的 `apply_voxel_changes` 写入。把脏集变成 mesh 重建计划的
//! `rebuild_plan` 属于表现层网格规划（见 `crate::presentation::voxel::plan`），
//! 世界层不反向依赖它。
//!
//! 确定性：所有会决定顺序的容器都是 BTreeMap / 排序 Vec。

use std::collections::BTreeMap;

use bevy::prelude::*;
use voxel::store::ChunkKey;

use crate::body_key::BodyKey;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirtyChunk {
    /// 所属 body；`None` 表示无 body 归属（旧调用 / 临时实体）。
    pub body: Option<BodyKey>,
    /// 子块坐标。
    pub key: ChunkKey,
    /// 块内被改动的体素范围（半开）。
    pub edited: VoxelBox,
}

/// `(body, ChunkKey)` 级脏集合，同时记录每个脏块的编辑体素范围。
///
/// dirty 用 BTreeMap：键是 `(Option<BodyKey>, ChunkKey)`，`None`（无 body 归属）
/// 自然排在 `Some` 之前；同一 body 内按 ChunkKey 升序。因此 take_edits() 输出
/// 先 body、后 key 升序（L3）。
#[derive(Resource, Default)]
pub struct VoxelDirtySet {
    dirty: BTreeMap<(Option<BodyKey>, ChunkKey), VoxelBox>,
}

impl VoxelDirtySet {
    /// 空集合。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 标记 `body` 的 `key` 内 `edited` 范围被改动；同一 `(body, key)`
    /// 重复标记取保守并集。
    ///
    /// `body` 为 `None` 表示无 body 归属（旧调用 / 临时实体），与任何真实
    /// body 的同一 ChunkKey 互不影响。
    pub fn mark_edited_for(&mut self, body: Option<BodyKey>, key: ChunkKey, edited: VoxelBox) {
        self.dirty
            .entry((body, key))
            .and_modify(|b| *b = b.union(edited))
            .or_insert(edited);
    }

    /// 旧语义入口：body 为 `None` 的部分范围标脏。
    pub fn mark_edited(&mut self, key: ChunkKey, edited: VoxelBox) {
        self.mark_edited_for(None, key, edited);
    }

    /// 旧语义：body 为 `None` 的整块脏。
    pub fn mark_dirty(&mut self, key: ChunkKey) {
        self.mark_edited(key, VoxelBox::FULL);
    }

    /// 批量整块标记（body 为 `None`）。
    pub fn mark_many<I: IntoIterator<Item = ChunkKey>>(&mut self, keys: I) {
        for key in keys {
            self.mark_dirty(key);
        }
    }

    /// key 是否在任意 body 上脏（保持旧语义）。
    #[must_use]
    pub fn is_dirty(&self, key: &ChunkKey) -> bool {
        self.dirty.keys().any(|(_, k)| k == key)
    }

    /// `body` 的 `key` 是否脏（精确到 body）。
    #[must_use]
    pub fn is_dirty_for(&self, body: Option<BodyKey>, key: &ChunkKey) -> bool {
        self.dirty.contains_key(&(body, *key))
    }

    /// 脏条目数，即 `(body, key)` 组合数（同一 key 在不同 body 上各算一条）。
    ///
    /// 语义变化：旧版数的是不同 ChunkKey 的数量；现在同一 ChunkKey 在不同
    /// body 上会分别计数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.dirty.len()
    }

    /// 是否无脏条目。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dirty.is_empty()
    }

    /// 清空。
    pub fn clear(&mut self) {
        self.dirty.clear();
    }

    /// 取出全部编辑（`None` body 在前，其后按 body、再按 ChunkKey 升序）并清空。
    #[must_use]
    pub fn take_edits(&mut self) -> Vec<DirtyChunk> {
        std::mem::take(&mut self.dirty)
            .into_iter()
            .map(|((body, key), edited)| DirtyChunk { body, key, edited })
            .collect()
    }

    /// 只读快照（`None` body 在前，其后按 body、再按 ChunkKey 升序），不清空。
    #[must_use]
    pub fn peek_edits(&self) -> Vec<DirtyChunk> {
        self.dirty
            .iter()
            .map(|(&(body, key), &edited)| DirtyChunk { body, key, edited })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::StableEntityId;
    use crate::voxel::chunk_key;

    fn body(id: u64) -> Option<BodyKey> {
        Some(BodyKey::new(StableEntityId(id)))
    }

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

    /// 同 key（同 body）重复标记取并集；take_edits / peek_edits 按 ChunkKey 升序。
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

    /// 两个 body 上的同一 ChunkKey 必须各自成条，且按 body 升序。
    #[test]
    fn same_key_on_different_bodies_stays_separate() {
        let key = chunk_key(4, 0, 0);
        let mut dirty = VoxelDirtySet::new();
        dirty.mark_edited_for(body(2), key, VoxelBox::of_voxel([1, 1, 1]));
        dirty.mark_edited_for(body(1), key, VoxelBox::of_voxel([2, 2, 2]));

        assert_eq!(dirty.len(), 2, "同 key 不同 body 是两条");
        let edits = dirty.take_edits();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].body, body(1));
        assert_eq!(edits[1].body, body(2));
        assert!(edits.iter().all(|e| e.key == key));
        assert_eq!(edits[0].edited, VoxelBox::of_voxel([2, 2, 2]));
        assert_eq!(edits[1].edited, VoxelBox::of_voxel([1, 1, 1]));
    }

    /// 同一 `(body, key)` 两次标记仍合并成一条，范围取保守并集。
    #[test]
    fn same_body_same_key_unions() {
        let key = chunk_key(0, 3, 0);
        let mut dirty = VoxelDirtySet::new();
        dirty.mark_edited_for(body(7), key, VoxelBox::of_voxel([0, 0, 0]));
        dirty.mark_edited_for(body(7), key, VoxelBox::of_voxel([7, 7, 7]));

        assert_eq!(dirty.len(), 1);
        let edits = dirty.take_edits();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].body, body(7));
        assert_eq!(edits[0].key, key);
        assert_eq!(
            edits[0].edited,
            VoxelBox::of_voxel([0, 0, 0]).union(VoxelBox::of_voxel([7, 7, 7]))
        );
        assert_eq!(edits[0].edited.min, [0, 0, 0]);
        assert_eq!(edits[0].edited.max, [8, 8, 8]);
    }

    /// 旧 API 的脏条目 body 恒为 None，is_dirty 仍为 true。
    #[test]
    fn legacy_mark_edited_is_bodyless() {
        let k1 = chunk_key(1, 0, 0);
        let k2 = chunk_key(2, 0, 0);
        let k3 = chunk_key(3, 0, 0);
        let mut dirty = VoxelDirtySet::new();
        dirty.mark_edited(k1, VoxelBox::of_voxel([1, 2, 3]));
        dirty.mark_dirty(k2);
        dirty.mark_many([k3]);

        assert_eq!(dirty.len(), 3);
        for key in [k1, k2, k3] {
            assert!(dirty.is_dirty(&key), "旧 API 标记后 is_dirty 必须为 true");
            assert!(dirty.is_dirty_for(None, &key));
        }
        let edits = dirty.take_edits();
        assert_eq!(edits.len(), 3);
        assert!(
            edits.iter().all(|e| e.body.is_none()),
            "旧 API 不得凭空造出 body"
        );
    }

    /// take_edits 的顺序：None 桶整体在前，随后 Some(body) 升序，同 body 内 key 升序。
    #[test]
    fn take_edits_orders_body_then_key() {
        let mut dirty = VoxelDirtySet::new();
        dirty.mark_edited_for(body(2), chunk_key(1, 0, 0), VoxelBox::FULL);
        dirty.mark_edited_for(body(1), chunk_key(2, 0, 0), VoxelBox::FULL);
        dirty.mark_edited_for(body(1), chunk_key(-2, 0, 0), VoxelBox::FULL);
        dirty.mark_dirty(chunk_key(5, 0, 0));
        dirty.mark_dirty(chunk_key(-5, 0, 0));

        let order: Vec<(Option<BodyKey>, ChunkKey)> =
            dirty.peek_edits().iter().map(|e| (e.body, e.key)).collect();
        assert_eq!(
            order,
            vec![
                (None, chunk_key(-5, 0, 0)),
                (None, chunk_key(5, 0, 0)),
                (body(1), chunk_key(-2, 0, 0)),
                (body(1), chunk_key(2, 0, 0)),
                (body(2), chunk_key(1, 0, 0)),
            ]
        );
        // take_edits 与 peek_edits 同序。
        let taken: Vec<(Option<BodyKey>, ChunkKey)> =
            dirty.take_edits().iter().map(|e| (e.body, e.key)).collect();
        assert_eq!(taken, order);
    }

    /// is_dirty_for 精确区分 body；is_dirty 仍是任意 body。
    #[test]
    fn is_dirty_for_filters_by_body() {
        let key = chunk_key(0, 0, 0);
        let mut dirty = VoxelDirtySet::new();
        dirty.mark_edited_for(body(1), key, VoxelBox::FULL);

        assert!(dirty.is_dirty(&key));
        assert!(dirty.is_dirty_for(body(1), &key));
        assert!(!dirty.is_dirty_for(body(2), &key));
        assert!(!dirty.is_dirty_for(None, &key));

        dirty.mark_edited_for(None, chunk_key(1, 0, 0), VoxelBox::FULL);
        assert!(!dirty.is_dirty_for(Some(BodyKey::INVALID), &chunk_key(1, 0, 0)));
        assert!(dirty.is_dirty_for(None, &chunk_key(1, 0, 0)));
    }
}
