/* Process information: prctl, capabilities and /proc: procfs's system-wide
 * files and /sys, the Linux server's per-process part (/proc/<pid>, self,
 * thread-self, mounts) and its magic links (/proc/self/fd). */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <linux/capability.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/socket.h>
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

    pid_t other = getppid();
    snprintf(path, sizeof path, "/proc/%d/fd", other);
    check("another process's descriptors: EACCES", opendir(path) == NULL && errno == EACCES);
}

int main(void) {
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
    check("/proc/<pid>/stat via openat on an O_PATH dir fd", consistent && seen >= 3 && found_self);
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
    printf("proctest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
