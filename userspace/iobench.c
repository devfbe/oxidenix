/* I/O benchmarks (docs/benchmarks/README.md): IPC round trip latency, /proc reads,
 * sequential block I/O, small synchronous reads, durable metadata operations
 * and small fsyncs on the disk, TCP throughput over
 * loopback and over the network card, each with the system calls, IPC
 * round trips, IPC bytes, address space switches, user copies and kernel
 * heap allocations per operation from /proc/counters (where it exists:
 * on Linux those columns are "-").
 *
 * Output: one line per result, "<name> <value> <unit>", and one line per
 * benchmark with the per-operation counters, so a script can collect them.
 *
 * Usage: iobench [dir]   (default /data; the disk under test) */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define KIB 1024L
#define MIB (1024 * 1024L)

static const char *dir = "/data";
static char *buf;

static uint64_t rdtsc(void) {
    uint32_t lo, hi;
    __asm__ volatile("lfence; rdtsc" : "=a"(lo), "=d"(hi));
    return (uint64_t)hi << 32 | lo;
}

static double now(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

/* ---------------------------------------------------------- counters */

enum { SYSCALLS, IPC_CALLS, IPC_BYTES, SWITCHES, COPY_BYTES, ALLOCS, NCOUNTERS };
static const char *counter_names[NCOUNTERS] = {"syscalls", "ipc_calls", "ipc_bytes", "address_space_switches", "user_copy_bytes", "heap_allocs"};
static int have_counters;

static void counters(long long out[NCOUNTERS]) {
    char text[1024] = {0};
    int fd = open("/proc/counters", O_RDONLY);
    have_counters = fd >= 0;
    for (int i = 0; i < NCOUNTERS; i++) out[i] = 0;
    if (fd < 0) return;
    read(fd, text, sizeof text - 1);
    close(fd);
    for (int i = 0; i < NCOUNTERS; i++) {
        char *p = strstr(text, counter_names[i]);
        if (p) out[i] = atoll(p + strlen(counter_names[i]) + 1);
    }
}

static long long before[NCOUNTERS];

static void start_counting(void) {
    counters(before);
}

/* Prints the counters per operation since start_counting(); the reading
 * of /proc/counters itself is subtracted (measured once at startup). */
static long long probe[NCOUNTERS];

static void per_op(const char *name, double ops) {
    long long after[NCOUNTERS];
    counters(after);
    printf("counters %s", name);
    for (int i = 0; i < NCOUNTERS; i++) {
        if (!have_counters) {
            printf(" %s=-", counter_names[i]);
            continue;
        }
        double d = (after[i] - before[i] - probe[i]) / ops;
        printf(" %s=%.2f", counter_names[i], d < 0 ? 0 : d);
    }
    printf("\n");
}

static void measure_probe(void) {
    long long a[NCOUNTERS], b[NCOUNTERS];
    counters(a);
    counters(b);
    for (int i = 0; i < NCOUNTERS; i++) probe[i] = b[i] - a[i];
}

/* ------------------------------------------------------- percentiles */

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

static void percentiles(const char *name, uint64_t *v, int n, const char *unit) {
    qsort(v, n, sizeof *v, cmp_u64);
    printf("%s_p50 %llu %s\n", name, (unsigned long long)v[n / 2], unit);
    printf("%s_p99 %llu %s\n", name, (unsigned long long)v[n * 99 / 100], unit);
}

/* ------------------------------------------------------------ latency */

#define SAMPLES 20000

static void null_syscall(void) {
    static uint64_t t[SAMPLES];
    start_counting();
    for (int i = 0; i < SAMPLES; i++) {
        uint64_t a = rdtsc();
        getppid();
        t[i] = rdtsc() - a;
    }
    per_op("null_syscall", SAMPLES);
    percentiles("null_syscall", t, SAMPLES, "cycles");
}

/* A system call no kernel knows (1999, ENOSYS): in oxidenix the Linux
 * server answers it without asking the kernel, so this is the cost of
 * forwarding a call to the server and back alone (two kernel entries and
 * two page table switches). */
static void forwarded_null_syscall(void) {
    static uint64_t t[SAMPLES];
    start_counting();
    for (int i = 0; i < SAMPLES; i++) {
        uint64_t a = rdtsc();
        syscall(1999);
        t[i] = rdtsc() - a;
    }
    per_op("forwarded_null_syscall", SAMPLES);
    percentiles("forwarded_null_syscall", t, SAMPLES, "cycles");
}

/* Processes: fork and wait of a child that exits at once, fork, exec of a
 * small static program and wait, and a signal sent to the caller and handled
 * (kill, the handler's frame, rt_sigreturn). Microseconds per operation. */
#define PROC_SAMPLES 200
#define SIGNAL_SAMPLES 5000

static volatile sig_atomic_t handled;
static void on_usr1(int sig) {
    (void)sig;
    handled++;
}

static void processes(void) {
    static uint64_t t[PROC_SAMPLES];
    start_counting();
    for (int i = 0; i < PROC_SAMPLES; i++) {
        double a = now();
        pid_t p = fork();
        if (p == 0) _exit(0);
        waitpid(p, NULL, 0);
        t[i] = (uint64_t)((now() - a) * 1e6);
    }
    per_op("fork_wait", PROC_SAMPLES);
    percentiles("fork_wait", t, PROC_SAMPLES, "us");
    start_counting();
    for (int i = 0; i < PROC_SAMPLES; i++) {
        double a = now();
        pid_t p = fork();
        if (p == 0) {
            /* (Its greeting goes nowhere.) */
            int null = open("/dev/null", O_WRONLY);
            dup2(null, 1);
            char *args[] = {"hello", NULL};
            execv("/bin/hello", args);
            _exit(127);
        }
        waitpid(p, NULL, 0);
        t[i] = (uint64_t)((now() - a) * 1e6);
    }
    per_op("fork_exec_wait", PROC_SAMPLES);
    percentiles("fork_exec_wait", t, PROC_SAMPLES, "us");
    static uint64_t s[SIGNAL_SAMPLES];
    signal(SIGUSR1, on_usr1);
    pid_t me = getpid();
    start_counting();
    for (int i = 0; i < SIGNAL_SAMPLES; i++) {
        uint64_t a = rdtsc();
        kill(me, SIGUSR1);
        s[i] = rdtsc() - a;
    }
    per_op("signal_handled", SIGNAL_SAMPLES);
    percentiles("signal_handled", s, SIGNAL_SAMPLES, "cycles");
    signal(SIGUSR1, SIG_DFL);
}

/* fstat of a file on the disk: one IPC round trip to the filesystem
 * server (a small request and reply), against fstat of a tmpfs file. */
static void ipc_round_trip(void) {
    static uint64_t t[SAMPLES];
    char path[256];
    snprintf(path, sizeof path, "%s/iobench.stat", dir);
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    struct stat st;
    start_counting();
    for (int i = 0; i < SAMPLES; i++) {
        uint64_t a = rdtsc();
        fstat(fd, &st);
        t[i] = rdtsc() - a;
    }
    per_op("fstat_disk", SAMPLES);
    percentiles("fstat_disk", t, SAMPLES, "cycles");
    close(fd);
    unlink(path);

    int tfd = open("/tmp/iobench.stat", O_RDWR | O_CREAT | O_TRUNC, 0644);
    start_counting();
    for (int i = 0; i < SAMPLES; i++) {
        uint64_t a = rdtsc();
        fstat(tfd, &st);
        t[i] = rdtsc() - a;
    }
    per_op("fstat_tmpfs", SAMPLES);
    percentiles("fstat_tmpfs", t, SAMPLES, "cycles");
    close(tfd);

    /* stat by path: resolution (four names, in the Linux server since
     * R6c.2b) plus the attributes. */
    mkdir("/tmp/iobench.d", 0755);
    mkdir("/tmp/iobench.d/a", 0755);
    rename("/tmp/iobench.stat", "/tmp/iobench.d/a/f");
    start_counting();
    for (int i = 0; i < SAMPLES; i++) {
        uint64_t a = rdtsc();
        stat("/tmp/iobench.d/a/f", &st);
        t[i] = rdtsc() - a;
    }
    per_op("stat_path_tmpfs", SAMPLES);
    percentiles("stat_path_tmpfs", t, SAMPLES, "cycles");
    unlink("/tmp/iobench.d/a/f");
    rmdir("/tmp/iobench.d/a");
    rmdir("/tmp/iobench.d");
}

/* -------------------------------------------------------------- /proc */

#define PROC_SAMPLES 2000

/* Reading /proc: a system-wide file (procfs's, /proc/meminfo) and a
 * process's own (/proc/self/stat) from the start with pread on an open
 * descriptor, as top and htop re-read them, and /proc/self/stat opened,
 * read and closed by path. */
static void proc_reads(void) {
    static uint64_t t[PROC_SAMPLES];
    static char text[8192];
    const char *files[] = {"/proc/meminfo", "/proc/self/stat"};
    const char *names[] = {"proc_meminfo_pread", "proc_self_stat_pread"};
    for (int f = 0; f < 2; f++) {
        int fd = open(files[f], O_RDONLY);
        if (fd < 0) return;
        start_counting();
        for (int i = 0; i < PROC_SAMPLES; i++) {
            uint64_t a = rdtsc();
            pread(fd, text, sizeof text, 0);
            t[i] = rdtsc() - a;
        }
        per_op(names[f], PROC_SAMPLES);
        percentiles(names[f], t, PROC_SAMPLES, "cycles");
        close(fd);
    }
    start_counting();
    for (int i = 0; i < PROC_SAMPLES; i++) {
        uint64_t a = rdtsc();
        int fd = open("/proc/self/stat", O_RDONLY);
        read(fd, text, sizeof text);
        close(fd);
        t[i] = rdtsc() - a;
    }
    per_op("proc_self_stat_open_read_close", PROC_SAMPLES);
    percentiles("proc_self_stat_open_read_close", t, PROC_SAMPLES, "cycles");
}

/* ------------------------------------------------------------- block */

#define FILE_SIZE (16 * MIB)
#define IO_SIZE (64 * KIB)

static void block_io(void) {
    char path[256];
    snprintf(path, sizeof path, "%s/iobench.data", dir);
    unlink(path);
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    for (long i = 0; i < IO_SIZE; i++) buf[i] = (char)i;

    start_counting();
    double t = now();
    for (long off = 0; off < FILE_SIZE; off += IO_SIZE) write(fd, buf, IO_SIZE);
    fsync(fd);
    t = now() - t;
    per_op("seq_write_64k", FILE_SIZE / IO_SIZE);
    printf("seq_write %.1f MB/s\n", FILE_SIZE / t / 1e6);
    close(fd);

    /* From the disk: O_DIRECT bypasses the page cache. */
    fd = open(path, O_RDONLY | O_DIRECT);
    start_counting();
    t = now();
    for (long off = 0; off < FILE_SIZE; off += IO_SIZE) pread(fd, buf, IO_SIZE, off);
    t = now() - t;
    per_op("seq_read_disk_64k", FILE_SIZE / IO_SIZE);
    printf("seq_read_disk %.1f MB/s\n", FILE_SIZE / t / 1e6);
    close(fd);

    /* From the page cache (read once to fill it). */
    fd = open(path, O_RDONLY);
    for (long off = 0; off < FILE_SIZE; off += IO_SIZE) pread(fd, buf, IO_SIZE, off);
    start_counting();
    t = now();
    for (long off = 0; off < FILE_SIZE; off += IO_SIZE) pread(fd, buf, IO_SIZE, off);
    t = now() - t;
    per_op("seq_read_cached_64k", FILE_SIZE / IO_SIZE);
    printf("seq_read_cached %.1f MB/s\n", FILE_SIZE / t / 1e6);

    /* 4 KiB synchronous reads at random offsets. */
    enum { N = 2000 };
    static uint64_t lat[N];
    unsigned seed = 1;
    start_counting();
    for (int i = 0; i < N; i++) {
        off_t off = (rand_r(&seed) % (FILE_SIZE / 4096)) * 4096;
        double a = now();
        pread(fd, buf, 4096, off);
        lat[i] = (uint64_t)((now() - a) * 1e9);
    }
    per_op("read_4k_cached", N);
    percentiles("read_4k_cached", lat, N, "ns");

    /* The same in cycles, without the clock calls around it, and the clock
     * call alone. */
    static uint64_t cyc[N];
    for (int i = 0; i < N; i++) {
        off_t off = (rand_r(&seed) % (FILE_SIZE / 4096)) * 4096;
        uint64_t a = rdtsc();
        pread(fd, buf, 4096, off);
        cyc[i] = rdtsc() - a;
    }
    percentiles("pread_4k_cached", cyc, N, "cycles");
    struct timespec ts;
    for (int i = 0; i < N; i++) {
        uint64_t a = rdtsc();
        clock_gettime(CLOCK_MONOTONIC, &ts);
        cyc[i] = rdtsc() - a;
    }
    percentiles("clock_gettime", cyc, N, "cycles");
    close(fd);

    fd = open(path, O_RDONLY | O_DIRECT);
    start_counting();
    for (int i = 0; i < N; i++) {
        off_t off = (rand_r(&seed) % (FILE_SIZE / 4096)) * 4096;
        double a = now();
        pread(fd, buf, 4096, off);
        lat[i] = (uint64_t)((now() - a) * 1e9);
    }
    per_op("read_4k_disk", N);
    percentiles("read_4k_disk", lat, N, "ns");
    close(fd);
    unlink(path);
}

/* ---------------------------------------------------------- metadata */

#define META_FILES 500

/* Metadata operations on the disk, each durable when it returns (a
 * transaction of diskfs's journal): creations, renames, removals, and a
 * 4 KiB overwrite made durable by fsync. */
static void metadata_ops(void) {
    static uint64_t lat[META_FILES];
    char d[256], path[300], other[300];
    snprintf(d, sizeof d, "%s/iobench.meta", dir);
    mkdir(d, 0755);
    const char *names[] = {"create_disk", "rename_disk", "unlink_disk"};
    for (int op = 0; op < 3; op++) {
        start_counting();
        for (int i = 0; i < META_FILES; i++) {
            snprintf(path, sizeof path, "%s/f%d", d, i);
            snprintf(other, sizeof other, "%s/renamed%d", d, i);
            double a = now();
            if (op == 0) close(open(path, O_RDWR | O_CREAT | O_EXCL, 0644));
            if (op == 1) rename(path, other);
            if (op == 2) unlink(other);
            lat[i] = (uint64_t)((now() - a) * 1e9);
        }
        per_op(names[op], META_FILES);
        percentiles(names[op], lat, META_FILES, "ns");
    }
    snprintf(path, sizeof path, "%s/sync", d);
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    pwrite(fd, buf, 4096, 0);
    fsync(fd);
    enum { N = 200 };
    start_counting();
    for (int i = 0; i < N; i++) {
        buf[0] = (char)i;
        double a = now();
        pwrite(fd, buf, 4096, 0);
        fsync(fd);
        lat[i] = (uint64_t)((now() - a) * 1e9);
    }
    per_op("write_fsync_4k", N);
    percentiles("write_fsync_4k", lat, N, "ns");
    close(fd);
    unlink(path);
    rmdir(d);
}

/* --------------------------------------------------------------- TCP */

static int tcp_connect(const char *ip, int port) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(port)};
    inet_pton(AF_INET, ip, &a.sin_addr);
    if (connect(fd, (struct sockaddr *)&a, sizeof a) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

#define TCP_BYTES (32 * MIB)

/* A client sends TCP_BYTES over loopback to a forked receiver, which
 * answers one byte at the end: the time covers the whole transfer. */
static void tcp_loopback(void) {
    int srv = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(srv, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = htons(5201), .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    if (bind(srv, (struct sockaddr *)&a, sizeof a) != 0 || listen(srv, 1) != 0) {
        printf("tcp_loopback - (no listener: %s)\n", strerror(errno));
        return;
    }
    pid_t child = fork();
    if (child == 0) {
        int c = accept(srv, NULL, NULL);
        static char rbuf[64 * KIB];
        long total = 0;
        ssize_t n;
        while (total < TCP_BYTES && (n = read(c, rbuf, sizeof rbuf)) > 0) total += n;
        write(c, "k", 1);
        _exit(total == TCP_BYTES ? 0 : 1);
    }
    close(srv);
    int fd = tcp_connect("127.0.0.1", 5201);
    start_counting();
    double t = now();
    for (long sent = 0; sent < TCP_BYTES;) {
        ssize_t n = write(fd, buf, IO_SIZE);
        if (n <= 0) break;
        sent += n;
    }
    char k;
    read(fd, &k, 1);
    t = now() - t;
    per_op("tcp_loopback_64k", TCP_BYTES / IO_SIZE);
    int st;
    waitpid(child, &st, 0);
    printf("tcp_loopback %.1f MB/s%s\n", TCP_BYTES / t / 1e6, WIFEXITED(st) && WEXITSTATUS(st) == 0 ? "" : " (incomplete)");
    close(fd);
}

#define NET_BYTES (4 * MIB)

/* Through the network card: QEMU's echo service (10.0.2.100:7, user
 * networking) returns what it gets; a forked reader drains the echo. */
static void tcp_network(const char *ip, int port) {
    int fd = tcp_connect(ip, port);
    if (fd < 0) {
        printf("tcp_network - (cannot connect to %s:%d)\n", ip, port);
        return;
    }
    pid_t reader = fork();
    if (reader == 0) {
        static char rbuf[64 * KIB];
        long total = 0;
        ssize_t n;
        while (total < NET_BYTES && (n = read(fd, rbuf, sizeof rbuf)) > 0) total += n;
        _exit(total == NET_BYTES ? 0 : 1);
    }
    start_counting();
    double t = now();
    for (long sent = 0; sent < NET_BYTES;) {
        ssize_t n = write(fd, buf, IO_SIZE);
        if (n <= 0) break;
        sent += n;
    }
    int st;
    waitpid(reader, &st, 0);
    t = now() - t;
    per_op("tcp_network_echo_64k", NET_BYTES / IO_SIZE);
    printf("tcp_network_echo %.1f MB/s%s\n", NET_BYTES / t / 1e6, WIFEXITED(st) && WEXITSTATUS(st) == 0 ? "" : " (incomplete)");
    close(fd);
}

int main(int argc, char **argv) {
    if (argc > 1) dir = argv[1];
    signal(SIGPIPE, SIG_IGN);
    if (posix_memalign((void **)&buf, 4096, IO_SIZE) != 0) return 1;
    measure_probe();
    null_syscall();
    forwarded_null_syscall();
    processes();
    ipc_round_trip();
    proc_reads();
    block_io();
    metadata_ops();
    tcp_loopback();
    tcp_network(argc > 2 ? argv[2] : "10.0.2.100", argc > 3 ? atoi(argv[3]) : 7);
    printf("iobench: done\n");
    return 0;
}
