/* Dynamic linking (ADR 0021): a position-independent program with an
 * interpreter, built twice by userspace/dynamic/default.nix, against glibc
 * (dyntest, ld-linux-x86-64.so.2) and against musl's dynamic loader (dynmusl,
 * ld-musl-x86_64.so.1), both unmodified with their /nix/store paths. It checks
 * the auxiliary vector against what the loader made of it (AT_PHDR and the
 * load bias, AT_ENTRY, AT_BASE the interpreter's, AT_SYSINFO_EHDR the vDSO's,
 * AT_RANDOM, AT_EXECFN, AT_PLATFORM, AT_HWCAP, AT_PAGESZ, AT_SECURE,
 * AT_MINSIGSTKSZ), that RELRO is read-only, thread-local storage of the program
 * and of a library loaded with dlopen (per thread, initialized and zeroed,
 * also in threads that were running before it was loaded),
 * dlsym, dladdr and dlerror (and libm.so.6 with glibc), threads with the C library's own locking and
 * arenas, the C.UTF-8 locale, the vDSO's clocks without system calls, and
 * running dynamic programs again: fork and exec of /proc/self/exe,
 * posix_spawn, system(3), and the interpreter run as a program. Passes on
 * Linux too. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <link.h>
#include <locale.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <wchar.h>
#include <wctype.h>
#ifdef __GLIBC__
#include <gnu/libc-version.h>
#endif

extern char **environ;
/* The program's entry point (crt1's), where AT_ENTRY points. */
extern char _start[];

static int failures;
static const char *self_name;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

/* Thread-local storage of the program itself: initialized (.tdata) and
 * zeroed (.tbss), in the static TLS block. */
static __thread int tls_main = 7;
static __thread char tls_zeroed[512];

/* The library's functions (dlsym). */
static int (*lib_get)(void);
static void (*lib_set)(int);
static int *(*lib_tls_addr)(void);
static int (*lib_tbss_zero)(void);
static void (*lib_tbss_fill)(char);
static int (*lib_format)(char *, unsigned, int);

/* The program's own entry of dl_iterate_phdr, the one at AT_BASE and the
 * vDSO's. */
struct found {
    uintptr_t base, vdso_phdr;
    ElfW(Addr) main_addr;
    const ElfW(Phdr) *main_phdr;
    int main_phnum, at_base, vdso, objects;
};

static int each_object(struct dl_phdr_info *info, size_t size, void *data) {
    struct found *f = data;
    (void)size;
    f->objects++;
    if (f->objects == 1) {
        /* The first is the program. */
        f->main_addr = info->dlpi_addr;
        f->main_phdr = info->dlpi_phdr;
        f->main_phnum = info->dlpi_phnum;
    }
    if (f->base && info->dlpi_addr == f->base) f->at_base = 1;
    if (f->vdso_phdr && (uintptr_t)info->dlpi_phdr == f->vdso_phdr) f->vdso = 1;
    return 0;
}

static const ElfW(Phdr) *find_phdr(const ElfW(Phdr) *ph, int n, unsigned type) {
    for (int i = 0; i < n; i++)
        if (ph[i].p_type == type) return &ph[i];
    return NULL;
}

