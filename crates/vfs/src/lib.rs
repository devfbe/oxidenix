//! The pure parts of the Linux server's namespace (docs/design/linux-server.md,
//! R6c): path arithmetic, the initramfs format and where reads and writes go.
//! Kept apart from the server so they can be tested on the host (the kernel
//! uses `rw` too, for the files it still serves).

#![no_std]

extern crate alloc;

pub mod cpio;
pub mod path;
pub mod rw;
pub mod stat;

/// File type bits of a mode (stat's st_mode), as Linux has them.
pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFIFO: u32 = 0o010000;
pub const S_IFSOCK: u32 = 0o140000;
pub const NAME_MAX: usize = 255;
/// statfs(2)'s `f_flags`: Linux sets ST_VALID on every answer (since 2.6.36), and a
/// read-only mount's ST_RDONLY.
pub const ST_RDONLY: u64 = 0x0001;
pub const ST_VALID: u64 = 0x0020;
