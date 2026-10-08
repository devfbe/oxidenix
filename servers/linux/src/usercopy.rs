//! Copies between the server's memory and the program's (which the server
//! sees in its view of the address space). A fault on program memory is
//! resolved by the kernel as the program's own would be; one the program
//! may not make resumes at `copy_fixup`, so the copy reports EFAULT
//! instead of the process dying (the kernel learns the two addresses once,
//! `SYS_SET_USERCOPY`).
//!
//! Every program address is checked against 64 TiB first: the server's
//! own memory lies above, and a program must not make the server write it.

use crate::syscall;
use restricted::{SHARED_BASE, SYS_SET_USERCOPY};

pub const EFAULT: i64 = 14;

unsafe extern "C" {
    static copy_insn: u8;
    static copy_fixup: u8;
}

/// Copies `len` bytes and returns how many were not copied (0: all).
#[unsafe(naked)]
unsafe extern "C" fn copy(dst: *mut u8, src: *const u8, len: usize) -> usize {
    core::arch::naked_asm!(
        "mov rcx, rdx",
        ".global copy_insn",
        "copy_insn:",
        "rep movsb",
        "xor eax, eax",
        "ret",
        ".global copy_fixup",
        "copy_fixup:",
        "mov rax, rcx",
        "ret",
    );
}

/// Tells the kernel where the copy may fault (once per instance; later
/// calls find it set).
pub fn register() {
    let (insn, fixup) = (&raw const copy_insn as u64, &raw const copy_fixup as u64);
    syscall(SYS_SET_USERCOPY, [insn, fixup, 0, 0, 0, 0]);
}

/// Whether [addr, addr+len) lies in the program's memory.
fn program_range(addr: u64, len: usize) -> bool {
    addr.checked_add(len as u64).is_some_and(|end| end <= SHARED_BASE)
}

/// Writes `data` to program memory at `addr`.
pub fn to_program(addr: u64, data: &[u8]) -> Result<(), i64> {
    if !program_range(addr, data.len()) {
        return Err(EFAULT);
    }
    match unsafe { copy(addr as *mut u8, data.as_ptr(), data.len()) } {
        0 => Ok(()),
        _ => Err(EFAULT),
    }
}

/// Reads program memory at `addr` into `buf`.
pub fn from_program(addr: u64, buf: &mut [u8]) -> Result<(), i64> {
    if !program_range(addr, buf.len()) {
        return Err(EFAULT);
    }
    match unsafe { copy(buf.as_mut_ptr(), addr as *const u8, buf.len()) } {
        0 => Ok(()),
        _ => Err(EFAULT),
    }
}

/// Writes a plain value (`#[repr(C)]` data) to program memory.
pub fn write<T: Copy>(addr: u64, value: &T) -> Result<(), i64> {
    let bytes = unsafe { core::slice::from_raw_parts(value as *const T as *const u8, core::mem::size_of::<T>()) };
    to_program(addr, bytes)
}

/// Reads a plain value from program memory.
pub fn read<T: Copy + Default>(addr: u64) -> Result<T, i64> {
    let mut value = T::default();
    let bytes = unsafe { core::slice::from_raw_parts_mut(&mut value as *mut T as *mut u8, core::mem::size_of::<T>()) };
    from_program(addr, bytes)?;
    Ok(value)
}

/// A NUL-terminated string of the program's (a path: at most 4096 bytes
/// with the NUL, else ENAMETOOLONG). Read a page piece at a time, so that
/// an unmapped page after the string's end does not matter.
pub fn read_cstr(addr: u64) -> Result<alloc::string::String, i64> {
    const MAX: usize = 4096;
    const ENAMETOOLONG: i64 = 36;
    let mut bytes = alloc::vec::Vec::new();
    let mut chunk = [0u8; 256];
    while bytes.len() < MAX {
        let at = addr.checked_add(bytes.len() as u64).ok_or(EFAULT)?;
        let n = chunk.len().min(4096 - (at % 4096) as usize).min(MAX - bytes.len());
        from_program(at, &mut chunk[..n])?;
        if let Some(end) = chunk[..n].iter().position(|&b| b == 0) {
            bytes.extend_from_slice(&chunk[..end]);
            return alloc::string::String::from_utf8(bytes).map_err(|_| EFAULT);
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    Err(ENAMETOOLONG)
}
