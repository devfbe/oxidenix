/* pidfds (the Linux server's): pidfd_open, clone's and clone3's CLONE_PIDFD,
 * pidfd_send_signal (as kill and as rt_sigqueueinfo), pidfd_getfd, waitid(P_PIDFD) with
 * PIDFD_NONBLOCK, readiness for poll and epoll when the process exits (POLLIN) and is
 * reaped (POLLHUP), and the errors. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif
#ifndef SYS_pidfd_send_signal
#define SYS_pidfd_send_signal 424
#endif
#ifndef SYS_pidfd_getfd
#define SYS_pidfd_getfd 438
#endif
#ifndef SYS_clone3
#define SYS_clone3 435
#endif
#ifndef CLONE_PIDFD
#define CLONE_PIDFD 0x1000
#endif
#ifndef P_PIDFD
#define P_PIDFD 3
#endif
#define PIDFD_NONBLOCK O_NONBLOCK

struct clone_args {
    uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls;
};

static int failures;

static void check(const char *name, int ok) {
    printf("%-58s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static int pidfd_open(pid_t pid, unsigned flags) { return (int)syscall(SYS_pidfd_open, pid, flags); }
static int pidfd_send_signal(int fd, int sig, siginfo_t *info, unsigned flags) { return (int)syscall(SYS_pidfd_send_signal, fd, sig, info, flags); }
static int pidfd_getfd(int fd, int target, unsigned flags) { return (int)syscall(SYS_pidfd_getfd, fd, target, flags); }

static void sleep_ms(long ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000};
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR) {
    }
}

static short revents(int fd, int timeout_ms) {
    struct pollfd p = {fd, POLLIN, 0};
    return poll(&p, 1, timeout_ms) == 1 ? p.revents : 0;
}

/* A child that waits for SIGUSR1 (or a kill) with a pipe to report on. */
static volatile sig_atomic_t usr1_got, usr1_code, usr1_value, usr1_pid;

static void on_usr1(int sig, siginfo_t *si, void *uc) {
    (void)sig;
    (void)uc;
    usr1_code = si->si_code;
    usr1_got = 1;
    usr1_value = si->si_value.sival_int;
    usr1_pid = si->si_pid;
}

