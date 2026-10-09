# Symmetric multiprocessing in oxidenix

Status: implemented (all six steps). The README describes the details as built. Since then,
syscalls run with interrupts enabled (the kernel stays non-preemptive; a switch an interrupt
asks for happens at the next return to user space, or at a `cond_resched` in long kernel work).

## Goals

- All CPUs run user programs **and kernel code** in parallel. There is no big kernel lock:
  every shared structure has its own lock with a short critical section.
- Correctness first: no lost wakeups, no task running on two CPUs, no use of a kernel stack
  that another CPU still runs on, no deadlocks through interrupts.
- The Linux syscall ABI and every existing test keep working on 1 to N CPUs.

## Starting point (single CPU)

The kernel was non-preemptive and ran syscalls with interrupts disabled (`SFMASK` clears IF);
only user code and the idle loop are interrupted. On one CPU this made `without_interrupts`
a sufficient lock, and "check a condition, then `sleep_on(chan)`" race-free. Neither holds
on several CPUs, so these are the parts that change.

## Building blocks

### Locks

- `IrqSpinLock<T>`: a ticket spinlock (FIFO-fair) that disables interrupts while held and
  restores the previous interrupt state on release. Every lock that an interrupt handler may
  take is an `IrqSpinLock`; so are almost all kernel locks. State held across operations
  that sleep (an address space during a page fault that reads a file, a file's offset during a
  read from a file server) uses a sleeping `sync::Mutex` instead.
- No lock is held across a context switch, except the run-queue lock, which `schedule`
  hands over to the next task and releases in `finish_switch`.
- Lock order (outer to inner): process table → process info → process signal state → thread
  signal state → wait-queue bucket → task wake lock → run queue. IPC, TTY, console, frames and
  heap are leaves.

### Per-CPU data

Each CPU has a `Cpu` block (id, APIC id, current task, idle task, run queue, GDT, TSS with
I/O bitmap, double-fault stack, scratch slot for the user stack pointer). `IA32_GS_BASE`
points to it while in the kernel; entries from user mode execute `swapgs` (user GS base is
always 0, `IA32_KERNEL_GS_BASE` holds the block). All interrupt and exception entries go
through one assembly stub that builds the uniform `Frame`, swaps GS when coming from ring 3
and dispatches by vector.

### Interrupt controllers

- ACPI: the RSDP from the boot info leads to the MADT, which lists the CPUs (local APIC ids),
  the I/O APIC and the interrupt source overrides (IRQ 0 is GSI 2 on QEMU, PCI lines are
  level-triggered).
- The 8259 PICs are masked. Each CPU uses its local APIC (xAPIC, MMIO mapped uncached), whose
  timer is programmed for the next deadline of the CPU's timer queue (`timer.rs`): in
  TSC-deadline mode where available, else one-shot with a frequency calibrated against the
  PIT. The scheduler tick (preemption every 10 ms) is one of those deadlines.
- The I/O APIC routes ISA and PCI interrupts. Device lines handed to user-space drivers are
  masked in their redirection entry when they fire and unmasked by `irq_enable`.
- Inter-processor interrupts: reschedule (wake an idle CPU), panic (stop all CPUs).

### Tasks

A process (`Task`, shared as `Arc<Task>`) splits into:

- **Owned state** (address space, descriptors, cwd, brk/mmap, FPU, FS base, I/O bitmap):
  touched only by the task itself, by the CPU switching to or from it, or by the reaper once
  it is dead and off every CPU.
- **Shared state** under locks: relations (parent, process group, session) and exit/stop
  reports under the process-table lock; signal actions, mask, pending set and timers under
  the task's signal lock; the scheduling state as an atomic.

### Scheduling

- Per-CPU run queues. An idle CPU steals from the longest other queue. A woken task goes to
  the CPU it last ran on unless another CPU is idle; idle CPUs are woken by IPI.
- `on_cpu` marks a task whose kernel stack is still in use. A CPU never picks a task whose
  `on_cpu` is still set (a CPU queues its current task before it switches away): it leaves it
  in its queue for a later schedule. Waiting for it with interrupts off could close a cycle,
  two CPUs each waiting for the other's outgoing task. The reaper frees a dead task only once
  `on_cpu` is clear.
- Each CPU has an idle task; idling loads the kernel page table, so an address space is only
  ever loaded on the CPU that runs its (single-threaded) process. No TLB shootdowns are
  needed as long as processes have one thread.

### Sleeping without lost wakeups

Every blocking path uses the same protocol:

```text
loop {
    let wait = prepare_to_wait(chan);   // state = Sleeping(chan), queued in chan's bucket
    if condition() { break }            // a wakeup from now on is not lost
    if signal pending { return EINTR }
    wait.sleep();                       // schedules unless already woken
}                                       // dropping `wait` dequeues the task
```

`wakeup(chan)` moves every task waiting on `chan` from Sleeping to Runnable with a
compare-and-swap and enqueues it. Signals use the same transition.

### Starting the other CPUs

A real-mode trampoline in a low page (reserved before the frame allocator hands it out)
switches straight to long mode with the kernel's page table, then jumps to `ap_entry` on a
fresh stack. The bootstrap CPU starts the others one at a time with INIT and two
STARTUP IPIs.

## Steps

1. `IrqSpinLock` and conversion of existing locks.
2. ACPI MADT, MMIO mappings, local APIC and I/O APIC instead of the PICs and the PIT.
3. Per-CPU blocks, `swapgs`, the uniform interrupt entry, per-CPU GDT/TSS.
4. Task split, wait protocol, per-CPU run queues, `on_cpu`, idle tasks (still one CPU).
5. Application processor start-up; QEMU with `-smp 4`.
6. `/proc/cpuinfo`, `sched_getaffinity`, the monitor's `cpus`, an SMP stress test.
