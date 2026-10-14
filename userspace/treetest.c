/* Two process trees at once (ADR 0002): the self-tests' tree starts a
 * second one (TEST_TREE_START: its own Linux server instance, no console)
 * running this program as "treetest peer", and the two talk over a TCP
 * connection on loopback, each step a command and an answer with a
 * deadline (a hang fails, it never blocks the suite). Checked across the
 * instances: each tree's own pid namespace (the peer is pid 1, without
 * standard descriptors), /proc showing only its own processes, its own
 * tmpfs, signals that cannot cross (kill(-1) included), netd's ports (a
 * port one tree holds, in TIME-WAIT too, is EADDRINUSE for the other and
 * never taken over; SO_REUSEADDR shares only within a tree), TCP and UDP
 * between the trees, the reserve of netd's budget (one tree that used up
 * its share leaves the other its reserve), the reserve of the server's
 * heap (one tree that took all memory leaves the other's server working),
 * and /data shared as on one Linux machine (ADR 0014: one page cache per
 * file for both trees): names at once, data written in one tree read in
 * the other at once without fsync, also over pages it had cached, appends
 * of both trees at once (none lost or torn), truncation, size and times
 * before any write-back, MAP_SHARED stores seen across the trees, an
 * unlinked file another tree holds open readable until it closes it and its
 * blocks free at that close, a file only cached in the other tree freed at
 * once when unlinked here; and a third tree killed while it owns a file's
 * dirty pages, whose data the first tree reads and writes back without
 * waiting for anything of the dead tree's. sync(2) writes back every
 * instance's dirty files. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/epoll.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define TEST_TREE_START 1528
#define TEST_TREE_WAIT 1529
#define TEST_TREE_KILL 1530

#define CTL_PORT 47100
#define TCP_PORT 47101
#define UDP_PORT 47102
#define UDP_SHARED 47103
#define BACK_PORT 47104
#define MIB (1024 * 1024)
/* A step's answer, and the peer's wait for the next command. */
#define STEP_MS 60000
#define IDLE_MS 300000

static int failures;
static const char *who = "treetest";
/* While set, results are kept in memory instead of printed (the peer's
 * log is on /data, whose cache needs memory the other tree took). */
static int holding;
static char held[4096];
static int held_len;

static void check(const char *name, int ok) {
    if (holding) {
        int n = snprintf(held + held_len, sizeof held - held_len, "%s: %-58s %s\n", who, name, ok ? "ok" : "FAIL");
        if (n > 0 && n < (int)sizeof held - held_len) held_len += n;
    } else {
        printf("%s: %-58s %s\n", who, name, ok ? "ok" : "FAIL");
    }
    if (!ok) failures++;
}

/* Prints the results kept while `holding`. */
static void release_held(void) {
    holding = 0;
    fwrite(held, 1, held_len, stdout);
    held_len = 0;
}

/* Maps the stack's next `STACK_TOUCH` bytes now, for the calls below the
 * caller's frame (a stack fault later could need memory that is gone). */
#define STACK_TOUCH (256 * 1024)
static __attribute__((noinline)) void touch_stack(void) {
    volatile char s[STACK_TOUCH];
    for (unsigned i = 0; i < sizeof s; i += 4096) s[i] = 0;
}

static long now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000 + t.tv_nsec / 1000000;
}

/* Waits up to `ms` for `events` on `fd`. */
static int wait_fd(int fd, short events, long ms) {
    struct pollfd p = {fd, events, 0};
    int r;
    do r = poll(&p, 1, ms); while (r < 0 && errno == EINTR);
    return r > 0;
}

static unsigned char pattern(long i, int seed) {
    return (unsigned char)(i * 31 + seed + (i >> 10));
}

static void fill(unsigned char *buf, long len, int seed) {
    for (long i = 0; i < len; i++) buf[i] = pattern(i, seed);
}

static int matches(const unsigned char *buf, long len, int seed) {
    for (long i = 0; i < len; i++)
        if (buf[i] != pattern(i, seed)) return 0;
    return 1;
}

static int write_all(int fd, const void *buf, long len) {
    const char *p = buf;
    while (len > 0) {
        if (!wait_fd(fd, POLLOUT, STEP_MS)) return -1;
        long n = write(fd, p, len);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return -1;
        p += n;
        len -= n;
    }
    return 0;
}

/* Reads `len` bytes, waiting `STEP_MS` at most for each piece: how many came. */
static long read_full(int fd, void *buf, long len) {
    long got = 0;
    while (got < len) {
        if (!wait_fd(fd, POLLIN, STEP_MS)) break;
        long n = read(fd, (char *)buf + got, len - got);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) break;
        got += n;
    }
    return got;
}

static struct sockaddr_in inet(const char *ip, int port) {
    struct sockaddr_in a = {0};
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    inet_pton(AF_INET, ip, &a.sin_addr);
    return a;
}

/* A socket of `type` with SO_REUSEADDR bound to ip:port: the descriptor,
 * or -1 with errno from the bind. */
static int bound(int type, const char *ip, int port) {
    int fd = socket(AF_INET, type, 0);
    if (fd < 0) return -1;
    int one = 1;
    setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in a = inet(ip, port);
    if (bind(fd, (struct sockaddr *)&a, sizeof a) < 0) {
        int e = errno;
        close(fd);
        errno = e;
        return -1;
    }
    return fd;
}

/* Whether a bind of ip:port (SO_REUSEADDR) fails with EADDRINUSE. */
static int in_use(int type, const char *ip, int port) {
    int fd = bound(type, ip, port);
    if (fd >= 0) {
        close(fd);
        return 0;
    }
    return errno == EADDRINUSE;
}

