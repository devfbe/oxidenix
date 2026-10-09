/* fork, exec of a child program, and wait with exit statuses of children running
 * concurrently; reaped processes leave no kernel memory behind. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

/* The kernel heap's size (Slab in /proc/meminfo), in KiB. */
static long kernel_heap_kib(void) {
    FILE *f = fopen("/proc/meminfo", "r");
    char line[128];
    long kib = -1;
    while (f && fgets(line, sizeof line, f))
        if (strncmp(line, "Slab:", 5) == 0) kib = atol(line + 5);
    if (f) fclose(f);
    return kib;
}

static void fork_and_reap(int n) {
    for (int i = 0; i < n; i++) {
        pid_t c = fork();
        if (c == 0) _exit(0);
        waitpid(c, NULL, 0);
    }
}

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

    /* A reaped process is gone for good: 4000 of them do not grow the
     * kernel heap (which grows a MiB at a time; each leaked process kept
     * about 2.5 KiB). */
    fork_and_reap(50);
    long before = kernel_heap_kib();
    fork_and_reap(4000);
    long after = kernel_heap_kib();
    printf("kernel heap: %ld KiB before 4000 processes, %ld KiB after\n", before, after);
    if (before < 0 || after > before) {
        printf("FAIL: reaped processes left kernel memory behind\n");
        return 1;
    }
    return 0;
}
