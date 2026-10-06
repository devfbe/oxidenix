//! Zugriff auf Userspeicher des aktuellen Prozesses mit Pruefung der Mappings.

use super::address_space::user_range_ok;
use super::errno::{E2BIG, EFAULT, ENAMETOOLONG};
use alloc::string::String;
use alloc::vec::Vec;

pub fn slice<'a>(ptr: u64, len: u64) -> Result<&'a [u8], i64> {
    if len == 0 {
        return Ok(&[]);
    }
    if !user_range_ok(ptr, len, false) {
        return Err(EFAULT);
    }
    Ok(unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) })
}

pub fn slice_mut<'a>(ptr: u64, len: u64) -> Result<&'a mut [u8], i64> {
    if len == 0 {
        return Ok(&mut []);
    }
    if !user_range_ok(ptr, len, true) {
        return Err(EFAULT);
    }
    Ok(unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, len as usize) })
}

pub fn read<T: Copy>(ptr: u64) -> Result<T, i64> {
    let bytes = slice(ptr, core::mem::size_of::<T>() as u64)?;
    Ok(unsafe { (bytes.as_ptr() as *const T).read_unaligned() })
}

pub fn write<T: Copy>(ptr: u64, val: T) -> Result<(), i64> {
    let bytes = slice_mut(ptr, core::mem::size_of::<T>() as u64)?;
    unsafe { (bytes.as_mut_ptr() as *mut T).write_unaligned(val) };
    Ok(())
}

pub fn read_cstr(ptr: u64) -> Result<String, i64> {
    let mut bytes = Vec::new();
    for i in 0..4096u64 {
        let a = ptr.checked_add(i).ok_or(EFAULT)?;
        if (i == 0 || a % 4096 == 0) && !user_range_ok(a, 1, false) {
            return Err(EFAULT);
        }
        let b = unsafe { *(a as *const u8) };
        if b == 0 {
            return String::from_utf8(bytes).map_err(|_| EFAULT);
        }
        bytes.push(b);
    }
    Err(ENAMETOOLONG)
}

/// Liest ein NULL-terminiertes Array von C-Strings (argv/envp).
pub fn read_cstr_array(ptr: u64) -> Result<Vec<String>, i64> {
    let mut out = Vec::new();
    if ptr == 0 {
        return Ok(out);
    }
    loop {
        if out.len() >= 1024 {
            return Err(E2BIG);
        }
        let p: u64 = read(ptr + out.len() as u64 * 8)?;
        if p == 0 {
            return Ok(out);
        }
        out.push(read_cstr(p)?);
    }
}
