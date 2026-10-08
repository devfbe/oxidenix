/* /data in the Linux server (phase R6c.3): its calls never pass through to
 * the kernel, descriptors and mappings of a file share one page cache,
 * write() leaves dirty pages that fsync makes durable (an O_DIRECT read
 * fetches the device's copy), children's stores survive write-back around
 * fork, truncation reaches mappings, many readers
 * and writers at once see consistent data, a file larger than the memory
 * the cache may use is written and read back whole, and a read finds room
 * when dirty pages fill memory. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/wait.h>
#include <unistd.h>

#define PG 4096
#define MIB (1024 * 1024L)

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
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

static long meminfo(const char *key) {
    char text[4096] = {0};
    int fd = open("/proc/meminfo", O_RDONLY);
    read(fd, text, sizeof text - 1);
    close(fd);
    char *p = strstr(text, key);
    return p ? strtol(p + strlen(key) + 1, NULL, 10) : -1;
}

static char *direct_buf;

/* `len` bytes at `off` of `path` as the device has them (O_DIRECT). */
static ssize_t on_disk(const char *path, off_t off, char *out, size_t len) {
    int fd = open(path, O_RDONLY | O_DIRECT);
    if (fd < 0) return -1;
    ssize_t n = pread(fd, direct_buf, len, off);
    close(fd);
    if (n > 0) memcpy(out, direct_buf, n);
    return n;
}

static unsigned char pattern(long off, int salt) {
    return (unsigned char)((off / PG * 13 + off + salt) % 251);
}

static sigjmp_buf env;
static volatile sig_atomic_t got;
static void on_fault(int sig) {
    got = sig;
    siglongjmp(env, 1);
}

static int faults(volatile char *p) {
    got = 0;
    if (!sigsetjmp(env, 1)) (void)*p;
    return got;
}

/* ------------------------------------------------------------------ */

static void no_pass_through(void) {
    const char *path = "/data/datatest.calls";
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    char block[PG], back[16];
    memset(block, 'd', sizeof block);
    struct stat st;
    long idle = legacy_calls();
    long base = legacy_calls() - idle;
    long l0 = legacy_calls();
    int io = 1;
    for (int i = 0; i < 25; i++) {
        io &= pwrite(fd, block, sizeof block, (off_t)i * PG) == PG;
        io &= pread(fd, back, 8, (off_t)i * PG + 100) == 8 && back[0] == 'd';
        io &= lseek(fd, 0, SEEK_END) == (off_t)(i + 1) * PG;
        io &= fstat(fd, &st) == 0 && stat(path, &st) == 0;
    }
    io &= fsync(fd) == 0 && ftruncate(fd, 10) == 0;
    long passed = legacy_calls() - l0 - base;
    printf("    (%ld of 127 /data calls passed through)\n", passed);
    check("reads, writes, lseek, stat, fsync of /data are the server's", passed == 0 && io);
    struct statfs fs;
    check("/data is ext2 with its own device", fstatfs(fd, &fs) == 0 && fs.f_type == 0xef53 && fstat(fd, &st) == 0 && st.st_dev != 0 && st.st_dev != 0x1a);
    close(fd);
    unlink(path);
}

