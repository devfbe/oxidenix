/* poll and select wake up when a descriptor becomes ready, not at the next
 * scheduler tick: the waiter sits on the wait queues of the files it
 * polls. */
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
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

static int cmp(const void *a, const void *b) {
    int64_t x = *(const int64_t *)a, y = *(const int64_t *)b;
    return (x > y) - (x < y);
}

static void pin(int cpu) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    sched_setaffinity(0, sizeof set, &set);
}

static void on_alarm(int sig) { (void)sig; }

enum { POLL, SELECT };

/* A child writes its clock reading into a pipe 2 ms after the parent
 * starts waiting on the read end (and on an idle second pipe); returns
 * how long after the write the wait ended, in microseconds. */
static int64_t wake_latency(int how) {
    int data[2], idle[2];
    pipe(data);
    pipe(idle);
    pid_t child = fork();
    if (child == 0) {
        struct timespec d = {0, 2000000};
        nanosleep(&d, NULL);
        int64_t t = now_ns();
        write(data[1], &t, sizeof t);
        _exit(0);
    }
    int ready;
    if (how == POLL) {
        struct pollfd fds[2] = {{idle[0], POLLIN, 0}, {data[0], POLLIN, 0}};
        ready = poll(fds, 2, 5000) == 1 && fds[1].revents == POLLIN;
    } else {
        fd_set r;
        FD_ZERO(&r);
        FD_SET(idle[0], &r);
        FD_SET(data[0], &r);
        int n = (idle[0] > data[0] ? idle[0] : data[0]) + 1;
        ready = select(n, &r, NULL, NULL, NULL) == 1 && FD_ISSET(data[0], &r);
    }
    int64_t woke = now_ns(), written = 0;
    read(data[0], &written, sizeof written);
    waitpid(child, NULL, 0);
    close(data[0]);
    close(data[1]);
    close(idle[0]);
    close(idle[1]);
    return ready ? (woke - written) / 1000 : 1000000;
}

static int64_t median_latency(int how) {
    int64_t t[9];
    for (int i = 0; i < 9; i++) t[i] = wake_latency(how);
    qsort(t, 9, sizeof t[0], cmp);
    return t[4];
}

int main(void) {
    int cpus = sysconf(_SC_NPROCESSORS_ONLN);
    int64_t lat = median_latency(POLL);
    printf("  (poll woke %lld us after the write)\n", (long long)lat);
    check("poll wakes within 1 ms of a pipe write", lat < 1000);
    lat = median_latency(SELECT);
    printf("  (select woke %lld us after the write)\n", (long long)lat);
    check("select wakes within 1 ms of a pipe write", lat < 1000);

    /* The same with the writer on another CPU. */
    if (cpus >= 2) {
        pin(0);
        lat = median_latency(POLL);
        pin(1);
        printf("  (poll woke %lld us after a write on another CPU)\n", (long long)lat);
        check("poll wakes on a write from another CPU", lat < 1000);
        cpu_set_t all;
        CPU_ZERO(&all);
        for (int i = 0; i < cpus; i++) CPU_SET(i, &all);
        sched_setaffinity(0, sizeof all, &all);
    }

    /* A full pipe becomes writable when the reader drains it. */
    int p[2];
    pipe(p);
    char buf[4096];
    memset(buf, 'x', sizeof buf);
    struct pollfd w = {p[1], POLLOUT, 0};
    int full = 0;
    while (poll(&w, 1, 0) == 1) {
        write(p[1], buf, sizeof buf);
        full++;
    }
    pid_t child = fork();
    if (child == 0) {
        struct timespec d = {0, 2000000};
        nanosleep(&d, NULL);
        while (read(p[0], buf, sizeof buf) == sizeof buf && poll(&(struct pollfd){p[0], POLLIN, 0}, 1, 0) == 1) {
        }
        _exit(0);
    }
    int64_t t0 = now_ns();
    int r = poll(&w, 1, 5000);
    int64_t waited = (now_ns() - t0) / 1000;
    waitpid(child, NULL, 0);
    check("a full pipe polls writable once drained", full > 0 && r == 1 && w.revents == POLLOUT && waited < 5000);

    /* The last writer closing wakes a reader with POLLHUP. */
    close(p[0]);
    pipe(p);
    child = fork();
    if (child == 0) {
        struct timespec d = {0, 2000000};
        nanosleep(&d, NULL);
        _exit(0);
    }
    close(p[1]);
    struct pollfd h = {p[0], POLLIN, 0};
    t0 = now_ns();
    r = poll(&h, 1, 5000);
    waited = (now_ns() - t0) / 1000;
    waitpid(child, NULL, 0);
    check("closing the last writer wakes poll with POLLHUP", r == 1 && (h.revents & POLLHUP) && waited < 5000);
    close(p[0]);

    /* A signal interrupts a poll that waits on files. */
    pipe(p);
    struct sigaction sa = {0};
    sa.sa_handler = on_alarm;
    sigaction(SIGALRM, &sa, NULL);
    alarm(1);
    h = (struct pollfd){p[0], POLLIN, 0};
    r = poll(&h, 1, -1);
    check("a signal interrupts poll with EINTR", r == -1 && errno == EINTR);
    close(p[0]);
    close(p[1]);

    printf("polltest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
