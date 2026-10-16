/* Core dumps (core(5), ADR 0022): children that die of a signal whose default action dumps
 * core write an ELF core file (ET_CORE) when RLIMIT_CORE and PR_SET_DUMPABLE allow it;
 * its notes (NT_PRSTATUS per thread with the registers, NT_PRPSINFO, NT_SIGINFO, NT_AUXV,
 * NT_FILE, NT_FPREGSET) and its PT_LOAD segments (memory written, holes for untouched
 * pages, the ELF header page of the program, coredump_filter) are read back; the wait
 * status has WCOREDUMP (waitid CLD_DUMPED) only if a core was written; core_pattern's
 * specifiers, core_uses_pid, a missing directory, an existing file of another mode and a
 * pipe pattern; every one of the ten core signals; a core on /data. */
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

#define R12 0x1122334455667788ull
#define R13 0x0123456789abcdefull
#define XMM0 0x55aa55aa12345678ull
#define R14 0x7777000011112222ull
#define MARK 0x5a

/* A buffer of the program's data (initialized: in its private file mapping) and areas of
 * anonymous memory: one written, one never touched, one touched once. */
static unsigned char marker[3 * 4096] = {1};
static unsigned char *anon;
static unsigned char *untouched;
static unsigned char *partial;

/* Faults at `crash_insn` with known registers: r12, r13, xmm0, and rax 0 (the address). */
extern char crash_insn[];
__attribute__((noinline, noclone)) static void crash_with_registers(void) {
    __asm__ volatile(
        "movabs $0x1122334455667788, %%r12\n"
        "movabs $0x0123456789abcdef, %%r13\n"
        "movabs $0x55aa55aa12345678, %%rax\n"
        "movq %%rax, %%xmm0\n"
        "xor %%eax, %%eax\n"
        ".globl crash_insn\n"
        "crash_insn:\n"
        "movq $1, (%%rax)\n" ::
            : "r12", "r13", "rax", "xmm0", "memory");
}

/* What the children tell the parent, in shared memory. */
struct shared {
    volatile int ready;
    volatile pid_t tids[2];
};
static struct shared *shared;

static void *spinner(void *arg) {
    (void)arg;
    shared->tids[0] = gettid();
    __atomic_add_fetch(&shared->ready, 1, __ATOMIC_SEQ_CST);
    __asm__ volatile("movabs $0x7777000011112222, %%r14\n1: jmp 1b" ::: "r14");
    return NULL;
}

static void *sleeper(void *arg) {
    (void)arg;
    shared->tids[1] = gettid();
    __atomic_add_fetch(&shared->ready, 1, __ATOMIC_SEQ_CST);
    pause();
    return NULL;
}

/* ------------------------------------------------------------------ reading a core */

struct core {
    unsigned char *data;
    size_t size;
    Elf64_Ehdr *eh;
    Elf64_Phdr *ph;
    int phnum;
    unsigned char *notes;
    size_t notes_len;
};

static int load_core(const char *path, struct core *c) {
    memset(c, 0, sizeof *c);
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    struct stat st;
    fstat(fd, &st);
    c->size = st.st_size;
    c->data = malloc(c->size ? c->size : 1);
    size_t got = 0;
    while (got < c->size) {
        ssize_t n = read(fd, c->data + got, c->size - got);
        if (n <= 0) break;
        got += n;
    }
    close(fd);
    if (got != c->size || c->size < sizeof(Elf64_Ehdr)) return -1;
    c->eh = (Elf64_Ehdr *)c->data;
    if (c->eh->e_phoff + (size_t)c->eh->e_phnum * sizeof(Elf64_Phdr) > c->size) return -1;
    c->ph = (Elf64_Phdr *)(c->data + c->eh->e_phoff);
    c->phnum = c->eh->e_phnum;
    for (int i = 0; i < c->phnum; i++) {
        if (c->ph[i].p_type == PT_NOTE && c->ph[i].p_offset + c->ph[i].p_filesz <= c->size) {
            c->notes = c->data + c->ph[i].p_offset;
            c->notes_len = c->ph[i].p_filesz;
        }
    }
    return 0;
}

static void free_core(struct core *c) {
    free(c->data);
    c->data = NULL;
}

