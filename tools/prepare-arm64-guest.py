#!/usr/bin/env python3
"""Prepare the matching API-37 ARM64 guest runtime for the Digitalis translator.

The ARM64-to-x86_64 bridge needs guest app_process64, linker64 and libraries
under /system/{bin,lib64}/arm64, alongside a verified x86_64 Android 17 host.
The official CP2A.260605.012 GSI archive is verified and extracted offline;
no image is mounted, booted, downloaded, or redistributed by this tool.
This is staged-only data, not a bridge-compatible runtime: the stock GSI
linker64 and libc are not native-bridge variants. Schema-2 manifests record
this blocker, and successful preparation and reuse both warn about it.

    python3 -B tools/prepare-arm64-guest.py --archive aosp_arm64-....zip

The complete overlay is built privately before publication. Only unchanged
schema-2 installations with complete provenance and staged-only metadata are
reused; replacement is deliberately unsupported.
Legacy/schema-1 guest files (including the earlier 832-file installation),
unowned paths and changed installations cause exit 1 without adopting or
replacing them. Inspect and move such installations aside manually if needed.
Expected publication errors roll back only entries created by this attempt;
crash remnants are refused on the next run, never merged into a new install.

--keep-extracted requires an absent destination: retained trees are NEVER
replaced or recursively deleted. Retained symlinks are copied as inert links,
not dereferenced. Runtime source links must resolve inside the extracted GSI.
The generated VDSO uses local, unpinned source/toolchain inputs whose identity
and build arguments are recorded honestly, separately from the GSI hash.
Requires debugfs and fsck.erofs, and the existing APEX/relink helpers.
"""

import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import stat
import struct
import subprocess
import sys
import tempfile
import zipfile

REPO = Path(__file__).resolve().parent.parent
FILENAME = "aosp_arm64-exp-CP2A.260605.012-15430684-8e545e1e.zip"
URL = "https://dl.google.com/developers/android/cinnamonbun/images/gsi/" + FILENAME
SHA256 = "8e545e1e1e977c3b43b4e54a3af463f3e7d1c3177b120e772138334e15f48f75"
BUILD = "CP2A.260605.012"
SYSROOT = REPO / ".toolchain/sysroot-android17"
DEST = REPO / ".toolchain/guest-arm64"
OWNER = "PenguDroid ARM64 guest preparation"
SCHEMA = 2
BRIDGE_COMPATIBILITY = {
    "state": "staged-only",
    "blocker": "Stock GSI linker64 and libc are not native-bridge variants; "
               "Digitalis requires the modified guest linker and libc.",
}
MANIFEST = "pengudroid-arm64-guest.json"
LOCK = ".pengudroid-arm64-guest.lock"
OVERLAYS = ("system/bin/arm64", "system/lib64/arm64")
ART_LIB = "system/apex/com.android.art/lib64"
BIONIC_LIB = "system/apex/com.android.runtime/lib64/bionic"
ART_NAMES = ("libart.so", "libartbase.so", "libdexfile.so", "libnativehelper.so")
GUEST_ELF_REQUIRED = (
    "system/bin/app_process64",
    "system/bin/linker64",
    "system/lib64/libandroid.so",
    "system/lib64/libdl_android.so",
    "system/lib64/liblog.so",
    *(f"{ART_LIB}/{name}" for name in ART_NAMES),
    *(f"{BIONIC_LIB}/{name}" for name in ("libc.so", "libdl.so", "libm.so")),
)
REQUIRED_OUTPUT = (
    *(f"{OVERLAYS[0]}/{name}" for name in ("app_process64", "linker64")),
    *(f"{OVERLAYS[1]}/{name}" for name in (
        *ART_NAMES, "libandroid.so", "libdl_android.so", "liblog.so",
        "libc.so", "libdl.so", "libm.so", "libnative_bridge_vdso.so")),
    *(f"{OVERLAYS[1]}/bootstrap/{name}" for name in ART_NAMES),
)
VDSO_SOURCE = REPO / ".toolchain/nativebridge-deps/sources/native_bridge_support/android_api/vdso/vdso_arm64.S"
COMPILER = Path("/usr/lib/llvm-22/bin/clang")
VDSO_FLAGS = (
    "--target=aarch64-linux-android37", "-fPIC", "-shared", "-nostdlib",
    "-Wl,-soname,linux-vdso.so.1", "-Wl,--exclude-libs,ALL",
    "-Wl,-z,separate-code", "-Wl,--image-base=0x4000", "-Wl,--hash-style=both",
)
SPARSE_MAGIC = 0xED26FF3A
MAX_IMAGE = 16 * 1024**3
BUFFER = 4 * 1024**2


