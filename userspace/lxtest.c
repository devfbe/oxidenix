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
#include <sched.h>
#include <sys/resource.h>
#include <sys/statfs.h>
#include <sys/sysmacros.h>
#include <sys/sysinfo.h>
#include <sys/utsname.h>
#include <sys/random.h>
#include <linux/futex.h>
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
/* 1517 was TEST_PASS_THROUGH: nothing passes through to the kernel since R9. */
#define TEST_SERVER_TICKS 1518
#define TEST_MKWRITE_FAIL 1519
#define TEST_SERVER_FAIL 1520
#define TEST_SLEEP_LOCKED 1521
#define TEST_HOST 1522

static int failures;

/* CPU time (user + system, in clock ticks) of the process named `name`,
 * from /proc/<pid>/stat; -1 if there is none (a process of another tree:
 * /proc shows the caller's tree's only). */
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

/* A thread's TLS pointer (the FS base), as arch_prctl(ARCH_GET_FS) reads it. */
static unsigned long fs_base(void) {
    unsigned long base = 0;
    syscall(SYS_arch_prctl, 0x1003, &base);
    return base;
}

/* arch_prctl without libc: while the FS base is moved, nothing may use the
 * thread pointer (errno, locks), so these are raw system calls. */
static long raw_arch_prctl(long code, unsigned long addr) {
    long r;
    __asm__ volatile("syscall" : "=a"(r) : "a"((long)SYS_arch_prctl), "D"(code), "S"(addr) : "rcx", "r11", "memory");
    return r;
}

/* Moves the FS base by `delta`, reads it, puts it back: whether all of it
 * worked and the base read was the moved one. */
static int fs_moved(unsigned long tls, unsigned long delta) {
    unsigned long seen = 0;
    long set = raw_arch_prctl(0x1002, tls + delta);
    long got = raw_arch_prctl(0x1003, (unsigned long)&seen);
    long back = raw_arch_prctl(0x1002, tls);
    return set == 0 && got == 0 && back == 0 && seen == tls + delta;
}

static int futex_word;

