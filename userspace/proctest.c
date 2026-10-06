/* Process information: prctl, capabilities and (later) /proc. */
#include <errno.h>
#include <linux/capability.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-52s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

int main(void) {
    char name[16] = {0};
    prctl(PR_SET_NAME, "renamed-process");
    prctl(PR_GET_NAME, name);
    check("PR_SET_NAME/PR_GET_NAME round-trip (15 bytes)", strcmp(name, "renamed-process") == 0);

    struct __user_cap_header_struct hdr = {_LINUX_CAPABILITY_VERSION_3, 0};
    struct __user_cap_data_struct data[2];
    int r = syscall(SYS_capget, &hdr, data);
    check("capget reports the full set (everything is root)", r == 0 && data[0].effective == 0xffffffff && data[0].inheritable == 0);
    hdr.version = 0x12345678;
    r = syscall(SYS_capget, &hdr, data);
    check("capget with an unknown version proposes v3", r == -1 && errno == EINVAL && hdr.version == _LINUX_CAPABILITY_VERSION_3);
    check("PR_CAPBSET_READ knows CAP_SYS_ADMIN", prctl(PR_CAPBSET_READ, CAP_SYS_ADMIN) == 1);
    check("PR_SET_NO_NEW_PRIVS sticks", prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0 && prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) == 1);

    /* A grandchild with PR_SET_PDEATHSIG learns when its parent dies. */
    int pipefd[2];
    pipe(pipefd);
    pid_t child = fork();
    if (child == 0) {
        pid_t grandchild = fork();
        if (grandchild == 0) {
            sigset_t set;
            sigemptyset(&set);
            sigaddset(&set, SIGUSR1);
            sigprocmask(SIG_BLOCK, &set, NULL);
            prctl(PR_SET_PDEATHSIG, SIGUSR1);
            write(pipefd[1], "r", 1);
            int sig = 0;
            sigwait(&set, &sig);
            write(pipefd[1], sig == SIGUSR1 ? "y" : "n", 1);
            _exit(0);
        }
        char c;
        read(pipefd[0], &c, 1);
        _exit(0);
    }
    int st;
    waitpid(child, &st, 0);
    char c = 0;
    close(pipefd[1]);
    read(pipefd[0], &c, 1);
    check("PR_SET_PDEATHSIG signals an orphaned child", c == 'y');

    printf("proctest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
