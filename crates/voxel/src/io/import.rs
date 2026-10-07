//! 确定性导入：严格校验 magic / version / depth / 截断与尾部多余字节。
//!
//! 通过 `get_or_create_leaf` / `get_or_create_branch` 重建结构，因此
//! 导入结果与导出前的结构一致，且共享子树自动去重。

use std::collections::BTreeMap;
use std::io::{self, Read};

use crate::store::{
    BlockId, ByteConversion, ChunkKey, MaxDepth, VoxInterner, VoxTree, VoxelTrait, EMPTY_CHILD,
    MAX_ALLOWED_DEPTH, MAX_CHILDREN,
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
            for i in 0..MAX_CHILDREN {
                if mask & (1 << i) != 0 {
                    children[i] = read_node(reader, interner)?;
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
