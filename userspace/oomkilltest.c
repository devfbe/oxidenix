/* The OOM killer and the overcommit modes (ADR 0013).
 *
 * - /proc/sys/vm/overcommit_{memory,ratio,kbytes}: strict (2) with a ratio of
 *   100 by default, CommitLimit in /proc/meminfo following the ratio and the
 *   KiB, values out of range refused, writable only with the host grant.
 * - /proc/<pid>/oom_score orders processes by their resident memory and
 *   follows oom_score_adj; oom_score_adj and the legacy oom_adj convert as
 *   Linux's, are inherited by fork, and cannot go below what a process may
 *   set without the host grant.
 * - Strict mode refuses a commit beyond the limit; the heuristic mode grants
 *   it (but not one larger than all memory), "always" grants everything.
 * - In the heuristic mode a child that touches more memory than there is
 *   makes the OOM killer act: first the process whose oom_score_adj is 1000
 *   (small as it is), then the biggest one, the toucher, both with SIGKILL;
 *   this program (oom_score_adj -1000, and big too), a middle-sized child,
 *   a second process tree and the system keep running, and the kernel's log
 *   says "Out of memory: Killed process" for both.
 * The mode and this program's oom_score_adj are restored at the end. */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/klog.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define MIB (1024L * 1024)
#define TEST_HOST 1522
#define TEST_TREE_START 1528
#define TEST_TREE_WAIT 1529
#define TEST_TREE_KILL 1530

static int failures;

