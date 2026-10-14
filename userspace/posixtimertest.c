/* POSIX timers on the monotonic and wall clocks (the Linux server's timer thread): one-shot
 * and periodic, relative and absolute, SIGEV_SIGNAL (with SIGALRM and the timer's id by
 * default), SIGEV_THREAD_ID, SIGEV_NONE and musl's SIGEV_THREAD; the siginfo of their
 * signals (SI_TIMER, si_timerid, si_value, si_overrun), one pending signal per timer with
 * the periods missed meanwhile counted, a signal of an old setting dropped, timers whose
 * ignored signals wait for a handler, RLIMIT_SIGPENDING, the errors, and timers across
 * fork and execve (POSIX timers neither inherited nor kept, interval timers kept). */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SIGEV_THREAD_ID
#define SIGEV_THREAD_ID 4
#endif

/* sigevent's thread id (sigev_notify_thread_id), after sigev_value, sigev_signo and
 * sigev_notify (older musl names no field for it). */
static void set_tid(struct sigevent *sev, pid_t tid) { memcpy((char *)sev + 16, &tid, sizeof tid); }

static int failures;

static void check(const char *name, int ok) {
    printf("%-58s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static int64_t ns_of(clockid_t clock) {
    struct timespec ts;
    clock_gettime(clock, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static int64_t now_ns(void) { return ns_of(CLOCK_MONOTONIC); }

static void sleep_ms(long ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000};
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR) {
    }
}

static volatile sig_atomic_t signals;
static volatile int64_t signal_at;
static volatile int si_code_seen, si_timerid_seen, si_overrun_seen;
static volatile intptr_t si_value_seen;
static volatile pid_t handler_tid;

static void on_timer(int sig, siginfo_t *si, void *uc) {
    (void)sig;
    (void)uc;
    signals++;
    signal_at = now_ns();
    si_code_seen = si->si_code;
    si_timerid_seen = si->si_timerid;
    si_overrun_seen = si->si_overrun;
    si_value_seen = (intptr_t)si->si_value.sival_ptr;
    handler_tid = (pid_t)syscall(SYS_gettid);
}

static void handle(int sig) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_timer;
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    sigaction(sig, &sa, NULL);
}

/* Waits up to `ms` for `signals` to reach `want`. */
static int wait_signals(int want, long ms) {
    int64_t end = now_ns() + ms * 1000000LL;
    while (signals < want && now_ns() < end) sleep_ms(5);
    return signals >= want;
}

static int is_pending(int sig) {
    sigset_t set;
    sigpending(&set);
    return sigismember(&set, sig);
}

static void block(int sig, int how) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, sig);
    sigprocmask(how, &set, NULL);
}

static long ksettime(int id, int flags, const struct itimerspec *n, struct itimerspec *o) { return syscall(SYS_timer_settime, id, flags, n, o); }

