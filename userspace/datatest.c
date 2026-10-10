/* /data in the Linux server (phase R6c.3): its calls are the server's,
 * descriptors and mappings of a file share one page cache,
 * write() leaves dirty pages that fsync makes durable (an O_DIRECT read
 * fetches the device's copy), children's stores survive write-back around
 * fork and mprotect, truncation reaches mappings (a fork child's copy
 * too, also when the two race), many readers
 * and writers at once see consistent data, a file larger than the memory
 * the cache may use is written and read back whole, a read finds room
 * when dirty pages fill memory, and a full disk fails write() itself with
 * ENOSPC (and a store into a hole with SIGBUS, a kernel copy into one with
 * EFAULT), never the write-back. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define PG 4096
#define MIB (1024 * 1024L)

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
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

static int store_faults(volatile char *p) {
    got = 0;
    if (!sigsetjmp(env, 1)) *p = 1;
    return got;
}

/* ------------------------------------------------------------------ */

static void server_io(void) {
    const char *path = "/data/datatest.calls";
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    char block[PG], back[16];
    memset(block, 'd', sizeof block);
    struct stat st;
    int io = 1;
    for (int i = 0; i < 25; i++) {
        io &= pwrite(fd, block, sizeof block, (off_t)i * PG) == PG;
        io &= pread(fd, back, 8, (off_t)i * PG + 100) == 8 && back[0] == 'd';
        io &= lseek(fd, 0, SEEK_END) == (off_t)(i + 1) * PG;
        io &= fstat(fd, &st) == 0 && stat(path, &st) == 0;
    }
    io &= fsync(fd) == 0 && ftruncate(fd, 10) == 0;
    check("reads, writes, lseek, stat, fsync of /data are the server's", io);
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
 * file back meanwhile): the child is among the file's mappers before it
 * gets copies of the parent's entries, so write-back write-protects the
 * child's copy of a page as it does the parent's. */
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

/* A fork racing a truncation of a file the parent maps shared: the child's
 * copy of the mapping is reached by the truncation like the parent's, so
 * once ftruncate returned the child's accesses beyond the new end raise
 * SIGBUS, never show the cut pages' old data. A thread truncates while the
 * main thread forks, at a delay that sweeps across the fork. */
#define FT_PAGES 8
#define FT_ROUNDS 400
static volatile int *ft_flags; /* [0]: go, [1]: truncated (shared with children) */
static int ft_fd;
static volatile int ft_delay, ft_stop;

static void *ft_truncator(void *arg) {
    (void)arg;
    for (;;) {
        while (!__atomic_load_n(&ft_flags[0], __ATOMIC_ACQUIRE))
            if (ft_stop) return NULL;
        for (volatile int i = 0; i < ft_delay; i++) {
        }
        ftruncate(ft_fd, PG);
        __atomic_store_n(&ft_flags[0], 0, __ATOMIC_RELAXED);
        __atomic_store_n(&ft_flags[1], 1, __ATOMIC_RELEASE);
    }
}

