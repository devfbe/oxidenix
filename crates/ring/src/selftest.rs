//! The protocol of the self-tests' channel service (servers/ringtest),
//! which the Linux server's test calls (`restricted::TEST_CHANNEL`) drive.
//!
//! A request names a grant by `Desc::grant` and a range of it by
//! `buf_off`/`len`; its completion echoes `tag` and carries the status in
//! `arg[0]` (a value, or a negative errno).

/// Slots per ring of the test channels.
pub const SLOTS: usize = 64;
/// The service's IPC name.
pub const SERVICE: &str = "ringtest";

/// `arg[0] + 1`.
pub const ECHO: u16 = 1;
/// The sum of the bytes of the range (EINVAL beyond the grant).
pub const READ: u16 = 2;
/// Fills the range with the byte `arg[0]` (EACCES for a read-only grant);
/// the length.
pub const WRITE: u16 = 3;
/// 0 if the grant is read-only for the service in every way tried: it
/// cannot be made writable or executable (mprotect: EACCES), the kernel
/// cannot be made to store into it (EFAULT), and it reads.
pub const PROBE_READ_ONLY: u16 = 4;
/// The device address of byte `buf_off` of the grant.
pub const DMA: u16 = 5;
/// The service lets go of the grant's device addresses.
pub const DMA_UNMAP: u16 = 6;
/// 0 if the grant is gone from the service: it cannot be mapped (ENOENT)
/// and its earlier mapping is gone (mprotect: ENOMEM).
pub const CHECK_GONE: u16 = 7;
/// Stores into the (read-only) grant: the service dies of it.
pub const CRASH: u16 = 8;
/// How many channels ended with their client gone and every grant mapping
/// of theirs removed by the kernel.
pub const CLEAN_ENDS: u16 = 9;
/// How often the service slept on the submission ring's doorbell.
pub const SLEEPS: u16 = 10;
/// The service executes itself again (`/sbin/ringtest after-exec`), and
/// the new program tries to reach the grant: it maps it and stores
/// `AFTER_EXEC` at its first byte, and asks for a device address. Neither
/// may work: an exec ends the service's end of its channels.
pub const EXEC: u16 = 11;
pub const AFTER_EXEC: u8 = 0x99;
/// The service answers the next offer only after it served that channel
/// (it attaches at once): attaching alone must complete the connect.
pub const ANSWER_LATE: u16 = 12;
