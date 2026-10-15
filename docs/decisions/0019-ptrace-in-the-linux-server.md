# ADR 0019: ptrace in the Linux server: stops in the tracee's own server thread, memory and breakpoints from the kernel

Date: 2026-10-15. Status: accepted.

## Context

Debuggers and tracers (gdb, strace, ltrace, a JIT's profiler) need `ptrace(2)`, and the
calls that share its machinery: `process_vm_readv`/`process_vm_writev` and
`/proc/<pid>/mem`. ptrace is a large part of Linux's process model: per-thread tracer
links, six kinds of stops (signal-delivery-stop, group-stop, the `PTRACE_EVENT` stops,
syscall-entry and syscall-exit stops, `PTRACE_LISTEN`'s state), their reports through
`wait`, signal injection and suppression, `PTRACE_SEIZE` against `PTRACE_ATTACH`, the
tracer's death, and the tracee's registers and memory. Processes and signals are the Linux
server's (ADR 0010), and the kernel implements no Linux call (ADR 0011). Five questions
shaped the design:

1. Which parts are Linux semantics (the server's), which mechanism only the kernel has.
2. Where a tracee stops, and how its tracer reads and changes its registers and FPU state
   while it is stopped.
3. How a tracer reaches another process's memory, with Linux's `FOLL_FORCE` (a breakpoint
   written into read-only text), without ever writing through to a file.
4. Single-stepping, `int3` and hardware breakpoints.
5. How traced threads' ends reach the tracer without an allocation at exit.

## Decision

1. **Everything Linux knows of ptrace is the server's** (`servers/linux/src/ptrace.rs`):
   the tracer and tracee relations (per thread, as Linux's: the tracer is a thread, its
   process's `wait` sees its tracees, its end detaches them), every stop and its report
   to `wait` (`(code << 8) | 0x7f`, `CLD_TRAPPED`, `SIGCHLD` to the tracer unless it
   ignores it or set `SA_NOCLDSTOP`), the options (`TRACESYSGOOD`, `TRACEFORK`,
   `TRACEVFORK`, `TRACECLONE`, `TRACEEXEC`, `TRACEVFORKDONE`, `TRACEEXIT`, `EXITKILL`;
   `TRACESECCOMP` is accepted and never fires: there is no seccomp), `SEIZE` (no `SIGSTOP`,
   group-stops as `PTRACE_EVENT_STOP`, `INTERRUPT`, `LISTEN`) against `ATTACH` and
   `TRACEME` (a `SIGSTOP`, plain group-stop reports), auto-attached children (a pending
   `SIGSTOP`, or an initial `PTRACE_EVENT_STOP` for a seized tracer), `GETSIGINFO`,
   `SETSIGINFO`, `PEEKSIGINFO`, `GETSIGMASK`, `SETSIGMASK`, `GETEVENTMSG`,
   `GET_SYSCALL_INFO`, `ARCH_PRCTL`, the register requests (`GETREGS`, `SETREGS`,
   `GETFPREGS`, `SETFPREGS`, `PEEKUSER`, `POKEUSER`, `GETREGSET`/`SETREGSET` with
   `NT_PRSTATUS` and `NT_PRFPREG`), `KILL`, `DETACH` and the tracer's end (detach, or kill
   with `EXITKILL`), and the access rules that do not depend on users (no attaching to the
   caller's own thread group, to a traced thread, to a dying one, or to another tree's,
   which the instance cannot even name; everyone in a tree is root, so pid 1 may be traced,
   as Linux lets root). The syscall stops cost nothing extra: the server sees every call
   anyway, and a thread's server checks one flag in its own block (`local`) on its way in
   and out.
2. **A tracee stops in its own server thread, and its registers wait in its record.** Every
   stop is `ptrace::stop`, on the tracee's thread in the server, where its program's
   registers are the `State` the server holds and its FPU registers are live in the CPU
   (the server uses none: ADR 0010). The stop copies the `State`, an `fxsave` image and the
   FS base (`thread_fs`) into the thread's trace record, under the process lock, reports
   the stop and sleeps (killable only, as Linux's `TASK_TRACED`) on its process's trace
   word. The tracer's requests read and change that record; a resume wakes the word, and
   the tracee loads the record back (`fxrstor`, `thread_fs`, `thread_debug` if the
   breakpoints changed) before its program goes on. So no kernel call reads or writes
   another thread's registers, and the kernel's check of every return to a program
   (`restricted_enter`: user addresses, the flags a program may have) stays the only gate.
   *Alternatives:* kernel calls on another thread's saved registers (`read_regs`,
   `write_regs` by key): the general registers are in the `State` page the server shares
   anyway, but the FPU state and the FS base of a thread stopped in its server are in its
   CPU or the kernel's save area, so a call would have to stop the thread in the kernel
   too; and the server's own aliasing of `State` (the tracee holds it as `&mut`) would be
   written under it. Rejected.
3. **The kernel gives the server another process's memory** (`SYS_VM_ACCESS`): a copy
   between a process's address space (by its handle, as it is now) and the server's
   memory, page by page through a kernel buffer, the target's space locked only for the
   page's own copy. Pages are faulted in as the target's own access would be (a pager's
   page is waited for with the space unlocked; a stack does not grow, as Linux's
   `get_user_pages`). Without `VM_ACCESS_FORCE` the mapping must allow the access
   (`process_vm_readv`, `process_vm_writev`); with it (ptrace's peeks and pokes,
   `/proc/<pid>/mem`) a read needs no read right and a write into a private mapping needs
   no write right: the page becomes the mapping's own copy first (copy-on-write broken,
   the mapping's rights kept), so a breakpoint written into a program's read-only text
   never reaches the file's page cache or a fork relative. A shared mapping is written only
   if it is writable (Linux refuses `FOLL_FORCE` writes to shared mappings since 6.2);
   `PROT_NONE` mappings, device memory and granted pages are never reached.
   `/proc/<pid>/mem` holds a handle on the space the process had when it was opened
   (`SYS_VM_SPACE`), which does not keep it: after the process's exec or end it reads and
   writes nothing, as Linux's (whose file holds the `mm` without its users).
   *Alternative:* the server mapping the target's pages into its region: a mapping per
   access, its teardown and TLB shootdown, for what a copy does.
4. **Single steps and breakpoints are the CPU's, their signals the server's.** A step sets
   TF in the tracee's flags (hidden from `GETREGS` while the tracer set it, Linux's
   `TIF_FORCED_TF`); its debug exception comes to the server as any exception
   (`REASON_EXCEPTION`, vector 1) and is `SIGTRAP` with `TRAP_TRACE`, `int3` is `SIGTRAP`
   with `SI_KERNEL` and the instruction pointer after it, as on Linux. `syscall` clears TF
   (the kernel's `SFMASK`), so a step over a system call is reported when it returns (x86's
   `user_single_step_report`: `SIGTRAP`, `TRAP_BRKPT`). **Hardware breakpoints** are
   per-thread state of the kernel's (`SYS_THREAD_DEBUG`: DR0-DR3 and DR7's enables, checked
   as Linux's `arch_build_bp_info`), set by the tracee's own server thread from what the
   tracer poked at `offsetof(struct user, u_debugreg)` when it resumes. They are armed only
   while the program runs: loaded when the thread enters its program or is switched to
   while it runs it, DR7 cleared when it leaves it, so the server's copies to program
   memory never hit a data breakpoint, and a CPU disarms before any other task runs
   (`kernel/src/process/debugreg.rs`). A debug exception hands the server DR6 (in
   `State::trap_error`; `SIGTRAP` with `TRAP_HWBKPT`, and the tracer's view of DR6), and the
   kernel sets RF for an instruction breakpoint's hit so that the instruction runs on
   resume. **A #DB in ring 0 is safe in any state** (CVE-2018-8897): a data breakpoint or
   single step that `mov ss`/`pop ss` holds off over the next instruction is delivered at
   the first instruction of the kernel entry that instruction makes (`syscall`, `int3`, a
   fault), still on the user's stack and with the user's GS. So #DB has its own IST stack
   and entry (`entry::debug_entry`): from ring 3 it moves to the task's kernel stack and
   goes the common way; from ring 0 it takes GS by IA32_GS_BASE (the user cannot set a
   kernel address there: FSGSBASE is off), acknowledges the exception (DR6 cleared, TF
   taken off the kernel's flags, the program's breakpoints disarmed until it is entered
   again) and never signals anyone. `syscall` masks TF, DF, AC and NT. Breakpoint addresses
   end below 64 TiB (never the server's region or the kernel), DR7 reaches the CPU only as
   the kernel builds it (local enables, RW, LEN; never GD; I/O breakpoints refused; other
   bits a tracer pokes are ignored, as Linux's `ptrace_write_dr7` masks them).
   *Alternative:* `EIO` for debug registers, as a kernel without
   `CONFIG_HAVE_HW_BREAKPOINT`: gdb's watchpoints would be software ones (a single step per
   instruction); the per-thread switch costs nothing for threads without breakpoints (one
   flag test per switch).
5. **A traced thread's end is reported through its own record.** When a traced thread ends
   (the service thread's `thread_ended`), its trace record becomes the zombie entry the
   tracer's `wait` reaps, moved onto a list of the tracer thread's (a pointer move, no
   allocation: an exit must not fail for memory). A process's main thread is reported when
   the whole process has ended (Linux's `delay_group_leader`); if its tracer is not in its
   real parent's process, the real parent hears of the end only once the tracer has reaped
   it (or detached by ending), as Linux's ptrace zombies. An exec by another thread drops
   the old main thread's record silently (Linux's `de_thread`), and the exec'ing thread's
   record moves with it to the process's id (`PTRACE_GETEVENTMSG` gives the old id).

Faults the kernel cannot resolve, the signal-delivery-stop's place in delivery (after a
signal is dequeued, before its action is looked at, so ignored signals stop too and a
changed or suppressed one acts as the tracer said), the restart of an interrupted call (a
tracer that changes `orig_rax` or `rax` at a stop changes it, as Linux decides restarts
from the registers), and the group stop's accounting (a traced thread takes part in it by
a `PTRACE_EVENT_STOP` or plain trap instead of sleeping, and its real parent hears of the
stop only if its tracer is in another process) are the server's signal code's
(`signal.rs`), with ptrace's hooks.

## Consequences

- The kernel's interface grew by three calls (`vm_access`, `vm_space`, `thread_debug`),
  DR6 in a debug exception's `trap_error`, and RF among the flags a program may have
  (`USER_RFLAGS`); nothing of ptrace's semantics is the kernel's.
- An untraced thread pays one load of its own flags per system call; a traced one, the
  process lock and a copy of its registers per stop.
- Not done (`docs/todo.md`): `PTRACE_SINGLEBLOCK` (`EIO`, as an architecture without
  block steps), `NT_X86_XSTATE` (`ENODEV`, as Linux on a CPU without XSAVE: the kernel
  enables no AVX state), `PTRACE_GET_THREAD_AREA`/`SET_THREAD_AREA` and 32-bit tracees,
  seccomp and its requests, `PTRACE_SET_SYSCALL_USER_DISPATCH_CONFIG`; `FOLL_FORCE`
  access to `PROT_NONE` mappings (Linux reads them; here `EIO`/`EFAULT`); the real
  parent's notice of a process end waits for the tracer's reaping of a traced main thread
  but not of other traced threads (Linux waits for all); hardware breakpoints are cleared
  at detach (Linux keeps them until exec or exit); a watchpoint on memory the server or the
  kernel touches for the program (a read(2) buffer) does not fire (Linux's kernel accesses
  record it in the tracer's DR6 without a signal); core files.
- Credentials (`docs/todo.md`, "Credentials"): Yama, `PTRACE_MODE_*` by uid, the dumpable
  flag's effect on access are not modeled: everyone in a tree is root.
