#!/usr/bin/env python3
"""Build a narrow, Android-only subset of the pinned Digitalis Blueprint rules.

Run: python3 -B tools/build-nativebridge.py [--offline] [--towards-bridge] [--module NAME ...]
     python3 -B tools/build-nativebridge.py --offline --check-platform [--platform-root PATH]
     python3 -B tools/build-nativebridge.py --offline --module libberberis_proxy_libc \
         --reuse-report .toolchain/nativebridge-build/RUN/report.json \
         [--de-bionic-tool target/debug/pengudroid]
This is an early native build driver, NOT a complete Soong implementation. It
supports literal Blueprint values/list variables, defaults, x86_64 Android
variants, exported headers, filegroups, simple Python genrules, and the pinned
berberis.translation_arch=arm64_to_x86_64 proxy-defaults contract. Unsupported
rules fail closed. The default steps attempt exec_region, assembler, base, then
the Android ARM64 NativeBridge entry archive, not the broad berberis_all target.

Only .toolchain/nativebridge-build/ is written. No sources are patched. Compiler
runs are serial, capped in address space, CPU/wall time, and output size. Every
command, selected module, source pin and first blocker is retained in report.json.
A separately checksummed last-successful-early.json survives failed probes. Platform
imports require the completed extractor manifest to pin each imported file;
shared-link inputs are rechecked immediately before use. Never inspect or repair an extraction stage.
--reuse-report checks shared artifacts without rebuilding their translator closure.
--de-bionic-tool uses an already-built CLI, never cargo; loader-compatible copies
are NOT glibc ABI implementations, and unsafe interfaces are recorded explicitly.
"""

import argparse
import base64
import copy
import fcntl
import glob
import hashlib
import importlib.util
import json
import os
import re
import resource
import shlex
import signal
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parent.parent
ROOT = REPO / ".toolchain/nativebridge-build"
DEPS = REPO / ".toolchain/nativebridge-deps"
LLVM = Path("/usr/lib/llvm-22/bin")
HOST_LIBCXX = REPO / ".toolchain/host-libcxx/usr"
PLATFORM_ROOT = REPO / ".toolchain/sysroot-android17"
RUNTIME_MANIFEST = "pengudroid-runtime.json"
GSI_BUILD = "CP2A.260605.012"
GSI_SHA256 = "3da2ca5599d304c4d92fe8c72f31d035a59db57c93ce073740ed4015ee034a8e"
PLATFORM_SHARED = {
    "libandroid": {"artifact": "libandroid_bp", "relative": "system/lib64/libandroid.so",
                   "required_exports": ["ANativeActivity_finish"]},
    "libdl_android": {"artifact": "bionic_libdl_bp", "relative": "system/lib64/libdl_android.so",
                      "required_exports": ["android_create_namespace", "android_link_namespaces",
                                           "android_get_exported_namespace", "android_init_anonymous_namespace"]},
    # `libbase` and the JNI host tool log through liblog. The API-37 GSI's
    # Bionic library is a verified link input; it is never recompiled from the
    # incompletely extractable system/core slice.
    "liblog": {"artifact": "", "relative": "system/lib64/liblog.so",
               "required_exports": ["__android_log_print", "__android_log_assert",
                                    "__android_log_write", "__android_log_vprint"]},
}
# Proxies use private platform libc APIs absent from the NDK link declarations.
# Import on demand rather than silently accepting an unresolved allocator API.
PLATFORM_PRIVATE = {
    "libc": {"relative": "system/lib64/libc.so",
             "required_exports": ["malloc_disable", "malloc_enable", "malloc_iterate"]},
}
OWNER = {"tool": "PenguDroid nativebridge-build", "schema": 1}
# Goals, not source/flag lists: all module contents come from Android.bp.
EARLY_GOALS = ("libberberis_exec_region", "libberberis_assembler", "libberberis_base",
               "libberberis_native_bridge_arm64")
MEMORY = 6_000_000_000  # Virtual-address limit; clang 22 maps ~3.8 GB for the ARM64 backend.
FILE_LIMIT = 64 * 1024 * 1024
TOKEN = re.compile(r'"(?:\\.|[^"\\])*"|//[^\n]*|/\*[\s\S]*?\*/|[A-Za-z_][\w.]*|\d+|\+=|[{}\[\]:,=+]|\S')
PATH_FIELDS = {"srcs", "exclude_srcs", "local_include_dirs", "export_include_dirs", "linker_scripts"}
# Device OS properties precede arch/multilib properties (pinned Soong android/arch.go).
# not_windows is HOST-only; linux_x86_64 and bionic_x86_64 also apply to Android.
OS_AXES = ("linux", "bionic", "android", "android64")
ARCH_AXES = ("linux_x86_64", "bionic_x86_64", "android_x86_64")
AXES = OS_AXES + ARCH_AXES
KNOWN_ARCHES = {"arm", "arm64", "riscv64", "x86", "x86_64"}
KNOWN_TARGETS = {
    "host", "linux", "host_linux", "bionic", "glibc", "musl", "android", "android32", "android64",
    "linux_glibc", "linux_bionic", "linux_musl", "darwin", "windows", "not_windows", "native_bridge",
    "vendor", "product", "ramdisk", "vendor_ramdisk", "recovery", "platform",
} | {f"{os_name}_{arch}" for os_name in ("linux", "host_linux", "bionic", "glibc", "musl", "android",
                                      "linux_glibc", "linux_bionic", "linux_musl", "darwin", "windows")
     for arch in KNOWN_ARCHES}
# The guest architecture is NEVER a global flag. It comes from each module's defaults.
# Platform constants/global includes: build/soong cc/config/global.go, pinned below.
PLATFORM_FLAGS = ("-DANDROID", "-DNDEBUG", "-UDEBUG", "-D__compiler_offsetof=__builtin_offsetof")
# Warnings the module's own `-Werror` would otherwise promote. build/soong
# cc/config/global.go sets this after the warning flags: "Allow using VLA CXX
# extension"; `guest_jni_trampolines.cc` relies on it for an ICU tag buffer.
TRAILING_FLAGS = ("-Wno-vla-cxx-extension",
                  # Clang 22 diagnostics absent from the pinned clang-r584948;
                  # the Android tree has not been cleaned up for them yet.
                  "-Wno-missing-designated-field-initializers")

METADATA = {
    "name", "defaults", "host_supported", "device_supported", "native_bridge_supported",
    "vendor_available", "product_available", "ramdisk_available", "vendor_ramdisk_available",
    "recovery_available", "apex_available", "visibility", "licenses", "min_sdk_version",
    "sdk_version", "compile_multilib", "enabled", "tidy", "tidy_flags", "tidy_checks",
    "tidy_disabled_srcs", "tidy_checks_as_errors", "owner", "double_loadable", "defaults_visibility",
    "unique_host_soname", "static_ndk_lib", "stubs", "header_abi_checker",
    # `sanitize` only selects Clang sanitizer instrumentation. This driver
    # never passes `-fsanitize`, so the property has no effect on the artifact
    # it produces and is ignored rather than silently enabling a sanitizer.
    "sanitize",
    # `afdo` picks a profile-guided-optimization profile. This driver builds
    # -O0 without `-fprofile-use`, so there is nothing to select.
    "afdo",
}
CC_FIELDS = {
    "srcs", "exclude_srcs", "cflags", "cppflags", "conlyflags", "asflags", "cpp_std", "c_std",
    "local_include_dirs", "include_dirs", "export_include_dirs", "header_libs", "static_libs",
    "whole_static_libs", "shared_libs", "export_header_lib_headers", "export_static_lib_headers",
    "export_shared_lib_headers", "generated_headers", "export_generated_headers", "generated_sources",
    "linker_scripts", "ldflags", "stl", "system_shared_libs", "rtti",
}

RULE_FIELDS = {
    "cc_defaults": CC_FIELDS, "cc_library_headers": CC_FIELDS,
    "cc_library_static": CC_FIELDS, "cc_library_shared": CC_FIELDS, "cc_library": CC_FIELDS,
    "cc_binary_host": CC_FIELDS,
    "filegroup": {"srcs", "exclude_srcs"},
    "python_binary_host": {"main", "srcs", "libs"},
    "python_library_host": {"srcs", "libs"},
    "genrule": {"out", "srcs", "tools", "cmd"},
    "genrule_defaults": {"out", "srcs", "tools", "cmd"},
}


class BuildError(RuntimeError):
    pass


def load_bootstrap():
    spec = importlib.util.spec_from_file_location("nativebridge_bootstrap", REPO / "tools/nativebridge-deps.py")
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


BOOT = load_bootstrap()
# Additional inputs only; bootstrap artifacts are verified in place, never fetched here.
EXTRAS = (
    BOOT.Artifact("libnativehelper", "libnativehelper", "5b63a7e308ca88c5c4a2a74dfda9c73ed17cfebb",
                  "extra/libnativehelper", tree="1291ec8f46bdae9cb92718e16eed0bf37979defa", limit=2_000_000),
    BOOT.Artifact("cutils_log", "system/core", "545d2487e38192a2ce25040897ced877cf6b4f53",
                  "extra/platform-headers/cutils/log.h", blob="0e0248e50fa7aa2370a12398e361e76e31e2686b",
                  subtree="libcutils/include/cutils/log.h", limit=4096),
    BOOT.Artifact("bionic_libc_bp", "bionic", "06356e41c5ed7b12220b24c05ba4fdb873b126a7",
                  "extra/bionic-libc/Android.bp", blob="3c56ed765520764d480c876823bf53f40fff6566",
                  subtree="libc/Android.bp", limit=150_000),
    BOOT.Artifact("libandroid_bp", "frameworks/base", "94b4c163b7dfe5ce3607f7bb8456f9573f7de57d",
                  "extra/frameworks-base/native/android/Android.bp", blob="6c172b98e7f80ee21fa53fc4b440adec4657bb50",
                  subtree="native/android/Android.bp", limit=50_000),
    BOOT.Artifact("bionic_libdl_bp", "bionic", "06356e41c5ed7b12220b24c05ba4fdb873b126a7",
                  "extra/bionic-libdl/Android.bp", blob="eb6afbf93b271ceb205a7a4e91e21ca791592370",
                  subtree="libdl/Android.bp", limit=50_000),
    BOOT.Artifact("power_aidl_bp", "hardware/interfaces", "0162af698935100a590b7359581ac8b1b80693e5",
                  "extra/hardware-interfaces/power/aidl/Android.bp", blob="1e1b51122fb4a4a8b0359135d7b315b9973db5fb",
                  subtree="power/aidl/Android.bp", limit=50_000),
    BOOT.Artifact("arect_bp", "frameworks/native", "ae266dcb706d083868578cfedce381ef44488a07",
                  "extra/frameworks-native/libs/arect/Android.bp", blob="cbba7118b66291733bd6f702103722bb35e6495f",
                  subtree="libs/arect/Android.bp", limit=20_000),
    BOOT.Artifact("arect_headers", "frameworks/native", "ae266dcb706d083868578cfedce381ef44488a07",
                  "extra/frameworks-native/libs/arect/include", tree="4ebbfb3ac0ab19c6b981fa6ee4782e15a2abecf3",
                  subtree="libs/arect/include", limit=100_000),
    BOOT.Artifact("libandroid_platform_headers", "frameworks/base", "94b4c163b7dfe5ce3607f7bb8456f9573f7de57d",
                  "extra/frameworks-base/native/android/include_platform", tree="66e8f444f6bb64c938e61bcee9d594c80a9f4a5f",
                  subtree="native/android/include_platform", limit=200_000),
    BOOT.Artifact("npumanager_bp", "packages/modules/NpuManager", "d194f8ada2cab31b8487fb3aaac6fd383fc7e07f",
                  "extra/npumanager/ndk/Android.bp", blob="b8af5827649d2b89aa416ffc7982d005f96f3ae6",
                  subtree="ndk/Android.bp", limit=50_000),
    BOOT.Artifact("npumanager_headers", "packages/modules/NpuManager", "d194f8ada2cab31b8487fb3aaac6fd383fc7e07f",
                  "extra/npumanager/ndk/include", tree="6f4548099a4d2542588903b70dd16869bccede59",
                  subtree="ndk/include", limit=200_000),
    BOOT.Artifact("libdl_private_headers", "bionic", "06356e41c5ed7b12220b24c05ba4fdb873b126a7",
                  "extra/bionic-libdl/include_private", tree="49dffaf48ec5021635e74fb1c45e30c50b8ea8a1",
                  subtree="libdl/include_private", limit=100_000),
    BOOT.Artifact("soong_bp", "build/soong", "6722dd8833db7482df1a2543ca3fcf67ddf0f7b1",
                  "extra/soong/Android.bp", blob="7c1d0963c27e8830f3bb1eaf81a182e5154d2979",
                  subtree="Android.bp", limit=50_000),
    BOOT.Artifact("soong_global", "build/soong", "6722dd8833db7482df1a2543ca3fcf67ddf0f7b1",
                  "extra/soong/global.go", blob="2188687602d4badafd300dff8c018c70cd5b8245",
                  subtree="cc/config/global.go", limit=100_000),
    # ART's JVMTI headers are the `libopenjdkjvmti_headers` contract the JNI
    # trampolines include. The GSI sysroot does not carry source headers, so the
    # exact android17-release slice is pinned here. The module is declared by a
    # driver-owned header-only Android.bp (see `ensure_art_header_module`), not by
    # patching the upstream file whose `art_defaults` live outside this slice.
    BOOT.Artifact("art_openjdkjvmti", "art", "b753cf97923c3695338d21466fa14c57b480a59a",
                  "extra/art-openjdkjvmti", subtree="openjdkjvmti", limit=2_000_000),
    BOOT.Artifact("bionic_pthread_internal", "bionic", "06356e41c5ed7b12220b24c05ba4fdb873b126a7",
                  "extra/platform-headers/bionic/pthread_internal.h", blob="b3133a92dfa7a26f9c3f5d6ec58ec2d1ea412f58",
                  subtree="libc/bionic/pthread_internal.h", limit=20_000),
)


