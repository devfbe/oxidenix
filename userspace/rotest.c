/* A read-only data disk (run by /etc/rotests.sh in the builder's read-only
 * boot): /data mounted read-only as Linux mounts a filesystem whose device
 * takes no writes. Readable: the file the builder put there, the first boot's
 * last file ("after-rw", with its FIFO and device node, which open and work),
 * directories, shared and private mappings; every
 * change EROFS (Linux's order: EEXIST for a name that exists, EISDIR for a
 * directory opened for writing); statfs's ST_RDONLY, /proc/mounts' "ro";
 * fsync and sync do nothing and return; nothing waits for ever (a watchdog
 * fails the test). */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/statvfs.h>
#include <sys/sysmacros.h>
#include <sys/un.h>
#include <sys/xattr.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

/* Whether `r` is -1 with errno `e`. */
static int fails(long r, int e) {
    return r == -1 && errno == e;
}

static void on_alarm(int sig) {
    (void)sig;
    static const char text[] = "rotest: watchdog expired (something hangs)\n";
    write(1, text, sizeof text - 1);
    _exit(1);
}

static long slurp(const char *path, char *buf, long cap) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    long got = 0, n;
    while (got < cap && (n = read(fd, buf + got, cap - got)) > 0) got += n;
    close(fd);
    return got;
}

