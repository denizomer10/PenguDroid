#!/usr/bin/env python3
"""Build a narrow, Android-only subset of the pinned Digitalis Blueprint rules.

Run: python3 -B tools/build-nativebridge.py [--offline] [--towards-bridge] [--module NAME ...]
     python3 -B tools/build-nativebridge.py --offline --check-platform [--platform-root PATH]
This is an early native build driver, NOT a complete Soong implementation. It
supports literal Blueprint values/list variables, defaults, x86_64 Android
variants, exported headers, filegroups, and simple Python genrules. Unsupported
rules fail closed. The default steps attempt exec_region, assembler, base, then
the Android ARM64 NativeBridge entry archive, not the broad berberis_all target.

Only .toolchain/nativebridge-build/ is written. No sources are patched. Compiler
runs are serial, capped in address space, CPU/wall time, and output size. Every
command, selected module, source pin and first blocker is retained in report.json.
A separately checksummed last-successful-early.json survives failed probes. Platform
imports are only inspected, not yet linked; they require the completed extractor
manifest to pin each imported file. Never inspect or repair an extraction stage.
"""

import argparse
import copy
import fcntl
import glob
import importlib.util
import json
import os
import re
import resource
import shlex
import signal
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
}
OWNER = {"tool": "PenguDroid nativebridge-build", "schema": 1}
# Goals, not source/flag lists: all module contents come from Android.bp.
EARLY_GOALS = ("libberberis_exec_region", "libberberis_assembler", "libberberis_base",
               "libberberis_native_bridge_arm64")
MEMORY = 1_250_000_000  # Virtual-address limit, leaving room for the Python parent.
FILE_LIMIT = 8 * 1024 * 1024
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

METADATA = {
    "name", "defaults", "host_supported", "device_supported", "native_bridge_supported",
    "vendor_available", "product_available", "ramdisk_available", "vendor_ramdisk_available",
    "recovery_available", "apex_available", "visibility", "licenses", "min_sdk_version",
    "sdk_version", "compile_multilib", "enabled", "tidy", "tidy_flags", "tidy_checks",
    "tidy_disabled_srcs", "tidy_checks_as_errors", "owner", "double_loadable",
    "unique_host_soname", "static_ndk_lib", "stubs", "header_abi_checker",
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
    "filegroup": {"srcs", "exclude_srcs"},
    "python_binary_host": {"main", "srcs", "libs"},
    "python_library_host": {"srcs", "libs"},
    "genrule": {"out", "srcs", "tools", "cmd"},
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
    BOOT.Artifact("soong_global", "build/soong", "6722dd8833db7482df1a2543ca3fcf67ddf0f7b1",
                  "extra/soong/global.go", blob="2188687602d4badafd300dff8c018c70cd5b8245",
                  subtree="cc/config/global.go", limit=100_000),
)


