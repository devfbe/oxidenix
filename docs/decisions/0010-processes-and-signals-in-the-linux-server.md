# ADR 0010: Processes and signals in the Linux server: containers, kicks, frames by the server

Date: 2026-10-09. Status: accepted.

## Context

R8 moves Linux's process model into the Linux server (`docs/design/linux-server.md`, "Processes
and signals"): pids, the tree, sessions and process groups, `fork`/`exec`/`exit`/`wait`,
signals and job control. Until now the kernel kept all of it and the server reached it through
transitional calls (`proc_ids`, `signal_group`, `signal_state`, `signal_thread`, `thread_ids`,
`thread_exists`, `fs_record`, `exec_target`, `EVENT_SESSION_END`). Five questions decide the
shape of the kernel's interface:

1. How a signal reaches a thread that runs its program, sleeps in the server, or waits in the
   kernel in a call the server passed through.
2. Who builds signal frames, and how the program's FPU state gets into them.
3. How `fork` clones memory and threads.
4. Where `execve`'s ELF loader lives.
5. How the kernel still starts a tree's first process (the monitor, autorun) and its native
   servers.

## Decision

1. **Kicks.** The kernel offers `thread_kick(key)`: a flag per thread (Linux's
   `TIF_SIGPENDING`) and a way back into the server: `REASON_KICK` from `restricted_enter`
   (an IPI for a thread running on another CPU), EINTR from any interruptible wait. The flag
   is cleared only by `restricted_enter`, which returns at once while it is set, so a signal
   posted after the server's last look still stops the program before it runs, and every
   wait after a kick ends. The kernel knows no signal numbers, masks or actions; posting a
   signal is server state under one lock plus a kick of the thread that takes it.
   `thread_kill(key)` marks a thread dying (every wait ends) and it exits at its next
   `restricted_enter`, where the server never holds a lock. Thread exits come to the service
   thread as `EVENT_THREAD_EXIT` after the thread's references are gone.
   *Alternatives:* signal numbers in the kernel (a pending set the server reads): the kernel
   would keep Linux semantics (masks decide which thread to wake); an upcall that pushes a
   frame from the kernel: the frame's layout and the restart rules are Linux's, and the
   server would have to tell the kernel everything it knows.
2. **Frames by the server**, in Linux's x86-64 `rt_sigframe` layout (`ucontext` with
   `sigcontext`, `siginfo`, `fxsave` image), on the program's stack with the server's copy
   routine. The program's FPU registers are live in the CPU while the server runs on its
   thread (the server uses no FPU and no FS/GS base), so the server saves them with
   `fxsave64` and restores them with `fxrstor64` itself (MXCSR's reserved bits cleared first);
   no kernel call per signal. `rt_sigreturn` is the server's; the kernel's `restricted_enter`
   still checks every return (user addresses, harmless flags).
   *Alternative:* `read_regs`/`write_regs` calls for the FPU state: two kernel entries per
   signal for state the server's thread already has in its CPU.
3. **fork clones the address space, not objects.** `proc_create(PROC_FORK)` makes a process
   whose address space is a copy-on-write clone of the caller's in one call (page tables and
   areas are the kernel's since R4); `thread_create` puts a thread into it with the caller's
   FPU state. A `mo_clone_cow` per memory object (as sketched in the design's table) would
   need the server to keep and replay every area, a kernel call per area, and a second copy
   of what the kernel already knows; it is not needed, and would come only with a use for a
   copy-on-write snapshot of a single object. Threads are named by **keys** (thread area and
   generation), processes by handles: no kernel id appears in Linux's namespace, which is the
   server's (pids per instance, its first process pid 1).
4. **The loader moves into the server.** It resolves and checks the program, reads the ELF
   headers from its own file object, copies the arguments, and only then calls `exec_space`,
   the point of no return: the kernel swaps in a fresh address space (attached to the
   instance; the program file's hold keeps it unwritten while it runs) and resets the FPU
   state and the FS base. The server maps the segments, the bss and the stack with its memory
   calls and writes the stack itself. The kernel keeps its loader for its native servers
   only (not Linux programs; R9 leaves them the kernel's interface).
5. **The first process of a tree starts in the server.** The kernel makes the process with an
   empty address space and the first thread in `ROLE_INIT`, and keeps the command line for it
   (`init_args`); the server registers pid 1 (`proc_self`), gives it the console and execs
   the program, as Linux's `kernel_init` runs `/init`. Native servers start as before.
   The kernel still waits for that process (the monitor needs its end), so it alone is a
   zombie of the kernel's as well; everything the server creates leaves the kernel's tables
   when its last thread ends.

Interval timers get a service thread of their own (`ROLE_TIMER`): the kernel's timers stay
mechanism (deadline sleeps), the timer's semantics (SIGALRM, reloading when the signal is
taken) are the server's.

## Consequences

- The kernel loses `exec.rs`'s and `loader.rs`'s Linux side, the Linux parts of `signal.rs`,
  `exit.rs` and `clone.rs` (the native servers keep the rest until R9), and every
  transitional call of R6/R7 that answered with the kernel's pids, groups or signal state.
- One lock guards the instance's processes and signals; it is never held across a copy to or
  from program memory, so the service thread (which must not wait for a page) can handle
  exits.
- Pids are per instance: two trees both have a pid 1, and `/proc/<pid>` is the server's.
  Kernel ids appear only in the kernel's monitor and messages.
- A signal costs a kick (a kernel call, and an IPI for a running thread) and the server's
  frame building; `fork` two calls (`proc_create`, `thread_create`); `exec` the server's
  loader with a few mapping calls. `iobench` measures fork, exec and wait.
- Not done: pidfds (`CLONE_PIDFD`, `P_PIDFD`), POSIX timers (`timer_create`),
  `ITIMER_VIRTUAL`/`ITIMER_PROF`, core files, ptrace, a vDSO.
