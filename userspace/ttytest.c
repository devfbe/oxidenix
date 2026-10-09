/* Terminals, the Linux server's (phase R6d), driven through pseudo-terminals: termios
 * round trips, canonical and raw reads with VMIN and VTIME, echo and line editing,
 * output processing, flow control, poll, window sizes, the controlling terminal, the
 * foreground process group and the signals of ^C, ^\ and ^Z, SIGTTIN and SIGTTOU for a
 * background group, hangups, devpts, and the console's device node. (The console's own
 * input is the keyboard's: checked interactively, not here.) */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    fflush(stdout);
    if (!ok) failures++;
}

static int64_t now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static void sleep_ms(int ms) {
    struct timespec d = {ms / 1000, (ms % 1000) * 1000000L};
    while (nanosleep(&d, &d) == -1 && errno == EINTR) {
    }
}

/* A new pair: the master's descriptor; the slave's path at `path`. */
static int new_pty(char *path, size_t len) {
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    if (m < 0) return -1;
    if (grantpt(m) || unlockpt(m) || ptsname_r(m, path, len)) {
        close(m);
        return -1;
    }
    return m;
}

/* Everything the master can read now (after the slave's output settled). */
static int drain(int m, char *buf, int cap) {
    int got = 0;
    struct pollfd p = {m, POLLIN, 0};
    while (got < cap && poll(&p, 1, 50) == 1 && (p.revents & POLLIN)) {
        int n = read(m, buf + got, cap - got);
        if (n <= 0) break;
        got += n;
    }
    buf[got < cap ? got : cap - 1] = 0;
    return got;
}

static void set_raw(int fd, int vmin, int vtime) {
    struct termios t;
    tcgetattr(fd, &t);
    t.c_lflag &= ~(ICANON | ECHO);
    t.c_cc[VMIN] = vmin;
    t.c_cc[VTIME] = vtime;
    tcsetattr(fd, TCSANOW, &t);
}

static void devices(void) {
    char path[64];
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    check("/dev/ptmx opens a master", m >= 0);
    int n = -1;
    check("TIOCGPTN gives its index", ioctl(m, TIOCGPTN, &n) == 0 && n >= 0);
    snprintf(path, sizeof path, "/dev/pts/%d", n);
    struct stat st;
    check("its /dev/pts node is a character device (136, n)",
          stat(path, &st) == 0 && S_ISCHR(st.st_mode) && major(st.st_rdev) == 136 && minor(st.st_rdev) == (unsigned)n);
    int s = open(path, O_RDWR | O_NOCTTY);
    check("a locked slave does not open (EIO)", s == -1 && errno == EIO);
    unlockpt(m);
    s = open(path, O_RDWR | O_NOCTTY);
    check("unlocked, it opens", s >= 0);
    int lock = 1;
    check("TIOCGPTLCK says unlocked", ioctl(m, TIOCGPTLCK, &lock) == 0 && lock == 0);
    check("both ends are terminals", isatty(m) && isatty(s));
    struct stat ms;
    check("fstat of the master is /dev/ptmx's node (5, 2)", fstat(m, &ms) == 0 && S_ISCHR(ms.st_mode) && major(ms.st_rdev) == 5 && minor(ms.st_rdev) == 2);
    int p = ioctl(m, TIOCGPTPEER, O_RDWR | O_NOCTTY);
    check("TIOCGPTPEER opens the slave from the master", p >= 0 && isatty(p));
    close(p);
    int seen = 0;
    DIR *d = opendir("/dev/pts");
    struct dirent *e;
    while (d && (e = readdir(d))) seen += atoi(e->d_name) == n && e->d_name[0] != 'p' && e->d_type == DT_CHR;
    if (d) closedir(d);
    check("/dev/pts lists it", seen == 1);
    char other[64];
    snprintf(other, sizeof other, "/dev/pts/x%d", n);
    check("devpts takes no names of programs (EACCES)", open(other, O_CREAT | O_RDWR, 0600) == -1 && errno == EACCES);
    check("nor removes its own (EPERM)", unlink(path) == -1 && errno == EPERM);
    int pfd[2];
    pipe(pfd);
    check("a pipe is no terminal (ENOTTY)", !isatty(pfd[0]) && errno == ENOTTY);
    close(pfd[0]);
    close(pfd[1]);
    close(s);
    close(m);
    check("the master's close removes the node", stat(path, &st) == -1 && errno == ENOENT);

    int ptys = 0;
    d = opendir("/dev/pts");
    while (d && (e = readdir(d))) ptys += e->d_name[0] >= '0' && e->d_name[0] <= '9';
    if (d) closedir(d);
    int op = open("/dev/ptmx", O_PATH);
    int after = 0;
    d = opendir("/dev/pts");
    while (d && (e = readdir(d))) after += e->d_name[0] >= '0' && e->d_name[0] <= '9';
    if (d) closedir(d);
    check("O_PATH opens /dev/ptmx's node, not a pty", op >= 0 && after == ptys && !isatty(op));
    struct stat ps;
    char one;
    check("an O_PATH descriptor: the node's status, EBADF for reads and writes",
          fstat(op, &ps) == 0 && S_ISCHR(ps.st_mode) && major(ps.st_rdev) == 5 && minor(ps.st_rdev) == 2 &&
              read(op, &one, 1) == -1 && errno == EBADF && write(op, "x", 1) == -1 && errno == EBADF);
    close(op);
    int mp = posix_openpt(O_RDWR | O_NOCTTY);
    unlockpt(mp);
    char pp[64];
    ptsname_r(mp, pp, sizeof pp);
    op = open(pp, O_PATH | O_RDWR);
    check("O_PATH on a devpts node ignores the access mode (EBADF)", op >= 0 && write(op, "x", 1) == -1 && errno == EBADF);
    close(op);
    close(mp);
    check("O_DIRECTORY on a device node: ENOTDIR", open("/dev/tty", O_RDONLY | O_DIRECTORY) == -1 && errno == ENOTDIR);

    int c = open("/dev/console", O_RDWR | O_NOCTTY);
    struct winsize ws = {0};
    check("/dev/console is a terminal with a size", c >= 0 && isatty(c) && ioctl(c, TIOCGWINSZ, &ws) == 0 && ws.ws_row > 0 && ws.ws_col > 0);
    close(c);
    int t = open("/dev/tty", O_RDWR);
    if (t >= 0) {
        check("/dev/tty is the controlling terminal of this session", tcgetsid(t) == getsid(0));
        close(t);
    } else {
        check("/dev/tty without a controlling terminal: ENXIO", errno == ENXIO);
    }
}