def verify_elf(path, elf_type):
    with Path(path).open("rb") as stream:
        header = stream.read(20)
    if (header[:6] != b"\x7fELF\x02\x01" or int.from_bytes(header[16:18], "little") != elf_type
            or int.from_bytes(header[18:20], "little") != 62):
        raise BuildError(f"not the required little-endian ELF64 x86_64 type {elf_type}: {path}")


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
    def __init__(self, roots):
        self.modules, self.resolved, self.used, self.inherited = {}, {}, {}, {}
        self.roots = roots
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

    def canonical(self, name):
        if name.startswith("//"):
            namespace, separator, module = name[2:].partition(":")
            if not separator or namespace not in self.roots:
                raise BuildError(f"unresolved Blueprint namespace: {name}")
            if module in self.modules and not self.modules[module][1].is_relative_to(self.roots[namespace]):
                raise BuildError(f"module outside declared namespace: {name}")
            name = module
        else:
            name = name.removeprefix(":")
        if not re.fullmatch(r"[A-Za-z0-9_.+-]+", name):
            raise BuildError(f"unsafe/unsupported Blueprint module name: {name}")
        return name

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
            for selector, allowed in (("arch", KNOWN_ARCHES), ("target", KNOWN_TARGETS), ("multilib", {"lib32", "lib64"})):
                variants = raw.get(selector, {})
                if not isinstance(variants, dict) or any(not isinstance(v, dict) for v in variants.values()):
                    raise BuildError(f"invalid {selector} selector map")
                if set(variants) - allowed:
                    raise BuildError(f"unsupported {selector} selectors: {sorted(set(variants) - allowed)}")
        except BuildError as error:
            raise BuildError(f"{name} ({path}): {error}") from error
        def paths(properties):
            for key, value in properties.items():
                if isinstance(value, dict):
                    paths(value)
                elif key in PATH_FIELDS:
                    properties[key] = [str(path.parent / p) if not p.startswith(":") else p for p in value]
                elif key == "main":
                    properties[key] = str(path.parent / value)

        paths(raw)
        result = {}
        for default in raw.get("defaults", []):
            default = self.canonical(default)
            if default in self.modules and self.modules[default][0] != "cc_defaults":
                raise BuildError(f"unsupported defaults module {default} for {name}")
            result = merge(result, self.raw_props(default, stack + (name,)))
        result = merge(result, raw)
        self.inherited[name] = result
        return copy.deepcopy(result)

    def props(self, name, stack=(), linkage=None, headers_only=False):
        name = self.canonical(name)
        key = (name, linkage, headers_only)
        if key in self.resolved:
            return self.resolved[key]
        raw = self.raw_props(name, stack)
        kind, path, _, _ = self.modules[name]
        # Merge defaults BEFORE applying variants: an arch-specific default must
        # override a module's general scalar, not vice versa.
        selectors = {"arch", "target", "multilib", "static", "shared"}
        selected = {k: v for k, v in raw.items() if k not in selectors}
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
        if kind not in RULE_FIELDS:
            raise BuildError(f"unsupported Blueprint rule {name} ({kind})")
        unsupported = set(selected) - METADATA - RULE_FIELDS[kind]
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
        if kind != "cc_defaults":
            if selected.get("enabled") is False or selected.get("device_supported") is False:
                raise BuildError(f"Android x86_64 variant disabled: {name}")
            if selected.get("compile_multilib", "both") not in ("both", "64", "first"):
                raise BuildError(f"not an x86_64 module: {name}")
        if "include_dirs" in selected:
            selected["include_dirs"] = [str(self.aosp_path(p)) for p in selected["include_dirs"]]
        for default in selected.get("defaults", []):
            self.props(default, stack + (name,), linkage=linkage, headers_only=headers_only)
        self.used[name] = {"kind": kind, "blueprint": str(path), "blueprint_sha256": BOOT.sha256_file(path),
                           "selected_properties": selected, "linkage": linkage,
                           "usage": "exported headers only" if headers_only else "compile"}
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
            if value.startswith(":"):
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
        resource.setrlimit(resource.RLIMIT_CPU, (90, 90))
        resource.setrlimit(resource.RLIMIT_FSIZE, (FILE_LIMIT, FILE_LIMIT))
        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))

    def run(self, argv, label, env=None):
        remaining = min(90, self.deadline - time.monotonic())
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
        if (record["children_peak_rss_kib"] + record["parent_peak_rss_kib"]) * 1024 > 1_500_000_000:
            raise BuildError("observed parent + child high-water RSS exceeds 1.5 GB")
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
        if module not in model.modules or model.modules[module][1] != blueprint:
            unresolved.append(module)
            return
        kind, _, tokens, variables = model.modules[module]
        props = Values(tokens, variables).value()
        if not isinstance(props, dict):
            raise BuildError(f"platform provider must have a property map: {module}")
        declarations[module] = {"kind": kind, "properties": props}
        for default in props.get("defaults", []):
            visit(default, blueprint, declarations, unresolved)

    for name, specification in PLATFORM_SHARED.items():
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
        if name not in PLATFORM_SHARED:
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
        specification = PLATFORM_SHARED[name]
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


