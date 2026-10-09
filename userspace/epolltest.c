/* epoll (the Linux server's, phase R6e): interest lists with level- and
 * edge-triggered readiness, the event loop interface of libuv and
 * therefore Node.js; EPOLLEXCLUSIVE, nesting, instances shared by fork and
 * passed with SCM_RIGHTS. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <arpa/inet.h>
#include <netinet/in.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/eventfd.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

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

static int add(int ep, int fd, uint32_t events, uint64_t data) {
    struct epoll_event ev = {.events = events, .data.u64 = data};
    return epoll_ctl(ep, EPOLL_CTL_ADD, fd, &ev);
}

static int mod(int ep, int fd, uint32_t events, uint64_t data) {
    struct epoll_event ev = {.events = events, .data.u64 = data};
    return epoll_ctl(ep, EPOLL_CTL_MOD, fd, &ev);
}

/* One non-blocking epoll_wait: the number of events, the first in *ev. */
static int peek(int ep, struct epoll_event *ev) {
    struct epoll_event evs[8];
    int n = epoll_wait(ep, evs, 8, 0);
    if (n > 0 && ev) *ev = evs[0];
    return n;
}

static void on_usr1(int sig) { (void)sig; }

static void sleep_ms(long ms) {
    struct timespec d = {ms / 1000, (ms % 1000) * 1000000};
    nanosleep(&d, NULL);
}

/* Two processes wait in epoll_wait, each on an instance of its own that
 * watches one eventfd (with `exclusive`: EPOLLEXCLUSIVE); one write. How
 * many of them woke? */
static int woken_by_one_write(int exclusive) {
    int efd = eventfd(0, EFD_NONBLOCK);
    int rep[2];
    pipe(rep);
    pid_t kids[2];
    for (int i = 0; i < 2; i++) {
        kids[i] = fork();
        if (kids[i] == 0) {
            int e = epoll_create1(0);
            add(e, efd, EPOLLIN | (exclusive ? EPOLLEXCLUSIVE : 0), 0);
            struct epoll_event ev;
            int n = epoll_wait(e, &ev, 1, 500);
            char c = n == 1 ? 'w' : 't';
            write(rep[1], &c, 1);
            _exit(0);
        }
    }
    sleep_ms(150);
    uint64_t one = 1;
    write(efd, &one, sizeof one);
    for (int i = 0; i < 2; i++) waitpid(kids[i], NULL, 0);
    char got[2] = {0, 0};
    read(rep[0], got, 2);
    close(rep[0]);
    close(rep[1]);
    close(efd);
    return (got[0] == 'w') + (got[1] == 'w');
}

static int send_fd(int sock, int fd) {
    char byte = 'F';
    struct iovec iov = {&byte, 1};
    char control[CMSG_SPACE(sizeof(int))];
    memset(control, 0, sizeof control);
    struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = control, .msg_controllen = sizeof control};
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &fd, sizeof fd);
    return sendmsg(sock, &msg, 0) == 1 ? 0 : -1;
}

static int recv_fd(int sock) {
    char byte;
    struct iovec iov = {&byte, 1};
    char control[CMSG_SPACE(sizeof(int))];
    struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = control, .msg_controllen = sizeof control};
    if (recvmsg(sock, &msg, 0) != 1) return -1;
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    if (!c || c->cmsg_type != SCM_RIGHTS) return -1;
    int fd;
    memcpy(&fd, CMSG_DATA(c), sizeof fd);
    return fd;
}

/* The epoll semantics R6e brought into the Linux server beyond the
 * basics: EPOLLEXCLUSIVE, instances shared by fork and passed with
 * SCM_RIGHTS, interests that belong to the description, files that take
 * no interest. */
