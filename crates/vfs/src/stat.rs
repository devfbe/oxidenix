//! File status as Linux reports it: `struct stat` (stat, fstat, newfstatat)
//! and `struct statx` (statx(2)), both in x86-64's layout, and the
//! arguments statx checks. The filesystems describe a file once, as a
//! `struct stat`; statx is made from it, with the birth time where a
//! filesystem knows one.

/// statx's mask bits (`STATX_*`): which fields are asked for, and which
/// the answer fills.
pub const STATX_TYPE: u32 = 0x1;
pub const STATX_MODE: u32 = 0x2;
pub const STATX_NLINK: u32 = 0x4;
pub const STATX_UID: u32 = 0x8;
pub const STATX_GID: u32 = 0x10;
pub const STATX_ATIME: u32 = 0x20;
pub const STATX_MTIME: u32 = 0x40;
pub const STATX_CTIME: u32 = 0x80;
pub const STATX_INO: u32 = 0x100;
pub const STATX_SIZE: u32 = 0x200;
pub const STATX_BLOCKS: u32 = 0x400;
/// Everything `struct stat` has.
pub const STATX_BASIC_STATS: u32 = 0x7ff;
pub const STATX_BTIME: u32 = 0x800;
/// Reserved for a future extension of the structure: EINVAL.
pub const STATX__RESERVED: u32 = 0x8000_0000;

/// The flags statx takes (`AT_*`).
pub const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
pub const AT_NO_AUTOMOUNT: u32 = 0x800;
pub const AT_EMPTY_PATH: u32 = 0x1000;
/// `AT_STATX_FORCE_SYNC` and `AT_STATX_DONT_SYNC`: how far to synchronize
/// with a remote filesystem; local ones ignore it.
pub const AT_STATX_SYNC_TYPE: u32 = 0x6000;

/// The size of `struct statx`.
pub const STATX_SIZE_BYTES: usize = 256;

const EINVAL: i64 = 22;

/// A point in time: seconds and nanoseconds since the epoch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Time {
    pub sec: i64,
    pub nsec: u32,
}

/// A file's status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: u64,
    pub blksize: u64,
    pub blocks: u64,
    pub atime: Time,
    pub mtime: Time,
    pub ctime: Time,
    /// The creation time, where the filesystem records one.
    pub btime: Option<Time>,
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// Linux's encoding of a device number (`new_encode_dev`'s inverse):
/// (major, minor).
pub fn dev_split(dev: u64) -> (u32, u32) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major as u32, minor as u32)
}

impl Stat {
    /// From a `struct stat` (144 bytes).
    pub fn from_bytes(st: &[u8; 144]) -> Stat {
        let time = |at: usize| Time { sec: u64_at(st, at) as i64, nsec: u64_at(st, at + 8) as u32 };
        Stat {
            dev: u64_at(st, 0),
            ino: u64_at(st, 8),
            nlink: u64_at(st, 16),
            mode: u32_at(st, 24),
            uid: u32_at(st, 28),
            gid: u32_at(st, 32),
            rdev: u64_at(st, 40),
            size: u64_at(st, 48),
            blksize: u64_at(st, 56),
            blocks: u64_at(st, 64),
            atime: time(72),
            mtime: time(88),
            ctime: time(104),
            btime: None,
        }
    }

    /// As a `struct stat` (144 bytes).
    pub fn to_bytes(&self) -> [u8; 144] {
        let mut st = [0u8; 144];
        let mut put = |at: usize, v: &[u8]| st[at..at + v.len()].copy_from_slice(v);
        put(0, &self.dev.to_le_bytes());
        put(8, &self.ino.to_le_bytes());
        put(16, &self.nlink.to_le_bytes());
        put(24, &self.mode.to_le_bytes());
        put(28, &self.uid.to_le_bytes());
        put(32, &self.gid.to_le_bytes());
        put(40, &self.rdev.to_le_bytes());
        put(48, &self.size.to_le_bytes());
        put(56, &self.blksize.to_le_bytes());
        put(64, &self.blocks.to_le_bytes());
        for (at, t) in [(72, self.atime), (88, self.mtime), (104, self.ctime)] {
            put(at, &t.sec.to_le_bytes());
            put(at + 8, &(t.nsec as u64).to_le_bytes());
        }
        st
    }

