/* The Linux server's kernel interface (docs/design/linux-server.md),
 * exercised through the server's test calls (1500 and up, see
 * crates/restricted): the program asks its server to create a memory
 * object, fill it, map it into the program, protect and unmap it, and
 * checks the effect from the program's side. It may run any number of
 * times in one boot, beside other programs; `lxtest crashloop` checks the
 * kernel's restart policy on a service that keeps dying (ADR 0006). */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <dirent.h>
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

#include "rwtest.h"

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
#define TEST_CHANNEL 1514
#define TEST_DISKRING 1515
#define TEST_CACHED 1516
#define TEST_PASS_THROUGH 1517

static int failures;

/* CPU time (user + system, in clock ticks) of the process named `name`,
 * from /proc/<pid>/stat; -1 if there is none. */
static long proc_ticks(const char *name) {
    DIR *d = opendir("/proc");
    struct dirent *e;
    long ticks = -1;
    while (d && (e = readdir(d))) {
        if (e->d_name[0] < '1' || e->d_name[0] > '9') continue;
        char path[64], buf[512];
        snprintf(path, sizeof path, "/proc/%s/stat", e->d_name);
        int fd = open(path, O_RDONLY);
        if (fd < 0) continue;
        ssize_t n = read(fd, buf, sizeof buf - 1);
        close(fd);
        if (n <= 0) continue;
        buf[n] = 0;
        char *p = strchr(buf, '('), *q = strrchr(buf, ')');
        if (!p || !q || (size_t)(q - p - 1) != strlen(name) || strncmp(p + 1, name, q - p - 1) != 0) continue;
        unsigned long ut, st;
        if (sscanf(q + 2, "%*c %*d %*d %*d %*d %*d %*u %*u %*u %*u %*u %lu %lu", &ut, &st) == 2) ticks = (long)(ut + st);
    }
    if (d) closedir(d);
    return ticks;
}

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static sigjmp_buf env;
static volatile int got;

static volatile int sigpipes;
static void on_sigpipe(int sig) {
    (void)sig;
    sigpipes++;
}

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

/* The kernel's count of this process's system calls the server passed back
 * to it (its own, not /proc/counters' for all: other programs running
 * meanwhile do not count). */
