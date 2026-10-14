/* The Linux server out of memory: a failed allocation a program drives is
 * answered (ENOMEM, ENOBUFS for a send, EAGAIN where Linux says so) and
 * leaves nothing behind, instead of breaking the whole tree.
 *
 * - Fail-nth sweeps (TEST_FAIL_NTH, as Linux's /proc/thread-self/fail-nth):
 *   each call is run with its 1st, 2nd, ... fallible allocation in the
 *   server failing, until it makes fewer: every failure must be the call's
 *   error with the state as before the call, then the call succeeds.
 * - Real exhaustion: with all commitment taken by a mapping, socket queues
 *   and pipes are filled until the server's heap has nothing left beyond
 *   its instance's reserve: sends fail with ENOBUFS, pipe writes with
 *   ENOMEM, the server's own allocations are still served (from the
 *   reserve, which no program could take), and everything works again
 *   once the memory is back. */
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
#include <sys/eventfd.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define MIB (1024L * 1024)
#define TEST_ALLOC 1509
#define TEST_HEAP_STATS 1526
#define TEST_FAIL_NTH 1527

static int failures;

static void check(const char *name, int ok) {
    printf("%-64s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

/* Arms the calling thread's n-th fallible allocation to fail (0: disarms);
 * what was left of the previous count (0: it fired). */
static long fail_nth(long n) {
    return syscall(TEST_FAIL_NTH, n);
}

/* How many descriptors are open below 256. */
static int open_fds(void) {
    int n = 0;
    for (int fd = 0; fd < 256; fd++) n += fcntl(fd, F_GETFD) >= 0;
    return n;
}

/* A call under test: `op` returns < 0 with errno set on failure; after a
 * failure `unchanged` says whether the state is as before (NULL: nothing to
 * check), after a success `undo` puts it back (NULL: nothing to undo). */
struct sweep {
    const char *name;
    long (*op)(void);
    int err;
    int (*unchanged)(void);
    void (*undo)(void);
    /* Another error Linux answers for one of its allocations (0: none). */
    int err2;
};

/* Fails the call's allocations one after the other: every failure must be
 * `err` with the state unchanged, and the call must succeed once it makes
 * fewer allocations than the one armed. */
static void sweep(const struct sweep *s) {
    int fired = 0, bad = 0, n;
    for (n = 1; n < 4096; n++) {
        fail_nth(n);
        errno = 0;
        long r = s->op();
        int e = errno;
        long left = fail_nth(0);
        if (left != 0) {
            /* Fewer allocations than n: the call must have succeeded. */
            if (r < 0) {
                printf("  %s: failed (%s) with no allocation failed\n", s->name, strerror(e));
                bad++;
            } else if (s->undo) {
                s->undo();
            }
            break;
        }
        fired++;
        if (r >= 0) {
            /* An allocation it could do without (a fallback): fine. */
            if (s->undo) s->undo();
            continue;
        }
        if (e != s->err && (s->err2 == 0 || e != s->err2)) {
            printf("  %s: allocation %d failed: %s, not %s\n", s->name, n, strerror(e), strerror(s->err));
            bad++;
        } else if (s->unchanged && !s->unchanged()) {
            printf("  %s: allocation %d failed: state left changed\n", s->name, n);
            bad++;
        }
    }
    char line[128];
    snprintf(line, sizeof line, "%s: %d allocations failed in turn", s->name, fired);
    check(line, fired > 0 && bad == 0 && n < 4096);
}

/* --- descriptors ------------------------------------------------------- */

static int fds_before;
static int pair[2];

static int same_fds(void) {
    return open_fds() == fds_before;
}

static long op_pipe(void) {
    fds_before = open_fds();
    return pipe2(pair, O_CLOEXEC);
}

static void close_pair(void) {
    close(pair[0]);
    close(pair[1]);
}

static long op_socketpair(void) {
    fds_before = open_fds();
    return socketpair(AF_UNIX, SOCK_STREAM, 0, pair);
}

static long op_eventfd(void) {
    fds_before = open_fds();
    return pair[0] = eventfd(0, EFD_CLOEXEC);
}

static long op_epoll_create(void) {
    fds_before = open_fds();
    return pair[0] = epoll_create1(EPOLL_CLOEXEC);
}

static long op_inotify_init(void) {
    fds_before = open_fds();
    return pair[0] = inotify_init1(IN_CLOEXEC);
}

static void close_first(void) {
    close(pair[0]);
}

static long op_dup_high(void) {
    fds_before = open_fds();
    return dup2(0, 200);
}

static void close_high(void) {
    close(200);
}

/* --- queues ------------------------------------------------------------ */

static int qa, qb;
static char payload[16384];

static int nothing_queued(void) {
    int n = -1;
    return ioctl(qb, FIONREAD, &n) == 0 && n == 0;
}

static void drain(void) {
    char buf[65536];
    while (recv(qb, buf, sizeof buf, MSG_DONTWAIT) > 0) {
    }
}

static void drain_pipe(void) {
    char buf[65536];
    while (read(qb, buf, sizeof buf) > 0) {
    }
}

/* (A pipe's read end, nonblocking.) */
static int pipe_empty(void) {
    char byte;
    return read(qb, &byte, 1) < 0 && errno == EAGAIN;
}

static long op_recv_queued(void) {
    char buf[100];
    return recv(qb, buf, sizeof buf, MSG_DONTWAIT);
}

static int all_still_queued(void) {
    int n = -1;
    return ioctl(qb, FIONREAD, &n) == 0 && n == 100;
}

static void requeue(void) {
    send(qa, payload, 100, 0);
}

static long op_send(void) {
    return send(qa, payload, sizeof payload, MSG_DONTWAIT);
}

static long op_write(void) {
    return write(qa, payload, sizeof payload);
}

static int passed_fd;

static long op_send_rights(void) {
    char byte = 'x';
    struct iovec iov = {&byte, 1};
    char cbuf[CMSG_SPACE(sizeof(int))];
    struct msghdr m = {0};
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    m.msg_control = cbuf;
    m.msg_controllen = sizeof cbuf;
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &passed_fd, sizeof(int));
    return sendmsg(qa, &m, MSG_DONTWAIT);
}

static void drain_rights(void) {
    char byte;
    struct iovec iov = {&byte, 1};
    char cbuf[CMSG_SPACE(sizeof(int))];
    struct msghdr m = {0};
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    m.msg_control = cbuf;
    m.msg_controllen = sizeof cbuf;
    if (recvmsg(qb, &m, MSG_DONTWAIT) == 1) {
        struct cmsghdr *c = CMSG_FIRSTHDR(&m);
        if (c && c->cmsg_type == SCM_RIGHTS) {
            int fd;
            memcpy(&fd, CMSG_DATA(c), sizeof fd);
            close(fd);
        }
    }
}

static void queues(void) {
    memset(payload, 'q', sizeof payload);
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    qa = sv[0];
    qb = sv[1];
    sweep(&(struct sweep){"stream send", op_send, ENOBUFS, nothing_queued, drain});
    passed_fd = open("/dev/null", O_RDONLY);
    /* (ENOMEM for the descriptors' list, as Linux's scm_fp_copy.) */
    sweep(&(struct sweep){"SCM_RIGHTS sendmsg", op_send_rights, ENOBUFS, nothing_queued, drain_rights, ENOMEM});
    requeue();
    sweep(&(struct sweep){"recv of queued data", op_recv_queued, ENOMEM, all_still_queued, requeue});
    drain();
    close(passed_fd);
    close(sv[0]);
    close(sv[1]);
    socketpair(AF_UNIX, SOCK_DGRAM, 0, sv);
    qa = sv[0];
    qb = sv[1];
    sweep(&(struct sweep){"datagram send", op_send, ENOBUFS, nothing_queued, drain});
    close(sv[0]);
    close(sv[1]);
    int p[2];
    pipe2(p, O_NONBLOCK);
    qa = p[1];
    qb = p[0];
    sweep(&(struct sweep){"pipe write", op_write, ENOMEM, pipe_empty, drain_pipe});
    close(p[0]);
    close(p[1]);
}

/* --- socket names ------------------------------------------------------ */

static int named, listener;
static struct sockaddr_un where;

static long op_bind(void) {
    return bind(named, (struct sockaddr *)&where, sizeof where);
}

/* Not bound: the name is free and no socket file is there. */
static int unbound(void) {
    struct sockaddr_un got = {0};
    socklen_t len = sizeof got;
    getsockname(named, (struct sockaddr *)&got, &len);
    return len == sizeof(sa_family_t) && (where.sun_path[0] == 0 || access(where.sun_path, F_OK) < 0);
}

static void unbind(void) {
    close(named);
    if (where.sun_path[0]) unlink(where.sun_path);
    named = socket(AF_UNIX, SOCK_STREAM, 0);
}

static int client;

static long op_connect(void) {
    client = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
    if (client < 0) return -1;
    long r = connect(client, (struct sockaddr *)&where, sizeof where);
    if (r < 0) {
        int e = errno;
        close(client);
        errno = e;
    }
    return r;
}

/* Nothing pending at the listener. */
static int none_pending(void) {
    int fd = accept4(listener, NULL, NULL, SOCK_NONBLOCK);
    if (fd >= 0) close(fd);
    return fd < 0 && errno == EAGAIN;
}

static void disconnect(void) {
    close(client);
    int fd = accept4(listener, NULL, NULL, SOCK_NONBLOCK);
    if (fd >= 0) close(fd);
}

static void names(void) {
    named = socket(AF_UNIX, SOCK_STREAM, 0);
    where = (struct sockaddr_un){AF_UNIX, "/tmp/nomem.sock"};
    sweep(&(struct sweep){"bind to a path", op_bind, ENOMEM, unbound, unbind});
    where = (struct sockaddr_un){AF_UNIX, "\0nomem.abstract"};
    sweep(&(struct sweep){"bind to an abstract name", op_bind, ENOMEM, unbound, unbind});
    close(named);
    listener = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
    where = (struct sockaddr_un){AF_UNIX, "/tmp/nomem.listen"};
    bind(listener, (struct sockaddr *)&where, sizeof where);
    listen(listener, 8);
    sweep(&(struct sweep){"connect", op_connect, ENOMEM, none_pending, disconnect});
    close(listener);
    unlink(where.sun_path);
}

/* --- terminals --------------------------------------------------------- */

static int master, slave;

static long op_pty_write(void) {
    return write(slave, "abc", 3);
}

static int master_empty(void) {
    int n = -1;
    return ioctl(master, FIONREAD, &n) == 0 && n == 0;
}

static void drain_master(void) {
    char buf[256];
    while (read(master, buf, sizeof buf) > 0) {
    }
}

static long op_ptmx(void) {
    fds_before = open_fds();
    return pair[0] = posix_openpt(O_RDWR | O_NOCTTY);
}

static void terminals(void) {
    sweep(&(struct sweep){"open /dev/ptmx", op_ptmx, ENOMEM, same_fds, close_first});
    master = posix_openpt(O_RDWR | O_NOCTTY | O_NONBLOCK);
    grantpt(master);
    unlockpt(master);
    slave = open(ptsname(master), O_RDWR | O_NOCTTY);
    sweep(&(struct sweep){"write to a pty", op_pty_write, ENOMEM, master_empty, drain_master});
    close(slave);
    close(master);
}

/* --- signals ----------------------------------------------------------- */

static long op_sigqueue(void) {
    siginfo_t si = {0};
    si.si_signo = SIGRTMIN;
    si.si_code = SI_QUEUE;
    si.si_pid = getpid();
    si.si_value.sival_int = 42;
    return syscall(SYS_rt_sigqueueinfo, getpid(), SIGRTMIN, &si);
}

static int none_pending_signal(void) {
    sigset_t set;
    sigpending(&set);
    return !sigismember(&set, SIGRTMIN);
}

static void take_signal(void) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGRTMIN);
    siginfo_t si;
    struct timespec zero = {0, 0};
    sigtimedwait(&set, &si, &zero);
}

