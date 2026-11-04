//! 确定性导入：严格校验 magic / version / depth / 截断与尾部多余字节。
//!
//! 通过 `get_or_create_leaf` / `get_or_create_branch` 重建结构，因此
//! 导入结果与导出前的结构一致，且共享子树自动去重。

use std::collections::BTreeMap;
use std::io::{self, Read};

use crate::store::{
    BlockId, ByteConversion, ChunkKey, Lod, MaxDepth, VoxInterner, VoxOpsConfig, VoxTree,
    VoxelTrait, EMPTY_CHILD, MAX_ALLOWED_DEPTH, MAX_CHILDREN,
};

use super::{MAGIC, TAG_BRANCH, TAG_EMPTY, TAG_LEAF, VERSION};

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_u8<R: Read>(reader: &mut R) -> io::Result<u8> {
    let mut bytes = [0u8; 1];
    reader.read_exact(&mut bytes)?;
    Ok(bytes[0])
}

fn read_u16<R: Read>(reader: &mut R) -> io::Result<u16> {
    let mut bytes = [0u8; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32<R: Read>(reader: &mut R) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_i32<R: Read>(reader: &mut R) -> io::Result<i32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(i32::from_le_bytes(bytes))
}

fn types_from_children(children: &[BlockId; MAX_CHILDREN]) -> u8 {
    let mut types = 0u8;
    for (i, child) in children.iter().enumerate() {
        if child.is_leaf() {
            types |= 1 << i;
        }
    }
    types
}

fn read_node<R: Read, T: VoxelTrait>(
    reader: &mut R,
    interner: &mut VoxInterner<T>,
) -> io::Result<BlockId> {
    let tag = read_u8(reader)?;
    match tag {
        TAG_EMPTY => Ok(BlockId::EMPTY),
        TAG_LEAF => {
            let value = T::read_from_le(reader)?;
            if value == T::default() {
                return Err(invalid("voxel io: default leaf value"));
            }
            Ok(interner.get_or_create_leaf(value))
        }
        TAG_BRANCH => {
            let mask = read_u8(reader)?;
            if mask == 0 {
                return Ok(BlockId::EMPTY);
            }
            let mut children = EMPTY_CHILD;
            for (i, child) in children.iter_mut().enumerate() {
                if mask & (1 << i) != 0 {
                    *child = read_node(reader, interner)?;
                }
            }
            let types = types_from_children(&children);
            Ok(interner.get_or_create_branch(children, types, mask))
        }
        _ => Err(invalid("voxel io: unknown node tag")),
    }
}

fn read_header<R: Read>(reader: &mut R) -> io::Result<()> {
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if magic != MAGIC {
        return Err(invalid("voxel io: bad magic"));
    }
    let version = read_u16(reader)?;
    if version != VERSION {
        return Err(invalid("voxel io: unsupported version"));
    }
    Ok(())
}

fn validate_max_depth(max_depth: u8) -> io::Result<MaxDepth> {
    if max_depth >= MAX_ALLOWED_DEPTH {
        return Err(invalid("voxel io: max_depth exceeds MAX_ALLOWED_DEPTH"));
    }
    Ok(MaxDepth::new(max_depth))
}

/// 从字节流导入单棵树。
pub fn import_tree<T: VoxelTrait>(bytes: &[u8]) -> io::Result<(VoxTree<T>, VoxInterner<T>)> {
    let mut cursor = io::Cursor::new(bytes);
    read_header(&mut cursor)?;
    let max_depth = validate_max_depth(read_u8(&mut cursor)?)?;

    let mut interner = VoxInterner::<T>::with_memory_budget(VoxInterner::<T>::node_size() * 16);
    let root = read_node(&mut cursor, &mut interner)?;
    let tree = VoxTree::from_root(max_depth, root);

    if cursor.position() as usize != bytes.len() {
        return Err(invalid("voxel io: trailing bytes"));
    }

    Ok((tree, interner))
}

/// 把一个节点从 `src` 克隆进 `dst`，返回的 id 在 `dst` 中**持有 1 个引用**。
///
/// 记忆化：同一棵子树第二次出现时不再重建，只对已有节点 `inc_ref`，
/// 因此 DAG 共享在 `dst` 里保持共享（不会被展开成指数级副本）。
/// `BTreeMap` 而不是 `HashMap`：遍历顺序只由 `BlockId` 的全序决定，
/// 不引入哈希种子导致的非确定性。
fn clone_node_into<T: VoxelTrait>(
    id: BlockId,
    src: &VoxInterner<T>,
    dst: &mut VoxInterner<T>,
    memo: &mut BTreeMap<BlockId, BlockId>,
) -> BlockId {
    if id.is_empty() {
        return BlockId::EMPTY;
    }

    if let Some(existing) = memo.get(&id) {
        // 第二处出现：共享同一节点，再 +1 引用交给调用方的父节点。
        // 空节点没有引用可加（哨兵节点的 ref_count 恒为 0）。
        if !existing.is_empty() {
            dst.inc_ref(existing);
        }
        return *existing;
    }

    let cloned = if id.is_leaf() {
        dst.get_or_create_leaf(*src.get_value(&id))
    } else {
        let mask = id.mask();
        if mask == 0 {
            // 与 `read_node` 一致：零掩码分支在字节格式里没有表示，
            // 方一出现就按空树落库（`get_or_create_branch` 也不允许构造它）。
            BlockId::EMPTY
        } else {
            let children = src.get_children(&id);
            let mut cloned_children = EMPTY_CHILD;
            for i in 0..MAX_CHILDREN {
                if mask & (1 << i) != 0 {
                    cloned_children[i] = clone_node_into(children[i], src, dst, memo);
                }
            }
            let types = types_from_children(&cloned_children);
            dst.get_or_create_branch(cloned_children, types, mask)
        }
    };

    memo.insert(id, cloned);
    cloned
}

/// 把 `src` 里的一棵树重建进 `dst`，返回指向 `dst` 的树。
///
/// - 只读 `src`（不改引用计数），写 `dst`（每个节点 1 个引用）；
/// - 共享子树按 `BlockId` 记忆化，DAG 结构与去重保持；
/// - `max_depth` 原样保留；空根保持 `BlockId::EMPTY`（不会凭空造节点）。
///
/// 调用方拿到返回值后，引用计数语义与 [`VoxTree::from_root`] 一致：
/// 这棵树持有根节点的 1 个引用，释放要用 `dst.dec_ref_recursive(&root)`
/// （`VoxTree` 没有 `Drop`）。
#[must_use]
pub fn clone_tree_into<T: VoxelTrait>(
    tree: &VoxTree<T>,
    src: &VoxInterner<T>,
    dst: &mut VoxInterner<T>,
) -> VoxTree<T> {
    let mut memo: BTreeMap<BlockId, BlockId> = BTreeMap::new();
    let root = clone_node_into(tree.get_root_id(), src, dst, &mut memo);
    VoxTree::from_root(tree.max_depth(Lod::new(0)), root)
}

/// 从字节流导入单棵树，装进**已有**的 `dst` interner。
///
/// 先用 [`import_tree`] 解码到临时 interner，成功后才 [`clone_tree_into`] 到 `dst`：
///
/// - **失败原子性**：任何解码失败都发生在临时 interner 里，`dst` 一个节点都不会多
///   （不会出现“解码到一半留下的半棵树”）；
/// - **不泄漏**：临时 interner 的根用 `dec_ref_recursive` 显式释放，
///   不依赖 Drop（`VoxTree` 没有 `Drop`）；
/// - **去重**：同一份字节连续导入两次，第二次不新增节点、返回相同根 id。
pub fn import_tree_into<T: VoxelTrait>(
    bytes: &[u8],
    dst: &mut VoxInterner<T>,
) -> io::Result<VoxTree<T>> {
    let (tree, mut temp) = import_tree::<T>(bytes)?;
    let cloned = clone_tree_into(&tree, &temp, dst);
    // VoxTree 没有 Drop：临时 interner 的整棵子树必须显式归还，
    // 否则节点会一直驻留到 temp 被丢弃（语义上就是泄漏）。
    temp.dec_ref_recursive(&tree.get_root_id());
    Ok(cloned)
}

/// 从字节流导入一组子块（`ChunkKey` 升序），共享同一个 interner。
pub fn import_volume<T: VoxelTrait>(
    bytes: &[u8],
) -> io::Result<(BTreeMap<ChunkKey, VoxTree<T>>, VoxInterner<T>)> {
    let mut cursor = io::Cursor::new(bytes);
    read_header(&mut cursor)?;
    let chunk_count = read_u32(&mut cursor)?;

    let mut interner = VoxInterner::<T>::with_memory_budget(VoxInterner::<T>::node_size() * 16);
    let mut chunks = BTreeMap::new();

    for _ in 0..chunk_count {
        let x = read_i32(&mut cursor)?;
        let y = read_i32(&mut cursor)?;
        let z = read_i32(&mut cursor)?;
        let max_depth = validate_max_depth(read_u8(&mut cursor)?)?;
        let root = read_node(&mut cursor, &mut interner)?;
        chunks.insert(ChunkKey::new(x, y, z), VoxTree::from_root(max_depth, root));
    }

    if cursor.position() as usize != bytes.len() {
        return Err(invalid("voxel io: trailing bytes"));
    }

    Ok((chunks, interner))
}

#[cfg(test)]
mod tests {
    use glam::IVec3;

    use crate::io::export_tree;
    use crate::store::{MaxDepth, VoxOpsBulkWrite, VoxOpsRead, VoxOpsWrite};

    use super::*;

    /// 建一棵含编辑、且天然带共享子树的树：
    /// `fill` 之后同一层的兄弟八分之一体结构完全相同，interner 会把它们去重成同一个
    /// `BlockId`，所以导出 / 克隆都必须保住这份 DAG 共享。
    fn build_tree(seed: u8) -> (VoxTree<u8>, VoxInterner<u8>) {
        let mut interner = VoxInterner::<u8>::with_memory_budget(1 << 16);
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(4));

        tree.fill(&mut interner, 3);
        tree.set(&mut interner, IVec3::new(0, 0, 0), seed);
        tree.set(&mut interner, IVec3::new(15, 15, 15), seed.wrapping_add(1));
        tree.set(&mut interner, IVec3::new(8, 4, 12), seed.wrapping_add(2));
        // 两个同构的八分之一体：值的形状相同 -> 子树被去重成同一个 id。
        for base in [IVec3::new(0, 0, 0), IVec3::new(8, 8, 8)] {
            tree.set(&mut interner, base + IVec3::new(1, 1, 1), 7);
            tree.set(&mut interner, base + IVec3::new(2, 2, 2), 9);
        }
        // 再把一个体素改回默认值：覆盖“编辑成空”的路径。
        tree.set(&mut interner, IVec3::new(3, 3, 3), 0);

        (tree, interner)
    }

    fn new_interner() -> VoxInterner<u8> {
        VoxInterner::<u8>::with_memory_budget(1 << 16)
    }

    #[test]
    fn clone_tree_into_rebuilds_into_a_fresh_interner() {
        let (tree, src) = build_tree(7);
        let bytes = export_tree(&tree, &src);

        let mut dst = new_interner();
        let before = dst.alive_nodes();
        let cloned = clone_tree_into(&tree, &src, &mut dst);

        assert_eq!(
            cloned.max_depth(Lod::new(0)),
            tree.max_depth(Lod::new(0)),
            "max_depth 必须原样保留"
        );
        let after = dst.alive_nodes();
        assert!(after > before, "克隆必须在 dst 里建出节点");
        assert_eq!(
            after - before,
            src.alive_nodes() - 1,
            "克隆必须一一复刻源 interner 的节点集合（不多不少；-1 是空分支哨兵）"
        );
        assert_eq!(
            export_tree(&cloned, &dst),
            bytes,
            "克隆树的结构必须与源树逐字节一致"
        );

        // 体素内容抽查。
        assert_eq!(cloned.get(&dst, IVec3::new(0, 0, 0)), Some(7));
        assert_eq!(cloned.get(&dst, IVec3::new(15, 15, 15)), Some(8));
        // (3,3,3) 被编辑回默认值 -> 空；(8,8,8) 仍是 fill 的 3。
        assert_eq!(cloned.get(&dst, IVec3::new(3, 3, 3)), None);
        assert_eq!(cloned.get(&dst, IVec3::new(8, 8, 8)), Some(3));

        // 共享子树仍然共享：多个兄弟叶节点指向同一个 BlockId。
        let children = dst.get_children(&cloned.get_root_id());
        let leaf_children: Vec<BlockId> = children
            .iter()
            .filter(|child| !child.is_empty() && child.is_leaf())
            .copied()
            .collect();
        assert!(leaf_children.len() >= 2, "构造的树里应当有多个同构兄弟子树");
        assert!(
            leaf_children.windows(2).all(|pair| pair[0] == pair[1]),
            "共享子树克隆后必须仍指向同一个 BlockId"
        );

        // 源 interner 一个节点都不能少（clone 只读 src）。
        assert_eq!(export_tree(&tree, &src), bytes);

        // 调用方释放克隆：dst 回落到克隆前。
        dst.dec_ref_recursive(&cloned.get_root_id());
        assert_eq!(dst.alive_nodes(), before);
    }

    #[test]
    fn import_tree_into_roundtrips_byte_identically() {
        let (tree, src) = build_tree(11);
        let bytes = export_tree(&tree, &src);

        let mut dst = new_interner();
        let before = dst.alive_nodes();
        let imported = import_tree_into::<u8>(&bytes, &mut dst).unwrap();

        assert!(dst.alive_nodes() > before);
        assert_eq!(
            export_tree(&imported, &dst),
            bytes,
            "导出 -> import_tree_into -> 再导出必须逐字节相同"
        );
        assert_eq!(imported.get(&dst, IVec3::new(0, 0, 0)), Some(11));
        assert_eq!(imported.get(&dst, IVec3::new(15, 15, 15)), Some(12));
        assert_eq!(imported.get(&dst, IVec3::new(3, 3, 3)), None);

        // 临时 interner 没有给 dst 留下任何东西：
        // 释放返回的树后，dst 的节点数正好回到导入前。
        dst.dec_ref_recursive(&imported.get_root_id());
        assert_eq!(dst.alive_nodes(), before);
    }

    #[test]
    fn import_tree_into_rejects_bad_bytes_without_touching_dst() {
        let (tree, src) = build_tree(1);
        let bytes = export_tree(&tree, &src);

        let mut dst = new_interner();
        // 先放一个既有节点：若实现“先清空 dst 再解码”，这里能直接看出来。
        let keep = dst.get_or_create_leaf(42u8);
        let before = dst.alive_nodes();

        let mut bad_magic = bytes.clone();
        bad_magic[0] = b'X';
        let mut bad_version = bytes.clone();
        bad_version[8] = 0xFF; // version 是 offset 8 的 u16LE
        let truncated = &bytes[..bytes.len() - 1];
        let mut trailing = bytes.clone();
        trailing.push(0);

        for bad in [
            bad_magic.as_slice(),
            bad_version.as_slice(),
            truncated,
            trailing.as_slice(),
        ] {
            assert!(
                import_tree_into::<u8>(bad, &mut dst).is_err(),
                "坏字节必须报错"
            );
            assert_eq!(dst.alive_nodes(), before, "解码失败不得给 dst 增加节点");
        }

        assert_eq!(dst.get_ref(&keep), 1, "解码失败不得改动 dst 里已有的节点");
    }

    #[test]
    fn importing_the_same_bytes_twice_dedups_and_reuses_the_root() {
        let (tree, src) = build_tree(5);
        let bytes = export_tree(&tree, &src);

        let mut dst = new_interner();
        let before = dst.alive_nodes();

        let first = import_tree_into::<u8>(&bytes, &mut dst).unwrap();
        let root = first.get_root_id();
        let after_first = dst.alive_nodes();
        assert_eq!(dst.get_ref(&root), 1, "第一次导入后根持有 1 个引用");

        let second = import_tree_into::<u8>(&bytes, &mut dst).unwrap();
        assert_eq!(second.get_root_id(), root, "去重后两次返回同一个根 id");
        assert_eq!(dst.alive_nodes(), after_first, "第二次导入不得新增节点");
        assert_eq!(dst.get_ref(&root), 2, "两棵树各持有根的一个引用");

        // 两棵树都释放后，节点数回到导入前。
        dst.dec_ref_recursive(&second.get_root_id());
        dst.dec_ref_recursive(&first.get_root_id());
        assert_eq!(dst.alive_nodes(), before);
    }

    #[test]
    fn empty_tree_clone_and_import_keep_the_empty_root() {
        let src = new_interner();
        let tree = VoxTree::<u8>::new(MaxDepth::new(4));

        let mut dst = new_interner();
        let before = dst.alive_nodes();

        let cloned = clone_tree_into(&tree, &src, &mut dst);
        assert!(cloned.get_root_id().is_empty(), "空根必须保持空根");
        assert_eq!(cloned.max_depth(Lod::new(0)), tree.max_depth(Lod::new(0)));
        assert_eq!(dst.alive_nodes(), before, "空树克隆不得造节点");

        let bytes = export_tree(&tree, &src);
        let imported = import_tree_into::<u8>(&bytes, &mut dst).unwrap();
        assert!(imported.get_root_id().is_empty());
        assert_eq!(imported.max_depth(Lod::new(0)).max(), 4);
        assert_eq!(dst.alive_nodes(), before, "空树导入不得造节点");
    }
}