static long legacy_calls(void) {
    char text[512] = {0};
    int fd = open("/proc/self/counters", O_RDONLY);
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

/* How many watched records the kernel still holds, once it released them
 * all or a second went by (releases arrive as events of the pager). */
static long held_records(void) {
    long held = syscall(TEST_FS_RECORDS, 0);
    for (int i = 0; i < 20 && held != 0; i++) {
        usleep(50 * 1000);
        held = syscall(TEST_FS_RECORDS, 0);
    }
    return held;
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "crashloop") == 0) {
        /* The test service dies at every use: the kernel restarts it with
         * a growing backoff, takes it down after six young deaths in a
         * row, and brings it back after the cooldown (some 8.5 s). */
        errno = 0;
        long r = syscall(TEST_CHANNEL, 8);
        if (r != 0) printf("    (scenario 8: check %d failed)\n", errno);
        check("channels: a crash loop: backoff, down (EIO), up after the cooldown", r == 0);
        return failures != 0;
    }
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

    /* A page the pager fails: SIGBUS, and a later access asks again. The
     * first request of every such object fails, not only the instance's
     * first (a second object here, as a second lxtest run in one shell),
     * and each request is the object's own: both exist before the first
     * is touched. */
    check("the server maps a paged object it fails once", syscall(TEST_PAGED_FAIL, (char *)0x230000000000) == 0);
    check("a second such object", syscall(TEST_PAGED_FAIL, (char *)0x231000000000) == 0);
    for (int round = 0; round < 2; round++) {
        char *fl = (char *)(0x230000000000 + round * 0x1000000000L);
        child = fork();
        if (child == 0) {
            (void)*(volatile char *)fl;
            _exit(0);
        }
        alarm(5);
        waitpid(child, &st, 0);
        alarm(0);
        check(round ? "the second object's page fails too: SIGBUS" : "a page the pager fails raises SIGBUS", WIFSIGNALED(st) && WTERMSIG(st) == SIGBUS);
        check(round ? "... and a later access gets it" : "... and a later access asks again and gets it", memcmp(fl, "retry", 5) == 0);
    }
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
    pid_t me = getpid();
    long c0 = legacy_calls();
    int own = 1;
    for (int i = 0; i < 5; i++) own &= syscall(TEST_PASS_THROUGH) == me;
    long on_purpose = legacy_calls() - c0 - base;
    printf("    (5 calls passed through on purpose counted as %ld)\n", on_purpose);
    check("/proc/self/counters counts the process's own passed-through calls", idle >= 0 && own && on_purpose == 5);
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
    /* The descriptor's ioctls (libuv makes Node's stdio pipes non-blocking
     * with FIONBIO): on the kernel's flags of the server's pipe. */
    int on = 1, off = 0;
    errno = 0;
    check("FIONBIO makes an empty pipe EAGAIN", ioctl(q[0], FIONBIO, &on) == 0 && (fcntl(q[0], F_GETFL) & O_NONBLOCK)
          && read(q[0], buf, 1) == -1 && errno == EAGAIN);
    check("... and blocking again", ioctl(q[0], FIONBIO, &off) == 0 && !(fcntl(q[0], F_GETFL) & O_NONBLOCK));
    check("FIONCLEX and FIOCLEX set the close-on-exec flag", ioctl(q[0], FIONCLEX) == 0 && !(fcntl(q[0], F_GETFD) & FD_CLOEXEC)
          && ioctl(q[0], FIOCLEX) == 0 && (fcntl(q[0], F_GETFD) & FD_CLOEXEC));
    check("... other ioctls on a pipe are ENOTTY", ioctl(q[0], TCGETS, buf) == -1 && errno == ENOTTY);
    errno = 0;
    check("... FIONBIO on a bad descriptor is EBADF (not EFAULT)", ioctl(999, FIONBIO, NULL) == -1 && errno == EBADF);
    close(q[1]);
    check("... end of file once the writer is closed", read(q[0], buf, 1) == 0);
    close(q[0]);
    pipe(q);
    close(q[0]);
    sigpipes = 0;
    signal(SIGPIPE, on_sigpipe);
    errno = 0;
    check("... a write without readers: EPIPE and SIGPIPE", write(q[1], "x", 1) == -1 && errno == EPIPE && sigpipes == 1);
    signal(SIGPIPE, SIG_DFL);
    close(q[1]);

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
    /* The children's own records, watched (not the count of all the
     * instance's records, which other processes change). */
    int hold[2];
    pipe(hold);
    pid_t holder = fork();
    if (holder == 0) {
        char x;
        close(hold[1]);
        syscall(TEST_FS_RECORDS, 1);
        read(hold[0], &x, 1);
        _exit(0);
    }
    close(hold[0]);
    long live = 0;
    for (int i = 0; i < 20 && live == 0; i++) {
        usleep(10 * 1000);
        live = syscall(TEST_FS_RECORDS, 0);
    }
    check("a running child's record is held", live == 1);
    close(hold[1]);
    waitpid(holder, &st, 0);
    for (int i = 0; i < 9; i++) {
        if ((c = fork()) == 0) _exit(syscall(TEST_FS_RECORDS, 1) == 0 ? 0 : 1);
        waitpid(c, &st, 0);
    }
    long held = held_records();
    printf("    (%ld of 10 ended children's records still held)\n", held);
    check("the kernel releases the records of ended processes", held == 0);

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
    symlink("nowhere", "dangling");
    errno = 0;
    check("O_CREAT through a dangling symlink fails (no endless retry)", open("dangling", O_CREAT | O_WRONLY, 0666) == -1 && errno == EEXIST);
    unlink("dangling");
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

    /* The root is the server's own tmpfs (R6c.2c), unpacked from the
     * initramfs: its files are file objects the server reads, writes and
     * maps without the kernel's VFS. */
    int tf = open("/tmp/lxfile", O_CREAT | O_RDWR | O_TRUNC, 0644);
    check("a /tmp file is the server's tmpfs (its own device)", tf >= 0 && fstat(tf, &sb) == 0 && sb.st_dev == 0x1a && S_ISREG(sb.st_mode));
    char block[4096];
    memset(block, 'z', sizeof block);
    l0 = legacy_calls();
    int io = 1;
    for (int i = 0; i < 25; i++) {
        io &= pwrite(tf, block, sizeof block, (off_t)i * 4096) == 4096;
        io &= pread(tf, cwd, 8, (off_t)i * 4096 + 100) == 8 && cwd[0] == 'z';
        io &= lseek(tf, 0, SEEK_END) == 25 * 4096 || i < 24;
        io &= fstat(tf, &sb) == 0;
    }
    passed = legacy_calls() - l0 - base;
    printf("    (%ld of 100 /tmp file calls passed through)\n", passed);
    check("reads, writes, lseek and fstat of a /tmp file are the server's", passed == 0 && io && sb.st_size == 25 * 4096);
    rw_flag_checks("/tmp/lxrw", check);
    l0 = legacy_calls();
    io = 1;
    for (int i = 0; i < 25; i++) {
        io &= rw_pwritev2(tf, "v2", (long)i * 4096, RWF_NOAPPEND) == 2;
        io &= rw_preadv2(tf, cwd, 2, (long)i * 4096, 0) == 2;
    }
    passed = legacy_calls() - l0 - base;
    printf("    (%ld of 50 preadv2/pwritev2 calls passed through)\n", passed);
    check("preadv2 and pwritev2 of a /tmp file are the server's", passed == 0 && io);
    check("O_APPEND writes at the end", (fd = open("/tmp/lxfile", O_WRONLY | O_APPEND)) >= 0 && write(fd, "end", 3) == 3 && lseek(tf, 0, SEEK_END) == 25 * 4096 + 3);
    close(fd);
    char *map = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, tf, 0);
    if (map != MAP_FAILED) map[0] = 'M';
    check("a shared mapping of a /tmp file writes the file", map != MAP_FAILED && pread(tf, cwd, 1, 0) == 1 && cwd[0] == 'M');
    if (map != MAP_FAILED) munmap(map, 4096);
    check("ftruncate", ftruncate(tf, 10) == 0 && fstat(tf, &sb) == 0 && sb.st_size == 10);
    close(tf);
    mkdir("/tmp/lxdir", 0755);
    close(open("/tmp/lxdir/one", O_CREAT | O_WRONLY, 0644));
    close(open("/tmp/lxdir/two", O_CREAT | O_WRONLY, 0644));
    DIR *d = opendir("/tmp/lxdir");
    int names = 0;
    struct dirent *e;
    while (d && (e = readdir(d))) names += strcmp(e->d_name, "one") == 0 || strcmp(e->d_name, "two") == 0;
    if (d) closedir(d);
    check("readdir of a /tmp directory", names == 2);
    errno = 0;
    check("rename between the server's tmpfs and /data is EXDEV", rename("/tmp/lxfile", "/data/lxfile") == -1 && errno == EXDEV);
    errno = 0;
    check("removing the mount point /proc is EBUSY", rmdir("/proc") == -1 && errno == EBUSY);
    check("the root and its programs are the server's tmpfs (from the initramfs)",
          stat("/", &sb) == 0 && sb.st_dev == 0x1a && stat("/bin/busybox", &sb) == 0 && sb.st_dev == 0x1a && S_ISREG(sb.st_mode));
    check("/proc and /dev are the kernel's", stat("/proc/counters", &sb) == 0 && sb.st_dev != 0x1a && stat("/dev/null", &sb) == 0 && S_ISCHR(sb.st_mode));
    unlink("/tmp/lxdir/one"); unlink("/tmp/lxdir/two"); rmdir("/tmp/lxdir"); unlink("/tmp/lxfile");
    check("unlinked /tmp files are gone", stat("/tmp/lxdir", &sb) == -1 && stat("/tmp/lxfile", &sb) == -1);

    /* Channels to a device server (I/O rings): the server opens them to
     * the test service (servers/ringtest) and reports the first check
     * that failed, if any. */
    static const char *scenarios[] = {
        "channels: rings and doorbells between the server and a service",
        "channels: grants (data, read-only, bounds, pinning, device addresses)",
        "channels: revoking a grant (and one a device may still reach)",
        "channels: the client's end goes (grants gone from the service)",
        "channels: the service dies (the client wakes; it comes back)",
        "channels: the service execs (its new program reaches no grant)",
        "channels: a connect is done when the service attached (no answer yet)",
    };
    for (int i = 0; i < 7; i++) {
        /* A failed check n comes back as -n: errno n. */
        errno = 0;
        long r = syscall(TEST_CHANNEL, i + 1);
        if (r != 0) printf("    (scenario %d: check %d failed)\n", i + 1, errno);
        check(scenarios[i], r == 0);
    }

    /* The file protocol over a channel to diskfs (I/O rings step 3): the
     * server reads and writes /data by DMA into and out of its granted
     * pages and reports the first check that failed, if any. */
    static const char *disk_scenarios[] = {
        "diskfs ring: a disk file read by DMA into a grant, metadata",
        "diskfs ring: writes, flush, read back, truncate, rename",
        "diskfs ring: malformed requests complete with errors",
        "diskfs ring: reads and writes in flight, any order",
        "diskfs ring: revoked grant and a client gone mid-flight",
        "diskfs ring: an unlinked inode freed only when no client holds it",
        "diskfs ring: a stalled write with every operation slot busy",
    };
    for (int i = 0; i < 7; i++) {
        errno = 0;
        long r = syscall(TEST_DISKRING, i + 1);
        if (r != 0) printf("    (scenario %d: check %d failed)\n", i + 1, errno);
        check(disk_scenarios[i], r == 0);
    }
    /* Requests waiting for room in their completion ring keep diskfs
     * asleep, not spinning: scenario 8 leaves them, 9 takes them. */
    {
        errno = 0;
        long r = syscall(TEST_DISKRING, 8);
        if (r != 0) printf("    (scenario 8: check %d failed)\n", errno);
        long before = proc_ticks("diskfs");
        usleep(500 * 1000);
        long spent = proc_ticks("diskfs") - before;
        if (spent > 5) printf("    (diskfs used %ld ticks in 500 ms)\n", spent);
        check("diskfs ring: requests waiting for room do not keep diskfs busy", r == 0 && before >= 0 && spent <= 5);
        errno = 0;
        r = syscall(TEST_DISKRING, 9);
        if (r != 0) printf("    (scenario 9: check %d failed)\n", errno);
        check("diskfs ring: they complete once the client makes room", r == 0);
    }
    /* What the ring wrote (and flushed), read through /data (the server's
     * page cache, over its own channel): byte i is i % 251. */
    {
        int fd = open("/data/ringtest.bin", O_RDONLY);
        static unsigned char ring_buf[70000];
        ssize_t n = fd >= 0 ? read(fd, ring_buf, sizeof ring_buf) : -1;
        int same = n == (ssize_t)sizeof ring_buf;
        for (ssize_t i = 0; same && i < n; i++) same = ring_buf[i] == (unsigned char)(i % 251);
        if (fd >= 0) close(fd);
        check("diskfs ring: a ring write read through /data", same);
        check("diskfs ring: its file removed through /data", unlink("/data/ringtest.bin") == 0);
    }
    /* The kernel's interface of the server's page cache. */
    static const char *cached_scenarios[] = {
        "page cache: a failed fill beyond the end leaves no trace",
        "page cache: a long write-back scan goes on where the kernel says",
        "page cache: a truncation does not wait for ever for a pinned page",
        "page cache: a sync across instances hands out tickets and waits",
    };
    for (int i = 0; i < 4; i++) {
        errno = 0;
        long r = syscall(TEST_CACHED, i + 1);
        if (r != 0) printf("    (scenario %d: check %d failed)\n", i + 1, errno);
        check(cached_scenarios[i], r == 0);
    }
    printf("lxtest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
