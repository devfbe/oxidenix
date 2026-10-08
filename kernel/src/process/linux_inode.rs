//! The kernel's tree for the Linux server, through handles on its inodes
//! (phase R6c.2b, `restricted::SYS_INODE_*`): the server resolves paths and
//! implements the calls that take one; the kernel's filesystems (tmpfs,
//! the remote filesystems of diskfs and procfs, the devices) answer the
//! operations on the inodes it reaches, until the server's own filesystems
//! serve them.
//!
//! Names and paths come from the server's memory as (pointer, length), at
//! most 4096 bytes. A walk takes names only: the server resolves "." and
//! ".." (lexically, as the kernel's VFS did) and every symlink, so a walk
//! stops after the first symlink it reaches.

use super::errno::*;
use super::linux::{ExecTarget, Instance, Object};
use super::uaccess::{copy_from_server, copy_to_server};
use super::with_current;
use crate::fs::{self, Inode, NewNode};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use restricted::*;

const PATH_MAX: u64 = 4096;

/// A string from the server's memory.
fn string(ptr: u64, len: u64) -> Result<String, i64> {
    if len > PATH_MAX {
        return Err(ENAMETOOLONG);
    }
    // An empty string may come with any pointer (Rust's dangle).
    if len == 0 {
        return Ok(String::new());
    }
    let mut bytes = vec![0u8; len as usize];
    copy_from_server(ptr, &mut bytes)?;
    String::from_utf8(bytes).map_err(|_| EINVAL)
}

/// One name of a directory: no separator, not "." or "..".
fn name(ptr: u64, len: u64) -> Result<String, i64> {
    let n = string(ptr, len)?;
    if n.is_empty() || n == "." || n == ".." || n.contains('/') {
        return Err(EINVAL);
    }
    if n.len() > fs::NAME_MAX {
        return Err(ENAMETOOLONG);
    }
    Ok(n)
}

fn inode(instance: &Instance, handle: u64) -> Result<Arc<Inode>, i64> {
    match instance.object(handle)? {
        Object::Inode(i) => Ok(i),
        _ => Err(EINVAL),
    }
}

fn dir(instance: &Instance, handle: u64) -> Result<Arc<Inode>, i64> {
    let d = inode(instance, handle)?;
    if !d.is_dir() {
        return Err(ENOTDIR);
    }
    Ok(d)
}

fn give(instance: &Instance, inode: Arc<Inode>) -> SysResult {
    Ok(instance.insert(Object::Inode(inode))? as i64)
}

/// The `SYS_INODE_*` calls, `SYS_KFD_INODE` and `SYS_EXEC_TARGET`.
pub fn call(instance: &Arc<Instance>, nr: u64, a: [u64; 6]) -> SysResult {
    match nr {
        SYS_INODE_ROOT => give(instance, fs::root()),
        SYS_INODE_WALK => {
            let (start, path, out) = (inode(instance, a[0])?, string(a[1], a[2])?, a[3]);
            let mut cur = start;
            let mut consumed = 0;
            for part in path.split('/') {
                consumed += part.len() as u64;
                if part.is_empty() {
                    consumed += 1;
                    continue;
                }
                if part == "." || part == ".." {
                    return Err(EINVAL);
                }
                if !cur.is_dir() {
                    return Err(ENOTDIR);
                }
                cur = cur.child(part)?;
                if cur.file_type() == fs::S_IFLNK {
                    break;
                }
                consumed += 1;
            }
            let walk = Walk { consumed: consumed.min(path.len() as u64), mode: cur.mode(), _pad: 0 };
            let bytes = unsafe { core::slice::from_raw_parts(&walk as *const Walk as *const u8, core::mem::size_of::<Walk>()) };
            copy_to_server(out, bytes)?;
            give(instance, cur)
        }
        SYS_INODE_STAT => {
            let st = super::sys_file::inode_stat(&*inode(instance, a[0])?)?;
            copy_to_server(a[1], &st)?;
            Ok(0)
        }
        SYS_INODE_READLINK => {
            let target = inode(instance, a[0])?.readlink()?;
            let n = target.len().min(a[2] as usize);
            copy_to_server(a[1], &target.as_bytes()[..n])?;
            Ok(n as i64)
        }
        SYS_INODE_CREATE => {
            let (d, n) = (dir(instance, a[0])?, name(a[1], a[2])?);
            let kind = match a[3] {
                INODE_FILE => NewNode::File,
                INODE_DIR => NewNode::Dir,
                _ => return Err(EINVAL),
            };
            let created = d.create(&n, kind, a[4] as u32 & 0o7777)?;
            give(instance, created)
        }
        SYS_INODE_SYMLINK => {
            let (d, n, target) = (dir(instance, a[0])?, name(a[1], a[2])?, string(a[3], a[4])?);
            d.create(&n, NewNode::Symlink(target), 0o777)?;
            Ok(0)
        }
        SYS_INODE_UNLINK => {
            let (d, n) = (dir(instance, a[0])?, name(a[1], a[2])?);
            d.unlink(&n, a[3] != 0)?;
            Ok(0)
        }
        SYS_INODE_RENAME => {
            let (odir, oname) = (dir(instance, a[0])?, name(a[1], a[2])?);
            let (ndir, nname) = (dir(instance, a[3])?, name(a[4], a[5])?);
            fs::rename(&odir, &oname, &ndir, &nname)?;
            Ok(0)
        }
        SYS_INODE_CHMOD => {
            inode(instance, a[0])?.set_perm(a[1] as u32 & 0o7777)?;
            Ok(0)
        }
        SYS_INODE_TRUNCATE => {
            let i = inode(instance, a[0])?;
            if i.is_dir() {
                return Err(EISDIR);
            }
            let _access = i.get_write_access()?;
            i.truncate(a[1])?;
            Ok(0)
        }
        SYS_INODE_OPEN => {
            let (i, flags, path) = (inode(instance, a[0])?, a[1] as u32, string(a[2], a[3])?);
            if !path.starts_with('/') {
                return Err(EINVAL);
            }
            super::sys_file::open_inode(i, flags, path)
        }
        SYS_INODE_STATFS => {
            let words = super::sys_file::statfs_words(&*inode(instance, a[0])?);
            let bytes: alloc::vec::Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            copy_to_server(a[1], &bytes)?;
            Ok(0)
        }
        SYS_KFD_INODE => {
            let (fd, buf, cap, len_out) = (a[0], a[1], a[2], a[3]);
            let f = with_current(|p| p.file(fd))?;
            let i = f.inode().cloned().ok_or(ENOTDIR)?;
            let path = f.path.clone().ok_or(ENOTDIR)?;
            if path.len() as u64 > cap {
                return Err(ENAMETOOLONG);
            }
            copy_to_server(buf, path.as_bytes())?;
            copy_to_server(len_out, &(path.len() as u64).to_le_bytes())?;
            give(instance, i)
        }
        SYS_EXEC_TARGET => {
            let target = match instance.object(a[0])? {
                Object::Inode(i) => ExecTarget::Inode(i),
                Object::File(cache, hold) => ExecTarget::File(cache, hold),
                _ => return Err(EINVAL),
            };
            let path = string(a[1], a[2])?;
            if !path.starts_with('/') {
                return Err(EINVAL);
            }
            with_current(|p| p.linux.as_mut().map(|l| l.exec_target = Some((target, path)))).ok_or(EPERM)?;
            Ok(0)
        }
        _ => Err(ENOSYS),
    }
}
