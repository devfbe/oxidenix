/* Signal-driven I/O (O_ASYNC, ADR 0023): F_SETFL and FIOASYNC, F_SETOWN, F_SETOWN_EX and
 * the sockets' FIOSETOWN, F_SETSIG with its siginfo (si_code POLL_IN .. POLL_HUP, si_band,
 * si_fd), the queue of a real-time signal and its fallback to SIGIO, and the events of
 * pipes, AF_UNIX socket pairs, TCP over the loopback, pseudo-terminals and inotify, to a
 * thread, a process and a process group. Every signal is taken with sigtimedwait (blocked),
 * so its siginfo is checked exactly. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-62s %s\n", name, ok ? "ok" : "FAIL");
    fflush(stdout);
    if (!ok) failures++;
}

#define RT (SIGRTMIN + 1)

/* Takes `sig` (blocked) if it comes within `ms`, its siginfo in `si`: whether it came. */
static int took(int sig, int ms, siginfo_t *si) {
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, sig);
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    siginfo_t spare;
    return sigtimedwait(&s, si ? si : &spare, &ts) == sig;
}

/* Takes every instance of `sig` pending now: how many. */
static int drain(int sig) {
    int n = 0;
    while (took(sig, 0, NULL)) n++;
    return n;
}

/* Takes instances of RT for up to `ms` until one for `fd` with `code` came: whether it did. */
static int took_for(int fd, int code, int ms) {
    siginfo_t si;
    while (took(RT, ms, &si)) {
        if (si.si_fd == fd && si.si_code == code) return 1;
    }
    return 0;
}

/* Signal-driven I/O on `fd` to this process, with signal `sig` (0: SIGIO). */
static void async_on(int fd, int sig) {
    fcntl(fd, F_SETOWN, getpid());
    fcntl(fd, F_SETSIG, sig);
    fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_ASYNC);
}

static int is_info(const siginfo_t *si, int fd, int code, long band) {
    return si->si_fd == fd && si->si_code == code && si->si_band == band;
}

static void basics(void) {
    int p[2];
    pipe2(p, O_NONBLOCK);
    check("F_GETSIG is 0 at first", fcntl(p[0], F_GETSIG) == 0);
    check("F_SETSIG 65 is EINVAL", fcntl(p[0], F_SETSIG, 65) == -1 && errno == EINVAL);
    check("F_SETSIG and F_GETSIG", fcntl(p[0], F_SETSIG, RT) == 0 && fcntl(p[0], F_GETSIG) == RT);
    fcntl(p[0], F_SETFL, fcntl(p[0], F_GETFL) | O_ASYNC);
    check("F_SETFL O_ASYNC shows in F_GETFL", fcntl(p[0], F_GETFL) & O_ASYNC);
    fcntl(p[0], F_SETFL, fcntl(p[0], F_GETFL) & ~O_ASYNC);
    check("and goes again", !(fcntl(p[0], F_GETFL) & O_ASYNC));
    int on = 1;
    check("FIOASYNC on a pipe", ioctl(p[1], FIOASYNC, &on) == 0 && (fcntl(p[1], F_GETFL) & O_ASYNC));
    int pid = getpid();
    check("FIOSETOWN on a pipe is ENOTTY", ioctl(p[0], FIOSETOWN, &pid) == -1 && errno == ENOTTY);
    check("F_SETOWN of nobody is ESRCH", fcntl(p[0], F_SETOWN, 999999) == -1 && errno == ESRCH);
    close(p[0]);
    close(p[1]);

    int e = eventfd(0, EFD_NONBLOCK);
    check("FIOASYNC on an eventfd is ENOTTY", ioctl(e, FIOASYNC, &on) == -1 && errno == ENOTTY);
    async_on(e, 0);
    check("F_SETFL O_ASYNC on an eventfd leaves the flag off", !(fcntl(e, F_GETFL) & O_ASYNC));
    uint64_t one = 1;
    write(e, &one, sizeof one);
    check("but an eventfd signals nothing", drain(SIGIO) == 0);
    close(e);
}

