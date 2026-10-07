//! 岛屿簿记：BiomeId / Biome / Island / IslandRegistry。
//!
//! **确定性（L3）**：注册表用 BTreeMap，迭代顺序只由 IslandId 升序决定，
//! 绝不让 HashMap 迭代顺序影响命令序 / 字节序。
//!
//! 岛 / 船的变换由 Godot 绘制节点携带：这里只记录逻辑侧的
//! 漂移量 drift（定点），不参与网格重建。

use bevy::prelude::*;
use game_engine::math::Vec3F;
use std::collections::BTreeMap;

/// BiomeId 是游戏数值（表在 static_data）；世界语义从这里统一出口。
pub use crate::static_data::voxel::BiomeId;

/// 岛屿稳定 id（独立于 Bevy Entity 的长期业务 id）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct IslandId(pub u64);

/// 生物群系组件：挂在岛屿实体上，供生成 / 表现查询。
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Biome(pub BiomeId);

impl Biome {
    pub const fn new(id: BiomeId) -> Self {
        Self(id)
    }

    pub const fn id(&self) -> BiomeId {
        self.0
    }

    /// 查静态生物群系定义（表在 static_data）。
    pub fn def(&self) -> Option<&'static crate::static_data::voxel::BiomeDef> {
        crate::static_data::voxel::biome_def(self.0)
    }
}

/// 岛屿簿记状态。drift 是逻辑侧定点漂移量，anchored 表示是否锚定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Island {
    /// 相对生成原点的漂移（定点）。
    pub drift: Vec3F,
    /// 是否锚定（锚定岛的 drift 恒为 0，由玩法系统保证；v1 只存字段）。
    pub anchored: bool,
    /// 生物群系。
    pub biome: BiomeId,
}

impl Island {
    pub const fn new(drift: Vec3F, anchored: bool, biome: BiomeId) -> Self {
        Self {
            drift,
            anchored,
            biome,
        }
    }
}

impl Default for Island {
    fn default() -> Self {
        Self {
            drift: Vec3F::ZERO,
            anchored: false,
            biome: crate::static_data::voxel::DEFAULT_BIOME,
        }
    }
}

/// 岛屿注册表：BTreeMap 保证确定性有序迭代。
#[derive(Resource, Default, Debug, Clone)]
pub struct IslandRegistry(BTreeMap<IslandId, Island>);

impl IslandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 插入 / 覆盖一个岛屿，返回被覆盖的旧值。
    pub fn insert(&mut self, id: IslandId, island: Island) -> Option<Island> {
        self.0.insert(id, island)
    }

    pub fn get(&self, id: IslandId) -> Option<&Island> {
        self.0.get(&id)
    }

    pub fn get_mut(&mut self, id: IslandId) -> Option<&mut Island> {
        self.0.get_mut(&id)
    }

    pub fn remove(&mut self, id: IslandId) -> Option<Island> {
        self.0.remove(&id)
    }

    pub fn contains(&self, id: IslandId) -> bool {
        self.0.contains_key(&id)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// 按 IslandId 升序迭代（确定性）。
    pub fn iter(&self) -> impl Iterator<Item = (IslandId, &Island)> + '_ {
        self.0.iter().map(|(id, island)| (*id, island))
    }

    /// 按 IslandId 升序返回所有 id。
    pub fn ids(&self) -> impl Iterator<Item = IslandId> + '_ {
        self.0.keys().copied()
    }

    /// 按 IslandId 升序返回所有岛屿。
    pub fn islands(&self) -> impl Iterator<Item = &Island> + '_ {
        self.0.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_iteration_is_stably_sorted() {
        let mut registry = IslandRegistry::new();
        for id in [5u64, 1, 3, 2, 4] {
            registry.insert(IslandId(id), Island::default());
        }

        let ids: Vec<u64> = registry.ids().map(|id| id.0).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);

        // 重复构建（不同插入顺序）-> 相同迭代序。
        let mut other = IslandRegistry::new();
        for id in [4u64, 2, 5, 1, 3] {
            other.insert(IslandId(id), Island::default());
        }
        let other_ids: Vec<u64> = other.ids().map(|id| id.0).collect();
        assert_eq!(ids, other_ids);
    }

    #[test]
    fn registry_remove_and_contains() {
        let mut registry = IslandRegistry::new();
        registry.insert(IslandId(7), Island::new(Vec3F::ZERO, true, BiomeId(2)));
        assert!(registry.contains(IslandId(7)));
        assert_eq!(registry.get(IslandId(7)).unwrap().biome, BiomeId(2));
        assert!(registry.remove(IslandId(7)).is_some());
        assert!(registry.is_empty());
    }
}
