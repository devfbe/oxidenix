/* The page cache of files on a filesystem server (/data): repeated reads
 * come from memory, writes and truncation stay coherent with cached pages
 * and mappings, programs run from the disk, and cached pages give way
 * when memory is committed. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define PG 4096
#define MIB (1024 * 1024L)
#define BIG (2 * MIB)

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static long now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000000L + ts.tv_nsec / 1000;
}

static char pattern(long off) {
    return (char)('A' + (off / PG * 7 + off) % 26);
}

static char buf[65536];

/* Reads the whole file, checks the pattern; returns microseconds or -1. */
static long read_all(const char *path, long size) {
    int fd = open(path, O_RDONLY);
    long t = now_us(), off = 0;
    ssize_t n;
    int good = 1;
    while ((n = read(fd, buf, sizeof buf)) > 0) {
        for (ssize_t i = 0; i < n; i += 997) good &= buf[i] == pattern(off + i);
        off += n;
    }
    close(fd);
    return good && off == size ? now_us() - t : -1;
}

static long meminfo(const char *key) {
    char text[4096] = {0};
    int fd = open("/proc/meminfo", O_RDONLY);
    read(fd, text, sizeof text - 1);
    close(fd);
    char *p = strstr(text, key);
    return p ? strtol(p + strlen(key) + 1, NULL, 10) : -1;
}

int main(int argc, char **argv) {
    /* Run as a program from the disk: a child that sleeps a while. */
    if (strcmp(argv[0], "sleeper") == 0) {
        sleep(5);
        return 0;
    }
    const char *dir = argc > 1 ? argv[1] : "/data";
    char path[256], prog[256];
    snprintf(path, sizeof path, "%s/cachetest.big", dir);
    snprintf(prog, sizeof prog, "%s/cachetest.prog", dir);

    /* Writes stay in the page cache (write-back, as on Linux) until fsync
     * makes them durable; then the pages are clean: reclaim may drop them. */
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    for (long off = 0; off < BIG; off += sizeof buf) {
        for (size_t i = 0; i < sizeof buf; i++) buf[i] = pattern(off + i);
        write(fd, buf, sizeof buf);
    }
    check("fsync writes the file back", fsync(fd) == 0);
    close(fd);

    long took = read_all(path, BIG);
    printf("    reading 2 MiB just written: %ld us\n", took);
    check("a file reads back correctly", took >= 0);
    long cached = meminfo("Cached:");
    printf("    Cached: %ld kB\n", cached);
    check("/proc/meminfo counts the cached pages", cached >= BIG / 1024);

    /* Cached pages make room for committed memory: commit all but 1 MiB
     * (the page tables of the mapping need some), touch every page. */
    long limit = meminfo("CommitLimit:"), committed = meminfo("Committed_AS:");
    long room = (limit - committed) * 1024 - MIB;
    char *all = mmap(NULL, room, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    printf("    committing %ld MiB\n", room / MIB);
    check("committing all free memory reclaims cached pages", all != MAP_FAILED);
    if (all != MAP_FAILED) {
        for (long off = 0; off < room; off += PG) all[off] = 1;
        check("... and all of it can be used", all[room - 1] == 0 && all[0] == 1);
        printf("    Cached while committed: %ld kB\n", meminfo("Cached:"));
        munmap(all, room);
    }
    took = read_all(path, BIG);
    printf("    reading it again from the disk: %ld us\n", took);
    check("the file reads correctly afterwards (from the disk)", took >= 0);
    check("... at more than 2 MB/s", took >= 0 && took < 1000000);

    /* Writes reach cached pages, mappings and the disk alike. */
    fd = open(path, O_RDWR);
    char *shared = mmap(NULL, 4 * PG, PROT_READ, MAP_SHARED, fd, 0);
    char *priv = mmap(NULL, 4 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    check("read-only shared and private mappings of a disk file", shared != MAP_FAILED && priv != MAP_FAILED && shared[5] == pattern(5) && priv[PG] == pattern(PG));
    pwrite(fd, "new", 3, PG + 10);
    char back[3] = {0};
    pread(fd, back, 3, PG + 10);
    check("pwrite is read back from the cache", memcmp(back, "new", 3) == 0);
    check("... and is visible in both mappings", memcmp(shared + PG + 10, "new", 3) == 0 && memcmp(priv + PG + 10, "new", 3) == 0);
    priv[0] = 'p';
    check("a private store stays private", shared[0] == pattern(0) && (pread(fd, back, 1, 0), back[0]) == pattern(0));

    /* Truncation drops cached pages and mappings beyond the end. */
    ftruncate(fd, PG + 100);
    struct stat st;
    fstat(fd, &st);
    check("ftruncate sets the size", st.st_size == PG + 100);
    check("reading beyond the new end returns nothing", pread(fd, back, 1, 3 * PG) == 0);
    ftruncate(fd, 3 * PG);
    check("growing again reads zero, not the old data", pread(fd, back, 1, 2 * PG) == 1 && back[0] == 0 && pread(fd, back, 1, PG + 200) == 1 && back[0] == 0);
    check("... and so does the shared mapping", shared[2 * PG] == 0 && shared[PG + 200] == 0 && shared[PG + 10] == 'n');
    munmap(shared, 4 * PG);
    munmap(priv, 4 * PG);
    close(fd);
    unlink(path);

    /* Programs on the disk run from its page cache (this one). */
    int in = open("/bin/cachetest", O_RDONLY), out = open(prog, O_WRONLY | O_CREAT | O_TRUNC, 0755);
    ssize_t n;
    while ((n = read(in, buf, sizeof buf)) > 0) write(out, buf, n);
    close(in);
    close(out);
    pid_t sleeper = fork();
    if (sleeper == 0) {
        execl(prog, "sleeper", (char *)NULL);
        _exit(127);
    }
    struct timespec ts = {0, 200 * 1000000L};
    nanosleep(&ts, NULL);
    check("a program on the disk cannot be written while it runs", open(prog, O_WRONLY) == -1 && errno == ETXTBSY);
    kill(sleeper, SIGTERM);
    int status;
    waitpid(sleeper, &status, 0);
    check("... and it ran (until killed)", WIFSIGNALED(status) && WTERMSIG(status) == SIGTERM);
    unlink(prog);

    printf("%s\n", failures ? "cachetest: FAILED" : "cachetest: all passed");
    return failures != 0;
}
