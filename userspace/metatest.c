/* File metadata in the Linux server, on tmpfs (/tmp) and on /data:
 * timestamps (set by utimensat, futimens, utimes; moved by writes,
 * truncation, chmod, directory changes; statx's birth time; on /data the
 * times a write set survive the write-back), modes through descriptors
 * (fchmod, fchmodat2), the chown family, and supplementary groups. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-64s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static struct stat st_of(const char *path) {
    struct stat st;
    memset(&st, 0, sizeof st);
    lstat(path, &st);
    return st;
}

static long long ns(struct timespec t) { return t.tv_sec * 1000000000LL + t.tv_nsec; }

static void times_on(const char *dir, int nanoseconds) {
    char path[96], sub[96], name[128];
    snprintf(path, sizeof path, "%s/meta.%d", dir, getpid());
    snprintf(sub, sizeof sub, "%s/metadir.%d", dir, getpid());
    int fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0644);

    struct timespec set[2] = {{1000000000, 123456789}, {1500000000, 987654321}};
    snprintf(name, sizeof name, "%s: utimensat sets atime and mtime", dir);
    struct stat st;
    int ok = utimensat(AT_FDCWD, path, set, 0) == 0 && stat(path, &st) == 0 && st.st_atim.tv_sec == 1000000000 && st.st_mtim.tv_sec == 1500000000;
    if (nanoseconds) ok = ok && st.st_atim.tv_nsec == 123456789 && st.st_mtim.tv_nsec == 987654321;
    check(name, ok);

    struct timespec omit[2] = {{0, UTIME_OMIT}, {0, UTIME_NOW}};
    time_t before = time(NULL);
    snprintf(name, sizeof name, "%s: UTIME_OMIT keeps one, UTIME_NOW sets the other", dir);
    check(name, futimens(fd, omit) == 0 && fstat(fd, &st) == 0 && st.st_atim.tv_sec == 1000000000 && st.st_mtim.tv_sec >= before &&
                    st.st_ctim.tv_sec >= before);

    struct timeval tv[2] = {{2000000000, 5}, {2000000001, 6}};
    snprintf(name, sizeof name, "%s: utimes (microseconds)", dir);
    check(name, utimes(path, tv) == 0 && stat(path, &st) == 0 && st.st_atim.tv_sec == 2000000000 && st.st_mtim.tv_sec == 2000000001 &&
                    (!nanoseconds || st.st_mtim.tv_nsec == 6000));
    errno = 0;
    struct timespec bad[2] = {{0, 1000000000}, {0, 0}};
    snprintf(name, sizeof name, "%s: nanoseconds out of range are EINVAL", dir);
    check(name, utimensat(AT_FDCWD, path, bad, 0) == -1 && errno == EINVAL);

    /* A write moves mtime and ctime to now, not atime. */
    utimensat(AT_FDCWD, path, set, 0);
    write(fd, "data", 4);
    stat(path, &st);
    snprintf(name, sizeof name, "%s: a write moves mtime and ctime", dir);
    check(name, st.st_mtim.tv_sec >= before && st.st_ctim.tv_sec >= before && st.st_atim.tv_sec == 1000000000);
    /* ... also once the data reached the disk. */
    fsync(fd);
    struct stat after;
    stat(path, &after);
    snprintf(name, sizeof name, "%s: the times stay after fsync", dir);
    check(name, ns(after.st_mtim) == ns(st.st_mtim) && after.st_atim.tv_sec == 1000000000);
    sync();

    utimensat(AT_FDCWD, path, set, 0);
    snprintf(name, sizeof name, "%s: ftruncate moves mtime", dir);
    check(name, ftruncate(fd, 1) == 0 && fstat(fd, &st) == 0 && st.st_mtim.tv_sec >= before);
    utimensat(AT_FDCWD, path, set, 0);
    snprintf(name, sizeof name, "%s: chmod moves ctime only", dir);
    check(name, chmod(path, 0600) == 0 && stat(path, &st) == 0 && st.st_mtim.tv_sec == 1500000000 && st.st_ctim.tv_sec >= before);

    mkdir(sub, 0755);
    utimensat(AT_FDCWD, sub, set, 0);
    char inner[128];
    snprintf(inner, sizeof inner, "%s/f", sub);
    close(open(inner, O_CREAT | O_WRONLY, 0644));
    snprintf(name, sizeof name, "%s: a new name moves its directory's mtime", dir);
    check(name, stat(sub, &st) == 0 && st.st_mtim.tv_sec >= before);
    utimensat(AT_FDCWD, sub, set, 0);
    unlink(inner);
    snprintf(name, sizeof name, "%s: ... and a removed one", dir);
    check(name, stat(sub, &st) == 0 && st.st_mtim.tv_sec >= before);
    rmdir(sub);

    /* Modes through descriptors, and fchmodat2's flags. */
    snprintf(name, sizeof name, "%s: fchmod", dir);
    check(name, fchmod(fd, 0640) == 0 && (st_of(path).st_mode & 0777) == 0640);
    snprintf(name, sizeof name, "%s: fchmodat2 with AT_EMPTY_PATH", dir);
    check(name, syscall(452, fd, "", 0604, AT_EMPTY_PATH) == 0 && (st_of(path).st_mode & 0777) == 0604);
    char link[128];
    snprintf(link, sizeof link, "%s.link", path);
    symlink(path, link);
    errno = 0;
    snprintf(name, sizeof name, "%s: fchmodat2 on a symlink itself is EOPNOTSUPP", dir);
    check(name, syscall(452, AT_FDCWD, link, 0600, AT_SYMLINK_NOFOLLOW) == -1 && errno == EOPNOTSUPP);
    snprintf(name, sizeof name, "%s: chown, fchown, lchown, fchownat (root keeps it)", dir);
    check(name, chown(path, 0, 0) == 0 && fchown(fd, -1, -1) == 0 && lchown(link, 0, 0) == 0 && fchownat(AT_FDCWD, link, 0, 0, AT_SYMLINK_NOFOLLOW) == 0 &&
                    st_of(path).st_uid == 0);
    errno = 0;
    snprintf(name, sizeof name, "%s: chown of a missing file is ENOENT", dir);
    check(name, chown("/tmp/no/such/file", 0, 0) == -1 && errno == ENOENT);
    unlink(link);
    close(fd);
    unlink(path);
}

