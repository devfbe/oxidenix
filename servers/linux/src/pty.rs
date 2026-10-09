//! Pseudo-terminals (phase R6d, docs/design/linux-server.md "Pseudo-terminals"): opening
//! /dev/ptmx makes a pair, a master (`PtyMaster`, a file of the server) and a slave (a
//! terminal, `tty`, whose driver hands its output to the master). The slave's node
//! `/dev/pts/n` is in devpts, a tmpfs mounted at /dev/pts whose names only the server
//! makes: from the master's open until its close (with `ptmx`, mode 000, as Linux's).
//!
//! The slave is locked until TIOCSPTLCK unlocks it (`unlockpt`). A master write is the
//! slave's input (waiting while the slave's buffer is full in noncanonical mode); the
//! slave's output waits in the master's buffer (`MASTER_CAP`). The master reads EIO once
//! the slave's last descriptor went and nothing is left; its close hangs the slave up and
//! removes its node. The master's termios, window size and process group requests act on
//! the slave, as Linux's.

use crate::files::{self, File, EINVAL, O_ACCMODE, O_CLOEXEC, O_NONBLOCK};
use crate::sync::Mutex;
use crate::namespace::Origin;
use crate::tmpfs;
use crate::tty::{self, Driver, PtyState, Tty, EAGAIN, EIO, ENXIO};
use crate::unix::{Sink, Source};
use crate::usercopy;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ldisc::Termios;
use restricted::*;

const ENOSPC: i64 = 28;
const EFAULT: i64 = 14;

/// Most pairs at once (Linux's default `kernel.pty.max`).
const MAX_PTYS: u32 = 4096;
/// The slaves' major number (UNIX98_PTY_SLAVE_MAJOR).
pub const SLAVE_MAJOR: u32 = 136;

const TCFLSH: u32 = 0x540b;
const TIOCOUTQ: u32 = 0x5411;
const TIOCSTI: u32 = 0x5412;
const FIONREAD: u32 = 0x541b;
const TIOCGPTN: u32 = 0x8004_5430;
const TIOCSPTLCK: u32 = 0x4004_5431;
const TIOCSIG: u32 = 0x4004_5436;
const TIOCGPTLCK: u32 = 0x8004_5439;
const TIOCGPTPEER: u32 = 0x5441;

/// The pairs, by index.
static PTYS: Mutex<BTreeMap<u32, Arc<Tty>>> = Mutex::new(BTreeMap::new());
/// devpts's root.
static DEVPTS: Mutex<Option<Arc<tmpfs::Inode>>> = Mutex::new(None);

/// A pty's master: the open file description /dev/ptmx made.
pub struct PtyMaster {
    /// The slave's terminal.
    pub tty: Arc<Tty>,
    pub index: u32,
    /// The node it was opened by (/dev/ptmx: fstat, fchmod and the like).
    pub origin: Origin,
}

/// devpts's root (made at first use; the namespace mounts it at /dev/pts).
pub fn devpts() -> Arc<tmpfs::Inode> {
    let mut root = DEVPTS.lock();
    if let Some(r) = root.as_ref() {
        return r.clone();
    }
    let r = tmpfs::Inode::new_sealed_dir(0o755);
    // Linux's devpts has its own ptmx node (mode 000 unless mounted otherwise).
    let _ = r.insert_device("ptmx", vfs::stat::dev_make(5, 2), 0);
    *root = Some(r.clone());
    r
}

/// Every pty's slave terminal.
pub fn all() -> Vec<Arc<Tty>> {
    PTYS.lock().values().cloned().collect()
}

/// Opens /dev/ptmx: a new pair, its master's descriptor.
pub fn open_master(flags: u32, origin: Origin) -> Result<i64, i64> {
    let id = files::new_id();
    let (index, tty) = {
        let mut ptys = PTYS.lock();
        let index = (0..MAX_PTYS).find(|i| !ptys.contains_key(i)).ok_or(ENOSPC)?;
        let state = PtyState {
            out: VecDeque::new(),
            out_epoch: 0,
            locked: true,
            master_open: true,
            slaves: 0,
            slave_closed: false,
            master_id: id,
            master_reported: 0,
        };
        let tty = Tty::new(Driver::Pty(index), Termios::default(), [0; 4], Some(state));
        ptys.insert(index, tty.clone());
        (index, tty)
    };
    let node = devpts().insert_device(&index.to_string(), vfs::stat::dev_make(SLAVE_MAJOR, index), 0o620);
    let master = Arc::new(PtyMaster { tty: tty.clone(), index, origin });
    {
        let mut inner = tty.inner.lock();
        let ready = Tty::master_readiness(&inner);
        if let Some(p) = inner.pty.as_mut() {
            p.master_reported = ready;
        }
    }
    let fd = node.and_then(|_| files::install(id, File::PtyMaster(master.clone()), flags & (O_ACCMODE | O_NONBLOCK | O_CLOEXEC)));
    match fd {
        Ok(fd) => Ok(fd),
        Err(e) => {
            master_closed(&master);
            Err(e)
        }
    }
}

