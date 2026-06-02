use std::cmp::Ordering;

/// IndexPath 表示 DFS 遍历中的位置路径。
///
/// 每个元素是父目录中子目录的索引。
/// Ord 实现使用反向比较（用于 BinaryHeap max-heap 语义）。
#[derive(Clone, Debug)]
pub struct IndexPath {
    pub indices: Vec<usize>,
}

impl IndexPath {
    /// Create a new IndexPath from a vector of indices.
    pub fn new(indices: Vec<usize>) -> IndexPath {
        IndexPath { indices }
    }

    /// Return a new IndexPath with an additional index appended.
    /// Does not modify self (immutable).
    pub fn adding(&self, index: usize) -> IndexPath {
        let mut indices = self.indices.clone();
        indices.push(index);
        IndexPath::new(indices)
    }

    /// Append an index to this IndexPath in place (mutable).
    pub fn push(&mut self, index: usize) {
        self.indices.push(index);
    }

    /// Increment the last index by 1. Used for DFS sibling traversal.
    /// Panics in debug mode if indices is empty.
    pub fn increment_last(&mut self) {
        debug_assert!(!self.indices.is_empty(), "IndexPath::increment_last called on empty indices");
        if let Some(last) = self.indices.last_mut() {
            *last += 1;
        }
    }

    /// Remove and return the last index. Returns `None` if empty.
    pub fn pop(&mut self) -> Option<usize> {
        self.indices.pop()
    }

    /// Returns `true` if this IndexPath has no indices (DFS traversal complete).
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
}

impl PartialEq for IndexPath {
    fn eq(&self, o: &Self) -> bool {
        self.indices.eq(&o.indices)
    }
}

impl Eq for IndexPath {}

impl PartialOrd for IndexPath {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        o.indices.partial_cmp(&self.indices)
    }
}

impl Ord for IndexPath {
    fn cmp(&self, o: &Self) -> Ordering {
        o.indices.cmp(&self.indices)
    }
}
