#!/usr/bin/env python3
"""Isolated host exec-region ABI/backend tests; never builds the production shim.

    python3 crates/pengudroid-shim/tests/bridge_exec.py

Uses the pinned Digitalis headers and unmodified anonymous allocation, mmap,
ExecRegion and raw-syscall sources. The only native-backend adapter is a tiny
cutils/log.h fatal-logging implementation under target/. No mmap or fd operation
is mocked. __ANDROID__/__BIONIC__ are not defined for the standalone host backend,
so Android-only fdsan tagging itself is not exercised. Production delegation
leaves that translator-owned code unchanged.
"""

from pathlib import Path
import hashlib
import json
import os
import platform
import shutil
import subprocess


SHIM = Path(__file__).resolve().parents[1]
REPO = SHIM.parents[1]
SOURCE = REPO / ".toolchain/nativebridge-deps/sources/digitalis"
OUT = REPO / "target/bridge-exec-probe"
FIXTURE = SHIM / "fixtures/bridge_exec.cpp"
ELF_CREATE = "_ZN8berberis26ExecRegionElfBackedFactory6CreateEm"
ANON_CREATE = "_ZN8berberis26ExecRegionAnonymousFactory6CreateEm"
LOG_HEADER = r"""// Test logging only; all allocation/fd/mmap code is upstream and unmodified.
#pragma once
#include <cstdio>
#include <cstdlib>
#define LOG_ALWAYS_FATAL(...) do { std::fprintf(stderr, __VA_ARGS__); std::fputc('\n', stderr); std::abort(); } while (false)
#define LOG_ALWAYS_FATAL_IF(condition, ...) do { if (condition) { LOG_ALWAYS_FATAL(__VA_ARGS__); } } while (false)
"""


def run(args, *, env, expected=0, diagnostic=None):
    result = subprocess.run(
        [str(arg) for arg in args], cwd=OUT, env=env, text=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=45,
    )
    if result.returncode != expected or (diagnostic and diagnostic not in result.stderr):
        raise RuntimeError(
            f"command returned {result.returncode}, expected {expected}: "
            f"{' '.join(map(str, args))}\n{result.stdout}{result.stderr}"
        )
    return result.stdout


