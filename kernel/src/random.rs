//! The kernel's randomness: a ChaCha20 generator (`csprng::ChaCha`, fast key
//! erasure) seeded at boot from the CPU's entropy source (RDSEED, else
//! RDRAND, when it has one) and from timing jitter of the time stamp
//! counter, reseeded from the CPU's source as it is used. It serves
//! getrandom(2) (for Linux programs and the servers), AT_RANDOM and every
//! secret the system needs (netd's TCP sequence numbers and ports).
//!
//! The pool is ready before the first process runs, so getrandom never
//! blocks (GRND_NONBLOCK and GRND_RANDOM change nothing, as on Linux since
//! 5.6 once its pool is initialized).

use crate::sync::IrqSpinLock;
use core::arch::x86_64::{__cpuid, __cpuid_count, _rdtsc};
use csprng::ChaCha;

static RNG: IrqSpinLock<ChaCha> = IrqSpinLock::new(ChaCha::new([0; 32]));
/// Requests between reseeds from the CPU's source.
const RESEED_EVERY: u32 = 4096;
static SINCE_RESEED: IrqSpinLock<u32> = IrqSpinLock::new(0);

/// Which entropy instruction the CPU has.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Rdseed,
    Rdrand,
    None,
}

fn source() -> Source {
    // CPUID.(EAX=7,ECX=0):EBX[18] RDSEED; CPUID.1:ECX[30] RDRAND.
    if __cpuid(0).eax >= 7 && __cpuid_count(7, 0).ebx & (1 << 18) != 0 {
        Source::Rdseed
    } else if __cpuid(1).ecx & (1 << 30) != 0 {
        Source::Rdrand
    } else {
        Source::None
    }
}

/// One 64-bit value from the CPU's source (it may be busy: retried a few
/// times, as Intel advises), or None.
fn hardware(src: Source) -> Option<u64> {
    for _ in 0..32 {
        let mut v = 0u64;
        let ok: u8;
        unsafe {
            match src {
                Source::Rdseed => core::arch::asm!("rdseed {v}", "setc {ok}", v = inout(reg) v, ok = out(reg_byte) ok, options(nomem, nostack)),
                Source::Rdrand => core::arch::asm!("rdrand {v}", "setc {ok}", v = inout(reg) v, ok = out(reg_byte) ok, options(nomem, nostack)),
                Source::None => return None,
            }
        }
        if ok != 0 {
            return Some(v);
        }
        core::hint::spin_loop();
    }
    None
}

/// Timing jitter: the time stamp counter around work whose duration varies
/// with caches, interrupts and the host's scheduling, eight deltas folded
/// into each byte of `out`.
fn jitter(out: &mut [u8]) {
    let mut scratch = [0u64; 64];
    let mut last = unsafe { _rdtsc() };
    for (i, b) in out.iter_mut().enumerate() {
        let mut acc = 0u8;
        for round in 0..8 {
            for (j, s) in scratch.iter_mut().enumerate() {
                *s = s.wrapping_mul(6364136223846793005).wrapping_add((i * 8 + round + j) as u64 ^ last);
            }
            let now = unsafe { _rdtsc() };
            acc = acc.rotate_left(1) ^ (now.wrapping_sub(last) as u8);
            last = now;
        }
        *b = acc ^ scratch[i % 64] as u8;
    }
}

/// Seeds the generator: called once at boot, before any process exists.
pub fn init() {
    let src = source();
    let mut entropy = [0u8; 128];
    let mut hw = 0;
    for chunk in entropy[..64].chunks_mut(8) {
        if let Some(v) = hardware(src) {
            chunk.copy_from_slice(&v.to_le_bytes());
            hw += 1;
        }
    }
    // Jitter always, and much of it without a hardware source.
    jitter(&mut entropy[64..]);
    let mut rng = RNG.lock();
    rng.reseed(&entropy);
    if hw < 8 {
        let mut more = [0u8; 512];
        jitter(&mut more);
        rng.reseed(&more);
    }
    drop(rng);
    let name = match src {
        Source::Rdseed => "RDSEED",
        Source::Rdrand => "RDRAND",
        Source::None => "timing jitter only",
    };
    crate::printkln!("[kernel] random: seeded from {} and timing jitter", name);
}

/// Fills `out` (at most 256 bytes at a time keep the lock short).
pub fn fill(out: &mut [u8]) {
    let reseed = {
        let mut n = SINCE_RESEED.lock();
        *n += 1;
        if *n >= RESEED_EVERY {
            *n = 0;
            true
        } else {
            false
        }
    };
    if reseed {
        let mut entropy = [0u8; 40];
        for chunk in entropy[..32].chunks_mut(8) {
            if let Some(v) = hardware(source()) {
                chunk.copy_from_slice(&v.to_le_bytes());
            }
        }
        entropy[32..].copy_from_slice(&unsafe { _rdtsc() }.to_le_bytes());
        RNG.lock().reseed(&entropy);
    }
    for piece in out.chunks_mut(256) {
        RNG.lock().fill(piece);
    }
}
