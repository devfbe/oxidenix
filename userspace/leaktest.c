/* Repeated operations leave the kernel's memory as it was: after a warm-up,
 * many rounds of fork and exit, fork and exec, a process whose long wait
 * ended early (its timer), threads, file mappings (the
 * server's tmpfs and /data), descriptors, pipes and AF_UNIX connections
 * keep the kernel heap in use (Slab in /proc/meminfo) and the free frames
 * (MemFree) flat. A leak of one object per round shows at once. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures;

static long meminfo(const char *name) {
    FILE *f = fopen("/proc/meminfo", "r");
    char line[128];
    long kib = -1;
    size_t n = strlen(name);
    while (f && fgets(line, sizeof line, f))
        if (strncmp(line, name, n) == 0 && line[n] == ':') kib = atol(line + n + 1);
    if (f) fclose(f);
    return kib;
}

static void *nothing(void *arg) {
    return arg;
}

static void map_file(const char *path) {
    int f = open(path, O_CREAT | O_RDWR, 0644);
    if (f < 0 || ftruncate(f, 65536) != 0) return;
    char *m = mmap(NULL, 65536, PROT_READ | PROT_WRITE, MAP_SHARED, f, 0);
    if (m != MAP_FAILED) {
        m[0]++;
        m[40000]++;
        munmap(m, 65536);
    }
    close(f);
}

/* One round of each kind of work. */
static void round_of(const char *what) {
    if (!strcmp(what, "fork and exit")) {
        pid_t c = fork();
        if (c == 0) _exit(0);
        waitpid(c, NULL, 0);
    } else if (!strcmp(what, "fork and exec")) {
        pid_t c = fork();
        if (c == 0) {
            execl("/bin/hello", "hello", (char *)NULL);
            _exit(1);
        }
        waitpid(c, NULL, 0);
    } else if (!strcmp(what, "a long wait cut short")) {
        /* A child waits with a minute's timeout, is woken at once and
         * ends: its timer must not outlive it. */
        int p[2];
        if (pipe(p) != 0) return;
        pid_t c = fork();
        if (c == 0) {
            struct pollfd pf = {p[0], POLLIN, 0};
            poll(&pf, 1, 60000);
            _exit(0);
        }
        write(p[1], "x", 1);
        waitpid(c, NULL, 0);
        close(p[0]);
        close(p[1]);
    } else if (!strcmp(what, "a thread")) {
        pthread_t t;
        if (pthread_create(&t, NULL, nothing, NULL) == 0) pthread_join(t, NULL);
    } else if (!strcmp(what, "a /tmp mapping")) {
        map_file("/tmp/leaktest.map");
    } else if (!strcmp(what, "a /data mapping")) {
        map_file("/data/leaktest.map");
    } else if (!strcmp(what, "open and close")) {
        close(open("/etc/passwd", O_RDONLY));
    } else if (!strcmp(what, "a pipe")) {
        int p[2];
        if (pipe(p) == 0) {
            write(p[1], "x", 1);
            close(p[0]);
            close(p[1]);
        }
    } else if (!strcmp(what, "a unix connection")) {
        struct sockaddr_un a = {.sun_family = AF_UNIX};
        memcpy(a.sun_path + 1, "leaktest", 8);
        socklen_t len = 2 + 1 + 8;
        int l = socket(AF_UNIX, SOCK_STREAM, 0), c = socket(AF_UNIX, SOCK_STREAM, 0);
        bind(l, (struct sockaddr *)&a, len);
        listen(l, 1);
        connect(c, (struct sockaddr *)&a, len);
        int x = accept(l, NULL, NULL);
        write(c, "x", 1);
        close(x);
        close(c);
        close(l);
    }
}

/* `rounds` rounds after a warm-up: the heap in use may grow by `heap_slack`
 * KiB, the free frames shrink by `frame_slack` KiB. */
static void check_flat(const char *what, int rounds, long heap_slack, long frame_slack) {
    for (int i = 0; i < 100; i++) round_of(what);
    long heap = meminfo("Slab"), free = meminfo("MemFree");
    for (int i = 0; i < rounds; i++) round_of(what);
    long heap2 = meminfo("Slab"), free2 = meminfo("MemFree");
    int ok = heap >= 0 && free >= 0 && heap2 - heap <= heap_slack && free - free2 <= frame_slack;
    char name[96];
    snprintf(name, sizeof name, "%d x %s", rounds, what);
    printf("%-36s heap %+5ld KiB, free frames %+6ld KiB  %s\n", name, heap2 - heap, free2 - free, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

int main(void) {
    /* Slack: a few KiB of heap for what other programs do meanwhile; for
     * tasks, the kernel stack region's page tables (frames), a bounded
     * cache of at most 1 MiB that new slots fill (`memory/kstack.rs`). */
    check_flat("fork and exit", 2000, 16, 1100);
    check_flat("fork and exec", 1000, 16, 1100);
    check_flat("a long wait cut short", 1000, 16, 1100);
    check_flat("a thread", 2000, 16, 1100);
    check_flat("a /tmp mapping", 1000, 16, 64);
    check_flat("a /data mapping", 1000, 16, 64);
    check_flat("open and close", 2000, 16, 64);
    check_flat("a pipe", 2000, 16, 64);
    check_flat("a unix connection", 2000, 16, 64);
    unlink("/tmp/leaktest.map");
    unlink("/data/leaktest.map");
    printf("%s\n", failures ? "leaktest: FAILURES" : "leaktest: all ok");
    return failures ? 1 : 0;
}
