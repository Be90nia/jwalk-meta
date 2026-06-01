//! FIXME: A workaround to fix https://github.com/timberio/vector/issues/1480 resulting from https://github.com/rust-lang/rust/issues/63010
//! Most of code is cribbed directly from the Rust stdlib and ported to work with winapi.
//!
//! In stdlib imported code, warnings are allowed.

use std::fs::{self, Permissions};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
#[cfg(windows)]
use std::ptr;
use std::time::SystemTime;
#[cfg(windows)]
use std::{fs::File, mem::zeroed};
#[cfg(windows)]
use winapi::shared::minwindef::DWORD;
#[cfg(windows)]
use winapi::um::{
    fileapi::GetFileInformationByHandle, fileapi::BY_HANDLE_FILE_INFORMATION,
    ioapiset::DeviceIoControl, winioctl::FSCTL_GET_REPARSE_POINT,
    winnt::FILE_ATTRIBUTE_REPARSE_POINT, winnt::MAXIMUM_REPARSE_DATA_BUFFER_SIZE,
};

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
    pub is_file: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    pub permissions: Option<Permissions>,
}

#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct MetaDataExt {
    pub st_mode: u32,
    pub st_ino: u64,
    pub st_dev: u64,
    pub st_nlink: u64,
    pub st_blksize: u64,
    pub st_blocks: u64,
    pub st_uid: u32,
    pub st_gid: u32,
    pub st_rdev: u64,
}

#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct MetaDataExt {
    pub file_attributes: u32,
    pub volume_serial_number: Option<u32>,
    pub number_of_links: Option<u32>,
    pub file_index: Option<u64>,
}


