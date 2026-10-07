//! 索引寻址的裸内存对象池（移植自 voxelis-memory，增加增长路径）。
//!
//! v1 改动：
//! - 新增 [`PoolAllocatorLite::grow`]：分配新块、按位拷贝旧内容、释放旧块；
//! - `allocate` 耗尽时使用明确文案，而非上游的 “Out of memory”；
//! - 仍然无条件 `Send + Sync`（可作 Bevy `Resource`）。
//!
//! 约定：池内只放 `Copy` 值（u8 / u16 / u32 / [u32; 8] / T: VoxelTrait），
//! 因此位拷贝与 `alloc_zeroed` 是安全且确定的。

use std::alloc::Layout;

/// 定长、索引寻址的对象池。
pub struct PoolAllocatorLite<T> {
    memory: *mut T,
    layout: Layout,
    capacity: usize,
    next: usize,
}

unsafe impl<T> Send for PoolAllocatorLite<T> {}
unsafe impl<T> Sync for PoolAllocatorLite<T> {}

impl<T> PoolAllocatorLite<T> {
    /// 单个元素的字节大小。
    #[must_use]
    #[inline(always)]
    pub const fn block_size() -> usize {
        std::mem::size_of::<T>()
    }

    /// 单个元素的对齐要求。
    #[must_use]
    #[inline(always)]
    pub const fn align() -> usize {
        std::mem::align_of::<T>()
    }

    /// 分配一个容量为 `capacity` 的零初始化池。
    ///
    /// # Panics
    ///
    /// `capacity` 必须大于 0 且小于 `u32::MAX`。
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "Capacity must be greater than 0");
        assert!(
            capacity < u32::MAX as usize,
            "Capacity must be less than u32::MAX"
        );

        let actual_size = Self::block_size() * capacity;
        let layout = Layout::from_size_align(actual_size, Self::align()).expect("Invalid layout");
        let memory = Self::alloc_zeroed(layout);

        Self {
            memory,
            layout,
            capacity,
            next: 0,
        }
    }

    fn alloc_zeroed(layout: Layout) -> *mut T {
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) as *mut T };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        ptr
    }

    /// 当前容量（元素个数）。
    #[must_use]
    #[inline(always)]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// 顺序分配游标（元素个数）。
    #[must_use]
    #[inline(always)]
    pub const fn used(&self) -> usize {
        self.next
    }

    /// 读取下标处的元素。
    ///
    /// # Panics
    ///
    /// 调试构建下越界会 panic。
    #[must_use]
    #[inline(always)]
    pub fn get(&self, index: u32) -> &T {
        debug_assert!(
            index < self.capacity as u32,
            "Block index out of bounds index: {index} capacity: {}",
            self.capacity
        );

        unsafe { &*self.memory.add(index as usize) }
    }

    /// 可变读取下标处的元素。
    #[must_use]
    #[inline(always)]
    pub fn get_mut(&mut self, index: u32) -> &mut T {
        debug_assert!(
            index < self.capacity as u32,
            "Block index out of bounds index: {index} capacity: {}",
            self.capacity
        );

        unsafe { &mut *self.memory.add(index as usize) }
    }

    /// 顺序分配一个元素并写入 `value`。
    ///
    /// `next_free` 给出可复用的空闲下标（由调用方的 free list 提供）。
    ///
    /// # Panics
    ///
    /// 池满时 panic；interner 的增长路径应优先使用 `get_next_index`，
    /// 它会在耗尽前扩容。
    pub fn allocate(&mut self, value: T, next_free: Option<u32>) -> u32 {
        let index = match next_free {
            Some(index) => index,
            None => {
                assert!(
                    self.next < self.capacity,
                    "PoolAllocatorLite capacity exhausted: {} >= {}",
                    self.next,
                    self.capacity
                );
                let index = self.next;
                self.next += 1;
                index as u32
            }
        };

        debug_assert!(
            index < self.capacity as u32,
            "Block index out of bounds index: {index} capacity: {}",
            self.capacity
        );

        unsafe { std::ptr::write(self.memory.add(index as usize), value) };
        index
    }

    /// 把下标处的元素标记为空闲（本池不做 Drop，调用方负责生命周期）。
    pub fn deallocate(&mut self, index: u32) {
        // 池只存 Copy 值，无需 drop_in_place；仅做越界检查。
        let _ = index;
        debug_assert!(
            index < self.capacity as u32,
            "Block index out of bounds index: {index} capacity: {}",
            self.capacity
        );
    }

    /// 把池扩容到 `new_capacity`：分配新块、位拷贝旧内容、释放旧块。
    ///
    /// 下标保持稳定（`[0, new_capacity)` 中旧元素位置不变），因此
    /// `BlockId` / patterns 无需重映射。
    ///
    /// # Panics
    ///
    /// `new_capacity` 必须严格大于当前容量且小于 `u32::MAX`。
    pub fn grow(&mut self, new_capacity: usize) {
        assert!(
            new_capacity > self.capacity,
            "grow must increase capacity: {new_capacity} <= {}",
            self.capacity
        );
        assert!(
            new_capacity < u32::MAX as usize,
            "Capacity must be less than u32::MAX"
        );

        let new_size = Self::block_size() * new_capacity;
        let new_layout = Layout::from_size_align(new_size, Self::align()).expect("Invalid layout");
        let new_memory = Self::alloc_zeroed(new_layout);

        // 位拷贝全部旧槽位（含未使用槽，均为 0）。旧池只含 Copy 值。
        unsafe {
            std::ptr::copy_nonoverlapping(self.memory as *const T, new_memory, self.capacity);
            std::alloc::dealloc(self.memory as *mut u8, self.layout);
        }

        self.memory = new_memory;
        self.layout = new_layout;
        self.capacity = new_capacity;
    }
}

impl<T> Drop for PoolAllocatorLite<T> {
    fn drop(&mut self) {
        unsafe {
            std::alloc::dealloc(self.memory as *mut u8, self.layout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_allocator_basic() {
        let mut allocator: PoolAllocatorLite<u32> = PoolAllocatorLite::new(4);
        let id1 = allocator.allocate(42, None);
        let id2 = allocator.allocate(24, None);
        assert_ne!(id1, id2);
        assert_eq!(*allocator.get(id1), 42);
        assert_eq!(*allocator.get(id2), 24);
    }

    #[test]
    fn pool_allocator_grow_preserves_items() {
        let mut allocator: PoolAllocatorLite<u32> = PoolAllocatorLite::new(2);
        let a = allocator.allocate(7, None);
        let b = allocator.allocate(9, None);
        allocator.grow(8);
        assert_eq!(allocator.capacity(), 8);
        assert_eq!(*allocator.get(a), 7);
        assert_eq!(*allocator.get(b), 9);
        let c = allocator.allocate(11, None);
        assert_eq!(*allocator.get(c), 11);
    }

    #[test]
    #[should_panic(expected = "capacity exhausted")]
    fn pool_allocator_exhaustion_is_explicit() {
        let mut allocator: PoolAllocatorLite<u8> = PoolAllocatorLite::new(1);
        let _ = allocator.allocate(1, None);
        let _ = allocator.allocate(2, None);
    }

    #[test]
    fn pool_allocator_alignment() {
        #[repr(align(16))]
        struct Aligned16;
        let _allocator: PoolAllocatorLite<Aligned16> = PoolAllocatorLite::new(4);
        assert_eq!(PoolAllocatorLite::<Aligned16>::align(), 16);
    }
}