static void basics(void) {
    int report[2];
    pipe(report);
    pid_t child = fork();
    if (child == 0) {
        struct sigaction sa;
        memset(&sa, 0, sizeof sa);
        sa.sa_sigaction = on_usr1;
        sa.sa_flags = SA_SIGINFO;
        sigaction(SIGUSR1, &sa, NULL);
        /* Taken only in sigsuspend: none comes between a look and the wait. */
        sigset_t usr1, open;
        sigemptyset(&usr1);
        sigaddset(&usr1, SIGUSR1);
        sigprocmask(SIG_BLOCK, &usr1, &open);
        write(report[1], "r", 1);
        for (int i = 0; i < 2; i++) {
            while (!usr1_got) sigsuspend(&open);
            int got[3] = {usr1_code, usr1_value, usr1_pid};
            write(report[1], got, sizeof got);
            usr1_got = 0;
        }
        pause();
        _exit(0);
    }
    char c;
    read(report[0], &c, 1);
    int fd = pidfd_open(child, 0);
    check("pidfd_open of a child", fd >= 0);
    check("... is close-on-exec", (fcntl(fd, F_GETFD) & FD_CLOEXEC) != 0);
    check("... and not readable while it runs", revents(fd, 0) == 0);
    check("pidfd_send_signal(0) of a live process", pidfd_send_signal(fd, 0, NULL, 0) == 0);

    int got[3];
    check("pidfd_send_signal sends SIGUSR1", pidfd_send_signal(fd, SIGUSR1, NULL, 0) == 0);
    read(report[0], got, sizeof got);
    check("... as kill(2): SI_USER and the sender's pid", got[0] == SI_USER && got[2] == getpid());
    siginfo_t info;
    memset(&info, 0, sizeof info);
    info.si_signo = SIGUSR1;
    info.si_code = SI_QUEUE;
    info.si_value.sival_int = 42;
    check("pidfd_send_signal with a siginfo", pidfd_send_signal(fd, SIGUSR1, &info, 0) == 0);
    read(report[0], got, sizeof got);
    check("... as rt_sigqueueinfo: SI_QUEUE and the value", got[0] == SI_QUEUE && got[1] == 42);

    /* Errors of pidfd_send_signal. */
    errno = 0;
    check("... with unknown flags is EINVAL", pidfd_send_signal(fd, SIGUSR1, NULL, 8) < 0 && errno == EINVAL);
    errno = 0;
    check("... of signal 65 is EINVAL", pidfd_send_signal(fd, 65, NULL, 0) < 0 && errno == EINVAL);
    info.si_signo = SIGUSR2;
    errno = 0;
    check("... with a siginfo of another signal is EINVAL", pidfd_send_signal(fd, SIGUSR1, &info, 0) < 0 && errno == EINVAL);
    info.si_signo = SIGUSR1;
    info.si_code = SI_USER;
    errno = 0;
    check("... claiming kill's code to another process is EPERM", pidfd_send_signal(fd, SIGUSR1, &info, 0) < 0 && errno == EPERM);
    errno = 0;
    check("... on a pipe is EBADF", pidfd_send_signal(report[0], SIGUSR1, NULL, 0) < 0 && errno == EBADF);

    /* Its end: readable (POLLIN) as a zombie, hung up (POLLHUP) once reaped. */
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN, .data.fd = fd}, out;
    epoll_ctl(ep, EPOLL_CTL_ADD, fd, &ev);
    check("epoll: nothing while it runs", epoll_wait(ep, &out, 1, 0) == 0);
    kill(child, SIGTERM);
    check("epoll: EPOLLIN when it ends", epoll_wait(ep, &out, 1, 5000) == 1 && (out.events & EPOLLIN));
    short re = revents(fd, 5000);
    check("poll: POLLIN, not POLLHUP, while it is a zombie", (re & POLLIN) && !(re & POLLHUP));
    check("pidfd_send_signal to a zombie is 0", pidfd_send_signal(fd, SIGUSR1, NULL, 0) == 0);
    errno = 0;
    check("pidfd_getfd of a zombie is EBADF", pidfd_getfd(fd, 0, 0) < 0 && errno == EBADF);
    siginfo_t si;
    memset(&si, 0, sizeof si);
    check("waitid(P_PIDFD) reaps it", waitid(P_PIDFD, fd, &si, WEXITED) == 0 && si.si_pid == child && si.si_code == CLD_KILLED && si.si_status == SIGTERM);
    check("poll: POLLHUP once it is reaped", (revents(fd, 0) & (POLLIN | POLLHUP)) == (POLLIN | POLLHUP));
    errno = 0;
    check("pidfd_send_signal after it was reaped is ESRCH", pidfd_send_signal(fd, 0, NULL, 0) < 0 && errno == ESRCH);
    errno = 0;
    check("waitid(P_PIDFD) of a reaped process is ECHILD", waitid(P_PIDFD, fd, &si, WEXITED) < 0 && errno == ECHILD);
    errno = 0;
    check("pidfd_getfd after it was reaped is ESRCH", pidfd_getfd(fd, 0, 0) < 0 && errno == ESRCH);
    char buf[8];
    errno = 0;
    check("reading a pidfd is EINVAL", read(fd, buf, sizeof buf) < 0 && errno == EINVAL);

    char path[64], link[64] = {0};
    snprintf(path, sizeof path, "/proc/self/fd/%d", fd);
    readlink(path, link, sizeof link - 1);
    check("/proc/self/fd shows anon_inode:[pidfd]", strcmp(link, "anon_inode:[pidfd]") == 0);
    close(ep);
    close(fd);
    close(report[0]);
    close(report[1]);
}

static void open_errors(void) {
    errno = 0;
    check("pidfd_open(0) is EINVAL", pidfd_open(0, 0) < 0 && errno == EINVAL);
    errno = 0;
    check("pidfd_open(-1) is EINVAL", pidfd_open(-1, 0) < 0 && errno == EINVAL);
    errno = 0;
    check("pidfd_open with unknown flags is EINVAL", pidfd_open(getpid(), 1) < 0 && errno == EINVAL);
    errno = 0;
    check("pidfd_open of no process is ESRCH", pidfd_open(30000, 0) < 0 && errno == ESRCH);
    int self = pidfd_open(getpid(), 0);
    siginfo_t si;
    errno = 0;
    check("waitid(P_PIDFD) of a process not a child is ECHILD", waitid(P_PIDFD, self, &si, WEXITED | WNOHANG) < 0 && errno == ECHILD);
    int pipefd[2];
    pipe(pipefd);
    errno = 0;
    check("waitid(P_PIDFD) of a pipe is EBADF", waitid(P_PIDFD, pipefd[0], &si, WEXITED) < 0 && errno == EBADF);
    errno = 0;
    check("waitid(P_PIDFD) of no descriptor is EBADF", waitid(P_PIDFD, 999, &si, WEXITED) < 0 && errno == EBADF);
    close(pipefd[0]);
    close(pipefd[1]);
    close(self);
}

static void nonblock(void) {
    int go[2];
    pipe(go);
    pid_t child = fork();
    if (child == 0) {
        char c;
        read(go[0], &c, 1);
        _exit(9);
    }
    int fd = pidfd_open(child, PIDFD_NONBLOCK);
    check("pidfd_open(PIDFD_NONBLOCK)", fd >= 0 && (fcntl(fd, F_GETFL) & O_NONBLOCK));
    siginfo_t si;
    errno = 0;
    check("waitid on a nonblocking pidfd of a live child is EAGAIN", waitid(P_PIDFD, fd, &si, WEXITED) < 0 && errno == EAGAIN);
    memset(&si, 0xff, sizeof si);
    check("... with WNOHANG it is 0 and nothing", waitid(P_PIDFD, fd, &si, WEXITED | WNOHANG) == 0 && si.si_pid == 0);
    write(go[1], "g", 1);
    check("... readable once it ended", revents(fd, 5000) & POLLIN);
    check("... and then it reaps", waitid(P_PIDFD, fd, &si, WEXITED) == 0 && si.si_pid == child && si.si_status == 9);
    close(fd);
    close(go[0]);
    close(go[1]);
}

