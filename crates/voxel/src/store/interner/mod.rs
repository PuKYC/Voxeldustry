//! 共享节点池 `VoxInterner`（v1 重写）。
//!
//! ## v1 节点布局（SoA pool 列）
//!
//! | 列 | 类型 | 字节 |
//! |---|---|---|
//! | `child_mask` | u8 | 1 |
//! | `children` | [u32; 8]（子节点下标列） | 32 |
//! | `ref_counts` | u32 | 4 |
//! | `generations` | u16（bit15 = 叶标记，0..14 = generation） | 2 |
//! | `values` | T（默认 u8） | 1 |
//! | `hashes` | u32 结构哈希 | 4 |
//!
//! 合计 **44 B**（`VoxInterner::<u8>::node_size()`）。这是 **pool 单列口径**；
//! 验收指标是 [`VoxInterner::estimated_total_bytes`]。
//!
//! ## 关键设计
//!
//! - **索引稳定**：增长只重分配 pool 内存，`BlockId` 与 patterns 无需重映射。
//! - **结构化哈希**：`hashes` 只依赖结构（子哈希 + child_mask 或叶值），与节点索引无关；
//! `content_hash` 因此可直接由引擎用于存档 / 脏比较。
//! - **碰撞 bucket**：紧凑 SoA 开放寻址表（`keys: Vec<u32>` + `values: Vec<u64>`，
//!   线性探测、2 的幂容量、装载因子 ≤ 0.75），`Bucket = Inline | Spill`；查表返回
//! 候选后必须按 `child_mask` + children（分支）或 value（叶）**结构校验**。
//! - **从不迭代 patterns 产生字节 / 哈希**；表内部的重排（rehash / spill 压缩）
//!   只由操作序列确定，不改变对外查询结果。
//!
//! 硬边界：只出现整数；不依赖 Bevy；不认识 voxel_size。

use crate::store::{
    memory::PoolAllocatorLite, BlockId, VoxelTrait, EMPTY_CHILD, MAX_ALLOWED_DEPTH, MAX_CHILDREN,
};

mod hash;
mod macros;
#[cfg(feature = "memory_stats")]
mod stats;

pub use hash::{
    compute_branch_hash_from_child_hashes, compute_empty_branch_hash, compute_leaf_hash_for_value,
};
use macros::get_next_index_macro;
#[cfg(feature = "memory_stats")]
pub use stats::InternerStats;

/// 供公开 API 使用的 children 数组（拥有所有权）。
pub type Children = [BlockId; MAX_CHILDREN];

/// 空 children 的索引列（children 列中 0 = 无子节点 / 空分支哨兵）。
const EMPTY_CHILD_INDICES: [u32; MAX_CHILDREN] = [0u32; MAX_CHILDREN];

/// generation 列中 generation 使用的位（低 15 位）。
const GENERATION_MASK: u16 = BlockId::MAX_GENERATION;
/// generation 列中“该节点是叶”的标记位。
const LEAF_FLAG: u16 = 1 << 15;

/// 结构等价候选桶。
///
/// 只按 hash 查表，命中后必须调用方**结构校验**，因为 u32 哈希碰撞是预期内的。
#[derive(Debug, Clone)]
pub enum Bucket {
    /// 只有一个候选。
    Inline(BlockId),
    /// 同一 hash 下有多个候选；删除时精确移除一个，剩 1 个时收回 Inline。
    Spill(Vec<BlockId>),
}

impl Bucket {
    /// 候选个数。
    #[must_use]
    #[inline]
    pub fn len(&self) -> usize {
        match self {
            Bucket::Inline(_) => 1,
            Bucket::Spill(v) => v.len(),
        }
    }

    /// 该桶是否为空（按构造恒为 false）。
    #[must_use]
    #[inline]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// 遍历候选（绝不用于产生字节 / 哈希）。
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = &BlockId> {
        match self {
            Bucket::Inline(id) => std::slice::from_ref(id).iter(),
            Bucket::Spill(v) => v.iter(),
        }
    }
}

/// 空槽哨兵。`BlockId::INVALID` 的 raw 也是 `u64::MAX`，但它从不会作为候选存储：
/// 内联候选的最高位是 spill 标记位（分支恒为 0，叶清除后为 0），spill 值 < 2^63。
const PATTERN_EMPTY: u64 = u64::MAX;
/// spill 标记位 = value 的最高位（第 63 位）。分支 raw 恒为 0；叶 raw 恒为 1，
/// 故内联叶值存储时清掉此位、读取时按表类型补回，此位专用于区分 inline / spill。
const PATTERN_SPILL_TAG: u64 = 1 << 63;
/// spill 索引用 value 的低 32 位。
const PATTERN_SPILL_INDEX_MASK: u64 = u32::MAX as u64;
/// spill 长度用 value 的第 32..62 位（31 位，足够容纳任何现实碰撞桶）。
const PATTERN_SPILL_LEN_SHIFT: u32 = 32;
const PATTERN_SPILL_LEN_MASK: u64 = 0x7FFF_FFFF;

/// 把 spill 段（起始下标 + 候选数）编码进 value 的高位。
#[inline(always)]
fn pattern_encode_spill(start: usize, len: usize) -> u64 {
    debug_assert!(start as u64 <= PATTERN_SPILL_INDEX_MASK);
    debug_assert!(len as u64 <= PATTERN_SPILL_LEN_MASK);
    PATTERN_SPILL_TAG | ((len as u64) << PATTERN_SPILL_LEN_SHIFT) | (start as u64)
}

/// hash → 候选桶的紧凑、确定性开放寻址表。
///
/// - **SoA**：`keys: Vec<u32>`（结构哈希）+ `values: Vec<u64>`（内联 `BlockId` 或
///   spill 索引）；多候选的溢出候选放在侧表 `spill: Vec<BlockId>`。
/// - **线性探测**、**2 的幂容量**、**装载因子 ≤ 0.75**；删除用后移（backward-shift），
///   不引入墓碑，故 `estimated_total_bytes` 只计算真实槽位。
/// - value 编码：内联 = `BlockId::raw()`（叶清第 63 位）；spill = 标记位 |
///   `(len << 32) | start`，指向 `spill` 中一段连续候选。
/// - **绝不按迭代序产生字节 / 哈希**；内部 rehash 只按槽位顺序重建同一集合。
pub struct PatternsHashmap {
    keys: Vec<u32>,
    values: Vec<u64>,
    spill: Vec<BlockId>,
    is_leaf: bool,
    len: usize,
}

impl PatternsHashmap {
    /// 新建空表；`is_leaf` 决定内联 value 读回时是否补上叶标记位（每张表类型固定）。
    #[must_use]
    pub fn new(is_leaf: bool) -> Self {
        Self {
            keys: Vec::new(),
            values: Vec::new(),
            spill: Vec::new(),
            is_leaf,
            len: 0,
        }
    }

    /// 表中的桶（distinct hash）数量。
    #[must_use]
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// 表是否为空。
    #[must_use]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 当前槽位容量（2 的幂；未分配时为 0）。
    #[must_use]
    #[inline]
    pub fn capacity(&self) -> usize {
        self.keys.len()
    }

    /// spill 侧表已分配槽位（内存统计用）。
    #[must_use]
    #[inline]
    pub fn spill_capacity(&self) -> usize {
        self.spill.capacity()
    }

    #[inline(always)]
    fn decode_inline(&self, value: u64) -> BlockId {
        BlockId::from_raw(value | ((self.is_leaf as u64) << 63))
    }

    #[inline(always)]
    fn encode_inline(&self, id: BlockId) -> u64 {
        id.raw() & !PATTERN_SPILL_TAG
    }