static void termios_and_reads(void) {
    char path[64], buf[256];
    int m = new_pty(path, sizeof path);
    int s = open(path, O_RDWR | O_NOCTTY);
    struct termios t, u;
    tcgetattr(s, &t);
    check("defaults: ICANON ECHO ISIG ICRNL IXON OPOST ONLCR",
          (t.c_lflag & (ICANON | ECHO | ISIG)) == (ICANON | ECHO | ISIG) && (t.c_iflag & (ICRNL | IXON)) == (ICRNL | IXON) &&
              (t.c_oflag & (OPOST | ONLCR)) == (OPOST | ONLCR));
    check("defaults: ^C ^\\ DEL ^U ^D ^Z, VMIN 1", t.c_cc[VINTR] == 3 && t.c_cc[VQUIT] == 0x1c && t.c_cc[VERASE] == 0x7f &&
                                                 t.c_cc[VKILL] == 0x15 && t.c_cc[VEOF] == 4 && t.c_cc[VSUSP] == 0x1a && t.c_cc[VMIN] == 1);
    u = t;
    u.c_lflag &= ~ECHOCTL;
    u.c_cc[VKILL] = 0x18;
    cfsetospeed(&u, B9600);
    tcsetattr(s, TCSANOW, &u);
    struct termios v;
    tcgetattr(m, &v);
    check("termios round trip (and the master shows the slave's)",
          v.c_lflag == u.c_lflag && v.c_cc[VKILL] == 0x18 && cfgetospeed(&v) == B9600);
    tcsetattr(s, TCSANOW, &t);

    /* Canonical: a line at a time, echoed with CR LF. */
    fcntl(s, F_SETFL, O_NONBLOCK);
    write(m, "hello\rwor", 9);
    sleep_ms(20);
    int n = read(s, buf, sizeof buf);
    check("canonical read: one line, CR mapped to NL", n == 6 && memcmp(buf, "hello\n", 6) == 0);
    n = read(s, buf, sizeof buf);
    check("a half-typed line is not readable (EAGAIN)", n == -1 && errno == EAGAIN);
    int avail = -1;
    write(m, "ld\n", 3);
    sleep_ms(20);
    check("FIONREAD counts complete lines", ioctl(s, FIONREAD, &avail) == 0 && avail == 6);
    n = read(s, buf, 3);
    check("a short read takes part of a line", n == 3 && memcmp(buf, "wor", 3) == 0);
    n = read(s, buf, sizeof buf);
    check("the rest of the line follows", n == 3 && memcmp(buf, "ld\n", 3) == 0);
    n = drain(m, buf, sizeof buf);
    check("echo: hello CR LF world CR LF", n == 14 && memcmp(buf, "hello\r\nworld\r\n", 14) == 0);

    write(m, "abc\x7f\x7f" "d\x15xyz\x17q\n", 13);
    sleep_ms(20);
    n = read(s, buf, sizeof buf);
    check("erase, kill and werase edit the line", n == 2 && memcmp(buf, "q\n", 2) == 0);
    n = drain(m, buf, sizeof buf);
    check("erasing echoes backspace, space, backspace", n > 6 && memcmp(buf, "abc\b \b\b \b", 9) == 0);
    write(m, "xy\x04\x04", 4);
    sleep_ms(20);
    n = read(s, buf, sizeof buf);
    int n2 = read(s, buf + 10, sizeof buf - 10);
    check("^D ends a line; at a line's start it reads 0", n == 2 && memcmp(buf, "xy", 2) == 0 && n2 == 0);
    write(m, "\x16\x03\n", 3);
    sleep_ms(20);
    n = read(s, buf, sizeof buf);
    check("^V makes the next character literal", n == 2 && buf[0] == 3);
    drain(m, buf, sizeof buf);
    char c = 'z';
    ioctl(s, TIOCSTI, &c);
    c = '\n';
    ioctl(s, TIOCSTI, &c);
    n = read(s, buf, sizeof buf);
    check("TIOCSTI inserts input", n == 2 && buf[0] == 'z');
    drain(m, buf, sizeof buf);

    /* Output processing. */
    write(s, "a\nb\tc", 5);
    n = drain(m, buf, sizeof buf);
    check("output: NL becomes CR NL (ONLCR)", n == 6 && memcmp(buf, "a\r\nb\tc", 6) == 0);
    u = t;
    u.c_oflag |= TAB3;
    tcsetattr(s, TCSANOW, &u);
    write(s, "\r\tx", 3);
    n = drain(m, buf, sizeof buf);
    check("XTABS expands tabs to the next stop", n == 10 && memcmp(buf, "\r        x", 10) == 0);
    u.c_oflag &= ~OPOST;
    tcsetattr(s, TCSANOW, &u);
    write(s, "a\n", 2);
    n = drain(m, buf, sizeof buf);
    check("without OPOST nothing changes", n == 2 && memcmp(buf, "a\n", 2) == 0);
    tcsetattr(s, TCSANOW, &t);

    /* Noncanonical: VMIN and VTIME. */
    fcntl(s, F_SETFL, 0);
    set_raw(s, 0, 0);
    int64_t t0 = now_ms();
    n = read(s, buf, sizeof buf);
    check("VMIN 0 VTIME 0: returns 0 at once", n == 0 && now_ms() - t0 < 50);
    set_raw(s, 0, 2);
    t0 = now_ms();
    n = read(s, buf, sizeof buf);
    int64_t took = now_ms() - t0;
    check("VMIN 0 VTIME 2: 0 after 0.2 s", n == 0 && took >= 180 && took < 1000);
    write(m, "k", 1);
    t0 = now_ms();
    n = read(s, buf, sizeof buf);
    check("VMIN 0 VTIME 2: a byte that is there comes at once", n == 1 && buf[0] == 'k' && now_ms() - t0 < 100);
    set_raw(s, 3, 0);
    write(m, "12", 2);
    sleep_ms(20);
    struct pollfd pf = {s, POLLIN, 0};
    check("VMIN 3: poll waits for 3 bytes", poll(&pf, 1, 0) == 0);
    write(m, "3", 1);
    check("VMIN 3: readable with 3", poll(&pf, 1, 1000) == 1 && (pf.revents & POLLIN));
    n = read(s, buf, sizeof buf);
    check("VMIN 3: read takes them", n == 3 && memcmp(buf, "123", 3) == 0);
    set_raw(s, 5, 1);
    write(m, "ab", 2);
    t0 = now_ms();
    n = read(s, buf, sizeof buf);
    took = now_ms() - t0;
    check("VMIN 5 VTIME 1: the inter-byte timer ends it", n == 2 && took >= 80 && took < 1000);
    /* Raw input is not edited, not even NL. */
    set_raw(s, 1, 0);
    write(m, "a\x7f\x15\r", 4);
    sleep_ms(20);
    n = read(s, buf, sizeof buf);
    check("raw input: erase and kill are data, CR still mapped", n == 4 && memcmp(buf, "a\x7f\x15\n", 4) == 0);
    /* Leaving canonical mode makes a half-typed line readable. */
    tcsetattr(s, TCSANOW, &t);
    write(m, "half", 4);
    sleep_ms(20);
    set_raw(s, 1, 0);
    n = read(s, buf, sizeof buf);
    check("leaving canonical mode hands over a half-typed line", n == 4 && memcmp(buf, "half", 4) == 0);
    tcsetattr(s, TCSANOW, &t);
    drain(m, buf, sizeof buf);

    /* Flow control. */
    fcntl(s, F_SETFL, O_NONBLOCK);
    write(m, "\x13", 1);
    sleep_ms(20);
    n = write(s, "x", 1);
    check("^S stops output (a non-blocking write: EAGAIN)", n == -1 && errno == EAGAIN);
    pf = (struct pollfd){s, POLLOUT, 0};
    check("stopped output does not poll writable", poll(&pf, 1, 0) == 0);
    write(m, "\x11", 1);
    sleep_ms(20);
    check("^Q starts it again", write(s, "y", 1) == 1);
    tcflow(s, TCOOFF);
    check("tcflow(TCOOFF) stops it", write(s, "x", 1) == -1 && errno == EAGAIN);
    tcflow(s, TCOON);
    check("tcflow(TCOON) starts it", write(s, "z", 1) == 1);
    n = drain(m, buf, sizeof buf);
    check("the master read what was written", n == 2 && memcmp(buf, "yz", 2) == 0);
    fcntl(s, F_SETFL, 0);

    /* Poll and flushing. */
    pf = (struct pollfd){m, POLLIN, 0};
    check("the master is not readable with nothing written", poll(&pf, 1, 0) == 0);
    write(s, "p", 1);
    check("the master polls readable when the slave writes", poll(&pf, 1, 1000) == 1 && (pf.revents & POLLIN));
    tcflush(s, TCOFLUSH);
    check("tcflush(TCOFLUSH) drops it", poll(&pf, 1, 0) == 0);
    write(m, "line\n", 5);
    sleep_ms(20);
    tcflush(s, TCIFLUSH);
    check("tcflush(TCIFLUSH) drops input", ioctl(s, FIONREAD, &avail) == 0 && avail == 0);
    drain(m, buf, sizeof buf);

    /* Window size. */
    struct winsize ws = {0};
    ioctl(s, TIOCGWINSZ, &ws);
    check("a new pty's window is 0x0", ws.ws_row == 0 && ws.ws_col == 0);
    ws = (struct winsize){24, 80, 0, 0};
    ioctl(m, TIOCSWINSZ, &ws);
    struct winsize ws2 = {0};
    ioctl(s, TIOCGWINSZ, &ws2);
    check("TIOCSWINSZ on the master sets the slave's", ws2.ws_row == 24 && ws2.ws_col == 80);

    /* The slave's close: the master reads what is left, then EIO. */
    write(s, "bye", 3);
    close(s);
    n = read(m, buf, sizeof buf);
    check("the master reads what the slave left", n == 3 && memcmp(buf, "bye", 3) == 0);
    pf = (struct pollfd){m, POLLIN, 0};
    check("then polls POLLHUP", poll(&pf, 1, 0) == 1 && (pf.revents & POLLHUP));
    n = read(m, buf, sizeof buf);
    check("and reads EIO", n == -1 && errno == EIO);
    close(m);
}

