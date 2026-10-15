/* membarrier(2) (ADR 0021; musl's dynamic loader registers for it at every
 * start and uses it when dlopen adds TLS beside running threads): QUERY says
 * what Linux without rseq says, the private expedited commands need their
 * registration (EPERM before), GET_REGISTRATIONS reports them, fork keeps
 * them, execve drops them (a vfork child's execve not its parent's), flags,
 * the rseq commands and unknown commands are EINVAL, each barrier holds in a
 * store-buffering race between a thread with a compiler barrier only and one
 * with membarrier, and it reaches the CPUs it should, as Linux's (the
 * kernel's count of the interrupts each CPU served, TEST_BARRIERS): the
 * private commands those that run the process, global expedited those that
 * run a registered process (fork inherits the registration), global none (it
 * waits for each CPU's tick). */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

enum {
    CMD_QUERY = 0,
    CMD_GLOBAL = 1,
    CMD_GLOBAL_EXPEDITED = 2,
    CMD_REGISTER_GLOBAL_EXPEDITED = 4,
    CMD_PRIVATE_EXPEDITED = 8,
    CMD_REGISTER_PRIVATE_EXPEDITED = 16,
    CMD_PRIVATE_EXPEDITED_SYNC_CORE = 32,
    CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE = 64,
    CMD_PRIVATE_EXPEDITED_RSEQ = 128,
    CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ = 256,
    CMD_GET_REGISTRATIONS = 512,
};

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

/* The call's result, or -errno. */
static long mb(int cmd, unsigned flags) {
    long r = syscall(SYS_membarrier, cmd, flags, 0);
    return r < 0 ? -errno : r;
}

/* The store-buffering race: the fast side stores x and loads y with only a
 * compiler barrier between, the slow side stores y, calls membarrier and
 * loads x. Without a barrier on the fast side's CPU both may read 0 (x86's
 * store buffer); membarrier forbids it. */
static volatile int x, y, r_fast;
static atomic_int round_go, round_done;
static int rounds;

static void *fast_side(void *arg) {
    (void)arg;
    for (int i = 1; i <= rounds; i++) {
        while (atomic_load_explicit(&round_go, memory_order_acquire) != i) {
        }
        x = 1;
        __asm__ volatile("" ::: "memory");
        r_fast = y;
        atomic_store_explicit(&round_done, i, memory_order_release);
    }
    return NULL;
}

/* `n` rounds of the race with membarrier(`cmd`) on the slow side: how many
 * had both sides read 0 (-1 if a call failed). */
static int store_buffering(int cmd, int n) {
    rounds = n;
    atomic_store(&round_go, 0);
    atomic_store(&round_done, 0);
    pthread_t t;
    pthread_create(&t, NULL, fast_side, NULL);
    int both_zero = 0, errors = 0;
    for (int i = 1; i <= n; i++) {
        x = 0;
        y = 0;
        atomic_store_explicit(&round_go, i, memory_order_release);
        y = 1;
        if (mb(cmd, 0) != 0) errors++;
        int r_slow = x;
        while (atomic_load_explicit(&round_done, memory_order_acquire) != i) {
        }
        if (r_slow == 0 && r_fast == 0) both_zero++;
    }
    pthread_join(t, NULL);
    printf("    %d rounds with command %d, %d with both sides reading 0\n", n, cmd, both_zero);
    return errors ? -1 : both_zero;
}

/* The barrier interrupts CPU `cpu` served (oxidenix's TEST_BARRIERS; -1
 * elsewhere). */
static long barriers(int cpu) {
    long r = syscall(1640, cpu);
    return r < 0 ? -1 : r;
}

static int pin(int cpu) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    return sched_setaffinity(0, sizeof set, &set);
}

static atomic_int spin_stop;

static void *spinner(void *arg) {
    pin((int)(intptr_t)arg);
    while (!atomic_load_explicit(&spin_stop, memory_order_relaxed)) {
    }
    return NULL;
}

