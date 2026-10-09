/* The Linux interfaces libuv (and so Node.js) uses beyond POSIX, all the
 * Linux server's: statx for every stat, the io_uring probe at start (no
 * io_uring: ENOSYS, quietly), and NETLINK_ROUTE for the interface list
 * (getifaddrs, os.networkInterfaces()) with the data netd has. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <ifaddrs.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <netpacket/packet.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-64s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

/* The kernel's count of system calls the server passed back to it. */
static long legacy_calls(void) {
    char text[512] = {0};
    int fd = open("/proc/counters", O_RDONLY);
    read(fd, text, sizeof text - 1);
    close(fd);
    char *p = strstr(text, "legacy_calls ");
    return p ? atol(p + 13) : -1;
}

/* Passed-through calls since `start`, without the measurement's own. */
static long passed_since(long start, long base) { return legacy_calls() - start - base; }

static int sx(int dirfd, const char *path, int flags, unsigned mask, struct statx *x) {
    memset(x, 0xa5, sizeof *x);
    return (int)syscall(SYS_statx, dirfd, path, flags, mask, x);
}

/* statx agrees with stat on every field stat has. */
static int same(const struct statx *x, const struct stat *st) {
    return (x->stx_mask & STATX_BASIC_STATS) == STATX_BASIC_STATS && x->stx_ino == st->st_ino && x->stx_mode == st->st_mode &&
           x->stx_size == (unsigned long long)st->st_size && x->stx_nlink == st->st_nlink && x->stx_blocks == (unsigned long long)st->st_blocks &&
           x->stx_blksize == st->st_blksize && makedev(x->stx_dev_major, x->stx_dev_minor) == st->st_dev &&
           x->stx_mtime.tv_sec == st->st_mtim.tv_sec && x->stx_uid == st->st_uid;
}