static void auxv_checks(char **argv) {
    unsigned long phdr = getauxval(AT_PHDR), entry = getauxval(AT_ENTRY), base = getauxval(AT_BASE);
    unsigned long ehdr = getauxval(AT_SYSINFO_EHDR);
    check("AT_PAGESZ is 4096 and sysconf says so", getauxval(AT_PAGESZ) == 4096 && sysconf(_SC_PAGESIZE) == 4096);
    check("AT_PHENT is the size of a program header", getauxval(AT_PHENT) == sizeof(ElfW(Phdr)));
    check("AT_ENTRY is the program's _start", entry == (unsigned long)_start);
    check("AT_SECURE is 0", getauxval(AT_SECURE) == 0);
#ifdef __GLIBC__
    /* (glibc answers with its own bits for x86, HWCAP_X86_64 and others.) */
    check("AT_HWCAP is given", getauxval(AT_HWCAP) != 0);
#else
    check("AT_HWCAP has the FPU and SSE2 bits (CPUID 1, EDX)", (getauxval(AT_HWCAP) & 1) && (getauxval(AT_HWCAP) & (1u << 26)));
#endif
    const char *platform = (const char *)getauxval(AT_PLATFORM);
    check("AT_PLATFORM is x86_64", platform && strcmp(platform, "x86_64") == 0);
    const unsigned char *random = (const unsigned char *)getauxval(AT_RANDOM);
    int nonzero = 0;
    for (int i = 0; random && i < 16; i++) nonzero |= random[i];
    check("AT_RANDOM points at 16 bytes, not all zero", random && nonzero);
    const char *execfn = (const char *)getauxval(AT_EXECFN);
    const char *slash = execfn ? strrchr(execfn, '/') : NULL;
    const char *base_name = strrchr(argv[0], '/');
    check("AT_EXECFN names the program", execfn && strcmp(slash ? slash + 1 : execfn, base_name ? base_name + 1 : argv[0]) == 0);
    check("AT_MINSIGSTKSZ is given", getauxval(AT_MINSIGSTKSZ) > 0);

    /* The program header table: in the program's image, with its PT_PHDR
     * entry giving the load bias that dl_iterate_phdr reports. */
    struct found f = {.base = base};
    const ElfW(Ehdr) *vdso = (const ElfW(Ehdr) *)ehdr;
    check("AT_SYSINFO_EHDR is an ELF image", vdso && memcmp(vdso->e_ident, ELFMAG, SELFMAG) == 0);
    if (vdso) f.vdso_phdr = ehdr + vdso->e_phoff;
    dl_iterate_phdr(each_object, &f);
    const ElfW(Phdr) *ph = (const ElfW(Phdr) *)phdr;
    const ElfW(Phdr) *pt_phdr = find_phdr(ph, getauxval(AT_PHNUM), PT_PHDR);
    check("AT_PHDR and AT_PHNUM are dl_iterate_phdr's program headers", ph == f.main_phdr && (int)getauxval(AT_PHNUM) == f.main_phnum);
    check("the load bias (AT_PHDR - PT_PHDR) is the program's", pt_phdr && phdr - pt_phdr->p_vaddr == f.main_addr);
    check("the program is position-independent, loaded above 0", f.main_addr != 0);
    check("AT_BASE is the interpreter's load address", base != 0 && f.at_base);
    check("the interpreter lies apart from the program", base > (unsigned long)_start + (64 << 20) || base + (64 << 20) < (unsigned long)_start);
#ifdef __GLIBC__
    /* (musl leaves the vDSO out of dl_iterate_phdr.) */
    check("the vDSO is among the loaded objects", f.vdso);
#endif
    check("dl_iterate_phdr sees the program, its libraries and the loader", f.objects >= 2);

    /* RELRO: read-only once relocated (a write ends a child with SIGSEGV). */
    const ElfW(Phdr) *relro = find_phdr(ph, getauxval(AT_PHNUM), PT_GNU_RELRO);
    int relro_ok = 0;
    if (relro) {
        uintptr_t start = (f.main_addr + relro->p_vaddr + 4095) & ~4095ul;
        uintptr_t end = (f.main_addr + relro->p_vaddr + relro->p_memsz) & ~4095ul;
        if (end > start) {
            pid_t pid = fork();
            if (pid == 0) {
                *(volatile char *)start = *(volatile char *)start;
                _exit(0);
            }
            int status;
            relro_ok = waitpid(pid, &status, 0) == pid && WIFSIGNALED(status) && WTERMSIG(status) == SIGSEGV;
        } else {
            /* Less than a page: nothing to protect. */
            relro_ok = 1;
        }
    }
    check("RELRO is read-only after relocation", relro_ok);
}

