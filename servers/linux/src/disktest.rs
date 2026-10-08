//! `TEST_DISKRING`: the client's side of the file protocol (`fsring`)
//! against diskfs, as the page cache will use it in step 4: a channel to
//! diskfs, grants of memory objects, files on /data read and written by
//! DMA into and out of the granted pages, metadata, flushes, malformed
//! requests, several requests in flight, grants revoked and a client
//! gone in the middle.

use crate::syscall;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::AtomicU32;
use fsring::errno::*;
use fsring::{op, Buf, Completion, Kind, Request, Stat, Usage, SERVICE};
use restricted::*;
use ring::channel::{Header, Layout};
use ring::{Consumer, Desc, Producer, Ring, Wait};

const N: usize = fsring::SLOTS as usize;
const PAGE: u64 = 4096;
const ROOT: u32 = 2;
/// How long a request may take before the test fails instead of hanging.
const TIMEOUT: u64 = 10_000_000_000;
const ETIMEDOUT: i64 = 110;
const EPROTO: i64 = 71;
const ENOSPC_RING: i64 = 28;
const EEXIST: i64 = 17;
/// The data disk's README (userspace/disk), which the test reads back.
const README: &[u8] = include_bytes!("../../../userspace/disk/README.txt");
/// The file the test leaves for lxtest, which reads it through /data (the
/// server's page cache, another channel) and removes it: `CROSS_LEN` bytes,
/// byte `i` being `i % 251`.
const CROSS: &[u8] = b"ringtest.bin";
const CROSS_LEN: usize = 70_000;

fn now() -> u64 {
    syscall(SYS_CLOCK_READ, [1, 0, 0, 0, 0, 0]) as u64
}

struct Doorbell {
    deadline: u64,
}

impl Wait for Doorbell {
    fn wait(&self, word: &AtomicU32, value: u32) {
        syscall(SYS_SERVER_FUTEX_WAIT, [word as *const AtomicU32 as u64, value as u64, self.deadline, 0, 0, 0]);
    }

    fn wake(&self, word: &AtomicU32) {
        syscall(SYS_SERVER_FUTEX_WAKE, [word as *const AtomicU32 as u64, 1, 0, 0, 0, 0]);
    }
}

/// A memory object of `pages` pages.
struct Object {
    handle: u64,
    pages: u64,
}

impl Object {
    fn new(pages: u64) -> Result<Object, i64> {
        let h = syscall(SYS_MO_CREATE, [pages, 0, 0, 0, 0, 0]);
        if h < 0 {
            return Err(h);
        }
        Ok(Object { handle: h as u64, pages })
    }

    fn write(&self, off: u64, data: &[u8]) {
        syscall(SYS_MO_WRITE, [self.handle, off, data.as_ptr() as u64, data.len() as u64, 0, 0]);
    }

    fn read(&self, off: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        syscall(SYS_MO_READ, [self.handle, off, buf.as_mut_ptr() as u64, len as u64, 0, 0]);
        buf
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        syscall(SYS_HANDLE_CLOSE, [self.handle, 0, 0, 0, 0, 0]);
    }
}

struct Client {
    handle: u64,
    header: &'static Header,
    requests: Producer<'static, N>,
    completions: Consumer<'static, N>,
    tag: u64,
}

impl Client {
    fn open() -> Result<Client, i64> {
        let mut addr = 0u64;
        let h = syscall(SYS_CHAN_CREATE, [N as u64, &mut addr as *mut u64 as u64, 0, 0, 0, 0]);
        if h < 0 {
            return Err(h);
        }
        let layout = Layout::new(N as u32).expect("a valid slot count");
        let base = addr as *const u8;
        // Mapped until the handle is closed (`drop`).
        let (sub, comp) = unsafe { (layout.ring::<N>(base, layout.submission), layout.ring::<N>(base, layout.completion)) };
        let c = Client { handle: h as u64, header: unsafe { Header::at(base) }, requests: Ring::new(sub).producer(), completions: Ring::new(comp).consumer(), tag: 0 };
        match syscall(SYS_CHAN_CONNECT, [c.handle, SERVICE.as_ptr() as u64, SERVICE.len() as u64, 0, 0, 0]) {
            0 => Ok(c),
            e => Err(e),
        }
    }

