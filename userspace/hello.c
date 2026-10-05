#include <stdio.h>

int main(int argc, char **argv) {
    printf("Hallo aus dem Userspace! argc=%d argv[0]=%s\n", argc, argv[0]);
    return 42;
}