ART_HEADER_BLUEPRINT = '''// Generated by tools/build-nativebridge.py: the header-only contract the
// Digitalis JNI trampolines compile against. The upstream `art_defaults` and
// full ART source tree are not part of this slice.
cc_library_headers {
    name: "libopenjdkjvmti_headers",
    host_supported: true,
    export_include_dirs: ["../art-openjdkjvmti/include"],
}
'''


def ensure_art_header_module():
    directory = ROOT / "extra/art-headers"
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / "Android.bp"
    if not path.exists() or path.read_text() != ART_HEADER_BLUEPRINT:
        path.write_text(ART_HEADER_BLUEPRINT)


def verify_elf(path, elf_type, machine=62):
    with Path(path).open("rb") as stream:
        header = stream.read(20)
    if (header[:6] != b"\x7fELF\x02\x01" or int.from_bytes(header[16:18], "little") != elf_type
            or int.from_bytes(header[18:20], "little") != machine):
        arch = "x86_64" if machine == 62 else str(machine)
        raise BuildError(f"not the required little-endian ELF64 {arch} type {elf_type}: {path}")


# The public header tree is the pinned Bionic libc/include slice already
# cached by the guest-linker build. Only read it; do not modify that tree.
BIONIC_INCLUDE_TREE = "bd088cdc87a48ec511e2d5b105be674a3d6b35b3"
BRIDGE_TLS_COOKIE = 0xf953e6ea42974914
BRIDGE_TLS_PATCH = REPO / "tools/nativebridge-glibc-tls.patch.json"
BRIDGE_TLS_MODULES = {
    "guest_loader/guest_loader.cc": "libberberis_guest_loader_arm64",
    "guest_os_primitives/guest_thread.cc": "libberberis_guest_os_primitives_arm64",
}
BRIDGE_TLS_LAYOUT = {
    "pthread_size": "sizeof(pthread_internal_t)",
    "pthread_align": "alignof(pthread_internal_t)",
    **{f"pthread_{field}": f"offsetof(pthread_internal_t, {field})" for field in (
        "tid", "attr", "join_state", "cleanup_stack", "start_routine", "stack_top", "stack_bottom",
        "current_dlerror", "dlerror_buffer", "bionic_tls", "errno_value", "bionic_tcb")},
    "attr_size": "sizeof(pthread_attr_t)",
    **{f"attr_{field}": f"offsetof(pthread_attr_t, {field})" for field in (
        "flags", "stack_base", "stack_size", "guard_size", "sched_policy", "sched_priority")},
    "bionic_tls_size": "sizeof(bionic_tls)",
    "bionic_tls_align": "alignof(bionic_tls)",
    **{f"tls_{field}": f"offsetof(bionic_tls, {field})" for field in (
        "key_data", "locale", "libgen_buffers_ptr", "fdtrack_disabled", "bionic_systrace_enabled")},
    "libgen_size": "sizeof(libgen_buffers)",
    "tcb_size": "sizeof(bionic_tcb)",
    "min_tls_slot": "MIN_TLS_SLOT", "max_tls_slot": "MAX_TLS_SLOT",
    "thread_id_slot": "TLS_SLOT_THREAD_ID", "bionic_tls_slot": "TLS_SLOT_BIONIC_TLS",
}


def probe_bridge_tls(runner, sysroot):
    public = DEPS / "sources/bionic-expansion/bionic/libc/include"
    if not public.is_dir() or BOOT.tree_digest(public) != BIONIC_INCLUDE_TREE:
        raise BuildError("bridge TLS ABI probe requires the unmodified pinned Bionic public headers")
    directory = runner.root / "tls-probes"
    directory.mkdir()
    source = directory / "layout.cc"
    source.write_text('#include <stddef.h>\n#include <stdint.h>\n#include "bionic/pthread_internal.h"\n'
                      'extern "C" __attribute__((used, section(".pengudroid_tls_abi")))\n'
                      'const int64_t pengudroid_tls_layout[] = {\n' +
                      ",\n".join(BRIDGE_TLS_LAYOUT.values()) + '\n};\n')
    result = {}
    for arch, triple, machine in (("x86_64", "x86_64-linux-android", 62),
                                   ("arm64", "aarch64-linux-android", 183)):
        obj, payload = directory / (arch + ".o"), directory / (arch + ".bin")
        argv = [str(LLVM / "clang++"), "--no-default-config", "--target=" + triple + "37",
                "--sysroot=" + str(sysroot), "-nostdinc++", "-isystem",
                str(DEPS / "toolchain/libcxx/android/x86_64/include/c++/v1"), "-isystem",
                str(DEPS / "toolchain/libcxx/include/c++/v1"), "-isystem", str(public), "-isystem",
                str(sysroot / "usr/include" / triple), "-I", str(ROOT / "extra/platform-headers"),
                "-I", str(DEPS / "sources/bionic/libc"), "-std=gnu++23", "-Wno-invalid-constexpr",
                "-Wno-invalid-offset-of", "-Xclang", "-fdump-record-layouts", "-c", str(source), "-o", str(obj)]
        record = runner.run(argv, "probe Bionic TLS layout " + arch)
        verify_elf(obj, 1, machine)
        runner.run([str(LLVM / "llvm-objcopy"), "--dump-section=.pengudroid_tls_abi=" + str(payload), str(obj)],
                   "extract Bionic TLS layout " + arch)
        data = payload.read_bytes()
        if len(data) != len(BRIDGE_TLS_LAYOUT) * 8:
            raise BuildError("Bionic TLS layout probe emitted an unexpected payload size")
        layout = dict(zip(BRIDGE_TLS_LAYOUT, struct.unpack("<" + "q" * len(BRIDGE_TLS_LAYOUT), data)))
        bitfield = re.findall(r"(?m)^\s*(\d+):(\d+)-(\d+)\s*\|\s*uint32_t cached_pid_\s*$",
                              Path(record["log"]).read_text())
        if len(bitfield) != 1:
            raise BuildError("Bionic cached_pid bitfield was not proven by the compiler record layout")
        layout.update(zip(("pthread_cached_pid", "cached_pid_bit_first", "cached_pid_bit_last"),
                          map(int, bitfield[0])))
        result[arch] = {"layout": layout, "object": str(obj), "sha256": BOOT.sha256_file(obj),
                        "record_layout_log": record["log"]}
    common = set(BRIDGE_TLS_LAYOUT) - {"min_tls_slot", "max_tls_slot", "bionic_tls_slot"}
    if any(result["x86_64"]["layout"][k] != result["arm64"]["layout"][k] for k in common):
        raise BuildError("Bionic pthread/bionic_tls layouts differ between host and guest")
    digest = hashlib.sha256(json.dumps(result["arm64"]["layout"], sort_keys=True, separators=(",", ":")).encode()).digest()
    cookie = int.from_bytes(digest[:8], "little")
    if cookie != BRIDGE_TLS_COOKIE:
        raise BuildError("compiled Bionic TLS layout does not match the tracked bridge TLS ABI cookie")
    host = result["x86_64"]["layout"]
    if (host["min_tls_slot"], host["max_tls_slot"], host["bionic_tls_slot"]) != (0, 10, 9):
        raise BuildError("unexpected native x86_64 Bionic TCB slot layout")
    result["layout_cookie"] = hex(cookie)
    result["public_headers_git_tree"] = BIONIC_INCLUDE_TREE
    runner.report["bridge_tls_abi"] = result
    return result


def merge(left, right):
    result = copy.deepcopy(left)
    for key, value in right.items():
        if isinstance(value, list):
            result[key] = result.get(key, []) + value
        elif isinstance(value, dict):
            result[key] = merge(result.get(key, {}), value)
        else:
            result[key] = value
    return result


class Values:
    """The small Blueprint expression subset needed here; never evaluate code."""
    def __init__(self, tokens, variables=None):
        self.tokens, self.variables, self.index = tokens, variables or {}, 0

    def take(self, expected=None):
        if self.index >= len(self.tokens):
            raise BuildError("truncated Blueprint expression")
        token = self.tokens[self.index]
        self.index += 1
        if expected is not None and token != expected:
            raise BuildError(f"unsupported Blueprint expression: expected {expected}, got {token}")
        return token

    def value(self):
        token = self.take()
        if token == "{":
            result = {}
            while self.index < len(self.tokens) and self.tokens[self.index] != "}":
                key = self.take()
                self.take(":")
                if key in result:
                    raise BuildError(f"duplicate Blueprint property: {key}")
                result[key] = self.value()
                if self.index < len(self.tokens) and self.tokens[self.index] != "}":
                    self.take(",")
            self.take("}")
        elif token == "[":
            result = []
            while self.index < len(self.tokens) and self.tokens[self.index] != "]":
                result.append(self.value())
                if self.index < len(self.tokens) and self.tokens[self.index] != "]":
                    self.take(",")
            self.take("]")
        elif token.startswith('"') or token.isdigit() or token in ("true", "false"):
            result = json.loads(token)
        elif token in self.variables:
            result = copy.deepcopy(self.variables[token])
        else:
            raise BuildError(f"unsupported/unresolved Blueprint value: {token}")
        if self.index < len(self.tokens) and self.tokens[self.index] == "+":
            self.take("+")
            other = self.value()
            if isinstance(result, list) and isinstance(other, list):
                result.extend(other)
            elif isinstance(result, str) and isinstance(other, str):
                result = result + other
            else:
                raise BuildError("unsupported Blueprint addition")
        return result


def tokenize(text):
    # Check gaps too: silently dropping an unknown operator would change the rule.
    result, end = [], 0
    for match in TOKEN.finditer(text):
        if text[end:match.start()].strip():
            raise BuildError(f"unsupported Blueprint syntax: {text[end:match.start()]!r}")
        token = match.group()
        if not token.startswith(("//", "/*")):
            result.append(token)
        end = match.end()
    if text[end:].strip():
        raise BuildError("unsupported Blueprint suffix")
    return result