/* Echoes of console input never wait for a program flooding the console (the server's
 * service thread echoes the keyboard's input; it must not stall behind such a write).
 * TIOCSTI takes the same path from a program thread: its echo must come back at once
 * while another process writes long palette changes (each a full redraw). */
static void console_flood(void) {
    int c = open("/dev/console", O_RDWR | O_NOCTTY);
    struct termios t;
    if (c < 0 || tcgetattr(c, &t) != 0 || !(t.c_lflag & ECHO)) {
        check("console flood: the console echoes", 0);
        return;
    }
    int ready[2];
    pipe(ready);
    pid_t f = fork();
    if (f == 0) {
        /* 10 palette changes per write, each a full redraw (about 0.35 s per write
         * under QEMU): a fair turn waits for one write of the flood, not for ever. */
        static char buf[100];
        for (int i = 0; i + 10 <= (int)sizeof buf; i += 10) memcpy(buf + i, (i / 10) % 2 ? "\033]P1ff0000" : "\033]P1000000", 10);
        write(ready[1], "r", 1);
        for (;;) write(c, buf, sizeof buf);
    }
    char r;
    read(ready[0], &r, 1);
    sleep_ms(100);
    /* Measured in a child: an echo stuck behind the flood ends it by its alarm. */
    int result[2];
    pipe(result);
    pid_t meter = fork();
    if (meter == 0) {
        alarm(10);
        int64_t worst[2] = {0, 0};
        for (int i = 0; i < 5; i++) {
            char x = 'x';
            int64_t t0 = now_ms();
            ioctl(c, TIOCSTI, &x);
            int64_t took = now_ms() - t0;
            if (took > worst[0]) worst[0] = took;
            sleep_ms(20);
        }
        write(result[1], &worst[0], sizeof worst[0]);
        /* A second writer gets its turn after the flood's current write, not never. */
        for (int i = 0; i < 3; i++) {
            int64_t t0 = now_ms();
            write(c, "\r", 1);
            int64_t took = now_ms() - t0;
            if (took > worst[1]) worst[1] = took;
        }
        write(result[1], &worst[1], sizeof worst[1]);
        _exit(0);
    }
    close(result[1]);
    int64_t worsts[2] = {-1, -1};
    for (int i = 0; i < 2; i++)
        if (read(result[0], &worsts[i], sizeof worsts[i]) != sizeof worsts[i]) break;
    int64_t worst = worsts[0];
    waitpid(meter, NULL, 0);
    close(result[0]);
    kill(f, SIGKILL);
    waitpid(f, NULL, 0);
    write(c, "\033]R\r\n", 5);
    tcflush(c, TCIFLUSH);
    close(c);
    printf("ttytest: during a console flood the slowest echo took %lld ms, the slowest write %lld ms\n", (long long)worst,
           (long long)worsts[1]);
    check("echoing console input does not wait for a flooding writer", worst >= 0 && worst < 100);
    check("a second console writer gets through a flood (turns are FIFO)", worsts[1] >= 0 && worsts[1] < 3000);
}

