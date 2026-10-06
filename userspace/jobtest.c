#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;
static volatile sig_atomic_t got_usr1;

static void check(const char *name, int ok) {
    printf("%-52s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static void sleep_ms(long ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000};
    nanosleep(&ts, NULL);
}

static void on_usr1(int sig) { got_usr1 = sig; }

/* Child that blocks in read() on the pipe and exits 0 if it gets "x". */
static pid_t reader(int fds[2]) {
    pid_t p = fork();
    if (p == 0) {
        char c = 0;
        ssize_t n = read(fds[0], &c, 1);
        _exit(n == 1 && c == 'x' ? 0 : 1);
    }
    return p;
}

int main(void) {
    int status;

    pid_t p = fork();
    if (p == 0) {
        raise(SIGSTOP);
        _exit(5);
    }
    waitpid(p, &status, WUNTRACED);
    check("waitpid(WUNTRACED) reports the stop", WIFSTOPPED(status) && WSTOPSIG(status) == SIGSTOP);
    kill(p, SIGCONT);
    waitpid(p, &status, WCONTINUED);
    check("waitpid(WCONTINUED) reports the continue", WIFCONTINUED(status));
    waitpid(p, &status, 0);
    check("continued child runs to completion", WIFEXITED(status) && WEXITSTATUS(status) == 5);

    int fds[2];
    pipe(fds);
    p = reader(fds);
    sleep_ms(50);
    kill(p, SIGTSTP);
    waitpid(p, &status, WUNTRACED);
    check("SIGTSTP stops a process blocked in read()", WIFSTOPPED(status) && WSTOPSIG(status) == SIGTSTP);
    kill(p, SIGCONT);
    sleep_ms(50);
    write(fds[1], "x", 1);
    waitpid(p, &status, 0);
    check("read() restarts after stop/continue", WIFEXITED(status) && WEXITSTATUS(status) == 0);

    p = fork();
    if (p == 0) {
        for (;;) pause();
    }
    kill(p, SIGSTOP);
    waitpid(p, &status, WUNTRACED);
    kill(p, SIGKILL);
    waitpid(p, &status, 0);
    check("SIGKILL kills a stopped process", WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL);

    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sa.sa_flags = SA_RESTART;
    sigaction(SIGUSR1, &sa, NULL);
    p = reader(fds);
    sleep_ms(50);
    kill(p, SIGUSR1);
    sleep_ms(50);
    write(fds[1], "x", 1);
    waitpid(p, &status, 0);
    check("SA_RESTART handler resumes the read()", WIFEXITED(status) && WEXITSTATUS(status) == 0);

    printf("jobtest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
