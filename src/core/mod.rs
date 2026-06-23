mod dir_entry;
mod dir_entry_iter;
mod metadata;
mod error;
mod index_path;
mod ordered;
mod ordered_queue;
mod read_dir;
mod read_dir_iter;
mod read_dir_spec;
mod run_context;
mod priority_queue;
mod weighted;
#[cfg(windows)]
mod nt_dir_enum;

use rayon::prelude::*;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use index_path::*;
use ordered::*;
use ordered_queue::*;
use priority_queue::*;
use read_dir_iter::*;
use run_context::RunContext;

pub use dir_entry::DirEntry;
pub use dir_entry_iter::DirEntryIter;
pub use error::Error;
pub use read_dir::ReadDir;
pub use read_dir_spec::ReadDirSpec;
pub use metadata::{get_metadata_ext, MetaData, MetaDataExt, AncestorIdentity};
pub(crate) use weighted::Weighted;
pub(crate) use read_dir_iter::StreamingContext;
pub(crate) use error::is_transient_error;

#[cfg(windows)]
pub(crate) use nt_dir_enum::{
    enumerate_dir, enumerate_dir_streaming, DirEntryInfo, batch_query_nlinks,
    detect_fs_type_and_vol_serial,
};

use crate::{ClientState, Parallelism};