/// Opens /dev/pts/`index`: EIO while it is locked or its master is gone.
pub fn open_slave(index: u32, flags: u32, origin: Origin) -> Result<i64, i64> {
    let tty = PTYS.lock().get(&index).cloned().ok_or(ENXIO)?;
    {
        let inner = tty.inner.lock();
        let p = inner.pty.as_ref().ok_or(ENXIO)?;
        if p.locked || !p.master_open {
            return Err(EIO);
        }
    }
    tty::open(&tty, flags, origin, true)
}

/// The master's last descriptor went: the slave hangs up, its node goes.
pub fn master_closed(m: &PtyMaster) {
    {
        let mut inner = m.tty.inner.lock();
        if let Some(p) = inner.pty.as_mut() {
            p.master_open = false;
        }
        m.tty.changed(&mut inner, false, false);
    }
    m.tty.hangup(false);
    devpts().remove_device(&m.index.to_string());
    let mut ptys = PTYS.lock();
    if ptys.get(&m.index).is_some_and(|t| Arc::ptr_eq(t, &m.tty)) {
        ptys.remove(&m.index);
    }
}

impl PtyMaster {
    /// Its readiness for poll and epoll now.
    pub fn readiness_now(&self) -> i16 {
        crate::tty::Tty::master_readiness(&self.tty.inner.lock())
    }

    /// read(2): the slave's output, what is there; EIO once the slave's last descriptor
    /// went and nothing is left.
    pub fn read(&self, mut sink: Sink, nonblock: bool) -> Result<i64, i64> {
        if sink.room() == 0 {
            return Ok(0);
        }
        // One read at a time for the whole read (its copies go to the program with no
        // lock held; a flush meanwhile is seen by `out_epoch`).
        let _turn = self.tty.turn(tty::TURN_MASTER_READ, nonblock)?;
        let mut done = 0usize;
        let mut chunk = [0u8; 2048];
        loop {
            let (n, epoch) = {
                let inner = self.tty.inner.lock();
                let p = inner.pty.as_ref().ok_or(EIO)?;
                if p.out.is_empty() {
                    if done > 0 {
                        return Ok(done as i64);
                    }
                    if p.slave_closed {
                        return Err(EIO);
                    }
                    let seen = self.tty.seen();
                    drop(inner);
                    if nonblock {
                        return Err(EAGAIN);
                    }
                    self.tty.wait(seen, 0)?;
                    continue;
                }
                let n = p.out.len().min(chunk.len()).min(sink.room());
                for (d, s) in chunk[..n].iter_mut().zip(p.out.iter()) {
                    *d = *s;
                }
                (n, p.out_epoch)
            };
            let k = match sink.put(&chunk[..n]) {
                Ok(k) => k,
                Err(e) => return if done > 0 { Ok(done as i64) } else { Err(e) },
            };
            {
                let mut inner = self.tty.inner.lock();
                if let Some(p) = inner.pty.as_mut() {
                    if p.out_epoch == epoch {
                        p.out.drain(..k.min(p.out.len()));
                    }
                }
                self.tty.changed(&mut inner, false, false);
            }
            done += k;
            if k < n || sink.room() == 0 {
                return Ok(done as i64);
            }
        }
    }

    /// write(2): input for the slave.
    pub fn write(&self, src: Source, nonblock: bool) -> Result<i64, i64> {
        self.tty.master_write(src, nonblock)
    }

