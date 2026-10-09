/* Socket tests: TCP and UDP over loopback and through QEMU's user network
 * (10.0.2.100:7 is an echo service, see builder/src/main.rs), with the
 * Linux semantics of the server's internet sockets (R7b): bulk data
 * intact, MSG_PEEK, MSG_DONTWAIT, MSG_WAITALL, MSG_TRUNC, timeouts,
 * half-close, EPIPE and SIGPIPE, SO_ERROR, SO_REUSEADDR, options, epoll's
 * edges. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/sockios.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-52s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static struct sockaddr_in addr(const char *ip, int port) {
    struct sockaddr_in a = {0};
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    inet_pton(AF_INET, ip, &a.sin_addr);
    return a;
}

static int tcp_connect(const char *ip, int port) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = addr(ip, port);
    if (connect(fd, (struct sockaddr *)&a, sizeof a) < 0) {
        int e = errno;
        close(fd);
        errno = e;
        return -1;
    }
    return fd;
}

/* Reads until `len` bytes arrived or the peer closed. */
static int read_all(int fd, char *buf, int len) {
    int got = 0;
    while (got < len) {
        int n = read(fd, buf + got, len - got);
        if (n <= 0) break;
        got += n;
    }
    return got;
}

static void on_alarm(int sig) { (void)sig; }

static volatile int sigpipes;
static void on_sigpipe(int sig) {
    (void)sig;
    sigpipes++;
}

static long now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000 + t.tv_nsec / 1000000;
}

/* A listener on a free loopback port (SO_REUSEADDR if `reuse`). */
static int listener(int *port, int reuse) {
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = addr("127.0.0.1", *port);
    setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &reuse, sizeof reuse);
    if (bind(l, (struct sockaddr *)&a, sizeof a) < 0 || listen(l, 4) < 0) {
        close(l);
        return -1;
    }
    socklen_t len = sizeof a;
    getsockname(l, (struct sockaddr *)&a, &len);
    *port = ntohs(a.sin_port);
    return l;
}

/* A connected TCP pair over loopback: fds[0] connected, fds[1] accepted. */
static int tcp_pair(int fds[2]) {
    int port = 0, l = listener(&port, 0);
    if (l < 0) return -1;
    fds[0] = tcp_connect("127.0.0.1", port);
    fds[1] = fds[0] >= 0 ? accept(l, NULL, NULL) : -1;
    close(l);
    return fds[0] >= 0 && fds[1] >= 0 ? 0 : -1;
}

/* Byte `i` of the bulk transfer's pattern. */
static unsigned char pattern(unsigned long i) { return (unsigned char)(i * 131 + (i >> 12)); }

