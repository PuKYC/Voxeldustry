//! 结构确定性哈希与释放路径。
//!
//! content_hash 只遍历子树形状与叶值，不使用 interner 节点索引，因此与构建
//! 顺序、节点分配顺序无关；同一逻辑体在任意 interner 实例上得到同一哈希。

use voxel::store::{BlockId, VoxInterner, VoxTree};

use super::volume::VoxVolume;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// TODO 可能独立出来
/// 自研 FNV-1a 64：跨平台 / 跨版本可复现（std DefaultHasher 不作确定性承诺）。
struct Fnv(u64);

impl Fnv {
    #[inline]
    fn new() -> Self {
        Self(FNV_OFFSET)
    }

    #[inline]
    fn write_u8(&mut self, b: u8) {
        self.0 ^= b as u64;
        self.0 = self.0.wrapping_mul(FNV_PRIME);
    }

    #[inline]
    fn write_i32(&mut self, v: i32) {
        for b in v.to_le_bytes() {
            self.write_u8(b);
        }
    }
}

fn hash_node(interner: &VoxInterner<u8>, id: BlockId, h: &mut Fnv) {
    if id.is_empty() {
        h.write_u8(0);
        return;
    }
    if id.is_leaf() {
        h.write_u8(1);
        h.write_u8(*interner.get_value(&id));
        return;
    }
    h.write_u8(2);
    let children = interner.get_children(&id);
    for child in children.iter() {
        hash_node(interner, *child, h);
    }
}

/// 单个树的结构哈希；只依赖子树形状与叶值，不依赖 interner 节点索引。
pub fn chunk_content_hash(interner: &VoxInterner<u8>, tree: &VoxTree<u8>) -> u64 {
    let mut h = Fnv::new();
    h.write_u8(0xC5);
    hash_node(interner, tree.get_root_id(), &mut h);
    h.0
}

/// 整个体素体的结构哈希；chunk 按 ChunkKey 升序遍历（确定）。
pub fn content_hash(interner: &VoxInterner<u8>, volume: &VoxVolume) -> u64 {
    let mut h = Fnv::new();
    h.write_u8(0xB0);
    for (key, tree) in volume.chunks.iter() {
        h.write_i32(key.x);
        h.write_i32(key.y);
        h.write_i32(key.z);
        hash_node(interner, tree.get_root_id(), &mut h);
    }
    h.0
}

fn collect_nodes(interner: &VoxInterner<u8>, id: BlockId, visited: &mut Vec<BlockId>) {
    if id.is_empty() || visited.contains(&id) {
        return;
    }
    visited.push(id);
    if id.is_leaf() {
        return;
    }
    for child in interner.get_children(&id).iter() {
        collect_nodes(interner, *child, visited);
    }
}

/// 统计 volume 中可达节点数（对共享子树去重）。
pub fn count_body_nodes(interner: &VoxInterner<u8>, volume: &VoxVolume) -> usize {
    let mut visited: Vec<BlockId> = Vec::new();
    for tree in volume.chunks.values() {
        collect_nodes(interner, tree.get_root_id(), &mut visited);
    }
    visited.len()
}

/// 对单个根的 dec_ref_recursive 暴露（空根跳过）。
pub fn dec_ref_recursive(interner: &mut VoxInterner<u8>, root: &BlockId) {
    if !root.is_empty() {
        interner.dec_ref_recursive(root);
    }
}

/// 释放一个体素体：遍历所有 chunk 根做 dec_ref_recursive 并清空容器。
/// 返回释放的根数量。
///
/// 注意：若调用点持有 [`super::wrapped::WrappedBlockCache`]，必须先用
/// [`release_body_with_cache`]，否则包装缓存里的 root 引用会把 body 节点钉住
/// （见）。本函数保持原签名不变，供无缓存的调用点使用。
pub fn release_body(interner: &mut VoxInterner<u8>, volume: &mut VoxVolume) -> usize {
    let mut released = 0;
    for tree in volume.chunks.values() {
        let root = tree.get_root_id();
        if !root.is_empty() {
            interner.dec_ref_recursive(&root);
            released += 1;
        }
    }
    volume.chunks.clear();
    released
}

/// 先清空包装缓存再释放 body。
///
/// 保持 `release_body` 的公开签名不变，生产调用链若持有 WrappedBlockCache
/// 应改用本函数。
pub fn release_body_with_cache(
    interner: &mut VoxInterner<u8>,
    volume: &mut VoxVolume,
    cache: &mut super::wrapped::WrappedBlockCache,
) -> usize {
    cache.clear(interner);
    release_body(interner, volume)
}
