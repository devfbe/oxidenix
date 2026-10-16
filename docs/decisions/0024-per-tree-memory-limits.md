# ADR 0024: Per-tree memory limits: every tree a memory group, charged by frame

Date: 2026-10-16. Status: accepted.

## Context

Memory was one pool (ADR 0013): commits are promised against one limit, and when memory runs
out the OOM killer chooses across all trees. Nothing stopped one process tree from taking
all of it; a tree started for a workload could not be given a share of the machine and be
held to it, as a container is on Linux with its memory cgroup. ADR 0013 left that for when
trees get limits: it needs every frame charged to its tree and an OOM within the tree when
the tree exceeds its limit, with the machine's killer as the fallback.

Six questions shaped the design:

1. What counts for a tree, and how it is counted so that a frame shared across trees (the
   file cache of /data, ADR 0015) is counted once and given back wherever it is freed.
2. What a limit does: which allocations wait, reclaim, kill, or fail.
3. Whom the killer may choose when a tree is at its limit.
4. Who sets a tree's limits, and when.
5. What programs inside see: Linux's interfaces, and whether `/proc/meminfo` changes.
6. What happens to what is still charged to a tree once it is gone.

## Decision

1. **Every frame is charged to the group of the task that allocates it, and uncharged where
   its last reference goes** (`kernel/src/memory/memcg.rs`, Linux's memcg with
   `page->memcg_data`). Each tree is one memory group, held by its Linux server instance
   and named by its processes (`ThreadGroup::memcg`). The frame allocator keeps a group id
   (two bytes) per frame beside the reference counts: `UserFrames` and `user_frame` charge
   the running task's group (`memcg::Target::current`) when they allocate, and
   `deallocate_frame` uncharges the frame's group when the last reference goes, whoever
   drops it (another tree, the reaper, a native server ending a grant). Usage changes only
   under the frames' lock, so a charge checks the limit exactly. What is charged: the
   programs' pages (anonymous, private copies, tmpfs and shared memory objects), their page
   tables, the file cache pages the tree brought in first (as on Linux: a page another tree
   reads afterwards stays the first tree's), the instance's region (the server's data,
   threads' areas, its heap, its channels' rings and buffers, the vDSO object) and the
   kernel stacks of the tree's threads (Linux's `kernel_stack`). A tree's start charges the
   new tree, not its starter (`memcg::Charging`, an override of the running task's group
   while the instance, the first address space and the pager are made). Not charged: the
   kernel's own memory and the native servers' (the root group, never limited), and kernel
   heap objects (Linux charges slab objects through its object groups; the kernel heap here
   never gives frames back, and what a tree can make of it is bounded by its task table
   share and its server's handles).

2. **memory.max is enforced at the charge; high throttles; low and min protect.** A charge
   that would exceed `max` is refused: the allocation reclaims the group's own clean cache
   pages (`memory::reclaim_group`, a scope of the cache's reclaim that takes only pages
   charged to the group, never another tree's) and asks the tree's server to shrink. An
   allocation that may wait (a page fault, the kernel's or the server's copy into program
   memory, a fill the pager waits for) goes on in `memory::reclaim_retry`: it retries
   while reclaim makes progress (waiting for write-back while the group's own pages are
   dirty or pinned), and when reclaim stops making progress it invokes the OOM killer for
   the group. One that may not wait fails as when memory runs out (ENOMEM; a refused
   commit does the same). What must not fail goes over the limit, as Linux's force charge:
   an OOM victim's or a killed thread's faults (they must get to their end) and the
   server's heap pages its reserve covers, with their page tables (a server cannot fail an
   allocation but by breaking its instance). `high` is Linux's throttle: a thread that
   charged above it reclaims the group's cache pages on its way back to its program (its
   fault resolved, `restricted_enter`) and, while the usage stays above, sleeps for Linux's
   penalty (the square of the excess over `high`, scaled by the pages it charged, at most
   2 s, none below 10 ms: `mem_cgroup_handle_over_high`). `min` and `low` protect a
   group's cache pages from the machine's reclaim while its usage is within them: `min`
   always, `low` unless a pass over the unprotected pages found nothing (Linux's best
   effort; each such breach counts as the group's `low` event). Lowering `max` below the
   usage reclaims and then kills within the group until it fits, lowering `high` reclaims
   (Linux's `memory_max_write`, `memory_high_write`); both stop once the writer has a
   signal to take.

3. **Within a tree, the killer chooses within the tree** (`oom::out_of_memory_memcg`,
   Linux's `mem_cgroup_out_of_memory`): the same badness as ADR 0013's, among the processes
   of that group only, scaled by the group's limit; its first process is a candidate unless
   its tree holds the host grant (a container's init is no global init); a victim of the
   group whose memory is still on its way holds the choice off (one of another tree does
   not); the reaper frees the victim's private memory as for a machine-wide kill. Never a
   process of another tree. The machine's killer stays as ADR 0013 has it, the fallback
   when memory itself runs out; every kill, the machine's too, counts in the victim's
   group's `oom_kill`. The log says what Linux's does: `memory: usage ...kB, limit ...kB,
   failcnt N`, the group's table of tasks, `oom-kill:constraint=CONSTRAINT_MEMCG,...,
   oom_memcg=/treeN,task_memcg=/treeN` and `Memory cgroup out of memory: Killed process ...`.

4. **A tree's limits are its starter's, set when the tree is made**, as a delegated
   cgroup's interface files are its parent's on Linux: the monitor's `run -m <size>`
   (memory.max), and in test mode `TEST_TREE_START_MEMCG` (max, high, low and min, in
   bytes); a tree started otherwise has none (Linux's defaults: max and high "max", low and
   min 0). From within, only a tree holding the host grant may change its own (the machine's
   administrator, as for the overcommit policy: `SYS_MEMCG_SET`, EPERM otherwise; the server
   answers EACCES at open as Linux's permission check does).

5. **Programs see Linux's cgroup v2, as in a container with its own cgroup namespace**: the
   server mounts cgroup2 on `/sys/fs/cgroup` (`/sys/fs` merged into procfs's `/sys`; in the
   mount table, `CGROUP2_SUPER_MAGIC` from `statfs`), whose root is the tree's group:
   `cgroup.controllers` ("memory"), `cgroup.procs` (the tree's processes; writing one of
   them is the no-op move into the group it is in), `memory.current`, `memory.peak`,
   `memory.max`, `memory.high`, `memory.low`, `memory.min` (sizes as Linux's `memparse`
   reads them, "max" for none) and `memory.events` and `memory.events.local` (`low`,
   `high`, `max`, `oom`, `oom_kill`, `oom_group_kill`), over the kernel's
   `SYS_MEMCG_STAT`; every process's `/proc/<pid>/cgroup` is `0::/`. libuv (and so
   Node.js) reads exactly these to size itself to a container. `/proc/meminfo` and
   `sysinfo` stay the machine's, as on Linux: a cgroup does not change them.

6. **A gone tree's charges go to the root.** A group lives while its instance or a process
   of its tree holds it (an `Arc`); its last reference marks its slot dying (a bit, from any
   context: a task's end in the scheduler included), and the background reclaimer (or the
   next tree's start, if no slot is free) moves the frames still charged to it (cache pages
   other trees use, grants a native server still holds) to the root, a range of frames per
   hold of the frames' lock, before the slot is used again (Linux's reparenting). A dying
   group's protection ends at once. Slots: 64 (the task table's reserves allow fewer trees).

## Consequences

- A tree can be given a share of the machine and is held to it: at its limit it reclaims
  its own cache, then loses its own biggest process, while the other trees and the machine
  go on untouched (memcgtest). Node.js in such a tree sees its limit where it looks for it.
- Costs: two bytes per frame (0.05 % of memory; 128 KiB for 256 MiB) for the group ids; on
  every user frame allocation the running task's group is read and its limit compared, and
  an atomic add (and a peak update) made under the frames' lock that is held anyway; every
  free reads the frame's group (an uncharge if it has one); a page table walk is unchanged.
  The cache's reclaim reads each page's group when it looks at it. A tree's end costs one
  scan of the id array by the background reclaimer. A thread above `high` is throttled
  where it returns to its program (a flag test on that path otherwise). Two kernel calls
  and a test call.
- Within a tree, a page fault at the limit waits about 1.6 s of fruitless reclaim before the
  killer acts (as a machine-wide OOM, `RECLAIM_RETRIES`; longer while the tree's own dirty
  pages are being written back): what is reclaimable is given time to be reclaimed first.
- In the strict overcommit mode a fault can now end in an OOM kill even though its page was
  committed: the commit is the machine's promise, the limit the tree's own, as on Linux with
  `overcommit_memory=2` and memory cgroups.
- An allocation of a system call (a pipe's buffer, a page cache fill for read(2), a tmpfs
  write, the server's heap beyond its reserve) that finds the tree at its limit fails with
  ENOMEM after reclaim within the tree, where Linux would invoke the group's killer: those
  paths cannot wait in the kernel or the server (the server may be what write-back waits
  for). Page faults and copies into program memory kill.
- Not done: sub-groups (`mkdir` in `/sys/fs/cgroup`: EPERM) and the other controllers,
  `memory.stat`, `memory.swap.*` (there is no swap), `memory.oom.group`, `memory.reclaim`,
  the pressure files and PSI, per-group dirty limits (a tree's writer is throttled by the
  machine's), charging kernel heap objects.

Tested by `userspace/memcgtest.c` (in the self-tests): the self-tests' tree sees `0::/` and
the cgroup2 mount with its files, `memory.current` grows by what a process touches and comes
back when it unmaps it, the boundaries take sizes and "max" with the host grant; a second tree
started with memory.max 32 MiB (and high 31 MiB, low 1 MiB) cannot write its own boundaries
(EACCES), and a child there that touches 64 MiB is killed with SIGKILL by the group's killer
(`max`, `high`, `oom` and one `oom_kill` in its memory.events; memory.peak at the limit;
memory.current back within 1 MiB of where it was; the tree allocates again), while a process
of the first tree keeps 8 MiB intact, the first tree's OOM counts stay, and the kernel's log
names the kill `Memory cgroup out of memory` with `CONSTRAINT_MEMCG`.
