use std::fmt::{Debug, Display};
use std::hash::Hash;

pub trait ByteConversion: Sized {
    type ByteArray: AsRef<[u8]> + AsMut<[u8]> + Default;

    fn to_be_bytes(&self) -> Self::ByteArray;
    fn to_le_bytes(&self) -> Self::ByteArray;
    fn from_be_bytes(bytes: Self::ByteArray) -> Self;
    fn from_le_bytes(bytes: Self::ByteArray) -> Self;

    fn read_from_be<R: std::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        let mut bytes = Self::ByteArray::default();
        reader.read_exact(bytes.as_mut())?;
        Ok(Self::from_be_bytes(bytes))
    }

    fn read_from_le<R: std::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        let mut bytes = Self::ByteArray::default();
        reader.read_exact(bytes.as_mut())?;
        Ok(Self::from_le_bytes(bytes))
    }

    fn write_as_be<W: std::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        writer.write_all(self.to_be_bytes().as_ref())
    }

    fn write_as_le<W: std::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        writer.write_all(self.to_le_bytes().as_ref())
    }
}

pub trait VoxelTrait:
    Default + Copy + Clone + Hash + PartialEq + Eq + PartialOrd + Ord + Display + Debug + ByteConversion
{
    #[inline(always)]
    fn average(children: &[Self]) -> Self {
        calc_average(children)
    }

    fn material_id(&self) -> usize;
}

macro_rules! impl_byte_conversion {
    ($($t:ty),+) => {
        $(
            impl ByteConversion for $t {
                type ByteArray = [u8; std::mem::size_of::<Self>()];

                #[inline(always)]
                fn to_be_bytes(&self) -> Self::ByteArray {
                    <$t>::to_be_bytes(*self)
                }

                #[inline(always)]
                fn to_le_bytes(&self) -> Self::ByteArray {
                    <$t>::to_le_bytes(*self)
                }

                #[inline(always)]
                fn from_be_bytes(bytes: Self::ByteArray) -> Self {
                    <$t>::from_be_bytes(bytes)
                }

                #[inline(always)]
                fn from_le_bytes(bytes: Self::ByteArray) -> Self {
                    <$t>::from_le_bytes(bytes)
                }
            }
        )+
    };
}

impl_byte_conversion!(u8, i8, u16, i16, u32, i32, u64, i64);

macro_rules! impl_voxel_trait_for_numerics {
    ($($t:ty),+) => {
        $(
            impl VoxelTrait for $t {
                #[inline(always)]
                fn material_id(&self) -> usize {
                    *self as usize
                }
            }
        )+
    };
}

impl_voxel_trait_for_numerics!(u8, i8, u16, i16, u32, i32, u64, i64);

/// 形状阈值：8 个子节点里至少这么多个实心，LOD 父节点才算实心（其余算空气）。
///
/// - 4（默认）= 过半，平局算实心。地表高度的期望不偏移，平地 / 缓坡最稳。
/// - 调小（2..=3）：山尖、细山脊更容易保留，整体地形略微"变胖"。
/// - 调大（5..=6）：地形略微"变瘦"，悬挑、小凸起更容易被抹平。
///
/// 只影响 LOD > 0 的显示，LOD0 直接读体素，不经过这里。
/// 改动后已缓存 / 已存档的父节点代表值需要重建。
pub const LOD_MIN_SOLID_CHILDREN: usize = 4;

/// LOD 父节点的代表值（8 个子节点，下标 = x | y<<1 | z<<2）。
///
/// 等价于 [`calc_average_with`] 取 [`LOD_MIN_SOLID_CHILDREN`]。
#[inline(always)]
pub fn calc_average<T>(children: &[T]) -> T
where
    T: VoxelTrait,
{
    calc_average_with(children, LOD_MIN_SOLID_CHILDREN)
}

/// 把「形状」和「材质」分开决定的 LOD 代表值。
///
/// 1. **形状**：实心子节点数 `< min_solid` 则整个父节点是空气（`min_solid`
///    会被夹到 `1..=children.len()`）。空气不参与材质投票，所以多材质地形不会
///    因为"空气票最多"而被挖出洞。
/// 2. **材质按列取顶**：8 个子节点按 (x, z) 分成 4 列，每列取最上面那个实心
///    子节点的值（上层实心用上层，否则用下层）。节点值本身递归地代表它那一
///    块的表层材质，所以逐层往上，取到的就是每一列真正的最上层材质。
/// 3. 4 列的材质取众数；平局时优先"来自上层"的材质（更靠近地表），再平局按
///    列下标，结果完全确定（节点值要进 DAG，必须是子节点的纯函数）。
///
/// 非 8 个子节点的输入（只有测试会这样）退化为"实心里的众数"。
#[inline(always)]
pub fn calc_average_with<T>(children: &[T], min_solid: usize) -> T
where
    T: VoxelTrait,
{
    let default = T::default();
    let solid = children.iter().filter(|c| **c != default).count();
    if solid == 0 || solid < min_solid.clamp(1, children.len().max(1)) {
        return default;
    }

    // (材质, 票数, 其中来自上层的票数)，最多 4 列。
    let mut tally: [(T, usize, usize); 4] = [(default, 0, 0); 4];
    let mut unique = 0usize;
    let mut vote = |value: T, from_top: bool| {
        if value == default {
            return;
        }
        let top = usize::from(from_top);
        for entry in tally.iter_mut().take(unique) {
            if entry.0 == value {
                entry.1 += 1;
                entry.2 += top;
                return;
            }
        }
        if unique < tally.len() {
            tally[unique] = (value, 1, top);
            unique += 1;
        }
    };

    if children.len() == 8 {
        for column in 0..4usize {
            let low = (column & 1) | ((column >> 1) << 2); // x | z<<2，y = 0
            let high = low | 2; // y = 1
            if children[high] != default {
                vote(children[high], true);
            } else {
                vote(children[low], false);
            }
        }
    } else {
        for &c in children {
            vote(c, false);
        }
    }

    let mut best = 0usize;
    for i in 1..unique {
        let (_, count, top) = tally[i];
        let (_, best_count, best_top) = tally[best];
        if count > best_count || (count == best_count && top > best_top) {
            best = i;
        }
    }
    if unique == 0 {
        default
    } else {
        tally[best].0
    }
}

