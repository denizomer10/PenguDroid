#!/usr/bin/env python3
"""Build the ARM64 native-bridge guest linker and bootstrap from pinned AOSP sources.

Run: python3 -B tools/build-guest-linker.py [--seconds N] [--stage all|compile|rust|link|verify]
     python3 -B tools/build-guest-linker.py --module libm
     python3 -B tools/build-guest-linker.py --module app_process --bundle

The bootstrap uses NBS's tiny native_bridge_guest_app_process, NOT the framework
app_process. New module builds reuse the already-built guest linker/libc/libdl;
--stage verify checks existing artifacts without running compilers or generators.

The 32 translation units, static closure, flags, version script and post-link
`llvm-objcopy --prefix-symbols=__dl_` come from
`.toolchain/nativebridge-deps/sources/bionic-expansion/REPORT.md`. The driver
reuses tools/build-nativebridge.py's bounded Runner (serial, RLIMIT_AS,
RLIMIT_CPU, RLIMIT_FSIZE, output cap) and Blueprint tokenizer, but selects the
`arm64` / `android` / `linux_bionic` / `native_bridge` variants instead of
x86_64. Sources are never patched. Every compiler/linker command and the final
ELF evidence are retained in report.json.

Only .toolchain/nativebridge-guest-linker-build/ is written.
"""

import argparse
import copy
import hashlib
import importlib.util
import json
import os
import re
import resource
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parent.parent
ROOT = REPO / ".toolchain/nativebridge-guest-linker-build"
DEPS = REPO / ".toolchain/nativebridge-deps"
# See tools/build-nativebridge.py: the pinned compiler is LLVM 22, and a
# relocated package tree can be selected instead of the distro path.
LLVM = Path(os.environ.get("PENGUDROID_LLVM_DIR", "/usr/lib/llvm-22/bin"))
NDK = REPO / ".toolchain/ndk-refs_tags_ndk-r30-platform-sysroot"
BIONIC = DEPS / "sources/bionic-expansion/bionic"
NBS = DEPS / "sources/native_bridge_support"
EXT = DEPS / "sources/ext"
PREBUILT = EXT / "clang-prebuilt"
LIBDL_SRC = EXT / "bionic-libdl"
SCUDO_SRC = EXT / "scudo"
GWP_ASAN_SRC = EXT / "gwp_asan"
# libc++ must be a single matching version: the NDK tree next to the sysroot is
# self-consistent (it also ships the `<string.h>`/`<wchar.h>` overlays), while
# mixing the platform slice with a different common header tree produced
# conflicting `__atomic` builtins. The slice remains the fallback for a sysroot
# without an NDK.
_NDK_LIBCXX = NDK / "usr/include/c++/v1"
if _NDK_LIBCXX.is_dir():
    LIBCXX = _NDK_LIBCXX
    LIBCXX_COMMON = _NDK_LIBCXX
else:
    LIBCXX = DEPS / "toolchain/libcxx/android/x86_64/include/c++/v1"
    LIBCXX_COMMON = DEPS / "toolchain/libcxx/include/c++/v1"
LINKER_ARTIFACT = ROOT / "out" / "native_bridge_guest_linker.native_bridge"
VDSO_FILE = REPO / ".toolchain/sysroot-android17/system/lib64/arm64/libnative_bridge_vdso.so"
LIBC_ARTIFACT = ROOT / "out" / "libnative_bridge_guest_libc.native_bridge"
APP_PROCESS = ROOT / "out" / "native_bridge_guest_app_process.native_bridge"
# Soong cc/binary.go uses the canonical device interpreter. Digitalis's
# kernel_api/open_emulation.cc redirects guest opens to /system/bin/arm64/linker64.
GUEST_INTERPRETER = "/system/bin/linker64"
CRT = NDK / "usr/lib/aarch64-linux-android/37"
# Digitalis installs this as system/etc/ld.config.arm64.txt; the guest linker
# reads it from kLdConfigArchFilePath and it is what makes the exported guest
# "default" namespace search /system/lib64/arm64/bootstrap:/system/lib64/arm64.
LD_CONFIG = (REPO / ".toolchain/platform_frameworks_libs_binary_translation-android16"
             / "prebuilt/system/etc/ld.config.arm64.txt")
BUNDLE = ROOT / "bundle"
RUSTC = Path.home() / ".cargo/bin/rustc"

BIONIC_PIN = "06356e41c5ed7b12220b24c05ba4fdb873b126a7"
EXT_PINS = {
    "external/arm-optimized-routines": "498b3de1a06e8cd87b2f9d49d1d8a23ebba5cc5a",
    "external/google-highway": "94b90ab8cb9df9b3719a64751ea2fcb62caa3391",
    "external/llvm-libc": "95e08d1d4ccefa41921bc00a369008f7618b7101",
    "external/rust/android-crates-io": "1f5e95cd6e996995023c9c0f7df0f15a8a43269e",
    "system/librustutils": "7fbb3e4aaf60",
    "external/scudo": "15715589eebe",
    "external/gwp_asan": "54826e96fc88",
    "system/libbase": "fef4173e19d97cd4b4fc09a06bccbf800d0d3937",
    "system/logging": "d35ba620451b28512f46ebb5421f783fd77968d5",
    "external/zlib": "8731e6760935ad5289fd64c37d28155a0fa289e3",
    "system/libziparchive": "bdd41f9c9940",
    "system/core/libcutils": "545d2487e38192a2ce25040897ced877cf6b4f53",
    "system/core/property_service/libpropertyinfoparser": "545d2487e38192a2ce25040897ced877cf6b4f53",
    "build/soong": "6722dd8833db7482df1a2543ca3fcf67ddf0f7b1",
    "prebuilts/clang/host/linux-x86": "291828893998",
}
OWNER = {"tool": "PenguDroid guest-linker build", "schema": 1}
MEMORY = 6_000_000_000
FILE_LIMIT = 512 * 1024 * 1024
PLATFORM_FLAGS = ("-DANDROID", "-DNDEBUG", "-UDEBUG", "-D__compiler_offsetof=__builtin_offsetof",
                  "-D__ANDROID_NATIVE_BRIDGE__=1")
# Diagnostics added by clang 22 relative to the pinned clang-r584948. They do
# not change code generation and would otherwise be promoted by -Werror.
TRAILING_FLAGS = ("-Wno-error", "-Wno-nullability-completeness",
                  "-Wno-missing-designated-field-initializers",
                  "-Wno-vla-cxx-extension", "-Wno-macro-redefined")
# build/soong cc/config/global.go device flags; required so --gc-sections can
# drop the libc paths (malloc/scudo/MTE) that the linker never calls.
DEVICE_CFLAGS = ("-ffunction-sections", "-fdata-sections", "-fno-short-enums",
                 "-Wa,--noexecstack", "-D_FORTIFY_SOURCE=3")
TARGET = "--target=aarch64-linux-android37"
RUST_TARGET = "aarch64-linux-android"

TEXT_FIELDS = ("srcs", "exclude_srcs", "cflags", "cppflags", "conlyflags", "asflags",
               "local_include_dirs", "include_dirs", "export_include_dirs",
               "export_system_include_dirs", "header_libs", "static_libs",
               "whole_static_libs", "shared_libs", "generated_headers",
               "export_generated_headers", "generated_sources",
               "export_header_lib_headers", "export_static_lib_headers",
               "export_shared_lib_headers", "linker_scripts", "ldflags", "rustlibs",
               "features", "crate_root", "crate_name", "edition", "srcs")

BP_FILES = (
    BIONIC / "libc/Android.bp",
    BIONIC / "libc/async_safe/Android.bp",
    BIONIC / "libc/system_properties/Android.bp",
    BIONIC / "libm/Android.bp",
    BIONIC / "linker/Android.bp",
    NBS / "android_api/linker/Android.bp",
    NBS / "android_api/libc/Android.bp",
    NBS / "android_api/app_process/Android.bp",
    DEPS / "sources/libbase/Android.bp",
    DEPS / "sources/fmtlib/Android.bp",
    DEPS / "sources/logging/liblog/Android.bp",
    DEPS / "sources/zlib/Android.bp",
    DEPS / "sources/libziparchive/Android.bp",
    DEPS / "sources/googletest/Android.bp",
    DEPS / "sources/googletest/googletest/Android.bp",
    EXT / "arm-optimized-routines/Android.bp",
    EXT / "google-highway/Android.bp",
    EXT / "llvm-libc/Android.bp",
    EXT / "librustutils/no_std/Android.bp",
    EXT / "libpropertyinfoparser/Android.bp",
    EXT / "soong/Android.bp",
    NBS / "Android.bp",
    NBS / "android_api/vdso/Android.bp",
    LIBDL_SRC / "Android.bp",
    SCUDO_SRC / "Android.bp",
    GWP_ASAN_SRC / "Android.bp",
)
IGNORED_DEFAULTS = {"linux_bionic_supported", "rust_sysroot_defaults"}
CRATE_DIR = EXT / "android-crates-io"
# Header-only modules whose Blueprint pulls in unrelated platform trees. The
# include directories below are the exact exports consumed by this closure.
SYNTHETIC_HEADERS = {
    "libcutils_headers": [EXT / "libcutils/include", EXT / "libutils/include",
                          EXT / "libsystem/include"],
    "libprocessgroup_headers": [],
    "libnative_bridge_vdso": [NBS / "android_api/vdso/include"],
}
PREBUILT_STATIC = {"libc++_static_noexcept", "libc++demangle_noexcept", "libunwind",
                   "libclang_rt.builtins-exported", "libclang_rt.builtins",
                   "libunwind-exported"}
PREBUILT_ARCHIVES = {
    "libc++_static_noexcept": PREBUILT / "libc++_static_noexcept.a",
    "libc++abi_noexcept": PREBUILT / "libc++abi_noexcept.a",
    "libc++demangle_noexcept": PREBUILT / "libc++demangle_noexcept.a",
    "libunwind": PREBUILT / "libunwind.a",
    "libunwind-exported": PREBUILT / "libunwind-exported.a",
    "libclang_rt.builtins-aarch64-android": PREBUILT / "libclang_rt.builtins-aarch64-android.a",
}


class BuildError(RuntimeError):
    pass


def load_driver():
    spec = importlib.util.spec_from_file_location("nativebridge_driver", REPO / "tools/build-nativebridge.py")
    if spec is None or spec.loader is None:
        raise BuildError("cannot load tools/build-nativebridge.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


DRIVER = load_driver()
BOOT = DRIVER.BOOT
# The flattened libc_nomalloc archive is larger than the early driver's 64 MiB
# per-process file cap; keep the bounded, explicit 512 MiB cap for llvm-ar.
DRIVER.FILE_LIMIT = FILE_LIMIT
Runner = DRIVER.Runner
Values, merge, tokenize = DRIVER.Values, DRIVER.merge, DRIVER.tokenize


def sha256(path):
    return BOOT.sha256_file(Path(path))


def command_digest(argv):
    return hashlib.sha256("\0".join(map(str, argv)).encode()).hexdigest()[:16]


def expand_flags(flags):
    result = []
    for flag in flags:
        # Blueprint stores one flag per string, but a few entries (for example
        # "-include netbsd-compat.h") contain two argv words. posix=False keeps
        # embedded quotes such as visibility("default") intact.
        result += __import__("shlex").split(str(flag), posix=False)
    return result


def expand_linker_flags(flags):
    result = []
    for flag in flags:
        for token in __import__("shlex").split(str(flag), posix=False):
            if token.startswith("-Wl,"):
                result += token[4:].split(",")
            else:
                result.append(token)
    return result


def version_script_symbols(path):
    """Global symbols listed by a generated version script."""
    symbols, in_global = [], False
    for raw in Path(path).read_text().splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        if line.startswith("global:"):
            in_global = True
            continue
        if line.startswith("local:") or line.startswith("}"):
            in_global = False
            continue
        if in_global:
            symbol = line.rstrip(";").strip()
            if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_.]*", symbol):
                symbols.append(symbol)
    return symbols