/* The `nth` note of `type` (0: the first), its description and size. */
static unsigned char *note(struct core *c, uint32_t type, int nth, size_t *len) {
    size_t at = 0;
    while (at + 12 <= c->notes_len) {
        uint32_t *h = (uint32_t *)(c->notes + at);
        size_t name = (h[0] + 3) & ~3u, desc = (h[1] + 3) & ~3u;
        unsigned char *d = c->notes + at + 12 + name;
        if (h[2] == type && memcmp(c->notes + at + 12, "CORE", 5) == 0 && nth-- == 0) {
            if (len) *len = h[1];
            return d;
        }
        at += 12 + name + desc;
    }
    return NULL;
}

static int count_notes(struct core *c, uint32_t type) {
    int n = 0;
    while (note(c, type, n, NULL)) n++;
    return n;
}

/* The PT_LOAD that maps `addr`. */
static Elf64_Phdr *segment(struct core *c, uintptr_t addr) {
    for (int i = 0; i < c->phnum; i++) {
        Elf64_Phdr *p = &c->ph[i];
        if (p->p_type == PT_LOAD && addr >= p->p_vaddr && addr < p->p_vaddr + p->p_memsz) return p;
    }
    return NULL;
}

/* The file's bytes of memory at `addr`, if dumped. */
static unsigned char *memory(struct core *c, uintptr_t addr, size_t len) {
    Elf64_Phdr *p = segment(c, addr);
    if (!p || addr + len > p->p_vaddr + p->p_filesz) return NULL;
    size_t off = p->p_offset + (addr - p->p_vaddr);
    if (off + len > c->size) return NULL;
    return c->data + off;
}

static uint64_t reg(unsigned char *prstatus, int index) {
    uint64_t v;
    memcpy(&v, prstatus + 112 + index * 8, 8);
    return v;
}

static int32_t i32_at(unsigned char *p, int at) {
    int32_t v;
    memcpy(&v, p + at, 4);
    return v;
}

/* user_regs_struct's words. */
enum { R_R15, R_R14, R_R13, R_R12, R_RBP, R_RBX, R_R11, R_R10, R_R9, R_R8, R_RAX, R_RCX, R_RDX, R_RSI, R_RDI, R_ORIG_RAX, R_RIP, R_CS, R_EFLAGS, R_RSP, R_SS, R_FS_BASE };

static int exists(const char *path) {
    struct stat st;
    return stat(path, &st) == 0;
}

static void set_pattern(const char *pattern) {
    int fd = open("/proc/sys/kernel/core_pattern", O_WRONLY);
    if (fd >= 0) {
        if (write(fd, pattern, strlen(pattern)) < 0) perror("core_pattern");
        close(fd);
    }
}

static void set_uses_pid(int on) {
    int fd = open("/proc/sys/kernel/core_uses_pid", O_WRONLY);
    if (fd >= 0) {
        if (write(fd, on ? "1\n" : "0\n", 2) < 0) perror("core_uses_pid");
        close(fd);
    }
}

static int read_text(const char *path, char *buf, size_t size) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    ssize_t n = read(fd, buf, size - 1);
    close(fd);
    if (n < 0) return -1;
    buf[n] = 0;
    return n;
}

static void set_core_limit(rlim_t soft) {
    struct rlimit rl = {soft, RLIM_INFINITY};
    setrlimit(RLIMIT_CORE, &rl);
}

/* Runs `body` in a child that dies of it; its wait status. */
static int child_status(void (*body)(void)) {
    pid_t p = fork();
    if (p == 0) {
        body();
        _exit(99);
    }
    int st = 0;
    waitpid(p, &st, 0);
    return st;
}

static pid_t last_child;

static int child_status_pid(void (*body)(void)) {
    pid_t p = fork();
    if (p == 0) {
        body();
        _exit(99);
    }
    last_child = p;
    int st = 0;
    waitpid(p, &st, 0);
    return st;
}

/* ------------------------------------------------------------------ the children */

static void fill_and_crash(void) {
    memset(marker, MARK, sizeof marker);
    memset(anon, 0xa5, 2 * 4096);
    partial[0] = 0x77;
    crash_with_registers();
}

static void threads_and_crash(void) {
    memset(marker, MARK, sizeof marker);
    pthread_t a, b;
    pthread_create(&a, NULL, spinner, NULL);
    pthread_create(&b, NULL, sleeper, NULL);
    while (shared->ready < 2) sched_yield();
    struct timespec ts = {0, 50 * 1000 * 1000};
    nanosleep(&ts, NULL);
    crash_with_registers();
}

