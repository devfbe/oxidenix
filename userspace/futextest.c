/* futex(2): waiting and waking on private and shared words, timeouts,
 * bitsets, requeueing and interruption by signals. Uses processes with
 * shared memory, so it works without threads. */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-52s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static long futex(volatile uint32_t *uaddr, int op, uint32_t val, const struct timespec *ts, volatile uint32_t *uaddr2, uint32_t val3) {
    return syscall(SYS_futex, uaddr, op, val, ts, uaddr2, val3);
}

static long ms_since(struct timespec *start) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (now.tv_sec - start->tv_sec) * 1000 + (now.tv_nsec - start->tv_nsec) / 1000000;
}

static void on_alarm(int sig) { (void)sig; }

/* Waits until *counter reaches n, then a little longer so that the
 * processes that counted are asleep in futex_wait. */
static void await_sleepers(volatile uint32_t *counter, uint32_t n) {
    while (__atomic_load_n(counter, __ATOMIC_SEQ_CST) < n) usleep(1000);
    usleep(100000);
}

int main(void) {
    static uint32_t word;
    struct timespec ts = {0, 50 * 1000 * 1000}, start;

    word = 1;
    check("FUTEX_WAIT on a changed value fails with EAGAIN", futex(&word, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0) == -1 && errno == EAGAIN);
    check("FUTEX_WAKE without waiters wakes nobody", futex(&word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0) == 0);
    clock_gettime(CLOCK_MONOTONIC, &start);
    long r = futex(&word, FUTEX_WAIT_PRIVATE, 1, &ts, NULL, 0);
    long waited = ms_since(&start);
    check("FUTEX_WAIT times out with ETIMEDOUT after 50 ms", r == -1 && errno == ETIMEDOUT && waited >= 40 && waited < 1000);
    check("an unaligned futex fails with EINVAL", futex((uint32_t *)((char *)&word + 1), FUTEX_WAIT, 1, &ts, NULL, 0) == -1 && errno == EINVAL);
    check("an unmapped futex fails with EFAULT", futex((uint32_t *)8, FUTEX_WAIT, 1, &ts, NULL, 0) == -1 && errno == EFAULT);

    struct sigaction sa = {0};
    sa.sa_handler = on_alarm; /* no SA_RESTART */
    sigaction(SIGALRM, &sa, NULL);
    ualarm(50000, 0);
    r = futex(&word, FUTEX_WAIT_PRIVATE, 1, NULL, NULL, 0);
    check("a signal interrupts FUTEX_WAIT with EINTR", r == -1 && errno == EINTR);

    /* Shared memory: the word lives at different addresses in no process,
     * but the key is the shared object, so a waiter in another process
     * is found. */
    volatile uint32_t *shared = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    volatile uint32_t *f1 = shared, *f2 = shared + 1, *counter = shared + 2;
    pid_t child = fork();
    if (child == 0) {
        __atomic_add_fetch(counter, 1, __ATOMIC_SEQ_CST);
        long r = futex(f1, FUTEX_WAIT, 0, NULL, NULL, 0);
        _exit(r == 0 ? 0 : 1);
    }
    await_sleepers(counter, 1);
    *f1 = 1;
    long n = futex(f1, FUTEX_WAKE, 1, NULL, NULL, 0);
    int st;
    waitpid(child, &st, 0);
    check("a shared futex wakes a waiter in another process", n == 1 && WIFEXITED(st) && WEXITSTATUS(st) == 0);

    /* Private memory after fork is a different word in each process. */
    static volatile uint32_t mine;
    mine = 0;
    child = fork();
    if (child == 0) {
        struct timespec t = {0, 300 * 1000 * 1000};
        long r = futex(&mine, FUTEX_WAIT, 0, &t, NULL, 0);
        _exit(r == -1 && errno == ETIMEDOUT ? 0 : 1);
    }
    usleep(100000);
    n = futex(&mine, FUTEX_WAKE, 1, NULL, NULL, 0);
    waitpid(child, &st, 0);
    check("private memory is not shared by futex keys", n == 0 && WIFEXITED(st) && WEXITSTATUS(st) == 0);

    /* Bitsets: a wakeup reaches only waiters with a common bit. */
    *f1 = 0;
    *counter = 0;
    child = fork();
    if (child == 0) {
        __atomic_add_fetch(counter, 1, __ATOMIC_SEQ_CST);
        long r = futex(f1, FUTEX_WAIT_BITSET, 0, NULL, NULL, 1);
        _exit(r == 0 ? 0 : 1);
    }
    await_sleepers(counter, 1);
    long miss = futex(f1, FUTEX_WAKE_BITSET, 1, NULL, NULL, 2);
    long hit = futex(f1, FUTEX_WAKE_BITSET, 1, NULL, NULL, 1);
    waitpid(child, &st, 0);
    check("FUTEX_WAKE_BITSET wakes only matching waiters", miss == 0 && hit == 1 && WIFEXITED(st) && WEXITSTATUS(st) == 0);

    /* Requeue: one of two waiters is woken, the other moves to f2. */
    *f1 = 0;
    *counter = 0;
    pid_t kids[2];
    for (int i = 0; i < 2; i++) {
        kids[i] = fork();
        if (kids[i] == 0) {
            __atomic_add_fetch(counter, 1, __ATOMIC_SEQ_CST);
            long r = futex(f1, FUTEX_WAIT, 0, NULL, NULL, 0);
            _exit(r == 0 ? 0 : 1);
        }
    }
    await_sleepers(counter, 2);
    long stale = futex(f1, FUTEX_CMP_REQUEUE, 1, (void *)1, f2, 7);
    int stale_errno = errno;
    long moved = futex(f1, FUTEX_CMP_REQUEUE, 1, (void *)1, f2, 0);
    long left = futex(f1, FUTEX_WAKE, 1, NULL, NULL, 0);
    long on_f2 = futex(f2, FUTEX_WAKE, 1, NULL, NULL, 0);
    int ok = 1;
    for (int i = 0; i < 2; i++) {
        waitpid(kids[i], &st, 0);
        ok = ok && WIFEXITED(st) && WEXITSTATUS(st) == 0;
    }
    check("FUTEX_CMP_REQUEUE fails with EAGAIN on a changed value", stale == -1 && stale_errno == EAGAIN);
    check("FUTEX_CMP_REQUEUE wakes one and moves one", moved == 2 && left == 0 && on_f2 == 1 && ok);

    printf("futextest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
