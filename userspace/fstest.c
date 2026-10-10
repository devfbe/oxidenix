/* Filesystem semantics on /data: symlinks and O_NOFOLLOW, unlinked files that stay open,
 * file size limits, the access modes of descriptors, and preadv2/pwritev2's flags. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#include "rwtest.h"

static int failures;

static void check(const char *name, int ok) {
    printf("%-56s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

/* Directories made in /tmp until the tree's tmpfs bound says ENOSPC (all removed again):
 * how many inodes are left. */
static int tmpfs_room(void) {
    char p[64];
    int n = 0;
    mkdir("/tmp/budget", 0755);
    for (;;) {
        snprintf(p, sizeof p, "/tmp/budget/d%d", n);
        if (mkdir(p, 0755) != 0) break;
        n++;
    }
    int full = errno == ENOSPC;
    for (int i = 0; i < n; i++) {
        snprintf(p, sizeof p, "/tmp/budget/d%d", i);
        rmdir(p);
    }
    rmdir("/tmp/budget");
    return full ? n : -1;
}

/* The tmpfs bounds give back exactly what an inode charged, once, when the inode goes: after
 * files made, renamed over (also while the replaced one is open), unlinked while open,
 * symlinks with long targets and failed creations, the room is what it was. */
static void tmpfs_budget(void) {
    int before = tmpfs_room();
    static char target[4000];
    memset(target, 't', sizeof target - 1);
    for (int i = 0; i < 300; i++) {
        int a = open("/tmp/budget-a", O_RDWR | O_CREAT | O_TRUNC, 0600);
        int b = open("/tmp/budget-b", O_RDWR | O_CREAT | O_TRUNC, 0600);
        write(a, "a", 1);
        /* b replaces a, which stays open. */
        rename("/tmp/budget-b", "/tmp/budget-a");
        /* Unlinked while open. */
        unlink("/tmp/budget-a");
        symlink(target, "/tmp/budget-l");
        /* A failed creation (the name is taken). */
        symlink(target, "/tmp/budget-l");
        mkdir("/tmp/budget-l", 0755);
        unlink("/tmp/budget-l");
        close(a);
        close(b);
    }
    int after = tmpfs_room();
    printf("    (tmpfs room: %d inodes before, %d after)\n", before, after);
    check("tmpfs bounds: ENOSPC, and every inode's charge given back once", before > 0 && after == before);
}

/* getdents64 with a buffer far larger than the directory (as a program may claim): each
 * call returns at most a bounded piece, and the calls together list every entry once; ".."
 * is the parent's inode. */
static void getdents_pieces(const char *parent, const char *dir, int files) {
    char p[96];
    mkdir(dir, 0755);
    for (int i = 0; i < files; i++) {
        snprintf(p, sizeof p, "%s/entry-with-a-longer-name-%05d", dir, i);
        close(open(p, O_WRONLY | O_CREAT, 0600));
    }
    int d = open(dir, O_RDONLY | O_DIRECTORY);
    size_t size = 1 << 20;
    char *buf = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    int seen = 0, calls = 0;
    unsigned long long dotdot = 0;
    long most = 0, n;
    while ((n = syscall(SYS_getdents64, d, buf, 0x7fffffffL)) > 0) {
        calls++;
        if (n > most) most = n;
        for (long at = 0; at < n;) {
            unsigned short reclen = *(unsigned short *)(buf + at + 16);
            if (strncmp(buf + at + 19, "entry-", 6) == 0) seen++;
            if (strcmp(buf + at + 19, "..") == 0) dotdot = *(unsigned long long *)(buf + at);
            at += reclen;
        }
    }
    printf("    (%s: %d entries in %d calls, at most %ld bytes a call)\n", dir, seen, calls, most);
    check("getdents64: a huge buffer gets bounded pieces, every entry once", n == 0 && seen == files && most <= 65536 && calls > 1);
    struct stat up;
    check("getdents64: \"..\" is the parent directory's inode", stat(parent, &up) == 0 && dotdot == up.st_ino);
    munmap(buf, size);
    close(d);
    for (int i = 0; i < files; i++) {
        snprintf(p, sizeof p, "%s/entry-with-a-longer-name-%05d", dir, i);
        unlink(p);
    }
    rmdir(dir);
}

int main(void) {
    getdents_pieces("/tmp", "/tmp/getdents", 3000);
    getdents_pieces("/data", "/data/getdents", 1500);
    tmpfs_budget();
    const char *dir = "/data/fstest";
    char path[64], buf[64];
    mkdir(dir, 0755);

    snprintf(path, sizeof path, "%s/link", dir);
    unlink(path);
    symlink("a-target-that-is-only-text", path);
    int fd = open(path, O_RDWR | O_NOFOLLOW);
    check("O_NOFOLLOW on a symlink fails with ELOOP", fd < 0 && errno == ELOOP);
    unlink(path);

    snprintf(path, sizeof path, "%s/orphan", dir);
    fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    write(fd, "before", 6);
    unlink(path);
    check("unlinked file is gone from the directory", access(path, F_OK) < 0);
    pwrite(fd, "after!", 6, 6);
    memset(buf, 0, sizeof buf);
    pread(fd, buf, 12, 0);
    check("open fd still reads and writes the unlinked file", strcmp(buf, "beforeafter!") == 0);

    char other[64];
    snprintf(other, sizeof other, "%s/new", dir);
    int fd2 = open(other, O_RDWR | O_CREAT | O_TRUNC, 0644);
    write(fd2, "fresh", 5);
    memset(buf, 0, sizeof buf);
    pread(fd, buf, 12, 0);
    check("a new file does not share the orphan's data", strcmp(buf, "beforeafter!") == 0);
    memset(buf, 0, sizeof buf);
    pread(fd2, buf, 5, 0);
    check("the new file has its own data", strcmp(buf, "fresh") == 0);
    close(fd);
    close(fd2);
    unlink(other);

    snprintf(path, sizeof path, "%s/big", dir);
    fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    check("truncate beyond the ext2 limit fails with EFBIG", ftruncate(fd, (off_t)1 << 62) < 0 && errno == EFBIG);
    check("write at a huge offset fails with EFBIG", pwrite(fd, "x", 1, (off_t)1 << 62) < 0 && errno == EFBIG);
    void *m = mmap(NULL, 8192, PROT_READ, MAP_PRIVATE, fd, (off_t)-4096);
    check("mmap with an overflowing offset fails", m == MAP_FAILED);
    close(fd);
    unlink(path);

    snprintf(path, sizeof path, "%s/modes", dir);
    fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    write(fd, "data", 4);
    errno = 0;
    check("read on a write-only fd fails with EBADF", read(fd, buf, 4) < 0 && errno == EBADF);
    close(fd);
    fd = open(path, O_RDONLY);
    errno = 0;
    check("write on a read-only fd fails with EBADF", write(fd, "x", 1) < 0 && errno == EBADF);
    errno = 0;
    check("pwrite on a read-only fd fails with EBADF", pwrite(fd, "x", 1, 0) < 0 && errno == EBADF);
    errno = 0;
    check("ftruncate on a read-only fd fails with EINVAL", ftruncate(fd, 0) < 0 && errno == EINVAL);
    memset(buf, 0, sizeof buf);
    check("the file is unchanged", pread(fd, buf, 8, 0) == 4 && strcmp(buf, "data") == 0);
    close(fd);
    unlink(path);

    snprintf(path, sizeof path, "%s/rw", dir);
    rw_flag_checks(path, check);
    rmdir(dir);

    printf("fstest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
