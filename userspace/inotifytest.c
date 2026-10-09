/* inotify in the Linux server, on tmpfs (/tmp) and on /data: the events
 * of creating, writing, changing, moving and removing files in a watched
 * directory and of a watched file itself, IN_ONESHOT, IN_ONLYDIR,
 * IN_MASK_CREATE, rm_watch and IN_IGNORED, merged events, reads of whole
 * events, FIONREAD, non-blocking and blocking reads, and poll. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-64s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

/* The events queued now, as "MASK:name" words ("" for none). */
static char events[4096];
static int last_cookie[2];

static const char *drain(int fd) {
    static char buf[8192] __attribute__((aligned(8)));
    events[0] = 0;
    int cookies = 0;
    for (;;) {
        ssize_t n = read(fd, buf, sizeof buf);
        if (n <= 0) break;
        for (char *p = buf; p < buf + n;) {
            struct inotify_event *e = (struct inotify_event *)p;
            const char *kind = e->mask & IN_CREATE ? "CREATE" : e->mask & IN_DELETE ? "DELETE" : e->mask & IN_MODIFY ? "MODIFY"
                             : e->mask & IN_ATTRIB ? "ATTRIB" : e->mask & IN_MOVED_FROM ? "FROM" : e->mask & IN_MOVED_TO ? "TO"
                             : e->mask & IN_DELETE_SELF ? "DELSELF" : e->mask & IN_MOVE_SELF ? "MOVESELF" : e->mask & IN_IGNORED ? "IGNORED"
                             : e->mask & IN_OPEN ? "OPEN" : e->mask & IN_CLOSE_WRITE ? "CLOSEW" : e->mask & IN_CLOSE_NOWRITE ? "CLOSE"
                             : e->mask & IN_ACCESS ? "ACCESS" : "?";
            char word[96];
            snprintf(word, sizeof word, "%s%s%s%s ", kind, e->mask & IN_ISDIR ? "+DIR" : "", e->len ? ":" : "", e->len ? e->name : "");
            strncat(events, word, sizeof events - strlen(events) - 1);
            if (e->cookie && cookies < 2) last_cookie[cookies++] = (int)e->cookie;
            p += sizeof *e + e->len;
        }
    }
    return events;
}

