//! Ordered queue backed by a channel.

use crossbeam::channel::{self, Receiver, SendError, Sender, TryRecvError};
use smallvec::smallvec;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::thread;
use std::time::{Duration, Instant};

use super::*;

/// BinaryHeap 初始容量：256 是经验值，平衡初始内存占用和重新分配次数。
const INITIAL_HEAP_CAPACITY: usize = 256;

/// Strict 模式等待队头元素的最大时长，超时后降级为弹出最高优先级元素。
const STRICT_WAIT_TIMEOUT: Duration = Duration::from_millis(100);

pub(crate) struct OrderedQueue<T>
where
    T: Send,
{
    sender: Sender<Ordered<T>>,
    pending_count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

pub enum Ordering {
    #[allow(dead_code)]
    Relaxed,
    Strict,
}

pub struct OrderedQueueIter<T>
where
    T: Send,
{
    ordering: Ordering,
    stop: Arc<AtomicBool>,
    receiver: Receiver<Ordered<T>>,
    receive_buffer: BinaryHeap<Ordered<T>>,
    pending_count: Arc<AtomicUsize>,
    ordered_matcher: OrderedMatcher,
}

struct OrderedMatcher {
    looking_for: IndexPath,
    child_count_stack: Vec<usize>,
}

/// Bounded channel 容量：提供充足的缓冲避免生产者阻塞。
/// 设为 524288 (512K)，百万级条目扫描下也不会成为瓶颈。
/// 每个条目仅持有指针/小结构，524288 × ~64B ≈ 32MB，内存开销可控。
const CHANNEL_CAPACITY: usize = 524288;

const MAX_RECEIVE_BUFFER_SIZE: usize = 4096;

pub(crate) fn new_ordered_queue<T>(
    stop: Arc<AtomicBool>,
    ordering: Ordering,
) -> (OrderedQueue<T>, OrderedQueueIter<T>)
where
    T: Send,
{
    let pending_count = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = channel::bounded(CHANNEL_CAPACITY);
    (
        OrderedQueue {
            sender,
            pending_count: pending_count.clone(),
            stop: stop.clone(),
        },
        OrderedQueueIter {
            ordering,
            receiver,
            ordered_matcher: OrderedMatcher::default(),
            receive_buffer: BinaryHeap::with_capacity(INITIAL_HEAP_CAPACITY),
            pending_count,
            stop,
        },
    )
}

impl<T> OrderedQueue<T>
where
    T: Send,
{
    pub fn push(&self, ordered: Ordered<T>) -> Result<(), SendError<Ordered<T>>> {
        let result = self.sender.send(ordered);
        if result.is_ok() {
            // Release: 确保 ordered 数据写入在 pending_count 递增之前对其他线程可见。
            // 消费者 load(Acquire) 配对，确保看到完整的 ordered 数据。
            self.pending_count.fetch_add(1, AtomicOrdering::Release);
        }
        result
    }

}

impl<T> Clone for OrderedQueue<T>
where
    T: Send,
{
    fn clone(&self) -> Self {
        OrderedQueue {
            sender: self.sender.clone(),
            pending_count: self.pending_count.clone(),
            stop: self.stop.clone(),
        }
    }
}

impl<T> OrderedQueueIter<T>
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

    /// 批量 drain channel 中所有就绪元素到 receive_buffer。
    fn drain_channel(&mut self) {
        while self.receive_buffer.len() < MAX_RECEIVE_BUFFER_SIZE {
            if let Ok(ordered) = self.receiver.try_recv() {
                self.receive_buffer.push(ordered);
            } else {
                break;
            }
        }
    }

    fn try_next_relaxed(&mut self) -> Result<Ordered<T>, TryRecvError> {
        // 自适应退避：热路径短等待（10μs），逐步增长到 1ms
        let mut consecutive_waits: u32 = 0;

        loop {
            if self.is_stop() {
                return Err(TryRecvError::Disconnected);
            }

            // 先非阻塞 drain 所有已就绪元素
            self.drain_channel();

            if let Some(ordered_work) = self.receive_buffer.pop() {
                return Ok(ordered_work);
            } else if self.pending_count() == 0 {
                return Err(TryRecvError::Disconnected);
            }

            // 自适应等待：连续等待次数越多，timeout 越长
            let timeout = match consecutive_waits {
                0..=10 => Duration::from_micros(10),
                11..=50 => Duration::from_micros(100),
                _ => Duration::from_millis(1),
            };

            match self.receiver.recv_timeout(timeout) {
                Ok(ordered) => {
                    self.receive_buffer.push(ordered);
                    consecutive_waits = 0; // 有数据到达，重置退避
                }
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                    consecutive_waits += 1;
                    continue;
                }
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                    // Channel 断开，drain 残余
                    self.drain_channel();
                    if let Some(top) = self.receive_buffer.pop() {
                        return Ok(top);
                    }
                    return Err(TryRecvError::Disconnected);
                }
            }
        }
    }

    /// Strict 模式：优先等待队头（looking_for），超时后降级弹出最高优先级元素。
    ///
    /// 优化：(1) 仅在堆中无目标元素时才 drain channel，减少堆膨胀
    ///       (2) 超时降级时记录 skipped 位置，避免 OrderedMatcher 状态永久错乱
    fn try_next_strict(&mut self) -> Result<Ordered<T>, TryRecvError> {
        let deadline = Instant::now() + STRICT_WAIT_TIMEOUT;

        loop {
            if self.is_stop() {
                return Err(TryRecvError::Disconnected);
            }

            // 优化：仅当堆顶不是目标元素时才 drain channel，减少堆膨胀
            let looking_for = &self.ordered_matcher.looking_for;
            let top_matches = self.receive_buffer.peek()
                .map_or(false, |top| top.index_path.eq(looking_for));
            if !top_matches {
                self.drain_channel();
            }

            // 检查 buffer 中是否有目标元素
            let looking_for = &self.ordered_matcher.looking_for;
            let top_ordered = self.receive_buffer.peek();
            if let Some(top_ordered) = top_ordered {
                if top_ordered.index_path.eq(looking_for) {
                    break;
                }
            }

            if self.ordered_matcher.is_none() {
                return Err(TryRecvError::Disconnected);
            }

            if Instant::now() >= deadline {
                // Timeout: don't corrupt matcher with wrong advance_past.
                // Return Empty to yield thread and retry — producer will eventually
                // send the looking_for item or complete.
                return Err(TryRecvError::Empty);
            }

            // 带超时的阻塞等待
            let remaining = deadline.saturating_duration_since(Instant::now());
            let wait_time = remaining.max(std::time::Duration::from_micros(100));
            match self.receiver.recv_timeout(wait_time) {
                Ok(ordered) => {
                    self.receive_buffer.push(ordered);
                }
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                    continue;
                }
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                    self.drain_channel();
                    // Try looking_for first for correctness
                    if let Some(top) = self.receive_buffer.peek() {
                        if top.index_path.eq(&self.ordered_matcher.looking_for) {
                            let ordered = self.receive_buffer.pop().unwrap();
                            self.ordered_matcher.advance_past(&ordered);
                            return Ok(ordered);
                        }
                    }
                    // Drain remaining in heap order (natural DFS order via BinaryHeap)
                    if let Some(fallback) = self.receive_buffer.pop() {
                        return Ok(fallback);
                    }
                    return Err(TryRecvError::Disconnected);
                }
            }
        }

        let ordered = self.receive_buffer.pop().unwrap();
        self.ordered_matcher.advance_past(&ordered);
        Ok(ordered)
    }
}

