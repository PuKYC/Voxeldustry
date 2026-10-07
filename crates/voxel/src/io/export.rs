//! 确定性导出（结构先序，子节点索引升序）。
//!
//! 绝不迭代任何 HashMap；体积按 `BTreeMap<ChunkKey, _>` 升序。

use std::collections::BTreeMap;
use std::io::{self, Write};

use crate::store::{
    BlockId, ByteConversion, ChunkKey, Lod, VoxInterner, VoxOpsConfig, VoxTree, VoxelTrait,
    MAX_CHILDREN,
};

use super::{MAGIC, TAG_BRANCH, TAG_EMPTY, TAG_LEAF, VERSION};

fn write_node<W: Write, T: VoxelTrait>(
    out: &mut W,
    interner: &VoxInterner<T>,
    id: &BlockId,
) -> io::Result<()> {
    if id.is_empty() {
        return out.write_all(&[TAG_EMPTY]);
    }
    if id.is_leaf() {
        out.write_all(&[TAG_LEAF])?;
        return interner.get_value(id).write_as_le(out);
    }

    out.write_all(&[TAG_BRANCH])?;
    let mask = id.mask();
    out.write_all(&[mask])?;

    let children = interner.get_children(id);
    for i in 0..MAX_CHILDREN {
        if mask & (1 << i) != 0 {
            write_node(out, interner, &children[i])?;
        }
    }
    Ok(())
}

/// 导出一棵树为确定性字节流。
#[must_use]
pub fn export_tree<T: VoxelTrait>(tree: &VoxTree<T>, interner: &VoxInterner<T>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.push(tree.max_depth(Lod::new(0)).max());
    write_node(&mut out, interner, &tree.get_root_id()).expect("writing to Vec cannot fail");
    out
}

/// 导出一组子块（按 `ChunkKey` 升序）为确定性字节流。
#[must_use]
pub fn export_volume<T: VoxelTrait>(
    chunks: &BTreeMap<ChunkKey, VoxTree<T>>,
    interner: &VoxInterner<T>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());

    for (key, tree) in chunks {
        out.extend_from_slice(&key.x.to_le_bytes());
        out.extend_from_slice(&key.y.to_le_bytes());
        out.extend_from_slice(&key.z.to_le_bytes());
        out.push(tree.max_depth(Lod::new(0)).max());
        write_node(&mut out, interner, &tree.get_root_id()).expect("writing to Vec cannot fail");
    }

    out
}
