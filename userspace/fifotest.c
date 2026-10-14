/* mknod(2), mknodat(2) and FIFOs (fifo(7)) in the Linux server, on tmpfs (/tmp)
 * and on /data, against Linux's semantics: regular files, FIFOs (mkfifo(3)
 * through musl), character and block devices and sockets made with the umask
 * applied, their status (S_IFIFO, st_rdev, d_type, statx's device numbers), a
 * character node opening its driver by number (null, zero), a block or unknown
 * one ENXIO, the errors (EEXIST also for a dangling symlink and a mount point,
 * ENOENT, ENOTDIR, a trailing slash, EINVAL for an unknown type, EPERM for a
 * directory's and in /sys and devpts, ENOENT in /proc); FIFOs as named pipes: the open rules
 * (a non-blocking reader at once, a non-blocking writer without readers ENXIO,
 * O_RDWR never waiting, a blocking reader or writer waiting for its partner,
 * EINVAL for access mode 3, ENOTDIR for O_DIRECTORY), the open interrupted by
 * a signal (EINTR without SA_RESTART, restarted with it, the interrupted
 * reader counted no more), reads, writes, end of file, EPIPE and SIGPIPE, the
 * buffer going with the last close, one pipe for every opener of the inode
 * (other processes, a symlink, /proc/self/fd), poll (no POLLHUP for a reader
 * that has seen no writer), select and epoll, fstat, fchmod, lseek, the times
 * a read and a write move, inotify's events, and a FIFO removed while open. */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/inotify.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;
static char where[16];

