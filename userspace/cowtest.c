#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define SIZE (1024 * 1024)

static int failures;

static void check(const char *name, int ok) {
    printf("%-48s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static int all(const unsigned char *buf, size_t len, unsigned char value) {
    for (size_t i = 0; i < len; i++) {
        if (buf[i] != value) return 0;
    }
    return 1;
}

int main(void) {
    unsigned char *heap = malloc(SIZE);
    memset(heap, 'p', SIZE);
    static unsigned char data[8192];
    memset(data, 'p', sizeof data);
    int fds[2];
    pipe(fds);

    pid_t child = fork();
    if (child == 0) {
        memset(heap, 'c', SIZE);
        memset(data, 'c', sizeof data);
        int ok = all(heap, SIZE, 'c') && all(data, sizeof data, 'c');
        // The kernel writes into a page that is still shared with the parent.
        static unsigned char target[4096];
        size_t n = read(fds[0], target, 5);
        ok = ok && n == 5 && memcmp(target, "hello", 5) == 0;
        _exit(ok ? 0 : 1);
    }
    write(fds[1], "hello", 5);
    int status;
    waitpid(child, &status, 0);
    check("child sees its own writes (heap, data, read())", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    check("parent heap unchanged after child wrote", all(heap, SIZE, 'p'));
    check("parent data unchanged after child wrote", all(data, sizeof data, 'p'));

    memset(heap, 'q', SIZE);
    check("parent can write its pages after the child exited", all(heap, SIZE, 'q'));

    for (int i = 0; i < 50; i++) {
        pid_t p = fork();
        if (p == 0) {
            heap[i] = 'x';
            _exit(0);
        }
        waitpid(p, NULL, 0);
    }
    check("50 forks with writes leave the parent intact", all(heap, SIZE, 'q'));

    printf("cowtest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
