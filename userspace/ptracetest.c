/* ptrace and the calls that share its machinery (the Linux server's, ADR 0019):
 * PTRACE_TRACEME with an exec's SIGTRAP, ATTACH and SEIZE, INTERRUPT and LISTEN with a
 * group stop, PEEK/POKE of text and data (an int3 written into read-only text, hit and
 * removed), GETREGS/SETREGS at syscall stops (a result changed, a call skipped with -1,
 * a call replaced), PTRACE_GET_SYSCALL_INFO, SYSEMU, GETFPREGS/SETFPREGS, single steps
 * over a known sequence and over a system call, hardware breakpoints and watchpoints
 * (POKEUSER of u_debugreg), the events of fork, vfork, clone, exec and exit with
 * GETEVENTMSG (and a real parent's wait that waits for its traced child's reaping), signal
 * injection and suppression, the tracer's end (detach, PTRACE_O_EXITKILL),
 * process_vm_readv and process_vm_writev, /proc/<pid>/mem (a write forced into read-only
 * text), a program's own TF, and the errors. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/user.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef NT_PRSTATUS
#define NT_PRSTATUS 1
#endif
#ifndef NT_X86_XSTATE
#define NT_X86_XSTATE 0x202
#endif
#define TRAP_BRKPT 1
#define TRAP_TRACE 2
#define TRAP_HWBKPT 4
#define CLD_TRAPPED_CODE 4

static int failures;

static void check(const char *name, int ok) {
    printf("%-62s %s\n", name, ok ? "ok" : "FAIL");
    fflush(stdout);
    if (!ok) failures++;
}

static void sleep_ms(long ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000};
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR) {
    }
}

/* waitpid that retries on EINTR. */
static pid_t wait_for(pid_t pid, int *status, int options) {
    pid_t r;
    do {
        r = waitpid(pid, status, options);
    } while (r < 0 && errno == EINTR);
    return r;
}

static int stopped_with(int status, int sig) { return WIFSTOPPED(status) && WSTOPSIG(status) == sig; }

static int event_of(int status) { return (unsigned)status >> 16; }

/* A child stopped by its PTRACE_TRACEME and a raise(SIGSTOP) runs `body` then exits with
 * its result. */
static pid_t traced_child(int (*body)(void)) {
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        raise(SIGSTOP);
        _exit(body());
    }
    int st;
    if (wait_for(pid, &st, 0) != pid || !stopped_with(st, SIGSTOP)) {
        printf("traced child did not stop (status %#x)\n", st);
        return -1;
    }
    return pid;
}

/* Runs the tracee on (signals other than SIGTRAP and SIGSTOP passed on) until it exits;
 * its exit status, or -1. */
static int finish(pid_t pid) {
    int st;
    if (ptrace(PTRACE_CONT, pid, 0, 0) < 0) return -1;
    for (;;) {
        if (wait_for(pid, &st, 0) != pid) return -1;
        if (WIFEXITED(st)) return WEXITSTATUS(st);
        if (WIFSIGNALED(st)) return 128 + WTERMSIG(st);
        int sig = WSTOPSIG(st);
        ptrace(PTRACE_CONT, pid, 0, (sig == SIGTRAP || sig == SIGSTOP || event_of(st)) ? 0 : sig);
    }
}

/* ------------------------------------------------------------------ TRACEME and exec */

static void traceme_exec(void) {
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        ptrace(PTRACE_TRACEME, 0, 0, 0);
        char *argv[] = {"ptracetest", "exit7", NULL};
        execv("/bin/ptracetest", argv);
        _exit(99);
    }
    int st;
    wait_for(pid, &st, 0);
    check("TRACEME: an execve stops the tracee with SIGTRAP", stopped_with(st, SIGTRAP) && event_of(st) == 0);
    siginfo_t si;
    memset(&si, 0, sizeof si);
    long r = ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    check("TRACEME: its siginfo is SI_USER from itself", r == 0 && si.si_signo == SIGTRAP && si.si_code == SI_USER && si.si_pid == pid);
    struct user_regs_struct regs;
    r = ptrace(PTRACE_GETREGS, pid, 0, &regs);
    check("TRACEME: GETREGS at the new program's start (cs, ss as Linux's)", r == 0 && regs.cs == 0x33 && regs.ss == 0x2b && regs.rip != 0 && regs.rsp != 0);
    errno = 0;
    long rip = ptrace(PTRACE_PEEKUSER, pid, offsetof(struct user_regs_struct, rip), 0);
    check("PEEKUSER reads rip as GETREGS does", errno == 0 && (unsigned long)rip == regs.rip);
    char path[64], status[2048];
    snprintf(path, sizeof path, "/proc/%d/status", pid);
    int fd = open(path, O_RDONLY);
    ssize_t n = fd >= 0 ? read(fd, status, sizeof status - 1) : -1;
    if (fd >= 0) close(fd);
    status[n > 0 ? n : 0] = 0;
    char tracer[32];
    snprintf(tracer, sizeof tracer, "\nTracerPid:\t%d\n", getpid());
    check("/proc/<pid>/status: t (tracing stop), its TracerPid", strstr(status, "\nState:\tt (tracing stop)\n") && strstr(status, tracer));
    check("TRACEME: the program runs on and exits 7", finish(pid) == 7);
}

/* ------------------------------------------------------------------ attach and seize */

static void spin_forever(void) {
    for (;;) pause();
}

static pid_t sleeper(void) {
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) spin_forever();
    sleep_ms(20);
    return pid;
}

static void reap_killed(pid_t pid) {
    kill(pid, SIGKILL);
    int st;
    while (wait_for(pid, &st, __WALL) == pid && !WIFEXITED(st) && !WIFSIGNALED(st)) {
    }
}

