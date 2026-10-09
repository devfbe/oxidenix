/* AF_UNIX sockets (the Linux server's, phase R7a): stream, datagram and
 * seqpacket socket pairs, names in the filesystem (tmpfs and /data) and in
 * the abstract namespace, descriptors passed between processes
 * (SCM_RIGHTS, also outliving the sender's close, and cycles of sockets in
 * flight collected, also after an exit), calls that outlive another
 * thread's close, credentials (SCM_CREDENTIALS, SO_PEERCRED), shutdown,
 * EPIPE and SIGPIPE, nonblocking I/O and poll/epoll readiness: what
 * Node.js's child_process and net modules rely on. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static short revents(int fd, short events) {
    struct pollfd p = {fd, events, 0};
    return poll(&p, 1, 0) == 1 ? p.revents : 0;
}

static volatile int sigpipes;
static void on_sigpipe(int sig) {
    (void)sig;
    sigpipes++;
}

static socklen_t path_addr(struct sockaddr_un *a, const char *path) {
    memset(a, 0, sizeof *a);
    a->sun_family = AF_UNIX;
    strcpy(a->sun_path, path);
    return offsetof(struct sockaddr_un, sun_path) + strlen(path) + 1;
}

static socklen_t abstract_addr(struct sockaddr_un *a, const char *name) {
    memset(a, 0, sizeof *a);
    a->sun_family = AF_UNIX;
    memcpy(a->sun_path + 1, name, strlen(name));
    return offsetof(struct sockaddr_un, sun_path) + 1 + strlen(name);
}

/* Sends `n` descriptors with one byte of data. */
static int send_fds(int sock, const int *fds, int n) {
    char byte = 'F';
    struct iovec iov = {&byte, 1};
    char control[CMSG_SPACE(sizeof(int) * 8)];
    memset(control, 0, sizeof control);
    struct msghdr msg = {0};
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control;
    msg.msg_controllen = CMSG_SPACE(sizeof(int) * n);
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int) * n);
    memcpy(CMSG_DATA(c), fds, sizeof(int) * n);
    return sendmsg(sock, &msg, 0) == 1 ? 0 : -errno;
}

/* Receives one byte and up to `max` descriptors (control space for
 * `room` of them); returns how many came, -1 on error; `flags_out` gets
 * msg_flags. */
static int recv_fds(int sock, int *fds, int room, int flags, int *flags_out) {
    char byte;
    struct iovec iov = {&byte, 1};
    char control[CMSG_SPACE(sizeof(int) * 8)];
    struct msghdr msg = {0};
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control;
    msg.msg_controllen = room ? CMSG_SPACE(sizeof(int) * room) : 0;
    if (recvmsg(sock, &msg, flags) != 1) return -1;
    if (flags_out) *flags_out = msg.msg_flags;
    int n = 0;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&msg); c; c = CMSG_NXTHDR(&msg, c)) {
        if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) {
            int k = (c->cmsg_len - CMSG_LEN(0)) / sizeof(int);
            memcpy(fds + n, CMSG_DATA(c), sizeof(int) * k);
            n += k;
        }
    }
    return n;
}

static void stream_pair(void) {
    int sv[2];
    check("socketpair(AF_UNIX, SOCK_STREAM)", socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0);
    struct stat st;
    check("fstat says it is a socket", fstat(sv[0], &st) == 0 && S_ISSOCK(st.st_mode));
    check("a fresh pair is writable, not readable", revents(sv[0], POLLIN | POLLOUT) == POLLOUT);
    check("write to one end", write(sv[0], "hello", 5) == 5);
    check("the other end is readable", revents(sv[1], POLLIN) == POLLIN);
    int avail = -1;
    check("FIONREAD counts the bytes", ioctl(sv[1], FIONREAD, &avail) == 0 && avail == 5);
    char buf[64];
    check("MSG_PEEK leaves the data", recv(sv[1], buf, 3, MSG_PEEK) == 3 && memcmp(buf, "hel", 3) == 0);
    check("read takes it", read(sv[1], buf, sizeof buf) == 5 && memcmp(buf, "hello", 5) == 0);
    write(sv[1], "ab", 2);
    write(sv[1], "cd", 2);
    check("a stream read takes across writes", read(sv[0], buf, sizeof buf) == 4 && memcmp(buf, "abcd", 4) == 0);

    struct sockaddr_un a;
    socklen_t len = sizeof a;
    check("getsockname of a pair: just the family", getsockname(sv[0], (struct sockaddr *)&a, &len) == 0 && len == 2 && a.sun_family == AF_UNIX);
    len = sizeof a;
    check("getpeername of a pair: just the family", getpeername(sv[0], (struct sockaddr *)&a, &len) == 0 && len == 2);
    int type = 0, domain = 0;
    socklen_t ol = sizeof type;
    check("SO_TYPE is SOCK_STREAM", getsockopt(sv[0], SOL_SOCKET, SO_TYPE, &type, &ol) == 0 && type == SOCK_STREAM && ol == sizeof type);
    ol = sizeof domain;
    check("SO_DOMAIN is AF_UNIX", getsockopt(sv[0], SOL_SOCKET, SO_DOMAIN, &domain, &ol) == 0 && domain == AF_UNIX);
    struct ucred cred;
    ol = sizeof cred;
    check("SO_PEERCRED of a pair is the creator", getsockopt(sv[0], SOL_SOCKET, SO_PEERCRED, &cred, &ol) == 0 && cred.pid == getpid() && ol == sizeof cred);
    int sndbuf = 0;
    ol = sizeof sndbuf;
    int want = 16384;
    setsockopt(sv[0], SOL_SOCKET, SO_SNDBUF, &want, sizeof want);
    check("SO_SNDBUF is kept doubled", getsockopt(sv[0], SOL_SOCKET, SO_SNDBUF, &sndbuf, &ol) == 0 && sndbuf == 32768);
    check("options of other levels: EOPNOTSUPP", setsockopt(sv[0], IPPROTO_TCP, 1, &want, sizeof want) == -1 && errno == EOPNOTSUPP);
    check("lseek on a socket: ESPIPE", lseek(sv[0], 0, SEEK_SET) == -1 && errno == ESPIPE);

    /* shutdown(SHUT_WR): the peer reads what is there, then end of file. */
    write(sv[0], "xy", 2);
    check("shutdown(SHUT_WR)", shutdown(sv[0], SHUT_WR) == 0);
    check("the peer sees POLLRDHUP", (revents(sv[1], POLLIN | POLLRDHUP) & (POLLIN | POLLRDHUP)) == (POLLIN | POLLRDHUP));
    check("the peer reads the rest", read(sv[1], buf, sizeof buf) == 2);
    check("then end of file", read(sv[1], buf, sizeof buf) == 0);
    check("writing after SHUT_WR: EPIPE", send(sv[0], "z", 1, MSG_NOSIGNAL) == -1 && errno == EPIPE);
    check("the peer can still write back", write(sv[1], "back", 4) == 4 && read(sv[0], buf, sizeof buf) == 4);

    /* The peer closes: POLLHUP, EPIPE and SIGPIPE. */
    close(sv[1]);
    check("after the peer closed: POLLHUP and POLLIN", (revents(sv[0], POLLIN) & (POLLIN | POLLHUP)) == (POLLIN | POLLHUP));
    check("read after the peer closed: end of file", read(sv[0], buf, sizeof buf) == 0);
    signal(SIGPIPE, on_sigpipe);
    sigpipes = 0;
    int r = write(sv[0], "x", 1);
    check("write to a closed peer: EPIPE and SIGPIPE", r == -1 && errno == EPIPE && sigpipes == 1);
    r = send(sv[0], "x", 1, MSG_NOSIGNAL);
    check("MSG_NOSIGNAL: EPIPE without SIGPIPE", r == -1 && errno == EPIPE && sigpipes == 1);
    signal(SIGPIPE, SIG_DFL);
    close(sv[0]);

    /* Closing with unread data resets the connection. */
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    write(sv[0], "unread", 6);
    write(sv[1], "left", 4);
    close(sv[1]);
    check("unread data, then ECONNRESET, then end of file",
          read(sv[0], buf, sizeof buf) == 4 && read(sv[0], buf, sizeof buf) == -1 && errno == ECONNRESET && read(sv[0], buf, sizeof buf) == 0);
    close(sv[0]);
}