    /// 定位 `key` 的槽位；表空或未命中返回 `None`。
    fn find(&self, key: u32) -> Option<usize> {
        let capacity = self.keys.len();
        if capacity == 0 {
            return None;
        }
        let mask = capacity - 1;
        let mut slot = (key as usize) & mask;
        loop {
            let value = self.values[slot];
            if value == PATTERN_EMPTY {
                return None;
            }
            if self.keys[slot] == key {
                return Some(slot);
            }
            slot = (slot + 1) & mask;
        }
    }

    /// 在空槽写入 `(key, value)`；调用方保证表内不存在 `key` 且有空槽。
    fn place(&mut self, key: u32, value: u64) -> usize {
        let mask = self.keys.len() - 1;
        let mut slot = (key as usize) & mask;
        while self.values[slot] != PATTERN_EMPTY {
            slot = (slot + 1) & mask;
        }
        self.keys[slot] = key;
        self.values[slot] = value;
        slot
    }

    /// 重建到 `new_capacity` 个槽位（2 的幂）；保持同一集合。
    fn resize(&mut self, new_capacity: usize) {
        debug_assert!(new_capacity.is_power_of_two());
        debug_assert!(new_capacity >= 2);
        let old_keys = std::mem::replace(&mut self.keys, vec![0u32; new_capacity]);
        let old_values = std::mem::replace(&mut self.values, vec![PATTERN_EMPTY; new_capacity]);
        for i in 0..old_values.len() {
            let value = old_values[i];
            if value != PATTERN_EMPTY {
                self.place(old_keys[i], value);
            }
        }
    }

    /// 查询 `key` 的候选桶（拥有所有权）；命中 / 未命中语义与旧 map 一致。
    #[must_use]
    pub fn get(&self, key: &u32) -> Option<Bucket> {
        let slot = self.find(*key)?;
        let value = self.values[slot];
        if value & PATTERN_SPILL_TAG != 0 {
            let start = (value & PATTERN_SPILL_INDEX_MASK) as usize;
            let len = ((value >> PATTERN_SPILL_LEN_SHIFT) & PATTERN_SPILL_LEN_MASK) as usize;
            Some(Bucket::Spill(self.spill[start..start + len].to_vec()))
        } else {
            Some(Bucket::Inline(self.decode_inline(value)))
        }
    }

    /// 为 `key` 追加一个候选 `id`（等价旧的 insert + Entry 合并语义）。
    pub fn insert(&mut self, key: u32, id: BlockId) {
        if self.keys.is_empty() {
            self.resize(2);
        }

        if let Some(slot) = self.find(key) {
            let value = self.values[slot];
            if value & PATTERN_SPILL_TAG != 0 {
                let start = (value & PATTERN_SPILL_INDEX_MASK) as usize;
                let len = ((value >> PATTERN_SPILL_LEN_SHIFT) & PATTERN_SPILL_LEN_MASK) as usize;
                let new_start = self.spill.len();
                self.spill.extend_from_within(start..start + len);
                self.spill.push(id);
                self.values[slot] = pattern_encode_spill(new_start, len + 1);
            } else {
                let existing = self.decode_inline(value);
                let new_start = self.spill.len();
                self.spill.push(existing);
                self.spill.push(id);
                self.values[slot] = pattern_encode_spill(new_start, 2);
            }
            return;
        }

        if (self.len + 1) * 4 > self.keys.len() * 3 {
            self.resize(self.keys.len() * 2);
        }
        let value = self.encode_inline(id);
        self.place(key, value);
        self.len += 1;
    }

    /// 从 `key` 的桶中精确移除一个 `id`；Spill 只剩 1 个时收回 Inline。
    ///
    /// 返回是否找到并移除；`key` 不存在时 debug 构建下触发断言。
    pub fn remove(&mut self, key: u32, id: BlockId) -> bool {
        let Some(slot) = self.find(key) else {
            debug_assert!(false, "pattern hash missing: {key:X} id {id:?}");
            return false;
        };

        let value = self.values[slot];
        if value & PATTERN_SPILL_TAG == 0 {
            let existing = self.decode_inline(value);
            debug_assert_eq!(existing, id, "pattern bucket mismatch");
            self.backward_shift_delete(slot);
            self.len -= 1;
            return true;
        }

        let start = (value & PATTERN_SPILL_INDEX_MASK) as usize;
        let len = ((value >> PATTERN_SPILL_LEN_SHIFT) & PATTERN_SPILL_LEN_MASK) as usize;
        let position = self.spill[start..start + len]
            .iter()
            .position(|candidate| *candidate == id)
            .expect("pattern id missing from Spill bucket");

        if len == 2 {
            let other = self.spill[start + (1 - position)];
            self.values[slot] = self.encode_inline(other);
        } else {
            let new_start = self.spill.len();
            for i in 0..len {
                if i != position {
                    let candidate = self.spill[start + i];
                    self.spill.push(candidate);
                }
            }
            self.values[slot] = pattern_encode_spill(new_start, len - 1);
        }
        self.maybe_compact_spill();
        true
    }

    /// 线性探测删除：把探测路径上受空槽影响的元素逐个后移，最后清空终点槽。
    ///
    /// 条件 `(hole - home) mod cap <= (slot - home) mod cap` 等价于
    /// “hole 位于 home→slot 的探测路径上”，此时后移不会破坏查找。
    fn backward_shift_delete(&mut self, mut hole: usize) {
        let mask = self.keys.len() - 1;
        let mut slot = (hole + 1) & mask;
        loop {
            let value = self.values[slot];
            if value == PATTERN_EMPTY {
                break;
            }
            let home = (self.keys[slot] as usize) & mask;
            if (hole.wrapping_sub(home)) & mask <= (slot.wrapping_sub(home)) & mask {
                self.keys[hole] = self.keys[slot];
                self.values[hole] = value;
                hole = slot;
            }
            slot = (slot + 1) & mask;
        }
        self.keys[hole] = 0;
        self.values[hole] = PATTERN_EMPTY;
    }

    /// 活跃 spill 候选总数。
    fn live_spill_len(&self) -> usize {
        let mut total = 0usize;
        for &value in &self.values {
            if value != PATTERN_EMPTY && value & PATTERN_SPILL_TAG != 0 {
                total += ((value >> PATTERN_SPILL_LEN_SHIFT) & PATTERN_SPILL_LEN_MASK) as usize;
            }
        }
        total
    }

    /// 追加式 spill 会留下旧区段垃圾；超过阈值时按槽位序压缩回紧凑布局。
    fn maybe_compact_spill(&mut self) {
        let live = self.live_spill_len();
        if live == 0 {
            if !self.spill.is_empty() {
                self.spill.clear();
            }
            return;
        }
        if self.spill.len() > live * 2 + 16 {
            let mut compacted: Vec<BlockId> = Vec::with_capacity(live);
            for slot in 0..self.values.len() {
                let value = self.values[slot];
                if value != PATTERN_EMPTY && value & PATTERN_SPILL_TAG != 0 {
                    let start = (value & PATTERN_SPILL_INDEX_MASK) as usize;
                    let len =
                        ((value >> PATTERN_SPILL_LEN_SHIFT) & PATTERN_SPILL_LEN_MASK) as usize;
                    let new_start = compacted.len();
                    compacted.extend_from_slice(&self.spill[start..start + len]);
                    self.values[slot] = pattern_encode_spill(new_start, len);
                }
            }
            self.spill = compacted;
        }
    }
}

#[inline(always)]
fn mask_from_children(children: &Children) -> u8 {
    let mut mask = 0u8;
    for (i, child) in children.iter().enumerate() {
        if !child.is_empty() {
            mask |= 1 << i;
        }
    }
    mask
}

#[inline(always)]
fn indices_from_children(children: &Children) -> [u32; MAX_CHILDREN] {
    let mut indices = EMPTY_CHILD_INDICES;
    for (i, child) in children.iter().enumerate() {
        if !child.is_empty() {
            indices[i] = child.index();
        }
    }
    indices
}

