#!/usr/bin/env python3
"""Standalone syscall path/export probes. No Cargo, ART or production build.

    python3 crates/pengudroid-shim/tests/syscall_paths.py

Artifacts and dummy root/overlay trees live only under target/syscall-paths-probe.
The real build.rs supplies the objects and version map. The isolated Rust DSO
contains the *actual* rootfs mapper prefix, not its unrelated libc interposers.
Its only edit is replacing the libc import with equivalent core FFI types.
Link-time test wrappers count map/free ownership and deliberately clobber errno.
"""
from pathlib import Path
import hashlib

import os
import platform
import shutil
import subprocess

SHIM = Path(__file__).resolve().parents[1]
REPO = SHIM.parents[1]
OUT = REPO / "target/syscall-paths-probe"
WRAPPERS = r"""
#include <errno.h>
#include <stddef.h>
#include <stdatomic.h>
extern char *__real___pengudroid_map_path(const char *);
extern void __real___pengudroid_free_path(char *);
static _Atomic size_t allocated, freed;
char *__wrap___pengudroid_map_path(const char *p) {
    char *mapped = __real___pengudroid_map_path(p);
    if (mapped) ++allocated;
    errno = EUCLEAN;
    return mapped;
}
void __wrap___pengudroid_free_path(char *p) {
    if (p) ++freed;
    __real___pengudroid_free_path(p);
    errno = ERANGE;
}
size_t __pd_native_count(int which) { return which ? freed : allocated; }
"""


def run(args, env):
    p = subprocess.run([str(a) for a in args], cwd=OUT, env=env, text=True,
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=45)
    if p.returncode:
        raise RuntimeError(f"failed ({p.returncode}): {' '.join(map(str, args))}\n{p.stdout}{p.stderr}")
    return p.stdout