static int64_t now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static void *futex_waker(void *arg) {
    (void)arg;
    usleep(20 * 1000);
    __atomic_store_n(&futex_word, 1, __ATOMIC_SEQ_CST);
    syscall(SYS_futex, &futex_word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
    return NULL;
}

/* R9: every Linux call is the server's; the kernel's mechanisms under the
 * last ones it implemented (futexes on program memory, the FS base, the wall
 * clock, power, the system's record) and /dev, now the server's devtmpfs. */
static void r9_checks(void) {
    errno = 0;
    check("nothing passes through: a call the server does not implement is ENOSYS", syscall(335) == -1 && errno == ENOSYS);
    errno = 0;
    check("... and the test call that passed one through is gone", syscall(1517) == -1 && errno == ENOSYS);
    struct timespec short_wait = {0, 2 * 1000 * 1000};
    int word = 7;
    errno = 0;
    check("futex: a wait on a word that changed is EAGAIN", syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 8, NULL, NULL, 0) == -1 && errno == EAGAIN);
    errno = 0;
    check("futex: a wait with a timeout ends with ETIMEDOUT", syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 7, &short_wait, NULL, 0) == -1 && errno == ETIMEDOUT);
    errno = 0;
    check("futex: an absolute deadline in the past (FUTEX_WAIT_BITSET) too",
          syscall(SYS_futex, &word, FUTEX_WAIT_BITSET_PRIVATE, 7, &(struct timespec){0, 0}, NULL, FUTEX_BITSET_MATCH_ANY) == -1 && errno == ETIMEDOUT);
    errno = 0;
    check("futex: a zero bitset is EINVAL", syscall(SYS_futex, &word, FUTEX_WAIT_BITSET_PRIVATE, 7, NULL, NULL, 0) == -1 && errno == EINVAL);
    errno = 0;
    check("futex: an unaligned word is EINVAL", syscall(SYS_futex, (char *)&word + 1, FUTEX_WAKE, 1, NULL, NULL, 0) == -1 && errno == EINVAL);
    errno = 0;
    check("futex: the server's memory is EFAULT", syscall(SYS_futex, (int *)0x404000009000, FUTEX_WAKE, 1, NULL, NULL, 0) == -1 && errno == EFAULT);
    pthread_t waker;
    futex_word = 0;
    pthread_create(&waker, NULL, futex_waker, NULL);
    long waited = 0;
    while (__atomic_load_n(&futex_word, __ATOMIC_SEQ_CST) == 0 && waited == 0)
        waited = syscall(SYS_futex, &futex_word, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0);
    pthread_join(waker, NULL);
    check("futex: a waiter is woken by another thread's FUTEX_WAKE", futex_word == 1 && (waited == 0 || errno == EAGAIN));
    check("futex: a wake with no waiter wakes none", syscall(SYS_futex, &word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0) == 0);
    unsigned long tls = fs_base();
    errno = 0;
    check("arch_prctl: ARCH_GET_FS is the TLS pointer, a pointer above 64 TiB EPERM",
          tls != 0 && tls == (unsigned long)pthread_self() && syscall(SYS_arch_prctl, 0x1002, 0x400000000000UL) == -1 && errno == EPERM && fs_base() == tls);
    check("arch_prctl: ARCH_SET_FS and back", fs_moved(tls, 4096) && fs_base() == tls);
    struct utsname u;
    check("uname: oxidenix on x86_64", uname(&u) == 0 && strcmp(u.sysname, "oxidenix") == 0 && strcmp(u.machine, "x86_64") == 0);
    struct sysinfo si;
    check("sysinfo: uptime, memory and processes", sysinfo(&si) == 0 && si.totalram > 0 && si.freeram <= si.totalram && si.procs > 0 && si.mem_unit == 1);
    unsigned char rnd[600] = {0};
    check("getrandom fills more than one piece", getrandom(rnd, sizeof rnd, 0) == (ssize_t)sizeof rnd && memcmp(rnd + 300, rnd + 400, 16) != 0);
    errno = 0;
    check("... GRND_RANDOM with GRND_INSECURE is EINVAL", getrandom(rnd, 1, GRND_RANDOM | 4) == -1 && errno == EINVAL);
    unsigned cpu = 99;
    check("getcpu names a CPU that runs", syscall(SYS_getcpu, &cpu, NULL, NULL) == 0 && cpu < (unsigned)sysconf(_SC_NPROCESSORS_ONLN));
    struct timespec mono;
    clock_gettime(CLOCK_MONOTONIC, &mono);
    errno = 0;
    check("clock_settime of the monotonic clock is EINVAL", clock_settime(CLOCK_MONOTONIC, &mono) == -1 && errno == EINVAL);
    struct timespec wall, back;
    clock_gettime(CLOCK_REALTIME, &wall);
    struct timespec later = {wall.tv_sec + 3600, wall.tv_nsec};
    int set = clock_settime(CLOCK_REALTIME, &later) == 0;
    clock_gettime(CLOCK_REALTIME, &back);
    struct timeval tv_back = {wall.tv_sec, 0};
    check("clock_settime sets the wall clock, settimeofday sets it back",
          set && back.tv_sec >= later.tv_sec && settimeofday(&tv_back, NULL) == 0 && time(NULL) < later.tv_sec - 1800);
    /* (`mono` was read before: the monotonic clock is past it now.) */
    errno = 0;
    check("clock_settime before the monotonic clock is EINVAL", clock_settime(CLOCK_REALTIME, &mono) == -1 && errno == EINVAL);
    /* Without the host grant the tree is a pid namespace that is not the
     * initial one: the wall clock is not its to set (reboot would end the
     * tree, which is the suite's own: not tried here). */
    long had = syscall(TEST_HOST, 0);
    struct timeval tv_now = {wall.tv_sec, 0};
    errno = 0;
    int eperm1 = clock_settime(CLOCK_REALTIME, &later) == -1 && errno == EPERM;
    errno = 0;
    int eperm2 = settimeofday(&tv_now, NULL) == -1 && errno == EPERM;
    syscall(TEST_HOST, 1);
    check("without the host grant clock_settime and settimeofday are EPERM", had == 1 && eperm1 && eperm2 && time(NULL) < later.tv_sec - 1800);
    errno = 0;
    check("reboot: CAD_OFF is taken, RESTART2 checks its string (EFAULT)",
          syscall(SYS_reboot, 0xfee1dead, 0x28121969, 0, NULL) == 0
          && syscall(SYS_reboot, 0xfee1dead, 0x28121969, 0xa1b2c3d4, NULL) == -1 && errno == EFAULT);
    int other = 0;
    errno = 0;
    check("futex: a negative requeue count is EINVAL", syscall(SYS_futex, &word, FUTEX_REQUEUE_PRIVATE, 0, -1, &other, 0) == -1 && errno == EINVAL);
    struct timespec cpu_now;
    check("the CPU clocks of the caller, also by pid 0's encoding",
          clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &cpu_now) == 0 && clock_gettime(CLOCK_THREAD_CPUTIME_ID, &cpu_now) == 0
          && clock_gettime((clockid_t)((~0u << 3) | 2), &cpu_now) == 0 && clock_gettime((clockid_t)((~0u << 3) | 6), &cpu_now) == 0);
    struct rlimit rl;
    check("prlimit: an 8 MiB stack, no core files", getrlimit(RLIMIT_STACK, &rl) == 0 && rl.rlim_cur == 8 << 20
          && getrlimit(RLIMIT_CORE, &rl) == 0 && rl.rlim_cur == 0 && getrlimit(RLIMIT_AS, &rl) == 0 && rl.rlim_cur == RLIM_INFINITY);
    struct rlimit bad = {10, 5};
    errno = 0;
    check("... a soft limit above the hard one is EINVAL, a resource beyond them too",
          setrlimit(RLIMIT_STACK, &bad) == -1 && errno == EINVAL && syscall(SYS_prlimit64, 0, 16, NULL, &rl) == -1);
    struct rlimit core_lim = {1 << 20, RLIM_INFINITY}, seen;
    int kept = setrlimit(RLIMIT_CORE, &core_lim) == 0 && getrlimit(RLIMIT_CORE, &seen) == 0 && seen.rlim_cur == 1 << 20;
    int lim_pipe[2];
    pipe(lim_pipe);
    pid_t lim_child = fork();
    if (lim_child == 0) {
        struct rlimit l;
        char x;
        read(lim_pipe[0], &x, 1);
        _exit(getrlimit(RLIMIT_CORE, &l) == 0 && l.rlim_cur == 1 << 20 ? 0 : 1);
    }
    struct rlimit child_lim = {4096, RLIM_INFINITY}, child_seen;
    int other_set = syscall(SYS_prlimit64, lim_child, RLIMIT_DATA, &child_lim, NULL) == 0
                    && syscall(SYS_prlimit64, lim_child, RLIMIT_DATA, NULL, &child_seen) == 0 && child_seen.rlim_cur == 4096
                    && getrlimit(RLIMIT_DATA, &seen) == 0 && seen.rlim_cur == RLIM_INFINITY;
    write(lim_pipe[1], "x", 1);
    int lim_status = 0;
    waitpid(lim_child, &lim_status, 0);
    close(lim_pipe[0]);
    close(lim_pipe[1]);
    core_lim.rlim_cur = 0;
    setrlimit(RLIMIT_CORE, &core_lim);
    check("resource limits are kept per process: set, inherited by fork, another's by prlimit",
          kept && other_set && WIFEXITED(lim_status) && WEXITSTATUS(lim_status) == 0);
    errno = 0;
    check("ioperm is EPERM (the tree has no ports)", syscall(SYS_ioperm, 0x80, 1, 1) == -1 && errno == EPERM);
    errno = 0;
    check("reboot with a wrong magic number is EINVAL", syscall(SYS_reboot, 0, 0, 0, NULL) == -1 && errno == EINVAL);
    /* The kernel's range checks on what reaches it (vm.rs): lengths near 2^64,
     * sums that would wrap, and ranges of tens of TiB looked at in one lookup
     * (no loop over their pages: each call returns at once). */
    int64_t t0 = now_ms();
    errno = 0;
    check("mmap of a length near 2^64 is ENOMEM", mmap(NULL, (size_t)-4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) == MAP_FAILED && errno == ENOMEM);
    char *huge_hint = (char *)0x100000000000UL;
    char *big = mmap(huge_hint, 1UL << 45, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    check("a 32 TiB hint is taken or placed elsewhere in one lookup", big != MAP_FAILED);
    errno = 0;
    check("MAP_FIXED_NOREPLACE over a 32 TiB mapping is EEXIST",
          big != MAP_FAILED && mmap(big + (1UL << 44), 1UL << 44, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE | MAP_FIXED_NOREPLACE, -1, 0) == MAP_FAILED && errno == EEXIST);
    errno = 0;
    check("mremap to a length near 2^64 fails", big != MAP_FAILED && mremap(big, 4096, (size_t)-4096, MREMAP_MAYMOVE) == MAP_FAILED && (errno == ENOMEM || errno == EINVAL));
    errno = 0;
    check("mremap to a fixed address whose end wraps is EINVAL",
          big != MAP_FAILED && mremap(big, 4096, 1UL << 40, MREMAP_MAYMOVE | MREMAP_FIXED, (void *)(-(1L << 39) & ~4095L)) == MAP_FAILED && errno == EINVAL);
    errno = 0;
    check("madvise and msync of a range that wraps fail", big != MAP_FAILED && madvise(big, (size_t)-4096, MADV_DONTNEED) == -1 && msync(big, (size_t)-4096, MS_SYNC) == -1);
    check("madvise(MADV_DONTNEED) of the 32 TiB mapping returns", big != MAP_FAILED && madvise(big, 1UL << 45, MADV_DONTNEED) == 0);
    if (big != MAP_FAILED) munmap(big, 1UL << 45);
    int64_t took = now_ms() - t0;
    printf("    (the range checks took %lld ms)\n", (long long)took);
    check("... all of it at once (no loop over the pages)", took < 2000);
    struct stat sb;
    struct statfs fs;
    char target[64] = {0};
    check("/dev is the server's devtmpfs: null (1,3), zero, the terminals",
          stat("/dev/null", &sb) == 0 && S_ISCHR(sb.st_mode) && sb.st_rdev == makedev(1, 3) && sb.st_dev == 5
          && stat("/dev/zero", &sb) == 0 && sb.st_rdev == makedev(1, 5) && stat("/dev/tty", &sb) == 0 && sb.st_rdev == makedev(5, 0)
          && stat("/dev/ptmx", &sb) == 0 && stat("/dev/console", &sb) == 0 && sb.st_rdev == makedev(5, 1));
    check("... its links into /proc/self/fd and its directories",
          readlink("/dev/stdin", target, sizeof target) == 15 && strcmp(target, "/proc/self/fd/0") == 0
          && stat("/dev/fd/1", &sb) == 0 && stat("/dev/pts", &sb) == 0 && S_ISDIR(sb.st_mode) && stat("/dev/shm", &sb) == 0 && (sb.st_mode & 07777) == 01777);
    int made = open("/dev/lxfile", O_CREAT | O_RDWR, 0644);
    errno = 0;
    check("... a tmpfs of its own (statfs, EXDEV) root writes in", statfs("/dev", &fs) == 0 && fs.f_type == 0x01021994
          && made >= 0 && write(made, "x", 1) == 1 && rename("/dev/lxfile", "/tmp/lxfile") == -1 && errno == EXDEV
          && unlink("/dev/lxfile") == 0);
    close(made);
    check("... devpts has its own kind (statfs)", statfs("/dev/pts", &fs) == 0 && fs.f_type == 0x1cd1);
    int cin = open("/tmp/lxcopy", O_CREAT | O_RDWR | O_TRUNC, 0644), cout = open("/dev/lxcopy", O_CREAT | O_RDWR | O_TRUNC, 0644);
    errno = 0;
    check("... copy_file_range from /tmp to /dev is EXDEV (another tmpfs)",
          cin >= 0 && cout >= 0 && write(cin, "abc", 3) == 3 && lseek(cin, 0, SEEK_SET) == 0
          && copy_file_range(cin, NULL, cout, NULL, 3, 0) == -1 && errno == EXDEV);
    close(cin);
    close(cout);
    unlink("/tmp/lxcopy");
    unlink("/dev/lxcopy");
    int zfd = open("/dev/zero", O_RDONLY);
    char *zmap = zfd >= 0 ? mmap(NULL, PG, PROT_READ | PROT_WRITE, MAP_PRIVATE, zfd, 0) : MAP_FAILED;
    check("... /dev/zero reads and maps zeros", zfd >= 0 && read(zfd, target, 8) == 8 && target[0] == 0 && zmap != MAP_FAILED && zmap[100] == 0);
    if (zmap != MAP_FAILED) munmap(zmap, PG);
    close(zfd);
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

static void pin(int cpu) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    sched_setaffinity(0, sizeof set, &set);
}