def rust_support_object(runner):
    """The no_std + panic=abort runtime contract for rustc 1.99.

    core's unwind tables carry a DW.ref to `rust_eh_personality`. Modern rustc
    relies on std (or a user-provided lang item) for it; under panic=abort the
    routine is unreachable, so a hidden trap definition keeps libstd (and its
    ELF TLS) out of the guest.
    """
    directory = ROOT / "gen" / "rust-support"
    directory.mkdir(parents=True, exist_ok=True)
    source = directory / "rust_eh_personality.c"
    source.write_text('__attribute__((visibility("hidden"), noreturn))\n'
                      "void rust_eh_personality(void) { __builtin_trap(); }\n")
    output = directory / "rust_eh_personality.o"
    argv = [str(LLVM / "clang"), "--no-default-config", TARGET, f"--sysroot={NDK}",
            "-fPIC", "-O0", "-g0", "-ffunction-sections", "-fdata-sections",
            "-c", str(source), "-o", str(output)]
    if not output.is_file() or output.stat().st_mtime < source.stat().st_mtime:
        runner.run(argv, "compile rust personality support")
        verify_elf_arm64(output, 1)
    return output


def verify_elf_arm64(path, elf_type):
    with Path(path).open("rb") as stream:
        header = stream.read(20)
    if (header[:6] != b"\x7fELF\x02\x01" or int.from_bytes(header[16:18], "little") != elf_type
            or int.from_bytes(header[18:20], "little") != 183):
        raise BuildError(f"not the required little-endian ELF64 AArch64 type {elf_type}: {path}")


def strip_select(tokens):
    """Resolve select(release_flag(...), {true: ..., default: ...}) to `default`.

    The pinned build sets no RELEASE_* flags, so every select in this closure
    takes its default branch. Anything else is rejected, never guessed.
    """
    out, index = [], 0
    while index < len(tokens):
        if tokens[index] != "select":
            out.append(tokens[index])
            index += 1
            continue
        if index + 1 >= len(tokens) or tokens[index + 1] != "(":
            raise BuildError("select() without an argument list")
        depth, end = 0, index + 1
        while end < len(tokens):
            if tokens[end] == "(":
                depth += 1
            elif tokens[end] == ")":
                depth -= 1
                if depth == 0:
                    break
            end += 1
        inner = tokens[index + 2:end]
        condition = inner[:inner.index(",")] if "," in inner else []
        if (not condition or condition[0] != "release_flag" or condition[1] != "("
                or len(condition) != 4 or condition[3] != ")"):
            raise BuildError(f"unsupported select() condition: {' '.join(condition)}")
        try:
            brace = inner.index("{")
        except ValueError as error:
            raise BuildError("select() has no map argument") from error
        depth, close = 0, brace
        while close < len(inner):
            if inner[close] == "{":
                depth += 1
            elif inner[close] == "}":
                depth -= 1
                if depth == 0:
                    break
            close += 1
        body = inner[brace + 1:close]
        level, position, value = 0, 0, None
        while position < len(body):
            token = body[position]
            if token in "{[":
                level += 1
            elif token in "}]":
                level -= 1
            elif level == 0 and token == "default" and body[position + 1] == ":":
                start = position + 2
                value_depth, value_end = 0, start
                while value_end < len(body):
                    if body[value_end] in "{[":
                        value_depth += 1
                    elif body[value_end] in "}]":
                        value_depth -= 1
                    if value_depth < 0:
                        break
                    if value_depth == 0 and body[value_end] == ",":
                        break
                    value_end += 1
                value = body[start:value_end]
                break
            position += 1
        if value is None:
            raise BuildError("select() map has no default branch")
        out += value
        index = end + 1
    return out


class Blueprints:
    """The pinned Blueprint subset, evaluated for the arm64 native-bridge variant."""

    def __init__(self):
        self.modules, self.variables, self.raw_cache = {}, {}, {}
        self.custom_kinds = {}
        self.rust_indexed = set()
        for path in BP_FILES:
            if path.is_file():
                self.index(path)
            else:
                raise BuildError(f"missing pinned Android.bp: {path}")

    def index(self, path):
        tokens, variables = tokenize(Path(path).read_text()), {}
        index = 0
        while index < len(tokens):
            kind = tokens[index]
            if index + 1 < len(tokens) and tokens[index + 1] in ("=", "+="):
                values = Values(tokens[index + 2:], variables)
                value = values.value()
                variables[kind] = (variables.get(kind, []) + value
                                   if tokens[index + 1] == "+=" else value)
                index += 2 + values.index
                continue
            if index + 1 >= len(tokens) or tokens[index + 1] != "{":
                raise BuildError(f"unsupported Blueprint statement in {path}: {kind}")
            start, depth, index = index + 1, 1, index + 2
            while depth and index < len(tokens):
                depth += (tokens[index] == "{") - (tokens[index] == "}")
                index += 1
            body = tokens[start:index]
            if kind == "soong_config_module_type":
                descriptor = Values(body, variables).value()
                if isinstance(descriptor, dict) and "name" in descriptor and "module_type" in descriptor:
                    self.custom_kinds[descriptor["name"]] = descriptor["module_type"]
                continue
            if (kind.startswith("ndk_")
                    or kind.startswith(("cc_test", "cc_benchmark", "cc_fuzz"))):
                continue
            level = 0
            for number, token in enumerate(body[:-2]):
                level += (token == "{") - (token == "}")
                if level == 1 and token == "name" and body[number + 1] == ":":
                    name = json.loads(body[number + 2])
                    if name in self.modules and self.modules[name][1] != path:
                        raise BuildError(f"ambiguous Blueprint module {name}: {path}")
                    self.modules[name] = (kind, Path(path), body, copy.deepcopy(variables))
                    break

    def kind(self, name):
        name = self.ensure(name)
        declared = self.modules[name][0]
        return self.custom_kinds.get(declared, declared)

    def source_dir(self, name):
        return self.modules[self.canonical(name)][1].parent

    def canonical(self, name):
        if name.startswith("//"):
            _namespace, separator, module = name[2:].partition(":")
            if not separator:
                raise BuildError(f"unresolved Blueprint namespace: {name}")
            name = module
        else:
            name = name.removeprefix(":")
        if not re.fullmatch(r"[A-Za-z0-9_.+-]+", name):
            raise BuildError(f"unsafe/unsupported Blueprint module name: {name}")
        return name

    def ensure(self, name):
        name = self.canonical(name)
        if name in self.modules:
            return name
        # Rust crates live one directory per crate; index only what is used.
        candidates = [name.removeprefix("lib"), name]
        for candidate in candidates:
            path = CRATE_DIR / candidate / "Android.bp"
            if path.is_file() and path not in self.rust_indexed:
                self.rust_indexed.add(path)
                self.index(path)
                if name in self.modules:
                    return name
        for path in sorted(CRATE_DIR.glob("*/Android.bp")):
            if path in self.rust_indexed:
                continue
            text = path.read_text()
            if f'name: "{name}"' in text:
                self.rust_indexed.add(path)
                self.index(path)
                return name
        raise BuildError(f"unresolved Blueprint module {name}")

    def raw(self, name, stack=()):
        name = self.ensure(name)
        if name in self.raw_cache:
            return copy.deepcopy(self.raw_cache[name])
        if name in stack:
            raise BuildError(f"cyclic Blueprint defaults: {stack} -> {name}")
        _kind, _path, body, variables = self.modules[name]
        try:
            parsed = Values(strip_select(body), variables).value()
        except BuildError as error:
            raise BuildError(f"{name} ({self.modules[name][1]}): {error}") from error
        if not isinstance(parsed, dict):
            raise BuildError(f"Blueprint module body must be a property map: {name}")
        result = {}
        for default in parsed.get("defaults", []):
            default = self.canonical(default)
            if default not in self.modules:
                if default in IGNORED_DEFAULTS:
                    continue
                raise BuildError(f"{name}: unresolved defaults module {default}")
            result = merge(result, self.raw(default, stack + (name,)))
        result = merge(result, parsed)
        self.raw_cache[name] = result
        return copy.deepcopy(result)

    def props(self, name, linkage=None):
        name = self.ensure(name)
        raw = self.raw(name)
        selected = {key: value for key, value in raw.items()
                    if key not in {"arch", "target", "multilib", "static", "shared"}}
        # Soong order: OS/target axes, then arch, then multilib. `linux_bionic`
        # is the host-bionic OS; the guest artifacts are Android device
        # variants, so selecting it here would pull `libstd` into
        # libc_common_shared.
        for axis in ("linux", "bionic", "android", "android64", "native_bridge"):
            if isinstance(raw.get("target", {}).get(axis), dict):
                selected = merge(selected, raw["target"][axis])
        for axis in ("bionic_arm64", "android_arm64"):
            if isinstance(raw.get("target", {}).get(axis), dict):
                selected = merge(selected, raw["target"][axis])
        if isinstance(raw.get("arch", {}).get("arm64"), dict):
            selected = merge(selected, raw["arch"]["arm64"])
        if isinstance(raw.get("multilib", {}).get("lib64"), dict):
            selected = merge(selected, raw["multilib"]["lib64"])
        if linkage in ("static", "shared") and isinstance(raw.get(linkage), dict):
            selected = merge(selected, raw[linkage])
        # platform_sdk_version is the one numeric product variable in this
        # closure; the guest targets API 37.
        platform_sdk = raw.get("product_variables", {}).get("platform_sdk_version")
        if isinstance(platform_sdk, dict):
            substituted = {}
            for key, value in platform_sdk.items():
                if isinstance(value, list):
                    substituted[key] = [str(item).replace("%d", "37") for item in value]
                else:
                    substituted[key] = value
            selected = merge(selected, substituted)
        return selected

    def aosp_path(self, value):
        mapping = {
            "bionic": BIONIC,
            "external/llvm-libc": EXT / "llvm-libc",
            "external/arm-optimized-routines": EXT / "arm-optimized-routines",
            "external/google-highway": EXT / "google-highway",
            "external/scudo": SCUDO_SRC,
            "external/gwp_asan": GWP_ASAN_SRC,
            "bionic/libdl": LIBDL_SRC,
            "external/rust/android-crates-io": CRATE_DIR,
            "system/librustutils": EXT / "librustutils",
            "system/core/libcutils": EXT / "libcutils",
            "system/core/property_service/libpropertyinfoparser": EXT / "libpropertyinfoparser",
            "system/libbase": DEPS / "sources/libbase",
            "system/logging": DEPS / "sources/logging",
            "external/zlib": DEPS / "sources/zlib",
            "system/libziparchive": DEPS / "sources/libziparchive",
            "external/googletest": DEPS / "sources/googletest",
            "external/fmtlib": DEPS / "sources/fmtlib",
            "build/soong": EXT / "soong",
        }
        if value == "bionic/libstdc++/include":
            return BIONIC / "libstdc++"
        for prefix, root in sorted(mapping.items(), key=lambda item: -len(item[0])):
            if value == prefix or value.startswith(prefix + "/"):
                return root / value[len(prefix):].lstrip("/")
        raise BuildError(f"unresolved AOSP include directory: {value}")

    def files(self, name, seen=(), linkage=None):
        name = self.ensure(name)
        if name in seen:
            raise BuildError(f"cyclic source reference: {' -> '.join(seen + (name,))}")
        props = self.props(name, linkage)
        source_dir = self.source_dir(name)
        result = []
        for entry in props.get("srcs", []):
            if entry.startswith(":"):
                target = self.canonical(entry)
                target_kind = self.kind(target)
                if target_kind == "filegroup":
                    files = self.files(target, seen + (name,))
                    excluded = set(self.files_values(target, "exclude_srcs", seen + (name,)))
                    result += [path for path in files if path not in excluded]
                elif target_kind == "genrule":
                    result += [GENERATED[target]]
                else:
                    raise BuildError(f"unsupported source reference {entry} in {name}")
            else:
                pattern = source_dir / entry
                matches = sorted(Path(p) for p in __import__("glob").glob(str(pattern), recursive=True))
                if not matches:
                    raise BuildError(f"missing Blueprint source in {name}: {entry}")
                result += matches
        return result

    def files_values(self, name, field, seen=(), linkage=None):
        props = self.props(name, linkage)
        source_dir = self.source_dir(name)
        result = []
        for entry in props.get(field, []):
            if entry.startswith(":"):
                result += self.files(entry, seen)
            else:
                result += sorted(Path(p) for p in __import__("glob").glob(
                    str(source_dir / entry), recursive=True))
        return result

    def export_dirs(self, name, seen=()):
        canonical = self.canonical(name)
        if canonical in SYNTHETIC_HEADERS:
            return [str(path) for path in SYNTHETIC_HEADERS[canonical]]
        if canonical in PREBUILT_STATIC:
            return []
        name = self.ensure(canonical)
        if name in seen:
            return []
        props = self.props(name)
        source_dir = self.source_dir(name)
        result = [str(source_dir / path) for path in
                  props.get("export_include_dirs", []) + props.get("export_system_include_dirs", [])]
        for key in ("header_libs", "export_header_lib_headers",
                    "static_libs", "whole_static_libs"):
            for dependency in props.get(key, []):
                result += self.export_dirs(dependency, seen + (name,))
        for generator in props.get("export_generated_headers", []):
            target = GENERATED[self.canonical(generator)]
            result.append(str(target.parent if Path(target).is_file() else target))
        return list(dict.fromkeys(result))


