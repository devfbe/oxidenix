/* SMP tests: CPU count, affinity and scheduling policy, nice values and
 * the CPU shares they give, real parallel speed-up, fork/exit and
 * cross-CPU wakeups (pipes, signals) under load on every CPU. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-52s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static double now(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static void pin(int cpu) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    sched_setaffinity(0, sizeof set, &set);
}

/* CPU-bound work without syscalls. */
static volatile unsigned long sink;
static void work(long rounds) {
    unsigned long x = 1;
    for (long i = 0; i < rounds; i++) x = x * 6364136223846793005UL + 1442695040888963407UL;
    sink = x;
}

static int wait_all(int n) {
    int ok = 1, status;
    for (int i = 0; i < n; i++) {
        while (wait(&status) < 0 && errno == EINTR) {
        }
        ok &= WIFEXITED(status) && WEXITSTATUS(status) == 0;
    }
    return ok;
}

/* Wall time of `n` processes doing `rounds` of work each, in parallel. */
static double parallel(int n, long rounds) {
    double t0 = now();
    for (int i = 0; i < n; i++)
        if (fork() == 0) {
            work(rounds);
            _exit(0);
        }
    wait_all(n);
    return now() - t0;
}

/* A thread that waits until a byte arrives on park_pipe. */
static int park_pipe[2];
static void *park(void *arg) {
    char c;
    read(park_pipe[0], &c, 1);
    return arg;
}

static volatile sig_atomic_t got_usr1;
static void on_usr1(int sig) {
    (void)sig;
    got_usr1++;
}

/* CPU time of process `pid` in clock ticks (utime + stime), and its nice
 * value, from /proc/<pid>/stat. */
static long proc_ticks(pid_t pid, long *nice) {
    char path[64], buf[512];
    snprintf(path, sizeof path, "/proc/%d/stat", pid);
    FILE *f = fopen(path, "r");
    if (!f) return -1;
    size_t n = fread(buf, 1, sizeof buf - 1, f);
    fclose(f);
    buf[n] = 0;
    char *p = strrchr(buf, ')');
    unsigned long ut = 0, st = 0;
    long prio = 0, ni = 0;
    if (!p || sscanf(p + 2, "%*c %*d %*d %*d %*d %*d %*u %*u %*u %*u %*u %lu %lu %*d %*d %ld %ld", &ut, &st, &prio, &ni) != 4) return -1;
    if (nice) *nice = ni;
    return (long)(ut + st);
}

/* Two CPU-bound processes on CPU 0 for 1.5 s, with nice values `a` and
 * `b`: their CPU times. */
static void share(int a, int b, long *ta, long *tb) {
    pid_t kids[2];
    int nices[2] = {a, b};
    for (int i = 0; i < 2; i++) {
        kids[i] = fork();
        if (kids[i] == 0) {
            pin(0);
            setpriority(PRIO_PROCESS, 0, nices[i]);
            for (;;) work(1000000);
        }
    }
    usleep(300 * 1000);
    long start[2] = {proc_ticks(kids[0], NULL), proc_ticks(kids[1], NULL)};
    usleep(1500 * 1000);
    *ta = proc_ticks(kids[0], NULL) - start[0];
    *tb = proc_ticks(kids[1], NULL) - start[1];
    for (int i = 0; i < 2; i++) kill(kids[i], SIGKILL);
    for (int i = 0; i < 2; i++) waitpid(kids[i], NULL, 0);
}