static void undumpable_crash(void) {
    prctl(PR_SET_DUMPABLE, 0, 0, 0, 0);
    if (prctl(PR_GET_DUMPABLE, 0, 0, 0, 0) != 0) _exit(98);
    crash_with_registers();
}

static void no_anonymous_crash(void) {
    int fd = open("/proc/self/coredump_filter", O_WRONLY);
    if (fd < 0 || write(fd, "0x10\n", 5) != 5) _exit(97);
    close(fd);
    memset(marker, MARK, sizeof marker);
    memset(anon, 0xa5, 2 * 4096);
    crash_with_registers();
}

static void just_crash(void) {
    crash_with_registers();
}

static void do_abort(void) {
    abort();
}

static void quit_self(void) {
    kill(getpid(), SIGQUIT);
    for (;;) pause();
}

static void wait_for_quit(void) {
    shared->ready = 1;
    for (;;) pause();
}

static void term_self(void) {
    kill(getpid(), SIGTERM);
    for (;;) pause();
}

static int signal_to_send;
static void raise_it(void) {
    /* Default actions everywhere (a test runner may have ignored some). */
    signal(signal_to_send, SIG_DFL);
    sigset_t all;
    sigemptyset(&all);
    sigprocmask(SIG_SETMASK, &all, NULL);
    raise(signal_to_send);
    for (;;) pause();
}

/* ------------------------------------------------------------------ the tests */

