/* eventfd: a 64-bit counter as a file, the wakeup primitive of event loops
 * (libuv wakes its loop through one). */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/eventfd.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-52s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static int64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static int64_t value(int fd) {
    uint64_t v;
    return read(fd, &v, sizeof v) == sizeof v ? (int64_t)v : -errno;
}

static int add(int fd, uint64_t v) {
    return write(fd, &v, sizeof v) == sizeof v ? 0 : -errno;
}

static short revents(int fd, short events) {
    struct pollfd p = {fd, events, 0};
    return poll(&p, 1, 0) == 1 ? p.revents : 0;
}

int main(void) {
    int fd = eventfd(5, EFD_NONBLOCK);
    check("eventfd starts at its initial value", fd >= 0 && value(fd) == 5);
    check("reading resets the counter", value(fd) == -EAGAIN);
    add(fd, 3);
    add(fd, 4);
    check("writes add up", value(fd) == 7);

    uint32_t small;
    check("a read of less than 8 bytes is EINVAL", read(fd, &small, sizeof small) == -1 && errno == EINVAL);
    check("writing 2^64-1 is EINVAL", add(fd, UINT64_MAX) == -EINVAL);

    check("an empty counter polls writable, not readable", revents(fd, POLLIN | POLLOUT) == POLLOUT);
    add(fd, UINT64_MAX - 1);
    check("a full counter polls readable, not writable", revents(fd, POLLIN | POLLOUT) == POLLIN);
    check("adding to a full counter is EAGAIN", add(fd, 1) == -EAGAIN);
    check("the full counter reads back whole", value(fd) == (int64_t)(UINT64_MAX - 1));
    close(fd);

    fd = eventfd(3, EFD_SEMAPHORE | EFD_NONBLOCK);
    int ones = 0;
    while (value(fd) == 1) ones++;
    check("EFD_SEMAPHORE reads count down by one", ones == 3);
    close(fd);

    fd = eventfd(0, EFD_CLOEXEC);
    check("EFD_CLOEXEC sets close-on-exec", fd >= 0 && (fcntl(fd, F_GETFD) & FD_CLOEXEC));
    check("unknown flags are EINVAL", eventfd(0, 0x10) == -1 && errno == EINVAL);

    /* A blocking read sleeps until another process adds. */
    pid_t child = fork();
    if (child == 0) {
        struct timespec d = {0, 5000000};
        nanosleep(&d, NULL);
        add(fd, 42);
        _exit(0);
    }
    int64_t t0 = now_ns();
    int64_t got = value(fd);
    int64_t waited = (now_ns() - t0) / 1000;
    waitpid(child, NULL, 0);
    check("a blocking read waits for a write", got == 42 && waited >= 4000);

    /* poll wakes when the counter becomes readable. */
    child = fork();
    if (child == 0) {
        struct timespec d = {0, 2000000};
        nanosleep(&d, NULL);
        uint64_t stamp = (uint64_t)now_ns();
        write(fd, &stamp, sizeof stamp);
        _exit(0);
    }
    struct pollfd p = {fd, POLLIN, 0};
    int r = poll(&p, 1, 5000);
    int64_t woke = now_ns();
    int64_t stamp = value(fd);
    waitpid(child, NULL, 0);
    printf("  (poll woke %lld us after the write)\n", (long long)(woke - stamp) / 1000);
    check("poll wakes within 1 ms of an eventfd write", r == 1 && woke - stamp < 1000000);
    close(fd);

    printf("eventfdtest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
