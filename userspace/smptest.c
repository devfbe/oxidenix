/* SMP tests: CPU count and affinity, real parallel speed-up, fork/exit and
 * cross-CPU wakeups (pipes, signals) under load on every CPU. */
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
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

static volatile sig_atomic_t got_usr1;
static void on_usr1(int sig) {
    (void)sig;
    got_usr1++;
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
