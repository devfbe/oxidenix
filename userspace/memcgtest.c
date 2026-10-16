/* Per-tree memory limits (ADR 0024): every process tree is one memory group, Linux's
 * cgroup v2 memory controller as a container with its own cgroup namespace sees it.
 *
 * In the self-tests' tree (which holds the host grant):
 * - /proc/self/cgroup is "0::/", cgroup2 is mounted at /sys/fs/cgroup (in /proc/mounts,
 *   statfs's CGROUP2_SUPER_MAGIC) with the memory controller's files; mkdir there is EPERM.
 * - memory.current counts what the tree uses: it grows by what a process touches and comes
 *   back when it unmaps it; memory.peak is at least it; cgroup.procs lists this process.
 * - The boundaries are writable with the host grant ("max", sizes with suffixes, rounded
 *   down to pages; "bogus" is EINVAL), memory.current is not (EACCES).
 * - A second tree started with memory.max 32 MiB (TEST_TREE_START_MEMCG), running this
 *   program as "memcgtest peer", passes its own checks (below) and exits 0, while a process
 *   of this tree that holds 8 MiB keeps it intact and this tree's OOM kill count stays as it
 *   was; the kernel's log names the kill "Memory cgroup out of memory" with
 *   CONSTRAINT_MEMCG.
 *
 * In the peer (no host grant): its boundaries read back as its starter set them, writing
 * one is EACCES; a child that touches twice its limit is killed by the OOM killer within
 * the tree (SIGKILL; memory.events: max, high, oom and one oom_kill), the peer itself (the
 * tree's first process) lives on, memory.peak reached the limit, memory.current comes back
 * to near what it was, and the tree can still allocate afterwards. */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/klog.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define MIB (1024L * 1024)
#define TEST_TREE_WAIT 1529
#define TEST_TREE_START_MEMCG 1670
#define CGROUP2_SUPER_MAGIC 0x63677270
#define SYSFS_MAGIC 0x62656572
#define CG "/sys/fs/cgroup/"

/* The peer's boundaries. */
#define PEER_MAX (32 * MIB)
#define PEER_HIGH (31 * MIB)
#define PEER_LOW (1 * MIB)

static int failures;
static const char *who = "memcgtest";
/* The peer's results, kept in memory and written to its log at the end (its log is on
 * /data, whose cache pages would count in its group while the test fills it). */
static int holding;
static char held[8192];
static int held_len;

