/* The Linux server's kernel interface (docs/design/linux-server.md),
 * exercised through the server's test calls (1500 and up, see
 * crates/restricted): the program asks its server to create a memory
 * object, fill it, map it into the program, protect and unmap it, and
 * checks the effect from the program's side. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <time.h>
#include <pthread.h>
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
#define TEST_PAGED_FAIL 1508
#define TEST_ALLOC 1509
#define TEST_LOCKED_ADD 1510
#define TEST_USERCOPY 1511
#define TEST_FS_VALUE 1512
#define TEST_FS_RECORDS 1513

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

/* The kernel's count of system calls the server passed back to it. */
static long legacy_calls(void) {
    char text[512] = {0};
    int fd = open("/proc/counters", O_RDONLY);
    read(fd, text, sizeof text - 1);
    close(fd);
    char *p = strstr(text, "legacy_calls ");
    return p ? atol(p + 13) : -1;
}

static void *adder(void *arg) {
    (void)arg;
    syscall(TEST_LOCKED_ADD, 2000);
    return NULL;
}

static void *set_eleven(void *arg) {
    (void)arg;
    syscall(TEST_FS_VALUE, 11);
    return NULL;
}

/* The count of the server's records once releases stopped arriving. */
static long settled_records(void) {
    long last = syscall(TEST_FS_RECORDS);
    for (int i = 0; i < 20; i++) {
        usleep(50 * 1000);
        long now = syscall(TEST_FS_RECORDS);
        if (now == last) break;
        last = now;
    }
    return last;
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

    /* A page the pager fails: SIGBUS, and a later access asks again. */
    char *fl = (char *)0x230000000000;
    check("the server maps a paged object it fails once", syscall(TEST_PAGED_FAIL, fl) == 0);
    child = fork();
    if (child == 0) {
        (void)*(volatile char *)fl;
        _exit(0);
    }
    alarm(5);
    waitpid(child, &st, 0);
    alarm(0);
    check("a page the pager fails raises SIGBUS", WIFSIGNALED(st) && WTERMSIG(st) == SIGBUS);
    check("... and a later access asks again and gets it", memcmp(fl, "retry", 5) == 0);
    /* The server's runtime: its heap, and its mutex across the threads of
     * the tree's processes (they all run the same server instance). */
    check("the server's heap: 2000 blocks of many sizes keep their contents", syscall(TEST_ALLOC, 2000) == 0);
    long start = syscall(TEST_LOCKED_ADD, 0);
    pid_t kid = fork();
    pthread_t th[3];
    for (int i = 0; i < 3; i++) pthread_create(&th[i], NULL, adder, NULL);
    for (int i = 0; i < 3; i++) pthread_join(th[i], NULL);
    if (kid == 0) _exit(0);
    waitpid(kid, &st, 0);
    long end = syscall(TEST_LOCKED_ADD, 0);
    printf("    (counter %ld -> %ld)\n", start, end);
    check("the server's mutex serializes 6 threads in 2 processes", end - start == 6 * 2000);
    /* Memory semantics are the server's (R4): mmap and friends no longer
     * pass through to the kernel's Linux implementation. */
    long idle = legacy_calls();
    long base = legacy_calls() - idle;
    long l0 = legacy_calls();
    for (int i = 0; i < 100; i++) {
        char *m = mmap(NULL, 3 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        m[PG] = 1;
        mprotect(m, PG, PROT_READ);
        munmap(m, 3 * PG);
    }
    long passed = legacy_calls() - l0 - base;
    printf("    (%ld of 300 memory calls passed through)\n", passed);
    check("mmap, mprotect and munmap are the server's (no pass-through)", passed == 0);

    /* Time is the server's too (R5), and it writes the program's memory
     * directly: faults there are the program's, bad addresses EFAULT. */
    l0 = legacy_calls();
    struct timespec ts, z = {0, 0};
    struct timeval tv;
    for (int i = 0; i < 100; i++) {
        clock_gettime(CLOCK_MONOTONIC, &ts);
        gettimeofday(&tv, NULL);
        nanosleep(&z, NULL);
    }
    passed = legacy_calls() - l0 - base;
    check("clock_gettime, gettimeofday and nanosleep are the server's", passed == 0 && ts.tv_sec >= 0 && tv.tv_sec > 1000000000);
    char *fresh = mmap(NULL, 2 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check("the server writes a page the program never touched", clock_gettime(CLOCK_REALTIME, (struct timespec *)(fresh + PG)) == 0 && *(long *)(fresh + PG) > 1000000000);
    mprotect(fresh, PG, PROT_READ);
    errno = 0;
    check("a read-only page is EFAULT, not death", clock_gettime(CLOCK_MONOTONIC, (struct timespec *)fresh) == -1 && errno == EFAULT);
    errno = 0;
    check("an unmapped address is EFAULT", clock_gettime(CLOCK_MONOTONIC, (struct timespec *)16) == -1 && errno == EFAULT);
    errno = 0;
    check("the server's own memory is EFAULT", clock_gettime(CLOCK_MONOTONIC, (struct timespec *)0x404000009000) == -1 && errno == EFAULT);
    errno = 0;
    check("... and across the 64 TiB line", syscall(TEST_USERCOPY, 0x400000000000 - 4) == -1 && errno == EFAULT);
    char *none = mmap(NULL, PG, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    errno = 0;
    check("a PROT_NONE page is EFAULT for the server's copy", syscall(TEST_USERCOPY, none) == -1 && errno == EFAULT);
    check("the server's copy reaches program memory", syscall(TEST_USERCOPY, fresh + PG) == 0 && memcmp(fresh + PG, "usercopy", 8) == 0);

    /* Pipes are the server's (R6a): reads and writes pass nothing through,
     * the kernel's table still holds their descriptors. */
    int q[2];
    check("pipe2 gives two descriptors", pipe2(q, O_CLOEXEC) == 0 && q[0] >= 0 && q[1] > q[0] && (fcntl(q[0], F_GETFD) & FD_CLOEXEC));
    l0 = legacy_calls();
    int same = 1;
    for (int i = 0; i < 100; i++) {
        char c = (char)i, d = 0;
        write(q[1], &c, 1);
        read(q[0], &d, 1);
        same &= d == c;
    }
    passed = legacy_calls() - l0 - base;
    printf("    (%ld of 200 pipe calls passed through)\n", passed);
    check("pipe reads and writes are the server's", passed == 0 && same);
    close(q[1]);
    check("... end of file once the writer is closed", read(q[0], buf, 1) == 0);
    close(q[0]);

    /* eventfd too (R6b). */
    int efd = eventfd(5, EFD_NONBLOCK);
    uint64_t v = 0;
    l0 = legacy_calls();
    int counted = 1;
    for (int i = 0; i < 50; i++) {
        uint64_t one = 1;
        write(efd, &one, 8);
        counted &= read(efd, &v, 8) == 8;
    }
    passed = legacy_calls() - l0 - base;
    check("eventfd reads and writes are the server's", passed == 0 && counted && v == 1);
    errno = 0;
    check("... an empty one is EAGAIN when non-blocking", read(efd, &v, 8) == -1 && errno == EAGAIN);
    close(efd);

    /* Records per working-directory context (R6c): a fork gets a copy of
     * its parent's, a thread shares its process's, and the kernel releases
     * the record of a context that ended. */
    syscall(TEST_FS_VALUE, 7);
    pid_t c = fork();
    if (c == 0) {
        long seen = syscall(TEST_FS_VALUE, 0);
        syscall(TEST_FS_VALUE, 9);
        _exit(seen == 7 ? 0 : 1);
    }
    waitpid(c, &st, 0);
    check("a forked child gets a copy of its parent's record", WIFEXITED(st) && WEXITSTATUS(st) == 0 && syscall(TEST_FS_VALUE, 0) == 7);
    pthread_t t;
    pthread_create(&t, NULL, set_eleven, NULL);
    pthread_join(t, NULL);
    check("a thread shares its process's record", syscall(TEST_FS_VALUE, 0) == 11);
    long recs = settled_records();
    for (int i = 0; i < 10; i++) {
        if ((c = fork()) == 0) _exit(0);
        waitpid(c, &st, 0);
    }
    long after = settled_records();
    printf("    (records %ld -> %ld after 10 forks)\n", recs, after);
    check("the kernel releases the records of ended processes", after <= recs);

    /* Paths are the server's (R6c.2b): resolution, the working directory
     * and umask live in it; the kernel's tree answers through handles. */
    struct stat sb;
    char cwd[256];
    l0 = legacy_calls();
    int found = 1;
    for (int i = 0; i < 50; i++) {
        found &= stat("/etc/runtests.sh", &sb) == 0 && access("/bin", F_OK) == 0 && getcwd(cwd, sizeof cwd) != NULL;
    }
    passed = legacy_calls() - l0 - base;
    printf("    (%ld of 150 path calls passed through)\n", passed);
    check("stat, access and getcwd are the server's", passed == 0 && found && S_ISREG(sb.st_mode));
    check("stat of the root", stat("/", &sb) == 0 && S_ISDIR(sb.st_mode) && stat("/..", &sb) == 0 && S_ISDIR(sb.st_mode));
    mkdir("/tmp/lx", 0777);
    check("chdir and getcwd", chdir("/tmp/lx") == 0 && getcwd(cwd, sizeof cwd) && strcmp(cwd, "/tmp/lx") == 0);
    mode_t old_mask = umask(077);
    int fd = open("f", O_CREAT | O_WRONLY | O_TRUNC, 0666);
    check("umask applies to a new file (0666 & ~077)", fd >= 0 && write(fd, "abc", 3) == 3 && stat("f", &sb) == 0 && (sb.st_mode & 0777) == 0600 && sb.st_size == 3);
    close(fd);
    check("umask returns the old mask", umask(old_mask) == 077);
    errno = 0;
    check("O_EXCL on an existing file is EEXIST", open("f", O_CREAT | O_EXCL | O_WRONLY, 0666) == -1 && errno == EEXIST);
    char link[64] = {0};
    check("symlink and readlink", symlink("f", "l") == 0 && readlink("l", link, sizeof link) == 1 && link[0] == 'f');
    check("stat follows a symlink, lstat does not", stat("l", &sb) == 0 && S_ISREG(sb.st_mode) && lstat("l", &sb) == 0 && S_ISLNK(sb.st_mode));
    errno = 0;
    check("O_NOFOLLOW on a symlink is ELOOP", open("l", O_RDONLY | O_NOFOLLOW) == -1 && errno == ELOOP);
    symlink("loop2", "/tmp/lx/loop1");
    symlink("loop1", "/tmp/lx/loop2");
    errno = 0;
    check("a symlink loop is ELOOP", stat("/tmp/lx/loop1", &sb) == -1 && errno == ELOOP);
    check("a path through a symlinked directory", symlink("/tmp/lx", "d") == 0 && stat("d/d/d/f", &sb) == 0 && S_ISREG(sb.st_mode));
    int dfd = open("/tmp", O_RDONLY | O_DIRECTORY);
    check("openat relative to a directory descriptor", dfd >= 0 && fstatat(dfd, "lx/f", &sb, 0) == 0 && sb.st_size == 3);
    check("rename", rename("f", "g") == 0 && stat("g", &sb) == 0 && stat("f", &sb) == -1);
    c = fork();
    if (c == 0) {
        int ok = getcwd(cwd, sizeof cwd) && strcmp(cwd, "/tmp/lx") == 0;
        chdir("/");
        _exit(ok ? 0 : 1);
    }
    waitpid(c, &st, 0);
    check("a child inherits the working directory, its chdir stays its own",
          WIFEXITED(st) && WEXITSTATUS(st) == 0 && getcwd(cwd, sizeof cwd) && strcmp(cwd, "/tmp/lx") == 0);
    check("fchdir", fchdir(dfd) == 0 && getcwd(cwd, sizeof cwd) && strcmp(cwd, "/tmp") == 0);
    close(dfd);
    errno = 0;
    check("rmdir of a non-empty directory fails", rmdir("lx") == -1 && errno == ENOTEMPTY);
    unlink("lx/g"); unlink("lx/l"); unlink("lx/d"); unlink("lx/loop1"); unlink("lx/loop2");
    check("unlink and rmdir", rmdir("lx") == 0 && stat("lx", &sb) == -1 && errno == ENOENT);
    chdir("/");
    printf("lxtest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