int main(void) {
    times_on("/tmp", 1);
    times_on("/data", 0);

    /* tmpfs has a birth time (statx), and times are nanoseconds. */
    char path[64];
    snprintf(path, sizeof path, "/tmp/birth.%d", getpid());
    struct timespec t0;
    clock_gettime(CLOCK_REALTIME, &t0);
    close(open(path, O_CREAT | O_WRONLY, 0644));
    struct statx x;
    check("statx reports tmpfs's birth time", syscall(SYS_statx, AT_FDCWD, path, 0, STATX_BTIME, &x) == 0 && (x.stx_mask & STATX_BTIME) &&
                                                     x.stx_btime.tv_sec >= t0.tv_sec - 1 && x.stx_btime.tv_sec <= t0.tv_sec + 5);
    unlink(path);
    check("/data reports none (ext2)", syscall(SYS_statx, AT_FDCWD, "/data", 0, STATX_BTIME, &x) == 0 && !(x.stx_mask & STATX_BTIME));

    /* Supplementary groups: none (root, started by init). */
    gid_t groups[4];
    check("getgroups: no supplementary groups", getgroups(4, groups) == 0 && getgroups(0, NULL) == 0);
    errno = 0;
    check("getgroups with a negative size is EINVAL", syscall(SYS_getgroups, -1, groups) == -1 && errno == EINVAL);
    check("setgroups is taken", setgroups(1, groups) == 0 && setgroups(0, NULL) == 0);

    printf("metatest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