static int load_library(void) {
    void *h = dlopen("libdyntls.so", RTLD_NOW);
    if (!h) {
        printf("dlopen: %s\n", dlerror());
        return 0;
    }
    lib_get = (int (*)(void))dlsym(h, "lib_get");
    lib_set = (void (*)(int))dlsym(h, "lib_set");
    lib_tls_addr = (int *(*)(void))dlsym(h, "lib_tls_addr");
    lib_tbss_zero = (int (*)(void))dlsym(h, "lib_tbss_zero");
    lib_tbss_fill = (void (*)(char))dlsym(h, "lib_tbss_fill");
    lib_format = (int (*)(char *, unsigned, int))dlsym(h, "lib_format");
    int *constructed = dlsym(h, "lib_constructed");
    return lib_get && lib_set && lib_tls_addr && lib_tbss_zero && lib_tbss_fill && lib_format && constructed && *constructed >= 1;
}

static void dl_checks(void) {
    check("dlopen of a library through the program's RUNPATH, dlsym", load_library());
    if (!lib_get) return;
    check("the library's TLS starts from its image", lib_get() == 5 && lib_tbss_zero());
    lib_set(9);
    lib_tbss_fill('x');
    check("the library's TLS keeps what this thread stores", lib_get() == 9 && *lib_tls_addr() == 9 && !lib_tbss_zero());
    char buf[32];
    check("the library calls into the C library", lib_format(buf, sizeof buf, 12) == 9 && strcmp(buf, "dyntls 12") == 0);
    Dl_info info;
    check("dladdr names the library of a symbol", dladdr((void *)lib_get, &info) && info.dli_fname && strstr(info.dli_fname, "libdyntls.so"));
    check("dlsym(RTLD_DEFAULT) finds the C library's functions", dlsym(RTLD_DEFAULT, "snprintf") != NULL);
    void *none = dlopen("libdoesnotexist.so", RTLD_NOW);
    const char *err = dlerror();
    check("dlopen of a missing library fails with a message", none == NULL && err && strstr(err, "libdoesnotexist.so"));
    void *again = dlopen("libdyntls.so", RTLD_NOW | RTLD_NOLOAD);
    check("RTLD_NOLOAD finds the loaded library", again != NULL);
    if (again) dlclose(again);
#ifdef __GLIBC__
    /* A library of the C library's own, with its own relocations. */
    void *m = dlopen("libm.so.6", RTLD_NOW);
    double (*cosine)(double) = m ? (double (*)(double))dlsym(m, "cos") : NULL;
    volatile double zero = 0.0;
    check("dlopen libm.so.6 and call cos", cosine && cosine(zero) == 1.0);
#endif
}

/* Threads that run before the library is loaded and use its TLS after (its
 * block made for threads that exist: glibc on first use, musl at dlopen,
 * with membarrier). */
static pthread_barrier_t loaded;
static int early_ok[2];

static void *early(void *arg) {
    int i = (int)(intptr_t)arg;
    pthread_barrier_wait(&loaded);
    early_ok[i] = lib_get && lib_get() == 5 && lib_tbss_zero();
    if (lib_get) {
        lib_set(300 + i);
        early_ok[i] &= lib_get() == 300 + i;
    }
    return NULL;
}

/* Threads: the program's and the library's TLS per thread, the C library's
 * locks and allocator. */
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static long counter;
static int thread_tls_ok = 1, thread_lib_ok = 1, thread_alloc_ok = 1, thread_cpu_ok = 1;
static int *lib_addrs[4];

