//! 懒生成机制：生成逻辑在 game-core，引擎只缓存。

use std::collections::BTreeMap;

use voxel::store::{ChunkKey, VoxInterner, VoxTree};

/// 子块生成器：由 game-core 实现或提供（定点噪声 + 高度函数等）。
pub trait ChunkGenerator {
    fn generate(&mut self, key: ChunkKey, interner: &mut VoxInterner<u8>) -> VoxTree<u8>;
}

impl<F> ChunkGenerator for F
where
    F: FnMut(ChunkKey, &mut VoxInterner<u8>) -> VoxTree<u8>,
{
    fn generate(&mut self, key: ChunkKey, interner: &mut VoxInterner<u8>) -> VoxTree<u8> {
        self(key, interner)
    }
}

/// ChunkKey 级懒生成缓存。用 BTreeMap 保证遍历顺序确定（L3）。
#[derive(Default)]
pub struct LazyChunks {
    cache: BTreeMap<ChunkKey, VoxTree<u8>>,
}

impl LazyChunks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    pub fn contains_key(&self, key: &ChunkKey) -> bool {
        self.cache.contains_key(key)
    }

    pub fn get(&self, key: &ChunkKey) -> Option<&VoxTree<u8>> {
        self.cache.get(key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &ChunkKey> {
        self.cache.keys()
    }

    pub fn into_inner(self) -> BTreeMap<ChunkKey, VoxTree<u8>> {
        self.cache
    }

    /// 命中缓存直接返回；未命中则调用 generator 生成并缓存。
    pub fn get_or_generate<G>(
        &mut self,
        key: ChunkKey,
        interner: &mut VoxInterner<u8>,
        generator: &mut G,
    ) -> &VoxTree<u8>
    where
        G: ChunkGenerator + ?Sized,
    {
        if !self.cache.contains_key(&key) {
            let tree = generator.generate(key, interner);
            self.cache.insert(key, tree);
        }
        self.cache.get(&key).expect("刚刚插入")
    }

    /// 只为缺失的 key 生成（不改变已有缓存），返回新生成的 key 数。
    pub fn pregenerate<G>(
        &mut self,
        keys: &[ChunkKey],
        interner: &mut VoxInterner<u8>,
        generator: &mut G,
    ) -> usize
    where
        G: ChunkGenerator + ?Sized,
    {
        let mut added = 0;
        for &key in keys {
            if !self.cache.contains_key(&key) {
                let tree = generator.generate(key, interner);
                self.cache.insert(key, tree);
                added += 1;
            }
        }
        added
    }
}