/// 共享节点池。所有节点按 u32 下标寻址；下标在增长时保持不变。
pub struct VoxInterner<T> {
    child_mask: PoolAllocatorLite<u8>,
    children: PoolAllocatorLite<[u32; MAX_CHILDREN]>,
    ref_counts: PoolAllocatorLite<u32>,
    generations: PoolAllocatorLite<u16>,
    values: PoolAllocatorLite<T>,
    hashes: PoolAllocatorLite<u32>,

    patterns: [PatternsHashmap; 2],
    free_indices: Vec<u32>,
    next_index: u32,
    capacity: usize,
    max_capacity: usize,

    empty_branch_id: BlockId,
    empty_branch_hash: u32,
    dec_ref_rec_stack: Vec<BlockId>,

    branch_hits: usize,
    branch_misses: usize,
    leaf_hits: usize,
    leaf_misses: usize,

    #[cfg(test)]
    forced_leaf_hash: Option<u32>,
    #[cfg(test)]
    forced_branch_hash: Option<u32>,

    #[cfg(feature = "memory_stats")]
    stats: InternerStats,
}

impl<T: VoxelTrait> VoxInterner<T> {
    /// pool 单列口径的每节点字节数（护栏断言 `<= 44` 用）。
    ///
    /// 这只是 **pool 列分摊**，不含 patterns / free list / 释放栈；
    /// 内存验收请用 [`VoxInterner::estimated_total_bytes`]。
    #[must_use]
    #[inline(always)]
    pub const fn node_size() -> usize {
        PoolAllocatorLite::<u8>::block_size()
            + PoolAllocatorLite::<[u32; MAX_CHILDREN]>::block_size()
            + PoolAllocatorLite::<u32>::block_size()
            + PoolAllocatorLite::<u16>::block_size()
            + PoolAllocatorLite::<T>::block_size()
            + PoolAllocatorLite::<u32>::block_size()
    }

    /// 用一个字节预算创建 interner；增长上限为 `u32::MAX - 1`。
    ///
    /// 与上游不同，容量耗尽时不再 panic，而是自动倍增。
    pub fn with_memory_budget(requested_budget: usize) -> Self {
        Self::with_memory_budget_and_capacity(requested_budget, u32::MAX as usize - 1)
    }

    /// 带显式 `max_capacity` 上界的构造器。
    pub fn with_memory_budget_and_capacity(requested_budget: usize, max_capacity: usize) -> Self {
        let single_node_size = Self::node_size();

        let mut nodes_capacity = requested_budget / single_node_size;
        assert!(nodes_capacity > 0, "Requested budget is too small");
        nodes_capacity = nodes_capacity.min(u32::MAX as usize - 1);

        let max_capacity = max_capacity.max(nodes_capacity).min(u32::MAX as usize - 1);

        let mut child_mask = PoolAllocatorLite::<u8>::new(nodes_capacity);
        let mut children = PoolAllocatorLite::<[u32; MAX_CHILDREN]>::new(nodes_capacity);
        let mut ref_counts = PoolAllocatorLite::<u32>::new(nodes_capacity);
        let mut generations = PoolAllocatorLite::<u16>::new(nodes_capacity);
        let mut values = PoolAllocatorLite::<T>::new(nodes_capacity);
        let mut hashes = PoolAllocatorLite::<u32>::new(nodes_capacity);

        let mut branch_patterns = PatternsHashmap::new(false);
        let leaf_patterns = PatternsHashmap::new(true);

        let empty_branch_hash = compute_empty_branch_hash();
        let empty_branch_id = BlockId::EMPTY;
        assert_eq!(empty_branch_id, BlockId::new_branch(0, 0, 0, 0));

        // index 0 = 空分支哨兵。
        *child_mask.get_mut(0) = 0;
        *children.get_mut(0) = EMPTY_CHILD_INDICES;
        *ref_counts.get_mut(0) = 0;
        *generations.get_mut(0) = 0; // 分支（叶标记 0），generation 0
        *values.get_mut(0) = T::default();
        *hashes.get_mut(0) = empty_branch_hash;
        branch_patterns.insert(empty_branch_hash, empty_branch_id);

        let next_index = 1;

        #[cfg(feature = "memory_stats")]
        let stats = InternerStats {
            requested_budget,
            actual_budget: nodes_capacity * single_node_size,
            node_size: single_node_size,
            nodes_capacity,
            total_allocations: 1,
            allocated_nodes: 1,
            alive_nodes: 1,
            patterns: 1,
            branch_nodes: 1,
            ..Default::default()
        };

        Self {
            child_mask,
            children,
            ref_counts,
            generations,
            values,
            hashes,
            patterns: [branch_patterns, leaf_patterns],
            free_indices: Vec::new(),
            next_index,
            capacity: nodes_capacity,
            max_capacity,
            empty_branch_id,
            empty_branch_hash,
            dec_ref_rec_stack: Vec::new(),
            branch_hits: 0,
            branch_misses: 0,
            leaf_hits: 0,
            leaf_misses: 0,
            #[cfg(test)]
            forced_leaf_hash: None,
            #[cfg(test)]
            forced_branch_hash: None,
            #[cfg(feature = "memory_stats")]
            stats,
        }
    }

    /// 当前节点池容量（节点数）。
    #[must_use]
    #[inline(always)]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// 增长容量上限。
    #[must_use]
    #[inline(always)]
    pub const fn max_capacity(&self) -> usize {
        self.max_capacity
    }

    /// 设置增长容量上限（不得低于当前容量）。
    pub fn set_max_capacity(&mut self, max_capacity: usize) {
        self.max_capacity = max_capacity.max(self.capacity).min(u32::MAX as usize - 1);
    }

    /// 当前存活节点数（含空分支哨兵）。
    #[must_use]
    #[inline]
    pub fn alive_nodes(&self) -> usize {
        self.next_index as usize - self.free_indices.len()
    }

    /// `(branch_hits, leaf_hits)`：结构命中计数。
    #[must_use]
    #[inline]
    pub fn cache_hits(&self) -> (usize, usize) {
        (self.branch_hits, self.leaf_hits)
    }

    /// `(branch_len, leaf_len)`：patterns 表中的条目数。
    #[must_use]
    #[inline]
    pub fn pattern_lengths(&self) -> (usize, usize) {
        (self.patterns[0].len(), self.patterns[1].len())
    }

    /// 分支 patterns 表（只读；**禁止**按迭代序产生字节）。
    #[must_use]
    #[inline]
    pub fn branch_patterns(&self) -> &PatternsHashmap {
        &self.patterns[0]
    }

    /// 叶 patterns 表（只读；**禁止**按迭代序产生字节）。
    #[must_use]
    #[inline]
    pub fn leaf_patterns(&self) -> &PatternsHashmap {
        &self.patterns[1]
    }

    /// patterns 是否只剩空分支哨兵。
    #[must_use]
    #[inline]
    pub fn patterns_empty(&self) -> bool {
        self.patterns[0].len() == 1 && self.patterns[1].is_empty()
    }

    /// 预分配至少 `additional` 个新索引；返回是否发生增长。
    pub fn reserve(&mut self, additional: usize) -> bool {
        let needed = self.next_index as usize + additional;
        if needed <= self.capacity {
            return false;
        }

        let mut new_capacity = self.capacity.max(1);
        while new_capacity < needed {
            match new_capacity.checked_mul(2) {
                Some(doubled) => new_capacity = doubled,
                None => {
                    new_capacity = usize::MAX;
                    break;
                }
            }
        }

        let new_capacity = new_capacity.min(self.max_capacity);
        if new_capacity <= self.capacity {
            return false;
        }
        self.grow_pools(new_capacity);
        true
    }