    /// ioctl(2) on the master: the pty's own requests; the others act on the slave.
    pub fn ioctl(&self, request: u64, arg: u64) -> Result<i64, i64> {
        match request as u32 {
            TIOCGPTN => {
                usercopy::write(arg, &self.index)?;
                Ok(0)
            }
            TIOCSPTLCK => {
                let lock: i32 = usercopy::read(arg)?;
                if let Some(p) = self.tty.inner.lock().pty.as_mut() {
                    p.locked = lock != 0;
                }
                Ok(0)
            }
            TIOCGPTLCK => {
                let locked = self.tty.inner.lock().pty.as_ref().is_some_and(|p| p.locked) as i32;
                usercopy::write(arg, &locked)?;
                Ok(0)
            }
            TIOCGPTPEER => {
                // A descriptor of the slave from the master (Linux's ptm_open_peer).
                const O_NOCTTY: u32 = 0o400;
                let flags = arg as u32 & (O_ACCMODE | O_NONBLOCK | O_CLOEXEC | O_NOCTTY);
                let name = self.index.to_string();
                let node = devpts().lookup(&name).map_err(|_| EIO)?;
                let path = alloc::format!("/dev/pts/{}", name);
                open_slave(self.index, flags, Origin::new(crate::namespace::Node::Tmp(node), path, vfs::S_IFCHR))
            }
            TIOCSIG => {
                // Only the terminal's own signals (Linux's `pty_signal`).
                const SIGINT: u64 = 2;
                const SIGQUIT: u64 = 3;
                const SIGTSTP: u64 = 20;
                let sig = arg;
                if !matches!(sig, SIGINT | SIGQUIT | SIGTSTP) {
                    return Err(EINVAL);
                }
                if let Some(fg) = self.tty.inner.lock().pgrp {
                    crate::syscall(SYS_SIGNAL_GROUP, [SIGNAL_PGRP, fg, sig, 0, 0, 0]);
                }
                Ok(0)
            }
            FIONREAD => {
                let n = self.tty.inner.lock().pty.as_ref().map_or(0, |p| p.out.len()) as i32;
                usercopy::write(arg, &n)?;
                Ok(0)
            }
            TIOCOUTQ => {
                usercopy::write(arg, &0i32)?;
                Ok(0)
            }
            TCFLSH => {
                // The master's input is the slave's output.
                if arg > 2 {
                    return Err(EINVAL);
                }
                if arg != 1 {
                    let mut inner = self.tty.inner.lock();
                    if let Some(p) = inner.pty.as_mut() {
                        p.out.clear();
                        p.out_epoch += 1;
                    }
                    self.tty.changed(&mut inner, false, false);
                }
                Ok(0)
            }
            TIOCSTI => {
                // Into the master's own input; dropped when that is full (as a
                // full line discipline drops it on Linux).
                let b: u8 = usercopy::read(arg)?;
                let mut inner = self.tty.inner.lock();
                if let Some(p) = inner.pty.as_mut() {
                    if p.out.len() < tty::MASTER_CAP {
                        p.out.push_back(b);
                    }
                }
                self.tty.changed(&mut inner, false, true);
                Ok(0)
            }
            _ => self.tty.ioctl(None, request, arg),
        }
    }
}

/// The calls on a pty's master.
pub fn call(nr: u64, m: &Arc<PtyMaster>, flags: u32, a1: u64, a2: u64) -> Result<i64, i64> {
    use crate::files::{SYS_FSTAT, SYS_IOCTL, SYS_READ, SYS_READV, SYS_WRITE, SYS_WRITEV};
    let readable = flags & O_ACCMODE != files::O_WRONLY;
    let writable = flags & O_ACCMODE != 0;
    let nonblock = flags & O_NONBLOCK != 0;
    match nr {
        SYS_READ | SYS_READV if !readable => Err(files::EBADF),
        SYS_WRITE | SYS_WRITEV if !writable => Err(files::EBADF),
        SYS_READ => m.read(Sink::program(&[(a1, a2)]), nonblock),
        SYS_WRITE => m.write(Source::program(&[(a1, a2)]), nonblock),
        SYS_READV => {
            let vecs = files::iovecs(a1, a2)?;
            m.read(Sink::program(&vecs), nonblock)
        }
        SYS_WRITEV => {
            let vecs = files::iovecs(a1, a2)?;
            m.write(Source::program(&vecs), nonblock)
        }
        SYS_FSTAT => usercopy::to_program(a1, &m.origin.stat()?).map(|_| 0).map_err(|_| EFAULT),
        SYS_IOCTL => m.ioctl(a1, a2),
        files::SYS_LSEEK | files::SYS_PREAD64 | files::SYS_PWRITE64 | files::SYS_PREADV | files::SYS_PWRITEV => Err(files::ESPIPE),
        files::SYS_GETDENTS64 => Err(files::ENOTDIR),
        _ => Err(EINVAL),
    }
}
