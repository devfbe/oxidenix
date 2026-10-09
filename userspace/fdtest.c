/* The descriptor table, the Linux server's (phase R6e): dup, dup2, dup3 and
 * fcntl's duplicates, close-on-exec, status flags shared by an open file
 * description's descriptors, close_range (with CLOSE_RANGE_CLOEXEC and
 * CLOSE_RANGE_UNSHARE), RLIMIT_NOFILE, tables copied by fork, shared by
 * threads and given up by execve and exit, a call keeping its description
 * while another thread closes the descriptor, and the kernel's own files
 * (/proc, /dev/null) held by the server: read, lseek, fstat, mmap, fchdir,
 * poll. Run as `fdtest exec-check FD FD`, it reports which of two
 * descriptors an execve left open. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <signal.h>
#include <sys/socket.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef CLOSE_RANGE_UNSHARE
#define CLOSE_RANGE_UNSHARE 2
#define CLOSE_RANGE_CLOEXEC 4
#endif

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static int is_open(int fd) {
    return fcntl(fd, F_GETFD) != -1;
}

static void sleep_ms(long ms) {
    struct timespec d = {ms / 1000, (ms % 1000) * 1000000};
    nanosleep(&d, NULL);
}

static long close_range_(unsigned first, unsigned last, unsigned flags) {
    return syscall(436, first, last, flags);
}

/* A thread that opens a descriptor in the shared table. */
static void *open_in_thread(void *arg) {
    *(int *)arg = open("/dev/null", O_RDONLY);
    return NULL;
}

/* A thread that unshares its table and closes descriptor 3.. in its own. */
static void *unshare_and_close(void *arg) {
    int fd = *(int *)arg;
    long r = close_range_(fd, fd, CLOSE_RANGE_UNSHARE);
    *(int *)arg = r == 0 && !is_open(fd);
    return NULL;
}

/* A thread blocked reading a pipe whose descriptor another thread closes. */
struct blocked {
    int fd;
    long got;
    char byte;
};

static void *read_blocked(void *arg) {
    struct blocked *b = arg;
    b->got = read(b->fd, &b->byte, 1);
    return NULL;
}

