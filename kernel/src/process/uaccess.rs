//! Access to the current process's user memory.
//!
//! The kernel touches user memory only through `copy_user`, never through
//! references: another thread of the process may unmap or protect any page
//! at any moment. A page fault inside `copy_user` is handled like a user
//! fault (demand paging, copy-on-write, stack growth); if the access is not
//! allowed the fault handler resumes at a fixup that ends the copy, and the
//! caller gets EFAULT. Because such a fault may sleep (it can read a file
//! page), user memory is never accessed with a spinlock held: data goes
//! through kernel buffers (`read_to_user`, `write_from_user`).

use super::address_space::USER_END;
use super::errno::{E2BIG, EFAULT, ENAMETOOLONG, ENOMEM};
use alloc::string::String;
use alloc::vec::Vec;

/// Size of the kernel buffers that file and socket data passes through.
pub const CHUNK: usize = 64 * 1024;

unsafe extern "C" {
    /// The instructions below that may fault, and where to resume when the
    /// fault cannot (or must not) be satisfied.
    static uaccess_copy_insn: u8;
    static uaccess_copy_fixup: u8;
    static uaccess_load32_insn: u8;
    static uaccess_load32_fixup: u8;
}

/// Copies `len` bytes and returns how many were *not* copied (0: all).
#[unsafe(naked)]
unsafe extern "sysv64" fn copy_user(dst: *mut u8, src: *const u8, len: usize) -> usize {
    core::arch::naked_asm!(
        "mov rcx, rdx",
        ".globl uaccess_copy_insn",
        "uaccess_copy_insn:",
        "rep movsb",
        "xor eax, eax",
        "ret",
        // rep movsb is restartable: rcx counts the bytes left at the fault.
        ".globl uaccess_copy_fixup",
        "uaccess_copy_fixup:",
        "mov rax, rcx",
        "ret",
    );
}

/// Loads a u32 into `*out`; returns 1 (and leaves `out`) on a fault, which
/// is never resolved (see `read_u32_atomic`).
#[unsafe(naked)]
unsafe extern "sysv64" fn load32(src: *const u32, out: *mut u32) -> u64 {
    core::arch::naked_asm!(
        ".globl uaccess_load32_insn",
        "uaccess_load32_insn:",
        "mov eax, dword ptr [rdi]",
        "mov dword ptr [rsi], eax",
        "xor eax, eax",
        "ret",
        ".globl uaccess_load32_fixup",
        "uaccess_load32_fixup:",
        "mov eax, 1",
        "ret",
    );
}

/// A kernel access to user memory that faulted: where to resume if the
/// fault is not satisfied, and whether to try (faults in `read_u32_atomic`
/// are not resolved: it runs with spinlocks held).
pub struct Fixup {
    pub to: u64,
    pub resolve: bool,
}

/// The fixup for a page fault at `rip`, if `rip` is one of the kernel's
/// accesses to user memory.
pub fn fixup(rip: u64) -> Option<Fixup> {
    if rip == &raw const uaccess_copy_insn as u64 {
        Some(Fixup { to: &raw const uaccess_copy_fixup as u64, resolve: true })
    } else if rip == &raw const uaccess_load32_insn as u64 {
        Some(Fixup { to: &raw const uaccess_load32_fixup as u64, resolve: false })
    } else {
        None
    }
}

/// Reads an aligned u32 with one load and without resolving page faults,
/// so it may run with a spinlock held (futexes compare the value under
/// their bucket lock). None if the page is not readable right now: the
/// caller drops its locks, faults the page in with `read` and retries.
pub fn read_u32_atomic(ptr: u64) -> Option<u32> {
    if ptr % 4 != 0 || range_ok(ptr, 4).is_err() {
        return None;
    }
    let mut v = 0u32;
    (unsafe { load32(ptr as *const u32, &mut v) } == 0).then_some(v)
}

fn range_ok(ptr: u64, len: usize) -> Result<(), i64> {
    match ptr.checked_add(len as u64) {
        Some(end) if end <= USER_END => Ok(()),
        _ => Err(EFAULT),
    }
}

/// Fills `dst` from user memory at `ptr`.
pub fn copy_from(ptr: u64, dst: &mut [u8]) -> Result<(), i64> {
    range_ok(ptr, dst.len())?;
    match unsafe { copy_user(dst.as_mut_ptr(), ptr as *const u8, dst.len()) } {
        0 => Ok(()),
        _ => Err(EFAULT),
    }
}

/// Writes `src` to user memory at `ptr`.
pub fn copy_to(ptr: u64, src: &[u8]) -> Result<(), i64> {
    range_ok(ptr, src.len())?;
    match unsafe { copy_user(ptr as *mut u8, src.as_ptr(), src.len()) } {
        0 => Ok(()),
        _ => Err(EFAULT),
    }
}

/// `len` bytes of user memory at `ptr`, copied into a kernel buffer.
pub fn read_vec(ptr: u64, len: u64) -> Result<Vec<u8>, i64> {
    range_ok(ptr, len as usize)?;
    let mut v = Vec::new();
    v.try_reserve_exact(len as usize).map_err(|_| ENOMEM)?;
    v.resize(len as usize, 0);
    copy_from(ptr, &mut v)?;
    Ok(v)
}

