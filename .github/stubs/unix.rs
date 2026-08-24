use std::io;
use std::os::unix::io::RawFd;
use std::fs::File;
use std::os::unix::io::FromRawFd;
use std::mem::ManuallyDrop;

pub struct MmapInner {
    ptr: *mut u8,
    len: usize,
}

impl MmapInner {
    pub fn map(_len: usize, _file: RawFd, _offset: u64, _populate: bool, _no_reserve: bool) -> io::Result<MmapInner> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub fn map_exec(_len: usize, _file: RawFd, _offset: u64, _populate: bool, _no_reserve: bool) -> io::Result<MmapInner> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub fn map_mut(_len: usize, _file: RawFd, _offset: u64, _populate: bool, _no_reserve: bool) -> io::Result<MmapInner> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub fn map_copy(_len: usize, _file: RawFd, _offset: u64, _populate: bool, _no_reserve: bool) -> io::Result<MmapInner> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub fn map_copy_read_only(_len: usize, _file: RawFd, _offset: u64, _populate: bool, _no_reserve: bool) -> io::Result<MmapInner> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub fn map_anon(_len: usize, _stack: bool, _populate: bool, _huge: Option<u8>, _no_reserve: bool) -> io::Result<MmapInner> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub fn flush(&self, _offset: usize, _len: usize) -> io::Result<()> { Ok(()) }
    pub fn flush_async(&self, _offset: usize, _len: usize) -> io::Result<()> { Ok(()) }
    pub fn make_read_only(&mut self) -> io::Result<()> { Ok(()) }
    pub fn make_exec(&mut self) -> io::Result<()> { Ok(()) }
    pub fn make_mut(&mut self) -> io::Result<()> { Ok(()) }
    pub fn ptr(&self) -> *const u8 { self.ptr }
    pub fn mut_ptr(&mut self) -> *mut u8 { self.ptr }
    pub fn len(&self) -> usize { self.len }
    pub unsafe fn advise(&self, _advice: i32, _offset: usize, _len: usize) -> io::Result<()> { Ok(()) }
    pub fn remap(&mut self, _new_len: usize, _options: crate::RemapOptions) -> io::Result<()> { Err(io::ErrorKind::Unsupported.into()) }
    pub fn lock(&self) -> io::Result<()> { Ok(()) }
    pub fn unlock(&self) -> io::Result<()> { Ok(()) }
}

pub fn file_len(file: RawFd) -> io::Result<u64> {
    unsafe {
        let f = ManuallyDrop::new(File::from_raw_fd(file));
        Ok(f.metadata()?.len())
    }
}

// Raw pointers are not Send/Sync by default, but the real memmap2
// implements them manually because the mapped memory is safe to share.
unsafe impl Send for MmapInner {}
unsafe impl Sync for MmapInner {}
