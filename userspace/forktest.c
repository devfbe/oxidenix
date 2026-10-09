/* fork, exec of a child program, and wait with exit statuses of children running
 * concurrently. */
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

static void busy(void) {
    for (volatile long i = 0; i < 30000000; i++) {
    }
}

static int wait_for(pid_t pid) {
    int status;
    if (waitpid(pid, &status, 0) != pid) {
        return -1;
    }
    return WEXITSTATUS(status);
}

int main(void) {
    printf("forktest: pid=%d\n", getpid());

    pid_t child = fork();
    if (child == 0) {
        printf("child: pid=%d ppid=%d, starting hello\n", getpid(), getppid());
        char *argv[] = {"hello", "from", "child", NULL};
        execv("/bin/hello", argv);
        printf("execv failed\n");
        return 1;
    }
    printf("parent: child %d exited with %d\n", child, wait_for(child));

    pid_t workers[2];
    for (int w = 0; w < 2; w++) {
        workers[w] = fork();
        if (workers[w] == 0) {
            for (int i = 0; i < 4; i++) {
                printf("worker %c: round %d\n", 'A' + w, i);
                busy();
            }
            return 10 + w;
        }
    }
    for (int w = 0; w < 2; w++) {
        printf("parent: worker %c exited with %d\n", 'A' + w, wait_for(workers[w]));
    }
    return 0;
}