static char a[4 << 20], b[4 << 20];

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    signal(SIGALRM, on_alarm);
    alarm(120);
    const char *hello = "/data/rotest/hello";

    /* Mounted read-only, and said so. */
    struct statfs fs;
    check("statfs /data", statfs("/data", &fs) == 0);
    check("... ST_RDONLY (and ST_VALID) in f_flags", (fs.f_flags & ST_RDONLY) && (fs.f_flags & 0x20));
    struct statvfs vfs;
    check("statvfs /data: ST_RDONLY", statvfs("/data", &vfs) == 0 && (vfs.f_flag & ST_RDONLY));
    check("/tmp is not read-only", statfs("/tmp", &fs) == 0 && !(fs.f_flags & ST_RDONLY) && (fs.f_flags & 0x20));
    char mounts[4096] = {0};
    slurp("/proc/mounts", mounts, sizeof mounts - 1);
    check("/proc/mounts: /data ro, / rw", strstr(mounts, "/dev/vda /data ext2 ro 0 0\n") && strstr(mounts, "tmpfs / tmpfs rw 0 0\n"));

    /* Readable. */
    long n = slurp(hello, a, sizeof a), m = slurp("/bin/hello", b, sizeof b);
    check("the builder's file reads as it put it", n > 0 && n == m && memcmp(a, b, n) == 0);
    struct stat st;
    check("stat", stat(hello, &st) == 0 && S_ISREG(st.st_mode) && st.st_size == n);
    if (argc > 1 && strcmp(argv[1], "after-rw") == 0) {
        char text[64] = {0};
        check("the first boot's last file reads back", slurp("/data/rotest/written", text, sizeof text - 1) == 26 &&
                                                              strcmp(text, "written by the self-tests\n") == 0);
        /* Its FIFO and device node: a read-only disk keeps them usable (Linux asks
         * for write access to a file, a directory or a symlink only). */
        const char *fifo = "/data/rotest/fifo", *null = "/data/rotest/null";
        check("its FIFO: S_IFIFO", stat(fifo, &st) == 0 && S_ISFIFO(st.st_mode) && (st.st_mode & 07777) == 0600);
        check("... access W_OK: 0", access(fifo, W_OK) == 0);
        int rw = open(fifo, O_RDWR | O_NONBLOCK);
        check("... opens O_RDWR, writes and reads", rw >= 0 && write(rw, "ro", 2) == 2 && read(rw, text, sizeof text) == 2 && memcmp(text, "ro", 2) == 0);
        int w = open(fifo, O_WRONLY | O_NONBLOCK);
        check("... a writer opens beside it", w >= 0);
        if (w >= 0) close(w);
        if (rw >= 0) close(rw);
        check("... O_WRONLY|O_NONBLOCK without readers: ENXIO", fails(open(fifo, O_WRONLY | O_NONBLOCK), ENXIO));
        check("... truncate: EINVAL", fails(truncate(fifo, 0), EINVAL));
        check("... mknod over it: EEXIST", fails(mknod(fifo, S_IFIFO | 0600, 0), EEXIST));
        check("its device node: (1,3)", stat(null, &st) == 0 && S_ISCHR(st.st_mode) && major(st.st_rdev) == 1 && minor(st.st_rdev) == 3);
        int fd = open(null, O_WRONLY);
        check("... opens null for writing", fd >= 0 && write(fd, "gone", 4) == 4);
        if (fd >= 0) close(fd);
        check("mknod of a new name: EROFS", fails(mknod("/data/rotest/new", S_IFIFO | 0600, 0), EROFS) &&
                                               fails(mknod("/data/rotest/new", S_IFCHR | 0600, makedev(1, 3)), EROFS));
    }
    DIR *d = opendir("/data/rotest");
    int listed = 0;
    struct dirent *e;
    while (d && (e = readdir(d)))
        if (strcmp(e->d_name, "hello") == 0) listed = 1;
    if (d) closedir(d);
    check("a directory lists", listed);
    int fd = open(hello, O_RDONLY);
    char *shared = fd >= 0 ? mmap(NULL, n, PROT_READ, MAP_SHARED, fd, 0) : MAP_FAILED;
    check("a shared read-only mapping shows the file", shared != MAP_FAILED && memcmp(shared, a, n) == 0);
    check("... a writable shared one is EACCES (opened read-only)",
          fd >= 0 && mmap(NULL, n, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) == MAP_FAILED && errno == EACCES);
    char *private = fd >= 0 ? mmap(NULL, n, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0) : MAP_FAILED;
    int copy_ok = private != MAP_FAILED;
    if (copy_ok) {
        private[0] ^= 0xff;
        copy_ok = private[0] != a[0] && shared[0] == a[0];
    }
    check("a private writable mapping takes stores of its own", copy_ok);
    check("fsync of a file opened read-only: 0", fd >= 0 && fsync(fd) == 0 && fdatasync(fd) == 0);
    check("syncfs: 0", fd >= 0 && syncfs(fd) == 0);
    if (fd >= 0) close(fd);
    check("access R_OK and X_OK", access(hello, R_OK | X_OK) == 0);

    /* Every change EROFS. */
    struct stat was;
    stat(hello, &was);
    check("access W_OK: EROFS", fails(access(hello, W_OK), EROFS));
    check("... of a directory too", fails(access("/data/rotest", W_OK), EROFS));
    check("open O_WRONLY: EROFS", fails(open(hello, O_WRONLY), EROFS));
    check("open O_RDWR: EROFS", fails(open(hello, O_RDWR), EROFS));
    check("open O_RDONLY|O_TRUNC: EROFS", fails(open(hello, O_RDONLY | O_TRUNC), EROFS));
    check("open O_CREAT of a new name: EROFS", fails(open("/data/rotest/new", O_WRONLY | O_CREAT, 0644), EROFS));
    fd = open(hello, O_RDONLY | O_CREAT, 0644);
    check("open O_RDONLY|O_CREAT of an existing file works", fd >= 0);
    if (fd >= 0) {
        check("... write on it: EBADF", fails(write(fd, "x", 1), EBADF));
        check("... ftruncate: EINVAL (not open for writing)", fails(ftruncate(fd, 0), EINVAL));
        check("... fchmod: EROFS", fails(fchmod(fd, 0600), EROFS));
        check("... fchown: EROFS", fails(fchown(fd, 0, 0), EROFS));
        check("... futimens: EROFS", fails(futimens(fd, NULL), EROFS));
        close(fd);
    }
    check("open O_CREAT|O_EXCL of an existing file: EEXIST", fails(open(hello, O_WRONLY | O_CREAT | O_EXCL, 0644), EEXIST));
    check("open a directory O_WRONLY: EISDIR", fails(open("/data/rotest", O_WRONLY), EISDIR));
    check("truncate: EROFS", fails(truncate(hello, 0), EROFS));
    check("truncate a directory: EISDIR", fails(truncate("/data/rotest", 0), EISDIR));
    check("mkdir: EROFS", fails(mkdir("/data/rotest/dir", 0755), EROFS));
    check("mkdir of an existing name: EEXIST", fails(mkdir("/data/rotest", 0755), EEXIST));
    check("symlink: EROFS", fails(symlink("hello", "/data/rotest/link"), EROFS));
    check("symlink over an existing name: EEXIST", fails(symlink("x", hello), EEXIST));
    check("mknod: EROFS", fails(mknod("/data/rotest/node", S_IFIFO | 0600, 0), EROFS) && fails(mkfifo("/data/rotest/node", 0600), EROFS));
    check("mknod of an existing name: EEXIST", fails(mknod(hello, S_IFCHR | 0600, makedev(1, 3)), EEXIST));
    check("unlink: EROFS", fails(unlink(hello), EROFS));
    check("unlink of a missing name: EROFS (before the lookup)", fails(unlink("/data/rotest/missing"), EROFS));
    check("rmdir: EROFS", fails(rmdir("/data/rotest"), EROFS));
    check("rename: EROFS", fails(rename(hello, "/data/rotest/moved"), EROFS));
    check("chmod: EROFS", fails(chmod(hello, 0600), EROFS));
    check("chown: EROFS", fails(chown(hello, 0, 0), EROFS));
    check("utimensat: EROFS", fails(utimensat(AT_FDCWD, hello, NULL, 0), EROFS));
    check("setxattr and removexattr: EROFS; getxattr still EOPNOTSUPP",
          fails(setxattr(hello, "user.x", "v", 1, 0), EROFS) && fails(removexattr(hello, "user.x"), EROFS) && fails(getxattr(hello, "user.x", NULL, 0), EOPNOTSUPP));
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un un = {.sun_family = AF_UNIX};
    strcpy(un.sun_path, "/data/rotest/sock");
    check("bind an AF_UNIX socket on /data: EROFS", s >= 0 && fails(bind(s, (struct sockaddr *)&un, sizeof un), EROFS));
    if (s >= 0) close(s);

    /* Nothing changed. */
    m = slurp(hello, b, sizeof b);
    check("the file is as it was", m == n && memcmp(a, b, n) == 0 && stat(hello, &st) == 0 && (st.st_mode & 07777) == 0755);
    check("... its times too (the refused changes left none behind)",
          st.st_atime == was.st_atime && st.st_mtime == was.st_mtime && st.st_ctime == was.st_ctime);
    check("no new name", access("/data/rotest/new", F_OK) < 0 && access("/data/rotest/dir", F_OK) < 0 && access("/data/rotest/moved", F_OK) < 0 &&
                             access("/data/rotest/node", F_OK) < 0);
    sync();
    check("sync returns", 1);
    printf("rotest: %s\n", failures ? "FAILED" : "all passed");
    return failures ? 1 : 0;
}
