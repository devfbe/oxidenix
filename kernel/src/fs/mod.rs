//! The boot image and the kernel's memory objects. The kernel has no
//! filesystem (R9): the initramfs it booted with is read-only bytes, which
//! it reads its own programs from (the native servers and the Linux server,
//! `program`) and hands to each Linux server instance whole
//! (`restricted::SYS_INITRAMFS`), which unpacks it into its own tmpfs. Every
//! file a program sees is a server's. The memory objects (`cache`: anonymous
//! and file objects, paged objects, the Linux server's page cache) are the
//! kernel's.

pub mod cache;
pub mod cpio;

use spin::Once;

/// The initramfs the kernel booted with.
static INITRAMFS: Once<&'static [u8]> = Once::new();

/// The boot image's initramfs, for the Linux server (`SYS_INITRAMFS`).
pub fn initramfs() -> Option<&'static [u8]> {
    INITRAMFS.get().copied()
}

pub fn init(ramdisk: Option<&'static [u8]>) {
    cache::init();
    if let Some(data) = ramdisk {
        INITRAMFS.call_once(|| data);
    }
}

/// The contents of the regular file at the absolute `path` of the boot
/// image (symlinks in the image followed), or None: the programs the kernel
/// starts itself, and the markers it looks for (`/etc/autorun`).
pub fn program(path: &str) -> Option<&'static [u8]> {
    cpio::find(initramfs()?, path)
}