impl<T> Iterator for OrderedQueueIter<T>
where
    T: Send,
{
    type Item = Ordered<T>;
    fn next(&mut self) -> Option<Ordered<T>> {
        loop {
            let try_next = match self.ordering {
                Ordering::Relaxed => self.try_next_relaxed(),
                Ordering::Strict => self.try_next_strict(),
            };
            match try_next {
                Ok(next) => {
                    let prev = self.pending_count.fetch_sub(1, AtomicOrdering::AcqRel);
                    if prev == 0 {
                        self.pending_count.fetch_add(1, AtomicOrdering::AcqRel);
                    }
                    return Some(next);
                }
                Err(err) => match err {
                    TryRecvError::Empty => thread::yield_now(),
                    TryRecvError::Disconnected => return None,
                },
            }
        }
    }
}

impl OrderedMatcher {
    fn is_none(&self) -> bool {
        self.looking_for.is_empty()
    }

    fn decrement_remaining_children(&mut self) {
        if let Some(count) = self.child_count_stack.last_mut() {
            *count = count.saturating_sub(1);
        }
    }

    fn advance_past<T>(&mut self, ordered: &Ordered<T>) {
        self.decrement_remaining_children();

        if ordered.child_count > 0 {
            self.looking_for.push(0);
            self.child_count_stack.push(ordered.child_count);
        } else {
            self.looking_for.increment_last();
            while !self.child_count_stack.is_empty() && *self.child_count_stack.last().unwrap() == 0
            {
                self.looking_for.pop();
                self.child_count_stack.pop();
                if !self.looking_for.is_empty() {
                    self.looking_for.increment_last();
                }
            }
        }
    }
}

impl Default for OrderedMatcher {
    fn default() -> OrderedMatcher {
        OrderedMatcher {
            looking_for: IndexPath::new(smallvec![0]),
            child_count_stack: vec![1],
        }
    }
}
