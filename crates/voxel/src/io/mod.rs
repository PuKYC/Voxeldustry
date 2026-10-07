//! 确定性存档层。
//!
//! 规则：
//! 1. **只按结构遍历**：子节点按索引 0..7 升序；体积按 `ChunkKey` 升序。
//! 2. **绝不迭代 patterns / HashMap** 来产生字节或哈希。
//! 3. 自描述格式：magic + version + len，导入时做边界与版本校验。
//!
//! 单棵树格式：
//! `magic[8] | version u16LE | max_depth u8 | node*`
//! 体积格式：
//! `magic[8] | version u16LE | chunk_count u32LE | (x i32LE, y i32LE, z i32LE,
//! max_depth u8, node*)*`
//!
//! 节点记录（前序）：
//! - `TAG_EMPTY`：空；
//! - `TAG_LEAF` + 值的 LE 字节；
//! - `TAG_BRANCH` + child_mask u8 + 依次递归每个已存在的子节点（下标升序）。

pub mod export;
pub mod import;

pub use export::{export_tree, export_volume};
pub use import::{import_tree, import_volume};

/// 格式魔数（8 字节，含结尾 NUL）。
pub const MAGIC: [u8; 8] = *b"VOXELIO\0";
/// 当前格式版本。
pub const VERSION: u16 = 1;

/// 空节点标签。
pub const TAG_EMPTY: u8 = 0;
/// 叶节点标签。
pub const TAG_LEAF: u8 = 1;
/// 分支节点标签。
pub const TAG_BRANCH: u8 = 2;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use glam::IVec3;

    use crate::store::{
        ChunkKey, MaxDepth, VoxInterner, VoxOpsBulkWrite, VoxOpsRead, VoxOpsWrite, VoxTree,
    };

    use super::*;

    fn build_tree(seed: u8) -> (VoxTree<u8>, VoxInterner<u8>) {
        let mut interner = VoxInterner::<u8>::with_memory_budget(1 << 16);
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(4));

        // 一个包含 fill / set 的确定性结构。
        tree.fill(&mut interner, 3);
        tree.set(&mut interner, IVec3::new(0, 0, 0), seed);
        tree.set(&mut interner, IVec3::new(15, 15, 15), seed + 1);
        tree.set(&mut interner, IVec3::new(8, 4, 12), seed + 2);
        tree.set(&mut interner, IVec3::new(3, 3, 3), 0);

        (tree, interner)
    }

    #[test]
    fn export_is_deterministic_across_independent_runs() {
        let (tree_a, interner_a) = build_tree(11);
        let (tree_b, interner_b) = build_tree(11);
        let bytes_a = export_tree(&tree_a, &interner_a);
        let bytes_b = export_tree(&tree_b, &interner_b);
        assert_eq!(bytes_a, bytes_b);
        assert!(bytes_a.starts_with(&MAGIC));
    }

    #[test]
    fn export_import_roundtrip_is_byte_identical() {
        let (tree, interner) = build_tree(7);
        let bytes = export_tree(&tree, &interner);

        let (imported, imported_interner) = import_tree::<u8>(&bytes).unwrap();
        let reexported = export_tree(&imported, &imported_interner);
        assert_eq!(bytes, reexported, "round-trip must be byte-identical");

        // 结构一致性抽查。
        assert_eq!(
            imported.get(&imported_interner, IVec3::new(0, 0, 0)),
            Some(7)
        );
        assert_eq!(
            imported.get(&imported_interner, IVec3::new(15, 15, 15)),
            Some(8)
        );
        assert_eq!(imported.get(&imported_interner, IVec3::new(3, 3, 3)), None);
        assert_eq!(
            imported.get(&imported_interner, IVec3::new(1, 2, 3)),
            Some(3)
        );
    }

    #[test]
    fn volume_export_is_sorted_by_chunk_key_and_roundtrips() {
        let mut interner = VoxInterner::<u8>::with_memory_budget(1 << 16);
        let mut chunks: BTreeMap<ChunkKey, VoxTree<u8>> = BTreeMap::new();

        for key in [
            ChunkKey::new(2, 0, 0),
            ChunkKey::new(-1, 0, 0),
            ChunkKey::new(0, 5, 0),
        ] {
            let mut tree = VoxTree::<u8>::new(MaxDepth::new(2));
            tree.set(&mut interner, IVec3::new(0, 0, 0), (key.x + 4) as u8);
            chunks.insert(key, tree);
        }

        let bytes = export_volume(&chunks, &interner);
        let (imported, imported_interner) = import_volume::<u8>(&bytes).unwrap();
        assert_eq!(imported.len(), chunks.len());
        assert_eq!(
            imported.keys().copied().collect::<Vec<_>>(),
            chunks.keys().copied().collect::<Vec<_>>()
        );

        let reexported = export_volume(&imported, &imported_interner);
        assert_eq!(bytes, reexported);
    }

    #[test]
    fn import_rejects_bad_magic_and_truncation() {
        let (tree, interner) = build_tree(1);
        let mut bytes = export_tree(&tree, &interner);
        bytes[0] = b'X';
        assert!(import_tree::<u8>(&bytes).is_err());

        let (tree, interner) = build_tree(1);
        let bytes = export_tree(&tree, &interner);
        assert!(import_tree::<u8>(&bytes[..bytes.len() - 1]).is_err());
    }
}
