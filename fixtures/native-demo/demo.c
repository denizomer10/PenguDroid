/*
 * A stand-in for an Android native library, used to test PenguDroid's
 * native-loading pipeline reproducibly (no network, no real APK needed).
 *
 * The interesting property is that it is linked against a *stub* `libc.so`
 * that exports its symbols under the single Android-style version node
 * `LIBC` — exactly how Bionic's libc is versioned. That gives this library a
 * `VERNEED` for `libc.so`/`LIBC`, which glibc cannot satisfy as-is, so loading
 * it exercises `pengudroid de-bionic` (version stripping) for real.
 *
 * It imports two Bionic symbols the shim provides (`__android_log_print`,
 * `ashmem_create_region`) and exports one nullary entry point whose result is
 * trivially checkable.
 */

extern int __android_log_print(int prio, const char *tag, const char *fmt, ...);
extern int ashmem_create_region(const char *name, unsigned int size);

/* Returns 1 + 2 + ... + 1000 == 500500, and logs through the Bionic shim. */
int pengudroid_demo_run(void) {
    long sum = 0;
    for (long i = 1; i <= 1000; i++) {
        sum += i;
    }
    int fd = ashmem_create_region("android-demo", 8192);
    __android_log_print(4, "android_demo", "pengudroid_demo_run: sum=%ld ashmem_fd=%d", sum, fd);
    (void)fd;
    return (int)sum;
}
