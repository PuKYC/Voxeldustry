//! An interner-free copy of a wrapped mesh block.
//!
//! crate::mesh::extract_block takes an immutable VoxInterner but still has to
//! mesh a 2^lod-wide group of base subchunks.  [`LocalTree`] materialises that
//! group as a plain tree so the occupancy extractors can run against it through
//! the same [`NodeAccess`] abstraction they use for the real interner.

use std::collections::BTreeMap;

use glam::UVec3;

use crate::store::{BlockId, ChunkKey, VoxInterner, VoxTree, VoxelTrait};

/// Read-only octree access used by the occupancy extractors.
///
/// Implemented for VoxInterner (via InternerAccess) and for the temporary
/// LocalTree wrapper built by crate::mesh::extract_block.
pub(crate) trait NodeAccess<T: VoxelTrait> {
    /// Node handle.
    type Id: Copy + Eq;

    /// Returns true when the handle is the empty sentinel.
    fn is_empty(&self, id: Self::Id) -> bool;

    /// Returns true when the handle is a leaf node.
    fn is_leaf(&self, id: Self::Id) -> bool;

    /// Returns the (averaged) value stored at a node.
    fn value(&self, id: Self::Id) -> T;

    /// Returns the child at index (only valid for branches).
    fn child(&self, id: Self::Id, index: usize) -> Self::Id;
}

/// NodeAccess adapter over a VoxInterner.
pub(crate) struct InternerAccess<'a, T: VoxelTrait>(pub &'a VoxInterner<T>);

impl<T: VoxelTrait> NodeAccess<T> for InternerAccess<'_, T> {
    /// 节点句柄：interner 里的 [`BlockId`]。
    type Id = BlockId;

    /// 空 sentinel 在 interner 里由 [`BlockId::is_empty`] 判定。
    #[inline(always)]
    fn is_empty(&self, id: BlockId) -> bool {
        id.is_empty()
    }

    /// 叶子判定直接透传 BlockId::is_leaf。
    #[inline(always)]
    fn is_leaf(&self, id: BlockId) -> bool {
        id.is_leaf()
    }

    /// 读取节点上缓存的（LOD 平均）值。
    #[inline(always)]
    fn value(&self, id: BlockId) -> T {
        *self.0.get_value(&id)
    }

    /// 读取分支的第 index 个子节点（index 是 Morton 序 0..8）。
    #[inline(always)]
    fn child(&self, id: BlockId, index: usize) -> BlockId {
        self.0.get_child_id(&id, index)
    }
}

/// Empty sentinel of LocalTree.
pub(crate) const LOCAL_EMPTY: u32 = u32::MAX;

/// 本地树节点：叶节点只存值，分支同时存 LOD 平均值与 8 个子节点下标。
///
/// 子节点下标可指向 [`LOCAL_EMPTY`]，表示该象限为空。
#[derive(Clone, Copy)]
pub(crate) enum LocalNode {
    Leaf(u8),
    /// A branch carries the same LOD averaged value the interner stores, so a
    /// traversal cut at max_depth reads the identical value from both trees.
    Branch {
        value: u8,
        children: [u32; 8],
    },
}

/// A temporary, interned-free copy of a wrapped mesh block.
///
/// Used by crate::mesh::extract_block, whose signature takes an immutable
/// VoxInterner but which still has to mesh a 2^lod-wide group of base
/// subchunks.  The DAG is expanded into a tree; every node below a base
/// subchunk root is copied verbatim from the interner.
pub(crate) struct LocalTree {
    nodes: Vec<LocalNode>,
    root: u32,
}

impl LocalTree {
    /// The empty local tree.
    #[must_use]
    pub(crate) fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            root: LOCAL_EMPTY,
        }
    }

    /// Root handle.
    #[must_use]
    pub(crate) fn root(&self) -> u32 {
        self.root
    }

    /// Number of materialised nodes (test/diagnostics helper).
    #[must_use]
    pub(crate) fn node_count(&self) -> usize {
        self.nodes.len()
    }
}

impl NodeAccess<u8> for LocalTree {
    /// 节点句柄：nodes 向量的下标，LOCAL_EMPTY 表示空。
    type Id = u32;

    /// LOCAL_EMPTY 是唯一被当作「空」的下标。
    #[inline(always)]
    fn is_empty(&self, id: u32) -> bool {
        id == LOCAL_EMPTY
    }

    /// 只看节点本身是否为叶，不递归。
    #[inline(always)]
    fn is_leaf(&self, id: u32) -> bool {
        matches!(self.nodes[id as usize], LocalNode::Leaf(_))
    }