static void basics(void) {
    handle(SIGALRM);

    /* No sigevent: SIGALRM, with the timer's id as its value. */
    timer_t t;
    check("timer_create(CLOCK_MONOTONIC, NULL)", timer_create(CLOCK_MONOTONIC, NULL, &t) == 0 && (intptr_t)t >= 0);
    struct itimerspec cur;
    check("a new timer is disarmed", timer_gettime(t, &cur) == 0 && cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec == 0);
    struct itimerspec its = {{0, 0}, {0, 50000000}};
    int64_t start = now_ns();
    check("timer_settime arms it", timer_settime(t, 0, &its, NULL) == 0);
    timer_gettime(t, &cur);
    check("timer_gettime counts down", cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec > 0 && cur.it_value.tv_nsec <= 50000000);
    check("a one-shot timer signals once", wait_signals(1, 10000));
    check("... not before its time", signal_at - start >= 50000000);
    check("... with SIGALRM's siginfo: SI_TIMER, its id as value",
          si_code_seen == SI_TIMER && si_timerid_seen == (int)(intptr_t)t && si_value_seen == (intptr_t)t && si_overrun_seen == 0);
    sleep_ms(100);
    check("... and only once", signals == 1);
    check("it is disarmed after it expired", timer_gettime(t, &cur) == 0 && cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec == 0);

    /* Periodic: one pending signal, the periods missed meanwhile its overrun. */
    signals = 0;
    block(SIGALRM, SIG_BLOCK);
    its = (struct itimerspec){{0, 10000000}, {0, 10000000}};
    struct itimerspec old;
    timer_settime(t, 0, &its, NULL);
    sleep_ms(200);
    check("a periodic timer's signal pends while blocked", is_pending(SIGALRM));
    sigset_t alrm;
    sigemptyset(&alrm);
    sigaddset(&alrm, SIGALRM);
    siginfo_t si;
    check("sigwaitinfo takes it", sigwaitinfo(&alrm, &si) == SIGALRM && si.si_code == SI_TIMER);
    check("... its si_overrun counts the periods missed", si.si_overrun >= 19 && si.si_overrun < 100000);
    check("timer_getoverrun says the same", timer_getoverrun(t) == si.si_overrun);
    block(SIGALRM, SIG_UNBLOCK);
    check("... and the timer goes on", wait_signals(3, 10000));
    timer_gettime(t, &cur);
    check("timer_gettime of a periodic timer: within its interval",
          cur.it_interval.tv_nsec == 10000000 && cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec <= 10000000);
    struct itimerspec zero = {{0, 0}, {0, 0}};
    check("disarming returns the old setting", timer_settime(t, 0, &zero, &old) == 0 && old.it_interval.tv_nsec == 10000000);
    timer_gettime(t, &cur);
    check("... and it is off (interval too)", cur.it_value.tv_nsec == 0 && cur.it_interval.tv_nsec == 0);

    /* A new setting drops the old one's signal still pending (Linux 6.13). */
    block(SIGALRM, SIG_BLOCK);
    its = (struct itimerspec){{0, 0}, {0, 1000000}};
    timer_settime(t, 0, &its, NULL);
    sleep_ms(50);
    int was = is_pending(SIGALRM);
    its.it_value.tv_sec = 100;
    timer_settime(t, 0, &its, NULL);
    check("a timer set again drops its pending signal", was && !is_pending(SIGALRM));
    its.it_value = (struct timespec){0, 1000000};
    timer_settime(t, 0, &its, NULL);
    sleep_ms(50);
    was = is_pending(SIGALRM);
    check("timer_delete drops its pending signal", timer_delete(t) == 0 && was && !is_pending(SIGALRM));
    block(SIGALRM, SIG_UNBLOCK);

    /* Absolute, on the wall clock; and one already past. */
    signals = 0;
    check("timer_create(CLOCK_REALTIME)", timer_create(CLOCK_REALTIME, NULL, &t) == 0);
    struct timespec wall;
    clock_gettime(CLOCK_REALTIME, &wall);
    its = (struct itimerspec){{0, 0}, wall};
    its.it_value.tv_nsec += 100000000;
    if (its.it_value.tv_nsec >= 1000000000) {
        its.it_value.tv_sec++;
        its.it_value.tv_nsec -= 1000000000;
    }
    timer_settime(t, TIMER_ABSTIME, &its, NULL);
    int fired = wait_signals(1, 10000);
    clock_gettime(CLOCK_REALTIME, &wall);
    int64_t late = (wall.tv_sec - its.it_value.tv_sec) * 1000000000LL + (wall.tv_nsec - its.it_value.tv_nsec);
    check("an absolute wall-clock timer expires at its time, not before", fired && late >= 0);
    signals = 0;
    its.it_value.tv_sec -= 10;
    timer_settime(t, TIMER_ABSTIME, &its, NULL);
    check("... one set for a time past expires at once", wait_signals(1, 2000));
    timer_delete(t);

    /* CLOCK_BOOTTIME. */
    signals = 0;
    check("timer_create(CLOCK_BOOTTIME)", timer_create(CLOCK_BOOTTIME, NULL, &t) == 0);
    its = (struct itimerspec){{0, 0}, {0, 20000000}};
    timer_settime(t, 0, &its, NULL);
    check("... expires", wait_signals(1, 10000));
    timer_delete(t);
}

static void notify_kinds(void) {
    /* SIGEV_NONE: nothing is told, but the time counts down (and a periodic one runs on). */
    struct sigevent sev;
    memset(&sev, 0, sizeof sev);
    sev.sigev_notify = SIGEV_NONE;
    timer_t t;
    signals = 0;
    check("timer_create(SIGEV_NONE)", timer_create(CLOCK_MONOTONIC, &sev, &t) == 0);
    struct itimerspec its = {{0, 30000000}, {0, 30000000}}, cur;
    timer_settime(t, 0, &its, NULL);
    sleep_ms(100);
    timer_gettime(t, &cur);
    check("a SIGEV_NONE timer sends nothing", signals == 0);
    check("... and its periods go on", cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec > 0 && cur.it_value.tv_nsec <= 30000000);
    timer_delete(t);

    /* SIGEV_SIGNAL with a value of the caller's, a real-time signal. */
    handle(SIGRTMIN + 2);
    signals = 0;
    sev.sigev_notify = SIGEV_SIGNAL;
    sev.sigev_signo = SIGRTMIN + 2;
    sev.sigev_value.sival_ptr = (void *)0x1234;
    check("timer_create(SIGEV_SIGNAL, SIGRTMIN+2)", timer_create(CLOCK_MONOTONIC, &sev, &t) == 0);
    its = (struct itimerspec){{0, 0}, {0, 10000000}};
    timer_settime(t, 0, &its, NULL);
    check("... sends its signal with the value given", wait_signals(1, 10000) && si_value_seen == 0x1234 && si_code_seen == SI_TIMER);
    timer_delete(t);

    /* SIGEV_THREAD_ID: the signal goes to that thread. */
    signals = 0;
    sev.sigev_notify = SIGEV_THREAD_ID;
    sev.sigev_signo = SIGRTMIN + 2;
    set_tid(&sev, (pid_t)syscall(SYS_gettid));
    int id;
    check("timer_create(SIGEV_THREAD_ID, the caller)", syscall(SYS_timer_create, CLOCK_MONOTONIC, &sev, &id) == 0);
    ksettime(id, 0, &its, NULL);
    check("... signals that thread", wait_signals(1, 10000) && handler_tid == (pid_t)syscall(SYS_gettid));
    syscall(SYS_timer_delete, id);
    set_tid(&sev, 99999);
    errno = 0;
    check("SIGEV_THREAD_ID of no thread of the process is EINVAL", syscall(SYS_timer_create, CLOCK_MONOTONIC, &sev, &id) < 0 && errno == EINVAL);
    set_tid(&sev, getppid());
    errno = 0;
    check("... nor of another process's", syscall(SYS_timer_create, CLOCK_MONOTONIC, &sev, &id) < 0 && errno == EINVAL);
}

