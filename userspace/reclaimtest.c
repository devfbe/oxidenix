/* Reclaim under stress: processes that map a disk file (shared, and
 * private with copies of their own of some pages) and read it keep
 * checking every page they touch while other processes commit and use all
 * the memory left, over and over, so the cache's pages, mapped ones
 * included, are reclaimed and read again all the time. Every page must
 * always hold what it should (the file's data, or the private copy's), no
 * process may be killed, and committed memory must always be usable. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define PG 4096
#define MIB (1024 * 1024L)
/* The file: as large as the memory that using all the commit limit
 * leaves free, and 8 MiB more, less the kernel's reserve (16 MiB, which
 * user memory cannot take), so that its pages must give way. */
static long SIZE, PAGES;
#define MAPPERS 4
#define HOGS 2
#define ROUNDS 3

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static unsigned char pattern(long off) {
    return (unsigned char)((off / PG) * 13 + off % 251);
}

static long meminfo(const char *key) {
    char text[4096] = {0};
    int fd = open("/proc/meminfo", O_RDONLY);
    read(fd, text, sizeof text - 1);
    close(fd);
    char *p = strstr(text, key);
    return p ? strtol(p + strlen(key) + 1, NULL, 10) : -1;
}

/* Checks a few bytes of every page of a mapping of the file; private
 * pages this process wrote (every 7th, from `own` on) hold its mark. */
static int verify(const unsigned char *m, int priv, int own) {
    for (long p = 0; p < PAGES; p++) {
        for (long o = p % 61; o < PG; o += 1021) {
            long off = p * PG + o;
            unsigned char want = pattern(off);
            if (priv && p % 7 == own % 7) want = (unsigned char)(0x80 | own);
            if (m[off] != want) {
                fprintf(stderr, "    page %ld byte %ld: %#x, want %#x\n", p, o, m[off], want);
                return 0;
            }
        }
    }
    return 1;
}

/* A mapper: maps the file shared (even ones) or private (odd ones, with
 * copies of their own of every 7th page), and checks it until `stop`. */
static void mapper(int i, const char *path, volatile int *stop) {
    int fd = open(path, O_RDONLY);
    int priv = i % 2;
    /* (Private ones commit only the pages they copy: MAP_NORESERVE.) */
    unsigned char *m = mmap(NULL, SIZE, priv ? PROT_READ | PROT_WRITE : PROT_READ, priv ? MAP_PRIVATE | MAP_NORESERVE : MAP_SHARED, fd, 0);
    if (m == MAP_FAILED) _exit(2);
    if (priv)
        for (long p = i % 7; p < PAGES; p += 7) memset(m + p * PG, 0x80 | i, PG);
    long rounds = 0;
    while (!*stop) {
        if (!verify(m, priv, i)) _exit(3);
        rounds++;
    }
    /* And once more after the pressure. */
    _exit(verify(m, priv, i) && rounds > 0 ? 0 : 4);
}

/* A reader: pread() of random pages, checked, until `stop`. */
static void reader(const char *path, volatile int *stop) {
    int fd = open(path, O_RDONLY);
    static unsigned char buf[PG];
    unsigned seed = 7;
    while (!*stop) {
        long p = rand_r(&seed) % PAGES;
        if (pread(fd, buf, PG, p * PG) != PG) _exit(2);
        for (long o = 0; o < PG; o += 509)
            if (buf[o] != pattern(p * PG + o)) _exit(3);
    }
    _exit(0);
}

/* A hog: commits `len` bytes, writes each page's number into it, checks
 * all of them, lets go; ROUNDS times. */
static void hog(long len) {
    for (int r = 0; r < ROUNDS; r++) {
        long *m = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (m == MAP_FAILED) _exit(2);
        for (long p = 0; p < len / PG; p++) m[p * (PG / sizeof(long)) + p % 512] = p ^ r;
        for (long p = 0; p < len / PG; p++)
            if (m[p * (PG / sizeof(long)) + p % 512] != (p ^ r)) _exit(3);
        munmap(m, len);
    }
    _exit(0);
}

int main(int argc, char **argv) {
    const char *dir = argc > 1 ? argv[1] : "/data";
    char path[256];
    snprintf(path, sizeof path, "%s/reclaimtest.file", dir);
    long left = (meminfo("CommitLimit:") - meminfo("Committed_AS:")) * 1024;
    SIZE = (meminfo("MemFree:") * 1024 - left - 8 * MIB) / MIB * MIB;
    if (SIZE < 8 * MIB) SIZE = 8 * MIB;
    if (SIZE > 40 * MIB) SIZE = 40 * MIB;
    PAGES = SIZE / PG;
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    static unsigned char chunk[64 * 1024];
    for (long off = 0; off < SIZE; off += sizeof chunk) {
        for (size_t i = 0; i < sizeof chunk; i++) chunk[i] = pattern(off + (long)i);
        write(fd, chunk, sizeof chunk);
    }
    printf("    a file of %ld MiB\n", SIZE / MIB);
    check("the file written back (fsync)", fsync(fd) == 0);
    close(fd);

    volatile int *stop = mmap(NULL, PG, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    *stop = 0;
    pid_t kids[MAPPERS + 1];
    for (int i = 0; i < MAPPERS; i++)
        if ((kids[i] = fork()) == 0) mapper(i, path, stop);
    if ((kids[MAPPERS] = fork()) == 0) reader(path, stop);
    struct timespec settle = {0, 300 * 1000000L};
    nanosleep(&settle, NULL);

    /* What the commit limit leaves, shared by the hogs, less 2 MiB. */
    long room = (meminfo("CommitLimit:") - meminfo("Committed_AS:")) * 1024 - 2 * MIB;
    long each = room / HOGS / PG * PG;
    printf("    %d hogs of %ld MiB, %d rounds, Cached %ld kB\n", HOGS, each / MIB, ROUNDS, meminfo("Cached:"));
    pid_t hogs[HOGS];
    for (int h = 0; h < HOGS; h++)
        if ((hogs[h] = fork()) == 0) hog(each);
    int hogs_ok = 1;
    for (int h = 0; h < HOGS; h++) {
        int st = -1;
        waitpid(hogs[h], &st, 0);
        if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) {
            printf("    hog %d: status %#x\n", h, st);
            hogs_ok = 0;
        }
    }
    check("committed memory is always usable, and holds what was written", hogs_ok);
    *stop = 1;
    int mappers_ok = 1;
    for (int i = 0; i <= MAPPERS; i++) {
        int st = -1;
        waitpid(kids[i], &st, 0);
        if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) {
            printf("    %s %d: status %#x\n", i < MAPPERS ? "mapper" : "reader", i, st);
            mappers_ok = 0;
        }
    }
    check("mapped pages (shared, private and copied) always hold their data", mappers_ok);
    unlink(path);
    printf("%s\n", failures ? "reclaimtest: FAILED" : "reclaimtest: all passed");
    return failures != 0;
}