static void sharing(void) {
    const char *path = "/data/datatest.share";
    int a = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    int b = open(path, O_RDWR);
    char buf[3 * PG];
    for (int i = 0; i < 3 * PG; i++) buf[i] = (char)pattern(i, 1);
    write(a, buf, sizeof buf);
    char *m = mmap(NULL, 3 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, b, 0);
    check("two descriptors and a mapping of one /data file", m != MAP_FAILED);
    if (m == MAP_FAILED) return;
    char c = 0;
    check("a write through one descriptor is in the mapping at once", (unsigned char)m[PG + 7] == pattern(PG + 7, 1));
    m[2 * PG + 1] = 'S';
    check("a store through the mapping is read through the other at once", pread(a, &c, 1, 2 * PG + 1) == 1 && c == 'S');
    pwrite(b, "W", 1, 5);
    check("... and a write through the other in the mapping", m[5] == 'W');
    pid_t kid = fork();
    if (kid == 0) {
        int fd = open(path, O_RDWR);
        char *k = mmap(NULL, PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (k == MAP_FAILED || k[5] != 'W') _exit(1);
        k[9] = 'K';
        _exit(0);
    }
    int status;
    waitpid(kid, &status, 0);
    check("another process's mapping shares the pages", WIFEXITED(status) && WEXITSTATUS(status) == 0 && m[9] == 'K' && pread(a, &c, 1, 9) == 1 && c == 'K');
    munmap(m, 3 * PG);
    close(a);
    close(b);
    unlink(path);
}

static void durability(void) {
    const char *path = "/data/datatest.durable";
    long dirty0 = meminfo("Dirty:");
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    char buf[16 * PG], back[2 * PG];
    for (int i = 0; i < (int)sizeof buf; i++) buf[i] = (char)pattern(i, 2);
    check("write 64 KiB", write(fd, buf, sizeof buf) == (ssize_t)sizeof buf);
    long dirty = meminfo("Dirty:");
    printf("    (Dirty: %ld kB before, %ld kB after the write)\n", dirty0, dirty);
    check("write() leaves dirty pages (write-back)", dirty >= dirty0 + 64);
    check("fsync writes them back", fsync(fd) == 0 && meminfo("Dirty:") <= dirty0);
    check("... and the device has the data (O_DIRECT)", on_disk(path, 5 * PG, back, 2 * PG) == 2 * PG && memcmp(back, buf + 5 * PG, 2 * PG) == 0);
    /* O_SYNC: durable when write() returns. */
    int sfd = open(path, O_WRONLY | O_SYNC);
    long d1 = meminfo("Dirty:");
    check("an O_SYNC write is clean when it returns", pwrite(sfd, "synced", 6, 3 * PG) == 6 && meminfo("Dirty:") <= d1);
    check("... and on the device", on_disk(path, 3 * PG, back, PG) == PG && memcmp(back, "synced", 6) == 0);
    close(sfd);
    /* Unflushed data is still read from the cache, then from the disk. */
    pwrite(fd, "late", 4, 20 * PG);
    struct stat st;
    check("a write past the end grows the file in the cache", fstat(fd, &st) == 0 && st.st_size == 20 * PG + 4 && pread(fd, back, 4, 20 * PG) == 4 && memcmp(back, "late", 4) == 0);
    check("... and the hole reads zero", pread(fd, back, 8, 18 * PG) == 8 && memcmp(back, "\0\0\0\0\0\0\0\0", 8) == 0);
    check("sync writes everything back", (sync(), meminfo("Dirty:") <= dirty0) && on_disk(path, 20 * PG, back, PG) == 4 && memcmp(back, "late", 4) == 0);
    close(fd);
    unlink(path);
}

static void truncation(void) {
    const char *path = "/data/datatest.trunc";
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    char buf[4 * PG];
    memset(buf, 'T', sizeof buf);
    write(fd, buf, sizeof buf);
    char *m = mmap(NULL, 4 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (m == MAP_FAILED) {
        check("mapping for truncation", 0);
        return;
    }
    m[3 * PG] = 'D';
    check("truncation with dirty pages mapped", ftruncate(fd, PG + 10) == 0);
    check("... the pages beyond raise SIGBUS", faults(m + 3 * PG) == SIGBUS && faults(m + 2 * PG) == SIGBUS);
    check("... the cut page reads zero beyond the end", m[PG + 9] == 'T' && m[PG + 10] == 0);
    char back[PG];
    check("... and the device has the new size after fsync", fsync(fd) == 0 && on_disk(path, 0, back, PG) == PG && on_disk(path, PG, back, PG) == 10);
    munmap(m, 4 * PG);
    close(fd);
    unlink(path);
}

/* A child's store into a shared mapping it inherited reaches the disk,
 * whatever write-back ran around the fork (a thread keeps writing the
 * file back meanwhile): the child's copy of a page whose stores must mark
 * it dirty is read-only until its first store. */
static volatile int syncing;
static int sync_fd;

static void *syncer(void *arg) {
    (void)arg;
    while (syncing) fdatasync(sync_fd);
    return NULL;
}

static void fork_and_writeback(void) {
    const char *path = "/data/datatest.fork";
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    char page[PG];
    memset(page, '.', sizeof page);
    write(fd, page, sizeof page);
    fsync(fd);
    char *m = mmap(NULL, PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (m == MAP_FAILED) {
        check("fork and write-back: mapping", 0);
        return;
    }
    sync_fd = fd;
    syncing = 1;
    pthread_t t;
    pthread_create(&t, NULL, syncer, NULL);
    int good = 1;
    for (int i = 0; i < 100 && good; i++) {
        m[0] = (char)('a' + i % 26);
        pid_t kid = fork();
        if (kid == 0) {
            m[100 + i] = (char)('A' + i % 26);
            _exit(0);
        }
        int status;
        waitpid(kid, &status, 0);
        good &= WIFEXITED(status) && WEXITSTATUS(status) == 0;
    }
    syncing = 0;
    pthread_join(t, NULL);
    good &= fsync(fd) == 0;
    char back[PG];
    good &= on_disk(path, 0, back, PG) == PG;
    for (int i = 0; good && i < 100; i++) good &= back[100 + i] == (char)('A' + i % 26);
    check("children's stores reach the disk with write-back around fork", good);
    munmap(m, PG);
    close(fd);
    unlink(path);
}

/* Readers at once, each over the whole file at its own offsets. */
#define READERS 8
#define SHARED_SIZE (4 * MIB)

static void readers(void) {
    const char *path = "/data/datatest.readers";
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    static char chunk[64 * 1024];
    for (long off = 0; off < SHARED_SIZE; off += sizeof chunk) {
        for (size_t i = 0; i < sizeof chunk; i++) chunk[i] = (char)pattern(off + i, 3);
        write(fd, chunk, sizeof chunk);
    }
    fsync(fd);
    close(fd);
    /* Drop the cached pages: commit what memory allows, then let go. */
    long room = (meminfo("CommitLimit:") - meminfo("Committed_AS:")) * 1024 - 4 * MIB;
    char *all = room > 0 ? mmap(NULL, room, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) : MAP_FAILED;
    if (all != MAP_FAILED) munmap(all, room);
    pid_t kids[READERS];
    for (int r = 0; r < READERS; r++) {
        kids[r] = fork();
        if (kids[r] == 0) {
            int f = open(path, O_RDONLY);
            char buf[3 * PG + 17];
            unsigned seed = r + 1;
            for (int i = 0; i < 400; i++) {
                long off = rand_r(&seed) % (SHARED_SIZE - sizeof buf);
                if (pread(f, buf, sizeof buf, off) != (ssize_t)sizeof buf) _exit(2);
                for (size_t j = 0; j < sizeof buf; j += 97)
                    if ((unsigned char)buf[j] != pattern(off + j, 3)) _exit(3);
            }
            _exit(0);
        }
    }
    int good = 1;
    for (int r = 0; r < READERS; r++) {
        int status;
        waitpid(kids[r], &status, 0);
        good &= WIFEXITED(status) && WEXITSTATUS(status) == 0;
    }
    check("8 processes reading one file at random offsets at once", good);
    unlink(path);
}

/* Threads writing their own parts of one file at once. */
#define WRITERS 4
#define PART (512 * 1024L)
static const char *wpath = "/data/datatest.writers";

static void *writer(void *arg) {
    long w = (long)arg;
    int fd = open(wpath, O_WRONLY);
    static __thread char buf[16 * 1024];
    for (long off = 0; off < PART; off += sizeof buf) {
        for (size_t i = 0; i < sizeof buf; i++) buf[i] = (char)pattern(w * PART + off + i, 4);
        if (pwrite(fd, buf, sizeof buf, w * PART + off) != (ssize_t)sizeof buf) return (void *)1;
    }
    close(fd);
    return NULL;
}

static void writers(void) {
    close(open(wpath, O_RDWR | O_CREAT | O_TRUNC, 0644));
    pthread_t t[WRITERS];
    for (long w = 0; w < WRITERS; w++) pthread_create(&t[w], NULL, writer, (void *)w);
    int good = 1;
    for (int w = 0; w < WRITERS; w++) {
        void *r;
        pthread_join(t[w], &r);
        good &= r == NULL;
    }
    int fd = open(wpath, O_RDONLY);
    good &= fsync(fd) == 0;
    static char back[64 * 1024];
    for (long off = 0; good && off < WRITERS * PART; off += 128 * 1024) {
        good &= on_disk(wpath, off, back, sizeof back) == (ssize_t)sizeof back;
        for (size_t i = 0; good && i < sizeof back; i += 61) good &= (unsigned char)back[i] == pattern(off + i, 4);
    }
    close(fd);
    check("4 threads writing parts of one file, on the device after fsync", good);
    unlink(wpath);
}

/* A file larger than the memory left for the cache: committing all but
 * ROOM bytes leaves the cache that much; the file is written (dirty pages
 * must go to the disk to make room) and read back whole. */
#define ROOM (12 * MIB)
#define LARGE (32 * MIB)

static void larger_than_cache(void) {
    const char *path = "/data/datatest.large";
    long room = (meminfo("CommitLimit:") - meminfo("Committed_AS:")) * 1024 - ROOM;
    char *all = mmap(NULL, room, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check("committing all but 12 MiB of memory", all != MAP_FAILED);
    if (all == MAP_FAILED) return;
    for (long off = 0; off < room; off += PG) all[off] = 1;
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    static char chunk[64 * 1024];
    int good = fd >= 0;
    for (long off = 0; good && off < LARGE; off += sizeof chunk) {
        for (size_t i = 0; i < sizeof chunk; i++) chunk[i] = (char)pattern(off + i, 5);
        good &= write(fd, chunk, sizeof chunk) == (ssize_t)sizeof chunk;
    }
    good &= fsync(fd) == 0;
    check("writing a 32 MiB file with 12 MiB to cache it", good);
    good = lseek(fd, 0, SEEK_SET) == 0;
    for (long off = 0; good && off < LARGE; off += sizeof chunk) {
        ssize_t n = read(fd, chunk, sizeof chunk);
        good &= n == (ssize_t)sizeof chunk;
        for (size_t i = 0; good && i < sizeof chunk; i += 89) good &= (unsigned char)chunk[i] == pattern(off + i, 5);
        if (!good) {
            printf("    (at %ld: read %zd, errno %d)\n", off, n, errno);
            for (size_t i = 0; i < sizeof chunk; i++)
                if ((unsigned char)chunk[i] != pattern(off + i, 5)) {
                    char d[PG];
                    ssize_t m = on_disk(path, (off + i) & ~(long)(PG - 1), d, PG);
                    printf("    (first bad byte at %ld: %d, want %d; on disk %d (read %zd))\n", off + (long)i, (unsigned char)chunk[i], pattern(off + i, 5), (unsigned char)d[(off + i) % PG], m);
                    break;
                }
        }
    }
    check("... and reading it back whole", good);
    printf("    (Cached: %ld kB, Dirty: %ld kB)\n", meminfo("Cached:"), meminfo("Dirty:"));
    munmap(all, room);
    close(fd);
    unlink(path);
}

/* A read that needs memory the cache's dirty pages hold: they are written
 * back to make room, the read does not fail. */
static void read_with_dirty_memory(void) {
    const char *a = "/data/datatest.dirty", *b = "/data/datatest.clean";
    static char chunk[64 * 1024];
    int fb = open(b, O_RDWR | O_CREAT | O_TRUNC, 0644);
    for (long off = 0; off < 4 * MIB; off += sizeof chunk) {
        for (size_t i = 0; i < sizeof chunk; i++) chunk[i] = (char)pattern(off + i, 6);
        write(fb, chunk, sizeof chunk);
    }
    fsync(fb);
    close(fb);
    long room = (meminfo("CommitLimit:") - meminfo("Committed_AS:")) * 1024 - 3 * MIB;
    char *all = mmap(NULL, room, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (all == MAP_FAILED) {
        check("a read with memory full of dirty pages (commit)", 0);
        return;
    }
    for (long off = 0; off < room; off += PG) all[off] = 1;
    /* Most of the rest dirty through a mapping, then unmapped: dirty pages
     * nothing maps, which reclaim cannot drop until they are written. */
    int fa = open(a, O_RDWR | O_CREAT | O_TRUNC, 0644);
    ftruncate(fa, 5 * MIB / 2);
    char *m = mmap(NULL, 5 * MIB / 2, PROT_READ | PROT_WRITE, MAP_SHARED, fa, 0);
    if (m != MAP_FAILED) {
        for (long off = 0; off < 5 * MIB / 2; off += PG) m[off] = 'd';
        munmap(m, 5 * MIB / 2);
    }
    fb = open(b, O_RDONLY);
    int good = fb >= 0;
    for (long off = 0; good && off < 4 * MIB; off += sizeof chunk) {
        ssize_t n = read(fb, chunk, sizeof chunk);
        good &= n == (ssize_t)sizeof chunk;
        for (size_t i = 0; good && i < sizeof chunk; i += 101) good &= (unsigned char)chunk[i] == pattern(off + i, 6);
        if (!good) printf("    (at %ld: read %zd, errno %d)\n", off, n, errno);
    }
    check("a read with memory full of dirty pages", good);
    munmap(all, room);
    close(fa);
    close(fb);
    unlink(a);
    unlink(b);
}

int main(void) {
    struct sigaction sa = {0};
    sa.sa_handler = on_fault;
    sigaction(SIGBUS, &sa, NULL);
    sigaction(SIGSEGV, &sa, NULL);
    if (posix_memalign((void **)&direct_buf, PG, 128 * 1024) != 0) return 1;
    no_pass_through();
    sharing();
    durability();
    truncation();
    fork_and_writeback();
    readers();
    writers();
    larger_than_cache();
    read_with_dirty_memory();
    printf("%s\n", failures ? "datatest: FAILED" : "datatest: all passed");
    return failures != 0;
}
