/* USB tethering's tests (the USB boot, /etc/usbtests.sh): the machine has no
 * network card, only USB Ethernet functions (QEMU's usb-net, see
 * builder/src/main.rs, USB_NETWORK): usb0 (CDC-ECM, on a root port, on
 * 10.0.3.0/24) and usb1 (RNDIS, behind a USB hub, on 10.0.4.0/24). Each
 * comes up with its DHCP lease, as Linux names and describes it, and
 * carries TCP to its network's echo service (.100:7) intact, also in bulk
 * and in both directions at once; and both come back after usbd dies, and
 * after netd dies. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <ifaddrs.h>
#include <net/if.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* restricted::TEST_KILL_SERVER (test mode): kills the kernel's server `name`. */
#define TEST_KILL_SERVER 1523

static int failures;

static void check(const char *name, int ok) {
    printf("%-60s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

static long now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000 + t.tv_nsec / 1000000;
}

struct link {
    const char *name;
    const char *subnet; /* the first three octets */
    unsigned char mac[6];
    const char *echo;
};

static const struct link links[] = {
    {"usb0", "10.0.3.", {0x52, 0x54, 0x00, 0xaa, 0x00, 0x01}, "10.0.3.100"},
    {"usb1", "10.0.4.", {0x52, 0x54, 0x00, 0xaa, 0x00, 0x02}, "10.0.4.100"},
};

/* The link's IPv4 address as text ("" while it has none). */
static void address_of(const char *name, char *out, size_t len) {
    out[0] = 0;
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct ifreq r = {0};
    strncpy(r.ifr_name, name, IFNAMSIZ - 1);
    if (ioctl(s, SIOCGIFADDR, &r) == 0) inet_ntop(AF_INET, &((struct sockaddr_in *)&r.ifr_addr)->sin_addr, out, len);
    close(s);
}

/* Waits up to `ms` for every link's lease. */
static int wait_for_leases(long ms) {
    long until = now_ms() + ms;
    for (;;) {
        int all = 1;
        for (size_t i = 0; i < sizeof links / sizeof *links; i++) {
            char a[32];
            address_of(links[i].name, a, sizeof a);
            all &= strncmp(a, links[i].subnet, strlen(links[i].subnet)) == 0;
        }
        if (all) return 1;
        if (now_ms() > until) return 0;
        usleep(100000);
    }
}

static void describes(const struct link *l) {
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct ifreq r = {0};
    char name[96];
    strncpy(r.ifr_name, l->name, IFNAMSIZ - 1);
    int up = ioctl(s, SIOCGIFFLAGS, &r) == 0 && (r.ifr_flags & (IFF_UP | IFF_RUNNING | IFF_BROADCAST)) == (IFF_UP | IFF_RUNNING | IFF_BROADCAST);
    snprintf(name, sizeof name, "%s: up, running, broadcast", l->name);
    check(name, up);
    int mac = ioctl(s, SIOCGIFHWADDR, &r) == 0 && memcmp(r.ifr_hwaddr.sa_data, l->mac, 6) == 0;
    snprintf(name, sizeof name, "%s: the function's MAC address", l->name);
    check(name, mac);
    int mtu = ioctl(s, SIOCGIFMTU, &r) == 0 && r.ifr_mtu == 1500;
    snprintf(name, sizeof name, "%s: MTU 1500 (%d)", l->name, r.ifr_mtu);
    check(name, mtu);
    int mask = ioctl(s, SIOCGIFNETMASK, &r) == 0 && ((struct sockaddr_in *)&r.ifr_netmask)->sin_addr.s_addr == inet_addr("255.255.255.0");
    snprintf(name, sizeof name, "%s: a /24 from its network's DHCP", l->name);
    check(name, mask);
    close(s);
}

static int tcp_connect(const char *ip, int port) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = {0};
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    inet_pton(AF_INET, ip, &a.sin_addr);
    struct timeval tv = {10, 0};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof tv);
    if (connect(fd, (struct sockaddr *)&a, sizeof a) < 0) {
        close(fd);
        return -1;
    }
    return fd;
}

/* Sends `len` bytes of a pattern to the echo service and reads them back
 * while it sends (both directions at once): whether all came back intact.
 * The rate in KiB/s goes to `rate`. */