static void nonblocking(void) {
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0, sv);
    check("SOCK_NONBLOCK and SOCK_CLOEXEC take", (fcntl(sv[0], F_GETFL) & O_NONBLOCK) && (fcntl(sv[0], F_GETFD) & FD_CLOEXEC));
    char buf[4096];
    check("an empty nonblocking read: EAGAIN", read(sv[0], buf, sizeof buf) == -1 && errno == EAGAIN);
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN | EPOLLRDHUP | EPOLLET, .data.u32 = 7};
    epoll_ctl(ep, EPOLL_CTL_ADD, sv[0], &ev);
    struct epoll_event out[4];
    check("epoll: nothing yet", epoll_wait(ep, out, 4, 0) == 0);
    write(sv[1], "x", 1);
    check("epoll: EPOLLIN after a write", epoll_wait(ep, out, 4, 1000) == 1 && out[0].events == EPOLLIN && out[0].data.u32 == 7);
    check("epoll (edge-triggered): nothing more", epoll_wait(ep, out, 4, 0) == 0);
    write(sv[1], "y", 1);
    check("epoll (edge-triggered): new data is a new edge", epoll_wait(ep, out, 4, 1000) == 1);

    memset(buf, 'q', sizeof buf);
    long total = 0;
    int n;
    while ((n = write(sv[1], buf, sizeof buf)) > 0) total += n;
    check("a full send buffer: EAGAIN", n == -1 && errno == EAGAIN && total > 0);
    check("and no POLLOUT", !(revents(sv[1], POLLOUT) & POLLOUT));
    long drained = 0;
    while ((n = read(sv[0], buf, sizeof buf)) > 0) drained += n;
    check("everything written arrives", drained == total + 2);
    check("POLLOUT once drained", revents(sv[1], POLLOUT) & POLLOUT);
    shutdown(sv[1], SHUT_WR);
    check("epoll: EPOLLRDHUP after the peer's shutdown", epoll_wait(ep, out, 4, 1000) == 1 && (out[0].events & EPOLLRDHUP));
    close(ep);
    close(sv[0]);
    close(sv[1]);

    struct timeval tv = {0, 50000};
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    setsockopt(sv[0], SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    check("SO_RCVTIMEO: a read times out with EAGAIN", read(sv[0], buf, 1) == -1 && errno == EAGAIN);
    close(sv[0]);
    close(sv[1]);
}

