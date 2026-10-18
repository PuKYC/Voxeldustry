//! 按 LOD 直接生成的块树缓存（godot_voxel 的 per-LOD generation）。
//!
//! LOD>0 的块不再从基础子块 `wrap_block` 折叠：那条路径的节点代表值由
//! `T::average` 决定（8 个子节点都在地下时被投票成 DIRT/STONE），会让粗格
//! 顶面露出地下材质。这里改为按块自己的 LOD 直接生成
//! （[`crate::voxel::terrain::generation::WorldGenerator::generate_block_at_lod`]），
//! 地表在生成时就是地表。
//!
//! **引用计数**：`VoxTree<u8>` 没有 Drop。缓存里的每棵树都由生成函数转移来
//! 一个 root 引用，因此 `invalidate_covered` / `clear` 必须显式
//! `dec_ref_recursive`，否则节点永久驻留。
//!
//! **确定性（L3）**：`blocks` 用 BTreeMap，key = (块原点, lod)；生成结果只依赖
//! `(origin, lod)`，与缓存插入顺序无关。

use std::collections::BTreeMap;

use game_engine::voxel::{lod_block_origin, ChunkKey, Lod, MeshBlock, VoxInterner, VoxTree};

use crate::voxel::lod::MAX_LOD;
use crate::voxel::terrain::generation::WorldGenerator;

/// 按 LOD 生成的块树缓存（key = (块原点, lod)）。
///
/// 只接收 `lod > 0` 的块；`lod == 0` 仍走
/// [`game_engine::presentation::voxel::WrappedBlockCache`]（逐位不变）。
#[derive(Default)]
pub struct GeneratedBlockCache {
    blocks: BTreeMap<(ChunkKey, Lod), VoxTree<u8>>,
}

impl GeneratedBlockCache {
    /// 缓存的块树数量。
    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// 缓存是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// 命中返回缓存；未命中则按该 LOD 生成并插入。
    ///
    /// 生成的树只依赖 `(origin, lod)`，因此命中/未命中不影响内容（L3）。
    pub fn get_or_generate(
        &mut self,
        interner: &mut VoxInterner<u8>,
        generator: &WorldGenerator,
        block: MeshBlock,
    ) -> &VoxTree<u8> {
        use std::collections::btree_map::Entry;
        match self.blocks.entry((block.origin, block.lod)) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let tree = generator.generate_block_at_lod(interner, block.origin, block.lod.lod());
                entry.insert(tree)
            }
        }
    }

    /// 只读查询：块必须已经预热（不存在返回 None）。
    ///
    /// 与 [`Self::get_or_generate`] 分离，是为了同时借用多个块（本块 + 26 邻）
    /// 时避开 `&mut interner` 冲突：先全部预热，再统一用本函数取不可变引用。
    #[must_use]
    pub fn get(&self, origin: ChunkKey, lod: Lod) -> Option<&VoxTree<u8>> {
        self.blocks.get(&(origin, lod))
    }

    /// 使覆盖 key 的所有 LOD 块失效并释放其引用。
    pub fn invalidate_covered(&mut self, interner: &mut VoxInterner<u8>, key: ChunkKey) {
        for lod in 0..=MAX_LOD {
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