static int echo(const char *ip, size_t len, long *rate) {
    int fd = tcp_connect(ip, 7);
    if (fd < 0) return 0;
    char *out = malloc(len), *in = malloc(len);
    for (size_t i = 0; i < len; i++) out[i] = (char)(i * 7 + i / 251);
    size_t sent = 0, got = 0;
    long start = now_ms();
    int ok = 1;
    while (got < len && ok) {
        struct pollfd p = {fd, POLLIN | (sent < len ? POLLOUT : 0), 0};
        if (poll(&p, 1, 10000) <= 0) {
            ok = 0;
            break;
        }
        if (p.revents & POLLOUT && sent < len) {
            ssize_t n = send(fd, out + sent, len - sent > 65536 ? 65536 : len - sent, MSG_DONTWAIT);
            if (n > 0) sent += n;
            else if (n < 0 && errno != EAGAIN) ok = 0;
        }
        if (p.revents & POLLIN) {
            ssize_t n = recv(fd, in + got, len - got, MSG_DONTWAIT);
            if (n > 0) got += n;
            else if (n == 0 || errno != EAGAIN) ok = 0;
        }
    }
    long took = now_ms() - start;
    *rate = took > 0 ? (long)(len / 1024) * 1000 / took : 0;
    ok = ok && got == len && memcmp(in, out, len) == 0;
    free(out);
    free(in);
    close(fd);
    return ok;
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    check("both USB links have their leases (within 30 s)", wait_for_leases(30000));
    for (size_t i = 0; i < sizeof links / sizeof *links; i++) describes(&links[i]);
    check("no network card (no eth0)", if_nametoindex("eth0") == 0);
    check("usb0 and usb1 by index", if_nametoindex("usb0") != 0 && if_nametoindex("usb1") != 0 && if_nametoindex("usb0") < if_nametoindex("usb1"));
    for (size_t i = 0; i < sizeof links / sizeof *links; i++) {
        char name[96];
        long rate = 0;
        int fd = tcp_connect(links[i].echo, 7);
        snprintf(name, sizeof name, "%s: TCP to its network's echo service", links[i].name);
        check(name, fd >= 0);
        if (fd >= 0) {
            const char hello[] = "hello through USB";
            char back[sizeof hello] = {0};
            int ok = send(fd, hello, sizeof hello, 0) == (ssize_t)sizeof hello;
            size_t got = 0;
            while (ok && got < sizeof hello) {
                ssize_t n = recv(fd, back + got, sizeof hello - got, 0);
                if (n <= 0) break;
                got += n;
            }
            snprintf(name, sizeof name, "%s: the echo comes back", links[i].name);
            check(name, ok && got == sizeof hello && memcmp(back, hello, sizeof hello) == 0);
            close(fd);
        }
        int ok = echo(links[i].echo, 1 << 20, &rate);
        snprintf(name, sizeof name, "%s: 1 MiB both ways at once, intact (%ld KiB/s)", links[i].name, rate);
        check(name, ok);
    }
    /* Both at once: frames of two functions on one controller (one of
     * them behind the hub) interleave. */
    pid_t kid = fork();
    if (kid == 0) {
        long rate;
        _exit(echo(links[1].echo, 512 << 10, &rate) ? 0 : 1);
    }
    long rate;
    int mine = echo(links[0].echo, 512 << 10, &rate);
    int st = 0;
    waitpid(kid, &st, 0);
    check("both links at once", mine && WIFEXITED(st) && WEXITSTATUS(st) == 0);

    /* usbd dies (as a crash would): the kernel starts it again (its
     * keeper), it finds the devices anew and offers netd new links, which
     * get their leases again. Then netd dies: the next use starts it again
     * and usbd offers it the links anew. Either way the links work after. */
    const char *servers[] = {"usb0", "net"};
    for (int i = 0; i < 2; i++) {
        char name[96];
        long t0 = now_ms();
        long killed = syscall(TEST_KILL_SERVER, servers[i]);
        snprintf(name, sizeof name, "the kernel's %s killed", servers[i]);
        check(name, killed == 0);
        /* The links go with it (the dead usbd's channels, or netd's). */
        int gone = 0;
        for (long until = now_ms() + 5000; !gone && now_ms() < until; usleep(20000)) {
            char a[32];
            address_of("usb0", a, sizeof a);
            gone = a[0] == 0;
        }
        snprintf(name, sizeof name, "with %s, usb0's lease goes", servers[i]);
        check(name, gone);
        int back = wait_for_leases(30000);
        long rate = 0;
        snprintf(name, sizeof name, "after %s's death: both links and their leases back (%ld ms)", servers[i], now_ms() - t0);
        check(name, back);
        snprintf(name, sizeof name, "after %s's death: TCP through usb0 and usb1", servers[i]);
        check(name, back && echo(links[0].echo, 64 << 10, &rate) && echo(links[1].echo, 64 << 10, &rate));
    }
    if (failures) printf("usbnettest: %d FAILED\n", failures);
    else printf("usbnettest: all passed\n");
    return failures != 0;
}
