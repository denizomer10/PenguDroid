# `native-activity` fixture

A minimal Android `NativeActivity` app that renders a spinning triangle with
EGL/GLES and swaps frames through PenguDroid's Bionic EGL shim. It is
self-contained (no NDK headers): the struct layouts and EGL/GLES entry points are
declared in `app.c`, and every symbol resolves at load time from the preloaded
shim.

```sh
./build.sh
pengudroid native --lib libmain.so --width 960 --height 540
```

It also reads `assets/hello.txt` from the APK when launched with
`pengudroid native app.apk`, which exercises `AAssetManager`.