pub fn read<T: Copy>(ptr: u64) -> Result<T, i64> {
    let mut val = core::mem::MaybeUninit::<T>::uninit();
    let bytes = unsafe { core::slice::from_raw_parts_mut(val.as_mut_ptr() as *mut u8, core::mem::size_of::<T>()) };
    copy_from(ptr, bytes)?;
    // T is plain data (Copy, read from raw bytes like read_unaligned would).
    Ok(unsafe { val.assume_init() })
}

pub fn write<T: Copy>(ptr: u64, val: T) -> Result<(), i64> {
    let bytes = unsafe { core::slice::from_raw_parts(&val as *const T as *const u8, core::mem::size_of::<T>()) };
    copy_to(ptr, bytes)
}

/// Moves up to `len` bytes that `produce` delivers into user memory at
/// `ptr`, a kernel buffer of at most CHUNK bytes at a time. `produce` gets
/// the buffer and the number of bytes delivered so far. With `more`, it is
/// called again as long as it fills whole buffers (regular files); else
/// once (pipes, terminals, sockets: a short read is their normal answer).
/// An error after some bytes were delivered ends the transfer with them.
pub fn read_to_user(ptr: u64, len: u64, more: bool, mut produce: impl FnMut(&mut [u8], u64) -> Result<usize, i64>) -> Result<usize, i64> {
    range_ok(ptr, len as usize)?;
    let mut buf = Vec::new();
    buf.try_reserve_exact((len as usize).min(CHUNK)).map_err(|_| ENOMEM)?;
    buf.resize((len as usize).min(CHUNK), 0);
    let mut done = 0u64;
    while done < len {
        let want = ((len - done) as usize).min(CHUNK);
        let n = match produce(&mut buf[..want], done) {
            Ok(n) => n.min(want),
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        };
        if let Err(e) = copy_to(ptr + done, &buf[..n]) {
            return if done == 0 { Err(e) } else { Ok(done as usize) };
        }
        done += n as u64;
        if !more || n < want {
            break;
        }
    }
    Ok(done as usize)
}

/// Hands up to `len` bytes of user memory at `ptr` to `consume`, at most
/// CHUNK bytes at a time, as long as it takes whole chunks. `consume` gets
/// the data and the number of bytes taken so far. An error after some
/// bytes were taken ends the transfer with them.
pub fn write_from_user(ptr: u64, len: u64, mut consume: impl FnMut(&[u8], u64) -> Result<usize, i64>) -> Result<usize, i64> {
    range_ok(ptr, len as usize)?;
    let mut buf = Vec::new();
    buf.try_reserve_exact((len as usize).min(CHUNK)).map_err(|_| ENOMEM)?;
    buf.resize((len as usize).min(CHUNK), 0);
    let mut done = 0u64;
    loop {
        let want = ((len - done) as usize).min(CHUNK);
        if let Err(e) = copy_from(ptr + done, &mut buf[..want]) {
            return if done == 0 { Err(e) } else { Ok(done as usize) };
        }
        let n = match consume(&buf[..want], done) {
            Ok(n) => n.min(want),
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        };
        done += n as u64;
        if n < want || done == len {
            break;
        }
    }
    Ok(done as usize)
}

/// A NUL-terminated string of at most 4 KiB (with the NUL).
pub fn read_cstr(ptr: u64) -> Result<String, i64> {
    const MAX: u64 = 4096;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 256];
    while (bytes.len() as u64) < MAX {
        let at = ptr.checked_add(bytes.len() as u64).ok_or(EFAULT)?;
        // Never read past the page the string continues in: the next page
        // may be unmapped although the string ends before it.
        let to_page_end = 4096 - at % 4096;
        let n = (chunk.len() as u64).min(to_page_end).min(MAX - bytes.len() as u64) as usize;
        copy_from(at, &mut chunk[..n])?;
        if let Some(end) = chunk[..n].iter().position(|&b| b == 0) {
            bytes.extend_from_slice(&chunk[..end]);
            return String::from_utf8(bytes).map_err(|_| EFAULT);
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    Err(ENAMETOOLONG)
}

/// Total size limit for argv or envp, like Linux's ARG_MAX.
const ARG_MAX: usize = 128 * 1024;

/// Reads a NULL-terminated array of C strings (argv/envp).
pub fn read_cstr_array(ptr: u64) -> Result<Vec<String>, i64> {
    let mut out = Vec::new();
    if ptr == 0 {
        return Ok(out);
    }
    let mut total = 0;
    loop {
        if out.len() >= 1024 {
            return Err(E2BIG);
        }
        let p: u64 = read(ptr + out.len() as u64 * 8)?;
        if p == 0 {
            return Ok(out);
        }
        let s = read_cstr(p)?;
        total += s.len() + 1;
        if total > ARG_MAX {
            return Err(E2BIG);
        }
        out.push(s);
    }
}
