#!/usr/bin/env python3
"""Fetch the pinned AOSP source closure the ARM64 guest build compiles from.

Run: python3 -B tools/fetch-guest-sources.py [--offline] [--only NAME ...] [--skip NAME ...]

`tools/nativebridge-deps.py` fetches header *slices* on purpose, so it cannot
supply the full trees `tools/build-guest-linker.py` compiles: the pinned Bionic
tree (`sources/bionic-expansion/bionic`) and the external projects under
`sources/ext/`. Those were previously assembled by hand, which made the guest
linker build unreproducible. This tool is the missing pinned fetcher.

Two facts drive the layout:

* `EXT_PINS` in the driver is keyed by **AOSP path** (`system/core/libcutils`),
  while a googlesource archive is a **project** snapshot (`platform/system/core`).
  So each input is (project, subpath): the project archive is downloaded once
  per revision and only the pinned subpath is materialised.
* The destination names are the ones the driver hard-codes
  (`sources/ext/libcutils`, `sources/bionic-expansion/bionic`, …).

Archives are hashed and recorded in a manifest, extraction is bounded and
path-checked, and nothing outside `.toolchain/nativebridge-deps/` is written.
An already-verified archive is reused, so a rerun is offline-cheap.
"""

import argparse
import ast
import fcntl
import gzip
import hashlib
import json
import os
import shutil
import sys
import tarfile
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, asdict
from pathlib import Path

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parent.parent
DEPS = REPO / ".toolchain/nativebridge-deps"
SOURCES = DEPS / "sources"
ARCHIVES = DEPS / "archives"
MANIFEST = "guest-sources.json"
OWNER = {"tool": "PenguDroid guest source fetcher", "schema": 1}
HOST = "https://android.googlesource.com"
MAX_ARCHIVE = 512 * 1024 * 1024
MAX_MEMBERS = 200_000
MAX_EXTRACTED = 4_000_000_000
TIMEOUT = 180

# (name, project, subpath, pin key, destination relative to sources/)
# `pin key` is looked up in the driver's EXT_PINS (or BIONIC_PIN for "bionic").
# libutils/libsystem take the libcutils revision: same project, same commit.
LAYOUT = (
    ("bionic", "platform/bionic", "", "bionic", "bionic-expansion/bionic"),
    # `bionic/libdl` is its own tree for the driver, from the same pinned archive.
    ("bionic-libdl", "platform/bionic", "libdl", "bionic", "ext/bionic-libdl"),
    ("arm-optimized-routines", "platform/external/arm-optimized-routines", "",
     "external/arm-optimized-routines", "ext/arm-optimized-routines"),
    ("google-highway", "platform/external/google-highway", "",
     "external/google-highway", "ext/google-highway"),
    ("llvm-libc", "platform/external/llvm-libc", "", "external/llvm-libc", "ext/llvm-libc"),
    # The driver indexes crates as `<dest>/<crate>/Android.bp`, so the pin's
    # `crates/` subtree is the destination root, not the project root.
    ("android-crates-io", "platform/external/rust/android-crates-io", "crates",
     "external/rust/android-crates-io", "ext/android-crates-io"),
    ("librustutils", "platform/system/librustutils", "", "system/librustutils", "ext/librustutils"),
    ("scudo", "platform/external/scudo", "", "external/scudo", "ext/scudo"),
    ("gwp_asan", "platform/external/gwp_asan", "", "external/gwp_asan", "ext/gwp_asan"),
    ("soong", "platform/build/soong", "", "build/soong", "ext/soong"),
    ("libcutils", "platform/system/core", "libcutils", "system/core/libcutils", "ext/libcutils"),
    ("libutils", "platform/system/core", "libutils", "system/core/libcutils", "ext/libutils"),
    ("libsystem", "platform/system/core", "libsystem", "system/core/libcutils", "ext/libsystem"),
    ("libpropertyinfoparser", "platform/system/core", "property_service/libpropertyinfoparser",
     "system/core/property_service/libpropertyinfoparser", "ext/libpropertyinfoparser"),
    # `sources/libbase`, `sources/logging`, `sources/zlib` and
    # `sources/libziparchive` are already provided (and hash-verified) by
    # `tools/nativebridge-deps.py`; re-fetching them here would break that
    # manifest's tree digest, so they are deliberately absent.
)

# The driver also links six prebuilt libc++/unwind/builtins archives from
# `.toolchain/nativebridge-deps/sources/ext/clang-prebuilt`. That tree is a
# multi-gigabyte compiler prebuilt, so it is reported, never silently faked.
PREBUILT_ARCHIVES = (
    "libc++_static_noexcept.a", "libc++abi_noexcept.a", "libc++demangle_noexcept.a",
    "libunwind.a", "libunwind-exported.a", "libclang_rt.builtins-aarch64-android.a",
)


class FetchError(RuntimeError):
    pass


