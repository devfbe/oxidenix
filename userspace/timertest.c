/* High-resolution timers: sleeps and timeouts end when they are due, not
 * at the next 10 ms timer tick, and never early. */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <sys/time.h>
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
    clock_gettime(clock, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static int cmp(const void *a, const void *b) {
    int64_t x = *(const int64_t *)a, y = *(const int64_t *)b;
    return (x > y) - (x < y);
}

/* Median and minimum of 9 runs of `wait`, in microseconds: the median
 * shows the resolution, the minimum that nothing ends early. */
static void measure(void (*wait)(void), int64_t *median, int64_t *min) {
    int64_t t[9];
    for (int i = 0; i < 9; i++) {
        int64_t start = ns(CLOCK_MONOTONIC);
        wait();
        t[i] = (ns(CLOCK_MONOTONIC) - start) / 1000;
    }
    qsort(t, 9, sizeof t[0], cmp);
    *median = t[4];
    *min = t[0];
}

static void sleep_1ms(void) {
    struct timespec ts = {0, 1000000};
    nanosleep(&ts, NULL);
}

static void poll_2ms(void) { poll(NULL, 0, 2); }

static void select_1500us(void) {
    struct timeval tv = {0, 1500};
    select(0, NULL, NULL, NULL, &tv);
}

static uint32_t word;
static void futex_1ms(void) {
    struct timespec ts = {0, 1000000};
    syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, &ts, NULL, 0);
}

static void sigtimedwait_1ms(void) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR2);
    struct timespec ts = {0, 1000000};
    sigtimedwait(&set, NULL, &ts);
}

static volatile int alarms;
static void on_alarm_once(int sig) {
    (void)sig;
    struct itimerval stop = {{0, 0}, {0, 0}};
    setitimer(ITIMER_REAL, &stop, NULL);
    alarms++;
}
static void on_alarm(int sig) {
    (void)sig;
    alarms++;
}

static void on_usr1(int sig) { (void)sig; }

/* Counts loop rounds for `len` nanoseconds of monotonic time. */
static long spin(int64_t len) {
    int64_t end = ns(CLOCK_MONOTONIC) + len;
    long rounds = 0;
    while (ns(CLOCK_MONOTONIC) < end) rounds++;
    return rounds;
}

