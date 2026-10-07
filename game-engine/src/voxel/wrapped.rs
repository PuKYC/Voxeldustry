//! LOD 包装树缓存 + 生产网格入口。
//!
//! 旧的 `extract_block` 走 `build_local_occupancy` → `build_local_block_tree`，
//! 会把 SVO-DAG 展开复制成临时 LocalTree；本模块改走
//! `wrap_block`（interner 上 hash-cons）+ `extract_block_tree_with_ao`（直接在
//! interner 上遍历、到 depth 5 即停），零复制。
//!
//! **引用计数**：`VoxTree<u8>` 没有 Drop。缓存里的每棵树都由
//! `wrap_block` 转移来一个 root 引用，因此 `invalidate_covered` / `clear` 必须
//! 显式 `interner.dec_ref_recursive`，否则节点永久驻留。
//!
//! **确定性（L3）**：`blocks` 用 BTreeMap，key = (块原点, lod)，遍历顺序确定。

use std::collections::BTreeMap;

use bevy::prelude::*;
use voxel::mesh::{extract_block_tree_with_ao, wrap_block, AoRectBatch, MeshBlock};
use voxel::store::{ChunkKey, Lod, VoxInterner, VoxTree};

use super::key::lod_block_origin;

/// LOD 包装树缓存。key = (块原点, lod)。
///
/// 每个条目持有一个 root 引用（wrap_block 转移所有权而来）。
/// 因为 VoxTree 没有 Drop，失效/清空时必须显式 dec_ref_recursive，
/// 否则节点永久驻留（见）。
#[derive(Resource, Default)]
pub struct WrappedBlockCache {
    blocks: BTreeMap<(ChunkKey, Lod), VoxTree<u8>>,
}

impl WrappedBlockCache {
    /// 缓存的包装块数量。
    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// 缓存是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// 命中返回缓存；未命中则 wrap 并插入。
    ///
    /// 返回的 `&VoxTree` 借用的是 `self`（出参生命周期取自 `&mut self`），
    /// `interner` 的 `&mut` 借用随调用结束，因此调用方随后仍可拿到
    /// `&interner` 去做提取。需要同时引用多个块时，先全部 `get_or_wrap`
    /// 预热，再用只读 `get` 取回引用（见 `mesh_block_wrapped`）。
    pub fn get_or_wrap(
        &mut self,
        chunks: &BTreeMap<ChunkKey, VoxTree<u8>>,
        interner: &mut VoxInterner<u8>,
        block: MeshBlock,
    ) -> &VoxTree<u8> {
        use std::collections::btree_map::Entry;
        match self.blocks.entry((block.origin, block.lod)) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let tree = wrap_block(chunks, interner, block);
                e.insert(tree)
            }
        }
    }

    /// 只读查询：块必须已经预热（不存在返回 None）。
    ///
    /// 与 `get_or_wrap` 分离，是为了在需要同时借用多个块（本块 + 6 邻）时
    /// 避免与 `&mut interner` 的冲突：先用 `get_or_wrap` 全部插入，再统一
    /// 用本函数取不可变引用。
    #[must_use]
    pub fn get(&self, origin: ChunkKey, lod: Lod) -> Option<&VoxTree<u8>> {
        self.blocks.get(&(origin, lod))
    }

    /// 使覆盖 key 的所有 LOD 块失效并释放其引用。
    pub fn invalidate_covered(&mut self, interner: &mut VoxInterner<u8>, key: ChunkKey) {
        for lod in 0..=voxel::mesh::MAX_LOD {
            let lod = Lod::new(lod);
            let origin = lod_block_origin(key, lod);
            if let Some(tree) = self.blocks.remove(&(origin, lod)) {
                let root = tree.get_root_id();
                if !root.is_empty() {
                    interner.dec_ref_recursive(&root);
                }
            }
        }
    }

    /// 清空并释放全部引用（release_body / 关卡卸载时调用）。
    pub fn clear(&mut self, interner: &mut VoxInterner<u8>) {
        for (_, tree) in std::mem::take(&mut self.blocks) {
            let root = tree.get_root_id();
            if !root.is_empty() {
                interner.dec_ref_recursive(&root);
            }
        }
    }
}

/// 只读网格入口：wrap 本块后调用 `extract_block_tree_with_ao`。
///
/// 借用顺序说明：先 `cache.get_or_wrap(.., interner, ..)` 以 `&mut cache` /
/// `&mut interner` 完成预热（返回引用借用 cache，interner 借用在此结束），
/// 再用只读 `cache.get` 取回 `&VoxTree`，最后以 `&interner` 提取。
/// 全程不复制/克隆整棵树。
#[must_use]
pub fn mesh_block_wrapped(
    chunks: &BTreeMap<ChunkKey, VoxTree<u8>>,
    interner: &mut VoxInterner<u8>,
    cache: &mut WrappedBlockCache,
    block: MeshBlock,
    external: [Option<&VoxTree<u8>>; 6],
) -> AoRectBatch {
    let _ = cache.get_or_wrap(chunks, interner, block);
    let tree = cache
        .get(block.origin, block.lod)
        .expect("get_or_wrap must have inserted the requested block");
    extract_block_tree_with_ao(tree, interner, block, external)
}
