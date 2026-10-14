/* The kernel's log: syslog(2) (busybox's dmesg) and /dev/kmsg (util-linux's dmesg),
 * with Linux's formats, positions, blocking reads and poll. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/klog.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-52s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static void log_line(const char *text) {
    int fd = open("/dev/kmsg", O_WRONLY);
    write(fd, text, strlen(text));
    close(fd);
}

static void sleep_ms(int ms) {
    struct timespec d = {0, ms * 1000000L};
    nanosleep(&d, NULL);
}

static char all[256 * 1024];

/* syslog's READ_ALL into `all`: its length. */
static int read_all(void) {
    int n = klogctl(3, all, sizeof all - 1);
    all[n > 0 ? n : 0] = 0;
    return n;
}

/* The sequence number of a /dev/kmsg line. */
static long long seq_of(const char *line) {
    const char *comma = strchr(line, ',');
    return comma ? atoll(comma + 1) : -1;
}

int main(void) {
    char line[2048];
    check("SIZE_BUFFER is the log's size (128 KiB)", klogctl(10, NULL, 0) == 128 * 1024);

    /* A record written to /dev/kmsg, read back by syslog. */
    int w = open("/dev/kmsg", O_WRONLY);
    const char *hello = "<5>kmsgtest: hello\n";
    check("/dev/kmsg takes a write whole", w >= 0 && write(w, hello, strlen(hello)) == (ssize_t)strlen(hello));
    int n = read_all();
    check("READ_ALL has the kernel's own messages (<6>)", n > 0 && strstr(all, "<6>[") != NULL);
    char *at = strstr(all, "kmsgtest: hello\n");
    char *start = at;
    while (start && start > all && start[-1] != '\n') start--;
    check("READ_ALL has the record, user facility (<13>)", at != NULL && strncmp(start, "<13>[", 5) == 0);
    /* "<13>[    1.234567] kmsgtest: hello" */
    check("each line reads <prio>[seconds.micros] text", at != NULL && at - start == 19 && at[-2] == ']' && at[-9] == '.');

    /* /dev/kmsg: positions, one record a read, poll. */
    int r = open("/dev/kmsg", O_RDONLY | O_NONBLOCK);
    check("lseek(SEEK_END) goes past the newest", r >= 0 && lseek(r, 0, SEEK_END) == 0);
    check("then a read is EAGAIN", read(r, line, sizeof line) == -1 && errno == EAGAIN);
    struct pollfd p = {r, POLLIN, 0};
    check("and poll says not readable", poll(&p, 1, 0) == 0);
    write(w, "<14>kmsgtest: second\n", 21);
    p.revents = 0;
    check("a write makes it readable", poll(&p, 1, 1000) == 1 && (p.revents & POLLIN));
    n = read(r, line, sizeof line);
    if (n > 0) line[n] = 0;
    check("a read is one record: prio,seq,micros,-;text", n > 0 && strncmp(line, "14,", 3) == 0 && strstr(line, ",-;kmsgtest: second\n") != NULL && line[n - 1] == '\n');
    long long second = n > 0 ? seq_of(line) : -1;

    write(w, "kmsgtest: tab\there\\", 19);
    char small[8];
    check("a buffer too small for the record is EINVAL", read(r, small, sizeof small) == -1 && errno == EINVAL);
    n = read(r, line, sizeof line);
    if (n > 0) line[n] = 0;
    check("the record stays for the next read", n > 0 && strstr(line, "kmsgtest: tab") != NULL);
    check("control bytes and \\ are escaped as \\xNN", n > 0 && strstr(line, "tab\\x09here\\x5c\n") != NULL);
    check("level 4, user facility without a prefix (12)", n > 0 && strncmp(line, "12,", 3) == 0);
    check("sequence numbers count on", n > 0 && seq_of(line) == second + 1);

    check("lseek(SEEK_SET) goes to the oldest record", lseek(r, 0, SEEK_SET) == 0);
    n = read(r, line, sizeof line);
    if (n > 0) line[n] = 0;
    check("which is older than ours", n > 0 && seq_of(line) < second);
    check("lseek with an offset is ESPIPE", lseek(r, 5, SEEK_SET) == -1 && errno == ESPIPE);
    check("lseek(SEEK_CUR) is EINVAL", lseek(r, 0, SEEK_CUR) == -1 && errno == EINVAL);

    /* epoll, and a blocking read woken by a write. */
    lseek(r, 0, SEEK_END);
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN, .data.fd = r};
    check("epoll takes /dev/kmsg", epoll_ctl(ep, EPOLL_CTL_ADD, r, &ev) == 0);
    check("nothing ready yet", epoll_wait(ep, &ev, 1, 0) == 0);
    write(w, "kmsgtest: epoll\n", 16);
    check("epoll wakes for a new record", epoll_wait(ep, &ev, 1, 1000) == 1 && (ev.events & EPOLLIN));
    close(ep);

    pid_t child = fork();
    if (child == 0) {
        alarm(5);
        int b = open("/dev/kmsg", O_RDONLY);
        lseek(b, 0, SEEK_END);
        char got[2048];
        int k = read(b, got, sizeof got - 1);
        if (k > 0) got[k] = 0;
        _exit(k > 0 && strstr(got, "kmsgtest: wake") ? 0 : 1);
    }
    sleep_ms(50);
    write(w, "kmsgtest: wake\n", 15);
    int status;
    waitpid(child, &status, 0);
    check("a blocking read waits for the next record", WIFEXITED(status) && WEXITSTATUS(status) == 0);

    /* syslog's READ: a reader's position, records split across reads. */
    while (klogctl(9, NULL, 0) > 0) {
        if (klogctl(2, all, sizeof all) <= 0) break;
    }
    check("READ takes everything unread", klogctl(9, NULL, 0) == 0);
    write(w, "kmsgtest: unread\n", 17);
    int unread = klogctl(9, NULL, 0);
    check("SIZE_UNREAD counts a new record", unread > 17);
    char piece[10];
    check("a short READ takes part of a record", klogctl(2, piece, sizeof piece) == (int)sizeof piece);
    check("and SIZE_UNREAD the rest", klogctl(9, NULL, 0) == unread - (int)sizeof piece);
    n = klogctl(2, all, sizeof all - 1);
    if (n > 0) all[n] = 0;
    check("the next READ goes on in the record", n == unread - (int)sizeof piece && strstr(all, "] kmsgtest: unread\n") != NULL);
    child = fork();
    if (child == 0) {
        alarm(5);
        char got[512];
        int k = klogctl(2, got, sizeof got - 1);
        if (k > 0) got[k] = 0;
        _exit(k > 0 && strstr(got, "kmsgtest: syslog wake") ? 0 : 1);
    }
    sleep_ms(50);
    write(w, "kmsgtest: syslog wake\n", 22);
    waitpid(child, &status, 0);
    check("READ waits for a record", WIFEXITED(status) && WEXITSTATUS(status) == 0);

    /* READ_ALL's newest records that fit, CLEAR. */
    write(w, "kmsgtest: last\n", 15);
    n = klogctl(3, all, 64);
    all[n > 0 ? n : 0] = 0;
    check("READ_ALL gives the newest records that fit", n > 0 && n <= 64 && strstr(all, "kmsgtest: last\n") != NULL && all[n - 1] == '\n');
    check("CLEAR", klogctl(5, NULL, 0) == 0);
    write(w, "kmsgtest: after clear\n", 22);
    n = read_all();
    check("READ_ALL then has only what came after", n > 0 && strstr(all, "kmsgtest: hello") == NULL && strstr(all, "kmsgtest: after clear") != NULL);
    n = klogctl(4, all, sizeof all - 1);
    check("READ_CLEAR reads, then clears", n > 0 && read_all() == 0);

    /* The arguments, as Linux checks them. */
    check("READ_ALL without a buffer is EINVAL", klogctl(3, NULL, 10) == -1 && errno == EINVAL);
    check("a negative length is EINVAL", klogctl(3, all, -1) == -1 && errno == EINVAL);
    check("a length of 0 reads nothing", klogctl(3, all, 0) == 0);
    check("an unknown action is EINVAL", klogctl(99, NULL, 0) == -1 && errno == EINVAL);
    check("CONSOLE_LEVEL 9 is EINVAL", klogctl(8, NULL, 9) == -1 && errno == EINVAL);
    check("CONSOLE_LEVEL 7 (the default)", klogctl(8, NULL, 7) == 0);
    char big[1100];
    memset(big, 'x', sizeof big);
    check("a write beyond 1024 bytes is EINVAL", write(w, big, sizeof big) == -1 && errno == EINVAL);
    check("a write-only open reads EBADF", read(w, line, sizeof line) == -1 && errno == EBADF);
    close(r);
    close(w);

    /* Without the kernel's host grant (TEST_HOST, test mode only) the tree is Linux in a
     * container: the log is not its to read, its writes are ratelimited. */
    long had = syscall(1522, 0);
    errno = 0;
    int no_read = klogctl(3, all, sizeof all) == -1 && errno == EPERM;
    errno = 0;
    int no_clear = klogctl(5, NULL, 0) == -1 && errno == EPERM;
    errno = 0;
    int no_level = klogctl(8, NULL, 7) == -1 && errno == EPERM;
    int size_ok = klogctl(10, NULL, 0) == 128 * 1024;
    errno = 0;
    int no_open = open("/dev/kmsg", O_RDONLY) == -1 && errno == EPERM;
    int wo = open("/dev/kmsg", O_WRONLY);
    int writes = 0;
    for (int i = 0; i < 30; i++) {
        char text[64];
        int len = snprintf(text, sizeof text, "<7>kmsgtest: limited %d\n", i);
        writes += write(wo, text, len) == len;
    }
    close(wo);
    syscall(1522, 1);
    check("without the host grant reading the log is EPERM", had == 1 && no_read && no_clear && no_open);
    check("... and so is the console's level; SIZE_BUFFER is not", no_level && size_ok);
    n = read_all();
    int kept = 0;
    for (char *s = all; (s = strstr(s, "kmsgtest: limited ")); s++) kept++;
    printf("  (%d of 30 writes kept)\n", kept);
    check("... writes succeed, at most 10 per 5 s reach the log", writes == 30 && kept >= 1 && kept <= 10);

    /* busybox's dmesg reads it all with READ_ALL. */
    log_line("kmsgtest: for dmesg\n");
    check("busybox dmesg shows the log", system("dmesg | grep -q 'kmsgtest: for dmesg'") == 0);

    printf("kmsgtest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
