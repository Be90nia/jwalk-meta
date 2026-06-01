use super::{ClientState, DirEntry, IndexPath, Weighted, ReadDirSpec};
use crate::Result;

/// Results of successfully reading a directory.
#[derive(Debug)]
pub struct ReadDir<C: ClientState> {
    pub(crate) read_dir_state: C::ReadDirState,
    pub(crate) results_list: Vec<Result<DirEntry<C>>>,
    /// 流式分发期间已提前调度的子目录数量。
    pub(crate) streamed_child_count: usize,
}

impl<C: ClientState> ReadDir<C> {
    pub fn new(
        read_dir_state: C::ReadDirState,
        results_list: Vec<Result<DirEntry<C>>>,
    ) -> ReadDir<C> {
        ReadDir {
            read_dir_state,
            results_list,
            streamed_child_count: 0,
        }
    }

    pub fn read_children_specs(&self) -> impl Iterator<Item = ReadDirSpec<C>> + '_ {
        self.results_list.iter().filter_map(move |each| {
            each.as_ref()
                .ok()?
                .read_children_spec(self.read_dir_state.clone())
        })
    }

    /// 单遍遍历：同时收集 specs 和计算 child_count。
    /// 已通过流式分发调度的子目录跳过（index >= streamed_child_count）。
    pub fn weighted_children_specs(
        &self,
        index_path: &IndexPath,
    ) -> Vec<Weighted<ReadDirSpec<C>>> {
        let skip = self.streamed_child_count;
        let mut specs = Vec::new();
        for (i, spec) in self.read_children_specs().enumerate() {
            if i < skip {
                continue;
            }
            specs.push((i, spec));
        }
        let child_count = skip + specs.len();
        specs
            .into_iter()
            .map(|(i, spec)| Weighted::new(spec, index_path.adding(i), child_count))
            .collect()
    }
}