static int tcp_connect(int port) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = inet("127.0.0.1", port);
    if (connect(fd, (struct sockaddr *)&a, sizeof a) < 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static int accept_within(int srv, long ms) {
    if (!wait_fd(srv, POLLIN, ms)) return -1;
    return accept(srv, NULL, NULL);
}

/* ---------------------------------------------------------------- control */

static int ctl = -1;
static char line_buf[1024];
static int line_len;

static void say(const char *fmt, ...) {
    char out[512];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(out, sizeof out - 1, fmt, ap);
    va_end(ap);
    out[n++] = '\n';
    write_all(ctl, out, n);
}

/* The next line from the other side into `out` (without its newline),
 * waiting up to `ms`: 0 on a timeout or the connection's end. */
static int hear(char *out, int cap, long ms) {
    long deadline = now_ms() + ms;
    for (;;) {
        char *nl = memchr(line_buf, '\n', line_len);
        if (nl) {
            int n = nl - line_buf;
            int copy = n < cap - 1 ? n : cap - 1;
            memcpy(out, line_buf, copy);
            out[copy] = 0;
            memmove(line_buf, nl + 1, line_len - n - 1);
            line_len -= n + 1;
            return 1;
        }
        long left = deadline - now_ms();
        if (left <= 0 || line_len == sizeof line_buf || !wait_fd(ctl, POLLIN, left)) return 0;
        long r = read(ctl, line_buf + line_len, sizeof line_buf - line_len);
        if (r < 0 && errno == EINTR) continue;
        if (r <= 0) return 0;
        line_len += r;
    }
}

/* Lists /proc's processes: their pids into `pids` (at most `cap`), and
 * whether any one's cmdline begins with `bytes` (of `len`). Their count. */
static int proc_scan(int *pids, int cap, const char *bytes, int len, int *found) {
    int n = 0;
    *found = 0;
    DIR *d = opendir("/proc");
    if (!d) return -1;
    struct dirent *e;
    while ((e = readdir(d))) {
        char *end;
        long pid = strtol(e->d_name, &end, 10);
        if (*end || pid <= 0) continue;
        if (n < cap) pids[n] = pid;
        n++;
        char path[64], cmd[256];
        snprintf(path, sizeof path, "/proc/%ld/cmdline", pid);
        int fd = open(path, O_RDONLY);
        if (fd < 0) continue;
        int got = read(fd, cmd, sizeof cmd);
        close(fd);
        if (got >= len && memcmp(cmd, bytes, len) == 0) *found = 1;
    }
    closedir(d);
    return n;
}

/* A child that counts SIGUSR2 until SIGTERM, then exits with the count; it
 * is ready (its handlers set) when this returns. */
static volatile sig_atomic_t usr2_count, term_seen;
static void on_usr2(int sig) { (void)sig; usr2_count++; }
static void on_term(int sig) { (void)sig; term_seen = 1; }

static pid_t counting_child(void) {
    int p[2];
    if (pipe(p) < 0) return -1;
    pid_t pid = fork();
    if (pid == 0) {
        usr2_count = 0;
        signal(SIGUSR2, on_usr2);
        signal(SIGTERM, on_term);
        close(p[0]);
        write(p[1], "r", 1);
        close(p[1]);
        while (!term_seen) pause();
        _exit(usr2_count);
    }
    close(p[1]);
    char c;
    int ready = read_full(p[0], &c, 1) == 1;
    close(p[0]);
    return ready ? pid : -1;
}

/* Ends a counting child: the SIGUSR2s it took, or -1. */
static int child_count(pid_t pid) {
    kill(pid, SIGTERM);
    int status;
    if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status)) return -1;
    return WEXITSTATUS(status);
}

static int read_file(const char *path, void *buf, long cap) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    long n = read_full(fd, buf, cap);
    close(fd);
    return n;
}

static int write_file(const char *path, const void *buf, long len, int sync) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) return -1;
    int ok = write(fd, buf, len) == len && (!sync || fsync(fd) == 0);
    return close(fd) == 0 && ok ? 0 : -1;
}

static int dir_has(const char *dir, const char *name) {
    DIR *d = opendir(dir);
    if (!d) return 0;
    struct dirent *e;
    int found = 0;
    while ((e = readdir(d)))
        if (strcmp(e->d_name, name) == 0) found = 1;
    closedir(d);
    return found;
}

static unsigned char big[4 * MIB];

/* ------------------------------------------------------------------- peer */

static char dir[128];
static int peer_failures_before;

/* Answers a step: OK if the checks since `peer_failures_before` held. */
static void answer(const char *step) {
    if (failures == peer_failures_before) say("OK %s", step);
    else say("FAIL %s (%d checks failed, see the peer's log)", step, failures - peer_failures_before);
    peer_failures_before = failures;
}

static void on_alarm(int sig) {
    (void)sig;
    static const char text[] = "treetest: watchdog expired\n";
    write(2, text, sizeof text - 1);
    _exit(3);
}

/* The peer's server work while the other tree holds all memory: none of
 * it may need memory of the program's own (static buffers, no malloc),
 * only its server's heap, whose reserve no other tree can take. */
static void heap_reserve(void) {
    static char buf[4096];
    static char dents[16384];
    int fd = open("/etc/passwd", O_RDONLY);
    check("memory full elsewhere: open and read a file", fd >= 0 && read(fd, buf, sizeof buf) > 0);
    if (fd >= 0) close(fd);
    int p[2];
    int ok = pipe(p) == 0;
    if (ok) {
        ok = write(p[1], "through the pipe", 16) == 16 && read(p[0], buf, 16) == 16;
        close(p[0]);
        close(p[1]);
    }
    check("memory full elsewhere: a pipe", ok);
    int sv[2];
    ok = socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0;
    if (ok) {
        ok = send(sv[0], "unix", 4, 0) == 4 && recv(sv[1], buf, 4, 0) == 4;
        close(sv[0]);
        close(sv[1]);
    }
    check("memory full elsewhere: an AF_UNIX socket pair", ok);
    int ep = epoll_create1(0);
    check("memory full elsewhere: epoll_create1", ep >= 0);
    if (ep >= 0) close(ep);
    /* A directory listing: the server's snapshot of it. */
    int d = open("/bin", O_RDONLY | O_DIRECTORY);
    long entries = 0, n;
    while (d >= 0 && (n = syscall(SYS_getdents64, d, dents, sizeof dents)) > 0) {
        for (long off = 0; off < n; off += *(unsigned short *)(dents + off + 16)) entries++;
    }
    if (d >= 0) close(d);
    check("memory full elsewhere: list /bin", entries > 50);
    /* A process may not fit (the kernel's memory for it), but nothing breaks. */
    pid_t c = fork();
    if (c == 0) _exit(7);
    int status = 0;
    check("memory full elsewhere: fork works or fails with ENOMEM/EAGAIN",
          c > 0 ? waitpid(c, &status, 0) == c && WIFEXITED(status) && WEXITSTATUS(status) == 7 : errno == ENOMEM || errno == EAGAIN);
}

