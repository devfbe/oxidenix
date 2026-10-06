#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

static int failures;

static void check(const char *name, int ok) {
    printf("%-56s %s\n", name, ok ? "ok" : "FAIL");
    if (!ok) failures++;
}

int main(void) {
    const char *dir = "/data/fstest";
    char path[64], buf[64];
    mkdir(dir, 0755);

    snprintf(path, sizeof path, "%s/link", dir);
    unlink(path);
    symlink("a-target-that-is-only-text", path);
    int fd = open(path, O_RDWR | O_NOFOLLOW);
    check("O_NOFOLLOW on a symlink fails with ELOOP", fd < 0 && errno == ELOOP);
    unlink(path);

    snprintf(path, sizeof path, "%s/orphan", dir);
    fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    write(fd, "before", 6);
    unlink(path);
    check("unlinked file is gone from the directory", access(path, F_OK) < 0);
    pwrite(fd, "after!", 6, 6);
    memset(buf, 0, sizeof buf);
    pread(fd, buf, 12, 0);
    check("open fd still reads and writes the unlinked file", strcmp(buf, "beforeafter!") == 0);

    char other[64];
    snprintf(other, sizeof other, "%s/new", dir);
    int fd2 = open(other, O_RDWR | O_CREAT | O_TRUNC, 0644);
    write(fd2, "fresh", 5);
    memset(buf, 0, sizeof buf);
    pread(fd, buf, 12, 0);
    check("a new file does not share the orphan's data", strcmp(buf, "beforeafter!") == 0);
    memset(buf, 0, sizeof buf);
    pread(fd2, buf, 5, 0);
    check("the new file has its own data", strcmp(buf, "fresh") == 0);
    close(fd);
    close(fd2);
    unlink(other);

    snprintf(path, sizeof path, "%s/big", dir);
    fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    check("truncate beyond the ext2 limit fails with EFBIG", ftruncate(fd, (off_t)1 << 62) < 0 && errno == EFBIG);
    check("write at a huge offset fails with EFBIG", pwrite(fd, "x", 1, (off_t)1 << 62) < 0 && errno == EFBIG);
    void *m = mmap(NULL, 8192, PROT_READ, MAP_PRIVATE, fd, (off_t)-4096);
    check("mmap with an overflowing offset fails", m == MAP_FAILED);
    close(fd);
    unlink(path);

    snprintf(path, sizeof path, "%s/modes", dir);
    fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    write(fd, "data", 4);
    errno = 0;
    check("read on a write-only fd fails with EBADF", read(fd, buf, 4) < 0 && errno == EBADF);
    close(fd);
    fd = open(path, O_RDONLY);
    errno = 0;
    check("write on a read-only fd fails with EBADF", write(fd, "x", 1) < 0 && errno == EBADF);
    errno = 0;
    check("pwrite on a read-only fd fails with EBADF", pwrite(fd, "x", 1, 0) < 0 && errno == EBADF);
    errno = 0;
    check("ftruncate on a read-only fd fails with EINVAL", ftruncate(fd, 0) < 0 && errno == EINVAL);
    memset(buf, 0, sizeof buf);
    check("the file is unchanged", pread(fd, buf, 8, 0) == 4 && strcmp(buf, "data") == 0);
    close(fd);
    unlink(path);
    rmdir(dir);

    printf("fstest: %s\n", failures ? "FAILED" : "all passed");
    return failures;
}
