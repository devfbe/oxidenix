/* pidfds (the Linux server's): pidfd_open, clone's and clone3's CLONE_PIDFD,
 * pidfd_send_signal (as kill and as rt_sigqueueinfo), pidfd_getfd, waitid(P_PIDFD) with
 * PIDFD_NONBLOCK, readiness for poll and epoll when the process exits (POLLIN) and is
 * reaped (POLLHUP), and the errors; thread pidfds (PIDFD_THREAD, CLONE_PIDFD with
 * CLONE_THREAD: Linux 6.9): signals to the thread or by flags to its process, readiness
 * when the thread exits (the main thread too, its process living on), an exec by another
 * thread. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
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
#define PIDFD_THREAD O_EXCL
#define PIDFD_SIGNAL_THREAD 1
#define PIDFD_SIGNAL_THREAD_GROUP 2
#define PIDFD_SIGNAL_PROCESS_GROUP 4

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

/* Which thread took the last SIGUSR1 or SIGUSR2. */
static volatile pid_t sig_tid;

static void on_thread_signal(int sig) {
    (void)sig;
    sig_tid = (pid_t)syscall(SYS_gettid);
}

static void wait_signal(void) {
    for (int i = 0; i < 2000 && sig_tid == 0; i++) sleep_ms(1);
}

static void read_byte(int fd) {
    char c;
    while (read(fd, &c, 1) < 0 && errno == EINTR) {
    }
}

/* A thread that blocks SIGUSR2, reports its id and waits for a byte. */
struct worker {
    int report[2], go[2];
};

static void *worker_main(void *arg) {
    struct worker *w = arg;
    sigset_t usr2;
    sigemptyset(&usr2);
    sigaddset(&usr2, SIGUSR2);
    pthread_sigmask(SIG_BLOCK, &usr2, NULL);
    pid_t tid = (pid_t)syscall(SYS_gettid);
    write(w->report[1], &tid, sizeof tid);
    read_byte(w->go[0]);
    return NULL;
}

static void thread_pidfds(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_thread_signal;
    sigaction(SIGUSR1, &sa, NULL);
    sigaction(SIGUSR2, &sa, NULL);
    struct worker w;
    pipe(w.report);
    pipe(w.go);
    pthread_t th;
    pthread_create(&th, NULL, worker_main, &w);
    pid_t tid;
    read(w.report[0], &tid, sizeof tid);

    errno = 0;
    check("pidfd_open of a thread not the main one is EINVAL", pidfd_open(tid, 0) < 0 && errno == EINVAL);
    int tfd = pidfd_open(tid, PIDFD_THREAD);
    check("pidfd_open(PIDFD_THREAD) of another thread", tfd >= 0);
    check("... shows PIDFD_THREAD (O_EXCL) in its status flags", (fcntl(tfd, F_GETFL) & O_EXCL) != 0);
    check("... and is not readable while the thread runs", revents(tfd, 0) == 0);
    sig_tid = 0;
    check("pidfd_send_signal on a thread pidfd", pidfd_send_signal(tfd, SIGUSR1, NULL, 0) == 0);
    wait_signal();
    check("... signals that thread", sig_tid == tid);
    sig_tid = 0;
    check("PIDFD_SIGNAL_THREAD_GROUP on it", pidfd_send_signal(tfd, SIGUSR2, NULL, PIDFD_SIGNAL_THREAD_GROUP) == 0);
    wait_signal();
    check("... signals its process (the thread that does not block it)", sig_tid == getpid());
    errno = 0;
    check("PIDFD_SIGNAL_PROCESS_GROUP of a thread's id is ESRCH",
          pidfd_send_signal(tfd, SIGUSR1, NULL, PIDFD_SIGNAL_PROCESS_GROUP) < 0 && errno == ESRCH);
    errno = 0;
    check("two pidfd_send_signal flags at once are EINVAL",
          pidfd_send_signal(tfd, 0, NULL, PIDFD_SIGNAL_THREAD | PIDFD_SIGNAL_THREAD_GROUP) < 0 && errno == EINVAL);
    int dup = pidfd_getfd(tfd, w.go[1], 0);
    check("pidfd_getfd through a thread pidfd", dup >= 0);
    close(dup);
    siginfo_t si;
    errno = 0;
    check("waitid(P_PIDFD) of a thread pidfd is ECHILD", waitid(P_PIDFD, tfd, &si, WEXITED | WNOHANG) < 0 && errno == ECHILD);

    /* The calling thread's own: kill's code is its own to claim. */
    int self = pidfd_open(getpid(), PIDFD_THREAD);
    siginfo_t info;
    memset(&info, 0, sizeof info);
    info.si_signo = SIGUSR1;
    info.si_code = SI_USER;
    sig_tid = 0;
    check("SI_USER through the calling thread's own thread pidfd", self >= 0 && pidfd_send_signal(self, SIGUSR1, &info, 0) == 0);
    wait_signal();
    check("... reaches it", sig_tid == getpid());
    errno = 0;
    check("... but towards another thread is EPERM", pidfd_send_signal(tfd, SIGUSR1, &info, 0) < 0 && errno == EPERM);
    close(self);

    write(w.go[1], "x", 1);
    pthread_join(th, NULL);
    check("a thread pidfd is readable and hung up once its thread is gone", (revents(tfd, 5000) & (POLLIN | POLLHUP)) == (POLLIN | POLLHUP));
    errno = 0;
    check("... then pidfd_send_signal is ESRCH", pidfd_send_signal(tfd, 0, NULL, 0) < 0 && errno == ESRCH);
    errno = 0;
    check("... and pidfd_getfd ESRCH", pidfd_getfd(tfd, 0, 0) < 0 && errno == ESRCH);
    close(tfd);
    errno = 0;
    check("pidfd_open(PIDFD_THREAD) of a thread gone is ESRCH", pidfd_open(tid, PIDFD_THREAD) < 0 && errno == ESRCH);
    close(w.report[0]);
    close(w.report[1]);
    close(w.go[0]);
    close(w.go[1]);
    signal(SIGUSR1, SIG_DFL);
    signal(SIGUSR2, SIG_DFL);
}

