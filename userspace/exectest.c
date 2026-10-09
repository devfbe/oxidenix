/* execve maps programs from the page cache: processes running the same
 * program share its pages, a program file cannot be written while it runs
 * (ETXTBSY) nor run while it is open for writing, and changing a program
 * file between runs takes effect. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysinfo.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

/* Initialized data and a zero-filled array right behind it: the loader
 * must clear the rest of the last file page of the data segment. */
static volatile int data_word = 42;
static volatile char bss[100000];

static long free_kib(void) {
    struct sysinfo si;
    sysinfo(&si);
    return (long)(si.freeram * si.mem_unit / 1024);
}

static void sleep_ms(int ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&ts, NULL);
}

/* Runs `path` with `args`; returns the exit status, or 100 + errno if
 * execve failed. */
static int run(const char *path, char *const args[]) {
    pid_t pid = fork();
    if (pid == 0) {
        execv(path, args);
        _exit(100 + errno);
    }
    int st;
    waitpid(pid, &st, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : -1;
}

static int copy_file(const char *from, const char *to) {
    int in = open(from, O_RDONLY), out = open(to, O_WRONLY | O_CREAT | O_TRUNC, 0755);
    if (in < 0 || out < 0) return -1;
    static char buf[65536];
    ssize_t n;
    while ((n = read(in, buf, sizeof buf)) > 0)
        if (write(out, buf, n) != n) return -1;
    close(in);
    close(out);
    return 0;
}

/* `--layout`: the arguments and the environment lie as Linux puts them:
 * each string right after the one before, the environment's after the
 * arguments'. */
static int layout(int argc, char **argv, char **envp) {
    for (int i = 0; i + 1 < argc; i++)
        if (argv[i + 1] != argv[i] + strlen(argv[i]) + 1) return 1;
    if (envp[0] && envp[0] != argv[argc - 1] + strlen(argv[argc - 1]) + 1) return 2;
    for (int i = 0; envp[i] && envp[i + 1]; i++)
        if (envp[i + 1] != envp[i] + strlen(envp[i]) + 1) return 3;
    return 0;
}

/* A thread that keeps making threads (each ends at once) while the main
 * thread executes a program: the execve ends every one of them, also those
 * made while it was under way. */
static void *spawner(void *arg) {
    (void)arg;
    for (;;) {
        pthread_t t;
        if (pthread_create(&t, NULL, (void *(*)(void *))pthread_self, NULL) == 0) pthread_detach(t);
    }
    return NULL;
}

int main(int argc, char **argv, char **envp) {
    if (argc > 1 && strcmp(argv[1], "--layout") == 0) return layout(argc, argv, envp);
    const char *busybox = argc > 1 ? argv[1] : "/bin/busybox";
    const char *hello = argc > 2 ? argv[2] : "/bin/hello";
    int clean = 1;
    for (size_t i = 0; i < sizeof bss; i++) clean &= bss[i] == 0;
    check("initialized data is there and bss is zero", data_word == 42 && clean);
    char *self[] = {"exectest", "--layout", "a", "", "bcd", NULL};
    check("arguments and environment lie in order, back to back", run("/bin/exectest", self) == 0);

    /* Eight processes running the same program share its pages. */
    long before = free_kib();
    pid_t kids[8];
    for (int i = 0; i < 8; i++) {
        kids[i] = fork();
        if (kids[i] == 0) {
            execl(busybox, "sleep", "3", (char *)NULL);
            _exit(127);
        }
    }
    sleep_ms(500);
    long used = before - free_kib();
    printf("    8 x busybox sleep: %ld KiB (busybox: 1.4 MB)\n", used);
    check("8 runs of busybox take less than one copy of it", used < 1300);
    for (int i = 0; i < 8; i++) kill(kids[i], SIGKILL);
    for (int i = 0; i < 8; i++) waitpid(kids[i], NULL, 0);

    /* A program on tmpfs. */
    check("copying busybox to /tmp", copy_file(busybox, "/tmp/bb") == 0);
    char *t[] = {"true", NULL};
    check("a program in /tmp runs", run("/tmp/bb", t) == 0);

    pid_t sleeper = fork();
    if (sleeper == 0) {
        execl("/tmp/bb", "sleep", "5", (char *)NULL);
        _exit(127);
    }
    sleep_ms(200);
    int w = open("/tmp/bb", O_WRONLY);
    check("opening a running program for writing fails with ETXTBSY", w == -1 && errno == ETXTBSY);
    check("... and so does truncating it", truncate("/tmp/bb", 0) == -1 && errno == ETXTBSY);
    kill(sleeper, SIGKILL);
    waitpid(sleeper, NULL, 0);
    w = open("/tmp/bb", O_WRONLY);
    check("once it ended, it can be opened for writing", w >= 0);
    check("running a program open for writing fails with ETXTBSY", run("/tmp/bb", t) == 100 + ETXTBSY);
    close(w);
    check("after close it runs again", run("/tmp/bb", t) == 0);

    /* A writable shared mapping keeps the right to write after close. */
    w = open("/tmp/bb", O_RDWR);
    char *map = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, w, 0);
    close(w);
    check("running a program mapped shared and writable fails (ETXTBSY)", map != MAP_FAILED && run("/tmp/bb", t) == 100 + ETXTBSY);
    munmap(map, 4096);
    check("after munmap it runs again", run("/tmp/bb", t) == 0);
    w = open("/tmp/bb", O_RDONLY);
    map = mmap(NULL, 4096, PROT_READ, MAP_SHARED, w, 0);
    close(w);
    check("... and a read-only shared mapping does not stop it", run("/tmp/bb", t) == 0);
    munmap(map, 4096);

    /* Replace the program's contents: the next run sees the new file. */
    check("overwriting it with another program", copy_file(hello, "/tmp/bb") == 0);
    check("the next run executes the new contents", run("/tmp/bb", t) == 42);
    unlink("/tmp/bb");

    /* A program whose file is deleted while it runs keeps running. */
    copy_file(busybox, "/tmp/bb2");
    pid_t runner = fork();
    if (runner == 0) {
        execl("/tmp/bb2", "sh", "-c", "sleep 0.3; echo still alive >/dev/null; exit 9", (char *)NULL);
        _exit(127);
    }
    sleep_ms(100);
    unlink("/tmp/bb2");
    int st;
    waitpid(runner, &st, 0);
    check("a running program survives unlink of its file", WIFEXITED(st) && WEXITSTATUS(st) == 9);

    /* The loader is the Linux server's (R8): #! scripts, execveat, and its errors. */
    FILE *f = fopen("/tmp/script.sh", "w");
    fprintf(f, "#!/bin/sh -e\nexit $(( $# + 20 ))\n");
    fclose(f);
    chmod("/tmp/script.sh", 0755);
    char *sargs[] = {"script.sh", "a", "b", NULL};
    check("a #! script runs with its interpreter and arguments", run("/tmp/script.sh", sargs) == 22);
    f = fopen("/tmp/loop.sh", "w");
    fprintf(f, "#!/tmp/loop.sh\n");
    fclose(f);
    chmod("/tmp/loop.sh", 0755);
    check("a script that is its own interpreter: ELOOP", run("/tmp/loop.sh", sargs) == 100 + ELOOP);
    f = fopen("/tmp/junk", "w");
    fprintf(f, "not a program\n");
    fclose(f);
    chmod("/tmp/junk", 0755);
    check("a file that is neither ELF nor #!: ENOEXEC", run("/tmp/junk", t) == 100 + ENOEXEC);
    chmod("/tmp/junk", 0644);
    check("a file without an execute bit: EACCES", run("/tmp/junk", t) == 100 + EACCES);
    check("a directory: EACCES", run("/tmp", t) == 100 + EACCES);
    pid_t ea = fork();
    if (ea == 0) {
        int fd = open("/bin/hello", O_RDONLY);
        char *hargs[] = {"hello", NULL};
        char *env[] = {NULL};
        syscall(SYS_execveat, fd, "", hargs, env, AT_EMPTY_PATH);
        _exit(100 + errno);
    }
    waitpid(ea, &st, 0);
    check("execveat(fd, \"\", AT_EMPTY_PATH) runs the descriptor's file", WIFEXITED(st) && WEXITSTATUS(st) == 42);
    static char huge[200 * 1024];
    memset(huge, 'x', sizeof huge - 1);
    char *bigargs[] = {"hello", huge, NULL};
    check("an argument beyond MAX_ARG_STRLEN: E2BIG", run("/bin/hello", bigargs) == 100 + E2BIG);
    int all_ran = 1;
    for (int round = 0; round < 20 && all_ran; round++) {
        pid_t c = fork();
        if (c == 0) {
            alarm(10);
            pthread_t t;
            pthread_create(&t, NULL, spawner, NULL);
            pthread_create(&t, NULL, spawner, NULL);
            struct timespec d = {0, 2000000};
            nanosleep(&d, NULL);
            char *hargs[] = {"hello", NULL};
            execv("/bin/hello", hargs);
            _exit(1);
        }
        waitpid(c, &st, 0);
        all_ran = WIFEXITED(st) && WEXITSTATUS(st) == 42;
    }
    check("execve while other threads keep making threads", all_ran);
    unlink("/tmp/script.sh");
    unlink("/tmp/loop.sh");
    unlink("/tmp/junk");

    printf("%s\n", failures ? "exectest: FAILED" : "exectest: all passed");
    return failures != 0;
}