/* Echoes cannot grow a pty master's buffer without bound when the master never reads,
 * TIOCSTI on a full master drops, and TIOCSIG takes only the terminal's signals. */
static void pty_bounds(void) {
    char path[64], buf[4096];
    int m = new_pty(path, sizeof path);
    int s = open(path, O_RDWR | O_NOCTTY);
    struct termios t;
    tcgetattr(s, &t);
    t.c_lflag &= ~ICANON;
    t.c_lflag |= ECHO;
    t.c_cc[VMIN] = 1;
    t.c_cc[VTIME] = 0;
    tcsetattr(s, TCSANOW, &t);
    memset(buf, 'e', sizeof buf);
    for (int i = 0; i < 40; i++) {
        write(m, buf, 4000);
        int got = 0;
        while (got < 4000) {
            int n = read(s, buf, 4000 - got);
            if (n <= 0) break;
            got += n;
        }
    }
    int queued = -1;
    ioctl(m, FIONREAD, &queued);
    check("echoes stop at the master's bound when it never reads", queued > 0 && queued <= 64 * 1024 + 4096);
    char x = 'q';
    check("TIOCSTI on a full master drops (no growth)", ioctl(m, TIOCSTI, &x) == 0 && ioctl(m, FIONREAD, &queued) == 0 && queued <= 64 * 1024 + 4096);
    check("TIOCSIG refuses a signal that is not the terminal's", ioctl(m, TIOCSIG, SIGKILL) == -1 && errno == EINVAL);
    check("TIOCSIG takes SIGINT", ioctl(m, TIOCSIG, SIGINT) == 0);
    close(s);
    close(m);
}

