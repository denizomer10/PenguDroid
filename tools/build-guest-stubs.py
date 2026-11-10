#!/usr/bin/env python3
"""Build the ARM64 native-bridge guest stub libraries and install them into the bundle.

Run: python3 -B tools/build-guest-stubs.py
     python3 -B tools/build-guest-stubs.py --check
     python3 -B tools/build-guest-stubs.py --only libEGL libGLESv2

Under translation the guest bionic `libEGL.so` cannot work: its `Loader`
discovers an OpenGL ES driver by reading `ro.hardware.egl` /
`persist.graphics.egl` / `ro.board.platform` and calls `LOG_FATAL` when none of
them name a known driver, which aborts the process ("couldn't find an OpenGL ES
implementation"). Replacing the bionic libraries with NativeBridgeSupport stubs
routes those calls to the host instead.

Each stub is `stubs_arm64.cc` from
`native_bridge_support/android_api/<library>/`: naked-asm entry points whose
addresses are handed to `native_bridge_intercept_symbol` from a load-time
constructor. Berberis rebinds every registered address to a trampoline that
marshals the call into `libberberis_proxy_<library>.so` on the host. The only
import in the stub is `native_bridge_intercept_symbol` itself, which the guest
vDSO provides.

The built `.so` files replace the bionic originals in
`.toolchain/nativebridge-guest-linker-build/bundle/system/lib64/arm64/`, so the
guest linker resolves an app's `DT_NEEDED libEGL.so` to the stub. Re-run
`tools/prepare-arm64-guest.py` before this script if the bundle was rebuilt from
scratch, since that restores the bionic copies.
"""

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.dont_write_bytecode = True

REPO = Path(__file__).resolve().parent.parent
DEPS = REPO / ".toolchain/nativebridge-deps"
LLVM = Path(os.environ.get("PENGUDROID_LLVM_DIR", "/usr/lib/llvm-22/bin"))
NBS_API = DEPS / "sources/native_bridge_support/android_api"
SYSROOTS = (
    REPO / ".toolchain/ndk-refs_tags_ndk-r30-platform-sysroot",
    REPO / ".toolchain/ndk-r30-sysroot",
)
BUNDLE_LIBDIR = (
    REPO / ".toolchain/nativebridge-guest-linker-build/bundle/system/lib64/arm64"
)
# Guest API level used for every stub; the intercepted ABI is independent of it,
# but the sysroot must provide a matching <stdint.h> and the android triple.
API_LEVEL = "28"

# library name -> subdirectory under NBS android_api. Each directory ships
# `stubs_arm64.cc`, whose exported stem matches the name (libGLESv3 is a
# distinct soname that re-exports the GLES2 entry points on Android).
STUB_LIBRARIES = (
    "libEGL",
    "libGLESv1_CM",
    "libGLESv2",
    "libGLESv3",
    "libandroid",
)

INTERCEPT_SYMBOL = "native_bridge_intercept_symbol"

# Guest platform libraries reference these entry points through the symbol
# versions of the bionic library being replaced (e.g. `glClear@LIBGLESV1_CM`,
# `AAsset_read@LIBANDROID`). The linker refuses to relocate a versioned
# reference against an unversioned definition of the same soname, so each stub
# must publish the bionic version nodes. Node names must match the referencing
# libraries' verneed strings byte for byte: bionic compares the target's verdef
# name against the verneed name, so `LIBGLESV1_CM` and `LIBGLESv1_CM` are not
# the same node. These spellings mirror bionic's libGLESv1_CM.map/libandroid.map.
VERSION_NODES: dict[str, tuple[tuple[str, str | None], ...]] = {
    "libGLESv1_CM": (("LIBGLESV1_CM", None),),
    "libandroid": (("LIBANDROID", None), ("LIBANDROID_PLATFORM", "LIBANDROID")),
}


class BuildError(RuntimeError):
    pass


def sysroot() -> Path:
    for candidate in SYSROOTS:
        if (candidate / "usr/lib/aarch64-linux-android" / API_LEVEL).is_dir():
            return candidate
    raise BuildError(
        "no ARM64 NDK sysroot found; expected one of "
        + ", ".join(str(path) for path in SYSROOTS)
    )


def run(command: list[str]) -> str:
    result = subprocess.run(
        command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True
    )
    if result.returncode != 0:
        raise BuildError(
            "command failed (%d): %s\n%s"
            % (result.returncode, " ".join(command), result.stdout)
        )
    return result.stdout


