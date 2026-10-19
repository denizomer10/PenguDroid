# egl-app fixture

A minimal native app that uses EGL/GLES the way an Android app would, loading
**PenguDroid's Bionic-compatible `libEGL`** (`crates/pengudroid-gleshim`) instead
of the system library.

On `eglSwapBuffers` the shim exports the frame's FBO as a dma-buf and sends it to
the daemon (`PresentDmabuf`), so a successful run proves the last piece of the
GPU path:

```
app EGL/GL -> host GPU -> dma-buf -> daemon -> Wayland (zero copy)
```

```sh
./run-test.sh
```

Environment knobs: `PENGUDROID_LIBEGL` (path to the shim), `PENGUDROID_SURFACE_W`
/ `_H` (virtual surface size), `PENGUDROID_SURFACE_ID` (present to an existing
surface instead of creating one).