static void test_segv(void) {
    set_pattern("core");
    set_uses_pid(0);
    unlink("core");
    int st = child_status_pid(fill_and_crash);
    pid_t child = last_child;
    check("SIGSEGV: killed by the signal, WCOREDUMP", WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV && WCOREDUMP(st));
    struct core c;
    if (load_core("core", &c) != 0) {
        check("SIGSEGV: ./core is an ELF file", 0);
        return;
    }
    Elf64_Ehdr *e = c.eh;
    check("ELF header: 64-bit, little-endian, ET_CORE, EM_X86_64",
          memcmp(e->e_ident, ELFMAG, SELFMAG) == 0 && e->e_ident[EI_CLASS] == ELFCLASS64 && e->e_ident[EI_DATA] == ELFDATA2LSB && e->e_type == ET_CORE &&
              e->e_machine == EM_X86_64 && e->e_phentsize == sizeof(Elf64_Phdr) && e->e_entry == 0);
    check("the first program header is PT_NOTE", c.phnum > 1 && c.ph[0].p_type == PT_NOTE && c.notes != NULL);
    size_t len = 0;
    unsigned char *pr = note(&c, NT_PRSTATUS, 0, &len);
    check("NT_PRSTATUS: one, 336 bytes", pr && len == 336 && count_notes(&c, NT_PRSTATUS) == 1);
    if (pr) {
        check("NT_PRSTATUS: si_signo and pr_cursig SIGSEGV, pr_pid the child", i32_at(pr, 0) == SIGSEGV && *(int16_t *)(pr + 12) == SIGSEGV && i32_at(pr, 32) == child);
        check("NT_PRSTATUS: pr_ppid us", i32_at(pr, 36) == getpid());
        check("NT_PRSTATUS: r12 and r13 as set", reg(pr, R_R12) == R12 && reg(pr, R_R13) == R13);
        check("NT_PRSTATUS: rip the faulting instruction, rax 0", reg(pr, R_RIP) == (uintptr_t)crash_insn && reg(pr, R_RAX) == 0);
        check("NT_PRSTATUS: orig_rax -1 (no call), cs/ss user", reg(pr, R_ORIG_RAX) == (uint64_t)-1 && reg(pr, R_CS) == 0x33 && reg(pr, R_SS) == 0x2b);
        check("NT_PRSTATUS: fs_base the thread's TLS", reg(pr, R_FS_BASE) == (uintptr_t)pthread_self());
        check("NT_PRSTATUS: rsp in a dumped stack", segment(&c, reg(pr, R_RSP)) && memory(&c, reg(pr, R_RSP), 8));
        check("NT_PRSTATUS: pr_fpvalid", i32_at(pr, 328) == 1);
    }
    unsigned char *fp = note(&c, NT_FPREGSET, 0, &len);
    uint64_t xmm0 = 0;
    if (fp) memcpy(&xmm0, fp + 160, 8);
    check("NT_FPREGSET: 512 bytes, xmm0 as set", fp && len == 512 && xmm0 == XMM0);
    unsigned char *ps = note(&c, NT_PRPSINFO, 0, &len);
    check("NT_PRPSINFO: 136 bytes, state R, the child's pid", ps && len == 136 && ps[1] == 'R' && i32_at(ps, 24) == child && i32_at(ps, 28) == getpid());
    check("NT_PRPSINFO: pr_fname coretest, pr_psargs its command line", ps && strcmp((char *)ps + 40, "coretest") == 0 && strstr((char *)ps + 56, "coretest") != NULL);
    unsigned char *si = note(&c, NT_SIGINFO, 0, &len);
    uint64_t fault_addr = 1;
    if (si) memcpy(&fault_addr, si + 16, 8);
    check("NT_SIGINFO: SIGSEGV, SEGV_MAPERR at address 0", si && len == 128 && i32_at(si, 0) == SIGSEGV && i32_at(si, 8) == SEGV_MAPERR && fault_addr == 0);
    unsigned char *av = note(&c, NT_AUXV, 0, &len);
    int pagesz = 0, entry = 0, ended = 0;
    for (size_t i = 0; av && i + 16 <= len; i += 16) {
        uint64_t k, v;
        memcpy(&k, av + i, 8);
        memcpy(&v, av + i + 8, 8);
        if (k == AT_PAGESZ && v == 4096) pagesz = 1;
        if (k == AT_ENTRY && v == getauxval(AT_ENTRY)) entry = 1;
        if (k == AT_NULL && i + 16 == len) ended = 1;
    }
    check("NT_AUXV: AT_PAGESZ, AT_ENTRY as ours, AT_NULL last", pagesz && entry && ended);
    unsigned char *fl = note(&c, NT_FILE, 0, &len);
    int named = 0;
    if (fl && len > 16) {
        uint64_t count;
        memcpy(&count, fl, 8);
        char *names = (char *)fl + 16 + count * 24;
        for (uint64_t i = 0; i < count && names < (char *)fl + len; i++, names += strlen(names) + 1) {
            size_t n = strlen(names);
            if (n >= 9 && strcmp(names + n - 9, "/coretest") == 0) named = 1;
        }
    }
    check("NT_FILE: the program's mappings by its path", named);
    unsigned char *m = memory(&c, (uintptr_t)marker, sizeof marker);
    int same = m != NULL;
    for (size_t i = 0; m && i < sizeof marker; i++) same &= m[i] == MARK;
    check("PT_LOAD: the program's written data as the child left it", same);
    m = memory(&c, (uintptr_t)anon, 2 * 4096);
    same = m != NULL;
    for (size_t i = 0; m && i < 2 * 4096; i++) same &= m[i] == 0xa5;
    check("PT_LOAD: anonymous memory as the child wrote it", same);
    Elf64_Phdr *u = segment(&c, (uintptr_t)untouched);
    check("PT_LOAD: memory nobody touched has no file bytes", u && u->p_filesz == 0 && u->p_memsz >= 64 * 4096);
    Elf64_Phdr *pp = segment(&c, (uintptr_t)partial);
    m = memory(&c, (uintptr_t)partial, 64 * 4096);
    int zero = m != NULL && m[0] == 0x77;
    for (size_t i = 1; m && i < 64 * 4096; i++) zero &= m[i] == 0;
    check("PT_LOAD: memory touched once is dumped whole, the rest holes (zeros)", pp && pp->p_filesz == pp->p_memsz && zero);
    int elf_page = 0, vdso = 0;
    for (int i = 0; i < c.phnum; i++) {
        Elf64_Phdr *p = &c.ph[i];
        if (p->p_type != PT_LOAD || p->p_filesz < 4 || memcmp(c.data + p->p_offset, ELFMAG, SELFMAG) != 0) continue;
        if (p->p_vaddr == getauxval(AT_SYSINFO_EHDR))
            vdso = p->p_filesz == p->p_memsz;
        else if (p->p_filesz == 4096)
            elf_page = 1;
    }
    check("PT_LOAD: the vDSO whole", vdso || !getauxval(AT_SYSINFO_EHDR));
    check("PT_LOAD: the program's ELF header page (coredump_filter bit 4)", elf_page);
    int ordered = 1;
    for (int i = 1; i < c.phnum; i++) {
        Elf64_Phdr *p = &c.ph[i];
        if (p->p_type != PT_LOAD || p->p_align != 4096 || p->p_offset % 4096 != 0 || p->p_offset + p->p_filesz > c.size) ordered = 0;
        if (i > 1 && p->p_vaddr < c.ph[i - 1].p_vaddr + c.ph[i - 1].p_memsz) ordered = 0;
    }
    check("PT_LOAD: page-aligned data in the file, mappings in order", ordered);
    struct stat st_core;
    stat("core", &st_core);
    check("the core file: mode 0600, regular", S_ISREG(st_core.st_mode) && (st_core.st_mode & 0777) == 0600);
    free_core(&c);
    unlink("core");
}

