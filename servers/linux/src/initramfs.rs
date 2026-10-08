//! The instance's root tmpfs from the boot image's initramfs (phase
//! R6c.2c): the server reads the archive's headers and names from the
//! kernel's image object (`SYS_INITRAMFS`) and makes each regular file a
//! file object over its bytes in the image (`SYS_MO_FROM_IMAGE`: nothing is
//! copied until a page is needed, and writes stay the instance's).

use crate::namespace::check;
use crate::syscall;
use crate::tmpfs::Inode;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use restricted::*;
use vfs::cpio::{header, member_path, HEADER_LEN, TRAILER};

/// Reads `len` bytes of the image at `pos`.
fn read(image: u64, pos: usize, len: usize) -> Result<alloc::vec::Vec<u8>, i64> {
    let mut buf = vec![0u8; len];
    check(syscall(SYS_MO_READ, [image, pos as u64, buf.as_mut_ptr() as u64, len as u64, 0, 0]))?;
    Ok(buf)
}

/// Fills `root` from the initramfs; a corrupt archive ends with what came
/// before it (as the kernel's own unpacking did).
pub fn unpack(root: &Arc<Inode>) {
    let mut size = 0u64;
    let image = syscall(SYS_INITRAMFS, [&mut size as *mut u64 as u64, 0, 0, 0, 0, 0]);
    if image < 0 {
        return;
    }
    let image = image as u64;
    let _ = members(root, image, size as usize);
    syscall(SYS_HANDLE_CLOSE, [image, 0, 0, 0, 0, 0]);
}

fn members(root: &Arc<Inode>, image: u64, size: usize) -> Result<(), i64> {
    const EINVAL: i64 = 22;
    let mut pos = 0;
    while pos + HEADER_LEN <= size {
        let h = header(&read(image, pos, HEADER_LEN)?).map_err(|_| EINVAL)?;
        let name = read(image, pos + HEADER_LEN, h.namesize.saturating_sub(1))?;
        let name = String::from_utf8(name).map_err(|_| EINVAL)?;
        if name == TRAILER {
            return Ok(());
        }
        let start = h.data_start(pos);
        if start.checked_add(h.filesize).is_none_or(|end| end > size) {
            return Err(EINVAL);
        }
        let path = member_path(&name);
        let (dir_path, base) = match path.rsplit_once('/') {
            Some((d, b)) => (d, b),
            None => ("", path),
        };
        if !base.is_empty() && base != "." {
            let mut dir = root.clone();
            for c in dir_path.split('/').filter(|c| !c.is_empty()) {
                dir = dir.subdir(c, 0o755)?;
            }
            let perm = h.mode & 0o7777;
            // One bad member is left out, not the rest.
            let _ = match h.mode & vfs::S_IFMT {
                vfs::S_IFDIR => dir.subdir(base, perm).map(|d| d.set_perm(perm)),
                vfs::S_IFREG => {
                    let object = check(syscall(SYS_MO_FROM_IMAGE, [image, start as u64, h.filesize as u64, 0, 0, 0]))?;
                    dir.insert_object(base, object as u64, perm)
                }
                vfs::S_IFLNK => {
                    let target = String::from_utf8(read(image, start, h.filesize)?).map_err(|_| EINVAL)?;
                    dir.symlink(base, target)
                }
                _ => Ok(()),
            };
        }
        pos = h.next(pos).ok_or(EINVAL)?;
    }
    Ok(())
}