static void on(const char *dir) {
    char d[96], f[128], g[128], sub[128], name[160];
    snprintf(d, sizeof d, "%s/inotify.%d", dir, getpid());
    mkdir(d, 0755);
    snprintf(f, sizeof f, "%s/f", d);
    snprintf(g, sizeof g, "%s/g", d);
    snprintf(sub, sizeof sub, "%s/sub", d);
    int in = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    int wd = inotify_add_watch(in, d, IN_CREATE | IN_DELETE | IN_MODIFY | IN_ATTRIB | IN_MOVED_FROM | IN_MOVED_TO | IN_DELETE_SELF);
    snprintf(name, sizeof name, "%s: a watch on a directory", dir);
    check(name, in >= 0 && wd > 0);
    snprintf(name, sizeof name, "%s: the same inode gives the same watch", dir);
    check(name, inotify_add_watch(in, d, IN_CREATE | IN_DELETE | IN_MODIFY | IN_ATTRIB | IN_MOVED_FROM | IN_MOVED_TO | IN_DELETE_SELF) == wd);
    errno = 0;
    snprintf(name, sizeof name, "%s: IN_MASK_CREATE on a watched inode is EEXIST", dir);
    check(name, inotify_add_watch(in, d, IN_CREATE | IN_MASK_CREATE) == -1 && errno == EEXIST);

    int fd = open(f, O_CREAT | O_WRONLY, 0644);
    write(fd, "a", 1);
    write(fd, "b", 1);
    fchmod(fd, 0600);
    close(fd);
    mkdir(sub, 0755);
    rename(f, g);
    unlink(g);
    rmdir(sub);
    const char *got = drain(in);
    printf("    (%s)\n", got);
    snprintf(name, sizeof name, "%s: create, modify (merged), attrib, mkdir, move, delete", dir);
    check(name, strcmp(got, "CREATE:f MODIFY:f ATTRIB:f CREATE+DIR:sub FROM:f TO:g DELETE:g DELETE+DIR:sub ") == 0);
    snprintf(name, sizeof name, "%s: a move's two events share a cookie", dir);
    check(name, last_cookie[0] != 0 && last_cookie[0] == last_cookie[1]);

    /* A watch on a file: its own events, then its removal. */
    fd = open(f, O_CREAT | O_WRONLY, 0644);
    int fw = inotify_add_watch(in, f, IN_ALL_EVENTS);
    drain(in);
    write(fd, "x", 1);
    close(fd);
    fd = open(f, O_RDONLY);
    char c;
    read(fd, &c, 1);
    close(fd);
    got = drain(in);
    snprintf(name, sizeof name, "%s: a watched file: modify, close, open, access, close", dir);
    check(name, fw > 0 && fw != wd && strcmp(got, "MODIFY MODIFY:f CLOSEW OPEN ACCESS CLOSE ") == 0);
    printf("    (%s)\n", got);
    unlink(f);
    got = drain(in);
    snprintf(name, sizeof name, "%s: removing it: attrib, delete, delete_self, ignored", dir);
    check(name, strcmp(got, "ATTRIB DELETE:f DELSELF IGNORED ") == 0);
    printf("    (%s)\n", got);

    /* IN_ONESHOT, IN_ONLYDIR, rm_watch. */
    close(open(f, O_CREAT | O_WRONLY, 0644));
    int once = inotify_add_watch(in, f, IN_ATTRIB | IN_ONESHOT);
    drain(in);
    chmod(f, 0600);
    chmod(f, 0644);
    got = drain(in);
    snprintf(name, sizeof name, "%s: IN_ONESHOT: one event, then ignored", dir);
    check(name, once > 0 && strstr(got, "ATTRIB IGNORED ") == got);
    errno = 0;
    snprintf(name, sizeof name, "%s: IN_ONLYDIR on a file is ENOTDIR", dir);
    check(name, inotify_add_watch(in, f, IN_ATTRIB | IN_ONLYDIR) == -1 && errno == ENOTDIR);
    snprintf(name, sizeof name, "%s: rm_watch queues IN_IGNORED, then EINVAL", dir);
    check(name, inotify_rm_watch(in, wd) == 0 && strcmp(drain(in), "IGNORED ") == 0 && inotify_rm_watch(in, wd) == -1 && errno == EINVAL);
    unlink(f);
    rmdir(d);
    close(in);
}

static int blocking_fd;

static void *creator(void *arg) {
    usleep(100 * 1000);
    close(open((const char *)arg, O_CREAT | O_WRONLY, 0644));
    return NULL;
}

/* A file removed while open goes at its last close: IN_DELETE_SELF then. */
static void unlinked_open(const char *dir) {
    char f[96], name[128];
    snprintf(f, sizeof f, "%s/inotify-open.%d", dir, getpid());
    int fd = open(f, O_CREAT | O_RDWR, 0644);
    int in = inotify_init1(IN_NONBLOCK);
    inotify_add_watch(in, f, IN_ATTRIB | IN_DELETE_SELF | IN_CLOSE_WRITE);
    unlink(f);
    const char *got = drain(in);
    snprintf(name, sizeof name, "%s: removed while open: the link count only", dir);
    check(name, strcmp(got, "ATTRIB ") == 0);
    close(fd);
    got = drain(in);
    snprintf(name, sizeof name, "%s: ... IN_DELETE_SELF at the last close", dir);
    check(name, strcmp(got, "CLOSEW DELSELF IGNORED ") == 0);
    printf("    (%s)\n", got);
    close(in);
}

