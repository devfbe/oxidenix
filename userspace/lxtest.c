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
#include <unistd.h>

#define PG 4096
#define TEST_MAP 1500
#define TEST_READ 1501
#define TEST_PROTECT 1502
#define TEST_UNMAP 1503
#define TEST_MAP_AT 1504

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

    printf("lxtest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
