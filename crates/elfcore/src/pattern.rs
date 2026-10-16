//! The core file's name from /proc/sys/kernel/core_pattern and core_uses_pid (core(5),
//! Linux's `format_corename`): the `%` specifiers expanded, `.PID` appended when asked for
//! and the pattern has no `%p`, or, for a pattern that starts with `|`, the command line of
//! a program to pipe the dump to (split on spaces before the expansion, as since Linux 5.3).

use crate::Error;
use alloc::vec::Vec;

/// The most core_pattern holds (Linux's CORENAME_MAX_SIZE, with its NUL).
pub const PATTERN_MAX: usize = 128;

/// What the specifiers stand for.
pub struct Facts<'a> {
    /// The process's id and the dumping thread's, in the process's pid namespace (`%p`,
    /// `%i`) and in the initial one (`%P`, `%I`).
    pub pid: u32,
    pub tid: u32,
    pub global_pid: u32,
    pub global_tid: u32,
    pub uid: u32,
    pub gid: u32,
    /// The signal (`%s`), the dump mode (`%d`, PR_GET_DUMPABLE's), the CPU the dumping
    /// thread ran on (`%C`), the soft RLIMIT_CORE (`%c`), the wall-clock seconds (`%t`).
    pub signal: u32,
    pub dumpable: u32,
    pub cpu: u32,
    pub limit: u64,
    pub time: i64,
    /// The host name (`%h`), the dumping thread's name (`%e`), and the program's path
    /// (`%E` whole, `%f` its last name), if it has one.
    pub hostname: &'a [u8],
    pub comm: &'a [u8],
    pub exe: Option<&'a [u8]>,
}

/// Where the dump goes.
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    /// A file, by this path (relative to the process's working directory unless absolute).
    File(Vec<u8>),
    /// A program's standard input: its command line.
    Pipe(Vec<Vec<u8>>),
}

fn push(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), Error> {
    out.try_reserve(bytes.len())?;
    out.extend_from_slice(bytes);
    Ok(())
}

fn number(out: &mut Vec<u8>, n: u64, negative: bool) -> Result<(), Error> {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    let mut n = n;
    loop {
        at -= 1;
        digits[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    if negative {
        push(out, b"-")?;
    }
    push(out, &digits[at..])
}

/// A value that becomes part of a name (Linux's `cn_esc_printf`): '/' becomes '!', a
/// value that would make the component "." or ".." gets a '!' first, an empty one is "!".
fn escaped(out: &mut Vec<u8>, parts: &[&[u8]]) -> Result<(), Error> {
    let start = out.len();
    for bytes in parts {
        push(out, bytes)?;
    }
    let added = &mut out[start..];
    if added == b"." || added == b".." {
        added[0] = b'!';
    }
    if added.is_empty() {
        push(out, b"!")?;
    }
    for b in &mut out[start..] {
        if *b == b'/' {
            *b = b'!';
        }
    }
    Ok(())
}

/// The name `pattern` (as core_pattern holds it) gives with `facts`; `uses_pid`:
/// core_uses_pid. An empty pattern gives an empty name (no file can be made by it), as
/// Linux's.
pub fn expand(pattern: &[u8], uses_pid: bool, facts: &Facts) -> Result<Target, Error> {
    let pipe = pattern.first() == Some(&b'|');
    let pattern = if pipe { &pattern[1..] } else { pattern };
    // The arguments so far (a pipe's), and the one being made.
    let mut args: Vec<Vec<u8>> = Vec::new();
    let mut out = Vec::new();
    let mut pid_in_pattern = false;
    let mut i = 0;
    while i < pattern.len() {
        let c = pattern[i];
        i += 1;
        if pipe && c.is_ascii_whitespace() {
            // A space ends an argument (several are one; none before the first).
            if !out.is_empty() {
                args.try_reserve(1)?;
                args.push(core::mem::take(&mut out));
            }
            continue;
        }
        if c != b'%' {
            push(&mut out, &[c])?;
            continue;
        }
        // A '%' at the end is dropped, as is one with an unknown specifier.
        let Some(&spec) = pattern.get(i) else { break };
        i += 1;
        match spec {
            b'%' => push(&mut out, b"%")?,
            b'p' => {
                pid_in_pattern = true;
                number(&mut out, facts.pid as u64, false)?
            }
            b'P' => number(&mut out, facts.global_pid as u64, false)?,
            b'i' => number(&mut out, facts.tid as u64, false)?,
            b'I' => number(&mut out, facts.global_tid as u64, false)?,
            b'u' => number(&mut out, facts.uid as u64, false)?,
            b'g' => number(&mut out, facts.gid as u64, false)?,
            b'd' => number(&mut out, facts.dumpable as u64, false)?,
            b's' => number(&mut out, facts.signal as u64, false)?,
            b't' => number(&mut out, facts.time.unsigned_abs(), facts.time < 0)?,
            b'c' => number(&mut out, facts.limit, false)?,
            b'C' => number(&mut out, facts.cpu as u64, false)?,
            b'h' => escaped(&mut out, &[facts.hostname])?,
            b'e' => escaped(&mut out, &[facts.comm])?,
            b'f' => {
                let exe = facts.exe.unwrap_or(b"");
                let name = exe.rsplit(|&b| b == b'/').next().unwrap_or(exe);
                escaped(&mut out, &[name])?
            }
            b'E' => match facts.exe {
                Some(exe) => escaped(&mut out, &[exe])?,
                None => escaped(&mut out, &[facts.comm, b" (path unknown)"])?,
            },
            // (`%F`, a pidfd of the process, is a pipe helper's only; there is none.)
            _ => {}
        }
    }
    if pipe {
        if !out.is_empty() {
            args.try_reserve(1)?;
            args.push(out);
        }
        return Ok(Target::Pipe(args));
    }
    if uses_pid && !pid_in_pattern {
        push(&mut out, b".")?;
        number(&mut out, facts.pid as u64, false)?;
    }
    Ok(Target::File(out))
}

/// What a write to core_pattern sets (Linux's `proc_dostring`): the bytes up to the first
/// newline or NUL, at most `PATTERN_MAX - 1` of them.
pub fn written(data: &[u8]) -> &[u8] {
    let end = data.iter().position(|&b| b == b'\n' || b == 0).unwrap_or(data.len());
    &data[..end.min(PATTERN_MAX - 1)]
}
