//! 结构化 u32 哈希。
//!
//! 关键：哈希只依赖**结构**（子节点的结构哈希 + child_mask，或叶值），
//! **不**依赖 interner 节点索引 / generation。因此：
//! - 相同结构的子树永远得到相同哈希；
//! - 倍增节点池（索引稳定但重分配）不改变任何哈希；
//! - content_hash 与节点索引解耦。
//!
//! 使用 rustc-hash 的 FxHasher（固定种子 → 跨运行确定）。

use std::hash::{Hash, Hasher};

use rustc_hash::FxHasher;

use crate::store::{VoxelTrait, MAX_CHILDREN, NODE_TYPE_BRANCH, NODE_TYPE_LEAF};

#[inline(always)]
fn finish_u32(hasher: FxHasher) -> u32 {
    let value = hasher.finish();
    // 折叠 64 -> 32 bit，降低截断带来的偏差。
    (value ^ (value >> 32)) as u32
}

/// 空分支（child_mask = 0）的结构哈希。
#[must_use]
pub fn compute_empty_branch_hash() -> u32 {
    compute_branch_hash_from_child_hashes(&[0u32; MAX_CHILDREN], 0)
}

/// 叶节点的结构哈希：叶域标签 + 值。
#[inline(always)]
pub fn compute_leaf_hash_for_value<T: VoxelTrait>(value: &T) -> u32 {
    let mut hasher = FxHasher::default();
    NODE_TYPE_LEAF.hash(&mut hasher);
    value.hash(&mut hasher);
    finish_u32(hasher)
}

/// 分支节点的结构哈希：分支域标签 + child_mask + 各子节点结构哈希。
///
/// 缺失的子节点以 0 参与混合，mask 本身也已入哈希，故不同 mask 不会混淆。
#[inline(always)]
pub fn compute_branch_hash_from_child_hashes(child_hashes: &[u32; MAX_CHILDREN], mask: u8) -> u32 {
    let mut hasher = FxHasher::default();
    NODE_TYPE_BRANCH.hash(&mut hasher);
    mask.hash(&mut hasher);

    for i in 0..MAX_CHILDREN {
        if mask & (1 << i) != 0 {
            child_hashes[i].hash(&mut hasher);
        } else {
            0u32.hash(&mut hasher);
        }
    }

    finish_u32(hasher)
}