static void *idle_main(void *arg) {
    read_byte(*(int *)arg);
    return NULL;
}

/* The main thread ends before its process: its thread pidfd is readable at once, the
 * process's only when the last thread ended. */
static void main_thread_exits(void) {
    int ready[2], step[2], go[2];
    pipe(ready);
    pipe(step);
    pipe(go);
    pid_t child = fork();
    if (child == 0) {
        pthread_t th;
        pthread_create(&th, NULL, idle_main, &go[0]);
        write(ready[1], "r", 1);
        read_byte(step[0]);
        pthread_exit(NULL);
    }
    read_byte(ready[0]);
    int tfd = pidfd_open(child, PIDFD_THREAD);
    int pfd = pidfd_open(child, 0);
    check("thread and process pidfds of a main thread", tfd >= 0 && pfd >= 0 && revents(tfd, 0) == 0 && revents(pfd, 0) == 0);
    write(step[1], "s", 1);
    short re = revents(tfd, 5000);
    check("the main thread's ends: its thread pidfd is readable, not hung up", (re & POLLIN) && !(re & POLLHUP));
    check("... the process's is not (a thread lives on)", revents(pfd, 0) == 0);
    int late = pidfd_open(child, PIDFD_THREAD);
    check("... a thread pidfd opened after it ended is readable", late >= 0 && (revents(late, 0) & POLLIN));
    check("... a signal to the exited main thread does nothing", pidfd_send_signal(tfd, SIGTERM, NULL, 0) == 0);
    errno = 0;
    check("... pidfd_getfd of its process is EBADF (the main thread's table)", pidfd_getfd(pfd, 0, 0) < 0 && errno == EBADF);
    write(go[1], "g", 1);
    check("the last thread's end makes the process pidfd readable", revents(pfd, 5000) & POLLIN);
    check("... the thread pidfd still not hung up (a zombie)", !(revents(tfd, 0) & POLLHUP));
    int status = 0;
    check("... it exited normally (the signal did nothing)", waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    check("reaped: both hung up", (revents(tfd, 0) & POLLHUP) && (revents(pfd, 0) & POLLHUP) && (revents(late, 0) & POLLHUP));
    close(late);
    close(tfd);
    close(pfd);
    int all[] = {ready[0], ready[1], step[0], step[1], go[0], go[1]};
    for (unsigned i = 0; i < sizeof all / sizeof all[0]; i++) close(all[i]);
}

struct execer {
    int report, go;
};

static void *exec_main(void *arg) {
    struct execer *e = arg;
    pid_t tid = (pid_t)syscall(SYS_gettid);
    write(e->report, &tid, sizeof tid);
    read_byte(e->go);
    execl("/bin/sleep", "sleep", "30", (char *)NULL);
    _exit(127);
}

/* Another thread execs: it takes the main thread's id (Linux's exchange_tids). */
static void exec_by_a_thread(void) {
    int report[2], go[2];
    pipe(report);
    pipe(go);
    pid_t child = fork();
    if (child == 0) {
        struct execer e = {report[1], go[0]};
        pthread_t th;
        pthread_create(&th, NULL, exec_main, &e);
        for (;;) pause();
    }
    pid_t tid;
    read(report[0], &tid, sizeof tid);
    int old = pidfd_open(tid, PIDFD_THREAD);
    int main_fd = pidfd_open(child, PIDFD_THREAD);
    write(go[1], "x", 1);
    check("an exec by another thread hangs up that thread's pidfd", (revents(old, 5000) & POLLHUP) != 0);
    check("... the main thread's names the thread that exec'd (not readable)", revents(main_fd, 0) == 0);
    check("... SIGKILL through it", pidfd_send_signal(main_fd, SIGKILL, NULL, 0) == 0);
    int status = 0;
    check("... ends the process", waitpid(child, &status, 0) == child && WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL);
    close(old);
    close(main_fd);
    int all[] = {report[0], report[1], go[0], go[1]};
    for (unsigned i = 0; i < sizeof all / sizeof all[0]; i++) close(all[i]);
}

static int clone_main(void *arg) {
    read_byte(*(int *)arg);
    return 0;
}

static char clone_stack[64 * 1024] __attribute__((aligned(16)));

/* clone(2) of a thread that runs fn(arg) on stack_top and exits with its result (musl's
 * clone() refuses CLONE_THREAD); the parent's tid argument is ptid. */
static long thread_clone(unsigned long flags, void *stack_top, int *ptid, int (*fn)(void *), void *arg) {
    void **sp = (void **)stack_top;
    *--sp = arg;
    *--sp = (void *)fn;
    long ret;
    register long r10 __asm__("r10") = 0;
    register long r8 __asm__("r8") = 0;
    __asm__ volatile("syscall\n\t"
                     "test %%rax, %%rax\n\t"
                     "jnz 1f\n\t"
                     "pop %%rax\n\t"
                     "pop %%rdi\n\t"
                     "call *%%rax\n\t"
                     "mov %%eax, %%edi\n\t"
                     "mov $60, %%eax\n\t"
                     "syscall\n\t"
                     "hlt\n\t"
                     "1:\n\t"
                     : "=a"(ret)
                     : "a"(56L), "D"(flags), "S"(sp), "d"(ptid), "r"(r10), "r"(r8)
                     : "rcx", "r11", "memory");
    return ret;
}

static void clone_thread_pidfd(void) {
    int go[2];
    pipe(go);
    int tpidfd = -1;
    unsigned long flags = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM | CLONE_PIDFD;
    long tid = thread_clone(flags, clone_stack + sizeof clone_stack, &tpidfd, clone_main, &go[0]);
    check("clone(CLONE_PIDFD | CLONE_THREAD) gives a thread pidfd", tid > 0 && tpidfd >= 0 && (fcntl(tpidfd, F_GETFL) & O_EXCL));
    check("... not readable while the thread runs", revents(tpidfd, 0) == 0);
    write(go[1], "x", 1);
    check("... readable and hung up once the thread is gone", (revents(tpidfd, 5000) & (POLLIN | POLLHUP)) == (POLLIN | POLLHUP));
    close(tpidfd);
    close(go[0]);
    close(go[1]);
}

int main(void) {
    basics();
    open_errors();
    nonblock();
    getfd();
    clone_pidfd();
    thread_pidfds();
    main_thread_exits();
    exec_by_a_thread();
    clone_thread_pidfd();
    if (failures) {
        printf("pidfdtest: %d FAILED\n", failures);
        return 1;
    }
    printf("pidfdtest: all passed\n");
    return 0;
}