    /// 容量翻倍一次（受 `max_capacity` 限制）；返回是否发生增长。
    pub fn grow(&mut self) -> bool {
        if self.capacity >= self.max_capacity {
            return false;
        }
        let doubled = self.capacity.saturating_mul(2).min(self.max_capacity);
        let new_capacity = doubled.max(self.capacity + 1);
        if new_capacity <= self.capacity {
            return false;
        }
        self.grow_pools(new_capacity);
        true
    }

    fn grow_pools(&mut self, new_capacity: usize) {
        debug_assert!(new_capacity > self.capacity);
        self.child_mask.grow(new_capacity);
        self.children.grow(new_capacity);
        self.ref_counts.grow(new_capacity);
        self.generations.grow(new_capacity);
        self.values.grow(new_capacity);
        self.hashes.grow(new_capacity);
        self.capacity = new_capacity;
    }

    /// 取得下一个可用索引；耗尽时自动倍增。索引稳定，无需重映射。
    ///
    /// 仅在达到 `max_capacity` 时 panic（正常增长路径不 panic）。
    pub fn get_next_index(&mut self) -> u32 {
        if let Some(index) = self.free_indices.pop() {
            #[cfg(feature = "memory_stats")]
            {
                self.stats.alive_nodes += 1;
                self.stats.max_alive_nodes = self.stats.max_alive_nodes.max(self.stats.alive_nodes);
                self.stats.recycled_nodes += 1;
                self.stats.total_allocations += 1;
            }
            return index;
        }

        if (self.next_index as usize) < self.capacity {
            let index = self.next_index;
            self.next_index += 1;
            #[cfg(feature = "memory_stats")]
            {
                self.stats.alive_nodes += 1;
                self.stats.max_alive_nodes = self.stats.max_alive_nodes.max(self.stats.alive_nodes);
                self.stats.allocated_nodes += 1;
                self.stats.total_allocations += 1;
                self.stats.max_node_id = self.stats.max_node_id.max(index as usize);
            }
            return index;
        }

        // 池满：自动倍增。
        if self.capacity < self.max_capacity {
            let doubled = self.capacity.saturating_mul(2).max(1);
            let mut new_capacity = doubled.min(self.max_capacity);
            new_capacity = new_capacity.max((self.next_index as usize).saturating_add(1));
            if new_capacity > self.capacity {
                self.grow_pools(new_capacity);
            }
        }

        if (self.next_index as usize) < self.capacity {
            let index = self.next_index;
            self.next_index += 1;
            #[cfg(feature = "memory_stats")]
            {
                self.stats.alive_nodes += 1;
                self.stats.max_alive_nodes = self.stats.max_alive_nodes.max(self.stats.alive_nodes);
                self.stats.allocated_nodes += 1;
                self.stats.total_allocations += 1;
                self.stats.max_node_id = self.stats.max_node_id.max(index as usize);
            }
            return index;
        }

        panic!(
            "VoxInterner capacity exhausted: next_index {} reached max_capacity {}",
            self.next_index, self.max_capacity
        );
    }

    #[inline(always)]
    fn node_is_leaf_index(&self, index: u32) -> bool {
        (*self.generations.get(index) & LEAF_FLAG) != 0
    }

    /// 由下标重建 `BlockId`（读取该节点自己的 generation + 叶标记）。
    fn block_id_for(&self, index: u32) -> BlockId {
        let raw = *self.generations.get(index);
        let generation = raw & GENERATION_MASK;
        if raw & LEAF_FLAG != 0 {
            BlockId::new_leaf(index, generation)
        } else {
            let mask = *self.child_mask.get(index);
            let types = self.types_for_index(index, mask);
            BlockId::new_branch(index, generation, types, mask)
        }
    }

    fn types_for_index(&self, index: u32, mask: u8) -> u8 {
        let indices = self.children.get(index);
        let mut types = 0u8;
        let mut bits = mask;
        while bits != 0 {
            let i = bits.trailing_zeros() as usize;
            bits &= !(1 << i);
            let child_index = indices[i];
            if child_index != 0 && self.node_is_leaf_index(child_index) {
                types |= 1 << i;
            }
        }
        types
    }

    fn types_from_indices(&self, indices: &[u32; MAX_CHILDREN], mask: u8) -> u8 {
        let mut types = 0u8;
        let mut bits = mask;
        while bits != 0 {
            let i = bits.trailing_zeros() as usize;
            bits &= !(1 << i);
            let child_index = indices[i];
            if child_index != 0 && self.node_is_leaf_index(child_index) {
                types |= 1 << i;
            }
        }
        types
    }

    /// 读取节点的值列。
    #[must_use]
    #[inline(always)]
    pub fn get_value(&self, block_id: &BlockId) -> &T {
        debug_assert!(
            self.is_valid_block_id(block_id),
            "Invalid block id: {block_id:?}"
        );
        self.values.get(block_id.index())
    }

    /// 读取节点结构哈希（与节点索引无关）。
    #[must_use]
    #[inline(always)]
    pub fn get_hash(&self, block_id: &BlockId) -> u32 {
        *self.hashes.get(block_id.index())
    }

    /// 结构哈希 = 内容哈希；与 interner 节点索引无关。
    #[must_use]
    #[inline(always)]
    pub fn content_hash(&self, block_id: &BlockId) -> u32 {
        self.get_hash(block_id)
    }

    /// 取得全部 8 个 children（缺失为 `BlockId::EMPTY`）。
    #[must_use]
    #[inline]
    pub fn get_children(&self, block_id: &BlockId) -> Children {
        debug_assert!(block_id.is_branch(), "Cannot get children for value node");
        debug_assert!(
            self.is_valid_block_id(block_id),
            "Invalid block id: {block_id:?}"
        );

        let index = block_id.index();
        let mask = *self.child_mask.get(index);
        if mask == 0 {
            return EMPTY_CHILD;
        }

        let indices = self.children.get(index);
        let mut out = EMPTY_CHILD;
        let mut bits = mask;
        while bits != 0 {
            let i = bits.trailing_zeros() as usize;
            bits &= !(1 << i);
            let child_index = indices[i];
            if child_index != 0 {
                out[i] = self.block_id_for(child_index);
            }
        }
        out
    }

    /// 与 [`VoxInterner::get_children`] 相同；保留上游方法名，返回拥有所有权的数组。
    #[must_use]
    #[inline]
    pub fn get_children_ref(&self, block_id: &BlockId) -> Children {
        self.get_children(block_id)
    }

    /// 取得指定下标的子节点；不存在返回 `BlockId::EMPTY`。
    #[must_use]
    #[inline]
    pub fn get_child_id(&self, block_id: &BlockId, index: usize) -> BlockId {
        debug_assert!(block_id.is_branch(), "Cannot get children for value node");
        debug_assert!(
            self.is_valid_block_id(block_id),
            "Invalid block id: {block_id:?}"
        );

        let node_index = block_id.index();
        let mask = *self.child_mask.get(node_index);
        if mask & (1 << index) == 0 {
            return BlockId::EMPTY;
        }
        let child_index = self.children.get(node_index)[index];
        if child_index == 0 {
            BlockId::EMPTY
        } else {
            self.block_id_for(child_index)
        }
    }

    /// 读取引用计数。
    #[must_use]
    #[inline(always)]
    pub fn get_ref(&self, block_id: &BlockId) -> u32 {
        debug_assert!(
            self.is_valid_block_id(block_id),
            "Invalid block id: {block_id:?}"
        );
        *self.ref_counts.get(block_id.index())
    }