static void more(int ep) {
    struct epoll_event ev;
    int p[2];
    pipe(p);
    int e = eventfd(0, 0);
    check("EPOLLEXCLUSIVE with EPOLLONESHOT is EINVAL", add(ep, e, EPOLLIN | EPOLLEXCLUSIVE | EPOLLONESHOT, 0) == -1 && errno == EINVAL);
    int other = epoll_create1(0);
    check("EPOLLEXCLUSIVE on an epoll instance is EINVAL", add(ep, other, EPOLLIN | EPOLLEXCLUSIVE, 0) == -1 && errno == EINVAL);
    check("EPOLLEXCLUSIVE is taken", add(ep, e, EPOLLIN | EPOLLEXCLUSIVE, 17) == 0);
    check("an exclusive interest cannot be modified", mod(ep, e, EPOLLIN, 17) == -1 && errno == EINVAL);
    check("EPOLL_CTL_MOD cannot make one exclusive", add(ep, p[0], EPOLLIN, 18) == 0 && mod(ep, p[0], EPOLLIN | EPOLLEXCLUSIVE, 18) == -1 && errno == EINVAL);
    uint64_t one = 1;
    write(e, &one, sizeof one);
    check("an exclusive interest reports", peek(ep, &ev) == 1 && ev.data.u64 == 17);
    epoll_ctl(ep, EPOLL_CTL_DEL, e, NULL);
    epoll_ctl(ep, EPOLL_CTL_DEL, p[0], NULL);
    close(e);
    close(other);
    int ex = woken_by_one_write(1), all = woken_by_one_write(0);
    printf("  (one write woke %d of two exclusive waiters, %d of two others)\n", ex, all);
    check("EPOLLEXCLUSIVE: one write wakes one of two waiting instances", ex == 1);
    check("without it, both", all == 2);

    /* An instance is a description: a forked child shares it, and an
     * interest added after the fork reaches the child's epoll_wait. */
    int shared = epoll_create1(0);
    pid_t child = fork();
    if (child == 0) {
        int n = epoll_wait(shared, &ev, 1, 2000);
        _exit(n == 1 && ev.data.u64 == 19 ? 0 : 1);
    }
    sleep_ms(50);
    add(shared, p[0], EPOLLIN, 19);
    write(p[1], "x", 1);
    int status;
    waitpid(child, &status, 0);
    check("a forked child shares the instance (interests added later)", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    char c;
    read(p[0], &c, 1);
    close(shared);

    /* An instance passed with SCM_RIGHTS: the receiver waits on it, the
     * sender's descriptor closed. */
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    int passed = epoll_create1(0);
    add(passed, p[0], EPOLLIN, 20);
    child = fork();
    if (child == 0) {
        int got = recv_fd(sv[1]);
        int n = got >= 0 ? epoll_wait(got, &ev, 1, 2000) : -1;
        _exit(n == 1 && ev.data.u64 == 20 ? 0 : 1);
    }
    check("an epoll instance is passed with SCM_RIGHTS", send_fd(sv[0], passed) == 0);
    close(passed);
    sleep_ms(50);
    write(p[1], "y", 1);
    waitpid(child, &status, 0);
    check("... and the receiver waits on it after the sender closed it", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    read(p[0], &c, 1);
    close(sv[0]);
    close(sv[1]);

    /* An interest belongs to (descriptor number, description): the same
     * number for another description is another interest. */
    int q[2];
    pipe(q);
    int num = dup(q[0]);
    check("an interest by number and description", add(ep, num, EPOLLIN, 21) == 0);
    dup2(p[0], num);
    check("the number names another description now: a new interest", add(ep, num, EPOLLIN, 22) == 0);
    write(q[1], "q", 1);
    check("the first interest lives with its description", peek(ep, &ev) == 1 && ev.data.u64 == 21);
    close(q[0]);
    close(q[1]);
    check("... and goes with it", peek(ep, NULL) == 0);
    epoll_ctl(ep, EPOLL_CTL_DEL, num, NULL);
    close(num);

    /* Files without readiness of their own. */
    int proc = open("/proc/self/stat", O_RDONLY);
    check("a file of the kernel's (/proc) is EPERM", add(ep, proc, EPOLLIN, 0) == -1 && errno == EPERM);
    close(proc);
    int opath = open("/tmp", O_PATH);
    check("an O_PATH descriptor is EBADF", add(ep, opath, EPOLLIN, 0) == -1 && errno == EBADF);
    check("epoll_wait on an O_PATH descriptor is EBADF", epoll_wait(opath, &ev, 1, 0) == -1 && errno == EBADF);
    close(opath);
    add(ep, p[0], EPOLLIN, 23);
    write(p[1], "z", 1);
    check("epoll_wait into memory it cannot write is EFAULT, the event kept",
          epoll_wait(ep, (struct epoll_event *)8, 1, 0) == -1 && errno == EFAULT && peek(ep, &ev) == 1 && ev.data.u64 == 23);
    epoll_ctl(ep, EPOLL_CTL_DEL, p[0], NULL);
    close(p[0]);
    close(p[1]);
}

int main(void) {
    int ep = epoll_create1(EPOLL_CLOEXEC);
    check("epoll_create1(EPOLL_CLOEXEC)", ep >= 0 && (fcntl(ep, F_GETFD) & FD_CLOEXEC));
    check("epoll_create rejects a size of 0", epoll_create(0) == -1 && errno == EINVAL);
    check("epoll_create1 rejects unknown flags", epoll_create1(1) == -1 && errno == EINVAL);

    int p[2];
    pipe(p);
    check("EPOLL_CTL_ADD a pipe", add(ep, p[0], EPOLLIN, 7) == 0);
    check("adding it twice is EEXIST", add(ep, p[0], EPOLLIN, 7) == -1 && errno == EEXIST);
    check("nothing is ready on an empty pipe", peek(ep, NULL) == 0);
    write(p[1], "ab", 2);
    struct epoll_event ev;
    check("a write makes it ready with its data",
          peek(ep, &ev) == 1 && ev.events == EPOLLIN && ev.data.u64 == 7);
    check("level-triggered: still ready until read", peek(ep, NULL) == 1);
    char buf[16];
    read(p[0], buf, sizeof buf);
    check("not ready once drained", peek(ep, NULL) == 0);

    check("EPOLL_CTL_MOD to edge-triggered", mod(ep, p[0], EPOLLIN | EPOLLET, 8) == 0);
    write(p[1], "a", 1);
    check("edge-triggered: reported once per write", peek(ep, &ev) == 1 && ev.data.u64 == 8 && peek(ep, NULL) == 0);
    write(p[1], "b", 1);
    check("edge-triggered: the next write is a new edge", peek(ep, NULL) == 1);
    read(p[0], buf, sizeof buf);

    check("EPOLL_CTL_MOD to one-shot", mod(ep, p[0], EPOLLIN | EPOLLONESHOT, 9) == 0);
    write(p[1], "a", 1);
    check("one-shot: reported once", peek(ep, NULL) == 1 && peek(ep, NULL) == 0);
    mod(ep, p[0], EPOLLIN | EPOLLONESHOT, 9);
    check("one-shot: armed again by EPOLL_CTL_MOD", peek(ep, NULL) == 1);
    read(p[0], buf, sizeof buf);

    check("EPOLL_CTL_DEL", epoll_ctl(ep, EPOLL_CTL_DEL, p[0], NULL) == 0);
    write(p[1], "a", 1);
    check("a removed file is not reported", peek(ep, NULL) == 0);
    check("removing it again is ENOENT", epoll_ctl(ep, EPOLL_CTL_DEL, p[0], NULL) == -1 && errno == ENOENT);
    check("modifying a file not added is ENOENT", mod(ep, p[0], EPOLLIN, 0) == -1 && errno == ENOENT);
    read(p[0], buf, sizeof buf);

    int file = open("/etc/runtests.sh", O_RDONLY);
    check("a regular file is EPERM", add(ep, file, EPOLLIN, 0) == -1 && errno == EPERM);
    close(file);
    check("an epoll instance cannot watch itself", add(ep, ep, EPOLLIN, 0) == -1 && errno == EINVAL);
    check("a descriptor that is not open is EBADF", add(ep, 999, EPOLLIN, 0) == -1 && errno == EBADF);
    check("epoll_wait on a pipe is EINVAL", epoll_wait(p[0], &ev, 1, 0) == -1 && errno == EINVAL);
    check("maxevents of 0 is EINVAL", epoll_wait(ep, &ev, 0, 0) == -1 && errno == EINVAL);

    /* A pipe's write end: writable, an error once the reader is gone. */
    int q[2];
    pipe(q);
    add(ep, q[1], EPOLLOUT, 10);
    check("an empty pipe's write end is writable", peek(ep, &ev) == 1 && ev.events == EPOLLOUT && ev.data.u64 == 10);
    close(q[0]);
    check("a write end without readers reports EPOLLERR", peek(ep, &ev) == 1 && (ev.events & EPOLLERR));
    close(q[1]);
    check("closing the only descriptor removes the file", peek(ep, NULL) == 0);

    /* The interest belongs to the open file, not the descriptor number. */
    pipe(q);
    int dup_fd = dup(q[0]);
    add(ep, q[0], EPOLLIN, 11);
    close(q[0]);
    write(q[1], "a", 1);
    check("a file with a descriptor left stays watched", peek(ep, &ev) == 1 && ev.data.u64 == 11);
    close(dup_fd);
    check("closing its last descriptor removes it", peek(ep, NULL) == 0);
    close(q[1]);

    /* More ready files than maxevents: the rest come next time. */
    int r[3][2];
    for (int i = 0; i < 3; i++) {
        pipe(r[i]);
        add(ep, r[i][0], EPOLLIN, 100 + i);
        write(r[i][1], "x", 1);
    }
    struct epoll_event two[2];
    int seen = 0;
    for (int round = 0; round < 2; round++) {
        int n = epoll_wait(ep, two, 2, 0);
        for (int i = 0; i < n; i++) {
            seen |= 1 << (two[i].data.u64 - 100);
        }
    }
    check("maxevents limits a call; level-triggered files rotate", seen == 7);
    for (int i = 0; i < 3; i++) {
        close(r[i][0]);
        close(r[i][1]);
    }

    /* eventfd in an epoll set. */
    int efd = eventfd(0, EFD_NONBLOCK);
    add(ep, efd, EPOLLIN, 12);
    check("an idle eventfd is not ready", peek(ep, NULL) == 0);
    uint64_t one = 1;
    write(efd, &one, sizeof one);
    check("an eventfd write makes it ready", peek(ep, &ev) == 1 && ev.data.u64 == 12);
    read(efd, &one, sizeof one);

    /* Nested: an epoll instance is itself pollable. */
    int inner = epoll_create1(0), outer = epoll_create1(0);
    pipe(q);
    add(inner, q[0], EPOLLIN, 1);
    check("an epoll instance can watch another", add(outer, inner, EPOLLIN, 13) == 0);
    check("a loop of epoll instances is ELOOP", add(inner, outer, EPOLLIN, 0) == -1 && errno == ELOOP);
    check("nothing ready inside, nothing outside", peek(outer, NULL) == 0);
    write(q[1], "a", 1);
    check("readiness inside reaches the outer instance", peek(outer, &ev) == 1 && ev.data.u64 == 13 && ev.events == EPOLLIN);
    struct pollfd pf = {inner, POLLIN, 0};
    check("poll reports an epoll instance with ready files", poll(&pf, 1, 0) == 1 && pf.revents == POLLIN);
    read(q[0], buf, sizeof buf);
    pf.revents = 0;
    check("and not once they are drained", poll(&pf, 1, 0) == 0);
    close(q[0]);
    close(q[1]);
    close(inner);
    close(outer);

    /* Chains of instances stay short however they are built: adding a
     * new outermost instance each time, or a new innermost one. */
    int up[8], down[8], up_fail = 0, down_fail = 0;
    up[0] = epoll_create1(0);
    down[0] = epoll_create1(0);
    for (int i = 1; i < 8; i++) {
        up[i] = epoll_create1(0);
        down[i] = epoll_create1(0);
        if (!up_fail && add(up[i], up[i - 1], EPOLLIN, 0) == -1 && errno == ELOOP) up_fail = i;
        if (!down_fail && add(down[i - 1], down[i], EPOLLIN, 0) == -1 && errno == ELOOP) down_fail = i;
    }
    printf("  (a chain of %d instances is refused, built upwards; %d, downwards)\n", up_fail + 1, down_fail + 1);
    check("nesting is limited when built upwards", up_fail == 5);
    check("nesting is limited when built downwards", down_fail == 5);
    for (int i = 0; i < 8; i++) {
        close(up[i]);
        close(down[i]);
    }

    /* Wide nesting is accepted. The nesting checks visit each instance
     * once, not every path (here 125000 paths over about 5000 interests);
     * the time is printed, not checked, as it depends on the machine. */
    enum { WIDTH = 50, LAYERS = 4 };
    static int layer[LAYERS][WIDTH];
    int wide_ok = 1;
    for (int l = LAYERS - 1; l >= 0; l--) {
        for (int i = 0; i < WIDTH; i++) {
            layer[l][i] = epoll_create1(0);
            for (int j = 0; l + 1 < LAYERS && j < WIDTH; j++) wide_ok &= add(layer[l][i], layer[l + 1][j], EPOLLIN, 0) == 0;
        }
    }
    int top = epoll_create1(0);
    int64_t built = now_ns();
    for (int i = 0; i < WIDTH; i++) wide_ok &= add(top, layer[0][i], EPOLLIN, 0) == 0;
    built = (now_ns() - built) / 1000;
    printf("  (watching %d instances over %d nested layers took %lld us)\n", WIDTH, LAYERS, (long long)built);
    check("three fully connected layers of 50 instances can be watched", wide_ok);
    close(top);
    for (int l = 0; l < LAYERS; l++)
        for (int i = 0; i < WIDTH; i++) close(layer[l][i]);

    /* A wakeup racing with EPOLL_CTL_DEL on another CPU must not leave
     * the removed interest on the ready list. */
    int race[2];
    pipe(race);
    fcntl(race[0], F_SETFL, O_NONBLOCK);
    pid_t hammer = fork();
    if (hammer == 0) {
        cpu_set_t one;
        CPU_ZERO(&one);
        CPU_SET(1, &one);
        sched_setaffinity(0, sizeof one, &one);
        for (;;) {
            write(race[1], "x", 1);
            read(race[0], buf, 1);
        }
    }
    int stale = 0;
    for (int i = 0; i < 20000 && !stale; i++) {
        add(ep, race[0], EPOLLIN, 16);
        epoll_ctl(ep, EPOLL_CTL_DEL, race[0], NULL);
        stale = peek(ep, NULL) != 0;
    }
    kill(hammer, SIGKILL);
    waitpid(hammer, NULL, 0);
    close(race[0]);
    close(race[1]);
    check("a removed interest is never reported", !stale);

    /* epoll_wait sleeps until a write and wakes right after it. */
    pipe(q);
    add(ep, q[0], EPOLLIN, 14);
    pid_t child = fork();
    if (child == 0) {
        struct timespec d = {0, 2000000};
        nanosleep(&d, NULL);
        int64_t t = now_ns();
        write(q[1], &t, sizeof t);
        _exit(0);
    }
    int n = epoll_wait(ep, &ev, 1, 5000);
    int64_t woke = now_ns(), written = 0;
    read(q[0], &written, sizeof written);
    waitpid(child, NULL, 0);
    printf("  (epoll_wait woke %lld us after the write)\n", (long long)(woke - written) / 1000);
    check("epoll_wait wakes within 1 ms of a write", n == 1 && ev.data.u64 == 14 && woke - written < 1000000);

    int64_t t0 = now_ns();
    n = epoll_wait(ep, &ev, 1, 20);
    int64_t waited = (now_ns() - t0) / 1000;
    check("the timeout ends an idle epoll_wait", n == 0 && waited >= 20000 && waited < 30000);

    /* A socket: its readiness lives in netd, so epoll asks again. */
    int rx = socket(AF_INET, SOCK_DGRAM, 0), tx = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in addr = {.sin_family = AF_INET, .sin_port = htons(47123), .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    bind(rx, (struct sockaddr *)&addr, sizeof addr);
    add(ep, rx, EPOLLIN, 15);
    check("an idle socket is not ready", peek(ep, NULL) == 0);
    sendto(tx, "ping", 4, 0, (struct sockaddr *)&addr, sizeof addr);
    t0 = now_ns();
    n = epoll_wait(ep, &ev, 1, 1000);
    waited = (now_ns() - t0) / 1000;
    check("a datagram makes a socket ready", n == 1 && ev.data.u64 == 15 && ev.events == EPOLLIN && waited < 100000);
    recv(rx, buf, sizeof buf, 0);
    check("not ready once it is read", peek(ep, NULL) == 0);
    close(rx);
    close(tx);

    /* Signals: EINTR, and epoll_pwait's temporary mask. */
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sigaction(SIGUSR1, &sa, NULL);
    sigset_t block, open_all;
    sigemptyset(&block);
    sigaddset(&block, SIGUSR1);
    sigemptyset(&open_all);
    sigprocmask(SIG_BLOCK, &block, NULL);
    raise(SIGUSR1);
    check("epoll_wait leaves a blocked signal pending", epoll_wait(ep, &ev, 1, 10) == 0);
    n = epoll_pwait(ep, &ev, 1, 1000, &open_all);
    check("epoll_pwait's mask lets it interrupt (EINTR)", n == -1 && errno == EINTR);
    sigset_t now;
    sigprocmask(SIG_BLOCK, NULL, &now);
    check("epoll_pwait restores the caller's mask", sigismember(&now, SIGUSR1));
    struct timespec ten_ms = {0, 10000000};
    t0 = now_ns();
    /* musl has no wrapper for it yet. */
    n = syscall(SYS_epoll_pwait2, ep, &ev, 1, &ten_ms, NULL, 8);
    waited = (now_ns() - t0) / 1000;
    check("epoll_pwait2 takes a timespec timeout", n == 0 && waited >= 10000 && waited < 20000);
    sigprocmask(SIG_UNBLOCK, &block, NULL);

    more(ep);

    printf("epolltest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
