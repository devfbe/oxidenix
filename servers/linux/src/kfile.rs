//! Open files of the kernel's (phase R6e): an open file description of the
//! kernel's tree a program opened (its /dev: null, zero, the directory),
//! which the server holds by
//! handle (`SYS_INODE_OPEN`) as one of its own descriptions holds a file
//! of its own: the inverse of R6's placeholders. The calls on it are the
//! kernel's (`SYS_KFILE_CALL`), with the program's buffers; mmap maps the
//! handle; *at calls and fchdir reach its inode (`SYS_KFILE_INODE`). The
//! description's status flags are the server's; the kernel's file gets
//! O_APPEND and O_NONBLOCK too, for its own operations. These files are
//! always ready: poll, select and epoll need nothing from the kernel.

use crate::files::{self, File, O_ACCMODE, O_APPEND, O_CLOEXEC, O_NONBLOCK};
use crate::namespace::{check, KInode};
use crate::syscall;
use crate::usercopy;
use alloc::string::String;
use alloc::sync::Arc;
use restricted::*;

const F_SETFL: u64 = 4;
const SYS_FCNTL: u64 = 72;

pub struct KernelFile {
    handle: u64,
    /// The absolute path it was opened by (for *at calls).
    pub path: String,
}

/// A descriptor for the kernel's open file `handle` (`inode_open`'s, which
/// it takes over), opened by `path` with open(2)'s `flags`.
pub fn install(handle: u64, flags: u32, path: String) -> Result<i64, i64> {
    let file = Arc::new(KernelFile { handle, path });
    let kept = flags & (O_ACCMODE | O_NONBLOCK | O_APPEND | O_CLOEXEC);
    // (Without a descriptor the file goes again, closing its handle.)
    files::install(files::new_id(), File::Kernel(file), kept)
}

impl KernelFile {
    /// The handle (for `mo_map`).
    pub fn handle(&self) -> u64 {
        self.handle
    }

    /// Linux's call `nr` on it with the arguments after the descriptor.
    pub fn call(&self, nr: u64, a: [u64; 4]) -> Result<i64, i64> {
        check(syscall(SYS_KFILE_CALL, [self.handle, nr, a[0], a[1], a[2], a[3]]))
    }

    /// The file calls (`files::on_file`).
    pub fn on_file(&self, nr: u64, a1: u64, a2: u64, a3: u64) -> Result<i64, i64> {
        match nr {
            files::SYS_FSTAT => usercopy::to_program(a1, &self.stat()?).map(|_| 0),
            files::SYS_FSTATFS => {
                let mut words = [0u8; 120];
                self.call(files::SYS_FSTATFS, [words.as_mut_ptr() as u64, 0, 0, 0])?;
                usercopy::to_program(a1, &words).map(|_| 0)
            }
            _ => self.call(nr, [a1, a2, a3, 0]),
        }
    }

    /// Its `struct stat`, with the times the server keeps for the kernel's
    /// files (`namespace::set_pseudo_times`).
    pub fn stat(&self) -> Result<[u8; 144], i64> {
        let mut st = [0u8; 144];
        self.call(files::SYS_FSTAT, [st.as_mut_ptr() as u64, 0, 0, 0])?;
        crate::namespace::pseudo_times(&mut st);
        Ok(st)
    }

    /// read(2) into the server's memory (sendfile).
    pub fn read_server(&self, buf: &mut [u8]) -> Result<i64, i64> {
        self.call(files::SYS_READ | KFILE_SERVER_BUFFER, [buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0])
    }

    /// write(2) from the server's memory (sendfile).
    pub fn write_server(&self, buf: &[u8]) -> Result<i64, i64> {
        self.call(files::SYS_WRITE | KFILE_SERVER_BUFFER, [buf.as_ptr() as u64, buf.len() as u64, 0, 0])
    }

    /// The description's status flags changed (F_SETFL, FIONBIO).
    pub fn set_flags(&self, flags: u32) {
        let _ = self.call(SYS_FCNTL, [F_SETFL, flags as u64, 0, 0]);
    }

    /// The inode it was opened by.
    pub fn inode(&self) -> Result<KInode, i64> {
        KInode::from_result(syscall(SYS_KFILE_INODE, [self.handle, 0, 0, 0, 0, 0]))
    }
}

impl Drop for KernelFile {
    fn drop(&mut self) {
        syscall(SYS_HANDLE_CLOSE, [self.handle, 0, 0, 0, 0, 0]);
    }
}
