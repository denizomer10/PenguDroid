# PenguDroid Invaders (`native-game`)

A small but real game running as an Android **NativeActivity** with no ART: it
renders on the host GPU through PenguDroid's Bionic EGL/GLES shim and reads the
keyboard, so it is playable.

```sh
./build.sh
pengudroid native --lib libmain.so --width 1280 --height 720
```

Controls: `A`/`D` or the arrow keys move the paddle, `Q` quits.

What it exercises end to end:

* `ANativeActivity_onCreate` and the lifecycle callbacks (the host drives them);
* `ANativeWindow_getWidth`/`getHeight` (geometry from the host);
* EGL/GLES on the **host** driver (NVIDIA/AMD/Intel), with `eglChooseConfig`
  rewritten from `EGL_WINDOW_BIT` to the surfaceless path;
* one dma-buf per `eglSwapBuffers`, imported by the daemon and committed to a
  Wayland surface (no CPU copy, no `wl_shm`).

Measured on an RTX 3080: 1280×720 at ~56 fps (283 frames in 5 s).