/* The reserve of netd's budget while the other tree used up its share. */
static void net_reserve(void) {
    int u = bound(SOCK_DGRAM, "127.0.0.1", 0);
    int ok = u >= 0;
    check("netd full elsewhere: a UDP socket", ok);
    if (ok) {
        struct sockaddr_in me;
        socklen_t len = sizeof me;
        char got[16] = {0};
        getsockname(u, (struct sockaddr *)&me, &len);
        ok = sendto(u, "reserve", 7, 0, (struct sockaddr *)&me, sizeof me) == 7 && wait_fd(u, POLLIN, STEP_MS) && recv(u, got, sizeof got, 0) == 7 &&
             memcmp(got, "reserve", 7) == 0;
        close(u);
    }
    check("netd full elsewhere: a datagram to itself", ok);
    int srv = bound(SOCK_STREAM, "127.0.0.1", 0);
    ok = srv >= 0 && listen(srv, 1) == 0;
    check("netd full elsewhere: a TCP listener", ok);
    if (!ok) return;
    struct sockaddr_in at;
    socklen_t len = sizeof at;
    getsockname(srv, (struct sockaddr *)&at, &len);
    int c = tcp_connect(ntohs(at.sin_port));
    int a = c >= 0 ? accept_within(srv, STEP_MS) : -1;
    check("netd full elsewhere: a TCP connection", c >= 0 && a >= 0);
    if (c >= 0 && a >= 0) {
        static unsigned char out[32768], in[32768];
        fill(out, sizeof out, 5);
        ok = write_all(c, out, sizeof out) == 0 && read_full(a, in, sizeof in) == sizeof in && matches(in, sizeof in, 5);
        check("netd full elsewhere: 32 KiB over it", ok);
    }
    if (c >= 0) close(c);
    if (a >= 0) close(a);
    close(srv);
}

/* ------------------------------------------------- /data coherence (ADR 0014) */

/* Both trees use one page cache per file: what one writes the other reads
 * at once, without fsync, as two processes on one Linux machine. */

#define RECORD 32
#define RECORDS 200

static char *at_dir(const char *name) {
    static char paths[4][192];
    static int next;
    char *p = paths[next++ % 4];
    snprintf(p, sizeof paths[0], "%s/%s", dir, name);
    return p;
}

/* Appends `n` records of `who` (one write(2) each, O_APPEND). */
static int append_records(const char *path, char who, int n) {
    int fd = open(path, O_WRONLY | O_APPEND);
    if (fd < 0) return -1;
    int ok = 1;
    for (int i = 0; i < n && ok; i++) {
        char rec[RECORD + 1];
        snprintf(rec, sizeof rec, "%c%05d-------------------------\n", who, i);
        ok = write(fd, rec, RECORD) == RECORD;
    }
    return close(fd) == 0 && ok ? 0 : -1;
}

/* Free blocks of /data, in KiB. */
static long free_kib(void) {
    struct statvfs s;
    if (statvfs(dir, &s) < 0) return -1;
    return (long)(s.f_bfree * s.f_bsize / 1024);
}

/* Waits up to `ms` until /data has at least `kib` KiB free: whether it did. */
static int free_reaches(long kib, long ms) {
    long until = now_ms() + ms;
    while (free_kib() < kib) {
        if (now_ms() >= until) return 0;
        usleep(20000);
    }
    return 1;
}

static int kept = -1, hold5 = -1;
static unsigned char *peer_map;

/* The peer's side of the coherence steps. */
static void coherent_peer(const char *cmd) {
    static char buf[4096];
    if (strcmp(cmd, "C_SEE1") == 0) {
        struct stat st;
        check("another tree's write, not fsync'ed, is read here at once",
              read_file(at_dir("c1"), buf, sizeof buf) == 26 && memcmp(buf, "written by main, no fsync\n", 26) == 0);
        check("... and its size", stat(at_dir("c1"), &st) == 0 && st.st_size == 26);
        fill(big, 65536, 11);
        check("write a file here without fsync", write_file(at_dir("c2"), big, 65536, 0) == 0);
    } else if (strcmp(cmd, "C_KEEP") == 0) {
        kept = open(at_dir("c1"), O_RDWR);
        check("open and read a file (its pages cached here)", kept >= 0 && pread(kept, buf, 26, 0) == 26);
    } else if (strcmp(cmd, "C_SEE2") == 0) {
        check("the other tree's overwrite reaches the pages read here before",
              pread(kept, buf, 26, 0) == 26 && memcmp(buf, "MAIN WROTE AGAIN", 16) == 0 && memcmp(buf + 16, " no fsync\n", 10) == 0);
        check("overwrite them here", pwrite(kept, "PEER WROTE BACK", 15, 0) == 15);
        close(kept);
    } else if (strncmp(cmd, "C_APPEND", 8) == 0) {
        check("200 appends (O_APPEND) while the other tree appends", append_records(at_dir("c_app"), 'P', RECORDS) == 0);
    } else if (strcmp(cmd, "C_TRUNC") == 0) {
        struct stat st;
        check("the other tree's truncation: its size here", stat(at_dir("c2"), &st) == 0 && st.st_size == 1000);
        check("... and its end", read_file(at_dir("c2"), big, 65536) == 1000 && matches(big, 1000, 11));
        check("truncate it to nothing here", truncate(at_dir("c2"), 0) == 0);
    } else if (strncmp(cmd, "C_TIMES ", 8) == 0) {
        struct stat st;
        long mtime = atol(cmd + 8);
        check("the other tree's write: size and mtime here before any write-back",
              stat(at_dir("c3"), &st) == 0 && st.st_size == 4096 && st.st_mtime == mtime);
        struct timespec ts[2] = {{0, UTIME_OMIT}, {1000000000, 0}};
        check("set its mtime here (utimensat)", utimensat(AT_FDCWD, at_dir("c3"), ts, 0) == 0);
    } else if (strcmp(cmd, "C_MAP") == 0) {
        int fd = open(at_dir("c4"), O_RDWR);
        peer_map = fd < 0 ? MAP_FAILED : mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (fd >= 0) close(fd);
        check("map the other tree's file shared", peer_map != MAP_FAILED);
        if (peer_map == MAP_FAILED) return;
        check("its store through its mapping is seen in this mapping", memcmp(peer_map, "mapped by main", 14) == 0);
        memcpy(peer_map + 4096, "mapped by peer", 14);
    } else if (strcmp(cmd, "C_MAP2") == 0) {
        if (peer_map == MAP_FAILED || !peer_map) return;
        check("its next store is seen here at once", memcmp(peer_map, "changed by main", 15) == 0);
        munmap(peer_map, 8192);
    } else if (strcmp(cmd, "C_HOLD") == 0) {
        hold5 = open(at_dir("c5"), O_RDONLY);
        check("open the other tree's file and keep it open", hold5 >= 0 && read(hold5, buf, 16) == 16);
    } else if (strcmp(cmd, "C_DROP") == 0) {
        check("the other tree's unlink is seen here", access(at_dir("c5"), F_OK) < 0 && errno == ENOENT);
        check("the file open here still reads", pread(hold5, big, MIB, 0) == MIB && matches(big, MIB, 12));
        check("close it (its last close anywhere)", close(hold5) == 0);
    } else if (strcmp(cmd, "C_GONE6") == 0) {
        long before = free_kib();
        check("unlink a file the other tree has cached (closed)", unlink(at_dir("c6")) == 0);
        check("its blocks are free at once (the other tree lets go of it)", before > 0 && free_reaches(before + 900, 10000));
        if (free_kib() < before + 900) printf("treetest peer: %ld KiB free before, %ld after\n", before, free_kib());
    } else {
        say("FAIL unknown command '%s'", cmd);
        return;
    }
    answer(cmd);
}

