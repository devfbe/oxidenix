//! Named pipes (FIFOs, fifo(7)): open(2) of a FIFO node of the server's tmpfs or of /data
//! (mknod(2) and mkfifo(3) make them, `paths`) opens an end of the pipe the node keeps
//! while anyone has it open (`pipe::fifo_pipe`: tmpfs's inode, /data's `DInode`, whose
//! inode stays cached meanwhile, so every opener of the inode, by any path or process,
//! gets the same pipe; ext2 keeps only the node). The open waits for a partner as
//! Linux's fifo_open (`pipe::open_fifo`); the end is a pipe end (`File::Pipe`) in all
//! else: reads, writes, end of file, EPIPE and SIGPIPE, poll, select and epoll.
//!
//! The end keeps the node it was opened by (`Named`, its `Origin`): fstat is the node's,
//! live, the calls that change the node (fchmod, fchown, futimens) change it,
//! /proc/<pid>/fd/N shows its path and leads to the node (opening it there is a FIFO
//! open again). As on Linux, reads move the node's access time and writes its
//! modification and change times (on /data kept by the server until the inode's next
//! write-back, `datafs::fifo_used`), and inotify sees IN_OPEN, IN_ACCESS, IN_MODIFY, the
//! IN_CLOSE events and, for a node removed while open, IN_DELETE_SELF at its last
//! close.

use crate::datafs;
use crate::files::{self, File, O_ACCMODE};
use crate::inotify;
use crate::namespace::{Node, Origin};
use crate::pipe;
use alloc::string::String;
use core::sync::atomic::{fence, AtomicBool, Ordering};

const ENXIO: i64 = 6;
const ENOTDIR: i64 = 20;
const O_DIRECTORY: u32 = 0o200000;

/// What a FIFO's end knows of its node (see the module comment).
pub struct Named {
    origin: Origin,
    /// Opened for writing: its close is IN_CLOSE_WRITE.
    write: bool,
    /// Its open succeeded (IN_OPEN went out): its close is reported.
    opened: AtomicBool,
}

impl Named {
    /// The node and the path it was opened by.
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// The pipe was read from (`modify`: written to) through this end.
    pub fn used(&self, modify: bool) {
        match &self.origin.node {
            Node::Tmp(t) if modify => t.modified(),
            Node::Tmp(t) => t.accessed(),
            Node::Data(d) => datafs::fifo_used(d, modify),
            Node::Proc(_) => {}
        }
        inotify::node_event(&self.origin.node, if modify { inotify::IN_MODIFY } else { inotify::IN_ACCESS });
    }
}

impl Drop for Named {
    /// The end's last reference went (its description's, after `PipeEnd::close`), or its
    /// open failed: the close's events, and a removed node's last close.
    fn drop(&mut self) {
        if self.opened.load(Ordering::Relaxed) {
            let mask = if self.write { inotify::IN_CLOSE_WRITE } else { inotify::IN_CLOSE_NOWRITE };
            inotify::node_event(&self.origin.node, mask);
        }
        // (SeqCst, as `TmpOpen`'s and `DataOpen`'s drops: the removal's store and load of
        // `opens` and this see each other one way.)
        match &self.origin.node {
            Node::Tmp(t) => {
                let last = t.opens.fetch_sub(1, Ordering::SeqCst) == 1;
                fence(Ordering::SeqCst);
                if last && t.removed() {
                    inotify::deleted(inotify::Key::tmp(t), false);
                }
            }
            Node::Data(d) => {
                if d.opens.fetch_sub(1, Ordering::SeqCst) == 1 && d.unlinked() {
                    inotify::deleted(inotify::Key::data(d), false);
                }
            }
            Node::Proc(_) => {}
        }
        // (The origin's drop lets go of a /data inode: `datafs::let_go`.)
    }
}

/// open(2) of the FIFO `node` (resolved by `path`) with `flags`: a descriptor of the
/// calling process for an end of its pipe, once the open's waiting is over
/// (`pipe::open_fifo`: EINTR, ENXIO, EINVAL). The descriptor and its memory are had
/// first, as Linux has them before it opens (EMFILE, ENOMEM): no partner sees an end
/// that then fails to be installed. O_DIRECTORY is ENOTDIR; O_TRUNC does nothing.
pub fn open(node: Node, flags: u32, path: String) -> Result<i64, i64> {
    if flags & O_DIRECTORY != 0 {
        return Err(ENOTDIR);
    }
    let shared = match &node {
        Node::Tmp(t) => t.fifo_pipe()?,
        Node::Data(d) => datafs::fifo_pipe(d)?,
        // (/proc and /sys hold no FIFOs: `paths` opens a pipe /proc led to as `procfile`'s.)
        Node::Proc(_) => return Err(ENXIO),
    };
    let id = files::new_id();
    let watch = crate::poll::Watch::new(id)?;
    let room = match files::room() {
        Ok(room) => room,
        Err(e) => {
            crate::poll::forget(id, &watch);
            return Err(e);
        }
    };
    match &node {
        Node::Tmp(t) => t.opens.fetch_add(1, Ordering::AcqRel),
        Node::Data(d) => d.opens.fetch_add(1, Ordering::AcqRel),
        Node::Proc(_) => 0,
    };
    let named = Named { origin: Origin::new(node, path, vfs::S_IFIFO), write: flags & O_ACCMODE != 0, opened: AtomicBool::new(false) };
    let end = match pipe::open_fifo(shared, flags, named, id) {
        Ok(end) => end,
        Err(e) => {
            crate::poll::forget(id, &watch);
            return Err(e);
        }
    };
    let kept = files::status_flags(flags);
    match files::install_in_watched(room, id, File::Pipe(end.clone()), kept, watch) {
        Ok(fd) => {
            end.installed();
            if let Some(named) = end.named() {
                named.opened.store(true, Ordering::Relaxed);
                inotify::node_event(&named.origin.node, inotify::IN_OPEN);
            }
            Ok(fd)
        }
        Err(e) => {
            end.close();
            Err(e)
        }
    }
}
