# ADR 0021: Dynamically linked programs from a Nix store on the data disk

Date: 2026-10-15. Status: accepted.

## Context

Until now only static musl programs ran (Bash, BusyBox, Node.js). The ELF loader in the
Linux server (`servers/linux/src/exec.rs`) handled `PT_INTERP`, `ET_DYN` and the auxiliary
vector, but no dynamically linked program had ever run: most programs on Linux are
dynamically linked against glibc, and the interesting parts of dynamic linking happen in user
mode (the dynamic loader maps libraries with `mmap`, write-protects RELRO with `mprotect`,
sets up TLS with `arch_prctl`, registers rseq and robust lists, starts threads with `clone3`,
`dlopen` adds libraries and TLS at run time).

Four questions:

1. How the programs, their interpreters and their libraries get into the machine. Programs
   built by Nix name their interpreter by its store path
   (`/nix/store/<hash>-glibc-2.44-25/lib/ld-linux-x86-64.so.2`) and find their libraries
   through `RUNPATH`s into the store; the initramfs is read by the bootloader at about
   1 MB/s without KVM and lives in memory (the root tmpfs); glibc's `libc.so.6` alone is
   2.4 MB, with libstdc++ and the C.UTF-8 locale the libraries come to about 9 MB.
2. What the loader and the server lack (found by running them; the server says "syscall N
   not implemented").
3. rseq: glibc (2.35 and later) registers every thread's rseq area at its start.
4. membarrier: musl's dynamic loader registers for `MEMBARRIER_CMD_PRIVATE_EXPEDITED` at
   every start and uses it when `dlopen` adds TLS while other threads run.

## Decision

1. **The data disk holds a Nix store; the programs stay as Nix built them.**
   `userspace/dynamic/default.nix` builds the test programs with nixpkgs' glibc and musl
   toolchains (no patchelf) and lists in `closure` exactly the store files they need: what
   each dynamic loader resolves (`ld.so --list`), what they open beyond that (`libm.so.6` by
   `dlopen`, glibc's `C.utf8` locale), and the programs' own outputs; not the packages' whole
   closures (glibc's output alone is 30 MB, mostly gconv modules and headers). The builder
   (`builder/src/main.rs`) copies these files onto the data disk under their own paths
   (`/nix/store/...`, with debugfs, compared with what is there and read back, so the
   persistent disk gets what changed only), puts copies of the programs in the initramfs's
   `/bin` (40 to 60 KB each) and makes `/nix` a link to `/data/nix`. A real program from
   nixpkgs runs as it is (GNU hello, `/bin/gnu-hello` a link into the store), and a store
   path copied from any Nix machine onto `/data/nix/store` would too. The initramfs grows by
   the three test programs and a link (about 150 KB); the data disk by about 9 MB (of the
   test disk's 64 MiB). Without a data disk dynamic programs do not run (ENOENT for their
   interpreter), as on Linux without `/nix`.

   Rejected: patching the programs to `/lib` (patchelf) would test something other than the
   programs Linux users run and needs every new program patched; the closure in the
   initramfs would cost several seconds of every boot without KVM and its memory at all
   times; the whole closures would cost tens of MB of disk for files nothing opens.

2. **What was missing.** The loader's auxiliary vector, `ET_DYN` placement, the
   interpreter's own mapping, `mmap` of shared libraries with their protections, RELRO,
   `arch_prctl`, `set_tid_address`, `set_robust_list`, `clone3`, `/proc/self/exe`,
   `prlimit64`, `getrandom` and the vDSO already worked: glibc's and musl's loaders, glibc's
   pthreads, dynamic TLS, libstdc++'s exceptions and threads, posix_spawn and the loader run
   as a program passed at once. Running them found:
   - **A bug**: a vfork child's execve replaced its parent's program break record (it was
     shared, as the address space is until the execve). The server's per-address-space
     record (`process::Mm`: the break and the membarrier registrations) is now shared by
     `CLONE_VM`, copied by fork, and new with every execve (made before the point of no
     return). glibc's `system(3)` and `posix_spawn` are such vforks, and glibc's malloc
     grows its main arena with brk.
   - `rseq` (334) and `membarrier` (324) were logged as not implemented.

3. **rseq: ENOSYS, as a Linux built without `CONFIG_RSEQ`.** glibc then sets `__rseq_size`
   to 0 and goes on (`sched_getcpu` asks the vDSO's `getcpu`); the server answers without
   logging. A real rseq needs the kernel: on every return to user mode after a preemption,
   a migration or a signal it would have to write the CPU number into the registered area
   and move an instruction pointer inside a registered critical section to its abort
   handler, with the kernel writing program memory on its return path; the signal frames
   are the server's, so both would take part. That is a mechanism of its own (and the
   scheduler's return path), worth it once a program needs per-CPU data structures
   (tcmalloc, liburcu's per-CPU flavor); `docs/todo.md` lists it. membarrier's rseq
   commands are EINVAL accordingly.

4. **membarrier: the barrier is the kernel's, the commands are the server's.** The kernel
   gets one mechanism, `SYS_MEMBARRIER` (1270): an interrupt on every CPU that has the
   caller's address space loaded (or on every CPU), acknowledged before the call returns,
   over the TLB shootdown's IPI protocol (`kernel/src/process/tlb.rs`, `barrier`: a request
   with an empty range). The IPI, its locked acknowledgement and `iret` give the full
   barrier and the core serialization `SYNC_CORE` asks for; a CPU that loads the address
   space meanwhile joins its CPU set with a SeqCst operation before it loads CR3. The server
   (`servers/linux/src/mm.rs`) keeps Linux's commands: QUERY, the registrations per address
   space (EPERM for an unregistered private command, `GET_REGISTRATIONS`), fork and execve,
   and the global commands as an interrupt on every CPU (which covers every registered
   process, and GLOBAL's every running thread). `TEST_BARRIERS` (1640) counts the barrier
   interrupts each CPU served, for `membarriertest` to check which CPUs a barrier reaches.

## Consequences

- `dyntest` (glibc) and `dynmusl` (musl, the same source) check the auxiliary vector
  against what the loader made of it, RELRO, TLS of the program and of a `dlopen`ed library
  (also in threads that ran before it was loaded), threads, C.UTF-8, the vDSO's clocks
  without system calls, fork and exec of `/proc/self/exe`, posix_spawn, `system(3)`, the
  break across them, and the interpreter run as a program; `dyncxx` C++ exceptions,
  `std::thread` and `thread_local` destructors with the shared libstdc++; GNU hello from
  nixpkgs runs. All of them pass on Linux too. The read-only boot runs the first two from
  the read-only store.
- The builder needs Nix for the data disk's store, as it does for the toolchains already.
- The persistent `disk.img` keeps the store paths of earlier nixpkgs pins (store paths never
  change, new ones are added); nothing collects the old ones yet.
- Other dynamically linked programs need their store paths on `/data/nix/store` (a Nix
  closure copied there); nothing in the guest installs them yet.