static void signals(void) {
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGRTMIN);
    sigprocmask(SIG_BLOCK, &set, NULL);
    sweep(&(struct sweep){"rt_sigqueueinfo", op_sigqueue, EAGAIN, none_pending_signal, take_signal});
    sigprocmask(SIG_UNBLOCK, &set, NULL);
}

/* --- watch lists ------------------------------------------------------- */

static int ep, watched;

static int not_watched(void) {
    struct epoll_event ev = {EPOLLIN, {0}};
    return epoll_ctl(ep, EPOLL_CTL_MOD, watched, &ev) < 0 && errno == ENOENT;
}

static long op_epoll_add(void) {
    struct epoll_event ev = {EPOLLIN, {0}};
    return epoll_ctl(ep, EPOLL_CTL_ADD, watched, &ev);
}

static void epoll_del(void) {
    epoll_ctl(ep, EPOLL_CTL_DEL, watched, NULL);
}

static struct pollfd many[64];

static long op_poll(void) {
    return poll(many, 64, 0);
}

static int ino, wd;

static long op_inotify_add(void) {
    return wd = inotify_add_watch(ino, "/tmp", IN_CREATE);
}

static int no_watch(void) {
    /* A watch on /tmp would get this event. */
    int fd = open("/tmp/nomem.watch", O_CREAT | O_RDONLY, 0644);
    close(fd);
    unlink("/tmp/nomem.watch");
    int n = -1;
    return ioctl(ino, FIONREAD, &n) == 0 && n == 0;
}

