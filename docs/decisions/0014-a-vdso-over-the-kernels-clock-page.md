# ADR 0014: A vDSO over the kernel's clock page

Date: 2026-10-15. Status: accepted.

## Context

Every Linux system call of a program goes to its Linux server: two kernel round trips and two
CR3 loads, about 1 330 cycles for a call the server answers itself, and `clock_gettime`
about 1 540 cycles at the median (`scripts/bench.sh`). Linux answers the clock calls in user
mode, in its vDSO, in 20 to 50 cycles; programs call them all the time (Node.js's event loop
reads `CLOCK_MONOTONIC` around every poll and timer, libuv caches it per loop iteration,
logging and profiling read the wall clock). They are the largest share of forwarded calls
that need no state of the server.

The kernel implements no Linux call (ADR 0011). A vDSO is Linux's interface (an ELF object,
its symbol names and version, the C library's lookup), so it cannot be the kernel's; but what
it reads, the clock, is the kernel's. Five questions shaped the design:

1. What the kernel publishes, and how a reader gets it consistent while the kernel changes it.
2. Who builds the vDSO, and from what.
3. Who maps it, where, and how it survives fork and exec.
4. What it answers itself and what it hands to the system call.
5. How a program and a user find it: the auxiliary vector, `/proc/<pid>/maps`.

## Decision

1. **The kernel publishes a clock page** (`crates/clockpage`, `kernel/src/time.rs`): one
   page with the TSC's scale (`mult`, nanoseconds per cycle in 32.32 fixed point), the boot
   counter, the wall clock's starting point, each CPU's counter offset, how a reader takes
   the counter (`mode`) and which instructions the CPUs offer (RDTSCP, RDPID), under a
   sequence count: the kernel makes it odd, changes the fields, makes it even (writers
   serialized by a lock), and a reader retries until it read between two equal even counts.
   The formulas (`clockpage::mono`, `clockpage::wall`) are the kernel's own `time::now` and
   `time::realtime`, shared by both, so a reading in user mode and the kernel's
   (`SYS_CLOCK_READ`) agree to the nanosecond for the same counter value. The page changes
   when the wall clock is set (`set_realtime`, before `clock_settime` returns: a program
   sees the new time at once) and when a CPU that starts publishes its offset.

   **Per-CPU offsets with RDTSCP.** The kernel corrects a CPU's counter where it is off the
   bootstrap CPU's (`sync_ap`). A reader in user mode can move between CPUs at any
   instruction, so it cannot read "its" CPU's offset and then the counter. Each CPU's
   `IA32_TSC_AUX` holds its index (Linux's encoding, the node above bit 12, 0 here); RDTSCP
   returns the counter and the index of the same instant, so the offset applied is that of
   the CPU the counter came from. With every offset 0 (the usual case, also under KVM) the
   reader uses an ordered RDTSC (`MODE_TSC`), the kernel's own instruction; with offsets and
   no RDTSCP (`MODE_NONE`) there is no reading in user mode and the vDSO makes the system
   call. The page has room for 64 CPUs (the kernel brings up 16).

   The page is a memory object of the kernel's (`SYS_CLOCK_PAGE`, the same for every
   instance, made at boot from a committed page that stays mapped in the kernel's direct
   map). It maps only shared and never writable (`MO_SHARED | MO_READONLY`, `PROT_READ`:
   EINVAL otherwise; `mprotect` to writable is EACCES, a store SIGSEGV), `mo_write` is
   EACCES. The kernel implements no clock call in it: it keeps a page current.

2. **The vDSO is the Linux server's, in Rust, built from the repository**
   (`servers/linux/vdso/vdso.rs`, `vdso.lds`, `vdso.map`, built by `servers/linux/build.rs`
   and embedded in the server). The server's build script compiles it with the toolchain's
   rustc for the host's Linux target (whose prebuilt `core` is in the toolchain; nothing of
   it is linked: every helper inlines) into one position-independent object, links it with
   the toolchain's rust-lld into `linux-vdso.so.1`: one read-only, executable `PT_LOAD` from
   address 0 holding the headers, the dynamic symbols with SysV and GNU hash tables, the
   `LINUX_2.6` version definitions, the dynamic section, the unwind tables and the code
   (after Linux's `vdso-layout.lds.S`), and checks the result (one `PT_LOAD` at 0, R+X, no
   relocation of any kind: nothing relocates the vDSO in a program). The clock page lies
   one page below the image and is reached RIP-relative through a linker-script symbol, so
   the code has no GOT, no data and no relocations; `--no-undefined` makes any call into
   code it does not contain (a panic, a library function) a build error. The same sources
   and toolchain give the same bytes (no build id, no debug information, no paths). It uses
   no stack at all (its functions keep everything in registers; Linux's use a few words).

   *Not C:* C would need a compiler in the server's build (the musl cross-compiler lives in
   `userspace/build.sh`'s Nix shell, not in cargo's), and the clock page's layout and reader
   would exist twice. In Rust, `vdso.rs` includes `crates/clockpage/src/lib.rs` as a module:
   the kernel (writer), the vDSO (reader) and the host tests share one file.

   *Not the kernel's:* the kernel could hand out the vDSO as it hands out the clock page,
   but the object is Linux's ABI (its names, versions, `struct timespec`, the system call
   numbers it falls back to): it is the server's, as everything Linux is (ADR 0011).