static void test_statx(void) {
    struct statx x;
    struct stat st;
    for (int i = 0; i < 2; i++) {
        const char *dir = i ? "/data" : "/tmp";
        char path[64], link[64];
        snprintf(path, sizeof path, "%s/statx.%d", dir, getpid());
        snprintf(link, sizeof link, "%s/statx-link.%d", dir, getpid());
        int fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0640);
        write(fd, "hello statx", 11);
        symlink(path, link);
        char name[96];
        snprintf(name, sizeof name, "statx on %s: the fields of stat", dir);
        check(name, sx(AT_FDCWD, path, 0, STATX_BASIC_STATS, &x) == 0 && stat(path, &st) == 0 && same(&x, &st) && x.stx_size == 11 &&
                        (x.stx_mode & 07777) == 0640);
        snprintf(name, sizeof name, "statx on %s: a symlink followed and not", dir);
        check(name, sx(AT_FDCWD, link, 0, STATX_TYPE, &x) == 0 && S_ISREG(x.stx_mode) && sx(AT_FDCWD, link, AT_SYMLINK_NOFOLLOW, STATX_TYPE, &x) == 0 &&
                        S_ISLNK(x.stx_mode));
        snprintf(name, sizeof name, "statx on %s: AT_EMPTY_PATH on a descriptor", dir);
        check(name, sx(fd, "", AT_EMPTY_PATH, STATX_ALL, &x) == 0 && fstat(fd, &st) == 0 && same(&x, &st));
        int dfd = open(dir, O_RDONLY | O_DIRECTORY);
        snprintf(name, sizeof name, "statx on %s: relative to a directory descriptor", dir);
        check(name, sx(dfd, strrchr(path, '/') + 1, 0, STATX_SIZE, &x) == 0 && x.stx_size == 11);
        close(dfd);
        close(fd);
        unlink(link);
        unlink(path);
    }
    chdir("/tmp");
    check("statx: AT_FDCWD with an empty path is the working directory", sx(AT_FDCWD, "", AT_EMPTY_PATH, STATX_INO, &x) == 0 && stat("/tmp", &st) == 0 && same(&x, &st));
    check("statx: ... and a NULL path with AT_EMPTY_PATH too", sx(AT_FDCWD, NULL, AT_EMPTY_PATH, STATX_INO, &x) == 0 && x.stx_ino == st.st_ino);
    chdir("/");
    int p[2];
    pipe(p);
    check("statx: AT_EMPTY_PATH on a pipe (the server's)", sx(p[0], "", AT_EMPTY_PATH, STATX_TYPE, &x) == 0 && S_ISFIFO(x.stx_mode));
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    check("statx: AT_EMPTY_PATH on a socket (the kernel's)", sx(s, "", AT_EMPTY_PATH, STATX_TYPE, &x) == 0 && S_ISSOCK(x.stx_mode));
    check("newfstatat: AT_EMPTY_PATH on a socket", fstatat(s, "", &st, AT_EMPTY_PATH) == 0 && S_ISSOCK(st.st_mode));
    int null = open("/dev/null", O_RDWR);
    check("statx: AT_EMPTY_PATH on /dev/null (the kernel's tree)", sx(null, "", AT_EMPTY_PATH, STATX_TYPE, &x) == 0 && S_ISCHR(x.stx_mode));
    errno = 0;
    check("statx: an empty path without AT_EMPTY_PATH is ENOENT", sx(AT_FDCWD, "", 0, STATX_TYPE, &x) == -1 && errno == ENOENT);
    errno = 0;
    check("statx: a missing file is ENOENT", sx(AT_FDCWD, "/tmp/no/such", 0, STATX_TYPE, &x) == -1 && errno == ENOENT);
    errno = 0;
    check("statx: unknown flags are EINVAL", sx(AT_FDCWD, "/", 0x1, STATX_TYPE, &x) == -1 && errno == EINVAL);
    errno = 0;
    check("statx: STATX__RESERVED in the mask is EINVAL", sx(AT_FDCWD, "/", 0, 0x80000000u, &x) == -1 && errno == EINVAL);
    errno = 0;
    check("statx: both sync types at once are EINVAL", sx(AT_FDCWD, "/", AT_STATX_FORCE_SYNC | AT_STATX_DONT_SYNC, STATX_TYPE, &x) == -1 && errno == EINVAL);
    errno = 0;
    check("statx: a bad buffer is EFAULT", syscall(SYS_statx, AT_FDCWD, "/", 0, STATX_TYPE, (void *)16) == -1 && errno == EFAULT);
    errno = 0;
    check("statx: a bad descriptor is EBADF", sx(999, "", AT_EMPTY_PATH, STATX_TYPE, &x) == -1 && errno == EBADF);

    long idle = legacy_calls();
    long base = legacy_calls() - idle;
    long start = legacy_calls();
    for (int i = 0; i < 20; i++) {
        sx(AT_FDCWD, "/bin/sh", 0, STATX_BASIC_STATS, &x);
        sx(s, "", AT_EMPTY_PATH, STATX_TYPE, &x);
        syscall(SYS_newfstatat, null, "", &st, AT_EMPTY_PATH);
    }
    long passed = passed_since(start, base);
    check("statx and newfstatat(AT_EMPTY_PATH) are the server's", passed == 0);
    close(null);
    close(s);
    close(p[0]);
    close(p[1]);
}

static void test_io_uring(void) {
    long idle = legacy_calls();
    long base = legacy_calls() - idle;
    long start = legacy_calls();
    char params[120] = {0};
    errno = 0;
    long setup = syscall(425, 8, params);
    int e1 = errno;
    long enter = syscall(426, -1, 0, 0, 0, NULL, 0);
    int e2 = errno;
    long reg = syscall(427, -1, 0, NULL, 0);
    int e3 = errno;
    long passed = passed_since(start, base);
    check("io_uring_setup is ENOSYS", setup == -1 && e1 == ENOSYS);
    check("io_uring_enter and io_uring_register too", enter == -1 && e2 == ENOSYS && reg == -1 && e3 == ENOSYS);
    check("... answered by the server (nothing passed to the kernel)", passed == 0);
}

