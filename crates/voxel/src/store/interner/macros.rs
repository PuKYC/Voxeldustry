//! 保留上游宏名，但内部委托给可增长的 `VoxInterner::get_next_index`。
//!
//! 上游在容量用满时直接 `panic!("Out of memory")`；v1 改为自动倍增
//! 索引稳定，因此 BlockId / patterns 无需重映射。

/// 取得下一个节点索引；池满时按 `max_capacity` 自动倍增。
macro_rules! get_next_index_macro {
    ($self:expr) => {
        $self.get_next_index()
    };
}

pub(crate) use get_next_index_macro;
