//! FIXME: A workaround to fix https://github.com/timberio/vector/issues/1480 resulting from https://github.com/rust-lang/rust/issues/63010
//! Most of code is cribbed directly from the Rust stdlib and ported to work with winapi.
//!
//! In stdlib imported code, warnings are allowed.

use std::fs::{self, Permissions};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::time::SystemTime;

#[inline]
pub fn get_metadata_ext(metadata: &fs::Metadata) -> MetaDataExt {
    #[cfg(unix)]
    {
        MetaDataExt {
            st_mode: metadata.mode(),
            st_ino: metadata.ino(),
            st_dev: metadata.dev(),
            st_nlink: metadata.nlink(),
            st_blksize: metadata.blksize(),
            st_blocks: metadata.blocks(),
            st_uid: metadata.uid(),
            st_gid: metadata.gid(),
            st_rdev: metadata.rdev(),
        }
    }
    #[cfg(windows)]
    {
        MetaDataExt {
            file_attributes: metadata.file_attributes(),
            volume_serial_number: metadata.volume_serial_number(),
            number_of_links: metadata.number_of_links(),
            file_index: metadata.file_index(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MetaData {
    /// True if DirEntry is a directory
    pub is_dir: bool,
    /// True if DirEntry is a regular file
    pub is_file: bool,
    /// True if DirEntry is a symbolic link
    pub is_symlink: bool,
    /// File size in bytes
    pub size: u64,
    /// Creation time, if available
    pub created: Option<SystemTime>,
    /// Last modification time, if available
    pub modified: Option<SystemTime>,
    /// Last access time, if available
    pub accessed: Option<SystemTime>,
    /// File permissions. `None` on Windows NT fast path (NT API doesn't return permissions),
    /// `Some` on all other paths. Always check with `is_some()` before use.
    pub permissions: Option<Permissions>,
}

/// Unix-specific file metadata, corresponding to POSIX `stat` fields.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct MetaDataExt {
    /// File mode (permissions + file type bits), corresponds to `st_mode`
    pub st_mode: u32,
    /// Inode number, corresponds to `st_ino`
    pub st_ino: u64,
    /// Device ID, corresponds to `st_dev`
    pub st_dev: u64,
    /// Number of hard links, corresponds to `st_nlink`
    pub st_nlink: u64,
    /// Block size for filesystem I/O, corresponds to `st_blksize`
    pub st_blksize: u64,
    /// Number of 512-byte blocks allocated, corresponds to `st_blocks`
    pub st_blocks: u64,
    /// User ID of owner, corresponds to `st_uid`
    pub st_uid: u32,
    /// Group ID of owner, corresponds to `st_gid`
    pub st_gid: u32,
    /// Device ID for special file, corresponds to `st_rdev`
    pub st_rdev: u64,
}

/// Windows-specific file metadata, corresponding to `BY_HANDLE_FILE_INFORMATION`.
#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct MetaDataExt {
    /// File attributes (e.g. `FILE_ATTRIBUTE_DIRECTORY` = `0x10`)
    pub file_attributes: u32,
    /// Volume serial number, may be `None` if not available
    pub volume_serial_number: Option<u32>,
    /// Number of links to this file, may be `None` if not available
    pub number_of_links: Option<u32>,
    /// File index (inode equivalent on NTFS/ReFS), may be `None` if not available
    pub file_index: Option<u64>,
}