static void attach_seize(void) {
    pid_t pid = sleeper();
    long r = ptrace(PTRACE_ATTACH, pid, 0, 0);
    int st = 0;
    wait_for(pid, &st, 0);
    check("ATTACH: the tracee stops with SIGSTOP", r == 0 && stopped_with(st, SIGSTOP));
    siginfo_t si;
    memset(&si, 0, sizeof si);
    r = ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    check("ATTACH: its SIGSTOP is the kernel's (SI_KERNEL)", r == 0 && si.si_signo == SIGSTOP && si.si_code == SI_KERNEL);
    errno = 0;
    r = ptrace(PTRACE_ATTACH, pid, 0, 0);
    check("ATTACH twice: EPERM", r < 0 && errno == EPERM);
    errno = 0;
    r = ptrace(PTRACE_ATTACH, getpid(), 0, 0);
    check("ATTACH to itself: EPERM", r < 0 && errno == EPERM);
    errno = 0;
    r = ptrace(PTRACE_CONT, pid, 0, 100);
    check("CONT with an invalid signal: EIO", r < 0 && errno == EIO);
    errno = 0;
    r = ptrace(0x4299, pid, 0, 0);
    check("an unknown request: EIO", r < 0 && errno == EIO);
    errno = 0;
    r = ptrace(PTRACE_INTERRUPT, pid, 0, 0);
    check("INTERRUPT of an attached (not seized) tracee: EIO", r < 0 && errno == EIO);
    r = ptrace(PTRACE_CONT, pid, 0, 0);
    errno = 0;
    struct user_regs_struct regs;
    long g = ptrace(PTRACE_GETREGS, pid, 0, &regs);
    check("GETREGS of a running tracee: ESRCH", r == 0 && g < 0 && errno == ESRCH);
    /* A group stop of an attached tracee: a plain stop, without siginfo. */
    kill(pid, SIGSTOP);
    wait_for(pid, &st, 0);
    int delivery = stopped_with(st, SIGSTOP) && ptrace(PTRACE_GETSIGINFO, pid, 0, &si) == 0;
    ptrace(PTRACE_CONT, pid, 0, SIGSTOP);
    wait_for(pid, &st, 0);
    errno = 0;
    r = ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    check("ATTACH: SIGSTOP stops, its group stop has no siginfo (EINVAL)", delivery && stopped_with(st, SIGSTOP) && event_of(st) == 0 && r < 0 && errno == EINVAL);
    r = ptrace(PTRACE_DETACH, pid, 0, 0);
    /* Detached while its process is stopped: it stays stopped (Linux's __ptrace_unlink). */
    sleep_ms(20);
    char path[64], buf[512];
    snprintf(path, sizeof path, "/proc/%d/stat", pid);
    int fd = open(path, O_RDONLY);
    ssize_t n = fd >= 0 ? read(fd, buf, sizeof buf - 1) : -1;
    if (fd >= 0) close(fd);
    buf[n > 0 ? n : 0] = 0;
    char *state = strrchr(buf, ')');
    check("DETACH in a group stop: the process stays stopped", r == 0 && state && state[2] == 'T');
    errno = 0;
    r = ptrace(PTRACE_GETREGS, pid, 0, &regs);
    check("GETREGS after DETACH: ESRCH", r < 0 && errno == ESRCH);
    kill(pid, SIGCONT);
    reap_killed(pid);

    errno = 0;
    r = ptrace(PTRACE_ATTACH, 32760, 0, 0);
    check("ATTACH to no such process: ESRCH", r < 0 && errno == ESRCH);

    /* SEIZE: no stop; INTERRUPT; a group stop as PTRACE_EVENT_STOP; LISTEN; SIGCONT. */
    pid = sleeper();
    r = ptrace(PTRACE_SEIZE, pid, 0, 0);
    st = 0;
    pid_t w = wait_for(pid, &st, WNOHANG);
    check("SEIZE: the tracee does not stop", r == 0 && w == 0);
    errno = 0;
    check("SEIZE with an unknown option: EIO", ptrace(PTRACE_SEIZE, pid, 0, 0x80000000ul) < 0 && errno == EIO);
    r = ptrace(PTRACE_INTERRUPT, pid, 0, 0);
    wait_for(pid, &st, 0);
    memset(&si, 0, sizeof si);
    long gi = ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    check("INTERRUPT: a PTRACE_EVENT_STOP with SIGTRAP", r == 0 && stopped_with(st, SIGTRAP) && event_of(st) == PTRACE_EVENT_STOP && gi == 0 && si.si_code >> 8 == PTRACE_EVENT_STOP);
    errno = 0;
    r = ptrace(PTRACE_LISTEN, pid, 0, 0);
    check("LISTEN outside a group stop's trap still allowed for an EVENT_STOP", r == 0);
    /* (Listening: not reported, requests ESRCH; INTERRUPT traps it again.) */
    errno = 0;
    g = ptrace(PTRACE_GETREGS, pid, 0, &regs);
    w = wait_for(pid, &st, WNOHANG);
    check("a listening tracee: not reported, GETREGS ESRCH", g < 0 && errno == ESRCH && w == 0);
    ptrace(PTRACE_INTERRUPT, pid, 0, 0);
    wait_for(pid, &st, 0);
    check("INTERRUPT of a listening tracee: it traps again", stopped_with(st, SIGTRAP) && event_of(st) == PTRACE_EVENT_STOP);
    ptrace(PTRACE_CONT, pid, 0, 0);
    kill(pid, SIGSTOP);
    wait_for(pid, &st, 0);
    int sigstop_delivery = stopped_with(st, SIGSTOP) && event_of(st) == 0;
    ptrace(PTRACE_CONT, pid, 0, SIGSTOP);
    wait_for(pid, &st, 0);
    check("SEIZE: SIGSTOP's delivery stop, then the group stop as EVENT_STOP", sigstop_delivery && stopped_with(st, SIGSTOP) && event_of(st) == PTRACE_EVENT_STOP);
    r = ptrace(PTRACE_LISTEN, pid, 0, 0);
    w = wait_for(pid, &st, WNOHANG);
    check("LISTEN: the tracee stays stopped, unreported", r == 0 && w == 0);
    kill(pid, SIGCONT);
    wait_for(pid, &st, 0);
    check("SIGCONT: the listening tracee traps (EVENT_STOP, SIGTRAP)", stopped_with(st, SIGTRAP) && event_of(st) == PTRACE_EVENT_STOP);
    ptrace(PTRACE_CONT, pid, 0, 0);
    wait_for(pid, &st, 0);
    check("then SIGCONT's own delivery stop", stopped_with(st, SIGCONT) && event_of(st) == 0);
    ptrace(PTRACE_CONT, pid, 0, 0);
    reap_killed(pid);

    /* strace's start: SEIZE a stopped process and SIGCONT it at once. Whichever the first
     * trap reports (the group stop, or its end), a LISTEN after a group stop's trap must
     * still hear of the SIGCONT: no notice is lost between a trap's decision and its stop. */
    int heard = 0;
    for (int i = 0; i < 20; i++) {
        pid = sleeper();
        kill(pid, SIGSTOP);
        wait_for(pid, &st, WSTOPPED);
        ptrace(PTRACE_SEIZE, pid, 0, 0);
        kill(pid, SIGCONT);
        wait_for(pid, &st, 0);
        if (stopped_with(st, SIGSTOP) && event_of(st) == PTRACE_EVENT_STOP) {
            ptrace(PTRACE_LISTEN, pid, 0, 0);
            wait_for(pid, &st, 0);
        }
        heard += stopped_with(st, SIGTRAP) && event_of(st) == PTRACE_EVENT_STOP;
        ptrace(PTRACE_CONT, pid, 0, 0);
        reap_killed(pid);
    }
    check("SEIZE of a stopped process and SIGCONT at once: the end is told", heard == 20);
}