static void packets(void) {
    int sv[2];
    char buf[64];
    check("socketpair(AF_UNIX, SOCK_DGRAM)", socketpair(AF_UNIX, SOCK_DGRAM, 0, sv) == 0);
    send(sv[0], "one", 3, 0);
    send(sv[0], "", 0, 0);
    send(sv[0], "three", 5, 0);
    int avail = -1;
    check("datagram FIONREAD: the next datagram's size", ioctl(sv[1], FIONREAD, &avail) == 0 && avail == 3);
    check("datagrams keep their boundaries", recv(sv[1], buf, sizeof buf, 0) == 3);
    check("an empty datagram is one", recv(sv[1], buf, sizeof buf, 0) == 0);
    struct iovec iov = {buf, 2};
    struct msghdr msg = {0};
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    check("a short buffer truncates, MSG_TRUNC in msg_flags", recvmsg(sv[1], &msg, 0) == 2 && (msg.msg_flags & MSG_TRUNC));
    send(sv[0], "four", 4, 0);
    check("MSG_TRUNC returns the real length", recv(sv[1], buf, 1, MSG_TRUNC) == 4);
    close(sv[0]);
    close(sv[1]);

    check("socketpair(AF_UNIX, SOCK_SEQPACKET)", socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv) == 0);
    send(sv[0], "abc", 3, 0);
    send(sv[0], "defg", 4, 0);
    check("packets keep their boundaries", recv(sv[1], buf, sizeof buf, 0) == 3 && recv(sv[1], buf, 2, 0) == 2);
    check("the rest of a truncated packet is gone", send(sv[0], "h", 1, 0) == 1 && recv(sv[1], buf, sizeof buf, 0) == 1 && buf[0] == 'h');
    close(sv[0]);
    check("a packet socket reads end of file after the peer closed", recv(sv[1], buf, sizeof buf, 0) == 0);
    close(sv[1]);
}

/* A listener at `addr`; a child connects, writes its pid, and reads an
 * answer. */
static void server_at(const char *what, struct sockaddr_un *addr, socklen_t alen) {
    char name[128];
    int l = socket(AF_UNIX, SOCK_STREAM, 0);
    snprintf(name, sizeof name, "bind %s", what);
    check(name, bind(l, (struct sockaddr *)addr, alen) == 0);
    int other = socket(AF_UNIX, SOCK_STREAM, 0);
    snprintf(name, sizeof name, "a second bind of %s: EADDRINUSE", what);
    check(name, bind(other, (struct sockaddr *)addr, alen) == -1 && errno == EADDRINUSE);
    close(other);
    check("connecting before listen: ECONNREFUSED", ({
              int c = socket(AF_UNIX, SOCK_STREAM, 0);
              int r = connect(c, (struct sockaddr *)addr, alen);
              int e = errno;
              close(c);
              r == -1 && e == ECONNREFUSED;
          }));
    check("listen", listen(l, 4) == 0);
    int acc = 0;
    socklen_t ol = sizeof acc;
    check("SO_ACCEPTCONN of a listener", getsockopt(l, SOL_SOCKET, SO_ACCEPTCONN, &acc, &ol) == 0 && acc == 1);
    struct sockaddr_un got;
    socklen_t glen = sizeof got;
    check("getsockname gives the name back", getsockname(l, (struct sockaddr *)&got, &glen) == 0 && glen == alen && memcmp(&got, addr, alen) == 0);
    pid_t child = fork();
    if (child == 0) {
        int c = socket(AF_UNIX, SOCK_STREAM, 0);
        if (connect(c, (struct sockaddr *)addr, alen) != 0) _exit(1);
        struct sockaddr_un peer;
        socklen_t plen = sizeof peer;
        if (getpeername(c, (struct sockaddr *)&peer, &plen) != 0 || plen != alen || memcmp(&peer, addr, alen) != 0) _exit(2);
        struct ucred cred;
        socklen_t cl = sizeof cred;
        if (getsockopt(c, SOL_SOCKET, SO_PEERCRED, &cred, &cl) != 0 || cred.pid != getppid()) _exit(3);
        pid_t me = getpid();
        write(c, &me, sizeof me);
        char answer[3];
        if (read(c, answer, 3) != 3 || memcmp(answer, "ack", 3) != 0) _exit(4);
        _exit(0);
    }
    struct pollfd p = {l, POLLIN, 0};
    check("poll: a pending connection makes the listener readable", poll(&p, 1, 5000) == 1 && (p.revents & POLLIN));
    struct sockaddr_un from;
    socklen_t flen = sizeof from;
    int c = accept4(l, (struct sockaddr *)&from, &flen, SOCK_CLOEXEC);
    check("accept4: the client is unnamed", c >= 0 && flen == 2 && (fcntl(c, F_GETFD) & FD_CLOEXEC));
    glen = sizeof got;
    check("the accepted socket has the listener's name", getsockname(c, (struct sockaddr *)&got, &glen) == 0 && glen == alen);
    pid_t sent = 0;
    check("read from the client", read(c, &sent, sizeof sent) == sizeof sent && sent == child);
    struct ucred cred;
    socklen_t cl = sizeof cred;
    check("SO_PEERCRED of an accepted socket: the client", getsockopt(c, SOL_SOCKET, SO_PEERCRED, &cred, &cl) == 0 && cred.pid == child);
    write(c, "ack", 3);
    int status;
    waitpid(child, &status, 0);
    snprintf(name, sizeof name, "the client over %s did its part", what);
    check(name, WIFEXITED(status) && WEXITSTATUS(status) == 0);
    close(c);

    /* A backlog of 0 takes one connection; a nonblocking second one
     * gets EAGAIN. */
    listen(l, 0);
    int c1 = socket(AF_UNIX, SOCK_STREAM, 0), c2 = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
    check("a full backlog: a nonblocking connect gets EAGAIN",
          connect(c1, (struct sockaddr *)addr, alen) == 0 && connect(c2, (struct sockaddr *)addr, alen) == -1 && errno == EAGAIN);
    close(c2);
    close(l);
    char buf[4];
    check("the listener closed: a pending client is reset", read(c1, buf, sizeof buf) == -1 && errno == ECONNRESET);
    close(c1);
}

