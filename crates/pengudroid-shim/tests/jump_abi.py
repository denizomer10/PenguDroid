#!/usr/bin/env python3
"""Run only the shim jump ABI probes; no Cargo, ART, sysroot, or rootfs changes.

    python3 crates/pengudroid-shim/tests/jump_abi.py

Artifacts remain under target/jump-abi-probe/. The real build.rs generates the
assembly object and export map. A tiny standalone Rust cdylib supplies rustc's
normal export-hiding rules, without building any workspace dependency. Only the
jump object is linked; unrelated generated C shims are not loaded/interposed.
"""

from pathlib import Path
import os
import platform
import shutil
import subprocess


SHIM = Path(__file__).resolve().parents[1]
REPO = SHIM.parents[1]
OUT = REPO / "target/jump-abi-probe"
SYMBOLS = ("sigsetjmp", "siglongjmp", "longjmp", "_longjmp")


def run(args, *, env=None):
    result = subprocess.run(
        [str(arg) for arg in args], cwd=REPO, env=env, text=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=40,
    )
    if result.returncode:
        raise RuntimeError(
            f"command failed ({result.returncode}): {' '.join(map(str, args))}\n"
            f"{result.stdout}{result.stderr}"
        )
    return result.stdout


def main():
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise SystemExit("this probe requires Linux x86_64 glibc")
    cc = os.environ.get("CC", "cc")
    rustc = os.environ.get("RUSTC") or shutil.which("rustc")
    if not rustc:
        candidate = Path.home() / ".cargo/bin/rustc"
        if candidate.is_file():
            rustc = str(candidate)
    if not rustc:
        raise SystemExit("rustc is required for the standalone cdylib export probe")
    OUT.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment.pop("LD_PRELOAD", None)
    environment.pop("LD_LIBRARY_PATH", None)
    build_environment = dict(
        environment, OUT_DIR=str(OUT), CARGO_CFG_TARGET_ARCH="x86_64",
        CARGO_CFG_TARGET_OS="linux", CARGO_CFG_TARGET_ENV="gnu",
        CARGO_CFG_TARGET_POINTER_WIDTH="64",
    )
    run([rustc, "--edition=2021", SHIM / "build.rs", "-o", OUT / "build-script"],
        env=environment)
    directives = run([OUT / "build-script"], env=build_environment)
    (OUT / "build-directives.txt").write_text(directives)

    prefix = "cargo:rustc-cdylib-link-arg="
    link_args = []
    for line in directives.splitlines():
        if not line.startswith(prefix):
            continue
        arg = line[len(prefix):]
        if arg.startswith("-Wl,--undefined="):
            if arg.removeprefix("-Wl,--undefined=") in SYMBOLS:
                link_args.append(arg)
        elif arg == str(OUT / "jump.o") or arg.startswith("-Wl,--version-script="):
            link_args.append(arg)
    assert str(OUT / "jump.o") in link_args
    assert not any(line.startswith("cargo:rustc-link-arg=") for line in directives.splitlines())
    for symbol in SYMBOLS:
        assert f"-Wl,--undefined={symbol}" in link_args

    empty = OUT / "empty.rs"
    empty.write_text('// Standalone export-list probe, not a workspace build.\n#![crate_type = "cdylib"]\n')
    library = OUT / "libjump_probe.so"
    for profile, rust_flags in (
        ("debug", []),
        ("release-lto", ["-Copt-level=z", "-Clto=yes", "-Ccodegen-units=1",
                         "-Cpanic=abort", "-Cstrip=symbols"]),
    ):
        command = [rustc, "--edition=2021", "--crate-name", "jump_export_probe",
                   "--crate-type", "cdylib", empty, "-o", library, *rust_flags]
        for arg in link_args:
            command.extend(["-C", f"link-arg={arg}"])
        run(command, env=environment)
        exported = run(["readelf", "--dyn-syms", "--wide", library], env=environment)
        for symbol in SYMBOLS:
            assert f" {symbol}@@LIBC" in exported, f"missing exported {symbol}"
        assert "UND siglongjmp@GLIBC_2.2.5" in exported
        print(f"{profile}: four LIBC exports and versioned host fallback verified", flush=True)
        preload = dict(environment, LD_PRELOAD=str(library))
        configurations = (
            ("o0", ["-O0", "-fno-omit-frame-pointer"]),
            ("o2", ["-O2", "-fomit-frame-pointer"]),
            ("o2-fp", ["-O2", "-fno-omit-frame-pointer"]),
            ("fortify", ["-O2", "-D_FORTIFY_SOURCE=2", "-fcf-protection=full"]),
        )
        for name, flags in configurations:
            executable = OUT / f"jump-abi-{name}"
            run([cc, "-std=gnu11", "-Wall", "-Wextra", "-Werror",
                 "-fstack-protector-all", *flags, SHIM / "fixtures/jump_abi.c",
                 f"-L{OUT}", "-ljump_probe", f"-Wl,-rpath,{OUT}", "-pthread", "-ldl",
                 "-o", executable], env=environment)
            print(f"{profile}/{name}:\n{run([executable], env=preload)}", end="", flush=True)
        eager = dict(preload, LD_BIND_NOW="1")
        print(f"{profile}/eager-binding:\n{run([OUT / 'jump-abi-o2'], env=eager)}",
              end="", flush=True)
        # This binary has no dependency on the probe DSO: its ordinary jump
        # imports are GLIBC-versioned, exactly as in an unrelated host caller.
        host = OUT / "jump-abi-host-only"
        run([cc, "-std=gnu11", "-Wall", "-Wextra", "-Werror", "-O2",
             "-fstack-protector-all", "-DPD_HOST_ONLY", SHIM / "fixtures/jump_abi.c",
             "-pthread", "-ldl", "-o", host], env=environment)
        imports = run(["readelf", "--dyn-syms", "--wide", host], env=environment)
        assert "longjmp@GLIBC_2.2.5" in imports
        print(f"{profile}/independent-host:\n{run([host], env=preload)}", end="", flush=True)
    print("jump ABI standalone probes passed (live CET support, if absent, is explicitly skipped)")


if __name__ == "__main__":
    main()