/* ------------------------------------------------------------------ peek, poke, int3 */

__attribute__((noipa)) int target_fn(int x) {
    __asm__ volatile("" ::: "memory");
    return x * 2 + 40;
}

static volatile int one = 1;
static volatile long data_word = 0x1122334455667788;

static int call_target(void) {
    int r = target_fn(one);
    return r == 42 ? (data_word == 0x0102030405060708 ? 0 : 2) : 1;
}

static void peek_poke(void) {
    pid_t pid = traced_child(call_target);
    if (pid < 0) {
        check("peek/poke: traced child", 0);
        return;
    }
    errno = 0;
    long text = ptrace(PTRACE_PEEKTEXT, pid, (void *)target_fn, 0);
    check("PEEKTEXT reads the tracee's code", errno == 0 && memcmp(&text, (void *)target_fn, 8) == 0);
    errno = 0;
    long data = ptrace(PTRACE_PEEKDATA, pid, (void *)&data_word, 0);
    check("PEEKDATA reads its data", errno == 0 && data == 0x1122334455667788);
    long r = ptrace(PTRACE_POKEDATA, pid, (void *)&data_word, (void *)0x0102030405060708);
    check("POKEDATA writes its data", r == 0 && data_word == 0x1122334455667788);
    long bp = (text & ~0xffl) | 0xcc;
    r = ptrace(PTRACE_POKETEXT, pid, (void *)target_fn, (void *)bp);
    errno = 0;
    long now = ptrace(PTRACE_PEEKTEXT, pid, (void *)target_fn, 0);
    check("POKETEXT writes an int3 into read-only text", r == 0 && errno == 0 && now == bp);
    check("the tracer's own text (its fork relative) is untouched", *(volatile unsigned char *)target_fn != 0xcc);
    ptrace(PTRACE_CONT, pid, 0, 0);
    int st;
    wait_for(pid, &st, 0);
    siginfo_t si;
    memset(&si, 0, sizeof si);
    ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    struct user_regs_struct regs;
    ptrace(PTRACE_GETREGS, pid, 0, &regs);
    check("the breakpoint: SIGTRAP (SI_KERNEL), rip after the int3", stopped_with(st, SIGTRAP) && si.si_code == SI_KERNEL && regs.rip == (unsigned long)target_fn + 1);
    r = ptrace(PTRACE_POKETEXT, pid, (void *)target_fn, (void *)text);
    regs.rip = (unsigned long)target_fn;
    long s = ptrace(PTRACE_SETREGS, pid, 0, &regs);
    check("restored and rewound: the call runs, the poked data seen", r == 0 && s == 0 && finish(pid) == 0);
    /* A new program of the same file sees its own text: the page cache was not written. */
    fflush(stdout);
    int fds[2];
    pipe(fds);
    pid = fork();
    if (pid == 0) {
        dup2(fds[1], 1);
        char *argv[] = {"ptracetest", "firstbyte", NULL};
        execv("/bin/ptracetest", argv);
        _exit(99);
    }
    close(fds[1]);
    char out[16] = {0};
    read(fds[0], out, sizeof out - 1);
    close(fds[0]);
    wait_for(pid, &st, 0);
    check("a fresh exec of the program has no int3 (no write to the file)", strtol(out, NULL, 16) == *(unsigned char *)target_fn);

    pid = traced_child(call_target);
    errno = 0;
    r = ptrace(PTRACE_PEEKDATA, pid, (void *)8, 0);
    check("PEEKDATA of an unmapped address: EIO", r == -1 && errno == EIO);
    errno = 0;
    r = ptrace(PTRACE_POKEDATA, pid, (void *)8, 0);
    check("POKEDATA of an unmapped address: EIO", r == -1 && errno == EIO);
    errno = 0;
    r = ptrace(PTRACE_PEEKDATA, pid, (void *)0xffff800000000000ul, 0);
    check("PEEKDATA of a kernel address: EIO", r == -1 && errno == EIO);
    errno = 0;
    r = ptrace(PTRACE_PEEKUSER, pid, 3, 0);
    check("PEEKUSER at an unaligned offset: EIO", r == -1 && errno == EIO);
    errno = 0;
    r = ptrace(PTRACE_POKEUSER, pid, offsetof(struct user_regs_struct, cs), (void *)0x23);
    check("POKEUSER of another code segment: EIO", r == -1 && errno == EIO);
    reap_killed(pid);
}

/* ------------------------------------------------------------------ syscall stops */

