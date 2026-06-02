//! Windows NT Native API 批量目录枚举。
//!
//! 使用 NtQueryDirectoryFileEx + 64KB 缓冲区替代 FindFirstFileW/FindNextFileW，
//! 实现单次系统调用批量获取目录条目，显著减少用户态-内核态切换次数。

#![cfg(windows)]

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::{io, ptr};

use winapi::shared::ntdef::{HANDLE, LPCWSTR};
use winapi::um::fileapi::{
    CreateFileW, OPEN_EXISTING,
};
use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
use winapi::um::libloaderapi::{GetProcAddress, LoadLibraryW};
use winapi::um::winbase::FILE_FLAG_BACKUP_SEMANTICS;
use winapi::um::winnt::{
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

/// NtQueryDirectoryFileEx 的 64KB I/O 缓冲区，减少系统调用次数。
const BUFFER_SIZE: usize = 64 * 1024;

/// FILE_ID_BOTH_DIR_INFO 的文件信息类编号。
const FILE_ID_BOTH_DIR_INFO_CLASS: u32 = 37;

/// NtQueryDirectoryFileEx 返回 "没有更多文件" 的状态码。
const STATUS_NO_MORE_FILES: i32 = -2147483642; // 0x80000006

// ── FFI 结构体 ──────────────────────────────────────────────────────────

/// NtQueryDirectoryFileEx 返回的目录条目信息结构。
/// 对应 Windows FILE_ID_BOTH_DIR_INFO。
#[repr(C)]
#[allow(non_snake_case)]
struct FILE_ID_BOTH_DIR_INFO {
    NextEntryOffset: u32,
    FileIndex: u32,
    CreationTime: i64,
    LastAccessTime: i64,
    LastWriteTime: i64,
    ChangeTime: i64,
    EndOfFile: i64,
    AllocationSize: i64,
    FileAttributes: u32,
    FileNameLength: u32,
    EaSize: u32,
    ShortNameLength: i16,
    ShortName: [u16; 12],
    FileId: i64,
    FileName: [u16; 1],
}

/// NT I/O 状态块，NtQueryDirectoryFileEx 的输出参数。
#[repr(C)]
#[allow(non_snake_case)]
struct IO_STATUS_BLOCK {
    Status: i32,
    Information: usize,
}

// ── 动态加载 ntdll ─────────────────────────────────────────────────────

/// NtQueryDirectoryFileEx 函数签名。
#[allow(non_snake_case)]
type NtQueryDirectoryFileExFn = unsafe extern "system" fn(
    FileHandle: HANDLE,
    Event: HANDLE,
    ApcRoutine: Option<unsafe extern "system" fn(...)>,
    ApcContext: *mut std::ffi::c_void,
    IoStatusBlock: *mut IO_STATUS_BLOCK,
    FileInformation: *mut u8,
    Length: u32,
    FileInformationClass: u32,
    ReturnSingleEntry: i32,
    FileName: LPCWSTR,
    RestartScan: i32,
) -> i32;

/// RtlNtStatusToDosError 函数签名，用于将 NTSTATUS 转换为 Win32 错误码。
type RtlNtStatusToDosErrorFn = unsafe extern "system" fn(status: i32) -> u32;

/// 从 ntdll.dll 动态加载的函数指针集合。
struct NtDllFuncs {
    query_dir: NtQueryDirectoryFileExFn,
    status_to_win32: RtlNtStatusToDosErrorFn,
}

/// 懒加载 ntdll 函数指针。失败时 panic（启动时一次性初始化）。
///
/// 设计决策：LoadLibraryW 增加了 ntdll.dll 的引用计数但未调用 FreeLibrary。
/// 这是有意为之——ntdll.dll 是 Windows 进程级核心 DLL，永远不会被卸载，
/// 函数指针需要在整个进程生命周期内保持有效。
fn ntdll_funcs() -> &'static NtDllFuncs {
    use std::sync::OnceLock;
    static FUNCS: OnceLock<NtDllFuncs> = OnceLock::new();
    FUNCS.get_or_init(|| unsafe {
        // SAFETY:
        // 1. LoadLibraryW("ntdll.dll") 安全——ntdll.dll 是 Windows 系统核心 DLL，始终存在
        // 2. GetProcAddress 获取的函数指针签名由上面的 type alias 保证与 NT API 文档一致
        // 3. transmute 将 raw 指针转为类型化函数指针，类型由 NtQueryDirectoryFileExFn
        //    和 RtlNtStatusToDosErrorFn 的签名定义保证正确
        let ntdll_name = to_wide_null("ntdll.dll\0");
        let module = LoadLibraryW(ntdll_name.as_ptr());
        if module.is_null() {
            panic!("nt_dir_enum: 无法加载 ntdll.dll");
        }

        let query_dir_ptr = GetProcAddress(module, b"NtQueryDirectoryFileEx\0".as_ptr() as *const i8);
        let status_to_win32_ptr = GetProcAddress(module, b"RtlNtStatusToDosError\0".as_ptr() as *const i8);

        if query_dir_ptr.is_null() || status_to_win32_ptr.is_null() {
            panic!("nt_dir_enum: 无法获取 ntdll 函数地址");
        }

        NtDllFuncs {
            query_dir: std::mem::transmute(query_dir_ptr),
            status_to_win32: std::mem::transmute(status_to_win32_ptr),
        }
    })
}