static void names(void) {
    struct sockaddr_un a;
    socklen_t alen = path_addr(&a, "/tmp/unixtest.sock");
    unlink(a.sun_path);
    server_at("a tmpfs path", &a, alen);
    struct stat st;
    check("the name is a socket inode", lstat("/tmp/unixtest.sock", &st) == 0 && S_ISSOCK(st.st_mode));
    check("open() of a socket inode: ENXIO", open("/tmp/unixtest.sock", O_RDONLY) == -1 && errno == ENXIO);
    check("the name stays after close; unlink removes it", unlink("/tmp/unixtest.sock") == 0);
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    check("connect to a missing path: ENOENT", connect(c, (struct sockaddr *)&a, alen) == -1 && errno == ENOENT);
    close(open("/tmp/unixtest.file", O_CREAT | O_WRONLY, 0644));
    alen = path_addr(&a, "/tmp/unixtest.file");
    check("connect to a file that is no socket: ECONNREFUSED", connect(c, (struct sockaddr *)&a, alen) == -1 && errno == ECONNREFUSED);
    unlink("/tmp/unixtest.file");
    close(c);

    /* A relative path, from the working directory. */
    chdir("/tmp");
    alen = path_addr(&a, "rel.sock");
    unlink("rel.sock");
    server_at("a relative path", &a, alen);
    unlink("rel.sock");
    chdir("/");

    alen = path_addr(&a, "/data/unixtest.sock");
    unlink(a.sun_path);
    server_at("a /data path", &a, alen);
    check("a /data socket inode", lstat("/data/unixtest.sock", &st) == 0 && S_ISSOCK(st.st_mode));
    unlink("/data/unixtest.sock");

    alen = abstract_addr(&a, "unixtest");
    server_at("an abstract name", &a, alen);

    int s = socket(AF_UNIX, SOCK_DGRAM, 0);
    struct sockaddr_un got;
    socklen_t glen;
    check("autobind (bind with the family only)", bind(s, (struct sockaddr *)&(struct sockaddr_un){.sun_family = AF_UNIX}, sizeof(sa_family_t)) == 0);
    glen = sizeof got;
    check("autobind gives a five-digit abstract name", getsockname(s, (struct sockaddr *)&got, &glen) == 0 && glen == 2 + 1 + 5 && got.sun_path[0] == 0);
    check("binding a bound socket: EINVAL", bind(s, (struct sockaddr *)&a, alen) == -1 && errno == EINVAL);
    close(s);
}

static void datagrams(void) {
    struct sockaddr_un a, b;
    socklen_t alen = path_addr(&a, "/tmp/unixtest.a"), blen = path_addr(&b, "/tmp/unixtest.b");
    unlink(a.sun_path);
    unlink(b.sun_path);
    int sa = socket(AF_UNIX, SOCK_DGRAM, 0), sb = socket(AF_UNIX, SOCK_DGRAM, 0);
    bind(sa, (struct sockaddr *)&a, alen);
    bind(sb, (struct sockaddr *)&b, blen);
    check("sendto a named datagram socket", sendto(sa, "ping", 4, 0, (struct sockaddr *)&b, blen) == 4);
    char buf[16];
    struct sockaddr_un from;
    socklen_t flen = sizeof from;
    check("recvfrom gives the sender's name", recvfrom(sb, buf, sizeof buf, 0, (struct sockaddr *)&from, &flen) == 4 && flen == alen &&
                                                    strcmp(from.sun_path, "/tmp/unixtest.a") == 0);
    check("connect a datagram socket", connect(sa, (struct sockaddr *)&b, blen) == 0 && send(sa, "x", 1, 0) == 1 && recv(sb, buf, sizeof buf, 0) == 1);
    int sc = socket(AF_UNIX, SOCK_DGRAM, 0);
    check("an unconnected datagram send: ENOTCONN", send(sc, "x", 1, 0) == -1 && errno == ENOTCONN);
    check("connect(AF_UNSPEC) disconnects", connect(sa, (struct sockaddr *)&(struct sockaddr){.sa_family = AF_UNSPEC}, sizeof(struct sockaddr)) == 0 &&
                                               send(sa, "x", 1, 0) == -1 && errno == ENOTCONN);
    int st = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un s;
    socklen_t slen = path_addr(&s, "/tmp/unixtest.s");
    unlink(s.sun_path);
    bind(st, (struct sockaddr *)&s, slen);
    listen(st, 1);
    check("a datagram to a stream socket: EPROTOTYPE", sendto(sc, "x", 1, 0, (struct sockaddr *)&s, slen) == -1 && errno == EPROTOTYPE);
    close(st);
    unlink(s.sun_path);
    close(sb);
    check("a datagram to a closed socket: ECONNREFUSED", sendto(sa, "x", 1, 0, (struct sockaddr *)&b, blen) == -1 && errno == ECONNREFUSED);
    close(sa);
    close(sc);
    unlink(a.sun_path);
    unlink(b.sun_path);
}

