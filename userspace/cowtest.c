#include <fcntl.h>
#include <stdint.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
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

    // A read-only private file mapping right above brk is shared after
    // fork. As on Linux, brk refuses to grow over it, and writing to it
    // faults; the shared frame never becomes writable.
    uintptr_t brk_top = (syscall(SYS_brk, 0) + 4095) & ~(uintptr_t)4095;
    unsigned char *ro = (unsigned char *)(brk_top + 2 * 4096);
    int fd = open("/etc/motd", O_RDONLY);
    mmap(ro, 4096, PROT_READ, MAP_PRIVATE | MAP_FIXED, fd, 0);
    unsigned char first = ro[0];
    pid_t grower = fork();
    if (grower == 0) {
        long r = syscall(SYS_brk, (uintptr_t)ro + 4096);
        _exit(r == (long)(uintptr_t)ro + 4096 ? 1 : 0);
    }
    waitpid(grower, &status, 0);
    check("brk does not grow over an existing mapping", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    pid_t writer = fork();
    if (writer == 0) {
        ro[0] = 'X';
        _exit(0);
    }
    waitpid(writer, &status, 0);
    check("writing a shared read-only mapping faults", WIFSIGNALED(status) && WTERMSIG(status) == SIGSEGV);
    check("shared read-only frame stays unchanged", ro[0] == first && first == 'W');

    printf("cowtest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
