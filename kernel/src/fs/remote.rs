//! Client side of a filesystem served by a user-space process (diskfs):
//! every operation becomes an IPC request in the `fsproto` format.

use super::{Inode, NewNode};
use crate::process::errno::*;
use crate::process::{ipc, Server};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use fsproto::{Op, MAX_DATA};
use spin::Mutex;

/// How often a dead server is restarted before its mount gives up.
const MAX_RESTARTS: u32 = 5;

/// Metadata of a remote inode.
#[derive(Clone, Copy)]
pub struct RemoteStat {
    pub mode: u32,
    pub size: u64,
    pub links: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
}

pub struct RemoteFs {
    service: AtomicUsize,
    /// The server behind the service, restarted after a crash.
    server: Arc<Server>,
    restarts: AtomicU32,
    restarting: AtomicBool,
    cache: Mutex<BTreeMap<u32, Weak<Inode>>>,
    /// Unlinked inodes still open here; released when the last VFS
    /// reference is dropped.
    deferred: Mutex<BTreeSet<u32>>,
}

struct Reply {
    status: i64,
    values: [u64; 6],
    payload: Vec<u8>,
}

impl RemoteFs {
    pub fn new(service: usize, server: Arc<Server>) -> Arc<RemoteFs> {
        Arc::new(RemoteFs {
            service: AtomicUsize::new(service),
            server,
            restarts: AtomicU32::new(0),
            restarting: AtomicBool::new(false),
            cache: Mutex::new(BTreeMap::new()),
            deferred: Mutex::new(BTreeSet::new()),
        })
    }

    /// Brings a dead server back: the first caller starts it again, others
    /// wait for it to register. Inode numbers live on disk, so every
    /// existing `Arc<Inode>` stays valid with the new server.
    fn revive(&self) -> Result<(), i64> {
        if let Some((service, _)) = ipc::lookup(self.server.name) {
            self.service.store(service, Ordering::Relaxed);
            return Ok(());
        }
        if self.restarting.swap(true, Ordering::Relaxed) {
            let (service, _) = ipc::wait_for(self.server.name, 3 * crate::process::TIMER_HZ).ok_or(EIO)?;
            self.service.store(service, Ordering::Relaxed);
            return Ok(());
        }
        let attempt = self.restarts.fetch_add(1, Ordering::Relaxed) + 1;
        let result = if attempt > MAX_RESTARTS {
            Err(EIO)
        } else {
            crate::printkln!("[kernel] {} died; restarting it (attempt {} of {})", self.server.name, attempt, MAX_RESTARTS);
            crate::process::spawn_server(&self.server)
                .ok()
                .and_then(|_| ipc::wait_for(self.server.name, 3 * crate::process::TIMER_HZ))
                .map(|(service, _)| self.service.store(service, Ordering::Relaxed))
                .ok_or(EIO)
        };
        self.restarting.store(false, Ordering::Relaxed);
        result
    }

    fn call(&self, op: Op, args: [u64; 4], payload: &[u8]) -> Result<Reply, i64> {
        // A request that was in flight when the server died stays EIO (it
        // may or may not have been carried out); the next one revives it.
        if !ipc::is_alive(self.service.load(Ordering::Relaxed)) {
            self.revive()?;
        }
        let message = fsproto::encode_request(op, args, payload);
        let raw = ipc::call(self.service.load(Ordering::Relaxed), message)?;
        let r = fsproto::decode_response(&raw).ok_or(EIO)?;
        if r.status < 0 {
            return Err(-r.status);
        }
        Ok(Reply { status: r.status, values: r.values, payload: r.payload.to_vec() })
    }

    /// The VFS inode for `ino`; one disk inode maps to one `Arc<Inode>`
    /// while it is in use.
    pub fn inode(self: &Arc<Self>, ino: u32) -> Arc<Inode> {
        let mut cache = self.cache.lock();
        if let Some(i) = cache.get(&ino).and_then(Weak::upgrade) {
            return i;
        }
        cache.retain(|_, w| w.strong_count() > 0);
        let inode = Inode::disk(self.clone(), ino);
        cache.insert(ino, Arc::downgrade(&inode));
        inode
    }

    pub fn stat(&self, ino: u32) -> Result<RemoteStat, i64> {
        let v = self.call(Op::Stat, [ino as u64, 0, 0, 0], &[])?.values;
        Ok(RemoteStat { mode: v[0] as u32, size: v[1], links: v[2], atime: v[3], mtime: v[4], ctime: v[5] })
    }

    pub fn read(&self, ino: u32, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        let mut done = 0;
        while done < buf.len() {
            let want = (buf.len() - done).min(MAX_DATA);
            let pos = off.checked_add(done as u64).ok_or(EINVAL)?;
            let r = self.call(Op::Read, [ino as u64, pos, want as u64, 0], &[])?;
            let n = r.payload.len().min(want);
            buf[done..done + n].copy_from_slice(&r.payload[..n]);
            done += n;
            if n < want {
                break;
            }
        }
        Ok(done)
    }

