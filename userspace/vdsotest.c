/* The vDSO (ADR 0014): AT_SYSINFO_EHDR names an ELF shared object
 * (linux-vdso.so.1) whose five functions are found by name and the version
 * LINUX_2.6, as musl and glibc look them up; every clock it answers agrees
 * with the system call (monotonic across threads and CPUs, the wall clock
 * following clock_settime and settimeofday at once, boot time, the coarse
 * clocks and their resolution), the CPU-time, alarm and invalid clocks and a
 * time zone are the system call's (EINVAL as Linux's), getcpu agrees with
 * the system call on every CPU, /proc/self/maps shows [vvar] and [vdso], fork
 * and exec keep it, the clock page cannot be written, and reading the clocks
 * makes no system call. */
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

typedef int (*cgt_t)(clockid_t, struct timespec *);
typedef int (*gtod_t)(struct timeval *, struct timezone *);
typedef time_t (*time_fn)(time_t *);
typedef int (*getres_t)(clockid_t, struct timespec *);
typedef long (*getcpu_t)(unsigned *, unsigned *, void *);

static cgt_t vcgt;
static gtod_t vgtod;
static time_fn vtime;
static getres_t vgetres;
static getcpu_t vgetcpu;

/* The dynamic table of the vDSO at `eh`: its load bias and entries. */
struct dso {
    uintptr_t base;
    const Elf64_Dyn *dyn;
    const char *strings;
    const Elf64_Sym *syms;
    const Elf32_Word *hash;
    const Elf64_Half *versym;
    const Elf64_Verdef *verdef;
    const char *soname;
    int relocs, loads, load_rx;
};

static struct dso parse(const Elf64_Ehdr *eh) {
    struct dso d = {0};
    d.base = (uintptr_t)-1;
    const char *p = (const char *)eh + eh->e_phoff;
    for (int i = 0; i < eh->e_phnum; i++, p += eh->e_phentsize) {
        const Elf64_Phdr *ph = (const Elf64_Phdr *)p;
        if (ph->p_type == PT_LOAD) {
            d.base = (uintptr_t)eh + ph->p_offset - ph->p_vaddr;
            d.loads++;
            d.load_rx = ph->p_flags == (PF_R | PF_X);
        } else if (ph->p_type == PT_DYNAMIC) {
            d.dyn = (const Elf64_Dyn *)((const char *)eh + ph->p_offset);
        }
    }
    if (!d.dyn || d.base == (uintptr_t)-1) return d;
    size_t soname = 0;
    for (const Elf64_Dyn *e = d.dyn; e->d_tag; e++) {
        const void *at = (const void *)(d.base + e->d_un.d_ptr);
        switch (e->d_tag) {
        case DT_STRTAB: d.strings = at; break;
        case DT_SYMTAB: d.syms = at; break;
        case DT_HASH: d.hash = at; break;
        case DT_VERSYM: d.versym = at; break;
        case DT_VERDEF: d.verdef = at; break;
        case DT_SONAME: soname = e->d_un.d_val; break;
        case DT_REL: case DT_RELA: case DT_JMPREL: case DT_TEXTREL: d.relocs++; break;
        }
    }
    if (d.strings) d.soname = d.strings + soname;
    return d;
}

/* musl's __vdsosym: a defined function or object `name` whose version is `version`. */
static void *lookup(const struct dso *d, const char *name, const char *version) {
    if (!d->strings || !d->syms || !d->hash) return NULL;
    for (Elf32_Word i = 0; i < d->hash[1]; i++) {
        const Elf64_Sym *s = &d->syms[i];
        int type = ELF64_ST_TYPE(s->st_info), bind = ELF64_ST_BIND(s->st_info);
        if (type != STT_FUNC && type != STT_NOTYPE && type != STT_OBJECT) continue;
        if (bind != STB_GLOBAL && bind != STB_WEAK) continue;
        if (!s->st_shndx || strcmp(name, d->strings + s->st_name)) continue;
        if (d->versym && d->verdef) {
            int want = d->versym[i] & 0x7fff, found = 0;
            const Elf64_Verdef *def = d->verdef;
            for (;;) {
                if (!(def->vd_flags & VER_FLG_BASE) && (def->vd_ndx & 0x7fff) == want) {
                    const Elf64_Verdaux *aux = (const Elf64_Verdaux *)((const char *)def + def->vd_aux);
                    found = !strcmp(version, d->strings + aux->vda_name);
                    break;
                }
                if (!def->vd_next) break;
                def = (const Elf64_Verdef *)((const char *)def + def->vd_next);
            }
            if (!found) continue;
        }
        return (void *)(d->base + s->st_value);
    }
    return NULL;
}

