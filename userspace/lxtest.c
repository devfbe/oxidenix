/* The Linux server's kernel interface (docs/design/linux-server.md),
 * exercised through the server's test calls (1500 and up, see
 * crates/restricted): the program asks its server to create a memory
 * object, fill it, map it into the program, protect and unmap it, and
 * checks the effect from the program's side. */
#define _GNU_SOURCE
#include <errno.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define PG 4096
#define TEST_MAP 1500
#define TEST_READ 1501
#define TEST_PROTECT 1502
#define TEST_UNMAP 1503
#define TEST_MAP_AT 1504
#define TEST_PAGED 1505
#define TEST_SUPPLIED 1506
#define TEST_PAGED_STUCK 1507

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static sigjmp_buf env;
static volatile int got;

static void on_fault(int sig) {
    got = sig;
    siglongjmp(env, 1);
}

static int faults(volatile char *p, int write) {
    got = 0;
    if (!sigsetjmp(env, 1)) {
        if (write) *p = 1;
        else (void)*p;
    }
    return got == SIGSEGV;
}

int main(void) {
    signal(SIGSEGV, on_fault);
    char *at = (char *)0x200000000000;

    long r = syscall(TEST_MAP, at);
    check("the server maps a memory object it filled into the program", r == 0 && memcmp(at + PG, "linux server", 12) == 0);
    check("... zero elsewhere, and writable", at[0] == 0 && at[3 * PG - 1] == 0 && !faults(at, 1));
    at[0] = 'x';
    check("a store of the program reaches the object (server reads it)", syscall(TEST_READ, 0) == 'x');

    check("the server write-protects the mapping", syscall(TEST_PROTECT, at) == 0 && faults(at, 1) && !faults(at, 0) && at[0] == 'x');
    check("the server unmaps it", syscall(TEST_UNMAP, at) == 0 && faults(at, 0) && faults(at + 2 * PG, 0));

    errno = 0;
    r = syscall(TEST_MAP_AT, (char *)0x400000000000);
    check("a mapping at the server's region is refused (EINVAL)", r == -1 && errno == EINVAL);
    errno = 0;
    r = syscall(TEST_MAP_AT, at + 1);
    check("an unaligned mapping is refused (EINVAL)", r == -1 && errno == EINVAL);

    /* A paged object: the server's pager thread supplies each page when
     * it is first needed, by the program or by the kernel. */
    char *pg = (char *)0x210000000000;
    long before = syscall(TEST_SUPPLIED);
    check("the server maps a paged object", syscall(TEST_PAGED, pg) == 0 && syscall(TEST_SUPPLIED) == before);
    int p[2];
    char buf[8] = {0};
    pipe(p);
    check("the kernel copies from a page the pager supplies (write)", write(p[1], pg + 2 * PG, 7) == 7 && read(p[0], buf, 7) == 7 && memcmp(buf, "paged 2", 7) == 0);
    check("the program reads pages the pager supplies",
          memcmp(pg, "paged 0", 7) == 0 && memcmp(pg + 3 * PG, "paged 3", 7) == 0 && memcmp(pg + PG, "paged 1", 7) == 0);
    check("... each page once", pg[2 * PG] == 'p' && syscall(TEST_SUPPLIED) - before == 4);

    /* A page that never comes: SIGKILL still ends the waiting thread. */
    pid_t child = fork();
    if (child == 0) {
        char *stuck = (char *)0x220000000000;
        if (syscall(TEST_PAGED_STUCK, stuck) != 0) _exit(1);
        (void)*(volatile char *)stuck;
        _exit(2);
    }
    usleep(200 * 1000);
    kill(child, SIGKILL);
    signal(SIGALRM, SIG_DFL);
    alarm(5);
    int st = 0;
    waitpid(child, &st, 0);
    alarm(0);
    check("SIGKILL ends a thread waiting for a page", WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL);
    printf("lxtest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