    fn grant(&self, o: &Object, writable: bool) -> i64 {
        syscall(SYS_GRANT, [self.handle, o.handle, 0, o.pages, if writable { GRANT_WRITE } else { 0 }, 0])
    }

    fn revoke(&self, grant: u32) -> i64 {
        syscall(SYS_REVOKE, [self.handle, grant as u64, 0, 0, 0, 0])
    }

    /// Sends descriptors without waiting; their tags.
    fn send(&mut self, ds: &[Desc]) -> Result<Vec<u64>, i64> {
        let mut tags = Vec::new();
        for d in ds {
            self.tag += 1;
            let d = Desc { tag: self.tag, ..*d };
            if !self.requests.push(&d) {
                return Err(-ENOSPC_RING);
            }
            tags.push(self.tag);
        }
        self.requests.ring_doorbell(&Doorbell { deadline: 0 });
        Ok(tags)
    }

    /// The next completion.
    fn next(&mut self) -> Result<Completion, i64> {
        let deadline = now() + TIMEOUT;
        let header = self.header;
        match self.completions.pop_wait_while(&Doorbell { deadline }, || header.state() == 0 && now() < deadline) {
            Some(c) => {
                // Room for a completion: requests still waiting for it
                // may be taken now (fsring, "Room").
                if self.requests.room() < N {
                    self.requests.ring_doorbell(&Doorbell { deadline: 0 });
                }
                Ok(Completion::from_desc(&c))
            }
            None if header.state() != 0 => Err(-(EPROTO)),
            None => Err(-ETIMEDOUT),
        }
    }

    /// Sends one descriptor and waits for its completion.
    fn raw(&mut self, d: Desc) -> Result<Completion, i64> {
        let tag = self.send(&[d])?[0];
        let c = self.next()?;
        if c.tag != tag || c.op != d.op {
            return Err(-EPROTO);
        }
        Ok(c)
    }

    fn call(&mut self, r: Request) -> Result<Completion, i64> {
        self.raw(r.encode(0))
    }

    /// The status of `r` (or the failure to get one).
    fn status(&mut self, r: Request) -> i64 {
        match self.call(r) {
            Ok(c) => c.status,
            Err(e) => e,
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        syscall(SYS_HANDLE_CLOSE, [self.handle, 0, 0, 0, 0, 0]);
    }
}

/// A grant for names: `name` at the start of a fresh object.
struct Names {
    object: Object,
    grant: u32,
}

impl Names {
    fn new(c: &Client) -> Result<Names, i64> {
        let object = Object::new(1)?;
        let g = c.grant(&object, true);
        if g <= 0 {
            return Err(g);
        }
        Ok(Names { object, grant: g as u32 })
    }

