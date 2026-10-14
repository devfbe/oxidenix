# ADR 0013: An OOM killer by badness, and Linux's overcommit modes

Date: 2026-10-14. Status: accepted.

## Context

The kernel commits memory strictly (Linux's `vm.overcommit_memory=2` with a ratio of 100 %):
writable private memory is promised when it is mapped, so running out is an ENOMEM at
`mmap`, `brk`, `mprotect` or `fork`. Memory nobody promised up front could still run out at a
touch: a `MAP_NORESERVE` page committed at its first touch (V8's code range), and frames that
reclaim cannot give back in time. Then the toucher was killed, whoever held the memory. That
is the right answer to a refused commit (the toucher asked for more than was promised), but
the wrong one to memory running out: the process that touches last is rarely the one that
took the memory.

Strict accounting also leaves Node.js little headroom: musl commits 8 MiB per thread stack,
eight workers promise about 190 MB of a 256 MB machine and touch less than half of it. Linux's
default, the heuristic mode, lets such promises through and settles the account at fault
time with the OOM killer. That mode needs an OOM killer that picks its victim by what it
holds.

Five questions shaped the design:

1. Whom the killer chooses, and across which processes: all trees, or each tree within itself?
2. When it acts, given allocations that may wait and allocations that may not.
3. How a victim's memory comes back if the victim cannot run to its end.
4. Which overcommit modes exist, who may change them, and which is the default.
5. Where Linux's interfaces live (oom_score, oom_score_adj, oom_adj, /proc/sys/vm), given
   that the kernel implements no Linux call (ADR 0011).

## Decision

1. **The victim is chosen by badness, across all trees** (`kernel/src/memory/oom.rs`, Linux's
   `oom_badness`): resident pages plus page tables (there is no swap), plus oom_score_adj
   thousandths of all memory. The kernel counts each address space's resident pages by kind
   (anonymous, shmem, file: two software bits in the page table entries) and its page tables,
   so the badness, the kill message and `/proc/<pid>/status` (RssAnon, RssFile, RssShmem,
   VmPTE) are exact. Never chosen: the kernel's threads, the native servers (diskfs, netd,
   procfs), a Linux server instance's service process, the first process of a tree that holds
   the host grant (Linux's global init; another tree's first process is a container's init,
   which Linux kills), and a process with oom_score_adj -1000. Every process sharing the
   victim's address space (`PROC_SHARE_VM`, vfork) dies with it.

   **Global, not per tree.** Memory is one pool: no tree has a limit of its own. A killer
   confined to the tree whose allocation failed would make the trees that need memory pay
   for the tree that took it (a hog in one tree, a shell in another touching one page: the
   shell's tree would lose a process, the hog none). Linux's memory cgroups kill within a
   group only when the group exceeds its own limit, and fall back to the global killer when
   the machine runs out. The equivalent here, a per-tree memory limit with an OOM within the
   tree, needs per-tree charging of every frame (anonymous pages, page tables, the cache's
   pages by owner) and is left for when trees get limits; the global killer stays its
   fallback then too. Isolation between trees is kept where it matters: a tree without the
   host grant cannot shield its processes (lowering oom_score_adj below the least it was
   given needs the grant, Linux's CAP_SYS_RESOURCE), can only make its own processes more
   likely victims, and cannot change the overcommit policy.

2. **When.** An allocation that may wait (a page fault, the kernel's or the Linux server's
   copy into program memory, a fill of a cached page) reclaims and retries as before
   (`reclaim_retry`: 16 rounds of 100 ms without progress, 300 while write-back may still
   clean pages); when it would give up it calls the killer (`out_of_memory`, Linux's
   `__alloc_pages_may_oom`) and, once a victim is killed, reclaims and retries anew. Only if
   no process may be killed does it fail, and its process dies as before. Allocations that
   cannot wait (page tables made with the frames locked, the kernel heap's growth, the Linux
   servers' own heap pages, which take frames of the kernel's reserve when they must not
   fail) have the background reclaimer act for them: when user memory stays exhausted as long
   as such a retry lasts, it calls the killer itself. One victim at a time: while a victim's
   memory has not come back (its address space alive and not reaped), no other is chosen
   (Linux's `tsk_is_oom_victim`). A commit refused in the strict mode is still the toucher's
   end (`MAP_NORESERVE`): it asked for more than may be promised.

3. **A victim gets to its end.** Its faults may take frames of the upper half of the
   kernel's reserve (Linux's TIF_MEMDIE; the lower half is the kernel heap's alone), as may
   the Linux server's allocations that cannot fail (its heap's commits from the instance's
   reserve: in a mode that overcommits, a commitment is no frame), at most as many frames as
   that reserve holds commitment for (a commit beyond it, which an overcommitting mode grants
   at will, never reaches the kernel's reserve). Every doubtful case fails closed: a tree's
   first process whose grant cannot be looked up (no memory) counts as the host's and is not
   killed, a victim that could not be queued still holds the next choice off, and lowering
   oom_score_adj without the grant is refused if any process sharing the address space would
   go below its own least. A kernel thread, the **OOM
   reaper**, frees the victim's private memory (anonymous pages, private copies, with their
   commitment) as soon as it gets the victim's address space (ten tries, 100 ms apart), so
   memory comes back even while a thread of the victim is held up; a reaped space makes no
   pages any more (its faults fail: the process is dying). Then the victim no longer counts
   (MMF_OOM_SKIP), and another may be chosen if memory is still short.

4. **Linux's three modes, strict by default.** `vm.overcommit_memory` 0 (heuristic: a commit
   fails only if it alone is more than all memory), 1 (always) and 2 (strict: committed memory
   within the commit limit), with `vm.overcommit_ratio` (percent of user memory, default 100)
   and `vm.overcommit_kbytes` (the limit in KiB instead; writing either zeroes the other, as
   Linux's handlers do). Committed_AS is counted in every mode and CommitLimit shown in every
   mode, so `/proc/meminfo` stays consistent; `MAP_NORESERVE` pages are committed one by one
   at their first touch in every mode (Linux leaves them out of Committed_AS outside the
   strict mode: here the count stays the same whichever mode the page was touched in, and
   switching to strict finds them counted). What is scaled by memory (the dirty ratios,
   tmpfs's size, the pinned and heap-reserve shares) is taken of user memory, not of the
   commit limit, so a ratio does not shrink them; the strict mode's own promises (dirty and
   pinned cache pages counted as taken, stores waiting while they crowd out committed memory)
   apply in the strict mode only.

   **Strict stays the default.** The page cache's design rests on the commit guarantee (every
   committed page has a frame, so a fault never waits for anything but write-back), the
   servers' heaps hold commitment as their reserve, and an ENOMEM at `mmap` is an error a
   program can handle where an OOM kill is not. The heuristic mode is there for the workloads
   that promise much and touch little (Node.js with many workers): a tree with the host grant
   switches it with `echo 0 > /proc/sys/vm/overcommit_memory`. The default ratio is 100, not
   Linux's 50: without swap a ratio of 50 would leave half of memory unpromisable.

5. **Linux's interfaces are the server's, the mechanism the kernel's.** The kernel keeps
   oom_score_adj and its least per process (inherited by `proc_create`, kept by exec) and
   offers `SYS_OOM_SCORE_ADJ` (read or set, for the process and every process sharing its
   address space; below the least only with the host grant, which then also lowers the least,
   but not for the legacy oom_adj) and `SYS_OVERCOMMIT` (read, or set with the host grant: the
   policy is the machine's, as the wall clock). `proc_info` reports the kernel's badness and
   the resident pages by kind. The Linux server serves `/proc/<pid>/oom_score` (Linux's
   scale: 0 for a process never chosen, else 1000 plus the badness in thousandths of memory,
   times 2/3), `oom_score_adj`, `oom_adj` (the legacy scale, ×17/1000) for processes and
   threads, and `/proc/sys/vm/overcommit_{memory,ratio,kbytes}` merged into procfs's
   `/proc/sys`; the vm files open for writing only with the host grant (EACCES otherwise,
   Linux's answer for a sysctl another namespace's root does not own). The kernel's log gets
   Linux's messages: `<comm> invoked oom-killer`, the table of candidates, `Out of memory:
   Killed process N (name) total-vm:...kB, anon-rss:...kB, file-rss:...kB, shmem-rss:...kB,
   UID:0 pgtables:...kB oom_score_adj:N` and the reaper's line, with the kernel's pids (the
   log is the machine's).

## Consequences

- An out-of-memory situation ends the process that holds the memory, not the one that
  touched last; the heuristic mode is safe to use, and Node.js gets the headroom its thread
  stacks need when its tree turns it on.
- Cost: two software bits per page table entry and three counters per address space (kept on
  every map, unmap, copy-on-write break and fork: a few atomic adds), a page-table count in the
  allocator wrapper, a kernel thread (the reaper), and two kernel calls. Choosing a victim
  walks the process table once (and once more for the table of candidates and the address
  space's other users); it runs only when memory has run out, under a lock of its own.
- The emergency frames come from the kernel's reserve: in a mode that overcommits, the
  reserve is what stands between an exhausted machine and a failed kernel allocation until
  the killer made room (the reserve's lower half stays the kernel heap's).
- In the strict mode nothing changes for programs: commits are refused as before, and the
  killer acts only when memory nobody committed (page tables, the kernel's own) runs out,
  where the toucher used to die.
- Not done: per-tree memory limits (decision 1), `oom_kill_allocating_task`, `panic_on_oom`
  and the other vm sysctls, the OOM notifier interfaces (memory pressure files, PSI).

Tested by `userspace/oomkilltest.c` (in the self-tests): the sysctls and CommitLimit, the
grant checks, oom_score's order and oom_score_adj/oom_adj, the modes' answers to a commit, and
in the heuristic mode a toucher beyond memory: the process with oom_score_adj 1000 is killed
first, then the toucher, the biggest, both with SIGKILL and named in the kernel's log, while
the test itself (oom_score_adj -1000), a middle-sized process and a second tree live on.
`oomtest`, `nomemtest` and `reclaimtest` check that the strict mode answers as before.