    pub fn write(&self, ino: u32, off: u64, data: &[u8]) -> Result<usize, i64> {
        let mut done = 0;
        while done < data.len() {
            let chunk = &data[done..(done + MAX_DATA).min(data.len())];
            let pos = off.checked_add(done as u64).ok_or(EFBIG)?;
            let n = match self.call(Op::Write, [ino as u64, pos, 0, 0], chunk) {
                Ok(r) => r.status as usize,
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            };
            done += n;
            if n < chunk.len() {
                break;
            }
        }
        Ok(done)
    }

    pub fn truncate(&self, ino: u32, len: u64) -> Result<(), i64> {
        self.call(Op::Truncate, [ino as u64, len, 0, 0], &[]).map(|_| ())
    }

    /// Entries as (name, inode, ext2 file type), including "." and "..".
    pub fn list(&self, dir: u32) -> Result<Vec<(String, u32, u8)>, i64> {
        let mut out = Vec::new();
        let mut cursor = 0;
        loop {
            let r = self.call(Op::List, [dir as u64, cursor, 0, 0], &[])?;
            for (ino, kind, name) in fsproto::entries(&r.payload) {
                out.push((String::from_utf8_lossy(name).into_owned(), ino, kind));
            }
            cursor = r.values[0];
            if cursor == 0 {
                return Ok(out);
            }
        }
    }

    pub fn lookup(&self, dir: u32, name: &str) -> Result<u32, i64> {
        Ok(self.call(Op::Lookup, [dir as u64, 0, 0, 0], name.as_bytes())?.values[0] as u32)
    }

    pub fn create(&self, dir: u32, name: &str, kind: &NewNode, perm: u32) -> Result<u32, i64> {
        let mut payload = Vec::from(name.as_bytes());
        let kind = match kind {
            NewNode::File => fsproto::KIND_FILE,
            NewNode::Dir => fsproto::KIND_DIR,
            NewNode::Symlink(target) => {
                payload.push(0);
                payload.extend_from_slice(target.as_bytes());
                fsproto::KIND_SYMLINK
            }
        };
        Ok(self.call(Op::Create, [dir as u64, kind, perm as u64, 0], &payload)?.values[0] as u32)
    }

    /// Inodes whose last link went away are released now, or deferred
    /// until the last open reference is dropped (see `forget`).
    fn settle(&self, gone: &[u8]) {
        for chunk in gone.chunks_exact(4) {
            let ino = u32::from_le_bytes(chunk.try_into().unwrap());
            let open = self.cache.lock().get(&ino).is_some_and(|w| w.strong_count() > 0);
            if open {
                self.deferred.lock().insert(ino);
            } else {
                self.release(ino);
            }
        }
    }

    fn release(&self, ino: u32) {
        ipc::post(self.service.load(Ordering::Relaxed), fsproto::encode_request(Op::Release, [ino as u64, 0, 0, 0], &[]));
    }

    pub fn unlink(&self, dir: u32, name: &str, want_dir: bool) -> Result<(), i64> {
        let r = self.call(Op::Unlink, [dir as u64, want_dir as u64, 0, 0], name.as_bytes())?;
        self.settle(&r.payload);
        Ok(())
    }

    pub fn rename(&self, odir: u32, oname: &str, ndir: u32, nname: &str) -> Result<(), i64> {
        let mut payload = Vec::from(oname.as_bytes());
        payload.extend_from_slice(nname.as_bytes());
        let r = self.call(Op::Rename, [odir as u64, ndir as u64, oname.len() as u64, 0], &payload)?;
        self.settle(&r.payload);
        Ok(())
    }

    pub fn readlink(&self, ino: u32) -> Result<String, i64> {
        let r = self.call(Op::Readlink, [ino as u64, 0, 0, 0], &[])?;
        String::from_utf8(r.payload).map_err(|_| EIO)
    }

    pub fn set_perm(&self, ino: u32, perm: u32) -> Result<(), i64> {
        self.call(Op::SetPerm, [ino as u64, perm as u64, 0, 0], &[]).map(|_| ())
    }

    /// (block size, total blocks, free blocks, total inodes, free inodes)
    pub fn usage(&self) -> (u64, u64, u64, u64, u64) {
        match self.call(Op::Usage, [0; 4], &[]) {
            Ok(r) => (r.values[0], r.values[1], r.values[2], r.values[3], r.values[4]),
            Err(_) => (1024, 0, 0, 0, 0),
        }
    }

    /// Called when the last VFS reference to `ino` is dropped. Must not
    /// sleep, hence `post`.
    pub fn forget(&self, ino: u32) {
        if self.deferred.lock().remove(&ino) {
            self.release(ino);
        }
    }
}
