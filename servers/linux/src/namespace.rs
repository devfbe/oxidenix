//! The server's namespace (phase R6c.2b): path resolution.
//!
//! The tree is, for now, the kernel's, reached through handles on its
//! inodes (`restricted::SYS_INODE_*`); the server resolves every path
//! itself: "." and ".." by name, before symlinks are looked at (as the
//! kernel's VFS did: "/a/link/.." is "/a"), and each symlink by reading it
//! and starting over from the root with its target in front of the rest
//! (at most 16, else ELOOP). One kernel call walks as many names as it can
//! and stops after a symlink, so a path without symlinks costs one call.

use crate::syscall;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use restricted::*;
use vfs::path::{join, normalize};

pub const ENOENT: i64 = 2;
pub const EEXIST: i64 = 17;
pub const ENOTDIR: i64 = 20;
pub const ELOOP: i64 = 40;

const MAX_LINKS: u32 = 16;

/// A handle on an inode of the kernel's tree, closed when dropped.
pub struct KInode(u64);

impl KInode {
    pub fn handle(&self) -> u64 {
        self.0
    }

    /// Takes a handle the kernel returned (or its error).
    pub fn from_result(r: i64) -> Result<KInode, i64> {
        if r < 0 { Err(-r) } else { Ok(KInode(r as u64)) }
    }

    /// Its `struct stat`.
    pub fn stat(&self) -> Result<[u8; 144], i64> {
        let mut st = [0u8; 144];
        check(syscall(SYS_INODE_STAT, [self.0, st.as_mut_ptr() as u64, 0, 0, 0, 0]))?;
        Ok(st)
    }

    pub fn readlink(&self) -> Result<String, i64> {
        let mut buf = alloc::vec![0u8; 4096];
        let n = check(syscall(SYS_INODE_READLINK, [self.0, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0]))?;
        buf.truncate(n as usize);
        String::from_utf8(buf).map_err(|_| ENOENT)
    }
}

impl Drop for KInode {
    fn drop(&mut self) {
        syscall(SYS_HANDLE_CLOSE, [self.0, 0, 0, 0, 0, 0]);
    }
}

pub fn check(r: i64) -> Result<i64, i64> {
    if r < 0 { Err(-r) } else { Ok(r) }
}

pub fn mode_of(st: &[u8; 144]) -> u32 {
    u32::from_le_bytes([st[24], st[25], st[26], st[27]])
}

/// The handle on the kernel's root, taken once for the instance and never
/// closed.
fn root() -> u64 {
    static ROOT: AtomicU64 = AtomicU64::new(0);
    let h = ROOT.load(Ordering::Acquire);
    if h != 0 {
        return h;
    }
    let new = syscall(SYS_INODE_ROOT, [0; 6]);
    if new <= 0 {
        // No handle left: the walks fail with it.
        return 0;
    }
    match ROOT.compare_exchange(0, new as u64, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => new as u64,
        Err(other) => {
            syscall(SYS_HANDLE_CLOSE, [new as u64, 0, 0, 0, 0, 0]);
            other
        }
    }
}

/// A resolved path: its inode, its mode, and its absolute path as given
/// (normalized, symlinks not replaced: what the descriptor and the working
/// directory remember).
pub struct Resolved {
    pub inode: KInode,
    pub mode: u32,
    pub path: Vec<String>,
}

/// Resolves `path` relative to the absolute directory `base`; `follow`:
/// a symlink as the last name is followed.
pub fn resolve(base: &str, path: &str, follow: bool) -> Result<Resolved, i64> {
    if path.is_empty() {
        return Err(ENOENT);
    }
    let given = normalize(base, path);
    let mut comps: VecDeque<String> = given.clone().into();
    let mut links = 0;
    loop {
        let rel = comps.iter().map(String::as_str).collect::<Vec<_>>().join("/");
        let mut walk = Walk::default();
        let inode = KInode::from_result(syscall(
            SYS_INODE_WALK,
            [root(), rel.as_ptr() as u64, rel.len() as u64, &mut walk as *mut Walk as u64, 0, 0],
        ))?;
        let is_link = walk.mode & vfs::S_IFMT == vfs::S_IFLNK;
        // Names walked, up to and with the symlink it stopped at.
        let walked = if rel.is_empty() { 0 } else { rel[..walk.consumed as usize].split('/').count() };
        let last = walked == comps.len();
        if !is_link || (last && !follow) {
            if !last {
                // A walk stops early only at a symlink.
                return Err(ENOENT);
            }
            return Ok(Resolved { inode, mode: walk.mode, path: given });
        }
        links += 1;
        if links > MAX_LINKS {
            return Err(ELOOP);
        }
        let target = inode.readlink()?;
        let before: Vec<String> = comps.iter().take(walked - 1).cloned().collect();
        let mut next: VecDeque<String> = normalize(&join(&before), &target).into();
        next.extend(comps.drain(walked..));
        comps = next;
    }
}

/// The directory a new name goes into and the name (EEXIST for the root,
/// which has none).
pub fn resolve_parent(base: &str, path: &str) -> Result<(Resolved, String), i64> {
    if path.is_empty() {
        return Err(ENOENT);
    }
    let mut comps = normalize(base, path);
    let name = comps.pop().ok_or(EEXIST)?;
    let parent = resolve("/", &join(&comps), true)?;
    if parent.mode & vfs::S_IFMT != vfs::S_IFDIR {
        return Err(ENOTDIR);
    }
    Ok((parent, name))
}