static void getfd(void) {
    int report[2], hold[2];
    pipe(report);
    pid_t child = fork();
    if (child == 0) {
        pipe(hold);
        write(hold[1], "hello", 5);
        write(report[1], &hold[0], sizeof hold[0]);
        pause();
        _exit(0);
    }
    int target;
    read(report[0], &target, sizeof target);
    int fd = pidfd_open(child, 0);
    int dup = pidfd_getfd(fd, target, 0);
    char buf[8] = {0};
    check("pidfd_getfd duplicates another process's descriptor", dup >= 0 && read(dup, buf, 5) == 5 && strcmp(buf, "hello") == 0);
    check("... close-on-exec", (fcntl(dup, F_GETFD) & FD_CLOEXEC) != 0);
    errno = 0;
    check("... of a descriptor it does not have is EBADF", pidfd_getfd(fd, 900, 0) < 0 && errno == EBADF);
    errno = 0;
    check("... with flags is EINVAL", pidfd_getfd(fd, target, 1) < 0 && errno == EINVAL);
    errno = 0;
    check("... through a pipe is EBADF", pidfd_getfd(report[0], target, 0) < 0 && errno == EBADF);
    kill(child, SIGKILL);
    waitpid(child, NULL, 0);
    close(dup);
    close(fd);
    close(report[0]);
    close(report[1]);
}

static void clone_pidfd(void) {
    /* clone3: the pidfd at cl_args.pidfd. */
    int pidfd = -1;
    struct clone_args args;
    memset(&args, 0, sizeof args);
    args.flags = CLONE_PIDFD;
    args.pidfd = (uint64_t)(uintptr_t)&pidfd;
    args.exit_signal = SIGCHLD;
    long pid = syscall(SYS_clone3, &args, sizeof args);
    if (pid == 0) _exit(3);
    check("clone3(CLONE_PIDFD) gives a pidfd", pid > 0 && pidfd >= 0 && (fcntl(pidfd, F_GETFD) & FD_CLOEXEC));
    siginfo_t si;
    memset(&si, 0, sizeof si);
    check("... that waitid(P_PIDFD) waits for", waitid(P_PIDFD, pidfd, &si, WEXITED) == 0 && si.si_pid == pid && si.si_status == 3);
    close(pidfd);

    /* clone: the pidfd where the parent's tid would go. */
    pidfd = -1;
    pid = syscall(SYS_clone, CLONE_PIDFD | SIGCHLD, 0, &pidfd, 0, 0);
    if (pid == 0) _exit(5);
    check("clone(CLONE_PIDFD) gives a pidfd at parent_tid", pid > 0 && pidfd >= 0);
    check("... readable once the child ended", revents(pidfd, 5000) & POLLIN);
    int status = 0;
    check("... and wait4 still reaps it", waitpid((pid_t)pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 5);
    check("... which hangs the pidfd up", revents(pidfd, 0) & POLLHUP);
    close(pidfd);

    errno = 0;
    check("clone(CLONE_PIDFD | CLONE_PARENT_SETTID) is EINVAL",
          syscall(SYS_clone, CLONE_PIDFD | CLONE_PARENT_SETTID | SIGCHLD, 0, &pidfd, 0, 0) < 0 && errno == EINVAL);
    errno = 0;
    check("clone(CLONE_PIDFD | CLONE_THREAD) is EINVAL",
          syscall(SYS_clone, CLONE_PIDFD | CLONE_THREAD | CLONE_SIGHAND | CLONE_VM, 0, &pidfd, 0, 0) < 0 && errno == EINVAL);
    args.flags = CLONE_PIDFD | 0x00400000; /* CLONE_DETACHED */
    errno = 0;
    check("clone3 with CLONE_DETACHED is EINVAL", syscall(SYS_clone3, &args, sizeof args) < 0 && errno == EINVAL);
    args.flags = CLONE_PIDFD;
    args.pidfd = 8;
    errno = 0;
    check("clone3(CLONE_PIDFD) with a bad pidfd address is EFAULT", syscall(SYS_clone3, &args, sizeof args) < 0 && errno == EFAULT);
    sleep_ms(10);
    check("... and leaves no child behind", waitpid(-1, NULL, WNOHANG) < 0 && errno == ECHILD);
}

int main(void) {
    basics();
    open_errors();
    nonblock();
    getfd();
    clone_pidfd();
    if (failures) {
        printf("pidfdtest: %d FAILED\n", failures);
        return 1;
    }
    printf("pidfdtest: all passed\n");
    return 0;
}