static int64_t ts_ns(struct timespec ts) { return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec; }

static int64_t sys_ns(clockid_t c) {
    struct timespec ts;
    if (syscall(SYS_clock_gettime, c, &ts) != 0) return -1;
    return ts_ns(ts);
}

static int64_t v_ns(clockid_t c) {
    struct timespec ts;
    if (vcgt(c, &ts) != 0) return -1;
    return ts_ns(ts);
}

static void pin(int cpu) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    sched_setaffinity(0, sizeof set, &set);
}

static void unpin(void) {
    cpu_set_t all;
    CPU_ZERO(&all);
    for (int i = 0; i < 64; i++) CPU_SET(i, &all);
    sched_setaffinity(0, sizeof all, &all);
}

/* The [vvar] and [vdso] lines of /proc/self/maps: their starts and permissions, and
 * whether the names start in Linux's column. */
struct maps {
    unsigned long vvar, vvar_end, vdso, vdso_end;
    char vvar_perm[5], vdso_perm[5];
    int column_ok, stack, exe;
};

static struct maps read_maps(void) {
    struct maps m = {0};
    static char buf[65536];
    int fd = open("/proc/self/maps", O_RDONLY);
    if (fd < 0) return m;
    size_t len = 0;
    ssize_t n;
    while (len < sizeof buf - 1 && (n = read(fd, buf + len, sizeof buf - 1 - len)) > 0) len += n;
    close(fd);
    buf[len] = 0;
    char exe[256] = "";
    ssize_t el = readlink("/proc/self/exe", exe, sizeof exe - 1);
    if (el > 0) exe[el] = 0;
    m.column_ok = 1;
    for (char *line = buf; *line;) {
        char *end = strchr(line, '\n');
        if (!end) break;
        *end = 0;
        unsigned long start, stop, off, ino;
        unsigned major, minor;
        char perm[5];
        int at = 0;
        if (sscanf(line, "%lx-%lx %4s %lx %x:%x %lu %n", &start, &stop, perm, &off, &major, &minor, &ino, &at) == 7 && at > 0) {
            const char *name = line + at;
            if (*name && end - line > 73 && line + 73 != name) m.column_ok = 0;
            if (!strcmp(name, "[vvar]")) {
                m.vvar = start, m.vvar_end = stop;
                memcpy(m.vvar_perm, perm, 5);
            } else if (!strcmp(name, "[vdso]")) {
                m.vdso = start, m.vdso_end = stop;
                memcpy(m.vdso_perm, perm, 5);
            } else if (!strcmp(name, "[stack]")) {
                m.stack = 1;
            } else if (el > 0 && !strcmp(name, exe) && ino != 0) {
                m.exe = 1;
            }
        }
        line = end + 1;
    }
    return m;
}

/* Finds the vDSO's functions; 0 if any is missing. */
static int find(void) {
    const Elf64_Ehdr *eh = (const Elf64_Ehdr *)getauxval(AT_SYSINFO_EHDR);
    if (!eh) return 0;
    struct dso d = parse(eh);
    vcgt = (cgt_t)lookup(&d, "__vdso_clock_gettime", "LINUX_2.6");
    vgtod = (gtod_t)lookup(&d, "__vdso_gettimeofday", "LINUX_2.6");
    vtime = (time_fn)lookup(&d, "__vdso_time", "LINUX_2.6");
    vgetres = (getres_t)lookup(&d, "__vdso_clock_getres", "LINUX_2.6");
    vgetcpu = (getcpu_t)lookup(&d, "__vdso_getcpu", "LINUX_2.6");
    return vcgt && vgtod && vtime && vgetres && vgetcpu;
}