class Blueprints:
    def __init__(self, roots, source_dirs=None, translation_arch=None):
        self.modules, self.resolved, self.used, self.inherited = {}, {}, {}, {}
        self.roots, self.source_dirs = roots, source_dirs or {}
        self.translation_arch = translation_arch
        self.config_selections = {}
        for root in roots.values():
            for path in sorted(root.rglob("Android.bp")):
                self.index_file(path)

    def index_file(self, path):
        tokens, variables = tokenize(path.read_text()), {}
        i = 0
        while i < len(tokens):
            kind = tokens[i]
            if i + 1 >= len(tokens):
                raise BuildError(f"unparsed Blueprint statement in {path}")
            if tokens[i + 1] in ("=", "+="):
                values = Values(tokens[i + 2:], variables)
                value = values.value()
                variables[kind] = variables.get(kind, []) + value if tokens[i + 1] == "+=" else value
                i += 2 + values.index
                continue
            if tokens[i + 1] != "{":
                raise BuildError(f"unsupported Blueprint top level in {path}: {kind}")
            start, depth = i + 1, 1
            i += 2
            while depth and i < len(tokens):
                depth += (tokens[i] == "{") - (tokens[i] == "}")
                i += 1
            body = tokens[start:i]
            # NDK declarations generate SDK stub/header modules under different
            # internal names; they are not the same-named platform cc_library.
            if kind.startswith("ndk_"):
                continue
            # Only index the top-level name; parse properties lazily for selected modules.
            level = 0
            for n, token in enumerate(body[:-2]):
                level += (token == "{") - (token == "}")
                if level == 1 and token == "name" and body[n + 1] == ":":
                    name = json.loads(body[n + 2])
                    if name in self.modules:
                        raise BuildError(f"ambiguous module {name}: {path}")
                    self.modules[name] = (kind, path, body, copy.deepcopy(variables))
                    break

    def source_directory(self, blueprint):
        return self.source_dirs.get(blueprint, blueprint.parent)

    def canonical(self, name):
        if name.startswith("//"):
            namespace, separator, module = name[2:].partition(":")
            if not separator or any(p in ("", ".", "..") for p in namespace.split("/")):
                raise BuildError(f"unresolved Blueprint namespace: {name}")
            try:
                scope = self.aosp_path(namespace)
            except BuildError as error:
                raise BuildError(f"unresolved Blueprint namespace: {name}") from error
            if module in self.modules and not self.modules[module][1].resolve().is_relative_to(scope.resolve()):
                raise BuildError(f"module outside declared namespace: {name}")
            name = module
        else:
            name = name.removeprefix(":")
        if not re.fullmatch(r"[A-Za-z0-9_.+-]+", name):
            raise BuildError(f"unsafe/unsupported Blueprint module name: {name}")
        return name

    def rule_kind(self, kind):
        declaration = self.modules.get(kind)
        if declaration is None or declaration[0] != "soong_config_module_type":
            return kind
        descriptor = Values(declaration[2], declaration[3]).value()
        if not isinstance(descriptor, dict):
            raise BuildError(f"invalid custom rule descriptor: {kind}")
        # This is the sole compilation-changing Soong config contract needed
        # by the proxies. Do not project arbitrary custom rules to cc_defaults.
        if (descriptor.get("module_type") == "cc_defaults"
                and descriptor.get("config_namespace") == "berberis"
                and descriptor.get("variables") == ["translation_arch"]
                and set(descriptor.get("properties", [])) == {
                    "defaults", "generated_headers", "header_libs", "shared_libs"}
                and set(descriptor) == {"name", "module_type", "config_namespace", "variables", "properties"}):
            return "cc_defaults"
        return kind

    def select_config(self, name, kind, raw):
        if self.rule_kind(kind) == kind:
            return raw
        variable = self.modules.get("translation_arch")
        if variable is None or variable[0] != "soong_config_string_variable":
            raise BuildError("missing Soong translation_arch string variable declaration")
        variable_props = Values(variable[2], variable[3]).value()
        if not isinstance(variable_props, dict):
            raise BuildError("invalid Soong translation_arch variable declaration")
        values = variable_props.get("values", [])
        if not isinstance(values, list) or any(not isinstance(v, str) for v in values):
            raise BuildError("invalid Soong translation_arch values")
        if self.translation_arch not in values:
            raise BuildError(f"explicit supported Soong translation_arch required for {name}: {values}")
        config = raw.get("soong_config_variables", {})
        if not isinstance(config, dict) or set(config) != {"translation_arch"}:
            raise BuildError(f"unsupported Soong variables in {name}")
        branches = config["translation_arch"]
        if not isinstance(branches, dict) or set(branches) - set(values) - {"conditions_default"}:
            raise BuildError(f"unsupported Soong translation_arch branch in {name}")
        allowed = {"defaults", "generated_headers", "header_libs", "shared_libs"}
        if any(not isinstance(branch, dict) or set(branch) - allowed for branch in branches.values()):
            raise BuildError(f"Soong config changes undeclared properties in {name}")
        selected = branches.get(self.translation_arch, branches.get("conditions_default", {}))
        self.config_selections[name] = {"translation_arch": self.translation_arch, "properties": selected}
        return merge({k: v for k, v in raw.items() if k != "soong_config_variables"}, selected)

    def raw_props(self, name, stack=()):
        name = self.canonical(name)
        if name in self.inherited:
            return copy.deepcopy(self.inherited[name])
        if name not in self.modules:
            raise BuildError(f"unresolved Blueprint module {name}; required by {' -> '.join(stack) or 'requested goal'}")
        if name in stack:
            raise BuildError(f"cyclic Blueprint defaults: {stack} -> {name}")
        _kind, path, tokens, variables = self.modules[name]
        try:
            raw = Values(tokens, variables).value()
            if not isinstance(raw, dict):
                raise BuildError("Blueprint module body must be a property map")
            raw = self.select_config(name, _kind, raw)
            for selector, allowed in (("arch", KNOWN_ARCHES), ("target", KNOWN_TARGETS), ("multilib", {"lib32", "lib64"})):
                variants = raw.get(selector, {})
                if not isinstance(variants, dict) or any(not isinstance(v, dict) for v in variants.values()):
                    raise BuildError(f"invalid {selector} selector map")
                if set(variants) - allowed:
                    raise BuildError(f"unsupported {selector} selectors: {sorted(set(variants) - allowed)}")
        except BuildError as error:
            raise BuildError(f"{name} ({path}): {error}") from error
        source_dir = self.source_directory(path)
        def paths(properties):
            for key, value in properties.items():
                if isinstance(value, dict):
                    paths(value)
                elif key in PATH_FIELDS:
                    properties[key] = [p if p.startswith((":", "//")) else str(source_dir / p) for p in value]
                elif key == "main":
                    properties[key] = str(source_dir / value)

        paths(raw)
        result = {}
        for default in raw.get("defaults", []):
            default = self.canonical(default)
            expected_default = "genrule_defaults" if _kind in ("genrule", "genrule_defaults") else "cc_defaults"
            if default in self.modules and self.rule_kind(self.modules[default][0]) != expected_default:
                raise BuildError(f"unsupported defaults module {default} for {name}; expected {expected_default}")
            result = merge(result, self.raw_props(default, stack + (name,)))
        result = merge(result, raw)
        self.inherited[name] = result
        return copy.deepcopy(result)

    def packaging_only_header_kind(self, kind):
        if kind in RULE_FIELDS:
            return kind
        declaration = self.modules.get(kind)
        if declaration is None or declaration[0] != "soong_config_module_type":
            return kind
        descriptor = Values(declaration[2], declaration[3]).value()
        # Only a proven packaging-only extension can be projected to headers.
        # Compilation of the custom type is still unsupported.
        if (not isinstance(descriptor, dict) or descriptor.get("module_type") != "cc_library"
                or descriptor.get("properties") != ["apex_available"]
                or set(descriptor) - {"name", "module_type", "config_namespace", "bool_variables", "properties"}):
            raise BuildError(f"custom header rule may change compilation/headers: {kind}")
        return "cc_library"

    def props(self, name, stack=(), linkage=None, headers_only=False, host=False):
        name = self.canonical(name)
        key = (name, linkage, headers_only, host)
        if key in self.resolved:
            return self.resolved[key]
        raw = self.raw_props(name, stack)
        kind, path, _, _ = self.modules[name]
        # Merge defaults BEFORE applying variants: an arch-specific default must
        # override a module's general scalar, not vice versa.
        selectors = {"arch", "target", "multilib", "static", "shared"}
        selected = {k: v for k, v in raw.items() if k not in selectors}
        if host:
            # Host tools select the `linux` + `x86_64` axes only. Android-only
            # selectors (`bionic`, `android`, `android64`) must not leak into a
            # glibc executable, and `linux_x86_64` is a host selector too.
            selected = merge(selected, raw.get("target", {}).get("linux", {}))
            selected = merge(selected, raw.get("arch", {}).get("x86_64", {}))
            selected = merge(selected, raw.get("multilib", {}).get("lib64", {}))
            selected = merge(selected, raw.get("target", {}).get("linux_x86_64", {}))
        else:
            for axis in OS_AXES:
                selected = merge(selected, raw.get("target", {}).get(axis, {}))
            selected = merge(selected, raw.get("arch", {}).get("x86_64", {}))
            selected = merge(selected, raw.get("multilib", {}).get("lib64", {}))
            for axis in ARCH_AXES:
                selected = merge(selected, raw.get("target", {}).get(axis, {}))
        if kind in ("cc_library_static", "cc_library_shared"):
            linkage = "static" if kind.endswith("_static") else "shared"
        if raw.get("static") or raw.get("shared"):
            if linkage not in ("static", "shared"):
                raise BuildError(f"explicit static/shared variant required: {name}")
            selected = merge(selected, raw.get(linkage, {}))
        # Product/Soong config, nested arch-feature selectors, and unknown
        # properties are intentionally NOT ignored or guessed.
        rule_kind = self.rule_kind(kind)
        if headers_only and rule_kind == kind:
            rule_kind = self.packaging_only_header_kind(kind)
        if rule_kind not in RULE_FIELDS:
            raise BuildError(f"unsupported Blueprint rule {name} ({kind})")
        unsupported = set(selected) - METADATA - RULE_FIELDS[rule_kind]
        if headers_only and rule_kind != kind and self.rule_kind(kind) == kind:
            descriptor = Values(self.modules[kind][2], self.modules[kind][3]).value()
            if not isinstance(descriptor, dict):
                raise BuildError(f"invalid custom rule descriptor: {kind}")
            config = selected.get("soong_config_variables", {})
            if set(config) - set(descriptor.get("bool_variables", [])):
                raise BuildError(f"unknown packaging-only Soong variable in {name}")
            for choice in config.values():
                if not isinstance(choice, dict) or set(choice) - {"apex_available", "conditions_default"}:
                    raise BuildError(f"custom config changes more than packaging: {name}")
                if set(choice.get("conditions_default", {})) - {"apex_available"}:
                    raise BuildError(f"custom default changes more than packaging: {name}")
            unsupported -= {"soong_config_variables", "nocrt", "native_coverage"}
        # Resolving a dependency's exported include paths is not compiling it.
        # Sanitizer/profile settings must block compilation, but do not propagate
        # into consumers of its headers. Header-changing product selectors and
        # all Soong config variables still block.
        if headers_only:
            unsupported -= {"sanitize", "afdo", "llndk", "logtags", "version_script"}
            variables = selected.get("product_variables", {})
            if all(isinstance(value, dict) and set(value) <= {"cflags", "cppflags", "conlyflags", "asflags"}
                   for value in variables.values()):
                unsupported.discard("product_variables")
        if unsupported:
            raise BuildError(f"unsupported properties in {name}: {', '.join(sorted(unsupported))}")
        if rule_kind not in ("cc_defaults", "genrule_defaults"):
            if not host and (selected.get("enabled") is False or selected.get("device_supported") is False):
                raise BuildError(f"Android x86_64 variant disabled: {name}")
            if host and selected.get("host_supported") is not True and kind not in ("cc_library_headers", "filegroup", "genrule"):
                raise BuildError(f"module is not host-supported: {name}")
            if selected.get("compile_multilib", "both") not in ("both", "64", "first"):
                raise BuildError(f"not an x86_64 module: {name}")
        if "include_dirs" in selected:
            selected["include_dirs"] = [str(self.aosp_path(p)) for p in selected["include_dirs"]]
        for default in selected.get("defaults", []):
            self.props(default, stack + (name,), linkage=linkage, headers_only=headers_only, host=host)
        self.used[name] = {"kind": kind, "blueprint": str(path), "blueprint_sha256": BOOT.sha256_file(path),
                           "selected_properties": selected, "linkage": linkage,
                           "usage": "exported headers only" if headers_only else "compile",
                           "source_directory": str(self.source_directory(path))}
        self.resolved[key] = selected
        return selected

    def aosp_path(self, value):
        for project, root in sorted(self.roots.items(), key=lambda item: -len(item[0])):
            if value == project or value.startswith(project + "/"):
                return root / value[len(project):].lstrip("/")
        raise BuildError(f"unresolved AOSP include directory: {value}")

    def files(self, entries, seen=()):
        result = []
        for value in entries:
            if value.startswith((":", "//")):
                name = self.canonical(value)
                if name not in self.modules or self.modules[name][0] != "filegroup":
                    raise BuildError(f"unsupported source reference: {value}")
                if name in seen:
                    raise BuildError(f"cyclic filegroup: {' -> '.join(seen + (name,))}")
                properties = self.props(name)
                files = self.files(properties.get("srcs", []), seen + (name,))
                excluded = set(self.files(properties.get("exclude_srcs", []), seen + (name,)))
                result += [p for p in files if p not in excluded]
            else:
                matches = sorted(glob.glob(value, recursive=True)) if glob.has_magic(value) else [value]
                if not matches or any(not Path(p).is_file() for p in matches):
                    raise BuildError(f"missing Blueprint source: {value}")
                result += matches
        return result


class Runner:
    def __init__(self, root, report, seconds):
        self.root, self.report, self.deadline = root, report, time.monotonic() + seconds
        injected = {"CPATH", "CPLUS_INCLUDE_PATH", "C_INCLUDE_PATH", "OBJC_INCLUDE_PATH", "LIBRARY_PATH",
                    "COMPILER_PATH", "GCC_EXEC_PREFIX", "CCC_OVERRIDE_OPTIONS", "PYTHONPATH", "PYTHONHOME",
                    "CLANG_CONFIG_FILE_SYSTEM_DIR", "CLANG_CONFIG_FILE_USER_DIR", "LD_PRELOAD"}
        self.env = {k: v for k, v in os.environ.items() if k not in injected}
        self.env.update(PYTHONDONTWRITEBYTECODE="1", PYTHONHASHSEED="0", TMPDIR=str(root / "tmp"))

    @staticmethod
    def limits():
        resource.setrlimit(resource.RLIMIT_AS, (MEMORY, MEMORY))
        resource.setrlimit(resource.RLIMIT_CPU, (300, 300))
        resource.setrlimit(resource.RLIMIT_FSIZE, (FILE_LIMIT, FILE_LIMIT))
        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
        # Clang 22's template instantiation for the ARM64 heavy optimizer
        # needs more than the default 8 MiB stack at -O0.
        resource.setrlimit(resource.RLIMIT_STACK, (64 * 1024 * 1024, 64 * 1024 * 1024))

    def run(self, argv, label, env=None):
        remaining = min(300, self.deadline - time.monotonic())
        if remaining <= 0:
            raise BuildError("total build wall-time budget exhausted")
        log = self.root / "logs" / f"{len(self.report['commands']):03d}.log"
        record = {"label": label, "argv": [str(p) for p in argv], "cwd": str(self.root), "log": str(log)}
        self.report["commands"].append(record)
        print(label, flush=True)
        started = time.monotonic()
        with log.open("wb") as output:
            process = subprocess.Popen(record["argv"], cwd=self.root, env=env or self.env,
                                       stdout=output, stderr=subprocess.STDOUT,
                                       preexec_fn=self.limits, start_new_session=True)  # noqa: PLW1509 (single-threaded driver)
            try:
                record["returncode"] = process.wait(timeout=remaining)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                record.update(returncode=process.returncode, timed_out=True)
        record["children_peak_rss_kib"] = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
        record["parent_peak_rss_kib"] = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        if (record["children_peak_rss_kib"] + record["parent_peak_rss_kib"]) * 1024 > 6_000_000_000:
            raise BuildError("observed parent + child high-water RSS exceeds 6 GB")
        record["elapsed_seconds"] = round(time.monotonic() - started, 3)
        if record.get("timed_out"):
            raise BuildError(f"{label} timed out after {remaining:.2f}s; log: {log}")
        if record["returncode"]:
            with log.open("rb") as stream:
                stream.seek(max(0, log.stat().st_size - 12000))
                detail = stream.read().decode(errors="replace")
            raise BuildError(f"{label} failed ({record['returncode']}):\n{shlex.join(record['argv'])}\n{detail}")
        return record


