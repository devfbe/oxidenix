/* Priority-inheritance futexes and robust futex lists, against Linux's
 * semantics: FUTEX_LOCK_PI, FUTEX_LOCK_PI2, FUTEX_TRYLOCK_PI and
 * FUTEX_UNLOCK_PI (the word's owner and waiter bits, the hand-over to a
 * waiter, EDEADLK, ESRCH, EPERM, EINVAL, timeouts, the restart after a
 * signal, an owner's death), FUTEX_WAIT_REQUEUE_PI with
 * FUTEX_CMP_REQUEUE_PI, the scheduler's priority inheritance (an owner
 * runs with its waiters' weight, along a chain; seen through the test call
 * TEST_PI_WEIGHT), set_robust_list and get_robust_list with the
 * owner-died marking at exit and exec, and musl's PTHREAD_PRIO_INHERIT and
 * robust mutexes. */
#define _GNU_SOURCE
#include <errno.h>
#include <limits.h>
#include <linux/futex.h>
#include <pthread.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef FUTEX_LOCK_PI2
#define FUTEX_LOCK_PI2 13
#endif
#define TEST_PI_WEIGHT 1550

/* The scheduler's weights of some nice values (Linux's sched_prio_to_weight). */
#define W_NICE_M10 9548
#define W_NICE_M5 3121
#define W_NICE_0 1024
#define W_NICE_10 110
#define W_NICE_19 15

#define PRIV FUTEX_PRIVATE_FLAG

static int failures;