static void fork_and_truncate(void) {
    const char *path = "/data/datatest.forktrunc";
    ft_fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    ft_flags = mmap(NULL, PG, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    char *m = mmap(NULL, FT_PAGES * PG, PROT_READ | PROT_WRITE, MAP_SHARED, ft_fd, 0);
    if (ft_fd < 0 || m == MAP_FAILED || ft_flags == MAP_FAILED) {
        check("fork and truncation: mapping", 0);
        return;
    }
    pthread_t t;
    ft_stop = 0;
    pthread_create(&t, NULL, ft_truncator, NULL);
    int good = 1, stale = 0, forks = 0;
    for (int round = 0; round < FT_ROUNDS && good; round++) {
        good &= ftruncate(ft_fd, FT_PAGES * PG) == 0;
        for (int p = 1; p < FT_PAGES; p++) m[p * PG] = 'X';
        ft_flags[1] = 0;
        ft_delay = (round * 97) % 20000;
        __atomic_store_n(&ft_flags[0], 1, __ATOMIC_RELEASE);
        pid_t kid = fork();
        if (kid == 0) {
            while (!__atomic_load_n(&ft_flags[1], __ATOMIC_ACQUIRE)) {
            }
            int old = 0;
            for (int p = 1; p < FT_PAGES; p++)
                if (faults(m + p * PG) != SIGBUS) old++;
            _exit(old ? 1 : 0);
        }
        if (kid < 0) {
            good = 0;
            break;
        }
        forks++;
        int status;
        waitpid(kid, &status, 0);
        good &= WIFEXITED(status);
        if (WIFEXITED(status) && WEXITSTATUS(status) != 0) stale++;
        while (!__atomic_load_n(&ft_flags[1], __ATOMIC_ACQUIRE)) {
        }
    }
    ft_stop = 1;
    pthread_join(t, NULL);
    printf("    (%d forks racing a truncation, %d children saw cut pages)\n", forks, stale);
    check("a fork racing a truncation: the child's cut pages raise SIGBUS", good && stale == 0);
    munmap(m, FT_PAGES * PG);
    munmap((void *)ft_flags, PG);
    close(ft_fd);
    unlink(path);
}

/* Rights raised again by mprotect never let a store skip marking its page
 * dirty: after PROT_NONE (with a faulting access meanwhile), or PROT_READ
 * across a write-back, and back to PROT_READ|PROT_WRITE, the first store
 * faults and dirties the page, so fsync writes it back. */
static int disk_has(const char *path, off_t off, char c) {
    char back[PG];
    off_t page = off / PG * PG;
    return on_disk(path, page, back, PG) == PG && back[off - page] == c;
}

static void reprotect(void) {
    const char *path = "/data/datatest.mprotect";
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    char page[2 * PG];
    memset(page, '.', sizeof page);
    write(fd, page, sizeof page);
    fsync(fd);
    char *m = mmap(NULL, 2 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (m == MAP_FAILED) {
        check("mprotect and write-back: mapping", 0);
        return;
    }
    /* A clean page (written back, write-protected) behind PROT_NONE. */
    m[0] = 'a';
    m[PG] = 'b';
    fsync(fd);
    int ok = mprotect(m, 2 * PG, PROT_NONE) == 0;
    check("PROT_NONE over a clean shared /data page: accesses fault", ok && faults(m) == SIGSEGV && store_faults(m + PG) == SIGSEGV);
    ok = mprotect(m, 2 * PG, PROT_READ | PROT_WRITE) == 0;
    long d0 = meminfo("Dirty:");
    m[10] = 'N';
    long d1 = meminfo("Dirty:");
    check("... back to read-write: the first store dirties the page", ok && m[0] == 'a' && d1 >= d0 + 4);
    check("... and fsync writes it back", fsync(fd) == 0 && disk_has(path, 10, 'N') && disk_has(path, 0, 'a'));
    /* A dirty page written back while it is PROT_NONE. */
    m[20] = 'x';
    ok = mprotect(m, 2 * PG, PROT_NONE) == 0 && fsync(fd) == 0 && disk_has(path, 20, 'x');
    ok &= mprotect(m, 2 * PG, PROT_READ | PROT_WRITE) == 0;
    m[30] = 'y';
    check("a page written back under PROT_NONE is dirtied by the next store", ok && fsync(fd) == 0 && disk_has(path, 30, 'y'));
    /* A dirty page made read-only, written back, then writable again. */
    m[PG + 1] = 'r';
    ok = mprotect(m + PG, PG, PROT_READ) == 0 && fsync(fd) == 0 && disk_has(path, PG + 1, 'r');
    ok &= m[PG + 1] == 'r' && store_faults(m + PG + 2) == SIGSEGV;
    ok &= mprotect(m + PG, PG, PROT_READ | PROT_WRITE) == 0;
    m[PG + 3] = 'w';
    check("a page written back under PROT_READ is dirtied by the next store", ok && fsync(fd) == 0 && disk_has(path, PG + 3, 'w'));
    munmap(m, 2 * PG);
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

/* Writes `step`-byte chunks of the pattern at the end of `fd` until write()
 * fails; its errno. */
static int fill_with(int fd, long *total, size_t step) {
    static char chunk[64 * 1024];
    for (int i = 0; i < 100000; i++) {
        for (size_t j = 0; j < step; j++) chunk[j] = pattern(*total + j, 3);
        ssize_t n = write(fd, chunk, step);
        if (n < 0) return errno;
        *total += n;
    }
    return 0;
}

/* The disk filled up: write() fails with ENOSPC as soon as the space its
 * data needs is gone (promised when the data enters the cache, as Linux's
 * delayed allocation reserves it), everything it accepted reaches the disk,
 * and a store through a shared mapping into a hole raises SIGBUS. Room
 * again once the file is gone. */
static void full_disk(void) {
    const char *path = "/data/datatest.full", *hole = "/data/datatest.hole";
    unlink(path);
    unlink(hole);
    struct statfs before;
    statfs("/data", &before);
    int hfd = open(hole, O_RDWR | O_CREAT | O_TRUNC, 0644);
    int sized = ftruncate(hfd, 64 * PG) == 0;
    volatile char *map = mmap(NULL, 64 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, hfd, 0);
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    long total = 0;
    int big = fill_with(fd, &total, 64 * 1024);
    int small = fill_with(fd, &total, 1024);
    long limit = (long)before.f_blocks * before.f_bsize;
    printf("    (%ld bytes until the disk was full)\n", total);
    check("full disk: write() itself fails with ENOSPC", big == ENOSPC && small == ENOSPC && total > 0 && total < limit);
    struct statfs full;
    statfs("/data", &full);
    check("full disk: statfs counts the promised space as used", full.f_bfree * full.f_bsize < 64 * 1024);
    check("full disk: what write() accepted is written back (fsync)", fsync(fd) == 0);
    struct stat st;
    fstat(fd, &st);
    char back[PG];
    long last = (total / PG - 1) * PG;
    int same = last >= 0 && on_disk(path, last, back, PG) == PG;
    for (int i = 0; same && i < PG; i++) same = (unsigned char)back[i] == pattern(last + i, 3);
    check("full disk: the file's size and its last page on the disk", st.st_size == total && same);
    check("full disk: a store into a hole raises SIGBUS", sized && map != MAP_FAILED && store_faults(map + 10 * PG) == SIGBUS);
    /* Copies into such a page fail too, and leave it clean: read() from a
     * pipe (EFAULT or a short count, nothing read), and a fork child's
     * CLONE_CHILD_SETTID word (the child writes it before its first
     * instruction; a failure is ignored, as Linux's schedule_tail does). */
    int pipefd[2] = {-1, -1};
    int piped = pipe(pipefd) == 0 && write(pipefd[1], "pipedata", 8) == 8;
    long dirty0 = meminfo("Dirty:");
    errno = 0;
    ssize_t got_n = map != MAP_FAILED ? read(pipefd[0], (char *)map + 20 * PG, 8) : 0;
    check("full disk: read() from a pipe into a hole fails (EFAULT)", piped && got_n <= 0 && (got_n == 0 || errno == EFAULT));
    errno = 0;
    long kid = map != MAP_FAILED ? syscall(SYS_clone, CLONE_CHILD_SETTID | SIGCHLD, 0, NULL, (void *)(map + 30 * PG), 0) : 0;
    if (kid == 0) _exit(0);
    if (kid > 0) waitpid((pid_t)kid, NULL, 0);
    check("full disk: clone's CHILD_SETTID into a hole is dropped, the child runs", kid > 0);
    check("... and neither left a dirty page", meminfo("Dirty:") <= dirty0);
    close(fd);
    unlink(path);
    // The space comes back once diskfs freed the file.
    struct statfs after = {0};
    for (int i = 0; i < 100; i++) {
        statfs("/data", &after);
        if (after.f_bfree * after.f_bsize > 1024 * 1024) break;
        usleep(20000);
    }
    check("full disk: room again once the file is gone", after.f_bfree * after.f_bsize > 1024 * 1024);
    check("full disk: then the store goes through", map != MAP_FAILED && store_faults(map + 10 * PG) == 0 && msync((void *)map, 64 * PG, MS_SYNC) == 0);
    int again = map != MAP_FAILED && piped && write(pipefd[1], "pipedata", 8) == 8;
    again &= read(pipefd[0], (char *)map + 20 * PG, 8) == 8 && memcmp((char *)map + 20 * PG, "pipedata", 8) == 0;
    kid = again ? syscall(SYS_clone, CLONE_CHILD_SETTID | SIGCHLD, 0, NULL, (void *)(map + 30 * PG), 0) : -1;
    if (kid == 0) _exit(0);
    if (kid > 0) waitpid((pid_t)kid, NULL, 0);
    int tid_seen = kid > 0 && *(volatile int *)(map + 30 * PG) == (int)kid;
    int durable = again && fsync(hfd) == 0 && on_disk(hole, 20 * PG, back, PG) == PG && memcmp(back, "pipedata", 8) == 0;
    int tid_durable = tid_seen && on_disk(hole, 30 * PG, back, PG) == PG && memcmp(back, &(int){(int)kid}, sizeof(int)) == 0;
    check("full disk: then read() from a pipe fills the page, on the disk after fsync", durable);
    check("full disk: then a child's CHILD_SETTID word reaches the file and the disk", tid_durable);
    close(pipefd[0]);
    close(pipefd[1]);
    if (map != MAP_FAILED) munmap((void *)map, 64 * PG);
    close(hfd);
    unlink(hole);
}

/* An open file unlinked is on ext2's orphan list until its last user lets go. A diskfs that
 * dies meanwhile (killed here: TEST_KILL_SERVER) leaves it there, and the diskfs that the
 * kernel starts next keeps it for the clients of the dead one: the Linux server connects
 * again and names the file (its handle) before anything else, so the still-open
 * descriptor reads its data back from the device (O_DIRECT) and writes on; once it is
 * closed, the file's blocks come back (e2fsck then finds the filesystem clean). */
static void orphan_survives_diskfs_restart(void) {
    const char *path = "/data/datatest.orphan";
    enum { CHUNK = 64 * 1024, CHUNKS = 64 };
    unlink(path);
    sync();
    struct statfs before, held, restarted, after;
    statfs("/data", &before);
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    int dfd = open(path, O_RDONLY | O_DIRECT);
    static char block[CHUNK];
    for (int i = 0; i < CHUNKS; i++) {
        for (int j = 0; j < CHUNK; j++) block[j] = pattern((long)i * CHUNK + j, 5);
        write(fd, block, CHUNK);
    }
    fsync(fd);
    unlink(path);
    sync();
    statfs("/data", &held);
    long killed = syscall(1522, "diskfs");
    /* The Linux server connects to the next diskfs at once (the kernel tells it the old
     * one died), without a use of /data: a while later there is a diskfs to kill again
     * (the file must outlive that restart too). */
    usleep(500 * 1000);
    long again = syscall(1522, "diskfs");
    int up = 0;
    for (int i = 0; i < 100 && !up; i++) {
        up = statfs("/data", &restarted) == 0;
        if (!up) usleep(50 * 1000);
    }
    /* Still allocated: its blocks did not come back. */
    int kept = up && restarted.f_bfree + 4000 <= before.f_bfree;
    /* Its data as the device has it, through the descriptor opened before. */
    int intact = 1;
    for (int i = 0; i < CHUNKS && intact; i++) {
        if (pread(dfd, direct_buf, CHUNK, (off_t)i * CHUNK) != CHUNK) intact = 0;
        for (int j = 0; j < CHUNK && intact; j++)
            if ((unsigned char)direct_buf[j] != pattern((long)i * CHUNK + j, 5)) intact = 0;
    }
    /* And it takes writes: through the cache, durable after fsync. */
    for (int j = 0; j < CHUNK; j++) block[j] = pattern((long)CHUNKS * CHUNK + j, 9);
    int wrote = pwrite(fd, block, CHUNK, (off_t)CHUNKS * CHUNK) == CHUNK && fsync(fd) == 0;
    wrote = wrote && pread(dfd, direct_buf, CHUNK, (off_t)CHUNKS * CHUNK) == CHUNK && memcmp(direct_buf, block, CHUNK) == 0;
    struct stat st;
    wrote = wrote && fstat(fd, &st) == 0 && st.st_size == (off_t)(CHUNKS + 1) * CHUNK && st.st_nlink == 0;
    close(dfd);
    close(fd);
    /* Closed: freed (once no client of the dead diskfs is left to name what it holds). */
    int freed = 0;
    for (int i = 0; i < 100 && !freed; i++) {
        freed = statfs("/data", &after) == 0 && after.f_bfree >= before.f_bfree;
        if (!freed) usleep(50 * 1000);
    }
    printf("    (free blocks: %ld before, %ld with the open unlinked file, %ld after diskfs restarted, %ld after the close)\n",
           (long)before.f_bfree, (long)held.f_bfree, (long)restarted.f_bfree, (long)after.f_bfree);
    check("diskfs is started again without a use of /data (EVENT_SERVICE_GONE)", killed == 0 && again == 0);
    check("an open unlinked file outlives two diskfs restarts: still allocated", held.f_bfree + 4000 <= before.f_bfree && kept);
    check("... the open descriptor reads its data back from the device", intact);
    check("... and writes on (durable, still unlinked)", wrote);
    check("... and its blocks come back after the last close", freed);
}

int main(void) {
    struct sigaction sa = {0};
    sa.sa_handler = on_fault;
    sigaction(SIGBUS, &sa, NULL);
    sigaction(SIGSEGV, &sa, NULL);
    if (posix_memalign((void **)&direct_buf, PG, 128 * 1024) != 0) return 1;
    server_io();
    sharing();
    durability();
    truncation();
    fork_and_writeback();
    fork_and_truncate();
    reprotect();
    readers();
    writers();
    larger_than_cache();
    read_with_dirty_memory();
    full_disk();
    orphan_survives_diskfs_restart();
    printf("%s\n", failures ? "datatest: FAILED" : "datatest: all passed");
    return failures != 0;
}