class NativeBuild:
    def __init__(self, model, runner, sysroot):
        self.model, self.runner, self.root, self.sysroot = model, runner, runner.root, sysroot
        self.generated = {}

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
        if not any(argv[:len(prefix)] == prefix for prefix in map(shlex.split, tools.values())):
            raise BuildError(f"genrule must invoke a declared Python tool directly: {name}")
        if not outputs:
            raise BuildError(f"genrule has no declared outputs: {name}")
        env = {**self.runner.env, "PYTHONPATH": os.pathsep.join(dict.fromkeys(python_paths))}
        self.runner.run(argv, "generate " + name, env)
        if not all(p.is_file() for p in outputs):
            raise BuildError(f"generator did not produce declared outputs: {name}")
        self.generated[name] = directory
        return directory

    def exports(self, name, seen=(), linkage="static"):
        name = self.model.canonical(name)
        if name in seen:
            raise BuildError(f"cyclic header reexport: {' -> '.join(seen + (name,))}")
        props = self.model.props(name, seen, linkage=linkage, headers_only=True)
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
                result += self.exports(dep, seen + (name,), linkage="shared" if key == "export_shared_lib_headers" else "static")
        result += [str(self.generate(gen)) for gen in props.get("export_generated_headers", [])]
        return list(dict.fromkeys(result))

    def compile_command(self, props, source, obj, includes):
        suffix = Path(source).suffix
        cpp = suffix in (".cc", ".cpp", ".cxx")
        if suffix not in (".c", ".S") and not cpp:
            raise BuildError(f"unsupported source language: {source}")
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
        return argv + ["-O0", "-g0", "-fno-lto", "-c", str(source), "-o", str(obj)]

    def build(self, name):
        name = self.model.canonical(name)
        props = self.model.props(name)
        kind, blueprint, _, _ = self.model.modules[name]
        if kind not in ("cc_library_static", "cc_library_shared"):
            raise BuildError(f"only explicit static/shared modules supported for early compile: {name} ({kind})")
        if props.get("sdk_version") not in (None, ""):
            raise BuildError(f"SDK module compilation is not the API37 platform variant: {name}")
        if props.get("stl") not in (None, "libc++") or "system_shared_libs" in props:
            raise BuildError(f"non-default STL/system libraries not implemented for compilation: {name}")
        includes = [str(ROOT / "extra/platform-headers"), str(DEPS / "sources/logging/liblog/include"),
                    str(blueprint.parent)] + props.get("export_include_dirs", []) + props.get("local_include_dirs", []) + props.get("include_dirs", [])
        for key in ("header_libs", "static_libs", "whole_static_libs", "shared_libs"):
            for dep in props.get(key, []):
                includes += self.exports(dep, (name,), linkage="shared" if key == "shared_libs" else "static")
        includes += [str(self.generate(gen)) for gen in props.get("generated_headers", [])]
        if props.get("generated_sources"):
            raise BuildError(f"generated-source compilation not implemented: {name}: {props['generated_sources']}")
        sources = self.model.files(props.get("srcs", []))
        excluded = set(self.model.files(props.get("exclude_srcs", [])))
        sources = [s for s in sources if s not in excluded]
        platform = DEPS / "toolchain/libcxx/android/x86_64"

        objects = []
        output_dir = self.root / "obj" / name
        output_dir.mkdir(parents=True)
        for number, source in enumerate(sources):
            obj = output_dir / f"{number:03d}-{Path(source).stem}.o"
            argv = self.compile_command(props, source, obj, includes)
            self.runner.run(argv, f"compile {name}: {Path(source).name}")
            verify_elf(obj, 1)
            objects.append(obj)
        result = {"module": name, "objects": [str(p) for p in objects], "kind": kind}
        output = self.root / "lib" / (name + (".a" if kind == "cc_library_static" else ".so"))
        if kind == "cc_library_static":
            if props.get("whole_static_libs"):
                raise BuildError(f"whole-static archive merging not implemented: {name}")
            self.runner.run([str(LLVM / "llvm-ar"), "rcsD", str(output), *map(str, objects)], "archive " + name)
        else:
            if any(props.get(k) for k in ("static_libs", "whole_static_libs", "shared_libs", "ldflags")):
                raise BuildError(f"full dependency/shared link not implemented: {name}; no missing library will be stubbed")
            libdir = self.sysroot / "usr/lib/x86_64-linux-android/37"
            argv = [str(LLVM / "ld.lld"), "--threads=1", "-m", "elf_x86_64", "-shared", "--no-undefined",
                    "-z", "max-page-size=16384", "--soname=" + output.name]
            for script in props.get("linker_scripts", []):
                argv += ["-T", script]
            argv += [str(libdir / "crtbegin_so.o"), *map(str, objects), "--as-needed",
                     str(platform / "lib/libc++.so"), f"-L{libdir}", "-lc", "-lm", "-ldl",
                     str(DEPS / "toolchain/libclang_rt.builtins-x86_64-android.a"),
                     str(libdir / "crtend_so.o"), "-o", str(output)]
            self.runner.run(argv, "link " + name)
            verify_elf(output, 3)
        result.update(output=str(output), bytes=output.stat().st_size, sha256=BOOT.sha256_file(output))
        self.runner.run([str(LLVM / "llvm-readelf"), "-h", "-s", str(output)], "inspect " + name)
        result["elf_machine"] = "x86_64"
        result["runtime_tested"] = False
        if name == "libberberis_native_bridge_arm64":
            record = self.runner.run([str(LLVM / "llvm-nm"), "--defined-only", str(output)], "verify bridge entry archive")
            if not re.search(r"(?m)^\S+ [BD] NativeBridgeItf$", Path(record["log"]).read_text()):
                raise BuildError("NativeBridgeItf global definition missing from the bridge entry archive")
            result["native_bridge_itf_archive_definition"] = True
        return result


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
                if (not path.resolve().is_relative_to((run / "obj" / name).resolve())
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
    parser.add_argument("--seconds", type=int, default=180, choices=range(1, 601), metavar="1..600")
    parser.add_argument("--offline", action="store_true", help="refuse missing extra-source downloads")
    parser.add_argument("--towards-bridge", action="store_true", help="after early goals, attempt libberberis_arm64")
    parser.add_argument("--platform-root", type=Path, default=PLATFORM_ROOT, help="read-only completed API37 GSI extraction")
    parser.add_argument("--check-platform", action="store_true", help="only verify platform shared imports; do not compile")
    args = parser.parse_args()
    if args.check_platform and (args.module or args.towards_bridge):
        parser.error("--check-platform cannot be combined with build goals")
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
                      "offline": args.offline, "no_source_patches": True,
                      "platform_root": str(args.platform_root), "platform_root_access": "read-only; completed manifest only",
                      "platform_linking_implemented": False,
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
            model = Blueprints(roots)
            report["platform_sources"] = describe_platform_sources(model)
            # Keep proof that the Android root retains its real bridge entry dependency.
            report["bridge_goal"] = model.props("libberberis_arm64")
            if "libberberis_native_bridge_arm64" not in report["bridge_goal"].get("whole_static_libs", []):
                raise BuildError("Android bridge goal lost its real native-bridge entry dependency")
            runner = Runner(run, report, args.seconds)
            build = NativeBuild(model, runner, Path(manifest["preflight"]["sysroot"]))
            version = runner.run([str(LLVM / "clang++"), "--no-default-config", "--version"], "compiler version")
            report["compiler_version"] = Path(version["log"]).read_text()
            imports = PlatformImports(args.platform_root, runner, report["platform_imports"])
            platform_ready = imports.probe()
            if args.check_platform:
                report["goals"] = ["verify platform shared imports"]
                if not platform_ready:
                    blockers = [f"{n}: {r['blocker']}" for n, r in report["platform_imports"].items() if r["status"] == "blocked"]
                    raise BuildError("platform shared imports not ready:\n" + "\n".join(blockers))
                report["requested_goals_passed"] = True
                report["blocker"] = "Platform inputs inspected only; source/header/codegen and full shared-link closure remain."
                return 0
            goals: list[str] = list(args.module or EARLY_GOALS)
            if args.towards_bridge and "libberberis_arm64" not in goals:
                goals.append("libberberis_arm64")
            report["goals"] = goals
            for goal in goals:
                report["attempting"] = goal
                report["artifacts"].append(build.build(goal))
                built = {a["module"] for a in report["artifacts"]}
                report["early_steps_passed"] = set(EARLY_GOALS) <= built
            report["requested_goals_passed"] = True
            report["blocker"] = "Dependency/whole-static closure and libberberis_arm64 shared link remain unimplemented."
        except (BuildError, BOOT.BootstrapError, OSError, ValueError, subprocess.SubprocessError) as error:
            report["blocker"] = str(error)
            print(str(error), file=sys.stderr)
        finally:
            if model is not None:
                report["modules"] = model.used
            report["retry"] = [sys.executable, "-B", str(Path(__file__).resolve())] + sys.argv[1:]
            publish_report(report)
            print(f"Report: {ROOT / 'report.json'}\nTranslator built: NO", flush=True)
        return 0 if report.get("requested_goals_passed") else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (BuildError, OSError, ValueError) as error:
        print(f"build-nativebridge: {error}", file=sys.stderr)
        raise SystemExit(1)