/* Reads take whole turns and see mode switches: a canonical read waiting for a line
 * returns the half line when the terminal goes raw; two readers each get a whole long
 * line (never parts of both); a slave open that fails (EMFILE) leaves the master as it
 * was (not hung up). */
static void read_rules(void) {
    char path[64], buf[4096];
    int m = new_pty(path, sizeof path);
    int s = open(path, O_RDWR | O_NOCTTY);
    struct termios t;
    tcgetattr(s, &t);
    t.c_lflag &= ~ECHO;
    tcsetattr(s, TCSANOW, &t);
    int res[2];
    pipe(res);
    pid_t r = fork();
    if (r == 0) {
        alarm(5);
        int n = read(s, buf, 100);
        write(res[1], &n, sizeof n);
        _exit(0);
    }
    write(m, "abc", 3);
    sleep_ms(100);
    struct termios raw = t;
    raw.c_lflag &= ~ICANON;
    raw.c_cc[VMIN] = 1;
    raw.c_cc[VTIME] = 0;
    tcsetattr(s, TCSANOW, &raw);
    int n = -1;
    read(res[0], &n, sizeof n);
    waitpid(r, NULL, 0);
    check("a waiting canonical read takes the half line when the mode goes raw", n == 3);
    tcsetattr(s, TCSANOW, &t);

    /* Two readers, two lines of 3000 bytes (the second waits for room). */
    pid_t readers[2];
    for (int i = 0; i < 2; i++) {
        readers[i] = fork();
        if (readers[i] == 0) {
            alarm(5);
            int k = read(s, buf, sizeof buf);
            int whole = k == 3000 && buf[2999] == '\n';
            for (int j = 1; whole && j < 2999; j++) whole = buf[j] == buf[0];
            int report = whole ? buf[0] : -k;
            write(res[1], &report, sizeof report);
            _exit(0);
        }
    }
    sleep_ms(50);
    char line[3000];
    memset(line, 'A', sizeof line);
    line[2999] = '\n';
    write(m, line, sizeof line);
    memset(line, 'B', sizeof line - 1);
    write(m, line, sizeof line);
    int a = 0, b = 0;
    read(res[0], &a, sizeof a);
    read(res[0], &b, sizeof b);
    for (int i = 0; i < 2; i++) waitpid(readers[i], NULL, 0);
    check("two readers each get one whole line", (a == 'A' && b == 'B') || (a == 'B' && b == 'A'));
    close(s);

    /* A slave whose open fails: the master is not hung up. */
    char path2[64];
    int m2 = new_pty(path2, sizeof path2);
    pid_t c = fork();
    if (c == 0) {
        while (dup(0) >= 0) {
        }
        _exit(open(path2, O_RDWR | O_NOCTTY) == -1 && errno == EMFILE ? 0 : 1);
    }
    int status = 0;
    waitpid(c, &status, 0);
    struct pollfd p = {m2, POLLIN, 0};
    fcntl(m2, F_SETFL, O_NONBLOCK);
    int pr = poll(&p, 1, 0);
    int rr = read(m2, buf, 1);
    check("a slave open that fails (EMFILE) leaves the master open, not hung up",
          WIFEXITED(status) && WEXITSTATUS(status) == 0 && pr == 0 && rr == -1 && errno == EAGAIN);
    close(m2);
    close(m);
}