// ── RAII 句柄守卫 ──────────────────────────────────────────────────────

/// RAII 守卫，Drop 时自动 CloseHandle。用于 CreateFileW 返回的句柄。
struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        // SAFETY: HandleGuard 仅包装 CreateFileW 返回的有效句柄，
        // Drop 时调用 CloseHandle 是安全的。
        unsafe {
            CloseHandle(self.0);
        }
    }
}

// ── 公共结构体 ─────────────────────────────────────────────────────────

/// NtQueryDirectoryFileEx 返回的目录条目信息。
#[derive(Debug)]
pub struct DirEntryInfo {
    pub file_name: OsString,
    pub file_attributes: u32,
    pub file_size: u64,
    pub creation_time: i64,
    pub last_write_time: i64,
    pub last_access_time: i64,
    pub file_id: u64,
}

// ── 辅助函数 ───────────────────────────────────────────────────────────

/// 将字节字符串转为 null-terminated UTF-16 Vec（用于 LoadLibraryW/GetProcAddress）。
fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 将 OS 路径转为 null-terminated UTF-16 Vec（用于 CreateFileW 等宽字符 API）。
fn path_to_widestring(path: &Path) -> Vec<u16> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    wide
}

/// 将 NTSTATUS 转换为 io::Error。
fn ntstatus_to_io_error(status: i32, funcs: &NtDllFuncs) -> io::Error {
    // SAFETY: RtlNtStatusToDosError 是稳定的 Windows API，输入任意 i32 均安全。
    let win32_err = unsafe { (funcs.status_to_win32)(status) };
    io::Error::from_raw_os_error(win32_err as i32)
}

// ── 核心枚举函数 ───────────────────────────────────────────────────────

/// 打开目录句柄，返回 RAII 守卫。
///
/// SAFETY: CreateFileW 参数合法，FILE_FLAG_BACKUP_SEMANTICS 允许打开目录。
fn open_dir_handle(path: &Path) -> io::Result<HandleGuard> {
    let wide_path = path_to_widestring(path);
    let handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }

    Ok(HandleGuard(handle))
}

/// 解析单次 NtQueryDirectoryFileEx 返回的 buffer，提取目录条目。
///
/// 对每个非 "."/".." 条目调用 `on_entry` 回调。
///
/// SAFETY: 调用者必须保证 buffer 包含有效的 NT API 返回数据。
fn parse_buffer_entries(
    buffer: &[u8],
    mut on_entry: impl FnMut(&FILE_ID_BOTH_DIR_INFO, &OsString),
) {
    let mut offset: usize = 0;
    loop {
        // SAFETY: offset 从 0 开始，每次递增 NextEntryOffset（对齐的 u32），
        // 且 bytes_returned 保证 offset < bytes_returned <= buffer.len()
        let entry_ptr = unsafe { buffer.as_ptr().add(offset) };
        // SAFETY: entry_ptr 指向 buffer 中有效的 FILE_ID_BOTH_DIR_INFO 数据。
        // 结构体对齐由 NT API 保证（4 字节对齐）。
        let entry = unsafe { &*(entry_ptr as *const FILE_ID_BOTH_DIR_INFO) };

        let name_len = entry.FileNameLength as usize;
        let name_chars = name_len / 2;

        // Raw UTF-16 比较 "." 和 ".."，避免 to_string_lossy 堆分配
        // FileName 是 C 变长数组 ([u16; 1])，必须用指针访问避免编译器越界检查
        let first_char = entry.FileName[0];
        let is_dot = name_chars == 1 && first_char == b'.' as u16;
        let is_dotdot = name_chars == 2
            && first_char == b'.' as u16
            // SAFETY: name_chars == 2 guarantees at least 2 u16 elements in FileName
            && unsafe { *entry.FileName.as_ptr().add(1) } == b'.' as u16;

        if !is_dot && !is_dotdot {
            // SAFETY: FileName 的字节数由 FileNameLength 保证，从 offset+offsetof(FileName) 开始。
            let name_slice = unsafe {
                let name_start =
                    entry_ptr.add(std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName));
                std::slice::from_raw_parts(name_start as *const u16, name_chars)
            };
            let file_name = OsString::from_wide(name_slice);

            on_entry(entry, &file_name);
        }

        let next_offset = entry.NextEntryOffset;
        if next_offset == 0 {
            break;
        }
        offset += next_offset as usize;
    }
}