static void basics(void) {
    int p[2];
    pipe(p);
    int d = dup(p[0]);
    check("dup takes the lowest free descriptor", d == p[1] + 1);
    close(d);
    check("dup2 onto itself returns it", dup2(p[0], p[0]) == p[0]);
    check("dup3 onto itself is EINVAL", dup3(p[0], p[0], 0) == -1 && errno == EINVAL);
    check("... also for a closed descriptor (before EBADF)", dup3(500, 500, 0) == -1 && errno == EINVAL);
    check("dup3 with a flag but O_CLOEXEC is EINVAL", dup3(p[0], 20, O_NONBLOCK) == -1 && errno == EINVAL);
    check("dup2 of a closed descriptor is EBADF", dup2(500, 20) == -1 && errno == EBADF);
    check("dup2 replaces an open descriptor", dup2(p[1], 20) == 20 && dup2(p[0], 20) == 20);
    char c = 'x';
    write(p[1], &c, 1);
    check("... which names the new file then", read(20, &c, 1) == 1);
    check("dup3 with O_CLOEXEC sets close-on-exec", dup3(p[0], 21, O_CLOEXEC) == 21 && fcntl(21, F_GETFD) == FD_CLOEXEC);
    check("dup does not copy close-on-exec", (d = dup(21)) >= 0 && fcntl(d, F_GETFD) == 0);
    close(d);
    check("F_DUPFD takes the lowest at or above its argument", fcntl(p[0], F_DUPFD, 30) == 30 && fcntl(p[0], F_DUPFD, 30) == 31);
    check("F_DUPFD_CLOEXEC sets close-on-exec", (d = fcntl(p[0], F_DUPFD_CLOEXEC, 40)) == 40 && fcntl(40, F_GETFD) == FD_CLOEXEC);
    check("FIONCLEX and FIOCLEX", ioctl(40, FIONCLEX) == 0 && fcntl(40, F_GETFD) == 0 && ioctl(40, FIOCLEX) == 0 && fcntl(40, F_GETFD) == FD_CLOEXEC);
    check("F_SETFD", fcntl(40, F_SETFD, 0) == 0 && fcntl(40, F_GETFD) == 0);
    check("close of a closed descriptor is EBADF", close(500) == -1 && errno == EBADF);
    check("an unknown fcntl is EINVAL", fcntl(p[0], 9999) == -1 && errno == EINVAL);

    /* Status flags belong to the description: every descriptor of it sees them. */
    check("F_SETFL on one descriptor ...", fcntl(30, F_SETFL, O_NONBLOCK) == 0);
    check("... shows on its duplicates", (fcntl(p[0], F_GETFL) & O_NONBLOCK) && (fcntl(31, F_GETFL) & O_NONBLOCK));
    check("... and the reads there do not block", read(p[0], &c, 1) == -1 && errno == EAGAIN);
    int on = 0;
    check("FIONBIO turns it off for all of them", ioctl(31, FIONBIO, &on) == 0 && !(fcntl(p[0], F_GETFL) & O_NONBLOCK));
    check("the access mode is kept", (fcntl(p[1], F_GETFL) & O_ACCMODE) == O_WRONLY);
    check("F_SETFL cannot change the access mode", fcntl(p[1], F_SETFL, O_RDWR) == 0 && (fcntl(p[1], F_GETFL) & O_ACCMODE) == O_WRONLY);
    int q[2];
    pipe(q);
    check("another pipe's flags are its own", !(fcntl(q[0], F_GETFL) & O_NONBLOCK) && fcntl(p[0], F_SETFL, O_NONBLOCK) == 0 && !(fcntl(q[0], F_GETFL) & O_NONBLOCK));

    /* close_range. */
    for (int fd = 50; fd < 60; fd++) dup2(q[0], fd);
    check("close_range with CLOSE_RANGE_CLOEXEC marks them", close_range_(50, 54, CLOSE_RANGE_CLOEXEC) == 0 && fcntl(50, F_GETFD) == FD_CLOEXEC && fcntl(54, F_GETFD) == FD_CLOEXEC && fcntl(55, F_GETFD) == 0);
    check("close_range closes a range", close_range_(52, 57, 0) == 0 && is_open(51) && !is_open(52) && !is_open(57) && is_open(58));
    check("close_range to ~0 closes the rest", close_range_(58, ~0u, 0) == 0 && !is_open(58) && !is_open(59));
    check("close_range with first above last is EINVAL", close_range_(5, 4, 0) == -1 && errno == EINVAL);
    check("close_range with an unknown flag is EINVAL", close_range_(50, 51, 1) == -1 && errno == EINVAL);
    int used[] = {20, 21, 30, 31, 40, 50, 51};
    for (unsigned i = 0; i < sizeof used / sizeof used[0]; i++) close(used[i]);
    close(p[0]);
    close(p[1]);
    close(q[0]);
    close(q[1]);
}

static void limits(void) {
    struct rlimit old, rl;
    check("getrlimit(RLIMIT_NOFILE)", getrlimit(RLIMIT_NOFILE, &old) == 0 && old.rlim_cur >= 1024 && old.rlim_cur <= old.rlim_max);
    rl = (struct rlimit){16, old.rlim_max};
    check("a lower soft limit is taken", setrlimit(RLIMIT_NOFILE, &rl) == 0);
    int last = -1, fd;
    while ((fd = open("/dev/null", O_RDONLY)) >= 0) last = fd;
    check("opens beyond it fail with EMFILE", last == 15 && errno == EMFILE);
    check("dup2 beyond it is EBADF", dup2(0, 16) == -1 && errno == EBADF);
    check("F_DUPFD at it is EINVAL", fcntl(0, F_DUPFD, 16) == -1 && errno == EINVAL);
    struct pollfd many[17];
    check("poll with more entries than the limit is EINVAL", poll(many, 17, 0) == -1 && errno == EINVAL);
    for (fd = 3; fd <= last; fd++) close(fd);
    rl = (struct rlimit){old.rlim_max + 1, old.rlim_max};
    check("a soft limit above the hard one is EINVAL", setrlimit(RLIMIT_NOFILE, &rl) == -1 && errno == EINVAL);
    rl = (struct rlimit){RLIM_INFINITY, RLIM_INFINITY};
    check("an unlimited hard limit is EPERM (fs.nr_open)", setrlimit(RLIMIT_NOFILE, &rl) == -1 && errno == EPERM);
    rl = (struct rlimit){8192, 8192};
    check("the hard limit may grow (root)", setrlimit(RLIMIT_NOFILE, &rl) == 0 && dup2(0, 6000) == 6000);
    close(6000);
    check("the old limits back", setrlimit(RLIMIT_NOFILE, &old) == 0 && getrlimit(RLIMIT_NOFILE, &rl) == 0 && rl.rlim_cur == old.rlim_cur);
}