static void inotify_rm(void) {
    inotify_rm_watch(ino, wd);
    char buf[4096];
    while (read(ino, buf, sizeof buf) > 0) {
    }
}

static void watch_lists(void) {
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    ep = epoll_create1(0);
    watched = sv[0];
    sweep(&(struct sweep){"epoll_ctl ADD", op_epoll_add, ENOMEM, not_watched, epoll_del});
    for (int i = 0; i < 64; i++) many[i] = (struct pollfd){sv[i % 2], POLLIN, 0};
    sweep(&(struct sweep){"poll of 64 descriptors", op_poll, ENOMEM, NULL, NULL});
    ino = inotify_init1(IN_NONBLOCK);
    sweep(&(struct sweep){"inotify_add_watch", op_inotify_add, ENOMEM, no_watch, inotify_rm});
    close(ino);
    close(ep);
    close(sv[0]);
    close(sv[1]);
}

/* --- descriptors (sweeps) ---------------------------------------------- */

static void descriptors(void) {
    sweep(&(struct sweep){"pipe2", op_pipe, ENOMEM, same_fds, close_pair});
    sweep(&(struct sweep){"socketpair", op_socketpair, ENOMEM, same_fds, close_pair});
    sweep(&(struct sweep){"eventfd", op_eventfd, ENOMEM, same_fds, close_first});
    sweep(&(struct sweep){"epoll_create1", op_epoll_create, ENOMEM, same_fds, close_first});
    sweep(&(struct sweep){"inotify_init1", op_inotify_init, ENOMEM, same_fds, close_first});
    sweep(&(struct sweep){"dup2 to a high descriptor", op_dup_high, ENOMEM, same_fds, close_high});
}

