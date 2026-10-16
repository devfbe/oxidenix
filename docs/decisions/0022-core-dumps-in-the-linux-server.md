# ADR 0022: Core dumps in the Linux server: threads park with their registers, memory from the kernel

Date: 2026-10-16. Status: accepted.

## Context

Ten signals (SIGQUIT, SIGILL, SIGTRAP, SIGABRT, SIGBUS, SIGFPE, SIGSEGV, SIGXCPU, SIGXFSZ,
SIGSYS) have the default action "core": Linux writes an ELF core file (`ET_CORE`) of the
process before it ends, if its soft `RLIMIT_CORE` allows (0 by default) and it is dumpable
(`PR_SET_DUMPABLE`), and the wait status gets the core flag (`WCOREDUMP`, `CLD_DUMPED`) only if
the file was written. The file has a `PT_NOTE` segment (per thread `NT_PRSTATUS` with the
registers and `NT_PRFPREG` with the FPU state; for the process `NT_PRPSINFO`, `NT_SIGINFO`,
`NT_AUXV`, `NT_FILE`) and a `PT_LOAD` per mapping whose dumped part Linux's `vma_dump_size`
decides under `/proc/<pid>/coredump_filter`: anonymous memory that was written to (its
`anon_vma`), anonymous shared memory, the first page of a file mapping that starts with an
ELF header, the vDSO; pages nobody touched are holes (`FOLL_DUMP`). The name comes from
`/proc/sys/kernel/core_pattern` (its `%` specifiers, or `|program`) and `core_uses_pid`.
Processes, signals and their exits are the Linux server's (ADR 0010), the kernel implements
no Linux call (ADR 0011), and ptrace already reads another thread's registers in its own
server thread and another process's memory through the kernel (ADR 0019). The questions:

1. Where the dump is made, and which thread makes it.
2. How the registers and FPU state of the process's other threads get into the file, and
   what keeps them (and the memory) from changing while it is written.
3. How the dumper reads the memory: without faulting in pages nobody touched (a
   reservation of gigabytes would be committed) and knowing which areas were written to.
4. How the file is opened and written, by whom, and what bounds it.

## Decision