static void nice_values(void) {
    errno = 0;
    check("getpriority: nice 0 to begin with", getpriority(PRIO_PROCESS, 0) == 0 && errno == 0);
    long ni = 99;
    check("setpriority 5, getpriority and /proc see it", setpriority(PRIO_PROCESS, 0, 5) == 0 && getpriority(PRIO_PROCESS, 0) == 5 &&
                                                         proc_ticks(getpid(), &ni) >= 0 && ni == 5);
    check("values beyond 19 and -20 are clamped", setpriority(PRIO_PROCESS, 0, 100) == 0 && getpriority(PRIO_PROCESS, 0) == 19 &&
                                                      setpriority(PRIO_PROCESS, 0, -100) == 0 && getpriority(PRIO_PROCESS, 0) == -20);
    setpriority(PRIO_PROCESS, 0, 3);
    pid_t child = fork();
    if (child == 0) _exit(getpriority(PRIO_PROCESS, 0) == 3 ? 0 : 1);
    int st = 0;
    waitpid(child, &st, 0);
    check("a child inherits the nice value", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    errno = 0;
    check("PRIO_PGRP and PRIO_USER answer the most favored", getpriority(PRIO_PGRP, 0) <= 3 && getpriority(PRIO_USER, 0) <= 3 && errno == 0);
    setpriority(PRIO_PROCESS, 0, 0);
    errno = 0;
    check("a missing process is ESRCH", setpriority(PRIO_PROCESS, 999999, 1) == -1 && errno == ESRCH);
    errno = 0;
    check("an unknown `which` is EINVAL", getpriority(7, 0) == -1 && errno == EINVAL);
    errno = 0;
    check("another user has no processes (ESRCH)", getpriority(PRIO_USER, 1000) == -1 && errno == ESRCH);

    long a, b;
    share(0, 19, &a, &b);
    printf("    nice 0 vs 19 on one CPU: %ld vs %ld ticks\n", a, b);
    check("nice 19 gets a small share against nice 0", a > 10 * (b > 0 ? b : 1) / 2 && a + b > 100);
    share(-5, 0, &a, &b);
    printf("    nice -5 vs 0 on one CPU: %ld vs %ld ticks\n", a, b);
    check("nice -5 gets about three times nice 0's share", a > 2 * b && a < 4 * b + 10);
}

int main(void) {
    long n = sysconf(_SC_NPROCESSORS_ONLN);
    cpu_set_t set;
    CPU_ZERO(&set);
    int got = sched_getaffinity(0, sizeof set, &set);
    check("sched_getaffinity lists every online CPU", got == 0 && CPU_COUNT(&set) == n && n >= 1);
    printf("smptest: %ld CPUs\n", n);

    int pinned_ok = 1;
    for (int cpu = 0; cpu < n; cpu++) {
        pin(cpu);
        work(1000);
        pinned_ok &= sched_getcpu() == cpu;
    }
    check("sched_setaffinity moves the caller to each CPU", pinned_ok);
    CPU_ZERO(&set);
    check("an empty affinity mask is rejected", sched_setaffinity(0, sizeof set, &set) == -1 && errno == EINVAL);
    for (int cpu = 0; cpu < n; cpu++) CPU_SET(cpu, &set);
    sched_setaffinity(0, sizeof set, &set);

    /* One scheduling policy, SCHED_OTHER with priority 0 (V8 asks through
     * pthread_getschedparam at startup). */
    struct sched_param sp = {.sched_priority = 7};
    int policy = -1;
    check("pthread_getschedparam: SCHED_OTHER, priority 0",
          pthread_getschedparam(pthread_self(), &policy, &sp) == 0 && policy == SCHED_OTHER && sp.sched_priority == 0);
    pthread_t other;
    pipe(park_pipe);
    pthread_create(&other, NULL, park, NULL);
    sp.sched_priority = 7;
    policy = -1;
    check("... also for another thread (its own id)",
          pthread_getschedparam(other, &policy, &sp) == 0 && policy == SCHED_OTHER && sp.sched_priority == 0);
    write(park_pipe[1], "x", 1);
    pthread_join(other, NULL);
    errno = 0;
    check("sched_getscheduler of a missing thread is ESRCH", syscall(SYS_sched_getscheduler, 999999) == -1 && errno == ESRCH);
    errno = 0;
    check("sched_getparam of a negative id is EINVAL", syscall(SYS_sched_getparam, -1, &sp) == -1 && errno == EINVAL);
    nice_values();

    if (n >= 2) {
        /* Calibrate to about half a second of work for one process. */
        long rounds = 1000000;
        while (parallel(1, rounds) < 0.5) rounds *= 2;
        /* Best of three: a busy emulator host must not fail the test. */
        double best = 0, one = 0, all = 0;
        for (int attempt = 0; attempt < 3 && best < 1.0 + (n - 1) * 0.25; attempt++) {
            double o = parallel(1, rounds), a = parallel(n, rounds);
            if (n * o / a > best) {
                best = n * o / a;
                one = o;
                all = a;
            }
        }
        printf("smptest: 1 process %.2f s, %ld processes %.2f s (speed-up %.1fx)\n", one, n, all, best);
        /* Every extra CPU must add at least a quarter of one (4 CPUs: 1.75x),
         * which one CPU can never reach; emulator hosts shared with other
         * work (CI runners) do not get close to the ideal. */
        check("CPU-bound processes run in parallel", best >= 1.0 + (n - 1) * 0.25);
    }

    /* Every CPU forks and reaps at the same time. */
    for (int i = 0; i < n; i++)
        if (fork() == 0) {
            int ok = 1;
            for (int k = 0; k < 100; k++) {
                pid_t c = fork();
                if (c == 0) _exit(7);
                int st;
                while (waitpid(c, &st, 0) < 0 && errno == EINTR) {
                }
                ok &= WIFEXITED(st) && WEXITSTATUS(st) == 7;
            }
            _exit(ok ? 0 : 1);
        }
    check("parallel fork/exit/wait on every CPU", wait_all(n));

    /* Ping-pong through two pipes between two CPUs: cross-CPU wakeups. */
    int ab[2], ba[2];
    pipe(ab);
    pipe(ba);
    const int rounds = 5000;
    pid_t peer = fork();
    if (peer == 0) {
        pin(n >= 2 ? 1 : 0);
        char c;
        for (int i = 0; i < rounds; i++) {
            if (read(ab[0], &c, 1) != 1) _exit(1);
            c++;
            if (write(ba[1], &c, 1) != 1) _exit(1);
        }
        _exit(0);
    }
    pin(0);
    char c = 0;
    int pong_ok = 1;
    for (int i = 0; i < rounds; i++) {
        char sent = (char)i;
        write(ab[1], &sent, 1);
        if (read(ba[0], &c, 1) != 1 || c != (char)(sent + 1)) pong_ok = 0;
    }
    pong_ok &= wait_all(1);
    check("5000 pipe round trips between two CPUs", pong_ok);

    /* Signals to a process spinning on another CPU arrive. */
    signal(SIGUSR1, on_usr1);
    pid_t spinner = fork();
    if (spinner == 0) {
        pin(n >= 2 ? n - 1 : 0);
        while (got_usr1 < 50) work(1000);
        _exit(0);
    }
    pin(0);
    int signals_ok = 1;
    double deadline = now() + 20;
    for (;;) {
        kill(spinner, SIGUSR1);
        int st;
        pid_t r = waitpid(spinner, &st, WNOHANG);
        if (r == spinner) {
            signals_ok = WIFEXITED(st) && WEXITSTATUS(st) == 0;
            break;
        }
        if (now() > deadline) {
            kill(spinner, SIGKILL);
            waitpid(spinner, &st, 0);
            signals_ok = 0;
            break;
        }
        usleep(1000);
    }
    check("signals reach a process running on another CPU", signals_ok);

    /* A program flooding the console (palette changes force full redraws)
     * must not hold interrupts off so long that timers on its CPU are
     * late: 100 sleeps of 2 ms next to it take well under a second. */
    if (n >= 2) {
        pid_t flood = fork();
        if (flood == 0) {
            pin(0);
            for (int i = 0;; i++) printf("\033]P1%02x0000\033]P2%02xff00.", i & 0xff, i & 0xff), fflush(stdout);
        }
        pin(0);
        usleep(200000);
        double m0 = now();
        for (int i = 0; i < 100; i++) usleep(2000);
        double slept = now() - m0;
        kill(flood, SIGKILL);
        waitpid(flood, NULL, 0);
        printf("\033]R\nsmptest: 100 sleeps of 2 ms took %.2f s during a console flood\n", slept);
        check("timers stay on time during a console flood", slept >= 0.2 && slept < 0.9);
    }

    printf("smptest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
