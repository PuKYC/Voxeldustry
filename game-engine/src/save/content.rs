//! 区块内容的导入 / 导出原语。
//!
//! 存档里的 `ChunkRecord::content` 是**整块最终内容**的树字节（`Some`），
//! 格式就是 `crates/voxel/src/io` 已有的 MAGIC/VERSION 单树格式，这里不另造容器。
//!
//! 职责边界：
//!
//! - 导出只读 volume / interner，不做任何写操作；
//! - 导入先用 `import_tree_into` 解码到临时 interner，**解码成功后才**替换块；
//!   坏字节时 volume 与目标 interner 都保持原样（`import_tree_into` 的原子性保证）；
//! - 被顶掉的旧树通过返回值交给调用方：`VoxTree` 没有 `Drop`，丢弃即泄漏，
//!   必须显式 `interner.dec_ref_recursive(&old.get_root_id())`。
//!
//! 判定路径上不出现 HashMap / f32 / DefaultHasher。

use ::voxel::io::{export_tree, import_tree_into};
use ::voxel::store::{ChunkKey, VoxInterner, VoxTree};

use super::{SaveError, SaveErrorKind};
use crate::voxel::VoxVolume;

/// 导出单个块的内容字节。
///
/// - 块不存在返回 `None`；
/// - 存在则用 voxel io 的 `export_tree` 导出（MAGIC/VERSION + 前序节点记录），
///   同一棵树 + 同一 interner 的输出逐字节确定。
#[must_use]
pub fn export_chunk(
    volume: &VoxVolume,
    key: ChunkKey,
    interner: &VoxInterner<u8>,
) -> Option<Vec<u8>> {
    let tree = volume.get_chunk(&key)?;
    Some(export_tree(tree, interner))
}

/// 用块内容字节替换 `key` 处的块，返回被顶掉的旧树。
///
/// - 解码失败返回 `SaveErrorKind::Payload { what: "chunk content decode", ... }`，
///   且 volume / interner 一个字节都不动；
/// - 解码成功才 `take_chunk_raw` + `insert_chunk`：块原本不存在时返回 `Ok(None)`；
/// - 返回的旧树持有根节点的 1 个引用，**调用方负责**显式
///   `interner.dec_ref_recursive(&old.get_root_id())`（`VoxTree` 没有 `Drop`）。
///
/// 注意不能先 `take_chunk_raw` 再解码：解码失败时旧块必须留在原位。
pub fn install_chunk(
    volume: &mut VoxVolume,
    key: ChunkKey,
    bytes: &[u8],
    interner: &mut VoxInterner<u8>,
) -> Result<Option<VoxTree<u8>>, SaveError> {
    let tree = import_tree_into(bytes, interner).map_err(|err| {
        SaveError::new(
            "chunk.content",
            0,
            SaveErrorKind::Payload {
                what: "chunk content decode",
                detail: err.to_string(),
            },
        )
    })?;

    // 解码已成功，才允许替换。先取旧树再插入，避免 insert_chunk 的覆盖 debug_assert。
    let old = volume.take_chunk_raw(&key);
    volume.insert_chunk(key, tree);
    Ok(old)
}

#[cfg(test)]
mod tests {
    use super::*;

    use ::voxel::store::{MaxDepth, VoxOpsBulkWrite, VoxOpsWrite, CHUNK_DEPTH};
    use bevy::math::IVec3;

    use crate::math::FixedPoint;
    use crate::voxel::{chunk_content_hash, dec_ref_recursive};

    /// 单元测试用的节点池预算（4 MiB）。
    const TEST_BUDGET: usize = 4 * 1024 * 1024;

    fn test_interner() -> VoxInterner<u8> {
        VoxInterner::with_memory_budget(TEST_BUDGET)
    }

    fn test_volume() -> VoxVolume {
        VoxVolume::new(FixedPoint::from_num(1))
    }

    /// 按 key 生成一棵非空树；salt 让不同测试来源的树互不共享节点，
    /// 这样释放旧树时能观察到 alive_nodes 下降。
    fn build_tree(key: ChunkKey, salt: u8, interner: &mut VoxInterner<u8>) -> VoxTree<u8> {
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
        let base = (((key.x + key.y + key.z).rem_euclid(6)) as u8) + 1 + salt;
        tree.fill(interner, base);
        let _ = tree.set(interner, IVec3::new(0, 0, 0), base + 1);
        tree
    }