static void pipes(void) {
    int p[2];
    char buf[64];
    siginfo_t si;
    pipe2(p, O_NONBLOCK);
    async_on(p[0], 0);
    write(p[1], "a", 1);
    check("pipe: a write signals the reader SIGIO", took(SIGIO, 1000, &si) && si.si_code == SI_KERNEL);
    check("pipe: once", drain(SIGIO) == 0);
    read(p[0], buf, sizeof buf);

    fcntl(p[0], F_SETSIG, RT);
    for (int i = 0; i < 3; i++) write(p[1], "b", 1);
    int n = 0, right = 1;
    while (took(RT, 0, &si)) {
        n++;
        right &= is_info(&si, p[0], POLL_IN, POLLIN | POLLRDNORM);
    }
    check("pipe: F_SETSIG queues one per write", n == 3);
    check("pipe: si_code POLL_IN, si_band, si_fd", right);

    async_on(p[1], RT);
    read(p[0], buf, sizeof buf);
    check("pipe: a read signals the writer POLL_OUT", took(RT, 1000, &si) && is_info(&si, p[1], POLL_OUT, POLLOUT | POLLWRNORM | POLLWRBAND));
    read(p[0], buf, sizeof buf);
    check("pipe: even one that found nothing (as Linux's pipe_read)", took(RT, 1000, &si) && si.si_fd == p[1] && si.si_code == POLL_OUT);
    drain(RT);
    close(p[1]);
    check("pipe: the last writer's close signals the reader", took(RT, 1000, &si) && is_info(&si, p[0], POLL_IN, POLLIN | POLLRDNORM));
    close(p[0]);

    /* F_SETSIG with a signal of its own codes: SI_SIGIO. */
    sigset_t chld;
    sigemptyset(&chld);
    sigaddset(&chld, SIGCHLD);
    sigprocmask(SIG_BLOCK, &chld, NULL);
    pipe2(p, O_NONBLOCK);
    async_on(p[0], SIGCHLD);
    write(p[1], "c", 1);
    check("pipe: F_SETSIG SIGCHLD has si_code SI_SIGIO", took(SIGCHLD, 1000, &si) && si.si_code == SI_SIGIO && si.si_fd == p[0]);
    close(p[0]);
    close(p[1]);
    sigprocmask(SIG_UNBLOCK, &chld, NULL);
}

/* A real-time signal that cannot be queued (RLIMIT_SIGPENDING) falls back to SIGIO. */
static void overflow(void) {
    pid_t c = fork();
    if (c == 0) {
        struct rlimit one = {1, 1};
        setrlimit(RLIMIT_SIGPENDING, &one);
        int p[2];
        pipe2(p, O_NONBLOCK);
        async_on(p[0], RT);
        for (int i = 0; i < 3; i++) write(p[1], "x", 1);
        int rt = drain(RT), io = drain(SIGIO);
        _exit(rt == 1 && io == 1 ? 0 : 1);
    }
    int status;
    waitpid(c, &status, 0);
    check("overflow: one queued, then SIGIO instead", WIFEXITED(status) && WEXITSTATUS(status) == 0);
}

struct thread_arg {
    volatile pid_t tid;
    int got, fd;
};

static void *taker(void *a) {
    struct thread_arg *t = a;
    t->tid = syscall(SYS_gettid);
    siginfo_t si;
    t->got = took(RT, 3000, &si) && si.si_fd == t->fd && si.si_code == POLL_IN;
    return NULL;
}

static void thread_owner(void) {
    int p[2];
    pipe2(p, O_NONBLOCK);
    struct thread_arg arg = {0, 0, p[0]};
    pthread_t t;
    pthread_create(&t, NULL, taker, &arg);
    while (arg.tid == 0) sched_yield();
    struct f_owner_ex ex = {F_OWNER_TID, arg.tid}, back;
    check("F_SETOWN_EX F_OWNER_TID", fcntl(p[0], F_SETOWN_EX, &ex) == 0);
    check("F_GETOWN_EX gives it back", fcntl(p[0], F_GETOWN_EX, &back) == 0 && back.type == F_OWNER_TID && back.pid == arg.tid);
    fcntl(p[0], F_SETSIG, RT);
    fcntl(p[0], F_SETFL, fcntl(p[0], F_GETFL) | O_ASYNC);
    write(p[1], "t", 1);
    pthread_join(t, NULL);
    check("thread owner: that thread takes the signal", arg.got);
    check("thread owner: not the process", drain(RT) == 0);
    close(p[0]);
    close(p[1]);
}