def tool(name: str) -> Path:
    for candidate in (LLVM / name, LLVM / "llvm-22" / "bin" / name):
        if candidate.is_file():
            return candidate
    found = shutil.which(name)
    if found is None:
        raise BuildError("required tool not found: %s (looked in %s)" % (name, LLVM))
    return Path(found)


def dynamic_symbols(library: Path) -> tuple[set[str], set[str]]:
    """Return (defined, undefined) dynamic symbol names, demangled names excluded."""
    output = run([str(tool("llvm-nm")), "-D", "--no-demangle", str(library)])
    defined: set[str] = set()
    undefined: set[str] = set()
    for line in output.splitlines():
        fields = line.split()
        if len(fields) < 2:
            continue
        kind, name = fields[-2], fields[-1]
        if kind in {"U", "w"}:
            undefined.add(name)
        elif kind.upper() in {"T", "W", "D", "B", "V", "I"}:
            defined.add(name)
    return defined, undefined


def stub_source(library: str) -> Path:
    source = NBS_API / library / "stubs_arm64.cc"
    if not source.is_file():
        raise BuildError("missing stub source: %s" % source)
    return source


def declared_symbols(source: Path) -> int:
    count = 0
    for line in source.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped.startswith("DEFINE_INTERCEPTABLE_STUB_"):
            count += 1
    return count


def version_map(library: str, destination: Path) -> Path | None:
    """Write the bionic-compatible symbol version script for `library`."""
    nodes = VERSION_NODES.get(library)
    if nodes is None:
        return None
    lines: list[str] = []
    for index, (name, parent) in enumerate(nodes):
        lines.append("%s {" % name)
        # The first node carries the stub's whole export set; the remaining
        # nodes stay empty, exactly like the optional bionic extensions that
        # guest libraries do not reference through these sonames today.
        if index == 0:
            lines.append("  global: *;")
        lines.append("}%s;" % ("" if parent is None else " " + parent))
    destination.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return destination


def check_library(library: Path, minimum_exports: int) -> None:
    defined, undefined = dynamic_symbols(library)
    unexpected = undefined - {INTERCEPT_SYMBOL}
    if unexpected:
        raise BuildError(
            "%s imports unexpected symbols: %s" % (library.name, sorted(unexpected))
        )
    if INTERCEPT_SYMBOL not in undefined:
        raise BuildError(
            "%s does not import %s, so guest calls will never be intercepted"
            % (library.name, INTERCEPT_SYMBOL)
        )
    if len(defined) < minimum_exports:
        raise BuildError(
            "%s exports %d symbols, expected at least %d"
            % (library.name, len(defined), minimum_exports)
        )
    stem = library.name[: -len(".so")]
    soname = stem + ".so"
    dynamic = run([str(tool("llvm-readelf")), "-d", str(library)])
    if "(SONAME)" not in dynamic or ("[" + soname + "]") not in dynamic:
        raise BuildError(
            "%s does not publish DT_SONAME %s, so guest verneed entries cannot "
            "be matched against it" % (library.name, soname)
        )
    for name, _parent in VERSION_NODES.get(stem, ()):
        if ("Name: " + name) not in run(
            [str(tool("llvm-readelf")), "--version-info", str(library)]
        ):
            raise BuildError(
                "%s does not define the %s symbol version, so versioned guest "
                "references to it cannot be relocated" % (library.name, name)
            )


def build_library(library: str, output_dir: Path, root: Path) -> Path:
    source = stub_source(library)
    output = output_dir / (library + ".so")
    script = version_map(library, output_dir / (library + ".map"))
    run(
        [
            str(tool("clang++")),
            "--target=aarch64-linux-android" + API_LEVEL,
            "--sysroot=" + str(root),
            "-O2",
            "-fPIC",
            "-shared",
            # The stub must not pull in libc or libgcc: it defines nothing but
            # naked-asm entry points plus a constructor that runs when the guest
            # linker loads it.
            "-nostdlib",
            "-I",
            str(NBS_API / "vdso/include"),
            str(source),
            "-o",
            str(output),
            # Older loaders only look at DT_HASH; AOSP builds stubs the same way.
            "-Wl,--hash-style=both",
            # bionic matches verneed entries against the loaded DT_SONAME. Without
            # one the soname is the realpath, so guest libraries that reference
            # e.g. LIBANDROID through libandroid.so fail to relocate.
            "-Wl,-soname=" + library + ".so",
            *(["-Wl,--version-script=" + str(script)] if script is not None else []),
        ]
    )
    check_library(output, declared_symbols(source))
    return output