static void rights(void) {
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    int p[2];
    pipe(p);
    int file = open("/tmp/unixtest.data", O_CREAT | O_RDWR | O_TRUNC, 0644);
    write(file, "0123456789", 10);
    lseek(file, 0, SEEK_SET);
    pid_t child = fork();
    if (child == 0) {
        /* Only what is passed: the pipe's ends and the file fork gave go. */
        close(sv[0]);
        close(p[0]);
        close(p[1]);
        close(file);
        int fds[8];
        int flags;
        int n = recv_fds(sv[1], fds, 8, MSG_CMSG_CLOEXEC, &flags);
        if (n != 2) _exit(10 + (n < 0 ? 9 : n));
        if (!(fcntl(fds[0], F_GETFD) & FD_CLOEXEC)) _exit(2);
        /* The pipe's write end, passed: the parent reads what goes in. */
        if (write(fds[0], "via passed pipe", 15) != 15) _exit(3);
        /* The file: one open file description, its offset shared. */
        char two[2];
        if (read(fds[1], two, 2) != 2 || two[0] != '0') _exit(4);
        close(fds[0]);
        close(fds[1]);
        /* Too little control space: the descriptors are dropped. */
        n = recv_fds(sv[1], fds, 0, 0, &flags);
        if (n != 0 || !(flags & MSG_CTRUNC)) _exit(5);
        /* A socket passed back. */
        int pair[2];
        socketpair(AF_UNIX, SOCK_STREAM, 0, pair);
        if (send_fds(sv[1], &pair[1], 1) != 0) _exit(6);
        close(pair[1]);
        if (write(pair[0], "over the passed socket", 22) != 22) _exit(7);
        _exit(0);
    }
    close(sv[1]);
    int fds[2] = {p[1], file};
    check("SCM_RIGHTS: send a pipe and a file", send_fds(sv[0], fds, 2) == 0);
    /* Gone here before the child receives them: the message keeps them. */
    close(p[1]);
    close(file);
    send_fds(sv[0], (int[]){p[0]}, 1);
    char buf[64];
    int r = read(p[0], buf, sizeof buf);
    check("a passed descriptor outlives the sender's close", r == 15 && memcmp(buf, "via passed pipe", 15) == 0);
    check("the passed pipe's only writer went: end of file", read(p[0], buf, sizeof buf) == 0);
    int got[8];
    int n = recv_fds(sv[0], got, 8, 0, NULL);
    check("a socket passed back from the child", n == 1);
    check("the passed socket carries data", n == 1 && read(got[0], buf, sizeof buf) == 22);
    int status;
    waitpid(child, &status, 0);
    check("the child received and used the descriptors", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    int f = open("/tmp/unixtest.data", O_RDONLY);
    check("the file's data is intact", read(f, buf, 10) == 10 && memcmp(buf, "0123456789", 10) == 0);
    close(f);
    unlink("/tmp/unixtest.data");
    close(p[0]);
    if (n == 1) close(got[0]);
    close(sv[0]);

    /* MSG_PEEK installs copies; the message keeps its descriptors. */
    socketpair(AF_UNIX, SOCK_DGRAM, 0, sv);
    pipe(p);
    send_fds(sv[0], &p[1], 1);
    close(p[1]);
    int a[2];
    int k1 = recv_fds(sv[1], a, 1, MSG_PEEK, NULL);
    int k2 = recv_fds(sv[1], a + 1, 1, 0, NULL);
    check("MSG_PEEK passes a copy, the receive the original", k1 == 1 && k2 == 1 && a[0] != a[1]);
    write(a[0], "1", 1);
    write(a[1], "2", 1);
    close(a[0]);
    close(a[1]);
    check("both name the same pipe", read(p[0], buf, sizeof buf) == 2 && read(p[0], buf, sizeof buf) == 0);
    close(p[0]);
    close(sv[0]);
    close(sv[1]);

    check("SCM_RIGHTS with a bad descriptor: EBADF", ({
              socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
              int r2 = send_fds(sv[0], (int[]){999}, 1);
              close(sv[0]);
              close(sv[1]);
              r2 == -EBADF;
          }));

    /* A cycle: each socket of a pair in flight in the other's queue, a
     * pipe's write end in flight too; once the descriptors are closed,
     * only the collector can let the pipe end go. */
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    pipe(p);
    send_fds(sv[0], (int[]){sv[0], p[1]}, 2);
    send_fds(sv[1], &sv[1], 1);
    close(p[1]);
    close(sv[0]);
    close(sv[1]);
    struct pollfd pp = {p[0], POLLIN, 0};
    check("sockets in flight in a cycle are collected", poll(&pp, 1, 5000) == 1 && (pp.revents & POLLHUP) && read(p[0], buf, 1) == 0);
    close(p[0]);
}

static void credentials(void) {
    int sv[2];
    socketpair(AF_UNIX, SOCK_DGRAM, 0, sv);
    int on = 1;
    check("SO_PASSCRED", setsockopt(sv[1], SOL_SOCKET, SO_PASSCRED, &on, sizeof on) == 0);
    pid_t child = fork();
    if (child == 0) {
        send(sv[0], "c", 1, 0);
        _exit(0);
    }
    waitpid(child, NULL, 0);
    char byte;
    struct iovec iov = {&byte, 1};
    char control[CMSG_SPACE(sizeof(struct ucred))];
    struct msghdr msg = {0};
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control;
    msg.msg_controllen = sizeof control;
    int r = recvmsg(sv[1], &msg, 0);
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    struct ucred cred = {0};
    if (c && c->cmsg_type == SCM_CREDENTIALS) memcpy(&cred, CMSG_DATA(c), sizeof cred);
    check("SCM_CREDENTIALS names the sending process", r == 1 && c && c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_CREDENTIALS && cred.pid == child &&
                                                          cred.uid == getuid());
    /* Sent explicitly: our own credentials. */
    struct ucred mine = {getpid(), getuid(), getgid()};
    char out[CMSG_SPACE(sizeof mine)];
    memset(out, 0, sizeof out);
    struct msghdr sm = {0};
    sm.msg_iov = &iov;
    sm.msg_iovlen = 1;
    sm.msg_control = out;
    sm.msg_controllen = sizeof out;
    c = CMSG_FIRSTHDR(&sm);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_CREDENTIALS;
    c->cmsg_len = CMSG_LEN(sizeof mine);
    memcpy(CMSG_DATA(c), &mine, sizeof mine);
    check("SCM_CREDENTIALS sent explicitly", sendmsg(sv[0], &sm, 0) == 1);
    msg.msg_controllen = sizeof control;
    r = recvmsg(sv[1], &msg, 0);
    c = CMSG_FIRSTHDR(&msg);
    if (c) memcpy(&cred, CMSG_DATA(c), sizeof cred);
    check("and received", r == 1 && c && cred.pid == getpid());
    close(sv[0]);
    close(sv[1]);
}

/* A thread blocked in a receive or an accept on a socket another thread
 * closes the descriptor of: the call keeps the socket (Linux's fdget) and
 * still gets what comes. */
struct blocked {
    int fd;
    int got[8];
    int n;
    int err;
    int result;
};

static void *recv_thread(void *arg) {
    struct blocked *b = arg;
    b->n = recv_fds(b->fd, b->got, 8, 0, NULL);
    b->err = errno;
    return NULL;
}

static void *accept_thread(void *arg) {
    struct blocked *b = arg;
    b->result = accept(b->fd, NULL, NULL);
    return NULL;
}

static void closing_under_calls(void) {
    /* A failure here is EPIPE, to be reported, not a death. */
    signal(SIGPIPE, SIG_IGN);
    char buf[16];
    int sv[2], p[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    pipe(p);
    struct blocked b = {.fd = sv[1]};
    pthread_t t;
    pthread_create(&t, NULL, recv_thread, &b);
    usleep(50000);
    close(sv[1]);
    usleep(20000);
    send_fds(sv[0], &p[1], 1);
    close(p[1]);
    pthread_join(t, NULL);
    check("a blocked recvmsg outlives another thread's close", b.n == 1);
    check("... and the descriptor it got works", b.n == 1 && write(b.got[0], "ok", 2) == 2 && read(p[0], buf, sizeof buf) == 2);
    if (b.n == 1) close(b.got[0]);
    close(p[0]);
    close(sv[0]);

    struct sockaddr_un a;
    socklen_t alen = abstract_addr(&a, "unixtest-accept");
    int l = socket(AF_UNIX, SOCK_STREAM, 0);
    bind(l, (struct sockaddr *)&a, alen);
    listen(l, 4);
    struct blocked acc = {.fd = l, .result = -2};
    pthread_create(&t, NULL, accept_thread, &acc);
    usleep(50000);
    close(l);
    usleep(20000);
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    int r = connect(c, (struct sockaddr *)&a, alen);
    pthread_join(t, NULL);
    check("a blocked accept outlives another thread's close", r == 0 && acc.result >= 0);
    if (acc.result >= 0) close(acc.result);
    close(c);

    /* Many rounds of the race between a close and a receive that takes a
     * socket out of flight, with the collector running meanwhile (cycles
     * made and dropped by another thread). */
    int lost = 0;
    /* The receiver's descriptor is replaced by /dev/null (a close that
     * leaves no number for a new socket to take, which the receiving
     * thread could then look up instead). */
    int devnull = open("/dev/null", O_RDONLY);
    for (int round = 0; round < 200; round++) {
        int x[2], y[2];
        socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
        socketpair(AF_UNIX, SOCK_STREAM, 0, y);
        /* y[0] only in flight, in sv[1]'s queue. */
        send_fds(sv[0], &y[0], 1);
        close(y[0]);
        struct blocked rb = {.fd = sv[1]};
        pthread_create(&t, NULL, recv_thread, &rb);
        usleep(round % 7 * 100);
        dup2(devnull, sv[1]);
        /* A cycle dropped: the collector runs. */
        socketpair(AF_UNIX, SOCK_STREAM, 0, x);
        send_fds(x[0], &x[1], 1);
        send_fds(x[1], &x[0], 1);
        close(x[0]);
        close(x[1]);
        pthread_join(t, NULL);
        /* Replaced before the call looked the descriptor up: ENOTSOCK (as
         * on Linux); a call that has it must get the descriptor, alive. */
        if (rb.n == 1 ? write(y[1], "z", 1) != 1 || read(rb.got[0], buf, 1) != 1 : rb.err != ENOTSOCK) lost++;
        close(sv[1]);
        if (rb.n == 1) close(rb.got[0]);
        close(y[1]);
        close(sv[0]);
    }
    close(devnull);
    check("200 races of close, receive and the collector lose nothing", lost == 0);
    signal(SIGPIPE, SIG_DFL);
}

/* Sockets in flight in a cycle that an exiting process leaves behind are
 * collected (its descriptors go with the exit, no close), and descriptors
 * one process keeps in flight never stop another from passing its own. */
static void exit_leaves_cycles(void) {
    int p[2];
    pipe(p);
    pid_t child = fork();
    if (child == 0) {
        int s[2];
        socketpair(AF_UNIX, SOCK_STREAM, 0, s);
        send_fds(s[0], (int[]){s[0], p[1]}, 2);
        send_fds(s[1], &s[1], 1);
        /* Many more in flight, in the same cycle. */
        int many[253];
        for (int i = 0; i < 253; i++) many[i] = s[0];
        for (int m = 0; m < 63; m++) {
            char byte = 'F';
            struct iovec iov = {&byte, 1};
            char control[CMSG_SPACE(sizeof many)];
            struct msghdr msg = {0};
            msg.msg_iov = &iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control;
            msg.msg_controllen = sizeof control;
            struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
            c->cmsg_level = SOL_SOCKET;
            c->cmsg_type = SCM_RIGHTS;
            c->cmsg_len = CMSG_LEN(sizeof many);
            memcpy(CMSG_DATA(c), many, sizeof many);
            if (sendmsg(s[1], &msg, 0) != 1) _exit(1);
        }
        _exit(0);
    }
    close(p[1]);
    int status;
    waitpid(child, &status, 0);
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    int ok = 1;
    int many[253];
    for (int i = 0; i < 253; i++) many[i] = sv[0];
    for (int m = 0; m < 8 && ok; m++) {
        char byte = 'F';
        struct iovec iov = {&byte, 1};
        char control[CMSG_SPACE(sizeof many)];
        struct msghdr msg = {0};
        msg.msg_iov = &iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control;
        msg.msg_controllen = sizeof control;
        struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
        c->cmsg_level = SOL_SOCKET;
        c->cmsg_type = SCM_RIGHTS;
        c->cmsg_len = CMSG_LEN(sizeof many);
        memcpy(CMSG_DATA(c), many, sizeof many);
        ok = sendmsg(sv[0], &msg, 0) == 1;
    }
    /* Before any close of ours: nothing but the exit triggers collection. */
    check("another process passes descriptors after it", ok);
    struct pollfd pp = {p[0], POLLIN, 0};
    char buf[1];
    check("a cycle an exited process left is collected", WIFEXITED(status) && WEXITSTATUS(status) == 0 && poll(&pp, 1, 5000) == 1 &&
                                                            (pp.revents & POLLHUP) && read(p[0], buf, 1) == 0);
    close(p[0]);
    close(sv[0]);
    close(sv[1]);
}

/* A datagram sender held back by a receiver others filled is not writable
 * for level-triggered epoll (no busy loop on EAGAIN), and is again once
 * the receiver reads. */
static void dgram_pollout(void) {
    struct sockaddr_un r;
    socklen_t rlen = abstract_addr(&r, "unixtest-full");
    int rs = socket(AF_UNIX, SOCK_DGRAM, 0);
    bind(rs, (struct sockaddr *)&r, rlen);
    int a = socket(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK, 0), b = socket(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK, 0);
    connect(a, (struct sockaddr *)&r, rlen);
    int filled = 0;
    while (sendto(b, "x", 1, 0, (struct sockaddr *)&r, rlen) == 1) filled++;
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLOUT}, out[2];
    epoll_ctl(ep, EPOLL_CTL_ADD, a, &ev);
    int r1 = send(a, "y", 1, 0);
    int e1 = errno;
    check("a full datagram receiver holds a sender back: EAGAIN", filled > 0 && r1 == -1 && e1 == EAGAIN);
    check("... and the sender is not writable for epoll", epoll_wait(ep, out, 2, 0) == 0);
    char buf[4];
    recv(rs, buf, sizeof buf, 0);
    check("... until the receiver reads", epoll_wait(ep, out, 2, 1000) == 1 && (out[0].events & EPOLLOUT));
    close(ep);
    close(a);
    close(b);
    close(rs);
}

/* MSG_PEEK installs only what the control buffer has room for (MSG_CTRUNC
 * for the rest); credentials naming a process of another tree are
 * refused; a control buffer that cannot be written loses the ancillary
 * data, not the bytes. */
static void ancillary_edges(void) {
    int sv[2], p[2], q[2];
    socketpair(AF_UNIX, SOCK_DGRAM, 0, sv);
    pipe(p);
    pipe(q);
    send_fds(sv[0], (int[]){p[1], q[1], p[1]}, 3);
    int lowest = dup(0);
    close(lowest);
    int got[8], flags = 0;
    /* CMSG_SPACE of one int has room for two. */
    int n = recv_fds(sv[1], got, 1, MSG_PEEK, &flags);
    int next = dup(0);
    close(next);
    check("MSG_PEEK installs only what fits, with MSG_CTRUNC", n == 2 && (flags & MSG_CTRUNC) && next == lowest + 2);
    for (int i = 0; i < n; i++) close(got[i]);
    n = recv_fds(sv[1], got, 3, 0, NULL);
    if (n > 0) {
        for (int i = 0; i < n; i++) close(got[i]);
    }
    close(p[0]);
    close(p[1]);
    close(q[0]);
    close(q[1]);

    /* A pid the tree's namespace never handed out (its pid 1 is its own init). */
    struct ucred other = {32767, 0, 0};
    char byte = 'c';
    struct iovec iov = {&byte, 1};
    char out[CMSG_SPACE(sizeof other)];
    memset(out, 0, sizeof out);
    struct msghdr sm = {0};
    sm.msg_iov = &iov;
    sm.msg_iovlen = 1;
    sm.msg_control = out;
    sm.msg_controllen = sizeof out;
    struct cmsghdr *c = CMSG_FIRSTHDR(&sm);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_CREDENTIALS;
    c->cmsg_len = CMSG_LEN(sizeof other);
    memcpy(CMSG_DATA(c), &other, sizeof other);
    check("SCM_CREDENTIALS of a process outside the tree: ESRCH", sendmsg(sv[0], &sm, 0) == -1 && errno == ESRCH);

    int on = 1;
    setsockopt(sv[1], SOL_SOCKET, SO_PASSCRED, &on, sizeof on);
    send(sv[0], "d", 1, 0);
    struct msghdr rm = {0};
    rm.msg_iov = &iov;
    rm.msg_iovlen = 1;
    rm.msg_control = (void *)8;
    rm.msg_controllen = 64;
    check("an unwritable control buffer: the bytes still come", recvmsg(sv[1], &rm, 0) == 1 && byte == 'd');
    close(sv[0]);
    close(sv[1]);
}

/* Sends `n` (up to 253) copies of descriptor `fd` with one byte. */
static int send_copies(int sock, int fd, int n) {
    int fds[253];
    for (int i = 0; i < n; i++) fds[i] = fd;
    char byte = 'F';
    struct iovec iov = {&byte, 1};
    char control[CMSG_SPACE(sizeof fds)];
    struct msghdr msg = {0};
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control;
    msg.msg_controllen = CMSG_SPACE(sizeof(int) * n);
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int) * n);
    memcpy(CMSG_DATA(c), fds, sizeof(int) * n);
    return sendmsg(sock, &msg, 0) == 1 ? 0 : -errno;
}