@dataclass(frozen=True)
class Input:
    name: str
    project: str
    subpath: str
    revision: str
    dest: str

    @property
    def archive_name(self):
        return self.project.replace("/", "_") + "@" + self.revision[:16] + ".tar.gz"

    @property
    def url(self):
        return f"{HOST}/{self.project}/+archive/{self.revision}.tar.gz"


def driver_pins():
    """Read the pins out of the build driver instead of restating them."""
    tree = ast.parse((REPO / "tools/build-guest-linker.py").read_text())
    pins = {}
    for node in tree.body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1 \
                and isinstance(node.targets[0], ast.Name):
            if node.targets[0].id == "BIONIC_PIN":
                pins["bionic"] = ast.literal_eval(node.value)
            elif node.targets[0].id == "EXT_PINS":
                pins.update(ast.literal_eval(node.value))
    if "bionic" not in pins:
        raise FetchError("BIONIC_PIN is missing from tools/build-guest-linker.py")
    return pins


def inputs():
    pins = driver_pins()
    out = []
    for name, project, subpath, key, dest in LAYOUT:
        revision = pins.get(key)
        if revision is None:
            raise FetchError(f"no pin for {name} ({key}); add it to EXT_PINS")
        out.append(Input(name, project, subpath, revision, dest))
    return out


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def download(item):
    ARCHIVES.mkdir(parents=True, exist_ok=True)
    destination = ARCHIVES / item.archive_name
    temporary = destination.with_suffix(".part")
    request = urllib.request.Request(item.url, headers={"User-Agent": OWNER["tool"]})
    written = 0
    try:
        with urllib.request.urlopen(request, timeout=TIMEOUT) as response, temporary.open("wb") as out:
            if response.status != 200:
                raise FetchError(f"{item.name}: HTTP {response.status} for {item.url}")
            while True:
                block = response.read(1 << 20)
                if not block:
                    break
                written += len(block)
                if written > MAX_ARCHIVE:
                    raise FetchError(f"{item.name}: archive exceeds the {MAX_ARCHIVE} byte budget")
                out.write(block)
    except urllib.error.HTTPError as error:
        raise FetchError(f"{item.name}: {error.code} {error.reason} for {item.url}") from error
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise
    if written == 0:
        temporary.unlink(missing_ok=True)
        raise FetchError(f"{item.name}: empty archive from {item.url}")
    os.replace(temporary, destination)
    return destination, written


def verify_gzip(path, name):
    with path.open("rb") as handle:
        if handle.read(2) != b"\x1f\x8b":
            raise FetchError(f"{name}: cached archive is not gzip")
    try:
        with gzip.open(path, "rb") as handle:
            while handle.read(1 << 20):
                pass
    except OSError as error:
        raise FetchError(f"{name}: corrupt gzip stream: {error}") from error


def escapes(member):
    """True when a symlink target leaves the extracted tree."""
    target = Path(member.linkname)
    if target.is_absolute():
        return True
    depth = 0
    for part in (Path(member.name).parent / target).parts:
        depth += -1 if part == ".." else 1
        if depth < 0:
            return True
    return False


def safe_member(member):
    name = member.name
    if name.startswith("/"):
        raise FetchError(f"refusing absolute archive member: {name}")
    parts = Path(name).parts
    if any(part == ".." for part in parts):
        raise FetchError(f"refusing escaping archive member: {name}")
    if not (member.isreg() or member.isdir() or member.issym()):
        raise FetchError(f"refusing archive member type of {name}")


def extract_members(tar, staging, item):
    """Extract bounded members; AOSP cross-project symlinks become inert text."""
    members = 0
    total = 0
    inert = 0
    for member in tar:
        members += 1
        if members > MAX_MEMBERS:
            raise FetchError(f"{item.name}: more than {MAX_MEMBERS} archive members")
        safe_member(member)
        total += max(member.size, 0)
        if total > MAX_EXTRACTED:
            raise FetchError(f"{item.name}: extraction exceeds the {MAX_EXTRACTED} byte budget")
        target = staging / member.name
        if member.isdir():
            target.mkdir(parents=True, exist_ok=True)
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        if member.issym():
            if escapes(member):
                # AOSP format/style links point at other projects. Keep the
                # target text, exactly as `tools/nativebridge-deps.py` does, so
                # no link can reach outside the owned tree.
                target.write_text(member.linkname + "\n")
                inert += 1
            elif not target.exists():
                os.symlink(member.linkname, target)
            continue
        source = tar.extractfile(member)
        if source is None:
            raise FetchError(f"{item.name}: cannot read {member.name}")
        with target.open("wb") as out:
            shutil.copyfileobj(source, out)
        os.chmod(target, (member.mode & 0o777) or 0o644)
    return members, total, inert


