/* Positional and vectored reads and writes with preadv2/pwritev2's flags,
 * on a file at `path` (fstest runs them on /data, the kernel's files;
 * lxtest on /tmp, the Linux server's): the offset -1 means the file
 * position, a positional write to an O_APPEND descriptor appends (as on
 * Linux), RWF_NOAPPEND and RWF_APPEND override O_APPEND per call, and
 * unsupported or contradicting flags fail. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

#ifndef RWF_NOWAIT
#define RWF_NOWAIT 0x08
#endif
#ifndef RWF_APPEND
#define RWF_APPEND 0x10
#endif
#ifndef RWF_NOAPPEND
#define RWF_NOAPPEND 0x20
#endif

static long rw_pwritev2(int fd, const char *s, long off, long flags) {
    struct iovec v = {(void *)s, strlen(s)};
    return syscall(SYS_pwritev2, fd, &v, 1, off, 0, flags);
}

static long rw_preadv2(int fd, char *buf, size_t len, long off, long flags) {
    struct iovec v = {buf, len};
    return syscall(SYS_preadv2, fd, &v, 1, off, 0, flags);
}

static int rw_contents(int fd, const char *want) {
    char buf[64] = {0};
    return pread(fd, buf, sizeof buf - 1, 0) == (ssize_t)strlen(want) && strcmp(buf, want) == 0;
}

static void rw_flag_checks(const char *path, void (*check)(const char *, int)) {
    char buf[16] = {0};
    int fd = open(path, O_CREAT | O_RDWR | O_TRUNC | O_APPEND, 0644);
    long w = fd >= 0 ? write(fd, "abc", 3) : -1;
    check("rw: open with O_APPEND", fd >= 0 && w == 3);
    if (fd < 0 || w != 3) printf("  (%s: open %d, write %ld, errno %d)\n", path, fd, w, errno);
    check("rw: pwrite64 on O_APPEND appends (Linux)",
          syscall(SYS_pwrite64, fd, "X", 1, 0) == 1 && rw_contents(fd, "abcX") && lseek(fd, 0, SEEK_CUR) == 3);
    check("rw: pwritev2 RWF_NOAPPEND writes at the offset", rw_pwritev2(fd, "Y", 0, RWF_NOAPPEND) == 1 && rw_contents(fd, "YbcX"));
    check("rw: pwritev2 at -1 writes at the file position",
          rw_pwritev2(fd, "Z", -1, RWF_NOAPPEND) == 1 && rw_contents(fd, "YbcZ") && lseek(fd, 0, SEEK_CUR) == 4);
    int plain = open(path, O_RDWR);
    check("rw: pwritev2 RWF_APPEND appends, position stays",
          rw_pwritev2(plain, "W", 1, RWF_APPEND) == 1 && rw_contents(fd, "YbcZW") && lseek(plain, 0, SEEK_CUR) == 0);
    check("rw: preadv2 at -1 reads at the file position",
          rw_preadv2(fd, buf, sizeof buf, -1, 0) == 1 && buf[0] == 'W' && lseek(fd, 0, SEEK_CUR) == 5);
    struct iovec v = {buf, sizeof buf};
    check("rw: preadv reads at the offset", syscall(SYS_preadv, fd, &v, 1, 1, 0) == 4 && memcmp(buf, "bcZW", 4) == 0);
    check("rw: pwritev on O_APPEND appends", syscall(SYS_pwritev, fd, &(struct iovec){"V", 1}, 1, 0, 0) == 1 && rw_contents(fd, "YbcZWV"));
    errno = 0;
    check("rw: RWF_NOWAIT is EOPNOTSUPP", rw_preadv2(fd, buf, 1, 0, RWF_NOWAIT) == -1 && errno == EOPNOTSUPP);
    errno = 0;
    check("rw: RWF_APPEND with RWF_NOAPPEND is EINVAL", rw_pwritev2(fd, "x", 0, RWF_APPEND | RWF_NOAPPEND) == -1 && errno == EINVAL);
    errno = 0;
    check("rw: an offset below -1 is EINVAL", rw_preadv2(fd, buf, 1, -2, 0) == -1 && errno == EINVAL);
    errno = 0;
    check("rw: preadv2 on a write-only descriptor is EBADF",
          (close(plain), plain = open(path, O_WRONLY)) >= 0 && rw_preadv2(plain, buf, 1, 0, 0) == -1 && errno == EBADF);
    close(plain);
    int p[2];
    errno = 0;
    check("rw: pwritev2 at an offset on a pipe is ESPIPE", pipe(p) == 0 && rw_pwritev2(p[1], "x", 0, 0) == -1 && errno == ESPIPE);
    check("rw: pwritev2 at -1 on a pipe writes", rw_pwritev2(p[1], "p", -1, 0) == 1 && read(p[0], buf, 1) == 1 && buf[0] == 'p');
    close(p[0]);
    close(p[1]);
    close(fd);
    unlink(path);
}
