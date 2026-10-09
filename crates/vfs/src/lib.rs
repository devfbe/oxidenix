//! The pure parts of the Linux server's namespace (docs/design/linux-server.md,
//! R6c): path arithmetic, the initramfs format and where reads and writes go.
//! Kept apart from the server so they can be tested on the host (the kernel
//! uses `rw` too, for the files it still serves).

#![no_std]

extern crate alloc;

pub mod cpio;
pub mod path;
pub mod rw;

/// File type bits of a mode (stat's st_mode), as Linux has them.
pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFSOCK: u32 = 0o140000;
pub const NAME_MAX: usize = 255;