static void group_owner(void) {
    int ready[2], p[2];
    pipe(ready);
    pipe2(p, O_NONBLOCK);
    pid_t kids[2];
    for (int i = 0; i < 2; i++) {
        kids[i] = fork();
        if (kids[i] == 0) {
            setpgid(0, i == 0 ? 0 : kids[0]);
            write(ready[1], "r", 1);
            siginfo_t si;
            _exit(took(SIGIO, 3000, &si) ? 0 : 1);
        }
        setpgid(kids[i], kids[0]);
    }
    char c;
    read(ready[0], &c, 1);
    read(ready[0], &c, 1);
    check("F_SETOWN of a process group", fcntl(p[0], F_SETOWN, -kids[0]) == 0 && fcntl(p[0], F_GETOWN) == -kids[0]);
    struct f_owner_ex back;
    check("F_GETOWN_EX: F_OWNER_PGRP", fcntl(p[0], F_GETOWN_EX, &back) == 0 && back.type == F_OWNER_PGRP && back.pid == kids[0]);
    fcntl(p[0], F_SETFL, fcntl(p[0], F_GETFL) | O_ASYNC);
    write(p[1], "g", 1);
    int got = 0;
    for (int i = 0; i < 2; i++) {
        int status;
        waitpid(kids[i], &status, 0);
        got += WIFEXITED(status) && WEXITSTATUS(status) == 0;
    }
    check("group owner: each process of the group gets SIGIO", got == 2);
    check("group owner: no one else", drain(SIGIO) == 0);
    check("F_GETOWN is 0 once the group is gone", fcntl(p[0], F_GETOWN) == 0);
    write(p[1], "g", 1);
    check("a write signals nobody once the group is gone", drain(SIGIO) == 0);
    close(p[0]);
    close(p[1]);
    close(ready[0]);
    close(ready[1]);

    /* An owner process reaped: its pid names nobody for the file any more (Linux holds the
     * owner's struct pid; a process given the pid again would not be it). */
    pipe2(p, O_NONBLOCK);
    pid_t k = fork();
    if (k == 0) {
        pause();
        _exit(0);
    }
    fcntl(p[0], F_SETOWN, k);
    fcntl(p[0], F_SETFL, fcntl(p[0], F_GETFL) | O_ASYNC);
    check("F_GETOWN of a process", fcntl(p[0], F_GETOWN) == k);
    kill(k, SIGKILL);
    waitpid(k, NULL, 0);
    check("F_GETOWN is 0 once it is reaped", fcntl(p[0], F_GETOWN) == 0);
    struct f_owner_ex gone;
    check("F_GETOWN_EX: its kind, id 0", fcntl(p[0], F_GETOWN_EX, &gone) == 0 && gone.type == F_OWNER_PID && gone.pid == 0);
    write(p[1], "k", 1);
    check("a write signals nobody once the owner is reaped", drain(SIGIO) == 0);
    close(p[0]);
    close(p[1]);
}

static void socket_pairs(void) {
    int sv[2];
    char buf[4096];
    siginfo_t si;
    socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, sv);
    int pid = getpid(), back = 0;
    check("FIOSETOWN and SIOCGPGRP on a socket", ioctl(sv[0], FIOSETOWN, &pid) == 0 && ioctl(sv[0], SIOCGPGRP, &back) == 0 && back == pid);
    async_on(sv[0], RT);
    write(sv[1], "s", 1);
    check("socketpair: data signals POLL_IN", took(RT, 1000, &si) && is_info(&si, sv[0], POLL_IN, POLLIN | POLLRDNORM));
    read(sv[0], buf, sizeof buf);
    write(sv[0], "x", 1);
    read(sv[1], buf, sizeof buf);
    check("socketpair: room without a send that found none: nothing", drain(RT) == 0);
    size_t sent = 0;
    while (write(sv[0], buf, sizeof buf) > 0) sent += sizeof buf;
    check("socketpair: filled up", errno == EAGAIN && sent > 0);
    while (read(sv[1], buf, sizeof buf) > 0) {
    }
    int outs = 0;
    while (took(RT, 200, &si)) outs += is_info(&si, sv[0], POLL_OUT, POLLOUT | POLLWRNORM | POLLWRBAND);
    check("socketpair: room after EAGAIN signals POLL_OUT once", outs == 1);
    shutdown(sv[1], SHUT_WR);
    check("socketpair: the peer's SHUT_WR signals POLL_IN", took(RT, 1000, &si) && is_info(&si, sv[0], POLL_IN, POLLIN | POLLRDNORM));
    close(sv[1]);
    check("socketpair: the peer's close signals POLL_HUP", took(RT, 1000, &si) && is_info(&si, sv[0], POLL_HUP, POLLHUP | POLLERR));
    close(sv[0]);

    socketpair(AF_UNIX, SOCK_DGRAM, 0, sv);
    async_on(sv[1], RT);
    send(sv[0], "d", 1, 0);
    check("datagram pair: a datagram signals POLL_IN", took(RT, 1000, &si) && is_info(&si, sv[1], POLL_IN, POLLIN | POLLRDNORM));
    close(sv[0]);
    close(sv[1]);
}