def describe_platform_sources(model):
    """Audit real declarations, without pretending unresolved defaults were built."""
    artifacts = {a.name: a for a in EXTRAS}
    result = {}

    def visit(module, blueprint, declarations, unresolved):
        if module in declarations:
            return
        if module not in model.modules or (model.modules[module][1] != blueprint and model.modules[module][0] != "cc_defaults"):
            unresolved.append(module)
            return
        kind, _, tokens, variables = model.modules[module]
        props = Values(tokens, variables).value()
        if not isinstance(props, dict):
            raise BuildError(f"platform provider must have a property map: {module}")
        declarations[module] = {"kind": kind, "properties": props,
                                "blueprint": str(model.modules[module][1]),
                                "blueprint_sha256": BOOT.sha256_file(model.modules[module][1])}
        for default in props.get("defaults", []):
            visit(default, blueprint, declarations, unresolved)

    for name, specification in PLATFORM_SHARED.items():
        if not specification.get("artifact"):
            # Verified link input only; its Blueprint packaging is not needed
            # because no module's exported headers come from this library.
            result[name] = {"project": "system/core", "revision": "", "subtree": "",
                            "git_blob": "", "blueprint": None, "blueprint_sha256": None,
                            "declarations": {}, "unresolved_source_defaults": [],
                            "resolution": "import verified CP2A.260605.012 platform shared library; not source-built here"}
            continue
        artifact = artifacts[specification["artifact"]]
        blueprint = ROOT / artifact.destination
        declarations, unresolved = {}, []
        visit(name, blueprint, declarations, unresolved)
        if name not in declarations:
            raise BuildError(f"pinned platform provider declaration missing: {name}")
        result[name] = {
            "project": artifact.project, "revision": artifact.revision, "subtree": artifact.subtree,
            "git_blob": artifact.blob, "blueprint": str(blueprint), "blueprint_sha256": BOOT.sha256_file(blueprint),
            "declarations": declarations, "unresolved_source_defaults": list(dict.fromkeys(unresolved)),
            "resolution": "import verified CP2A.260605.012 platform shared library; not source-built here",
            "runtime_relative": specification["relative"],
            "note": "Source pin identifies the provider, not the exact source revision used to produce the GSI binary.",
        }
    return result


class PlatformImports:
    """Read-only import gate, never a sysroot search or an NDK-stub fallback.

    The completed extractor manifest is the provenance boundary. Each imported
    library must have its own hash in files, not just an archive identity string.
    This checks link inputs, not their transitive loader closure or executability.
    """
    def __init__(self, root, runner, records):
        self.root, self.runner, self.records = Path(root), runner, records

    def checked_file(self, relative, files):
        expected = files.get(relative)
        if not isinstance(expected, str) or not re.fullmatch(r"[0-9a-f]{64}", expected):
            raise BuildError(f"completed runtime manifest must pin files[{relative!r}]; no unrecorded platform binary accepted")
        root = self.root.resolve()
        path = self.root / relative
        resolved = path.resolve(strict=True)
        if not resolved.is_relative_to(root) or not resolved.is_file():
            raise BuildError(f"platform file/link escapes the extracted runtime: {relative}")
        if BOOT.sha256_file(resolved) != expected:
            raise BuildError(f"platform file hash mismatch: {relative}")
        return resolved

    def inspect(self, name):
        specifications = {**PLATFORM_SHARED, **PLATFORM_PRIVATE}
        if name not in specifications:
            raise BuildError(f"unsupported platform import: {name}")
        if self.root.is_symlink():
            raise BuildError("symlink platform root refused")
        manifest_path = self.root / RUNTIME_MANIFEST
        if not manifest_path.is_file():
            raise BuildError(f"API37 platform extraction not ready: missing completed {manifest_path}; not polling or using a staging tree")
        if not manifest_path.resolve().is_relative_to(self.root.resolve()):
            raise BuildError("runtime manifest escapes extracted tree")
        digest = BOOT.sha256_file(manifest_path)
        manifest = json.loads(manifest_path.read_text())
        if not isinstance(manifest, dict):
            raise BuildError("runtime manifest must be an object")
        expected = {"tool": "PenguDroid API-37 runtime preparation", "schema": 1, "build": GSI_BUILD,
                    "archive_sha256": GSI_SHA256, "sdk": 37, "release": "17", "architecture": "x86_64"}
        if any(manifest.get(key) != value for key, value in expected.items()):
            raise BuildError("platform manifest must attest Android17/API37/x86_64 CP2A.260605.012 and its pinned GSI SHA256; no fallback")
        files = manifest.get("files")
        if not isinstance(files, dict):
            raise BuildError("runtime manifest files must be a hash map")
        prop_path = self.checked_file("system/build.prop", files)
        props = {}
        for line in prop_path.read_text().splitlines():
            if line.strip().startswith("#") or "=" not in line:
                continue
            key, value = (part.strip() for part in line.split("=", 1))
            if key in ("ro.build.id", "ro.build.version.sdk", "ro.build.version.release"):
                if key in props:
                    raise BuildError(f"duplicate platform build property: {key}")
                props[key] = value
        if props != {"ro.build.id": GSI_BUILD, "ro.build.version.sdk": "37", "ro.build.version.release": "17"}:
            raise BuildError("platform build.prop does not match the pinned API37 GSI")
        specification = specifications[name]
        relative = specification["relative"]
        path = self.checked_file(relative, files)
        verify_elf(path, 3)
        record = self.runner.run([str(LLVM / "llvm-readelf"), "--wide", "--dynamic", "--dyn-syms", str(path)],
                                 "verify platform import " + name)
        text = Path(record["log"]).read_text()
        sonames = re.findall(r"\(SONAME\).*?\[([^\]]+)\]", text)
        if sonames != [name + ".so"]:
            raise BuildError(f"incorrect platform SONAME for {name}: {sonames}")
        needed = re.findall(r"\(NEEDED\).*?\[([^\]]+)\]", text)
        if "__ndk1" in text or "GLIBC_" in text or "GLIBCXX_" in text or any("libstdc++" in n or "libc++_shared" in n for n in needed):
            raise BuildError(f"host/NDK C++ runtime detected in platform import {name}")
        exports = set()
        for line in text.splitlines():
            fields = line.split()
            if (len(fields) >= 8 and fields[0].endswith(":") and fields[3] == "FUNC"
                    and fields[4] in ("GLOBAL", "WEAK") and fields[5] in ("DEFAULT", "PROTECTED") and fields[6] != "UND"):
                exports.add(fields[7].split("@", 1)[0])
        missing = set(specification["required_exports"]) - exports
        if missing:
            raise BuildError(f"platform import {name} is missing defined exports: {sorted(missing)}")
        if (BOOT.sha256_file(path) != files[relative] or BOOT.sha256_file(manifest_path) != digest
                or BOOT.sha256_file(prop_path) != files["system/build.prop"]):
            raise BuildError("platform inputs changed during inspection")
        result = {"status": "verified link input only", "path": str(path), "relative": relative,
                  "bytes": path.stat().st_size, "sha256": files[relative], "runtime_manifest_sha256": digest,
                  "soname": sonames[0], "needed": needed, "checked_exports": specification["required_exports"],
                  "needed_closure_verified": False, "runtime_tested": False}
        self.records[name] = result
        return path

    def probe(self):
        for name in PLATFORM_SHARED:
            try:
                self.inspect(name)
            except (BuildError, OSError, ValueError) as error:
                self.records[name] = {"status": "blocked", "blocker": str(error), "runtime_tested": False}
        return all(r["status"] == "verified link input only" for r in self.records.values())


def proxy_support_evidence(model):
    source = model.aosp_path("frameworks/libs/binary_translation/guest_loader/arm64/guest_loader_arch.cc")
    prefix = re.search(r'\bkProxyPrefix\s*=\s*"([^"]+)"', source.read_text())
    if prefix is None or prefix.group(1) != "libberberis_proxy_":
        raise BuildError("pinned ARM64 loader has an unexpected proxy prefix")
    defaults = model.modules["liblog.defaults"]
    log_props = Values(defaults[2], defaults[3]).value()
    if not isinstance(log_props, dict) or log_props.get("native_bridge_supported") is not True:
        raise BuildError("pinned liblog no longer declares ordinary native-bridge support")
    return {"proxy_prefix": prefix.group(1), "prefix_source": str(source),
            "prefix_source_sha256": BOOT.sha256_file(source),
            "liblog": {
                "guest_stub_module_present": "libnative_bridge_guest_liblog" in model.modules,
                "native_proxy_module_present": "libberberis_proxy_liblog" in model.modules,
                "blueprint": str(defaults[1]), "blueprint_sha256": BOOT.sha256_file(defaults[1]),
                "native_bridge_supported": True,
                "note": "Pinned liblog is an ordinary guest library. No stub/proxy is invented; build/stage its guest closure separately.",
            }}


def dynamic_evidence(runner, path, label):
    verify_elf(path, 3)
    record = runner.run([str(LLVM / "llvm-readelf"), "--wide", "--dynamic", "--dyn-syms", str(path)], label)
    text = Path(record["log"]).read_text()
    symbols = []
    for line in text.splitlines():
        fields = line.split()
        if len(fields) >= 8 and re.fullmatch(r"\d+:", fields[0]):
            symbols.append({"name": fields[7], "type": fields[3], "binding": fields[4],
                            "visibility": fields[5], "defined": fields[6] != "UND"})
    return {"soname": re.findall(r"\(SONAME\).*?\[([^\]]+)\]", text),
            "needed": re.findall(r"\(NEEDED\).*?\[([^\]]+)\]", text),
            "undefined": [s["name"] for s in symbols if not s["defined"] and s["name"]],
            "init_proxy_library": any(s["name"] == "InitProxyLibrary" and s["defined"]
                                      and s["type"] == "FUNC" and s["binding"] == "GLOBAL"
                                      and s["visibility"] == "DEFAULT" for s in symbols),
            "log": record["log"]}


class SharedReuse:
    """Explicit, hash-checked shared inputs from a successful owned build.

    Never import archives/objects implicitly or fall back to rebuilding the
    translator if an explicitly requested reuse report is invalid.
    """
    def __init__(self, path, model, runner, manifest_sha256):
        self.path, self.model, self.runner = Path(path), model, runner
        if self.path.is_symlink() or not self.path.resolve().is_relative_to(ROOT.resolve()):
            raise BuildError("reuse report must be inside the owned nativebridge build root")
        self.path = self.path.resolve()
        self.report_sha256 = BOOT.sha256_file(self.path)
        self.report = json.loads(self.path.read_text())
        if (not isinstance(self.report, dict) or any(self.report.get(k) != v for k, v in OWNER.items())
                or self.report.get("requested_goals_passed") is not True
                or self.report.get("dependencies_manifest_sha256") != manifest_sha256):
            raise BuildError("reuse report must attest a successful build with the current verified dependencies")
        if not isinstance(self.report.get("run"), str) or not isinstance(self.report.get("modules"), dict):
            raise BuildError("reuse report is missing run/module provenance")
        self.run = Path(self.report["run"])
        if (self.path != self.run / "report.json"
                or not self.run.resolve().is_relative_to(ROOT.resolve())):
            raise BuildError("reuse report run/path mismatch")
        self.artifacts = {}
        artifacts, dependencies = self.report.get("artifacts", []), self.report.get("dependency_artifacts", [])
        if not isinstance(artifacts, list) or not isinstance(dependencies, list):
            raise BuildError("reuse report artifacts must be lists")
        for artifact in artifacts + dependencies:
            if not isinstance(artifact, dict):
                raise BuildError("reuse artifact must be an object")
            if artifact.get("kind") != "cc_library_shared" or artifact.get("host"):
                continue
            if (not isinstance(artifact.get("module"), str) or not isinstance(artifact.get("output"), str)
                    or not isinstance(artifact.get("bytes"), int) or not 0 < artifact["bytes"] <= FILE_LIMIT
                    or not isinstance(artifact.get("sha256"), str)
                    or not re.fullmatch(r"[0-9a-f]{64}", artifact["sha256"])):
                raise BuildError("reuse shared artifact is missing file/hash provenance")
            name = model.canonical(artifact["module"])
            if name in self.artifacts:
                raise BuildError(f"duplicate shared artifact in reuse report: {name}")
            self.artifacts[name] = artifact

    def inspect(self, name):
        artifact = copy.deepcopy(self.artifacts[name])
        output = Path(artifact["output"])
        if (output != self.run / "lib" / (name + ".so")
                or not output.resolve().is_relative_to(self.run.resolve())):
            raise BuildError(f"reused shared artifact path mismatch/escape: {name}")
        module = self.report.get("modules", {}).get(name, {})
        current = self.model.props(name, linkage="shared")
        blueprint = self.model.modules[name][1]
        if (not isinstance(module, dict) or module.get("blueprint_sha256") != BOOT.sha256_file(blueprint)
                or module.get("selected_properties") != current):
            raise BuildError(f"reused module Blueprint/properties changed: {name}")
        if output.stat().st_size != artifact["bytes"] or BOOT.sha256_file(output) != artifact["sha256"]:
            raise BuildError(f"reused shared artifact changed: {output}")
        evidence = dynamic_evidence(self.runner, output, "verify reused shared " + name)
        if evidence["soname"] != [output.name] or BOOT.sha256_file(self.path) != self.report_sha256:
            raise BuildError(f"reused shared SONAME/report changed: {name}")
        artifact.update(reused_from_report=str(self.path), reuse_report_sha256=self.report_sha256,
                        dynamic=evidence, needed_closure_verified=False, runtime_tested=False)
        self.runner.report.setdefault("reused_shared", {})[name] = artifact
        return artifact