3. **The server maps it at exec** (`servers/linux/src/vdso.rs`). At the instance's start
   (`ROLE_INIT`) it makes a memory object of the image's pages and gets the clock page's
   handle and both objects' numbers (`SYS_MO_ID`); every program of the tree maps the same
   pages. `execve` reserves room for both where the kernel finds some (as for an
   interpreter), maps the clock page shared and read-only as `[vvar]` and the vDSO private
   (copy-on-write, as Linux's: a debugger may write breakpoints into its copy) and
   executable one page above it as `[vdso]`, and puts the vDSO's address first in the
   auxiliary vector as `AT_SYSINFO_EHDR` (Linux's `ARCH_DLINFO`). fork keeps both as every
   other mapping; exec maps them anew. Without memory for the object, or without a clock
   page, programs run without a vDSO (no `AT_SYSINFO_EHDR`) and their C library makes the
   system calls, as with Linux's `vdso=0`.

4. **What it answers is Linux's set; the rest is the system call**, exactly as Linux's
   vDSO decides: `__vdso_clock_gettime` answers `CLOCK_REALTIME`, `MONOTONIC`, `BOOTTIME`,
   `TAI`, the two `_COARSE` and `MONOTONIC_RAW` (with the server's meanings: nothing
   suspends, so boot time is monotonic time; the coarse and raw clocks are the precise
   ones, resolution 1 ns; TAI is the wall clock); `__vdso_gettimeofday` the wall clock
   (a time zone, which is the server's, goes to the system call); `__vdso_time`;
   `__vdso_clock_getres` 1 ns for those clocks; `__vdso_getcpu` the CPU and node from
   TSC_AUX (RDPID, else RDTSCP; else the system call). The CPU-time clocks, the alarm clocks,
   invalid clocks (EINVAL, which musl passes on), and every clock when the page offers no
   reading are the real `syscall` instruction, which goes to the server as any other call.
   The return values are the system call's (0 or a negative errno), as Linux's. Each
   function has its unprefixed weak alias, as Linux's.

5. **`/proc/<pid>/maps`** (it did not exist before) lists every mapping from the kernel's
   record (`SYS_VM_AREAS`: start, end, offset, access, shared, stack, and the memory object
   by a number, `SYS_MO_ID`'s: a keyed hash of the object's address, the key random from
   boot, so it names objects without telling anything of the kernel's memory) in Linux's
   format (`show_map_vma`, names from column 74). The server names `[vdso]` and `[vvar]` by
   their objects (wherever a program moved them with mremap), `[stack]`, `[heap]` (Linux's
   rule: an anonymous area between the break's start and end) and the program file's
   mappings (its path, " (deleted)" once removed, with its device and inode); other mapped
   files show no name yet. `[vvar]` reads `r--s` (Linux's `r--p`: it is a shared mapping
   here, and like Linux's it can never be made writable).

## Consequences

- `clock_gettime`, `gettimeofday` and `time` of musl's programs (every program here, Node.js
  included) never leave user mode: see the measurements below. `sched_getcpu` neither.
- Cost: one page of kernel memory, a seqlock write per `clock_settime` and per CPU start,
  the MSR write of TSC_AUX per CPU; per instance a memory object of one page (the vDSO is
  about 2.3 KiB); per exec three mappings (a reservation replaced by two); three kernel
  calls (`clock_page` 1200, `vm_areas` 1201, `mo_id` 1202). The server's build compiles and
  links one more object (well under a second).
- The vDSO trusts the clock page: a kernel that left `seq` odd would make readers spin. The
  kernel only writes it under its lock, with interrupts off.
- A program that writes into its vDSO (after `mprotect`) changes its own copy only. The
  clock page is the same for every process: none can write it.
- The coarse clocks cost what the precise ones cost (a TSC read). Linux's tick-updated
  coarse time would need the kernel to publish the time at every tick from one CPU; with
  the precise clocks at about 50 cycles there is little to gain.
- Not done: `[vvar]`'s Linux name of the per-clock page (`[vvar_vclock]`, Linux 6.13), the
  vDSO's `getrandom` (Linux 6.11; musl does not use it), names of other mapped files in
  `/proc/<pid>/maps` and `smaps`.

## Measurements

Filled in below (`scripts/bench.sh`, several boots before and after, idle host).

Tested by `userspace/vdsotest.c` (in the self-tests): `AT_SYSINFO_EHDR`, the ELF (one R+X
`PT_LOAD`, no relocations, `linux-vdso.so.1`, `DT_HASH`, versions) and the five functions
found by musl's lookup with `LINUX_2.6` (another version finds nothing; the aliases); every
clock it answers between two system calls, boot time, nanosecond resolution, `clock_getres`
against the system call (the coarse clocks too), `gettimeofday` and `time`, the time zone,
the CPU-time clocks (also a thread's by `pthread_getcpuclockid`), the alarm clocks and
invalid clocks (EINVAL) through the system call; `clock_settime` and `settimeofday` seen at
once; `CLOCK_MONOTONIC` never going back between CPUs and across threads moving between
them; `getcpu` against the system call and `sched_getcpu` on every CPU; 20 000 clock reads
making no system call (`/proc/counters`); `/proc/self/maps` with `[vdso]` at
`AT_SYSINFO_EHDR`, `[vvar]` below it, `[stack]` and the program file, in Linux's column;
`mprotect` of `[vvar]` to writable EACCES, stores into `[vvar]` and `[vdso]` SIGSEGV; a
forked child and an exec'd program with their vDSO. `cargo test -p clockpage` tests the
formulas, the per-CPU offsets and that no reader sees a torn update while a writer changes
the page.
