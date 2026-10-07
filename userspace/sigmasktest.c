/* Calls that wait with a temporary signal mask (sigsuspend, ppoll,
 * pselect): the mask applies while they wait and to a handler that
 * interrupts them; afterwards the caller's own mask is back. */
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/select.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-56s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static volatile sig_atomic_t got_usr1, usr1_was_blocked;

static void on_usr1(int sig) {
    (void)sig;
    got_usr1++;
    sigset_t now;
    sigprocmask(SIG_BLOCK, NULL, &now);
    /* SIGUSR2 is blocked by every temporary mask below, never otherwise. */
    usr1_was_blocked = sigismember(&now, SIGUSR2);
}

static int usr1_blocked(void) {
    sigset_t now;
    sigprocmask(SIG_BLOCK, NULL, &now);
    return sigismember(&now, SIGUSR1);
}

/* Blocks SIGUSR1 and makes it pending. */
static void pend_usr1(void) {
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGUSR1);
    sigprocmask(SIG_BLOCK, &s, NULL);
    got_usr1 = 0;
    raise(SIGUSR1);
}

/* A temporary mask that lets SIGUSR1 through and blocks SIGUSR2. */
static sigset_t open_usr1(void) {
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGUSR2);
    return s;
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sigaction(SIGUSR1, &sa, NULL);
    sigset_t tmp = open_usr1();

    pend_usr1();
    int r = sigsuspend(&tmp);
    check("sigsuspend runs a handler its mask lets through", r == -1 && errno == EINTR && got_usr1 == 1);
    check("the handler runs with the temporary mask", usr1_was_blocked);
    check("sigsuspend restores the caller's mask", usr1_blocked());

    /* The signal arrives while sigsuspend sleeps. */
    got_usr1 = 0;
    pid_t parent = getpid(), child = fork();
    if (child == 0) {
        struct timespec d = {0, 5000000};
        nanosleep(&d, NULL);
        kill(parent, SIGUSR1);
        _exit(0);
    }
    r = sigsuspend(&tmp);
    waitpid(child, NULL, 0);
    check("sigsuspend sleeps until a signal arrives", r == -1 && errno == EINTR && got_usr1 == 1);

    int p[2];
    pipe(p);
    struct pollfd pf = {p[0], POLLIN, 0};
    struct timespec second = {1, 0};
    pend_usr1();
    r = ppoll(&pf, 1, &second, &tmp);
    check("ppoll with a mask is interrupted by a pending signal", r == -1 && errno == EINTR && got_usr1 == 1);
    check("ppoll restores the caller's mask", usr1_blocked());

    pend_usr1();
    fd_set rd;
    FD_ZERO(&rd);
    FD_SET(p[0], &rd);
    r = pselect(p[0] + 1, &rd, NULL, NULL, &second, &tmp);
    check("pselect with a mask is interrupted by a pending signal", r == -1 && errno == EINTR && got_usr1 == 1);
    check("pselect restores the caller's mask", usr1_blocked());

    /* A ppoll that succeeds restores the mask before any handler runs:
     * the pending signal stays pending. */
    write(p[1], "x", 1);
    pend_usr1();
    r = ppoll(&pf, 1, &second, &tmp);
    check("a successful ppoll leaves a blocked signal pending", r == 1 && got_usr1 == 0);
    sigset_t pending;
    sigpending(&pending);
    check("the signal is still pending afterwards", sigismember(&pending, SIGUSR1));

    /* A mask that blocks a signal holds it off during the wait. */
    sigset_t none;
    sigemptyset(&none);
    sigprocmask(SIG_SETMASK, &none, NULL); /* runs the pending handler */
    got_usr1 = 0;
    char c;
    read(p[0], &c, 1);
    sigset_t block;
    sigemptyset(&block);
    sigaddset(&block, SIGUSR1);
    child = fork();
    if (child == 0) {
        struct timespec d = {0, 5000000};
        nanosleep(&d, NULL);
        kill(parent, SIGUSR1);
        _exit(0);
    }
    struct timespec brief = {0, 50000000};
    /* Interrupted, it would fail with EINTR; held off, it times out. */
    r = ppoll(&pf, 1, &brief, &block);
    waitpid(child, NULL, 0);
    check("ppoll's mask holds a signal off while it waits", r == 0);
    check("the held-off signal arrives once ppoll returns", got_usr1 == 1);

    check("a mask of the wrong size is EINVAL",
          syscall(271, &pf, 1, &second, &tmp, 4) == -1 && errno == EINVAL);

    printf("sigmasktest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