class NativeBuild:
    def __init__(self, model, runner, sysroot, platform_imports=None, shared_reuse=None):
        self.model, self.runner, self.root, self.sysroot = model, runner, runner.root, sysroot
        self.platform_imports, self.shared_reuse = platform_imports, shared_reuse
        self.generated, self.built, self.building = {}, {}, []

    def python_tool(self, name, seen=()):
        if name in seen:
            raise BuildError(f"cyclic Python tool dependency: {name}")
        name = self.model.canonical(name)
        if name not in self.model.modules:
            self.model.props(name)
        kind = self.model.modules[name][0]
        if kind not in ("python_binary_host", "python_library_host"):
            raise BuildError(f"unimplemented host codegen tool {name} ({kind}); its Blueprint must be built, not omitted")
        props = self.model.props(name)
        paths = [str(Path(p).parent) for p in self.model.files(props.get("srcs", []))]
        for dep in props.get("libs", []):
            _, more = self.python_tool(dep, seen + (name,))
            paths += more
        main = props.get("main")
        if main is not None and main not in self.model.files(props.get("srcs", [])):
            raise BuildError(f"Python tool main must be a declared source: {name}")
        return main, paths

    def generate(self, name):
        name = self.model.canonical(name)
        if name in self.generated:
            return self.generated[name]
        props = self.model.props(name)
        if self.model.modules[name][0] != "genrule" or props.get("tool_files"):
            raise BuildError(f"unsupported generator rule {name}")
        directory = self.root / "gen" / name
        directory.mkdir(parents=True)
        outputs = [directory / p for p in props.get("out", [])]
        for path in outputs:
            if not path.is_relative_to(directory) or ".." in path.parts:
                raise BuildError("unsafe generated output path")
            path.parent.mkdir(parents=True, exist_ok=True)
        inputs = self.model.files(props.get("srcs", []))
        tools, python_paths = {}, []
        for tool in props.get("tools", []):
            kind = self.model.modules[self.model.canonical(tool)][0]
            if kind == "cc_binary_host":
                tools[tool] = shlex.join([str(self.build(tool, host=True)["output"])])
                continue
            main, paths = self.python_tool(tool)
            if main is None:
                raise BuildError(f"missing Python tool main: {tool}")
            tools[tool] = shlex.join([sys.executable, "-B", main])
            python_paths += paths
        def substitute(match):
            key = match.group(1)
            if key == "out":
                return shlex.join([str(p) for p in outputs])
            if key == "in":
                return shlex.join(inputs)
            if key.startswith("location ") and key[9:] in tools:
                return tools[key[9:]]
            raise BuildError(f"unsupported genrule substitution: {key}")
        command = re.sub(r"\$\(([^)]+)\)", substitute, props["cmd"])
        if any(c in command for c in "|;&<>`$"):
            raise BuildError(f"unsupported shell genrule: {name}: {props['cmd']}")
        argv = shlex.split(command)
        prefixes = [shlex.split(value) for value in tools.values()]
        if not any(argv[:len(prefix)] == prefix for prefix in prefixes):
            raise BuildError(f"genrule must invoke a declared tool directly: {name}")
        if not outputs:
            raise BuildError(f"genrule has no declared outputs: {name}")
        env = {**self.runner.env, "PYTHONPATH": os.pathsep.join(dict.fromkeys(python_paths))}
        self.runner.run(argv, "generate " + name, env)
        if not all(p.is_file() for p in outputs):
            raise BuildError(f"generator did not produce declared outputs: {name}")
        self.generated[name] = directory
        return directory

    def exports(self, name, seen=(), linkage="static", host=False):
        name = self.model.canonical(name)
        if name in seen:
            raise BuildError(f"cyclic header reexport: {' -> '.join(seen + (name,))}")
        props = self.model.props(name, seen, linkage=linkage, headers_only=True, host=host)
        for field, deps in (("header_libs", "export_header_lib_headers"), ("static_libs", "export_static_lib_headers"),
                            ("shared_libs", "export_shared_lib_headers"), ("generated_headers", "export_generated_headers")):
            declared = set(props.get(field, []))
            if field == "static_libs":
                declared.update(props.get("whole_static_libs", []))
            if set(props.get(deps, [])) - declared:
                raise BuildError(f"undeclared reexport in {name}: {deps}")
        result = list(props.get("export_include_dirs", []))
        for key in ("export_header_lib_headers", "export_static_lib_headers", "export_shared_lib_headers"):
            for dep in props.get(key, []):
                result += self.exports(dep, seen + (name,), linkage="shared" if key == "export_shared_lib_headers" else "static", host=host)
        result += [str(self.generate(gen)) for gen in props.get("export_generated_headers", [])]
        return list(dict.fromkeys(result))

    def compile_command(self, props, source, obj, includes, host=False):
        suffix = Path(source).suffix
        cpp = suffix in (".cc", ".cpp", ".cxx")
        if suffix not in (".c", ".S") and not cpp:
            raise BuildError(f"unsupported source language: {source}")
        if host:
            argv = [str(LLVM / ("clang++" if cpp else "clang")), "-fPIC", "-O0", "-g0"]
            if cpp:
                argv += ["-std=" + props.get("cpp_std", "gnu++23"), "-fno-exceptions", "-fno-rtti",
                         "-nostdinc++", "-isystem", str(HOST_LIBCXX / "include/c++/v1")]
            for directory in dict.fromkeys(includes):
                if not Path(directory).is_dir():
                    raise BuildError(f"missing exported include directory: {directory}")
                argv += ["-I", directory]
            argv += ["-I", str(ROOT / "extra/host-support/include")]
            argv += list(PLATFORM_FLAGS) + list(props.get("cflags", [])) + list(props.get("cppflags", []))
            return argv + list(TRAILING_FLAGS) + ["-c", str(source), "-o", str(obj)]
        platform = DEPS / "toolchain/libcxx/android/x86_64"
        argv = [str(LLVM / ("clang++" if cpp else "clang")), "--no-default-config",
                "--target=x86_64-linux-android37", f"--sysroot={self.sysroot}", "-fintegrated-cc1", "-fPIC"]
        if cpp:
            argv += ["-nostdinc++", "-isystem", str(platform / "include/c++/v1"),
                     "-isystem", str(DEPS / "toolchain/libcxx/include/c++/v1")]
        argv += ["-isystem", str(self.sysroot / "usr/include/x86_64-linux-android")]
        for directory in dict.fromkeys(includes):
            if not Path(directory).is_dir():
                raise BuildError(f"missing exported include directory: {directory}")
            argv += ["-I", directory]
        argv += list(PLATFORM_FLAGS) + list(props.get("cflags", []))
        if suffix == ".S":
            argv += props.get("asflags", [])
        elif suffix == ".c":
            argv += ["-std=" + props.get("c_std", "gnu17")] + props.get("conlyflags", [])
        else:
            argv += ["-std=" + props.get("cpp_std", "gnu++23"), "-fno-exceptions"] + props.get("cppflags", [])
            argv += ["-frtti" if props.get("rtti") else "-fno-rtti"]
        return argv + list(TRAILING_FLAGS) + ["-O0", "-g0", "-fno-lto", "-c", str(source), "-o", str(obj)]

    def build(self, name, linkage=None, host=False):
        name = self.model.canonical(name)
        if name not in self.model.modules:
            self.model.props(name, tuple(n for n, _ in self.building))
        declared = self.model.modules[name][0]
        if host:
            key = (name, "host")
            if key in self.built:
                return self.built[key]
            if key in self.building:
                raise BuildError(f"cyclic native dependency: {self.building} -> {key}")
            self.building.append(key)
            try:
                result = self._build(name, "static", host=True)
                self.built[key] = result
            finally:
                self.building.pop()
            if self.building:
                self.runner.report.setdefault("dependency_artifacts", []).append(result)
            return result
        if declared in ("cc_library_static", "cc_library_shared"):
            required = "static" if declared.endswith("_static") else "shared"
            if linkage is not None and linkage != required:
                raise BuildError(f"requested {linkage} variant unavailable: {name} ({declared})")
            linkage = required
        if linkage not in ("static", "shared"):
            raise BuildError(f"explicit static/shared build variant required: {name} ({declared})")
        key = (name, linkage)
        if linkage == "shared" and self.shared_reuse is not None:
            if name in self.shared_reuse.artifacts:
                return self.shared_reuse.inspect(name)
            if name == "libberberis_arm64":
                raise BuildError("reuse report lacks libberberis_arm64; refusing an implicit full translator rebuild")
        if key in self.built:
            return self.built[key]
        if key in self.building:
            raise BuildError(f"cyclic native dependency: {self.building} -> {key}")
        self.building.append(key)
        try:
            result = self._build(name, linkage)
            self.built[key] = result
        finally:
            self.building.pop()
        if self.building:
            self.runner.report.setdefault("dependency_artifacts", []).append(result)
        return result

    def host_log_shim(self):
        """Compile the tiny glibc implementation of the two liblog entry points.

        The Android bridge's header/liblog imports are logging only; a host tool
        cannot link the Bionic `liblog.so`. The shim writes fatal messages to
        stderr and aborts, which is exactly the contract `LOG_ALWAYS_FATAL_IF`
        needs from a build-time generator.
        """
        directory = ROOT / "extra/host-support"
        include = directory / "include" / "android" / "log.h"
        source = directory / "log_shim.cc"
        include.parent.mkdir(parents=True, exist_ok=True)
        include.write_text(
            "#pragma once\n"
            "#include <stdarg.h>\n"
            "#ifdef __cplusplus\nextern \"C\" {\n#endif\n"
            "enum { ANDROID_LOG_UNKNOWN = 0, ANDROID_LOG_DEFAULT, ANDROID_LOG_VERBOSE,\n"
            "       ANDROID_LOG_DEBUG, ANDROID_LOG_INFO, ANDROID_LOG_WARN,\n"
            "       ANDROID_LOG_ERROR, ANDROID_LOG_FATAL, ANDROID_LOG_SILENT };\n"
            "int __android_log_print(int prio, const char* tag, const char* fmt, ...);\n"
            "int __android_log_vprint(int prio, const char* tag, const char* fmt, va_list ap);\n"
            "int __android_log_write(int prio, const char* tag, const char* text);\n"
            "void __android_log_assert(const char* cond, const char* tag, const char* fmt, ...);\n"
            "#ifdef __cplusplus\n}\n#endif\n")
        source.write_text(
            "#include <cstdarg>\n#include <cstdio>\n#include <cstdlib>\n"
            "static void emit(int prio, const char* tag, const char* fmt, va_list ap) {\n"
            "  std::fprintf(stderr, \"[berberis-host %d %s] \", prio, tag ? tag : \"?\");\n"
            "  std::vfprintf(stderr, fmt, ap);\n  std::fputc('\\n', stderr);\n}\n"
            "extern \"C\" int __android_log_print(int prio, const char* tag, const char* fmt, ...) {\n"
            "  va_list ap; va_start(ap, fmt); emit(prio, tag, fmt, ap); va_end(ap); return 0;\n}\n"
            "extern \"C\" int __android_log_vprint(int prio, const char* tag, const char* fmt, va_list ap) {\n"
            "  emit(prio, tag, fmt, ap); return 0;\n}\n"
            "extern \"C\" int __android_log_write(int prio, const char* tag, const char* text) {\n"
            "  std::fprintf(stderr, \"[berberis-host %d %s] %s\\n\", prio, tag ? tag : \"?\", text ? text : \"\"); return 0;\n}\n"
            "extern \"C\" void __android_log_assert(const char* cond, const char* tag, const char* fmt, ...) {\n"
            "  va_list ap; va_start(ap, fmt); emit(6, tag, fmt, ap); va_end(ap);\n"
            "  if (cond) std::fprintf(stderr, \"assert: %s\\n\", cond);\n  std::abort();\n}\n")
        output = directory / "log_shim.o"
        if not output.is_file() or output.stat().st_mtime < source.stat().st_mtime:
            self.runner.run([str(LLVM / "clang++"), "-O0", "-g0", "-fPIC",
                             "-nostdinc++", "-isystem", str(HOST_LIBCXX / "include/c++/v1"),
                             "-c", str(source), "-o", str(output)],
                            "compile host log shim")
        return output

    def _build_host(self, name, declared, blueprint, props):
        if declared == "cc_binary_host":
            kind = "cc_binary_host"
            output = self.root / "host" / "bin" / name
        elif declared in ("cc_library_static", "cc_library"):
            kind = "cc_library_static"
            output = self.root / "host" / "lib" / (name + ".a")
        else:
            raise BuildError(f"unsupported host rule: {name} ({declared})")
        output.parent.mkdir(parents=True, exist_ok=True)
        includes = [str(ROOT / "extra/platform-headers"),
                    str(DEPS / "sources/logging/liblog/include"),
                    str(self.model.source_directory(blueprint))]
        includes += props.get("export_include_dirs", []) + props.get("local_include_dirs", []) + props.get("include_dirs", [])
        for key in ("header_libs", "static_libs", "whole_static_libs"):
            for dep in props.get(key, []):
                includes += self.exports(dep, (name,), linkage="static", host=True)
        includes += [str(self.generate(gen)) for gen in props.get("generated_headers", [])]
        sources = self.model.files(props.get("srcs", []))
        for generator in props.get("generated_sources", []):
            directory = self.generate(generator)
            sources += [str(directory / out) for out in self.model.props(generator)["out"]]
        excluded = set(self.model.files(props.get("exclude_srcs", [])))
        sources = [s for s in sources if s not in excluded]
        objects = []
        output_dir = self.root / "host" / "obj" / name
        output_dir.mkdir(parents=True, exist_ok=True)
        for number, source in enumerate(sources):
            obj = output_dir / f"{number:03d}-{Path(source).stem}.o"
            self.runner.run(self.compile_command(props, source, obj, includes, host=True),
                            f"host compile {name}: {Path(source).name}")
            verify_elf(obj, 1)
            objects.append(obj)
        whole = [self.build(dep, "static", host=True) for dep in dict.fromkeys(props.get("whole_static_libs", []))]
        static = [self.build(dep, "static", host=True) for dep in dict.fromkeys(props.get("static_libs", []))]
        shared = [dep for dep in props.get("shared_libs", []) if dep != "liblog"]
        if shared:
            raise BuildError(f"host tool links unsupported shared libraries: {name}: {shared}")
        if kind == "cc_library_static":
            operation = "qcsDL" if whole else "rcsD"
            self.runner.run([str(LLVM / "llvm-ar"), operation, str(output), *map(str, objects),
                             *(archive["output"] for archive in whole)], "host archive " + name)
        else:
            if props.get("ldflags"):
                raise BuildError(f"host linker flags are not implemented: {name}: {props['ldflags']}")
            argv = [str(LLVM / "clang++"), "-O0", "-g0", "-nostdlib++", "-o", str(output), *map(str, objects),
                    "-Wl,--start-group", *(archive["output"] for archive in whole),
                    *(archive["output"] for archive in static), str(self.host_log_shim()),
                    "-Wl,--end-group", str(HOST_LIBCXX / "lib/x86_64-linux-gnu/libc++.a"),
                    str(HOST_LIBCXX / "lib/x86_64-linux-gnu/libc++abi.a"),
                    "-l:libunwind.so.8", "-pthread", "-ldl", "-lm"]
            self.runner.run(argv, "host link " + name)
            verify_elf(output, 3)
        result = {"module": name, "kind": kind, "host": True, "objects": [str(p) for p in objects],
                  "output": str(output), "bytes": output.stat().st_size,
                  "sha256": BOOT.sha256_file(output),
                  "static_archives": [{"module": a["module"], "output": a["output"]} for a in static + whole]}
        return result

    def _build(self, name, linkage, host=False):
        props = self.model.props(name, linkage=linkage, host=host)
        declared, blueprint, _, _ = self.model.modules[name]
        if host:
            return self._build_host(name, declared, blueprint, props)
        if declared not in ("cc_library_static", "cc_library_shared", "cc_library"):
            raise BuildError(f"unsupported native library rule: {name} ({declared})")
        kind = "cc_library_" + linkage
        if props.get("sdk_version") not in (None, ""):
            raise BuildError(f"SDK module compilation is not the API37 platform variant: {name}")
        if props.get("stl") not in (None, "libc++", "none"):
            raise BuildError(f"non-default STL not implemented for compilation: {name}")
        # `system_shared_libs: []` is how linker-embedded C libraries such as
        # libz avoid a libc dependency. It only matters when the final shared
        # object is linked; static archives here carry no such reference.
        if "system_shared_libs" in props and linkage == "shared":
            raise BuildError(f"non-default system libraries not implemented for a shared build: {name}")
        includes = [str(ROOT / "extra/platform-headers"), str(DEPS / "sources/logging/liblog/include"),
                    str(self.model.source_directory(blueprint))] + props.get("export_include_dirs", []) + props.get("local_include_dirs", []) + props.get("include_dirs", [])
        for key in ("header_libs", "static_libs", "whole_static_libs", "shared_libs"):
            for dep in props.get(key, []):
                # liblog is a verified link input with no source declaration in
                # this slice; its public `log/log.h` headers are already on the
                # include path. The other platform imports (`libandroid`,
                # `libdl_android`) do export headers consumers compile against.
                if key == "shared_libs" and self.model.canonical(dep) == "liblog":
                    continue
                includes += self.exports(dep, (name,), linkage="shared" if key == "shared_libs" else "static")
        includes += [str(self.generate(gen)) for gen in props.get("generated_headers", [])]
        sources = self.model.files(props.get("srcs", []))
        for generator in props.get("generated_sources", []):
            directory = self.generate(generator)
            sources += [str(directory / output) for output in self.model.props(generator)["out"]]
        excluded = set(self.model.files(props.get("exclude_srcs", [])))
        sources = [s for s in sources if s not in excluded]
        platform = DEPS / "toolchain/libcxx/android/x86_64"

        objects = []
        output_dir = self.root / "obj" / f"{name}-{linkage}"
        output_dir.mkdir(parents=True, exist_ok=True)
        for number, source in enumerate(sources):
            obj = output_dir / f"{number:03d}-{Path(source).stem}.o"
            argv = self.compile_command(props, source, obj, includes)
            self.runner.run(argv, f"compile {name}: {Path(source).name}")
            verify_elf(obj, 1)
            objects.append(obj)
        result = {"module": name, "objects": [str(p) for p in objects], "kind": kind}
        output = self.root / "lib" / (name + (".a" if kind == "cc_library_static" else ".so"))
        whole = [self.build(dep, "static") for dep in dict.fromkeys(props.get("whole_static_libs", []))]
        result["whole_static_archives"] = [{k: archive[k] for k in ("module", "output", "sha256")} for archive in whole]
        result["static_link_requirements"] = list(dict.fromkeys(
            props.get("static_libs", []) + [dep for archive in whole for dep in archive["static_link_requirements"]]))
        result["shared_link_requirements"] = list(dict.fromkeys(
            props.get("shared_libs", []) + [dep for archive in whole for dep in archive["shared_link_requirements"]]))
        if kind == "cc_library_static":
            # qL flattens archives and preserves duplicate member basenames; r
            # would silently replace them. Never put nested .a files in an .a.
            operation = "qcsDL" if whole else "rcsD"
            self.runner.run([str(LLVM / "llvm-ar"), operation, str(output), *map(str, objects),
                             *(archive["output"] for archive in whole)], "archive " + name)
        else:
            ldflags = []
            for flag in props.get("ldflags", []):
                if not re.fullmatch(r"-Wl,--wrap=[A-Za-z_][A-Za-z0-9_]*", flag):
                    raise BuildError(f"unsupported shared linker flags in {name}: {props['ldflags']}; no flags discarded")
                ldflags.append(flag.removeprefix("-Wl,"))
            static, linked, visiting = [], set(), set()
            shared_names = list(result["shared_link_requirements"])
            def collect_static(dep):
                dep = self.model.canonical(dep)
                if dep in visiting:
                    raise BuildError(f"cyclic static link requirements at {dep}")
                if dep in linked:
                    return
                visiting.add(dep)
                archive = self.build(dep, "static")
                static.append(archive)
                shared_names.extend(archive["shared_link_requirements"])
                for child in archive["static_link_requirements"]:
                    collect_static(child)
                visiting.remove(dep)
                linked.add(dep)
            for dep in result["static_link_requirements"]:
                collect_static(dep)
            shared = []
            for dep in dict.fromkeys(shared_names):
                dep = self.model.canonical(dep)
                if dep in PLATFORM_SHARED:
                    if self.platform_imports is None:
                        raise BuildError(f"verified platform import required for {dep}; no stub fallback")
                    shared.append(str(self.platform_imports.inspect(dep)))
                else:
                    shared.append(self.build(dep, "shared")["output"])
            result["static_archives"] = [{k: archive[k] for k in ("module", "output", "sha256")} for archive in static]
            result["shared_inputs"] = shared
            libdir = self.sysroot / "usr/lib/x86_64-linux-android/37"
            system_libc = "-lc"
            if name.startswith("libberberis_proxy_"):
                if self.platform_imports is None:
                    raise BuildError(f"verified platform libc required for {name}; private APIs cannot use NDK stubs")
                system_libc = str(self.platform_imports.inspect("libc"))
                result["platform_libc"] = system_libc
            argv = [str(LLVM / "ld.lld"), "--threads=1", "-m", "elf_x86_64", "-shared", "--no-undefined",
                    "-z", "max-page-size=16384", "--soname=" + output.name, *ldflags]
            for script in props.get("linker_scripts", []):
                argv += ["-T", script]
            argv += [str(libdir / "crtbegin_so.o"), *map(str, objects), "--start-group", "--whole-archive",
                     *(archive["output"] for archive in whole), "--no-whole-archive",
                     *(archive["output"] for archive in static), "--end-group", *shared, "--as-needed",
                     str(platform / "lib/libc++.so"), f"-L{libdir}", system_libc, "-lm", "-ldl",
                     str(DEPS / "toolchain/libclang_rt.builtins-x86_64-android.a"),
                     str(libdir / "crtend_so.o"), "-o", str(output)]
            self.runner.run(argv, "link " + name)
            verify_elf(output, 3)
        result.update(output=str(output), bytes=output.stat().st_size, sha256=BOOT.sha256_file(output))
        self.runner.run([str(LLVM / "llvm-readelf"), "-h", "-s", str(output)], "inspect " + name)
        result["elf_machine"] = "x86_64"
        result["runtime_tested"] = False
        if name == "libberberis_arm64":
            record = self.runner.run([str(LLVM / "llvm-readelf"), "--wide", "--dyn-syms", "--dynamic", str(output)],
                                     "verify translator dynamic entry")
            entries = [line.split() for line in Path(record["log"]).read_text().splitlines()]
            if not any(len(f) >= 8 and f[3:6] == ["OBJECT", "GLOBAL", "DEFAULT"] and f[6] != "UND"
                       and f[7].split("@", 1)[0] == "NativeBridgeItf" for f in entries):
                raise BuildError("translator shared library is missing its global dynamic NativeBridgeItf export")
            result["native_bridge_itf_dynamic_export"] = True
        if name.startswith("libberberis_proxy_") and linkage == "shared":
            result["dynamic"] = dynamic_evidence(self.runner, output, "verify proxy entry " + name)
            if not result["dynamic"]["init_proxy_library"]:
                raise BuildError(f"proxy is missing its global dynamic InitProxyLibrary function: {name}")
            result["init_proxy_library_dynamic_export"] = True
        if name == "libberberis_native_bridge_arm64":
            record = self.runner.run([str(LLVM / "llvm-nm"), "--defined-only", str(output)], "verify bridge entry archive")
            if not re.search(r"(?m)^\S+ [BD] NativeBridgeItf$", Path(record["log"]).read_text()):
                raise BuildError("NativeBridgeItf global definition missing from the bridge entry archive")
            result["native_bridge_itf_archive_definition"] = True
        return result