static void test_threads(void) {
    unlink("core");
    shared->ready = 0;
    int st = child_status_pid(threads_and_crash);
    check("threads: WCOREDUMP", WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV && WCOREDUMP(st));
    struct core c;
    if (load_core("core", &c) != 0) {
        check("threads: ./core", 0);
        return;
    }
    check("threads: three NT_PRSTATUS, three NT_FPREGSET", count_notes(&c, NT_PRSTATUS) == 3 && count_notes(&c, NT_FPREGSET) == 3);
    unsigned char *first = note(&c, NT_PRSTATUS, 0, NULL);
    check("threads: the dumping thread's first", first && i32_at(first, 32) == last_child && reg(first, R_RIP) == (uintptr_t)crash_insn);
    int spin_ok = 0, sleep_ok = 0, all_sigsegv = 1;
    for (int i = 0; i < 3; i++) {
        unsigned char *pr = note(&c, NT_PRSTATUS, i, NULL);
        if (!pr) continue;
        all_sigsegv &= *(int16_t *)(pr + 12) == SIGSEGV;
        if (i32_at(pr, 32) == shared->tids[0] && reg(pr, R_R14) == R14) spin_ok = 1;
        if (i32_at(pr, 32) == shared->tids[1] && reg(pr, R_ORIG_RAX) == SYS_pause && (int64_t)reg(pr, R_RAX) == -514) sleep_ok = 1;
    }
    check("threads: a thread running its program, its r14", spin_ok);
    check("threads: a thread in pause(), orig_rax and ERESTARTNOHAND", sleep_ok);
    check("threads: pr_cursig SIGSEGV in each", all_sigsegv);
    free_core(&c);
    unlink("core");
}

static void test_limits(void) {
    unlink("core");
    set_core_limit(0);
    int st = child_status(just_crash);
    check("RLIMIT_CORE 0: no core, no WCOREDUMP", WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV && !WCOREDUMP(st) && !exists("core"));
    set_core_limit(4095);
    st = child_status(just_crash);
    check("RLIMIT_CORE below a page: none either", !WCOREDUMP(st) && !exists("core"));
    set_core_limit(8192);
    st = child_status(just_crash);
    struct core c;
    int loaded = load_core("core", &c) == 0;
    check("RLIMIT_CORE 8 KiB: a core, no WCOREDUMP", WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV && !WCOREDUMP(st) && loaded && c.eh->e_type == ET_CORE);
    int cut = 0;
    for (int i = 0; loaded && i < c.phnum; i++) cut |= c.ph[i].p_offset + c.ph[i].p_filesz > c.size;
    check("RLIMIT_CORE 8 KiB: cut short of its headers' data", cut);
    if (loaded) free_core(&c);
    unlink("core");
    set_core_limit(RLIM_INFINITY);
    st = child_status(undumpable_crash);
    check("PR_SET_DUMPABLE 0: no core, no WCOREDUMP", WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV && !WCOREDUMP(st) && !exists("core"));
    st = child_status(term_self);
    check("SIGTERM dumps no core", WIFSIGNALED(st) && WTERMSIG(st) == SIGTERM && !WCOREDUMP(st) && !exists("core"));
    check("PR_GET_DUMPABLE is 1 here", prctl(PR_GET_DUMPABLE, 0, 0, 0, 0) == 1);
}