static void report_hup(int sig) {
    (void)sig;
    write(3, "H", 1);
}

/* A stopped job whose group the exit of its session's leader orphans gets SIGHUP and
 * SIGCONT (POSIX; Linux's kill_orphaned_pgrp), instead of staying stopped for ever. */
static void orphans(void) {
    int rep[2];
    pipe(rep);
    pid_t leader = fork();
    if (leader == 0) {
        setsid();
        pid_t job = fork();
        if (job == 0) {
            setpgid(0, 0);
            dup2(rep[1], 3);
            struct sigaction sa = {0};
            sa.sa_handler = report_hup;
            sigaction(SIGHUP, &sa, NULL);
            raise(SIGSTOP);
            _exit(0);
        }
        setpgid(job, job);
        int status;
        waitpid(job, &status, WUNTRACED);
        _exit(WIFSTOPPED(status) ? 0 : 1);
    }
    close(rep[1]);
    int status = 0;
    waitpid(leader, &status, 0);
    struct pollfd p = {rep[0], POLLIN, 0};
    char c = 0;
    int ok = WIFEXITED(status) && WEXITSTATUS(status) == 0 && poll(&p, 1, 5000) == 1 && read(rep[0], &c, 1) == 1 && c == 'H';
    check("an orphaned stopped job gets SIGHUP and SIGCONT", ok);
    close(rep[0]);
}

static volatile sig_atomic_t got;
static void on_signal(int sig) { got = sig; }

/* Job control in a session of its own whose controlling terminal is the slave: a
 * foreground job (a process group of its own) gets ^C, ^\, ^Z and SIGWINCH; in the
 * background it gets SIGTTIN for reading, SIGTTOU for writing with TOSTOP and for
 * tcsetattr; ignoring SIGTTIN makes the read EIO. Reports single bytes on `rep`. */