static int syscall_body(void) {
    int bad = 0;
    long a = syscall(SYS_getpid, 0x1111);
    if (a != 12345) bad |= 1;
    errno = 0;
    long b = syscall(SYS_getpid, 0x2222);
    if (b != -1 || errno != ENOSYS) bad |= 2;
    long c = syscall(SYS_getppid, 0x3333);
    if (c != syscall(SYS_gettid)) bad |= 4;
    return bad;
}

static void syscall_stops(void) {
    pid_t pid = traced_child(syscall_body);
    ptrace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACESYSGOOD);
    int st, in_call = 0, entries = 0, sysgood = 1, saw_info = 0, saw_rval = 0, skip_ok = 0, swapped = 0, enosys_shown = 1;
    long marker = 0;
    ptrace(PTRACE_SYSCALL, pid, 0, 0);
    for (;;) {
        if (wait_for(pid, &st, 0) != pid || !WIFSTOPPED(st)) break;
        if (WSTOPSIG(st) != (SIGTRAP | 0x80)) {
            sysgood = WSTOPSIG(st) == SIGTRAP ? 0 : sysgood;
            ptrace(PTRACE_SYSCALL, pid, 0, WSTOPSIG(st) == SIGSTOP ? 0 : WSTOPSIG(st));
            continue;
        }
        struct user_regs_struct regs;
        ptrace(PTRACE_GETREGS, pid, 0, &regs);
        struct __ptrace_syscall_info info;
        memset(&info, 0, sizeof info);
        long size = ptrace(PTRACE_GET_SYSCALL_INFO, pid, sizeof info, &info);
        unsigned long msg = 0;
        ptrace(PTRACE_GETEVENTMSG, pid, 0, &msg);
        if (!in_call) {
            entries++;
            if ((long)regs.rax != -ENOSYS) enosys_shown = 0;
            marker = regs.rdi;
            if (regs.orig_rax == SYS_getpid && marker == 0x1111) {
                saw_info = size == 80 && info.op == PTRACE_SYSCALL_INFO_ENTRY && info.entry.nr == SYS_getpid && info.entry.args[0] == 0x1111 &&
                           info.arch == 0xc000003e && msg == 1;
            } else if (regs.orig_rax == SYS_getpid && marker == 0x2222) {
                regs.orig_rax = -1;
                ptrace(PTRACE_SETREGS, pid, 0, &regs);
            } else if (regs.orig_rax == SYS_getppid && marker == 0x3333) {
                regs.orig_rax = SYS_gettid;
                ptrace(PTRACE_SETREGS, pid, 0, &regs);
                swapped = 1;
            }
        } else {
            if (marker == 0x1111 && regs.orig_rax == SYS_getpid) {
                saw_rval = (long)regs.rax == pid && size == 33 && info.op == PTRACE_SYSCALL_INFO_EXIT && info.exit.rval == pid && !info.exit.is_error && msg == 2;
                regs.rax = 12345;
                ptrace(PTRACE_SETREGS, pid, 0, &regs);
            } else if (marker == 0x2222 && (long)regs.orig_rax == -1) {
                skip_ok = (long)regs.rax == -ENOSYS;
            }
        }
        in_call = !in_call;
        ptrace(PTRACE_SYSCALL, pid, 0, 0);
    }
    check("syscall stops: SIGTRAP|0x80 with TRACESYSGOOD, -ENOSYS in rax at entry", sysgood && enosys_shown && entries >= 4);
    check("GET_SYSCALL_INFO at entry (nr, args, arch) and exit (rval), event messages", saw_info && saw_rval);
    check("orig_rax -1 at entry skips the call (-ENOSYS)", skip_ok);
    check("a result changed at exit, a call replaced at entry: the tracee saw both", swapped && WIFEXITED(st) && WEXITSTATUS(st) == 0);

    /* PTRACE_SYSEMU: the entry stop, and the call is not made. */
    pid = traced_child(syscall_body);
    ptrace(PTRACE_SYSEMU, pid, 0, 0);
    wait_for(pid, &st, 0);
    struct user_regs_struct regs;
    ptrace(PTRACE_GETREGS, pid, 0, &regs);
    int at_entry = stopped_with(st, SIGTRAP) && (long)regs.rax == -ENOSYS;
    regs.rax = 4242;
    ptrace(PTRACE_SETREGS, pid, 0, &regs);
    ptrace(PTRACE_SYSEMU, pid, 0, 0);
    wait_for(pid, &st, 0);
    struct user_regs_struct after;
    ptrace(PTRACE_GETREGS, pid, 0, &after);
    /* The next stop is the next call's entry: the first call returned 4242 without a stop. */
    int sysemu_ok = at_entry && stopped_with(st, SIGTRAP) && (long)after.rax == -ENOSYS;
    if (!sysemu_ok) printf("  at_entry %d status %#x rax %ld orig %ld\n", at_entry, st, (long)after.rax, (long)after.orig_rax);
    check("SYSEMU: an entry stop, no exit stop, the call not made", sysemu_ok);
    reap_killed(pid);
}

/* ------------------------------------------------------------------ FPU state */

static int fpu_body(void) {
    unsigned long long out;
    unsigned long long v = 0x1122334455667788ull;
    pid_t me = getpid();
    __asm__ volatile("movq %[v], %%xmm7\n\t"
                     "mov $62, %%eax\n\t"
                     "mov %[pid], %%edi\n\t"
                     "mov $19, %%esi\n\t"
                     "syscall\n\t"
                     "movq %%xmm7, %[o]"
                     : [o] "=r"(out)
                     : [v] "r"(v), [pid] "r"(me)
                     : "rax", "rdi", "rsi", "rcx", "r11", "xmm7", "memory");
    return out == 0x8877665544332211ull ? 0 : 1;
}