static void test_getifaddrs(void) {
    struct ifaddrs *list = NULL;
    check("getifaddrs works (RTM_GETLINK and RTM_GETADDR dumps)", getifaddrs(&list) == 0 && list != NULL);
    int lo_inet = 0, lo_link = 0, eth_link = 0, eth_inet = 0;
    for (struct ifaddrs *i = list; i; i = i->ifa_next) {
        if (!i->ifa_addr) continue;
        int family = i->ifa_addr->sa_family;
        if (strcmp(i->ifa_name, "lo") == 0) {
            int flags_ok = (i->ifa_flags & (IFF_UP | IFF_LOOPBACK | IFF_RUNNING)) == (IFF_UP | IFF_LOOPBACK | IFF_RUNNING);
            if (family == AF_INET) {
                struct sockaddr_in *a = (struct sockaddr_in *)i->ifa_addr, *m = (struct sockaddr_in *)i->ifa_netmask;
                lo_inet = flags_ok && a->sin_addr.s_addr == htonl(0x7f000001) && m && m->sin_addr.s_addr == htonl(0xff000000);
            } else if (family == AF_PACKET) {
                lo_link = flags_ok;
            }
        } else if (strcmp(i->ifa_name, "eth0") == 0) {
            int flags_ok = (i->ifa_flags & (IFF_UP | IFF_BROADCAST | IFF_RUNNING)) == (IFF_UP | IFF_BROADCAST | IFF_RUNNING) && !(i->ifa_flags & IFF_LOOPBACK);
            if (family == AF_PACKET) {
                struct sockaddr_ll *ll = (struct sockaddr_ll *)i->ifa_addr;
                static const unsigned char qemu[6] = {0x52, 0x54, 0x00, 0x12, 0x34, 0x56};
                eth_link = flags_ok && ll->sll_halen == 6 && memcmp(ll->sll_addr, qemu, 6) == 0 && ll->sll_ifindex == 2;
            } else if (family == AF_INET) {
                struct sockaddr_in *a = (struct sockaddr_in *)i->ifa_addr, *m = (struct sockaddr_in *)i->ifa_netmask;
                struct sockaddr_in *b = (struct sockaddr_in *)i->ifa_broadaddr;
                char text[32];
                inet_ntop(AF_INET, &a->sin_addr, text, sizeof text);
                printf("    (eth0: %s)\n", text);
                eth_inet = flags_ok && a->sin_addr.s_addr == inet_addr("10.0.2.15") && m && m->sin_addr.s_addr == inet_addr("255.255.255.0") && b &&
                           b->sin_addr.s_addr == inet_addr("10.0.2.255");
            }
        }
    }
    check("lo: up, loopback, 127.0.0.1/8", lo_inet && lo_link);
    check("eth0: up, broadcast, netd's MAC, index 2", eth_link);
    check("eth0: netd's DHCP address 10.0.2.15/24, broadcast 10.0.2.255", eth_inet);
    freeifaddrs(list);
}

/* netdevice(7)'s requests, on a socket of the kernel's (AF_INET) and on
 * a netlink socket of the server's. */