    /// `a` (and `b` right after it) in the grant: their buffers.
    fn put(&self, a: &[u8], b: &[u8]) -> (Buf, Buf) {
        let mut page = vec![0u8; PAGE as usize];
        page[..a.len()].copy_from_slice(a);
        page[a.len()..a.len() + b.len()].copy_from_slice(b);
        self.object.write(0, &page);
        (Buf { grant: self.grant, offset: 0, len: a.len() as u32 }, Buf { grant: self.grant, offset: a.len() as u32, len: b.len() as u32 })
    }
}

fn lookup(c: &mut Client, names: &Names, dir: u32, name: &[u8]) -> Result<u32, i64> {
    let (name, _) = names.put(name, b"");
    let r = c.call(Request::Lookup { dir, name })?;
    if r.status < 0 { Err(r.status) } else { Ok(r.values[0] as u32) }
}

/// Removes `name` from the root if it is there (left by an earlier run).
fn remove(c: &mut Client, names: &Names, name: &[u8]) -> Result<(), i64> {
    let (n, _) = names.put(name, b"");
    let r = c.call(Request::Unlink { dir: ROOT, name: n, is_dir: false })?;
    if r.status == 0 && r.values[0] != 0 {
        let r = c.call(Request::Release { ino: r.values[0] as u32 })?;
        if r.status != 0 {
            return Err(r.status);
        }
    }
    Ok(())
}

fn create(c: &mut Client, names: &Names, name: &[u8]) -> Result<u32, i64> {
    remove(c, names, name)?;
    let (n, _) = names.put(name, b"");
    let r = c.call(Request::Create { dir: ROOT, name: n, kind: Kind::File, perm: 0o644 })?;
    if r.status < 0 { Err(r.status) } else { Ok(r.values[0] as u32) }
}

fn buf(grant: u32, offset: u64, len: usize) -> Buf {
    Buf { grant, offset: offset as u32, len: len as u32 }
}

macro_rules! check {
    ($n:expr, $cond:expr) => {
        if !$cond {
            return Err($n);
        }
    };
}

pub fn run(scenario: u64) -> i64 {
    let result = match scenario {
        1 => reading(),
        2 => writing(),
        3 => errors(),
        4 => in_flight(),
        5 => revoked_and_gone(),
        6 => holds(),
        7 => stalls_with_full_slots(),
        8 => block_on_room(),
        9 => unblock(),
        _ => Err(1000),
    };
    match result {
        Ok(()) => 0,
        Err(n) => -n,
    }
}

/// A file of the disk image read by DMA into a granted page; metadata.
fn reading() -> Result<(), i64> {
    let mut c = Client::open().map_err(|_| 1)?;
    let names = Names::new(&c).map_err(|_| 2)?;
    let ino = lookup(&mut c, &names, ROOT, b"README.txt").map_err(|_| 3)?;
    let st = c.call(Request::Stat { ino }).map_err(|_| 4)?;
    check!(5, st.status == 0);
    let stat = Stat::from_values(&st.values);
    check!(6, stat.size == README.len() as u64 && stat.mode & 0o170000 == 0o100000 && stat.links == 1);
    let data = Object::new(2).map_err(|_| 7)?;
    data.write(0, &[0xee; 2 * PAGE as usize]);
    let g = c.grant(&data, true);
    check!(8, g > 0);
    // From an unaligned offset of the file into an unaligned offset of the
    // grant, asking for more than there is.
    let r = c.call(Request::Read { ino, offset: 3, buf: buf(g as u32, 100, 8000) }).map_err(|_| 9)?;
    check!(10, r.status == README.len() as i64 - 3 && r.values[0] == README.len() as u64);
    let got = data.read(0, 2 * PAGE as usize);
    check!(11, got[..100].iter().all(|&b| b == 0xee));
    check!(12, got[100..100 + README.len() - 3] == README[3..]);
    check!(13, got[100 + README.len() - 3..].iter().all(|&b| b == 0xee));
    // At and beyond the end: nothing.
    check!(14, c.status(Request::Read { ino, offset: README.len() as u64, buf: buf(g as u32, 0, 10) }) == 0);
    // The root's entries.
    let r = c.call(Request::Readdir { dir: ROOT, cursor: 0, buf: buf(g as u32, 0, 2 * PAGE as usize) }).map_err(|_| 15)?;
    check!(16, r.status > 0 && r.values[0] == 0);
    let entries = data.read(0, r.status as usize);
    check!(17, fsring::dirents(&entries).any(|(i, t, n)| i == ino && t == fsring::TYPE_FILE && n == b"README.txt"));
    check!(18, fsring::dirents(&entries).any(|(i, t, n)| i == ROOT && t == fsring::TYPE_DIR && n == b".."));
    // A small buffer: the listing goes on at the cursor.
    let r = c.call(Request::Readdir { dir: ROOT, cursor: 0, buf: buf(g as u32, 0, 20) }).map_err(|_| 19)?;
    check!(20, r.status > 0 && r.values[0] > 0);
    let u = c.call(Request::Statfs).map_err(|_| 21)?;
    let usage = Usage::from_values(&u.values);
    // The builder's disks (the persistent one or the tests' own) are whole
    // MiB in 1 KiB blocks; /data's own channel (the page cache's) sees the
    // same filesystem.
    let through_data = crate::datafs::usage().map_err(|_| 21)?;
    check!(22, u.status == 0 && usage.block_size == 1024 && usage.blocks % 1024 == 0 && usage.blocks >= 64 * 1024
        && usage.blocks == through_data.blocks && usage.block_size == through_data.block_size && usage.free_blocks < usage.blocks);
    // A symlink, made and read back, then gone.
    let _ = remove(&mut c, &names, b"ringtest.link");
    let (n, target) = names.put(b"ringtest.link", b"../some/target");
    let r = c.call(Request::Create { dir: ROOT, name: n, kind: Kind::Symlink(target), perm: 0 }).map_err(|_| 23)?;
    check!(24, r.status == 0);
    let link = r.values[0] as u32;
    let r = c.call(Request::Readlink { ino: link, buf: buf(g as u32, 0, 64) }).map_err(|_| 25)?;
    check!(26, r.status == 14 && data.read(0, 14) == b"../some/target");
    check!(27, c.status(Request::Readlink { ino: link, buf: buf(g as u32, 0, 5) }) == -ERANGE);
    remove(&mut c, &names, b"ringtest.link").map_err(|_| 28)?;
    check!(29, lookup(&mut c, &names, ROOT, b"ringtest.link") == Err(-ENOENT));
    Ok(())
}

/// The model of a file: what the ring wrote.
fn pattern(i: usize, salt: u8) -> u8 {
    (i % 251) as u8 ^ salt
}

/// Writes (aligned, unaligned, past the end leaving a hole), a flush, the
/// file read back, truncated, renamed; the file lxtest checks is left.
fn writing() -> Result<(), i64> {
    let mut c = Client::open().map_err(|_| 30)?;
    let names = Names::new(&c).map_err(|_| 31)?;
    let ino = create(&mut c, &names, b"ringtest.tmp").map_err(|_| 32)?;
    let src = Object::new(32).map_err(|_| 33)?;
    let mut model = vec![0u8; 0];
    let put = |model: &mut Vec<u8>, off: usize, data: &[u8]| {
        if model.len() < off + data.len() {
            model.resize(off + data.len(), 0);
        }
        model[off..off + data.len()].copy_from_slice(data);
    };
    let g = c.grant(&src, false);
    check!(34, g > 0);
    let g = g as u32;
    // 64 KiB from offset 0, page-aligned.
    let a: Vec<u8> = (0..65536).map(|i| pattern(i, 0)).collect();
    src.write(0, &a);
    let r = c.call(Request::Write { ino, offset: 0, buf: buf(g, 0, a.len()) }).map_err(|_| 35)?;
    check!(36, r.status == 65536 && r.values[0] == 65536);
    put(&mut model, 0, &a);
    // Unaligned within existing blocks (sectors read first), from an
    // unaligned offset of the grant.
    let b: Vec<u8> = (0..3000).map(|i| pattern(i, 0x5a)).collect();
    src.write(70_000, &b);
    check!(37, c.status(Request::Write { ino, offset: 1000, buf: buf(g, 70_000, b.len()) }) == 3000);
    put(&mut model, 1000, &b);
    // Within one sector.
    check!(38, c.status(Request::Write { ino, offset: 20_000, buf: buf(g, 70_000, 10) }) == 10);
    put(&mut model, 20_000, &b[..10]);
    // Past the end, unaligned, into a new block: a hole before it.
    src.write(80_000, b"tail");
    let r = c.call(Request::Write { ino, offset: 100 * 1024 + 10, buf: buf(g, 80_000, 4) }).map_err(|_| 39)?;
    check!(40, r.status == 4 && r.values[0] == 100 * 1024 + 14);
    put(&mut model, 100 * 1024 + 10, b"tail");
    check!(41, c.status(Request::Flush) == 0);
    let st = Stat::from_values(&c.call(Request::Stat { ino }).map_err(|_| 42)?.values);
    check!(43, st.size == model.len() as u64);
    // Read back: by DMA into another grant.
    let dst = Object::new(32).map_err(|_| 44)?;
    let gd = c.grant(&dst, true);
    check!(45, gd > 0);
    let gd = gd as u32;
    let r = c.call(Request::Read { ino, offset: 0, buf: buf(gd, 0, 128 * 1024) }).map_err(|_| 46)?;
    check!(47, r.status == model.len() as i64);
    check!(48, dst.read(0, model.len()) == model);
    // Truncation: shorter, then longer (zeros, not old data).
    check!(49, c.status(Request::Truncate { ino, len: 1500 }) == 0);
    check!(50, c.status(Request::Truncate { ino, len: 5000 }) == 0);
    let r = c.call(Request::Read { ino, offset: 0, buf: buf(gd, 0, 8192) }).map_err(|_| 51)?;
    let back = dst.read(0, 5000);
    check!(52, r.status == 5000 && back[..1500] == model[..1500] && back[1500..].iter().all(|&x| x == 0));
    // Rename over nothing, then remove.
    let _ = remove(&mut c, &names, b"ringtest.moved");
    let (old, new) = names.put(b"ringtest.tmp", b"ringtest.moved");
    let r = c.call(Request::Rename { from: ROOT, name: old, to: ROOT, new_name: new }).map_err(|_| 53)?;
    check!(54, r.status == 0 && r.values[0] == 0);
    check!(55, lookup(&mut c, &names, ROOT, b"ringtest.moved") == Ok(ino));
    check!(56, c.status(Request::SetPerm { ino, perm: 0o600 }) == 0);
    let st = Stat::from_values(&c.call(Request::Stat { ino }).map_err(|_| 57)?.values);
    check!(58, st.mode & 0o7777 == 0o600);
    remove(&mut c, &names, b"ringtest.moved").map_err(|_| 59)?;
    // The file lxtest reads through /data.
    let ino = create(&mut c, &names, CROSS).map_err(|_| 60)?;
    let cross: Vec<u8> = (0..CROSS_LEN).map(|i| pattern(i, 0)).collect();
    src.write(0, &cross);
    check!(61, c.status(Request::Write { ino, offset: 0, buf: buf(g, 0, CROSS_LEN) }) == CROSS_LEN as i64);
    check!(62, c.status(Request::Flush) == 0);
    Ok(())
}

/// Malformed requests complete with errors; the channel goes on.
fn errors() -> Result<(), i64> {
    let mut c = Client::open().map_err(|_| 70)?;
    let names = Names::new(&c).map_err(|_| 71)?;
    let ino = lookup(&mut c, &names, ROOT, b"README.txt").map_err(|_| 72)?;
    let data = Object::new(1).map_err(|_| 73)?;
    let rw = c.grant(&data, true);
    let ro = c.grant(&data, false);
    check!(74, rw > 0 && ro > 0);
    let (rw, ro) = (rw as u32, ro as u32);
    let read = |grant, offset: u64, len: usize| Request::Read { ino, offset: 0, buf: buf(grant, offset, len) };
    check!(75, c.raw(Desc { op: 99, ..Desc::default() }).map(|r| r.status) == Ok(-ENOSYS));
    check!(76, c.raw(Desc { flags: 1, ..read(rw, 0, 10).encode(0) }).map(|r| r.status) == Ok(-EINVAL));
    check!(77, c.raw(Desc { arg: [1, 0, 0], ..read(rw, 0, 10).encode(0) }).map(|r| r.status) == Ok(-EINVAL));
    check!(78, c.status(read(999, 0, 10)) == -EBADF);
    check!(79, c.status(read(rw, PAGE - 10, 11)) == -EINVAL);
    check!(80, c.status(read(rw, 0, fsring::MAX_TRANSFER as usize + 1)) == -EINVAL);
    check!(81, c.status(read(ro, 0, 10)) == -EACCES);
    for dead in [0u32, 999_999, 16_000] {
        check!(82, c.status(Request::Stat { ino: dead }) == -ENOENT);
        check!(83, c.status(Request::Read { ino: dead, offset: 0, buf: buf(rw, 0, 10) }) == -ENOENT);
    }
    check!(84, c.status(Request::Read { ino: ROOT, offset: 0, buf: buf(rw, 0, 10) }) == -EINVAL);
    check!(85, lookup(&mut c, &names, ROOT, b"a/b") == Err(-EINVAL));
    check!(86, lookup(&mut c, &names, ROOT, b"no such file") == Err(-ENOENT));

    check!(87, c.raw(Desc { op: op::LOOKUP, object: ROOT as u64, grant: names.grant, len: 256, ..Desc::default() }).map(|r| r.status) == Ok(-ENAMETOOLONG));

    check!(88, c.status(Request::Readdir { dir: ROOT, cursor: 0, buf: buf(ro, 0, 100) }) == -EACCES);
    check!(89, c.status(Request::Readdir { dir: ino, cursor: 0, buf: buf(rw, 0, 100) }) == -20); // ENOTDIR
    let (n, _) = names.put(b"README.txt", b"");
    check!(90, c.status(Request::Create { dir: ROOT, name: n, kind: Kind::File, perm: 0o644 }) == -EEXIST);
    // A write of a read-only grant is fine (the device reads it); of a
    // file that is a directory, not.
    check!(91, c.status(Request::Write { ino: ROOT, offset: 0, buf: buf(ro, 0, 10) }) == -EINVAL);
    // After all that, the channel still works.
    check!(92, c.status(Request::Stat { ino }) == 0);
    Ok(())
}

/// Several reads and writes in flight at once, completing in any order.
fn in_flight() -> Result<(), i64> {
    const READS: usize = 24;
    const CHUNK: usize = 16 * 1024;
    let mut c = Client::open().map_err(|_| 100)?;
    let names = Names::new(&c).map_err(|_| 101)?;
    let ino = create(&mut c, &names, b"ringtest.par").map_err(|_| 102)?;
    let src = Object::new((READS * CHUNK) as u64 / PAGE).map_err(|_| 103)?;
    let all: Vec<u8> = (0..READS * CHUNK).map(|i| pattern(i, 0x33)).collect();
    src.write(0, &all);
    let g = c.grant(&src, false);
    check!(104, g > 0);
    // Writes in flight, each its own blocks.
    let writes: Vec<Desc> = (0..READS).map(|i| Request::Write { ino, offset: (i * CHUNK) as u64, buf: buf(g as u32, (i * CHUNK) as u64, CHUNK) }.encode(0)).collect();
    let tags = c.send(&writes).map_err(|_| 105)?;
    let mut done = vec![false; READS];
    for _ in 0..READS {
        let r = c.next().map_err(|_| 106)?;
        let i = tags.iter().position(|&t| t == r.tag).ok_or(107)?;
        check!(108, !done[i] && r.status == CHUNK as i64);
        done[i] = true;
    }
    check!(109, c.status(Request::Flush) == 0);
    // Reads in flight, into separate parts of one grant.
    let dst = Object::new((READS * CHUNK) as u64 / PAGE).map_err(|_| 110)?;
    let gd = c.grant(&dst, true);
    check!(111, gd > 0);
    let reads: Vec<Desc> = (0..READS)
        .map(|i| {
            // In reverse: the file's last chunk into the grant's first.
            let at = (READS - 1 - i) * CHUNK;
            Request::Read { ino, offset: at as u64, buf: buf(gd as u32, (i * CHUNK) as u64, CHUNK) }.encode(0)
        })
        .collect();
    let tags = c.send(&reads).map_err(|_| 112)?;
    let mut out_of_order = false;
    for n in 0..READS {
        let r = c.next().map_err(|_| 113)?;
        let i = tags.iter().position(|&t| t == r.tag).ok_or(114)?;
        out_of_order |= i != n;
        check!(115, r.status == CHUNK as i64);
    }
    let _ = out_of_order;
    for i in 0..READS {
        let at = (READS - 1 - i) * CHUNK;
        check!(116, dst.read((i * CHUNK) as u64, CHUNK) == all[at..at + CHUNK]);
    }
    remove(&mut c, &names, b"ringtest.par").map_err(|_| 117)?;
    Ok(())
}

/// A grant revoked while diskfs still knows it fails the requests on it
/// (its copies fault and fail, diskfs lives on); FORGET lets it go; a
/// client that goes with requests in flight leaves diskfs serving.
fn revoked_and_gone() -> Result<(), i64> {
    let mut c = Client::open().map_err(|_| 130)?;
    let names = Names::new(&c).map_err(|_| 131)?;
    let ino = lookup(&mut c, &names, ROOT, b"README.txt").map_err(|_| 132)?;
    let data = Object::new(2).map_err(|_| 133)?;
    let g = c.grant(&data, true);
    check!(134, g > 0);
    let g = g as u32;
    check!(135, c.status(Request::Read { ino, offset: 0, buf: buf(g, 0, 100) }) == 100);
    // diskfs holds a device address of it: the revoke drains.
    check!(136, c.revoke(g) == REVOKE_DRAINING as i64);
    // Its mapping in diskfs is gone: a copy into it fails, diskfs lives.
    check!(137, c.status(Request::Readdir { dir: ROOT, cursor: 0, buf: buf(g, 0, 100) }) == -EFAULT);
    check!(138, c.status(Request::Stat { ino }) == 0);
    // The range the revoked grant had in diskfs goes to no other grant
    // (it stays reserved until FORGET): a copy to the stale grant faults
    // and leaves the next grant diskfs maps untouched.
    let other = Object::new(2).map_err(|_| 150)?;
    other.write(0, &[0xee; 2 * PAGE as usize]);
    let g2 = c.grant(&other, true);
    check!(151, g2 > 0 && g2 != g as i64);
    check!(152, c.status(Request::Read { ino, offset: 0, buf: buf(g2 as u32, 0, 100) }) == 100);
    check!(153, c.status(Request::Readdir { dir: ROOT, cursor: 0, buf: buf(g, 0, 2 * PAGE as usize) }) == -EFAULT);
    let after = other.read(0, 2 * PAGE as usize);
    check!(154, after[..100] == README[..100] && after[100..].iter().all(|&b| b == 0xee));
    check!(155, c.status(Request::Forget { grant: g2 as u32 }) == 0);
    check!(156, c.revoke(g2 as u32) == 0);
    // FORGET lets go: the id comes back for the next grant.
    check!(139, c.status(Request::Forget { grant: g }) == 0);
    let again = c.grant(&data, true);
    check!(140, again == g as i64);
    // FORGET before the revoke: the revoke does not drain.
    check!(141, c.status(Request::Read { ino, offset: 0, buf: buf(g, 0, 100) }) == 100);
    check!(142, c.status(Request::Forget { grant: g }) == 0);
    check!(143, c.revoke(g) == 0);
    // Requests in flight when the client goes.
    let big = Object::new(64).map_err(|_| 144)?;
    let gb = c.grant(&big, true);
    check!(145, gb > 0);
    let reads: Vec<Desc> = (0..16).map(|i| Request::Read { ino, offset: 0, buf: buf(gb as u32, i * 4 * PAGE, 4096) }.encode(0)).collect();
    c.send(&reads).map_err(|_| 146)?;
    drop(c);
    drop(names);
    // diskfs serves the next channel.
    let mut c = Client::open().map_err(|_| 147)?;
    check!(148, c.status(Request::Stat { ino }) == 0);
    Ok(())
}

fn pause_ms(ms: u64) {
    syscall(SYS_SLEEP_UNTIL, [now() + ms * 1_000_000, 0, 0, 0, 0, 0]);
}

/// An unlinked inode is freed only when no client holds it: one client's
/// RELEASE leaves another one's file working, and a channel's holds go
/// with it.
fn holds() -> Result<(), i64> {
    let mut a = Client::open().map_err(|_| 160)?;
    let na = Names::new(&a).map_err(|_| 161)?;
    let mut b = Client::open().map_err(|_| 162)?;
    let nb = Names::new(&b).map_err(|_| 163)?;
    let x = create(&mut a, &na, b"ringtest.held").map_err(|_| 164)?;
    let src = Object::new(1).map_err(|_| 165)?;
    src.write(0, b"hello");
    let ga = a.grant(&src, false);
    check!(166, ga > 0 && a.status(Request::Write { ino: x, offset: 0, buf: buf(ga as u32, 0, 5) }) == 5);
    check!(167, lookup(&mut b, &nb, ROOT, b"ringtest.held") == Ok(x));
    // a unlinks it and lets go; b still has it.
    let (n, _) = na.put(b"ringtest.held", b"");
    let r = a.call(Request::Unlink { dir: ROOT, name: n, is_dir: false }).map_err(|_| 168)?;
    check!(169, r.status == 0 && r.values[0] == x as u64);
    check!(170, a.status(Request::Release { ino: x }) == 0);
    let dst = Object::new(1).map_err(|_| 171)?;
    let gb = b.grant(&dst, true);
    check!(172, gb > 0);
    check!(173, b.status(Request::Write { ino: x, offset: 5, buf: buf(gb as u32, 0, 3) }) == 3);
    check!(174, b.status(Request::Read { ino: x, offset: 0, buf: buf(gb as u32, 0, 100) }) == 8);
    check!(175, dst.read(0, 5) == b"hello");
    // b lets go: now it is freed.
    check!(176, b.status(Request::Release { ino: x }) == 0);
    check!(177, a.status(Request::Stat { ino: x }) == -ENOENT);
    // A channel that goes lets go of what it held. (Naming an inode holds
    // it: the checks below look from channels of their own, which go.)
    let y = create(&mut a, &na, b"ringtest.held").map_err(|_| 178)?;
    let stat = |ino: u32| Client::open().map(|mut d| d.status(Request::Stat { ino })).unwrap_or(-1);
    {
        let mut c = Client::open().map_err(|_| 179)?;
        let nc = Names::new(&c).map_err(|_| 180)?;
        check!(181, lookup(&mut c, &nc, ROOT, b"ringtest.held") == Ok(y));
        remove(&mut a, &na, b"ringtest.held").map_err(|_| 182)?;
        check!(183, stat(y) == 0);
    }
    let deadline = now() + TIMEOUT;
    while stat(y) != -ENOENT {
        check!(184, now() < deadline);
        pause_ms(10);
    }
    Ok(())
}

/// A write that waits for an overlapping one while another channel keeps
/// every operation slot busy: diskfs starts it when a slot is free (it
/// never assumes one).
fn stalls_with_full_slots() -> Result<(), i64> {
    const READS: usize = 100;
    let mut a = Client::open().map_err(|_| 190)?;
    let na = Names::new(&a).map_err(|_| 191)?;
    let mut b = Client::open().map_err(|_| 192)?;
    let nb = Names::new(&b).map_err(|_| 193)?;
    let f = create(&mut a, &na, b"ringtest.stall").map_err(|_| 194)?;
    let src = Object::new(128).map_err(|_| 195)?;
    let ga = a.grant(&src, false);
    check!(196, ga > 0);
    // Long reads (of the file the writes go to) keep the slots busy.
    const LONG: usize = 256 * 1024;
    check!(197, a.status(Request::Write { ino: f, offset: 0, buf: buf(ga as u32, 0, 128 * PAGE as usize) }) == 128 * PAGE as i64);
    check!(198, lookup(&mut b, &nb, ROOT, b"ringtest.stall") == Ok(f));
    let dst = Object::new((LONG as u64) / PAGE).map_err(|_| 199)?;
    let gb = b.grant(&dst, true);
    check!(207, gb > 0);
    let reads: Vec<Desc> = (0..READS).map(|i| Request::Read { ino: f, offset: (i % 2 * LONG) as u64, buf: buf(gb as u32, 0, LONG) }.encode(0)).collect();
    let writes = [
        Request::Write { ino: f, offset: 0, buf: buf(ga as u32, 0, 128 * PAGE as usize) }.encode(0),
        Request::Write { ino: f, offset: 0, buf: buf(ga as u32, 0, PAGE as usize) }.encode(0),
    ];
    for _ in 0..8 {
        b.send(&reads).map_err(|_| 200)?;
        a.send(&writes).map_err(|_| 201)?;
        for _ in 0..READS {
            check!(202, b.next().map(|r| r.status) == Ok(LONG as i64));
        }
        let (w1, w2) = (a.next().map_err(|_| 203)?, a.next().map_err(|_| 204)?);
        check!(205, w1.status > 0 && w2.status > 0);
    }
    remove(&mut a, &na, b"ringtest.stall").map_err(|_| 206)?;
    Ok(())
}

/// The channel scenario 8 leaves for scenario 9: requests waiting for room
/// in its completion ring.
static BLOCKED: crate::sync::Mutex<Option<(Client, u32, Vec<u64>)>> = crate::sync::Mutex::new(None);

/// Fills the completion ring and leaves as many requests waiting for room
/// (lxtest then checks that diskfs does not spin meanwhile).
fn block_on_room() -> Result<(), i64> {
    let mut c = Client::open().map_err(|_| 210)?;
    let names = Names::new(&c).map_err(|_| 211)?;
    let ino = lookup(&mut c, &names, ROOT, b"README.txt").map_err(|_| 212)?;
    let stats: Vec<Desc> = (0..N).map(|_| Request::Stat { ino }.encode(0)).collect();
    let mut tags = c.send(&stats).map_err(|_| 213)?;
    // Long enough for diskfs to complete them all.
    pause_ms(200);
    check!(214, !c.completions.is_empty());
    tags.extend(c.send(&stats).map_err(|_| 215)?);
    *BLOCKED.lock() = Some((c, ino, tags));
    Ok(())
}

/// Takes the completions of scenario 8 (ringing the doorbell as it makes
/// room): every request completes.
fn unblock() -> Result<(), i64> {
    let Some((mut c, _, tags)) = BLOCKED.lock().take() else { return Err(220) };
    for &tag in &tags {
        let r = c.next().map_err(|_| 221)?;
        check!(222, r.tag == tag && r.status == 0);
    }
    Ok(())
}