static void fpu_regs(void) {
    pid_t pid = traced_child(fpu_body);
    ptrace(PTRACE_CONT, pid, 0, 0);
    int st;
    wait_for(pid, &st, 0);
    struct user_fpregs_struct fp;
    memset(&fp, 0, sizeof fp);
    long r = ptrace(PTRACE_GETFPREGS, pid, 0, &fp);
    unsigned long long xmm7;
    memcpy(&xmm7, &fp.xmm_space[28], 8);
    check("GETFPREGS: the tracee's xmm7 at its SIGSTOP", r == 0 && stopped_with(st, SIGSTOP) && xmm7 == 0x1122334455667788ull && fp.cwd == 0x37f);
    xmm7 = 0x8877665544332211ull;
    memcpy(&fp.xmm_space[28], &xmm7, 8);
    r = ptrace(PTRACE_SETFPREGS, pid, 0, &fp);
    struct user_fpregs_struct bad = fp;
    bad.mxcsr = 0xffffffff;
    errno = 0;
    long b = ptrace(PTRACE_SETFPREGS, pid, 0, &bad);
    check("SETFPREGS (an MXCSR with reserved bits: EINVAL)", r == 0 && b < 0 && errno == EINVAL);
    struct iovec iov = {&bad, sizeof bad};
    errno = 0;
    long x = ptrace(PTRACE_GETREGSET, pid, NT_X86_XSTATE, &iov);
    struct user_regs_struct rs;
    iov.iov_base = &rs;
    iov.iov_len = sizeof rs;
    long p = ptrace(PTRACE_GETREGSET, pid, NT_PRSTATUS, &iov);
    check("GETREGSET: NT_PRSTATUS (216 bytes), no XSAVE state (ENODEV)", x < 0 && errno == ENODEV && p == 0 && iov.iov_len == sizeof rs && rs.cs == 0x33);
    check("the tracee resumes with the changed xmm7", finish(pid) == 0);
}

/* ------------------------------------------------------------------ single steps */

extern char step_start[], step_end[], sc_before[], sc_after[];

__attribute__((noinline)) static int step_body(void) {
    __asm__ volatile("int3\n"
                     ".globl step_start\n"
                     "step_start:\n"
                     "nop\n nop\n nop\n nop\n nop\n"
                     ".globl step_end\n"
                     "step_end:\n"
                     "int3\n"
                     ".globl sc_before\n"
                     "sc_before:\n"
                     "mov $39, %%eax\n"
                     "syscall\n"
                     ".globl sc_after\n"
                     "sc_after:\n"
                     "nop\n" ::
                         : "rax", "rcx", "r11", "memory");
    return 0;
}

static void single_step(void) {
    pid_t pid = traced_child(step_body);
    ptrace(PTRACE_CONT, pid, 0, 0);
    int st;
    wait_for(pid, &st, 0);
    struct user_regs_struct regs;
    ptrace(PTRACE_GETREGS, pid, 0, &regs);
    int at_start = stopped_with(st, SIGTRAP) && regs.rip == (unsigned long)step_start;
    int steps = 0, codes = 1, tf_hidden = 1;
    while (regs.rip != (unsigned long)step_end && steps < 20) {
        ptrace(PTRACE_SINGLESTEP, pid, 0, 0);
        wait_for(pid, &st, 0);
        siginfo_t si;
        ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
        if (!stopped_with(st, SIGTRAP) || si.si_code != TRAP_TRACE) codes = 0;
        ptrace(PTRACE_GETREGS, pid, 0, &regs);
        if (regs.eflags & 0x100) tf_hidden = 0;
        steps++;
    }
    if (!(at_start && steps == 5 && codes))
        printf("  at_start %d steps %d codes %d rip %#lx start %p end %p status %#x\n", at_start, steps, codes, regs.rip, (void *)step_start, (void *)step_end, st);
    check("SINGLESTEP: five steps over five nops, each TRAP_TRACE", at_start && steps == 5 && codes);
    check("the single step's TF is hidden from GETREGS", tf_hidden);
    /* On to the int3 before the system call, then step over it. */
    ptrace(PTRACE_CONT, pid, 0, 0);
    wait_for(pid, &st, 0);
    ptrace(PTRACE_GETREGS, pid, 0, &regs);
    int at_sc = stopped_with(st, SIGTRAP) && regs.rip == (unsigned long)sc_before;
    ptrace(PTRACE_SINGLESTEP, pid, 0, 0);
    wait_for(pid, &st, 0);
    ptrace(PTRACE_GETREGS, pid, 0, &regs);
    int at_syscall = regs.rip == (unsigned long)sc_before + 5;
    ptrace(PTRACE_SINGLESTEP, pid, 0, 0);
    wait_for(pid, &st, 0);
    siginfo_t si;
    ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    ptrace(PTRACE_GETREGS, pid, 0, &regs);
    check("a step over syscall stops after it (TRAP_BRKPT), with its result", at_sc && at_syscall && stopped_with(st, SIGTRAP) && si.si_code == TRAP_BRKPT && regs.rip == (unsigned long)sc_after && (long)regs.rax == pid);
    check("the stepped tracee runs on and exits", finish(pid) == 0);
}

/* ------------------------------------------------------------------ hardware breakpoints */

static volatile int watched;

static int hw_body(void) {
    watched = 5;
    return target_fn(one) == 42 ? 0 : 1;
}