static sem_t fired;
static volatile int thread_value;

static void on_thread(union sigval v) {
    thread_value = v.sival_int;
    sem_post(&fired);
}

static void sigev_thread(void) {
    /* musl's SIGEV_THREAD: a thread of its own waits for SIGEV_THREAD_ID signals. */
    sem_init(&fired, 0, 0);
    struct sigevent sev;
    memset(&sev, 0, sizeof sev);
    sev.sigev_notify = SIGEV_THREAD;
    sev.sigev_notify_function = on_thread;
    sev.sigev_value.sival_int = 77;
    timer_t t;
    check("timer_create(SIGEV_THREAD)", timer_create(CLOCK_MONOTONIC, &sev, &t) == 0);
    struct itimerspec its = {{0, 20000000}, {0, 20000000}};
    timer_settime(t, 0, &its, NULL);
    struct timespec until;
    clock_gettime(CLOCK_REALTIME, &until);
    until.tv_sec += 10;
    int n = 0;
    while (n < 3 && sem_timedwait(&fired, &until) == 0) n++;
    check("SIGEV_THREAD runs the function again and again", n == 3 && thread_value == 77);
    check("... until the timer is deleted", timer_delete(t) == 0);
}

static void errors(void) {
    struct sigevent sev;
    memset(&sev, 0, sizeof sev);
    timer_t t;
    errno = 0;
    check("timer_create of an unknown clock is EINVAL", timer_create(100, NULL, &t) < 0 && errno == EINVAL);
    errno = 0;
    check("timer_create(CLOCK_MONOTONIC_COARSE) is EOPNOTSUPP", timer_create(CLOCK_MONOTONIC_COARSE, NULL, &t) < 0 && errno == EOPNOTSUPP);
    sev.sigev_notify = SIGEV_SIGNAL;
    sev.sigev_signo = 65;
    errno = 0;
    check("a sigevent with signal 65 is EINVAL", timer_create(CLOCK_MONOTONIC, &sev, &t) < 0 && errno == EINVAL);
    sev.sigev_signo = SIGUSR1;
    sev.sigev_notify = 99;
    errno = 0;
    check("an unknown sigev_notify is EINVAL", syscall(SYS_timer_create, CLOCK_MONOTONIC, &sev, &t) < 0 && errno == EINVAL);
    errno = 0;
    check("a sigevent at a bad address is EFAULT", syscall(SYS_timer_create, CLOCK_MONOTONIC, (void *)8, &t) < 0 && errno == EFAULT);
    struct itimerspec its = {{0, 0}, {0, 1000000000}};
    int id;
    syscall(SYS_timer_create, CLOCK_MONOTONIC, NULL, &id);
    errno = 0;
    check("timer_settime with 10^9 nanoseconds is EINVAL", ksettime(id, 0, &its, NULL) < 0 && errno == EINVAL);
    its.it_value.tv_nsec = -1;
    errno = 0;
    check("... and with negative ones", ksettime(id, 0, &its, NULL) < 0 && errno == EINVAL);
    syscall(SYS_timer_delete, id);
    its.it_value.tv_nsec = 1000;
    errno = 0;
    check("timer_settime of a deleted timer is EINVAL", ksettime(id, 0, &its, NULL) < 0 && errno == EINVAL);
    errno = 0;
    check("timer_gettime of no timer is EINVAL", syscall(SYS_timer_gettime, 12345, &its) < 0 && errno == EINVAL);
    errno = 0;
    check("timer_getoverrun of no timer is EINVAL", syscall(SYS_timer_getoverrun, 12345) < 0 && errno == EINVAL);
    errno = 0;
    check("timer_delete twice is EINVAL", syscall(SYS_timer_delete, id) < 0 && errno == EINVAL);

    /* Ids: the lowest free one. */
    int a, b, c;
    syscall(SYS_timer_create, CLOCK_MONOTONIC, NULL, &a);
    syscall(SYS_timer_create, CLOCK_MONOTONIC, NULL, &b);
    syscall(SYS_timer_delete, a);
    syscall(SYS_timer_create, CLOCK_MONOTONIC, NULL, &c);
    check("a deleted timer's id is used again", c == a && b != a);
    syscall(SYS_timer_delete, b);
    syscall(SYS_timer_delete, c);

    /* Each timer counts against RLIMIT_SIGPENDING (in a child: the limit is the
     * process's). */
    pid_t child = fork();
    if (child == 0) {
        struct rlimit rl = {4, 4};
        setrlimit(RLIMIT_SIGPENDING, &rl);
        int n = 0, ids[16];
        while (n < 16 && syscall(SYS_timer_create, CLOCK_MONOTONIC, NULL, &ids[n]) == 0) n++;
        int again = errno == EAGAIN;
        for (int i = 0; i < n; i++) syscall(SYS_timer_delete, ids[i]);
        int after = syscall(SYS_timer_create, CLOCK_MONOTONIC, NULL, &ids[0]) == 0;
        _exit(again && n >= 1 && n <= 4 && after ? 0 : 1);
    }
    int status = 0;
    waitpid(child, &status, 0);
    check("timers beyond RLIMIT_SIGPENDING are EAGAIN", WIFEXITED(status) && WEXITSTATUS(status) == 0);
}

