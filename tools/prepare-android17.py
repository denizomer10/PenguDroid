#!/usr/bin/env python3
"""Prepare the pinned API-37 x86_64 runtime without mounts or root privileges.

The GSI is an acquisition archive, never a mounted/booted runtime dependency.
Only extracted files survive preparation. Review Google's GSI license before
obtaining the archive; do not redistribute its binaries with this project.

    python3 -B tools/prepare-android17.py --archive /path/to/aosp_x86_64-....zip

Needs debugfs (ext4) and fsck.erofs (EROFS/APEX), found on PATH or locally in
.toolchain/host-tools/. Android sparse images are expanded with bounded memory.
"""

import argparse
import hashlib

import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
import zipfile
import zlib

REPO = Path(__file__).resolve().parent.parent
FILENAME = "aosp_x86_64-exp-CP2A.260605.012-15430684-3da2ca55.zip"
URL = "https://dl.google.com/developers/android/cinnamonbun/images/gsi/" + FILENAME
SHA256 = "3da2ca5599d304c4d92fe8c72f31d035a59db57c93ce073740ed4015ee034a8e"
BUILD = "CP2A.260605.012"
OWNER = "PenguDroid API-37 runtime preparation"
MANIFEST = "pengudroid-runtime.json"
SPARSE_MAGIC = 0xED26FF3A
MAX_IMAGE = 16 * 1024**3
BUFFER = 4 * 1024**2
REQUIRED = (
    "system/build.prop",
    "system/etc/classpaths/bootclasspath.pb",
    "system/framework/framework.jar",
    "system/framework/x86_64/boot.art",
    "system/framework/x86_64/boot.oat",
    "system/apex/com.android.art/apex_manifest.pb",
    "system/apex/com.android.art/lib64/libart.so",
    "system/apex/com.android.art/lib64/libartbase.so",
    "system/apex/com.android.art/lib64/libdexfile.so",
    "system/apex/com.android.art/lib64/libnativehelper.so",
    "system/apex/com.android.art/javalib/core-oj.jar",
    "system/apex/com.android.art/javalib/core-libart.jar",
    "system/apex/com.android.i18n/javalib/core-icu4j.jar",
    "system/lib64/libc.so",
    "system/lib64/libandroid.so",
    "system/lib64/libdl_android.so",
    # Link input for the native bridge's libbase dependency; the bridge build
    # does not recompile system/core's liblog from the incomplete source slice.
    "system/lib64/liblog.so",
)


class PreparationError(RuntimeError):
    pass


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def exact(stream, count):
    data = stream.read(count)
    if len(data) != count:
        raise PreparationError("truncated image")
    return data


def copy_bytes(source, destination, count, crc=0):
    while count:
        chunk = exact(source, min(count, BUFFER))
        destination.write(chunk)
        crc = zlib.crc32(chunk, crc)
        count -= len(chunk)
    return crc


