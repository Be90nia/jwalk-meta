use std::fs::FileType;
use std::mem;

fn main() {
    let ft_dir: FileType = unsafe { std::fs::metadata(r"Z:\").unwrap().file_type() };
    let ft_file: FileType = unsafe { std::fs::metadata(r"Z:\品质部\内部文件").unwrap().file_type() };
    println!("FileType size: {}", mem::size_of::<FileType>());
    println!("FileType align: {}", mem::align_of::<FileType>());
    let dir_bytes = unsafe { mem::transmute::<FileType, [u8; 4]>(ft_dir.clone()) };
    let file_bytes = unsafe { mem::transmute::<FileType, [u8; 4]>(ft_file.clone()) };
    println!("dir  is_dir={} is_file={} is_symlink={}", ft_dir.is_dir(), ft_dir.is_file(), ft_dir.is_symlink());
    println!("dir  bytes: {:?}", dir_bytes);
    println!("file is_dir={} is_file={} is_symlink={}", ft_file.is_dir(), ft_file.is_file(), ft_file.is_symlink());
    println!("file bytes: {:?}", file_bytes);
}