def main():
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise SystemExit("requires Linux x86_64 GNU LP64")
    rustc = os.environ.get("RUSTC") or shutil.which("rustc")
    if not rustc and (Path.home() / ".cargo/bin/rustc").is_file():
        rustc = str(Path.home() / ".cargo/bin/rustc")
    if not rustc:
        raise SystemExit("rustc is needed for the isolated export-list probe")
    cc = os.environ.get("CC", "cc")
    OUT.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    for variable in ("LD_PRELOAD", "LD_LIBRARY_PATH", "PENGUDROID_ROOT", "PENGUDROID_ARM64_GUEST_ROOT"):
        environment.pop(variable, None)
    build_env = dict(environment, OUT_DIR=str(OUT), CARGO_CFG_TARGET_ARCH="x86_64",
                     CARGO_CFG_TARGET_OS="linux", CARGO_CFG_TARGET_ENV="gnu",
                     CARGO_CFG_TARGET_POINTER_WIDTH="64")
    run([rustc, "--edition=2021", SHIM / "build.rs", "-o", OUT / "build-script"], environment)
    directives = run([OUT / "build-script"], build_env)
    (OUT / "build-directives.txt").write_text(directives)
    link_args = []
    prefix = "cargo:rustc-cdylib-link-arg="
    for line in directives.splitlines():
        if line.startswith(prefix):
            arg = line[len(prefix):]
            if (arg in (str(OUT / "syscall_paths_c.o"), str(OUT / "syscall_paths_asm.o"),
                        "-Wl,--undefined=syscall") or arg.startswith("-Wl,--version-script=")):
                link_args.append(arg)
    assert str(OUT / "syscall_paths_asm.o") in link_args
    assert str(OUT / "syscall_paths_c.o") in link_args
    assert "-Wl,--undefined=syscall" in link_args

    rootfs = (SHIM / "src/rootfs.rs").read_text()
    mapper = rootfs[:rootfs.index("/// Run `f` with a mapped path")]
    mapper = mapper.replace("use libc::{c_char, c_int, c_void};", "use core::ffi::c_char;")
    assert "libc::" not in mapper
    (OUT / "rootfs-mapper.rs").write_text(mapper)
    (OUT / "rootfs-sha256.txt").write_text(hashlib.sha256(rootfs.encode()).hexdigest() + "\n")
    (OUT / "probe.rs").write_text(
        '#![crate_type = "cdylib"]\n#[path = "rootfs-mapper.rs"]\nmod rootfs;\n'
        'extern "C" { fn __pd_native_count(which: core::ffi::c_int) -> usize; }\n'
        '#[no_mangle]\npub extern "C" fn __pd_probe_counts(which: core::ffi::c_int) -> usize {\n'
        '    unsafe { __pd_native_count(which) }\n}\n'
    )
    (OUT / "cleanup.c").write_text(WRAPPERS)
    run([cc, "-std=gnu11", "-Wall", "-Wextra", "-Werror", "-O2", "-fPIC", "-fvisibility=hidden",
         "-c", OUT / "cleanup.c", "-o", OUT / "cleanup.o"], environment)
    for arg in (str(OUT / "cleanup.o"), "-Wl,--wrap=__pengudroid_map_path", "-Wl,--wrap=__pengudroid_free_path"):
        link_args.append(arg)

    primary, overlay = OUT / "root", OUT / "arm64-root"
    for root, size in ((primary, 111), (overlay, 222)):
        app = root / "system/bin/arm64/app_process64"
        app.parent.mkdir(parents=True, exist_ok=True)
        app.write_bytes(b"a" * size)
    generic = primary / "system/lib64/pd_syscall_paths.so"
    generic.parent.mkdir(parents=True, exist_ok=True)
    generic.write_bytes(b"g" * 33)
    generic.chmod(0o644)
    symlink = generic.with_suffix(".link")
    if symlink.is_symlink():
        symlink.unlink()
    symlink.symlink_to(generic.name)
    (primary / "system/bin/arm64/only-in-primary").write_bytes(b"primary")
    absent = overlay / "system/bin/arm64/only-in-primary"
    if absent.exists():
        absent.unlink()

    library = OUT / "libsyscall_paths_probe.so"
    profiles = (("debug", []), ("release-lto", ["-Copt-level=z", "-Clto=yes", "-Ccodegen-units=1", "-Cpanic=abort"]))
    for profile, flags in profiles:
        command = [rustc, "--edition=2021", "--crate-name", "syscall_paths_probe", "--crate-type", "cdylib",
                   OUT / "probe.rs", *flags, "-o", library]
        for arg in link_args:
            command.extend(["-C", f"link-arg={arg}"])
        run(command, environment)
        exports = run(["readelf", "--dyn-syms", "--wide", library], environment)
        assert " syscall@@LIBC" in exports and "UND syscall@GLIBC_2.2.5" in exports
        assert "__pengudroid_syscall_paths" not in exports
        for name, optimization in (("o0", "-O0"), ("o2", "-O2")):
            probe = OUT / f"probe-{name}"
            run([cc, "-std=gnu11", "-Wall", "-Wextra", "-Werror", "-fstack-protector-all", optimization,
                 SHIM / "fixtures/syscall_paths.c", f"-L{OUT}", "-lsyscall_paths_probe", f"-Wl,-rpath,{OUT}",
                 "-ldl", "-o", probe], environment)
            imports = run(["readelf", "--dyn-syms", "--wide", probe], environment)
            assert "UND syscall@LIBC" in imports
            scenarios = (
                ("no-roots", {}, (-1, -1, -1)),
                ("primary", {"PENGUDROID_ROOT": str(primary)}, (111, 33, 7)),
                ("arm64-overlay", {"PENGUDROID_ROOT": str(primary), "PENGUDROID_ARM64_GUEST_ROOT": str(overlay)}, (222, 33, -1)),
                ("overlay-only", {"PENGUDROID_ARM64_GUEST_ROOT": str(overlay)}, (222, -1, -1)),
                ("disabled-root", {"PENGUDROID_ROOT": "/"}, (-1, -1, -1)),
            )
            for scenario, overrides, sizes in scenarios:
                env = dict(environment, LD_PRELOAD=str(library), **overrides)
                output = run([probe, *sizes], env)
                assert "all pathname cases" in output
                print(f"{profile}/{name}/{scenario}: passed", flush=True)
            eager = dict(environment, LD_PRELOAD=str(library), LD_BIND_NOW="1", PENGUDROID_ROOT=str(primary),
                         PENGUDROID_ARM64_GUEST_ROOT=str(overlay))
            output = run([probe, 222, 33, -1], eager)
            print(f"{profile}/{name}/eager:\n{output}", end="", flush=True)
        host = OUT / "host-only"
        run([cc, "-std=gnu11", "-Wall", "-Wextra", "-Werror", "-O2", "-DPD_HOST_ONLY",
             SHIM / "fixtures/syscall_paths.c", "-o", host], environment)
        imports = run(["readelf", "--dyn-syms", "--wide", host], environment)
        assert "UND syscall@GLIBC_2.2.5" in imports
        run([host], dict(environment, LD_PRELOAD=str(library), PENGUDROID_ROOT=str(primary)),)
        print(f"{profile}: independent GLIBC-versioned host caller passed", flush=True)
    print("syscall path standalone probes passed; no Cargo, ART or production execution")


if __name__ == "__main__":
    main()
