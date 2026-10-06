#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define MIB (1024 * 1024)

static int failures;

static void check(const char *name, int ok) {
    printf("%-52s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static void fork_bomb(void) {
    static pid_t kids[1024];
    int n = 0, err = 0;
    while (n < 1024) {
        pid_t p = fork();
        if (p == 0) {
            for (;;) pause();
        }
        if (p < 0) {
            err = errno;
            break;
        }
        kids[n++] = p;
    }
    printf("fork bomb: %d children, then %s\n", n, strerror(err));
    check("fork fails with EAGAIN/ENOMEM instead of a panic", n > 10 && (err == EAGAIN || err == ENOMEM));
    for (int i = 0; i < n; i++) kill(kids[i], SIGKILL);
    int reaped = 0;
    while (waitpid(-1, NULL, 0) > 0) reaped++;
    check("all children reaped", reaped == n);
    pid_t p = fork();
    if (p == 0) _exit(7);
    int status;
    waitpid(p, &status, 0);
    check("fork works again afterwards", WIFEXITED(status) && WEXITSTATUS(status) == 7);
}

static void memory_hog(void) {
    static void *chunks[4096];
    int n = 0;
    while (n < 4096) {
        void *p = mmap(NULL, MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) break;
        memset(p, 0xab, MIB);
        chunks[n++] = p;
    }
    printf("memory hog: %d MiB mapped before ENOMEM\n", n);
    check("mmap fails with ENOMEM when memory runs out", n > 16 && errno == ENOMEM);
    for (int i = 0; i < n; i++) munmap(chunks[i], MIB);
    void *again = mmap(NULL, 8 * MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check("memory is usable again after munmap", again != MAP_FAILED);
    if (again != MAP_FAILED) munmap(again, 8 * MIB);
}

static void pipe_flood(void) {
    static int fds[200][2];
    static char block[4096];
    long total = 0;
    int pipes = 0, err = 0;
    for (; pipes < 100; pipes++) {
        if (pipe2(fds[pipes], O_NONBLOCK) < 0) break;
        ssize_t w;
        while ((w = write(fds[pipes][1], block, sizeof block)) > 0) total += w;
        err = errno;
    }
    printf("pipe flood: %d pipes, %ld KiB buffered\n", pipes, total / 1024);
    check("full pipes report EAGAIN, no panic", err == EAGAIN && total > 0);
    for (int i = 0; i < pipes; i++) {
        close(fds[i][0]);
        close(fds[i][1]);
    }
}

int main(void) {
    fork_bomb();
    memory_hog();
    pipe_flood();
    printf("oomtest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