static volatile int stop_cycles;

static void *cycles_thread(void *arg) {
    (void)arg;
    while (!stop_cycles) {
        int x[2];
        socketpair(AF_UNIX, SOCK_STREAM, 0, x);
        send_fds(x[0], &x[1], 1);
        send_fds(x[1], &x[0], 1);
        close(x[0]);
        close(x[1]);
    }
    return NULL;
}

/* Receives into pages of a /data file's mapping that are not in memory
 * yet (the pager brings each), while sockets close under the receiver
 * (the pager shuts their peers down) and the collector runs: no server
 * lock may be held across such a copy. */
static void paging_under_locks(void) {
    const size_t pages = 1024, len = pages * 4096;
    int f = open("/data/unixtest.map", O_CREAT | O_RDWR | O_TRUNC, 0644);
    int ok = f >= 0 && ftruncate(f, len) == 0;
    /* Private: each page is read in by the pager, then copied; nothing is
     * left to write back. */
    char *map = ok ? mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE, f, 0) : MAP_FAILED;
    ok = ok && map != MAP_FAILED;
    pthread_t g;
    stop_cycles = 0;
    pthread_create(&g, NULL, cycles_thread, NULL);
    static char msg[4096];
    memset(msg, 'm', sizeof msg);
    size_t page = 0;
    for (int round = 0; round < 300 && ok; round++) {
        /* A datagram with a descriptor, peeked and then received. */
        int d[2], p[2];
        socketpair(AF_UNIX, SOCK_DGRAM, 0, d);
        pipe(p);
        send_fds(d[0], &p[1], 1);
        char control[CMSG_SPACE(sizeof(int))];
        for (int peek = 1; peek >= 0; peek--) {
            struct iovec iov = {map + page++ * 4096, 1};
            struct msghdr m = {0};
            m.msg_iov = &iov;
            m.msg_iovlen = 1;
            m.msg_control = control;
            m.msg_controllen = sizeof control;
            ok &= recvmsg(d[1], &m, peek ? MSG_PEEK : 0) == 1;
            struct cmsghdr *c = CMSG_FIRSTHDR(&m);
            if (c) {
                int got;
                memcpy(&got, CMSG_DATA(c), sizeof got);
                close(got);
            }
        }
        close(d[0]);
        close(d[1]);
        close(p[0]);
        close(p[1]);
        /* A stream whose writer (another process) goes while the
         * receiver copies. */
        int s[2];
        socketpair(AF_UNIX, SOCK_STREAM, 0, s);
        pid_t child = fork();
        if (child == 0) {
            close(s[1]);
            write(s[0], msg, sizeof msg);
            usleep(round % 5 * 50);
            _exit(0);
        }
        close(s[0]);
        char *at = map + page++ * 4096;
        ssize_t n, total = 0;
        while ((n = read(s[1], at + total, 4096 - total)) > 0) total += n;
        ok &= total == 4096 && at[0] == 'm' && at[4095] == 'm';
        close(s[1]);
        waitpid(child, NULL, 0);
    }
    stop_cycles = 1;
    pthread_join(g, NULL);
    if (map != MAP_FAILED) munmap(map, len);
    if (f >= 0) close(f);
    unlink("/data/unixtest.map");
    check("copies into pages the pager brings, sockets closing, collecting", ok);
}

