/* Process information: prctl, capabilities and (later) /proc. */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <linux/capability.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
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
    printf("proctest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
