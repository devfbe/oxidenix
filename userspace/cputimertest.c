/* Timers on CPU clocks: ITIMER_VIRTUAL (the process's user time) and ITIMER_PROF (its user
 * and system time), POSIX timers on the process's and a thread's CPU clocks and on another
 * process's: they expire once their clock has counted the time they were set for, never
 * before, not while the process sleeps, and soon after (the kernel's CPU-time alarms tell
 * the Linux server's timer thread; nothing polls a clock). */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-56s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static int64_t ns_of(clockid_t clock) {
    struct timespec ts;
    clock_gettime(clock, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static int64_t wall_ns(void) { return ns_of(CLOCK_MONOTONIC); }
static int64_t cpu_ns(void) { return ns_of(CLOCK_PROCESS_CPUTIME_ID); }

static int64_t user_ns(void) {
    struct rusage ru;
    getrusage(RUSAGE_SELF, &ru);
    return (int64_t)ru.ru_utime.tv_sec * 1000000000 + (int64_t)ru.ru_utime.tv_usec * 1000;
}

static void sleep_ms(long ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000};
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR) {
    }
}

static volatile sig_atomic_t vtalrms, profs, usr1s, usr2s;
static volatile int64_t cpu_at_signal;
static volatile int last_code, last_timerid;

static void on_vtalrm(int sig) {
    (void)sig;
    vtalrms++;
    cpu_at_signal = cpu_ns();
}

static void on_prof(int sig) {
    (void)sig;
    profs++;
}

static void on_info(int sig, siginfo_t *si, void *uc) {
    (void)uc;
    if (sig == SIGUSR1) usr1s++;
    else usr2s++;
    last_code = si->si_code;
    last_timerid = si->si_timerid;
    cpu_at_signal = cpu_ns();
}

static void handle(int sig, void (*fn)(int)) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = fn;
    sa.sa_flags = SA_RESTART;
    sigaction(sig, &sa, NULL);
}

static void handle_info(int sig) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_info;
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    sigaction(sig, &sa, NULL);
}

static volatile uint64_t sink;

/* Spins in user mode until `*flag` reaches `want` or `limit_ms` of wall time passed. */
static int spin_until(volatile sig_atomic_t *flag, int want, long limit_ms) {
    int64_t end = wall_ns() + limit_ms * 1000000LL;
    while (*flag < want && wall_ns() < end) {
        for (int i = 0; i < 100000; i++) sink += i;
    }
    return *flag >= want;
}

/* Spins in user mode for `ms` of the process's CPU time. */
static void spin_cpu(long ms) {
    int64_t end = cpu_ns() + ms * 1000000LL;
    while (cpu_ns() < end) {
        for (int i = 0; i < 10000; i++) sink += i;
    }
}

