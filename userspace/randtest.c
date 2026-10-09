/* getrandom(2) and AT_RANDOM: the kernel's generator (ChaCha20 seeded from
 * the CPU's entropy source and timing jitter). Flags as man 2 getrandom
 * says, every byte set, no two answers alike, large requests whole,
 * EFAULT, and AT_RANDOM's 16 bytes differing between processes. */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/random.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef GRND_INSECURE
#define GRND_INSECURE 4
#endif

static int failures;

static void check(const char *name, int ok) {
    printf("%-52s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "--at-random") == 0) {
        /* The child: its AT_RANDOM to the parent. */
        return write(1, (void *)getauxval(AT_RANDOM), 16) == 16 ? 0 : 1;
    }
    unsigned char a[64], b[64];
    check("getrandom fills the buffer", getrandom(a, sizeof a, 0) == sizeof a);
    check("... and the next answer differs", getrandom(b, sizeof b, 0) == sizeof b && memcmp(a, b, sizeof a) != 0);
    check("GRND_NONBLOCK: never EAGAIN once seeded", getrandom(a, sizeof a, GRND_NONBLOCK) == sizeof a);
    check("GRND_RANDOM: the same pool", getrandom(a, sizeof a, GRND_RANDOM) == sizeof a);
    check("GRND_INSECURE", getrandom(a, sizeof a, GRND_INSECURE) == sizeof a);
    check("GRND_INSECURE with GRND_RANDOM: EINVAL", getrandom(a, sizeof a, GRND_INSECURE | GRND_RANDOM) == -1 && errno == EINVAL);
    check("an unknown flag: EINVAL", getrandom(a, sizeof a, 0x40) == -1 && errno == EINVAL);
    check("a zero length: 0", getrandom(a, 0, 0) == 0);
    check("a bad buffer: EFAULT", getrandom((void *)8, 16, 0) == -1 && errno == EFAULT);

    /* A large request comes whole, and its bytes look uniform. */
    size_t big = 1 << 20;
    unsigned char *p = malloc(big);
    long got = p ? getrandom(p, big, 0) : -1;
    unsigned counts[256] = {0};
    for (long i = 0; i < got; i++) counts[p[i]]++;
    int uniform = got == (long)big;
    for (int v = 0; v < 256; v++) uniform &= counts[v] > 3500 && counts[v] < 4700;
    check("1 MiB at once, every byte value about as often", uniform);
    free(p);

    /* AT_RANDOM: 16 bytes, not the same in a child process. */
    unsigned char mine[16], theirs[16];
    memcpy(mine, (void *)getauxval(AT_RANDOM), 16);
    int pipefd[2];
    pipe(pipefd);
    if (fork() == 0) {
        char *args[] = {argv[0], "--at-random", NULL};
        dup2(pipefd[1], 1);
        execvp(argv[0], args);
        _exit(1);
    }
    close(pipefd[1]);
    int n = read(pipefd[0], theirs, 16);
    wait(NULL);
    check("AT_RANDOM differs between processes", n == 16 && memcmp(mine, theirs, 16) != 0);

    printf("randtest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
