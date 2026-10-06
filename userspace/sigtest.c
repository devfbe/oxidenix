#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t got_int, got_usr1, got_usr2, got_chld;

static void on_int(int sig) { got_int = sig; }
static void on_usr1(int sig) { got_usr1 = sig; }
static void on_usr2(int sig) { got_usr2 = sig; }
static void on_chld(int sig) { got_chld++; (void)sig; }

static volatile double handler_sink;
static volatile int fpu_signals;

/* Uses SSE registers, like any compiled code doing floating point. */
static void on_alrm(int sig) {
    double d = sig;
    for (int i = 0; i < 100; i++) d = d * 1.5 + 0.25;
    handler_sink = d;
    fpu_signals++;
}

/* Floating-point work without syscalls, so signals arrive asynchronously. */
static double fpu_work(void) {
    double acc = 0.0;
    for (int i = 1; i < 3000000; i++) acc += 1.0 / (double)i;
    return acc;
}

static int failures;

static void check(const char *name, int ok) {
    printf("%-44s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static void handle(int sig, void (*fn)(int)) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = fn;
    sigaction(sig, &sa, NULL);
}

static void sleep_ms(long ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000};
    nanosleep(&ts, NULL);
}

int main(void) {
    handle(SIGINT, on_int);
    raise(SIGINT);
    check("handler runs for raise(SIGINT)", got_int == SIGINT);

    handle(SIGCHLD, on_chld);
    pid_t spinner = fork();
    if (spinner == 0) {
        for (;;) {
        }
    }
    sleep_ms(100);
    kill(spinner, SIGTERM);
    int status = 0;
    waitpid(spinner, &status, 0);
    check("SIGTERM kills a busy loop without syscalls",
          WIFSIGNALED(status) && WTERMSIG(status) == SIGTERM);
    check("parent receives SIGCHLD", got_chld >= 1);

    handle(SIGUSR1, on_usr1);
    int fds[2];
    pipe(fds);
    pid_t sender = fork();
    if (sender == 0) {
        sleep_ms(100);
        kill(getppid(), SIGUSR1);
        sleep_ms(100);
        _exit(0);
    }
    char c;
    ssize_t n = read(fds[0], &c, 1);
    check("blocking pipe read fails with EINTR", n == -1 && errno == EINTR);
    check("SIGUSR1 handler ran", got_usr1 == SIGUSR1);
    waitpid(sender, NULL, 0);

    handle(SIGUSR2, on_usr2);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR2);
    sigprocmask(SIG_BLOCK, &set, NULL);
    raise(SIGUSR2);
    check("blocked signal stays pending", got_usr2 == 0);
    sigprocmask(SIG_UNBLOCK, &set, NULL);
    check("unblocking delivers it", got_usr2 == SIGUSR2);

    signal(SIGUSR1, SIG_IGN);
    raise(SIGUSR1);
    check("ignored signal does nothing", 1);

    handle(SIGALRM, on_alrm);
    double expected = fpu_work();
    pid_t pinger = fork();
    if (pinger == 0) {
        for (int i = 0; i < 40; i++) {
            sleep_ms(10);
            kill(getppid(), SIGALRM);
        }
        _exit(0);
    }
    int same = 1;
    for (int round = 0; round < 6; round++) same &= fpu_work() == expected;
    while (waitpid(pinger, NULL, 0) < 0 && errno == EINTR) {
    }
    check("async handlers keep the FPU/SSE state intact", same && fpu_signals > 0);

    fpu_signals = 0;
    alarm(1);
    pause();
    check("alarm() delivers SIGALRM", fpu_signals == 1);
    check("alarm(0) reports no time left", alarm(0) == 0);

    struct itimerval it = {{0, 20000}, {0, 20000}}, cur;
    setitimer(ITIMER_REAL, &it, NULL);
    while (fpu_signals < 4) pause();
    getitimer(ITIMER_REAL, &cur);
    check("setitimer repeats with its interval", cur.it_interval.tv_usec == 20000);
    struct itimerval off = {{0, 0}, {0, 0}};
    setitimer(ITIMER_REAL, &off, NULL);
    int count = fpu_signals;
    sleep_ms(100);
    check("a zero itimerval disarms the timer", fpu_signals == count);

    printf("sigtest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