/* What a forked or exec'd program checks of its vDSO: 0 if all is well. */
static int child_checks(void) {
    if (!find()) return 1;
    int64_t a = sys_ns(CLOCK_MONOTONIC), b = v_ns(CLOCK_MONOTONIC), c = sys_ns(CLOCK_MONOTONIC);
    if (!(a <= b && b <= c)) return 2;
    struct maps m = read_maps();
    if (m.vdso != getauxval(AT_SYSINFO_EHDR) || m.vvar != m.vdso - 4096) return 3;
    return 0;
}

/* Cross-CPU monotonicity through the vDSO: one thread publishes its readings, the other
 * (on another CPU) must never read an earlier time afterwards. */
static volatile int64_t published;
static volatile int stop_ping;

static void *publisher(void *arg) {
    (void)arg;
    pin(1);
    while (!stop_ping) __atomic_store_n(&published, v_ns(CLOCK_MONOTONIC), __ATOMIC_SEQ_CST);
    return NULL;
}

/* Threads that move between CPUs (they yield all the time) and check their own readings
 * and the latest of all of them never go back. */
static volatile int64_t latest;
static volatile int wanderers_back;

static void *wanderer(void *arg) {
    (void)arg;
    int64_t prev = 0;
    for (int i = 0; i < 20000; i++) {
        int64_t seen = __atomic_load_n(&latest, __ATOMIC_SEQ_CST);
        int64_t t = v_ns(CLOCK_MONOTONIC);
        if (t < prev || t < seen) wanderers_back = 1;
        prev = t;
        int64_t cur = seen;
        while (t > cur && !__atomic_compare_exchange_n(&latest, &cur, t, 0, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST)) {
        }
        if (i % 64 == 0) sched_yield();
    }
    return NULL;
}