GENERATED = {}
VERSION_SCRIPTS = {}


def generate_android_ids(destination):
    """Reproduce build/make/tools/fs_config/fs_config_generator.py aidarray.

    The pinned system/core slice does not carry the host generator, so the
    generator's verified logic (skip UNUSED/APP/USER and _START/_END, friendly
    lowercase names, fixed-up media names, identifier-valued entries) is
    applied directly to the pinned header.
    """
    header = EXT / "libcutils/include/private/android_filesystem_config.h"
    fixups = {"media_drm": "mediadrm", "media_ex": "mediaex", "media_codec": "mediacodec"}
    skip = (re.compile(r"AID_UNUSED[0-9].*"), re.compile(r"AID_APP"), re.compile(r"AID_USER"))
    define = re.compile(r"\s*#define\s+AID_.*")
    entries = []
    for line in header.read_text().splitlines():
        if not define.match(line):
            continue
        chunks = line.split()
        identifier, value = chunks[1], chunks[2]
        if any(pattern.match(identifier) for pattern in skip):
            continue
        if identifier.endswith(("_START", "_END")):
            continue
        friendly = identifier[len("AID_"):].lower()
        friendly = fixups.get(friendly, friendly)
        if len(friendly) > 31:
            raise BuildError(f"AID friendly name too long: {identifier}")
        entries.append((friendly, identifier))
    width = max(len(name) + 1 for name, _ in entries)
    body = ["/*", " * THIS IS AN AUTOGENERATED FILE! DO NOT MODIFY!", " */", "",
            "#include <private/android_filesystem_config.h>", "",
            "struct android_id_info {", f"    const char name[{width}];",
            "    unsigned aid;", "};", "",
            "static const struct android_id_info android_ids[] = {"]
    body += [f'    {{ "{name}", {identifier} }},' for name, identifier in entries]
    body += ["};", "",
             "#define android_id_count \\\n    (sizeof(android_ids) / sizeof(android_ids[0]))", ""]
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text("\n".join(body))
    return {name: identifier for name, identifier in entries}