    /// 引用计数 +1。
    #[inline(always)]
    pub fn inc_ref(&mut self, block_id: &BlockId) {
        debug_assert!(
            self.is_valid_block_id(block_id),
            "Invalid block id: {block_id:?}"
        );
        *self.ref_counts.get_mut(block_id.index()) += 1;
        #[cfg(feature = "memory_stats")]
        {
            if block_id.is_branch() {
                self.stats.max_branch_ref_count = self
                    .stats
                    .max_branch_ref_count
                    .max(*self.ref_counts.get(block_id.index()) as usize);
            } else {
                self.stats.max_leaf_ref_count = self
                    .stats
                    .max_leaf_ref_count
                    .max(*self.ref_counts.get(block_id.index()) as usize);
            }
        }
    }

    /// 引用计数 -1；归零时从 patterns 精确移除并回收。
    pub fn dec_ref(&mut self, block_id: &BlockId) -> bool {
        debug_assert!(
            self.is_valid_block_id(block_id),
            "Invalid block id: {block_id:?}"
        );

        let index = block_id.index();
        let ref_count = self.ref_counts.get_mut(index);
        debug_assert!(*ref_count > 0, "Ref count should be greater than zero");
        *ref_count -= 1;

        if *ref_count == 0 {
            let hash = *self.hashes.get(index);
            let is_leaf = self.node_is_leaf_index(index);
            self.remove_from_patterns(is_leaf, hash, *block_id);
            self.recycle(block_id);
            true
        } else {
            false
        }
    }

    /// 引用计数 +count。
    pub fn inc_ref_by(&mut self, block_id: &BlockId, count: u32) {
        debug_assert!(
            self.is_valid_block_id(block_id),
            "Invalid block id: {block_id:?}"
        );
        *self.ref_counts.get_mut(block_id.index()) += count;
    }

    /// 引用计数 -count；归零时回收。
    pub fn dec_ref_by(&mut self, block_id: &BlockId, count: u32) {
        let index = block_id.index();
        let ref_count = self.ref_counts.get_mut(index);
        debug_assert!(*ref_count >= count, "Ref count underflow");
        *ref_count -= count;

        if *ref_count == 0 {
            let hash = *self.hashes.get(index);
            let is_leaf = self.node_is_leaf_index(index);
            self.remove_from_patterns(is_leaf, hash, *block_id);
            self.recycle(block_id);
        }
    }

    /// 为 `children` 中除 `index` 外的所有非空子节点 +1。
    pub fn inc_child_refs(&mut self, children: &Children, index: usize) {
        for (i, child_id) in children.iter().enumerate() {
            if i == index || child_id.is_empty() {
                continue;
            }
            self.inc_ref(child_id);
        }
    }

    /// 为 `children` 中所有非空子节点 +1。
    pub fn inc_all_child_refs(&mut self, children: &Children) {
        for child_id in children.iter() {
            if !child_id.is_empty() {
                self.inc_ref(child_id);
            }
        }
    }

    /// 为 `children` 中所有非空子节点 -1（归零即回收）。
    pub fn dec_child_refs(&mut self, children: &Children) {
        for child_id in children.iter() {
            if !child_id.is_empty() {
                self.dec_ref(child_id);
            }
        }
    }

    /// 释放整棵以 `block_id` 为根的子树。
    ///
    /// 使用复用的 `Vec` 栈，**边界安全**，不依赖固定大小的 raw 指针栈。
    pub fn dec_ref_recursive(&mut self, block_id: &BlockId) {
        if block_id.is_empty() {
            return;
        }

        let mut stack = std::mem::take(&mut self.dec_ref_rec_stack);
        stack.clear();
        stack.push(*block_id);

        while let Some(current_id) = stack.pop() {
            let index = current_id.index();
            debug_assert!(self.is_valid_block_id(&current_id));

            let ref_count = self.ref_counts.get_mut(index);
            if *ref_count == 0 {
                debug_assert!(false, "dec_ref_recursive on zero ref: {current_id:?}");
                continue;
            }
            *ref_count -= 1;

            if *ref_count == 0 {
                let mask = *self.child_mask.get(index);
                let indices = *self.children.get(index);

                let mut bits = mask;
                while bits != 0 {
                    let i = bits.trailing_zeros() as usize;
                    bits &= !(1 << i);
                    let child_index = indices[i];
                    if child_index == 0 {
                        continue;
                    }
                    let child_ref = *self.ref_counts.get(child_index);
                    if child_ref > 1 {
                        *self.ref_counts.get_mut(child_index) -= 1;
                    } else if child_ref == 1 {
                        let child_id = self.block_id_for(child_index);
                        stack.push(child_id);
                    }
                }

                let hash = *self.hashes.get(index);
                let is_leaf = self.node_is_leaf_index(index);
                self.remove_from_patterns(is_leaf, hash, current_id);
                self.recycle(&current_id);
            }
        }

        stack.clear();
        self.dec_ref_rec_stack = stack;
    }

    /// 回收一个已归零的节点：清空数据、generation +1、压入 free list。
    pub fn recycle(&mut self, block_id: &BlockId) {
        debug_assert!(
            block_id != &self.empty_branch_id,
            "Cannot recycle empty branch"
        );

        let index = block_id.index();
        *self.child_mask.get_mut(index) = 0;
        *self.children.get_mut(index) = EMPTY_CHILD_INDICES;
        *self.values.get_mut(index) = T::default();
        *self.hashes.get_mut(index) = 0;
        *self.ref_counts.get_mut(index) = 0;

        let raw = self.generations.get_mut(index);
        let mut generation = (*raw & GENERATION_MASK) + 1;
        if generation >= BlockId::MAX_GENERATION {
            generation = 0;
            #[cfg(feature = "memory_stats")]
            {
                self.stats.generations_overflows += 1;
            }
        }
        *raw = generation;

        debug_assert!(!self.free_indices.contains(&index), "Double free detected!");
        self.free_indices.push(index);

        #[cfg(feature = "memory_stats")]
        {
            self.stats.alive_nodes -= 1;
            self.stats.total_deallocations += 1;
            self.stats.recycled_nodes += 1;
            let is_leaf = block_id.is_leaf();
            self.stats.leaf_nodes -= is_leaf as usize;
            self.stats.branch_nodes -= (!is_leaf) as usize;
            self.stats.max_generation = self.stats.max_generation.max(generation as usize);
        }
    }

    /// 校验 `block_id` 的 generation / 叶标记是否与当前节点一致。
    #[must_use]
    #[inline]
    pub fn is_valid_block_id(&self, block_id: &BlockId) -> bool {
        let index = block_id.index();
        if index as usize >= self.capacity {
            return false;
        }
        #[cfg(debug_assertions)]
        if self.free_indices.contains(&index) {
            return false;
        }
        let raw = *self.generations.get(index);
        (raw & GENERATION_MASK) == block_id.generation()
            && ((raw & LEAF_FLAG) != 0) == block_id.is_leaf()
    }

    /// 断言 children 中的每个非空节点都有效。
    pub fn ensure_valid_children(&self, children: &Children) {
        for child_id in children.iter() {
            if !child_id.is_empty() {
                assert!(
                    self.is_valid_block_id(child_id),
                    "Invalid child id: {child_id:?}"
                );
            }
        }
    }

    #[inline]
    fn leaf_hash(&self, value: &T) -> u32 {
        #[cfg(test)]
        {
            if let Some(hash) = self.forced_leaf_hash {
                return hash;
            }
        }
        compute_leaf_hash_for_value(value)
    }

    #[inline]
    fn branch_hash(&self, child_hashes: &[u32; MAX_CHILDREN], mask: u8) -> u32 {
        #[cfg(test)]
        {
            if let Some(hash) = self.forced_branch_hash {
                return hash;
            }
        }
        compute_branch_hash_from_child_hashes(child_hashes, mask)
    }