/* A nice 19 thread that takes a server lock again and again on a CPU a
 * nice -20 loop holds: a thread of the instance on another CPU that needs
 * the lock never waits long for it (the holder runs with nice -20's weight
 * while it holds it, so it is not stuck behind the loop). */
static void priority_inversion(void) {
    pid_t hog = fork();
    if (hog == 0) {
        pin(0);
        setpriority(PRIO_PROCESS, 0, -20);
        for (volatile unsigned long x = 0;; x++) {
        }
    }
    pid_t holder = fork();
    if (holder == 0) {
        pin(0);
        setpriority(PRIO_PROCESS, 0, 19);
        for (;;) syscall(TEST_LOCKED_ADD, 1000);
    }
    pin(1);
    usleep(200 * 1000);
    struct timespec t0, t1, start;
    clock_gettime(CLOCK_MONOTONIC, &start);
    double worst = 0;
    int n = 0;
    do {
        clock_gettime(CLOCK_MONOTONIC, &t0);
        syscall(TEST_LOCKED_ADD, 1);
        clock_gettime(CLOCK_MONOTONIC, &t1);
        double d = (t1.tv_sec - t0.tv_sec) + (t1.tv_nsec - t0.tv_nsec) / 1e9;
        if (d > worst) worst = d;
        n++;
    } while ((t1.tv_sec - start.tv_sec) + (t1.tv_nsec - start.tv_nsec) / 1e9 < 1.5);
    kill(hog, SIGKILL);
    kill(holder, SIGKILL);
    waitpid(hog, NULL, 0);
    waitpid(holder, NULL, 0);
    cpu_set_t all;
    CPU_ZERO(&all);
    for (int c = 0; c < 64; c++) CPU_SET(c, &all);
    sched_setaffinity(0, sizeof all, &all);
    printf("    (%d lock takes, the slowest %.1f ms)\n", n, worst * 1000);
    check("a nice 19 lock holder next to a nice -20 loop does not hold others up", worst < 0.2);
}

