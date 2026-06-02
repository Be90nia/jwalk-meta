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

pub(crate) fn new_priority_queue<T>(
    stop: Arc<AtomicBool>,
) -> (PriorityQueue<T>, PriorityQueueIter<T>)
where
    T: Send,
{
    let pending_count = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = channel::unbounded();
    (
        PriorityQueue {
            sender,
            pending_count: pending_count.clone(),
            stop: stop.clone(),
        },
        PriorityQueueIter {
            receiver,
            receive_buffer: BinaryHeap::with_capacity(256),
            pending_count,
            stop,
        },
    )
}

impl<T> PriorityQueue<T>
where
    T: Send,
{
    pub fn push(&self, weighted: Weighted<T>) -> Result<(), SendError<Weighted<T>>> {
        self.pending_count.fetch_add(1, AtomicOrdering::Release);
        self.sender.send(weighted)
    }

    pub fn complete_item(&self) {
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
        self.pending_count.load(AtomicOrdering::Acquire)
    }

    fn is_stop(&self) -> bool {
        self.stop.load(AtomicOrdering::Acquire)
    }

    fn try_next(&mut self) -> Result<Weighted<T>, TryRecvError> {
        loop {
            if self.is_stop() {
                return Err(TryRecvError::Disconnected);
            }

            // 1ms 超时 + 批量 drain：保留原始响应速度，
            // drain 减少后续 recv 调用次数
            match self.receiver.recv_timeout(std::time::Duration::from_millis(1)) {
                Ok(weighted) => {
                    self.receive_buffer.push(weighted);
                    // 批量 drain channel 中所有已就绪的元素
                    while let Ok(w) = self.receiver.try_recv() {
                        self.receive_buffer.push(w);
                    }
                }
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                    // 超时：检查是否所有工作已完成
                }
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                    // Channel 已断开，drain 残余元素
                    while let Ok(w) = self.receiver.try_recv() {
                        self.receive_buffer.push(w);
                    }
                }
            }

            if let Some(weighted) = self.receive_buffer.pop() {
                return Ok(weighted);
            } else if self.pending_count() == 0 {
                return Err(TryRecvError::Disconnected);
            }
            // buffer 为空但仍有 pending 项（被其他线程处理中），继续等待
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