class GuestBuild:
    def __init__(self, model, runner):
        self.model, self.runner, self.root = model, runner, ROOT
        self.built = {}
        self.building = []
        self.rust_built = {}
        self.rust_building = []

    # ---------------------------------------------------------------- generated
    def generate(self):
        directory = self.root / "gen"
        syscalls = directory / "syscalls-arm64" / "syscalls-arm64.S"
        if not syscalls.is_file():
            syscalls.parent.mkdir(parents=True, exist_ok=True)
            with syscalls.open("wb") as output:
                env = {**self.runner.env,
                       "PYTHONPATH": str(BIONIC / "libc/tools")}
                process = subprocess.run(
                    [sys.executable, "-B", str(BIONIC / "libc/tools/gensyscalls.py"),
                     "arm64", str(BIONIC / "libc/SYSCALLS.TXT")],
                    cwd=BIONIC / "libc", env=env, stdout=output, stderr=subprocess.PIPE,
                    timeout=120, check=False)
            if process.returncode:
                raise BuildError(f"gensyscalls failed: {process.stderr.decode(errors='replace')[-4000:]}")
        GENERATED["syscalls-arm64"] = syscalls
        ids = directory / "generated_android_ids" / "generated_android_ids.h"
        if not ids.is_file():
            generate_android_ids(ids)
        GENERATED["generated_android_ids"] = ids
        maps = self.generate_version_maps()
        return {"syscalls-arm64": str(syscalls), "generated_android_ids": str(ids), **maps}

    def generate_version_maps(self):
        plans = (
            ("libc.arm64.map", BIONIC / "libc/libc.map.txt",
             ("__libc_add_main_thread", "__libc_init_scudo",
              "__libc_set_target_sdk_version")),
            ("libdl.arm64.map", LIBDL_SRC / "libdl.map.txt", ()),
            ("libm.arm64.map", BIONIC / "libm/libm.map.txt", ()),
        )
        for name, source, extra in plans:
            output = self.root / "gen" / name / name
            output.parent.mkdir(parents=True, exist_ok=True)
            process = subprocess.run(
                [sys.executable, "-B", str(BIONIC / "tools/generate-version-script.py"),
                 "arm64", str(source), str(output)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60, check=False)
            if process.returncode or not output.is_file():
                raise BuildError(f"generate-version-script failed for {name}: "
                                 + process.stderr.decode(errors="replace")[-4000:])
            if extra:
                text = output.read_text()
                marker = re.search(r"\n\s*global:\s*\n", text)
                if marker is None:
                    raise BuildError(f"version script has no global block: {name}")
                insertion = "".join(f"    {symbol};\n" for symbol in extra)
                output.write_text(text[:marker.end()] + insertion + text[marker.end():])
            VERSION_SCRIPTS[name] = output
        return {name: str(path) for name, path in VERSION_SCRIPTS.items()}

    def rust_support(self):
        return rust_support_object(self.runner)

    # ------------------------------------------------------------------ compile
    @staticmethod
    def is_cpp(source):
        return Path(source).suffix in (".cc", ".cpp", ".cxx")

    @staticmethod
    def forced_c(name, source):
        # These native-bridge replacements are plain C despite their .cpp
        # names; AOSP's guest libc exports the unmangled names that the guest
        # libc version script and callers use.
        return name == "libnative_bridge_guest_libc" and Path(source).name in {
            "__libc_add_main_thread.cpp", "__libc_init_scudo.cpp",
            "__libc_set_target_sdk_version.cpp"}

    def include_flags(self, name, props):
        source_dir = self.model.source_dir(name)
        include = [str(source_dir)]
        include += [str(source_dir / path) for path in
                    props.get("local_include_dirs", []) + props.get("export_include_dirs", [])
                    + props.get("export_system_include_dirs", [])]
        include += [str(self.model.aosp_path(path)) for path in props.get("include_dirs", [])]
        for dependency in (props.get("header_libs", []) + props.get("static_libs", [])
                           + props.get("whole_static_libs", []) + props.get("shared_libs", [])):
            include += self.model.export_dirs(dependency)
        include += [str(target.parent if target.is_file() else target)
                    for target in (GENERATED[self.model.canonical(gen)]
                                   for gen in props.get("generated_headers", [])
                                   if self.model.canonical(gen) in GENERATED)]
        if name == "libnative_bridge_guest_libc":
            include += self.model.export_dirs("libc_headers")
            include += self.model.export_dirs("bionic_libc_platform_headers")
        return list(dict.fromkeys(path for path in include if Path(path).is_dir()))

    def compile_command(self, name, props, source, output):
        cpp = self.is_cpp(source) and not self.forced_c(name, source)
        suffix = Path(source).suffix
        tool = LLVM / ("clang++" if cpp else "clang")
        argv = [str(tool), "--no-default-config", TARGET, f"--sysroot={NDK}",
                "-fPIC", "-O0", "-g0", "-fno-lto", "-fstack-protector-strong",
                "-funwind-tables", *DEVICE_CFLAGS]
        stl = props.get("stl")
        if cpp:
            # Always drive libc++ explicitly. `--sysroot` points at the NDK, so
            # clang would otherwise inject that libc++ *after* the module's own
            # includes, which breaks libc++'s `<string.h>` overlay against
            # `bionic/libc/include`. Declaring it here puts libc++ first, so its
            # wrappers `#include_next` into bionic's headers, as in AOSP.
            argv += ["-nostdinc++", "-I", str(LIBCXX)]
            if LIBCXX_COMMON != LIBCXX:
                argv += ["-I", str(LIBCXX_COMMON)]
            if "noexcept" in self.linkage_hint(name):
                argv += ["-D_LIBCPP_HAS_NO_EXCEPTIONS"]
        del stl
        for directory in self.include_flags(name, props):
            argv += ["-I", directory]
        cflags = expand_flags(props.get("cflags", []))
        if suffix == ".S":
            # Soong's arm-optimized-routines `--include` only declares weak C
            # functions; it is not assembly input.
            cflags = [flag for flag in cflags if not flag.startswith(("-include", "--include"))]
        argv += list(PLATFORM_FLAGS) + cflags
        if suffix == ".S":
            argv += ["-D__ASSEMBLY__"] + expand_flags(props.get("asflags", []))
        elif cpp:
            argv += ["-std=" + props.get("cpp_std", "gnu++23"), "-fno-exceptions", "-fno-rtti",
                     "-fvisibility-inlines-hidden"]
            argv += expand_flags(props.get("cppflags", []))
        else:
            argv += ["-std=" + props.get("c_std", "gnu17")] + expand_flags(props.get("conlyflags", []))
            argv += expand_flags(props.get("cppflags", []))
        argv += list(TRAILING_FLAGS)
        if self.forced_c(name, source):
            argv += ["-x", "c"]
        argv += ["-c", str(source), "-o", str(output)]
        return argv

    def linkage_hint(self, name):
        # The linker's own TUs and the manually linked static runtime are the
        # no-exceptions libc++ variant; libbase/fmtlib are also linked into it.
        return "noexcept"

    def object_for(self, module, props, number, source, output_dir):
        output = output_dir / f"{number:03d}-{Path(source).stem}.o"
        argv = self.compile_command(module, props, source, output)
        digest = BOOT.sha256_file(Path(source))[:16] + "-" + command_digest(argv)
        sidecar = output.with_suffix(".cmd")
        if output.is_file() and sidecar.is_file() and sidecar.read_text() == digest:
            self.runner.report["reused_objects"] = self.runner.report.get("reused_objects", 0) + 1
            return output
        output.parent.mkdir(parents=True, exist_ok=True)
        self.runner.run(argv, f"compile {module}: {Path(source).name}")
        verify_elf_arm64(output, 1)
        sidecar.write_text(digest)
        return output

    # ------------------------------------------------------------------ targets
    def build(self, name, linkage=None):
        name = self.model.canonical(name)
        kind = self.model.kind(name)
        if kind == "cc_library_static":
            required = "static"
        elif kind == "cc_library_shared":
            required = "shared"
        else:
            required = None
        if required is not None:
            if linkage is not None and linkage != required:
                raise BuildError(f"requested {linkage} variant unavailable: {name} ({kind})")
            linkage = required
        elif kind == "cc_library" and linkage not in ("static", "shared"):
            raise BuildError(f"explicit static/shared variant required: {name} ({kind})")
        key = (name, linkage)
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
        return result

    def _build(self, name, linkage):
        kind = self.model.kind(name)
        props = self.model.props(name, linkage)
        if kind in ("rust_library", "rust_library_rlib", "rust_proc_macro"):
            return self.rust(name, props)
        if kind.startswith("cc_library_headers") or kind == "cc_defaults":
            raise BuildError(f"header/defaults module cannot be built: {name}")
        if props.get("sdk_version") not in (None, ""):
            raise BuildError(f"SDK/NDK variant is not the platform guest variant: {name}")
        if name == "native_bridge_guest_app_process":
            return self._build_app_process(name, props)
        if linkage == "shared":
            if name == "libm":
                return self._build_libm_shared(props)
            return self._build_shared(name, props)
        return self._build_archive(name, props)

    def _sources(self, name, props, linkage):
        sources = self.model.files(name, linkage=linkage)
        excluded = set(self.model.files_values(name, "exclude_srcs", linkage=linkage))
        sources = [source for source in sources if source not in excluded]
        for generator in props.get("generated_sources", []):
            target = self.model.canonical(generator)
            sources.append(GENERATED[target])
        return sources

    def _whole_inputs(self, props, rust_inputs):
        whole, normal = [], []
        for dependency in dict.fromkeys(props.get("whole_static_libs", [])):
            canonical = self.model.canonical(dependency)
            if canonical in PREBUILT_ARCHIVES:
                whole.append(str(PREBUILT_ARCHIVES[canonical]))
                normal.append(str(PREBUILT_ARCHIVES[canonical]))
                continue
            archive = self.build(canonical, "static")
            if archive["kind"] == "rlib":
                rust_inputs += archive["rust_inputs"] + [archive["output"]]
            else:
                if archive.get("output"):
                    whole.append(archive["output"])
                whole += archive.get("whole_inputs", [])
                normal += archive.get("link_inputs", [])
                rust_inputs += archive["rust_inputs"]
        return list(dict.fromkeys(whole)), list(dict.fromkeys(normal))

    def _build_archive(self, name, props):
        sources = self._sources(name, props, "static")
        if not sources and not props.get("whole_static_libs") and not props.get("static_libs"):
            raise BuildError(f"archive has neither sources nor static members: {name}")
        output_dir = self.root / "obj" / name
        objects = [self.object_for(name, props, number, source, output_dir)
                   for number, source in enumerate(sources)]
        archives, rust_inputs = [], []
        whole, normal = self._whole_inputs(props, rust_inputs)
        archives += normal
        static = []
        for dependency in dict.fromkeys(props.get("static_libs", [])):
            canonical = self.model.canonical(dependency)
            if canonical in PREBUILT_ARCHIVES:
                archives.append(str(PREBUILT_ARCHIVES[canonical]))
                continue
            archive = self.build(canonical, "static")
            static.append(canonical)
            archives += archive.get("link_inputs", [])
            rust_inputs += archive["rust_inputs"]
        shared = [self.model.canonical(dependency) for dependency in props.get("shared_libs", [])]
        output = self.root / "lib" / (name + ".a")
        output.parent.mkdir(parents=True, exist_ok=True)
        link_inputs = []
        if objects:
            self.runner.run([str(LLVM / "llvm-ar"), "rcsD", str(output), *map(str, objects)],
                            "archive " + name)
            if not output.is_file():
                raise BuildError(f"archive was not produced: {name}")
            link_inputs.append(str(output))
        else:
            # A group archive (only whole-static members) must not leave a
            # stale flattened archive from an earlier build on disk.
            output.unlink(missing_ok=True)
        link_inputs += archives
        link_inputs = list(dict.fromkeys(link_inputs))
        return {"module": name, "kind": "archive",
                "output": str(output) if output.is_file() else None,
                "objects": [str(path) for path in objects],
                "link_inputs": link_inputs,
                "whole_inputs": whole,
                "static_requirements": list(dict.fromkeys(static)),
                "shared_requirements": shared,
                "rust_inputs": list(dict.fromkeys(rust_inputs)),
                "bytes": output.stat().st_size if output.is_file() else 0,
                "sha256": sha256(output) if output.is_file() else None}

    def shared_input(self, dependency):
        dependency = self.model.canonical(dependency)
        if dependency == "ld-android":
            return str(LINKER_ARTIFACT)
        if dependency == "libnative_bridge_vdso":
            return str(VDSO_FILE)
        if dependency in PREBUILT_ARCHIVES:
            return str(PREBUILT_ARCHIVES[dependency])
        if dependency in self.model.modules:
            return self.build(dependency, "shared")["output"]
        raise BuildError(f"unresolved shared library dependency: {dependency}")

    def _build_shared(self, name, props):
        sources = self._sources(name, props, "shared")
        output_dir = self.root / "obj" / (name + "-shared")
        objects = [self.object_for(name, props, number, source, output_dir)
                   for number, source in enumerate(sources)]
        archives, rust_inputs = [], []
        whole, normal = self._whole_inputs(props, rust_inputs)
        for dependency in dict.fromkeys(props.get("static_libs", [])):
            canonical = self.model.canonical(dependency)
            if canonical in PREBUILT_ARCHIVES:
                archives.append(str(PREBUILT_ARCHIVES[canonical]))
                continue
            archive = self.build(canonical, "static")
            archives += archive.get("link_inputs", [])
            rust_inputs += archive["rust_inputs"]
        if name == "libnative_bridge_guest_libc":
            # AArch64 b.cond reaches only +/-1 MiB. libc_syscalls is rooted in
            # full by the version script, and placing it after libc_bionic puts
            # __set_errno_internal out of range under --gc-sections.
            syscalls = [path for path in whole if Path(path).name == "libc_syscalls.a"]
            if syscalls:
                whole = [path for path in whole if path not in syscalls]
                anchor = next((index for index, path in enumerate(whole)
                               if Path(path).name == "libc_bionic.a"), 0)
                whole[anchor:anchor] = syscalls
            archives.insert(0, str(PREBUILT / "libclang_rt.builtins-aarch64-android.a"))
        shared = [self.shared_input(dependency) for dependency in
                  dict.fromkeys(props.get("shared_libs", []))]
        stem = str(props.get("stem") or name)
        soname = stem + ".so"
        if name == "libnative_bridge_guest_libc":
            output = ROOT / "out" / "libnative_bridge_guest_libc.native_bridge"
        else:
            output = self.root / "lib" / soname
        output.parent.mkdir(parents=True, exist_ok=True)
        version_script = props.get("version_script")
        if version_script:
            version_script = VERSION_SCRIPTS.get(self.model.canonical(version_script))
            if version_script is None:
                raise BuildError(f"unresolved version script for {name}: {props.get('version_script')}")
        argv = [str(LLVM / "ld.lld"), "-m", "aarch64linux", "--threads=1", "-shared",
                "--no-undefined", "-soname=" + soname,
                "-z", "noexecstack", "-z", "relro", "-z", "now",
                "--build-id=md5", "--fatal-warnings", "--no-undefined-version",
                "--icf=safe", "--gc-sections", "--hash-style=both",
                *map(str, objects)]
        if version_script:
            argv += ["--version-script=" + str(version_script)]
            # Version-script globals must survive --gc-sections; weak libdl
            # stubs are otherwise discarded and the library comes out empty.
            for symbol in version_script_symbols(version_script):
                argv += ["-u", symbol]
        for flag in expand_linker_flags(props.get("ldflags", [])):
            argv.append(flag)
        argv += ["--start-group", *map(str, archives), "--end-group",
                 "--whole-archive", *map(str, whole), "--no-whole-archive",
                 "--no-as-needed", *map(str, shared),
                 "--whole-archive", *map(str, rust_inputs), "--no-whole-archive"]
        if name == "libnative_bridge_guest_libc":
            support = self.rust_support()
            argv.append(str(support))
            runtime = sorted((Path.home() / ".rustup/toolchains/stable-x86_64-unknown-linux-gnu"
                              / "lib/rustlib" / RUST_TARGET / "lib").glob("libcore-*.rlib"))
            alloc = sorted((Path.home() / ".rustup/toolchains/stable-x86_64-unknown-linux-gnu"
                            / "lib/rustlib" / RUST_TARGET / "lib").glob("liballoc-*.rlib"))
            panic_abort = sorted((Path.home() / ".rustup/toolchains/stable-x86_64-unknown-linux-gnu"
                                  / "lib/rustlib" / RUST_TARGET / "lib").glob("libpanic_abort-*.rlib"))
            builtins = sorted((Path.home() / ".rustup/toolchains/stable-x86_64-unknown-linux-gnu"
                               / "lib/rustlib" / RUST_TARGET / "lib").glob("libcompiler_builtins-*.rlib"))
            argv += [str(path) for group in (runtime, alloc, panic_abort, builtins) for path in group]
        argv += ["-o", str(output)]
        self.runner.run(argv, "link " + soname)
        verify_elf_arm64(output, 3)
        return {"module": name, "kind": "shared", "soname": soname,
                "output": str(output), "objects": [str(path) for path in objects],
                "link_inputs": [str(output)], "whole_inputs": whole,
                "static_requirements": [],
                "shared_requirements": [str(path) for path in shared],
                "rust_inputs": list(dict.fromkeys(rust_inputs)),
                "bytes": output.stat().st_size, "sha256": sha256(output)}

    def bootstrap_shared_inputs(self, props):
        # Soong defaults system_shared_libs to libc/libm/libdl; libm explicitly
        # narrows that to libc. Never substitute the NDK's stub DSOs or build
        # ordinary bionic libc instead of the NBS guest replacement.
        dependencies = props.get("shared_libs", []) + props.get(
            "system_shared_libs", ["libc", "libm", "libdl"])
        paths = {"libc": LIBC_ARTIFACT, "libdl": self.root / "lib/libdl.so",
                 "libm": self.root / "lib/libm.so", "ld-android": LINKER_ARTIFACT,
                 "libnative_bridge_vdso": VDSO_FILE}
        result = []
        for dependency in dict.fromkeys(dependencies):
            if dependency not in paths:
                raise BuildError(f"unsupported bootstrap shared dependency: {dependency}")
            path = paths[dependency]
            if dependency == "libm" and not path.exists():
                path = Path(self.build("libm", "shared")["output"])
            if not path.is_file():
                raise BuildError(f"missing built guest {dependency}: {path}; "
                                 "build the guest linker/libc first")
            verify_elf_arm64(path, 3)
            result.append(str(path))
        return result

    @staticmethod
    def bootstrap_crt(begin, end):
        paths = [CRT / (name + ".o") for name in (begin, end)]
        for path in paths:
            if not path.is_file():
                raise BuildError(f"missing ARM64 API-37 CRT: {path}")
            verify_elf_arm64(path, 1)
        return paths

    def link_bootstrap(self, name, argv, output, objects, shared, required_exports=()):
        check_output_path(output)
        output.parent.mkdir(parents=True, exist_ok=True)
        fd, temporary = tempfile.mkstemp(prefix=output.name + ".", dir=output.parent)
        os.close(fd)
        temporary = Path(temporary)
        try:
            self.runner.run([*map(str, argv), "-o", str(temporary)], "link " + name)
            artifact = inspect_runtime(temporary)
            validate_runtime_module("app_process" if name == "app_process64" else "libm", artifact)
            missing = set(required_exports) - set(artifact["exports"])
            if missing:
                raise BuildError(f"{name} is missing version-script exports: {sorted(missing)}")
            temporary.chmod(0o755 if name == "app_process64" else 0o644)
            temporary.replace(output)
        finally:
            temporary.unlink(missing_ok=True)
        return {"output": str(output), "objects": list(map(str, objects)),
                "shared_requirements": shared, "bytes": output.stat().st_size,
                "sha256": sha256(output)}

    def _build_app_process(self, name, props):
        if self.model.kind(name) != "cc_binary" or not props.get("enabled"):
            raise BuildError("NBS app_process must be the enabled native-bridge cc_binary")
        sources = self._sources(name, props, None)
        if sources != [NBS / "android_api/app_process/app_process.cc"]:
            raise BuildError(f"unexpected NBS app_process sources: {sources}")
        objects = [self.object_for(name, props, number, source,
                                   self.root / "obj" / name)
                   for number, source in enumerate(sources)]
        begin, end = self.bootstrap_crt("crtbegin_dynamic", "crtend_android")
        shared = self.bootstrap_shared_inputs(props)
        # With exceptions/RTTI disabled, this main has no C++ runtime references.
        # --as-needed drops the unused implicit libm/libdl (and no libc++ is
        # required); CRT's __libc_init and main's vDSO call retain their providers.
        argv = [LLVM / "ld.lld", "-m", "aarch64linux", "--threads=1", "-pie",
                "--entry=_start", "--dynamic-linker=" + GUEST_INTERPRETER,
                "--no-undefined", "--fatal-warnings", "--build-id=md5",
                "-z", "noexecstack", "-z", "relro", "-z", "now",
                "-z", "max-page-size=16384", "--gc-sections", "--hash-style=both",
                *expand_linker_flags(props.get("ldflags", [])), begin, *objects,
                "--as-needed", *shared,
                PREBUILT_ARCHIVES["libclang_rt.builtins-aarch64-android"], end]
        result = self.link_bootstrap("app_process64", argv, APP_PROCESS, objects, shared)
        return {"module": name, "kind": "binary", **result}

    def bootstrap_static_inputs(self, name, props):
        sources = self._sources(name, props, "static")
        objects = [self.object_for(name, props, number, source, self.root / "obj" / name)
                   for number, source in enumerate(sources)]
        if props.get("static_libs") or props.get("rustlibs"):
            raise BuildError(f"unsupported extra bootstrap static dependencies in {name}")
        archive = self.root / "lib" / (name + ".a")
        reused = False
        # Earlier flattened archives can contain duplicate or stale members.
        # Reuse only an exact byte-for-byte packing of the verified objects;
        # otherwise pass those objects directly, as Soong's shared variant does.
        if archive.is_file() and archive.stat().st_size <= FILE_LIMIT:
            members = subprocess.run([str(LLVM / "llvm-ar"), "t", str(archive)],
                                     capture_output=True, text=True, check=True, timeout=30).stdout.splitlines()
            if members == [path.name for path in objects]:
                packed = subprocess.run([str(LLVM / "llvm-ar"), "p", str(archive)],
                                        capture_output=True, check=True, timeout=30).stdout
                digest = hashlib.sha256()
                for path in objects:
                    with path.open("rb") as stream:
                        for block in iter(lambda: stream.read(1024 * 1024), b""):
                            digest.update(block)
                reused = hashlib.sha256(packed).digest() == digest.digest()
        self.runner.report.setdefault("bootstrap_archive_reuse", {})[name] = {
            "archive": str(archive), "reused": reused, "objects": len(objects),
            "reason": "exact verified object contents" if reused else "using verified objects; archive absent or not exact"}
        inputs = [archive] if reused else objects
        for dependency in dict.fromkeys(props.get("whole_static_libs", [])):
            dependency = self.model.canonical(dependency)
            inputs += self.bootstrap_static_inputs(dependency, self.model.props(dependency, "static"))
        return inputs

    def _build_libm_shared(self, props):
        static_props = self.model.props("libm", "static")
        sources = self._sources("libm", props, "shared")
        if sources != self._sources("libm", static_props, "static"):
            raise BuildError("cannot reuse static libm: shared sources differ")
        for source in sources:
            output = self.root / "obj/libm" / (source.stem + ".o")
            if (self.compile_command("libm", props, source, output)
                    != self.compile_command("libm", static_props, source, output)):
                raise BuildError(f"cannot reuse static libm: shared compile flags differ: {source}")
        # Both variants use -fPIC and identical source/compile rules. Validate
        # their cached source/command digests without rewriting old archives.
        inputs = self.bootstrap_static_inputs("libm", static_props)
        begin, end = self.bootstrap_crt("crtbegin_so", "crtend_so")
        shared = self.bootstrap_shared_inputs(props)
        version_script = VERSION_SCRIPTS.get(self.model.canonical(props.get("version_script", "")))
        if version_script is None:
            raise BuildError("missing generated libm.arm64.map")
        inputs = list(dict.fromkeys(inputs))
        argv = [LLVM / "ld.lld", "-m", "aarch64linux", "--threads=1", "-shared",
                "--no-undefined", "--fatal-warnings", "--no-undefined-version",
                "--soname=libm.so", "--build-id=md5", "--hash-style=both",
                "-z", "noexecstack", "-z", "relro", "-z", "now",
                "-z", "max-page-size=16384", "--gc-sections",
                "--version-script=" + str(version_script),
                *expand_linker_flags(props.get("ldflags", [])), begin]
        for symbol in version_script_symbols(version_script):
            argv += ["-u", symbol]
        argv += ["--whole-archive", *inputs, "--no-whole-archive",
                 "--start-group",
                 PREBUILT_ARCHIVES["libclang_rt.builtins-aarch64-android"], "--end-group",
                 "--as-needed", *shared, end]
        result = self.link_bootstrap("libm.so", argv, self.root / "lib/libm.so", [], shared,
                                     required_exports=version_script_symbols(version_script))
        return {"module": "libm", "kind": "shared", "soname": "libm.so",
                "reused_static_variant": True, "static_inputs": list(map(str, inputs)), **result}

    def stage_proc_macro(self, name, crate_root):
        """rustc 1.99 no longer puts `proc_macro` in the extern prelude for
        `--crate-type proc-macro`; the pinned compiler did. Stage an unmodified
        copy of the crate with the explicit standard-prelude item, leaving the
        pinned source tree untouched."""
        text = Path(crate_root).read_text()
        if re.search(r"^\s*extern\s+crate\s+proc_macro\s*;", text, re.MULTILINE):
            return Path(crate_root)
        source_dir = self.model.source_dir(name)
        stage = ROOT / "rust" / "proc-src" / name
        if stage.exists():
            shutil.rmtree(stage)
        shutil.copytree(source_dir, stage)
        staged = stage / Path(crate_root).relative_to(source_dir)
        lines = text.splitlines(keepends=True)
        index, block = 0, False
        while index < len(lines):
            stripped = lines[index].strip()
            if block:
                block = "*/" not in stripped
                index += 1
                continue
            if not stripped or stripped.startswith("//"):
                index += 1
                continue
            if stripped.startswith("/*"):
                block = "*/" not in stripped
                index += 1
                continue
            if stripped.startswith("#!["):
                depth = stripped.count("[") - stripped.count("]")
                index += 1
                while depth > 0 and index < len(lines):
                    depth += lines[index].count("[") - lines[index].count("]")
                    index += 1
                continue
            break
        staged.write_text("".join(lines[:index]) + "extern crate proc_macro;\n"
                          + "".join(lines[index:]))
        return staged

    # --------------------------------------------------------------------- rust
    def rust(self, name, props, host=False):
        """Build one Rust crate. Target rlibs and host proc-macro graphs are
        separate: proc macros must run on the build host while their consumers
        are AArch64 objects."""
        name = self.model.canonical(name)
        key = (name, host)
        if key in self.rust_built:
            return self.rust_built[key]
        if key in self.rust_building:
            raise BuildError(f"cyclic Rust dependency: {self.rust_building} -> {key}")
        self.rust_building.append(key)
        try:
            result = self._rust(name, props, host)
            self.rust_built[key] = result
        finally:
            self.rust_building.pop()
        return result

    def _rust(self, name, props, host):
        kind = self.model.modules[name][0]
        no_std = props.get("no_std") if isinstance(props.get("no_std"), dict) else None
        # Soong's no_std variant overrides features/rustlibs when it supplies
        # them; otherwise the crate's top-level values remain in effect.
        variant = no_std if (no_std and no_std.get("enabled") and not host) else None
        features = (variant or {}).get("features", props.get("features", []))
        rustlibs = (variant or {}).get("rustlibs", props.get("rustlibs", []))
        crate_name = props.get("crate_name") or name.removeprefix("lib").replace("-", "_")
        crate_root = props.get("crate_root")
        if crate_root is None:
            sources = self.model.files(name)
            if len(sources) != 1:
                raise BuildError(f"Rust module needs one crate root: {name}")
            crate_root = str(sources[0])
        else:
            crate_root = str(self.model.source_dir(name) / crate_root)
        if not Path(crate_root).is_file():
            raise BuildError(f"missing Rust crate root: {crate_root}")
        target = "x86_64-unknown-linux-gnu" if host else RUST_TARGET
        crate_type = "proc-macro" if kind == "rust_proc_macro" else "rlib"
        suffix = ".so" if crate_type == "proc-macro" else ".rlib"
        if crate_type == "proc-macro":
            crate_root = str(self.stage_proc_macro(name, crate_root))
        output = ROOT / "rust" / ("host" if host else "target") / ("lib" + crate_name + suffix)
        output.parent.mkdir(parents=True, exist_ok=True)
        externs, rust_inputs, dependency_dirs = [], [], []
        for dependency in rustlibs:
            archive = self.rust(self.model.canonical(dependency),
                                self.model.props(dependency), host=host)
            externs += ["--extern", f"{archive['crate_name']}={archive['output']}"]
            dependency_dirs.append(str(Path(archive["output"]).parent))
            if not host:
                rust_inputs += archive["rust_inputs"] + [archive["output"]]
        for dependency in props.get("proc_macros", []):
            macro = self.rust(self.model.canonical(dependency),
                              self.model.props(dependency), host=True)
            externs += ["--extern", f"{macro['crate_name']}={macro['output']}"]
            dependency_dirs.append(str(Path(macro["output"]).parent))
        search = ["-L", "dependency=" + str(ROOT / "rust" / ("host" if host else "target"))]
        if not host:
            # Proc-macro dylibs referenced by target rlib metadata live here.
            search += ["-L", "dependency=" + str(ROOT / "rust" / "host")]
        for directory in dict.fromkeys(dependency_dirs):
            search += ["-L", "dependency=" + directory]
        argv = [str(RUSTC), "--crate-name", crate_name, "--edition", str(props.get("edition", "2021")),
                "--crate-type", crate_type, "--target", target, *search,
                "-C", "opt-level=0", "-C", "debuginfo=0", "-C", "embed-bitcode=no",
                "-C", "panic=abort", "-C", "codegen-units=1",
                "-C", "debug-assertions=no", "-C", "overflow-checks=no",
                "-C", "relocation-model=pic",
                *( [] if host else ["-C", "target-feature=-sve2"] ),
                "--cap-lints", "allow", *externs]
        for feature in features:
            argv += ["--cfg", f'feature="{feature}"']
        argv += ["-o", str(output), crate_root]
        digest = BOOT.sha256_file(Path(crate_root))[:16] + "-" + command_digest(argv)
        sidecar = output.with_suffix(output.suffix + ".cmd")
        if not (output.is_file() and sidecar.is_file() and sidecar.read_text() == digest):
            env = dict(self.runner.env)
            version = str(props.get("cargo_pkg_version") or "0.0.0")
            core_version, _, pre = version.partition("-")
            parts = (core_version.split(".") + ["0", "0"])[:3]
            env.update({
                "CARGO_PKG_NAME": crate_name,
                "CARGO_PKG_VERSION": version,
                "CARGO_PKG_VERSION_MAJOR": parts[0],
                "CARGO_PKG_VERSION_MINOR": parts[1],
                "CARGO_PKG_VERSION_PATCH": parts[2],
                "CARGO_PKG_VERSION_PRE": pre,
                "CARGO_PKG_AUTHORS": "",
                "CARGO_PKG_DESCRIPTION": "",
                "CARGO_PKG_HOMEPAGE": "",
                "CARGO_PKG_REPOSITORY": "",
                "CARGO_PKG_LICENSE": "",
                "CARGO_PKG_LICENSE_FILE": "",
                "CARGO_PKG_RUST_VERSION": "",
                "CARGO_CRATE_NAME": crate_name,
                "CARGO_MANIFEST_DIR": str(self.model.source_dir(name)),
            })
            build_output = self.model.source_dir(name) / "out"
            if build_output.is_dir():
                # cargo_embargo commits the build script's OUT_DIR for crates
                # that use include!(concat!(env!("OUT_DIR"), ...)).
                env["OUT_DIR"] = str(build_output)
            self.runner.run(argv, f"rustc {crate_name}{' (host)' if host else ''}", env)
            sidecar.write_text(digest)
        return {"module": name, "crate_name": crate_name, "kind": "rlib",
                "proc_macro": crate_type == "proc-macro", "host": host,
                "output": str(output), "objects": [], "static_requirements": [],
                "rust_inputs": list(dict.fromkeys(rust_inputs + ([str(output)] if not host else []))),
                "bytes": output.stat().st_size, "sha256": sha256(output)}


class LinkerBuild:
    def __init__(self, model, build, runner):
        self.model, self.build, self.runner = model, build, runner

    def rust_support(self):
        """The no_std + panic=abort runtime contract for rustc 1.99.

        core's unwind tables carry a DW.ref to `rust_eh_personality`. Modern
        rustc relies on std (or a user-provided lang item) for it; the pinned
        AOSP no_std crates were built by an older rustc. Under panic=abort the
        routine is unreachable, so a hidden trap definition is sufficient and
        keeps libstd (and its ELF TLS) out of the loader.
        """
        directory = ROOT / "gen" / "rust-support"
        directory.mkdir(parents=True, exist_ok=True)
        source = directory / "rust_eh_personality.c"
        source.write_text('__attribute__((visibility("hidden"), noreturn))\n'
                          "void rust_eh_personality(void) { __builtin_trap(); }\n")
        output = directory / "rust_eh_personality.o"
        argv = [str(LLVM / "clang"), "--no-default-config", TARGET, f"--sysroot={NDK}",
                "-fPIC", "-O0", "-g0", "-ffunction-sections", "-fdata-sections",
                "-c", str(source), "-o", str(output)]
        if not output.is_file() or output.stat().st_mtime < source.stat().st_mtime:
            self.runner.run(argv, "compile rust personality support")
            verify_elf_arm64(output, 1)
        return output

    def compile_translation_units(self):
        name = self.model.canonical("native_bridge_guest_linker")
        props = self.model.props(name)
        sources = self.model.files(name)
        excluded = set(self.model.files_values(name, "exclude_srcs"))
        sources = [source for source in sources if source not in excluded]
        if len(sources) != 32:
            raise BuildError(f"expected the 32 REPORT translation units, found {len(sources)}: "
                             + ", ".join(Path(s).name for s in sources))
        output_dir = ROOT / "obj" / "native_bridge_guest_linker"
        objects = [self.build.object_for(name, props, number, source, output_dir)
                   for number, source in enumerate(sources)]
        return {"name": name, "props": props, "sources": [str(s) for s in sources],
                "objects": [str(o) for o in objects]}

    def link(self, units):
        version_script = NBS / "android_api/linker/linker.generic.map"
        if not version_script.is_file():
            raise BuildError(f"missing version script: {version_script}")
        archives, rust = [], []
        visited = set()

        def collect(module):
            module = self.model.canonical(module)
            if module in visited:
                return
            visited.add(module)
            archive = self.build.build(module, "static")
            for path in archive.get("link_inputs", []):
                if path not in archives:
                    archives.append(path)
            rust.extend(archive["rust_inputs"])
            for dependency in archive.get("static_requirements", []):
                if self.model.canonical(dependency) in self.model.modules \
                        and self.model.canonical(dependency) not in PREBUILT_STATIC:
                    collect(dependency)

        for module in ("liblinker_main", "liblinker_malloc", "liblinker_debuggerd_stub",
                       "libc_nomalloc", "libc_dynamic_dispatch", "libm",
                       "libziparchive", "libbase", "libz", "libasync_safe",
                       "liblog_for_runtime_apex"):
            collect(module)
        rust += [self.build.build(module)["output"]
                 for module in ("libbionic_panic_abort", "liblibc_alloc_rust")]
        pragmas = [p for p in ("libbionic_panic_abort", "liblibc_alloc_rust")
                   if p in self.build.built]
        rustlib = Path.home() / ".rustup/toolchains/stable-x86_64-unknown-linux-gnu/lib/rustlib" / RUST_TARGET / "lib"
        core = sorted(rustlib.glob("libcore-*.rlib"))
        alloc = sorted(rustlib.glob("liballoc-*.rlib"))
        panic_abort = sorted(rustlib.glob("libpanic_abort-*.rlib"))
        builtins = sorted(rustlib.glob("libcompiler_builtins-*.rlib"))
        if len(core) != 1 or len(alloc) != 1 or len(panic_abort) != 1 or len(builtins) != 1:
            raise BuildError(f"expected one Rust core/alloc/panic_abort/compiler_builtins rlib in {rustlib}")
        runtime = [str(PREBUILT / "libc++_static_noexcept.a"),
                   str(PREBUILT / "libc++abi_noexcept.a"),
                   str(PREBUILT / "libc++demangle_noexcept.a"),
                   str(PREBUILT / "libunwind.a"),
                   str(PREBUILT / "libclang_rt.builtins-aarch64-android.a")]
        for path in runtime + [version_script]:
            if not Path(path).is_file():
                raise BuildError(f"missing pinned prebuilt/version script: {path}")
        output = ROOT / "out" / "ld-android.raw.so"
        output.parent.mkdir(parents=True, exist_ok=True)
        support = self.rust_support()
        argv = [str(LLVM / "ld.lld"), "-m", "aarch64linux", "--threads=1", "-shared",
                "--no-undefined", "--pack-dyn-relocs=relr", "--use-android-relr-tags",
                "-Bsymbolic",
                "--exclude-libs=ALL", "--soname=ld-android.so",
                "-z", "noexecstack", "-z", "relro", "-z", "now",
                "--build-id=md5", "--fatal-warnings", "--no-undefined-version",
                "--icf=safe", "--gc-sections",
                "--version-script=" + str(version_script),
                *map(str, units["objects"]), str(support),
                "--start-group", *map(str, archives), *runtime,
                "--whole-archive", *rust, "--no-whole-archive",
                str(core[0]), str(alloc[0]), str(panic_abort[0]), str(builtins[0]), "--end-group",
                "-o", str(output)]
        self.runner.run(argv, "link ld-android.so")
        verify_elf_arm64(output, 3)
        prefixed = ROOT / "out" / "native_bridge_guest_linker.native_bridge"
        self.runner.run([str(LLVM / "llvm-objcopy"), "--prefix-symbols=__dl_",
                         str(output), str(prefixed)], "prefix symbols __dl_")
        verify_elf_arm64(prefixed, 3)
        shutil.copyfile(prefixed, ROOT / "out" / "ld-android.so")
        return {"raw": str(output), "output": str(prefixed),
                "soname_copy": str(ROOT / "out" / "ld-android.so"),
                "archives": archives, "rust_inputs": list(dict.fromkeys(rust)),
                "command": [str(p) for p in argv]}


def inspect_artifact(path, required):
    readelf = str(LLVM / "llvm-readelf")
    dynamic = subprocess.run([readelf, "--wide", "--dynamic", path],
                             capture_output=True, text=True, check=True).stdout
    symbols = subprocess.run([readelf, "--wide", "--dyn-syms", path],
                             capture_output=True, text=True, check=True).stdout
    header = subprocess.run([readelf, "-h", "-S", "-l", path],
                            capture_output=True, text=True, check=True).stdout
    everything = subprocess.run([readelf, "--wide", "--symbols", path],
                                capture_output=True, text=True, check=True).stdout
    sonames = re.findall(r"\(SONAME\).*?\[([^\]]+)\]", dynamic)
    needed = re.findall(r"\(NEEDED\).*?\[([^\]]+)\]", dynamic)
    entries = []
    for line in symbols.splitlines():
        fields = line.split()
        if len(fields) >= 8 and fields[0].endswith(":") and fields[6] != "UND":
            entries.append(fields[7].split("@", 1)[0])
    exports = set(entries)
    symtab_names = set()
    in_symtab = False
    for line in everything.splitlines():
        if line.strip().startswith("Symbol table '.symtab'"):
            in_symtab = True
            continue
        if line.strip().startswith("Symbol table '.") and "symtab" not in line:
            in_symtab = False
            continue
        if in_symtab:
            fields = line.split()
            if len(fields) >= 8 and fields[0].endswith(":"):
                symtab_names.add(fields[7].split("@", 1)[0])
    dynsym_count = len(re.findall(r"^\s+\d+:", symbols, re.MULTILINE))
    symtab_count = len(re.findall(r"^\s+\d+:", everything, re.MULTILINE)) - dynsym_count
    checks = {
        "elf64_little_aarch64": "ELF64" in header and "AArch64" in header and "DYN" in header,
        "soname_ld_android_so": sonames == ["ld-android.so"],
        "no_pt_interp": "INTERP" not in header,
        "gnu_hash": ".gnu.hash" in header,
        "android_relr": "ANDROID_RELR" in dynamic,
        "exports_present": sorted(set(required) - exports),
        "symtab_retained": ".symtab" in header and symtab_count > 0,
        "progname_symtab": "__dl___progname" in symtab_names or "__dl___loader_dlopen" in symtab_names,
        "needed": needed,
    }
    return {"path": str(path), "bytes": Path(path).stat().st_size, "sha256": sha256(path),
            "needed": needed, "soname": sonames, "dynsym_count": dynsym_count,
            "symtab_count": symtab_count, "required_missing": sorted(set(required) - exports),
            "checks": checks, "entry": any("Entry point address" in line for line in header.splitlines()
                                           if not line.rstrip().endswith("0x0"))}


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
    for name in ("logs", "tmp", "gen", "obj", "lib", "rust", "out"):
        path = ROOT / name
        if path.is_symlink():
            raise BuildError(f"symlink output refused: {path}")
        path.mkdir(exist_ok=True)


def atomic_json(path, data):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(data, indent=2) + "\n")
    temporary.replace(path)