static void check(const char *name, int ok) {
    printf("%-66s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

/* The first number in a file, or LONG_MIN if it cannot be read. */
static long read_long(const char *path) {
    char buf[64] = {0};
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1L << 62;
    ssize_t n = read(fd, buf, sizeof buf - 1);
    close(fd);
    return n > 0 ? strtol(buf, NULL, 10) : -1L << 62;
}

/* Writes `text` to `path` in one write: 0, or the errno of the open or the write. */
static int write_text(const char *path, const char *text) {
    int fd = open(path, O_WRONLY | O_TRUNC);
    if (fd < 0) return errno;
    ssize_t n = write(fd, text, strlen(text));
    int e = n < 0 ? errno : 0;
    close(fd);
    return e;
}

static long meminfo(const char *key) {
    char text[4096] = {0};
    int fd = open("/proc/meminfo", O_RDONLY);
    read(fd, text, sizeof text - 1);
    close(fd);
    char *p = strstr(text, key);
    return p ? strtol(p + strlen(key) + 1, NULL, 10) : -1;
}

static long pid_long(pid_t pid, const char *file) {
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/%s", pid, file);
    return read_long(path);
}

static int pid_write(pid_t pid, const char *file, const char *text) {
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/%s", pid, file);
    return write_text(path, text);
}

static int set_mode(int mode) {
    char text[8];
    snprintf(text, sizeof text, "%d\n", mode);
    return write_text("/proc/sys/vm/overcommit_memory", text);
}

/* A child that names itself `name`, sets its oom_score_adj to `adj`, touches
 * `mib` MiB, says so on the pipe `ready` and waits to be killed. */
static pid_t holder(const char *name, long mib, const char *adj, int ready[2]) {
    pid_t pid = fork();
    if (pid == 0) {
        prctl(PR_SET_NAME, name);
        write_text("/proc/self/oom_score_adj", adj);
        char *m = mmap(NULL, mib * MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (m == MAP_FAILED) _exit(2);
        for (long i = 0; i < mib * MIB; i += 4096) m[i] = 1;
        write(ready[1], "r", 1);
        for (;;) pause();
    }
    char c;
    read(ready[0], &c, 1);
    return pid;
}

/* How often `needle` is in the kernel's log. */
static int log_count(const char *needle) {
    static char all[128 * 1024 + 1];
    int n = klogctl(3, all, sizeof all - 1);
    if (n < 0) return -1;
    all[n] = 0;
    int count = 0;
    for (char *p = all; (p = strstr(p, needle)); p += strlen(needle)) count++;
    return count;
}

static void sysctls(void) {
    check("overcommit_memory is strict (2) by default", read_long("/proc/sys/vm/overcommit_memory") == 2);
    check("overcommit_ratio is 100, overcommit_kbytes 0", read_long("/proc/sys/vm/overcommit_ratio") == 100 && read_long("/proc/sys/vm/overcommit_kbytes") == 0);
    DIR *d = opendir("/proc/sys");
    int vm = 0;
    for (struct dirent *e; d && (e = readdir(d));) vm |= strcmp(e->d_name, "vm") == 0;
    if (d) closedir(d);
    d = opendir("/proc/sys/vm");
    int files = 0;
    for (struct dirent *e; d && (e = readdir(d));) files += strncmp(e->d_name, "overcommit_", 11) == 0;
    if (d) closedir(d);
    check("/proc/sys lists vm, which lists the three overcommit files", vm && files == 3);

    long limit = meminfo("CommitLimit:");
    check("a ratio of 50 halves CommitLimit", write_text("/proc/sys/vm/overcommit_ratio", "50\n") == 0 && labs(meminfo("CommitLimit:") * 2 - limit) <= 2048);
    check("overcommit_kbytes sets CommitLimit itself and zeroes the ratio",
          write_text("/proc/sys/vm/overcommit_kbytes", "65536") == 0 && meminfo("CommitLimit:") == 65536 && read_long("/proc/sys/vm/overcommit_ratio") == 0);
    check("a ratio zeroes overcommit_kbytes again",
          write_text("/proc/sys/vm/overcommit_ratio", "100") == 0 && read_long("/proc/sys/vm/overcommit_kbytes") == 0 && labs(meminfo("CommitLimit:") - limit) <= 1024);
    check("overcommit_memory 3, -1 and junk: EINVAL",
          set_mode(3) == EINVAL && write_text("/proc/sys/vm/overcommit_memory", "-1") == EINVAL && write_text("/proc/sys/vm/overcommit_memory", "two") == EINVAL &&
              read_long("/proc/sys/vm/overcommit_memory") == 2);

    /* This tree's pid 1 is the machine's init while the tree holds the host grant:
     * never chosen (oom_score 0), and chosen like any other without it. */
    long init_score = pid_long(1, "oom_score");
    /* Without the host grant (a tree that is not the machine's owner): read-only. */
    long had = syscall(TEST_HOST, 0);
    long init_plain = pid_long(1, "oom_score");
    int ro = open("/proc/sys/vm/overcommit_memory", O_WRONLY) < 0 && errno == EACCES;
    int readable = read_long("/proc/sys/vm/overcommit_memory") == 2;
    int below = pid_write(getpid(), "oom_score_adj", "-5") == EACCES;
    int raise = pid_write(getpid(), "oom_score_adj", "5") == 0 && pid_write(getpid(), "oom_score_adj", "0") == 0;
    syscall(TEST_HOST, had);
    check("without the host grant: overcommit_memory is EACCES to write, readable", had == 1 && ro && readable);
    check("... and oom_score_adj cannot go below its least (EACCES), up it can", below && raise);
    check("pid 1 of the host's tree scores 0, of a tree without the grant more", init_score == 0 && init_plain > 0);
}

static void scores(void) {
    int p[2];
    pipe(p);
    pid_t small = holder("oomsmall", 4, "0", p);
    pid_t big = holder("oombig", 32, "0", p);
    long s = pid_long(small, "oom_score"), b = pid_long(big, "oom_score");
    printf("oom_score: 4 MiB %ld, 32 MiB %ld\n", s, b);
    check("oom_score orders processes by their memory", s > 0 && b > s);
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/status", big);
    FILE *f = fopen(path, "r");
    long anon = -1;
    for (char line[128]; f && fgets(line, sizeof line, f);)
        if (strncmp(line, "RssAnon:", 8) == 0) anon = strtol(line + 8, NULL, 10);
    if (f) fclose(f);
    check("status counts the touched memory as RssAnon", anon >= 32 * 1024);

    check("oom_score_adj 500 lifts the small one above the big one", pid_write(small, "oom_score_adj", "500") == 0 && pid_long(small, "oom_score") > pid_long(big, "oom_score"));
    check("... oom_adj reads 500 * 17 / 1000", pid_long(small, "oom_adj") == 8);
    check("oom_adj 15 is oom_score_adj 1000", pid_write(small, "oom_adj", "15") == 0 && pid_long(small, "oom_score_adj") == 1000);
    check("oom_adj -17 is -1000, which makes oom_score 0",
          pid_write(small, "oom_adj", "-17") == 0 && pid_long(small, "oom_score_adj") == -1000 && pid_long(small, "oom_score") == 0);
    check("oom_score_adj 1001 and oom_adj 16: EINVAL", pid_write(small, "oom_score_adj", "1001") == EINVAL && pid_write(small, "oom_adj", "16") == EINVAL);
    char task[64];
    snprintf(task, sizeof task, "task/%d/oom_score_adj", small);
    check("a thread's directory shows its process's", pid_long(small, task) == -1000);
    check("only -1000 exempts: -999 still scores", pid_write(small, "oom_score_adj", "-999") == 0 && pid_long(small, "oom_score") > 0);
    kill(small, SIGKILL);
    kill(big, SIGKILL);
    waitpid(small, NULL, 0);
    waitpid(big, NULL, 0);

    pid_write(getpid(), "oom_score_adj", "123");
    pid_t kid = fork();
    if (kid == 0) _exit(read_long("/proc/self/oom_score_adj") == 123 ? 0 : 1);
    int st = -1;
    waitpid(kid, &st, 0);
    check("a fork inherits oom_score_adj", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    pid_write(getpid(), "oom_score_adj", "0");
    close(p[0]);
    close(p[1]);
}

/* A mapping of `bytes`, writable and private: whether it was granted. */
static int commit(long bytes) {
    void *m = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (m == MAP_FAILED) return 0;
    munmap(m, bytes);
    return 1;
}

static void modes(void) {
    /* Beyond what may be promised, within all memory. */
    long beyond = (meminfo("CommitLimit:") - meminfo("Committed_AS:")) * 1024 + 8 * MIB;
    long all = meminfo("MemTotal:") * 1024;
    check("(a commit beyond the limit within all memory)", beyond < all - 8 * MIB);
    check("strict: a commit beyond the limit is ENOMEM", !commit(beyond) && errno == ENOMEM);
    check("heuristic: the same commit is granted", set_mode(0) == 0 && commit(beyond));
    void *m = mmap(NULL, beyond, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check("... and counted: Committed_AS beyond CommitLimit", m != MAP_FAILED && meminfo("Committed_AS:") > meminfo("CommitLimit:"));
    if (m != MAP_FAILED) munmap(m, beyond);
    check("... but not one larger than all memory", !commit(all + 64 * MIB) && errno == ENOMEM);
    check("always: even that one is granted", set_mode(1) == 0 && commit(all + 64 * MIB));
    check("strict again: refused again", set_mode(2) == 0 && !commit(beyond) && errno == ENOMEM);
}

static void alarm_handler(int sig) {
    (void)sig;
    set_mode(2);
    printf("oomkilltest: timed out\n");
    _exit(1);
}

static void killer(void) {
    sync();
    int before = log_count("Out of memory: Killed process");
    int bait_before = log_count("(oombait)"), hog_before = log_count("(oomhog)");
    /* This program is exempt, and holds 32 MiB itself. */
    check("oom_score_adj -1000 for this program (with the host grant)", pid_write(getpid(), "oom_score_adj", "-1000") == 0);
    int p[2];
    pipe(p);
    /* (Children inherit -1000: each sets its own.) */
    pid_t bait = holder("oombait", 2, "1000", p);
    pid_t keeper = holder("oomkeep", 16, "0", p);
    char *own = mmap(NULL, 32 * MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    for (long i = 0; own != MAP_FAILED && i < 32 * MIB; i += 4096) own[i] = 1;
    char *args[] = {"sleep", "600", NULL};
    long tree = syscall(TEST_TREE_START, "/bin/sleep", args);
    check("a second tree runs beside", tree > 0);
    check("heuristic mode", set_mode(0) == 0);

    long all = meminfo("MemTotal:") * 1024 - 4 * MIB;
    struct timespec t0, t1;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    pid_t hog = fork();
    if (hog == 0) {
        prctl(PR_SET_NAME, "oomhog");
        write_text("/proc/self/oom_score_adj", "0");
        char *m = mmap(NULL, all, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (m == MAP_FAILED) _exit(2);
        for (long i = 0; i < all; i += 4096) m[i] = 1;
        _exit(3);
    }
    int hs = -1, bs = -1, ks = -1;
    waitpid(hog, &hs, 0);
    clock_gettime(CLOCK_MONOTONIC, &t1);
    long ms = (t1.tv_sec - t0.tv_sec) * 1000 + (t1.tv_nsec - t0.tv_nsec) / 1000000;
    int bait_gone = waitpid(bait, &bs, WNOHANG) == bait;
    int keeper_alive = waitpid(keeper, &ks, WNOHANG) == 0 && kill(keeper, 0) == 0;
    set_mode(2);
    printf("hog status %#x after %ld ms, bait status %#x, keeper %s\n", hs, ms, bs, keeper_alive ? "alive" : "gone");
    check("the toucher, the biggest, was killed with SIGKILL", WIFSIGNALED(hs) && WTERMSIG(hs) == SIGKILL);
    check("... after the process with oom_score_adj 1000 (SIGKILL too)", bait_gone && WIFSIGNALED(bs) && WTERMSIG(bs) == SIGKILL);
    check("a middle-sized process with oom_score_adj 0 lives on", keeper_alive);
    check("this program (oom_score_adj -1000) lives on, its memory intact", own != MAP_FAILED && own[0] == 1 && own[32 * MIB - 4096] == 1);
    check("the second tree still runs", tree > 0 && syscall(TEST_TREE_WAIT, tree, 0L) < 0 && errno == ETIMEDOUT);
    int killed = log_count("Out of memory: Killed process") - before;
    check("the kernel's log says Out of memory: Killed process, twice", killed == 2);
    check("... naming oombait and oomhog", log_count("(oombait)") > bait_before && log_count("(oomhog)") > hog_before);
    check("... with who invoked the killer and its table", log_count("invoked oom-killer: order=0") > 0 && log_count("rss_anon rss_file rss_shmem pgtables_bytes") > 0);

    if (tree > 0) {
        syscall(TEST_TREE_KILL, tree);
        syscall(TEST_TREE_WAIT, tree, 10000000000L);
    }
    kill(keeper, SIGKILL);
    waitpid(keeper, NULL, 0);
    if (own != MAP_FAILED) munmap(own, 32 * MIB);
    close(p[0]);
    close(p[1]);

    pid_t kid = fork();
    if (kid == 0) {
        execl("/bin/true", "true", NULL);
        _exit(9);
    }
    int st = -1;
    waitpid(kid, &st, 0);
    check("the system goes on: fork and exec", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    char *m = mmap(NULL, 16 * MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    for (long i = 0; m != MAP_FAILED && i < 16 * MIB; i += 4096) m[i] = 2;
    check("... and memory can be committed and used again", m != MAP_FAILED);
    if (m != MAP_FAILED) munmap(m, 16 * MIB);
    check("oom_score_adj back to 0 (with the host grant)", pid_write(getpid(), "oom_score_adj", "0") == 0);
}

int main(void) {
    signal(SIGALRM, alarm_handler);
    alarm(300);
    sysctls();
    scores();
    modes();
    killer();
    check("strict mode at the end", read_long("/proc/sys/vm/overcommit_memory") == 2);
    printf("oomkilltest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