/* --- files ------------------------------------------------------------- */

static int no_file(void) {
    return access("/tmp/nomem.file", F_OK) < 0 && errno == ENOENT;
}

static long op_create(void) {
    long fd = open("/tmp/nomem.file", O_CREAT | O_EXCL | O_WRONLY, 0644);
    if (fd >= 0) close(fd);
    return fd;
}

static void rm_file(void) {
    unlink("/tmp/nomem.file");
}

static long op_mkdir(void) {
    return mkdir("/tmp/nomem.file", 0755);
}

static void rm_dir(void) {
    rmdir("/tmp/nomem.file");
}

static long op_symlink(void) {
    return symlink("/a/target/of/some/length", "/tmp/nomem.file");
}

static const char *create_path = "/tmp/nomem.file";

static int not_created(void) {
    return access(create_path, F_OK) < 0 && errno == ENOENT;
}

static long op_create_at(void) {
    long fd = open(create_path, O_CREAT | O_EXCL | O_RDWR, 0644);
    if (fd >= 0) close(fd);
    return fd;
}

static void rm_created(void) {
    unlink(create_path);
}

/* rename of a directory with a subtree: both names as they were. */
static long op_rename(void) {
    return rename("/tmp/nomem.from", "/tmp/nomem.to");
}

static int not_renamed(void) {
    return access("/tmp/nomem.from/a/b", F_OK) == 0 && access("/tmp/nomem.to", F_OK) < 0;
}

static void rename_back(void) {
    rename("/tmp/nomem.to", "/tmp/nomem.from");
}

static char cwd_before[256];

static long op_chdir(void) {
    return chdir("/tmp/nomem.from/a");
}

static int same_cwd(void) {
    char now[256];
    return getcwd(now, sizeof now) && strcmp(now, cwd_before) == 0;
}

static void chdir_back(void) {
    chdir(cwd_before);
}

static long op_getcwd(void) {
    char buf[256];
    return getcwd(buf, sizeof buf) ? 0 : -1;
}

static long op_readlink(void) {
    char buf[256];
    return readlink("/proc/self/fd/0", buf, sizeof buf);
}

static long op_read_proc(void) {
    char buf[4096];
    int fd = open("/proc/self/status", O_RDONLY);
    if (fd < 0) return -1;
    long n = read(fd, buf, sizeof buf);
    int e = errno;
    close(fd);
    errno = e;
    return n;
}

static int dirfd_many;

