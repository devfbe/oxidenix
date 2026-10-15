/* The shared library dyntest loads with dlopen (userspace/dynamic): thread-local
 * variables of a module loaded at run time (dynamic TLS: __tls_get_addr, the
 * initialized ones from the module's TLS image, the zeroed ones from its tbss),
 * a constructor, and a call back into the C library that loaded it. */
#include <stdio.h>
#include <string.h>

__thread int lib_tls = 5;
__thread char lib_tbss[1000];
int lib_constructed;

__attribute__((constructor)) static void lib_init(void) {
    lib_constructed++;
}

int lib_get(void) {
    return lib_tls;
}

void lib_set(int v) {
    lib_tls = v;
}

int *lib_tls_addr(void) {
    return &lib_tls;
}

/* Whether this thread's zeroed TLS block is all zeros. */
int lib_tbss_zero(void) {
    for (unsigned i = 0; i < sizeof lib_tbss; i++)
        if (lib_tbss[i]) return 0;
    return 1;
}

void lib_tbss_fill(char c) {
    memset(lib_tbss, c, sizeof lib_tbss);
}

/* A call into the C library through this module's own relocations. */
int lib_format(char *buf, unsigned len, int v) {
    return snprintf(buf, len, "dyntls %d", v);
}
