# Native bridge: ARM64 guest code on x86_64

PenguDroid runs Android applications as host processes. When an application
ships only ARM64 native code, that code cannot execute on an x86_64 CPU
directly. Android solves this with a **native bridge**: a shared library that
implements the same ABI hooks the runtime uses for native code, but executes the
guest instruction set through a translator.

PenguDroid uses that public contract rather than inventing one. The translator
itself is **not** part of this project — it is Google's
[`frameworks/libs/binary_translation`](https://android.googlesource.com/platform/frameworks/libs/binary_translation/)
(Berberis), whose ARM64-to-x86_64 build is the Digitalis tree.

## Module boundary

All knowledge of the bridge ABI lives in one crate:

```
crates/pengudroid-nativebridge/       # ABI, loader, path rule, self-test calls
  src/lib.rs                          # Callbacks / RuntimeCallbacks / Translator
  tests/abi.rs                        # header layout + stub-translator round trip
```

Call sites are deliberately few:

| Call site | What it does |
| --- | --- |
| `pengudroid-cli/src/sysroot.rs` | `--bridge-selftest <lib>#<symbol>` drives `initialize` → `loadLibrary` → `getTrampoline` → call; `setup_native_bridge` uses the crate's `RuntimeCallbacks` for `libnativebridge` |
| `pengudroid-cli/src/launcher.rs` | opt-in env (`PENGUDROID_NATIVE_BRIDGE_LIB`) forwards the bridge name and `arm64` ISA when launching an ARM-only app |

Nothing else may restate vtable indices or function signatures. Adding a field,
changing a path rule or supporting a newer interface version is a one-file
change in the crate; removing the whole feature means deleting the crate, those
two call sites, and this document.

## Measured contract

Source: `frameworks/libs/binary_translation` at
`android17-release` revision `1290e88c60304844bd6e1c0778241ca249d64019`
(cloned to `.toolchain/references/binary_translation-android17`) and the
Digitalis ARM64 tree in `.toolchain/platform_frameworks_libs_binary_translation-android16`.
Both agree on the following.

### `NativeBridgeItf`

The bridge exports a symbol named `NativeBridgeItf` whose contents are
`android::NativeBridgeCallbacks`:

* **21 members**, in declaration order, interface **version 8**.
* `version` is a `uint32_t`; the first function pointer follows at the natural
  alignment boundary (offset 8 on LP64).
* Slots this project uses: `initialize` (2nd member), `loadLibrary` (3rd),
  `getTrampoline` (4th), `isCompatibleWith` (7th), `isPathSupported` (11th),
  `getError` (10th), `unloadLibrary` (9th).

`initialize` takes `(runtime_cbs, private_dir, instruction_set)` — in that
order. `getTrampoline` takes `(handle, name, shorty, shorty_len)`, where the
shorty is the **JNI shorty** (`I` for a static `()I`), not the descriptor
`()I`.

### Runtime callbacks

`NativeBridgeRuntimeCallbacks` is three function pointers, in order:
`getMethodShorty`, `getNativeMethodCount`, `getNativeMethods`. The table is
retained by the bridge for the process lifetime, so it must be a `static`; a
stack-local copy is dereferenced after the frame is gone.

### ARM64 parameters

```c
kGuestIsa                     = "arm64"
kSupportedLibraryPathSubstring = "/lib/arm64"
NativeBridgeRuntimeValues     = { .os_arch = "aarch64",
                                  .cpu_abi = "arm64-v8a",
                                  .cpu_abi2 = nullptr }
```

The path rule is a **substring** test (`strstr`), not a directory-component
test. Consequently both of these are accepted:

```
.../lib/arm64/libSDL3.so        # canonical device layout
.../lib/arm64-v8a/libSDL3.so    # an APK's own ABI directory
```

while `.../lib/x86_64/libSDL3.so` is not. PenguDroid stages guest libraries
into `lib/arm64` as the canonical layout (the crate copies APK libraries there),
but it does not claim the bridge rejects `arm64-v8a` — it does not. A "tighter"
rule invented on our side would reject libraries the bridge accepts.

## Build result (2026-10-29)

`tools/build-nativebridge.py` now builds the ARM64-to-x86_64 translator on this
host without a Soong checkout:

```
python3 -B tools/build-nativebridge.py --towards-bridge --seconds 1800
# Translator shared built: YES
```

Measured artifact:

* `.toolchain/nativebridge-build/run-g5xion4t/lib/libberberis_arm64.so`
* 37,719,928 bytes, SHA-256
  `11fec1897d6ec4cb5faa19eb14753db980e7d4ea43d5dcf2383d36581e976f35`
* ELF64 x86_64 shared object with a global dynamic `NativeBridgeItf`
  (version 8, 168-byte table).

What the driver had to add to get there, all measured against the pinned
`android17-release` sources:

1. The `libopenjdkjvmti_headers` slice (`art/openjdkjvmti/include`, pinned at
   `b753cf97923c3695338d21466fa14c57b480a59a`); the header-only module is
   declared by the driver because upstream `art_defaults` is outside the slice.
2. `-Wno-vla-cxx-extension` and
   `-Wno-missing-designated-field-initializers` *after* the module's `-Werror`
   (both are AOSP global cflags/newer-Clang diagnostics).
3. A host build for `cc_binary_host` code generators. The host uses the pinned
   glibc libc++ headers/static libraries, a small `liblog` shim, `-DANDROID`
   (the Digitalis `logging.h` contract), and a 64 MiB stack. The generated
   `gen_riscv64_to_x86_64_intrinsics` runs before the ARM64 objects compile.
4. `sanitize` and `afdo` are inert: this driver never passes `-fsanitize` or
   `-fprofile-use`. `libz` is allowed to be a static archive with an empty
   `system_shared_libs`; `liblog` is imported from the verified API-37 GSI
   instead of being compiled from an incomplete `system/core` slice.
5. The build needs 6 GiB of virtual address space and up to 300 s per command
   for `heavy_optimizer/arm64/frontend.cc` (measured 3.75 GiB RSS, 77.5 s).

## ARM64 guest runtime

The translator starts `/system/bin/arm64/app_process64` and needs the guest
linker, libc and framework libraries under `/system/lib64/arm64`.
`tools/prepare-arm64-guest.py` extracts the official **same-build**
`aosp_arm64` GSI (`CP2A.260605.012`, archive SHA-256
`8e545e1e1e977c3b43b4e54a3af463f3e7d1c3177b120e772138334e15f48f75`) without
mounting it, verifies the archive, and stages 832 guest files. It also builds
`libnative_bridge_vdso.so` from the cached `native_bridge_support` source (the
preparation tool records this input identity honestly as **not pinned**; it
records source hash, tool identity and arguments rather than enforcing a pin).

A real run now gets through `LoadNativeBridge` and `PreInitializeNativeBridge`
and into `native_bridge_initialize`, then stops at:

```
FATAL berberis: native_bridge_initialize:
  couldn't find "__native_bridge_config_static_tls" symbol in linker
```

The stock GSI `linker64` is not the native-bridge product's modified linker;
the latter exports that symbol and configures the guest's static TLS, and the
guest libc is built with the matching hooks. Building
`native_bridge_guest_linker` and `libnative_bridge_guest_libc` from
`frameworks/libs/native_bridge_support` + the matching Bionic tree is the next
concrete task. Until then no ARM64 app, including BBR2, is claimed to run.

The runtime is deliberately **fail-closed** about this now:

* `tools/prepare-arm64-guest.py` publishes a schema-2 manifest whose
  `bridge_compatibility.state` is `staged-only`, and refuses to adopt or rewrite
  the legacy schema-1 stock staging.
* `pengudroid-cli/src/sysroot.rs` refuses any staging manifest before
  `JNI_CreateJavaVM`, and checks the source-defined ELF contracts of a
  manifest-free bundle instead: the guest linker must export
  `__native_bridge_config_static_tls` and `__native_bridge_get_host_pthread`,
  the guest libc must import `native_bridge_intercept_symbol`, and the VDSO must
  export its four hooks. Stock GSI binaries fail these checks by construction.
* The selected translator is preloaded by exact path and its basename
  resolution is proven identical (same `NativeBridgeItf` object) before
  `LoadNativeBridge`, so the state machine and the direct self-test cannot
  silently drive different libraries.
* `NativeBridgeGetSignalHandler` yields a **callback** (`bool (*)(int,
  siginfo_t*, void*)`), not a `struct sigaction`. The shim chains that callback
  ahead of the previous ART handler and re-raises the default disposition when
  nothing handles the fault; a missing or failing chain adapter is an error,
  never a replaced ART handler.

## Using it

### Host requirements

The bridge is built for Android: it carries Bionic symbol versions. glibc cannot
resolve those, so a bridge `.so` must be prepared before `dlopen`:

```
pengudroid de-bionic <bridge-in.so> <bridge-out.so>
```

and its directory passed to the launcher. The crate itself does no stripping; it
records that as the caller's responsibility so the boundary stays explicit.

### Self-test

```
PENGUDROID_PROP_ro.dalvik.vm.native.bridge=libberberis_arm64.so \
./target/debug/pengudroid art-test --sysroot <api37-tree> \
    --bridge-lib /path/to/prepared --init-bridge arm64 \
    --bridge-selftest /path/lib/arm64/libfixture.so#pengudroid_fixture_answer
```

Expected sequence in the log:

```
art: NativeBridgeItf.version = 8 (...)
art: Itf.initialize(<dir>, arm64) = true
art: Itf.loadLibrary(...) = 0x...
art: Itf.getTrampoline(pengudroid_fixture_answer, shorty=I) = 0x...
art: calling the translated guest function
art: guest pengudroid_fixture_answer() returned 42
```

A failure is reported with the contract that was not met — a missing slot, a
refused path, a null trampoline, an unsupported version. There is no silent
fallback.

### Launching an ARM-only application

```
PENGUDROID_NATIVE_BRIDGE_LIB=/path/to/prepared \
PENGUDROID_NATIVE_BRIDGE_NAME=libberberis_arm64.so \
./target/debug/pengudroid launch app.apk --sysroot <api37-tree>
```

The launcher sets `ro.dalvik.vm.native.bridge`, the ARM64 ISA properties and
`--init-bridge arm64`, and raises the heap to 128 MiB because the bridge
reserves the guest address layout.

## Guest-side artifact

`fixtures/arm64-bridge/fixture.c` is ordinary Android ARM64 code: it is what a
translated APK library looks like to the bridge.

```
tools/build-arm64-bridge-fixture.sh
```

cross-compiles it with clang (`--target=aarch64-linux-android37`) against
`.toolchain/ndk-refs_tags_ndk-r30-platform-sysroot`, links with `ld.lld`
(`-m aarch64linux -shared --no-undefined -z max-page-size=16384`), and fails
unless the result is AArch64 and exports the expected symbols. On this machine
it produces:

```
libpengudroid_arm64_fixture.so: ELF 64-bit LSB shared object, ARM aarch64
NEEDED: libc.so, liblog.so
undefined: __android_log_print, __cxa_atexit@LIBC, __cxa_finalize@LIBC,
           __register_atfork@LIBC
```

The `@LIBC` versions are exactly what the translator's guest linker and libc
proxy must resolve. The fixture is **not** run on the host; it is the input to
translation.

## Build progress (native-only driver)

`tools/build-nativebridge.py` reproduces a narrow, Android-only subset of the
pinned Digitalis Blueprint rules with LLVM 22 and the API-37 platform CRT. It is
**not** a Soong replacement: unsupported rules fail closed. All compiler runs are
serial, capped in address space/CPU/wall time/output size, and recorded in
`.toolchain/nativebridge-build/report.json`.

Built and verified as little-endian ELF64 x86_64, compiled for
`x86_64-linux-android37` against the Android platform libc++ (`std::__1`):

| Artifact | Kind | SHA-256 (prefix) |
| --- | --- | --- |
| `libberberis_exec_region.so` | shared | `1f7a8dff044b…` |
| `libberberis_assembler.a` | static | `1b46c8bca1c1…` |
| `libberberis_base.a` (11 objects) | static | `871128168148…` |
| `libberberis_native_bridge_arm64.a` (3 objects) | static | `8ddcc7049323…` |
| `libberberis_guest_abi_arm64.a` | static | `e8f5bc3356e4…` |

`libberberis_native_bridge_arm64.a` contains the **archive definition**
`0000000000000000 D NativeBridgeItf`. That is an object-file symbol, **not** a
translator `.so` export and **not** evidence of ARM64 execution.

Platform inputs are imported only from the completed API-37 extraction and only
after a read-only gate (`--check-platform`) that checks the runtime manifest
identity, per-file SHA-256, in-tree symlinks, ELF machine/SONAME and required
defined exports. That gate verifies **link inputs only** — not their transitive
loader closure, and nothing is executed.

**Historical (superseded by "Build result" above):** this section described the
point before whole-static flattening, verified platform imports and the host
codegen build landed. The translator shared object now builds; the remaining
blocker is the guest runtime, not the translator. No guest instruction has been
translated **on this host** yet. Nothing here establishes that Beach Buggy
Racing 2 runs.

## Verified, and not verified

Verified in this checkout, with no Android runtime:

* The Rust table matches the real `native_bridge.h` **offset by offset** and in
  total size. The test compiles the header and compares every field, so a
  reordered or resized member fails immediately.
* A stub translator, whose table is declared in the header's order, is driven
  end to end: `initialize` rejects the wrong argument order, an x86_64 path is
  refused, a staged ARM64 path loads, a descriptor shorty is rejected while `I`
  is accepted, and the "translated" function returns 42.
* A bridge with interface version 99 and a library without `NativeBridgeItf` are
  both rejected.
* The path rule is a substring test, proven for `arm64`, `arm64-v8a` and
  `x86_64` paths.
* An API-37 AArch64 guest library cross-builds and links with Bionic versions.

**Not verified:** the translator binary exists and loads (`LoadNativeBridge =
true`, `PreInitializeNativeBridge = true`, `Initialized Berberis (aarch64)`),
but no guest instruction has been translated on this host: initialization stops
at the missing `__native_bridge_config_static_tls` guest-linker hook described
above. The full guest-runtime quartet must come from a matching Android-17
Soong product build (`native_bridge_guest_linker.native_bridge`,
`libnative_bridge_guest_libc.native_bridge`,
`native_bridge_guest_app_process.native_bridge`,
`libnative_bridge_vdso.native_bridge`); the cached Bionic tree is header slices
only, so extending `tools/build-nativebridge.py` into a Bionic builder is not a
small change. A full AOSP product build with `enable_arm64_to_x86_64.mk` remains
the supported upstream route. Nothing here establishes that Beach Buggy Racing
2 runs.

## What Beach Buggy Racing 2 needs

The supplied XAPK (`com.vectorunit.cobalt.googleplay`, 2026.10.05) contains 18
native libraries, all `lib/arm64-v8a/*.so` with ELF machine 183 (AArch64).
Offline inventory across every APK member found no x86_64 payload. Reaching that
code therefore needs, in order:

1. An Android 17 / API 37 x86_64 runtime. **Present** — see
   [ANDROID17.md](ANDROID17.md); it currently runs imageless.
2. A built Digitalis ARM64 bridge, prepared for glibc.
3. Guest libraries staged where the bridge accepts them.
4. Graphics, input and audio proxying for an SDL3/Unity-class engine — the
   translator only covers instruction execution, not rendering.
5. Framework services the game calls at startup, which are the remaining
   `docs/ANDROID17.md` work.

The bridge is necessary and not sufficient. Each of those is separately
testable, and the self-test above is the first.

## Removing the feature later

When x86_64 application support is mature enough that ARM64 translation is no
longer carried, delete:

* `crates/pengudroid-nativebridge/`
* the `pengudroid-nativebridge` entries in the workspace and CLI manifests
* the `--bridge-selftest`/`--init-bridge` path in `sysroot.rs` and the
  `PENGUDROID_NATIVE_BRIDGE_*` block in `launcher.rs`
* this document and its `STATUS.md` section

No other file contains bridge ABI knowledge, which is the point of the module.