def expand_sparse(source, destination):
    """Android sparse v1; validate sizes/CRC and retain DONT_CARE as holes."""
    header = exact(source, 28)
    magic, major, minor, file_header, chunk_header, block, blocks, chunks, checksum = struct.unpack("<I4H4I", header)
    if magic != SPARSE_MAGIC or major != 1 or minor != 0:
        raise PreparationError("unsupported Android sparse image version")
    if not (28 <= file_header <= 4096 and 12 <= chunk_header <= 4096):
        raise PreparationError("invalid sparse header sizes")
    if not block or block % 4 or blocks * block > MAX_IMAGE or chunks > 4_000_000:
        raise PreparationError("invalid or oversized sparse image")
    exact(source, file_header - 28)
    written = 0
    crc = 0
    zero = bytes(BUFFER)
    for _ in range(chunks):
        kind, reserved, count, size = struct.unpack("<2H2I", exact(source, 12))
        exact(source, chunk_header - 12)
        length = count * block
        if size < chunk_header or written + length > blocks * block:
            raise PreparationError("invalid sparse chunk size")
        payload = size - chunk_header
        if kind == 0xCAC1:
            if payload != length:
                raise PreparationError("RAW sparse chunk payload mismatch")
            crc = copy_bytes(source, destination, length, crc)
        elif kind in (0xCAC2, 0xCAC3):
            expected = 4 if kind == 0xCAC2 else 0
            if payload != expected:
                raise PreparationError("FILL/DONT_CARE sparse chunk payload mismatch")
            pattern = exact(source, 4) if kind == 0xCAC2 else b"\0" * 4
            buffer = pattern * (BUFFER // 4) if kind == 0xCAC2 else zero
            remaining = length
            while remaining:
                amount = min(remaining, BUFFER)
                part = buffer[:amount]
                if kind == 0xCAC2:
                    destination.write(part)
                else:
                    destination.seek(amount, os.SEEK_CUR)
                crc = zlib.crc32(part, crc)
                remaining -= amount
        elif kind == 0xCAC4:
            if count or payload != 4:
                raise PreparationError("invalid CRC32 sparse chunk")
            if struct.unpack("<I", exact(source, 4))[0] != crc:
                raise PreparationError("sparse CRC32 chunk mismatch")
        else:
            raise PreparationError(f"unknown sparse chunk type: {kind:#x}")
        written += length
    if written != blocks * block:
        raise PreparationError("sparse image block count mismatch")
    if checksum and checksum != crc:
        raise PreparationError("sparse image checksum mismatch")
    if source.read(1):
        raise PreparationError("trailing sparse image data")
    destination.truncate(written)
    return written


def filesystem(image):
    with image.open("rb") as stream:
        stream.seek(1024)
        if stream.read(4) == bytes.fromhex("e2e1f5e0"):
            return "erofs"
        stream.seek(1080)
        if stream.read(2) == b"\x53\xef":
            return "ext4"
    raise PreparationError("image is neither EROFS nor ext4")


def image_from_archive(archive, work):
    if digest(archive) != SHA256:
        raise PreparationError(f"GSI SHA-256 mismatch: expected {SHA256}; nothing extracted")
    print(f"verified {archive.name}: {SHA256}", flush=True)
    with zipfile.ZipFile(archive) as container:
        # Never extractall: only this exact member is read, ignoring ZIP paths.
        members = [entry for entry in container.infolist() if entry.filename == "system.img"]
        if len(members) != 1 or not 0 < members[0].file_size <= MAX_IMAGE:
            raise PreparationError("archive must contain exactly one bounded system.img")
        image = work / "system.raw"
        with container.open(members[0]) as source, image.open("w+b") as output:
            prefix = exact(source, 4)
            source.seek(0)
            if struct.unpack("<I", prefix)[0] == SPARSE_MAGIC:
                expand_sparse(source, output)
            else:
                copy_bytes(source, output, members[0].file_size)
    return image


def tool(name, explicit=None):
    candidates = [explicit] if explicit else [
        shutil.which(name),
        str(REPO / ".toolchain/host-tools/usr/bin" / name),
        str(REPO / ".toolchain/host-tools/usr/sbin" / name),
        "/usr/sbin/" + name,
    ]
    for candidate in candidates:
        if candidate and Path(candidate).is_file() and os.access(candidate, os.X_OK):
            return str(Path(candidate).resolve())
    raise PreparationError(f"missing {name}; supply --{name.replace('.', '-')} or install it locally")


def validate_runtime(root):
    for relative in REQUIRED:
        path = root / relative
        if not path.is_file() or not path.stat().st_size:
            raise PreparationError(f"incomplete API-37 runtime: {relative}")
        if not path.resolve().is_relative_to(root.resolve()):
            raise PreparationError(f"runtime link escapes extracted tree: {relative}")
    props = {}
    for line in (root / "system/build.prop").read_text().splitlines():
        if not line.strip() or line.lstrip().startswith("#") or "=" not in line:
            continue
        key, value = (part.strip() for part in line.split("=", 1))
        if key not in ("ro.build.version.sdk", "ro.build.version.release", "ro.build.id"):
            continue
        if key in props:
            raise PreparationError(f"duplicate build property: {key}")
        props[key] = value
    for key, expected in (("ro.build.version.sdk", "37"), ("ro.build.version.release", "17"), ("ro.build.id", BUILD)):
        if props.get(key) != expected:
            raise PreparationError(f"{key}: expected {expected}, found {props.get(key)!r}; no runtime fallback")
    with (root / "system/apex/com.android.art/lib64/libart.so").open("rb") as stream:
        header = exact(stream, 20)
    if header[:6] != b"\x7fELF\x02\x01" or struct.unpack_from("<H", header, 18)[0] != 62:
        raise PreparationError("ART must be a little-endian ELF64 x86_64 library")
    return {relative: digest(root / relative) for relative in REQUIRED}



def prepare(archive, output, erofs=None, debugfs=None):
    if output.is_symlink():
        raise PreparationError(f"refusing output symlink: {output}")
    if output.exists():
        manifest = output / MANIFEST
        if not manifest.is_file():
            raise PreparationError(f"refusing existing unowned directory: {output}")
        record = json.loads(manifest.read_text())
        if record.get("tool") != OWNER or record.get("archive_sha256") != SHA256:
            raise PreparationError("existing runtime was prepared from a different source")
        hashes = validate_runtime(output)
        if record.get("files") != hashes:
            raise PreparationError("prepared runtime files changed; not overwriting them")
        print(f"API-37 x86_64 runtime already verified: {output}", flush=True)
        return
    if not archive.is_file():
        raise PreparationError(f"missing archive: {archive}\nOfficial source: {URL}\nExpected SHA-256: {SHA256}")
    erofs = tool("fsck.erofs", erofs)
    debugfs = tool("debugfs", debugfs)
    if shutil.disk_usage(output.parent).free < 12 * 1024**3:
        raise PreparationError("runtime preparation requires at least 12 GiB free disk space")
    with tempfile.TemporaryDirectory(prefix=".prepare-api37-", dir=output.parent) as temporary:
        work = Path(temporary)
        image = image_from_archive(archive, work)
        root = work / "root"
        root.mkdir()
        kind = filesystem(image)
        print(f"extracting {kind} offline, without mounts", flush=True)
        if kind == "erofs":
            command = [erofs, f"--extract={root}", "--no-preserve-owner", str(image)]
        else:
            # debugfs has its own command parser, so quote its destination too.
            quoted = str(root).replace("\\", "\\\\").replace('"', '\\"')
            if any(c in quoted for c in "\n\r\0"):
                raise PreparationError("unsupported control character in output path")
            command = [debugfs, "-R", f'rdump / "{quoted}"', str(image)]
        result = subprocess.run(command, capture_output=True, text=True, timeout=300)
        if result.returncode or not (root / "system/build.prop").is_file():
            raise PreparationError(f"filesystem extraction failed: {result.stdout[-3000:]}{result.stderr[-3000:]}")
        (root / "pengudroid-extraction.log").write_text(result.stdout + result.stderr)
        image.unlink()
        # unpack-apex resolves debugfs via PATH; /usr/sbin need not be on users' PATH.
        env = dict(os.environ)
        env["PATH"] = str(Path(debugfs).parent) + os.pathsep + env.get("PATH", "")
        subprocess.run([sys.executable, "-B", str(REPO / "tools/art/unpack-apex.py"), str(root), "--erofs", erofs],
                       check=True, env=env, timeout=300)
        subprocess.run([sys.executable, "-B", str(REPO / "tools/art/sysroot-relink.py"), str(root)], check=True, timeout=60)
        files = validate_runtime(root)
        record = {"tool": OWNER, "schema": 1, "archive_url": URL, "archive_sha256": SHA256,
                  "build": BUILD, "sdk": 37, "release": "17", "architecture": "x86_64", "files": files,
                  "note": "Local compatibility-test fixture only; no image is mounted, booted, or redistributed."}
        (root / MANIFEST).write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")
        # Never merge into a partially prepared/user-owned runtime.
        if output.exists() or output.is_symlink():
            raise PreparationError(f"destination appeared during preparation: {output}")
        root.rename(output)
    print(f"prepared Android 17 / API 37 x86_64: {output}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, default=REPO / ".toolchain/downloads" / FILENAME)
    parser.add_argument("--output", type=Path, default=REPO / ".toolchain/sysroot-android17")
    parser.add_argument("--fsck-erofs", dest="erofs")
    parser.add_argument("--debugfs")
    args = parser.parse_args()
    output = args.output.absolute()
    if not output.parent.is_dir():
        parser.error(f"output parent does not exist: {output.parent}")
    try:
        prepare(args.archive, output, args.erofs, args.debugfs)
    except (PreparationError, OSError, ValueError, subprocess.SubprocessError, zipfile.BadZipFile) as error:
        print(f"prepare-android17: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