    #[test]
    fn export_then_install_roundtrips_content_hash() {
        let key = ChunkKey::new(2, -1, 3);

        let mut src_interner = test_interner();
        let mut src_volume = test_volume();
        src_volume.insert_chunk(key, build_tree(key, 0, &mut src_interner));
        let bytes = export_chunk(&src_volume, key, &src_interner).expect("块存在必须导出");
        let expected = chunk_content_hash(&src_interner, src_volume.get_chunk(&key).unwrap());

        // 全新 volume + 全新 interner：内容哈希必须与原块一致。
        let mut dst_interner = test_interner();
        let mut dst_volume = test_volume();
        let old = install_chunk(&mut dst_volume, key, &bytes, &mut dst_interner)
            .expect("合法字节必须导入成功");
        assert!(old.is_none(), "空 volume 导入不应有旧树");
        assert_eq!(
            chunk_content_hash(&dst_interner, dst_volume.get_chunk(&key).unwrap()),
            expected
        );
        // 导出格式确定：同一内容再导出逐字节相同。
        assert_eq!(
            export_chunk(&dst_volume, key, &dst_interner).as_deref(),
            Some(bytes.as_slice())
        );
    }

    #[test]
    fn install_replaces_existing_chunk_and_returns_old_tree() {
        let key = ChunkKey::new(1, 2, -3);

        // 目标 volume 先放一棵与来源不同的树。
        let mut interner = test_interner();
        let mut volume = test_volume();
        let old_tree = build_tree(key, 8, &mut interner);
        let old_hash = chunk_content_hash(&interner, &old_tree);
        volume.insert_chunk(key, old_tree);

        // 来源树（salt = 0，节点与旧树不共享）。
        let mut src_interner = test_interner();
        let mut src_volume = test_volume();
        let new_tree = build_tree(key, 0, &mut src_interner);
        let new_hash = chunk_content_hash(&src_interner, &new_tree);
        src_volume.insert_chunk(key, new_tree);
        let bytes = export_chunk(&src_volume, key, &src_interner).expect("来源块存在");

        let alive_before = interner.alive_nodes();
        let old = install_chunk(&mut volume, key, &bytes, &mut interner)
            .expect("合法字节必须导入成功")
            .expect("覆盖已有块必须返回旧树");

        assert_eq!(chunk_content_hash(&interner, &old), old_hash);
        assert_eq!(
            chunk_content_hash(&interner, volume.get_chunk(&key).unwrap()),
            new_hash
        );

        // 旧树必须显式归还引用；节点池应随之收缩。
        let alive_after_install = interner.alive_nodes();
        assert!(alive_after_install > alive_before);
        dec_ref_recursive(&mut interner, &old.get_root_id());
        assert!(
            interner.alive_nodes() < alive_after_install,
            "旧树释放后 alive_nodes 必须下降"
        );
    }

    #[test]
    fn install_rejects_bad_bytes_without_touching_volume_or_interner() {
        let key = ChunkKey::new(0, 0, 0);

        let mut interner = test_interner();
        let mut volume = test_volume();
        volume.insert_chunk(key, build_tree(key, 3, &mut interner));
        let before_bytes = export_chunk(&volume, key, &interner).expect("块存在");
        let before_hash = chunk_content_hash(&interner, volume.get_chunk(&key).unwrap());
        let before_alive = interner.alive_nodes();

        // 截断最后一个字节：结构不完整，解码必须失败。
        let bad = &before_bytes[..before_bytes.len() - 1];
        let err = match install_chunk(&mut volume, key, bad, &mut interner) {
            Ok(_) => panic!("坏字节必须报错"),
            Err(err) => err,
        };
        match err.kind() {
            SaveErrorKind::Payload { what, detail } => {
                assert_eq!(*what, "chunk content decode");
                assert!(!detail.is_empty(), "detail 必须是 io 错误文本");
            }
            other => panic!("期望 Payload，实际 {other:?}"),
        }

        // 失败原子性：块内容与 interner 节点数都不变。
        assert_eq!(
            chunk_content_hash(&interner, volume.get_chunk(&key).unwrap()),
            before_hash
        );
        assert_eq!(interner.alive_nodes(), before_alive);
        assert_eq!(export_chunk(&volume, key, &interner), Some(before_bytes));
    }

    #[test]
    fn missing_chunk_exports_none_and_install_into_empty_volume_returns_none() {
        let key = ChunkKey::new(-4, 5, 0);

        let mut interner = test_interner();
        let mut volume = test_volume();
        assert!(
            export_chunk(&volume, key, &interner).is_none(),
            "不存在的块导出必须是 None"
        );

        // 用另一个 volume 造一份合法字节。
        let mut src_interner = test_interner();
        let mut src_volume = test_volume();
        src_volume.insert_chunk(key, build_tree(key, 0, &mut src_interner));
        let bytes = export_chunk(&src_volume, key, &src_interner).expect("来源块存在");
        let expected = chunk_content_hash(&src_interner, src_volume.get_chunk(&key).unwrap());

        let old =
            install_chunk(&mut volume, key, &bytes, &mut interner).expect("合法字节必须导入成功");
        assert!(old.is_none(), "空 volume 导入必须返回 Ok(None)");
        assert!(volume.contains_chunk(&key));
        assert_eq!(
            chunk_content_hash(&interner, volume.get_chunk(&key).unwrap()),
            expected
        );
    }
}