/* The CLONE_VM|CLONE_VFORK|CLONE_FILES child (on a stack of its own):
 * runs `fdtest exec-check vfork_fd 999`. */
static int vfork_fd;
static int vfork_exec(void *arg) {
    (void)arg;
    char a[16];
    snprintf(a, sizeof a, "%d", vfork_fd);
    execl("/bin/fdtest", "fdtest", "exec-check", a, "999", (char *)NULL);
    _exit(9);
}

static void processes(void) {
    int p[2];
    pipe(p);
    /* fork copies the table: each side's later changes are its own. */
    int status;
    pid_t child = fork();
    if (child == 0) {
        int ok = is_open(p[0]) && is_open(p[1]);
        close(p[0]);
        int extra = open("/dev/null", O_RDONLY);
        _exit(ok && extra >= 0 ? 0 : 1);
    }
    waitpid(child, &status, 0);
    check("a forked child has the parent's descriptors", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    check("its close does not reach the parent", is_open(p[0]));
    /* The offset is the description's, shared across fork. */
    int f = open("/tmp/fdtest.offset", O_RDWR | O_CREAT | O_TRUNC, 0600);
    child = fork();
    if (child == 0) {
        write(f, "child", 5);
        _exit(0);
    }
    waitpid(child, NULL, 0);
    check("a forked child shares the file offset", lseek(f, 0, SEEK_CUR) == 5);
    close(f);
    unlink("/tmp/fdtest.offset");

    /* Threads share the table. */
    pthread_t t;
    int opened = -1;
    pthread_create(&t, NULL, open_in_thread, &opened);
    pthread_join(t, NULL);
    check("a thread's open is the process's", opened >= 0 && is_open(opened));
    close(opened);
    /* ... unless one unshares it. */
    int mine = dup(p[0]);
    int arg = mine;
    pthread_create(&t, NULL, unshare_and_close, &arg);
    pthread_join(t, NULL);
    check("close_range(CLOSE_RANGE_UNSHARE) closes in the thread's own copy", arg == 1 && is_open(mine));
    close(mine);

    /* A call holds its description: a read blocked in one thread goes on
     * after another closes the descriptor, and gets the data. */
    struct blocked b = {p[0], -2, 0};
    pthread_create(&t, NULL, read_blocked, &b);
    sleep_ms(50);
    close(p[0]);
    sleep_ms(20);
    write(p[1], "z", 1);
    pthread_join(t, NULL);
    check("a blocked read survives another thread's close", b.got == 1 && b.byte == 'z');
    close(p[1]);

    /* An exit closes the table: the only writer gone is end of file. */
    pipe(p);
    child = fork();
    if (child == 0) {
        close(p[0]);
        sleep_ms(20);
        _exit(0);
    }
    close(p[1]);
    char c;
    check("an exit closes the child's descriptors (end of file)", read(p[0], &c, 1) == 0);
    waitpid(child, NULL, 0);
    close(p[0]);

    /* execve closes the close-on-exec descriptors, and only those. */
    int keep = open("/dev/null", O_RDONLY);
    int gone = open("/dev/null", O_RDONLY | O_CLOEXEC);
    child = fork();
    if (child == 0) {
        char a[16], b2[16];
        snprintf(a, sizeof a, "%d", keep);
        snprintf(b2, sizeof b2, "%d", gone);
        execl("/bin/fdtest", "fdtest", "exec-check", a, b2, (char *)NULL);
        _exit(9);
    }
    waitpid(child, &status, 0);
    check("execve keeps descriptors and closes close-on-exec ones", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    /* A failed execve changes nothing (the kernel refuses a file that is
     * no program after the server handed it the new table). */
    f = open("/tmp/fdtest.notelf", O_WRONLY | O_CREAT | O_TRUNC, 0700);
    write(f, "no program", 10);
    close(f);
    check("a failed execve leaves the table as it was",
          execl("/tmp/fdtest.notelf", "x", (char *)NULL) == -1 && errno == ENOEXEC && is_open(gone) && is_open(keep) && fcntl(gone, F_GETFD) == FD_CLOEXEC);
    unlink("/tmp/fdtest.notelf");
    close(keep);
    close(gone);

    /* A process killed inside execve (its server waits for an argument on
     * a page the pager never supplies: TEST_PAGED_STUCK) still lets its
     * table go, and before its parent learns of the end (Linux's
     * exit_files before exit_notify): the pipe's only writer is gone by the
     * time waitpid returns. */
    pipe(p);
    child = fork();
    if (child == 0) {
        close(p[0]);
        char *stuck = (char *)0x221000000000;
        if (syscall(1507, stuck) != 0) _exit(1);
        char *args[] = {"fdtest", stuck, NULL};
        execv("/bin/fdtest", args);
        _exit(2);
    }
    close(p[1]);
    sleep_ms(200);
    kill(child, SIGKILL);
    waitpid(child, &status, 0);
    struct pollfd hup = {p[0], POLLIN, 0};
    check("a process killed inside execve lets its descriptors go before wait",
          WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL && poll(&hup, 1, 0) == 1 && (hup.revents & POLLHUP));
    close(p[0]);

    /* The same for a process killed while it runs its program (SIGKILL
     * from outside, no handler): its descriptors are closed when waitpid
     * returns, and its port is free. */
    pipe(p);
    int ls = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in la = {.sin_family = AF_INET, .sin_port = htons(47191), .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    int one = 1;
    setsockopt(ls, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    child = fork();
    if (child == 0) {
        close(p[0]);
        if (bind(ls, (struct sockaddr *)&la, sizeof la) != 0 || listen(ls, 1) != 0) _exit(1);
        write(p[1], "r", 1);
        for (volatile unsigned long spin = 0;; spin++) {
        }
    }
    close(ls);
    close(p[1]);
    char r = 0;
    read(p[0], &r, 1);
    kill(child, SIGKILL);
    waitpid(child, &status, 0);
    hup = (struct pollfd){p[0], POLLIN, 0};
    int closed = poll(&hup, 1, 0) == 1 && (hup.revents & POLLHUP);
    int again = socket(AF_INET, SOCK_STREAM, 0);
    setsockopt(again, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    int rebound = bind(again, (struct sockaddr *)&la, sizeof la) == 0 && listen(again, 1) == 0;
    check("a process killed in its program closes its descriptors before wait", r == 'r' && WIFSIGNALED(status) && closed);
    check("... and its listening port is free when wait returns", rebound);
    close(again);
    close(p[0]);

    /* A vfork child sharing the table (CLONE_VFORK|CLONE_FILES): its
     * parent goes on only after the new program's table was made, so a
     * descriptor the parent closes then is still the child's. */
    pipe(p);
    vfork_fd = dup(p[0]);
    static char vfork_stack[16384] __attribute__((aligned(16)));
    child = clone(vfork_exec, vfork_stack + sizeof vfork_stack, CLONE_VM | CLONE_VFORK | CLONE_FILES | SIGCHLD, NULL);
    close(vfork_fd);
    waitpid(child, &status, 0);
    check("a vfork parent sharing the table goes on after the child's copy", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    close(p[0]);
    close(p[1]);

    /* An exec closes a close-on-exec pipe end before the new program runs:
     * the reader sees the end at once. */
    pipe2(p, O_CLOEXEC);
    child = fork();
    if (child == 0) {
        close(p[0]);
        execl("/bin/sleep", "sleep", "2", (char *)NULL);
        _exit(9);
    }
    close(p[1]);
    struct pollfd pf = {p[0], POLLIN, 0};
    check("a close-on-exec write end goes with the execve", poll(&pf, 1, 300) == 1 && (pf.revents & POLLHUP));
    close(p[0]);
    kill(child, SIGKILL);
    waitpid(child, NULL, 0);

    /* ... and so does a close-on-exec socket: the new program can bind its
     * port at once. */
    int s = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
    struct sockaddr_in addr = {.sin_family = AF_INET, .sin_port = htons(47190), .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    int bound = bind(s, (struct sockaddr *)&addr, sizeof addr) == 0 && listen(s, 1) == 0;
    child = fork();
    if (child == 0) {
        /* The parent's descriptor is gone by then: the execve lets go of
         * the last one. */
        sleep_ms(50);
        execl("/bin/fdtest", "fdtest", "bind-check", "47190", (char *)NULL);
        _exit(9);
    }
    close(s);
    waitpid(child, &status, 0);
    check("a close-on-exec socket's port is free for the new program", bound && WIFEXITED(status) && WEXITSTATUS(status) == 0);
}

static void kernel_files(void) {
    /* /proc's files (the server's, always ready). */
    int f = open("/proc/self/stat", O_RDONLY);
    char buf[256];
    check("a /proc file opens and reads", f >= 0 && read(f, buf, sizeof buf) > 0);
    check("lseek on it", lseek(f, 0, SEEK_SET) == 0 && read(f, buf, 4) == 4);
    struct stat st;
    check("fstat on it", fstat(f, &st) == 0 && S_ISREG(st.st_mode));
    int d = dup(f);
    check("its duplicate shares the offset", lseek(f, 2, SEEK_SET) == 2 && lseek(d, 0, SEEK_CUR) == 2);
    struct pollfd pf = {f, POLLIN | POLLOUT, 0};
    check("it is always ready", poll(&pf, 1, 0) == 1 && (pf.revents & (POLLIN | POLLOUT)) == (POLLIN | POLLOUT));
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN};
    check("epoll refuses it (EPERM)", epoll_ctl(ep, EPOLL_CTL_ADD, f, &ev) == -1 && errno == EPERM);
    close(ep);
    check("O_APPEND changes on it", fcntl(f, F_SETFL, O_APPEND) == 0 && (fcntl(f, F_GETFL) & O_APPEND));
    close(d);
    close(f);
    /* /dev: the server's devtmpfs (R9; the kernel's tree before). */
    int dir = open("/dev", O_RDONLY | O_DIRECTORY);
    char cwd[64];
    check("fchdir to /dev", dir >= 0 && fchdir(dir) == 0 && getcwd(cwd, sizeof cwd) && strcmp(cwd, "/dev") == 0);
    check("openat relative to it", (f = openat(dir, "zero", O_RDONLY)) >= 0 && read(f, buf, 5) == 5 && buf[4] == 0);
    close(f);
    chdir("/");
    long n = syscall(SYS_getdents64, dir, buf, sizeof buf);
    check("getdents64 on it", n > 0);
    check("fstat on it", fstat(dir, &st) == 0 && S_ISDIR(st.st_mode));
    close(dir);
    f = open("/dev/null", O_RDONLY);
    pf = (struct pollfd){f, POLLIN, 0};
    check("/dev/null is always ready", poll(&pf, 1, 0) == 1 && pf.revents == POLLIN);
    ep = epoll_create1(0);
    check("epoll refuses it (EPERM)", epoll_ctl(ep, EPOLL_CTL_ADD, f, &ev) == -1 && errno == EPERM);
    close(ep);
    check("fstat: a character device", fstat(f, &st) == 0 && S_ISCHR(st.st_mode));
    d = dup(f);
    check("dup and F_SETFL on it", d >= 0 && fcntl(d, F_SETFL, O_NONBLOCK) == 0 && (fcntl(f, F_GETFL) & O_NONBLOCK));
    close(d);
    close(f);
    int z = open("/dev/zero", O_RDONLY);
    char *m = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, z, 0);
    check("mmap of /dev/zero", m != MAP_FAILED && m[100] == 0);
    munmap(m, 4096);
    close(z);
    int null = open("/dev/null", O_WRONLY);
    check("writes to /dev/null", write(null, buf, 10) == 10);
    close(null);
}

int main(int argc, char **argv) {
    if (argc == 4 && strcmp(argv[1], "exec-check") == 0) {
        return is_open(atoi(argv[2])) && !is_open(atoi(argv[3])) ? 0 : 1;
    }
    if (argc == 3 && strcmp(argv[1], "bind-check") == 0) {
        int s = socket(AF_INET, SOCK_STREAM, 0);
        struct sockaddr_in addr = {.sin_family = AF_INET, .sin_port = htons(atoi(argv[2])), .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
        return bind(s, (struct sockaddr *)&addr, sizeof addr) == 0 && listen(s, 1) == 0 ? 0 : 1;
    }
    basics();
    limits();
    processes();
    kernel_files();
    printf("fdtest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
