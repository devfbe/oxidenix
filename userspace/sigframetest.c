/* Signal delivery by the Linux server (R8): Linux's x86-64 signal frame (siginfo and
 * ucontext as a handler sees them), who sent a signal (SI_USER, SI_TKILL, SI_QUEUE with
 * its value), real-time signals queued in order, the alternate signal stack (SA_ONSTACK,
 * SS_AUTODISARM), SA_RESETHAND, SA_NODEFER, sigtimedwait's timeout, a frame that cannot be
 * written, and a fault's address. */
#define _GNU_SOURCE
#include <errno.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static volatile int seen_code, seen_pid, seen_value, seen_count;
static volatile uintptr_t seen_rsp, seen_rip, seen_addr;
static volatile int order[16];

static void on_info(int sig, siginfo_t *si, void *ucv) {
    ucontext_t *uc = ucv;
    (void)sig;
    seen_code = si->si_code;
    seen_pid = si->si_pid;
    seen_value = si->si_value.sival_int;
    seen_rip = uc->uc_mcontext.gregs[REG_RIP];
    int local;
    seen_rsp = (uintptr_t)&local;
}

static void on_rt(int sig, siginfo_t *si, void *uc) {
    (void)uc;
    if (seen_count < 16) order[seen_count] = sig * 100 + si->si_value.sival_int;
    seen_count++;
}

static volatile int resets, depth, max_depth;

static void on_reset(int sig) {
    (void)sig;
    resets++;
}

static void on_nested(int sig) {
    depth++;
    if (depth > max_depth) max_depth = depth;
    if (depth < 3) raise(sig);
    depth--;
}

static volatile int calls;

/* Raises its signal again once: delivered after it returns (it is blocked meanwhile). */
static void on_once(int sig) {
    depth++;
    if (depth > max_depth) max_depth = depth;
    if (calls++ == 0) raise(sig);
    depth--;
}

static void on_set_rax(int sig, siginfo_t *si, void *ucv) {
    (void)sig;
    (void)si;
    ucontext_t *uc = ucv;
    uc->uc_mcontext.gregs[REG_RAX] = -514;
}

static sigjmp_buf env;