static void hw_breakpoints(void) {
    pid_t pid = traced_child(hw_body);
    long dr = offsetof(struct user, u_debugreg);
    /* DR0: a 4-byte write watchpoint on `watched`; DR1: an instruction breakpoint. */
    long a = ptrace(PTRACE_POKEUSER, pid, dr, (void *)&watched);
    long b = ptrace(PTRACE_POKEUSER, pid, dr + 8, (void *)target_fn);
    unsigned long dr7 = 1 | (1ul << 16) | (3ul << 18) | (1ul << 2);
    long c = ptrace(PTRACE_POKEUSER, pid, dr + 7 * 8, (void *)dr7);
    errno = 0;
    long back = ptrace(PTRACE_PEEKUSER, pid, dr + 7 * 8, 0);
    errno = 0;
    long bad = ptrace(PTRACE_POKEUSER, pid, dr + 7 * 8, (void *)(dr7 | (2ul << 20)));
    int bad_errno = errno;
    check("POKEUSER of DR0, DR1, DR7 (an I/O breakpoint: EINVAL)", a == 0 && b == 0 && c == 0 && (unsigned long)back == dr7 && bad < 0 && bad_errno == EINVAL);
    ptrace(PTRACE_CONT, pid, 0, 0);
    int st;
    wait_for(pid, &st, 0);
    siginfo_t si;
    memset(&si, 0, sizeof si);
    ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    errno = 0;
    long dr6 = ptrace(PTRACE_PEEKUSER, pid, dr + 6 * 8, 0);
    check("a watchpoint hit: SIGTRAP (TRAP_HWBKPT), DR6 says slot 0", stopped_with(st, SIGTRAP) && si.si_code == TRAP_HWBKPT && (dr6 & 0xf) == 1);
    ptrace(PTRACE_CONT, pid, 0, 0);
    wait_for(pid, &st, 0);
    struct user_regs_struct regs;
    ptrace(PTRACE_GETREGS, pid, 0, &regs);
    dr6 = ptrace(PTRACE_PEEKUSER, pid, dr + 6 * 8, 0);
    check("an instruction breakpoint: SIGTRAP before the instruction, DR6 slot 1", stopped_with(st, SIGTRAP) && regs.rip == (unsigned long)target_fn && (dr6 & 0xf) == 2);
    check("resumed past it (RF): the call runs and the tracee exits", finish(pid) == 0);
}

/* ------------------------------------------------------------------ events */

static void *thread_fn(void *arg) {
    (void)arg;
    return NULL;
}

static int events_body(void) {
    fflush(stdout);
    pid_t g = fork();
    if (g == 0) _exit(3);
    int st;
    if (wait_for(g, &st, 0) != g || !WIFEXITED(st) || WEXITSTATUS(st) != 3) return 100;
    pid_t v = vfork();
    if (v == 0) _exit(4);
    if (wait_for(v, &st, 0) != v || !WIFEXITED(st) || WEXITSTATUS(st) != 4) return 101;
    pthread_t th;
    if (pthread_create(&th, NULL, thread_fn, NULL) != 0) return 102;
    pthread_join(th, NULL);
    char *argv[] = {"ptracetest", "exit5", NULL};
    execv("/bin/ptracetest", argv);
    return 103;
}

static void events(void) {
    pid_t pid = traced_child(events_body);
    long r = ptrace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACEFORK | PTRACE_O_TRACEVFORK | PTRACE_O_TRACEVFORKDONE | PTRACE_O_TRACECLONE | PTRACE_O_TRACEEXEC | PTRACE_O_TRACEEXIT);
    errno = 0;
    long bad = ptrace(PTRACE_SETOPTIONS, pid, 0, 0x400000);
    check("SETOPTIONS (an unknown option: EINVAL)", r == 0 && bad < 0 && errno == EINVAL);
    ptrace(PTRACE_CONT, pid, 0, 0);
    int forks = 0, vforks = 0, vfork_dones = 0, clones = 0, execs = 0, exit_events = 0, exited = 0, new_stops = 0, msgs = 1, final = -1;
    pid_t g = 0, v = 0, t = 0;
    for (;;) {
        int st;
        pid_t w = wait_for(-1, &st, __WALL);
        if (w < 0) break;
        if (WIFEXITED(st) || WIFSIGNALED(st)) {
            exited++;
            if (w == pid) {
                final = WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
                break;
            }
            continue;
        }
        unsigned long msg = 0;
        ptrace(PTRACE_GETEVENTMSG, w, 0, &msg);
        int inject = 0;
        switch (event_of(st)) {
        case PTRACE_EVENT_FORK:
            forks++;
            g = msg;
            break;
        case PTRACE_EVENT_VFORK:
            vforks++;
            v = msg;
            break;
        case PTRACE_EVENT_VFORK_DONE:
            vfork_dones++;
            if (msg != (unsigned long)v) msgs = 0;
            break;
        case PTRACE_EVENT_CLONE:
            clones++;
            t = msg;
            break;
        case PTRACE_EVENT_EXEC:
            execs++;
            if (msg != (unsigned long)pid || w != pid) msgs = 0;
            break;
        case PTRACE_EVENT_EXIT:
            exit_events++;
            if (w == g && msg != 3 << 8) msgs = 0;
            if (w == v && msg != 4 << 8) msgs = 0;
            if (w == pid && msg != 5 << 8) msgs = 0;
            break;
        case 0:
            /* A new tracee's first stop (SIGSTOP), or a signal to pass on. */
            if (WSTOPSIG(st) == SIGSTOP && (w == g || w == v || w == t)) {
                new_stops++;
            } else if (WSTOPSIG(st) != SIGTRAP) {
                inject = WSTOPSIG(st);
            }
            break;
        }
        ptrace(PTRACE_CONT, w, 0, inject);
    }
    check("PTRACE_EVENT_FORK, VFORK, VFORK_DONE, CLONE with the new ids", forks == 1 && vforks == 1 && vfork_dones == 1 && clones == 1 && g > 0 && v > 0 && t > 0);
    check("each new tracee starts with a SIGSTOP", new_stops == 3);
    check("PTRACE_EVENT_EXEC (its old id), PTRACE_EVENT_EXIT (the status)", execs == 1 && exit_events == 4 && msgs);
    check("every tracee's end reported; the real parent's waits saw them reaped", exited == 4 && final == 5);
}

/* ------------------------------------------------------------------ signals */

static volatile sig_atomic_t usr1;

static void on_usr1(int sig) {
    (void)sig;
    usr1 = 1;
}

static int signal_body(void) {
    signal(SIGUSR1, on_usr1);
    /* (tkill itself: raise blocks every signal around it, which would hold an injected
     * one back until its mask is restored, then stop for it once more.) */
    syscall(SYS_tkill, syscall(SYS_gettid), SIGUSR2);
    syscall(SYS_tkill, syscall(SYS_gettid), SIGSTOP);
    return usr1 ? 0 : 1;
}