def extract(item, archive):
    """Extract the pinned subpath of `archive` into the driver's destination."""
    destination = SOURCES / item.dest
    staging = destination.with_name(destination.name + ".extract")
    shutil.rmtree(staging, ignore_errors=True)
    staging.mkdir(parents=True)
    with tarfile.open(archive, "r:gz") as tar:
        members, _unpacked, inert = extract_members(tar, staging, item)
    source = staging / item.subpath if item.subpath else staging
    if not source.is_dir():
        raise FetchError(f"{item.name}: pinned subpath {item.subpath!r} is absent from {item.project}")
    if not any(source.rglob("Android.bp")):
        raise FetchError(f"{item.name}: no Android.bp anywhere under {item.subpath or '.'}; wrong pin?")
    shutil.rmtree(destination, ignore_errors=True)
    destination.parent.mkdir(parents=True, exist_ok=True)
    os.replace(source, destination)
    shutil.rmtree(staging, ignore_errors=True)
    return members, inert


def tree_bytes(path):
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            try:
                total += (Path(root) / name).stat().st_size
            except OSError:
                pass
    return total


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--offline", action="store_true", help="verify and reuse the cache only")
    parser.add_argument("--only", nargs="+", default=None, metavar="NAME")
    parser.add_argument("--skip", nargs="+", default=[], metavar="NAME")
    parser.add_argument("--force", action="store_true", help="re-extract even when already verified")
    args = parser.parse_args()

    SOURCES.mkdir(parents=True, exist_ok=True)
    ARCHIVES.mkdir(parents=True, exist_ok=True)
    path = DEPS / MANIFEST
    manifest = json.loads(path.read_text()) if path.exists() else {
        **OWNER, "platform": "Android 17 / API 37", "inputs": {},
    }
    if manifest.get("tool") != OWNER["tool"] or manifest.get("schema") != OWNER["schema"]:
        raise FetchError("manifest owner/schema changed")

    with (DEPS / ".guest-sources.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise FetchError("another guest source fetch is running") from error

        selected = [i for i in inputs()
                    if (args.only is None or i.name in args.only) and i.name not in args.skip]
        for item in selected:
            destination = SOURCES / item.dest
            record = manifest["inputs"].get(item.name)
            cached = ARCHIVES / item.archive_name
            if (not args.force and record and record.get("revision") == item.revision
                    and record.get("subpath") == item.subpath and cached.is_file()
                    and sha256(cached) == record.get("sha256") and destination.is_dir()):
                print(f"verified {item.name} ({record['extracted_bytes']:,} bytes, "
                      f"{item.revision[:12]})", flush=True)
                continue
            if args.offline:
                raise FetchError(f"{item.name}: no verified cache and --offline was requested")
            started = time.monotonic()
            archive, downloaded = download(item)
            verify_gzip(archive, item.name)
            members, inert = extract(item, archive)
            manifest["inputs"][item.name] = {
                **asdict(item), "url": item.url, "sha256": sha256(archive),
                "archive_bytes": downloaded, "extracted_bytes": tree_bytes(destination),
                "members": members, "inert_links": inert,
                "seconds": round(time.monotonic() - started, 1),
            }
            path.write_text(json.dumps(manifest, indent=2) + "\n")
            print(f"fetched  {item.name} ({downloaded:,} archive bytes, {members:,} entries, "
                  f"{manifest['inputs'][item.name]['extracted_bytes']:,} on disk, "
                  f"{item.revision[:12]})", flush=True)

    report = ["# Pinned guest source closure", "",
              "Written by `tools/fetch-guest-sources.py`; revisions come from "
              "`tools/build-guest-linker.py`.", ""]
    for name, record in sorted(manifest["inputs"].items()):
        report.append(f"- `{name}`: `{record['project']}`"
                      + (f" `/`{record['subpath']}`" if record.get("subpath") else "")
                      + f" @ `{record['revision']}`, archive SHA-256 `{record['sha256'][:16]}…`, "
                      f"{record['extracted_bytes']:,} bytes, {record['members']:,} entries")
    prebuilt = SOURCES / "ext/clang-prebuilt"
    missing = [name for name in PREBUILT_ARCHIVES if not (prebuilt / name).is_file()]
    report += ["", "## Not fetched", "",
               "`sources/ext/clang-prebuilt` is a multi-gigabyte `prebuilts/clang/host/linux-x86` "
               "snapshot and is not downloaded here. The driver links these archives from it, "
               "so the guest link step cannot run until they are supplied:", ""]
    report += [f"- `{name}`" for name in missing] or ["- (all present)"]
    (SOURCES / "GUEST_SOURCES.md").write_text("\n".join(report) + "\n")
    print(f"\nManifest: {path}")
    if missing:
        print(f"note: {len(missing)} prebuilt archive(s) still missing from "
              f"{prebuilt} (see {SOURCES / 'GUEST_SOURCES.md'})")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except FetchError as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)