class PreparationError(RuntimeError):
    pass


def load_prepare_module():
    spec = importlib.util.spec_from_file_location(
        "prepare_api37", REPO / "tools/prepare-android17.py"
    )
    if spec is None or spec.loader is None:
        raise PreparationError("cannot load tools/prepare-android17.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def owned(info, path):
    if info.st_uid != os.geteuid() or info.st_mode & 0o022:
        raise PreparationError(f"refusing non-owned or group/world-writable path: {path}")


@contextmanager
def directory(path, *, writable=False):
    """Walk without following even intermediate symlinks; pin the final directory."""
    path = Path(path).absolute()
    if ".." in path.parts:
        raise PreparationError(f"parent traversal is unsupported: {path}")
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    fd = os.open(path.anchor, flags)
    try:
        for part in path.parts[1:]:
            next_fd = os.open(part, flags, dir_fd=fd)
            os.close(fd)
            fd = next_fd
        if writable:
            owned(os.fstat(fd), path)
        yield fd
    finally:
        os.close(fd)


@contextmanager
def regular(path):
    path = Path(path)
    with directory(path.parent) as parent:
        fd = os.open(path.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                     dir_fd=parent)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            raise PreparationError(f"refusing non-regular or hardlinked file: {path}")
        yield stream


def unchanged(stream, before, path):
    after = os.fstat(stream.fileno())
    fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns", "st_mode", "st_nlink")
    if any(getattr(before, field) != getattr(after, field) for field in fields):
        raise PreparationError(f"file changed while reading: {path}")


def digest(path):
    with regular(path) as stream:
        before = os.fstat(stream.fileno())
        result = hashlib.file_digest(stream, "sha256").hexdigest()
        unchanged(stream, before, path)
        return result


def read_record(path):
    with regular(path) as stream:
        owned(os.fstat(stream.fileno()), path)
        before = os.fstat(stream.fileno())
        data = stream.read()
        unchanged(stream, before, path)
    record = json.loads(data)
    if not isinstance(record, dict):
        raise PreparationError(f"invalid manifest object: {path}")
    return record, hashlib.sha256(data).hexdigest()


def confined(root, path, *, is_directory=False):
    try:
        resolved = path.resolve(strict=True)
    except (OSError, RuntimeError) as error:
        raise PreparationError(f"missing or invalid guest source: {path}") from error
    if not resolved.is_relative_to(root):
        raise PreparationError(f"source link escapes extracted tree: {path}")
    if is_directory:
        with directory(resolved):
            pass
    else:
        with regular(resolved) as stream:
            if not os.fstat(stream.fileno()).st_size:
                raise PreparationError(f"empty required guest input: {path}")
    return resolved


def validate_elf(path):
    with regular(path) as stream:
        header = stream.read(64)
    if (len(header) != 64 or header[:7] != b"\x7fELF\x02\x01\x01"
            or struct.unpack_from("<H", header, 16)[0] not in (2, 3)
            or struct.unpack_from("<H", header, 18)[0] != 183
            or struct.unpack_from("<I", header, 20)[0] != 1
            or struct.unpack_from("<H", header, 52)[0] != 64):
        raise PreparationError(f"guest input must be a little-endian ELF64 AArch64 executable/library: {path}")


def validate_guest(root):
    root = Path(root).absolute()
    with directory(root):
        pass
    props = {}
    prop_path = confined(root, root / "system/build.prop")
    with regular(prop_path) as stream:
        lines = stream.read().decode("utf-8").splitlines()
    expected = {"ro.build.version.sdk": "37", "ro.build.version.release": "17", "ro.build.id": BUILD}
    for line in lines:
        if not line.strip() or line.lstrip().startswith("#") or "=" not in line:
            continue
        key, value = (part.strip() for part in line.split("=", 1))
        if key in expected:
            if key in props:
                raise PreparationError(f"duplicate guest build property: {key}")
            props[key] = value
    for key, value in expected.items():
        if props.get(key) != value:
            raise PreparationError(f"guest {key}: expected {value}, found {props.get(key)!r}")
    for relative in ("system/bin", "system/lib64", ART_LIB, BIONIC_LIB):
        confined(root, root / relative, is_directory=True)
    for relative in GUEST_ELF_REQUIRED:
        path = confined(root, root / relative)
        validate_elf(path)
        if relative.startswith("system/bin/"):
            with regular(path) as stream:
                if not os.fstat(stream.fileno()).st_mode & 0o111:
                    raise PreparationError(f"guest executable lacks execute permission: {relative}")
    return root


def validate_host(sysroot, prepare):
    for relative in (".", "system", "system/bin", "system/lib64"):
        with directory(sysroot / relative, writable=True):
            pass
    record, manifest_hash = read_record(sysroot / prepare.MANIFEST)
    expected = {"tool": prepare.OWNER, "schema": 1, "archive_sha256": prepare.SHA256,
                "build": BUILD, "sdk": 37, "release": "17", "architecture": "x86_64"}
    if prepare.BUILD != BUILD or any(record.get(key) != value for key, value in expected.items()):
        raise PreparationError("host manifest is not the matching prepared API-37 x86_64 runtime")
    for relative in prepare.REQUIRED:
        confined(sysroot, sysroot / relative)
    try:
        hashes = prepare.validate_runtime(sysroot)
    except prepare.PreparationError as error:
        raise PreparationError(f"invalid host runtime: {error}") from error
    if record.get("files") != hashes:
        raise PreparationError("host runtime files changed; refusing guest preparation")
    return manifest_hash


def inventory(root):
    files, directories = {}, {}

    def visit(path):
        relative = path.relative_to(root).as_posix()
        info = path.lstat()
        owned(info, path)
        if stat.S_ISDIR(info.st_mode):
            with directory(path):
                directories[relative] = stat.S_IMODE(info.st_mode)
                for child in sorted(path.iterdir()):
                    visit(child)
        elif stat.S_ISREG(info.st_mode):
            files[relative] = {"sha256": digest(path), "size": info.st_size,
                               "mode": stat.S_IMODE(info.st_mode)}
        else:
            raise PreparationError(f"refusing linked or special overlay path: {path}")

    for relative in OVERLAYS:
        visit(root / relative)
    return {"files": files, "directories": directories}


def validate_vdso_build(build):
    def require(condition, field):
        if not condition:
            raise PreparationError(f"invalid or incomplete VDSO provenance: {field}")

    def text(value):
        return isinstance(value, str) and bool(value.strip()) and "\0" not in value

    require(isinstance(build, dict), "vdso_build")
    compiler = build.get("compiler")
    require(isinstance(compiler, dict), "vdso_build.compiler")
    for field, value in (("source_path", build.get("source_path")), ("note", build.get("note")),
                         ("compiler.path", compiler.get("path")),
                         ("compiler.version", compiler.get("version"))):
        require(text(value), "vdso_build." + field)
    for field, value in (("source_path", build["source_path"]), ("compiler.path", compiler["path"])):
        require(Path(value).is_absolute(), "vdso_build." + field)
    for field, value in (("source_sha256", build.get("source_sha256")),
                         ("compiler.sha256", compiler.get("sha256"))):
        require(isinstance(value, str) and len(value) == 64
                and all(character in "0123456789abcdef" for character in value), "vdso_build." + field)
    arguments = build.get("arguments")
    require(isinstance(arguments, list) and bool(arguments) and all(text(value) for value in arguments),
            "vdso_build.arguments")
    environment = build.get("environment")
    require(isinstance(environment, dict) and {"PATH", "LC_ALL"}.issubset(environment)
            and all(text(key) and text(value) for key, value in environment.items()),
            "vdso_build.environment")


def warn_staged_only():
    print("prepare-arm64-guest: warning: ARM64 guest is staged-only: "
          + BRIDGE_COMPATIBILITY["blocker"], file=sys.stderr)


def existing_overlay(sysroot, host_hash):
    paths = [sysroot / relative for relative in (*OVERLAYS, MANIFEST)]
    if not any(os.path.lexists(path) for path in paths):
        return None
    if not all(os.path.lexists(path) for path in paths):
        raise PreparationError("unowned/incomplete ARM64 guest installation; not adopting or replacing existing files")
    record, _ = read_record(sysroot / MANIFEST)
    expected = {"tool": OWNER, "schema": SCHEMA, "archive_sha256": SHA256, "build": BUILD,
                "sdk": 37, "release": "17", "architecture": "aarch64", "guest_abi": "arm64-v8a",
                "host_manifest_sha256": host_hash}
    if any(record.get(key) != value for key, value in expected.items()):
        raise PreparationError("unowned/legacy or mismatched ARM64 guest installation; replacement is unsupported")
    if record.get("bridge_compatibility") != BRIDGE_COMPATIBILITY:
        raise PreparationError("missing or invalid bridge_compatibility; expected the stock GSI "
                               "staged-only metadata; not replacing installation")
    validate_vdso_build(record.get("vdso_build"))
    actual = inventory(sysroot)
    if (any(record.get(key) != value for key, value in actual.items())
            or not set(REQUIRED_OUTPUT).issubset(actual["files"])):
        raise PreparationError("ARM64 guest files changed or manifest is incomplete; not replacing them")
    return record


@contextmanager
def publication_lock(sysroot):
    with directory(sysroot, writable=True) as parent:
        fd = os.open(LOCK, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                     0o600, dir_fd=parent)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            raise PreparationError("refusing linked or non-regular guest lock")
        owned(info, sysroot / LOCK)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise PreparationError("another ARM64 guest preparation holds the lock") from error
        yield
    finally:
        os.close(fd)
    # Keep the lock inode: unlinking it would allow competing locks on new files.


def remember(journal, path, info):
    if journal is not None:
        journal.append((path, info.st_dev, info.st_ino, stat.S_IFMT(info.st_mode)))


def copy_regular(source, destination, journal=None):
    source, destination = Path(source), Path(destination)
    with regular(source) as stream, directory(destination.parent, writable=True) as parent:
        before = os.fstat(stream.fileno())
        fd = os.open(destination.name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                     0o600, dir_fd=parent)
        with os.fdopen(fd, "wb") as output:
            remember(journal, destination, os.fstat(output.fileno()))
            shutil.copyfileobj(stream, output, BUFFER)
            unchanged(stream, before, source)
            os.fchmod(output.fileno(), stat.S_IMODE(before.st_mode) & 0o755)


def build_vdso(work, destination, source, compiler):
    snapshot = work / "vdso_arm64.S"
    copy_regular(Path(source), snapshot)
    source_hash = digest(snapshot)
    compiler = Path(compiler).resolve(strict=True)
    if not os.access(compiler, os.X_OK):
        raise PreparationError(f"compiler is not executable: {compiler}")
    compiler_hash = digest(compiler)
    env = {"PATH": str(compiler.parent) + os.pathsep + os.defpath, "LC_ALL": "C"}
    version = subprocess.run([str(compiler), "--version"], capture_output=True, text=True,
                             check=True, timeout=30, env=env)
    subprocess.run([str(compiler), *VDSO_FLAGS, str(snapshot), "-o", str(destination)],
                   check=True, timeout=120, env=env)
    if digest(compiler) != compiler_hash or digest(snapshot) != source_hash:
        raise PreparationError("VDSO compiler or source changed during the build")
    validate_elf(destination)
    with regular(destination) as stream:
        os.fchmod(stream.fileno(), stat.S_IMODE(os.fstat(stream.fileno()).st_mode) & 0o755)
    return {"source_path": str(Path(source).absolute()), "source_sha256": source_hash,
            "compiler": {"path": str(compiler), "sha256": compiler_hash,
                         "version": version.stdout.strip()},
            "arguments": [*VDSO_FLAGS, "<source>/vdso_arm64.S", "-o", "<overlay>/libnative_bridge_vdso.so"],
            "environment": env,
            "note": "Local source and compiler are recorded, NOT pinned by the GSI hash. "
                    "Compiler-selected linker and other toolchain inputs are not exhaustively pinned."}


def prepare_overlay(root, work, host_hash, vdso_source, compiler):
    root = validate_guest(root)
    overlay = work / "overlay"
    overlay.mkdir(mode=0o700)
    for relative in ("system", "system/bin", "system/lib64", *OVERLAYS, OVERLAYS[1] + "/bootstrap"):
        (overlay / relative).mkdir(mode=0o755)
    plan = {f"{OVERLAYS[0]}/{name}": confined(root, root / "system/bin" / name)
            for name in ("app_process64", "linker64")}
    # Preserve the original, explicit precedence: system, ART, then Bionic.
    for source_dir, destination in (("system/lib64", OVERLAYS[1]), (ART_LIB, OVERLAYS[1]),
                                    (ART_LIB, OVERLAYS[1] + "/bootstrap"), (BIONIC_LIB, OVERLAYS[1])):
        source = confined(root, root / source_dir, is_directory=True)
        for entry in sorted(source.iterdir()):
            resolved = confined(root, entry, is_directory=entry.is_dir())
            if resolved.is_dir():
                continue
            if entry.name.endswith(".so") or ".so." in entry.name:
                validate_elf(resolved)
            plan[f"{destination}/{entry.name}"] = resolved
    for relative, source in sorted(plan.items()):
        copy_regular(source, overlay / relative)
    vdso = overlay / OVERLAYS[1] / "libnative_bridge_vdso.so"
    if os.path.lexists(vdso):
        raise PreparationError("guest archive collides with the generated VDSO path")
    vdso_build = build_vdso(work, vdso, vdso_source, compiler)
    validate_vdso_build(vdso_build)
    actual = inventory(overlay)
    if not set(REQUIRED_OUTPUT).issubset(actual["files"]):
        raise PreparationError("incomplete staged ARM64 guest overlay")
    record = {"tool": OWNER, "schema": SCHEMA, "build": BUILD, "sdk": 37, "release": "17",
              "archive_url": URL, "archive_sha256": SHA256, "architecture": "aarch64",
              "guest_abi": "arm64-v8a", "host_manifest_sha256": host_hash,
              "bridge_compatibility": BRIDGE_COMPATIBILITY.copy(), "vdso_build": vdso_build, **actual,
              "note": "Guest runtime only; extracted without mounting or booting; not redistributed."}
    (overlay / MANIFEST).write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")
    return overlay, record


def publish_tree(source, destination, journal, *, allow_links=False):
    with directory(destination.parent, writable=True) as parent:
        os.mkdir(destination.name, 0o700, dir_fd=parent)
        remember(journal, destination, os.stat(destination.name, dir_fd=parent, follow_symlinks=False))
    for child in sorted(source.iterdir()):
        target = destination / child.name
        info = child.lstat()
        if stat.S_ISDIR(info.st_mode):
            publish_tree(child, target, journal, allow_links=allow_links)
        elif stat.S_ISREG(info.st_mode):
            copy_regular(child, target, journal)
        elif stat.S_ISLNK(info.st_mode) and allow_links:
            with directory(destination, writable=True) as parent:
                os.symlink(os.readlink(child), child.name, dir_fd=parent)
                remember(journal, target, os.stat(child.name, dir_fd=parent, follow_symlinks=False))
        else:
            raise PreparationError(f"unsupported staged path: {child}")
    with directory(destination, writable=True) as fd:
        os.fchmod(fd, stat.S_IMODE(source.stat().st_mode) & 0o755)


def rollback(journal):
    failures = []
    for path, device, inode, kind in reversed(journal):
        try:
            with directory(path.parent, writable=True) as parent:
                info = os.stat(path.name, dir_fd=parent, follow_symlinks=False)
                if (info.st_dev, info.st_ino, stat.S_IFMT(info.st_mode)) != (device, inode, kind):
                    raise PreparationError(f"created path was replaced: {path}")
                if stat.S_ISDIR(kind):
                    os.rmdir(path.name, dir_fd=parent)
                else:
                    os.unlink(path.name, dir_fd=parent)
        except (OSError, PreparationError) as error:
            failures.append(f"{path}: {error}")
    return failures


def publish(overlay, sysroot, record, retained_stage=None, retained=None):
    expected = {key: record[key] for key in ("files", "directories")}
    if inventory(overlay) != expected:
        raise PreparationError("staged overlay changed before publication")
    journal = []
    try:
        for relative in OVERLAYS:
            publish_tree(overlay / relative, sysroot / relative, journal)
        if retained_stage is not None:
            publish_tree(retained_stage, retained, journal, allow_links=True)
        # Retention can take a long time; verify after it, not before it.
        if inventory(sysroot) != expected:
            raise PreparationError("published overlay does not match staged inventory")
        # Exclusive creation, last: a partial/crashed installation is never owned.
        copy_regular(overlay / MANIFEST, sysroot / MANIFEST, journal)
        if inventory(sysroot) != expected:
            raise PreparationError("published overlay changed during manifest publication")
    except BaseException as error:
        failures = rollback(journal)
        if failures:
            raise PreparationError("publication failed; rollback left paths for manual inspection: "
                                   + "; ".join(failures)) from error
        raise


def require_fresh_retention(retained):
    with directory(retained.parent, writable=True):
        pass
    if os.path.lexists(retained):
        raise PreparationError(f"refusing existing retained extraction (never replaced or deleted): {retained}")


def install(source, *, sysroot=None, vdso_source=None, compiler=None, keep_extracted=False, retained=None):
    sysroot = Path(sysroot if sysroot is not None else SYSROOT).absolute()
    retained = Path(retained if retained is not None else DEST).absolute()
    prepare = load_prepare_module()
    validate_host(sysroot, prepare)
    with publication_lock(sysroot):
        host_hash = validate_host(sysroot, prepare)
        existing = existing_overlay(sysroot, host_hash)
        if existing is not None:
            if keep_extracted:
                raise PreparationError("guest already installed; retaining a new extraction requires a fresh installation")
            print(f"ARM64 guest already verified; no files changed: {sysroot}")
            warn_staged_only()
            return len(existing["files"]), sysroot / MANIFEST
        if keep_extracted:
            require_fresh_retention(retained)
        with directory(sysroot.parent, writable=True):
            pass
        with tempfile.TemporaryDirectory(prefix=".prepare-arm64-", dir=sysroot.parent) as temporary:
            work = Path(temporary)
            root = Path(source(work))
            overlay, record = prepare_overlay(root, work, host_hash,
                                               vdso_source if vdso_source is not None else VDSO_SOURCE,
                                               compiler if compiler is not None else COMPILER)
            retained_stage = None
            if keep_extracted:
                retained_stage = work / "retained"
                retained_stage.mkdir(mode=0o700)
                # Exclusive private copies also keep permissive umasks from creating
                # writable staging directories. Links are data, never traversal paths.
                publish_tree(root, retained_stage / "root", None, allow_links=True)
                (retained_stage / MANIFEST).write_text(json.dumps({
                    "tool": OWNER, "schema": SCHEMA, "archive_sha256": SHA256,
                    "bridge_compatibility": BRIDGE_COMPATIBILITY.copy(),
                    "note": "Retained extraction; never automatically replaced or deleted. Symlinks are inert copies."
                }, indent=2, sort_keys=True) + "\n")
            if validate_host(sysroot, prepare) != host_hash:
                raise PreparationError("host manifest changed during guest preparation")
            if existing_overlay(sysroot, host_hash) is not None:
                raise PreparationError("guest destination appeared during preparation; not replacing it")
            if keep_extracted:
                require_fresh_retention(retained)
            publish(overlay, sysroot, record, retained_stage, retained)
    warn_staged_only()
    return len(record["files"]), sysroot / MANIFEST


def stage(root, **kwargs):
    """Install from an already extracted tree; also used by offline fixture tests."""
    return install(lambda work: root, **kwargs)


def tool(name):
    for candidate in (shutil.which(name), str(REPO / ".toolchain/host-tools/usr/bin" / name),
                      str(REPO / ".toolchain/host-tools/usr/sbin" / name), "/usr/sbin/" + name):
        if candidate and Path(candidate).is_file() and os.access(candidate, os.X_OK):
            return str(Path(candidate).resolve())
    raise PreparationError(f"missing {name}")


def extract_image(archive, work):
    prepare = load_prepare_module()
    try:
        # Hash and read the same open inode, not a subsequently replaced pathname.
        with regular(archive) as stream:
            before = os.fstat(stream.fileno())
            if hashlib.file_digest(stream, "sha256").hexdigest() != SHA256:
                raise PreparationError(f"ARM64 GSI SHA-256 mismatch: expected {SHA256}; nothing extracted")
            stream.seek(0)
            with zipfile.ZipFile(stream) as container:
                members = [entry for entry in container.infolist() if entry.filename == "system.img"]
                if len(members) != 1 or not 0 < members[0].file_size <= MAX_IMAGE:
                    raise PreparationError("archive must contain exactly one bounded system.img")
                image = work / "system.raw"
                with container.open(members[0]) as source, image.open("x+b") as output:
                    prefix = prepare.exact(source, 4)
                    source.seek(0)
                    if struct.unpack("<I", prefix)[0] == SPARSE_MAGIC:
                        prepare.expand_sparse(source, output)
                    else:
                        prepare.copy_bytes(source, output, members[0].file_size)
            unchanged(stream, before, archive)
    except prepare.PreparationError as error:
        raise PreparationError(str(error)) from error
    return image


def extract_filesystem(image, root):
    prepare = load_prepare_module()
    try:
        kind = prepare.filesystem(image)
    except prepare.PreparationError as error:
        raise PreparationError(str(error)) from error
    if kind == "erofs":
        command = [tool("fsck.erofs"), f"--extract={root}", "--no-preserve-owner", str(image)]
    else:
        quoted = str(root).replace("\\", "\\\\").replace('"', '\\"')
        if any(character in quoted for character in "\n\r\0"):
            raise PreparationError("unsupported control character in extraction path")
        command = [tool("debugfs"), "-R", f'rdump / "{quoted}"', str(image)]
    result = subprocess.run(command, capture_output=True, text=True, timeout=1200)
    if result.returncode:
        raise PreparationError(f"filesystem extraction failed: {result.stdout[-3000:]}{result.stderr[-3000:]}")
    confined(root, root / "system/bin/app_process64")


def unpack_archive(archive, work):
    image = extract_image(archive, work)
    root = work / "root"
    root.mkdir()
    extract_filesystem(image, root)
    image.unlink()
    env = dict(os.environ)
    env["PATH"] = str(Path(tool("debugfs")).parent) + os.pathsep + env.get("PATH", "")
    subprocess.run([sys.executable, "-B", str(REPO / "tools/art/unpack-apex.py"), str(root),
                    "--erofs", tool("fsck.erofs")], check=True, env=env, timeout=1200)
    subprocess.run([sys.executable, "-B", str(REPO / "tools/art/sysroot-relink.py"), str(root)],
                   check=True, timeout=120)
    return root


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, default=REPO / ".toolchain/downloads/aosp_arm64-gsi.zip")
    parser.add_argument("--keep-extracted", action="store_true")
    args = parser.parse_args()
    try:
        copied, manifest = install(lambda work: unpack_archive(args.archive, work),
                                   keep_extracted=args.keep_extracted)
    except (PreparationError, OSError, ValueError, subprocess.SubprocessError, zipfile.BadZipFile) as error:
        print(f"prepare-arm64-guest: {error}", file=sys.stderr)
        return 1
    print(f"verified {copied} ARM64 guest files from {BUILD}")
    print(f"manifest: {manifest}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