static long counted_syscalls(void) {
    char buf[4096];
    int fd = open("/proc/counters", O_RDONLY);
    if (fd < 0) return -1;
    ssize_t n = read(fd, buf, sizeof buf - 1);
    close(fd);
    if (n <= 0) return -1;
    buf[n] = 0;
    char *at = strstr(buf, "syscalls ");
    return at ? atol(at + 9) : -1;
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "exec-child")) return child_checks();

    /* The object. */
    const Elf64_Ehdr *eh = (const Elf64_Ehdr *)getauxval(AT_SYSINFO_EHDR);
    check("AT_SYSINFO_EHDR names the vDSO", eh != NULL);
    if (!eh) {
        puts("vdsotest: no vDSO");
        return 1;
    }
    check("it is an ELF64 shared object for x86-64", !memcmp(eh->e_ident, ELFMAG, SELFMAG) && eh->e_ident[EI_CLASS] == ELFCLASS64 && eh->e_type == ET_DYN && eh->e_machine == EM_X86_64);
    struct dso d = parse(eh);
    check("one read-only executable PT_LOAD, no relocations", d.loads == 1 && d.load_rx && d.relocs == 0 && d.base == (uintptr_t)eh);
    check("its soname is linux-vdso.so.1", d.soname && !strcmp(d.soname, "linux-vdso.so.1"));
    check("DT_HASH, DT_VERSYM and DT_VERDEF are there", d.hash && d.versym && d.verdef);
    check("the five functions are found with version LINUX_2.6", find());
    const char *aliases[] = {"clock_gettime", "gettimeofday", "time", "clock_getres", "getcpu"};
    const char *full[] = {"__vdso_clock_gettime", "__vdso_gettimeofday", "__vdso_time", "__vdso_clock_getres", "__vdso_getcpu"};
    int same = 1;
    for (int i = 0; i < 5; i++) same &= lookup(&d, aliases[i], "LINUX_2.6") == lookup(&d, full[i], "LINUX_2.6") && lookup(&d, aliases[i], "LINUX_2.6");
    check("the unprefixed names are aliases of them", same);
    check("another version finds nothing", !lookup(&d, "__vdso_clock_gettime", "LINUX_2.5") && !lookup(&d, "__vdso_nothing", "LINUX_2.6"));
    if (failures) {
        puts("vdsotest: the object is broken; stopping");
        return 1;
    }

    /* Every clock it answers, between two system calls. */
    struct { clockid_t id; const char *name; } clocks[] = {
        {CLOCK_REALTIME, "CLOCK_REALTIME"}, {CLOCK_MONOTONIC, "CLOCK_MONOTONIC"}, {CLOCK_MONOTONIC_RAW, "CLOCK_MONOTONIC_RAW"},
        {CLOCK_REALTIME_COARSE, "CLOCK_REALTIME_COARSE"}, {CLOCK_MONOTONIC_COARSE, "CLOCK_MONOTONIC_COARSE"}, {CLOCK_BOOTTIME, "CLOCK_BOOTTIME"},
        {CLOCK_TAI, "CLOCK_TAI"},
    };
    for (size_t i = 0; i < sizeof clocks / sizeof clocks[0]; i++) {
        int ok = 1;
        for (int j = 0; j < 1000 && ok; j++) {
            int64_t a = sys_ns(clocks[i].id), b = v_ns(clocks[i].id), c = sys_ns(clocks[i].id);
            ok = a > 0 && a <= b && b <= c;
        }
        char name[80];
        snprintf(name, sizeof name, "%s lies between two system calls", clocks[i].name);
        check(name, ok);
    }
    int64_t mono = v_ns(CLOCK_MONOTONIC), boot = v_ns(CLOCK_BOOTTIME);
    check("CLOCK_BOOTTIME is CLOCK_MONOTONIC (nothing suspends)", boot >= mono && boot - mono < 10000000);
    int fine = 0;
    for (int i = 0; i < 100; i++) fine |= v_ns(CLOCK_MONOTONIC) % 1000 != 0;
    check("readings have nanosecond resolution", fine);
    struct timespec vres = {9, 9}, sres = {9, 9};
    int res_same = 1;
    for (size_t i = 0; i < sizeof clocks / sizeof clocks[0]; i++) {
        res_same &= vgetres(clocks[i].id, &vres) == 0 && syscall(SYS_clock_getres, clocks[i].id, &sres) == 0 && vres.tv_sec == sres.tv_sec && vres.tv_nsec == sres.tv_nsec;
    }
    check("clock_getres agrees with the system call (also _COARSE)", res_same);
    check("clock_getres with no result pointer is 0", vgetres(CLOCK_MONOTONIC_COARSE, NULL) == 0);

    struct timeval tv;
    int64_t a = sys_ns(CLOCK_REALTIME);
    int gr = vgtod(&tv, NULL);
    int64_t c = sys_ns(CLOCK_REALTIME);
    int64_t g = (int64_t)tv.tv_sec * 1000000000 + (int64_t)tv.tv_usec * 1000;
    check("gettimeofday lies between two system calls", gr == 0 && a / 1000 * 1000 <= g && g <= c && tv.tv_usec < 1000000);
    check("gettimeofday with neither pointer is 0", vgtod(NULL, NULL) == 0);
    struct timezone vtz = {1, 1}, stz = {2, 2};
    check("gettimeofday's time zone is the system call's", vgtod(&tv, &vtz) == 0 && syscall(SYS_gettimeofday, NULL, &stz) == 0 && vtz.tz_minuteswest == stz.tz_minuteswest && vtz.tz_dsttime == stz.tz_dsttime);
    time_t t0 = syscall(SYS_time, NULL), t1, t2 = 0;
    t1 = vtime(&t2);
    check("time agrees with the system call and stores its result", t1 >= t0 && t1 <= t0 + 1 && t2 == t1 && vtime(NULL) >= t1);

    /* What it does not answer is the system call's. */
    int64_t cpu0 = sys_ns(CLOCK_PROCESS_CPUTIME_ID), cpu1 = v_ns(CLOCK_PROCESS_CPUTIME_ID);
    check("CLOCK_PROCESS_CPUTIME_ID goes to the system call", cpu0 > 0 && cpu1 >= cpu0);
    int64_t th0 = sys_ns(CLOCK_THREAD_CPUTIME_ID), th1 = v_ns(CLOCK_THREAD_CPUTIME_ID);
    check("CLOCK_THREAD_CPUTIME_ID goes to the system call", th0 > 0 && th1 >= th0);
    clockid_t mine;
    check("a thread's CPU clock (pthread_getcpuclockid) works", pthread_getcpuclockid(pthread_self(), &mine) == 0 && v_ns(mine) > 0);
    struct timespec ts;
    check("an invalid clock is EINVAL (-22)", vcgt(100, &ts) == -EINVAL && vcgt(12, &ts) == -EINVAL);
    /* MAKE_PROCESS_CPUCLOCK of a pid that does not exist. */
    clockid_t nobody = (clockid_t)((~(unsigned)999999) << 3);
    check("the CPU clock of no process is EINVAL", vcgt(nobody, &ts) == -EINVAL);
    check("clock_getres of an invalid clock is EINVAL", vgetres(100, &ts) == -EINVAL);
    check("the alarm clocks go to the system call", vcgt(CLOCK_REALTIME_ALARM, &ts) == 0 && vcgt(CLOCK_BOOTTIME_ALARM, &ts) == 0);
    check("musl's clock_gettime is EINVAL for an invalid clock", clock_gettime(100, &ts) == -1 && errno == EINVAL);

    /* The wall clock follows clock_settime and settimeofday at once. */
    struct timespec now, later;
    vcgt(CLOCK_REALTIME, &now);
    int64_t mono_before = v_ns(CLOCK_MONOTONIC);
    later = now;
    later.tv_sec += 1000;
    int set = clock_settime(CLOCK_REALTIME, &later) == 0;
    int64_t moved = v_ns(CLOCK_REALTIME) - ts_ns(later);
    check("clock_settime moves the vDSO's CLOCK_REALTIME at once", set && moved >= 0 && moved < 1000000000);
    struct timeval back = {now.tv_sec + 2000, 0};
    set = settimeofday(&back, NULL) == 0;
    vgtod(&tv, NULL);
    check("settimeofday moves the vDSO's gettimeofday at once", set && tv.tv_sec >= back.tv_sec && tv.tv_sec <= back.tv_sec + 1);
    check("and its time", vtime(NULL) >= back.tv_sec && vtime(NULL) <= back.tv_sec + 1);
    check("CLOCK_MONOTONIC did not move", v_ns(CLOCK_MONOTONIC) - mono_before < 1000000000);
    /* Back to where it was, plus the time that passed. */
    int64_t passed = v_ns(CLOCK_MONOTONIC) - mono_before;
    int64_t restore = ts_ns(now) + passed;
    struct timespec restored = {restore / 1000000000, restore % 1000000000};
    clock_settime(CLOCK_REALTIME, &restored);
    int64_t diff = v_ns(CLOCK_REALTIME) - sys_ns(CLOCK_REALTIME);
    check("the wall clock is set back", llabs(v_ns(CLOCK_REALTIME) - restore) < 1000000000 && diff <= 0);

    /* Monotonic across CPUs and threads. */
    long cpus = sysconf(_SC_NPROCESSORS_ONLN);
    if (cpus > 1) {
        pthread_t p;
        pthread_create(&p, NULL, publisher, NULL);
        pin(0);
        int64_t end = v_ns(CLOCK_MONOTONIC) + 200000000;
        long compared = 0;
        int back_seen = 0;
        while (v_ns(CLOCK_MONOTONIC) < end) {
            int64_t seen = __atomic_load_n(&published, __ATOMIC_SEQ_CST);
            int64_t t = v_ns(CLOCK_MONOTONIC);
            if (seen && t < seen) back_seen = 1;
            if (seen) compared++;
        }
        stop_ping = 1;
        pthread_join(p, NULL);
        unpin();
        check("CLOCK_MONOTONIC never goes back between CPUs", !back_seen && compared > 1000);
    }
    pthread_t w[4];
    for (int i = 0; i < 4; i++) pthread_create(&w[i], NULL, wanderer, NULL);
    for (int i = 0; i < 4; i++) pthread_join(w[i], NULL);
    check("nor across four threads that move between CPUs", !wanderers_back);

    /* getcpu on every CPU, against the system call and musl's sched_getcpu. */
    int cpu_ok = 1;
    for (int cpu = 0; cpu < cpus && cpu < 64; cpu++) {
        pin(cpu);
        unsigned vc = 99, vn = 99, sc = 98, sn = 98;
        long r = vgetcpu(&vc, &vn, NULL);
        syscall(SYS_getcpu, &sc, &sn, NULL);
        cpu_ok &= r == 0 && vc == (unsigned)cpu && sc == (unsigned)cpu && vn == 0 && sn == 0 && sched_getcpu() == cpu;
    }
    unpin();
    check("getcpu agrees with the system call on every CPU", cpu_ok);
    check("getcpu with no pointers is 0", vgetcpu(NULL, NULL, NULL) == 0);

    /* No system call: musl's clock_gettime goes through the vDSO too. */
    long s0 = counted_syscalls();
    for (int i = 0; i < 10000; i++) {
        v_ns(CLOCK_MONOTONIC);
        clock_gettime(CLOCK_REALTIME, &ts);
    }
    long s1 = counted_syscalls();
    check("20000 clock reads make no system call", s0 > 0 && s1 - s0 < 2000);

    /* /proc/self/maps. */
    struct maps m = read_maps();
    check("/proc/self/maps shows [vdso] at AT_SYSINFO_EHDR", m.vdso == (unsigned long)eh && !strcmp(m.vdso_perm, "r-xp"));
    check("and [vvar] read-only just below it", m.vvar == m.vdso - 4096 && m.vvar_end == m.vdso && m.vvar_perm[0] == 'r' && m.vvar_perm[1] == '-' && m.vvar_perm[2] == '-');
    check("and [stack] and the program file", m.stack && m.exe);
    check("names start in Linux's column", m.column_ok);

    /* The clock page cannot be written. */
    volatile int *vvar = (volatile int *)(m.vvar ? m.vvar : (unsigned long)eh - 4096);
    check("mprotect cannot make [vvar] writable (EACCES)", mprotect((void *)vvar, 4096, PROT_READ | PROT_WRITE) == -1 && errno == EACCES);
    pid_t pid = fork();
    if (pid == 0) {
        vvar[0] = 1;
        _exit(0);
    }
    int status;
    waitpid(pid, &status, 0);
    check("a store into [vvar] is SIGSEGV", WIFSIGNALED(status) && WTERMSIG(status) == SIGSEGV);
    pid = fork();
    if (pid == 0) {
        *(volatile char *)eh = 0;
        _exit(0);
    }
    waitpid(pid, &status, 0);
    check("a store into [vdso] is SIGSEGV", WIFSIGNALED(status) && WTERMSIG(status) == SIGSEGV);
    check("the clocks still work after that", v_ns(CLOCK_MONOTONIC) > mono);

    /* fork and exec keep it. */
    pid = fork();
    if (pid == 0) _exit(getauxval(AT_SYSINFO_EHDR) == (unsigned long)eh ? child_checks() : 9);
    waitpid(pid, &status, 0);
    check("a forked child has the vDSO at the same address", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    pid = fork();
    if (pid == 0) {
        execl("/proc/self/exe", "vdsotest", "exec-child", (char *)NULL);
        _exit(8);
    }
    waitpid(pid, &status, 0);
    check("an exec'd program has its own", WIFEXITED(status) && WEXITSTATUS(status) == 0);

    if (failures) {
        printf("vdsotest: %d failed\n", failures);
        return 1;
    }
    puts("vdsotest: all passed");
    return 0;
}