static int session(int m, const char *path, int rep) {
    int bad = 0;
#define EXPECT(cond) do { if (!(cond)) { bad++; dprintf(1, "  session check failed: line %d\n", __LINE__); } } while (0)
    /* A signal that never comes ends the test instead of hanging it. */
    alarm(20);
    setsid();
    int fd = open(path, O_RDWR);
    EXPECT(fd >= 0);
    EXPECT(tcgetsid(fd) == getpid());
    EXPECT(tcgetpgrp(fd) == getpgrp());
    int t = open("/dev/tty", O_RDWR);
    EXPECT(t >= 0 && isatty(t));
    close(t);
    /* As a shell: it takes the terminal back from the background. */
    signal(SIGTTOU, SIG_IGN);
    int ready[2];
    pipe(ready);
    pid_t job = fork();
    if (job == 0) {
        close(rep);
        setpgid(0, 0);
        struct sigaction sa = {0};
        sa.sa_handler = on_signal;
        sigaction(SIGINT, &sa, NULL);
        sigaction(SIGQUIT, &sa, NULL);
        sigaction(SIGWINCH, &sa, NULL);
        sigset_t block, old;
        sigemptyset(&block);
        sigaddset(&block, SIGINT);
        sigaddset(&block, SIGQUIT);
        sigaddset(&block, SIGWINCH);
        sigprocmask(SIG_BLOCK, &block, &old);
        write(ready[1], "r", 1);
        for (int want = 0; want < 3; want++) {
            while (!got) sigsuspend(&old);
            char c = got == SIGINT ? 'I' : got == SIGQUIT ? 'Q' : 'W';
            got = 0;
            write(ready[1], &c, 1);
        }
        sigprocmask(SIG_SETMASK, &old, NULL);
        /* Stopped by ^Z here (SIGTSTP's default). */
        for (;;) pause();
    }
    setpgid(job, job);
    char c;
    read(ready[0], &c, 1);
    EXPECT(tcsetpgrp(fd, job) == 0);
    EXPECT(tcgetpgrp(fd) == job);
    write(m, "\x03", 1);
    read(ready[0], &c, 1);
    EXPECT(c == 'I');
    write(rep, "I", 1);
    write(m, "\x1c", 1);
    read(ready[0], &c, 1);
    EXPECT(c == 'Q');
    write(rep, "Q", 1);
    struct winsize ws = {30, 100, 0, 0};
    ioctl(m, TIOCSWINSZ, &ws);
    read(ready[0], &c, 1);
    EXPECT(c == 'W');
    write(rep, "W", 1);
    write(m, "\x1a", 1);
    int status = 0;
    EXPECT(waitpid(job, &status, WUNTRACED) == job && WIFSTOPPED(status) && WSTOPSIG(status) == SIGTSTP);
    write(rep, "Z", 1);
    /* The shell takes the terminal back (ignoring SIGTTOU, as shells do). */
    EXPECT(tcsetpgrp(fd, getpgrp()) == 0);
    kill(job, SIGKILL);
    waitpid(job, NULL, 0);

    /* A background job reading and writing. */
    for (int round = 0; round < 4; round++) {
        if (round == 2) {
            struct termios tio;
            tcgetattr(fd, &tio);
            tio.c_lflag |= TOSTOP;
            tcsetattr(fd, TCSANOW, &tio);
        }
        job = fork();
        if (job == 0) {
            close(rep);
            setpgid(0, 0);
            signal(SIGTTOU, SIG_DFL);
            char b[8];
            if (round == 0) read(fd, b, 1);        /* SIGTTIN: stops */
            if (round == 1) _exit(write(fd, "w", 1) == 1 ? 0 : 1); /* no TOSTOP: goes */
            if (round == 2) write(fd, "w", 1);       /* TOSTOP: SIGTTOU stops */
            if (round == 3) {
                signal(SIGTTIN, SIG_IGN);
                _exit(read(fd, b, 1) == -1 && errno == EIO ? 0 : 1);
            }
            _exit(9);
        }
        setpgid(job, job);
        EXPECT(waitpid(job, &status, WUNTRACED) == job);
        if (round == 0) EXPECT(WIFSTOPPED(status) && WSTOPSIG(status) == SIGTTIN);
        if (round == 1 || round == 3) EXPECT(WIFEXITED(status) && WEXITSTATUS(status) == 0);
        if (round == 2) EXPECT(WIFSTOPPED(status) && WSTOPSIG(status) == SIGTTOU);
        if (WIFSTOPPED(status)) {
            kill(job, SIGKILL);
            waitpid(job, NULL, 0);
        }
        write(rep, "B", 1);
    }
    /* tcsetattr from the background: SIGTTOU (not ignored there). */
    job = fork();
    if (job == 0) {
        close(rep);
        setpgid(0, 0);
        signal(SIGTTOU, SIG_DFL);
        struct termios tio;
        tcgetattr(fd, &tio);
        tcsetattr(fd, TCSANOW, &tio);
        _exit(9);
    }
    setpgid(job, job);
    EXPECT(waitpid(job, &status, WUNTRACED) == job && WIFSTOPPED(status) && WSTOPSIG(status) == SIGTTOU);
    kill(job, SIGKILL);
    waitpid(job, NULL, 0);
    write(rep, "S", 1);
    /* TIOCSPGRP checks: a group of another session is EPERM, none ESRCH. */
    pid_t other = getpgid(getppid());
    pid_t none = 0x7ffffff0;
    EXPECT(tcsetpgrp(fd, other) == -1 && errno == EPERM);
    EXPECT(tcsetpgrp(fd, none) == -1 && errno == ESRCH);
    /* TIOCSCTTY: already ours; TIOCNOTTY gives it up (and, the leader's, sends the
     * foreground group, its own, SIGHUP). */
    EXPECT(ioctl(fd, TIOCSCTTY, 0) == 0);
    signal(SIGHUP, SIG_IGN);
    EXPECT(ioctl(fd, TIOCNOTTY) == 0);
    EXPECT(tcgetpgrp(fd) == -1 && errno == ENOTTY);
    EXPECT(open("/dev/tty", O_RDWR) == -1 && errno == ENXIO);
    EXPECT(ioctl(fd, TIOCSCTTY, 0) == 0);
    EXPECT(tcgetsid(fd) == getpid());
    return bad;
}