static void say(const char *fmt, ...) {
    char line[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(line, sizeof line, fmt, ap);
    va_end(ap);
    if (holding) {
        int n = snprintf(held + held_len, sizeof held - held_len, "%s\n", line);
        if (n > 0 && n < (int)sizeof held - held_len) held_len += n;
    } else {
        printf("%s\n", line);
        fflush(stdout);
    }
}

static void check(const char *name, int ok) {
    say("%s: %-58s %s", who, name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

/* A file's contents (NUL-terminated), its length or -errno. */
static long read_file(const char *path, char *buf, size_t size) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -errno;
    long n = 0;
    while ((size_t)n < size - 1) {
        ssize_t r = read(fd, buf + n, size - 1 - n);
        if (r < 0) {
            int e = errno;
            close(fd);
            return -e;
        }
        if (r == 0) break;
        n += r;
    }
    close(fd);
    buf[n] = 0;
    return n;
}

/* A memory group file's value: its number, UINT64_MAX for "max", UINT64_MAX - 1 if it
 * cannot be read. */
static uint64_t cg_value(const char *name) {
    char path[128], buf[64];
    snprintf(path, sizeof path, CG "%s", name);
    if (read_file(path, buf, sizeof buf) <= 0) return UINT64_MAX - 1;
    if (strcmp(buf, "max\n") == 0) return UINT64_MAX;
    return strtoull(buf, NULL, 10);
}

/* A count of memory.events (UINT64_MAX if missing). */
static uint64_t cg_event(const char *key) {
    char buf[512];
    if (read_file(CG "memory.events", buf, sizeof buf) <= 0) return UINT64_MAX;
    size_t len = strlen(key);
    for (char *line = buf; line && *line; line = strchr(line, '\n') ? strchr(line, '\n') + 1 : NULL) {
        if (strncmp(line, key, len) == 0 && line[len] == ' ') return strtoull(line + len + 1, NULL, 10);
    }
    return UINT64_MAX;
}

/* Writes `text` to a memory group file in one write: 0, or the errno of the open or the
 * write. */
static int cg_write(const char *name, const char *text) {
    char path[128];
    snprintf(path, sizeof path, CG "%s", name);
    int fd = open(path, O_WRONLY | O_TRUNC);
    if (fd < 0) return errno;
    ssize_t n = write(fd, text, strlen(text));
    int e = n < 0 ? errno : 0;
    close(fd);
    return e;
}

static long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

/* Maps and touches `bytes` of anonymous memory, every page written with its index. */
static char *touch(size_t bytes) {
    char *p = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) return NULL;
    for (size_t off = 0; off < bytes; off += 4096) *(size_t *)(p + off) = off;
    return p;
}

static int intact(const char *p, size_t bytes) {
    for (size_t off = 0; off < bytes; off += 4096)
        if (*(const size_t *)(p + off) != off) return 0;
    return 1;
}

/* ------------------------------------------------------------------ the peer */

static int peer(int argc, char **argv) {
    who = "memcgtest peer";
    holding = 1;
    const char *log = argc > 2 ? argv[2] : "/data/memcgtest.log";

    check("its memory.max is what its starter set", cg_value("memory.max") == (uint64_t)PEER_MAX);
    check("its memory.high is what its starter set", cg_value("memory.high") == (uint64_t)PEER_HIGH);
    check("its memory.low is what its starter set", cg_value("memory.low") == (uint64_t)PEER_LOW);
    check("its memory.min is 0", cg_value("memory.min") == 0);
    check("memory.max is not its own to write (EACCES)", cg_write("memory.max", "max") == EACCES);
    check("memory.high neither", cg_write("memory.high", "max") == EACCES);
    char buf[256];
    check("/proc/self/cgroup is 0::/", read_file("/proc/self/cgroup", buf, sizeof buf) > 0 && strcmp(buf, "0::/\n") == 0);

    uint64_t base = cg_value("memory.current");
    say("memcgtest peer: memory.current %lu KiB at start, limit %lu KiB", (unsigned long)(base / 1024), (unsigned long)(PEER_MAX / 1024));
    check("memory.current is below the limit", base > 0 && base < (uint64_t)PEER_MAX);
    check("no OOM event yet", cg_event("oom") == 0 && cg_event("oom_kill") == 0);

    /* A child that touches twice the limit: the group's OOM killer ends it. */
    long start = now_ms();
    pid_t hog = fork();
    if (hog == 0) {
        char *p = touch(2 * PEER_MAX);
        _exit(p ? 7 : 8);
    }
    int status = 0;
    pid_t got = waitpid(hog, &status, 0);
    say("memcgtest peer: the hog ended after %ld ms, status %#x", now_ms() - start, status);
    check("a child touching twice the limit is killed (SIGKILL)", got == hog && WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL);
    check("memory.events: max counted", cg_event("max") >= 1 && cg_event("max") != UINT64_MAX);
    check("memory.events: high counted", cg_event("high") >= 1 && cg_event("high") != UINT64_MAX);
    check("memory.events: oom counted", cg_event("oom") >= 1 && cg_event("oom") != UINT64_MAX);
    check("memory.events: one oom_kill", cg_event("oom_kill") == 1);
    check("memory.events: no group kill", cg_event("oom_group_kill") == 0);
    check("memory.events.local says the same",
          read_file(CG "memory.events", buf, sizeof buf) > 0 &&
              ({ char local[256]; read_file(CG "memory.events.local", local, sizeof local) > 0 && strcmp(buf, local) == 0; }));
    uint64_t peak = cg_value("memory.peak");
    say("memcgtest peer: memory.peak %lu KiB", (unsigned long)(peak / 1024));
    check("memory.peak reached the limit's last pages", peak >= (uint64_t)PEER_MAX - 64 * 4096 && peak != UINT64_MAX - 1);

    /* Its memory comes back: near where it was. */
    uint64_t now = cg_value("memory.current");
    for (long until = now_ms() + 5000; now > base + MIB && now_ms() < until; now = cg_value("memory.current")) usleep(50000);
    say("memcgtest peer: memory.current %lu KiB after", (unsigned long)(now / 1024));
    check("memory.current comes back to near what it was", now <= base + MIB);

    /* The tree goes on: the first process lives, and new memory can be had. */
    char *p = touch(4 * MIB);
    check("the tree allocates and touches 4 MiB afterwards", p && intact(p, 4 * MIB));
    if (p) munmap(p, 4 * MIB);
    check("no further kill", cg_event("oom_kill") == 1);

    say("memcgtest peer: %d failed", failures);
    int fd = open(log, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd >= 0) {
        if (write(fd, held, held_len) != held_len) failures++;
        close(fd);
    }
    return failures ? 1 : 0;
}

/* ------------------------------------------------------------ the first tree */

static int listed(const char *dir, const char *name) {
    DIR *d = opendir(dir);
    if (!d) return 0;
    struct dirent *e;
    int found = 0;
    while ((e = readdir(d)))
        if (strcmp(e->d_name, name) == 0) found = 1;
    closedir(d);
    return found;
}

/* Whether the kernel's log holds `text`. */
static int klog_has(const char *text) {
    static char log[1 << 18];
    int n = klogctl(3, log, sizeof log - 1);
    if (n < 0) return 0;
    log[n] = 0;
    return strstr(log, text) != NULL;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc > 1 && strcmp(argv[1], "peer") == 0) return peer(argc, argv);
    char buf[4096];

    /* The view of the group. */
    check("/proc/self/cgroup is 0::/", read_file("/proc/self/cgroup", buf, sizeof buf) > 0 && strcmp(buf, "0::/\n") == 0);
    char path[64];
    snprintf(path, sizeof path, "/proc/self/task/%d/cgroup", getpid());
    check("a thread's cgroup file too", read_file(path, buf, sizeof buf) > 0 && strcmp(buf, "0::/\n") == 0);
    check("cgroup2 is mounted at /sys/fs/cgroup",
          read_file("/proc/self/mounts", buf, sizeof buf) > 0 && strstr(buf, "cgroup2 /sys/fs/cgroup cgroup2 ") != NULL);
    struct statfs sf;
    check("statfs: CGROUP2_SUPER_MAGIC", statfs("/sys/fs/cgroup", &sf) == 0 && sf.f_type == CGROUP2_SUPER_MAGIC);
    check("statfs of /sys/fs: sysfs", statfs("/sys/fs", &sf) == 0 && sf.f_type == SYSFS_MAGIC);
    check("/sys lists fs, /sys/fs lists cgroup", listed("/sys", "fs") && listed("/sys/fs", "cgroup"));
    check("/sys/fs/cgroup lists memory.max and memory.events", listed(CG, "memory.max") && listed(CG, "memory.events") && listed(CG, "cgroup.procs"));
    check("cgroup.controllers: memory", read_file(CG "cgroup.controllers", buf, sizeof buf) > 0 && strcmp(buf, "memory\n") == 0);
    check("no sub-groups: mkdir is EPERM", mkdir(CG "sub", 0755) < 0 && errno == EPERM);
    check("memory.max and memory.high: max", cg_value("memory.max") == UINT64_MAX && cg_value("memory.high") == UINT64_MAX);
    check("memory.low and memory.min: 0", cg_value("memory.low") == 0 && cg_value("memory.min") == 0);
    check("memory.events has its six counts",
          cg_event("low") != UINT64_MAX && cg_event("high") != UINT64_MAX && cg_event("max") != UINT64_MAX && cg_event("oom") != UINT64_MAX &&
              cg_event("oom_kill") != UINT64_MAX && cg_event("oom_group_kill") != UINT64_MAX);
    char me[32];
    snprintf(me, sizeof me, "%d", getpid());
    check("cgroup.procs lists this process", read_file(CG "cgroup.procs", buf, sizeof buf) > 0 &&
                                                  ({ int f = 0; for (char *l = strtok(buf, "\n"); l; l = strtok(NULL, "\n")) f |= strcmp(l, me) == 0; f; }));

    /* What the tree uses. */
    uint64_t before = cg_value("memory.current");
    check("memory.current: some pages", before > 0 && before % 4096 == 0 && before != UINT64_MAX - 1);
    char *p = touch(16 * MIB);
    uint64_t during = cg_value("memory.current");
    check("touching 16 MiB adds them to memory.current", p && during >= before + 15 * MIB && during <= before + 18 * MIB);
    check("memory.peak is at least memory.current", cg_value("memory.peak") >= during);
    if (p) munmap(p, 16 * MIB);
    uint64_t after = cg_value("memory.current");
    check("unmapping them takes them off", after <= before + MIB);
    say("memcgtest: memory.current %lu KiB, %lu KiB with 16 MiB touched, %lu KiB after",
        (unsigned long)(before / 1024), (unsigned long)(during / 1024), (unsigned long)(after / 1024));

    /* The boundaries, with the host grant. */
    check("memory.current is not writable (EACCES)", cg_write("memory.current", "0") == EACCES);
    check("memory.low takes 1M", cg_write("memory.low", "1M") == 0 && cg_value("memory.low") == (uint64_t)MIB);
    check("memory.low back to 0", cg_write("memory.low", "0") == 0 && cg_value("memory.low") == 0);
    check("memory.min takes a size rounded down to pages", cg_write("memory.min", "5000\n") == 0 && cg_value("memory.min") == 4096);
    check("memory.min back to 0", cg_write("memory.min", "0") == 0 && cg_value("memory.min") == 0);
    check("memory.high takes max", cg_write("memory.high", "max") == 0 && cg_value("memory.high") == UINT64_MAX);
    check("memory.max takes 4G and goes back to max",
          cg_write("memory.max", "4G") == 0 && cg_value("memory.max") == 4096 * MIB && cg_write("memory.max", "max\n") == 0 && cg_value("memory.max") == UINT64_MAX);
    check("a size that is none: EINVAL", cg_write("memory.max", "bogus") == EINVAL && cg_write("memory.low", "12Q") == EINVAL);
    check("cgroup.procs takes this process (no move)", cg_write("cgroup.procs", me) == 0);

    /* A second tree with a limit of its own exhausts it; this tree is not touched. */
    uint64_t kills_before = cg_event("oom_kill"), ooms_before = cg_event("oom");
    int ready[2], go[2];
    if (pipe(ready) || pipe(go)) return 1;
    pid_t bystander = fork();
    if (bystander == 0) {
        char *mine = touch(8 * MIB);
        char c = mine ? 'r' : 'x';
        if (write(ready[1], &c, 1) != 1) _exit(3);
        if (read(go[0], &c, 1) != 1) _exit(4);
        _exit(mine && intact(mine, 8 * MIB) ? 0 : 5);
    }
    char c = 0;
    check("a process of this tree holds 8 MiB", read(ready[0], &c, 1) == 1 && c == 'r');
    const char *log = "/data/memcgtest.log";
    unlink(log);
    char *args[] = {"memcgtest", "peer", (char *)log, NULL};
    uint64_t limits[4] = {PEER_MAX, PEER_HIGH, PEER_LOW, 0};
    long tree = syscall(TEST_TREE_START_MEMCG, "/bin/memcgtest", args, limits);
    check("a tree with memory.max 32 MiB starts", tree > 0);
    if (tree > 0) {
        long st = syscall(TEST_TREE_WAIT, tree, 120L * 1000000000L);
        check("its first process passed its checks (exit 0)", st >= 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0);
        if (read_file(log, buf, sizeof buf) > 0) {
            printf("--- the second tree's log\n%s---\n", buf);
        } else {
            check("the second tree's log is there", 0);
        }
    }
    unlink(log);
    check("this tree's process is alive", kill(bystander, 0) == 0);
    c = 'g';
    int status = 0;
    check("and its 8 MiB are intact", write(go[1], &c, 1) == 1 && waitpid(bystander, &status, 0) == bystander && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    check("this tree's OOM counts did not move", cg_event("oom_kill") == kills_before && cg_event("oom") == ooms_before);
    check("the kernel's log: Memory cgroup out of memory", klog_has("Memory cgroup out of memory: Killed process"));
    check("the kernel's log: CONSTRAINT_MEMCG", klog_has("oom-kill:constraint=CONSTRAINT_MEMCG"));

    printf("memcgtest: %s\n", failures ? "FAILED" : "all passed");
    return failures ? 1 : 0;
}
