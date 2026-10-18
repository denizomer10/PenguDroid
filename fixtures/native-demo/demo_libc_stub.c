/*
 * A stub "libc.so" used only at *link* time by build.sh. It exports the two
 * Bionic symbols the demo library uses under Android's single `LIBC` version
 * node, which is what makes the demo library carry an Android-style
 * `VERNEED` (libc.so / LIBC).
 *
 * At run time this file is irrelevant: PenguDroid maps the Bionic soname
 * `libc.so` to the host glibc and the Bionic symbols to the shim.
 */

int __android_log_print(int prio, const char *tag, const char *fmt, ...) {
    (void)prio;
    (void)tag;
    (void)fmt;
    return 0;
}

int ashmem_create_region(const char *name, unsigned int size) {
    (void)name;
    (void)size;
    return -1;
}
