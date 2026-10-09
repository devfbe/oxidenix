# ADR 0009: The descriptor table in the Linux server: per-table records until R8, the kernel's files by handle, readiness through the server's watches

Date: 2026-10-09. Status: accepted.

## Context

Through R6 and R7 the server took over every kind of file but kept the kernel's descriptor
table: a file of the server's was a placeholder in it, and poll, select and epoll were the
kernel's, over the readiness the server reported (`kfd_ready`). That cost a kernel call for
every descriptor a call used (`kfd_lookup`, with a pin so that another thread's close could
not take the file from under the call), for every readiness change (`kfd_ready`), and a
family of bridges for descriptors in flight (`kfile_object`, `kfd_install_file`,
`kfile_info`, `EVENT_INFLIGHT`). `fstat` of a file on /data took 9 to 10 system calls of the
kernel's per call. The kernel also kept Linux semantics it should not have: descriptor flags,
close-on-exec, dup, fcntl and the event loop interfaces.

R6e moves the table, poll, select and epoll into the server. Three questions:

1. **Who decides which processes share a table** while processes (fork, exec, exit,
   CLONE_FILES) are still the kernel's (until R8)?
2. **What becomes of the kernel's own files** a program still opens (the kernel's `/dev`:
   null, zero, the directory)?
3. **How does the server wait for many files at once**, with ppoll's and pselect6's
   temporary signal masks, and how do readiness changes reach it?

Alternatives considered:

- *Tables as a kernel object the server manipulates by handle* (the kernel keeps slots of
  opaque words, the server fills them): keeps the kernel in every lookup, which is what R6e
  is to remove, and keeps fork's and exec's semantics in the kernel.
- *The server tracks processes itself* (it sees every clone and exec pass through): it would
  have to guess the kernel's decisions (a clone that fails late, CLONE_FILES without
  CLONE_THREAD, a thread's exec killing its siblings) and duplicates R8's work.
- *The kernel's files as placeholders of the kernel's in the server's table* with readiness
  reported by the kernel: every kernel file a program can still reach is always ready, so
  there is nothing to report.
- *poll waiting on every polled file's own word* through a vectored kernel wait (no
  subscription lists): scales with the number of distinct words the kernel must queue on
  (thousands for select), and epoll needs subscriptions anyway.

## Decision

- **Records, as for working directories.** Each descriptor table of the kernel's carries a
  record of the server's (`SYS_FILES_RECORD`, the same protocol as `SYS_FS_RECORD`): the
  kernel's clone still decides who shares a table (CLONE_FILES), the server hands the kernel
  the table a clone makes (a copy made before the call passes through); an execve that
  succeeded says so when it returns to the server, which makes the new program's table then
  (the copy without close-on-exec descriptors, from the point of no return on) and lets the
  old one go on that thread before the new program runs; the kernel gives a record back when
  the last process using its table exited (`EVENT_RELEASE`), and the worker thread lets the
  table go and its descriptors close (never the pager: closing a socket takes its locks). The kernel's tables of Linux programs hold no descriptors. A thread
  remembers its table in the server's own words of its State page, so a descriptor's lookup
  is a lock and a reference count. R8 takes over the decisions through a small interface:
  `FilesContext::fork`, `FilesContext::for_exec`, and dropping a table at exit.
- **Open file descriptions are the server's objects** with reference counts (descriptors,
  calls that use them, descriptors in flight): a file closes with its last reference, on
  whatever thread lets go of it, as Linux's fput; SCM_RIGHTS moves references in the
  server's memory, and the collector of sockets in flight compares a description's
  references with those in flight.
- **The kernel's files by handle**: `inode_open` returns a handle on the kernel's open file
  description, which the server's description holds; its calls go to the kernel with the
  program's buffers (`kfile_call`), mmap maps the handle. They are always ready.
- **Readiness through the server's watch lists**: every file reports to its description's
  watch (as before to the kernel), pollers and epoll interests subscribe to it; readiness is
  asked of each file when it is checked. The kernel's part is one primitive, a wait for any
  of up to 64 words of the server's memory (or of objects mapped there) with a deadline and
  a temporary signal mask (`SYS_SERVER_WAIT`): a poller waits on its own word and on the
  control-block words netd wakes for the internet sockets it polls.
- **Restart semantics by code**: the server returns Linux's kernel-internal restart codes
  (ERESTARTNOHAND for select and ppoll, ERESTART_RESTARTBLOCK for poll, whose deadline it
  keeps for restart_syscall), and the kernel's signal delivery maps them as Linux's does.

## Consequences

- No kernel call for a descriptor's lookup, a readiness change, dup, close or fcntl; the
  bridges for placeholders and descriptors in flight are gone from the ABI, and so are the
  kernel's epoll, poll and select (the native servers never used them).
- An exited process's descriptors close on the worker thread; close-on-exec descriptors close
  on the execve's thread before the new program runs (a reader sees end of file at once, a
  port is free for the new program); close(2) closes on the calling thread before it
  returns.
- The copy at fork and execve costs what Linux's dup_fd costs: one reference per descriptor.
- RLIMIT_NOFILE is kept with the table until processes are the server's (CLONE_FILES
  without CLONE_THREAD shares it).
- With R8 the records go: the server's process records own their tables directly.

## Update (R8, ADR 0010)

The records went with R8: each thread record of the server's process table holds its table
(`CLONE_FILES` shares it, a fork copies it before the child exists, an execve makes the new
one after its point of no return), and an exiting thread lets its table go itself (one the
kernel ended in its program: the worker). The temporary signal masks and the restart codes
are the server's own signal delivery's now; `server_wait` takes no mask any more.
RLIMIT_NOFILE stays with the table.
