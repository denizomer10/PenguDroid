# opensl-app fixture

End-to-end check of the OpenSL ES **buffer-queue player** implemented in
`pengudroid-shim`.

The app is compiled against the **canonical Khronos/AOSP OpenSL ES headers**
vendored under `SLES/` (plus the AOSP `jni.h` that `OpenSLES_Android.h` includes)
and linked directly against `libpengudroid_shim.so`. That matters: because it
uses the real headers, the shim's hand-written `#[repr(C)]` vtables
(`SLObjectItf_`, `SLEngineItf_`, `SLPlayItf_`, `SLAndroidSimpleBufferQueueItf_`)
are checked against the actual ABI rather than against a mirrored guess.

It creates an engine and an output mix, creates an audio player backed by an
Android simple buffer queue, registers a callback that re-enqueues, sets the
player to `PLAYING`, and enqueues **silence** (so nothing audible is produced).
Success requires the player to report `PLAYING` and at least one buffer-queue
callback to have fired.

```sh
./run-test.sh
```

The shim spawns `pw-cat` (or `aplay`) per player to actually play the PCM; with
silence buffers this is inaudible.

## Header provenance

`SLES/OpenSLES.h`, `SLES/OpenSLES_Platform.h`, `SLES/OpenSLES_Android.h`,
`SLES/OpenSLES_AndroidConfiguration.h`, `SLES/OpenSLES_AndroidMetadata.h` and
`jni.h` are the upstream Khronos/AOSP headers, licensed Apache-2.0
(`Copyright (C) The Android Open Source Project`). They are vendored unchanged so
the fixture builds offline.