static long op_getdents(void) {
    char buf[1024];
    lseek(dirfd_many, 0, SEEK_SET);
    return syscall(SYS_getdents64, dirfd_many, buf, sizeof buf);
}

static void files(void) {
    sweep(&(struct sweep){"create a tmpfs file", op_create, ENOMEM, no_file, rm_file});
    sweep(&(struct sweep){"mkdir on tmpfs", op_mkdir, ENOMEM, no_file, rm_dir});
    sweep(&(struct sweep){"symlink on tmpfs", op_symlink, ENOMEM, no_file, rm_file});
    create_path = "/data/nomem.file";
    unlink(create_path);
    sweep(&(struct sweep){"create a /data file", op_create_at, ENOMEM, not_created, rm_created});
    mkdir("/tmp/nomem.from", 0755);
    mkdir("/tmp/nomem.from/a", 0755);
    mkdir("/tmp/nomem.from/a/b", 0755);
    sweep(&(struct sweep){"rename a tmpfs directory", op_rename, ENOMEM, not_renamed, rename_back});
    getcwd(cwd_before, sizeof cwd_before);
    sweep(&(struct sweep){"chdir", op_chdir, ENOMEM, same_cwd, chdir_back});
    chdir("/tmp/nomem.from/a");
    sweep(&(struct sweep){"getcwd", op_getcwd, ENOMEM, NULL, NULL});
    chdir(cwd_before);
    rmdir("/tmp/nomem.from/a/b");
    rmdir("/tmp/nomem.from/a");
    rmdir("/tmp/nomem.from");
    sweep(&(struct sweep){"readlink of /proc/self/fd/0", op_readlink, ENOMEM, NULL, NULL});
    sweep(&(struct sweep){"read of /proc/self/status", op_read_proc, ENOMEM, NULL, NULL});
    mkdir("/tmp/nomem.dir", 0755);
    char path[64];
    for (int i = 0; i < 100; i++) {
        snprintf(path, sizeof path, "/tmp/nomem.dir/entry-%03d", i);
        close(open(path, O_CREAT | O_WRONLY, 0644));
    }
    dirfd_many = open("/tmp/nomem.dir", O_RDONLY | O_DIRECTORY);
    sweep(&(struct sweep){"getdents64 of a tmpfs directory", op_getdents, ENOMEM, NULL, NULL});
    close(dirfd_many);
    for (int i = 0; i < 100; i++) {
        snprintf(path, sizeof path, "/tmp/nomem.dir/entry-%03d", i);
        unlink(path);
    }
    rmdir("/tmp/nomem.dir");
}

/* --- processes --------------------------------------------------------- */

static int no_child(void) {
    return waitpid(-1, NULL, WNOHANG) < 0 && errno == ECHILD;
}

static long op_fork(void) {
    pid_t p = fork();
    if (p == 0) _exit(0);
    return p;
}

static void reap(void) {
    while (waitpid(-1, NULL, 0) > 0) {
    }
}

/* execve with large arguments, in a child: its exit status says how it went
 * (0: /bin/true ran, 2: ENOMEM with the allocation failed, 3: otherwise). */
static void exec_sweep(void) {
    static char big[64 * 1024];
    memset(big, 'a', sizeof big - 1);
    char *argv[] = {"true", big, big, NULL};
    int fired = 0, bad = 0, n;
    for (n = 1; n < 4096; n++) {
        pid_t p = fork();
        if (p == 0) {
            fail_nth(n);
            execve("/bin/true", argv, (char *[]){"A=1", NULL});
            int e = errno;
            long left = fail_nth(0);
            _exit(left == 0 && e == ENOMEM ? 2 : 3);
        }
        int status = 0;
        waitpid(p, &status, 0);
        if (!WIFEXITED(status) || WEXITSTATUS(status) == 3 || (WEXITSTATUS(status) != 0 && WEXITSTATUS(status) != 2)) {
            printf("  execve: allocation %d: status %#x\n", n, status);
            bad++;
            break;
        }
        if (WEXITSTATUS(status) == 0) break;
        fired++;
    }
    char line[128];
    snprintf(line, sizeof line, "execve with large arguments: %d allocations failed in turn", fired);
    check(line, fired > 0 && bad == 0 && n < 4096);
}

static void processes(void) {
    sweep(&(struct sweep){"fork", op_fork, ENOMEM, no_child, reap});
    exec_sweep();
}

/* --- real exhaustion --------------------------------------------------- */