/// 核心枚举循环：NtQueryDirectoryFileEx 批量查询 + buffer 解析。
///
/// `on_entry` 对每个非 "."/".." 条目调用，接收解析出的原始 entry 和文件名。
/// 调用者负责构造 `DirEntryInfo` 并决定是否收集/分发。
fn enumerate_dir_core(
    guard: &HandleGuard,
    funcs: &NtDllFuncs,
    mut on_entry: impl FnMut(&FILE_ID_BOTH_DIR_INFO, &OsString),
) -> io::Result<()> {
    let mut buffer = vec![0u8; BUFFER_SIZE];

    // 首次调用时 RestartScan = 1（重新开始扫描）
    let mut restart_scan: i32 = 1;

    loop {
        let mut iosb = IO_STATUS_BLOCK {
            Status: 0,
            Information: 0,
        };

        // SAFETY: handle 是有效的目录句柄，buffer 大小充足，
        // FILE_ID_BOTH_DIR_INFO_CLASS 是合法的信息类。
        let status = unsafe {
            (funcs.query_dir)(
                guard.0,
                ptr::null_mut(),
                None,
                ptr::null_mut(),
                &mut iosb,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                FILE_ID_BOTH_DIR_INFO_CLASS,
                0, // ReturnSingleEntry = FALSE
                ptr::null(),
                restart_scan,
            )
        };

        restart_scan = 0;

        if status != 0 {
            if status == STATUS_NO_MORE_FILES {
                break;
            }
            return Err(ntstatus_to_io_error(status, funcs));
        }

        // 安全：iosb.Information 包含 buffer 中有效数据的字节数
        let bytes_returned = iosb.Information;
        if bytes_returned == 0 {
            break;
        }

        parse_buffer_entries(&buffer, &mut on_entry);
    }

    Ok(())
}

/// 使用 NtQueryDirectoryFileEx 批量枚举目录中的所有文件和子目录。
///
/// 返回除 "." 和 ".." 外的所有条目。使用 64KB 缓冲区减少系统调用次数，
/// 并预分配 Vec 以减少堆重分配。
pub fn enumerate_dir(path: &Path, capacity: usize) -> io::Result<Vec<DirEntryInfo>> {
    let funcs = ntdll_funcs();
    let guard = open_dir_handle(path)?;
    let mut result = Vec::with_capacity(capacity);

    enumerate_dir_core(&guard, funcs, |entry, file_name| {
        result.push(DirEntryInfo {
            file_name: file_name.clone(),
            file_attributes: entry.FileAttributes,
            file_size: entry.EndOfFile as u64,
            creation_time: entry.CreationTime,
            last_write_time: entry.LastWriteTime,
            last_access_time: entry.LastAccessTime,
            file_id: entry.FileId as u64,
        });
    })?;

    Ok(result)
}

/// 使用 NtQueryDirectoryFileEx 批量枚举，并对每个子目录调用回调实现流式分发。
///
/// 子目录通过回调立即分发（无需等待完整枚举），所有条目（文件和子目录）
/// 仍然收集到返回的 Vec 中，不遗漏任何条目。
pub fn enumerate_dir_streaming(
    path: &Path,
    capacity: usize,
    mut on_subdir: impl FnMut(&DirEntryInfo),
) -> io::Result<Vec<DirEntryInfo>> {
    let funcs = ntdll_funcs();
    let guard = open_dir_handle(path)?;
    let mut result = Vec::with_capacity(capacity);

    enumerate_dir_core(&guard, funcs, |entry, file_name| {
        let info = DirEntryInfo {
            file_name: file_name.clone(),
            file_attributes: entry.FileAttributes,
            file_size: entry.EndOfFile as u64,
            creation_time: entry.CreationTime,
            last_write_time: entry.LastWriteTime,
            last_access_time: entry.LastAccessTime,
            file_id: entry.FileId as u64,
        };

        // FILE_ATTRIBUTE_DIRECTORY = 0x10
        if info.file_attributes & 0x10 != 0 {
            on_subdir(&info);
        }

        result.push(info);
    })?;

    Ok(result)
}