def stage_bridge_tls_adapter(source_root, destination, patch_path=BRIDGE_TLS_PATCH):
    patch = json.loads(Path(patch_path).read_text())
    if (patch.get("schema") != 1 or len(patch.get("sources", [])) != 2
            or {s.get("path") for s in patch["sources"]} != set(BRIDGE_TLS_MODULES)
            or set(patch.get("added_files", {})) != {"pengudroid_bridge_tls.h"}):
        raise BuildError("unsupported bridge TLS adapter patch contract")
    staged, records = {}, []
    for specification in patch["sources"]:
        relative = specification["path"]
        original = Path(source_root) / relative
        digest = BOOT.sha256_file(original)
        if digest != specification["base_sha256"]:
            raise BuildError(f"bridge TLS adapter base source hash mismatch: {relative}")
        text = original.read_text()
        for replacement in specification["replacements"]:
            if not replacement["old"] or text.count(replacement["old"]) != 1:
                raise BuildError(f"bridge TLS adapter context is not unique: {relative}")
            text = text.replace(replacement["old"], replacement["new"], 1)
        staged[relative] = text
        records.append({"source": str(original), "source_sha256": digest,
                        "staged": str(Path(destination) / relative),
                        "staged_sha256": hashlib.sha256(text.encode()).hexdigest()})
    header = patch["added_files"]["pengudroid_bridge_tls.h"]
    rust = (REPO / "crates/pengudroid-shim/src/bridge_tls.rs").read_text()
    cookie = f"0x{BRIDGE_TLS_COOKIE:x}"
    if cookie not in header or f"pub const LAYOUT_COOKIE: u64 = {cookie};" not in rust:
        raise BuildError("tracked C++/Rust bridge TLS ABI cookies differ")
    staged.update(patch["added_files"])
    # Validate every input before creating any output. No cached file is written.
    Path(destination).mkdir()
    for relative, content in staged.items():
        output = Path(destination) / relative
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(content)
    return {"patch": str(patch_path), "patch_sha256": BOOT.sha256_file(Path(patch_path)),
            "sources": records, "header_sha256": hashlib.sha256(header.encode()).hexdigest(),
            "layout_cookie": cookie, "cached_sources_modified": False}