REQUIRED_EXPORTS = (
    "__native_bridge_config_static_tls", "__native_bridge_get_host_pthread",
    "__loader_dlopen", "__loader_dlsym", "__loader_dladdr", "__loader_dlerror",
    "__loader_android_create_namespace", "__loader_android_dlopen_ext",
    "__loader_android_get_exported_namespace", "__loader_android_init_anonymous_namespace",
    "__loader_android_link_namespaces", "__loader_android_set_application_target_sdk_version",
)
REQUIRED_LIBC_EXPORTS = (
    "__libc_init", "malloc", "pthread_self", "__cxa_atexit", "__register_atfork",
    "__libc_add_main_thread", "__libc_init_scudo", "__libc_set_target_sdk_version",
)


def inspect_libc(path, required):
    readelf = str(LLVM / "llvm-readelf")
    dynamic = subprocess.run([readelf, "--wide", "--dynamic", path],
                             capture_output=True, text=True, check=True).stdout
    symbols = subprocess.run([readelf, "--wide", "--dyn-syms", path],
                             capture_output=True, text=True, check=True).stdout
    versions = subprocess.run([readelf, "--wide", "--version-info", path],
                              capture_output=True, text=True, check=True).stdout
    header = subprocess.run([readelf, "-h", "-S", "-l", path],
                            capture_output=True, text=True, check=True).stdout
    everything = subprocess.run([readelf, "--wide", "--symbols", path],
                                capture_output=True, text=True, check=True).stdout
    sonames = re.findall(r"\(SONAME\).*?\[([^\]]+)\]", dynamic)
    needed = re.findall(r"\(NEEDED\).*?\[([^\]]+)\]", dynamic)
    defined, undefined = set(), set()
    for line in symbols.splitlines():
        fields = line.split()
        if len(fields) >= 8 and fields[0].endswith(":"):
            name = fields[7].split("@", 1)[0]
            (undefined if fields[6] == "UND" else defined).add(name)
    nodes = sorted(set(re.findall(r"Name: (\S+)", versions)))
    dynsym_count = len(re.findall(r"^\s+\d+:", symbols, re.MULTILINE))
    symtab_count = len(re.findall(r"^\s+\d+:", everything, re.MULTILINE)) - dynsym_count
    checks = {
        "elf64_little_aarch64": "ELF64" in header and "AArch64" in header and "DYN" in header,
        "soname_libc_so": sonames == ["libc.so"],
        "no_pt_interp": "INTERP" not in header,
        "version_nodes": bool(nodes),
        "imports_intercept_symbol": "native_bridge_intercept_symbol" in undefined,
        "exports_present": sorted(set(required) - defined),
        "symtab_retained": ".symtab" in header and symtab_count > 0,
        "needed": needed,
    }
    return {"path": str(path), "bytes": Path(path).stat().st_size, "sha256": sha256(path),
            "soname": sonames, "needed": needed, "version_nodes": nodes,
            "dynsym_count": dynsym_count, "symtab_count": symtab_count,
            "imports": sorted(undefined), "undefined_count": len(undefined),
            "required_missing": sorted(set(required) - defined), "checks": checks}