static int peer(int argc, char **argv) {
    who = "treetest peer";
    /* Started by the kernel without the console: no standard descriptors.
     * Its output goes to a log on /data, which the first tree prints. */
    int no_stdio = fcntl(0, F_GETFD) < 0 && fcntl(1, F_GETFD) < 0 && fcntl(2, F_GETFD) < 0;
    if (argc < 4) return 2;
    int port = atoi(argv[2]);
    snprintf(dir, sizeof dir, "%s", argv[3]);
    int null = open("/dev/null", O_RDWR);
    char path[192];
    snprintf(path, sizeof path, "%s/peer.log", dir);
    int log = open(path, O_WRONLY | O_CREAT | O_TRUNC | O_APPEND, 0644);
    if (null != 0 || log < 0 || dup2(log, 1) != 1 || dup2(log, 2) != 2) return 2;
    if (log > 2) close(log);
    setvbuf(stdout, NULL, _IOLBF, 0);
    signal(SIGALRM, on_alarm);
    alarm(IDLE_MS / 1000 * 2);
    /* Stack pages for every step now, while memory is there (the step
     * that runs while the other tree holds all of it needs none). */
    touch_stack();
    signal(SIGUSR2, on_usr2);

    long until = now_ms() + STEP_MS;
    while ((ctl = tcp_connect(port)) < 0 && now_ms() < until) usleep(50000);
    if (ctl < 0) {
        printf("treetest peer: cannot reach the first tree on port %d\n", port);
        return 2;
    }
    say("HELLO %d %d %d", getpid(), getppid(), no_stdio);

    int tcp = -1, udp = -1;
    pid_t child = -1;
    char cmd[256];
    while (hear(cmd, sizeof cmd, IDLE_MS)) {
        if (strncmp(cmd, "PROC ", 5) == 0) {
            int other = atoi(cmd + 5);
            int pids[64], found;
            int n = proc_scan(pids, 64, "sh\0/etc/autorun", 15, &found);
            int self_only = n == 1 && pids[0] == 1;
            check("/proc lists this tree's one process: pid 1", self_only);
            check("/proc shows nothing of the other tree's", !found);
            char path2[64];
            snprintf(path2, sizeof path2, "/proc/%d", other);
            check("/proc/<the other's pid> does not exist here", other == 1 || access(path2, F_OK) < 0);
            char link[16] = {0};
            readlink("/proc/self", link, sizeof link - 1);
            check("/proc/self is 1", strcmp(link, "1") == 0);
            answer("PROC");
        } else if (strcmp(cmd, "TMP") == 0) {
            check("the other tree's /tmp file is not here", access("/tmp/treetest.mark", F_OK) < 0 && errno == ENOENT);
            check("nor its file in the root tmpfs", access("/etc/treetest.mark", F_OK) < 0 && errno == ENOENT);
            check("a file of the same name here",
                  write_file("/tmp/treetest.mark", "peer\n", 5, 0) == 0 && write_file("/etc/treetest.mark", "peer\n", 5, 0) == 0);
            answer("TMP");
        } else if (strncmp(cmd, "SIG ", 4) == 0) {
            int other = 0, other_child = 0;
            sscanf(cmd + 4, "%d %d", &other, &other_child);
            child = counting_child();
            check("a child to count signals", child > 0);
            /* Pids are the tree's own: the other's numbers name nobody here. */
            for (int k = 0; k < 2; k++) {
                int target = k ? other_child : other;
                if (target == 1 || target == child) continue;
                check("the other tree's pid is ESRCH here", kill(target, 0) < 0 && errno == ESRCH);
                check("... and SIGKILL to it too", kill(target, SIGKILL) < 0 && errno == ESRCH);
            }
            check("kill(-1) reaches this tree's processes",  kill(-1, SIGUSR2) == 0);
            say("CHILD %d", child);
            answer("SIG");
        } else if (strcmp(cmd, "SIGEND") == 0) {
            check("the child got only this tree's kill(-1)", child_count(child) == 1);
            check("pid 1 got no signal of the other tree's", usr2_count == 0);
            answer("SIGEND");
        } else if (strcmp(cmd, "LISTEN") == 0) {
            tcp = bound(SOCK_STREAM, "127.0.0.1", TCP_PORT);
            check("listen on a TCP port (SO_REUSEADDR)", tcp >= 0 && listen(tcp, 4) == 0);
            udp = bound(SOCK_DGRAM, "127.0.0.1", UDP_PORT);
            check("bind a UDP port (SO_REUSEADDR)", udp >= 0);
            answer("LISTEN");
        } else if (strcmp(cmd, "XFER") == 0) {
            int c = accept_within(tcp, STEP_MS);
            check("the other tree's connection arrives here", c >= 0);
            if (c >= 0) {
                check("1 MiB from the other tree intact", read_full(c, big, MIB) == MIB && matches(big, MIB, 1));
                fill(big, MIB, 2);
                check("1 MiB back to it", write_all(c, big, MIB) == 0);
                /* This side closes first: its end goes to TIME-WAIT. */
                close(c);
            }
            char got[32] = {0};
            check("the other tree's datagram arrives here",
                  wait_fd(udp, POLLIN, STEP_MS) && recv(udp, got, sizeof got, 0) == 13 && memcmp(got, "udp-from-main", 13) == 0);
            answer("XFER");
        } else if (strcmp(cmd, "UNLISTEN") == 0) {
            close(tcp);
            close(udp);
            answer("UNLISTEN");
        } else if (strcmp(cmd, "CONNECT") == 0) {
            int c = tcp_connect(BACK_PORT);
            check("connect to the other tree's listener", c >= 0);
            if (c >= 0) {
                fill(big, 65536, 3);
                check("64 KiB to the other tree", write_all(c, big, 65536) == 0);
                check("64 KiB from it intact", read_full(c, big, 65536) == 65536 && matches(big, 65536, 4));
                close(c);
            }
            answer("CONNECT");
        } else if (strcmp(cmd, "NETRESERVE") == 0) {
            net_reserve();
            answer("NETRESERVE");
        } else if (strcmp(cmd, "MEMRESERVE") == 0) {
            holding = 1;
            heap_reserve();
            answer("MEMRESERVE");
        } else if (strcmp(cmd, "DATA1") == 0) {
            char p1[192], p2[192], buf[64] = {0};
            snprintf(p1, sizeof p1, "%s/m1", dir);
            check("the other tree's new file is listed here", dir_has(dir, "m1"));
            check("its contents (fsync'ed there)", read_file(p1, buf, sizeof buf) == 10 && memcmp(buf, "from main\n", 10) == 0);
            snprintf(p2, sizeof p2, "%s/p1", dir);
            check("rename it", rename(p1, p2) == 0);
            snprintf(p1, sizeof p1, "%s/pd", dir);
            check("mkdir", mkdir(p1, 0755) == 0);
            snprintf(p1, sizeof p1, "%s/p2", dir);
            fill(big, 65536, 6);
            check("write and fsync a file", write_file(p1, big, 65536, 1) == 0);
            answer("DATA1");
        } else if (strcmp(cmd, "SYNC") == 0) {
            char p[192];
            snprintf(p, sizeof p, "%s/m3", dir);
            /* The other tree wrote m3 without fsync: sync(2) here writes
             * back every instance's caches. */
            sync();
            check("sync(2) writes back the other tree's cache: its data here", read_file(p, big, 2 * 16384) == 16384 && matches(big, 16384, 7));
            answer("SYNC");
        } else if (strcmp(cmd, "MAKE4") == 0) {
            char p[192];
            snprintf(p, sizeof p, "%s/p4", dir);
            fill(big, 262144, 8);
            check("a 256 KiB file, fsync'ed", write_file(p, big, 262144, 1) == 0);
            answer("MAKE4");
        } else if (strcmp(cmd, "UNLINK4") == 0) {
            char p[192];
            snprintf(p, sizeof p, "%s/p4", dir);
            check("unlink the file the other tree holds open", unlink(p) == 0);
            answer("UNLINK4");
        } else if (strncmp(cmd, "C_", 2) == 0) {
            coherent_peer(cmd);
        } else if (strcmp(cmd, "PING") == 0) {
            /* Memory is back: the results of MEMRESERVE into the log. */
            release_held();
            answer("PING");
        } else if (strcmp(cmd, "BYE") == 0) {
            say("BYE %d", failures);
            fsync(1);
            return failures ? 1 : 0;
        } else {
            say("FAIL unknown command '%s'", cmd);
        }
    }
    printf("treetest peer: the first tree went silent\n");
    fsync(1);
    return 2;
}