int main(void) {
    int64_t median, min;

    measure(sleep_1ms, &median, &min);
    check("nanosleep(1 ms) never ends early", min >= 1000);
    check("nanosleep(1 ms) takes well under a tick", median < 4000);
    measure(poll_2ms, &median, &min);
    check("poll timeout of 2 ms is precise", min >= 2000 && median < 5000);
    measure(select_1500us, &median, &min);
    check("select timeout of 1.5 ms is precise", min >= 1500 && median < 4500);
    measure(futex_1ms, &median, &min);
    check("futex timeout of 1 ms is precise", min >= 1000 && median < 4000);
    measure(sigtimedwait_1ms, &median, &min);
    check("sigtimedwait timeout of 1 ms is precise", min >= 1000 && median < 4000);

    /* usleep(100) a hundred times: 10 ms of sleeping, not a second. */
    int64_t start = ns(CLOCK_MONOTONIC);
    for (int i = 0; i < 100; i++) usleep(100);
    int64_t total = (ns(CLOCK_MONOTONIC) - start) / 1000;
    check("100 x usleep(100) take 10 ms, not 100 ticks", total >= 10000 && total < 60000);

    /* Absolute sleeps on the monotonic and the wall clock. */
    struct timespec at;
    clock_gettime(CLOCK_MONOTONIC, &at);
    at.tv_nsec += 3000000;
    if (at.tv_nsec >= 1000000000) {
        at.tv_sec++;
        at.tv_nsec -= 1000000000;
    }
    int64_t target = (int64_t)at.tv_sec * 1000000000 + at.tv_nsec;
    int r = clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &at, NULL);
    int64_t late = ns(CLOCK_MONOTONIC) - target;
    check("clock_nanosleep(TIMER_ABSTIME) wakes on time", r == 0 && late >= 0 && late < 3000000);
    clock_gettime(CLOCK_REALTIME, &at);
    at.tv_sec -= 1;
    check("an absolute sleep in the past returns at once", clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME, &at, NULL) == 0);
    struct timespec rel = {0, 2000000};
    start = ns(CLOCK_MONOTONIC);
    r = clock_nanosleep(CLOCK_BOOTTIME, 0, &rel, NULL);
    check("clock_nanosleep(CLOCK_BOOTTIME) sleeps", r == 0 && ns(CLOCK_MONOTONIC) - start >= 2000000);
    check("clock_nanosleep on a CPU clock is refused", clock_nanosleep(CLOCK_THREAD_CPUTIME_ID, 0, &rel, NULL) == EINVAL);
    struct timespec bad = {0, 1000000000};
    check("nanosleep with tv_nsec >= 1e9 fails with EINVAL", nanosleep(&bad, NULL) == -1 && errno == EINVAL);

    /* An interrupted relative sleep reports the time left. */
    struct sigaction sa = {0};
    sa.sa_handler = on_usr1; /* no SA_RESTART */
    sigaction(SIGUSR1, &sa, NULL);
    pid_t me = getpid();
    if (fork() == 0) {
        usleep(50000);
        kill(me, SIGUSR1);
        _exit(0);
    }
    struct timespec req = {2, 0}, rem = {0, 0};
    r = nanosleep(&req, &rem);
    check("a signal interrupts nanosleep with EINTR", r == -1 && errno == EINTR);
    int64_t left = (int64_t)rem.tv_sec * 1000000000 + rem.tv_nsec;
    check("nanosleep reports the time left", left > 1500000000 && left < 1960000000);

    /* A repeating interval timer at 2 ms: about 50 signals in 100 ms. */
    sa.sa_handler = on_alarm;
    sa.sa_flags = SA_RESTART;
    sigaction(SIGALRM, &sa, NULL);
    struct itimerval it = {{0, 2000}, {0, 2000}};
    setitimer(ITIMER_REAL, &it, NULL);
    start = ns(CLOCK_MONOTONIC);
    while (ns(CLOCK_MONOTONIC) - start < 100000000) pause();
    struct itimerval left_it, off = {{0, 0}, {0, 0}};
    setitimer(ITIMER_REAL, &off, &left_it);
    printf("  (%d SIGALRMs at a 2 ms interval in 100 ms)\n", alarms);
    check("a 2 ms interval timer fires about 50 times in 100 ms", alarms >= 35 && alarms <= 52);
    check("getitimer keeps microsecond values", left_it.it_interval.tv_usec == 2000 && left_it.it_interval.tv_sec == 0);

    /* A 1 us interval timer keeps its own process busy with handlers (as
     * on Linux), but must not drown the CPU in timer interrupts: it reloads
     * only once its signal is taken, so another process on the same CPU
     * still gets its share. */
    cpu_set_t cpu0;
    CPU_ZERO(&cpu0);
    CPU_SET(0, &cpu0);
    sched_setaffinity(0, sizeof cpu0, &cpu0);
    long base = spin(50000000);
    struct itimerval fast = {{0, 1}, {0, 1}};
    pid_t storm = fork();
    if (storm == 0) {
        setitimer(ITIMER_REAL, &fast, NULL);
        for (;;) {
        }
    }
    long busy = spin(100000000);
    kill(storm, SIGKILL);
    waitpid(storm, NULL, 0);
    printf("  (%ld loop rounds in 100 ms next to a 1 us interval timer, %ld in 50 ms alone)\n", busy, base);
    check("a 1 us interval timer leaves other processes their CPU time", busy > base / 5);

    /* Ignored, it fires into nothing; once handled again it runs on. */
    sa.sa_handler = SIG_IGN;
    sigaction(SIGALRM, &sa, NULL);
    setitimer(ITIMER_REAL, &fast, NULL);
    busy = spin(50000000);
    check("an ignored 1 us interval timer leaves the program running", busy > base / 5);
    /* The handler turns the timer off: whether the 1 us timer still
     * leaves the program any time between signals depends on how long a
     * signal's round trip takes, which is not what this checks. */
    alarms = 0;
    sa.sa_handler = on_alarm_once;
    sigaction(SIGALRM, &sa, NULL);
    spin(5000000);
    setitimer(ITIMER_REAL, &off, NULL);
    check("an ignored interval timer resumes when handled again", alarms > 0);

    printf("timertest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