    /// 叶与分支都返回构建时算好的 LOD 平均值。
    #[inline(always)]
    fn value(&self, id: u32) -> u8 {
        match self.nodes[id as usize] {
            LocalNode::Leaf(value) => value,
            LocalNode::Branch { value, .. } => value,
        }
    }

    /// 分支返回第 index 个子节点；叶节点没有子节点，返回 LOCAL_EMPTY。
    #[inline(always)]
    fn child(&self, id: u32, index: usize) -> u32 {
        match self.nodes[id as usize] {
            LocalNode::Branch { children, .. } => children[index],
            LocalNode::Leaf(_) => LOCAL_EMPTY,
        }
    }
}

/// Copies an interner subtree into out, returning the local root handle.
///
/// Shared subtrees are expanded (the v1 blocks are small enough that this is
/// cheaper than an interner round-trip).
fn copy_subtree(interner: &VoxInterner<u8>, id: BlockId, out: &mut Vec<LocalNode>) -> u32 {
    if id.is_empty() {
        return LOCAL_EMPTY;
    }
    if id.is_leaf() {
        out.push(LocalNode::Leaf(*interner.get_value(&id)));
        return (out.len() - 1) as u32;
    }
    let value = *interner.get_value(&id);
    let mut children = [LOCAL_EMPTY; 8];
    for (index, slot) in children.iter_mut().enumerate() {
        let child = interner.get_child_id(&id, index);
        *slot = copy_subtree(interner, child, out);
    }
    out.push(LocalNode::Branch { value, children });
    (out.len() - 1) as u32
}

/// Builds the temporary wrapped tree for a 2^lod x 2^lod x 2^lod group of base
/// subchunks.
///
/// The wrapper root sits lod levels above the base subchunk roots, exactly
/// like crate::mesh::wrap_block but without touching the interner.
#[must_use]
pub(crate) fn build_local_block_tree(
    chunks: &BTreeMap<ChunkKey, VoxTree<u8>>,
    interner: &VoxInterner<u8>,
    origin: ChunkKey,
    lod: u8,
) -> LocalTree {
    let mut nodes = Vec::new();
    let root = build_local_node(
        chunks,
        interner,
        origin,
        UVec3::ZERO,
        lod as u32,
        &mut nodes,
    );
    LocalTree { nodes, root }
}

/// 递归构建一层本地节点：level == 0 时复制对应的基础子块。
///
/// `local` 是当前节点在 wrap 根下的最小角偏移（以基础子块为单位），
/// `level` 是距离 wrap 根的层数；返回新节点的下标，全空时返回
/// LOCAL_EMPTY，从而让空象限不占节点。
fn build_local_node(
    chunks: &BTreeMap<ChunkKey, VoxTree<u8>>,
    interner: &VoxInterner<u8>,
    origin: ChunkKey,
    local: UVec3,
    level: u32,
    nodes: &mut Vec<LocalNode>,
) -> u32 {
    if level == 0 {
        let key = ChunkKey {
            x: origin.x + local.x as i32,
            y: origin.y + local.y as i32,
            z: origin.z + local.z as i32,
        };
        match chunks.get(&key) {
            Some(tree) => {
                let root = tree.get_root_id();
                if root.is_empty() {
                    LOCAL_EMPTY
                } else {
                    copy_subtree(interner, root, nodes)
                }
            }
            None => LOCAL_EMPTY,
        }
    } else {
        let half = 1u32 << (level - 1);
        let mut children = [LOCAL_EMPTY; 8];
        for (index, slot) in children.iter_mut().enumerate() {
            let dx = (index & 1) as u32;
            let dy = ((index >> 1) & 1) as u32;
            let dz = ((index >> 2) & 1) as u32;
            *slot = build_local_node(
                chunks,
                interner,
                origin,
                UVec3::new(
                    local.x + dx * half,
                    local.y + dy * half,
                    local.z + dz * half,
                ),
                level - 1,
                nodes,
            );
        }
        if children.iter().all(|&child| child == LOCAL_EMPTY) {
            return LOCAL_EMPTY;
        }
        let mut values = [0u8; 8];
        for (index, &child) in children.iter().enumerate() {
            if child != LOCAL_EMPTY {
                values[index] = match nodes[child as usize] {
                    LocalNode::Leaf(value) => value,
                    LocalNode::Branch { value, .. } => value,
                };
            }
        }
        let value = <u8 as VoxelTrait>::average(&values);
        nodes.push(LocalNode::Branch { value, children });
        (nodes.len() - 1) as u32
    }
}