    /// As a `struct statx`. Like Linux's filesystems, it fills every field
    /// it has, whatever the caller's mask asked for, and says which in
    /// `stx_mask`: the basic fields, and the birth time where known.
    pub fn to_statx(&self) -> [u8; STATX_SIZE_BYTES] {
        let mut x = [0u8; STATX_SIZE_BYTES];
        let mut put = |at: usize, v: &[u8]| x[at..at + v.len()].copy_from_slice(v);
        let mask = STATX_BASIC_STATS | if self.btime.is_some() { STATX_BTIME } else { 0 };
        put(0, &mask.to_le_bytes());
        put(4, &(self.blksize as u32).to_le_bytes());
        // stx_attributes and stx_attributes_mask (8, 56): none supported.
        put(16, &(self.nlink.min(u32::MAX as u64) as u32).to_le_bytes());
        put(20, &self.uid.to_le_bytes());
        put(24, &self.gid.to_le_bytes());
        put(28, &(self.mode as u16).to_le_bytes());
        put(32, &self.ino.to_le_bytes());
        put(40, &self.size.to_le_bytes());
        put(48, &self.blocks.to_le_bytes());
        let times = [(64, Some(self.atime)), (80, self.btime), (96, Some(self.ctime)), (112, Some(self.mtime))];
        for (at, t) in times {
            if let Some(t) = t {
                put(at, &t.sec.to_le_bytes());
                put(at + 8, &t.nsec.to_le_bytes());
            }
        }
        let (rmaj, rmin) = dev_split(self.rdev);
        let (dmaj, dmin) = dev_split(self.dev);
        put(128, &rmaj.to_le_bytes());
        put(132, &rmin.to_le_bytes());
        put(136, &dmaj.to_le_bytes());
        put(140, &dmin.to_le_bytes());
        x
    }
}

/// statx's checks of its flags and mask (EINVAL), as Linux's
/// `statx_lookup_flags` and `do_statx` make them.
pub fn statx_check(flags: u32, mask: u32) -> Result<(), i64> {
    if mask & STATX__RESERVED != 0 {
        return Err(EINVAL);
    }
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT | AT_EMPTY_PATH | AT_STATX_SYNC_TYPE) != 0 {
        return Err(EINVAL);
    }
    if flags & AT_STATX_SYNC_TYPE == AT_STATX_SYNC_TYPE {
        return Err(EINVAL);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Stat {
        Stat {
            dev: (8 << 8) | 1,
            ino: 42,
            nlink: 3,
            mode: 0o100644,
            uid: 1000,
            gid: 100,
            rdev: 0,
            size: 5000,
            blksize: 4096,
            blocks: 16,
            atime: Time { sec: 1_700_000_000, nsec: 5 },
            mtime: Time { sec: 1_700_000_001, nsec: 6 },
            ctime: Time { sec: 1_700_000_002, nsec: 7 },
            btime: None,
        }
    }

    #[test]
    fn stat_round_trip() {
        let s = sample();
        assert_eq!(Stat::from_bytes(&s.to_bytes()), s);
    }

    #[test]
    fn statx_fields() {
        let s = sample();
        let x = s.to_statx();
        assert_eq!(u32_at(&x, 0), STATX_BASIC_STATS);
        assert_eq!(u32_at(&x, 4), 4096);
        assert_eq!(u32_at(&x, 16), 3);
        assert_eq!(u32_at(&x, 20), 1000);
        assert_eq!(u32_at(&x, 24), 100);
        assert_eq!(u16::from_le_bytes([x[28], x[29]]), 0o100644);
        assert_eq!(u64_at(&x, 32), 42);
        assert_eq!(u64_at(&x, 40), 5000);
        assert_eq!(u64_at(&x, 48), 16);
        assert_eq!((u64_at(&x, 64), u32_at(&x, 72)), (1_700_000_000, 5));
        assert_eq!(u64_at(&x, 80), 0, "no birth time");
        assert_eq!((u64_at(&x, 96), u32_at(&x, 104)), (1_700_000_002, 7));
        assert_eq!((u64_at(&x, 112), u32_at(&x, 120)), (1_700_000_001, 6));
        assert_eq!((u32_at(&x, 136), u32_at(&x, 140)), (8, 1));
    }

    #[test]
    fn statx_birth_time() {
        let s = Stat { btime: Some(Time { sec: 99, nsec: 1 }), ..sample() };
        let x = s.to_statx();
        assert_eq!(u32_at(&x, 0), STATX_BASIC_STATS | STATX_BTIME);
        assert_eq!((u64_at(&x, 80), u32_at(&x, 88)), (99, 1));
    }

    #[test]
    fn device_numbers() {
        assert_eq!(dev_split((259 << 8) | 7), (259, 7));
        // Large numbers use the high bits (new_encode_dev).
        let dev = (0x12345u64 & 0xff) | ((0x1234 & 0xfff) << 8) | ((0x12345u64 & !0xff) << 12) | ((0x1234u64 & !0xfff) << 32);
        assert_eq!(dev_split(dev), (0x1234, 0x12345));
    }

    #[test]
    fn checks() {
        assert_eq!(statx_check(0, STATX_BASIC_STATS), Ok(()));
        assert_eq!(statx_check(AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW | 0x2000, 0xfff), Ok(()));
        assert_eq!(statx_check(0, STATX__RESERVED), Err(EINVAL));
        assert_eq!(statx_check(0x1, 0), Err(EINVAL));
        assert_eq!(statx_check(AT_STATX_SYNC_TYPE, 0), Err(EINVAL));
    }
}
