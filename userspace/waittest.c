/* Processes in the Linux server (R8): wait4 and waitid with their options (WNOHANG,
 * WNOWAIT, WEXITED, WSTOPPED, WCONTINUED, __WCLONE, __WALL), the siginfo and rusage they
 * report, SIGCHLD ignored or SA_NOCLDWAIT (children reaped at once), child subreapers,
 * the parent-death signal, process groups and sessions (setpgid, setsid, getsid and their
 * errors), clone3, and vfork. */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/sched.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static void sleep_ms(long ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000};
    nanosleep(&ts, NULL);
}

static void spin_ms(long ms) {
    struct timespec a, b;
    clock_gettime(CLOCK_MONOTONIC, &a);
    do clock_gettime(CLOCK_MONOTONIC, &b);
    while ((b.tv_sec - a.tv_sec) * 1000 + (b.tv_nsec - a.tv_nsec) / 1000000 < ms);
}

static volatile sig_atomic_t chld_seen;
static volatile int chld_code, chld_status, chld_pid;

static void on_chld(int sig, siginfo_t *si, void *uc) {
    (void)sig;
    (void)uc;
    chld_seen++;
    chld_code = si->si_code;
    chld_status = si->si_status;
    chld_pid = si->si_pid;
}

int main(void) {
    int st = 0;

    /* waitid: WNOWAIT leaves the zombie, the siginfo tells who and how. */
    pid_t p = fork();
    if (p == 0) _exit(7);
    siginfo_t si;
    memset(&si, 0, sizeof si);
    int r = waitid(P_PID, p, &si, WEXITED | WNOWAIT);
    check("waitid(WNOWAIT) reports CLD_EXITED with the status", r == 0 && si.si_pid == p && si.si_code == CLD_EXITED && si.si_status == 7 && si.si_signo == SIGCHLD);
    memset(&si, 0, sizeof si);
    r = waitid(P_ALL, 0, &si, WEXITED);
    check("... and the child is still there to reap", r == 0 && si.si_pid == p);
    check("then it is gone (ECHILD)", waitpid(p, &st, WNOHANG) == -1 && errno == ECHILD);

    /* A child killed by a signal: CLD_KILLED, WTERMSIG. */
    p = fork();
    if (p == 0) {
        for (;;) pause();
    }
    kill(p, SIGUSR2);
    memset(&si, 0, sizeof si);
    waitid(P_PID, p, &si, WEXITED);
    check("a killed child: CLD_KILLED with the signal", si.si_code == CLD_KILLED && si.si_status == SIGUSR2);

    /* WNOHANG with a child still running answers 0; waitid zeroes si_pid. */
    p = fork();
    if (p == 0) {
        sleep_ms(200);
        _exit(0);
    }
    check("wait4(WNOHANG) of a running child is 0", waitpid(p, &st, WNOHANG) == 0);
    memset(&si, 0xff, sizeof si);
    r = waitid(P_PID, p, &si, WEXITED | WNOHANG);
    check("waitid(WNOHANG) of a running child: si_pid 0", r == 0 && si.si_pid == 0);
    waitpid(p, &st, 0);

    /* waitid's stop and continue reports. */
    p = fork();
    if (p == 0) {
        raise(SIGSTOP);
        _exit(3);
    }
    memset(&si, 0, sizeof si);
    waitid(P_PID, p, &si, WSTOPPED);
    check("waitid(WSTOPPED): CLD_STOPPED with SIGSTOP", si.si_code == CLD_STOPPED && si.si_status == SIGSTOP);
    kill(p, SIGCONT);
    memset(&si, 0, sizeof si);
    waitid(P_PID, p, &si, WCONTINUED);
    check("waitid(WCONTINUED): CLD_CONTINUED", si.si_code == CLD_CONTINUED && si.si_status == SIGCONT);
    waitpid(p, &st, 0);
    check("... then it exits", WIFEXITED(st) && WEXITSTATUS(st) == 3);
    check("waitid without WEXITED, WSTOPPED or WCONTINUED is EINVAL", waitid(P_ALL, 0, &si, WNOHANG) == -1 && errno == EINVAL);

    /* A "clone" child (exit signal not SIGCHLD) waits for __WCLONE or __WALL; its end
     * sends its exit signal (ignored here, or it would end this process). */
    signal(SIGUSR1, SIG_IGN);
    p = (pid_t)syscall(SYS_clone, SIGUSR1, 0, NULL, NULL, 0);
    if (p == 0) _exit(9);
    sleep_ms(50);
    check("a clone child is not wait4's without __WCLONE", waitpid(p, &st, WNOHANG) == -1 && errno == ECHILD);
    check("__WALL takes it", waitpid(p, &st, __WALL) == p && WIFEXITED(st) && WEXITSTATUS(st) == 9);
    signal(SIGUSR1, SIG_DFL);

    /* rusage of a reaped child: its CPU time. */
    p = fork();
    if (p == 0) {
        spin_ms(120);
        _exit(0);
    }
    struct rusage ru;
    memset(&ru, 0, sizeof ru);
    wait4(p, &st, 0, &ru);
    long ms = ru.ru_utime.tv_sec * 1000 + ru.ru_utime.tv_usec / 1000 + ru.ru_stime.tv_sec * 1000 + ru.ru_stime.tv_usec / 1000;
    check("wait4's rusage counts the child's CPU time", ms >= 80);
    getrusage(RUSAGE_CHILDREN, &ru);
    ms = ru.ru_utime.tv_sec * 1000 + ru.ru_utime.tv_usec / 1000 + ru.ru_stime.tv_sec * 1000 + ru.ru_stime.tv_usec / 1000;
    check("RUSAGE_CHILDREN adds it up", ms >= 80);

    /* SIGCHLD with SA_SIGINFO: who ended, how. */
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_chld;
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    sigaction(SIGCHLD, &sa, NULL);
    p = fork();
    if (p == 0) _exit(42);
    waitpid(p, &st, 0);
    check("SIGCHLD's siginfo names the child and its status", chld_seen >= 1 && chld_pid == p && chld_code == CLD_EXITED && chld_status == 42);

    /* SA_NOCLDSTOP: no SIGCHLD for a stop. */
    sa.sa_flags = SA_SIGINFO | SA_RESTART | SA_NOCLDSTOP;
    sigaction(SIGCHLD, &sa, NULL);
    chld_seen = 0;
    p = fork();
    if (p == 0) {
        raise(SIGSTOP);
        _exit(0);
    }
    waitpid(p, &st, WUNTRACED);
    sleep_ms(30);
    check("SA_NOCLDSTOP: a stop sends no SIGCHLD", WIFSTOPPED(st) && chld_seen == 0);
    kill(p, SIGCONT);
    waitpid(p, &st, 0);

    /* SA_NOCLDWAIT: children are reaped at once; wait blocks until all are gone, ECHILD. */
    /* (Without a handler: one would interrupt the wait.) */
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = SIG_DFL;
    sa.sa_flags = SA_NOCLDWAIT;
    sigaction(SIGCHLD, &sa, NULL);
    for (int i = 0; i < 3; i++)
        if (fork() == 0) _exit(0);
    r = wait(&st);
    check("SA_NOCLDWAIT: no zombies, wait is ECHILD", r == -1 && errno == ECHILD);
    signal(SIGCHLD, SIG_IGN);
    p = fork();
    if (p == 0) _exit(1);
    r = waitpid(p, &st, 0);
    check("SIGCHLD ignored: the child is reaped by itself", r == -1 && errno == ECHILD);
    signal(SIGCHLD, SIG_DFL);

    /* A child subreaper adopts its orphaned grandchildren. */
    prctl(PR_SET_CHILD_SUBREAPER, 1);
    int sub = 0;
    prctl(PR_GET_CHILD_SUBREAPER, &sub);
    p = fork();
    if (p == 0) {
        pid_t g = fork();
        if (g == 0) {
            sleep_ms(100);
            _exit(11);
        }
        _exit(0);
    }
    waitpid(p, &st, 0);
    pid_t orphan = wait(&st);
    check("a child subreaper reaps its orphaned grandchild", sub == 1 && orphan > 0 && orphan != p && WEXITSTATUS(st) == 11);
    prctl(PR_SET_CHILD_SUBREAPER, 0);

    /* The parent-death signal comes when the parent ends. */
    int pfd[2];
    pipe(pfd);
    p = fork();
    if (p == 0) {
        prctl(PR_SET_CHILD_SUBREAPER, 0);
        pid_t c = fork();
        if (c == 0) {
            prctl(PR_SET_PDEATHSIG, SIGUSR1);
            sigset_t s;
            sigemptyset(&s);
            sigaddset(&s, SIGUSR1);
            sigprocmask(SIG_BLOCK, &s, NULL);
            write(pfd[1], "r", 1);
            int got = 0;
            sigwait(&s, &got);
            write(pfd[1], got == SIGUSR1 ? "y" : "n", 1);
            _exit(0);
        }
        char c0;
        read(pfd[0], &c0, 1);
        _exit(0);
    }
    waitpid(p, &st, 0);
    char c = 0;
    read(pfd[0], &c, 1);
    check("PR_SET_PDEATHSIG: the signal comes when the parent ends", c == 'y');

    /* Process groups and sessions. */
    check("getsid(0) and getpgid(0) answer", getsid(0) > 0 && getpgid(0) > 0);
    check("getpgid of no process is ESRCH", getpgid(32767) == -1 && errno == ESRCH);
    p = fork();
    if (p == 0) {
        int leader = setsid() == getpid();
        int again = setsid() == -1 && errno == EPERM;
        int own = setpgid(0, 0) == -1 && errno == EPERM;
        _exit(leader && again && own ? 0 : 1);
    }
    waitpid(p, &st, 0);
    check("setsid makes a leader; a leader's setsid and setpgid are EPERM", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    pid_t me = getpid();
    p = fork();
    if (p == 0) {
        sleep_ms(100);
        _exit(getpgid(0) == getpid() ? 0 : 1);
    }
    check("a parent puts its child into a group of its own", setpgid(p, p) == 0 && getpgid(p) == p);
    waitpid(p, &st, 0);
    check("... which the child sees", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    p = fork();
    if (p == 0) {
        execl("/bin/sleep", "sleep", "1", (char *)NULL);
        _exit(127);
    }
    sleep_ms(100);
    check("setpgid on a child after its exec is EACCES", setpgid(p, p) == -1 && errno == EACCES);
    kill(p, SIGKILL);
    waitpid(p, &st, 0);
    check("setpgid of a process not a child is ESRCH", setpgid(getppid(), 0) == -1 && errno == ESRCH);
    (void)me;

    /* clone3: a child with its exit signal, a thread-less fork. */
    struct clone_args ca;
    memset(&ca, 0, sizeof ca);
    ca.exit_signal = SIGCHLD;
    p = (pid_t)syscall(SYS_clone3, &ca, sizeof ca);
    if (p == 0) _exit(5);
    check("clone3 makes a child", p > 0 && waitpid(p, &st, 0) == p && WEXITSTATUS(st) == 5);
    ca.flags = CLONE_THREAD;
    check("clone3: CLONE_THREAD without CLONE_SIGHAND is EINVAL", syscall(SYS_clone3, &ca, sizeof ca) == -1 && errno == EINVAL);
    memset(&ca, 0, sizeof ca);
    ca.exit_signal = SIGCHLD;
    char big[128];
    memset(big, 0, sizeof big);
    memcpy(big, &ca, sizeof ca);
    big[100] = 1;
    check("clone3: unknown bytes not zero are E2BIG", syscall(SYS_clone3, big, sizeof big) == -1 && errno == E2BIG);

    /* vfork: the parent waits until the child execs or exits; the child shares memory. */
    static volatile int shared_word;
    shared_word = 0;
    p = vfork();
    if (p == 0) {
        shared_word = 1;
        _exit(0);
    }
    waitpid(p, &st, 0);
    check("vfork: the child ran first, in the parent's memory", shared_word == 1);

    printf("waittest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