static void test_filter(void) {
    char text[64];
    check("coredump_filter: 00000033 by default", read_text("/proc/self/coredump_filter", text, sizeof text) > 0 && strcmp(text, "00000033\n") == 0);
    unlink("core");
    int st = child_status(no_anonymous_crash);
    struct core c;
    int loaded = load_core("core", &c) == 0;
    check("coredump_filter 0x10: a core", WCOREDUMP(st) && loaded);
    if (loaded) {
        Elf64_Phdr *a = segment(&c, (uintptr_t)anon);
        Elf64_Phdr *d = segment(&c, (uintptr_t)marker);
        check("coredump_filter 0x10: anonymous and written private memory left out", a && a->p_filesz == 0 && d && d->p_filesz == 0);
        free_core(&c);
    }
    unlink("core");
    int fd = open("/proc/self/coredump_filter", O_WRONLY);
    int wrote = fd >= 0 && write(fd, "7", 1) == 1;
    if (fd >= 0) close(fd);
    pid_t p = fork();
    if (p == 0) {
        char t[32];
        _exit(read_text("/proc/self/coredump_filter", t, sizeof t) > 0 && strcmp(t, "00000007\n") == 0 ? 0 : 1);
    }
    int s2 = 0;
    waitpid(p, &s2, 0);
    check("coredump_filter: written, read back, inherited by fork", wrote && WIFEXITED(s2) && WEXITSTATUS(s2) == 0);
    fd = open("/proc/self/coredump_filter", O_WRONLY);
    errno = 0;
    check("coredump_filter: EINVAL for no number", fd >= 0 && write(fd, "x", 1) < 0 && errno == EINVAL);
    if (fd >= 0) {
        if (write(fd, "0x33", 4) != 4) perror("coredump_filter");
        close(fd);
    }
}

static void test_names(void) {
    char text[256];
    set_pattern("%e.%p.%s.core\n");
    check("core_pattern: read back without what followed a newline", read_text("/proc/sys/kernel/core_pattern", text, sizeof text) > 0 && strcmp(text, "%e.%p.%s.core\n") == 0);
    int st = child_status_pid(do_abort);
    char name[128];
    snprintf(name, sizeof name, "coretest.%d.%d.core", last_child, SIGABRT);
    check("core_pattern %e.%p.%s: SIGABRT's core by that name", WIFSIGNALED(st) && WTERMSIG(st) == SIGABRT && WCOREDUMP(st) && exists(name));
    unlink(name);
    set_pattern("core");
    set_uses_pid(1);
    check("core_uses_pid: 1", read_text("/proc/sys/kernel/core_uses_pid", text, sizeof text) > 0 && strcmp(text, "1\n") == 0);
    st = child_status_pid(just_crash);
    snprintf(name, sizeof name, "core.%d", last_child);
    check("core_uses_pid: core.PID", WCOREDUMP(st) && exists(name) && !exists("core"));
    unlink(name);
    set_uses_pid(0);
    set_pattern("/tmp/coretest/missing/core");
    st = child_status(just_crash);
    check("a pattern in a missing directory: no core, no WCOREDUMP", WIFSIGNALED(st) && !WCOREDUMP(st));
    set_pattern("|/bin/cat");
    st = child_status(just_crash);
    check("a pipe pattern: no core (not supported), no WCOREDUMP", WIFSIGNALED(st) && !WCOREDUMP(st) && !exists("core"));
    set_pattern("core");
    /* An existing core whose mode is not 0600 is left alone (Linux's "cannot preserve file
     * permissions"). */
    int fd = open("core", O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd >= 0) {
        if (write(fd, "keep", 4) != 4) perror("core");
        fchmod(fd, 0644);
        close(fd);
    }
    st = child_status(just_crash);
    check("an existing core of mode 0644: kept, no WCOREDUMP", !WCOREDUMP(st) && read_text("core", text, sizeof text) == 4 && strcmp(text, "keep") == 0);
    /* With mode 0600 it is replaced (truncated, then written). */
    chmod("core", 0600);
    st = child_status(just_crash);
    struct core c;
    int loaded = load_core("core", &c) == 0;
    check("an existing core of mode 0600: replaced", WCOREDUMP(st) && loaded && c.eh->e_type == ET_CORE);
    if (loaded) free_core(&c);
    unlink("core");
}