1. **The server dumps, on the thread that takes the signal** (`servers/linux/src/coredump.rs`,
   Linux's `get_signal` → `do_coredump` → `do_group_exit`). A signal that dumps core is not a
   group exit when it is posted (Linux's `complete_signal` leaves it to delivery): the chosen
   thread dequeues it, its delivery finds the default action "core", and it dumps before the
   process ends (`signal::deliver`, `Next::Die`). A traced thread's signal-delivery-stop comes
   first, `PTRACE_EVENT_EXIT` after the dump, as on Linux. Not dumpable, a soft `RLIMIT_CORE`
   below a page (Linux's `min_coredump`), or a pipe pattern: no dump, the process ends as by
   "terminate".
2. **The other threads park in their own server threads, with their registers.** The dumper
   starts the group exit (every other thread killed) with a record in the process
   (`Proc::core`: room for each thread's registers, allocated before; a clone of an exiting
   process fails, so none is added). Each killed thread comes back to its server (from its
   program, `REASON_EXIT`, or from an interrupted call through delivery) and, before anything
   of its end (its robust futex list, its descriptor table, the kernel's clear of its tid
   word), leaves its registers in the record (`park`: the `State` the server holds, the call
   it was in as `orig_rax` with the restart code in `rax` as Linux shows a thread blocked in a
   call, its `fxsave` image, its FS base), and waits until the dump is over — a wait its
   death does not end (`FUTEX_LOCK`'s, which only the word's wake or a broken instance ends:
   Linux's `coredump_task_exit` sleeps uninterruptibly too). The dumper waits until every
   other thread has parked or ended, so no thread runs the program while its memory is read,
   and every way the dump ends (written, failed, a SIGKILL, the kernel's kill, a broken
   instance) wakes the parked threads. A SIGKILL to a dumping process stops the dump (the
   dumper is killed, its reads and writes come back short) and the process ends by SIGKILL,
   without the core flag (Linux's `dump_interrupted`). *Alternative:* a kernel call reading a
   stopped thread's registers: rejected for the same reasons as in ADR 0019 (the FPU state
   and FS base of a thread in its server are its CPU's), and it would still need the thread
   held.
3. **The memory comes from the kernel, which also says which pages exist.** The mappings are
   `vm_areas`', the contents `vm_access` with `VM_ACCESS_FORCE` (a mapping's read right is not
   needed, as Linux's `FOLL_FORCE`). A new call, `vm_resident` (`SYS_VM_RESIDENT`, 1280),
   reports the runs of resident pages of a range and whether they are the mapping's own
   (`VM_RUN_PRIVATE`: anonymous memory and private copies of file pages, the page table's
   `ANON` bit), walking only the page tables that exist and faulting nothing in. It gives
   Linux's two notions: an area with an own page was written to (`anon_vma`), and in
   anonymous memory a page without one is a hole (`FOLL_DUMP`), so a reservation of
   gigabytes costs what was touched. `vm_areas` marks anonymous shared memory
   (`VMA_ANON_SHARED`: the kernel's `mo_create` objects), which the filter tells from a file's
   shared mapping. File-backed areas the filter dumps whole are read whole (their pages are
   faulted in from the page cache, as Linux's). A page that cannot be read (`PROT_NONE`) is a
   hole. *Alternatives:* reading through the server's own view of the address space
   (`usercopy`): every untouched anonymous page would be faulted in (committed) and nothing
   tells an area that was written to; a byte per page instead of runs: as large as the range,
   for reservations of gigabytes.
4. **The file is the server's own open, with Linux's rules, and its format a library.** The
   pattern is expanded in the dumping thread's terms (`%p %P %i %I %u %g %d %s %t %h %e %f %E
   %c %C %%`, values escaped as Linux's `cn_esc_printf`; `.PID` appended for `core_uses_pid`
   when there is no `%p`) and resolved from its working directory (`paths::core_file`):
   created with mode 0600 less the umask, opened without following a final symlink and without
   a descriptor (`tmpfile::open_server`, `datafile::open_server`: Linux's `filp_open`), kept
   only if it is a regular file with one link and mode 0600 (Linux's "cannot preserve file
   permissions"), truncated only then. The file is laid out as Linux's `elf_core_dump` (the
   header, the program headers with extended numbering past 65534, the notes in Linux's order,
   the data from the next page, a section header after it for extended numbering) by
   `crates/elfcore`, which also holds the filter and the pattern and is tested on the host.
   Bytes written (not holes) count against `RLIMIT_CORE`; a write past it ends the dump there,
   the file cut short and no core flag, as Linux's `dump_emit`. A file that ends in a hole gets
   its last byte written. **`core_pattern` and `core_uses_pid` are each tree's own** (the
   instance's, ADR 0002; Linux's are the machine's), writable by its processes;
   `/proc/<pid>/coredump_filter` is the process's (copied by fork, kept by execve; Linux keeps
   it with the mm). **A pipe pattern runs nothing**: no file, a line in the kernel's log.

## Consequences

- The kernel's interface grew by `vm_resident` (1280) and `VMA_ANON_SHARED`; nothing of a core
  file is the kernel's.
- A thread's end looks at one record under the process lock (`park`), which costs nothing
  measurable; a dump holds the process's threads until its file is written.
- Not done (`docs/todo.md`): a pipe to a helper program (`|program`, `core_pipe_limit`, `%F`):
  it would need the server to start a process in the tree's initial namespaces with the dump
  on its standard input; `NT_FILE` and the anonymous-or-file distinction know only the
  objects the server can name (the program file; an unlinked tmpfile counts as a file);
  `MADV_DONTDUMP` is not kept; `PROT_NONE` pages are holes; `NT_X86_XSTATE` (no XSAVE state
  is enabled, as for ptrace); the dumpable flag's credential rules (`suid_dumpable`).