/* `lxtest serverfail` (on its own, by autorun: it ends the whole tree): the server fails
 * on this thread while it holds a plain lock and a sleeping one, which other processes of
 * the tree wait for (and a /data file is being written meanwhile). The instance breaks:
 * every process is killed, every lock wait ends, the service threads wind down, and the
 * tree ends (no hang: the run ends, the kernel says why). */
static int server_fail(void) {
    pid_t plain = fork();
    if (plain == 0) {
        for (;;) syscall(TEST_LOCKED_ADD, 1000);
    }
    pid_t sleeping = fork();
    if (sleeping == 0) {
        for (;;) syscall(TEST_SLEEP_LOCKED, 1000000);
    }
    pid_t writer = fork();
    if (writer == 0) {
        int f = open("/data/serverfail.tmp", O_WRONLY | O_CREAT | O_TRUNC, 0600);
        char block[4096];
        memset(block, 'w', sizeof block);
        for (;;) {
            pwrite(f, block, sizeof block, 0);
            fsync(f);
        }
    }
    usleep(300 * 1000);
    printf("lxtest serverfail: failing the server now\n");
    fflush(stdout);
    syscall(TEST_SERVER_FAIL);
    printf("lxtest serverfail: still here (wrong)\n");
    return 1;
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "serverfail") == 0) {
        return server_fail();
    }
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
    /* A page whose pager let go of the object (its last handle, as for a
     * /data file whose inode the server dropped): no answer can come, so
     * the wait ends (SIGBUS) rather than sleeping on. */
    int mapped[2];
    pipe(mapped);
    child = fork();
    if (child == 0) {
        char *stuck = (char *)0x221000000000;
        if (syscall(TEST_PAGED_STUCK, stuck) != 0) _exit(1);
        write(mapped[1], "m", 1);
        (void)*(volatile char *)stuck;
        _exit(2);
    }
    char m = 0;
    close(mapped[1]);
    read(mapped[0], &m, 1);
    close(mapped[0]);
    // (Waiting by now, or soon: a fault after the drop fails too.)
    usleep(100 * 1000);
    long dropped = m == 'm' ? syscall(TEST_PAGED_STUCK, 0) : -1;
    alarm(5);
    st = 0;
    waitpid(child, &st, 0);
    alarm(0);
    check("a page of an object its pager let go of raises SIGBUS", dropped == 0 && WIFSIGNALED(st) && WTERMSIG(st) == SIGBUS);

    /* A page the pager fails: SIGBUS, and a later access asks again. The
     * first request of every such object fails, not only the instance's
     * first (a second object here, as a second lxtest run in one shell),
     * and each request is the object's own: both exist before the first
     * is touched. The failure reaches the access that asked however soon
     * it comes: the faulting thread waits for the page from before its
     * request (it was lost if it came before the wait began). */
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
    /* A store the pager cannot back (as on a full disk): SIGBUS for the
     * store that asked, however soon the answer comes (the store waits
     * from before it asks), and a later store asks again and goes
     * through. The page is read in first, so the store asks only for its
     * backing. */
    {
        const char *path = "/data/lxtest.mkwrite";
        int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
        struct stat fst;
        volatile char *mw = MAP_FAILED;
        if (fd >= 0 && ftruncate(fd, PG) == 0 && fstat(fd, &fst) == 0)
            mw = mmap(NULL, PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        int ready = mw != MAP_FAILED && mw[0] == 0 && syscall(TEST_MKWRITE_FAIL, (long)fst.st_ino) == 0;
        int bus = 0, later = 0;
        for (int round = 0; ready && round < 2; round++) {
            child = fork();
            if (child == 0) {
                mw[0] = round ? 'y' : 'x';
                _exit(0);
            }
            alarm(5);
            waitpid(child, &st, 0);
            alarm(0);
            if (round == 0) bus = WIFSIGNALED(st) && WTERMSIG(st) == SIGBUS;
            else later = WIFEXITED(st) && WEXITSTATUS(st) == 0 && mw[0] == 'y';
        }
        check("a store the pager cannot back raises SIGBUS", ready && bus);
        check("... and a later store asks again and goes through", ready && later);
        /* (Never left armed for a later file, should the store not have
         * asked.) */
        syscall(TEST_MKWRITE_FAIL, 0L);
        if (mw != MAP_FAILED) munmap((void *)mw, PG);
        if (fd >= 0) close(fd);
        unlink(path);
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
    /* The kernel implements no Linux call (R9). */
    r9_checks();
    /* Memory semantics are the server's (R4). */
    int mem_ok = 1;
    for (int i = 0; i < 100; i++) {
        char *m = mmap(NULL, 3 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (m == MAP_FAILED) {
            mem_ok = 0;
            break;
        }
        m[PG] = 1;
        mem_ok &= mprotect(m, PG, PROT_READ) == 0 && munmap(m, 3 * PG) == 0;
    }
    check("mmap, mprotect and munmap are the server's", mem_ok);

    /* Time is the server's too (R5), and it writes the program's memory
     * directly: faults there are the program's, bad addresses EFAULT. */
    struct timespec ts, z = {0, 0};
    struct timeval tv;
    for (int i = 0; i < 100; i++) {
        clock_gettime(CLOCK_MONOTONIC, &ts);
        gettimeofday(&tv, NULL);
        nanosleep(&z, NULL);
    }
    check("clock_gettime, gettimeofday and nanosleep are the server's", ts.tv_sec >= 0 && tv.tv_sec > 1000000000);
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

    /* Pipes are the server's (R6a). */
    int q[2];
    check("pipe2 gives two descriptors", pipe2(q, O_CLOEXEC) == 0 && q[0] >= 0 && q[1] > q[0] && (fcntl(q[0], F_GETFD) & FD_CLOEXEC));
    int same = 1;
    for (int i = 0; i < 100; i++) {
        char c = (char)i, d = 0;
        write(q[1], &c, 1);
        read(q[0], &d, 1);
        same &= d == c;
    }
    check("pipe reads and writes are the server's", same);
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
    int counted = 1;
    for (int i = 0; i < 50; i++) {
        uint64_t one = 1;
        write(efd, &one, 8);
        counted &= read(efd, &v, 8) == 8;
    }
    check("eventfd reads and writes are the server's", counted && v == 1);
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
     * and umask live in it. */
    struct stat sb;
    char cwd[256];
    int found = 1;
    for (int i = 0; i < 50; i++) {
        found &= stat("/etc/runtests.sh", &sb) == 0 && access("/bin", F_OK) == 0 && getcwd(cwd, sizeof cwd) != NULL;
    }
    check("stat, access and getcwd are the server's", found && S_ISREG(sb.st_mode));
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
    int io = 1;
    for (int i = 0; i < 25; i++) {
        io &= pwrite(tf, block, sizeof block, (off_t)i * 4096) == 4096;
        io &= pread(tf, cwd, 8, (off_t)i * 4096 + 100) == 8 && cwd[0] == 'z';
        io &= lseek(tf, 0, SEEK_END) == 25 * 4096 || i < 24;
        io &= fstat(tf, &sb) == 0;
    }
    check("reads, writes, lseek and fstat of a /tmp file are the server's", io && sb.st_size == 25 * 4096);
    rw_flag_checks("/tmp/lxrw", check);
    io = 1;
    for (int i = 0; i < 25; i++) {
        io &= rw_pwritev2(tf, "v2", (long)i * 4096, RWF_NOAPPEND) == 2;
        io &= rw_preadv2(tf, cwd, 2, (long)i * 4096, 0) == 2;
    }
    check("preadv2 and pwritev2 of a /tmp file are the server's", io);
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
    check("/proc is procfs's (0:21), /dev the server's devtmpfs (0:5)", stat("/proc/counters", &sb) == 0 && sb.st_dev == 0x15 && stat("/dev/null", &sb) == 0 && S_ISCHR(sb.st_mode) && sb.st_dev == 5);
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
        long before = syscall(TEST_SERVER_TICKS, "diskfs");
        usleep(500 * 1000);
        long spent = syscall(TEST_SERVER_TICKS, "diskfs") - before;
        if (spent > 50) printf("    (diskfs used %ld ms of CPU in 500 ms)\n", spent);
        check("diskfs ring: requests waiting for room do not keep diskfs busy", r == 0 && before >= 0 && spent <= 50);
        check("/proc shows no other tree's processes (not diskfs's)", proc_ticks("diskfs") == -1);
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
    if (argc == 1) priority_inversion();
    printf("lxtest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