/* ------------------------------------------------------------------- main */

static long tree = -1;

static void main_alarm(int sig) {
    (void)sig;
    static const char text[] = "treetest: watchdog expired, the second tree is killed\n";
    write(1, text, sizeof text - 1);
    if (tree > 0) {
        syscall(TEST_TREE_KILL, tree);
        syscall(TEST_TREE_WAIT, tree, 10000000000L);
    }
    _exit(1);
}

/* Sends a command (none if `fmt` is NULL: it went already) and checks the
 * answer of the peer's checks. */
static int step(const char *name, const char *fmt, ...) {
    char out[256], in[512];
    if (fmt) {
        va_list ap;
        va_start(ap, fmt);
        vsnprintf(out, sizeof out, fmt, ap);
        va_end(ap);
        say("%s", out);
    }
    int heard = hear(in, sizeof in, STEP_MS);
    char want[64];
    snprintf(want, sizeof want, "OK %s", name);
    char what[128];
    snprintf(what, sizeof what, "the second tree's checks: %s", name);
    check(what, heard && strcmp(in, want) == 0);
    if (!heard) printf("treetest: no answer to %s\n", name);
    else if (strcmp(in, want) != 0) printf("treetest: the second tree answered '%s'\n", in);
    return heard;
}

static void print_log(void) {
    char path[192];
    snprintf(path, sizeof path, "%s/peer.log", dir);
    int fd = open(path, O_RDONLY);
    if (fd < 0) return;
    printf("--- the second tree's log\n");
    fflush(stdout);
    char buf[4096];
    long n;
    while ((n = read(fd, buf, sizeof buf)) > 0) write(1, buf, n);
    close(fd);
    printf("---\n");
}

/* Ends the second tree (killed if it still runs) and returns its wait status. */
static int end_tree(long ms) {
    long status = syscall(TEST_TREE_WAIT, tree, ms * 1000000L);
    if (status < 0) {
        printf("treetest: the second tree did not end in time (%s): killed\n", strerror(errno));
        syscall(TEST_TREE_KILL, tree);
        status = syscall(TEST_TREE_WAIT, tree, 30000000000L);
    }
    return status;
}

/* Checks a file of the two trees' records: every record whole, each
 * tree's in its order and all of them there. */
static int records_ok(const char *path) {
    static char data[2 * RECORDS * RECORD + 1];
    long n = read_file(path, data, sizeof data);
    if (n != 2 * RECORDS * RECORD) {
        printf("treetest: the appended file has %ld bytes, not %d\n", n, 2 * RECORDS * RECORD);
        return 0;
    }
    int next[2] = {0, 0};
    for (long at = 0; at < n; at += RECORD) {
        char *rec = data + at;
        int who = rec[0] == 'M' ? 0 : rec[0] == 'P' ? 1 : -1;
        if (who < 0 || atoi(rec + 1) != next[who] || rec[RECORD - 1] != '\n') {
            printf("treetest: a broken record at %ld: %.31s\n", at, rec);
            return 0;
        }
        next[who]++;
    }
    return next[0] == RECORDS && next[1] == RECORDS;
}

/* The main tree's side of the coherence steps (ADR 0014): writes seen both
 * ways without fsync, by cached pages too, appends of both trees, a
 * truncation, size and times, MAP_SHARED, and an unlinked file's blocks
 * freed at its last close in either tree. */