/* Which CPUs a barrier reaches (each CPU's count of barrier interrupts):
 * the private commands those that run the process (a thread of ours spinning
 * on CPU 1), not those that run another (an unregistered program spinning on
 * CPU 2, a registered child on CPU 3); global expedited those that run a
 * registered process (ours and the child, which inherited the registration
 * by fork), not the unregistered one; global none at all (it waits for each
 * CPU's tick instead) and returns within a few ticks. */
static void reach(void) {
    if (barriers(0) < 0 || sysconf(_SC_NPROCESSORS_ONLN) < 4) {
        printf("    (no TEST_BARRIERS or fewer than 4 CPUs: which CPUs a barrier reaches is not checked)\n");
        return;
    }
    pin(0);
    pthread_t t;
    pthread_create(&t, NULL, spinner, (void *)(intptr_t)1);
    pid_t unregistered = fork();
    if (unregistered == 0) {
        execl("/proc/self/exe", "membarriertest", "spin", "2", (char *)NULL);
        _exit(99);
    }
    pid_t registered = fork();
    if (registered == 0) {
        pin(3);
        for (;;) {
        }
    }
    usleep(300 * 1000);
    long c0[4], c1[4], c2[4], c3[4];
    for (int i = 1; i < 4; i++) c0[i] = barriers(i);
    int ok = 1;
    for (int i = 0; i < 10; i++) ok &= mb(CMD_PRIVATE_EXPEDITED, 0) == 0;
    for (int i = 1; i < 4; i++) c1[i] = barriers(i);
    for (int i = 0; i < 10; i++) ok &= mb(CMD_GLOBAL_EXPEDITED, 0) == 0;
    for (int i = 1; i < 4; i++) c2[i] = barriers(i);
    struct timespec a, b;
    clock_gettime(CLOCK_MONOTONIC, &a);
    for (int i = 0; i < 10; i++) ok &= mb(CMD_GLOBAL, 0) == 0;
    clock_gettime(CLOCK_MONOTONIC, &b);
    for (int i = 1; i < 4; i++) c3[i] = barriers(i);
    long ms = (b.tv_sec - a.tv_sec) * 1000 + (b.tv_nsec - a.tv_nsec) / 1000000;
    kill(unregistered, SIGKILL);
    kill(registered, SIGKILL);
    waitpid(unregistered, NULL, 0);
    waitpid(registered, NULL, 0);
    atomic_store(&spin_stop, 1);
    pthread_join(t, NULL);
    for (int i = 1; i < 4; i++)
        printf("    CPU %d: private %ld, global expedited %ld, global %ld\n", i, c1[i] - c0[i], c2[i] - c1[i], c3[i] - c2[i]);
    printf("    10 global barriers: %ld ms\n", ms);
    check("the calls succeed", ok);
    check("PRIVATE_EXPEDITED interrupts the CPU that runs a thread of ours", c1[1] - c0[1] >= 10);
    check("... and not those that run other processes", c1[2] == c0[2] && c1[3] == c0[3]);
    check("GLOBAL_EXPEDITED interrupts the CPUs that run registered processes", c2[1] - c1[1] >= 10 && c2[3] - c1[3] >= 10);
    check("... and not the one that runs an unregistered process", c2[2] == c1[2]);
    check("GLOBAL interrupts no CPU", c3[1] == c2[1] && c3[2] == c2[2] && c3[3] == c2[3]);
    check("... and returns within a few ticks each (10 in under a second)", ms < 1000);
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "registrations") == 0) return (int)mb(CMD_GET_REGISTRATIONS, 0);
    if (argc > 2 && strcmp(argv[1], "spin") == 0) {
        pin(atoi(argv[2]));
        for (;;) {
        }
    }

    /* (Linux built with rseq offers its commands too: there the checks expect them.) */
    int rseq = syscall(SYS_rseq, NULL, 0, 0, 0) == -1 && errno != ENOSYS;
    long query = mb(CMD_QUERY, 0);
    check("QUERY: every command (but the rseq ones without rseq)", query == (rseq ? 0x3ff : 0x27f));
    check("PRIVATE_EXPEDITED before registering: EPERM", mb(CMD_PRIVATE_EXPEDITED, 0) == -EPERM);
    check("PRIVATE_EXPEDITED_SYNC_CORE before registering: EPERM", mb(CMD_PRIVATE_EXPEDITED_SYNC_CORE, 0) == -EPERM);
    check("GET_REGISTRATIONS: none yet", mb(CMD_GET_REGISTRATIONS, 0) == 0);
    check("GLOBAL and GLOBAL_EXPEDITED need no registration", mb(CMD_GLOBAL, 0) == 0 && mb(CMD_GLOBAL_EXPEDITED, 0) == 0);
    check("REGISTER_PRIVATE_EXPEDITED", mb(CMD_REGISTER_PRIVATE_EXPEDITED, 0) == 0);
    check("PRIVATE_EXPEDITED once registered", mb(CMD_PRIVATE_EXPEDITED, 0) == 0);
    check("... SYNC_CORE still needs its own registration", mb(CMD_PRIVATE_EXPEDITED_SYNC_CORE, 0) == -EPERM);
    check("REGISTER_PRIVATE_EXPEDITED_SYNC_CORE, then SYNC_CORE",
          mb(CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE, 0) == 0 && mb(CMD_PRIVATE_EXPEDITED_SYNC_CORE, 0) == 0);
    check("REGISTER_GLOBAL_EXPEDITED", mb(CMD_REGISTER_GLOBAL_EXPEDITED, 0) == 0);
    check("GET_REGISTRATIONS reports the three", mb(CMD_GET_REGISTRATIONS, 0) == 0x54);
    check("a flag: EINVAL", mb(CMD_PRIVATE_EXPEDITED, 1) == -EINVAL && mb(CMD_QUERY, 1) == -EINVAL);
    if (!rseq)
        check("the rseq commands: EINVAL (no rseq)",
              mb(CMD_PRIVATE_EXPEDITED_RSEQ, 0) == -EINVAL && mb(CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ, 0) == -EINVAL);
    check("an unknown command: EINVAL", mb(3, 0) == -EINVAL && mb(1024, 0) == -EINVAL && mb(-1, 0) == -EINVAL);

    fflush(stdout);
    pid_t child = fork();
    if (child == 0) _exit(mb(CMD_GET_REGISTRATIONS, 0) == 0x54 && mb(CMD_PRIVATE_EXPEDITED, 0) == 0 ? 0 : 1);
    int status;
    waitpid(child, &status, 0);
    check("fork keeps the registrations", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    child = fork();
    if (child == 0) {
        execl("/proc/self/exe", "membarriertest", "registrations", (char *)NULL);
        _exit(99);
    }
    waitpid(child, &status, 0);
    check("execve drops them", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    child = vfork();
    if (child == 0) {
        execl("/proc/self/exe", "membarriertest", "registrations", (char *)NULL);
        _exit(99);
    }
    waitpid(child, &status, 0);
    check("a vfork child's execve drops its own, not its parent's",
          WIFEXITED(status) && WEXITSTATUS(status) == 0 && mb(CMD_GET_REGISTRATIONS, 0) == 0x54);

    /* The barriers themselves, with the fast side on another CPU where there
     * is one. */
    check("store buffering never shows across PRIVATE_EXPEDITED", store_buffering(CMD_PRIVATE_EXPEDITED, 20000) == 0);
    check("... nor across GLOBAL_EXPEDITED", store_buffering(CMD_GLOBAL_EXPEDITED, 20000) == 0);
    check("... nor across GLOBAL", store_buffering(CMD_GLOBAL, 200) == 0);
    reach();

    if (failures) {
        printf("membarriertest: %d failed\n", failures);
        return 1;
    }
    printf("membarriertest: all passed\n");
    return 0;
}