static long meminfo(const char *key) {
    char text[4096] = {0};
    int fd = open("/proc/meminfo", O_RDONLY);
    read(fd, text, sizeof text - 1);
    close(fd);
    char *p = strstr(text, key);
    return p ? strtol(p + strlen(key) + 1, NULL, 10) : -1;
}

struct heap { unsigned long committed, in_use, free, decommitted, shrinks; };

static struct heap server_heap(void) {
    struct heap h = {0};
    syscall(TEST_HEAP_STATS, &h);
    return h;
}

#define PAIRS 160
#define PIPES 64

static void exhaustion(void) {
    static int sock[PAIRS][2], pipes[PIPES][2];
    static char chunk[32 * 1024];
    memset(chunk, 'm', sizeof chunk);
    int made = 1;
    for (int i = 0; i < PAIRS; i++) {
        made &= socketpair(AF_UNIX, SOCK_STREAM, 0, sock[i]) == 0;
        fcntl(sock[i][0], F_SETFL, O_NONBLOCK);
    }
    for (int i = 0; i < PIPES; i++) made &= pipe2(pipes[i], O_NONBLOCK) == 0;
    check("exhaustion: sockets and pipes made", made);
    /* All commitment programs may take, taken (a mapping never touched). */
    long left = (meminfo("CommitLimit:") - meminfo("Committed_AS:")) * 1024;
    void *all = MAP_FAILED;
    for (long len = left; all == MAP_FAILED && len > 0; len -= 64 * 1024) {
        all = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (all != MAP_FAILED) left = len;
    }
    /* Queues filled until the server's heap refuses. */
    long queued = 0;
    int send_err = 0, write_err = 0;
    for (int i = 0; i < PAIRS && !send_err; i++) {
        for (;;) {
            ssize_t n = send(sock[i][0], chunk, sizeof chunk, MSG_DONTWAIT);
            if (n > 0) {
                queued += n;
                continue;
            }
            if (errno != EAGAIN) send_err = errno;
            break;
        }
    }
    for (int i = 0; i < PIPES && !write_err; i++) {
        for (;;) {
            ssize_t n = write(pipes[i][1], chunk, sizeof chunk);
            if (n > 0) {
                queued += n;
                continue;
            }
            if (errno != EAGAIN) write_err = errno;
            break;
        }
    }
    struct heap h = server_heap();
    /* The server's own allocations (TEST_ALLOC: about 1.4 MiB of blocks for
     * a moment) are served from the reserve the programs could not take. */
    long served = syscall(TEST_ALLOC, 60);
    /* (Memory back before anything is said: the terminal's buffer is the
     * server's too.) */
    if (all != MAP_FAILED) munmap(all, left);
    printf("exhaustion: %ld kB taken, %ld kB queued, send %s, pipe write %s, heap %lu kB in use, %lu kB free\n",
           all == MAP_FAILED ? 0 : left / 1024, queued / 1024, strerror(send_err), strerror(write_err), h.in_use / 1024,
           h.free / 1024);
    check("exhaustion: a send the heap cannot hold is ENOBUFS", all != MAP_FAILED && send_err == ENOBUFS);
    check("exhaustion: a pipe write it cannot hold is ENOMEM", write_err == ENOMEM);
    check("exhaustion: the server's own allocations are still served", served == 0);
    /* The tree goes on: everything works again. */
    for (int i = 0; i < PAIRS; i++) {
        close(sock[i][0]);
        close(sock[i][1]);
    }
    for (int i = 0; i < PIPES; i++) {
        close(pipes[i][0]);
        close(pipes[i][1]);
    }
    int sv[2];
    char back[sizeof chunk];
    int ok = socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0 && send(sv[0], chunk, sizeof chunk, 0) == sizeof chunk &&
             recv(sv[1], back, sizeof back, MSG_WAITALL) == sizeof back && memcmp(back, chunk, sizeof back) == 0;
    close(sv[0]);
    close(sv[1]);
    pid_t p = fork();
    if (p == 0) _exit(7);
    int status = 0;
    waitpid(p, &status, 0);
    check("exhaustion: afterwards sends and forks work again", ok && WIFEXITED(status) && WEXITSTATUS(status) == 7);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (fail_nth(0) < 0) {
        printf("nomemtest: TEST_FAIL_NTH not available (%s)\n", strerror(errno));
        return 1;
    }
    descriptors();
    queues();
    names();
    terminals();
    signals();
    watch_lists();
    files();
    processes();
    exhaustion();
    printf("nomemtest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