static void test_signals(void) {
    static const int sigs[] = {SIGQUIT, SIGILL, SIGTRAP, SIGABRT, SIGBUS, SIGFPE, SIGSEGV, SIGXCPU, SIGXFSZ, SIGSYS};
    set_pattern("core.%s");
    int all = 1;
    for (size_t i = 0; i < sizeof sigs / sizeof sigs[0]; i++) {
        signal_to_send = sigs[i];
        int st = child_status(raise_it);
        char name[32];
        snprintf(name, sizeof name, "core.%d", sigs[i]);
        struct core c;
        int ok = WIFSIGNALED(st) && WTERMSIG(st) == sigs[i] && WCOREDUMP(st) && load_core(name, &c) == 0;
        if (ok) {
            unsigned char *si = note(&c, NT_SIGINFO, 0, NULL);
            ok = si && i32_at(si, 0) == sigs[i];
            free_core(&c);
        }
        if (!ok) printf("  signal %d: status %#x\n", sigs[i], st);
        all &= ok;
        unlink(name);
    }
    check("each of the ten core signals dumps (raise)", all);
    /* One sent by another process: its siginfo says who. waitid: CLD_DUMPED. */
    shared->ready = 0;
    pid_t p = fork();
    if (p == 0) wait_for_quit();
    while (!shared->ready) sched_yield();
    kill(p, SIGQUIT);
    siginfo_t info;
    memset(&info, 0, sizeof info);
    waitid(P_PID, p, &info, WEXITED);
    check("SIGQUIT from us: waitid CLD_DUMPED, si_status SIGQUIT", info.si_code == CLD_DUMPED && info.si_status == SIGQUIT);
    char name[32];
    snprintf(name, sizeof name, "core.%d", SIGQUIT);
    struct core c;
    int loaded = load_core(name, &c) == 0;
    unsigned char *si = loaded ? note(&c, NT_SIGINFO, 0, NULL) : NULL;
    check("SIGQUIT from us: NT_SIGINFO SI_USER from our pid", si && i32_at(si, 0) == SIGQUIT && i32_at(si, 8) == SI_USER && i32_at(si, 16) == getpid());
    if (loaded) free_core(&c);
    unlink(name);
    int st = child_status(quit_self);
    check("SIGQUIT by kill(getpid()): WCOREDUMP", WIFSIGNALED(st) && WTERMSIG(st) == SIGQUIT && WCOREDUMP(st) && exists(name));
    unlink(name);
    set_pattern("core");
}

static void test_data(void) {
    if (mkdir("/data/coretest", 0755) != 0 && errno != EEXIST) {
        check("/data/coretest", 0);
        return;
    }
    if (chdir("/data/coretest") != 0) return;
    unlink("core");
    int st = child_status(fill_and_crash);
    struct core c;
    int loaded = load_core("core", &c) == 0;
    check("on /data: WCOREDUMP and a core", WCOREDUMP(st) && loaded);
    if (loaded) {
        unsigned char *m = memory(&c, (uintptr_t)marker, sizeof marker);
        int same = m != NULL;
        for (size_t i = 0; m && i < sizeof marker; i++) same &= m[i] == MARK;
        unsigned char *pr = note(&c, NT_PRSTATUS, 0, NULL);
        check("on /data: memory and registers read back", same && pr && reg(pr, R_R12) == R12);
        free_core(&c);
    }
    unlink("core");
    if (chdir("/tmp/coretest") != 0) perror("chdir");
    rmdir("/data/coretest");
}

int main(void) {
    /* The anonymous areas in one reservation with inaccessible pages between them, so
     * that each is a mapping of its own. */
    shared = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    unsigned char *base = mmap(NULL, 136 * 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED || base == MAP_FAILED) {
        perror("mmap");
        return 1;
    }
    anon = base + 4096;
    untouched = anon + 5 * 4096;
    partial = untouched + 65 * 4096;
    if (mprotect(anon, 4 * 4096, PROT_READ | PROT_WRITE) || mprotect(untouched, 64 * 4096, PROT_READ | PROT_WRITE) || mprotect(partial, 64 * 4096, PROT_READ | PROT_WRITE)) {
        perror("mprotect");
        return 1;
    }
    /* The core signals act by default here, whatever the shell left. */
    sigset_t none;
    sigemptyset(&none);
    sigprocmask(SIG_SETMASK, &none, NULL);
    signal(SIGQUIT, SIG_DFL);
    mkdir("/tmp/coretest", 0755);
    if (chdir("/tmp/coretest") != 0) {
        perror("chdir");
        return 1;
    }
    set_core_limit(RLIM_INFINITY);
    test_segv();
    test_threads();
    test_limits();
    test_filter();
    test_names();
    test_signals();
    test_data();
    set_pattern("core");
    set_uses_pid(0);
    if (chdir("/") == 0) rmdir("/tmp/coretest");
    if (failures) {
        printf("coretest: %d failed\n", failures);
        return 1;
    }
    printf("coretest: all passed\n");
    return 0;
}