static void signals(void) {
    pid_t pid = traced_child(signal_body);
    ptrace(PTRACE_CONT, pid, 0, 0);
    int st;
    wait_for(pid, &st, 0);
    siginfo_t si;
    memset(&si, 0, sizeof si);
    ptrace(PTRACE_GETSIGINFO, pid, 0, &si);
    check("a fatal signal's delivery stop (SIGUSR2 from tkill)", stopped_with(st, SIGUSR2) && si.si_signo == SIGUSR2 && si.si_code == SI_TKILL);
    unsigned long mask = 0;
    long m = ptrace(PTRACE_GETSIGMASK, pid, 8, &mask);
    errno = 0;
    long mb = ptrace(PTRACE_GETSIGMASK, pid, 4, &mask);
    check("GETSIGMASK (a size other than 8: EINVAL)", m == 0 && mb < 0 && errno == EINVAL);
    /* Suppressed: it survives. */
    ptrace(PTRACE_CONT, pid, 0, 0);
    wait_for(pid, &st, 0);
    check("suppressed (CONT with 0): the tracee lives on to its SIGSTOP", stopped_with(st, SIGSTOP));
    /* Replaced by SIGUSR1: its handler runs. */
    ptrace(PTRACE_CONT, pid, 0, SIGUSR1);
    wait_for(pid, &st, 0);
    if (!(WIFEXITED(st) && WEXITSTATUS(st) == 0)) printf("  status %#x\n", st);
    check("a signal injected on CONT runs the tracee's handler", WIFEXITED(st) && WEXITSTATUS(st) == 0);

    /* A signal the tracee ignores still stops it (the tracer sees every signal). */
    pid = sleeper();
    ptrace(PTRACE_SEIZE, pid, 0, 0);
    kill(pid, SIGWINCH);
    wait_for(pid, &st, 0);
    check("an ignored signal (SIGWINCH) still stops a tracee", stopped_with(st, SIGWINCH));
    siginfo_t info;
    memset(&info, 0, sizeof info);
    struct __ptrace_peeksiginfo_args args = {0, 0, 4};
    kill(pid, SIGUSR1);
    sigqueue(pid, SIGRTMIN, (union sigval){.sival_int = 77});
    sleep_ms(10);
    siginfo_t queued[4];
    long n = ptrace(PTRACE_PEEKSIGINFO, pid, &args, queued);
    args.flags = PTRACE_PEEKSIGINFO_SHARED;
    long ns = ptrace(PTRACE_PEEKSIGINFO, pid, &args, queued);
    int found = 0;
    for (long i = 0; i < ns; i++) found |= queued[i].si_signo == SIGRTMIN && queued[i].si_value.sival_int == 77;
    check("PEEKSIGINFO: the process's queued signals", n == 0 && ns == 2 && found);
    waitid(P_PID, pid, &info, WEXITED | WSTOPPED | WNOHANG | WNOWAIT);
    reap_killed(pid);
}

/* ------------------------------------------------------------------ the tracer's end */

static void tracer_end(void) {
    /* A counter in shared memory: the child's progress. */
    volatile long *counter = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        for (;;) {
            (*counter)++;
            sleep_ms(1);
        }
    }
    pid_t tracer = fork();
    if (tracer == 0) {
        int st;
        if (ptrace(PTRACE_ATTACH, c, 0, 0) < 0) _exit(1);
        wait_for(c, &st, 0);
        /* Exits with the tracee stopped and reported: it runs on (its SIGSTOP was seen). */
        _exit(0);
    }
    int st;
    wait_for(tracer, &st, 0);
    long before = *counter;
    sleep_ms(50);
    check("the tracer's end detaches a stopped tracee, which runs on", WIFEXITED(st) && WEXITSTATUS(st) == 0 && *counter > before);
    tracer = fork();
    if (tracer == 0) {
        if (ptrace(PTRACE_SEIZE, c, 0, PTRACE_O_EXITKILL) < 0) _exit(1);
        _exit(0);
    }
    wait_for(tracer, &st, 0);
    int cst;
    pid_t w = wait_for(c, &cst, 0);
    check("PTRACE_O_EXITKILL: the tracer's end kills the tracee", w == c && WIFSIGNALED(cst) && WTERMSIG(cst) == SIGKILL);
    munmap((void *)counter, 4096);
}

/* ------------------------------------------------------------------ process_vm, /proc/pid/mem */

static char shared_buf[64] = "process_vm source text";

__attribute__((noipa)) int ret_const(void) {
    __asm__ volatile("" ::: "memory");
    return 0x11223344;
}