    fn child_hash_array(&self, indices: &[u32; MAX_CHILDREN], mask: u8) -> [u32; MAX_CHILDREN] {
        let mut hashes = [0u32; MAX_CHILDREN];
        let mut bits = mask;
        while bits != 0 {
            let i = bits.trailing_zeros() as usize;
            bits &= !(1 << i);
            if indices[i] != 0 {
                hashes[i] = *self.hashes.get(indices[i]);
            }
        }
        hashes
    }

    fn insert_into_patterns(&mut self, is_leaf: bool, hash: u32, id: BlockId) {
        self.patterns[is_leaf as usize].insert(hash, id);
    }

    /// 从 bucket 中精确移除一个 `id`；Spill 只剩 1 个时收回 Inline。
    fn remove_from_patterns(&mut self, is_leaf: bool, hash: u32, id: BlockId) {
        self.patterns[is_leaf as usize].remove(hash, id);
    }

    /// 取得或创建叶节点；结构（值）相同则复用并 +1 引用。
    pub fn get_or_create_leaf(&mut self, value: T) -> BlockId {
        debug_assert_ne!(value, T::default(), "Leaf value should not be default");

        let hash = self.leaf_hash(&value);

        let mut found = None;
        if let Some(bucket) = self.patterns[1].get(&hash) {
            for candidate in bucket.iter() {
                let index = candidate.index();
                if candidate.is_leaf() && *self.values.get(index) == value {
                    found = Some(*candidate);
                    break;
                }
            }
        }

        if let Some(existing) = found {
            self.leaf_hits += 1;
            #[cfg(feature = "memory_stats")]
            {
                self.stats.total_cache_hits += 1;
                self.stats.leaf_cache_hits += 1;
            }
            self.inc_ref(&existing);
            return existing;
        }

        self.leaf_misses += 1;
        #[cfg(feature = "memory_stats")]
        {
            self.stats.total_cache_misses += 1;
            self.stats.leaf_cache_misses += 1;
        }

        let index = get_next_index_macro!(self);
        let generation = *self.generations.get(index) & GENERATION_MASK;

        *self.generations.get_mut(index) = generation | LEAF_FLAG;
        *self.child_mask.get_mut(index) = 0;
        *self.children.get_mut(index) = EMPTY_CHILD_INDICES;
        *self.values.get_mut(index) = value;
        *self.hashes.get_mut(index) = hash;
        *self.ref_counts.get_mut(index) = 0;

        let block_id = BlockId::new_leaf(index, generation);
        self.insert_into_patterns(true, hash, block_id);
        self.inc_ref(&block_id);

        #[cfg(feature = "memory_stats")]
        {
            self.stats.leaf_nodes += 1;
            self.stats.patterns += 1;
        }

        block_id
    }

    /// 取得或创建分支节点。
    ///
    /// 调用约定（与上游一致）：传入的 `children` 中非空子节点已经持有临时引用；
    /// 复用已有分支时会释放这些临时引用并增加已有分支的引用计数。
    pub fn get_or_create_branch(&mut self, children: Children, _types: u8, _mask: u8) -> BlockId {
        let mask = mask_from_children(&children);
        let indices = indices_from_children(&children);
        let child_hashes = self.child_hash_array(&indices, mask);
        let hash = self.branch_hash(&child_hashes, mask);

        debug_assert_ne!(hash, self.empty_branch_hash, "Empty branch hash collision");

        let mut found = None;
        if let Some(bucket) = self.patterns[0].get(&hash) {
            for candidate in bucket.iter() {
                let index = candidate.index();
                if candidate.is_branch()
                    && *self.child_mask.get(index) == mask
                    && *self.children.get(index) == indices
                {
                    found = Some(*candidate);
                    break;
                }
            }
        }

        if let Some(existing) = found {
            self.branch_hits += 1;
            #[cfg(feature = "memory_stats")]
            {
                self.stats.total_cache_hits += 1;
                self.stats.branch_cache_hits += 1;
            }
            self.dec_child_refs(&children);
            self.inc_ref(&existing);
            return existing;
        }

        self.branch_misses += 1;
        #[cfg(feature = "memory_stats")]
        {
            self.stats.total_cache_misses += 1;
            self.stats.branch_cache_misses += 1;
        }

        let index = get_next_index_macro!(self);
        let generation = *self.generations.get(index) & GENERATION_MASK;
        let types = self.types_from_indices(&indices, mask);

        // 平均值为 LOD 服务（与上游一致）。
        let mut values = [T::default(); MAX_CHILDREN];
        let mut bits = mask;
        while bits != 0 {
            let i = bits.trailing_zeros() as usize;
            bits &= !(1 << i);
            values[i] = *self.values.get(indices[i]);
        }
        let average = T::average(&values);

        *self.generations.get_mut(index) = generation; // 分支：叶标记 0
        *self.child_mask.get_mut(index) = mask;
        *self.children.get_mut(index) = indices;
        *self.values.get_mut(index) = average;
        *self.hashes.get_mut(index) = hash;
        *self.ref_counts.get_mut(index) = 0;

        let block_id = BlockId::new_branch(index, generation, types, mask);
        self.insert_into_patterns(false, hash, block_id);
        self.inc_ref(&block_id);

        #[cfg(feature = "memory_stats")]
        {
            self.stats.branch_nodes += 1;
            self.stats.patterns += 1;
        }

        block_id
    }

    /// 创建（不查重）一个空分支节点，返回时引用计数为 1。
    pub fn create_empty_branch(&mut self) -> BlockId {
        let index = get_next_index_macro!(self);
        let generation = *self.generations.get(index) & GENERATION_MASK;

        *self.generations.get_mut(index) = generation;
        *self.child_mask.get_mut(index) = 0;
        *self.children.get_mut(index) = EMPTY_CHILD_INDICES;
        *self.values.get_mut(index) = T::default();
        *self.ref_counts.get_mut(index) = 0;
        let hash = compute_empty_branch_hash();
        *self.hashes.get_mut(index) = hash;

        let block_id = BlockId::new_branch(index, generation, 0, 0);
        self.insert_into_patterns(false, hash, block_id);
        self.inc_ref(&block_id);

        #[cfg(feature = "memory_stats")]
        {
            self.stats.branch_nodes += 1;
            self.stats.patterns += 1;
        }

        block_id
    }

    /// 创建（不查重）一个分支节点，返回时引用计数为 1。
    ///
    /// 子节点引用计数由调用方负责（与上游 `create_branch` 一致）。
    pub fn create_branch(&mut self, children: Children, types: u8, mask: u8) -> BlockId {
        let index = get_next_index_macro!(self);
        let generation = *self.generations.get(index) & GENERATION_MASK;
        let indices = indices_from_children(&children);

        *self.generations.get_mut(index) = generation;
        *self.child_mask.get_mut(index) = mask;
        *self.children.get_mut(index) = indices;
        *self.values.get_mut(index) = T::default();
        *self.ref_counts.get_mut(index) = 0;

        let child_hashes = self.child_hash_array(&indices, mask);
        let hash = compute_branch_hash_from_child_hashes(&child_hashes, mask);
        *self.hashes.get_mut(index) = hash;

        let block_id = BlockId::new_branch(index, generation, types, mask);
        self.insert_into_patterns(false, hash, block_id);
        self.inc_ref(&block_id);

        #[cfg(feature = "memory_stats")]
        {
            self.stats.branch_nodes += 1;
            self.stats.patterns += 1;
        }

        block_id
    }

    /// 就地替换分支的一个子节点（不改变 patterns 归属）。
    pub fn update_branch(
        &mut self,
        block_id: &BlockId,
        child_id: &BlockId,
        child_index: usize,
        _types: u8,
        _mask: u8,
    ) -> BlockId {
        let index = block_id.index();
        let mut indices = *self.children.get(index);
        let mut mask = *self.child_mask.get(index);

        if child_id.is_empty() {
            indices[child_index] = 0;
            mask &= !(1 << child_index);
        } else {
            indices[child_index] = child_id.index();
            mask |= 1 << child_index;
        }

        *self.children.get_mut(index) = indices;
        *self.child_mask.get_mut(index) = mask;
        let types = self.types_from_indices(&indices, mask);
        let generation = *self.generations.get(index) & GENERATION_MASK;
        BlockId::new_branch(index, generation, types, mask)
    }

