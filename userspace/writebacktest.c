/* Write-back of shared writable mappings of disk files: stores make pages
 * dirty (Dirty: in /proc/meminfo), msync, fsync and the flusher write them
 * (Dirty: back to 0), also after the mapping is gone; they survive
 * reclaim, mix with write() in one page and with truncation. O_DIRECT
 * reads show what is on the disk (after writing back their range, as on
 * Linux). Expects no other dirty pages meanwhile. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
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

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static char path[256];
static char *direct_buf;

/* The byte at `off` as stored on the disk (an O_DIRECT read of its page);
 * -1 if the read fails, 0 past the end. */
static int on_disk(off_t off) {
    int fd = open(path, O_RDONLY | O_DIRECT);
    if (fd < 0) return -1;
    off_t page = off & ~(off_t)(PG - 1);
    ssize_t n = pread(fd, direct_buf, PG, page);
    close(fd);
    if (n < 0) return -1;
    return off - page < n ? (unsigned char)direct_buf[off - page] : 0;
}

static long meminfo(const char *key) {
    char text[4096] = {0};
    int fd = open("/proc/meminfo", O_RDONLY);
    read(fd, text, sizeof text - 1);
    close(fd);
    char *p = strstr(text, key);
    return p ? strtol(p + strlen(key) + 1, NULL, 10) : -1;
}

static long dirty_kb(void) {
    return meminfo("Dirty:");
}

/* A file of `pages` pages, page i filled with 'a' + i, written back
 * (write() leaves dirty pages, as on Linux: fsync writes them). */
static void make_file(int pages) {
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    char buf[PG];
    for (int i = 0; i < pages; i++) {
        memset(buf, 'a' + i, PG);
        write(fd, buf, PG);
    }
    fsync(fd);
    close(fd);
}

int main(int argc, char **argv) {
    const char *dir = argc > 1 ? argv[1] : "/data";
    snprintf(path, sizeof path, "%s/writebacktest.file", dir);
    if (posix_memalign((void **)&direct_buf, PG, PG) != 0) return 1;

    make_file(4);
    check("O_DIRECT reads what write() and fsync put on the disk", on_disk(0) == 'a' && on_disk(3 * PG + 9) == 'd');

    int fd = open(path, O_RDWR);
    char *m = mmap(NULL, 4 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    check("a writable shared mapping of a disk file", m != MAP_FAILED);
    if (m == MAP_FAILED) return 1;

    check("reading through it makes nothing dirty", m[0] == 'a' && m[3 * PG] == 'd' && dirty_kb() == 0);
    m[PG + 5] = 'X';
    check("a store makes its page dirty", dirty_kb() == 4);
    check("msync(MS_SYNC) writes it back", msync(m, 4 * PG, MS_SYNC) == 0 && dirty_kb() == 0 && on_disk(PG + 5) == 'X');
    m[2 * PG] = 'Y';
    check("fsync writes a store back", dirty_kb() == 4 && fsync(fd) == 0 && dirty_kb() == 0 && on_disk(2 * PG) == 'Y');
    m[PG + 6] = 'X';
    check("... also a page stored to again after a write-back", dirty_kb() == 4 && fdatasync(fd) == 0 && dirty_kb() == 0 && on_disk(PG + 6) == 'X');

    /* A store and a write() in the same page both arrive. */
    m[10] = 'M';
    pwrite(fd, "W", 1, 20);
    check("an O_DIRECT read writes back what write() left first", on_disk(20) == 'W');
    msync(m, PG, MS_SYNC);
    check("... and a store in the same page arrives too", on_disk(10) == 'M' && on_disk(20) == 'W');

    /* Without being asked: the flusher writes within a few seconds, also
     * once the mapping is gone. */
    m[3 * PG + 1] = 'Z';
    munmap(m, 4 * PG);
    close(fd);
    int flushed = 0;
    for (int i = 0; i < 100 && !flushed; i++) {
        struct timespec ts = {0, 100 * 1000000L};
        nanosleep(&ts, NULL);
        flushed = dirty_kb() == 0;
    }
    check("a store is written back on its own after munmap", flushed && on_disk(3 * PG + 1) == 'Z');

    /* A child stores and exits. */
    fd = open(path, O_RDWR);
    if (fork() == 0) {
        char *c = mmap(NULL, PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        c[100] = 'C';
        _exit(0);
    }
    wait(NULL);
    fsync(fd);
    check("a store of a process that exited is written by fsync", on_disk(100) == 'C');

    /* Dirty pages survive reclaim: commit all free memory. */
    m = mmap(NULL, 4 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    m[2 * PG + 7] = 'R';
    munmap(m, 4 * PG);
    long room = (meminfo("CommitLimit:") - meminfo("Committed_AS:")) * 1024 - MIB;
    char *all = mmap(NULL, room, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (all != MAP_FAILED) munmap(all, room);
    char c = 0;
    pread(fd, &c, 1, 2 * PG + 7);
    check("a dirty page is not dropped by reclaim", all != MAP_FAILED && c == 'R');
    fsync(fd);
    check("... and reaches the disk", on_disk(2 * PG + 7) == 'R');

    /* Truncating a file with dirty pages. */
    m = mmap(NULL, 4 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    m[3 * PG + 2] = 'T';
    m[PG + 8] = 'K';
    ftruncate(fd, 2 * PG);
    fsync(fd);
    struct stat st;
    fstat(fd, &st);
    check("truncating drops dirty pages beyond the end", st.st_size == 2 * PG && on_disk(3 * PG + 2) == 0);
    check("... and keeps the ones before it", on_disk(PG + 8) == 'K');
    munmap(m, 4 * PG);
    close(fd);

    /* Many pages at once. */
    unlink(path);
    fd = open(path, O_RDWR | O_CREAT, 0644);
    ftruncate(fd, MIB);
    m = mmap(NULL, MIB, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    for (long off = 0; off < MIB; off += PG) m[off] = (char)('0' + off / PG % 10);
    msync(m, MIB, MS_SYNC);
    int all_there = 1;
    for (long off = 0; off < MIB; off += 64 * PG) all_there &= on_disk(off) == '0' + off / PG % 10;
    check("1 MiB of stores through a mapping reaches the disk", all_there);
    munmap(m, MIB);
    close(fd);
    unlink(path);

    printf("%s\n", failures ? "writebacktest: FAILED" : "writebacktest: all passed");
    return failures != 0;
}
