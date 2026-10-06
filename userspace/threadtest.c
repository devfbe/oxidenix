/* Threads: pthreads on clone/futex, shared memory and descriptors, TLS,
 * thread and process signals, group exit, fork and exec from threads,
 * vfork and posix_spawn, and TLB coherence (munmap and mprotect while
 * another thread on another CPU uses the memory). */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

extern char **environ;

static int failures;

static void check(const char *name, int ok) {
    printf("%-52s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static pid_t gettid_(void) { return (pid_t)syscall(SYS_gettid); }

/* Runs `f` in a child process and returns its wait status. */
static int in_child(void (*f)(void)) {
    pid_t pid = fork();
    if (pid == 0) {
        f();
        _exit(0);
    }
    int st;
    waitpid(pid, &st, 0);
    return st;
}

/* ---- basics ---- */

static void *ids(void *arg) {
    pid_t *out = arg;
    out[0] = getpid();
    out[1] = gettid_();
    return (void *)(intptr_t)42;
}

static __thread int tls_value = 1;

static void *tls_thread(void *arg) {
    tls_value = (int)(intptr_t)arg;
    sched_yield();
    return (void *)(intptr_t)tls_value;
}

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static long counter;
#define ROUNDS 20000

static void *count(void *arg) {
    (void)arg;
    for (int i = 0; i < ROUNDS; i++) {
        pthread_mutex_lock(&lock);
        counter++;
        pthread_mutex_unlock(&lock);
    }
    return NULL;
}

static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static int turn, pings;

static void *pong(void *arg) {
    (void)arg;
    for (int i = 0; i < 100; i++) {
        pthread_mutex_lock(&lock);
        while (turn != 1) pthread_cond_wait(&cond, &lock);
        pings++;
        turn = 0;
        pthread_cond_signal(&cond);
        pthread_mutex_unlock(&lock);
    }
    return NULL;
}

static void *quick(void *arg) { return arg; }

/* ---- process-wide effects ---- */

/* Blocks every signal, so process signals go to the other threads. */
static void *sleeper(void *arg) {
    (void)arg;
    sigset_t all;
    sigfillset(&all);
    pthread_sigmask(SIG_BLOCK, &all, NULL);
    for (;;) pause();
}

static void *exit_three(void *arg) {
    (void)arg;
    usleep(20000);
    exit(3);
}

static void group_exit(void) {
    pthread_t t[3];
    for (int i = 0; i < 2; i++) pthread_create(&t[i], NULL, sleeper, NULL);
    pthread_create(&t[2], NULL, exit_three, NULL);
    for (;;) pause();
}

static void *exit_seven(void *arg) {
    (void)arg;
    usleep(50000);
    exit(7);
}

static void main_exits_first(void) {
    pthread_t t;
    pthread_create(&t, NULL, exit_seven, NULL);
    pthread_exit(NULL);
}

static void killed_by_signal(void) {
    pthread_t t[3];
    for (int i = 0; i < 3; i++) pthread_create(&t[i], NULL, sleeper, NULL);
    for (;;) pause();
}

/* ---- signals ---- */

static volatile pid_t handled_by;

static void on_usr1(int sig) {
    (void)sig;
    handled_by = gettid_();
}

static volatile pid_t worker_tid;
static volatile int worker_ready;

static void *wait_for_signal(void *arg) {
    (void)arg;
    sigset_t none;
    sigemptyset(&none);
    pthread_sigmask(SIG_SETMASK, &none, NULL);
    worker_tid = gettid_();
    worker_ready = 1;
    while (!handled_by) usleep(1000);
    return NULL;
}

/* ---- fork and exec from threads ---- */

static void *fork_from_thread(void *arg) {
    (void)arg;
    pid_t pid = fork();
    if (pid == 0) _exit(getpid() == gettid_() ? 11 : 12);
    int st;
    waitpid(pid, &st, 0);
    return (void *)(intptr_t)(WIFEXITED(st) ? WEXITSTATUS(st) : -1);
}

static void *exec_from_thread(void *arg) {
    (void)arg;
    execl("/bin/sh", "sh", "-c", "exit 5", (char *)NULL);
    return NULL;
}

static void exec_in_thread(void) {
    pthread_t t[2];
    pthread_create(&t[0], NULL, sleeper, NULL);
    pthread_create(&t[1], NULL, exec_from_thread, NULL);
    for (;;) pause();
}

/* ---- TLB coherence ---- */

static volatile char *shared_page;
static volatile long writes;

static void *writer(void *arg) {
    (void)arg;
    for (;;) {
        shared_page[writes % 4096] = 1;
        writes++;
    }
}

static void unmap_while_writing(void) {
    shared_page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    pthread_t t;
    pthread_create(&t, NULL, writer, NULL);
    while (writes < 100000) sched_yield();
    munmap((void *)shared_page, 4096);
    /* The writer must fault now on every CPU: the process dies of SIGSEGV. */
    sleep(2);
    _exit(1);
}

static void protect_while_writing(void) {
    shared_page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    pthread_t t;
    pthread_create(&t, NULL, writer, NULL);
    while (writes < 100000) sched_yield();
    mprotect((void *)shared_page, 4096, PROT_READ);
    sleep(2);
    _exit(1);
}

int main(void) {
    /* Creation and join. */
    pid_t got[2] = {0, 0};
    pthread_t t;
    void *ret = NULL;
    int rc = pthread_create(&t, NULL, ids, got);
    pthread_join(t, &ret);
    check("pthread_create and join return the value", rc == 0 && ret == (void *)42);
    check("a thread shares the pid, has its own tid", got[0] == getpid() && got[1] != getpid() && got[1] > 0);

    pthread_t tt[4];
    for (int i = 0; i < 4; i++) pthread_create(&tt[i], NULL, tls_thread, (void *)(intptr_t)(i + 10));
    int tls_ok = 1;
    for (int i = 0; i < 4; i++) {
        pthread_join(tt[i], &ret);
        tls_ok = tls_ok && ret == (void *)(intptr_t)(i + 10);
    }
    check("thread-local storage is per thread", tls_ok && tls_value == 1);

    for (int i = 0; i < 4; i++) pthread_create(&tt[i], NULL, count, NULL);
    for (int i = 0; i < 4; i++) pthread_join(tt[i], NULL);
    check("4 threads count under a mutex without losses", counter == 4L * ROUNDS);

    pthread_create(&t, NULL, pong, NULL);
    for (int i = 0; i < 100; i++) {
        pthread_mutex_lock(&lock);
        turn = 1;
        pthread_cond_signal(&cond);
        while (turn != 0) pthread_cond_wait(&cond, &lock);
        pthread_mutex_unlock(&lock);
    }
    pthread_join(t, NULL);
    check("condition variables ping-pong 100 times", pings == 100);

    struct timespec deadline;
    clock_gettime(CLOCK_REALTIME, &deadline);
    deadline.tv_nsec += 50 * 1000 * 1000;
    if (deadline.tv_nsec >= 1000000000) {
        deadline.tv_sec++;
        deadline.tv_nsec -= 1000000000;
    }
    pthread_mutex_lock(&lock);
    rc = pthread_cond_timedwait(&cond, &lock, &deadline);
    pthread_mutex_unlock(&lock);
    check("pthread_cond_timedwait times out", rc == ETIMEDOUT);

    int all = 1;
    for (int i = 0; i < 300 && all; i++) {
        all = pthread_create(&t, NULL, quick, (void *)(intptr_t)i) == 0 && pthread_join(t, &ret) == 0 && ret == (void *)(intptr_t)i;
    }
    check("300 threads one after another (exited ones go)", all);

    char status[4096] = {0};
    pthread_create(&tt[0], NULL, sleeper, NULL);
    pthread_create(&tt[1], NULL, sleeper, NULL);
    int fd = open("/proc/self/status", O_RDONLY);
    read(fd, status, sizeof status - 1);
    close(fd);
    check("/proc/self/status counts 3 threads", strstr(status, "Threads:\t3\n") != NULL);

    /* Process-wide effects, each in a child process. */
    int st = in_child(group_exit);
    check("exit() in a thread ends all threads", WIFEXITED(st) && WEXITSTATUS(st) == 3);
    st = in_child(main_exits_first);
    check("the process lives on after its main thread", WIFEXITED(st) && WEXITSTATUS(st) == 7);
    pid_t child = fork();
    if (child == 0) killed_by_signal();
    usleep(50000);
    kill(child, SIGTERM);
    waitpid(child, &st, 0);
    check("a fatal signal ends all threads", WIFSIGNALED(st) && WTERMSIG(st) == SIGTERM);

    child = fork();
    if (child == 0) killed_by_signal();
    usleep(50000);
    kill(child, SIGSTOP);
    pid_t w = waitpid(child, &st, WUNTRACED);
    int stopped = w == child && WIFSTOPPED(st) && WSTOPSIG(st) == SIGSTOP;
    kill(child, SIGCONT);
    w = waitpid(child, &st, WCONTINUED);
    int continued = w == child && WIFCONTINUED(st);
    kill(child, SIGKILL);
    waitpid(child, &st, 0);
    check("SIGSTOP and SIGCONT stop and resume all threads", stopped && continued && WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL);

    /* Signals: to a thread, and to the process (taken by a thread that
     * does not block it). */
    signal(SIGUSR1, on_usr1);
    sigset_t usr1;
    sigemptyset(&usr1);
    sigaddset(&usr1, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &usr1, NULL);
    pthread_create(&t, NULL, wait_for_signal, NULL);
    while (!worker_ready) usleep(1000);
    kill(getpid(), SIGUSR1);
    pthread_join(t, NULL);
    check("a process signal goes to a thread not blocking it", handled_by == worker_tid);
    pthread_sigmask(SIG_UNBLOCK, &usr1, NULL);
    handled_by = 0;
    worker_ready = 0;
    pthread_create(&t, NULL, wait_for_signal, NULL);
    while (!worker_ready) usleep(1000);
    pthread_kill(t, SIGUSR1);
    pthread_join(t, NULL);
    check("pthread_kill signals exactly that thread", handled_by == worker_tid);

    /* fork and exec from threads. */
    pthread_create(&t, NULL, fork_from_thread, NULL);
    pthread_join(t, &ret);
    check("fork in a thread: the child has one thread", ret == (void *)11);
    st = in_child(exec_in_thread);
    check("exec in a thread ends the others, runs the program", WIFEXITED(st) && WEXITSTATUS(st) == 5);

    long r = syscall(SYS_clone, 200UL, 0, 0, 0, 0);
    if (r == 0) _exit(0);
    check("clone refuses an exit signal beyond 64", r == -1 && errno == EINVAL);
    r = syscall(SYS_clone, (unsigned long)SIGCHLD, 0x8000000000000000UL, 0, 0, 0);
    if (r == 0) _exit(0);
    check("clone refuses a stack outside user space", r == -1 && errno == EINVAL);

    /* vfork shares memory until exec or exit; posix_spawn uses it. */
    static volatile int shared_by_vfork;
    shared_by_vfork = 0;
    child = vfork();
    if (child == 0) {
        shared_by_vfork = 1;
        _exit(0);
    }
    waitpid(child, &st, 0);
    check("vfork shares memory with the parent", shared_by_vfork == 1);
    char *argv[] = {"sh", "-c", "exit 9", NULL};
    pid_t sp;
    rc = posix_spawn(&sp, "/bin/sh", NULL, NULL, argv, environ);
    waitpid(sp, &st, 0);
    check("posix_spawn runs a program", rc == 0 && WIFEXITED(st) && WEXITSTATUS(st) == 9);

    /* TLB coherence: the writer runs on another CPU with the page cached. */
    st = in_child(unmap_while_writing);
    check("munmap reaches a thread on another CPU", WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV);
    st = in_child(protect_while_writing);
    check("mprotect reaches a thread on another CPU", WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV);

    printf("threadtest: %s\n", failures ? "FAILED" : "all passed");
    fflush(stdout);
    /* exit_group: the two sleepers end with the process. */
    exit(failures);
}