static void check(const char *name, int ok) {
    printf("%-6s %-66s %s\n", where, name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

/* Whether `r` is -1 with errno `e`. */
static int fails(long r, int e) {
    return r == -1 && errno == e;
}

static void on_watchdog(int sig) {
    (void)sig;
    static const char text[] = "fifotest: watchdog expired (something hangs)\n";
    write(1, text, sizeof text - 1);
    _exit(1);
}

static volatile sig_atomic_t usr1, sigpipes;
static void on_usr1(int sig) {
    (void)sig;
    usr1++;
}
static void on_sigpipe(int sig) {
    (void)sig;
    sigpipes++;
}

static void sleep_ms(long ms) {
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000};
    nanosleep(&t, NULL);
}

/* Whether a read of `fd` (non-blocking or ready) gives exactly `want`. */
static int reads(int fd, const char *want) {
    char buf[64];
    ssize_t n = read(fd, buf, sizeof buf);
    return n == (ssize_t)strlen(want) && memcmp(buf, want, n) == 0;
}

/* poll's revents for `fd` asked for `events`, without waiting. */
static int revents(int fd, short events) {
    struct pollfd p = {.fd = fd, .events = events};
    return poll(&p, 1, 0) < 0 ? -1 : p.revents;
}

static int dtype_of(const char *dir, const char *name) {
    DIR *d = opendir(dir);
    struct dirent *e;
    int t = -1;
    while (d && (e = readdir(d)))
        if (strcmp(e->d_name, name) == 0) t = e->d_type;
    if (d) closedir(d);
    return t;
}

/* A child that, after `delay_ms`, sends its parent SIGUSR1, waits `then_ms`
 * and (if `fifo`) opens it for writing and writes `text`. */
static pid_t helper(long delay_ms, long then_ms, const char *fifo, const char *text) {
    pid_t parent = getpid(), pid = fork();
    if (pid == 0) {
        sleep_ms(delay_ms);
        kill(parent, SIGUSR1);
        if (fifo) {
            sleep_ms(then_ms);
            int w = open(fifo, O_WRONLY);
            write(w, text, strlen(text));
            close(w);
        }
        _exit(0);
    }
    return pid;
}

/* The node calls: what mknod makes, its status and its errors. */
static void nodes(const char *d) {
    char p[160], q[160];
    struct stat st;
    umask(022);

    snprintf(p, sizeof p, "%s/fifo", d);
    check("mkfifo (musl: mknod S_IFIFO)", mkfifo(p, 0666) == 0);
    check("... S_IFIFO, the umask applied, no size, one link, no rdev",
          stat(p, &st) == 0 && S_ISFIFO(st.st_mode) && (st.st_mode & 07777) == 0644 && st.st_size == 0 && st.st_nlink == 1 && st.st_rdev == 0);
    check("... again: EEXIST", fails(mkfifo(p, 0600), EEXIST));
    check("... d_type DT_FIFO", dtype_of(d, "fifo") == DT_FIFO);
    check("truncate a FIFO: EINVAL", fails(truncate(p, 0), EINVAL));

    snprintf(p, sizeof p, "%s/reg0", d);
    check("mknod type 0: a regular file", mknod(p, 0666, 0) == 0 && stat(p, &st) == 0 && S_ISREG(st.st_mode) && (st.st_mode & 07777) == 0644);
    snprintf(p, sizeof p, "%s/reg", d);
    int fd = -1;
    check("mknod S_IFREG: a regular file, readable and writable",
          mknod(p, S_IFREG | 0600, 0) == 0 && stat(p, &st) == 0 && S_ISREG(st.st_mode) && (st.st_mode & 07777) == 0600 &&
              (fd = open(p, O_RDWR)) >= 0 && write(fd, "data", 4) == 4 && pread(fd, q, 4, 0) == 4 && memcmp(q, "data", 4) == 0);
    close(fd);
    check("... d_type DT_REG", dtype_of(d, "reg") == DT_REG);

    snprintf(p, sizeof p, "%s/null", d);
    check("mknod c 1 3", mknod(p, S_IFCHR | 0666, makedev(1, 3)) == 0);
    check("... S_IFCHR, rdev (1,3), the umask applied",
          stat(p, &st) == 0 && S_ISCHR(st.st_mode) && st.st_rdev == makedev(1, 3) && (st.st_mode & 07777) == 0644 && st.st_size == 0);
    fd = open(p, O_RDWR);
    check("... opens null: writes go, reads end", fd >= 0 && write(fd, "gone", 4) == 4 && read(fd, q, 8) == 0);
    struct stat fst;
    check("... fstat is the node's", fd >= 0 && fstat(fd, &fst) == 0 && fst.st_ino == st.st_ino && fst.st_rdev == makedev(1, 3));
    close(fd);
    check("... d_type DT_CHR", dtype_of(d, "null") == DT_CHR);
    snprintf(p, sizeof p, "%s/zero", d);
    fd = mknod(p, S_IFCHR | 0600, makedev(1, 5)) == 0 ? open(p, O_RDONLY) : -1;
    memset(q, 7, 16);
    check("mknod c 1 5 opens zero", fd >= 0 && read(fd, q, 16) == 16 && q[0] == 0 && q[15] == 0);
    close(fd);

    snprintf(p, sizeof p, "%s/sda", d);
    check("mknod b 8 0", mknod(p, S_IFBLK | 0660, makedev(8, 0)) == 0);
    check("... S_IFBLK, rdev (8,0)", stat(p, &st) == 0 && S_ISBLK(st.st_mode) && st.st_rdev == makedev(8, 0) && (st.st_mode & 07777) == 0640);
    check("... opening it: ENXIO (no driver)", fails(open(p, O_RDONLY), ENXIO));
    check("... d_type DT_BLK", dtype_of(d, "sda") == DT_BLK);
    snprintf(p, sizeof p, "%s/wide", d);
    struct statx x;
    check("mknod c 259 70000 (beyond 8 bits each)",
          mknod(p, S_IFCHR | 0600, makedev(259, 70000)) == 0 && stat(p, &st) == 0 && major(st.st_rdev) == 259 && minor(st.st_rdev) == 70000);
    check("... statx's rdev", syscall(SYS_statx, AT_FDCWD, p, 0, STATX_BASIC_STATS, &x) == 0 && x.stx_rdev_major == 259 && x.stx_rdev_minor == 70000);
    check("... no driver: ENXIO", fails(open(p, O_WRONLY), ENXIO));
    check("... renamed, still a device", snprintf(q, sizeof q, "%s/wide2", d) > 0 && rename(p, q) == 0 && stat(q, &st) == 0 && S_ISCHR(st.st_mode) &&
                                                 st.st_rdev == makedev(259, 70000) && dtype_of(d, "wide2") == DT_CHR);
    unlink(q);

    snprintf(p, sizeof p, "%s/sock", d);
    check("mknod S_IFSOCK", mknod(p, S_IFSOCK | 0666, 0) == 0 && stat(p, &st) == 0 && S_ISSOCK(st.st_mode) && dtype_of(d, "sock") == DT_SOCK);
    check("... opening it: ENXIO", fails(open(p, O_RDONLY), ENXIO));
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un un = {.sun_family = AF_UNIX};
    snprintf(un.sun_path, sizeof un.sun_path, "%s", p);
    check("... connecting: ECONNREFUSED (nothing bound)", fails(connect(s, (struct sockaddr *)&un, sizeof un), ECONNREFUSED));
    close(s);

    /* Errors, in Linux's order: the type before the name. */
    snprintf(p, sizeof p, "%s/x", d);
    check("mknod S_IFDIR: EPERM", fails(mknod(p, S_IFDIR | 0755, 0), EPERM));
    check("... also where the name cannot be: EPERM", fails(mknod("/no/such/dir/x", S_IFDIR | 0755, 0), EPERM));
    check("an unknown type: EINVAL", fails(mknod(p, 0170000 | 0644, 0), EINVAL) && fails(mknod(p, 0030000 | 0644, 0), EINVAL));
    check("... nothing made", lstat(p, &st) == -1 && errno == ENOENT);
    snprintf(p, sizeof p, "%s/missing/x", d);
    check("a missing directory: ENOENT", fails(mknod(p, S_IFIFO | 0644, 0), ENOENT));
    snprintf(p, sizeof p, "%s/reg/x", d);
    check("through a file: ENOTDIR", fails(mknod(p, S_IFIFO | 0644, 0), ENOTDIR));
    snprintf(p, sizeof p, "%s/null", d);
    check("an existing name: EEXIST", fails(mknod(p, S_IFIFO | 0644, 0), EEXIST));
    snprintf(p, sizeof p, "%s/dangling", d);
    symlink("nowhere", p);
    check("a dangling symlink: EEXIST (not followed)", fails(mknod(p, S_IFIFO | 0644, 0), EEXIST));
    unlink(p);
    snprintf(p, sizeof p, "%s/new/", d);
    check("a new name with a trailing slash: ENOENT", fails(mknod(p, S_IFIFO | 0644, 0), ENOENT));
    snprintf(p, sizeof p, "%s/fifo/", d);
    check("an existing one with a trailing slash: EEXIST", fails(mknod(p, S_IFIFO | 0644, 0), EEXIST));
    int dfd = open(d, O_RDONLY | O_DIRECTORY);
    check("mknodat relative to a directory", dfd >= 0 && mknodat(dfd, "at", S_IFIFO | 0600, 0) == 0 && fstatat(dfd, "at", &st, 0) == 0 && S_ISFIFO(st.st_mode));
    unlinkat(dfd, "at", 0);
    close(dfd);
}

/* FIFOs as named pipes. */
static void fifos(const char *d) {
    char p[160], link[160], proc[64], buf[256];
    struct stat st, fst;
    snprintf(p, sizeof p, "%s/fifo", d);

    /* Non-blocking opens. */
    check("O_WRONLY|O_NONBLOCK without readers: ENXIO", fails(open(p, O_WRONLY | O_NONBLOCK), ENXIO));
    int r = open(p, O_RDONLY | O_NONBLOCK);
    check("O_RDONLY|O_NONBLOCK opens at once", r >= 0);
    check("... no writer yet: read is end of file", read(r, buf, 8) == 0);
    check("... poll: nothing (no POLLHUP before a writer came)", revents(r, POLLIN) == 0);
    int w = open(p, O_WRONLY | O_NONBLOCK);
    check("O_WRONLY|O_NONBLOCK with a reader opens", w >= 0);
    check("... poll: writable", revents(w, POLLOUT) == POLLOUT);
    check("... the reader: empty with a writer, EAGAIN", fails(read(r, buf, 8), EAGAIN) && revents(r, POLLIN) == 0);
    check("write and read", write(w, "hello", 5) == 5 && revents(r, POLLIN) == POLLIN && reads(r, "hello"));
    check("fstat: S_IFIFO, the node's inode and device",
          fstat(r, &fst) == 0 && stat(p, &st) == 0 && S_ISFIFO(fst.st_mode) && fst.st_ino == st.st_ino && fst.st_dev == st.st_dev);
    check("lseek: ESPIPE", fails(lseek(r, 0, SEEK_SET), ESPIPE) && fails(pread(r, buf, 1, 0), ESPIPE));
    snprintf(proc, sizeof proc, "/proc/self/fd/%d", r);
    ssize_t n = readlink(proc, buf, sizeof buf - 1);
    check("/proc/self/fd/N: the FIFO's path", n == (ssize_t)strlen(p) && memcmp(buf, p, n) == 0);
    int r2 = open(proc, O_RDONLY | O_NONBLOCK);
    check("... opened: another reader of the same pipe", r2 >= 0 && write(w, "via proc", 8) == 8 && reads(r2, "via proc"));
    close(r2);
    check("fchmod changes the node", fchmod(r, 0600) == 0 && stat(p, &st) == 0 && (st.st_mode & 07777) == 0600);
    close(w);
    check("the writer gone: POLLHUP, end of file", (revents(r, POLLIN) & POLLHUP) && read(r, buf, 8) == 0);
    close(r);

    /* O_RDWR never waits; the data goes with the last close. */
    int rw = open(p, O_RDWR);
    check("O_RDWR opens without a partner", rw >= 0 && write(rw, "abc", 3) == 3 && reads(rw, "abc"));
    write(rw, "left", 4);
    close(rw);
    rw = open(p, O_RDWR | O_NONBLOCK);
    check("... the buffer went with the last close", rw >= 0 && fails(read(rw, buf, 8), EAGAIN));
    close(rw);
    /* (3, not musl's O_ACCMODE, which has O_PATH in it.) */
    check("access mode 3: EINVAL", fails(open(p, 3), EINVAL));
    check("O_DIRECTORY: ENOTDIR", fails(open(p, O_RDONLY | O_DIRECTORY), ENOTDIR));
    check("O_CREAT of the existing FIFO opens it", (rw = open(p, O_RDWR | O_CREAT | O_TRUNC, 0644)) >= 0 && fstat(rw, &fst) == 0 && S_ISFIFO(fst.st_mode));
    close(rw);

    /* A blocking reader waits for a writer: a child's, by another path. */
    int pp[2];
    pipe(pp);
    snprintf(link, sizeof link, "%s/link", d);
    symlink("fifo", link);
    pid_t pid = fork();
    if (pid == 0) {
        int fd = open(link, O_RDONLY);
        write(pp[1], "o", 1);
        n = read(fd, buf, sizeof buf);
        write(pp[1], buf, n > 0 ? n : 0);
        _exit(0);
    }
    struct pollfd wait_open = {.fd = pp[0], .events = POLLIN};
    check("a blocking reader waits for a writer", poll(&wait_open, 1, 300) == 0);
    w = open(p, O_WRONLY);
    check("... a writer opens (the waiting reader counts) and it goes on", w >= 0 && poll(&wait_open, 1, 5000) == 1 && read(pp[0], buf, 1) == 1);
    check("... one pipe for both paths and processes", write(w, "shared", 6) == 6 && poll(&wait_open, 1, 5000) == 1 && reads(pp[0], "shared"));
    waitpid(pid, NULL, 0);
    close(w);

    /* A blocking writer waits for a reader. */
    pid = fork();
    if (pid == 0) {
        int fd = open(p, O_WRONLY);
        write(pp[1], "o", 1);
        write(fd, "from the child", 14);
        _exit(0);
    }
    check("a blocking writer waits for a reader", poll(&wait_open, 1, 300) == 0);
    r = open(p, O_RDONLY);
    check("... a reader opens and it goes on", r >= 0 && poll(&wait_open, 1, 5000) == 1 && read(pp[0], buf, 1) == 1);
    waitpid(pid, NULL, 0);
    check("... its data, then end of file", reads(r, "from the child") && read(r, buf, 8) == 0);
    close(r);
    close(pp[0]);
    close(pp[1]);
    unlink(link);

    /* The blocking open and signals. */
    struct sigaction sa = {.sa_handler = on_usr1};
    sigaction(SIGUSR1, &sa, NULL);
    usr1 = 0;
    pid = helper(300, 0, NULL, NULL);
    check("a signal ends a waiting open: EINTR (no SA_RESTART)", fails(open(p, O_RDONLY), EINTR) && usr1 == 1);
    waitpid(pid, NULL, 0);
    check("... the interrupted reader counts no more", fails(open(p, O_WRONLY | O_NONBLOCK), ENXIO));
    sa.sa_flags = SA_RESTART;
    sigaction(SIGUSR1, &sa, NULL);
    usr1 = 0;
    pid = helper(300, 300, p, "restarted");
    r = open(p, O_RDONLY);
    check("with SA_RESTART the open goes on after the handler", r >= 0 && usr1 == 1 && reads(r, "restarted"));
    waitpid(pid, NULL, 0);
    close(r);
    signal(SIGUSR1, SIG_DFL);

    /* EPIPE and SIGPIPE. */
    signal(SIGPIPE, on_sigpipe);
    r = open(p, O_RDONLY | O_NONBLOCK);
    w = open(p, O_WRONLY | O_NONBLOCK);
    close(r);
    sigpipes = 0;
    check("a write without readers: EPIPE and SIGPIPE", fails(write(w, "x", 1), EPIPE) && sigpipes == 1);
    check("... poll: POLLERR", revents(w, POLLOUT) & POLLERR);
    close(w);
    signal(SIGPIPE, SIG_DFL);

    /* select and epoll. */
    r = open(p, O_RDONLY | O_NONBLOCK);
    w = open(p, O_WRONLY | O_NONBLOCK);
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN, .data.fd = r}, got;
    check("epoll takes a FIFO", epoll_ctl(ep, EPOLL_CTL_ADD, r, &ev) == 0 && epoll_wait(ep, &got, 1, 0) == 0);
    write(w, "e", 1);
    check("... and sees its data", epoll_wait(ep, &got, 1, 1000) == 1 && got.data.fd == r && (got.events & EPOLLIN));
    fd_set rs, ws;
    FD_ZERO(&rs);
    FD_ZERO(&ws);
    FD_SET(r, &rs);
    FD_SET(w, &ws);
    struct timeval tv = {0, 0};
    check("select: readable and writable", select((r > w ? r : w) + 1, &rs, &ws, NULL, &tv) == 2 && FD_ISSET(r, &rs) && FD_ISSET(w, &ws));
    reads(r, "e");
    close(ep);

    /* The times a write and a read move, and inotify. */
    int in = inotify_init1(IN_NONBLOCK);
    inotify_add_watch(in, p, IN_OPEN | IN_ACCESS | IN_MODIFY | IN_CLOSE_WRITE | IN_CLOSE_NOWRITE);
    stat(p, &st);
    sleep_ms(1100);
    write(w, "tick", 4);
    struct stat after;
    check("a write moves mtime and ctime", stat(p, &after) == 0 && after.st_mtime > st.st_mtime && after.st_ctime > st.st_ctime);
    sleep_ms(1100);
    reads(r, "tick");
    struct stat read_after;
    check("a read moves atime (relatime: after the write)", stat(p, &read_after) == 0 && read_after.st_atime > after.st_mtime);
    int rw2 = open(p, O_RDWR);
    close(rw2);
    char evbuf[1024] __attribute__((aligned(8)));
    n = read(in, evbuf, sizeof evbuf);
    unsigned seen = 0;
    for (char *e = evbuf; n > 0 && e < evbuf + n; e += sizeof(struct inotify_event) + ((struct inotify_event *)e)->len)
        seen |= ((struct inotify_event *)e)->mask;
    check("inotify: IN_MODIFY, IN_ACCESS, IN_OPEN, IN_CLOSE_WRITE", (seen & (IN_MODIFY | IN_ACCESS | IN_OPEN | IN_CLOSE_WRITE)) == (IN_MODIFY | IN_ACCESS | IN_OPEN | IN_CLOSE_WRITE));
    close(in);

    /* Removed while open: the pipe goes on. */
    check("unlink while open", unlink(p) == 0 && write(w, "still", 5) == 5 && reads(r, "still"));
    snprintf(proc, sizeof proc, "/proc/self/fd/%d", r);
    n = readlink(proc, buf, sizeof buf - 1);
    buf[n > 0 ? n : 0] = 0;
    check("... /proc/self/fd/N says (deleted)", n > 0 && strstr(buf, " (deleted)") != NULL);
    close(r);
    close(w);
    check("... a new FIFO of the name is another pipe", mkfifo(p, 0600) == 0 && (r = open(p, O_RDONLY | O_NONBLOCK)) >= 0 && read(r, buf, 8) == 0);
    close(r);
}

