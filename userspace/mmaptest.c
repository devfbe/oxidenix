/* File mappings through the page cache: shared mappings see write() and
 * read() sees stores through them, across processes; private mappings
 * see the file until they write; truncation and the end of the file give
 * SIGBUS; a mapping outlives its descriptor and its name. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#define PG 4096

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static sigjmp_buf env;
static volatile sig_atomic_t got;
static void on_fault(int sig) {
    got = sig;
    siglongjmp(env, 1);
}

/* Whether touching `p` raises a signal; which one in `got`. */
static int faults(volatile char *p, int write) {
    got = 0;
    if (!sigsetjmp(env, 1)) {
        if (write) *p = 1;
        else (void)*p;
    }
    return got != 0;
}

/* A fresh file of `pages` pages, page i filled with 'a' + i. */
static int make_file(const char *path, int pages) {
    unlink(path);
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    char buf[PG];
    for (int i = 0; i < pages; i++) {
        memset(buf, 'a' + i, PG);
        if (write(fd, buf, PG) != PG) return -1;
    }
    return fd;
}

static char byte_at(int fd, off_t off) {
    char c = 0;
    return pread(fd, &c, 1, off) == 1 ? c : 0;
}

int main(int argc, char **argv) {
    const char *dir = argc > 1 ? argv[1] : "/tmp";
    char path[256];
    snprintf(path, sizeof path, "%s/mmaptest.file", dir);

    struct sigaction sa = {0};
    sa.sa_handler = on_fault;
    sigaction(SIGBUS, &sa, NULL);
    sigaction(SIGSEGV, &sa, NULL);

    int fd = make_file(path, 4);
    char *s = mmap(NULL, 4 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    check("MAP_SHARED of a file succeeds", s != MAP_FAILED);
    check("the mapping shows the file", s[0] == 'a' && s[3 * PG + 17] == 'd');

    s[PG + 5] = 'X';
    check("a store through a shared mapping is read() at once", byte_at(fd, PG + 5) == 'X');
    pwrite(fd, "Y", 1, 2 * PG + 9);
    check("write() is visible in a shared mapping at once", s[2 * PG + 9] == 'Y');

    struct stat st;
    fstat(fd, &st);
    check("stores through a mapping do not change the size", st.st_size == 4 * PG);

    /* Another process maps the file itself (not inherited). */
    pid_t pid = fork();
    if (pid == 0) {
        int cfd = open(path, O_RDWR);
        char *c = mmap(NULL, PG, PROT_READ | PROT_WRITE, MAP_SHARED, cfd, 0);
        if (c == MAP_FAILED) _exit(2);
        c[100] = 'C';
        _exit(c[0] == 'a' ? 0 : 1);
    }
    int status;
    waitpid(pid, &status, 0);
    check("another process's own mapping shares the pages", WIFEXITED(status) && WEXITSTATUS(status) == 0 && s[100] == 'C');

    /* Private mappings see the file until they write a page. */
    char *p = mmap(NULL, 4 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    check("MAP_PRIVATE of the file shows its contents", p != MAP_FAILED && p[PG + 5] == 'X');
    (void)p[3 * PG];
    pwrite(fd, "Z", 1, 3 * PG);
    check("a private page not yet written sees write()", p[3 * PG] == 'Z');
    p[0] = 'P';
    check("a private store does not reach the file", byte_at(fd, 0) == 'a' && s[0] == 'a');
    pwrite(fd, "Q", 1, 1);
    check("a privately written page no longer sees the file", p[1] == 'a' && p[0] == 'P');

    /* The mapping outlives the descriptor and the name. */
    close(fd);
    unlink(path);
    s[2 * PG] = 'U';
    check("a shared mapping outlives close() and unlink()", s[2 * PG] == 'U' && s[PG + 5] == 'X' && p[PG + 5] == 'X');
    munmap(s, 4 * PG);
    munmap(p, 4 * PG);

    /* Beyond the end of the file. */
    fd = make_file(path, 1);
    pwrite(fd, "tail", 4, PG);
    s = mmap(NULL, 4 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    check("the last partial page reads zero after the end", s[PG] == 't' && s[PG + 4] == 0 && s[2 * PG - 1] == 0);
    s[PG + 100] = 'w';
    check("... and stores there do not reach the file", byte_at(fd, PG + 100) == 0);
    check("a page wholly beyond the end raises SIGBUS", faults(s + 2 * PG, 0) && got == SIGBUS);

    /* Growing the file makes the page reachable; holes read as zero. */
    ftruncate(fd, 3 * PG);
    check("after ftruncate grows the file the page is there", !faults(s + 2 * PG, 0) && s[2 * PG + 1] == 0);
    s[2 * PG + 1] = 'G';
    check("... and a store there reaches the file", byte_at(fd, 2 * PG + 1) == 'G');

    /* Shrinking unmaps, private copies included. */
    p = mmap(NULL, 3 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    p[2 * PG] = 'c';
    ftruncate(fd, PG);
    check("after ftruncate shrinks, a shared page beyond raises SIGBUS", faults(s + 2 * PG, 0) && got == SIGBUS);
    check("... and so does a private copy beyond", faults(p + 2 * PG, 0) && got == SIGBUS);
    check("... while the first page stays", s[0] == 'a' && p[0] == 'a');
    ftruncate(fd, 3 * PG);
    check("growing again reads zero, not the old data", s[2 * PG + 1] == 0 && byte_at(fd, 2 * PG + 1) == 0);
    check("... and the partial page's cut tail stays zero", s[PG + 100] == 0 && byte_at(fd, PG + 100) == 0);
    munmap(s, 4 * PG);
    munmap(p, 3 * PG);
    close(fd);
    unlink(path);

    /* Read-only descriptors. */
    fd = make_file(path, 1);
    close(fd);
    fd = open(path, O_RDONLY);
    check("MAP_SHARED writable on a read-only fd fails with EACCES", mmap(NULL, PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) == MAP_FAILED && errno == EACCES);
    s = mmap(NULL, PG, PROT_READ, MAP_SHARED, fd, 0);
    check("... while read-only sharing works", s != MAP_FAILED && s[0] == 'a');
    check("... and mprotect to writable fails with EACCES", mprotect(s, PG, PROT_READ | PROT_WRITE) == -1 && errno == EACCES);
    p = mmap(NULL, PG, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    p[0] = 'p';
    check("MAP_PRIVATE writable on a read-only fd copies", p[0] == 'p' && s[0] == 'a');
    munmap(s, PG);
    munmap(p, PG);
    close(fd);
    unlink(path);

    /* Shared anonymous memory across fork, and many processes. */
    int *counter = mmap(NULL, PG, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    for (int i = 0; i < 8; i++) {
        if (fork() == 0) {
            __atomic_fetch_add(counter, 1, __ATOMIC_SEQ_CST);
            _exit(0);
        }
    }
    while (wait(NULL) > 0) {}
    check("shared anonymous memory is shared by 8 children", *counter == 8);

    /* Files from the initramfs map like any other. */
    fd = open("/bin/busybox", O_RDONLY);
    char *elf = mmap(NULL, 2 * PG, PROT_READ, MAP_PRIVATE, fd, 0);
    check("an initramfs file maps (ELF magic)", elf != MAP_FAILED && memcmp(elf, "\177ELF", 4) == 0);
    char buf[64];
    pread(fd, buf, sizeof buf, PG + 3);
    check("... with the same bytes read() returns", memcmp(elf + PG + 3, buf, sizeof buf) == 0);
    close(fd);

    printf("%s\n", failures ? "mmaptest: FAILED" : "mmaptest: all passed");
    return failures != 0;
}