static void ignored(void) {
    /* A timer whose signal is dropped as ignored waits until someone can take it. */
    signal(SIGUSR2, SIG_IGN);
    struct sigevent sev;
    memset(&sev, 0, sizeof sev);
    sev.sigev_notify = SIGEV_SIGNAL;
    sev.sigev_signo = SIGUSR2;
    timer_t t;
    timer_create(CLOCK_MONOTONIC, &sev, &t);
    struct itimerspec its = {{0, 10000000}, {0, 10000000}};
    timer_settime(t, 0, &its, NULL);
    sleep_ms(100);
    signals = 0;
    handle(SIGUSR2);
    check("an ignored timer's signals come once a handler is there", wait_signals(2, 10000));
    timer_delete(t);
}

static void inheritance(const char *self) {
    timer_t t;
    timer_create(CLOCK_MONOTONIC, NULL, &t);
    struct itimerspec its = {{0, 0}, {100, 0}};
    timer_settime(t, 0, &its, NULL);
    pid_t child = fork();
    if (child == 0) {
        struct itimerspec cur;
        int none = syscall(SYS_timer_gettime, (int)(intptr_t)t, &cur) < 0 && errno == EINVAL;
        int id = -1;
        syscall(SYS_timer_create, CLOCK_MONOTONIC, NULL, &id);
        _exit(none && id == 0 ? 0 : 1);
    }
    int status = 0;
    waitpid(child, &status, 0);
    check("a fork's child has none of its parent's timers", WIFEXITED(status) && WEXITSTATUS(status) == 0);

    /* execve: POSIX timers go, interval timers stay. */
    child = fork();
    if (child == 0) {
        struct itimerval it = {{0, 0}, {100, 0}};
        setitimer(ITIMER_REAL, &it, NULL);
        setitimer(ITIMER_VIRTUAL, &it, NULL);
        int id = -1;
        syscall(SYS_timer_create, CLOCK_MONOTONIC, NULL, &id);
        if (id != 0) _exit(3);
        execl(self, self, "exec", NULL);
        _exit(2);
    }
    waitpid(child, &status, 0);
    check("execve deletes POSIX timers and keeps interval timers", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    timer_delete(t);
}

/* The exec'd side of `inheritance`: its timer 0 is gone, its interval timers run. */
static int after_exec(void) {
    struct itimerspec cur;
    int gone = syscall(SYS_timer_gettime, 0, &cur) < 0 && errno == EINVAL;
    struct itimerval real, virt;
    getitimer(ITIMER_REAL, &real);
    getitimer(ITIMER_VIRTUAL, &virt);
    return gone && real.it_value.tv_sec > 90 && virt.it_value.tv_sec > 90 ? 0 : 1;
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "exec") == 0) return after_exec();
    basics();
    notify_kinds();
    sigev_thread();
    errors();
    ignored();
    inheritance(argv[0][0] == '/' ? argv[0] : "/bin/posixtimertest");
    if (failures) {
        printf("posixtimertest: %d FAILED\n", failures);
        return 1;
    }
    printf("posixtimertest: all passed\n");
    return 0;
}
