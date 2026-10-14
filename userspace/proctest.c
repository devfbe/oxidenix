/* Process information: prctl, capabilities and /proc: procfs's system-wide
 * files and /sys, the Linux server's per-process part (/proc/<pid>, self,
 * thread-self, mounts, the memory figures of statm and status) and its magic
 * links (/proc/self/fd, /proc/<pid>/exe). */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <linux/capability.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <poll.h>
#include <sys/epoll.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/sysinfo.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <pthread.h>
#include <unistd.h>

static int failures;

/* The whole file, NUL-terminated. */
static int slurp(const char *path, char *buf, int size) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    int got = 0, n;
    while (got < size - 1 && (n = read(fd, buf + got, size - 1 - got)) > 0) got += n;
    close(fd);
    buf[got] = 0;
    return got;
}

static int count_fields(const char *s) {
    int n = 0, in = 0;
    for (; *s && *s != '\n'; s++) {
        if (*s != ' ' && !in) n++;
        in = *s != ' ';
    }
    return n;
}

static void check(const char *name, int ok) {
    printf("%-52s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

/* A thread that says its id and waits until a byte arrives. */
static int tid_pipe[2], park_pipe[2];
static void *say_tid(void *arg) {
    long tid = syscall(SYS_gettid);
    write(tid_pipe[1], &tid, sizeof tid);
    char c;
    read(park_pipe[0], &c, 1);
    return arg;
}

/* /proc/<tid> of a thread that is not the main one: its own Pid, the
 * process's Tgid (as Linux, which lists processes only). */
static void thread_dir(void) {
    pipe(tid_pipe);
    pipe(park_pipe);
    pthread_t t;
    pthread_create(&t, NULL, say_tid, NULL);
    long tid = 0;
    read(tid_pipe[0], &tid, sizeof tid);
    char path[64], buf[2048], want_pid[32], want_tgid[32];
    snprintf(path, sizeof path, "/proc/%ld/status", tid);
    snprintf(want_pid, sizeof want_pid, "\nPid:\t%ld\n", tid);
    snprintf(want_tgid, sizeof want_tgid, "\nTgid:\t%d\n", getpid());
    int got = slurp(path, buf, sizeof buf);
    check("/proc/<tid> of a thread: its Pid, the process's Tgid",
          tid != getpid() && got > 0 && strstr(buf, want_pid) && strstr(buf, want_tgid));
    write(park_pipe[1], "x", 1);
    pthread_join(t, NULL);
}

/* A thread that answers what /proc/thread-self is for it. */
static char thread_self[64];
static long thread_tid;
static void *read_thread_self(void *arg) {
    thread_tid = syscall(SYS_gettid);
    readlink("/proc/thread-self", thread_self, sizeof thread_self - 1);
    return arg;
}

/* What the Linux server makes of /proc itself (I/O rings step 5): the
 * mounts, the threads, the working directory, and /proc's rules: nothing
 * can be created, removed, written or chmodded; reads from the start are
 * current, reads further on continue the same contents. */
static void the_servers_part(void) {
    char buf[4096], link[64] = {0}, want[64];
    slurp("/proc/mounts", buf, sizeof buf);
    check("/proc/mounts lists the namespace's mounts",
          strstr(buf, "proc /proc proc rw") && strstr(buf, "sysfs /sys sysfs rw") && strstr(buf, " /data ext2 rw") && strstr(buf, "devpts /dev/pts devpts rw"));
    check("/proc/mounts is a link to self/mounts", readlink("/proc/mounts", link, sizeof link - 1) == 11 && strcmp(link, "self/mounts") == 0);

    pthread_t t[2];
    pthread_create(&t[0], NULL, read_thread_self, NULL);
    pthread_join(t[0], NULL);
    snprintf(want, sizeof want, "%d/task/%ld", getpid(), thread_tid);
    check("/proc/thread-self names the calling thread", strcmp(thread_self, want) == 0);

    pipe(tid_pipe);
    pipe(park_pipe);
    long tids[2] = {0};
    for (int i = 0; i < 2; i++) {
        pthread_create(&t[i], NULL, say_tid, NULL);
        read(tid_pipe[0], &tids[i], sizeof tids[i]);
    }
    int listed = 0, found = 0;
    DIR *task = opendir("/proc/self/task");
    struct dirent *e;
    while (task && (e = readdir(task))) {
        if (e->d_name[0] == '.') continue;
        listed++;
        long tid = atol(e->d_name);
        found += tid == getpid() || tid == tids[0] || tid == tids[1];
    }
    if (task) closedir(task);
    snprintf(want, sizeof want, "/proc/self/task/%ld/stat", tids[1]);
    slurp(want, buf, sizeof buf);
    check("/proc/self/task lists every thread", listed == 3 && found == 3 && atol(buf) == tids[1]);
    for (int i = 0; i < 2; i++) write(park_pipe[1], "x", 1);
    for (int i = 0; i < 2; i++) pthread_join(t[i], NULL);

    char cwd[64] = {0};
    chdir("/tmp");
    readlink("/proc/self/cwd", cwd, sizeof cwd - 1);
    chdir("/");
    check("/proc/self/cwd is the working directory", strcmp(cwd, "/tmp") == 0);

    struct statfs fs;
    check("statfs: /proc is proc, /sys is sysfs", statfs("/proc", &fs) == 0 && fs.f_type == 0x9fa0 && statfs("/sys/devices", &fs) == 0 && fs.f_type == 0x62656572);
    check("/proc makes no names (EPERM, EACCES)", mkdir("/proc/x", 0755) == -1 && errno == EPERM &&
                                                    open("/proc/x", O_RDWR | O_CREAT, 0644) == -1 && errno == EACCES &&
                                                    symlink("x", "/proc/y") == -1 && errno == EPERM);
    check("/proc removes, renames and chmods nothing (EPERM)", unlink("/proc/uptime") == -1 && errno == EPERM &&
                                                                rename("/proc/uptime", "/proc/x") == -1 && errno == EPERM &&
                                                                chmod("/proc/uptime", 0600) == -1 && errno == EPERM);
    check("/proc's files are read-only (EACCES)", open("/proc/uptime", O_WRONLY) == -1 && errno == EACCES);

    /* seq_file's rules: a read from the start is made now, reads further on
     * continue in it; pread at 0 makes it anew. */
    int fd = open("/proc/self/stat", O_RDONLY);
    char whole[1024] = {0}, pieces[1024] = {0};
    int n = pread(fd, whole, sizeof whole - 1, 0), got = 0, k;
    lseek(fd, 0, SEEK_SET);
    while ((k = read(fd, pieces + got, 7)) > 0) got += k;
    check("/proc files read in pieces are one snapshot", n > 0 && got == n && count_fields(pieces) == 52);
    check("/proc files seek from the start, not the end", lseek(fd, 5, SEEK_SET) == 5 && lseek(fd, 0, SEEK_END) == -1 && errno == EINVAL);
    close(fd);
    fd = open("/proc/uptime", O_RDONLY);
    double a = 0, b = 0;
    pread(fd, buf, sizeof buf, 0);
    sscanf(buf, "%lf", &a);
    usleep(50000);
    memset(buf, 0, sizeof buf);
    pread(fd, buf, sizeof buf, 0);
    sscanf(buf, "%lf", &b);
    check("pread at 0 of a /proc file is current", b > a);
    close(fd);
}

/* /proc/self/fd: the descriptors, as magic links. */
static void descriptors(void) {
    char link[128] = {0}, path[64], buf[64] = {0};
    int f = open("/tmp/proc-fd", O_RDWR | O_CREAT | O_TRUNC, 0644);
    write(f, "kept", 4);
    snprintf(path, sizeof path, "/proc/self/fd/%d", f);
    readlink(path, link, sizeof link - 1);
    check("/proc/self/fd/N reads as the file's path", strcmp(link, "/tmp/proc-fd") == 0);
    unlink("/tmp/proc-fd");
    memset(link, 0, sizeof link);
    readlink(path, link, sizeof link - 1);
    int again = open(path, O_RDONLY);
    check("... unlinked: \"(deleted)\", and opening it opens the file",
          strcmp(link, "/tmp/proc-fd (deleted)") == 0 && again >= 0 && read(again, buf, sizeof buf) == 4 && memcmp(buf, "kept", 4) == 0);
    close(again);

    int seen = 0;
    DIR *d = opendir("/proc/self/fd");
    struct dirent *e;
    while (d && (e = readdir(d))) seen += atoi(e->d_name) == f || strcmp(e->d_name, "0") == 0 || strcmp(e->d_name, "2") == 0;
    check("/proc/self/fd lists the descriptors", d != NULL && seen == 3);
    if (d) closedir(d);
    close(f);

    int p[2];
    pipe(p);
    char r[64] = {0}, w[64] = {0};
    snprintf(path, sizeof path, "/proc/self/fd/%d", p[0]);
    readlink(path, r, sizeof r - 1);
    snprintf(path, sizeof path, "/proc/self/fd/%d", p[1]);
    readlink(path, w, sizeof w - 1);
    check("a pipe's ends read as one pipe:[ino]", strncmp(r, "pipe:[", 6) == 0 && strcmp(r, w) == 0);
    int w2 = open(path, O_WRONLY);
    snprintf(path, sizeof path, "/dev/fd/%d", p[0]);
    int r2 = open(path, O_RDONLY);
    check("reopening a pipe's end gives another end", w2 >= 0 && r2 >= 0 && write(w2, "via proc", 8) == 8 && read(r2, buf, 8) == 8 && memcmp(buf, "via proc", 8) == 0);
    close(p[1]);
    write(w2, "y", 1);
    check("... the pipe has a writer while one is open", read(p[0], buf, 1) == 1 && buf[0] == 'y');
    close(w2);
    check("... and none after the last: end of file", read(p[0], buf, 1) == 0);
    close(p[0]);
    close(r2);

    int s[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, s);
    snprintf(path, sizeof path, "/proc/self/fd/%d", s[0]);
    memset(link, 0, sizeof link);
    readlink(path, link, sizeof link - 1);
    check("a socket reads as socket:[ino] and opens ENXIO", strncmp(link, "socket:[", 8) == 0 && open(path, O_RDWR) == -1 && errno == ENXIO);
    close(s[0]);
    close(s[1]);

    f = open("/tmp/proc-opath", O_RDWR | O_CREAT | O_TRUNC, 0644);
    write(f, "opath", 5);
    close(f);
    int o = open("/tmp/proc-opath", O_PATH);
    snprintf(path, sizeof path, "/proc/self/fd/%d", o);
    int rw = open(path, O_RDWR);
    struct stat st;
    check("an O_PATH descriptor opens for real through /proc/self/fd", rw >= 0 && read(rw, buf, 5) == 5 && memcmp(buf, "opath", 5) == 0);
    check("musl's fchmod of it goes through /proc/self/fd", fchmod(o, 0600) == 0 && stat("/tmp/proc-opath", &st) == 0 && (st.st_mode & 0777) == 0600);
    close(rw);
    close(o);
    unlink("/tmp/proc-opath");

    /* A reopened pipe end gets its readiness reports: epoll on it wakes
     * when a writer (another process) feeds the pipe. */
    pipe(p);
    snprintf(path, sizeof path, "/proc/self/fd/%d", p[0]);
    int again_r = open(path, O_RDONLY | O_NONBLOCK);
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN}, got_ev;
    epoll_ctl(ep, EPOLL_CTL_ADD, again_r, &ev);
    pid_t feeder = fork();
    if (feeder == 0) {
        usleep(100000);
        write(p[1], "fed", 3);
        _exit(0);
    }
    int woke = epoll_wait(ep, &got_ev, 1, 3000);
    waitpid(feeder, NULL, 0);
    check("epoll on a reopened pipe end wakes for a writer", woke == 1 && (got_ev.events & EPOLLIN) && read(again_r, buf, 3) == 3);
    write(p[1], "now", 3);
    int r3 = open(path, O_RDONLY | O_NONBLOCK);
    struct pollfd pfd = {r3, POLLIN, 0};
    check("... and one reopened with data waiting is readable at once", poll(&pfd, 1, 0) == 1 && (pfd.revents & POLLIN));
    close(r3);
    close(ep);
    close(again_r);
    close(p[0]);
    close(p[1]);

    /* An O_PATH descriptor through a magic link keeps nothing alive: 130
     * inotify instances (the limit is 128) each closed after one. */
    int opaths[130], made = 0;
    for (int i = 0; i < 130; i++) {
        int in = inotify_init();
        if (in < 0) break;
        snprintf(path, sizeof path, "/proc/self/fd/%d", in);
        opaths[i] = open(path, O_PATH);
        close(in);
        made++;
    }
    check("an O_PATH of an inotify instance keeps it from ending: no", made == 130);
    for (int i = 0; i < made; i++) close(opaths[i]);

    /* A symlink a magic link leads to is no file to open: ELOOP. */
    symlink("/tmp/nowhere", "/tmp/proc-link");
    int l = open("/tmp/proc-link", O_PATH | O_NOFOLLOW);
    snprintf(path, sizeof path, "/proc/self/fd/%d", l);
    check("open of a symlink through /proc/self/fd: ELOOP", open(path, O_RDONLY) == -1 && errno == ELOOP);
    close(l);
    unlink("/tmp/proc-link");

    /* musl's fexecve runs /proc/self/fd/N. */
    int prog = open("/bin/hello", O_RDONLY | O_CLOEXEC);
    pid_t kid = fork();
    if (kid == 0) {
        char *argv[] = {"hello", NULL}, *envp[] = {NULL};
        fexecve(prog, argv, envp);
        _exit(1);
    }
    int status = 0;
    waitpid(kid, &status, 0);
    close(prog);
    check("fexecve runs a program through /proc/self/fd", WIFEXITED(status) && WEXITSTATUS(status) == 42);
}

static volatile pid_t limit_tid;
static volatile int limit_done;

static void *limit_thread(void *arg) {
    (void)arg;
    limit_tid = (pid_t)syscall(SYS_gettid);
    while (!limit_done)
        usleep(1000);
    return NULL;
}

/* Another process's descriptors: listed and described (everyone is root),
 * and its RLIMIT_NOFILE: per process, named by any of its threads' ids, kept
 * while it is a zombie. */
static void other_descriptors(void) {
    char path[64];
    int p[2];
    if (pipe(p) != 0) {
        check("pipe", 0);
        return;
    }
    pid_t kid = fork();
    if (kid == 0) {
        close(p[1]);
        char c;
        _exit(read(p[0], &c, 1) == 0 ? 0 : 1);
    }
    snprintf(path, sizeof path, "/proc/%d/fd/%d", kid, p[0]);
    char target[64] = {0};
    ssize_t n = readlink(path, target, sizeof target - 1);
    check("another process's descriptor reads as its pipe", n > 0 && strncmp(target, "pipe:[", 6) == 0);
    snprintf(path, sizeof path, "/proc/%d/fd", kid);
    DIR *d = opendir(path);
    int seen = 0;
    for (struct dirent *e; d && (e = readdir(d));) {
        if (e->d_name[0] != '.' && atoi(e->d_name) == p[0]) {
            seen = 1;
        }
    }
    if (d) {
        closedir(d);
    }
    check("another process's descriptors are listed", seen);
    struct rlimit rl = {0}, kid_lim = {100, 4096}, mine;
    check("prlimit of another process's RLIMIT_NOFILE", syscall(SYS_prlimit64, kid, RLIMIT_NOFILE, NULL, &rl) == 0 && rl.rlim_cur == 4096);
    check("... set for it alone", syscall(SYS_prlimit64, kid, RLIMIT_NOFILE, &kid_lim, NULL) == 0
          && getrlimit(RLIMIT_NOFILE, &mine) == 0 && mine.rlim_cur == 4096);
    close(p[0]);
    close(p[1]);
    siginfo_t si;
    waitid(P_PID, kid, &si, WEXITED | WNOWAIT);
    rl.rlim_cur = 0;
    check("... and kept while it is a zombie", syscall(SYS_prlimit64, kid, RLIMIT_NOFILE, NULL, &rl) == 0 && rl.rlim_cur == 100);
    int status = 0;
    waitpid(kid, &status, 0);
    check("the other process saw its pipe's end", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    pthread_t t;
    limit_done = 0;
    limit_tid = 0;
    pthread_create(&t, NULL, limit_thread, NULL);
    while (!limit_tid)
        usleep(1000);
    struct rlimit by_tid = {200, 4096}, now = {0}, back = {4096, 4096};
    int set = syscall(SYS_prlimit64, limit_tid, RLIMIT_NOFILE, &by_tid, NULL) == 0;
    int got = getrlimit(RLIMIT_NOFILE, &now) == 0 && now.rlim_cur == 200;
    errno = 0;
    int emfile = dup2(0, 200) == -1 && errno == EBADF && fcntl(0, F_DUPFD, 200) == -1 && errno == EINVAL;
    setrlimit(RLIMIT_NOFILE, &back);
    limit_done = 1;
    pthread_join(t, NULL);
    check("a thread's id names its process's RLIMIT_NOFILE, which bounds the descriptors", set && got && emfile);
}

/* Whether `s` ends with `end`. */
static int ends_with(const char *s, const char *end) {
    size_t n = strlen(s), m = strlen(end);
    return n >= m && strcmp(s + n - m, end) == 0;
}

/* readlink into `buf` (NUL-terminated), its result. */
static ssize_t link_of(const char *path, char *buf, size_t size) {
    ssize_t n = readlink(path, buf, size - 1);
    buf[n > 0 ? n : 0] = 0;
    return n;
}

/* Whether the program file `path` leads to, opened for reading, is an ELF image with
 * inode `ino` and `links` links. */
static int opens_program(const char *path, ino_t ino, nlink_t links) {
    int fd = open(path, O_RDONLY);
    char magic[4] = {0};
    struct stat st;
    int ok = fd >= 0 && read(fd, magic, 4) == 4 && memcmp(magic, "\177ELF", 4) == 0 && fstat(fd, &st) == 0
             && st.st_ino == ino && st.st_nlink == links && S_ISREG(st.st_mode);
    if (fd >= 0) close(fd);
    return ok;
}

/* The program copy's modes (`exe_links`): "--exe-child <fd>" waits for a byte on fd, then
 * runs /proc/self/exe again (its file is removed by then) as "--exe-removed", which exits
 * 7 if its exe reads " (deleted)" and opens the program, unlinked. */
static int exe_mode(int argc, char **argv) {
    char buf[512];
    if (strcmp(argv[1], "--exe-removed") == 0) {
        struct stat st;
        int ok = link_of("/proc/self/exe", buf, sizeof buf) > 0 && ends_with(buf, " (deleted)")
                 && stat("/proc/self/exe", &st) == 0 && opens_program("/proc/self/exe", st.st_ino, 0);
        return ok ? 7 : 8;
    }
    if (strcmp(argv[1], "--exe-child") == 0 && argc > 2) {
        char c;
        if (read(atoi(argv[2]), &c, 1) != 1) return 10;
        char *args[] = {"exe-removed", "--exe-removed", NULL}, *env[] = {NULL};
        execve("/proc/self/exe", args, env);
        return 9;
    }
    return 11;
}

/* Copies the running program (read through /proc/self/exe) to `to`, executable: 0, or -1
 * with errno set. */
static int copy_self(const char *to) {
    int in = open("/proc/self/exe", O_RDONLY);
    if (in < 0) return -1;
    int out = open(to, O_WRONLY | O_CREAT | O_TRUNC, 0755);
    if (out < 0) {
        close(in);
        return -1;
    }
    static char buf[65536];
    ssize_t n;
    int ok = 1;
    while ((n = read(in, buf, sizeof buf)) > 0) ok &= write(out, buf, n) == n;
    close(in);
    close(out);
    return ok && n == 0 ? 0 : -1;
}

/* /proc/<pid>/exe is a magic link to the program file itself (Linux's mm->exe_file): it
 * reads as the path the program was run by, opens and stats that file also after it was
 * renamed and removed (" (deleted)" then), runs it again (/proc/self/exe), and is ENOENT
 * for a zombie. On tmpfs and on /data. */
static void exe_links(const char *dir) {
    char prog[64], moved[64], path[64], buf[512], label[96];
    snprintf(prog, sizeof prog, "%s/exe-prog", dir);
    snprintf(moved, sizeof moved, "%s/exe-moved", dir);
    unlink(prog);
    unlink(moved);
    if (copy_self(prog) != 0) {
        if (errno == EROFS) {
            printf("  %s is read-only: its exe checks are skipped\n", dir);
            return;
        }
        snprintf(label, sizeof label, "%s: the program copied through /proc/self/exe", dir);
        check(label, 0);
        return;
    }
    struct stat want, got, link;
    stat(prog, &want);
    int p[2];
    pipe(p);
    pid_t kid = fork();
    if (kid == 0) {
        close(p[1]);
        char fd[16];
        snprintf(fd, sizeof fd, "%d", p[0]);
        execl(prog, "exe-prog", "--exe-child", fd, (char *)NULL);
        _exit(12);
    }
    close(p[0]);
    /* Until the child runs the copy, its exe is this program's. */
    snprintf(path, sizeof path, "/proc/%d/exe", kid);
    for (int i = 0; i < 5000 && !(link_of(path, buf, sizeof buf) > 0 && strcmp(buf, prog) == 0); i++) usleep(1000);
    snprintf(label, sizeof label, "%s: /proc/<pid>/exe names the program run", dir);
    check(label, strcmp(buf, prog) == 0);
    snprintf(label, sizeof label, "%s: ... stat follows it, lstat is the link", dir);
    check(label, stat(path, &got) == 0 && got.st_ino == want.st_ino && got.st_dev == want.st_dev
                     && lstat(path, &link) == 0 && S_ISLNK(link.st_mode));
    snprintf(label, sizeof label, "%s: ... opens the program after a rename", dir);
    check(label, rename(prog, moved) == 0 && opens_program(path, want.st_ino, 1));
    snprintf(label, sizeof label, "%s: ... reads \" (deleted)\" once it is removed", dir);
    check(label, unlink(moved) == 0 && link_of(path, buf, sizeof buf) > 0 && ends_with(buf, " (deleted)"));
    snprintf(label, sizeof label, "%s: ... and opens the removed program", dir);
    check(label, stat(path, &got) == 0 && got.st_nlink == 0 && opens_program(path, want.st_ino, 0));
    /* The child runs the removed program again through /proc/self/exe. */
    write(p[1], "x", 1);
    close(p[1]);
    siginfo_t si = {0};
    waitid(P_PID, kid, &si, WEXITED | WNOWAIT);
    snprintf(label, sizeof label, "%s: exec of /proc/self/exe runs the removed program", dir);
    check(label, si.si_code == CLD_EXITED && si.si_status == 7);
    errno = 0;
    int gone = readlink(path, buf, sizeof buf) == -1 && errno == ENOENT;
    errno = 0;
    gone &= stat(path, &got) == -1 && errno == ENOENT && lstat(path, &link) == 0 && S_ISLNK(link.st_mode);
    snprintf(label, sizeof label, "%s: a zombie's exe is ENOENT (the link is there)", dir);
    check(label, gone);
    waitpid(kid, NULL, 0);
}

/* The figures of /proc/self/status (kB) and /proc/self/statm (pages) by kind of memory. */
struct vm {
    long size, rss, data, stk, exe, lib;
    long statm[7];
    int fields;
};

static long status_field(const char *status, const char *key) {
    const char *at = strstr(status, key);
    long v = -1;
    if (at) sscanf(at + strlen(key), " %ld", &v);
    return v;
}

static void vm_now(struct vm *v) {
    char buf[4096];
    memset(v, 0, sizeof *v);
    slurp("/proc/self/statm", buf, sizeof buf);
    v->fields = count_fields(buf);
    sscanf(buf, "%ld %ld %ld %ld %ld %ld %ld", &v->statm[0], &v->statm[1], &v->statm[2], &v->statm[3], &v->statm[4],
           &v->statm[5], &v->statm[6]);
    slurp("/proc/self/status", buf, sizeof buf);
    v->size = status_field(buf, "\nVmSize:");
    v->rss = status_field(buf, "\nVmRSS:");
    v->data = status_field(buf, "\nVmData:");
    v->stk = status_field(buf, "\nVmStk:");
    v->exe = status_field(buf, "\nVmExe:");
    v->lib = status_field(buf, "\nVmLib:");
}

/* Grows the stack by about `bytes`, touching it from the top down. */
static void __attribute__((noinline)) deep(size_t bytes) {
    volatile char big[2 << 20];
    for (size_t i = bytes < sizeof big ? bytes : sizeof big; i > 0; i -= 4096) big[i - 1] = 1;
    big[0] = 1;
}

/* statm's seven fields and status's VmData, VmStk, VmExe and VmLib as Linux's: private
 * writable memory is data (also brk and what mprotect made writable), shared or
 * read-only memory is not, executable memory that is not the program's code is VmLib, a
 * grown stack VmStk; statm's data is VmData + VmStk, its text VmExe; fork copies them. */
static void memory_kinds(void) {
    struct vm a, b;
    vm_now(&a);
    check("statm has 7 fields, lib and dt 0", a.fields == 7 && a.statm[4] == 0 && a.statm[6] == 0);
    check("statm's size and resident are VmSize and VmRSS", a.statm[0] * 4 == a.size && a.statm[1] * 4 == a.rss);
    check("statm's text is VmExe, its data VmData + VmStk", a.statm[3] * 4 == a.exe && a.statm[5] * 4 == a.data + a.stk);
    check("a static program: VmExe > 0, VmLib 0, VmStk > 0, VmData > 0", a.exe > 0 && a.lib == 0 && a.stk > 0 && a.data > 0);
    const size_t len = 16 << 20;
    char *m = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    vm_now(&b);
    check("private writable memory is VmData (and statm's data)", m != MAP_FAILED && b.data == a.data + 16384 && b.statm[5] == a.statm[5] + 4096);
    munmap(m, len);
    vm_now(&b);
    check("... and goes with munmap", b.data == a.data && b.size == a.size);
    m = mmap(NULL, len, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    vm_now(&b);
    check("read-only memory is no data, but address space", m != MAP_FAILED && b.data == a.data && b.size == a.size + 16384);
    mprotect(m, len / 2, PROT_READ | PROT_WRITE);
    vm_now(&b);
    check("mprotect to writable makes it data", b.data == a.data + 8192);
    mprotect(m, len, PROT_READ | PROT_EXEC);
    vm_now(&b);
    check("executable memory beside the program's code is VmLib", b.data == a.data && b.lib == a.lib + 16384 && b.exe == a.exe && b.statm[3] == a.statm[3]);
    munmap(m, len);
    m = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    vm_now(&b);
    check("shared memory is no data", m != MAP_FAILED && b.data == a.data && b.size == a.size + 16384);
    munmap(m, len);
    long brk0 = syscall(SYS_brk, 0);
    long brk1 = syscall(SYS_brk, brk0 + (1 << 20));
    vm_now(&b);
    check("the heap (brk) is data", brk1 == brk0 + (1 << 20) && b.data == a.data + 1024);
    syscall(SYS_brk, brk0);
    vm_now(&a);
    deep(2 << 20);
    vm_now(&b);
    check("a grown stack is VmStk (and statm's data), not VmData", b.stk >= a.stk + 1024 && b.data == a.data && b.statm[5] * 4 == b.data + b.stk);
    pid_t kid = fork();
    if (kid == 0) {
        struct vm c;
        vm_now(&c);
        _exit(c.data == b.data && c.stk == b.stk && c.exe == b.exe && c.lib == b.lib && c.size == b.size ? 0 : 1);
    }
    int status = 0;
    waitpid(kid, &status, 0);
    check("a child after fork has its parent's figures", WIFEXITED(status) && WEXITSTATUS(status) == 0);
}

int main(int argc, char **argv) {
    if (argc > 1) return exe_mode(argc, argv);
    char name[16] = {0};
    prctl(PR_SET_NAME, "renamed-process");
    prctl(PR_GET_NAME, name);
    check("PR_SET_NAME/PR_GET_NAME round-trip (15 bytes)", strcmp(name, "renamed-process") == 0);

    struct __user_cap_header_struct hdr = {_LINUX_CAPABILITY_VERSION_3, 0};
    struct __user_cap_data_struct data[2];
    int r = syscall(SYS_capget, &hdr, data);
    check("capget reports the full set (everything is root)", r == 0 && data[0].effective == 0xffffffff && data[0].inheritable == 0);
    hdr.version = 0x12345678;
    r = syscall(SYS_capget, &hdr, data);
    check("capget with an unknown version proposes v3", r == -1 && errno == EINVAL && hdr.version == _LINUX_CAPABILITY_VERSION_3);
    check("PR_CAPBSET_READ knows CAP_SYS_ADMIN", prctl(PR_CAPBSET_READ, CAP_SYS_ADMIN) == 1);
    check("PR_SET_NO_NEW_PRIVS sticks", prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0 && prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) == 1);

    /* A grandchild with PR_SET_PDEATHSIG learns when its parent dies. */
    int pipefd[2];
    pipe(pipefd);
    pid_t child = fork();
    if (child == 0) {
        pid_t grandchild = fork();
        if (grandchild == 0) {
            sigset_t set;
            sigemptyset(&set);
            sigaddset(&set, SIGUSR1);
            sigprocmask(SIG_BLOCK, &set, NULL);
            prctl(PR_SET_PDEATHSIG, SIGUSR1);
            write(pipefd[1], "r", 1);
            int sig = 0;
            sigwait(&set, &sig);
            write(pipefd[1], sig == SIGUSR1 ? "y" : "n", 1);
            _exit(0);
        }
        char c;
        read(pipefd[0], &c, 1);
        _exit(0);
    }
    int st;
    waitpid(child, &st, 0);
    char c = 0;
    close(pipefd[1]);
    read(pipefd[0], &c, 1);
    check("PR_SET_PDEATHSIG signals an orphaned child", c == 'y');

    /* htop's access pattern: a directory fd per process (O_PATH), the
     * files opened relative to it. Every /proc/<pid>/stat must name <pid>. */
    DIR *proc = opendir("/proc");
    int consistent = proc != NULL, seen = 0, found_self = 0;
    struct dirent *e;
    while (proc && (e = readdir(proc))) {
        int pid = atoi(e->d_name);
        if (pid <= 0) continue;
        int pfd = openat(dirfd(proc), e->d_name, O_PATH | O_DIRECTORY | O_NOFOLLOW);
        int sfd = pfd >= 0 ? openat(pfd, "stat", O_RDONLY) : -1;
        char buf[256] = {0};
        if (sfd < 0 || read(sfd, buf, sizeof buf - 1) <= 0 || atoi(buf) != pid) {
            printf("  /proc/%d/stat via openat: '%.40s'\n", pid, buf);
            consistent = 0;
        }
        seen++;
        found_self |= pid == getpid();
        if (sfd >= 0) close(sfd);
        if (pfd >= 0) close(pfd);
    }
    if (proc) closedir(proc);
    /* (The tree's own processes only: its pid 1 and this one at least.) */
    check("/proc/<pid>/stat via openat on an O_PATH dir fd", consistent && seen >= 2 && found_self);

    /* The pids are the tree's own namespace: its init is pid 1 with no parent in it, and a
     * process's parent in /proc is getppid's. */
    {
        char sbuf[512] = {0};
        int fd = open("/proc/1/stat", O_RDONLY);
        int ok1 = fd >= 0 && read(fd, sbuf, sizeof sbuf - 1) > 0;
        if (fd >= 0) close(fd);
        char *rp = strrchr(sbuf, ')');
        int ppid1 = -1;
        if (rp) sscanf(rp + 2, "%*c %d", &ppid1);
        char ppath[64];
        snprintf(ppath, sizeof ppath, "/proc/%d/stat", getpid());
        memset(sbuf, 0, sizeof sbuf);
        fd = open(ppath, O_RDONLY);
        int ok2 = fd >= 0 && read(fd, sbuf, sizeof sbuf - 1) > 0;
        if (fd >= 0) close(fd);
        rp = strrchr(sbuf, ')');
        int myppid = -1;
        if (rp) sscanf(rp + 2, "%*c %d", &myppid);
        check("/proc/1 is the tree's init, /proc/self's parent getppid's", ok1 && ok2 && ppid1 == 0 && myppid == getppid());
    }
    char link[32] = {0};
    readlink("/proc/self", link, sizeof link - 1);
    check("/proc/self points to the caller", atoi(link) == getpid());

    char buf[4096], path[64];
    long ncpu = sysconf(_SC_NPROCESSORS_ONLN);
    slurp("/proc/stat", buf, sizeof buf);
    int cpu_lines = 0;
    for (char *l = buf; (l = strstr(l, "\ncpu")); l++) cpu_lines++;
    check("/proc/stat: a total and one line per CPU, btime", strncmp(buf, "cpu ", 4) == 0 && cpu_lines == ncpu && strstr(buf, "\nbtime "));
    slurp("/proc/meminfo", buf, sizeof buf);
    long total = 0, avail = 0;
    sscanf(strstr(buf, "MemTotal:"), "MemTotal: %ld", &total);
    sscanf(strstr(buf, "MemAvailable:"), "MemAvailable: %ld", &avail);
    check("/proc/meminfo: MemTotal > MemAvailable > 0 (kB)", total > avail && avail > 0);
    long sys1 = -1, sys2 = -1, allocs = -1;
    if (slurp("/proc/counters", buf, sizeof buf) > 0 && strstr(buf, "syscalls ")) sys1 = atol(strstr(buf, "syscalls ") + 9);
    for (int i = 0; i < 10; i++) getppid();
    if (slurp("/proc/counters", buf, sizeof buf) > 0 && strstr(buf, "syscalls ")) sys2 = atol(strstr(buf, "syscalls ") + 9);
    if (strstr(buf, "heap_allocs ")) allocs = atol(strstr(buf, "heap_allocs ") + 12);
    check("/proc/counters counts system calls and allocations", sys1 > 0 && sys2 >= sys1 + 10 && allocs > 0 && strstr(buf, "ipc_calls ") && strstr(buf, "address_space_switches "));
    double l1, l5, l15;
    int running, procs;
    slurp("/proc/loadavg", buf, sizeof buf);
    check("/proc/loadavg has the Linux format", sscanf(buf, "%lf %lf %lf %d/%d", &l1, &l5, &l15, &running, &procs) == 5 && procs >= 3);
    double up = 0;
    slurp("/proc/uptime", buf, sizeof buf);
    struct sysinfo si;
    check("/proc/uptime and sysinfo agree", sscanf(buf, "%lf", &up) == 1 && sysinfo(&si) == 0 && (long)up - si.uptime <= 1 && si.totalram > si.freeram);
    snprintf(path, sizeof path, "/proc/%d/stat", getpid());
    slurp(path, buf, sizeof buf);
    check("/proc/<pid>/stat has 52 fields and the name", count_fields(buf) == 52 && strstr(buf, "(renamed-process)"));
    snprintf(path, sizeof path, "/proc/%d/cmdline", getpid());
    int n = slurp(path, buf, sizeof buf);
    check("/proc/<pid>/cmdline is argv with NULs", n > 0 && strstr(buf, "proctest") && buf[n - 1] == 0);
    n = readlink("/proc/self/exe", buf, sizeof buf - 1);
    buf[n > 0 ? n : 0] = 0;
    check("/proc/self/exe names the program", n > 0 && strstr(buf, "proctest"));
    slurp("/sys/devices/system/cpu/online", buf, sizeof buf);
    char want[16];
    snprintf(want, sizeof want, ncpu > 1 ? "0-%ld\n" : "0\n", ncpu - 1);
    check("/sys/devices/system/cpu/online matches nproc", strcmp(buf, want) == 0);
    int dirs = 0;
    DIR *cpus = opendir("/sys/devices/system/cpu");
    while (cpus && (e = readdir(cpus))) dirs += strncmp(e->d_name, "cpu", 3) == 0;
    if (cpus) closedir(cpus);
    check("/sys/devices/system/cpu has a cpuN per CPU", dirs == ncpu);
    check("/proc is read-only", open("/proc/stat", O_WRONLY) < 0 || write(open("/proc/stat", O_WRONLY), "x", 1) < 0);

    thread_dir();
    the_servers_part();
    descriptors();
    other_descriptors();
    exe_links("/tmp");
    exe_links("/data");
    errno = 0;
    char none[64];
    check("/proc/<pid>/exe of no process: ENOENT", readlink("/proc/999999/exe", none, sizeof none) == -1 && errno == ENOENT);
    memory_kinds();
    printf("proctest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
