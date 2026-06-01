use super::*;
use crate::Result;

/// Client's read dir function.
///
/// 第二个参数 `Option<StreamingContext<C>>` 在并行模式下提供流式分发上下文，
/// 允许在枚举期间立即调度子目录（而非等待完整枚举结束后再调度）。
/// 单线程 Walk 模式传 `None`。
pub(crate) type ReadDirCallback<C> =
    dyn Fn(ReadDirSpec<C>, Option<StreamingContext<C>>) -> Result<ReadDir<C>> + Send + Sync + 'static;

/// 流式分发上下文，用于在 NT 枚举期间立即调度子目录。
///
/// 在 `multi_threaded_walk_dir` 中创建，传递给 callback。
/// callback 内部对每个子目录调用 `schedule()` 实现即时调度。
pub(crate) struct StreamingContext<C: ClientState> {
    pub index_path: IndexPath,
    run_context: RunContext<C>,
}

impl<C: ClientState> StreamingContext<C> {
    pub(crate) fn new(index_path: IndexPath, run_context: RunContext<C>) -> Self {
        StreamingContext {
            index_path,
            run_context,
        }
    }

    /// 调度一个子目录的读取任务到优先队列。
    pub(crate) fn schedule(&self, weighted: Weighted<ReadDirSpec<C>>) {
        self.run_context.schedule_read_dir_spec(weighted);
    }
}

/// Result<ReadDir> Iterator.
///
/// Yields ReadDirs (results of fs::read_dir) in order required for recursive
/// directory traversal. Depending on Walk/ParWalk state these reads might be
/// computed in parallel.
pub enum ReadDirIter<C: ClientState> {
    Walk {
        read_dir_spec_stack: Vec<ReadDirSpec<C>>,
        core_read_dir_callback: Arc<ReadDirCallback<C>>,
    },
    ParWalk {
        read_dir_result_iter: OrderedQueueIter<Result<ReadDir<C>>>,
    },
}

impl<C: ClientState> ReadDirIter<C> {
    pub(crate) fn try_new(
        read_dir_specs: Vec<ReadDirSpec<C>>,
        parallelism: Parallelism,
        core_read_dir_callback: Arc<ReadDirCallback<C>>,
    ) -> Option<(Self, Option<Arc<AtomicBool>>)> {
        if let Parallelism::Serial = parallelism {
            Some((
                ReadDirIter::Walk {
                    read_dir_spec_stack: read_dir_specs,
                    core_read_dir_callback,
                },
                None,
            ))
        } else {
            let stop = Arc::new(AtomicBool::new(false));
            let read_dir_result_queue = new_ordered_queue(stop.clone(), Ordering::Strict);
            let (read_dir_result_queue, read_dir_result_iter) = read_dir_result_queue;
            let (read_dir_spec_queue, read_dir_spec_iter) =
                new_priority_queue(stop.clone());

            for read_dir_spec in read_dir_specs.into_iter() {
                read_dir_spec_queue
                    .push(Weighted::new(read_dir_spec, IndexPath::new(vec![0]), 0))
                    .unwrap();
            }

            let run_context = RunContext {
                stop: stop.clone(),
                read_dir_spec_queue,
                read_dir_result_queue,
                core_read_dir_callback,
            };

            let (startup_tx, startup_rx) = parallelism
                .timeout()
                .map(|duration| {
                    let (tx, rx) = crossbeam::channel::unbounded();
                    (Some(tx), Some((rx, duration)))
                })
                .unwrap_or((None, None));
            parallelism.spawn(move || {
                if let Some(tx) = startup_tx {
                    if tx.send(()).is_err() {
                        return;
                    }
                }
                read_dir_spec_iter.par_bridge().for_each_with(
                    run_context,
                    |run_context, weighted_read_dir_spec| {
                        multi_threaded_walk_dir(weighted_read_dir_spec, run_context);
                    },
                );
            });
            if startup_rx.map_or(false, |(rx, duration)| rx.recv_timeout(duration).is_err()) {
                return None;
            }
            Some((
                ReadDirIter::ParWalk {
                    read_dir_result_iter,
                },
                Some(stop),
            ))
        }
    }
}

impl<C: ClientState> Iterator for ReadDirIter<C> {
    type Item = Result<ReadDir<C>>;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            ReadDirIter::Walk {
                read_dir_spec_stack,
                core_read_dir_callback,
            } => {
                let read_dir_spec = read_dir_spec_stack.pop()?;
                // Walk 模式不使用流式分发
                let read_dir_result = core_read_dir_callback(read_dir_spec, None);

                if let Ok(read_dir) = read_dir_result.as_ref() {
                    for each_spec in read_dir
                        .read_children_specs()
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                    {
                        read_dir_spec_stack.push(each_spec);
                    }
                }

                Some(read_dir_result)
            }

            ReadDirIter::ParWalk {
                read_dir_result_iter,
            } => read_dir_result_iter
                .next()
                .map(|read_dir_result| read_dir_result.value),
        }
    }
}

fn multi_threaded_walk_dir<C: ClientState>(
    weighted_read_dir_spec: Weighted<ReadDirSpec<C>>,
    run_context: &mut RunContext<C>,
) {
    let Weighted {
        value: read_dir_spec,
        index_path,
        ..
    } = weighted_read_dir_spec;

    // 创建流式分发上下文，传递给 callback
    let streaming_ctx = StreamingContext::new(index_path.clone(), run_context.clone());

    let read_dir_result = (run_context.core_read_dir_callback)(read_dir_spec, Some(streaming_ctx));

    // 流式分发后：已通过 streaming callback 调度的子目录不需要再次调度
    // 只需调度 results_list 中非流式部分的子目录
    let schedule_regular = read_dir_result.as_ref().map_or(false, |read_dir| {
        read_dir.streamed_child_count == 0
    });

    let weighted_children_specs = if schedule_regular {
        read_dir_result
            .as_ref()
            .ok()
            .map(|read_dir| read_dir.weighted_children_specs(&index_path))
    } else {
        None
    };

    // child_count = 流式已调度数 + 常规调度数
    let streamed_count = read_dir_result.as_ref().map_or(0, |rd| rd.streamed_child_count);
    let regular_count = weighted_children_specs.as_ref().map_or(0, Vec::len);
    let child_count = streamed_count + regular_count;

    let ordered_read_dir_result = Ordered::new(
        read_dir_result,
        index_path,
        child_count,
    );

    let send_ok = run_context.send_read_dir_result(ordered_read_dir_result);

    if send_ok {
        if let Some(weighted_children_specs) = weighted_children_specs {
            for each in weighted_children_specs {
                let _ = run_context.schedule_read_dir_spec(each);
            }
        }
    }

    run_context.complete_item();
}