/* Descriptors in flight cannot take the instance's handle table, however
 * many processes try: five each put as many in flight as they may and
 * keep them; together they stay within the instance's bound, and the
 * parent can still map files (which needs handles). */
static void inflight_bound(void) {
    int report[2], go[2];
    pipe(report);
    pipe(go);
    pid_t kids[5];
    for (int k = 0; k < 5; k++) {
        kids[k] = fork();
        if (kids[k] == 0) {
            close(report[0]);
            close(go[1]);
            int s[2];
            socketpair(AF_UNIX, SOCK_STREAM, 0, s);
            int sent = 0;
            for (int m = 0; m < 80 && send_copies(s[1], s[0], 253) == 0; m++) sent += 253;
            write(report[1], &sent, sizeof sent);
            char c;
            read(go[0], &c, 1);
            _exit(0);
        }
    }
    close(report[1]);
    close(go[0]);
    long total = 0;
    int sent;
    for (int k = 0; k < 5; k++)
        if (read(report[0], &sent, sizeof sent) == sizeof sent) total += sent;
    int f = open("/tmp/unixtest.bound", O_CREAT | O_RDWR | O_TRUNC, 0644);
    ftruncate(f, 4096);
    char *m = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, f, 0);
    int mapped = m != MAP_FAILED;
    if (mapped) {
        m[0] = 'b';
        munmap(m, 4096);
    }
    close(f);
    unlink("/tmp/unixtest.bound");
    close(go[1]);
    for (int k = 0; k < 5; k++) waitpid(kids[k], NULL, 0);
    close(report[0]);
    check("descriptors in flight stay within the instance's bound", total > 0 && total <= 16 * 1024);
    check("... and files can still be mapped meanwhile", mapped);
}

/* Leaves nothing behind for the tests after this one: a last cycle with a
 * pipe's write end, dropped; the collector takes cycles in the order their
 * sockets were made, so once that pipe's reader sees the end, the garbage
 * the tests above left is gone too. */
static void quiesce(void) {
    int s[2], p[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, s);
    pipe(p);
    send_fds(s[0], (int[]){s[0], p[1]}, 2);
    send_fds(s[1], &s[1], 1);
    close(p[1]);
    close(s[0]);
    close(s[1]);
    struct pollfd pp = {p[0], POLLIN, 0};
    char c;
    check("everything left in flight is collected", poll(&pp, 1, 10000) == 1 && read(p[0], &c, 1) == 0);
    close(p[0]);
}

int main(void) {
    stream_pair();
    nonblocking();
    packets();
    names();
    datagrams();
    rights();
    credentials();
    closing_under_calls();
    exit_leaves_cycles();
    dgram_pollout();
    ancillary_edges();
    paging_under_locks();
    inflight_bound();
    quiesce();
    printf("%s\n", failures ? "unixtest: FAILURES" : "unixtest: all ok");
    return failures ? 1 : 0;
}