static void tcp(void) {
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    socklen_t len = sizeof a;
    bind(l, (struct sockaddr *)&a, sizeof a);
    listen(l, 4);
    getsockname(l, (struct sockaddr *)&a, &len);
    async_on(l, RT);
    int c = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    async_on(c, RT);
    int r = connect(c, (struct sockaddr *)&a, sizeof a);
    check("tcp: a non-blocking connect", r == 0 || errno == EINPROGRESS);
    int listener = 0, connected = 0;
    siginfo_t si;
    while ((!listener || !connected) && took(RT, 3000, &si)) {
        listener |= si.si_fd == l && si.si_code == POLL_IN;
        connected |= si.si_fd == c && si.si_code == POLL_OUT;
    }
    check("tcp: the listener's connection signals POLL_IN", listener);
    check("tcp: the connection made signals POLL_OUT", connected);
    int s = accept(l, NULL, NULL);
    async_on(s, RT);
    drain(RT);
    send(c, "hello", 5, 0);
    check("tcp: data signals POLL_IN", took_for(s, POLL_IN, 3000));
    char buf[16];
    struct pollfd pf = {s, POLLIN, 0};
    poll(&pf, 1, 1000);
    check("tcp: the data is there", recv(s, buf, sizeof buf, MSG_DONTWAIT) == 5);
    drain(RT);
    close(c);
    check("tcp: the peer's FIN signals POLL_IN", took_for(s, POLL_IN, 3000));
    close(s);
    close(l);
    drain(RT);
}

static void ptys(void) {
    int m = posix_openpt(O_RDWR | O_NOCTTY | O_NONBLOCK);
    grantpt(m);
    unlockpt(m);
    int s = open(ptsname(m), O_RDWR | O_NOCTTY | O_NONBLOCK);
    check("pty: opened", m >= 0 && s >= 0);
    check("pty: no owner at first", fcntl(m, F_GETOWN) == 0);
    fcntl(m, F_SETSIG, RT);
    fcntl(m, F_SETFL, fcntl(m, F_GETFL) | O_ASYNC);
    check("pty: O_ASYNC makes the caller the master's owner", fcntl(m, F_GETOWN) == getpid());
    write(s, "x", 1);
    check("pty: the slave's output signals the master POLL_IN", took_for(m, POLL_IN, 1000));
    char buf[64];
    read(m, buf, sizeof buf);
    drain(RT);
    fcntl(s, F_SETSIG, RT);
    fcntl(s, F_SETFL, fcntl(s, F_GETFL) | O_ASYNC);
    check("pty: and the slave's", fcntl(s, F_GETOWN) == getpid());
    write(m, "ab", 2);
    siginfo_t si;
    int slave = 0;
    while (took(RT, 200, &si)) slave += si.si_fd == s;
    check("pty: canonical input without a line's end: nothing", slave == 0);
    write(m, "\n", 1);
    check("pty: a line's end signals the slave POLL_IN", took_for(s, POLL_IN, 1000));
    read(s, buf, sizeof buf);
    drain(RT);
    close(m);
    check("pty: the hangup takes O_ASYNC", !(fcntl(s, F_GETFL) & O_ASYNC));
    check("pty: and a hung-up slave refuses it (ENOTTY)", fcntl(s, F_SETFL, fcntl(s, F_GETFL) | O_ASYNC) == -1 && errno == ENOTTY);
    close(s);
}

static void inotify(void) {
    int i = inotify_init1(IN_NONBLOCK);
    mkdir("/tmp/sigiotest", 0700);
    inotify_add_watch(i, "/tmp/sigiotest", IN_CREATE);
    async_on(i, RT);
    close(open("/tmp/sigiotest/f", O_CREAT | O_WRONLY, 0600));
    siginfo_t si;
    check("inotify: an event signals POLL_IN", took(RT, 1000, &si) && is_info(&si, i, POLL_IN, POLLIN | POLLRDNORM));
    unlink("/tmp/sigiotest/f");
    rmdir("/tmp/sigiotest");
    close(i);
    drain(RT);
}

int main(void) {
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGIO);
    sigaddset(&s, RT);
    sigprocmask(SIG_BLOCK, &s, NULL);
    basics();
    pipes();
    overflow();
    thread_owner();
    group_owner();
    socket_pairs();
    tcp();
    ptys();
    inotify();
    printf("sigiotest: %d failed\n", failures);
    return failures ? 1 : 0;
}