BOOTSTRAP_PATHS = {
    "linker": "system/bin/arm64/linker64",
    "app_process64": "system/bin/arm64/app_process64",
    "libc": "system/lib64/arm64/libc.so",
    "libdl": "system/lib64/arm64/libdl.so",
    "libm": "system/lib64/arm64/libm.so",
    "vdso": "system/lib64/arm64/libnative_bridge_vdso.so",
}
REQUIRED_LIBM_EXPORTS = ("cos", "sin", "pow", "sqrt", "exp", "log", "fesetround")


def inspect_runtime(path):
    """ELF evidence shared by module acceptance and bootstrap closure checks."""
    path = Path(path)
    verify_elf_arm64(path, 3)
    text = subprocess.run(
        [str(LLVM / "llvm-readelf"), "--wide", "--file-header", "--program-headers",
         "--dynamic", "--dyn-syms", str(path)],
        capture_output=True, text=True, check=True, timeout=30).stdout
    exports, imports = set(), set()
    for line in text.splitlines():
        fields = line.split()
        if len(fields) >= 8 and fields[0].rstrip(":").isdigit() and fields[0].endswith(":"):
            name = fields[7].split("@", 1)[0]
            if fields[6] != "UND":
                exports.add(name)
            elif fields[4] != "WEAK":
                imports.add(name)
    entry = re.search(r"Entry point address:\s*(0x[0-9a-fA-F]+)", text)
    return {"path": str(path), "bytes": path.stat().st_size, "sha256": sha256(path),
            "soname": re.findall(r"\(SONAME\).*?\[([^\]]+)\]", text),
            "needed": re.findall(r"\(NEEDED\).*?\[([^\]]+)\]", text),
            "interpreter": re.findall(r"\[Requesting program interpreter: ([^\]]+)\]", text),
            "entry": int(entry[1], 16) if entry else 0,
            "pie": bool(re.search(r"\(FLAGS_1\).*\bPIE\b", text)),
            "exports": sorted(exports), "imports": sorted(imports), "required_missing": []}