static void memory_calls(void) {
    int to_child[2], from_child[2];
    pipe(to_child);
    pipe(from_child);
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        char go;
        read(to_child[0], &go, 1);
        int ok = strcmp(shared_buf, "written by process_vm") == 0;
        int patched = ret_const() == 0x55667788;
        char res = (ok ? 1 : 0) | (patched ? 2 : 0);
        write(from_child[1], &res, 1);
        read(to_child[0], &go, 1);
        _exit(0);
    }
    char got[32] = {0};
    struct iovec local = {got, 22};
    struct iovec remote = {shared_buf, 22};
    ssize_t n = process_vm_readv(c, &local, 1, &remote, 1, 0);
    check("process_vm_readv reads another process's memory", n == 22 && strcmp(got, "process_vm source text") == 0);
    char text[] = "written by process_vm";
    local.iov_base = text;
    local.iov_len = sizeof text;
    remote.iov_len = sizeof text;
    n = process_vm_writev(c, &local, 1, &remote, 1, 0);
    check("process_vm_writev writes it", n == (ssize_t)sizeof text && strcmp(shared_buf, "process_vm source text") == 0);
    struct iovec two[2] = {{shared_buf, 8}, {(void *)8, 8}};
    local.iov_base = got;
    local.iov_len = 16;
    n = process_vm_readv(c, &local, 1, two, 2, 0);
    check("a partial read stops at the first bad remote range", n == 8);
    errno = 0;
    n = process_vm_readv(c, &local, 1, &two[1], 1, 0);
    check("an unmapped remote range: EFAULT", n < 0 && errno == EFAULT);
    remote.iov_base = (void *)ret_const;
    remote.iov_len = 4;
    errno = 0;
    n = process_vm_writev(c, &local, 1, &remote, 1, 0);
    check("process_vm_writev to read-only text: EFAULT (no FOLL_FORCE)", n < 0 && errno == EFAULT);
    errno = 0;
    n = process_vm_readv(c, &local, 1, &remote, 1, 1);
    check("flags other than 0: EINVAL", n < 0 && errno == EINVAL);
    errno = 0;
    n = process_vm_readv(32760, &local, 1, &remote, 1, 0);
    check("no such process: ESRCH", n < 0 && errno == ESRCH);

    char path[64];
    snprintf(path, sizeof path, "/proc/%d/mem", c);
    int fd = open(path, O_RDWR);
    char peek[8] = {0};
    n = pread(fd, peek, 7, (off_t)(uintptr_t)shared_buf);
    check("/proc/<pid>/mem: pread at an address", fd >= 0 && n == 7 && memcmp(peek, "written", 7) == 0);
    /* Patch ret_const's immediate in the child's read-only text (forced, as Linux's). */
    unsigned char code[16];
    memcpy(code, (void *)ret_const, sizeof code);
    int at = -1;
    for (int i = 0; i + 4 <= 16; i++) {
        if (code[i] == 0x44 && code[i + 1] == 0x33 && code[i + 2] == 0x22 && code[i + 3] == 0x11) {
            at = i;
            break;
        }
    }
    unsigned char imm[4] = {0x88, 0x77, 0x66, 0x55};
    n = at >= 0 ? pwrite(fd, imm, 4, (off_t)(uintptr_t)ret_const + at) : -1;
    check("/proc/<pid>/mem: a write into read-only text", n == 4 && ret_const() == 0x11223344);
    lseek(fd, (off_t)8, SEEK_SET);
    errno = 0;
    n = read(fd, peek, 8);
    check("/proc/<pid>/mem: an unmapped address reads EIO", n < 0 && errno == EIO);
    write(to_child[1], "g", 1);
    char res = 0;
    read(from_child[0], &res, 1);
    if (res != 3) printf("  child saw %d\n", res);
    check("the child saw process_vm's write and its patched code", res == 3);
    write(to_child[1], "g", 1);
    int st;
    wait_for(c, &st, 0);
    n = pread(fd, peek, 4, (off_t)(uintptr_t)shared_buf);
    check("/proc/<pid>/mem of a process that ended reads nothing", n == 0);
    close(fd);

    /* /proc/self/mem: its own read-only text, forced. */
    fd = open("/proc/self/mem", O_RDWR);
    unsigned char orig = *(volatile unsigned char *)target_fn;
    unsigned char ret = 0xc3;
    n = pwrite(fd, &ret, 1, (off_t)(uintptr_t)target_fn);
    int patched = *(volatile unsigned char *)target_fn == 0xc3;
    pwrite(fd, &orig, 1, (off_t)(uintptr_t)target_fn);
    check("/proc/self/mem: a write into its own read-only text", fd >= 0 && n == 1 && patched && *(volatile unsigned char *)target_fn == orig);
    close(fd);
    close(to_child[0]);
    close(to_child[1]);
    close(from_child[0]);
    close(from_child[1]);
}

/* ------------------------------------------------------------------ waitid of a stop */

static int nothing(void) { return 0; }

/* ------------------------------------------------------------------ a program's own TF */

static volatile int tf_traps, tf_codes = 1;

static void tf_handler(int sig, siginfo_t *si, void *uc) {
    (void)sig;
    (void)uc;
    tf_traps++;
    if (si->si_code != TRAP_TRACE) tf_codes = 0;
}

/* TF set by popf traps after each instruction from the next on (the handler runs without
 * it, its sigreturn sets it again): six traps up to the popf that clears it. */
static void own_trap_flag(void) {
    struct sigaction sa, old;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = tf_handler;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGTRAP, &sa, &old);
    __asm__ volatile("pushf\n orq $0x100, (%%rsp)\n popf\n nop\n nop\n nop\n pushf\n andq $~0x100, (%%rsp)\n popf\n" ::: "memory", "cc");
    sigaction(SIGTRAP, &old, NULL);
    check("a program's own TF: a SIGTRAP (TRAP_TRACE) after each instruction", tf_traps == 6 && tf_codes);
}

static void waitid_stop(void) {
    pid_t pid = traced_child(nothing);
    /* (The stop was reported by traced_child's wait; a new one now.) */
    ptrace(PTRACE_SETOPTIONS, pid, 0, PTRACE_O_TRACESYSGOOD);
    ptrace(PTRACE_SYSCALL, pid, 0, 0);
    siginfo_t si;
    memset(&si, 0, sizeof si);
    int r = waitid(P_PID, pid, &si, WEXITED | WSTOPPED);
    check("waitid of a ptrace stop: CLD_TRAPPED, its code", r == 0 && si.si_pid == pid && si.si_code == CLD_TRAPPED_CODE && si.si_status == (SIGTRAP | 0x80));
    errno = 0;
    long k = ptrace(PTRACE_KILL, pid, 0, 0);
    int st;
    wait_for(pid, &st, 0);
    check("PTRACE_KILL", k == 0 && WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL);
    errno = 0;
    long g = ptrace(PTRACE_GETREGS, getppid(), 0, &si);
    check("a request on a process not traced: ESRCH", g < 0 && errno == ESRCH);
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "exit7") == 0) return 7;
    if (argc > 1 && strcmp(argv[1], "exit5") == 0) return 5;
    if (argc > 1 && strcmp(argv[1], "firstbyte") == 0) {
        printf("%x\n", *(unsigned char *)target_fn);
        return 0;
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    own_trap_flag();
    traceme_exec();
    attach_seize();
    peek_poke();
    syscall_stops();
    fpu_regs();
    single_step();
    hw_breakpoints();
    events();
    signals();
    tracer_end();
    memory_calls();
    waitid_stop();
    if (failures) {
        printf("ptracetest: %d FAILED\n", failures);
        return 1;
    }
    printf("ptracetest: all passed\n");
    return 0;
}
