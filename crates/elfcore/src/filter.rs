//! Which part of a mapping a core dump writes (Linux's `vma_dump_size`), under the
//! process's `coredump_filter` (/proc/<pid>/coredump_filter, core(5)): its bits, by
//! Linux's `MMF_DUMP_*` order.

/// Anonymous private memory (and private file mappings written to).
pub const ANON_PRIVATE: u32 = 1 << 0;
/// Anonymous shared memory.
pub const ANON_SHARED: u32 = 1 << 1;
/// File-backed private mappings.
pub const MAPPED_PRIVATE: u32 = 1 << 2;
/// File-backed shared mappings.
pub const MAPPED_SHARED: u32 = 1 << 3;
/// The first page of a private file mapping that starts with an ELF header.
pub const ELF_HEADERS: u32 = 1 << 4;
/// Huge pages, private and shared, and DAX pages (kept, not used: there are neither).
pub const HUGETLB_PRIVATE: u32 = 1 << 5;
pub const HUGETLB_SHARED: u32 = 1 << 6;
pub const DAX_PRIVATE: u32 = 1 << 7;
pub const DAX_SHARED: u32 = 1 << 8;
/// The bits the file keeps (Linux's MMF_DUMP_FILTER_BITS).
pub const MASK: u32 = (1 << 9) - 1;
/// Linux's default (CONFIG_CORE_DUMP_DEFAULT_ELF_HEADERS): anonymous memory, private and
/// shared, the ELF headers and private huge pages, `0x33`.
pub const DEFAULT: u32 = ANON_PRIVATE | ANON_SHARED | ELF_HEADERS | HUGETLB_PRIVATE;

/// What a mapping is, as the filter tells mappings apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Device memory or a channel's grant (Linux's VM_IO): never dumped.
    Device,
    /// The vDSO: always dumped (Linux's `always_dump_vma`).
    Vdso,
    /// Anonymous private memory.
    AnonPrivate,
    /// A private mapping of a file (or of any other object).
    FilePrivate,
    /// Anonymous shared memory (Linux: a shmem object without a link).
    AnonShared,
    /// A shared mapping of a file.
    FileShared,
}

/// How much of a mapping goes into the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dump {
    Nothing,
    Whole,
    /// Its first page, if it starts with an ELF header (the caller looks).
    ElfHeader,
}

/// Linux's `vma_dump_size` for a mapping of `kind` under `filter`: `written` if the
/// mapping has pages of its own (Linux's `anon_vma`: anonymous memory touched, a private
/// file mapping written to), `readable` and `at_start` (its file offset is 0) for the ELF
/// header rule.
pub fn dump(kind: Kind, filter: u32, written: bool, readable: bool, at_start: bool) -> Dump {
    let whole = |on: bool| if on { Dump::Whole } else { Dump::Nothing };
    match kind {
        Kind::Vdso => Dump::Whole,
        Kind::Device => Dump::Nothing,
        Kind::AnonShared => whole(filter & ANON_SHARED != 0),
        Kind::FileShared => whole(filter & MAPPED_SHARED != 0),
        _ if written && filter & ANON_PRIVATE != 0 => Dump::Whole,
        Kind::AnonPrivate => Dump::Nothing,
        Kind::FilePrivate if filter & MAPPED_PRIVATE != 0 => Dump::Whole,
        Kind::FilePrivate if filter & ELF_HEADERS != 0 && at_start && readable => Dump::ElfHeader,
        Kind::FilePrivate => Dump::Nothing,
    }
}