static void on(const char *dir) {
    char d[96], p[160];
    snprintf(where, sizeof where, "%s", dir);
    snprintf(d, sizeof d, "%s/fifotest.%d", dir, getpid());
    mkdir(d, 0755);
    nodes(d);
    fifos(d);
    const char *names[] = {"fifo", "reg0", "reg", "null", "zero", "sda", "sock"};
    for (unsigned i = 0; i < sizeof names / sizeof *names; i++) {
        snprintf(p, sizeof p, "%s/%s", d, names[i]);
        unlink(p);
    }
    check("everything removed", rmdir(d) == 0);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    signal(SIGALRM, on_watchdog);
    alarm(120);
    on("/tmp");
    on("/data");

    snprintf(where, sizeof where, "other");
    check("mknod in /proc: ENOENT (its lookup fails a new name)", fails(mknod("/proc/fifotest", S_IFIFO | 0600, 0), ENOENT));
    check("... of an existing name: EEXIST", fails(mknod("/proc/self", S_IFIFO | 0600, 0), EEXIST));
    check("mknod in /sys: EPERM, a regular file EACCES",
          fails(mknod("/sys/fifotest", S_IFIFO | 0600, 0), EPERM) && fails(mknod("/sys/fifotest", S_IFREG | 0600, 0), EACCES));
    check("mknod in devpts: EPERM, a regular file EACCES",
          fails(mknod("/dev/pts/fifotest", S_IFIFO | 0600, 0), EPERM) && fails(mknod("/dev/pts/fifotest", 0600, 0), EACCES));
    check("mknod of a mount point: EEXIST", fails(mknod("/data", S_IFIFO | 0600, 0), EEXIST));
    struct stat st;
    check("mknod in /dev (devtmpfs, root may)",
          mknod("/dev/fifotest-null", S_IFCHR | 0666, makedev(1, 3)) == 0 && stat("/dev/fifotest-null", &st) == 0 && st.st_rdev == makedev(1, 3) &&
              unlink("/dev/fifotest-null") == 0);
    printf("fifotest: %s\n", failures ? "FAILED" : "all passed");
    return failures ? 1 : 0;
}