def install(library: Path, bundle_libdir: Path) -> Path:
    bundle_libdir.mkdir(parents=True, exist_ok=True)
    destination = bundle_libdir / library.name
    if destination.exists() or destination.is_symlink():
        destination.unlink()
    shutil.copy2(library, destination)
    return destination


def version_section(library: Path, heading: str) -> list[str]:
    """Return the llvm-readelf version-info lines below `heading`."""
    output = run([str(tool("llvm-readelf")), "--version-info", str(library)])
    lines: list[str] = []
    inside = False
    for line in output.splitlines():
        stripped = line.strip()
        if stripped.startswith("Version "):
            inside = stripped.startswith(heading)
            continue
        if inside:
            lines.append(stripped)
    return lines


def version_field(line: str, key: str) -> str | None:
    """Return the token after `key + ": "` anywhere in a readelf version line.

    `llvm-readelf --version-info` puts the interesting fields mid-line, e.g.
    `0x0000: Version: 1  File: libandroid.so  Cnt: 1` and
    `0x0010:   Name: LIBANDROID  Flags: none  Version: 12`, so the fields are
    located rather than assumed to start the line.
    """
    marker = key + ": "
    index = line.find(marker)
    if index < 0:
        return None
    fields = line[index + len(marker):].split()
    return fields[0] if fields else None


def defined_version_nodes(library: Path) -> set[str]:
    """Version node names the library itself publishes, excluding its `BASE` node.

    The `BASE` node is the soname, not a bionic spelling any import can match.
    """
    names: set[str] = set()
    for line in version_section(library, "Version definition section"):
        if "Flags: BASE" in line:
            continue
        name = version_field(line, "Name")
        if name is not None:
            names.add(name)
    return names


def referenced_version_nodes(directory: Path, soname: str) -> set[str]:
    """Version node names that libraries in `directory` expect from `soname`."""
    names: set[str] = set()
    for library in sorted(directory.glob("*.so")):
        wanted = False
        for line in version_section(library, "Version needs section"):
            required_from = version_field(line, "File")
            if required_from is not None:
                wanted = required_from == soname
                continue
            name = version_field(line, "Name")
            if wanted and name is not None:
                names.add(name)
    return names


def check_installed(bundle_libdir: Path, libraries: tuple[str, ...]) -> list[str]:
    problems: list[str] = []
    for library in libraries:
        installed = bundle_libdir / (library + ".so")
        if not installed.is_file() or installed.is_symlink():
            problems.append("missing %s" % installed)
            continue
        try:
            check_library(installed, declared_symbols(stub_source(library)))
        except BuildError as error:
            problems.append(str(error))
            continue
        missing = referenced_version_nodes(
            bundle_libdir, library + ".so"
        ) - defined_version_nodes(installed)
        if missing:
            problems.append(
                "%s does not publish the %s version node(s) that guest "
                "libraries reference" % (installed, ", ".join(sorted(missing)))
            )
    return problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="verify the stubs already installed in the bundle, without building",
    )
    parser.add_argument(
        "--only",
        nargs="+",
        choices=STUB_LIBRARIES,
        help="build only the listed stub libraries",
    )
    parser.add_argument(
        "--bundle-libdir",
        type=Path,
        default=BUNDLE_LIBDIR,
        help="guest library directory that receives the stubs",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        help="keep the built stubs here instead of a temporary directory",
    )
    args = parser.parse_args()

    libraries = tuple(args.only) if args.only else STUB_LIBRARIES
    bundle_libdir = args.bundle_libdir.resolve()

    if args.check:
        problems = check_installed(bundle_libdir, libraries)
        if problems:
            for problem in problems:
                print("guest-stub: " + problem, file=sys.stderr)
            return 1
        print("guest-stub: %d stub libraries verified in %s"
              % (len(libraries), bundle_libdir))
        return 0

    root = sysroot()
    temporary = None
    if args.output_dir is None:
        temporary = tempfile.TemporaryDirectory(prefix="pengudroid-guest-stubs-")
        output_dir = Path(temporary.name)
    else:
        output_dir = args.output_dir.resolve()
        output_dir.mkdir(parents=True, exist_ok=True)

    try:
        for library in libraries:
            built = build_library(library, output_dir, root)
            installed = install(built, bundle_libdir)
            print("guest-stub: %s -> %s (%d bytes)"
                  % (built.name, installed, installed.stat().st_size))
    finally:
        if temporary is not None:
            temporary.cleanup()
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except BuildError as error:
        print("guest-stub: " + str(error), file=sys.stderr)
        sys.exit(1)
