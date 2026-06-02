//! Priority queue backed by a channel and BinaryHeap.

use crossbeam::channel::{self, Receiver, SendError, Sender, TryRecvError};
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use super::*;

pub(crate) struct PriorityQueue<T>
where
    T: Send,
{
    sender: Sender<Weighted<T>>,
    pending_count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

pub struct PriorityQueueIter<T>
where
    T: Send,
{
    stop: Arc<AtomicBool>,
    receiver: Receiver<Weighted<T>>,
    receive_buffer: BinaryHeap<Weighted<T>>,
    pending_count: Arc<AtomicUsize>,
}

/// Bounded channel 容量：提供充足的缓冲避免生产者阻塞。
/// 设为 524288 (512K)，百万级条目扫描下也不会成为瓶颈。
/// 每个条目仅持有指针/小结构，524288 × ~64B ≈ 32MB，内存开销可控。
const CHANNEL_CAPACITY: usize = 524288;

/// BinaryHeap 初始容量：256 是经验值，平衡初始内存占用和重新分配次数。
/// 普通目录子项数通常 < 256，避免首次扩展。
const INITIAL_HEAP_CAPACITY: usize = 256;

pub(crate) fn new_priority_queue<T>(
    stop: Arc<AtomicBool>,
) -> (PriorityQueue<T>, PriorityQueueIter<T>)
where
    T: Send,
{
    let pending_count = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = channel::bounded(CHANNEL_CAPACITY);
    (
        PriorityQueue {
            sender,
            pending_count: pending_count.clone(),
            stop: stop.clone(),
        },
        PriorityQueueIter {
            receiver,
            receive_buffer: BinaryHeap::with_capacity(INITIAL_HEAP_CAPACITY),
            pending_count,
            stop,
        },
    )
}

impl<T> PriorityQueue<T>
where
    T: Send,
{
    /// 阻塞 push：使用 send 确保子目录调度不丢失。
    /// channel 容量已设为 524288，实际场景中几乎不会阻塞。
    /// 之前用 try_send 在 channel 满时丢弃调度，导致目录分支被跳过、并行度下降。
    pub fn push(&self, weighted: Weighted<T>) -> Result<(), SendError<Weighted<T>>> {
        let result = self.sender.send(weighted);
        if result.is_ok() {
            self.pending_count.fetch_add(1, AtomicOrdering::Release);
        }
        result
    }

    pub fn complete_item(&self) {
        // NOTE: fetch_sub 无 saturating 版本；若 count=0 会下溢为 usize::MAX。
        // 安全性依赖上游逻辑保证 complete_item 只在 push 成功后调用。
        // 参见 jwalk-meta-lbi / jwalk-meta-779 的 push-send 时序修复。
        self.pending_count.fetch_sub(1, AtomicOrdering::AcqRel);
    }
}

impl<T> Clone for PriorityQueue<T>
where
    T: Send,
{
    fn clone(&self) -> Self {
        PriorityQueue {
            sender: self.sender.clone(),
            pending_count: self.pending_count.clone(),
            stop: self.stop.clone(),
        }
    }
}

impl<T> PriorityQueueIter<T>
where
    T: Send,
{
    fn pending_count(&self) -> usize {
        // Acquire: 与 push 中的 Release 配对，确保看到完整的数据写入。
        self.pending_count.load(AtomicOrdering::Acquire)
    }

    fn is_stop(&self) -> bool {
        // Acquire: 确保看到 stop 标志的最新值，由 producer 端 Release 写入。
        self.stop.load(AtomicOrdering::Acquire)
    }

    fn try_next(&mut self) -> Result<Weighted<T>, TryRecvError> {
        // 自适应退避：热路径短等待（10μs），逐步增长到 1ms
        // 避免空闲时空转浪费 CPU，同时保持对新数据的快速响应
        let mut spin_count: u32 = 0;

        loop {
            if self.is_stop() {
                return Err(TryRecvError::Disconnected);
            }

            // 先非阻塞 drain 所有已就绪元素
            while let Ok(weighted) = self.receiver.try_recv() {
                self.receive_buffer.push(weighted);
                spin_count = 0; // 有数据时重置退避计数
            }

            if let Some(weighted) = self.receive_buffer.pop() {
                return Ok(weighted);
            } else if self.pending_count() == 0 {
                return Err(TryRecvError::Disconnected);
            }

            // 自适应等待：逐步增加超时，减少空转 CPU 浪费
            let timeout = match spin_count {
                0..=10 => std::time::Duration::from_micros(10),
                11..=50 => std::time::Duration::from_micros(100),
                _ => std::time::Duration::from_millis(1),
            };
            spin_count += 1;

            match self.receiver.recv_timeout(timeout) {
                Ok(weighted) => {
                    self.receive_buffer.push(weighted);
                }
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                    return Err(TryRecvError::Disconnected);
                }
            }
        }
    }
}

impl<T> Iterator for PriorityQueueIter<T>
where
    T: Send,
{
    type Item = Weighted<T>;

    fn next(&mut self) -> Option<Weighted<T>> {
        loop {
            match self.try_next() {
                Ok(next) => return Some(next),
                Err(TryRecvError::Empty) => {
                    std::thread::yield_now();
                }
                Err(TryRecvError::Disconnected) => return None,
            }
        }
    }
}