    /// 占位创建一个分支下标（导入用），返回对应 `BlockId`。
    pub fn preallocate_branch_id(&mut self, index: u32, types: u8, mask: u8) -> BlockId {
        let next_id = get_next_index_macro!(self);
        assert_eq!(next_id, index, "Invalid block id");
        let generation = *self.generations.get(index) & GENERATION_MASK;
        assert_eq!(generation, 0, "Invalid generation");

        *self.generations.get_mut(index) = generation;
        *self.child_mask.get_mut(index) = mask;
        *self.children.get_mut(index) = EMPTY_CHILD_INDICES;
        *self.ref_counts.get_mut(index) = 0;
        BlockId::new_branch(index, generation, types, mask)
    }

    /// 反序列化一个叶节点（保持给定下标）。
    pub fn deserialize_leaf(&mut self, index: u32, value: T) -> BlockId {
        debug_assert_ne!(value, T::default(), "Leaf value should not be default");
        let hash = compute_leaf_hash_for_value(&value);

        let next_id = get_next_index_macro!(self);
        assert_eq!(next_id, index, "Invalid block id");
        let generation = *self.generations.get(index) & GENERATION_MASK;
        assert_eq!(generation, 0, "Invalid generation");

        *self.generations.get_mut(index) = generation | LEAF_FLAG;
        *self.child_mask.get_mut(index) = 0;
        *self.children.get_mut(index) = EMPTY_CHILD_INDICES;
        *self.values.get_mut(index) = value;
        *self.hashes.get_mut(index) = hash;
        *self.ref_counts.get_mut(index) = 0;

        let block_id = BlockId::new_leaf(index, generation);
        self.insert_into_patterns(true, hash, block_id);
        block_id
    }

    /// 反序列化一个分支节点（保持给定 `BlockId`，并为 all children +1 引用）。
    pub fn deserialize_branch(
        &mut self,
        block_id: BlockId,
        children: Children,
        _types: u8,
        mask: u8,
        average: T,
    ) {
        let index = block_id.index();
        let indices = indices_from_children(&children);
        let derived_mask = mask_from_children(&children);
        let child_hashes = self.child_hash_array(&indices, derived_mask);
        let hash = compute_branch_hash_from_child_hashes(&child_hashes, derived_mask);
        let _ = mask;

        *self.generations.get_mut(index) = block_id.generation();
        *self.child_mask.get_mut(index) = derived_mask;
        *self.children.get_mut(index) = indices;
        *self.values.get_mut(index) = average;
        *self.hashes.get_mut(index) = hash;
        *self.ref_counts.get_mut(index) = 0;

        self.insert_into_patterns(false, hash, block_id);
        self.inc_all_child_refs(&children);
    }

    /// 以人类可读形式转储整棵树。
    pub fn dump_node(&self, node_id: BlockId, depth: u8, prefix: &str) {
        let discovered = self.dump_node_internal(node_id, depth, prefix);
        println!("{prefix}Discovered nodes: {discovered}");
    }

    /// 递归统计节点数。
    pub fn count_nodes(&self, node_id: BlockId) -> u32 {
        if !self.is_valid_block_id(&node_id) || node_id.is_empty() {
            return 0;
        }
        let mut count = 1;
        if node_id.is_branch() {
            let children = self.get_children(&node_id);
            for child in children.iter() {
                if !child.is_empty() {
                    count += self.count_nodes(*child);
                }
            }
        }
        count
    }

    fn dump_node_internal(&self, node_id: BlockId, depth: u8, prefix: &str) -> u32 {
        if node_id.is_empty() {
            return 0;
        }
        if !self.is_valid_block_id(&node_id) {
            println!("{prefix}Invalid block id: {node_id:?}");
            return 0;
        }
        if depth > MAX_ALLOWED_DEPTH {
            panic!("{prefix}Max depth reached");
        }

        let current_prefix = prefix.repeat((depth + 1) as usize);
        let hash = self.get_hash(&node_id);
        let ref_count = self.get_ref(&node_id);

        if node_id.is_leaf() {
            let value = self.get_value(&node_id);
            println!(
                "{current_prefix}Leaf[{}, {}] value: {value}, hash: {hash:08X}, ref_count: {ref_count}",
                node_id.index(),
                node_id.generation(),
            );
            return 1;
        }

        println!(
            "{current_prefix}Branch[{}, {}] hash: {hash:08X}, ref_count: {ref_count}",
            node_id.index(),
            node_id.generation(),
        );

        let mask = node_id.mask();
        let children = self.get_children(&node_id);
        let mut discovered = 1;
        for (i, child) in children.iter().enumerate() {
            if mask & (1 << i) == 0 || child.is_empty() {
                println!("{current_prefix}[{i}]: -");
            } else if child.is_leaf() {
                let value = self.get_value(child);
                println!(
                    "{current_prefix}[{i}]: Leaf[{}, {}] value: {value}",
                    child.index(),
                    child.generation(),
                );
                discovered += 1;
            } else {
                discovered += self.dump_node_internal(*child, depth + 1, prefix);
            }
        }
        discovered
    }

    /// 池 + patterns + free list + 释放栈的估算总字节数。
    ///
    /// 验收指标：不依赖具体 allocator 的精确值，而是
    /// `capacity * node_size()`
    /// `+ Σ (keys.capacity() * 4 + values.capacity() * 8 + spill.capacity() * 8)`
    /// `+ free_indices.capacity() * 4 + stack.capacity() * size_of::<BlockId>()`。
    ///
    /// 目标：`estimated_total_bytes / alive_nodes <= 68`（v1）。
    #[must_use]
    pub fn estimated_total_bytes(&self) -> usize {
        let pools = self.capacity * Self::node_size();

        let mut patterns = 0usize;
        for map in &self.patterns {
            patterns += map.keys.capacity() * std::mem::size_of::<u32>();
            patterns += map.values.capacity() * std::mem::size_of::<u64>();
            patterns += map.spill.capacity() * std::mem::size_of::<BlockId>();
        }

        let free = self.free_indices.capacity() * std::mem::size_of::<u32>();
        let stack = self.dec_ref_rec_stack.capacity() * std::mem::size_of::<BlockId>();

        pools + patterns + free + stack
    }

    /// interner 运行期统计快照（需启用 `memory_stats` feature）。
    #[cfg(feature = "memory_stats")]
    #[must_use]
    pub fn stats(&self) -> InternerStats {
        self.stats
    }

