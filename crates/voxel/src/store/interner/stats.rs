//! interner 内存 / 命中统计（仅在 `memory_stats` feature 下启用）。

/// `VoxInterner` 的运行期统计快照。
#[derive(Debug, Default, Copy, Clone)]
pub struct InternerStats {
    /// 调用方请求的预算（字节）。
    pub requested_budget: usize,
    /// 实际按整节点取整后的预算（字节）。
    pub actual_budget: usize,
    /// 单节点 pool 列口径（见 `VoxInterner::node_size`）。
    pub node_size: usize,
    /// 节点池容量（节点数）。
    pub nodes_capacity: usize,
    /// 累计分配次数（含复用）。
    pub total_allocations: usize,
    /// 累计释放次数。
    pub total_deallocations: usize,
    /// 曾分配过的节点下标总数。
    pub allocated_nodes: usize,
    /// 通过 free list 复用的次数。
    pub recycled_nodes: usize,
    /// 当前存活节点数（含空分支哨兵）。
    pub alive_nodes: usize,
    /// patterns 表中的候选条目数。
    pub patterns: usize,
    /// 结构命中总次数。
    pub total_cache_hits: usize,
    /// 结构未命中总次数。
    pub total_cache_misses: usize,
    /// 分支命中次数。
    pub branch_cache_hits: usize,
    /// 分支未命中次数。
    pub branch_cache_misses: usize,
    /// 叶命中次数。
    pub leaf_cache_hits: usize,
    /// 叶未命中次数。
    pub leaf_cache_misses: usize,
    /// 批处理中塌陷的分支数。
    pub collapsed_branches: usize,
    /// 当前叶节点数。
    pub leaf_nodes: usize,
    /// 当前分支节点数。
    pub branch_nodes: usize,
    /// 历史最大存活节点数。
    pub max_alive_nodes: usize,
    /// 曾使用的最大节点下标。
    pub max_node_id: usize,
    /// 分支最大引用计数。
    pub max_branch_ref_count: usize,
    /// 叶最大引用计数。
    pub max_leaf_ref_count: usize,
    /// 观察到的最大 generation。
    pub max_generation: usize,
    /// generation 回绕次数。
    pub generations_overflows: usize,
}