def validate_runtime_module(module, artifact):
    if module == "app_process":
        if (artifact["soname"] or artifact["interpreter"] != [GUEST_INTERPRETER]
                or not artifact["entry"] or not artifact["pie"]):
            raise BuildError("guest app_process must be an ARM64 PIE with PT_INTERP " + GUEST_INTERPRETER)
        if set(artifact["needed"]) != {"libc.so", "linux-vdso.so.1"}:
            raise BuildError(f"unexpected app_process DT_NEEDED (unused runtime libraries?): {artifact['needed']}")
        missing = {"native_bridge_post_init", "__libc_init"} - set(artifact["imports"])
        if missing:
            raise BuildError(f"guest app_process is missing bootstrap imports: {sorted(missing)}")
    elif module == "libm":
        if artifact["soname"] != ["libm.so"] or artifact["interpreter"]:
            raise BuildError("guest libm must have SONAME libm.so and no PT_INTERP")
        missing = set(REQUIRED_LIBM_EXPORTS) - set(artifact["exports"])
        if missing:
            raise BuildError(f"guest libm is missing required exports: {sorted(missing)}")
        if artifact["needed"] != ["libc.so"]:
            raise BuildError(f"unexpected libm DT_NEEDED: {artifact['needed']}")
    else:
        raise BuildError(f"unsupported bootstrap module: {module}")


def bootstrap_artifacts(libc_result=None, libdl_result=None, libm_result=None, app_result=None):
    return {"linker": LINKER_ARTIFACT, "vdso": VDSO_FILE,
            "app_process64": Path(app_result["output"]) if app_result else APP_PROCESS,
            "libc": Path(libc_result["output"]) if libc_result else LIBC_ARTIFACT,
            "libdl": Path(libdl_result["output"]) if libdl_result else ROOT / "lib/libdl.so",
            "libm": Path(libm_result["output"]) if libm_result else ROOT / "lib/libm.so"}


