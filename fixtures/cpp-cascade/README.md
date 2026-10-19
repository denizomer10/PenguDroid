# cpp-cascade fixture

Reproducible check that PenguDroid loads an **application's own libraries as a
set**, resolving dependencies inside the APK rather than one file at a time.

It builds:

* `lib/x86_64/libc++_shared.so` — a stand-in for an app's bundled C++ runtime,
  exporting `demo_cpp_answer()`.
* `lib/x86_64/libconsumer.so` — `DT_NEEDED`s `libc++_shared.so` and calls into
  it.

and packs both into an APK-shaped zip. `pengudroid nativelib --apk` extracts and
strips the whole set into one directory, so the consumer's dependency resolves
there; the consumer is then called and must return `42`.

Why a fixture instead of a real APK: the F-Droid apps checked (DroidFish,
Dolphin, PPSSPP) all statically link their C++ runtime, so none ships
`libc++_shared.so`. This fixture exercises the same loader path with real ELF
objects and a real zip container.

```sh
./run-test.sh
```
