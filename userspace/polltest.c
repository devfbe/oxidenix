/* poll and select (the Linux server's, phase R6e) wake up when a
 * descriptor becomes ready, not at the next scheduler tick: the waiter is
 * subscribed to the watch lists of the files it polls (and waits on the
 * words netd wakes for sockets). Signals end them as on Linux: EINTR
 * after a handler, even under SA_RESTART; a stop and a continue do not end
 * them (restarted with the time left, which select and ppoll write back);
 * descriptors that are not open are POLLNVAL for poll, EBADF for select
 * (an O_PATH one: POLLNVAL, never ready for select). */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <arpa/inet.h>
#include <netinet/in.h>
#include <sys/select.h>
#include <sys/socket.h>
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

/* The same for a datagram over loopback: readiness that netd announces. */
static int64_t socket_latency(void) {
    int rx = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in addr = {.sin_family = AF_INET, .sin_port = htons(47124), .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    if (bind(rx, (struct sockaddr *)&addr, sizeof addr) < 0) printf("  (socket or bind: %s)\n", strerror(errno));
    pid_t child = fork();
    if (child == 0) {
        int tx = socket(AF_INET, SOCK_DGRAM, 0);
        struct timespec d = {0, 2000000};
        nanosleep(&d, NULL);
        int64_t t = now_ns();
        if (sendto(tx, &t, sizeof t, 0, (struct sockaddr *)&addr, sizeof addr) != sizeof t) {
            printf("  (sendto: %s)\n", strerror(errno));
            _exit(1);
        }
        _exit(0);
    }
    struct pollfd fds[1] = {{rx, POLLIN, 0}};
    int ready = poll(fds, 1, 5000) == 1 && fds[0].revents == POLLIN;
    int64_t woke = now_ns(), sent = 0;
    // Never waiting for a datagram that did not come.
    recv(rx, &sent, sizeof sent, ready ? 0 : MSG_DONTWAIT);
    waitpid(child, NULL, 0);
    close(rx);
    return ready ? (woke - sent) / 1000 : 1000000;
}

/* The same for data on a TCP connection over loopback. */
static int64_t tcp_latency(int port) {
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in addr = {.sin_family = AF_INET, .sin_port = htons(port), .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    int one = 1;
    setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    if (bind(l, (struct sockaddr *)&addr, sizeof addr) < 0 || listen(l, 1) < 0) {
        printf("  (bind or listen: %s)\n", strerror(errno));
        close(l);
        return 1000000;
    }
    int c = socket(AF_INET, SOCK_STREAM, 0);
    connect(c, (struct sockaddr *)&addr, sizeof addr);
    int s = accept(l, NULL, NULL);
    pid_t child = fork();
    if (child == 0) {
        struct timespec d = {0, 2000000};
        nanosleep(&d, NULL);
        int64_t t = now_ns();
        write(c, &t, sizeof t);
        _exit(0);
    }
    struct pollfd fds[1] = {{s, POLLIN, 0}};
    int ready = poll(fds, 1, 5000) == 1 && fds[0].revents == POLLIN;
    int64_t woke = now_ns(), sent = 0;
    recv(s, &sent, sizeof sent, ready ? MSG_WAITALL : MSG_DONTWAIT);
    waitpid(child, NULL, 0);
    close(s);
    close(c);
    close(l);
    return ready ? (woke - sent) / 1000 : 1000000;
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

    int64_t t[9];
    for (int i = 0; i < 9; i++) t[i] = socket_latency();
    qsort(t, 9, sizeof t[0], cmp);
    printf("  (poll woke %lld us after a datagram was sent)\n", (long long)t[4]);
    check("poll wakes within 1 ms of a datagram", t[4] < 1000);

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
    /* Even under SA_RESTART: poll and select are not restarted after a handler. */
    sa.sa_flags = SA_RESTART;
    sigaction(SIGALRM, &sa, NULL);
    struct itimerval it = {{0, 0}, {0, 50000}};
    setitimer(ITIMER_REAL, &it, NULL);
    r = poll(&h, 1, 2000);
    check("... also with SA_RESTART (Linux's ERESTARTNOHAND)", r == -1 && errno == EINTR);
    setitimer(ITIMER_REAL, &it, NULL);
    fd_set rs;
    FD_ZERO(&rs);
    FD_SET(p[0], &rs);
    r = select(p[0] + 1, &rs, NULL, NULL, NULL);
    check("select likewise", r == -1 && errno == EINTR);
    close(p[0]);
    close(p[1]);

    /* Descriptors that are not open: POLLNVAL for poll (a negative one is
     * skipped), EBADF for select. */
    pipe(p);
    int bad = p[0];
    close(p[0]);
    struct pollfd nv[2] = {{bad, POLLIN, 0}, {-1, POLLIN, 0x7}};
    r = poll(nv, 2, 0);
    check("poll: a closed descriptor is POLLNVAL, a negative one skipped", r == 1 && nv[0].revents == POLLNVAL && nv[1].revents == 0);
    FD_ZERO(&rs);
    FD_SET(bad, &rs);
    struct timeval tv0 = {0, 0};
    check("select: a closed descriptor is EBADF", select(bad + 1, &rs, NULL, NULL, &tv0) == -1 && errno == EBADF);
    int opath = open("/tmp", O_PATH);
    nv[0] = (struct pollfd){opath, POLLIN, 0};
    check("poll: an O_PATH descriptor is POLLNVAL", poll(nv, 1, 0) == 1 && nv[0].revents == POLLNVAL);
    FD_ZERO(&rs);
    FD_SET(opath, &rs);
    check("select: an O_PATH descriptor is never ready", select(opath + 1, &rs, NULL, NULL, &tv0) == 0);
    close(opath);
    close(p[1]);

    /* select takes microseconds beyond a second as seconds (Linux). */
    pipe(p);
    write(p[1], "x", 1);
    FD_ZERO(&rs);
    FD_SET(p[0], &rs);
    struct timeval long_usec = {0, 1500000};
    check("select: tv_usec beyond a second is no EINVAL", syscall(SYS_select, p[0] + 1, &rs, NULL, NULL, &long_usec) == 1);
    close(p[0]);
    close(p[1]);

    /* select writes back the time left (as Linux; musl's wrapper hides it,
     * so the call itself). */
    pipe(p);
    child = fork();
    if (child == 0) {
        struct timespec d = {0, 100000000};
        nanosleep(&d, NULL);
        write(p[1], "x", 1);
        _exit(0);
    }
    FD_ZERO(&rs);
    FD_SET(p[0], &rs);
    struct timeval tv = {1, 0};
    r = syscall(SYS_select, p[0] + 1, &rs, NULL, NULL, &tv);
    waitpid(child, NULL, 0);
    long left = tv.tv_sec * 1000000 + tv.tv_usec;
    printf("  (select left %ld us of 1 s)\n", left);
    check("select writes back the time left", r == 1 && left > 500000 && left < 950000);
    struct timespec ts = {1, 0};
    char x;
    read(p[0], &x, 1);
    child = fork();
    if (child == 0) {
        struct timespec d = {0, 100000000};
        nanosleep(&d, NULL);
        write(p[1], "x", 1);
        _exit(0);
    }
    struct pollfd pf = {p[0], POLLIN, 0};
    r = syscall(SYS_ppoll, &pf, 1, &ts, NULL, 8);
    waitpid(child, NULL, 0);
    left = ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
    check("ppoll writes back the time left", r == 1 && left > 500000 && left < 950000);
    close(p[0]);
    close(p[1]);

    /* A stop and a continue (no handler) do not end a poll or a select:
     * they go on with the time left (poll by restart_syscall). */
    for (int how = 0; how < 3; how++) {
        pipe(p);
        child = fork();
        if (child == 0) {
            int64_t start = now_ns();
            struct pollfd idle = {p[0], POLLIN, 0};
            int rr;
            if (how == 0) {
                rr = poll(&idle, 1, 300);
            } else if (how == 1) {
                fd_set s;
                FD_ZERO(&s);
                FD_SET(p[0], &s);
                struct timeval t300 = {0, 300000};
                rr = select(p[0] + 1, &s, NULL, NULL, &t300);
            } else {
                struct timespec t300 = {0, 300000000};
                rr = ppoll(&idle, 1, &t300, NULL);
            }
            int64_t took = (now_ns() - start) / 1000000;
            _exit(rr == 0 && took >= 290 && took < 600 ? 0 : 1);
        }
        struct timespec d = {0, 80000000};
        nanosleep(&d, NULL);
        kill(child, SIGSTOP);
        nanosleep(&d, NULL);
        kill(child, SIGCONT);
        int status;
        waitpid(child, &status, 0);
        const char *names[] = {"poll goes on after a stop and a continue", "select goes on after a stop and a continue",
                               "ppoll goes on after a stop and a continue"};
        check(names[how], WIFEXITED(status) && WEXITSTATUS(status) == 0);
        close(p[0]);
        close(p[1]);
    }
    errno = 0;
    check("restart_syscall with nothing to restart is EINTR", syscall(SYS_restart_syscall) == -1 && errno == EINTR);

    /* A TCP connection: netd wakes the poller directly. */
    int64_t tt[5];
    for (int i = 0; i < 5; i++) tt[i] = tcp_latency(47125 + i);
    qsort(tt, 5, sizeof tt[0], cmp);
    printf("  (poll woke %lld us after data was sent on a TCP connection)\n", (long long)tt[2]);
    check("poll wakes within 1 ms of TCP data", tt[2] < 1000);

    printf("polltest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