def main():
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise SystemExit("requires Linux x86_64 glibc and the pinned Digitalis sources")
    rustc = os.environ.get("RUSTC") or shutil.which("rustc")
    if not rustc and (Path.home() / ".cargo/bin/rustc").is_file():
        rustc = str(Path.home() / ".cargo/bin/rustc")
    if not rustc:
        raise SystemExit("rustc is needed for the isolated, dependency-free hook cdylib")
    compilers = [os.environ.get("CXX", "c++")]
    clang = shutil.which("clang++")
    if not clang and Path("/usr/lib/llvm-22/bin/clang++").is_file():
        clang = "/usr/lib/llvm-22/bin/clang++"
    if clang and clang not in compilers:
        compilers.append(clang)

    OUT.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment.pop("LD_PRELOAD", None)
    environment.pop("LD_LIBRARY_PATH", None)
    inputs = [
        "base/include/berberis/base/exec_region.h",
        "runtime_primitives/include/berberis/runtime_primitives/exec_region_anonymous.h",
        "runtime_primitives/include/berberis/runtime_primitives/exec_region_elf_backed.h",
        "runtime_primitives/exec_region_anonymous.cc", "base/exec_region.cc",
        "base/mmap_posix.cc", "base/raw_syscall_x86_64.S", "exec_region/sections.ld",
    ]
    provenance = {name: hashlib.sha256((SOURCE / name).read_bytes()).hexdigest() for name in inputs}
    (OUT / "source-sha256.json").write_text(json.dumps(provenance, indent=2) + "\n")
    includes = [f"-I{SOURCE / 'base/include'}", f"-I{SOURCE / 'runtime_primitives/include'}",
                f"-I{SOURCE.parent / 'libbase/include'}"]
    common = ["-std=c++20", "-Wall", "-Wextra", "-Werror", "-fstack-protector-all", *includes]

    # Establish the C++ ABI using proper C++ constructors and an assembly adapter
    # before the module's explicit pointer ABI participates in any test.
    for index, cxx in enumerate(compilers):
        for optimization in ("-O0", "-O2"):
            native = OUT / f"native-abi-{index}-{optimization[1:]}"
            run([cxx, *common, optimization, "-fno-elide-constructors", "-DBRIDGE_EXEC_ABI",
                 FIXTURE, "-o", native], env=environment)
            print(f"compiler {index}/{optimization}: {run([native], env=environment)}", end="", flush=True)
    if clang:
        ir = OUT / "abi.ll"
        run([clang, "-std=c++20", *includes, "-O0", "-S", "-emit-llvm", "-DBRIDGE_EXEC_ABI",
             FIXTURE, "-o", ir], env=environment)
        definition = next(line for line in ir.read_text().splitlines()
                          if line.startswith("define ") and f"@{ANON_CREATE}(" in line)
        assert "sret(" in definition and "i64" in definition
        print(f"Clang ABI lowering: {definition}", flush=True)

    logging = OUT / "include/cutils/log.h"
    logging.parent.mkdir(parents=True, exist_ok=True)
    logging.write_text(LOG_HEADER)
    (OUT / "shim.rs").write_text(
        '#![crate_type = "cdylib"]\n'
        f'#[path = {json.dumps(str(SHIM / "src/bridge_exec.rs"))}]\nmod bridge_exec;\n'
        '#[no_mangle]\npub extern "C" fn __bridge_exec_probe_anchor() {}\n'
    )
    # Also exercise rustc's export list alongside a named LIBC version script,
    # as used by the real shim. The new #[export_name] needs no build.rs change.
    versions = OUT / "versions.map"
    versions.write_text("LIBC { global: __bridge_exec_probe_anchor; };\n")
    library = OUT / "libbridge_exec_probe.so"
    for profile, flags in (
        ("debug", []),
        ("release-lto", ["-Copt-level=z", "-Clto=yes", "-Ccodegen-units=1",
                         "-Cpanic=abort", "-Cstrip=symbols"]),
    ):
        run([rustc, "--edition=2021", "--crate-name", "bridge_exec_probe", "--crate-type", "cdylib",
             OUT / "shim.rs", *flags, "-C", f"link-arg=-Wl,--version-script={versions},--undefined-version",
             "-o", library], env=environment)
        exports = run(["readelf", "--dyn-syms", "--wide", library], env=environment)
        assert f" {ELF_CREATE}\n" in exports
        assert " __bridge_exec_probe_anchor" in exports
        version_info = run(["readelf", "--version-info", "--wide", library], env=environment)
        assert "Name: LIBC" in version_info
        preload = dict(environment, LD_PRELOAD=str(library))
        for index, cxx in enumerate(compilers):
            fake = OUT / f"libfake_delegate-{index}.so"
            backend = OUT / f"libanonymous_backend-{index}.so"
            driver = OUT / f"driver-{index}"
            shared = [cxx, *common, "-O2", "-fPIC", "-shared", "-fsemantic-interposition",
                      "-Wl,-z,noexecstack"]
            run([*shared, "-DBRIDGE_EXEC_FAKE_BACKEND", FIXTURE, "-o", fake], env=environment)
            # The three upstream C++ units and the raw syscall assembly are
            # compiled as shipped. ANDROID selects their logging header only;
            # it does not select Bionic or Android-only fd ownership branches.
            run([*shared, "-DANDROID", f"-I{OUT / 'include'}", "-DBRIDGE_EXEC_REAL_BACKEND",
                 FIXTURE, SOURCE / "runtime_primitives/exec_region_anonymous.cc",
                 SOURCE / "base/exec_region.cc", SOURCE / "base/mmap_posix.cc",
                 SOURCE / "base/raw_syscall_x86_64.S", "-o", backend], env=environment)
            run([cxx, *common, "-O2", "-DBRIDGE_EXEC_DRIVER", FIXTURE, "-ldl", "-o", driver],
                env=environment)
            # Without the hook the same internal call must reach the deliberately
            # failing original ELF factory, proving the interposition is real.
            run([driver, "fake", fake], env=environment, expected=90,
                diagnostic="original ELF-backed factory reached")
            for mode, delegate in (("fake", fake), ("real", backend)):
                print(f"{profile}/compiler-{index}/{mode}: "
                      f"{run([driver, mode, delegate], env=preload)}", end="", flush=True)
            eager = dict(preload, LD_BIND_NOW="1")
            run([driver, "real", backend], env=eager)
            for mode, message in (
                ("missing", "missing RTLD_NEXT ExecRegionAnonymousFactory::Create"),
                ("null", "null hidden result pointer"),
                ("oversize", "exceeds its fixed 512 KiB capacity"),
                ("overflow", "exceeds its fixed 512 KiB capacity"),
            ):
                run([driver, mode], env=preload, expected=127, diagnostic=message)
            run([driver, "local", fake], env=preload, expected=127,
                diagnostic="missing RTLD_NEXT ExecRegionAnonymousFactory::Create")
            print(f"{profile}/compiler-{index}: eager binding and explicit failure paths passed", flush=True)
    print("bridge exec-region standalone probes passed; no Cargo, ART, or production translator run")


if __name__ == "__main__":
    main()