static void job_control(void) {
    char path[64];
    int m = new_pty(path, sizeof path);
    int rep[2];
    pipe(rep);
    int s = open(path, O_RDWR | O_NOCTTY);
    check("O_NOCTTY: not the opener's controlling terminal", tcgetpgrp(s) == -1 && errno == ENOTTY);
    close(s);
    pid_t leader = fork();
    if (leader == 0) {
        close(rep[0]);
        _exit(session(m, path, rep[1]));
    }
    close(rep[1]);
    char got_[16] = {0};
    int n = 0, r;
    while (n < (int)sizeof got_ - 1 && (r = read(rep[0], got_ + n, 1)) == 1) n++;
    int status = 0;
    waitpid(leader, &status, 0);
    check("^C sends SIGINT to the foreground group", strchr(got_, 'I') != NULL);
    check("^\\ sends SIGQUIT", strchr(got_, 'Q') != NULL);
    check("TIOCSWINSZ sends SIGWINCH", strchr(got_, 'W') != NULL);
    check("^Z stops the foreground job (SIGTSTP)", strchr(got_, 'Z') != NULL);
    check("background read: SIGTTIN; write: SIGTTOU with TOSTOP", strstr(got_, "BBBB") != NULL);
    check("background tcsetattr: SIGTTOU", strchr(got_, 'S') != NULL);
    check("every check of the session passed", WIFEXITED(status) && WEXITSTATUS(status) == 0);

    /* The session's leader ended: a new session leader takes the terminal (the
     * server learns of the end a little later). Then a hangup: the master's close
     * sends that leader SIGHUP; its slave reads 0 and fails writes with EIO. */
    int sync_[2];
    pipe(sync_);
    leader = fork();
    if (leader == 0) {
        alarm(10);
        /* The parent's close of the master must be its last. */
        close(m);
        setsid();
        int fd = open(path, O_RDWR);
        for (int i = 0; i < 200 && tcgetsid(fd) != getpid(); i++) {
            ioctl(fd, TIOCSCTTY, 0);
            sleep_ms(5);
        }
        int ours = tcgetsid(fd) == getpid();
        struct sigaction sa = {0};
        sa.sa_handler = on_signal;
        sigaction(SIGHUP, &sa, NULL);
        sigset_t block, old;
        sigemptyset(&block);
        sigaddset(&block, SIGHUP);
        sigprocmask(SIG_BLOCK, &block, &old);
        write(sync_[1], ours ? "y" : "n", 1);
        if (!ours) _exit(2);
        while (got != SIGHUP) sigsuspend(&old);
        char b[4];
        int rd = read(fd, b, sizeof b);
        int wr = write(fd, "x", 1);
        int werr = errno;
        _exit(rd == 0 && wr == -1 && werr == EIO ? 0 : 1);
    }
    char c = 0;
    read(sync_[0], &c, 1);
    check("the leader's end freed the terminal for a new session", c == 'y');
    close(m);
    status = 0;
    waitpid(leader, &status, 0);
    check("the master's close hangs the slave up: SIGHUP, EOF, EIO", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    check("a hung-up slave cannot be opened again", open(path, O_RDWR | O_NOCTTY) == -1);
}

int main(void) {
    signal(SIGTTOU, SIG_IGN);
    devices();
    termios_and_reads();
    console_flood();
    pty_bounds();
    read_rules();
    orphans();
    job_control();
    printf("ttytest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