def validate_bootstrap_closure(artifacts):
    expected = {"linker": ("ld-android.so", REQUIRED_EXPORTS),
                "vdso": ("linux-vdso.so.1", ("native_bridge_post_init",)),
                "libc": ("libc.so", REQUIRED_LIBC_EXPORTS),
                "libdl": ("libdl.so", ("dlopen", "dlsym")),
                "libm": ("libm.so", REQUIRED_LIBM_EXPORTS)}
    evidence, providers = {}, {}
    for label in BOOTSTRAP_PATHS:
        path = artifacts.get(label)
        if path is None or not Path(path).is_file():
            raise BuildError(f"cannot assemble guest bundle; missing {label}: {path}")
        artifact = inspect_runtime(path)
        evidence[label] = artifact
        if label == "app_process64":
            validate_runtime_module("app_process", artifact)
            continue
        soname, required = expected[label]
        if artifact["soname"] != [soname] or artifact["interpreter"]:
            raise BuildError(f"unexpected {label} SONAME/PT_INTERP: {artifact['soname']}, {artifact['interpreter']}")
        missing = set(required) - set(artifact["exports"])
        if missing:
            raise BuildError(f"{label} is missing required exports: {sorted(missing)}")
        providers[soname] = label
    if evidence["linker"]["needed"] or evidence["vdso"]["needed"]:
        raise BuildError("guest linker and preloaded vDSO must not have DT_NEEDED")
    validate_runtime_module("libm", evidence["libm"])
    resolutions = {}
    for label, artifact in evidence.items():
        resolutions[label] = {}
        for needed in artifact["needed"]:
            if needed not in providers:
                raise BuildError(f"bootstrap closure missing DT_NEEDED {needed} required by {label}")
            resolutions[label][needed] = BOOTSTRAP_PATHS[providers[needed]]
        # Include the two objects Digitalis preloads, then the transitive
        # dependency group. Do not let unrelated bundled DSOs mask an import.
        reachable, pending = set(), [label, "linker", "vdso"]
        while pending:
            current = pending.pop()
            if current in reachable:
                continue
            reachable.add(current)
            for needed in evidence[current]["needed"]:
                if needed not in providers:
                    raise BuildError(f"bootstrap closure missing DT_NEEDED {needed} required by {current}")
                pending.append(providers[needed])
        exports = set().union(*(set(evidence[item]["exports"]) for item in reachable))
        missing = set(artifact["imports"]) - exports
        if missing:
            raise BuildError(f"unresolved bootstrap imports in {label}: {sorted(missing)}")
    return {"validated": True, "interpreter": GUEST_INTERPRETER,
            "resident_sonames": {name: BOOTSTRAP_PATHS[providers[name]]
                                 for name in ("ld-android.so", "linux-vdso.so.1")},
            "needed_resolution": resolutions,
            "artifacts": {label: {key: value for key, value in artifact.items() if key != "exports"}
                          for label, artifact in evidence.items()}}


def check_output_path(path):
    path = Path(path)
    if ".." in path.parts or not path.is_relative_to(ROOT) or path == ROOT:
        raise BuildError(f"output must be inside the owned build root: {path}")
    for candidate in (ROOT, *(ROOT / parent for parent in reversed(path.relative_to(ROOT).parents))):
        if candidate.is_symlink() or (candidate.exists() and not candidate.is_dir()):
            raise BuildError(f"unsafe output directory refused: {candidate}")
    if path.is_symlink() or (path.exists() and not path.is_file()):
        raise BuildError(f"unsafe output file refused: {path}")


def assemble_bundle(libc_result=None, libdl_result=None, libm_result=None, app_result=None):
    """Validate before updating only known files in the owned, manifest-free bundle."""
    artifacts = bootstrap_artifacts(libc_result, libdl_result, libm_result, app_result)
    closure = validate_bootstrap_closure(artifacts)
    if not LD_CONFIG.is_file():
        raise BuildError(f"cannot assemble guest bundle; missing ld_config: {LD_CONFIG}")
    if BUNDLE != ROOT / "bundle":
        raise BuildError("bundle must be the owned build root's bundle directory")
    copies = {BUNDLE / BOOTSTRAP_PATHS[label]: source for label, source in artifacts.items()}
    copies[BUNDLE / "system/etc/ld.config.arm64.txt"] = LD_CONFIG
    for path in copies:
        check_output_path(path)
    allowed_dirs = {parent for path in copies for parent in path.parents if parent.is_relative_to(BUNDLE)}
    if BUNDLE.exists():
        for path in BUNDLE.rglob("*"):
            if path.is_symlink() or (path not in copies and path not in allowed_dirs):
                raise BuildError(f"unknown/symlink bundle entry refused (not removed): {path}")
    for destination, source in copies.items():
        mode = 0o755 if destination.parent == BUNDLE / "system/bin/arm64" else 0o644
        destination.parent.mkdir(parents=True, exist_ok=True)
        fd, temporary = tempfile.mkstemp(prefix=".bundle-", dir=destination.parent)
        temporary = Path(temporary)
        try:
            with os.fdopen(fd, "wb") as output, Path(source).open("rb") as stream:
                shutil.copyfileobj(stream, output)
            temporary.chmod(mode)
            temporary.replace(destination)
        finally:
            temporary.unlink(missing_ok=True)
    # ld-android.so is the running linker; linux-vdso.so.1 is supplied via
    # AT_SYSINFO_EHDR. Neither SONAME needs a duplicate file in the library dir.
    return {"root": str(BUNDLE), "manifest_free": True, "closure": closure,
            "default_namespace": "/system/lib64/arm64/bootstrap:/system/lib64/arm64",
            "alternate_lookup": ["system/bin/berberis/arm64/<name>",
                                 "system/lib64/berberis/arm64/<name>"],
            "alternate_lookup_note": "preferred per GetSystemPath if the berberis dir exists",
            "files": {str(path.relative_to(BUNDLE)): {
                "bytes": path.stat().st_size, "sha256": sha256(path),
                "mode": format(path.stat().st_mode & 0o777, "04o")}
                for path in sorted(copies)}}


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=int, default=14400, choices=range(60, 86401), metavar="60..86400")
    parser.add_argument("--stage", choices=("all", "compile", "rust", "link", "verify"), default="all")
    parser.add_argument("--module", choices=("libnative_bridge_guest_libc", "app_process", "libm"),
                        help="build a guest runtime module instead of the linker")
    parser.add_argument("--bundle", action="store_true",
                        help="validate and package the complete bootstrap (automatic for libc)")
    args = parser.parse_args(argv)
    if args.bundle and not args.module:
        parser.error("--bundle requires --module")
    if args.module in ("app_process", "libm") and args.stage not in ("all", "verify"):
        parser.error("app_process/libm support --stage all or verify")
    return args


def main():
    args = parse_args()
    prepare_root()
    with (ROOT / ".lock").open("a") as lock:
        import fcntl
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        run = Path(tempfile.mkdtemp(prefix="run-", dir=ROOT))
        for directory in ("logs", "tmp"):
            (run / directory).mkdir()
        report = {**OWNER, "target": "aarch64-linux-android37 native_bridge guest",
                  "module": args.module or "native_bridge_guest_linker",
                  "bionic_pin": BIONIC_PIN, "ext_pins": EXT_PINS,
                  "driver_sha256": sha256(Path(__file__).resolve()),
                  "run": str(run), "commands": [], "stages": {}, "artifact": None,
                  "blocker": None, "policy": {
                      "jobs": 1, "child_address_space_bytes": MEMORY,
                      "max_output_bytes_per_file": FILE_LIMIT,
                      "optimization": "-O0 -g0 -fno-lto", "target": TARGET,
                      "platform_flags": PLATFORM_FLAGS, "no_source_patches": True,
                      "variant": "arm64 / android / bionic / native_bridge"}}
        try:
            model = Blueprints()
            runner = Runner(run, report, args.seconds)
            build = GuestBuild(model, runner)
            linker = LinkerBuild(model, build, runner)
            if args.module not in ("app_process", "libm") or args.stage != "verify":
                report["stages"]["generated"] = build.generate()
            if args.module in ("app_process", "libm"):
                output = APP_PROCESS if args.module == "app_process" else ROOT / "lib/libm.so"
                if args.stage != "verify":
                    name = "native_bridge_guest_app_process" if args.module == "app_process" else "libm"
                    linked = build.build(name, "shared" if args.module == "libm" else None)
                    report["stages"]["link"] = linked
                report["artifact"] = inspect_runtime(output)
                validate_runtime_module(args.module, report["artifact"])
                if args.module == "libm" and args.stage != "verify":
                    missing = set(version_script_symbols(VERSION_SCRIPTS["libm.arm64.map"])) - set(
                        report["artifact"]["exports"])
                    if missing:
                        raise BuildError(f"guest libm version-script exports missing: {sorted(missing)}")
                if args.bundle:
                    report["bundle"] = assemble_bundle()
            elif args.module == "libnative_bridge_guest_libc":
                if not LINKER_ARTIFACT.is_file():
                    raise BuildError(f"guest linker artifact is missing: {LINKER_ARTIFACT}; "
                                     "run the linker build first")
                libdl = build.build("libdl", "shared")
                libc = build.build("libnative_bridge_guest_libc", "shared")
                report["stages"]["shared"] = {
                    "libdl": {k: libdl[k] for k in ("soname", "output", "bytes", "sha256")},
                    "libc": {k: libc[k] for k in ("soname", "output", "bytes", "sha256")}}
                report["artifact"] = inspect_libc(libc["output"], REQUIRED_LIBC_EXPORTS)
                if report["artifact"]["required_missing"]:
                    raise BuildError("guest libc is missing required exports: "
                                     + ", ".join(report["artifact"]["required_missing"]))
                failed = [key for key, check in report["artifact"]["checks"].items()
                          if key not in ("exports_present", "needed") and check is not True]
                if failed:
                    raise BuildError(f"guest libc acceptance check failed: {failed}: "
                                     f"{report['artifact']['checks']}")
                libm = build.build("libm", "shared")
                app = build.build("native_bridge_guest_app_process")
                report["stages"]["bootstrap"] = {"libm": libm, "app_process": app}
                report["bundle"] = assemble_bundle(libc, libdl, libm, app)
            else:
                if args.stage in ("all", "compile"):
                    units = linker.compile_translation_units()
                    report["stages"]["translation_units"] = {
                        "count": len(units["sources"]),
                        "sources": [str(Path(p).relative_to(REPO)) if Path(p).is_relative_to(REPO) else p
                                    for p in units["sources"]]}
                if args.stage in ("all", "rust"):
                    report["stages"]["rust"] = {
                        module: build.build(module)["output"]
                        for module in ("libbionic_panic_abort", "liblibc_alloc_rust", "libicu4x_bionic")}
                if args.stage in ("all", "link"):
                    units = locals().get("units") or linker.compile_translation_units()
                    linked = linker.link(units)
                    report["stages"]["link"] = {key: linked[key] for key in ("raw", "output", "soname_copy", "command")}
                    report["artifact"] = inspect_artifact(linked["output"], REQUIRED_EXPORTS)
                    if report["artifact"]["required_missing"]:
                        raise BuildError("final ELF is missing required dynsym exports: "
                                         + ", ".join(report["artifact"]["required_missing"]))
                    if report["artifact"]["needed"]:
                        raise BuildError(f"final ELF has unexpected DT_NEEDED: {report['artifact']['needed']}")
                    if not all(check is True for key, check in report["artifact"]["checks"].items()
                               if key not in ("exports_present", "needed")):
                        raise BuildError(f"final ELF acceptance check failed: {report['artifact']['checks']}")
                if args.stage in ("all", "verify") and report["artifact"] is None:
                    output = ROOT / "out" / "native_bridge_guest_linker.native_bridge"
                    report["artifact"] = inspect_artifact(output, REQUIRED_EXPORTS)
            report["verified"] = bool(report["artifact"]) and not report["artifact"]["required_missing"]
        except (BuildError, DRIVER.BuildError, OSError, ValueError, subprocess.SubprocessError) as error:
            report["blocker"] = str(error)
            print(str(error), file=sys.stderr)
        finally:
            report["reused_objects"] = report.get("reused_objects", 0)
            report["retry"] = [sys.executable, "-B", str(Path(__file__).resolve())] + sys.argv[1:]
            atomic_json(run / "report.json", report)
            atomic_json(ROOT / "report.json", report)
            print(f"Report: {ROOT / 'report.json'}", flush=True)
        return 0 if report.get("verified") else 1


if __name__ == "__main__":
    sys.exit(main())