static void *worker(void *arg) {
    int i = (int)(intptr_t)arg;
    int tls_fresh = tls_main == 7 && tls_zeroed[0] == 0 && tls_zeroed[511] == 0;
    tls_main = 100 + i;
    tls_zeroed[0] = 1;
    int lib_fresh = !lib_get || (lib_get() == 5 && lib_tbss_zero());
    if (lib_get) {
        lib_set(200 + i);
        lib_tbss_fill('t');
        lib_addrs[i] = lib_tls_addr();
    }
    int alloc = 1;
    for (int n = 0; n < 2000; n++) {
        char *p = malloc(16 + (n % 64) * 128);
        if (!p) {
            alloc = 0;
            break;
        }
        p[0] = (char)n;
        free(p);
        pthread_mutex_lock(&lock);
        counter++;
        pthread_mutex_unlock(&lock);
    }
    int cpu = sched_getcpu();
    sched_yield();
    pthread_mutex_lock(&lock);
    if (!tls_fresh || tls_main != 100 + i) thread_tls_ok = 0;
    if (!lib_fresh || (lib_get && lib_get() != 200 + i)) thread_lib_ok = 0;
    if (!alloc) thread_alloc_ok = 0;
    if (cpu < 0) thread_cpu_ok = 0;
    pthread_mutex_unlock(&lock);
    return (void *)(intptr_t)(i * 3);
}

static void thread_checks(void) {
    tls_main = 1;
    pthread_t t[4];
    int created = 1, joined = 1;
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    for (int i = 0; i < 4; i++) {
        /* Two with a stack size of their own. */
        if (i >= 2) pthread_attr_setstacksize(&attr, 256 * 1024);
        if (pthread_create(&t[i], i >= 2 ? &attr : NULL, worker, (void *)(intptr_t)i) != 0) created = 0;
    }
    for (int i = 0; created && i < 4; i++) {
        void *r;
        if (pthread_join(t[i], &r) != 0 || r != (void *)(intptr_t)(i * 3)) joined = 0;
    }
    pthread_attr_destroy(&attr);
    check("four threads created and joined with their results", created && joined);
    check("the C library's mutex counts every increment", counter == 4 * 2000);
    check("each thread has the program's TLS fresh from its image", thread_tls_ok && tls_main == 1);
    check("each thread has the library's TLS of its own", thread_lib_ok && (!lib_get || lib_get() == 9));
    int distinct = 1;
    for (int i = 0; i < 4; i++)
        for (int j = i + 1; j < 4; j++)
            if (lib_addrs[i] == lib_addrs[j]) distinct = 0;
    check("the library's TLS lies apart in each thread", distinct);
    check("malloc and free in every thread", thread_alloc_ok);
    check("sched_getcpu in every thread", thread_cpu_ok && sched_getcpu() >= 0);
}

static void locale_checks(void) {
    const char *l = setlocale(LC_ALL, "C.UTF-8");
    check("setlocale C.UTF-8", l != NULL);
    if (!l) return;
    wchar_t w = 0;
    int n = mbtowc(&w, "\xc3\xa4", 2);
    check("UTF-8 decodes in C.UTF-8", MB_CUR_MAX > 1 && n == 2 && w == 0xe4);
    check("towupper knows the letter", towupper(0xe4) == 0xc4);
    char out[8];
    check("UTF-8 encodes in C.UTF-8", wctomb(out, 0x20ac) == 3 && memcmp(out, "\xe2\x82\xac", 3) == 0);
    setlocale(LC_ALL, "C");
}

/* The server's count of system calls (oxidenix's /proc/counters; -1 on Linux). */
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

static void vdso_checks(void) {
    struct timespec a, b;
    int ok = clock_gettime(CLOCK_MONOTONIC, &a) == 0;
    long s0 = counted_syscalls();
    for (int i = 0; i < 20000 && ok; i++) {
        ok = clock_gettime(CLOCK_MONOTONIC, &b) == 0 && (b.tv_sec > a.tv_sec || (b.tv_sec == a.tv_sec && b.tv_nsec >= a.tv_nsec));
        a = b;
    }
    long s1 = counted_syscalls();
    check("clock_gettime is monotonic", ok);
    struct timespec rt;
    check("the wall clock is past 2020", clock_gettime(CLOCK_REALTIME, &rt) == 0 && rt.tv_sec > 1577836800);
    if (s0 >= 0) check("20000 clock reads through the vDSO make no system call", s1 - s0 < 2000);
}

