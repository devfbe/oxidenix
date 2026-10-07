/* Clocks: nanosecond resolution, monotonic across CPUs, the CPU-time
 * clocks of threads and processes, wall-clock time and its setting, and
 * the accounting behind getrusage and times. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/times.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-52s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static int64_t ns(clockid_t clock) {
    struct timespec ts;
    if (clock_gettime(clock, &ts) != 0) return -1;
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

/* Burns CPU time for `ms` milliseconds of the monotonic clock. */
static void spin_ms(int ms) {
    int64_t end = ns(CLOCK_MONOTONIC) + (int64_t)ms * 1000000;
    while (ns(CLOCK_MONOTONIC) < end) {
    }
}

static int64_t tv_us(struct timeval tv) { return (int64_t)tv.tv_sec * 1000000 + tv.tv_usec; }

static void pin(int cpu) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    sched_setaffinity(0, sizeof set, &set);
}

/* Cross-CPU monotonicity: one thread publishes its clock readings, the
 * other (on another CPU) must never read an earlier time afterwards. */
static volatile int64_t published;
static volatile int stop_ping, backwards;

static void *publisher(void *arg) {
    (void)arg;
    pin(1);
    while (!stop_ping) __atomic_store_n(&published, ns(CLOCK_MONOTONIC), __ATOMIC_SEQ_CST);
    return NULL;
}

static void *thread_cpu(void *arg) {
    int64_t *out = arg;
    spin_ms(100);
    out[0] = ns(CLOCK_THREAD_CPUTIME_ID);
    clockid_t mine;
    out[1] = pthread_getcpuclockid(pthread_self(), &mine) == 0 ? ns(mine) : -1;
    return NULL;
}

