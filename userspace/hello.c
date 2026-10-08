/* The smallest program: prints its arguments and exits with 42. */
#include <stdio.h>

int main(int argc, char **argv) {
    printf("Hello from user space! argc=%d argv[0]=%s\n", argc, argv[0]);
    return 42;
}