def relink_bridge_tls(model, runner, sysroot, reuse, manifest, imports):
    if reuse is None or "libberberis_arm64" not in reuse.artifacts:
        raise BuildError("TLS adapter requires a successful reuse report containing libberberis_arm64")
    translator = reuse.inspect("libberberis_arm64")
    report = reuse.report
    source_root = model.roots["frameworks/libs/binary_translation"]
    stage = runner.root / "bridge-tls-source"
    adapter = stage_bridge_tls_adapter(source_root, stage)
    runner.report["bridge_tls_adapter"] = adapter
    artifacts = {}
    for artifact in report.get("artifacts", []) + report.get("dependency_artifacts", []):
        if artifact.get("host"):
            continue
        key = (artifact["module"], artifact["kind"])
        if key in artifacts:
            raise BuildError(f"duplicate reuse archive/module: {key}")
        artifacts[key] = artifact
    link_commands = [c for c in report["commands"] if c["label"] == "link libberberis_arm64"]
    if len(link_commands) != 1:
        raise BuildError("TLS relink requires exactly one recorded translator link command")
    link = list(link_commands[0]["argv"])
    if (link[0] != str(LLVM / "ld.lld") or link[-2:] != ["-o", translator["output"]]
            or "--no-undefined" not in link or "--threads=1" not in link):
        raise BuildError("unrecognized recorded translator link command")
    # Check every archive/shared/CRT/builtins input against its original report
    # or bootstrap attestation before passing it to the linker. No guessed .a
    # and no rebuild fallback, especially for the heavy optimizer/frontend.
    known = dict(manifest["preflight"]["inputs"])
    for artifact in artifacts.values():
        output = Path(artifact["output"])
        if (not output.resolve().is_relative_to(reuse.run.resolve())
                or output.stat().st_size != artifact["bytes"]):
            raise BuildError(f"reused archive/shared artifact path/size mismatch: {output}")
        known[str(output)] = artifact["sha256"]
    for name in PLATFORM_SHARED:
        path = imports.inspect(name)
        known[str(path)] = imports.records[name]["sha256"]
    inputs = []
    for argument in link[1:-2]:
        path = Path(argument)
        if path.is_absolute() and path.suffix in (".a", ".so", ".o"):
            if str(path) not in known or BOOT.sha256_file(path) != known[str(path)]:
                raise BuildError(f"translator relink input is not attested or changed: {path}")
            inputs.append({"path": str(path), "sha256": known[str(path)]})
    adapter["reused_link_inputs"] = inputs
    replacements = {}
    generators = NativeBuild(model, runner, sysroot)
    for relative, name in BRIDGE_TLS_MODULES.items():
        original = source_root / relative
        archive = artifacts.get((name, "cc_library_static"))
        if archive is None or archive.get("whole_static_archives"):
            raise BuildError(f"unsupported/missing small TLS adapter archive: {name}")
        if model.props(name, linkage="static") != report["modules"][name]["selected_properties"]:
            raise BuildError(f"small TLS module properties changed: {name}")
        old_archive = Path(archive["output"])
        if BOOT.sha256_file(old_archive) != archive["sha256"]:
            raise BuildError(f"small TLS archive changed: {name}")
        commands = [c for c in report["commands"] if c["label"] == f"compile {name}: {original.name}"]
        if len(commands) != 1:
            raise BuildError(f"TLS source compile command is missing/ambiguous: {relative}")
        argv = list(commands[0]["argv"])
        if (argv[0] != str(LLVM / "clang++") or argv[-4:-2] != ["-c", str(original)]
                or argv[-2] != "-o" or argv[-1] not in archive["objects"]
                or "--target=x86_64-linux-android37" not in argv):
            raise BuildError(f"unrecognized TLS compile command: {relative}")
        object_dir = runner.root / "obj" / name
        object_dir.mkdir()
        members = runner.run([str(LLVM / "llvm-ar"), "t", str(old_archive)], "check TLS archive members " + name)
        names = Path(members["log"]).read_text().splitlines()
        if (names != [Path(p).name for p in archive["objects"]] or len(set(names)) != len(names)
                or any(Path(n).name != n or n in ("", ".", "..") for n in names)):
            raise BuildError(f"TLS archive object list mismatch: {name}")
        runner.run([str(LLVM / "llvm-ar"), "x", "--output=" + str(object_dir), str(old_archive)],
                   "extract verified small TLS archive " + name)
        # Regenerate only Python-generated public headers in fresh output dirs.
        # Never use mutable old generated headers as unstated compile inputs.
        for i, argument in enumerate(argv):
            old_gen = reuse.run / "gen"
            if argument.startswith(str(old_gen) + "/"):
                generator = Path(argument).relative_to(old_gen).parts[0]
                props = model.props(generator)
                if any(model.modules[model.canonical(t)][0] != "python_binary_host" for t in props.get("tools", [])):
                    raise BuildError("TLS incremental build refuses native/heavy host codegen")
                argv[i] = argument.replace(str(old_gen / generator), str(generators.generate(generator)), 1)
        new_object = object_dir / Path(argv[-1]).name
        argv[-4:-2] = ["-c", str(stage / relative)]
        argv[-1] = str(new_object)
        argv[1:1] = ["-I", str(stage)]
        runner.run(argv, "compile TLS adapter " + relative)
        verify_elf(new_object, 1)
        output = runner.root / "lib" / old_archive.name
        objects = [object_dir / n for n in names]
        for obj in objects:
            verify_elf(obj, 1)
        runner.run([str(LLVM / "llvm-ar"), "rcsD", str(output), *map(str, objects)], "archive TLS adapter " + name)
        replacements[str(old_archive)] = str(output)
        runner.report.setdefault("dependency_artifacts", []).append({
            **archive, "output": str(output), "objects": list(map(str, objects)),
            "bytes": output.stat().st_size, "sha256": BOOT.sha256_file(output),
            "adapter_source": relative, "reused_archive_sha256": archive["sha256"],
        })
    if len(replacements) != 2 or any(link.count(old) != 1 for old in replacements):
        raise BuildError("TLS adapter archives are not exactly once in the translator link closure")
    link = [replacements.get(arg, arg) for arg in link]
    output = runner.root / "lib/libberberis_arm64.so"
    link[-1] = str(output)
    runner.run(link, "relink translator with TLS adapter")
    evidence = dynamic_evidence(runner, output, "inspect TLS-adapted translator")
    log = Path(evidence["log"]).read_text()
    if not re.search(r"(?m)^\s*\d+:\s+\S+\s+\d+\s+OBJECT\s+GLOBAL\s+DEFAULT\s+(?!UND)\S+\s+NativeBridgeItf\s*$", log):
        raise BuildError("TLS-adapted translator is missing its NativeBridgeItf dynamic entry")
    # Recheck immutable originals after compile/link as well.
    for record in adapter["sources"]:
        if BOOT.sha256_file(Path(record["source"])) != record["source_sha256"]:
            raise BuildError("cached Digitalis source changed during TLS adapter build")
    for record in inputs:
        if BOOT.sha256_file(Path(record["path"])) != record["sha256"]:
            raise BuildError("reused translator closure changed during TLS adapter link")
    adapter["compiled_translation_units"] = list(BRIDGE_TLS_MODULES)
    adapter["required_runtime_opt_in"] = ["PENGUDROID_BRIDGE_TLS=1", "BERBERIS_FLAGS=glibc-host-thread-id-handoff"]
    adapter["limitations"] = ["guest pthread handles require separate proxy translation before glibc pthread calls",
                              "guest thread registry, key destructors, MTE and host debuggerd TLS slots are not implemented",
                              "synthetic storage is retained for process lifetime; guest TCB binds at ConfigStaticTls"]
    return {"module": "libberberis_arm64", "kind": "cc_library_shared", "output": str(output),
            "objects": [], "bytes": output.stat().st_size, "sha256": BOOT.sha256_file(output),
            "elf_machine": "x86_64", "native_bridge_itf_dynamic_export": True,
            "dynamic": evidence, "runtime_tested": False, "relinked_from_report": str(reuse.path)}


def stage_debionic_proxy(artifact, runner, tool):
    """Make a separate loader-compatible copy, NOT an assertion of glibc ABI safety."""
    translator = artifact.get("native_bridge_itf_dynamic_export") is True
    if not artifact.get("init_proxy_library_dynamic_export") and not translator:
        raise BuildError("de-bionic staging is restricted to verified native-bridge proxies/translators")
    source, tool = Path(artifact["output"]), Path(tool).resolve()
    if not tool.is_file() or not os.access(tool, os.X_OK):
        raise BuildError(f"missing executable de-bionic tool: {tool}; no cargo build attempted")
    if BOOT.sha256_file(source) != artifact["sha256"]:
        raise BuildError(f"proxy changed before de-bionic staging: {source}")
    tool_digest = BOOT.sha256_file(tool)
    output = runner.root / "glibc" / source.name
    output.parent.mkdir(exist_ok=True)
    runner.run([str(tool), "de-bionic", str(source), str(output)], "de-bionic " + artifact["module"])
    evidence = dynamic_evidence(runner, output, "inspect de-bionic " + artifact["module"])
    entry = ("NativeBridgeItf" in Path(evidence["log"]).read_text()) if translator else evidence["init_proxy_library"]
    if not entry or evidence["soname"] != [source.name]:
        raise BuildError("de-bionic staging lost proxy entry/SONAME")
    if BOOT.sha256_file(source) != artifact["sha256"] or BOOT.sha256_file(tool) != tool_digest:
        raise BuildError("proxy/de-bionic executable changed during staging")
    return {"output": str(output), "bytes": output.stat().st_size, "sha256": BOOT.sha256_file(output),
            "tool": str(tool), "tool_sha256": tool_digest, "dynamic": evidence,
            "runtime_tested": False, "glibc_abi_compatible": False,
            "warning": "Only ELF loader metadata was transformed. Bionic ABI adaptation is still required.",
            "unverified_interfaces": ([
                "pthread_attr_t/internal thread and cleanup layouts; raw pthread passthrough in the table",
                "Bionic mallinfo structure return ABI vs glibc mallinfo",
                "malloc_disable/malloc_enable/malloc_iterate allocator semantics (glibc has no equivalents)",
                "system-property callback APIs and opaque prop_info objects",
                "Bionic host sigjmp_buf used by the translator vs glibc siglongjmp",
            ] if artifact["module"] == "libberberis_proxy_libc" else [
                "ProxyLibraryBuilder and translator/C++ runtime closure must use the same ABI",
            ])}


def prepare_root():
    if ROOT.is_symlink():
        raise BuildError("symlink build root refused")
    ROOT.mkdir(exist_ok=True)
    marker = ROOT / ".owner.json"
    if marker.exists():
        if marker.is_symlink() or json.loads(marker.read_text()) != OWNER:
            raise BuildError("unrecognized build-root owner")
    else:
        if any(ROOT.iterdir()):
            raise BuildError("nonempty unowned build directory refused")
        marker.write_text(json.dumps(OWNER))
    for name in ("downloads", "work", "extra"):
        path = ROOT / name
        if path.is_symlink():
            raise BuildError(f"symlink output refused: {path}")
        path.mkdir(exist_ok=True)


def fetch_extras(offline=False):
    destination = ROOT / "extras-manifest.json"
    previous = json.loads(destination.read_text()) if destination.exists() else {}
    legacy = ROOT / "extra-manifest.json"
    if "libnativehelper" not in previous and legacy.exists():
        previous["libnativehelper"] = json.loads(legacy.read_text())
    downloader, records = BOOT.Downloader(offline=offline), dict(previous)
    for extra in EXTRAS:
        # Reuse only exact pinned blobs from the guest-linker agent's read-only
        # Bionic cache. Never repoint the project root at an unverified tree.
        cache = ROOT / "downloads" / (extra.name + ".b64")
        local = DEPS / "sources/bionic-expansion/bionic" / extra.subtree
        if extra.project == "bionic" and extra.blob and not cache.exists() and local.is_file():
            data = local.read_bytes()
            if BOOT.git_hash(b"blob", data).hex() != extra.blob:
                raise BuildError(f"cached Bionic blob does not match pin: {local}")
            encoded = base64.b64encode(data)
            if len(encoded) > extra.limit:
                raise BuildError(f"cached Bionic blob exceeds download limit: {extra.name}")
            cache.parent.mkdir(parents=True, exist_ok=True)
            with cache.open("xb") as stream:
                stream.write(encoded)
        records[extra.name] = BOOT.fetch_artifact(ROOT, extra, downloader, previous.get(extra.name))
        temporary = destination.with_suffix(".tmp")
        temporary.write_text(json.dumps(records, indent=2) + "\n")
        temporary.replace(destination)
    return records, downloader.received


def atomic_json(path, data):
    temporary = path.with_suffix(".tmp")
    if path.is_symlink() or temporary.is_symlink():
        raise BuildError(f"symlink report path refused: {path}")
    temporary.write_text(json.dumps(data, indent=2) + "\n")
    temporary.replace(path)


