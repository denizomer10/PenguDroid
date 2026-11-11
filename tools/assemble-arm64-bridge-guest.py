#!/usr/bin/env python3
"""Assemble a native-bridge ARM64 guest bundle the translator can boot.

`tools/prepare-arm64-guest.py` stages the stock GSI overlay. That overlay is
*staged-only* because the stock `linker64`/`libc.so` are not native-bridge
variants: the translator looks for `__native_bridge_config_static_tls` in the
guest linker and the guest libc imports `native_bridge_intercept_symbol`.

`tools/build-guest-linker.py` produces those variants (guest linker, guest libc,
guest `libm`/`libdl`, guest `app_process64` and the Digitalis `ld.config.arm64.txt`)
plus a minimal, closure-verified bundle. The full guest runtime also needs the
remaining ARM64 framework libraries from the stock staging, which the runtime
overlay resolves from the bundle root instead of the host sysroot.

This tool composes the two, without editing either input:

    python3 -B tools/assemble-arm64-bridge-guest.py

    .toolchain/guest-arm64-bridge/            # PENGUDROID_ARM64_GUEST_ROOT
      system/bin/arm64/                       # staging + built linker/app_process64
      system/lib64/arm64/[bootstrap/]         # staging + built libc/libm/libdl
      system/lib64/arm64/libnative_bridge_vdso.so
      system/etc/ld.config.arm64.txt
      pengudroid-arm64-bridge-bundle.json     # provenance of every replaced file

The staging manifest itself is never copied: a bundle that carries
`pengudroid-arm64-guest.json` is refused by the runtime by design. The produced
bundle records its own manifest name and per-file digests instead, so it stays
auditable without claiming the stock staging is executable.

Staging files are hard-linked (they are immutable build inputs); every file the
native-bridge build replaces is copied, so the bundle never aliases the staged
stock variant. An existing destination is never rewritten, deleted or merged --
move it aside and re-run.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import struct
import sys

REPO = Path(__file__).resolve().parent.parent
STAGING = REPO / ".toolchain/sysroot-android17"
GUEST_BUILD = REPO / ".toolchain/nativebridge-guest-linker-build"
BUILT_BUNDLE = GUEST_BUILD / "bundle"
DEST = REPO / ".toolchain/guest-arm64-bridge"
# Guest stubs built by `tools/build-guest-stubs.py --bundle-libdir` (the
# interception entry points that make the host proxy libraries reachable).
STUB_ROOT = REPO / ".toolchain/guest-stubs"
STAGING_MANIFEST = "pengudroid-arm64-guest.json"
MANIFEST = "pengudroid-arm64-bridge-bundle.json"
OWNER = "PenguDroid ARM64 native-bridge guest bundle"
SCHEMA = 1
RUNTIME_DIRS = ("system/bin/arm64", "system/lib64/arm64")
GUEST_LINKER_HOOKS = ("__native_bridge_config_static_tls", "__native_bridge_get_host_pthread")
GUEST_LIBC_EXPORTS = ("__libc_init", "pthread_self")
GUEST_LIBC_IMPORTS = ("native_bridge_intercept_symbol",)
# Every stub library registers its entry points through this vDSO-provided
# symbol; without the import nothing is ever intercepted and the guest would run
# the bionic implementation instead of the host proxy.
STUB_IMPORTS = ("native_bridge_intercept_symbol",)
GUEST_VDSO_HOOKS = ("native_bridge_trace", "native_bridge_intercept_symbol",
                    "native_bridge_post_init", "native_bridge_call_guest")


# Sonames the kernel, not a file in the bundle, satisfies.
KERNEL_PROVIDED = frozenset({"linux-vdso.so.1"})


class AssemblyError(Exception):
    pass


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def elf_sections(path):
    """Return (data, sections) for an ELF64 little-endian AArch64 file."""
    data = Path(path).read_bytes()
    if len(data) < 64 or data[:4] != b"\x7fELF":
        raise AssemblyError(f"{path} is not an ELF file")
    if data[4] != 2 or data[5] != 1:
        raise AssemblyError(f"{path} is not ELF64 little-endian")
    machine, = struct.unpack_from("<H", data, 18)
    if machine != 183:
        raise AssemblyError(f"{path} is not AArch64 (machine {machine})")
    shoff, = struct.unpack_from("<Q", data, 40)
    shentsize, shnum = struct.unpack_from("<HH", data, 58)
    sections = []
    for index in range(shnum):
        base = shoff + index * shentsize
        if base + 64 > len(data):
            raise AssemblyError(f"{path} has a truncated section table")
        name, kind, _, _, offset, size, link, _, _, entsize = struct.unpack_from("<IIQQQQIIQQ", data, base)
        sections.append({"kind": kind, "offset": offset, "size": size, "link": link, "entsize": entsize})
    return data, sections


def dynamic_symbols(path):
    """Return (exports, imports) from an ELF64 little-endian AArch64 file."""
    data, sections = elf_sections(path)
    exports, imports = set(), set()
    for section in sections:
        # SHT_DYNSYM
        if section["kind"] != 11 or not section["entsize"]:
            continue
        strings = sections[section["link"]]
        table = section["offset"]
        for index in range(section["size"] // section["entsize"]):
            entry = table + index * section["entsize"]
            name, info, _, shndx, _, _ = struct.unpack_from("<IBBHQQ", data, entry)
            if not name:
                continue
            end = data.find(b"\0", strings["offset"] + name)
            text = data[strings["offset"] + name:end].decode("ascii", "replace")
            # STB_GLOBAL/STB_WEAK and a defined section index: an export. An
            # undefined section index (SHN_UNDEF = 0) is an import.
            binding = info >> 4
            if binding in (1, 2) and text:
                (exports if shndx else imports).add(text)
    return exports, imports


def require_contract(path, exports=(), imports=()):
    if not path.is_file():
        raise AssemblyError(f"native-bridge guest bundle requires {path}")
    have_exports, have_imports = dynamic_symbols(path)
    missing = [s for s in exports if s not in have_exports]
    if missing:
        raise AssemblyError(f"{path} is missing required dynamic export(s): {', '.join(missing)}")
    missing = [s for s in imports if s not in have_imports]
    if missing:
        raise AssemblyError(f"{path} is missing required dynamic import(s): {', '.join(missing)}")


def dt_needed(path):
    """Return the DT_NEEDED sonames of an ELF64 AArch64 file, in link order."""
    data, sections = elf_sections(path)
    names = []
    for section in sections:
        # SHT_DYNAMIC
        if section["kind"] != 6 or not section["entsize"]:
            continue
        strings = sections[section["link"]]
        for index in range(section["size"] // section["entsize"]):
            base = section["offset"] + index * section["entsize"]
            tag, value = struct.unpack_from("<QQ", data, base)
            if tag == 0:
                break
            if tag != 1:
                continue
            start = strings["offset"] + value
            end = data.find(b"\0", start)
            names.append(data[start:end].decode("ascii", "replace"))
    return names


def guest_libraries(root):
    """Every `.so` file the guest linker can resolve from the bundle."""
    return [p for p in (root / "system/lib64/arm64").rglob("*.so") if p.is_file()]


def resolve_soname(soname, roots):
    """The first AArch64 library named `soname` under the supplied roots."""
    for candidate in roots:
        for path in sorted(Path(candidate).rglob(soname)):
            if not path.is_file():
                continue
            try:
                dynamic_symbols(path)  # rejects a non-AArch64 or non-ELF file
            except AssemblyError:
                continue
            return path
    return None


def complete_closure(root, roots):
    """Copy sonames the staged framework libraries need but the bundle lacks.

    The stock GSI keeps some framework libraries inside APEX packages
    (`libicu.so`, `libcom.android.tethering.connectivity_native.so`, ...). The
    overlay only redirects `/system/{bin,lib64}/arm64`, so those libraries have
    to live in the bundle itself or `dlopen` of a game library that reaches
    `libandroid.so` fails with the APEX dependency named.

    Resolved libraries bring their own `DT_NEEDED` entries, so the scan repeats
    until it reaches a fixed point.
    """
    added = {}
    for _ in range(64):
        present = {}
        for path in guest_libraries(root):
            present.setdefault(path.name, path)
        missing = {}
        for path in present.values():
            for soname in dt_needed(path):
                # The kernel provides the vDSO; it is never a file in the bundle.
                if soname in present or soname in missing or soname in KERNEL_PROVIDED:
                    continue
                missing[soname] = path.name
        if not missing:
            return added
        resolved = {name: resolve_soname(name, roots) for name in sorted(missing)}
        unresolved = sorted(name for name, path in resolved.items() if path is None)
        if unresolved:
            detail = ", ".join(f"{name} (needed by {missing[name]})" for name in unresolved)
            hint = "" if roots else "; pass --apex-libs with an extracted ARM64 GSI"
            raise AssemblyError(f"guest library closure is incomplete: {detail}{hint}")
        for soname, source in sorted(resolved.items()):
            target = root / "system/lib64/arm64" / soname
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)
            os.chmod(target, 0o755)
            added[soname] = {
                "bytes": target.stat().st_size,
                "sha256": sha256_file(target),
                "source": str(source),
                "needed_by": sorted(name for name, path in present.items() if soname in dt_needed(path)),
            }
    raise AssemblyError("guest library closure did not converge")


def staged_files(root):
    """Every regular file and symlink under the runtime directories."""
    found = []
    for directory in RUNTIME_DIRS:
        base = root / directory
        if not base.is_dir():
            raise AssemblyError(f"staged guest is missing {base}")
        for entry in sorted(base.rglob("*")):
            if entry.is_symlink() or entry.is_file():
                found.append(entry.relative_to(root))
    return found


def link_into(source, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    os.link(source, destination)


def stub_files(stubs):
    """Libraries a `build-guest-stubs.py` run installed under `stubs`.

    Only `system/lib64/arm64` is read: the stubs are guest shared objects and
    nothing else from that tree belongs in the runtime bundle.
    """
    base = stubs / "system/lib64/arm64"
    if not base.is_dir():
        raise AssemblyError(f"guest stub root {stubs} has no system/lib64/arm64")
    found = sorted(p for p in base.iterdir() if p.is_file() or p.is_symlink())
    if not found:
        raise AssemblyError(f"guest stub root {base} is empty")
    for path in found:
        require_contract(path, imports=STUB_IMPORTS)
    return found


def assemble(staging=None, built=None, dest=None, apex_libs=(), stubs=None):
    staging = Path(staging or STAGING).absolute()
    built = Path(built or BUILT_BUNDLE).absolute()
    dest = Path(dest or DEST).absolute()
    if stubs is None and STUB_ROOT.is_dir():
        stubs = STUB_ROOT
    stubs = Path(stubs).absolute() if stubs is not None else None
    roots = [Path(root).absolute() for root in apex_libs]
    for root in roots:
        if not root.is_dir():
            raise AssemblyError(f"APEX library root {root} is not a directory")
    if not built.is_dir():
        raise AssemblyError(f"native-bridge bundle {built} is missing; run tools/build-guest-linker.py first")
    if os.path.lexists(dest):
        raise AssemblyError(f"refusing existing destination {dest}; move it aside and re-run")
    staging_files = staged_files(staging)
    built_files = sorted(p.relative_to(built) for p in built.rglob("*") if p.is_file() or p.is_symlink())
    if not built_files:
        raise AssemblyError(f"native-bridge bundle {built} is empty")
    stub_libraries = stub_files(stubs) if stubs is not None else []
    # Compose privately, then publish, so a failure never leaves a partial bundle
    # that the runtime would try to boot.
    dest.parent.mkdir(parents=True, exist_ok=True)
    partial = dest.parent / ("." + dest.name + ".partial")
    if os.path.lexists(partial):
        shutil.rmtree(partial) if partial.is_dir() else partial.unlink()
    try:
        for relative in staging_files:
            link_into(staging / relative, partial / relative)
        replaced = {}
        for relative in built_files:
            source = built / relative
            target = partial / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            if os.path.lexists(target):
                target.unlink()
            shutil.copyfile(source, target)
            os.chmod(target, 0o755 if os.access(source, os.X_OK) else 0o644)
            replaced[relative.as_posix()] = {
                "bytes": target.stat().st_size,
                "sha256": sha256_file(target),
                "source": str(source),
            }
        # Stubs go on top of everything: they must win over both the linker
        # bundle and the stock GSI's bionic originals, because their whole point
        # is to replace that implementation with a call into the host proxy.
        stubbed = {}
        for source in stub_libraries:
            relative = source.relative_to(stubs)
            target = partial / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            if os.path.lexists(target):
                target.unlink()
            shutil.copyfile(source, target)
            os.chmod(target, 0o755)
            entry = {
                "bytes": target.stat().st_size,
                "sha256": sha256_file(target),
                "source": str(source),
            }
            if relative.as_posix() in replaced:
                entry["replaced"] = replaced.pop(relative.as_posix())
            stubbed[relative.as_posix()] = entry
        require_contract(partial / "system/bin/arm64/linker64", exports=GUEST_LINKER_HOOKS)
        require_contract(partial / "system/lib64/arm64/libc.so",
                         exports=GUEST_LIBC_EXPORTS, imports=GUEST_LIBC_IMPORTS)
        require_contract(partial / "system/lib64/arm64/libnative_bridge_vdso.so",
                         exports=GUEST_VDSO_HOOKS)
        require_contract(partial / "system/bin/arm64/app_process64")
        closure = complete_closure(partial, roots)
        if (partial / STAGING_MANIFEST).exists():
            raise AssemblyError(f"assembled bundle must not contain {STAGING_MANIFEST}")
        manifest = {
            "tool": OWNER,
            "schema": SCHEMA,
            "staging": {
                "root": str(staging),
                "manifest": str(staging / STAGING_MANIFEST),
                "manifest_sha256": sha256_file(staging / STAGING_MANIFEST)
                if (staging / STAGING_MANIFEST).is_file() else None,
                "files": len(staging_files),
                "note": "Stock staging hard-linked as-is; the staging manifest is deliberately not copied.",
            },
            "native_bridge": {
                "bundle": str(built),
                "report": str(GUEST_BUILD / "report.json"),
                "report_sha256": sha256_file(GUEST_BUILD / "report.json")
                if (GUEST_BUILD / "report.json").is_file() else None,
            },
            "files": replaced,
            "stubs": {
                "root": str(stubs) if stubs is not None else None,
                "libraries": stubbed,
                "imports": list(STUB_IMPORTS),
                "note": "NativeBridgeSupport stubs; they replace the bionic "
                        "implementations so guest calls are marshalled into the "
                        "host proxy library of the same soname.",
            },
            "closure": {
                "roots": [str(root) for root in roots],
                "added": closure,
                "note": "Framework sonames that only APEX packages provide, copied "
                        "into system/lib64/arm64 so the guest linker resolves them.",
            },
            "contracts": {
                "linker_exports": list(GUEST_LINKER_HOOKS),
                "libc_exports": list(GUEST_LIBC_EXPORTS),
                "libc_imports": list(GUEST_LIBC_IMPORTS),
                "vdso_exports": list(GUEST_VDSO_HOOKS),
            },
        }
        (partial / MANIFEST).write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
        os.chmod(partial / MANIFEST, 0o644)
        os.rename(partial, dest)
    except BaseException:
        shutil.rmtree(partial, ignore_errors=True)
        raise
    return len(staging_files), len(replaced), len(stubbed), len(closure), dest


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--staging", default=str(STAGING), help="stock ARM64 GSI staging root")
    parser.add_argument("--bundle", default=str(BUILT_BUNDLE), help="native-bridge guest bundle to overlay")
    parser.add_argument("--dest", default=str(DEST), help="destination bundle root")
    parser.add_argument("--apex-libs", action="append", default=[],
                        help="extracted ARM64 GSI root to resolve APEX-only sonames from (repeatable)")
    parser.add_argument("--guest-stubs", default=None,
                        help="root holding system/lib64/arm64 stubs built by "
                             "tools/build-guest-stubs.py (default: %s when present)" % STUB_ROOT)
    args = parser.parse_args(argv)
    try:
        staged, replaced, stubbed, closure, dest = assemble(
            args.staging, args.bundle, args.dest, args.apex_libs, args.guest_stubs)
    except (AssemblyError, OSError) as error:
        print(f"assemble-arm64-bridge-guest: {error}", file=sys.stderr)
        return 1
    print(f"assemble-arm64-bridge-guest: {staged} staged file(s), {replaced} native-bridge file(s), "
          f"{stubbed} stub file(s), {closure} closure file(s)")
    print(f"assemble-arm64-bridge-guest: bundle published at {dest}")
    print(f"assemble-arm64-bridge-guest: launch with PENGUDROID_ARM64_GUEST_ROOT={dest}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