static int coherent_main(void) {
    static char buf[4096];
    int alive;
    check("write a file without fsync", write_file(at_dir("c1"), "written by main, no fsync\n", 26, 0) == 0);
    if (!(alive = step("C_SEE1", "C_SEE1"))) return 0;
    check("the other tree's write, not fsync'ed, is read here at once", read_file(at_dir("c2"), big, 2 * 65536) == 65536 && matches(big, 65536, 11));

    /* Pages cached in both trees. */
    if (!(alive = step("C_KEEP", "C_KEEP"))) return 0;
    int fd = open(at_dir("c1"), O_RDWR);
    check("overwrite the file the other tree has cached", fd >= 0 && pwrite(fd, "MAIN WROTE AGAIN", 16, 0) == 16);
    if (!(alive = step("C_SEE2", "C_SEE2"))) return 0;
    check("its overwrite reaches the pages cached here", fd >= 0 && pread(fd, buf, 26, 0) == 26 && memcmp(buf, "PEER WROTE BACKN no fsync\n", 26) == 0);
    if (fd >= 0) close(fd);

    /* Appends of both trees at once: none lost, none torn. */
    close(open(at_dir("c_app"), O_WRONLY | O_CREAT | O_TRUNC, 0644));
    say("C_APPEND %d", RECORDS);
    check("200 appends (O_APPEND) while the other tree appends", append_records(at_dir("c_app"), 'M', RECORDS) == 0);
    if (!(alive = step("C_APPEND 200", NULL))) return 0;
    check("... every record of both trees there, whole and in order", records_ok(at_dir("c_app")));

    /* A truncation either way. */
    fd = open(at_dir("c2"), O_RDWR);
    check("truncate the other tree's file", fd >= 0 && ftruncate(fd, 1000) == 0);
    if (!(alive = step("C_TRUNC", "C_TRUNC"))) return 0;
    struct stat st;
    check("its truncation is seen here: size and end", fd >= 0 && fstat(fd, &st) == 0 && st.st_size == 0 && pread(fd, buf, 16, 0) == 0);
    if (fd >= 0) close(fd);

    /* Size and times before any write-back. */
    fill(big, 4096, 14);
    check("write a file without fsync", write_file(at_dir("c3"), big, 4096, 0) == 0);
    check("... its mtime", stat(at_dir("c3"), &st) == 0);
    char name[64];
    snprintf(name, sizeof name, "C_TIMES %ld", (long)st.st_mtime);
    if (!(alive = step(name, "%s", name))) return 0;
    check("the mtime the other tree set is seen here", stat(at_dir("c3"), &st) == 0 && st.st_mtime == 1000000000);
    check("... and stays after fsync", (fd = open(at_dir("c3"), O_RDONLY)) >= 0 && fsync(fd) == 0 && stat(at_dir("c3"), &st) == 0 && st.st_mtime == 1000000000);
    if (fd >= 0) close(fd);

    /* MAP_SHARED in both trees: one page, each store seen at once. */
    fd = open(at_dir("c4"), O_RDWR | O_CREAT | O_TRUNC, 0644);
    unsigned char *map = fd >= 0 && ftruncate(fd, 8192) == 0 ? mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) : MAP_FAILED;
    if (fd >= 0) close(fd);
    check("map a file shared", map != MAP_FAILED);
    if (map != MAP_FAILED) {
        memcpy(map, "mapped by main", 14);
        if (!(alive = step("C_MAP", "C_MAP"))) return 0;
        check("the other tree's store through its mapping is seen here at once", memcmp(map + 4096, "mapped by peer", 14) == 0);
        memcpy(map, "changed by main", 15);
        if (!(alive = step("C_MAP2", "C_MAP2"))) return 0;
        munmap(map, 8192);
    }

    /* An unlinked file's blocks: kept while the other tree has it open, free
     * at its last close there. */
    fill(big, MIB, 12);
    check("a 1 MiB file, fsync'ed", write_file(at_dir("c5"), big, MIB, 1) == 0);
    if (!(alive = step("C_HOLD", "C_HOLD"))) return 0;
    long before = free_kib();
    check("unlink it while the other tree has it open", unlink(at_dir("c5")) == 0);
    usleep(200000);
    check("... its blocks stay meanwhile", free_kib() < before + 900);
    if (!(alive = step("C_DROP", "C_DROP"))) return 0;
    check("... and are free at its last close there", free_reaches(before + 900, 10000));

    /* And a file only cached here (read and closed) that the other tree unlinks. */
    fill(big, MIB, 15);
    check("a 1 MiB file, fsync'ed, read and closed here", write_file(at_dir("c6"), big, MIB, 1) == 0 && read_file(at_dir("c6"), big, MIB) == MIB);
    return step("C_GONE6", "C_GONE6");
}

/* A third tree that becomes the owner of a file's dirty pages (it writes
 * without fsync, and stores through a shared mapping) and is killed while
 * it is: the main tree, which shares the file, reads the data, writes it
 * back, and nothing of it waits for the dead tree. */