/* fs.inotify's limits: instances, watches, queued events. */
static void limits(void) {
    int fds[200], n = 0;
    while (n < 200 && (fds[n] = inotify_init1(0)) >= 0) n++;
    int e = errno;
    for (int i = 0; i < n; i++) close(fds[i]);
    check("at most 128 instances, then EMFILE", n == 128 && e == EMFILE);

    int in = inotify_init1(IN_NONBLOCK);
    check("the same kernel file is one watch", inotify_add_watch(in, "/dev/null", IN_ATTRIB) == inotify_add_watch(in, "/dev/null", IN_ATTRIB));
    char d[64], f[96];
    snprintf(d, sizeof d, "/tmp/inotify-lim.%d", getpid());
    mkdir(d, 0755);
    int watches = 1, wd = 0;
    for (int i = 0; i < 9000; i++) {
        snprintf(f, sizeof f, "%s/%d", d, i);
        close(open(f, O_CREAT | O_WRONLY, 0644));
        wd = inotify_add_watch(in, f, IN_ATTRIB);
        if (wd < 0) break;
        watches++;
    }
    e = errno;
    printf("    (%d watches)\n", watches);
    check("at most 8192 watches, then ENOSPC", wd == -1 && e == ENOSPC && watches == 8192);
    close(in);
    for (int i = 0; i < 9000; i++) {
        snprintf(f, sizeof f, "%s/%d", d, i);
        unlink(f);
    }

    /* More events than the queue takes: one IN_Q_OVERFLOW. */
    in = inotify_init1(IN_NONBLOCK);
    inotify_add_watch(in, d, IN_CREATE | IN_DELETE);
    snprintf(f, sizeof f, "%s/x", d);
    for (int i = 0; i < 8300; i++) {
        close(open(f, O_CREAT | O_WRONLY, 0644));
        unlink(f);
    }
    static char buf[1 << 16] __attribute__((aligned(8)));
    int total = 0, overflow = 0, after = 0;
    for (;;) {
        ssize_t r = read(in, buf, sizeof buf);
        if (r <= 0) break;
        for (char *p = buf; p < buf + r;) {
            struct inotify_event *ev = (struct inotify_event *)p;
            if (ev->mask & IN_Q_OVERFLOW) overflow++;
            else if (overflow) after++;
            else total++;
            p += sizeof *ev + ev->len;
        }
    }
    printf("    (%d events, %d overflow)\n", total, overflow);
    check("16384 events, then one IN_Q_OVERFLOW", total == 16384 && overflow == 1 && after == 0);
    close(in);
    rmdir(d);
}

int main(void) {
    on("/tmp");
    on("/data");
    unlinked_open("/tmp");
    unlinked_open("/data");
    limits();

    int in = inotify_init();
    char d[64], f[96];
    snprintf(d, sizeof d, "/tmp/inotify-b.%d", getpid());
    snprintf(f, sizeof f, "%s/new", d);
    mkdir(d, 0755);
    inotify_add_watch(in, d, IN_CREATE);
    char small[8];
    errno = 0;
    close(open(f, O_CREAT | O_WRONLY, 0644));
    int avail = 0;
    check("FIONREAD counts the queued bytes", ioctl(in, FIONREAD, &avail) == 0 && avail == (int)(sizeof(struct inotify_event) + 16));
    check("a buffer too small for an event is EINVAL", read(in, small, sizeof small) == -1 && errno == EINVAL);
    char buf[256];
    struct inotify_event *e = (struct inotify_event *)buf;
    check("the name is padded to 16 bytes", read(in, buf, sizeof buf) == (ssize_t)(sizeof *e + 16) && e->len == 16 && strcmp(e->name, "new") == 0);
    unlink(f);
    struct pollfd p = {.fd = in, .events = POLLIN};
    check("poll: nothing to read", poll(&p, 1, 0) == 0);
    pthread_t t;
    blocking_fd = in;
    pthread_create(&t, NULL, creator, f);
    check("poll wakes for an event", poll(&p, 1, 2000) == 1 && (p.revents & POLLIN));
    pthread_join(t, NULL);
    read(in, buf, sizeof buf);
    unlink(f);
    pthread_create(&t, NULL, creator, f);
    check("a blocking read waits for the event", read(in, buf, sizeof buf) > 0 && strcmp(e->name, "new") == 0);
    pthread_join(t, NULL);
    unlink(f);
    errno = 0;
    check("an inotify call on another file is EINVAL", inotify_add_watch(0, d, IN_CREATE) == -1 && errno == EINVAL);
    errno = 0;
    check("... on no file EBADF", inotify_rm_watch(999, 1) == -1 && errno == EBADF);
    close(in);
    rmdir(d);
    printf("inotifytest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