int main(void) {
    /* Resolution: readings are not quantized to a timer tick. */
    int fine = 0;
    int64_t prev = ns(CLOCK_MONOTONIC), steps_ok = 1;
    for (int i = 0; i < 1000; i++) {
        int64_t t = ns(CLOCK_MONOTONIC);
        if (t < prev) steps_ok = 0;
        if (t % 1000000 != 0) fine = 1;
        prev = t;
    }
    check("CLOCK_MONOTONIC has sub-millisecond resolution", fine);
    check("CLOCK_MONOTONIC never goes back on one CPU", steps_ok);
    struct timespec res = {99, 99};
    check("clock_getres(CLOCK_MONOTONIC) is at most 1 us", clock_getres(CLOCK_MONOTONIC, &res) == 0 && res.tv_sec == 0 && res.tv_nsec <= 1000);
    check("clock_getres on a bad clock fails with EINVAL", clock_getres(100, &res) == -1 && errno == EINVAL);
    check("clock_gettime on a bad clock fails with EINVAL", syscall(SYS_clock_gettime, 100, &res) == -1 && errno == EINVAL);

    int64_t m0 = ns(CLOCK_MONOTONIC);
    usleep(20000);
    int64_t elapsed = ns(CLOCK_MONOTONIC) - m0;
    check("CLOCK_MONOTONIC advances with real time", elapsed >= 19000000 && elapsed < 1000000000);
    int64_t b = ns(CLOCK_BOOTTIME), m = ns(CLOCK_MONOTONIC);
    check("CLOCK_BOOTTIME matches CLOCK_MONOTONIC", b > 0 && m - b >= 0 && m - b < 10000000);
    check("CLOCK_MONOTONIC_RAW and _COARSE work", ns(CLOCK_MONOTONIC_RAW) > 0 && ns(CLOCK_MONOTONIC_COARSE) > 0);

    /* Wall-clock time: three interfaces agree. */
    struct timeval tv;
    int64_t rt = ns(CLOCK_REALTIME);
    check("gettimeofday works", gettimeofday(&tv, NULL) == 0);
    int64_t gt = tv_us(tv) * 1000;
    time_t t = syscall(SYS_time, NULL);
    check("gettimeofday agrees with CLOCK_REALTIME", gt - rt >= -1000000 && gt - rt < 100000000);
    check("time() agrees with CLOCK_REALTIME", t >= rt / 1000000000 && t <= rt / 1000000000 + 1);
    check("the wall clock is after 2020", rt / 1000000000 > 1577836800);

    /* Setting the wall clock moves CLOCK_REALTIME, not CLOCK_MONOTONIC. */
    struct timespec now, later;
    clock_gettime(CLOCK_REALTIME, &now);
    int64_t mono_before = ns(CLOCK_MONOTONIC);
    later = now;
    later.tv_sec += 1000;
    check("clock_settime(CLOCK_REALTIME) works", clock_settime(CLOCK_REALTIME, &later) == 0);
    int64_t moved = ns(CLOCK_REALTIME) - ((int64_t)now.tv_sec * 1000000000 + now.tv_nsec);
    check("the wall clock moved by 1000 s", moved >= 1000000000000LL && moved < 1001000000000LL);
    check("CLOCK_MONOTONIC did not move", ns(CLOCK_MONOTONIC) - mono_before < 1000000000);
    later.tv_sec -= 1000;
    clock_settime(CLOCK_REALTIME, &later);
    check("clock_settime(CLOCK_MONOTONIC) fails with EINVAL", clock_settime(CLOCK_MONOTONIC, &now) == -1 && errno == EINVAL);
    struct timespec bad = {0, 1000000000};
    check("clock_settime with tv_nsec >= 1e9 fails", clock_settime(CLOCK_REALTIME, &bad) == -1 && errno == EINVAL);

    /* Monotonic across CPUs. */
    if (sysconf(_SC_NPROCESSORS_ONLN) > 1) {
        pthread_t p;
        pthread_create(&p, NULL, publisher, NULL);
        pin(0);
        int64_t end = ns(CLOCK_MONOTONIC) + 200000000;
        long compared = 0;
        while (ns(CLOCK_MONOTONIC) < end) {
            int64_t seen = __atomic_load_n(&published, __ATOMIC_SEQ_CST);
            int64_t mine = ns(CLOCK_MONOTONIC);
            if (seen && mine < seen) backwards = 1;
            if (seen) compared++;
        }
        stop_ping = 1;
        pthread_join(p, NULL);
        check("CLOCK_MONOTONIC never goes back between CPUs", !backwards && compared > 1000);
        cpu_set_t all;
        CPU_ZERO(&all);
        for (int i = 0; i < 16; i++) CPU_SET(i, &all);
        sched_setaffinity(0, sizeof all, &all);
    }

    /* CPU-time clocks: busy time counts, sleeping does not. */
    int64_t c0 = ns(CLOCK_THREAD_CPUTIME_ID);
    spin_ms(100);
    int64_t c1 = ns(CLOCK_THREAD_CPUTIME_ID);
    usleep(100000);
    int64_t c2 = ns(CLOCK_THREAD_CPUTIME_ID);
    check("thread CPU time counts 100 ms of spinning", c1 - c0 >= 80000000 && c1 - c0 < 500000000);
    check("thread CPU time is not tick-quantized", c1 % 1000000 != 0 || c0 % 1000000 != 0);
    check("thread CPU time does not count sleeping", c2 - c1 < 20000000);
    int64_t threads[2];
    pthread_t th;
    pthread_create(&th, NULL, thread_cpu, threads);
    pthread_join(th, NULL);
    check("another thread's CPU time is its own", threads[0] >= 80000000 && threads[0] < 500000000);
    check("pthread_getcpuclockid reads a thread's clock", threads[1] >= threads[0]);
    int64_t pc = ns(CLOCK_PROCESS_CPUTIME_ID);
    check("process CPU time includes all threads", pc >= c1 + threads[0] - 10000000);
    clockid_t pclock;
    check("clock_getcpuclockid(0) reads the process clock", clock_getcpuclockid(0, &pclock) == 0 && ns(pclock) >= pc);

    /* getrusage and times, for the process and its reaped children. */
    struct rusage ru;
    check("getrusage(RUSAGE_SELF) works", getrusage(RUSAGE_SELF, &ru) == 0);
    int64_t self_us = tv_us(ru.ru_utime) + tv_us(ru.ru_stime);
    check("getrusage(RUSAGE_SELF) counts the spinning", self_us >= 180000 && self_us * 1000 <= ns(CLOCK_PROCESS_CPUTIME_ID) + 1000000);
    check("getrusage(RUSAGE_THREAD) counts this thread", getrusage(RUSAGE_THREAD, &ru) == 0 && tv_us(ru.ru_utime) + tv_us(ru.ru_stime) >= 80000);
    check("getrusage with a bad target fails", getrusage(5, &ru) == -1 && errno == EINVAL);
    struct tms before, after;
    clock_t e0 = times(&before);
    pid_t child = fork();
    if (child == 0) {
        spin_ms(150);
        _exit(0);
    }
    waitpid(child, NULL, 0);
    clock_t e1 = times(&after);
    long hz = sysconf(_SC_CLK_TCK);
    check("times() returns elapsed clock ticks", e1 - e0 >= hz / 10 && e1 - e0 < 10 * hz);
    check("times() counts the reaped child", after.tms_cutime + after.tms_cstime >= hz / 10);
    check("getrusage(RUSAGE_CHILDREN) counts it", getrusage(RUSAGE_CHILDREN, &ru) == 0 && tv_us(ru.ru_utime) + tv_us(ru.ru_stime) >= 120000);

    printf("timetest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