static void test_netdevice(void) {
    int socks[2] = {socket(AF_INET, SOCK_DGRAM, 0), socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE)};
    for (int k = 0; k < 2; k++) {
        int s = socks[k];
        const char *kind = k ? "netlink socket" : "inet socket";
        char name[96];
        struct ifreq r = {0};
        strcpy(r.ifr_name, "eth0");
        snprintf(name, sizeof name, "SIOCGIFINDEX, SIOCGIFMTU of eth0 (%s)", kind);
        int ok = ioctl(s, SIOCGIFINDEX, &r) == 0 && r.ifr_ifindex == 2;
        ok = ok && ioctl(s, SIOCGIFMTU, &r) == 0 && r.ifr_mtu == 1500;
        check(name, ok);
        snprintf(name, sizeof name, "SIOCGIFHWADDR, SIOCGIFFLAGS of eth0 (%s)", kind);
        static const unsigned char qemu[6] = {0x52, 0x54, 0x00, 0x12, 0x34, 0x56};
        ok = ioctl(s, SIOCGIFHWADDR, &r) == 0 && r.ifr_hwaddr.sa_family == 1 && memcmp(r.ifr_hwaddr.sa_data, qemu, 6) == 0;
        ok = ok && ioctl(s, SIOCGIFFLAGS, &r) == 0 && (r.ifr_flags & (IFF_UP | IFF_RUNNING | IFF_BROADCAST)) == (IFF_UP | IFF_RUNNING | IFF_BROADCAST);
        check(name, ok);
        snprintf(name, sizeof name, "SIOCGIFADDR, SIOCGIFNETMASK, SIOCGIFBRDADDR of eth0 (%s)", kind);
        struct sockaddr_in *in = (struct sockaddr_in *)&r.ifr_addr;
        ok = ioctl(s, SIOCGIFADDR, &r) == 0 && in->sin_family == AF_INET && in->sin_addr.s_addr == inet_addr("10.0.2.15");
        ok = ok && ioctl(s, SIOCGIFNETMASK, &r) == 0 && in->sin_addr.s_addr == inet_addr("255.255.255.0");
        ok = ok && ioctl(s, SIOCGIFBRDADDR, &r) == 0 && in->sin_addr.s_addr == inet_addr("10.0.2.255");
        check(name, ok);
        memset(&r, 0, sizeof r);
        r.ifr_ifindex = 1;
        snprintf(name, sizeof name, "SIOCGIFNAME of index 1 is lo, a loopback (%s)", kind);
        check(name, ioctl(s, SIOCGIFNAME, &r) == 0 && strcmp(r.ifr_name, "lo") == 0 && ioctl(s, SIOCGIFFLAGS, &r) == 0 && (r.ifr_flags & IFF_LOOPBACK));
        struct ifreq reqs[8];
        struct ifconf conf = {.ifc_len = 0, .ifc_buf = NULL};
        snprintf(name, sizeof name, "SIOCGIFCONF: the length, then lo and eth0 (%s)", kind);
        ok = ioctl(s, SIOCGIFCONF, &conf) == 0 && conf.ifc_len == 2 * (int)sizeof(struct ifreq);
        conf.ifc_len = sizeof reqs;
        conf.ifc_req = reqs;
        ok = ok && ioctl(s, SIOCGIFCONF, &conf) == 0 && conf.ifc_len == 2 * (int)sizeof(struct ifreq) && strcmp(reqs[0].ifr_name, "lo") == 0 &&
             strcmp(reqs[1].ifr_name, "eth0") == 0 && ((struct sockaddr_in *)&reqs[1].ifr_addr)->sin_addr.s_addr == inet_addr("10.0.2.15");
        check(name, ok);
        strcpy(r.ifr_name, "nope0");
        errno = 0;
        snprintf(name, sizeof name, "an unknown interface is ENODEV (%s)", kind);
        check(name, ioctl(s, SIOCGIFINDEX, &r) == -1 && errno == ENODEV);
        close(s);
    }
    int p[2];
    pipe(p);
    struct ifreq r = {0};
    strcpy(r.ifr_name, "lo");
    errno = 0;
    check("interface requests on a pipe are ENOTTY", ioctl(p[0], SIOCGIFINDEX, &r) == -1 && errno == ENOTTY);
    close(p[0]);
    close(p[1]);
}

/* Sends one request (header and `body`) to the kernel's end. */
static int nl_send(int fd, int type, int flags, unsigned seq, const void *body, size_t len) {
    char buf[256] = {0};
    struct nlmsghdr *h = (struct nlmsghdr *)buf;
    h->nlmsg_len = NLMSG_LENGTH(len);
    h->nlmsg_type = type;
    h->nlmsg_flags = flags;
    h->nlmsg_seq = seq;
    memcpy(NLMSG_DATA(h), body, len);
    struct sockaddr_nl kernel = {.nl_family = AF_NETLINK};
    return (int)sendto(fd, buf, h->nlmsg_len, 0, (struct sockaddr *)&kernel, sizeof kernel);
}

