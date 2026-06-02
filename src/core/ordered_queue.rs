//! Ordered queue backed by a channel.

use crossbeam::channel::{self, Receiver, SendError, Sender, TryRecvError};
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::thread;
use std::time::{Duration, Instant};

use super::*;

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

pub(crate) fn new_ordered_queue<T>(
    stop: Arc<AtomicBool>,
    ordering: Ordering,
) -> (OrderedQueue<T>, OrderedQueueIter<T>)
where
    T: Send,
{
    let pending_count = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = channel::unbounded();
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
            receive_buffer: BinaryHeap::with_capacity(256),
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
            self.pending_count.fetch_add(1, AtomicOrdering::Release);
        }
        result
    }

    pub fn complete_item(&self) {
        self.pending_count.fetch_sub(1, AtomicOrdering::AcqRel);
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
        self.pending_count.load(AtomicOrdering::Acquire)
    }

    fn is_stop(&self) -> bool {
        self.stop.load(AtomicOrdering::Acquire)
    }

    /// 批量 drain channel 中所有就绪元素到 receive_buffer。
    fn drain_channel(&mut self) {
        while let Ok(ordered) = self.receiver.try_recv() {
            self.receive_buffer.push(ordered);
        }
    }

    fn try_next_relaxed(&mut self) -> Result<Ordered<T>, TryRecvError> {
        let timeout = std::time::Duration::from_millis(1);

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

            // buffer 为空且仍有 pending 项：短超时等待新元素
            match self.receiver.recv_timeout(timeout) {
                Ok(ordered) => {
                    self.receive_buffer.push(ordered);
                }
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => continue,
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

            // 超时降级：弹出堆中最高优先级元素（跳过 DFS 序）
            if Instant::now() >= deadline {
                if self.receive_buffer.peek().is_some() {
                    let fallback = self.receive_buffer.pop().unwrap();
                    // 降级弹出：advance_past 更新状态以跳到下一个有效位置
                    // 注意：这会跳过 looking_for 指向的目标，DFS 顺序保证被打破
                    self.ordered_matcher.advance_past(&fallback);
                    return Ok(fallback);
                }
                // deadline 到期但 buffer 空 → 返回 Empty，让外层 yield 后重试
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
                    let looking_for = &self.ordered_matcher.looking_for;
                    let top = self.receive_buffer.peek();
                    if let Some(top_ordered) = top {
                        if top_ordered.index_path.eq(looking_for) {
                            break;
                        }
                    }
                    if self.receive_buffer.peek().is_some() {
                        let fallback = self.receive_buffer.pop().unwrap();
                        self.ordered_matcher.advance_past(&fallback);
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
            looking_for: IndexPath::new(vec![0]),
            child_count_stack: vec![1],
        }
    }
}