/* Linux's semantics of the server's TCP sockets (R7b). */
static void stream_semantics(void) {
    int p[2];
    char buf[256];

    /* 8 MiB in pieces of many sizes through a loopback connection, the
     * reader in another process checking every byte. */
    check("a TCP pair over loopback", tcp_pair(p) == 0);
    pid_t reader = fork();
    if (reader == 0) {
        close(p[0]);
        static unsigned char in[70000];
        unsigned long got = 0, total = 8UL << 20;
        while (got < total) {
            int n = read(p[1], in, 1 + (got % sizeof in));
            if (n <= 0) _exit(1);
            for (int k = 0; k < n; k++)
                if (in[k] != pattern(got + k)) _exit(2);
            got += n;
        }
        _exit(read(p[1], in, 1) == 0 ? 0 : 3);
    }
    close(p[1]);
    static unsigned char out[100000];
    unsigned long sent = 0, total = 8UL << 20;
    int ok = 1;
    while (sent < total && ok) {
        unsigned long n = 1 + (sent * 7919) % sizeof out;
        if (n > total - sent) n = total - sent;
        for (unsigned long k = 0; k < n; k++) out[k] = pattern(sent + k);
        ok = write(p[0], out, n) == (long)n;
        sent += n;
    }
    close(p[0]);
    int st = -1;
    waitpid(reader, &st, 0);
    check("8 MiB arrive intact, in order, then end of file", ok && WIFEXITED(st) && WEXITSTATUS(st) == 0);

    /* MSG_PEEK, MSG_DONTWAIT, FIONREAD, MSG_WAITALL. */
    tcp_pair(p);
    check("an empty connection: MSG_DONTWAIT is EAGAIN", recv(p[1], buf, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN);
    write(p[0], "peekaboo", 8);
    struct pollfd pf = {p[1], POLLIN, 0};
    poll(&pf, 1, 2000);
    int avail = -1;
    ioctl(p[1], FIONREAD, &avail);
    check("FIONREAD counts the bytes waiting", avail == 8);
    check("MSG_PEEK leaves the data", recv(p[1], buf, 4, MSG_PEEK) == 4 && memcmp(buf, "peek", 4) == 0);
    check("... for the next read", recv(p[1], buf, 8, MSG_WAITALL) == 8 && memcmp(buf, "peekaboo", 8) == 0);
    pid_t writer = fork();
    if (writer == 0) {
        write(p[0], "abc", 3);
        usleep(100000);
        write(p[0], "def", 3);
        _exit(0);
    }
    check("MSG_WAITALL waits for all of it", recv(p[1], buf, 6, MSG_WAITALL) == 6 && memcmp(buf, "abcdef", 6) == 0);
    waitpid(writer, NULL, 0);

    /* Timeouts. */
    struct timeval tv = {0, 150000};
    setsockopt(p[1], SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    long t0 = now_ms();
    int r = recv(p[1], buf, 1, 0);
    long waited = now_ms() - t0;
    check("SO_RCVTIMEO: a receive gives up with EAGAIN", r == -1 && errno == EAGAIN && waited >= 140 && waited < 2000);
    struct timeval got_tv;
    socklen_t tl = sizeof got_tv;
    check("... and getsockopt reads it back", getsockopt(p[1], SOL_SOCKET, SO_RCVTIMEO, &got_tv, &tl) == 0 && got_tv.tv_usec == 150000);
    tv.tv_usec = 200000;
    setsockopt(p[0], SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof tv);
    static char big[8 << 20];
    long n = write(p[0], big, sizeof big);
    check("SO_SNDTIMEO: a send to a reader that never reads stops with what went", n > 0 && n < (long)sizeof big);
    t0 = now_ms();
    n = write(p[0], big, sizeof big);
    waited = now_ms() - t0;
    check("... then with EAGAIN after the time", n == -1 && errno == EAGAIN && waited >= 190);
    int out_q = -1;
    ioctl(p[0], SIOCOUTQ, &out_q);
    check("SIOCOUTQ counts what waits to go", out_q > 0);
    close(p[0]);
    close(p[1]);

    /* Half-close: SHUT_WR sends end of file, the other way stays open. */
    tcp_pair(p);
    write(p[0], "last", 4);
    check("shutdown(SHUT_WR)", shutdown(p[0], SHUT_WR) == 0);
    check("the peer reads the data, then end of file", recv(p[1], buf, 4, MSG_WAITALL) == 4 && read(p[1], buf, 1) == 0);
    pf = (struct pollfd){p[1], POLLIN | POLLRDHUP, 0};
    check("... and polls POLLRDHUP", poll(&pf, 1, 1000) == 1 && (pf.revents & POLLRDHUP));
    check("the other way still carries data", write(p[1], "back", 4) == 4 && recv(p[0], buf, 4, MSG_WAITALL) == 4 && memcmp(buf, "back", 4) == 0);
    struct sigaction sa = {0};
    sa.sa_handler = on_sigpipe;
    sigaction(SIGPIPE, &sa, NULL);
    sigpipes = 0;
    check("writing after SHUT_WR is EPIPE with SIGPIPE", write(p[0], "x", 1) == -1 && errno == EPIPE && sigpipes == 1);
    close(p[0]);
    close(p[1]);

    /* A peer that closed: its stack resets what comes, the writer gets
     * EPIPE (an ECONNRESET first, at most), SIGPIPE unless MSG_NOSIGNAL. */
    tcp_pair(p);
    close(p[1]);
    usleep(50000);
    sigpipes = 0;
    int epipe = 0, other = 0;
    for (int i = 0; i < 20 && !epipe; i++) {
        if (write(p[0], "x", 1) == -1) {
            if (errno == EPIPE) epipe = 1;
            else if (errno != ECONNRESET) other = 1;
        }
        usleep(20000);
    }
    check("writing to a closed peer ends in EPIPE", epipe && !other);
    check("... with SIGPIPE", sigpipes == 1);
    check("MSG_NOSIGNAL: EPIPE without the signal", send(p[0], "x", 1, MSG_NOSIGNAL) == -1 && errno == EPIPE && sigpipes == 1);
    pf = (struct pollfd){p[0], POLLOUT, 0};
    check("... and the socket polls POLLHUP", poll(&pf, 1, 0) == 1 && (pf.revents & POLLHUP));
    close(p[0]);
    signal(SIGPIPE, SIG_DFL);

    /* A refused nonblocking connect: the error is SO_ERROR's, once. */
    int fd = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    struct sockaddr_in a = addr("127.0.0.1", 1);
    r = connect(fd, (struct sockaddr *)&a, sizeof a);
    check("nonblocking connect to a closed port: EINPROGRESS", r == -1 && (errno == EINPROGRESS || errno == ECONNREFUSED));
    pf = (struct pollfd){fd, POLLOUT, 0};
    poll(&pf, 1, 2000);
    int err = -1;
    socklen_t el = sizeof err;
    getsockopt(fd, SOL_SOCKET, SO_ERROR, &err, &el);
    check("... SO_ERROR is ECONNREFUSED", err == ECONNREFUSED && (pf.revents & POLLERR));
    getsockopt(fd, SOL_SOCKET, SO_ERROR, &err, &el);
    check("... and 0 when read again", err == 0);
    close(fd);

    /* Options. */
    fd = socket(AF_INET, SOCK_STREAM, 0);
    int v = -1;
    socklen_t vl = sizeof v;
    check("TCP_NODELAY is off by default (Nagle's algorithm)", getsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &v, &vl) == 0 && v == 0);
    int one = 1;
    check("TCP_NODELAY can be set and read back",
          setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one) == 0 && getsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &v, &vl) == 0 && v == 1);
    check("SO_KEEPALIVE can be set and read back",
          setsockopt(fd, SOL_SOCKET, SO_KEEPALIVE, &one, sizeof one) == 0 && getsockopt(fd, SOL_SOCKET, SO_KEEPALIVE, &v, &vl) == 0 && v == 1);
    check("SO_TYPE, SO_PROTOCOL, SO_DOMAIN",
          getsockopt(fd, SOL_SOCKET, SO_TYPE, &v, &vl) == 0 && v == SOCK_STREAM && getsockopt(fd, SOL_SOCKET, SO_PROTOCOL, &v, &vl) == 0 &&
              v == IPPROTO_TCP && getsockopt(fd, SOL_SOCKET, SO_DOMAIN, &v, &vl) == 0 && v == AF_INET);
    check("an unknown option is ENOPROTOOPT", setsockopt(fd, IPPROTO_TCP, 9999, &one, sizeof one) == -1 && errno == ENOPROTOOPT);
    struct sockaddr_in peer;
    socklen_t pl = sizeof peer;
    check("getpeername of an unconnected socket is ENOTCONN", getpeername(fd, (struct sockaddr *)&peer, &pl) == -1 && errno == ENOTCONN);
    check("recv on an unconnected socket is ENOTCONN", recv(fd, buf, 1, 0) == -1 && errno == ENOTCONN);
    close(fd);

    /* SO_REUSEADDR: an accepted connection keeps its port; without the
     * option nobody may bind it, with it a new listener may. */
    int port = 0, l = listener(&port, 1);
    int c = tcp_connect("127.0.0.1", port);
    int s = accept4(l, NULL, NULL, SOCK_NONBLOCK | SOCK_CLOEXEC);
    check("accept4 gives SOCK_NONBLOCK and SOCK_CLOEXEC", s >= 0 && (fcntl(s, F_GETFL) & O_NONBLOCK) && (fcntl(s, F_GETFD) & FD_CLOEXEC));
    close(l);
    int b1 = socket(AF_INET, SOCK_STREAM, 0);
    a = addr("127.0.0.1", port);
    check("a connection's port: bind without SO_REUSEADDR is EADDRINUSE", bind(b1, (struct sockaddr *)&a, sizeof a) == -1 && errno == EADDRINUSE);
    int b2 = socket(AF_INET, SOCK_STREAM, 0);
    setsockopt(b2, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    check("... with it, bind and listen work", bind(b2, (struct sockaddr *)&a, sizeof a) == 0 && listen(b2, 1) == 0);
    close(b1);
    close(b2);
    /* Two sockets sharing a port by SO_REUSEADDR: only one may listen (the
     * port is claimed again at listen, as Linux does). */
    port = 0;
    l = listener(&port, 1);
    close(l);
    int r1 = socket(AF_INET, SOCK_STREAM, 0), r2 = socket(AF_INET, SOCK_STREAM, 0);
    setsockopt(r1, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    setsockopt(r2, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    a = addr("127.0.0.1", 0);
    bind(r1, (struct sockaddr *)&a, sizeof a);
    socklen_t alen = sizeof a;
    getsockname(r1, (struct sockaddr *)&a, &alen);
    check("two SO_REUSEADDR sockets bind one port", bind(r2, (struct sockaddr *)&a, sizeof a) == 0);
    check("... the first listens, the second's listen is EADDRINUSE", listen(r1, 1) == 0 && listen(r2, 1) == -1 && errno == EADDRINUSE);
    close(r1);
    close(r2);
    /* SO_REUSEADDR counts as it is at listen, not as it was at bind. */
    r1 = socket(AF_INET, SOCK_STREAM, 0);
    r2 = socket(AF_INET, SOCK_STREAM, 0);
    setsockopt(r1, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    setsockopt(r2, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    a = addr("127.0.0.1", 0);
    bind(r1, (struct sockaddr *)&a, sizeof a);
    alen = sizeof a;
    getsockname(r1, (struct sockaddr *)&a, &alen);
    bind(r2, (struct sockaddr *)&a, sizeof a);
    int zero = 0;
    setsockopt(r1, SOL_SOCKET, SO_REUSEADDR, &zero, sizeof zero);
    check("SO_REUSEADDR cleared after bind: listen is EADDRINUSE", listen(r1, 1) == -1 && errno == EADDRINUSE);
    close(r1);
    close(r2);
    close(c);
    close(s);

    /* Edge-triggered epoll: an event for each arrival of data. */
    tcp_pair(p);
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN | EPOLLET, .data.fd = p[1]};
    epoll_ctl(ep, EPOLL_CTL_ADD, p[1], &ev);
    write(p[0], "1", 1);
    int first = epoll_wait(ep, &ev, 1, 2000);
    int again = epoll_wait(ep, &ev, 1, 100);
    write(p[0], "2", 1);
    int second = epoll_wait(ep, &ev, 1, 2000);
    check("EPOLLET: an edge per arrival, none without", first == 1 && again == 0 && second == 1);
    close(ep);

    /* A blocking accept is interrupted by a signal. */
    port = 0;
    l = listener(&port, 0);
    struct sigaction al = {0};
    al.sa_handler = on_alarm;
    sigaction(SIGALRM, &al, NULL);
    alarm(1);
    check("accept without a connection is interrupted with EINTR", accept(l, NULL, NULL) == -1 && errno == EINTR);
    close(l);
    close(p[0]);
    close(p[1]);
}

/* UDP: truncation, connected sockets. */
static void datagram_semantics(void) {
    char buf[128];
    int rx = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = addr("127.0.0.1", 0);
    bind(rx, (struct sockaddr *)&a, sizeof a);
    socklen_t len = sizeof a;
    getsockname(rx, (struct sockaddr *)&a, &len);
    int tx = socket(AF_INET, SOCK_DGRAM, 0);
    char hundred[100];
    memset(hundred, 'u', sizeof hundred);
    sendto(tx, hundred, 100, 0, (struct sockaddr *)&a, sizeof a);
    sendto(tx, hundred, 100, 0, (struct sockaddr *)&a, sizeof a);
    struct iovec iov = {buf, 10};
    struct msghdr m = {0};
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    int avail = -1;
    struct pollfd pf = {rx, POLLIN, 0};
    poll(&pf, 1, 2000);
    ioctl(rx, FIONREAD, &avail);
    check("UDP FIONREAD is the next datagram's length", avail == 100);
    long n = recvmsg(rx, &m, MSG_TRUNC);
    check("MSG_TRUNC returns the datagram's whole length", n == 100 && (m.msg_flags & MSG_TRUNC));
    m.msg_flags = 0;
    n = recvmsg(rx, &m, 0);
    check("... without it, what fit, flagged MSG_TRUNC", n == 10 && (m.msg_flags & MSG_TRUNC));
    check("connect a UDP socket", connect(tx, (struct sockaddr *)&a, sizeof a) == 0);
    struct sockaddr_in peer;
    socklen_t pl = sizeof peer;
    check("... getpeername tells the peer", getpeername(tx, (struct sockaddr *)&peer, &pl) == 0 && peer.sin_port == a.sin_port);
    check("... send needs no address", send(tx, "c", 1, 0) == 1 && recv(rx, buf, sizeof buf, 0) == 1 && buf[0] == 'c');
    struct sockaddr unspec = {.sa_family = AF_UNSPEC};
    check("AF_UNSPEC disconnects it", connect(tx, &unspec, sizeof unspec) == 0 && send(tx, "c", 1, 0) == -1 && errno == EDESTADDRREQ);
    check("socketpair of AF_INET is EOPNOTSUPP", socketpair(AF_INET, SOCK_STREAM, 0, (int[2]){0, 0}) == -1 && errno == EOPNOTSUPP);
    /* A UDP port: shared only by sockets that both allow reuse. */
    int u1 = socket(AF_INET, SOCK_DGRAM, 0), u2 = socket(AF_INET, SOCK_DGRAM, 0), u3 = socket(AF_INET, SOCK_DGRAM, 0);
    int one = 1;
    setsockopt(u1, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    setsockopt(u3, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in ua = addr("127.0.0.1", 0);
    bind(u1, (struct sockaddr *)&ua, sizeof ua);
    socklen_t ul = sizeof ua;
    getsockname(u1, (struct sockaddr *)&ua, &ul);
    check("a UDP port without SO_REUSEADDR: EADDRINUSE", bind(u2, (struct sockaddr *)&ua, sizeof ua) == -1 && errno == EADDRINUSE);
    check("... with it on both: bound", bind(u3, (struct sockaddr *)&ua, sizeof ua) == 0);
    close(u1);
    close(u3);
    check("a closed socket's port is free when close returns", bind(u2, (struct sockaddr *)&ua, sizeof ua) == 0);
    close(u2);
    close(rx);
    close(tx);
}

/* One ICMP echo request to `ip`; returns 1 if the matching reply arrives
 * within two seconds (as a whole IPv4 packet, like on Linux). */
static int icmp_echo(const char *ip) {
    int fd = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    if (fd < 0) return 0;
    unsigned char req[16] = {8, 0, 0, 0, 0x12, 0x34, 0, 1, 'o', 'x', 'i', 'd', 'e', 'n', 'i', 'x'};
    unsigned sum = 0;
    for (int i = 0; i < 16; i += 2) sum += req[i] << 8 | req[i + 1];
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    sum = ~sum & 0xffff;
    req[2] = sum >> 8;
    req[3] = sum & 0xff;
    struct sockaddr_in a = addr(ip, 0);
    if (sendto(fd, req, sizeof req, 0, (struct sockaddr *)&a, sizeof a) != sizeof req) {
        close(fd);
        return 0;
    }
    int found = 0;
    for (int tries = 0; tries < 8 && !found; tries++) {
        struct pollfd p = {fd, POLLIN, 0};
        if (poll(&p, 1, 2000) != 1) break;
        unsigned char reply[128];
        int n = recv(fd, reply, sizeof reply, 0);
        int ihl = (reply[0] & 0xf) * 4;
        /* Skip our own request when it comes back over loopback. */
        found = n >= ihl + 16 && reply[9] == 1 && reply[ihl] == 0 && reply[ihl + 4] == 0x12 && reply[ihl + 5] == 0x34;
    }
    close(fd);
    return found;
}

/* IP_TTL on a raw socket: the request it sends over loopback carries it. */
static int raw_ttl(void) {
    int fd = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    if (fd < 0) return 0;
    int ttl = 7;
    if (setsockopt(fd, IPPROTO_IP, IP_TTL, &ttl, sizeof ttl) != 0) {
        close(fd);
        return 0;
    }
    /* An echo request with a checksum over id 0x5678, sequence 1. */
    unsigned char req[8] = {8, 0, 0, 0, 0x56, 0x78, 0, 1};
    unsigned sum = 0;
    for (int i = 0; i < 8; i += 2) sum += req[i] << 8 | req[i + 1];
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    sum = ~sum & 0xffff;
    req[2] = sum >> 8;
    req[3] = sum & 0xff;
    struct sockaddr_in a = addr("127.0.0.1", 0);
    int found = 0;
    if (sendto(fd, req, sizeof req, 0, (struct sockaddr *)&a, sizeof a) == sizeof req) {
        for (int tries = 0; tries < 8 && !found; tries++) {
            struct pollfd p = {fd, POLLIN, 0};
            if (poll(&p, 1, 2000) != 1) break;
            unsigned char packet[128];
            int n = recv(fd, packet, sizeof packet, 0);
            int ihl = (packet[0] & 0xf) * 4;
            found = n >= ihl + 8 && packet[ihl] == 8 && packet[ihl + 4] == 0x56 && packet[ihl + 5] == 0x78 && packet[8] == 7;
        }
    }
    close(fd);
    return found;
}

int main(void) {
    char buf[256];

    /* TCP through the user network to the host's echo service. */
    int fd = tcp_connect("10.0.2.100", 7);
    check("connect to the echo service", fd >= 0);
    if (fd >= 0) {
        write(fd, "ping over tcp", 13);
        int n = read_all(fd, buf, 13);
        check("echo service returns the data", n == 13 && memcmp(buf, "ping over tcp", 13) == 0);
        struct sockaddr_in me;
        socklen_t len = sizeof me;
        getsockname(fd, (struct sockaddr *)&me, &len);
        check("getsockname reports the DHCP address", me.sin_addr.s_addr == inet_addr("10.0.2.15") && ntohs(me.sin_port) >= 1024);
        close(fd);
    }

    /* Nothing listens on loopback port 1. */
    fd = tcp_connect("127.0.0.1", 1);
    check("connect to a closed port fails with ECONNREFUSED", fd < 0 && errno == ECONNREFUSED);

    /* listen/accept over loopback with a forked client. */
    int srv = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = addr("127.0.0.1", 8080);
    int one = 1;
    setsockopt(srv, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    check("bind and listen on 127.0.0.1:8080",
          bind(srv, (struct sockaddr *)&a, sizeof a) == 0 && listen(srv, 4) == 0);
    pid_t client = fork();
    if (client == 0) {
        int c = tcp_connect("127.0.0.1", 8080);
        if (c < 0) _exit(1);
        write(c, "hello server", 12);
        char reply[16];
        int n = read_all(c, reply, 12);
        _exit(n == 12 && memcmp(reply, "hello client", 12) == 0 ? 0 : 2);
    }
    struct sockaddr_in peer;
    socklen_t plen = sizeof peer;
    int conn = accept(srv, (struct sockaddr *)&peer, &plen);
    check("accept returns the connection", conn >= 0);
    check("accept reports the peer address", conn >= 0 && peer.sin_addr.s_addr == inet_addr("127.0.0.1"));
    int n = conn >= 0 ? read_all(conn, buf, 12) : -1;
    check("server receives the client's data", n == 12 && memcmp(buf, "hello server", 12) == 0);
    if (conn >= 0) write(conn, "hello client", 12);
    int status = -1;
    waitpid(client, &status, 0);
    check("client receives the server's reply", WIFEXITED(status) && WEXITSTATUS(status) == 0);
    if (conn >= 0) {
        n = read(conn, buf, sizeof buf);
        check("read returns 0 after the peer closed", n == 0);
        close(conn);
    }

    /* A non-blocking accept with nothing pending. */
    fcntl(srv, F_SETFL, O_NONBLOCK);
    check("non-blocking accept fails with EAGAIN", accept(srv, NULL, NULL) < 0 && errno == EAGAIN);

    /* The listener's port is in use, also for the wildcard address and
     * with SO_REUSEADDR; bind(0) takes a port at once. */
    int second = socket(AF_INET, SOCK_STREAM, 0);
    setsockopt(second, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    errno = 0;
    check("bind to a listener's port fails with EADDRINUSE", bind(second, (struct sockaddr *)&a, sizeof a) < 0 && errno == EADDRINUSE);
    struct sockaddr_in any = addr("0.0.0.0", 8080);
    errno = 0;
    check("... also on the wildcard address", bind(second, (struct sockaddr *)&any, sizeof any) < 0 && errno == EADDRINUSE);
    struct sockaddr_in zero = addr("127.0.0.1", 0), got;
    socklen_t glen = sizeof got;
    check("bind to port 0 takes a free port (getsockname)", bind(second, (struct sockaddr *)&zero, sizeof zero) == 0 &&
                                                                getsockname(second, (struct sockaddr *)&got, &glen) == 0 && ntohs(got.sin_port) != 0);
    errno = 0;
    check("binding a bound socket again fails with EINVAL", bind(second, (struct sockaddr *)&zero, sizeof zero) < 0 && errno == EINVAL);
    close(second);
    close(srv);

    /* Non-blocking connect, then poll for completion and read SO_ERROR. */
    fd = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    a = addr("10.0.2.100", 7);
    int r = connect(fd, (struct sockaddr *)&a, sizeof a);
    check("non-blocking connect returns EINPROGRESS", r < 0 && errno == EINPROGRESS);
    struct pollfd p = {fd, POLLOUT, 0};
    r = poll(&p, 1, 5000);
    int err = -1;
    socklen_t elen = sizeof err;
    getsockopt(fd, SOL_SOCKET, SO_ERROR, &err, &elen);
    check("poll reports the connection writable, SO_ERROR 0", r == 1 && (p.revents & POLLOUT) && err == 0);
    close(fd);

    /* Off-subnet destinations get the DHCP address as source, never 127.0.0.1. */
    fd = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    a = addr("192.0.2.1", 80);
    connect(fd, (struct sockaddr *)&a, sizeof a);
    struct sockaddr_in src;
    socklen_t slen = sizeof src;
    getsockname(fd, (struct sockaddr *)&src, &slen);
    check("off-subnet connections come from the DHCP address", src.sin_addr.s_addr == inet_addr("10.0.2.15"));
    close(fd);

    /* A blocking recv is interrupted by a signal. */
    fd = tcp_connect("10.0.2.100", 7);
    struct sigaction sa = {0};
    sa.sa_handler = on_alarm;
    sigaction(SIGALRM, &sa, NULL);
    alarm(1);
    n = fd >= 0 ? recv(fd, buf, sizeof buf, 0) : 0;
    check("recv without data is interrupted with EINTR", n < 0 && errno == EINTR);
    close(fd);

    /* UDP over loopback. */
    int rx = socket(AF_INET, SOCK_DGRAM, 0);
    a = addr("127.0.0.1", 5353);
    check("bind a UDP socket", bind(rx, (struct sockaddr *)&a, sizeof a) == 0);
    int tx = socket(AF_INET, SOCK_DGRAM, 0);
    r = sendto(tx, "datagram", 8, 0, (struct sockaddr *)&a, sizeof a);
    check("sendto a UDP datagram", r == 8);
    struct sockaddr_in from;
    socklen_t flen = sizeof from;
    n = recvfrom(rx, buf, sizeof buf, 0, (struct sockaddr *)&from, &flen);
    check("recvfrom gets it with the sender's address",
          n == 8 && memcmp(buf, "datagram", 8) == 0 && from.sin_addr.s_addr == inet_addr("127.0.0.1"));
    close(rx);
    close(tx);

    check("raw ICMP echo to the gateway gets a reply", icmp_echo("10.0.2.2"));
    check("raw ICMP echo over loopback gets a reply", icmp_echo("127.0.0.1"));
    check("IP_TTL on a raw socket sets its packets' TTL", raw_ttl());

    /* Lengths chosen to overflow naive arithmetic: an iovec length that is
     * negative as an ssize_t is EINVAL, a datagram over 65507 bytes
     * EMSGSIZE (Linux's answers). */
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    a = addr("127.0.0.1", 9);
    struct iovec iov[2] = {{buf, 8}, {buf, (size_t)-4}};
    struct msghdr m = {0};
    m.msg_name = &a;
    m.msg_namelen = sizeof a;
    m.msg_iov = iov;
    m.msg_iovlen = 2;
    check("sendmsg with an overflowing iovec fails with EINVAL", sendmsg(u, &m, 0) < 0 && errno == EINVAL);
    iov[1].iov_len = (size_t)-1;
    check("recvmsg with an overflowing iovec fails with EINVAL", recvmsg(u, &m, MSG_DONTWAIT) < 0 && errno == EINVAL);
    check("sendto with a huge length fails with EMSGSIZE",
          sendto(u, buf, (size_t)1 << 40, 0, (struct sockaddr *)&a, sizeof a) < 0 && errno == EMSGSIZE);
    close(u);

    check("AF_INET6 sockets are not supported", socket(AF_INET6, SOCK_STREAM, 0) < 0 && errno == EAFNOSUPPORT);

    stream_semantics();
    datagram_semantics();

    printf("nettest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