static int victim(int argc, char **argv) {
    if (argc < 3) return 2;
    snprintf(dir, sizeof dir, "%s", argv[2]);
    fill(big, 65536, 13);
    int fd = open(at_dir("c7"), O_RDWR);
    if (fd < 0 || write(fd, big, 65536) != 65536) return 2;
    unsigned char *map = mmap(NULL, 65536 + 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (map == MAP_FAILED) return 2;
    memcpy(map + 65536, "stored by the victim", 20);
    close(open(at_dir("c7.ready"), O_WRONLY | O_CREAT, 0644));
    for (;;) pause();
}

static void killed_tree(void) {
    /* The file, cached here first (so this tree shares its object). */
    int fd = open(at_dir("c7"), O_RDWR | O_CREAT | O_TRUNC, 0644);
    check("a file for the tree to be killed, cached here", fd >= 0 && ftruncate(fd, 65536 + 4096) == 0 && pread(fd, big, 16, 0) == 16);
    if (fd < 0) return;
    char *args[] = {"treetest", "victim", dir, NULL};
    long victim_tree = syscall(TEST_TREE_START, "/bin/treetest", args);
    check("a third tree starts", victim_tree > 0);
    if (victim_tree <= 0) return;
    long until = now_ms() + STEP_MS;
    while (access(at_dir("c7.ready"), F_OK) < 0 && now_ms() < until) usleep(20000);
    check("it wrote and stored into the file (no fsync)", access(at_dir("c7.ready"), F_OK) == 0);
    check("kill it while its writes are dirty", syscall(TEST_TREE_KILL, victim_tree) == 0);
    long status = syscall(TEST_TREE_WAIT, victim_tree, 30000000000L);
    check("... it ends", status >= 0);
    static char tail[32];
    long start = now_ms();
    check("its writes are read here", pread(fd, big, 65536, 0) == 65536 && matches(big, 65536, 13));
    check("... and its store through its mapping", pread(fd, tail, 20, 65536) == 20 && memcmp(tail, "stored by the victim", 20) == 0);
    check("fsync here writes them back, waiting for nothing of the dead tree's", fsync(fd) == 0);
    check("... and so does an append", write(fd, "more", 4) == 4 && fsync(fd) == 0);
    check("... in a few seconds", now_ms() - start < 10000);
    close(fd);
}

static void cleanup(void) {
    static const char *names[] = {"m1", "p1", "p2", "m3", "p4", "peer.log", "c1", "c2", "c3", "c4", "c5", "c6", "c7", "c7.ready", "c_app"};
    char p[192];
    for (unsigned i = 0; i < sizeof names / sizeof *names; i++) {
        snprintf(p, sizeof p, "%s/%s", dir, names[i]);
        unlink(p);
    }
    snprintf(p, sizeof p, "%s/pd", dir);
    rmdir(p);
    rmdir(dir);
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "peer") == 0) return peer(argc, argv);
    if (argc > 1 && strcmp(argv[1], "victim") == 0) return victim(argc, argv);
    setvbuf(stdout, NULL, _IOLBF, 0);
    signal(SIGALRM, main_alarm);
    alarm(600);
    touch_stack();
    snprintf(dir, sizeof dir, "/data/treetest");
    cleanup();
    if (mkdir(dir, 0755) < 0) {
        printf("treetest: cannot make %s: %s\n", dir, strerror(errno));
        return 1;
    }

    char *bad[] = {"treetest", NULL};
    check("a tree from a relative path: EINVAL", syscall(TEST_TREE_START, "bin/treetest", bad) < 0 && errno == EINVAL);
    check("a handle that names no tree: ECHILD", syscall(TEST_TREE_WAIT, 1L, 0L) < 0 && errno == ECHILD);

    int srv = bound(SOCK_STREAM, "127.0.0.1", CTL_PORT);
    if (srv < 0 || listen(srv, 1) < 0) {
        printf("treetest: cannot listen on %d: %s\n", CTL_PORT, strerror(errno));
        return 1;
    }
    char port[16];
    snprintf(port, sizeof port, "%d", CTL_PORT);
    char *args[] = {"treetest", "peer", port, dir, NULL};
    tree = syscall(TEST_TREE_START, "/bin/treetest", args);
    check("the kernel starts a second process tree", tree > 0);
    if (tree <= 0) return 1;
    check("... which still runs (wait with no time: ETIMEDOUT)", syscall(TEST_TREE_WAIT, tree, 0L) < 0 && errno == ETIMEDOUT);
    ctl = accept_within(srv, STEP_MS);
    close(srv);
    check("the second tree connects over loopback", ctl >= 0);
    char in[512];
    int pid = 0, ppid = -1, no_stdio = 0;
    int alive = ctl >= 0 && hear(in, sizeof in, STEP_MS) && sscanf(in, "HELLO %d %d %d", &pid, &ppid, &no_stdio) == 3;
    check("it says hello", alive);
    if (alive) {
        check("its program is pid 1 of its own namespace, without a parent", pid == 1 && ppid == 0);
        check("this one is not pid 1 (its tree's init is)", getpid() != 1);
        check("it started without standard descriptors (no console)", no_stdio == 1);

        /* /proc: each tree's own processes. */
        int pids[512], found;
        proc_scan(pids, 512, "treetest\0peer", 13, &found);
        check("/proc here shows nothing of the second tree's", !found);
        char cmd[64] = {0};
        read_file("/proc/1/cmdline", cmd, sizeof cmd - 1);
        check("/proc/1 here is this tree's init", strcmp(cmd, "sh") == 0);
        alive = step("PROC", "PROC %d", getpid());

        /* tmpfs: each tree's own. */
        if (alive) {
            check("files in /tmp and the root tmpfs",
                  write_file("/tmp/treetest.mark", "main\n", 5, 0) == 0 && write_file("/etc/treetest.mark", "main\n", 5, 0) == 0);
            alive = step("TMP", "TMP");
            char buf[16] = {0}, buf2[16] = {0};
            check("they keep their contents beside the second tree's",
                  read_file("/tmp/treetest.mark", buf, 15) == 5 && strcmp(buf, "main\n") == 0 && read_file("/etc/treetest.mark", buf2, 15) == 5 &&
                      strcmp(buf2, "main\n") == 0);
            unlink("/tmp/treetest.mark");
            unlink("/etc/treetest.mark");
        }

        /* Signals stay in their tree. */
        if (alive) {
            pid_t mine = counting_child();
            check("a child to count signals", mine > 0);
            say("SIG %d %d", getpid(), mine);
            int theirs = 0;
            alive = hear(in, sizeof in, STEP_MS) && sscanf(in, "CHILD %d", &theirs) == 1;
            check("the second tree's child", alive && theirs > 0);
            if (alive) alive = step("SIG", NULL);
            check("the second tree's kill(-1) did not end this tree's child", kill(mine, 0) == 0);
            /* Its child's pid names nobody here, or one of this tree's own. */
            if (theirs > 1 && kill(theirs, 0) < 0) {
                check("the second tree's child's pid is ESRCH here", errno == ESRCH);
                check("... and SIGKILL to it too", kill(theirs, SIGKILL) < 0 && errno == ESRCH);
            }
            check("kill(-1) here", kill(-1, SIGUSR2) == 0);
            check("this tree's child got only this tree's kill(-1)", child_count(mine) == 1);
            if (alive) alive = step("SIGEND", "SIGEND");
        }

        /* netd: ports, connections and datagrams between the trees. */
        if (alive) alive = step("LISTEN", "LISTEN");
        if (alive) {
            check("its TCP port is EADDRINUSE here (SO_REUSEADDR)", in_use(SOCK_STREAM, "127.0.0.1", TCP_PORT));
            check("... also on the wildcard address", in_use(SOCK_STREAM, "0.0.0.0", TCP_PORT));
            check("its UDP port is EADDRINUSE here (SO_REUSEADDR)", in_use(SOCK_DGRAM, "127.0.0.1", UDP_PORT));
            int u1 = bound(SOCK_DGRAM, "127.0.0.1", UDP_SHARED), u2 = bound(SOCK_DGRAM, "127.0.0.1", UDP_SHARED);
            check("SO_REUSEADDR still shares a UDP port within this tree", u1 >= 0 && u2 >= 0);
            if (u1 >= 0) {
                struct sockaddr_in to = inet("127.0.0.1", UDP_PORT);
                check("a datagram to the second tree's port",
                      sendto(u1, "udp-from-main", 13, 0, (struct sockaddr *)&to, sizeof to) == 13);
                close(u1);
            }
            if (u2 >= 0) close(u2);
            say("XFER");
            int c = tcp_connect(TCP_PORT);
            check("connect to the second tree's listener", c >= 0);
            if (c >= 0) {
                fill(big, MIB, 1);
                check("1 MiB to the second tree", write_all(c, big, MIB) == 0);
                check("1 MiB from it intact", read_full(c, big, MIB) == MIB && matches(big, MIB, 2));
                char x;
                check("then its end of the connection", read_full(c, &x, 1) == 0);
                close(c);
            }
            alive = hear(in, sizeof in, STEP_MS);
            check("the second tree's checks: XFER", alive && strcmp(in, "OK XFER") == 0);
            if (alive && strcmp(in, "OK XFER") != 0) printf("treetest: the second tree answered '%s'\n", in);
        }
        if (alive) {
            check("its listener's port is still EADDRINUSE here", in_use(SOCK_STREAM, "127.0.0.1", TCP_PORT));
            alive = step("UNLISTEN", "UNLISTEN");
            check("its closed connection (TIME-WAIT) keeps the port from this tree", in_use(SOCK_STREAM, "127.0.0.1", TCP_PORT));
            int u = bound(SOCK_DGRAM, "127.0.0.1", UDP_PORT);
            check("the UDP port it closed is free for this tree", u >= 0);
            if (u >= 0) close(u);
        }
        if (alive) {
            int back = bound(SOCK_STREAM, "127.0.0.1", BACK_PORT);
            check("listen here", back >= 0 && listen(back, 1) == 0);
            say("CONNECT");
            int c = accept_within(back, STEP_MS);
            check("the second tree's connection arrives", c >= 0);
            if (c >= 0) {
                check("64 KiB from it intact", read_full(c, big, 65536) == 65536 && matches(big, 65536, 3));
                fill(big, 65536, 4);
                check("64 KiB back", write_all(c, big, 65536) == 0);
                close(c);
            }
            if (back >= 0) close(back);
            alive = hear(in, sizeof in, STEP_MS);
            check("the second tree's checks: CONNECT", alive && strcmp(in, "OK CONNECT") == 0);
        }

        /* netd's budget: this tree takes its whole share, the other keeps its reserve. */
        if (alive) {
            struct rlimit rl;
            getrlimit(RLIMIT_NOFILE, &rl);
            rl.rlim_cur = rl.rlim_max;
            setrlimit(RLIMIT_NOFILE, &rl);
            static int socks[4096];
            int n = 0, err = 0;
            while (n < 4096) {
                int s = bound(SOCK_DGRAM, "127.0.0.1", 0);
                if (s < 0) {
                    err = errno;
                    break;
                }
                socks[n++] = s;
            }
            printf("treetest: %d UDP sockets, then %s\n", n, strerror(err));
            check("this tree uses up its share of netd: ENOBUFS", err == ENOBUFS && n >= 8);
            alive = step("NETRESERVE", "NETRESERVE");
            for (int i = 0; i < n; i++) close(socks[i]);
            int s = bound(SOCK_DGRAM, "127.0.0.1", 0);
            check("its sockets closed, this tree has sockets again", s >= 0);
            if (s >= 0) close(s);
        }

        /* The server's heap: this tree takes all memory, the other's server keeps its reserve. */
        if (alive) {
            static void *chunks[4096];
            int n = 0;
            while (n < 4096) {
                void *p = mmap(NULL, MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
                if (p == MAP_FAILED) break;
                memset(p, 0x5a, MIB);
                chunks[n++] = p;
            }
            int err = errno;
            printf("treetest: %d MiB mapped, then %s\n", n, strerror(err));
            check("this tree takes all memory: ENOMEM", n > 16 && err == ENOMEM);
            alive = step("MEMRESERVE", "MEMRESERVE");
            for (int i = 0; i < n; i++) munmap(chunks[i], MIB);
            if (alive) alive = step("PING", "PING");
        }

        /* /data: one disk for both, through diskfs. */
        if (alive) {
            char p[192], buf[64] = {0};
            snprintf(p, sizeof p, "%s/m1", dir);
            check("write and fsync a file on /data", write_file(p, "from main\n", 10, 1) == 0);
            alive = step("DATA1", "DATA1");
            check("its rename is seen here at once", access(p, F_OK) < 0 && errno == ENOENT);
            snprintf(p, sizeof p, "%s/p1", dir);
            check("the file under its new name", read_file(p, buf, sizeof buf) == 10 && memcmp(buf, "from main\n", 10) == 0);
            check("the second tree's directory and file are listed", dir_has(dir, "pd") && dir_has(dir, "p2") && !dir_has(dir, "m1"));
            snprintf(p, sizeof p, "%s/p2", dir);
            check("its fsync'ed file's contents", read_file(p, big, 2 * 65536) == 65536 && matches(big, 65536, 6));
        }
        if (alive) {
            char p[192];
            snprintf(p, sizeof p, "%s/m3", dir);
            fill(big, 16384, 7);
            check("write a file without fsync", write_file(p, big, 16384, 0) == 0);
            alive = step("SYNC", "SYNC");
        }
        if (alive) alive = step("MAKE4", "MAKE4");
        if (alive) {
            char p[192];
            snprintf(p, sizeof p, "%s/p4", dir);
            int fd = open(p, O_RDONLY);
            check("open the second tree's file", fd >= 0);
            alive = step("UNLINK4", "UNLINK4");
            check("its unlink is seen here", access(p, F_OK) < 0 && errno == ENOENT);
            check("the open file is still readable here, intact",
                  fd >= 0 && read_full(fd, big, 262144 + 1) == 262144 && matches(big, 262144, 8));
            /* (Its blocks go with this close: C_DROP and C_GONE6 check.) */
            if (fd >= 0) close(fd);
        }
        if (alive) alive = coherent_main();

        if (alive) {
            say("BYE");
            int theirs = -1;
            check("the second tree says goodbye", hear(in, sizeof in, STEP_MS) && sscanf(in, "BYE %d", &theirs) == 1);
        }
    }
    if (ctl >= 0) close(ctl);
    int status = end_tree(STEP_MS);
    check("the second tree ends (its pid 1 exits 0)", status >= 0 && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    if (status >= 0 && !(WIFEXITED(status) && WEXITSTATUS(status) == 0)) printf("treetest: its wait status: %#x\n", status);
    check("its handle is gone after the wait", syscall(TEST_TREE_WAIT, tree, 0L) < 0 && errno == ECHILD);
    check("... and kills nothing", syscall(TEST_TREE_KILL, tree) < 0 && errno == ECHILD);
    killed_tree();
    print_log();
    cleanup();
    printf("treetest: %s\n", failures ? "FAILED" : "all passed");
    return failures ? 1 : 0;
}