/* The program again, as a child: through fork and exec of /proc/self/exe,
 * posix_spawn, and its interpreter run as a program. */
static int child_status(pid_t pid) {
    int status;
    if (pid < 0 || waitpid(pid, &status, 0) != pid) return -1;
    return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
}

static void exec_checks(void) {
    char exe[4096];
    ssize_t n = readlink("/proc/self/exe", exe, sizeof exe - 1);
    exe[n > 0 ? n : 0] = 0;
    const char *slash = strrchr(exe, '/');
    check("/proc/self/exe names the program", n > 0 && exe[0] == '/' && slash && strcmp(slash + 1, self_name) == 0);
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        char *args[] = {(char *)self_name, "child", NULL};
        execv(exe, args);
        _exit(99);
    }
    check("fork and exec of the dynamic program", child_status(pid) == 42);

    /* (posix_spawn and system share the address space with the child until
     * its execve: the break stays the caller's.) */
    void *brk0 = sbrk(0);
    char *args[] = {(char *)self_name, "child", NULL};
    pid_t spawned = -1;
    check("posix_spawn of the dynamic program", posix_spawn(&spawned, exe, NULL, NULL, args, environ) == 0 && child_status(spawned) == 42);
    int s = system("exit 3");
    check("system(3) runs a shell", WIFEXITED(s) && WEXITSTATUS(s) == 3);
    check("the program break stays the program's", sbrk(0) == brk0);

    /* The interpreter as the program: ld.so maps the program itself. */
    const ElfW(Phdr) *ph = (const ElfW(Phdr) *)getauxval(AT_PHDR);
    const ElfW(Phdr) *interp = find_phdr(ph, getauxval(AT_PHNUM), PT_INTERP);
    const ElfW(Phdr) *pt_phdr = find_phdr(ph, getauxval(AT_PHNUM), PT_PHDR);
    if (interp && pt_phdr) {
        const char *ld = (const char *)(getauxval(AT_PHDR) - pt_phdr->p_vaddr + interp->p_vaddr);
        fflush(stdout);
        pid = fork();
        if (pid == 0) {
            char *ld_args[] = {(char *)ld, exe, "child", NULL};
            execv(ld, ld_args);
            _exit(99);
        }
        check("the interpreter run as a program runs the program", child_status(pid) == 42);
    } else {
        check("the program has PT_INTERP and PT_PHDR", 0);
    }
}

/* The child the exec checks start: fresh TLS, a library, then status 42. */
static int child(void) {
    if (tls_main != 7 || !load_library() || lib_get() != 5) return 1;
    return 42;
}

int main(int argc, char **argv) {
    const char *slash = strrchr(argv[0], '/');
    self_name = slash ? slash + 1 : argv[0];
    if (argc > 1 && strcmp(argv[1], "child") == 0) return child();
    setvbuf(stdout, NULL, _IOLBF, 0);
#ifdef __GLIBC__
    extern const unsigned int __rseq_size __attribute__((weak));
    printf("%s: glibc %s, rseq %s\n", self_name, gnu_get_libc_version(), &__rseq_size && __rseq_size ? "registered" : "not registered");
#else
    printf("%s: musl\n", self_name);
#endif
    auxv_checks(argv);
    pthread_t early_threads[2];
    pthread_barrier_init(&loaded, NULL, 3);
    for (int i = 0; i < 2; i++) pthread_create(&early_threads[i], NULL, early, (void *)(intptr_t)i);
    dl_checks();
    pthread_barrier_wait(&loaded);
    for (int i = 0; i < 2; i++) pthread_join(early_threads[i], NULL);
    check("threads started before dlopen get the library's TLS", early_ok[0] && early_ok[1] && (!lib_get || lib_get() == 9));
    thread_checks();
    locale_checks();
    vdso_checks();
    exec_checks();
    if (failures) {
        printf("%s: %d failed\n", self_name, failures);
        return 1;
    }
    printf("%s: all passed\n", self_name);
    return 0;
}
