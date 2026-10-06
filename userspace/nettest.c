/* Socket tests: TCP and UDP over loopback and through QEMU's user network
 * (10.0.2.100:7 is an echo service, see builder/src/main.rs). */
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
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

    check("AF_INET6 sockets are not supported", socket(AF_INET6, SOCK_STREAM, 0) < 0 && errno == EAFNOSUPPORT);

    printf("nettest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