static void test_netlink_socket(void) {
    errno = 0;
    check("netlink: an unknown protocol is EPROTONOSUPPORT", socket(AF_NETLINK, SOCK_RAW, 99) == -1 && errno == EPROTONOSUPPORT);
    errno = 0;
    check("netlink: SOCK_STREAM is ESOCKTNOSUPPORT", socket(AF_NETLINK, SOCK_STREAM, NETLINK_ROUTE) == -1 && errno == ESOCKTNOSUPPORT);
    int fd = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    check("netlink: a NETLINK_ROUTE socket", fd >= 0 && (fcntl(fd, F_GETFD) & FD_CLOEXEC));
    struct sockaddr_nl me = {0};
    socklen_t len = sizeof me;
    check("netlink: unbound, its port is 0", getsockname(fd, (struct sockaddr *)&me, &len) == 0 && me.nl_family == AF_NETLINK && me.nl_pid == 0 &&
                                                  len == sizeof me);
    struct sockaddr_nl any = {.nl_family = AF_NETLINK};
    len = sizeof me;
    check("netlink: bind gives it a port of its own", bind(fd, (struct sockaddr *)&any, sizeof any) == 0 &&
                                                          getsockname(fd, (struct sockaddr *)&me, &len) == 0 && me.nl_pid != 0);
    int type = 0, proto = -1, domain = 0;
    len = sizeof type;
    getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &len);
    len = sizeof proto;
    getsockopt(fd, SOL_SOCKET, SO_PROTOCOL, &proto, &len);
    len = sizeof domain;
    getsockopt(fd, SOL_SOCKET, SO_DOMAIN, &domain, &len);
    check("netlink: SO_TYPE, SO_PROTOCOL, SO_DOMAIN", type == SOCK_RAW && proto == NETLINK_ROUTE && domain == AF_NETLINK);
    struct stat st;
    check("netlink: fstat says socket", fstat(fd, &st) == 0 && S_ISSOCK(st.st_mode));

    int other = socket(AF_NETLINK, SOCK_DGRAM, NETLINK_ROUTE);
    struct sockaddr_nl taken = {.nl_family = AF_NETLINK, .nl_pid = me.nl_pid};
    errno = 0;
    check("netlink: a port in use is EADDRINUSE", bind(other, (struct sockaddr *)&taken, sizeof taken) == -1 && errno == EADDRINUSE);

    char buf[8192];
    errno = 0;
    check("netlink: nothing queued: EAGAIN with MSG_DONTWAIT", recv(fd, buf, sizeof buf, MSG_DONTWAIT) == -1 && errno == EAGAIN);
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN, .data.fd = fd}, out;
    epoll_ctl(ep, EPOLL_CTL_ADD, fd, &ev);
    check("netlink: epoll sees no input yet", epoll_wait(ep, &out, 1, 0) == 0);

    /* One link by index, acknowledged. */
    struct ifinfomsg ifi = {.ifi_family = AF_UNSPEC, .ifi_index = 1};
    check("netlink: RTM_GETLINK for index 1 is sent", nl_send(fd, RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK, 41, &ifi, sizeof ifi) == NLMSG_LENGTH(sizeof ifi));
    check("netlink: epoll and poll see the answer", epoll_wait(ep, &out, 1, 1000) == 1 && out.data.fd == fd &&
                                                        poll(&(struct pollfd){.fd = fd, .events = POLLIN}, 1, 0) == 1);
    struct sockaddr_nl from = {0};
    len = sizeof from;
    int n = (int)recvfrom(fd, buf, sizeof buf, 0, (struct sockaddr *)&from, &len);
    struct nlmsghdr *h = (struct nlmsghdr *)buf;
    struct ifinfomsg *got = NLMSG_DATA(h);
    int named_lo = 0;
    if (n > 0 && NLMSG_OK(h, (unsigned)n) && h->nlmsg_type == RTM_NEWLINK) {
        struct rtattr *a = IFLA_RTA(got);
        int alen = IFLA_PAYLOAD(h);
        for (; RTA_OK(a, alen); a = RTA_NEXT(a, alen))
            if (a->rta_type == IFLA_IFNAME) named_lo = strcmp(RTA_DATA(a), "lo") == 0;
    }
    check("netlink: RTM_NEWLINK for lo, with our sequence number and port", named_lo && h->nlmsg_seq == 41 && h->nlmsg_pid == me.nl_pid &&
                                                                                got->ifi_index == 1 && (got->ifi_flags & IFF_LOOPBACK) && from.nl_pid == 0);
    n = (int)recv(fd, buf, sizeof buf, 0);
    struct nlmsgerr *err = NLMSG_DATA(h);
    check("netlink: then the acknowledgement (NLMSG_ERROR 0)", n > 0 && h->nlmsg_type == NLMSG_ERROR && err->error == 0 && err->msg.nlmsg_seq == 41);

    /* A request it does not implement. */
    struct rtmsg rt = {.rtm_family = AF_INET};
    nl_send(fd, RTM_NEWROUTE, NLM_F_REQUEST, 42, &rt, sizeof rt);
    n = (int)recv(fd, buf, sizeof buf, 0);
    check("netlink: an unsupported request answers EOPNOTSUPP", n > 0 && h->nlmsg_type == NLMSG_ERROR && err->error == -EOPNOTSUPP && err->msg.nlmsg_type == RTM_NEWROUTE);

    /* A dump read through a short buffer: truncated, MSG_TRUNC says so. */
    struct rtgenmsg g = {.rtgen_family = AF_UNSPEC};
    nl_send(fd, RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 43, &g, sizeof g);
    n = (int)recv(fd, buf, 16, MSG_PEEK | MSG_TRUNC);
    check("netlink: MSG_PEEK|MSG_TRUNC gives the datagram's length", n > 16);
    int full = n;
    struct iovec iov = {.iov_base = buf, .iov_len = 16};
    struct msghdr msg = {.msg_name = &from, .msg_namelen = sizeof from, .msg_iov = &iov, .msg_iovlen = 1};
    n = (int)recvmsg(fd, &msg, 0);
    check("netlink: a short buffer truncates (MSG_TRUNC in msg_flags)", n == 16 && (msg.msg_flags & MSG_TRUNC) && msg.msg_namelen == sizeof from);
    errno = 0;
    check("netlink: the rest of the datagram is gone", recv(fd, buf, sizeof buf, MSG_DONTWAIT) == -1 && errno == EAGAIN && full > 16);

    /* Datagrams between two sockets of the instance, by port. */
    len = sizeof me;
    struct sockaddr_nl them = {0};
    bind(other, (struct sockaddr *)&any, sizeof any);
    getsockname(other, (struct sockaddr *)&them, &len);
    check("netlink: a datagram to another socket's port", sendto(fd, "ping", 4, 0, (struct sockaddr *)&them, sizeof them) == 4 &&
                                                              recvfrom(other, buf, sizeof buf, 0, (struct sockaddr *)&from, &len) == 4 &&
                                                              memcmp(buf, "ping", 4) == 0 && from.nl_pid == me.nl_pid);
    close(other);
    errno = 0;
    check("netlink: to a port nobody has: ECONNREFUSED", sendto(fd, "x", 1, 0, (struct sockaddr *)&them, sizeof them) == -1 && errno == ECONNREFUSED);
    int p[2];
    pipe(p);
    errno = 0;
    check("netlink: socket calls on a pipe are ENOTSOCK", recv(p[0], buf, 1, MSG_DONTWAIT) == -1 && errno == ENOTSOCK);
    close(p[0]);
    close(p[1]);

    long idle = legacy_calls();
    long base = legacy_calls() - idle;
    long start = legacy_calls();
    for (int i = 0; i < 10; i++) {
        nl_send(fd, RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 100 + i, &g, sizeof g);
        recv(fd, buf, sizeof buf, 0);
    }
    long passed = passed_since(start, base);
    check("netlink: the calls are the server's", passed == 0);
    close(ep);
    close(fd);
}

int main(void) {
    test_statx();
    test_io_uring();
    test_getifaddrs();
    test_netdevice();
    test_netlink_socket();
    printf("libuvtest: %s\n", failures ? "FAILED" : "all passed");
    return failures != 0;
}
