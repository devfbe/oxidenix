#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t got_int, got_usr1, got_usr2, got_chld;

static void on_int(int sig) { got_int = sig; }
static void on_usr1(int sig) { got_usr1 = sig; }
static void on_usr2(int sig) { got_usr2 = sig; }
static void on_chld(int sig) { got_chld++; (void)sig; }

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

    printf("sigtest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