    /// 批处理塌陷分支计数（需启用 `memory_stats` feature）。
    #[cfg(feature = "memory_stats")]
    pub fn bump_collapsed_branches(&mut self) {
        self.stats.collapsed_branches += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_size_within_v1_budget() {
        let size = VoxInterner::<u8>::node_size();
        assert!(size <= 44, "VoxInterner<u8>::node_size() = {size} > 44");
        assert_eq!(size, 1 + 32 + 4 + 2 + 1 + 4);
    }

    /// 按“恰好够用”的预算建数千个不同节点时，
    /// `estimated_total_bytes() / alive_nodes() <= 68`（v1）。
    #[test]
    fn exact_sized_interner_meets_per_node_budget() {
        const BRANCHES: usize = 3000;
        const LEAVES: usize = 255; // 值 1..=255（0 是默认值，不能建叶）
                                   // 空分支哨兵 + 叶 + 分支，按 node_size 恰好预分配。
        let nodes = 1 + LEAVES + BRANCHES;
        let budget = VoxInterner::<u8>::node_size() * nodes;
        let mut interner = VoxInterner::<u8>::with_memory_budget(budget);
        assert_eq!(
            interner.capacity(),
            nodes,
            "budget must be sized close to the node need"
        );

        // 结构唯一的链：每个分支把上一个分支作为 child 0，并在轮转位置放一个叶；
        // 叶通过 get_or_create_leaf 获取，保证每个引用都被正确计数。
        let mut previous = interner.get_or_create_leaf(1);
        for i in 0..BRANCHES {
            let mut children = EMPTY_CHILD;
            children[0] = previous;
            children[1 + (i % 7)] = interner.get_or_create_leaf((i % (LEAVES - 1) + 2) as u8);
            previous = interner.get_or_create_branch(children, 0, 0);
        }

        let alive = interner.alive_nodes();
        assert_eq!(alive, nodes, "all constructed nodes must stay alive");
        let total = interner.estimated_total_bytes();
        let per_node = total / alive;
        println!(
            "exact-sized interner: alive={alive} capacity={} estimated_total_bytes={total} bytes/node={per_node}",
            interner.capacity()
        );
        assert!(
            per_node <= 68,
            "estimated_total_bytes()/alive_nodes() = {per_node} B > 68 B (total={total}, alive={alive})"
        );
    }

    #[test]
    fn growth_auto_doubles_and_keeps_block_ids_stable() {
        let mut interner = VoxInterner::<u8>::with_memory_budget(VoxInterner::<u8>::node_size());
        assert_eq!(interner.capacity(), 1);

        let leaf = interner.get_or_create_leaf(7);
        assert!(interner.capacity() >= 2, "capacity {}", interner.capacity());

        let mut children = EMPTY_CHILD;
        children[0] = leaf;
        let branch = interner.get_or_create_branch(children, 1, 1);

        let capacity_before = interner.capacity();
        let hash_before = interner.content_hash(&branch);

        for value in 8u8..=250 {
            let _ = interner.get_or_create_leaf(value);
        }
        assert!(
            interner.capacity() > capacity_before,
            "expected growth from {}",
            capacity_before
        );

        // 索引稳定：同一 BlockId 仍解析到同一子树。
        assert!(interner.is_valid_block_id(&leaf));
        assert_eq!(*interner.get_value(&leaf), 7);
        assert!(interner.is_valid_block_id(&branch));
        assert_eq!(interner.content_hash(&branch), hash_before);
        let children_after = interner.get_children(&branch);
        assert_eq!(*interner.get_value(&children_after[0]), 7);
    }

    #[test]
    fn growth_preserves_dedup_and_structural_hash() {
        let mut interner =
            VoxInterner::<u8>::with_memory_budget(VoxInterner::<u8>::node_size() * 2);

        fn build(interner: &mut VoxInterner<u8>) -> BlockId {
            let a = interner.get_or_create_leaf(11);
            let b = interner.get_or_create_leaf(22);
            let mut children = EMPTY_CHILD;
            children[0] = a;
            children[1] = b;
            interner.get_or_create_branch(children, 0b11, 0b11)
        }

        let first = build(&mut interner);
        let first_hash = interner.content_hash(&first);

        let capacity_before = interner.capacity();
        for value in 30u8..=250 {
            let _ = interner.get_or_create_leaf(value);
        }
        assert!(interner.capacity() > capacity_before);

        let (branch_hits_before, leaf_hits_before) = interner.cache_hits();
        let second = build(&mut interner);
        let (branch_hits_after, leaf_hits_after) = interner.cache_hits();

        // 增长不破坏去重：结构相同 -> 复用同一节点；结构哈希不变。
        assert_eq!(first, second);
        assert_eq!(interner.content_hash(&first), first_hash);
        assert!(leaf_hits_after > leaf_hits_before);
        assert!(branch_hits_after > branch_hits_before);
    }

    #[test]
    fn hash_collision_keeps_distinct_nodes_and_bucket_converges() {
        let mut interner = VoxInterner::<u8>::with_memory_budget(1 << 16);

        let leaf_a = interner.get_or_create_leaf(1);
        let leaf_b = interner.get_or_create_leaf(2);

        const FORCED: u32 = 0xDEAD_BEEF;
        // 仅测试：强制所有分支哈希相同，制造结构不同却同 hash 的路径。
        interner.forced_branch_hash = Some(FORCED);

        let mut children_a = EMPTY_CHILD;
        children_a[0] = leaf_a;
        let branch_a = interner.get_or_create_branch(children_a, 1, 1);

        let mut children_b = EMPTY_CHILD;
        children_b[0] = leaf_b;
        let branch_b = interner.get_or_create_branch(children_b, 1, 1);

        // 绝不误合并：两个结构不同的分支是不同节点。
        assert_ne!(branch_a, branch_b);
        assert_eq!(interner.get_hash(&branch_a), FORCED);
        assert_eq!(interner.get_hash(&branch_b), FORCED);

        match interner.branch_patterns().get(&FORCED) {
            Some(Bucket::Spill(ids)) => {
                assert_eq!(ids.len(), 2);
                assert!(ids.contains(&branch_a));
                assert!(ids.contains(&branch_b));
            }
            other => panic!("expected Spill bucket, got {other:?}"),
        }

        // 释放一个 -> Spill 收回 Inline，精确移除一个候选。
        interner.dec_ref(&branch_a);
        match interner.branch_patterns().get(&FORCED) {
            Some(Bucket::Inline(only)) => assert_eq!(only, branch_b),
            other => panic!("expected collapsed Inline bucket, got {other:?}"),
        }

        let children = interner.get_children(&branch_b);
        assert_eq!(*interner.get_value(&children[0]), 2);

        interner.forced_branch_hash = None;
    }

    #[test]
    fn dec_ref_recursive_frees_nodes_back_to_baseline() {
        let mut interner = VoxInterner::<u8>::with_memory_budget(1 << 16);
        let baseline = interner.alive_nodes();

        let mut roots = Vec::new();
        for seed in 1u8..=4 {
            let mut children = EMPTY_CHILD;
            for i in 0..8 {
                children[i] = interner.get_or_create_leaf(seed * 10 + i as u8);
            }
            roots.push(interner.get_or_create_branch(children, 0xFF, 0xFF));
        }

        assert!(interner.alive_nodes() > baseline);

        for root in roots {
            interner.dec_ref_recursive(&root);
        }

        assert_eq!(interner.alive_nodes(), baseline, "release must not leak");
        assert!(interner.patterns_empty());
    }

    #[test]
    fn dec_ref_recursive_respects_shared_subtrees() {
        let mut interner = VoxInterner::<u8>::with_memory_budget(1 << 16);
        let baseline = interner.alive_nodes();

        let leaf = interner.get_or_create_leaf(5);
        let mut c1 = EMPTY_CHILD;
        c1[0] = leaf;
        let b1 = interner.get_or_create_branch(c1, 1, 1);

        let leaf_shared = interner.get_or_create_leaf(5);
        assert_eq!(leaf, leaf_shared);
        // b2 must be structurally distinct from b1, otherwise hash-consing
        // legitimately returns the same branch and the leaf is only referenced
        // once. Give b2 a second, different child (still sharing leaf).
        let mut c2 = EMPTY_CHILD;
        c2[0] = leaf_shared;
        c2[1] = interner.get_or_create_leaf(6);
        let b2 = interner.get_or_create_branch(c2, 1, 1);
        assert_ne!(b1, b2);

        assert_eq!(interner.get_ref(&leaf), 2);

        interner.dec_ref_recursive(&b1);
        assert!(interner.is_valid_block_id(&leaf));
        assert_eq!(interner.get_ref(&leaf), 1);

        interner.dec_ref_recursive(&b2);
        assert_eq!(interner.alive_nodes(), baseline);
    }
}
