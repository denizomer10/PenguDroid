# native-demo fixture

A tiny, network-free stand-in for an Android native library, used to test
PenguDroid's native-loading pipeline end to end.

Why it exists: it is linked against a stub `libc.so` whose symbols are versioned
under the single node `LIBC` — the way Bionic's libc is versioned — so the
resulting `libandroid_demo.so` carries a `VERNEED` for `libc.so`/`LIBC`. That is
exactly the version requirement glibc cannot satisfy, so loading it proves that
`pengudroid de-bionic` (version stripping) and `pengudroid libpath` (soname
mapping) actually work — not just on a hand-waved copy, but on a reproducible
artifact.

It imports two Bionic symbols that the shim provides
(`__android_log_print`, `ashmem_create_region`) and exports one entry point,
`pengudroid_demo_run`, which returns `500500` (the sum 1..1000).

## Build and test

```sh
./build.sh                 # produces lib/libandroid_demo.so
./run-test.sh              # builds, then loads + calls it via PenguDroid
```

`run-test.sh` uses `pengudroid nativelib lib/libandroid_demo.so --call
pengudroid_demo_run` and fails unless the output contains `500500`.