#[cfg(test)]
mod average_tests {
    use super::{calc_average, calc_average_with, LOD_MIN_SOLID_CHILDREN};

    // 下标 = x | y<<1 | z<<2（y 位 = bit1，上层 = 2,3,6,7）。
    // 材质：0 = 空气，1 = 草，2 = 泥土，3 = 石头。

    #[test]
    fn surface_cell_shows_top_material() {
        // 下层石头 / 泥土，上层 2 草 + 2 空气：4 列里 2 列顶是草，2 列顶是
        // 下层（石头、泥土各 1）。草 2 票最多。
        let cells = [3u8, 2, 1, 0, 3, 2, 1, 0];
        assert_eq!(calc_average(&cells), 1);
    }

    #[test]
    fn column_top_ignores_what_is_buried() {
        // 每列上层都是草，下层全是石头：草 4 票，石头 0 票。
        let cells = [3u8, 3, 1, 1, 3, 3, 1, 1];
        assert_eq!(calc_average(&cells), 1);
    }

    #[test]
    fn mixed_materials_are_not_outvoted_by_air() {
        // 5/8 实心、3 空气：必须是实心（旧实现会返回空气）。
        assert_ne!(calc_average(&[0u8, 0, 0, 1, 1, 2, 2, 3]), 0);
    }

    #[test]
    fn mostly_air_is_air() {
        assert_eq!(calc_average(&[0u8, 0, 0, 0, 0, 0, 0, 3]), 0);
        assert_eq!(calc_average(&[0u8; 8]), 0);
    }

    #[test]
    fn tie_goes_to_solid() {
        assert_eq!(calc_average(&[2u8, 2, 2, 2, 0, 0, 0, 0]), 2);
    }

    #[test]
    fn peak_survives_with_lower_threshold() {
        let peak = [0u8, 0, 0, 0, 0, 0, 0, 3];
        assert_eq!(calc_average_with(&peak, LOD_MIN_SOLID_CHILDREN), 0);
        assert_eq!(calc_average_with(&peak, 1), 3);
        // 阈值 0 / 超界会被夹住，不 panic。
        assert_eq!(calc_average_with(&peak, 0), 3);
        assert_eq!(calc_average_with(&peak, 99), 0);
    }

    #[test]
    fn overhang_uses_the_topmost_material() {
        // 一列：下层泥土，上层石头（悬挑）；其余三列只有下层泥土。
        // 石头 1 票，泥土 3 票 -> 泥土；但这一列自己的顶确实是石头。
        let cells = [2u8, 2, 3, 0, 2, 2, 0, 0];
        assert_eq!(calc_average(&cells), 2);
        // 两列石头在上、两列泥土在下：平局时优先来自上层的材质。
        let cells = [2u8, 2, 3, 3, 0, 0, 0, 0];
        assert_eq!(calc_average(&cells), 3);
    }

    #[test]
    fn result_is_deterministic_and_comes_from_children() {
        // 穷举 8 个子节点 空气 / 实心 的全部 256 种组合，材质 = 下标相关。
        for mask in 0u32..256 {
            let mut cells = [0u8; 8];
            for (i, cell) in cells.iter_mut().enumerate() {
                if mask >> i & 1 == 1 {
                    *cell = 1 + (i as u8 % 3);
                }
            }
            let solid = mask.count_ones() as usize;
            let got = calc_average(&cells);
            assert_eq!(got, calc_average(&cells), "mask {mask:#b}");
            if solid < LOD_MIN_SOLID_CHILDREN {
                assert_eq!(got, 0, "mask {mask:#b}");
            } else {
                assert!(cells.contains(&got) && got != 0, "mask {mask:#b}");
            }
        }
    }

    #[test]
    fn non_octant_input_falls_back_to_solid_mode() {
        assert_eq!(calc_average_with(&[2u8, 2, 3, 0, 0], 1), 2);
        assert_eq!(calc_average_with::<u8>(&[], 1), 0);
    }
}