static void on_segv(int sig, siginfo_t *si, void *uc) {
    (void)sig;
    (void)uc;
    seen_addr = (uintptr_t)si->si_addr;
    seen_code = si->si_code;
    siglongjmp(env, 1);
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_info;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGUSR1, &sa, NULL);

    kill(getpid(), SIGUSR1);
    check("kill: SI_USER with the sender's pid", seen_code == SI_USER && seen_pid == getpid());
    check("the frame's ucontext has the interrupted rip", seen_rip != 0);
    syscall(SYS_tgkill, getpid(), syscall(SYS_gettid), SIGUSR1);
    check("tgkill: SI_TKILL", seen_code == SI_TKILL && seen_pid == getpid());
    union sigval v = {.sival_int = 1234};
    sigqueue(getpid(), SIGUSR1, v);
    check("sigqueue: SI_QUEUE with its value", seen_code == SI_QUEUE && seen_value == 1234);

    /* Real-time signals queue every instance, lowest number first, in order (each handler
     * blocks both, so their frames do not nest). */
    sigset_t rt;
    sigemptyset(&rt);
    sigaddset(&rt, SIGRTMIN + 1);
    sigaddset(&rt, SIGRTMIN + 2);
    sa.sa_sigaction = on_rt;
    sa.sa_mask = rt;
    sigaction(SIGRTMIN + 1, &sa, NULL);
    sigaction(SIGRTMIN + 2, &sa, NULL);
    sigemptyset(&sa.sa_mask);
    sigprocmask(SIG_BLOCK, &rt, NULL);
    for (int i = 0; i < 3; i++) {
        v.sival_int = i;
        sigqueue(getpid(), SIGRTMIN + 2, v);
        sigqueue(getpid(), SIGRTMIN + 1, v);
    }
    sigset_t pend;
    sigpending(&pend);
    int both = sigismember(&pend, SIGRTMIN + 1) && sigismember(&pend, SIGRTMIN + 2);
    seen_count = 0;
    sigprocmask(SIG_UNBLOCK, &rt, NULL);
    int r1 = (SIGRTMIN + 1) * 100, r2 = (SIGRTMIN + 2) * 100;
    int in_order = seen_count == 6 && order[0] == r1 && order[1] == r1 + 1 && order[2] == r1 + 2 && order[3] == r2 && order[4] == r2 + 1 && order[5] == r2 + 2;
    check("real-time signals: every instance, in order", both && in_order);

    /* sigtimedwait: takes a pending signal with its data; times out with EAGAIN. */
    sigset_t w;
    sigemptyset(&w);
    sigaddset(&w, SIGUSR2);
    sigprocmask(SIG_BLOCK, &w, NULL);
    v.sival_int = 77;
    sigqueue(getpid(), SIGUSR2, v);
    siginfo_t si;
    struct timespec zero = {0, 0};
    int got = sigtimedwait(&w, &si, &zero);
    check("sigtimedwait takes a pending signal with its data", got == SIGUSR2 && si.si_value.sival_int == 77);
    struct timespec shortly = {0, 20 * 1000 * 1000};
    got = sigtimedwait(&w, &si, &shortly);
    check("sigtimedwait times out with EAGAIN", got == -1 && errno == EAGAIN);

    /* The alternate stack: a handler with SA_ONSTACK runs on it; SS_AUTODISARM disarms it
     * while the handler runs. */
    stack_t ss;
    ss.ss_sp = malloc(64 * 1024);
    ss.ss_size = 64 * 1024;
    ss.ss_flags = 0;
    check("sigaltstack takes a stack", sigaltstack(&ss, NULL) == 0);
    sa.sa_sigaction = on_info;
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
    sigaction(SIGUSR1, &sa, NULL);
    raise(SIGUSR1);
    uintptr_t lo = (uintptr_t)ss.ss_sp, hi = lo + ss.ss_size;
    check("SA_ONSTACK: the handler runs on the alternate stack", seen_rsp > lo && seen_rsp < hi);
    stack_t old;
    sigaltstack(NULL, &old);
    check("sigaltstack reports it (not on it now)", old.ss_sp == ss.ss_sp && old.ss_size == ss.ss_size && !(old.ss_flags & SS_ONSTACK));
    stack_t tiny = {.ss_sp = ss.ss_sp, .ss_size = 512, .ss_flags = 0};
    check("a stack below MINSIGSTKSZ is ENOMEM", sigaltstack(&tiny, NULL) == -1 && errno == ENOMEM);
    ss.ss_flags = SS_AUTODISARM;
    sigaltstack(&ss, NULL);
    raise(SIGUSR1);
    sigaltstack(NULL, &old);
    check("SS_AUTODISARM: on the stack, which is back afterwards", seen_rsp > lo && seen_rsp < hi && old.ss_size == ss.ss_size);
    ss.ss_flags = SS_DISABLE;
    sigaltstack(&ss, NULL);
    raise(SIGUSR1);
    check("SS_DISABLE: back on the normal stack", !(seen_rsp > lo && seen_rsp < hi));

    /* SA_RESETHAND: the action is the default after one delivery. */
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_reset;
    sa.sa_flags = SA_RESETHAND;
    sigaction(SIGWINCH, &sa, NULL);
    raise(SIGWINCH);
    struct sigaction now;
    sigaction(SIGWINCH, NULL, &now);
    check("SA_RESETHAND: one delivery, then the default", resets == 1 && now.sa_handler == SIG_DFL);

    /* SA_NODEFER: the handler may be interrupted by its own signal. */
    sa.sa_handler = on_nested;
    sa.sa_flags = SA_NODEFER;
    sigaction(SIGUSR2, &sa, NULL);
    sigprocmask(SIG_UNBLOCK, &w, NULL);
    raise(SIGUSR2);
    check("SA_NODEFER: frames nest", max_depth == 3);
    sa.sa_handler = on_once;
    sa.sa_flags = 0;
    max_depth = depth = 0;
    sigaction(SIGUSR2, &sa, NULL);
    raise(SIGUSR2);
    check("without it the signal waits for the handler's return", max_depth == 1 && calls == 2);

    /* A fault: the handler gets the address and SEGV_MAPERR or SEGV_ACCERR. */
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_segv;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGSEGV, &sa, NULL);
    char *ro = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (!sigsetjmp(env, 1)) *(volatile char *)(ro + 8) = 1;
    check("a write to a read-only page: SEGV_ACCERR at its address", seen_code == SEGV_ACCERR && seen_addr == (uintptr_t)(ro + 8));
    munmap(ro, 4096);
    if (!sigsetjmp(env, 1)) *(volatile char *)(ro + 16) = 1;
    check("an unmapped page: SEGV_MAPERR at its address", seen_code == SEGV_MAPERR && seen_addr == (uintptr_t)(ro + 16));

    /* A frame that cannot be written (the stack pointer at an unmapped page) kills with
     * SIGSEGV. */
    pid_t p = fork();
    if (p == 0) {
        struct sigaction h;
        memset(&h, 0, sizeof h);
        h.sa_handler = on_reset;
        sigaction(SIGUSR1, &h, NULL);
        __asm__ volatile("mov $0x10000, %%rsp\n\tmov $62, %%eax\n\tmov %0, %%edi\n\tmov $10, %%esi\n\tsyscall\n\t1: jmp 1b" ::"r"(getpid()) : "rax", "rdi", "rsi", "rcx", "r11", "memory");
        _exit(0);
    }
    int st = 0;
    waitpid(p, &st, 0);
    check("a frame that cannot be written: SIGSEGV", WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV);

    /* A handler may set the interrupted context's rax to anything, also to one of the
     * kernel's restart codes (-512..-516): rt_sigreturn restores it as it is (the call
     * returns it), never restarts anything. Also a run of real-time signals queued and
     * left at exit gives the instance's queue back (pending signals of an ended thread or
     * process do not stay counted). */
    sa.sa_sigaction = on_set_rax;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGUSR2, &sa, NULL);
    long r = syscall(SYS_kill, getpid(), SIGUSR2);
    check("rt_sigreturn restores a restart code in rax untouched", r == -1 && errno == 514);
    int queued_ok = 1;
    for (int round = 0; round < 3 && queued_ok; round++) {
        pid_t q = fork();
        if (q == 0) {
            sigset_t all;
            sigfillset(&all);
            sigprocmask(SIG_BLOCK, &all, NULL);
            /* 3000 queued, then exit with them pending. */
            for (int i = 0; i < 3000; i++) {
                union sigval w = {.sival_int = i};
                if (sigqueue(getpid(), SIGRTMIN + 3, w) != 0) _exit(1);
            }
            _exit(0);
        }
        int qs = 0;
        waitpid(q, &qs, 0);
        queued_ok = WIFEXITED(qs) && WEXITSTATUS(qs) == 0;
    }
    check("queued real-time signals of an ended process are given back", queued_ok);

    printf("sigframetest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