def early_snapshot(report):
    """Validate a completed early milestone; never reuse it as this run's output."""
    if not report.get("early_steps_passed"):
        raise BuildError("early checkpoint does not attest a completed early milestone")
    if any(report.get(key) != value for key, value in OWNER.items()):
        raise BuildError("unrecognized early checkpoint owner/schema")
    try:
        run = Path(report["run"])
        if not run.resolve().is_relative_to(ROOT.resolve()):
            raise BuildError("early checkpoint run escapes owned build root")
        artifacts = [a for a in report["artifacts"] if a["module"] in EARLY_GOALS]
        if len(artifacts) != len(EARLY_GOALS) or {a["module"] for a in artifacts} != set(EARLY_GOALS):
            raise BuildError("early checkpoint is missing or duplicating early artifacts")
        for artifact in artifacts:
            name = artifact["module"]
            suffix = ".so" if name == "libberberis_exec_region" else ".a"
            output = Path(artifact["output"])
            if output != run / "lib" / (name + suffix) or not output.resolve().is_relative_to(run.resolve()):
                raise BuildError("early checkpoint artifact path mismatch/escape")
            if output.stat().st_size != artifact["bytes"] or BOOT.sha256_file(output) != artifact["sha256"]:
                raise BuildError(f"retained early artifact changed: {output}")
            for obj in artifact["objects"]:
                path = Path(obj)
                module_dirs = [run / "obj" / name, run / "obj" / f"{name}-static",
                               run / "obj" / f"{name}-shared"]
                if (not any(path.resolve().is_relative_to(d.resolve()) for d in module_dirs)
                        or not path.resolve().is_relative_to(run.resolve())):
                    raise BuildError("early checkpoint object escapes its module directory")
                verify_elf(path, 1)
        return {**OWNER, "run": str(run), "early_steps_passed": True, "complete_translator": False,
                "driver_sha256": report["driver_sha256"],
                "dependencies_manifest_sha256": report["dependencies_manifest_sha256"],
                "artifacts": artifacts, "report": str(run / "report.json"),
                "reused_for_current_run": False}
    except (KeyError, TypeError, OSError) as error:
        raise BuildError(f"invalid retained early checkpoint: {error}") from error


def load_early_checkpoint():
    checkpoint = ROOT / "last-successful-early.json"
    if checkpoint.is_symlink():
        raise BuildError("symlink early checkpoint refused")
    if checkpoint.exists():
        return early_snapshot(json.loads(checkpoint.read_text()))
    legacy = ROOT / "report.json"
    if legacy.exists():
        if legacy.is_symlink():
            raise BuildError("symlink legacy report refused")
        previous = json.loads(legacy.read_text())
        candidate = previous if previous.get("early_steps_passed") else previous.get("last_successful_early")
        if candidate:
            snapshot = early_snapshot(candidate)
            atomic_json(checkpoint, snapshot)
            return snapshot
    return None


def publish_report(report):
    if report.get("early_steps_passed"):
        snapshot = early_snapshot(report)
        atomic_json(ROOT / "last-successful-early.json", snapshot)
        report["last_successful_early"] = snapshot
    elif report.get("last_successful_early"):
        report["last_successful_early"] = early_snapshot(report["last_successful_early"])
    atomic_json(Path(report["run"]) / "report.json", report)
    atomic_json(ROOT / "report.json", report)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--module", action="append", help="an explicit early compile goal, repeatable")
    parser.add_argument("--seconds", type=int, default=180, choices=range(1, 3601), metavar="1..3600")
    parser.add_argument("--offline", action="store_true", help="refuse missing extra-source downloads")
    parser.add_argument("--towards-bridge", action="store_true", help="after early goals, attempt libberberis_arm64")
    parser.add_argument("--platform-root", type=Path, default=PLATFORM_ROOT, help="read-only completed API37 GSI extraction")
    parser.add_argument("--check-platform", action="store_true", help="only verify platform shared imports; do not compile")
    parser.add_argument("--reuse-report", type=Path, help="reuse checked shared libraries from a successful owned run/report.json")
    parser.add_argument("--de-bionic-tool", type=Path, help="existing pengudroid executable; stage separate glibc-loader copies of built proxies")
    parser.add_argument("--probe-bridge-tls", action="store_true", help="only compile pinned x86_64/ARM64 Bionic TLS ABI probes")
    parser.add_argument("--bridge-tls-adapter", action="store_true", help="probe ABI, stage tracked TLS adapter, rebuild two small TUs, relink reused translator")
    args = parser.parse_args()
    if args.check_platform and (args.module or args.towards_bridge or args.reuse_report or args.de_bionic_tool):
        parser.error("--check-platform cannot be combined with build goals")
    if args.bridge_tls_adapter and (not args.reuse_report or args.module or args.towards_bridge or args.check_platform):
        parser.error("--bridge-tls-adapter requires --reuse-report and cannot be combined with other goals")
    if args.probe_bridge_tls and (args.module or args.towards_bridge or args.check_platform or args.reuse_report or args.de_bionic_tool or args.bridge_tls_adapter):
        parser.error("--probe-bridge-tls is an independent, compile-only ABI check")
    prepare_root()
    with (ROOT / ".lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        run = Path(tempfile.mkdtemp(prefix="run-", dir=ROOT))
        for directory in ("logs", "gen", "obj", "lib", "tmp"):
            (run / directory).mkdir()
        report = {**OWNER, "target": "x86_64-linux-android37, ARM64 guest", "complete_translator": False,
                  "early_steps_passed": False, "requested_goals_passed": False,
                  "driver_sha256": BOOT.sha256_file(Path(__file__).resolve()),
                  "run": str(run), "commands": [], "artifacts": [], "last_successful_early": None,
                  "platform_imports": {}, "policy": {
                      "jobs": 1, "child_address_space_bytes": MEMORY, "max_output_bytes_per_file": FILE_LIMIT,
                      "optimization": "-O0 -g0 -fno-lto", "default_cpp_std": "gnu++23",
                      "platform_flags": PLATFORM_FLAGS, "os_selectors": OS_AXES, "arch_selectors": ARCH_AXES,
                      "platform_libcxx_abi": "std::__1 (not NDK std::__ndk1)",
                      "offline": args.offline, "no_source_patches": not args.bridge_tls_adapter,
                      "cached_sources_modified": False, "staged_bridge_tls_adapter": args.bridge_tls_adapter,
                      "platform_root": str(args.platform_root), "platform_root_access": "read-only; completed manifest only",
                      "platform_linking_implemented": True,
                      "shared_link_scope": "explicit static/whole/shared dependencies; only literal --wrap linker flags supported",
                      "translation_arch": "arm64_to_x86_64", "private_libc": "proxies require verified platform libc, not NDK stubs",
                      "reuse_report": str(args.reuse_report) if args.reuse_report else None,
                      "de_bionic_tool": str(args.de_bionic_tool) if args.de_bionic_tool else None,
                      "whole_static_merging": "llvm-ar qcsDL; deterministic non-thin flattened archives",
                      "scope": "early native compilation, not complete Soong platform defaults/ABI/sanitizer variants"}}
        model = None
        try:
            report["last_successful_early"] = load_early_checkpoint()
            manifest_path = DEPS / "manifest.json"
            manifest = json.loads(manifest_path.read_text())
            BOOT.check_manifest(manifest)
            missing = {a.name for a in BOOT.ARTIFACTS} - set(manifest["artifacts"])
            if missing or manifest.get("preflight", {}).get("passed") is not True:
                raise BuildError(f"incomplete verified bootstrap; missing artifacts: {sorted(missing)}; no bootstrap downloads attempted")
            roots = {}
            for artifact in manifest["artifacts"].values():
                path = DEPS / artifact["destination"]
                if "git_tree" in artifact:
                    if BOOT.tree_digest(path, artifact.get("inert_links", {})) != artifact["git_tree"]:
                        raise BuildError(f"modified dependency: {path}")
                    if not artifact.get("subtree"):
                        roots[artifact["project"].removeprefix("DigitalisX64/")] = path
            roots["frameworks/libs/binary_translation"] = roots.pop("platform_frameworks_libs_binary_translation")
            # Private Bionic headers are intentionally slices, not a full guest build tree.
            roots["bionic"] = DEPS / "sources/bionic"
            for filename, digest in manifest["preflight"]["inputs"].items():
                if BOOT.sha256_file(Path(filename)) != digest:
                    raise BuildError(f"preflight input changed: {filename}")
            report["dependencies_manifest_sha256"] = BOOT.sha256_file(manifest_path)
            report["source_pins"] = {name: {k: a[k] for k in ("project", "revision", "download_sha256")}
                                     for name, a in manifest["artifacts"].items()}
            report["extra_sources"], report["network_bytes_this_run"] = fetch_extras(args.offline)
            roots["libnativehelper"] = ROOT / EXTRAS[0].destination
            roots["build-support/bionic-libc"] = ROOT / "extra/bionic-libc"
            roots["frameworks/base"] = ROOT / "extra/frameworks-base"
            roots["build-support/bionic-libdl"] = ROOT / "extra/bionic-libdl"
            roots["hardware/interfaces"] = ROOT / "extra/hardware-interfaces"
            roots["frameworks/native"] = ROOT / "extra/frameworks-native"
            roots["packages/modules/NpuManager"] = ROOT / "extra/npumanager"
            roots["build/soong"] = ROOT / "extra/soong"
            ensure_art_header_module()
            roots["art-headers"] = ROOT / "extra/art-headers"
            source_dirs = {ROOT / "extra/bionic-libc/Android.bp": DEPS / "sources/bionic/libc"}
            model = Blueprints(roots, source_dirs, translation_arch="arm64_to_x86_64")
            report["platform_sources"] = describe_platform_sources(model)
            # Keep proof that the Android root retains its real bridge entry dependency.
            report["bridge_goal"] = model.props("libberberis_arm64")
            if "libberberis_native_bridge_arm64" not in report["bridge_goal"].get("whole_static_libs", []):
                raise BuildError("Android bridge goal lost its real native-bridge entry dependency")
            runner = Runner(run, report, args.seconds)
            reuse = (SharedReuse(args.reuse_report, model, runner, report["dependencies_manifest_sha256"])
                     if args.reuse_report else None)
            build = NativeBuild(model, runner, Path(manifest["preflight"]["sysroot"]), shared_reuse=reuse)
            version = runner.run([str(LLVM / "clang++"), "--no-default-config", "--version"], "compiler version")
            report["compiler_version"] = Path(version["log"]).read_text()
            if args.probe_bridge_tls:
                report["goals"] = ["compile pinned Bionic TLS ABI probes"]
                probe_bridge_tls(runner, Path(manifest["preflight"]["sysroot"]))
                report["requested_goals_passed"] = True
                report["blocker"] = "ABI probes only; no runtime execution attempted."
                return 0
            imports = PlatformImports(args.platform_root, runner, report["platform_imports"])
            platform_ready = imports.probe()
            build.platform_imports = imports
            if args.bridge_tls_adapter:
                report["goals"] = ["incremental TLS-adapted libberberis_arm64"]
                probe_bridge_tls(runner, Path(manifest["preflight"]["sysroot"]))
                artifact = relink_bridge_tls(model, runner, Path(manifest["preflight"]["sysroot"]), reuse, manifest, imports)
                report["artifacts"].append(artifact)
                if args.de_bionic_tool:
                    artifact["de_bionic"] = stage_debionic_proxy(artifact, runner, args.de_bionic_tool)
                report["requested_goals_passed"] = True
                report["blocker"] = "Only bootstrap TLS contracts are adapted; guest runtime and pthread proxies remain unvalidated."
                return 0
            if args.check_platform:
                report["goals"] = ["verify platform shared imports"]
                if not platform_ready:
                    blockers = [f"{n}: {r['blocker']}" for n, r in report["platform_imports"].items() if r["status"] == "blocked"]
                    raise BuildError("platform shared imports not ready:\n" + "\n".join(blockers))
                report["requested_goals_passed"] = True
                report["blocker"] = "Platform inputs inspected only; source/header/codegen and full shared-link closure remain."
                return 0
            goals: list[str] = list(dict.fromkeys(args.module or EARLY_GOALS))
            if args.towards_bridge and "libberberis_arm64" not in goals:
                goals.append("libberberis_arm64")
            report["goals"] = goals
            if any(goal.startswith("libberberis_proxy_") for goal in goals):
                report["proxy_support"] = proxy_support_evidence(model)
            for goal in goals:
                report["attempting"] = goal
                artifact = build.build(goal)
                report["artifacts"].append(artifact)
                if args.de_bionic_tool and artifact.get("init_proxy_library_dynamic_export"):
                    artifact["de_bionic"] = stage_debionic_proxy(artifact, runner, args.de_bionic_tool)
                built = {a["module"] for a in report["artifacts"]}
                report["early_steps_passed"] = set(EARLY_GOALS) <= built
            report["requested_goals_passed"] = True
            report["blocker"] = "Requested native goals built; full translator/guest/loader/runtime closure is not established."
        except (BuildError, BOOT.BootstrapError, OSError, ValueError, subprocess.SubprocessError) as error:
            report["blocker"] = str(error)
            print(str(error), file=sys.stderr)
        finally:
            if model is not None:
                report["modules"] = model.used
                report["soong_config_selections"] = model.config_selections
            report["retry"] = [sys.executable, "-B", str(Path(__file__).resolve())] + sys.argv[1:]
            translator_built = any(a.get("native_bridge_itf_dynamic_export") for a in report["artifacts"])
            report["translator_shared_built"] = translator_built
            publish_report(report)
            print(f"Report: {ROOT / 'report.json'}\nTranslator shared built: {'YES' if translator_built else 'NO'}\nARM64 execution tested: NO", flush=True)
        return 0 if report.get("requested_goals_passed") else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (BuildError, OSError, ValueError) as error:
        print(f"build-nativebridge: {error}", file=sys.stderr)
        raise SystemExit(1)