static void check(const char *name, int ok) {
    printf("%-64s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static long futex(volatile uint32_t *uaddr, int op, uint32_t val, const struct timespec *ts, volatile uint32_t *uaddr2, uint32_t val3) {
    return syscall(SYS_futex, uaddr, op, val, ts, uaddr2, val3);
}

static uint32_t gettid_(void) { return (uint32_t)syscall(SYS_gettid); }

/* The scheduler's weight of thread `tid` now (0: the caller). */
static long weight(uint32_t tid) { return syscall(TEST_PI_WEIGHT, tid); }

static uint32_t load(volatile uint32_t *w) { return __atomic_load_n(w, __ATOMIC_SEQ_CST); }

/* Waits until the word has waiters in the kernel (they set the bit before
 * they sleep), then a little longer so that they are asleep. */
static void await_waiters(volatile uint32_t *w) {
    for (int i = 0; i < 5000 && !(load(w) & FUTEX_WAITERS); i++) usleep(1000);
    usleep(50000);
}

/* Takes the PI lock at w, in user space or through the kernel. */
static long pi_lock(volatile uint32_t *w, int private) {
    uint32_t zero = 0;
    if (__atomic_compare_exchange_n(w, &zero, gettid_(), 0, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST)) return 0;
    return futex(w, FUTEX_LOCK_PI | private, 0, NULL, NULL, 0);
}

static long pi_unlock(volatile uint32_t *w, int private) {
    uint32_t me = gettid_();
    if (__atomic_compare_exchange_n(w, &me, 0, 0, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST)) return 0;
    return futex(w, FUTEX_UNLOCK_PI | private, 0, NULL, NULL, 0);
}

static void set_nice(int n) { setpriority(PRIO_PROCESS, gettid_(), n); }

static struct timespec in_ms(clockid_t clock, long ms) {
    struct timespec t;
    clock_gettime(clock, &t);
    t.tv_sec += ms / 1000;
    t.tv_nsec += (ms % 1000) * 1000000;
    if (t.tv_nsec >= 1000000000) t.tv_sec++, t.tv_nsec -= 1000000000;
    return t;
}

/* ---------------------------------------------------------------- threads */

/* Takes `lock` (and then waits for `then`, if any), holds what it got
 * until `release` or for `hold_ms`, lets go. */
struct locker {
    volatile uint32_t *lock;
    volatile uint32_t *then;
    int nice;
    long hold_ms;
    volatile uint32_t tid;
    volatile int done;
    volatile long result;
    volatile uint32_t release;
};

static void *locker(void *arg) {
    struct locker *l = arg;
    set_nice(l->nice);
    l->tid = gettid_();
    long r = pi_lock(l->lock, PRIV);
    if (r == 0 && l->then) r = pi_lock(l->then, PRIV);
    l->result = r;
    l->done = 1;
    if (l->hold_ms) usleep(l->hold_ms * 1000);
    else
        while (!load(&l->release)) usleep(1000);
    if (l->then && r == 0) pi_unlock(l->then, PRIV);
    pi_unlock(l->lock, PRIV);
    return NULL;
}

static void await_done(struct locker *l) {
    while (!l->done) usleep(1000);
}

/* Blocks in the futex operation `op` on an owned lock; keeps what it got
 * until `release`. */
struct blocker {
    volatile uint32_t *lock;
    int nice;
    int op;
    volatile uint32_t tid;
    volatile int done;
    volatile long result;
    volatile int err;
    volatile uint32_t word_after;
    volatile uint32_t release;
};

static void *blocker(void *arg) {
    struct blocker *b = arg;
    set_nice(b->nice);
    b->tid = gettid_();
    long r = futex(b->lock, b->op, 0, NULL, NULL, 0);
    b->err = errno;
    b->word_after = load(b->lock);
    b->result = r;
    b->done = 1;
    while (!load(&b->release)) usleep(1000);
    if (r == 0) pi_unlock(b->lock, PRIV);
    return NULL;
}

static void await_blocker(struct blocker *b) {
    while (!b->done) usleep(1000);
}

/* ------------------------------------------------------------- the calls */

static void basics(void) {
    static uint32_t w;
    uint32_t me = gettid_();
    w = 0;
    long r = futex(&w, FUTEX_TRYLOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    check("FUTEX_TRYLOCK_PI takes a free lock (the word is the tid)", r == 0 && w == me);
    r = futex(&w, FUTEX_LOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    check("FUTEX_LOCK_PI of one's own lock fails with EDEADLK", r == -1 && errno == EDEADLK);
    r = futex(&w, FUTEX_TRYLOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    check("FUTEX_TRYLOCK_PI of one's own lock fails with EDEADLK", r == -1 && errno == EDEADLK);
    r = futex(&w, FUTEX_UNLOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    check("FUTEX_UNLOCK_PI without waiters leaves the word 0", r == 0 && w == 0);
    r = futex(&w, FUTEX_UNLOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    check("FUTEX_UNLOCK_PI of a lock one does not own fails with EPERM", r == -1 && errno == EPERM);
    r = futex(&w, FUTEX_LOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    check("FUTEX_LOCK_PI takes a free lock in the kernel", r == 0 && w == me);

    /* An owner id no thread has: ESRCH, after FUTEX_WAITERS is set. */
    w = 0x3ffffff0;
    r = futex(&w, FUTEX_LOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    check("FUTEX_LOCK_PI of a lock whose owner does not exist: ESRCH", r == -1 && errno == ESRCH);
    check("... after setting FUTEX_WAITERS, as Linux", w == (0x3ffffff0 | FUTEX_WAITERS));
    w = 0x3ffffff0;
    r = futex(&w, FUTEX_TRYLOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    check("FUTEX_TRYLOCK_PI likewise: ESRCH", r == -1 && errno == ESRCH);

    /* A dead owner's mark without an owner: the lock is free, the mark stays. */
    w = FUTEX_OWNER_DIED;
    r = futex(&w, FUTEX_TRYLOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    check("FUTEX_TRYLOCK_PI takes an owner-died lock, keeping the mark", r == 0 && w == (FUTEX_OWNER_DIED | me));
    w = 0;

    check("an unaligned PI word fails with EINVAL", futex((uint32_t *)((char *)&w + 1), FUTEX_LOCK_PI_PRIVATE, 0, NULL, NULL, 0) == -1 && errno == EINVAL);
    check("an unmapped PI word fails with EFAULT", futex((uint32_t *)8, FUTEX_LOCK_PI_PRIVATE, 0, NULL, NULL, 0) == -1 && errno == EFAULT);
    check("FUTEX_TRYLOCK_PI | FUTEX_CLOCK_REALTIME: ENOSYS", futex(&w, FUTEX_TRYLOCK_PI | FUTEX_CLOCK_REALTIME, 0, NULL, NULL, 0) == -1 && errno == ENOSYS);
    check("FUTEX_LOCK_PI | FUTEX_CLOCK_REALTIME: ENOSYS", futex(&w, FUTEX_LOCK_PI | FUTEX_CLOCK_REALTIME, 0, NULL, NULL, 0) == -1 && errno == ENOSYS);
}

static void timeouts(void) {
    static uint32_t w;
    struct locker owner = {.lock = &w};
    pthread_t t;
    pthread_create(&t, NULL, locker, &owner);
    await_done(&owner);

    struct timespec start, end, at = in_ms(CLOCK_REALTIME, 50);
    clock_gettime(CLOCK_MONOTONIC, &start);
    long r = futex(&w, FUTEX_LOCK_PI_PRIVATE, 0, &at, NULL, 0);
    int e = errno;
    clock_gettime(CLOCK_MONOTONIC, &end);
    long ms = (end.tv_sec - start.tv_sec) * 1000 + (end.tv_nsec - start.tv_nsec) / 1000000;
    check("FUTEX_LOCK_PI times out at an absolute wall-clock time", r == -1 && e == ETIMEDOUT && ms >= 40 && ms < 1000);
    check("... the word keeps its owner, with FUTEX_WAITERS", w == (owner.tid | FUTEX_WAITERS));

    at = in_ms(CLOCK_MONOTONIC, 50);
    r = futex(&w, FUTEX_LOCK_PI2 | PRIV, 0, &at, NULL, 0);
    check("FUTEX_LOCK_PI2 times out at an absolute monotonic time", r == -1 && errno == ETIMEDOUT);
    at = in_ms(CLOCK_REALTIME, 50);
    r = futex(&w, FUTEX_LOCK_PI2 | PRIV | FUTEX_CLOCK_REALTIME, 0, &at, NULL, 0);
    check("FUTEX_LOCK_PI2 | FUTEX_CLOCK_REALTIME on the wall clock", r == -1 && errno == ETIMEDOUT);
    r = futex(&w, FUTEX_TRYLOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    check("FUTEX_TRYLOCK_PI of an owned lock: EAGAIN", r == -1 && errno == EAGAIN);
    struct timespec bad = {0, 1000000000};
    check("a bad timeout fails with EINVAL", futex(&w, FUTEX_LOCK_PI_PRIVATE, 0, &bad, NULL, 0) == -1 && errno == EINVAL);
    owner.release = 1;
    pthread_join(t, NULL);
    check("the owner's unlock through the kernel leaves the word 0", w == 0);
}

static void handover(void) {
    static uint32_t w;
    uint32_t me = gettid_();
    w = me;
    struct blocker b = {.lock = &w, .op = FUTEX_LOCK_PI_PRIVATE};
    pthread_t t;
    pthread_create(&t, NULL, blocker, &b);
    await_waiters(&w);
    check("a waiter sets FUTEX_WAITERS", w == (me | FUTEX_WAITERS));
    /* User space cannot unlock now: the kernel hands the lock over. */
    long r = futex(&w, FUTEX_UNLOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    await_blocker(&b);
    check("FUTEX_UNLOCK_PI hands the lock to the waiter", r == 0 && b.result == 0 && b.word_after == (b.tid | FUTEX_WAITERS));
    b.release = 1;
    pthread_join(t, NULL);
    check("... whose own unlock (through the kernel) frees it", w == 0);
}

static void *plain_waiter(void *arg) {
    struct timespec ts = {1, 0};
    futex(arg, FUTEX_WAIT_PRIVATE, load(arg), &ts, NULL, 0);
    return NULL;
}

static void mixed(void) {
    static uint32_t w;
    w = 0x3ffffff0;
    pthread_t t;
    pthread_create(&t, NULL, plain_waiter, &w);
    usleep(100000);
    long r = futex(&w, FUTEX_LOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    check("FUTEX_LOCK_PI of a word with a FUTEX_WAIT waiter: EINVAL", r == -1 && errno == EINVAL);
    futex(&w, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
    pthread_join(t, NULL);
}

static void inheritance(void) {
    static uint32_t a, b;
    check("TEST_PI_WEIGHT: a nice-0 thread runs with weight 1024", weight(0) == W_NICE_0);

    /* A nice-19 owner, a nice -5 waiter. */
    a = 0;
    struct locker low = {.lock = &a, .nice = 19};
    pthread_t tl, th;
    pthread_create(&tl, NULL, locker, &low);
    await_done(&low);
    check("a nice-19 owner runs with weight 15", weight(low.tid) == W_NICE_19);
    struct blocker hi = {.lock = &a, .nice = -5, .op = FUTEX_LOCK_PI_PRIVATE};
    pthread_create(&th, NULL, blocker, &hi);
    await_waiters(&a);
    check("a nice -5 waiter lends the owner its weight (3121)", weight(low.tid) == W_NICE_M5);
    setpriority(PRIO_PROCESS, hi.tid, -10);
    check("... its new one when its nice value changes (9548)", weight(low.tid) == W_NICE_M10);
    setpriority(PRIO_PROCESS, hi.tid, 10);
    check("... a lesser waiter's weight (110) over the owner's own", weight(low.tid) == W_NICE_10);
    low.release = 1;
    pthread_join(tl, NULL);
    await_blocker(&hi);
    check("the unlock hands the lock to the waiter", hi.result == 0 && hi.word_after == (hi.tid | FUTEX_WAITERS));
    check("... which inherits nothing (nobody waits for it)", weight(hi.tid) == W_NICE_10);
    hi.release = 1;
    pthread_join(th, NULL);

    /* A chain: L (nice 19) owns a; M (nice 10) owns b and waits for a; H
     * (nice -10) waits for b: M and L run with H's weight. */
    a = b = 0;
    struct locker l = {.lock = &a, .nice = 19};
    struct locker m = {.lock = &b, .then = &a, .nice = 10};
    struct blocker h = {.lock = &b, .nice = -10, .op = FUTEX_LOCK_PI_PRIVATE};
    pthread_t t1, t2, t3;
    pthread_create(&t1, NULL, locker, &l);
    await_done(&l);
    pthread_create(&t2, NULL, locker, &m);
    await_waiters(&a);
    check("chain: M (nice 10) waiting lends L its weight (110)", weight(l.tid) == W_NICE_10);
    pthread_create(&t3, NULL, blocker, &h);
    await_waiters(&b);
    check("chain: H (nice -10) waiting for M's lock boosts M (9548)", weight(m.tid) == W_NICE_M10);
    check("chain: ... and L, whose lock M waits for (9548)", weight(l.tid) == W_NICE_M10);

    /* A cycle: the caller owns c, D owns d and waits for c; the caller
     * asking for d would wait for itself. */
    static uint32_t c, d;
    c = gettid_();
    d = 0;
    struct locker dl = {.lock = &d, .then = &c};
    pthread_t t4;
    pthread_create(&t4, NULL, locker, &dl);
    await_waiters(&c);
    long r = futex(&d, FUTEX_LOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    check("waiting for a lock whose owner waits for one's own: EDEADLK", r == -1 && errno == EDEADLK);
    pi_unlock(&c, PRIV);
    await_done(&dl);
    dl.release = 1;
    pthread_join(t4, NULL);

    l.release = 1;
    pthread_join(t1, NULL);
    await_done(&m);
    check("chain: M gets a when L lets go", m.result == 0);
    check("chain: M inherits H's weight still, L its own again", weight(m.tid) == W_NICE_M10);
    m.release = 1;
    pthread_join(t2, NULL);
    await_blocker(&h);
    check("chain: H gets b when M lets go", h.result == 0 && h.word_after == (h.tid | FUTEX_WAITERS));
    h.release = 1;
    pthread_join(t3, NULL);
    check("chain: every word is free again", a == 0 && b == 0 && c == 0 && d == 0);
}

static volatile int handled;
static void on_alarm(int sig) { (void)sig; handled++; }

static void restart(void) {
    static uint32_t w;
    w = 0;
    struct locker owner = {.lock = &w, .hold_ms = 300};
    pthread_t t;
    pthread_create(&t, NULL, locker, &owner);
    await_done(&owner);
    struct sigaction sa = {0};
    sa.sa_handler = on_alarm; /* no SA_RESTART: FUTEX_LOCK_PI restarts anyway */
    sigaction(SIGALRM, &sa, NULL);
    handled = 0;
    ualarm(50000, 0);
    long r = futex(&w, FUTEX_LOCK_PI_PRIVATE, 0, NULL, NULL, 0);
    check("FUTEX_LOCK_PI goes on after a handler without SA_RESTART", r == 0 && handled == 1 && (w & FUTEX_TID_MASK) == gettid_());
    pi_unlock(&w, PRIV);
    sa.sa_handler = SIG_DFL;
    sigaction(SIGALRM, &sa, NULL);
    pthread_join(t, NULL);
}

/* An owner's death hands the lock to a waiter, with FUTEX_OWNER_DIED. */
static void owner_death(void) {
    volatile uint32_t *sw = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    pid_t child = fork();
    if (child == 0) {
        sw[0] = gettid_();
        /* Dies holding it once the parent waits for it. */
        for (int i = 0; i < 5000 && !(load(sw) & FUTEX_WAITERS); i++) usleep(1000);
        usleep(50000);
        _exit(0);
    }
    while (!load(sw)) usleep(1000);
    long r = futex(sw, FUTEX_LOCK_PI, 0, NULL, NULL, 0);
    uint32_t word = load(sw);
    check("an owner's death hands a shared PI lock to its waiter", r == 0);
    check("... which finds FUTEX_OWNER_DIED | FUTEX_WAITERS | its tid", word == (FUTEX_OWNER_DIED | FUTEX_WAITERS | gettid_()));
    r = futex(sw, FUTEX_UNLOCK_PI, 0, NULL, NULL, 0);
    check("... and lets go of it (the word is 0)", r == 0 && load(sw) == 0);
    waitpid(child, NULL, 0);
    munmap((void *)sw, 4096);
}

/* --------------------------------------------------------- requeue to PI */

struct cond_waiter {
    volatile uint32_t *cond, *lock;
    volatile uint32_t tid;
    volatile int done;
    volatile long result;
    volatile int err;
    volatile uint32_t word;
    volatile uint32_t *arrived;
};

static void *cond_waiter(void *arg) {
    struct cond_waiter *c = arg;
    c->tid = gettid_();
    __atomic_add_fetch(c->arrived, 1, __ATOMIC_SEQ_CST);
    long r = futex(c->cond, FUTEX_WAIT_REQUEUE_PI_PRIVATE, 0, NULL, c->lock, 0);
    c->err = errno;
    c->word = load(c->lock);
    c->result = r;
    c->done = 1;
    if (r == 0) {
        usleep(20000);
        pi_unlock(c->lock, PRIV);
    }
    return NULL;
}

static void requeue_pi(void) {
    static uint32_t cond, lock, other, arrived;
    cond = lock = other = arrived = 0;
    long r = futex(&cond, FUTEX_WAIT_REQUEUE_PI_PRIVATE, 1, NULL, &lock, 0);
    check("FUTEX_WAIT_REQUEUE_PI on a changed value: EAGAIN", r == -1 && errno == EAGAIN);
    r = futex(&cond, FUTEX_WAIT_REQUEUE_PI_PRIVATE, 0, NULL, &cond, 0);
    check("FUTEX_WAIT_REQUEUE_PI to the same word: EINVAL", r == -1 && errno == EINVAL);
    struct timespec at = in_ms(CLOCK_MONOTONIC, 50);
    r = futex(&cond, FUTEX_WAIT_REQUEUE_PI_PRIVATE, 0, &at, &lock, 0);
    check("FUTEX_WAIT_REQUEUE_PI times out (absolute, monotonic)", r == -1 && errno == ETIMEDOUT);

    struct cond_waiter w[2];
    pthread_t t[2];
    for (int i = 0; i < 2; i++) {
        w[i] = (struct cond_waiter){.cond = &cond, .lock = &lock, .arrived = &arrived};
        pthread_create(&t[i], NULL, cond_waiter, &w[i]);
    }
    while (load(&arrived) < 2) usleep(1000);
    usleep(100000);
    r = futex(&cond, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
    check("FUTEX_WAKE of a requeue-PI waiter: EINVAL", r == -1 && errno == EINVAL);
    r = futex(&cond, FUTEX_CMP_REQUEUE_PRIVATE, 1, (void *)1, &other, 0);
    check("FUTEX_CMP_REQUEUE of a requeue-PI waiter: EINVAL", r == -1 && errno == EINVAL);
    r = futex(&cond, FUTEX_CMP_REQUEUE_PI_PRIVATE, 2, (void *)INT_MAX, &lock, 0);
    check("FUTEX_CMP_REQUEUE_PI waking more than one: EINVAL", r == -1 && errno == EINVAL);
    r = futex(&cond, FUTEX_CMP_REQUEUE_PI_PRIVATE, 1, (void *)INT_MAX, &cond, 0);
    check("FUTEX_CMP_REQUEUE_PI to the same word: EINVAL", r == -1 && errno == EINVAL);
    r = futex(&cond, FUTEX_CMP_REQUEUE_PI_PRIVATE, 1, (void *)INT_MAX, &lock, 1);
    check("FUTEX_CMP_REQUEUE_PI on a changed value: EAGAIN", r == -1 && errno == EAGAIN);
    r = futex(&cond, FUTEX_CMP_REQUEUE_PI_PRIVATE, 1, (void *)INT_MAX, &other, 0);
    check("FUTEX_CMP_REQUEUE_PI to another lock than the waiters': EINVAL", r == -1 && errno == EINVAL);
    r = futex(&cond, FUTEX_CMP_REQUEUE_PI_PRIVATE, 1, (void *)INT_MAX, &lock, 0);
    check("FUTEX_CMP_REQUEUE_PI wakes one (owning the lock), moves one", r == 2);
    for (int i = 0; i < 2; i++) pthread_join(t[i], NULL);
    int ok = 1;
    for (int i = 0; i < 2; i++) ok = ok && w[i].result == 0 && (w[i].word & FUTEX_TID_MASK) == w[i].tid;
    check("... both return owning the lock in turn", ok);
    check("... which is free in the end", lock == 0);

    /* A lock that is owned: both are moved, then handed it in turn. */
    arrived = 0;
    lock = gettid_();
    for (int i = 0; i < 2; i++) {
        w[i] = (struct cond_waiter){.cond = &cond, .lock = &lock, .arrived = &arrived};
        pthread_create(&t[i], NULL, cond_waiter, &w[i]);
    }
    while (load(&arrived) < 2) usleep(1000);
    usleep(100000);
    r = futex(&cond, FUTEX_CMP_REQUEUE_PI_PRIVATE, 1, (void *)0, &lock, 0);
    check("FUTEX_CMP_REQUEUE_PI with the lock owned moves one (n=0)", r == 1 && lock == (gettid_() | FUTEX_WAITERS));
    r = futex(&cond, FUTEX_CMP_REQUEUE_PI_PRIVATE, 1, (void *)INT_MAX, &lock, 0);
    check("... and the other", r == 1);
    pi_unlock(&lock, PRIV);
    for (int i = 0; i < 2; i++) pthread_join(t[i], NULL);
    ok = 1;
    for (int i = 0; i < 2; i++) ok = ok && w[i].result == 0 && (w[i].word & FUTEX_TID_MASK) == w[i].tid;
    check("... both get it as it is let go of", ok && lock == 0);
}

/* ------------------------------------------------------ pthread mutexes */

static pthread_mutex_t pim;
static long counter;

static void *count_up(void *arg) {
    (void)arg;
    for (int i = 0; i < 20000; i++) {
        pthread_mutex_lock(&pim);
        counter++;
        pthread_mutex_unlock(&pim);
    }
    return NULL;
}

struct mutex_holder {
    pthread_mutex_t *m;
    int nice;
    volatile uint32_t tid;
    volatile int locked;
    volatile int result;
    volatile uint32_t release;
};

static void *hold_mutex(void *arg) {
    struct mutex_holder *h = arg;
    set_nice(h->nice);
    h->tid = gettid_();
    h->result = pthread_mutex_lock(h->m);
    h->locked = 1;
    while (!load(&h->release)) usleep(1000);
    pthread_mutex_unlock(h->m);
    return NULL;
}

static void pi_mutexes(void) {
    pthread_mutexattr_t a;
    pthread_mutexattr_init(&a);
    int r = pthread_mutexattr_setprotocol(&a, PTHREAD_PRIO_INHERIT);
    check("pthread_mutexattr_setprotocol(PTHREAD_PRIO_INHERIT)", r == 0);
    pthread_mutex_init(&pim, &a);
    pthread_t t[4];
    counter = 0;
    for (int i = 0; i < 4; i++) pthread_create(&t[i], NULL, count_up, NULL);
    for (int i = 0; i < 4; i++) pthread_join(t[i], NULL);
    check("a PI mutex keeps four threads' counting exact", counter == 4 * 20000);

    /* A nice-19 holder runs with the weight of a nice -5 thread blocked in
     * pthread_mutex_lock. */
    struct mutex_holder low = {.m = &pim, .nice = 19};
    pthread_t tl, th;
    pthread_create(&tl, NULL, hold_mutex, &low);
    while (!low.locked) usleep(1000);
    check("pthread_mutex_trylock of a held PI mutex: EBUSY", pthread_mutex_trylock(&pim) == EBUSY);
    struct mutex_holder hi = {.m = &pim, .nice = -5};
    pthread_create(&th, NULL, hold_mutex, &hi);
    volatile uint32_t *word = (volatile uint32_t *)&pim + 1; /* musl's _m_lock */
    await_waiters(word);
    check("a PI mutex's holder inherits its blocked waiter's weight", weight(low.tid) == W_NICE_M5);
    low.release = 1;
    pthread_join(tl, NULL);
    while (!hi.locked) usleep(1000);
    check("... and the waiter gets the mutex", hi.result == 0 && weight(low.tid) < 0);
    hi.release = 1;
    pthread_join(th, NULL);
    pthread_mutex_destroy(&pim);
}

/* A process-shared robust mutex whose owner process dies: EOWNERDEAD, and
 * consistent again once marked so. With `pi` a PI mutex, whose waiter
 * blocks in the kernel while the owner dies. */
static void robust_mutex(int pi) {
    pthread_mutex_t *m = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    volatile uint32_t *word = (volatile uint32_t *)m + 1; /* musl's _m_lock */
    pthread_mutexattr_t a;
    pthread_mutexattr_init(&a);
    pthread_mutexattr_setpshared(&a, PTHREAD_PROCESS_SHARED);
    pthread_mutexattr_setrobust(&a, PTHREAD_MUTEX_ROBUST);
    if (pi) pthread_mutexattr_setprotocol(&a, PTHREAD_PRIO_INHERIT);
    pthread_mutex_init(m, &a);
    pid_t child = fork();
    if (child == 0) {
        pthread_mutex_lock(m);
        /* Dies holding it: with a PI mutex once the parent waits in the kernel. */
        if (pi)
            for (int i = 0; i < 5000 && !(load(word) & FUTEX_WAITERS); i++) usleep(1000);
        usleep(50000);
        _exit(0);
    }
    while (!(load(word) & FUTEX_TID_MASK)) usleep(1000);
    int r = pthread_mutex_lock(m);
    char name[96];
    snprintf(name, sizeof name, "a dead owner's robust%s mutex: EOWNERDEAD", pi ? " PI" : "");
    check(name, r == EOWNERDEAD);
    check("... pthread_mutex_consistent, unlock, lock again", pthread_mutex_consistent(m) == 0 && pthread_mutex_unlock(m) == 0 && pthread_mutex_lock(m) == 0 && pthread_mutex_unlock(m) == 0);
    waitpid(child, NULL, 0);
    munmap(m, 4096);
}

/* --------------------------------------------------------- robust lists */

struct robust_list_ {
    struct robust_list_ *next;
};
struct robust_head_ {
    struct robust_list_ list;
    long futex_offset;
    struct robust_list_ *pending;
};

/* A robust list in shared memory: three entries, each followed by its word. */
struct entry {
    struct robust_list_ link;
    uint32_t word;
};
struct shared {
    struct robust_head_ head;
    struct entry e[4];
    volatile uint32_t go;
    volatile uint32_t ready;
};

static void *wait_word(void *arg) {
    volatile uint32_t *w = arg;
    long r = futex(w, FUTEX_WAIT, load(w), NULL, NULL, 0);
    return (void *)r;
}

static void robust_lists(void) {
    struct robust_head_ h0;
    check("set_robust_list with a wrong size: EINVAL", syscall(SYS_set_robust_list, &h0, sizeof h0 - 1) == -1 && errno == EINVAL);
    void *head;
    size_t len;
    long r = syscall(SYS_get_robust_list, 0, &head, &len);
    check("get_robust_list of the caller: its head, size 24", r == 0 && len == 24);
    check("get_robust_list of no thread: ESRCH", syscall(SYS_get_robust_list, 0x3ffffff0, &head, &len) == -1 && errno == ESRCH);
    check("get_robust_list to an unmapped place: EFAULT", syscall(SYS_get_robust_list, 0, (void *)8, &len) == -1 && errno == EFAULT);

    struct shared *s = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    pid_t child = fork();
    if (child == 0) {
        uint32_t me = gettid_();
        /* e0: a plain lock with waiters; e1: a PI lock (bit 0 of the link);
         * e2: another thread's; e3: the pending one, unlocked (0). */
        s->e[0].word = me | FUTEX_WAITERS;
        s->e[1].word = me;
        s->e[2].word = 12345;
        s->e[3].word = 0;
        s->head.list.next = &s->e[0].link;
        s->e[0].link.next = (struct robust_list_ *)((uintptr_t)&s->e[1].link | 1);
        s->e[1].link.next = &s->e[2].link;
        s->e[2].link.next = &s->head.list;
        s->head.futex_offset = offsetof(struct entry, word);
        s->head.pending = &s->e[3].link;
        long r = syscall(SYS_set_robust_list, &s->head, sizeof s->head);
        void *got;
        size_t n;
        syscall(SYS_get_robust_list, 0, &got, &n);
        s->ready = (r == 0 && got == &s->head) ? 1 : 2;
        while (!load(&s->go)) usleep(1000);
        _exit(0);
    }
    while (!load(&s->ready)) usleep(1000);
    check("set_robust_list, and get_robust_list gives it back", s->ready == 1);
    void *h;
    check("get_robust_list of another thread (as root)", syscall(SYS_get_robust_list, child, &h, &len) == 0 && h == &s->head);
    pthread_t t0, t3;
    pthread_create(&t0, NULL, wait_word, &s->e[0].word);
    pthread_create(&t3, NULL, wait_word, &s->e[3].word);
    usleep(100000);
    s->go = 1;
    waitpid(child, NULL, 0);
    void *r0, *r3;
    pthread_join(t0, &r0);
    pthread_join(t3, &r3);
    check("at exit a held lock gets FUTEX_OWNER_DIED, its waiters bit kept", s->e[0].word == (FUTEX_OWNER_DIED | FUTEX_WAITERS));
    check("... and one waiter woken", (long)r0 == 0);
    check("a held PI lock gets FUTEX_OWNER_DIED", s->e[1].word == FUTEX_OWNER_DIED);
    check("another thread's lock is left alone", s->e[2].word == 12345);
    check("the pending entry's free word wakes a waiter", s->e[3].word == 0 && (long)r3 == 0);

    /* exec: the old program's robust locks are marked too. */
    memset(s, 0, sizeof *s);
    child = fork();
    if (child == 0) {
        s->e[0].word = gettid_();
        s->head.list.next = &s->e[0].link;
        s->e[0].link.next = &s->head.list;
        s->head.futex_offset = offsetof(struct entry, word);
        syscall(SYS_set_robust_list, &s->head, sizeof s->head);
        execl("/bin/true", "true", (char *)NULL);
        _exit(127);
    }
    int st;
    waitpid(child, &st, 0);
    check("execve marks the old program's robust locks", WIFEXITED(st) && WEXITSTATUS(st) == 0 && s->e[0].word == FUTEX_OWNER_DIED);
    munmap(s, 4096);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    basics();
    timeouts();
    handover();
    mixed();
    inheritance();
    restart();
    owner_death();
    requeue_pi();
    pi_mutexes();
    robust_mutex(0);
    robust_mutex(1);
    robust_lists();
    printf("pitest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
