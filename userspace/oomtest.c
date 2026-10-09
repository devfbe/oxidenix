/* Running out of resources: fork bombs, memory hogs, full pipes and full
 * descriptor tables fail with errors (EAGAIN, ENOMEM, EMFILE) instead of
 * bringing the kernel down, and a process touching uncommitted
 * (MAP_NORESERVE) memory beyond the commit limit is the one killed. */
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

#define MIB (1024 * 1024)

static int failures;

static void check(const char *name, int ok) {
    printf("%-52s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static void fork_bomb(void) {
    static pid_t kids[1024];
    int n = 0, err = 0;
    while (n < 1024) {
        pid_t p = fork();
        if (p == 0) {
            for (;;) pause();
        }
        if (p < 0) {
            err = errno;
            break;
        }
        kids[n++] = p;
    }
    printf("fork bomb: %d children, then %s\n", n, strerror(err));
    check("fork fails with EAGAIN/ENOMEM instead of a panic", n > 10 && (err == EAGAIN || err == ENOMEM));
    for (int i = 0; i < n; i++) kill(kids[i], SIGKILL);
    int reaped = 0;
    while (waitpid(-1, NULL, 0) > 0) reaped++;
    check("all children reaped", reaped == n);
    pid_t p = fork();
    if (p == 0) _exit(7);
    int status;
    waitpid(p, &status, 0);
    check("fork works again afterwards", WIFEXITED(status) && WEXITSTATUS(status) == 7);
}

static void memory_hog(void) {
    static void *chunks[4096];
    int n = 0;
    while (n < 4096) {
        void *p = mmap(NULL, MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) break;
        memset(p, 0xab, MIB);
        chunks[n++] = p;
    }
    printf("memory hog: %d MiB mapped before ENOMEM\n", n);
    check("mmap fails with ENOMEM when memory runs out", n > 16 && errno == ENOMEM);
    for (int i = 0; i < n; i++) munmap(chunks[i], MIB);
    void *again = mmap(NULL, 8 * MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check("memory is usable again after munmap", again != MAP_FAILED);
    if (again != MAP_FAILED) munmap(again, 8 * MIB);
}

static long meminfo(const char *key) {
    char text[4096] = {0};
    int fd = open("/proc/meminfo", O_RDONLY);
    read(fd, text, sizeof text - 1);
    close(fd);
    char *p = strstr(text, key);
    return p ? strtol(p + strlen(key) + 1, NULL, 10) : -1;
}

/* Memory that MAP_NORESERVE leaves uncommitted is committed page by page
 * when touched: a process that touches more of it than is left is killed
 * then, and a process whose memory was committed keeps it (also while the
 * toucher would hold the frames it took). */
static void noreserve_toucher(void) {
    int ready[2], go[2], held[2];
    pipe(ready);
    pipe(go);
    long room = (meminfo("CommitLimit:") - meminfo("Committed_AS:")) * 1024 - 16 * MIB;
    pid_t committed = fork();
    if (committed == 0) {
        /* All but 16 MiB of what is left, committed, touched only later. */
        char *mem = mmap(NULL, room, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        char c = mem == MAP_FAILED ? 'n' : 'y';
        write(ready[1], &c, 1);
        read(go[0], &c, 1);
        if (mem == MAP_FAILED) _exit(2);
        for (long i = 0; i < room; i += 4096) mem[i] = 1;
        _exit(0);
    }
    char c = 0;
    read(ready[0], &c, 1);
    /* Made after the committed process forked: only the toucher holds
     * its write end, so the read below ends when the toucher dies. */
    pipe(held);
    pid_t toucher = fork();
    if (toucher == 0) {
        /* 64 MiB of uncommitted memory, touched page by page, then held. */
        close(held[0]);
        char *mem = mmap(NULL, 4096L * MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
        if (mem == MAP_FAILED) _exit(2);
        for (long i = 0; i < 64 * MIB; i += 4096) mem[i] = 1;
        write(held[1], "h", 1);
        for (;;) pause();
    }
    close(held[1]);
    char h = 0;
    long got = read(held[0], &h, 1);
    /* The committed process touches its memory now, whatever the toucher holds. */
    write(go[1], "g", 1);
    int ts = 0, cs = 0;
    waitpid(committed, &cs, 0);
    kill(toucher, SIGKILL);
    waitpid(toucher, &ts, 0);
    printf("noreserve: committed %ld MiB first (%c), toucher %s, committed status %#x\n", room / MIB, c,
           got == 1 ? "held 64 MiB" : "died", cs);
    check("a MAP_NORESERVE toucher beyond the limit is killed", c == 'y' && got == 0 && WIFSIGNALED(ts) && WTERMSIG(ts) == SIGKILL);
    check("... and committed memory stays usable", WIFEXITED(cs) && WEXITSTATUS(cs) == 0);
    close(ready[0]);
    close(ready[1]);
    close(go[0]);
    close(go[1]);
    close(held[0]);
}

/* The same at a copy into such memory: read() from a pipe into an
 * untouched MAP_NORESERVE buffer when nothing is left to commit kills the
 * reader (as its own touch would), rather than failing with EFAULT. */
static void noreserve_copy(void) {
    pid_t kid = fork();
    if (kid == 0) {
        int p[2];
        pipe(p);
        write(p[1], "sixteen bytes!!!", 16);
        char *buf = mmap(NULL, MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
        /* Commit everything that is left, down to the last page. */
        for (long chunk = 64 * MIB; chunk >= 4096; chunk /= 2)
            while (mmap(NULL, chunk, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) != MAP_FAILED) {}
        ssize_t n = read(p[0], buf, 16);
        _exit(n == 16 ? 4 : 3);
    }
    int st = 0;
    waitpid(kid, &st, 0);
    printf("noreserve copy: child status %#x\n", st);
    check("a copy into uncommitted memory beyond the limit kills", WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL);
}

static void pipe_flood(void) {
    static int fds[200][2];
    static char block[4096];
    long total = 0;
    int pipes = 0, err = 0;
    for (; pipes < 100; pipes++) {
        if (pipe2(fds[pipes], O_NONBLOCK) < 0) break;
        ssize_t w;
        while ((w = write(fds[pipes][1], block, sizeof block)) > 0) total += w;
        err = errno;
    }
    printf("pipe flood: %d pipes, %ld KiB buffered\n", pipes, total / 1024);
    check("full pipes report EAGAIN, no panic", err == EAGAIN && total > 0);
    for (int i = 0; i < pipes; i++) {
        close(fds[i][0]);
        close(fds[i][1]);
    }
}

/* Descriptors up to RLIMIT_NOFILE (as getrlimit reports it), then EMFILE;
 * a fork copies the full table and exec closes the close-on-exec ones. */
static void descriptor_flood(void) {
    struct rlimit rl;
    check("getrlimit(RLIMIT_NOFILE) is 4096", getrlimit(RLIMIT_NOFILE, &rl) == 0 && rl.rlim_cur == 4096 && rl.rlim_max == 4096);
    int first = -1, last = -1, n = 0;
    for (;;) {
        int fd = open("/dev/null", O_RDONLY | O_CLOEXEC);
        if (fd < 0) break;
        if (first < 0) first = fd;
        last = fd;
        n++;
    }
    int e = errno;
    check("open fails with EMFILE once the table is full", e == EMFILE && last == (int)rl.rlim_cur - 1);
    pid_t child = fork();
    if (child == 0) _exit(fcntl(last, F_GETFD) >= 0 ? 0 : 1);
    int status = -1;
    waitpid(child, &status, 0);
    check("a fork copies all of them", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    for (int fd = first; fd <= last; fd++) close(fd);
    check("after closing them, open works again", (first = open("/dev/null", O_RDONLY)) >= 0 && close(first) == 0 && n > 4000);
}

int main(void) {
    descriptor_flood();
    fork_bomb();
    memory_hog();
    noreserve_toucher();
    noreserve_copy();
    pipe_flood();
    printf("oomtest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
