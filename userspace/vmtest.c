/* Virtual memory: demand paging, protection, remapping, sharing, stacks,
 * commit accounting, and the patterns JIT compilers rely on. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define MIB (1024 * 1024L)

static int failures;

static void check(const char *name, int ok) {
    printf("%-52s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

/* Resident pages of this process (/proc/self/statm, second field). */
static long resident(void) {
    char buf[128] = {0};
    int fd = open("/proc/self/statm", O_RDONLY);
    read(fd, buf, sizeof buf - 1);
    close(fd);
    long size, rss;
    sscanf(buf, "%ld %ld", &size, &rss);
    return rss;
}

static sigjmp_buf env;
static volatile sig_atomic_t got;
static void on_fault(int sig) {
    got = sig;
    siglongjmp(env, 1);
}

/* Whether touching `p` (read or write) raises a signal; which one in `got`. */
static int faults(volatile char *p, int write) {
    got = 0;
    if (!sigsetjmp(env, 1)) {
        if (write) *p = 1;
        else (void)*p;
    }
    return got != 0;
}

static int depth(int n) {
    volatile char frame[1024];
    frame[0] = (char)n;
    return n == 0 ? frame[0] : depth(n - 1) + frame[0];
}

int main(void) {
    struct sigaction sa = {0};
    sa.sa_handler = on_fault;
    sigaction(SIGSEGV, &sa, NULL);
    sigaction(SIGBUS, &sa, NULL);

    /* Demand paging: a big mapping costs nothing until it is touched. */
    long before = resident();
    char *big = mmap(NULL, 64 * MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    long after_map = resident();
    big[0] = 1;
    big[32 * MIB] = 2;
    long after_touch = resident();
    check("64 MiB mmap is not resident until touched", big != MAP_FAILED && after_map - before < 4 && after_touch - after_map == 2);
    check("untouched pages read as zero", big[48 * MIB] == 0 && big[1] == 0);
    munmap(big, 64 * MIB);

    /* Protection. */
    char *p = mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    p[0] = 'a';
    p[4096] = 'b';
    mprotect(p, 4096, PROT_READ);
    check("write to a read-only page raises SIGSEGV", faults(p, 1) && got == SIGSEGV && p[0] == 'a');
    mprotect(p + 4096, 4096, PROT_NONE);
    check("PROT_NONE denies reads", faults(p + 4096, 0));
    mprotect(p + 4096, 4096, PROT_READ | PROT_WRITE);
    check("PROT_NONE keeps the contents", p[4096] == 'b' && !faults(p + 4096, 1));
    mprotect(p, 4096, PROT_READ | PROT_WRITE);
    check("mprotect back to writable works", !faults(p, 1));
    check("mprotect over unmapped memory fails with ENOMEM", mprotect(p + 3 * 4096, 4096, PROT_READ) == -1 && errno == ENOMEM);

    /* Partial munmap splits the area. */
    munmap(p + 4096, 4096);
    check("unmapped hole faults, neighbors stay", faults(p + 4096, 0) && !faults(p, 1) && !faults(p + 8192, 1));
    munmap(p, 3 * 4096);

    /* A JIT: reserve with PROT_NONE, commit with mprotect, write code, run it. */
    char *jit = mmap(NULL, 1024 * MIB, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    check("1 GiB PROT_NONE reservation succeeds", jit != MAP_FAILED);
    mprotect(jit, 4096, PROT_READ | PROT_WRITE);
    static const unsigned char code[] = {0xb8, 0x2a, 0x00, 0x00, 0x00, 0xc3}; /* mov eax, 42; ret */
    memcpy(jit, code, sizeof code);
    got = 0;
    if (!sigsetjmp(env, 1)) ((int (*)(void))jit)();
    check("executing non-executable memory raises SIGSEGV", got == SIGSEGV);
    mprotect(jit, 4096, PROT_READ | PROT_EXEC);
    int (*fn)(void) = (int (*)(void))jit;
    check("W^X: write, mprotect to exec, call returns 42", fn() == 42);
    munmap(jit, 1024 * MIB);

    /* Commit accounting: more writable memory than exists is refused. */
    void *huge = mmap(NULL, 64L * 1024 * MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check("64 GiB writable mmap fails with ENOMEM", huge == MAP_FAILED && errno == ENOMEM);
    huge = mmap(NULL, 64L * 1024 * MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    check("... but succeeds with MAP_NORESERVE", huge != MAP_FAILED);
    if (huge != MAP_FAILED) munmap(huge, 64L * 1024 * MIB);

    /* mremap: grow in place or move, contents kept. */
    char *r = mmap(NULL, 2 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    strcpy(r, "remap me");
    r[4096] = 'z';
    mmap(r + 2 * 4096, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0); /* block growth */
    check("mremap without MAYMOVE fails if it cannot grow", mremap(r, 2 * 4096, 8 * 4096, 0) == MAP_FAILED);
    char *moved = mremap(r, 2 * 4096, 8 * 4096, MREMAP_MAYMOVE);
    check("mremap MAYMOVE moves with the contents", moved != MAP_FAILED && moved != r && strcmp(moved, "remap me") == 0 && moved[4096] == 'z');
    check("the old range is gone, the new tail is zero", faults(r, 0) && moved[7 * 4096] == 0);
    char *shrunk = mremap(moved, 8 * 4096, 4096, 0);
    check("mremap shrinks in place", shrunk == moved && faults(moved + 4096, 0));
    munmap(moved, 4096);
    munmap(r + 2 * 4096, 4096);

    /* madvise(MADV_DONTNEED) drops private pages. */
    char *d = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    d[0] = 7;
    madvise(d, 4096, MADV_DONTNEED);
    check("MADV_DONTNEED makes a page read as zero", d[0] == 0);
    munmap(d, 4096);

    /* Shared anonymous memory is shared across fork. */
    volatile int *shared = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    volatile int *priv = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    *shared = 1;
    *priv = 1;
    pid_t child = fork();
    if (child == 0) {
        *shared = 42;
        *priv = 42;
        _exit(0);
    }
    waitpid(child, NULL, 0);
    check("MAP_SHARED anonymous memory is shared with a child", *shared == 42);
    check("MAP_PRIVATE memory stays private", *priv == 1);

    /* File mappings are read lazily; beyond the end of file is SIGBUS. */
    int fd = open("/etc/motd", O_RDONLY);
    char *file = mmap(NULL, 2 * 4096, PROT_READ, MAP_PRIVATE, fd, 0);
    check("file mapping reads the file", file != MAP_FAILED && strncmp(file, "Welcome", 7) == 0);
    check("a page beyond the end of the file raises SIGBUS", faults(file + 4096, 0) && got == SIGBUS);
    munmap(file, 2 * 4096);
    close(fd);

    /* MAP_FIXED_NOREPLACE refuses to overwrite. */
    char *a = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check("MAP_FIXED_NOREPLACE fails with EEXIST", mmap(a, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0) == MAP_FAILED && errno == EEXIST);
    munmap(a, 4096);

    /* The kernel's own accesses to user memory follow the same rules. */
    int pfd[2];
    pipe(pfd);
    char *kb = mmap(NULL, 2 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    write(pfd[1], "lazy", 4);
    check("read into a lazily mapped buffer works", read(pfd[0], kb + 4096 - 2, 4) == 4 && memcmp(kb + 4094, "lazy", 4) == 0);
    mprotect(kb, 4096, PROT_READ);
    write(pfd[1], "ro", 2);
    check("read into a read-only buffer fails with EFAULT", read(pfd[0], kb, 2) == -1 && errno == EFAULT);
    char kept[2] = {0};
    check("... and the data stays in the pipe", read(pfd[0], kept, 2) == 2 && memcmp(kept, "ro", 2) == 0);
    munmap(kb, 2 * 4096);
    kb = mmap(NULL, 2 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    munmap(kb + 4096, 4096);
    write(pfd[1], "hello", 5);
    long part = read(pfd[0], kb + 4096 - 2, 100);
    char rest[8] = {0};
    long more = read(pfd[0], rest, sizeof rest);
    check("a read up to a hole keeps the rest in the pipe", part == 2 && memcmp(kb + 4094, "he", 2) == 0 && more == 3 && memcmp(rest, "llo", 3) == 0);
    munmap(kb, 4096);
    check("write from unmapped memory fails with EFAULT", write(pfd[1], kb, 1) == -1 && errno == EFAULT);
    check("a path in unmapped memory fails with EFAULT", open((char *)kb, O_RDONLY) == -1 && errno == EFAULT);
    close(pfd[0]);
    close(pfd[1]);

    /* The stack grows on demand (here to about 4 MiB). */
    pid_t deep = fork();
    if (deep == 0) _exit(depth(4000) == 0 ? 1 : 0);
    int st;
    waitpid(deep, &st, 0);
    check("the stack grows to 4 MiB on demand", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    pid_t overflow = fork();
    if (overflow == 0) _exit(depth(20000));
    waitpid(overflow, &st, 0);
    check("beyond 8 MiB the stack overflows with SIGSEGV", WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV);

    printf("vmtest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