static void itimers(void) {
    handle(SIGVTALRM, on_vtalrm);
    handle(SIGPROF, on_prof);

    /* A one-shot ITIMER_VIRTUAL: after 100 ms of user time, not before. */
    struct itimerval it = {{0, 0}, {0, 100000}}, cur;
    int64_t start = cpu_ns();
    check("setitimer(ITIMER_VIRTUAL) is accepted", setitimer(ITIMER_VIRTUAL, &it, NULL) == 0);
    check("getitimer(ITIMER_VIRTUAL) reports it running",
          getitimer(ITIMER_VIRTUAL, &cur) == 0 && cur.it_value.tv_sec == 0 && cur.it_value.tv_usec > 0 && cur.it_value.tv_usec <= 100000);
    int got = spin_until(&vtalrms, 1, 20000);
    check("ITIMER_VIRTUAL expires while the process spins", got);
    check("... not before 100 ms of CPU time", cpu_at_signal - start >= 100000000);
    check("... and soon after (within 1 s of CPU time)", cpu_at_signal - start < 1100000000);
    check("a one-shot timer is off after it expired", getitimer(ITIMER_VIRTUAL, &cur) == 0 && cur.it_value.tv_sec == 0 && cur.it_value.tv_usec == 0);

    /* CPU clocks do not move while the process sleeps. */
    vtalrms = 0;
    it.it_value.tv_usec = 50000;
    setitimer(ITIMER_VIRTUAL, &it, NULL);
    sleep_ms(300);
    check("ITIMER_VIRTUAL does not expire while the process sleeps", vtalrms == 0);
    check("... and expires once it runs", spin_until(&vtalrms, 1, 20000));

    /* A periodic ITIMER_PROF: five expiries take at least five intervals of CPU time. */
    profs = 0;
    struct itimerval prof = {{0, 20000}, {0, 20000}};
    start = cpu_ns();
    setitimer(ITIMER_PROF, &prof, NULL);
    got = spin_until(&profs, 5, 30000);
    int64_t used = cpu_ns() - start;
    struct itimerval off = {{0, 0}, {0, 0}}, old;
    setitimer(ITIMER_PROF, &off, &old);
    check("periodic ITIMER_PROF expires again and again", got);
    check("... after its intervals of CPU time, not before", used >= 100000000);
    check("setitimer returns the old interval", old.it_interval.tv_sec == 0 && old.it_interval.tv_usec == 20000);

    /* System time: a process that only makes calls moves ITIMER_PROF's clock, hardly
     * ITIMER_VIRTUAL's (its calls' time is the system's). */
    profs = vtalrms = 0;
    struct itimerval both = {{0, 0}, {0, 100000}};
    int64_t user0 = user_ns();
    start = cpu_ns();
    setitimer(ITIMER_PROF, &both, NULL);
    setitimer(ITIMER_VIRTUAL, &both, NULL);
    int64_t end = wall_ns() + 20000000000LL;
    while (!profs && wall_ns() < end) {
        for (int i = 0; i < 100; i++) syscall(SYS_getppid);
    }
    int64_t user_used = user_ns() - user0;
    check("ITIMER_PROF counts the time of system calls", profs == 1 && cpu_ns() - start >= 100000000);
    check("... which ITIMER_VIRTUAL does not", vtalrms == 0 && user_used < 100000000);
    setitimer(ITIMER_VIRTUAL, &off, NULL);

    /* Errors. */
    errno = 0;
    check("setitimer of an unknown timer is EINVAL", setitimer(3, &it, NULL) < 0 && errno == EINVAL);
    struct itimerval bad = {{0, 0}, {0, 1000000}};
    errno = 0;
    check("setitimer with 1000000 usec is EINVAL", setitimer(ITIMER_PROF, &bad, NULL) < 0 && errno == EINVAL);
    errno = 0;
    check("getitimer of an unknown timer is EINVAL", getitimer(3, &cur) < 0 && errno == EINVAL);

    /* Not inherited by fork: the child's timers are off, and it may spin unharmed
     * (SIGVTALRM's default would kill it). */
    struct itimerval longer = {{0, 0}, {0, 200000}};
    setitimer(ITIMER_VIRTUAL, &longer, NULL);
    pid_t child = fork();
    if (child == 0) {
        struct itimerval c;
        getitimer(ITIMER_VIRTUAL, &c);
        signal(SIGVTALRM, SIG_DFL);
        spin_cpu(300);
        _exit(c.it_value.tv_sec == 0 && c.it_value.tv_usec == 0 ? 0 : 1);
    }
    int status = 0;
    waitpid(child, &status, 0);
    check("a fork's child has no interval timers", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    setitimer(ITIMER_VIRTUAL, &off, NULL);
}

static volatile sig_atomic_t spin_flag;
static volatile int spinner_go;

static void *spinner(void *arg) {
    long ms = (long)arg;
    int64_t end = ns_of(CLOCK_THREAD_CPUTIME_ID) + ms * 1000000LL;
    while (ns_of(CLOCK_THREAD_CPUTIME_ID) < end && !spin_flag) {
        for (int i = 0; i < 10000; i++) sink += i;
    }
    return NULL;
}

static void posix_cpu_timers(void) {
    handle_info(SIGUSR1);
    handle_info(SIGUSR2);

    /* On the process's CPU clock. */
    timer_t t;
    struct sigevent sev;
    memset(&sev, 0, sizeof sev);
    sev.sigev_notify = SIGEV_SIGNAL;
    sev.sigev_signo = SIGUSR1;
    check("timer_create(CLOCK_PROCESS_CPUTIME_ID)", timer_create(CLOCK_PROCESS_CPUTIME_ID, &sev, &t) == 0);
    struct itimerspec its = {{0, 0}, {0, 100000000}}, cur;
    int64_t start = cpu_ns();
    timer_settime(t, 0, &its, NULL);
    int got = spin_until(&usr1s, 1, 20000);
    check("it expires after 100 ms of CPU time", got && cpu_at_signal - start >= 100000000);
    check("... with SI_TIMER and its id", last_code == SI_TIMER && last_timerid == (int)(intptr_t)t);

    /* Absolute, on the clock's own scale. */
    usr1s = 0;
    struct timespec now;
    clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &now);
    its.it_value = now;
    its.it_value.tv_nsec += 50000000;
    if (its.it_value.tv_nsec >= 1000000000) {
        its.it_value.tv_sec++;
        its.it_value.tv_nsec -= 1000000000;
    }
    start = cpu_ns();
    timer_settime(t, TIMER_ABSTIME, &its, NULL);
    timer_gettime(t, &cur);
    check("timer_gettime reports the CPU time left", cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec > 0 && cur.it_value.tv_nsec <= 50000000);
    got = spin_until(&usr1s, 1, 20000);
    check("an absolute CPU-clock timer expires at its time", got && cpu_at_signal - start >= 50000000);
    timer_delete(t);

    /* On the calling thread's clock: other threads' time does not move it. */
    usr2s = 0;
    sev.sigev_signo = SIGUSR2;
    check("timer_create(CLOCK_THREAD_CPUTIME_ID)", timer_create(CLOCK_THREAD_CPUTIME_ID, &sev, &t) == 0);
    its = (struct itimerspec){{0, 0}, {0, 100000000}};
    timer_settime(t, 0, &its, NULL);
    pthread_t th;
    spin_flag = 0;
    pthread_create(&th, NULL, spinner, (void *)300L);
    pthread_join(th, NULL);
    check("a thread's CPU timer waits while another thread runs", usr2s == 0);
    check("... and expires once its thread runs", spin_until(&usr2s, 1, 20000));
    timer_delete(t);

    /* On another thread's clock (pthread_getcpuclockid). */
    usr1s = 0;
    sev.sigev_signo = SIGUSR1;
    spin_flag = 0;
    pthread_create(&th, NULL, spinner, (void *)20000L);
    clockid_t thread_clock;
    pthread_getcpuclockid(th, &thread_clock);
    check("timer_create on another thread's CPU clock", timer_create(thread_clock, &sev, &t) == 0);
    its = (struct itimerspec){{0, 0}, {0, 100000000}};
    timer_settime(t, 0, &its, NULL);
    sigset_t none;
    sigemptyset(&none);
    int64_t wall_end = wall_ns() + 20000000000LL;
    while (!usr1s && wall_ns() < wall_end) sleep_ms(10);
    spin_flag = 1;
    pthread_join(th, NULL);
    check("... expires as that thread runs (its caller asleep)", usr1s == 1);
    timer_delete(t);

    /* On another process's clock (clock_getcpuclockid): the child's time, the parent's signal. */
    usr1s = 0;
    pid_t child = fork();
    if (child == 0) {
        sleep_ms(100);
        spin_cpu(20000);
        _exit(0);
    }
    clockid_t child_clock;
    check("clock_getcpuclockid of a child", clock_getcpuclockid(child, &child_clock) == 0);
    check("timer_create on another process's CPU clock", timer_create(child_clock, &sev, &t) == 0);
    its = (struct itimerspec){{0, 0}, {0, 100000000}};
    timer_settime(t, 0, &its, NULL);
    int64_t own = cpu_ns();
    wall_end = wall_ns() + 20000000000LL;
    while (!usr1s && wall_ns() < wall_end) sleep_ms(10);
    check("... expires as the other process runs", usr1s == 1 && cpu_ns() - own < 100000000);
    kill(child, SIGKILL);
    waitpid(child, NULL, 0);
    errno = 0;
    check("timer_settime on the clock of a process gone is ESRCH", timer_settime(t, 0, &its, NULL) < 0 && errno == ESRCH);
    timer_delete(t);

    /* A timer on a clock that has no timers. */
    errno = 0;
    check("timer_create(CLOCK_MONOTONIC_RAW) is EOPNOTSUPP", timer_create(CLOCK_MONOTONIC_RAW, &sev, &t) < 0 && errno == EOPNOTSUPP);
    errno = 0;
    check("timer_create on a CPU clock of no process is EINVAL", timer_create((clockid_t)((-30000 - 1) * 8 + 2), &sev, &t) < 0 && errno == EINVAL);
}

int main(void) {
    itimers();
    posix_cpu_timers();
    if (failures) {
        printf("cputimertest: %d FAILED\n", failures);
        return 1;
    }
    printf("cputimertest: all passed\n");
    return 0;
}
