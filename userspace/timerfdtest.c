/* timerfd (the Linux server's): a timer read as a count of expiries; one-shot and periodic,
 * relative and absolute, blocking and nonblocking reads, poll and epoll (an event per
 * expiry for EPOLLET), timerfd_gettime, the old setting, TFD_IOC_SET_TICKS, cancelation by
 * a setting of the wall clock, sharing across fork, and the errors. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/timerfd.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef TFD_TIMER_CANCEL_ON_SET
#define TFD_TIMER_CANCEL_ON_SET 2
#endif
#ifndef TFD_IOC_SET_TICKS
#define TFD_IOC_SET_TICKS _IOW('T', 0, uint64_t)
#endif

static int failures;

static void check(const char *name, int ok) {
    printf("%-56s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static int64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static void sleep_ms(long ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000};
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR) {
    }
}

static int64_t count(int fd) {
    uint64_t v;
    return read(fd, &v, sizeof v) == sizeof v ? (int64_t)v : -errno;
}

static int arm(int fd, int flags, long value_ms, long interval_ms) {
    struct itimerspec its = {{interval_ms / 1000, (interval_ms % 1000) * 1000000}, {value_ms / 1000, (value_ms % 1000) * 1000000}};
    return timerfd_settime(fd, flags, &its, NULL);
}

static void on_alarm(int sig) { (void)sig; }

int main(void) {
    int fd = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC);
    check("timerfd_create(CLOCK_MONOTONIC)", fd >= 0 && (fcntl(fd, F_GETFD) & FD_CLOEXEC));
    struct itimerspec cur;
    check("a new timerfd is disarmed", timerfd_gettime(fd, &cur) == 0 && cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec == 0);

    /* One-shot: a read waits for it, then takes 1. */
    int64_t start = now_ns();
    arm(fd, 0, 50, 0);
    timerfd_gettime(fd, &cur);
    check("timerfd_gettime counts down", cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec > 0 && cur.it_value.tv_nsec <= 50000000);
    int64_t n = count(fd);
    check("a read waits for the expiry and takes 1", n == 1);
    check("... not before its time", now_ns() - start >= 50000000);
    check("a one-shot timerfd is disarmed after it", timerfd_gettime(fd, &cur) == 0 && cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec == 0);

    /* Periodic: the expiries add up until read. */
    start = now_ns();
    arm(fd, 0, 10, 10);
    sleep_ms(105);
    n = count(fd);
    int64_t elapsed = now_ns() - start;
    check("a periodic timerfd counts every expiry", n >= 10 && n <= elapsed / 10000000);
    timerfd_gettime(fd, &cur);
    check("... and runs on", cur.it_interval.tv_nsec == 10000000 && cur.it_value.tv_nsec > 0 && cur.it_value.tv_nsec <= 10000000);
    struct itimerspec zero = {{0, 0}, {0, 0}}, old;
    check("disarming returns the old setting", timerfd_settime(fd, 0, &zero, &old) == 0 && old.it_interval.tv_nsec == 10000000);

    /* Nonblocking. */
    int nb = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    check("a nonblocking read with no expiry is EAGAIN", count(nb) == -EAGAIN);
    arm(nb, 0, 20, 0);
    check("... and with the timer running too", count(nb) == -EAGAIN);
    sleep_ms(40);
    check("... and takes the expiry once it is there", count(nb) == 1);

    /* poll waits for the expiry; then the descriptor is readable until read. */
    start = now_ns();
    arm(nb, 0, 50, 0);
    struct pollfd p = {nb, POLLIN, 0};
    int early = poll(&p, 1, 0);
    int64_t looked = now_ns() - start;
    /* (Unless the host stalled past the expiry meanwhile.) */
    check("poll: not readable before the expiry", early == 0 || looked >= 50000000);
    int r = poll(&p, 1, 5000);
    elapsed = now_ns() - start;
    check("poll wakes at the expiry", r == 1 && (p.revents & POLLIN) && elapsed < 4000000000LL);
    check("... not before it", elapsed >= 50000000);
    check("... and stays readable until read", poll(&p, 1, 0) == 1 && count(nb) == 1 && poll(&p, 1, 0) == 0);

    /* epoll, edge-triggered: an event per expiry, even unread. */
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN | EPOLLET, .data.fd = nb}, out;
    epoll_ctl(ep, EPOLL_CTL_ADD, nb, &ev);
    arm(nb, 0, 20, 20);
    int events = 0;
    for (int i = 0; i < 3; i++) events += epoll_wait(ep, &out, 1, 5000) == 1;
    check("EPOLLET: each expiry is an event", events == 3);
    n = count(nb);
    check("... and they were counted", n >= 3);
    timerfd_settime(nb, 0, &zero, NULL);
    close(ep);

    /* Absolute, on the wall clock. */
    int rt = timerfd_create(CLOCK_REALTIME, 0);
    struct timespec wall;
    clock_gettime(CLOCK_REALTIME, &wall);
    struct itimerspec abs = {{0, 0}, wall};
    abs.it_value.tv_nsec += 50000000;
    if (abs.it_value.tv_nsec >= 1000000000) {
        abs.it_value.tv_sec++;
        abs.it_value.tv_nsec -= 1000000000;
    }
    check("TFD_TIMER_ABSTIME on CLOCK_REALTIME", timerfd_settime(rt, TFD_TIMER_ABSTIME, &abs, NULL) == 0);
    n = count(rt);
    clock_gettime(CLOCK_REALTIME, &wall);
    int64_t late = (wall.tv_sec - abs.it_value.tv_sec) * 1000000000LL + (wall.tv_nsec - abs.it_value.tv_nsec);
    check("... expires at that time, not before", n == 1 && late >= 0);
    abs.it_value.tv_sec -= 10;
    timerfd_settime(rt, TFD_TIMER_ABSTIME, &abs, NULL);
    check("... and at once for a time past", count(rt) == 1);

    /* Canceled when the wall clock is set (to its own time: nothing else changes). */
    abs.it_value.tv_sec += 1000;
    timerfd_settime(rt, TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET, &abs, NULL);
    struct timespec t;
    clock_gettime(CLOCK_REALTIME, &t);
    if (clock_settime(CLOCK_REALTIME, &t) == 0) {
        check("TFD_TIMER_CANCEL_ON_SET: a read after a clock change is ECANCELED", count(rt) == -ECANCELED);
        timerfd_settime(rt, 0, &zero, NULL);
    } else {
        printf("(no permission to set the clock: cancelation not checked)\n");
    }
    close(rt);

    /* TFD_IOC_SET_TICKS. */
    uint64_t ticks = 5;
    check("TFD_IOC_SET_TICKS sets the count", ioctl(nb, TFD_IOC_SET_TICKS, &ticks) == 0 && count(nb) == 5);
    ticks = 0;
    errno = 0;
    check("... of 0 is EINVAL", ioctl(nb, TFD_IOC_SET_TICKS, &ticks) < 0 && errno == EINVAL);

    /* Shared across fork: the child reads the parent's expiries. */
    arm(fd, 0, 30, 0);
    pid_t child = fork();
    if (child == 0) _exit(count(fd) == 1 ? 0 : 1);
    int status = 0;
    waitpid(child, &status, 0);
    check("a fork's child reads the same timer", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    uint64_t v = 0;
    struct pollfd shared = {fd, POLLIN, 0};
    check("... whose expiry is gone for the parent", poll(&shared, 1, 0) == 0);

    /* A blocking read ends with EINTR for a handler without SA_RESTART. */
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alarm;
    sigaction(SIGALRM, &sa, NULL);
    arm(fd, 0, 5000, 0);
    alarm(1);
    check("a read interrupted by a signal is EINTR", count(fd) == -EINTR);
    timerfd_settime(fd, 0, &zero, NULL);

    /* /proc/self/fd names it. */
    char path[64], link[64] = {0};
    snprintf(path, sizeof path, "/proc/self/fd/%d", fd);
    readlink(path, link, sizeof link - 1);
    check("/proc/self/fd shows anon_inode:[timerfd]", strcmp(link, "anon_inode:[timerfd]") == 0);

    /* Errors. */
    errno = 0;
    check("timerfd_create of a CPU clock is EINVAL", timerfd_create(CLOCK_PROCESS_CPUTIME_ID, 0) < 0 && errno == EINVAL);
    errno = 0;
    check("timerfd_create with unknown flags is EINVAL", timerfd_create(CLOCK_MONOTONIC, 1) < 0 && errno == EINVAL);
    struct itimerspec bad = {{0, 0}, {0, 1000000000}};
    errno = 0;
    check("timerfd_settime with 10^9 nanoseconds is EINVAL", timerfd_settime(fd, 0, &bad, NULL) < 0 && errno == EINVAL);
    errno = 0;
    check("timerfd_settime with unknown flags is EINVAL", timerfd_settime(fd, 4, &zero, NULL) < 0 && errno == EINVAL);
    int pipefd[2];
    pipe(pipefd);
    errno = 0;
    check("timerfd_settime of another file is EINVAL", timerfd_settime(pipefd[0], 0, &zero, NULL) < 0 && errno == EINVAL);
    errno = 0;
    check("timerfd_gettime of no descriptor is EBADF", timerfd_gettime(999, &cur) < 0 && errno == EBADF);
    char small[4];
    errno = 0;
    check("a read of less than 8 bytes is EINVAL", read(fd, small, sizeof small) < 0 && errno == EINVAL);
    errno = 0;
    check("a write is EINVAL", write(fd, &v, sizeof v) < 0 && errno == EINVAL);

    if (failures) {
        printf("timerfdtest: %d FAILED\n", failures);
        return 1;
    }
    printf("timerfdtest: all passed\n");
    return 0;
}
