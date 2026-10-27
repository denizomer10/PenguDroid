#!/usr/bin/env python3
"""Fetch pinned native-bridge build inputs; this does NOT build a translator.

Run: python3 -B tools/nativebridge-deps.py [--offline] [--download-only]
Tests: python3 -B -m unittest discover -s tools -p test_nativebridge_deps.py -v

Requires Python 3.11+ on Linux. All writes, including preflight outputs, stay in
.toolchain/nativebridge-deps/.
An existing API-37 NDK platform sysroot is read, not changed. No Android runtime,
full NDK, LLVM checkout, or AOSP build environment is downloaded. Archives retain
upstream copyright/license files. LLVM's NOTICE also covers the runtime inputs.
Allow-listed cross-project config links become inert link-target text files;
the manifest records both their original and materialized Git tree identities.
"""

import argparse
import base64
import fcntl
import gzip
import hashlib
import json
import os
import re
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request
from dataclasses import asdict, dataclass
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
ROOT = REPO / ".toolchain/nativebridge-deps"
SYSROOT = REPO / ".toolchain/ndk-refs_tags_ndk-r30-platform-sysroot"
SUPERPROJECT = "d6985d7ea38e602e9b1c94e2ebcbafacd7cc9892"
CLANG_REV = "29182889399818f97baa65ea238c80e8117981eb"
CLANG_PROJECT = "prebuilts/clang/host/linux-x86"
CLANG_PATH = "clang-r584948"
MAX_DOWNLOAD = 150_000_000  # Includes base64 transport bytes, not just decoded files.
MAX_TAR = 512 * 1024 * 1024
MAX_MEMBERS = 30_000
MAX_EXTRACTED = 1_000_000_000
# AOSP format configs and libc++'s default HOST config link outside their slice.
# Keep exact link-target bytes as inert text, not escaping links. Android's real
# __config_site must precede the common headers (enforced by the smoke compile).
INERT_LINKS = {
    ".clang-format": "../../build/soong/scripts/system-clang-format-2",
    ".clang-format-2": "../../build/soong/scripts/system-clang-format-2",
    ".clang-format-4": "../../build/soong/scripts/system-clang-format",
    "rust/rustfmt.toml": "../../../build/soong/scripts/rustfmt.toml",
    "__config_site": "../../x86_64-unknown-linux-gnu/c++/v1/__config_site",
}
OWNER = {"tool": "PenguDroid nativebridge-deps", "schema": 1}
NEXT_STEPS = [
    ("Not built: a native-only driver reproducing Soong's per-module sources, guest-state "
     "headers/defines, Python codegen, host intrinsic generator, and whole-static linkage."),
    ("Not built: support archives, libberberis_arm64, executable-region helper, and host proxies; "
     "the API-37 platform libdl_android/namespace dependencies are not NDK link stubs."),
    ("Not supplied: matching modified ARM64 linker, app_process, vdso, libc and other guest "
     "libraries. Bionic here is private/platform header slices, not a guest build tree; "
     "an ordinary x86_64 GSI does not supply that translated guest environment."),
    ("Not tested: Bionic TLS/signals/namespace integration under PenguDroid, NativeBridgeItf "
     "initialization, or execution of the ARM64 fixture. No older runtime is a fallback."),
]


class BootstrapError(RuntimeError):
    pass


@dataclass(frozen=True)
class Artifact:
    name: str
    project: str
    revision: str
    destination: str
    tree: str = ""
    subtree: str = ""
    sha256: str = ""
    blob: str = ""
    github: bool = False
    limit: int = 32_000_000

    @property
    def url(self):
        if self.github:
            return f"https://codeload.github.com/{self.project}/tar.gz/{self.revision}"
        base = f"https://android.googlesource.com/platform/{self.project}"
        if self.blob:
            return f"{base}/+/{self.revision}/{self.subtree}?format=TEXT"
        suffix = f"/{self.subtree}" if self.subtree else ""
        return f"{base}/+archive/{self.revision}{suffix}.tar.gz"

    @property
    def prefix(self):
        return f"platform_frameworks_libs_binary_translation-{self.revision}" if self.github else ""


def source(name, project, revision, tree):
    return Artifact(name, project, revision, f"sources/{name}", tree=tree)


# AOSP tree IDs were checked against commit metadata in the pinned superproject.
# The Digitalis checksum pins the publisher's commit-addressed codeload archive;
# it is not an independently signed upstream release checksum.
ARTIFACTS = (
    Artifact("digitalis", "DigitalisX64/platform_frameworks_libs_binary_translation",
             "2d03335e3c6bee947ace72f0aa717ff4f8db138b", "sources/digitalis",
             sha256="dd2e63f14581c6baa756c6000a939c204658a606764af756b1e708e7c8559f1f", github=True),
    # Whole Bionic includes large test payloads; these are the bridge's private headers only.
    Artifact("bionic_private", "bionic", "06356e41c5ed7b12220b24c05ba4fdb873b126a7", "sources/bionic/libc/private",
             tree="77d85e8bb228598a67bb794cdc530d9a64dd247f", subtree="libc/private", limit=2_000_000),
    Artifact("bionic_platform", "bionic", "06356e41c5ed7b12220b24c05ba4fdb873b126a7", "sources/bionic/libc/platform",
             tree="e9fd388f218c650698f9e30d9fe78b1d0189e0dc", subtree="libc/platform", limit=2_000_000),
    Artifact("bionic_async_headers", "bionic", "06356e41c5ed7b12220b24c05ba4fdb873b126a7", "sources/bionic/libc/async_safe/include",
             tree="fe2ee912650640af72794aa67d934b58743c49a9", subtree="libc/async_safe/include", limit=1_000_000),
    Artifact("bionic_notice", "bionic", "06356e41c5ed7b12220b24c05ba4fdb873b126a7", "sources/bionic/libc/NOTICE",
             blob="65229c48bcfcd63f6b3f6e64f7745688dcd8a118", subtree="libc/NOTICE", limit=3_000_000),
    source("native_bridge_support", "frameworks/libs/native_bridge_support", "3d6ed1bcc2f0be54f92cf40458253647e8891176", "8df11afe742fc30403e7e9d961a9441d45a13744"),
    source("libbase", "system/libbase", "fef4173e19d97cd4b4fc09a06bccbf800d0d3937", "4b0a1a3f9ad1c7bb80400e007d52656537798720"),
    source("libprocinfo", "system/libprocinfo", "b58259e79c58deb1132e11d74ea90a2dc441d84f", "548f404af16e058d114aebfbaaab4249665c21ec"),
    source("libziparchive", "system/libziparchive", "bdd41f9c99400f241363b035eb64ad3d5f063c82", "6019fcd35eac4cd92dca3aee6271c557148f7938"),
    source("logging", "system/logging", "d35ba620451b28512f46ebb5421f783fd77968d5", "2d287c1e04ae4ebd59b12f1b1aabcfa1f0fb8db4"),
    source("fmtlib", "external/fmtlib", "e3bef0ad2e9d4ea38ba3b4b2b59e0514f7025fed", "8e56efb48e45b106b4aefb3a3d99e59452e48c7d"),
    source("zlib", "external/zlib", "8731e6760935ad5289fd64c37d28155a0fa289e3", "576622c9d9cd32d08283e65cf0d85171db86c98e"),
    source("googletest", "external/googletest", "9bcfc87429e9a9e02ccf2b8424b748f05c4842a5", "d5dbf0dadc7234659387d1f3acb8aba1e7f7a55c"),
    Artifact("libcxx_headers", CLANG_PROJECT, CLANG_REV, "toolchain/libcxx/include/c++/v1",
             tree="3b8952d3cbcf0ae7bf4cc978f3f7e5e7f0c648cf", subtree=f"{CLANG_PATH}/include/c++/v1", limit=4_000_000),
    Artifact("libcxx_platform_x86_64", CLANG_PROJECT, CLANG_REV, "toolchain/libcxx/android/x86_64",
             tree="00d5c88fca6f96f107bdb1305cdbb454eb8226f0", subtree=f"{CLANG_PATH}/android_libc++/platform/x86_64", limit=16_000_000),
    Artifact("builtins_x86_64", CLANG_PROJECT, CLANG_REV, "toolchain/libclang_rt.builtins-x86_64-android.a",
             blob="4740d9cea7a5f454eb759b769aa60269f0f2154b", subtree=f"{CLANG_PATH}/lib/clang/22/lib/linux/libclang_rt.builtins-x86_64-android.a", limit=4_000_000),
    Artifact("llvm_notice", CLANG_PROJECT, CLANG_REV, "toolchain/NOTICE",
             blob="b051002f8a69f553700ee907ccd3c56d9a2c6c8e", subtree=f"{CLANG_PATH}/NOTICE", limit=8_000_000),
)


def sha256_file(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def git_hash(kind, data):
    return hashlib.sha1(kind + b" " + str(len(data)).encode() + b"\0" + data).digest()


def tree_digest(root, inert_links=None):
    """Git's directory ordering/modes; optional recorded inert link reconstruction."""
    entries = []
    for path in root.iterdir():
        name = os.fsencode(path.name)
        if inert_links and path.name in inert_links:
            if path.is_symlink() or not path.is_file() or path.read_bytes() != inert_links[path.name].encode():
                raise BootstrapError(f"modified inert upstream link: {path}")
            mode, digest, key = b"120000", git_hash(b"blob", path.read_bytes()), name
        elif path.is_symlink():
            mode, digest, key = b"120000", git_hash(b"blob", os.fsencode(os.readlink(path))), name
        elif path.is_dir():
            child_links = {key[len(path.name) + 1:]: value for key, value in (inert_links or {}).items()
                           if key.startswith(path.name + "/")}
            mode, digest, key = b"40000", bytes.fromhex(tree_digest(path, child_links)), name + b"/"
        elif path.is_file():
            mode = b"100755" if path.stat().st_mode & 0o111 else b"100644"
            h = hashlib.sha1(b"blob " + str(path.stat().st_size).encode() + b"\0")
            with path.open("rb") as stream:
                for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                    h.update(chunk)
            digest, key = h.digest(), name
        else:
            raise BootstrapError(f"unsupported filesystem entry: {path}")
        entries.append((key, mode + b" " + name + b"\0" + digest))
    return git_hash(b"tree", b"".join(value for _, value in sorted(entries))).hex()


def relative_parts(name):
    if not name or name.startswith("/") or "\\" in name or re.match(r"^[A-Za-z]:", name):
        raise BootstrapError(f"unsafe archive path: {name!r}")
    if any(ord(c) < 32 or ord(c) == 127 for c in name):
        raise BootstrapError(f"control character in archive path: {name!r}")
    parts = tuple(p for p in name.split("/") if p not in ("", "."))
    if ".." in parts or ".git" in parts or len(name) > 4096:
        raise BootstrapError(f"unsafe archive path: {name!r}")
    return parts


def link_parts(target):
    if not target or target.startswith("/") or "\\" in target or re.match(r"^[A-Za-z]:", target):
        raise BootstrapError(f"unsafe symlink target: {target!r}")
    if any(ord(c) < 32 or ord(c) == 127 for c in target) or len(target) > 4096:
        raise BootstrapError("invalid symlink target")
    return target.split("/")


def validate_links(entries):
    for path, member in entries.items():
        for end in range(1, len(path)):
            ancestor = entries.get(path[:end])
            if ancestor is not None and not ancestor.isdir():
                raise BootstrapError(f"archive writes through a file/symlink: {'/'.join(path)}")
        if not member.issym():
            continue
        pending = list(path[:-1]) + link_parts(member.linkname)
        resolved = []
        hops = 0
        while pending:
            part = pending.pop(0)
            if part in ("", "."):
                continue
            if part == "..":
                if not resolved:
                    raise BootstrapError("symlink escapes archive")
                resolved.pop()
                continue
            resolved.append(part)
            other = entries.get(tuple(resolved))
            if other is not None and other.issym():
                hops += 1
                if hops > 40:
                    raise BootstrapError("cyclic/excessive archive symlink chain")
                resolved.pop()
                # Resolve links before '..', as the filesystem does, not lexically.
                pending = link_parts(other.linkname) + pending


def bounded_copy(source, destination, limit):
    total = 0
    while True:
        chunk = source.read(min(1024 * 1024, limit - total + 1))
        if not chunk:
            return total
        total += len(chunk)
        if total > limit:
            raise BootstrapError(f"expanded archive exceeds {limit} bytes")
        destination.write(chunk)


def safe_extract(archive, destination, prefix="", max_tar=MAX_TAR, max_members=MAX_MEMBERS,
                 inert_links=None, max_extracted=MAX_EXTRACTED):
    """Validate the whole archive, extract privately, and never replace a directory."""
    if destination.exists() or destination.is_symlink():
        raise BootstrapError(f"refusing to replace destination: {destination}")
    with tempfile.TemporaryDirectory(prefix="extract-", dir=destination.parent) as temporary:
        temporary = Path(temporary)
        raw = temporary / "payload.tar"
        with gzip.open(archive, "rb") as src, raw.open("xb") as out:
            bounded_copy(src, out, max_tar)
        tree = temporary / "tree"
        tree.mkdir()
        with tarfile.open(raw, "r:") as container:
            entries = {}
            logical_bytes = 0
            for count, member in enumerate(container, 1):
                if count > max_members:
                    raise BootstrapError("too many archive members")
                parts = relative_parts(member.name)
                if prefix:
                    if not parts or parts[0] != prefix:
                        raise BootstrapError("unexpected GitHub archive prefix")
                    parts = parts[1:]
                if not parts and member.isdir():
                    continue
                if not parts or parts in entries:
                    raise BootstrapError(f"duplicate/empty archive member: {member.name!r}")
                if not (member.isfile() or member.isdir() or member.issym()):
                    raise BootstrapError(f"unsupported archive entry (including hardlinks): {member.name}")
                if member.mode & 0o7000 or member.size < 0:
                    raise BootstrapError(f"unsafe archive mode/size: {member.name}")
                # Sparse files can reconstruct enormous holes from a tiny tar.
                # Neither compressed size nor the raw tar byte cap bounds them.
                if member.issparse() or any(key.startswith("GNU.sparse.") for key in member.pax_headers):
                    raise BootstrapError(f"sparse archive members are unsupported: {member.name}")
                if member.isfile():
                    logical_bytes += member.size
                elif member.issym() and (inert_links or {}).get("/".join(parts)) == member.linkname:
                    logical_bytes += len(member.linkname.encode())
                if logical_bytes > max_extracted:
                    raise BootstrapError(f"logical extracted bytes exceed {max_extracted} bytes")
                entries[parts] = member
            inert = {parts: member.linkname for parts, member in entries.items()
                     if member.issym() and (inert_links or {}).get("/".join(parts)) == member.linkname}
            validate_links({parts: tarfile.TarInfo(member.name) if parts in inert else member
                            for parts, member in entries.items()})
            remaining = max_extracted
            for parts, member in entries.items():
                path = tree.joinpath(*parts)
                path.parent.mkdir(parents=True, exist_ok=True)
                if member.isdir():
                    path.mkdir(exist_ok=True)
                elif parts in inert:
                    data = member.linkname.encode()
                    if len(data) > remaining:
                        raise BootstrapError("logical extraction budget exhausted")
                    with path.open("xb") as out:
                        out.write(data)
                    remaining -= len(data)
                elif member.isfile():
                    src = container.extractfile(member)
                    if src is None:
                        raise BootstrapError(f"missing archive payload: {member.name}")
                    with src, path.open("xb") as out:
                        size = bounded_copy(src, out, min(member.size, remaining))
                    remaining -= size
                    if size != member.size:
                        raise BootstrapError(f"truncated archive member: {member.name}")
                    path.chmod(0o755 if member.mode & 0o111 else 0o644)
            # No archive member can write through links, regardless of entry order.
            for parts, member in entries.items():
                if member.issym() and parts not in inert:
                    tree.joinpath(*parts).symlink_to(member.linkname)
        if destination.exists() or destination.is_symlink():
            raise BootstrapError(f"destination appeared during extraction: {destination}")
        tree.rename(destination)
        return {"/".join(parts): target for parts, target in inert.items()}


def prepare_root(root):
    if root.is_symlink():
        raise BootstrapError(f"refusing symlink dependency root: {root}")
    root.mkdir(parents=False, exist_ok=True)
    marker = root / ".owner.json"
    if marker.is_symlink():
        raise BootstrapError("symlink ownership marker")
    if marker.exists():
        if json.loads(marker.read_text()) != OWNER:
            raise BootstrapError("unrecognized dependency-root owner")
    else:
        if any(root.iterdir()):
            raise BootstrapError(f"refusing nonempty unowned directory: {root}")
        with marker.open("x") as out:
            json.dump(OWNER, out)
    for name in ("downloads", "sources", "toolchain", "work"):
        owned_path(root, name).mkdir(exist_ok=True)


def owned_path(root, relative):
    parts = relative_parts(relative)
    path = root
    for part in parts:
        path = path / part
        if path.is_symlink():
            raise BootstrapError(f"refusing symlink output/cache: {path}")
    return path


def save_manifest(root, manifest):
    path = owned_path(root, "manifest.json")
    with tempfile.NamedTemporaryFile(mode="w", dir=root, prefix="manifest-", delete=False) as out:
        json.dump(manifest, out, indent=2, sort_keys=True)
        out.write("\n")
        temporary = Path(out.name)
    temporary.replace(path)


def pins_digest():
    return hashlib.sha256(json.dumps([asdict(a) for a in ARTIFACTS], sort_keys=True).encode()).hexdigest()


class HTTPSRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        if not newurl.startswith("https://"):
            raise BootstrapError("refusing non-HTTPS redirect")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


class Downloader:
    def __init__(self, offline=False, already_received=0):
        self.offline = offline
        self.already_received = already_received
        self.received = 0
        self.opener = urllib.request.build_opener(HTTPSRedirects())

    def fetch(self, url, destination, limit):
        if self.offline:
            raise BootstrapError(f"offline cache miss: {destination.name}")
        if not url.startswith("https://"):
            raise BootstrapError("downloads require HTTPS")
        remaining = min(limit, MAX_DOWNLOAD - self.already_received - self.received)
        if remaining <= 0:
            raise BootstrapError("cumulative download budget exhausted")
        request = urllib.request.Request(url, headers={"User-Agent": "PenguDroid-nativebridge-deps/1"})
        with self.opener.open(request, timeout=45) as response, destination.open("xb") as out:
            length = response.headers.get("Content-Length")
            if length and int(length) > remaining:
                raise BootstrapError(f"download exceeds byte budget: {url}")
            size = 0
            while size < remaining:
                chunk = response.read(min(1024 * 1024, remaining - size))
                if not chunk:
                    if length and size != int(length):
                        raise BootstrapError(f"truncated download: {url}")
                    return response.geturl()
                out.write(chunk)
                size += len(chunk)
                self.received += len(chunk)
            raise BootstrapError(f"download reached byte limit ({remaining} bytes): {url}")


def fetch_artifact(root, artifact, downloader, previous=None, max_tar=MAX_TAR):
    suffix = ".b64" if artifact.blob else ".tar.gz"
    cache = owned_path(root, f"downloads/{artifact.name}{suffix}")
    resolved_url = previous.get("resolved_url", artifact.url) if previous else artifact.url
    if not cache.exists():
        with tempfile.TemporaryDirectory(prefix="download-", dir=root / "work") as temporary:
            partial = Path(temporary) / "payload"
            resolved_url = downloader.fetch(artifact.url, partial, artifact.limit)
            partial.rename(cache)
    if not cache.is_file() or cache.stat().st_size > artifact.limit:
        raise BootstrapError(f"invalid cached download: {cache}")
    digest = sha256_file(cache)
    if artifact.sha256 and digest != artifact.sha256:
        raise BootstrapError(f"pinned SHA256 mismatch: {artifact.name}")
    if previous and digest != previous["download_sha256"]:
        raise BootstrapError(f"cached SHA256 mismatch: {artifact.name}")
    destination = owned_path(root, artifact.destination)
    destination.parent.mkdir(parents=True, exist_ok=True)
    record = {**asdict(artifact), "url": artifact.url, "resolved_url": resolved_url,
              "download_path": str(cache.relative_to(root)), "download_bytes": cache.stat().st_size,
              "download_sha256": digest}
    if artifact.blob:
        encoded = cache.read_bytes().replace(b"\n", b"").replace(b"\r", b"")
        data = base64.b64decode(encoded, validate=True)
        if git_hash(b"blob", data).hex() != artifact.blob:
            raise BootstrapError(f"pinned Git blob mismatch: {artifact.name}")
        if destination.exists():
            if not destination.is_file() or sha256_file(destination) != hashlib.sha256(data).hexdigest():
                raise BootstrapError(f"modified dependency; not replacing: {destination}")
        else:
            with destination.open("xb") as out:
                out.write(data)
        record.update(extracted_bytes=len(data), files=1, content_sha256=hashlib.sha256(data).hexdigest())
    else:
        if destination.exists():
            if not previous or not destination.is_dir():
                raise BootstrapError(f"unrecorded destination; not replacing: {destination}")
            inert = previous.get("inert_links", {})
            observed = tree_digest(destination, inert)
        else:
            with tempfile.TemporaryDirectory(prefix="verify-", dir=root / "work") as temporary:
                stage = Path(temporary) / "source"
                inert = safe_extract(cache, stage, artifact.prefix, max_tar=max_tar,
                                     inert_links=INERT_LINKS if artifact.tree else None, max_extracted=max_tar)
                observed = tree_digest(stage, inert)
                if artifact.tree and observed != artifact.tree:
                    raise BootstrapError(f"pinned Git tree mismatch: {artifact.name}: {observed} != {artifact.tree}")
                stage.rename(destination)
        if (artifact.tree and observed != artifact.tree) or (previous and observed != previous["git_tree"]):
            raise BootstrapError(f"modified dependency Git tree: {artifact.name}")
        files = [p for p in destination.rglob("*") if p.is_file() and not p.is_symlink()]
        record.update(git_tree=observed, materialized_git_tree=tree_digest(destination), inert_links=inert,
                      extracted_bytes=sum(p.stat().st_size for p in files), files=len(files),
                      license_files=[str(p.relative_to(root)) for p in files
                                     if p.name.upper().startswith(("LICENSE", "NOTICE", "COPYING"))])
    return record


SMOKE_SOURCE = """#include <android/api-level.h>
#include <vector>
#include <string_view>
#ifndef __BIONIC__
#error Android Bionic headers required
#endif
static_assert(__ANDROID_API__ == 37);
static_assert(sizeof(void*) == 8);
static_assert(_LIBCPP_ABI_VERSION == 1);
extern "C" int nativebridge_dependency_smoke() {
    std::__1::vector<int> values{20, 22};
    return values[0] + values[1] + std::string_view{}.size();
}
"""


def preflight(root, sysroot, clang, lld):
    result = {"passed": False, "scope": "C++ compile and CRT shared-link only; no code executed",
              "sysroot": str(sysroot), "commands": [], "inputs": {}}
    try:
        libdir = sysroot / "usr/lib/x86_64-linux-android/37"
        platform = root / "toolchain/libcxx/android/x86_64"
        required = [libdir / name for name in ("crtbegin_so.o", "crtend_so.o", "libc.so", "libm.so", "libdl.so")]
        required += [platform / "include/c++/v1/__config_site", platform / "lib/libc++.so",
                     root / "toolchain/libcxx/include/c++/v1/vector",
                     root / "toolchain/libclang_rt.builtins-x86_64-android.a"]
        for path in required:
            if not path.is_file():
                raise BootstrapError(f"missing API-37 preflight input: {path}")
            result["inputs"][str(path)] = sha256_file(path)
        for tool in (clang, lld):
            if not tool.is_file() or not os.access(tool, os.X_OK):
                raise BootstrapError(f"missing executable: {tool}")
        result["compiler"] = subprocess.check_output([str(clang), "--version"], text=True, timeout=15).strip()
        result["sysroot_provenance"] = "Existing user-supplied sysroot, not downloaded/pinned by this tool; selected API-37 inputs hashed above."
        work = Path(tempfile.mkdtemp(prefix="preflight-", dir=root / "work"))
        cpp, obj, output = work / "smoke.cpp", work / "smoke.o", work / "libnativebridge_deps_smoke.so"
        cpp.write_text(SMOKE_SOURCE)
        commands = [
            [str(clang), "--target=x86_64-linux-android37", f"--sysroot={sysroot}", "-nostdinc++",
             "-isystem", str(platform / "include/c++/v1"),
             "-isystem", str(root / "toolchain/libcxx/include/c++/v1"),
             "-isystem", str(sysroot / "usr/include/x86_64-linux-android"),
             "-std=gnu++23", "-fPIC", "-O0", "-Wall", "-Werror", "-c", str(cpp), "-o", str(obj)],
            [str(lld), "-m", "elf_x86_64", "-shared", "--no-undefined", "-z", "max-page-size=16384",
             "--soname=libnativebridge_deps_smoke.so", str(libdir / "crtbegin_so.o"), str(obj),
             str(platform / "lib/libc++.so"), f"-L{libdir}", "-lc", "-lm", "-ldl",
             str(root / "toolchain/libclang_rt.builtins-x86_64-android.a"),
             str(libdir / "crtend_so.o"), "-o", str(output)],
        ]
        for command in commands:
            process = subprocess.run(command, capture_output=True, text=True, timeout=90, check=False)
            result["commands"].append({"argv": command, "returncode": process.returncode,
                                       "stdout": process.stdout, "stderr": process.stderr})
            if process.returncode:
                raise BootstrapError(f"preflight command failed:\n{process.stderr}")
        header = output.read_bytes()[:20]
        if header[:6] != b"\x7fELF\x02\x01" or int.from_bytes(header[18:20], "little") != 62:
            raise BootstrapError("smoke output is not little-endian ELF64 x86_64")
        result.update(passed=True, output=str(output), output_bytes=output.stat().st_size,
                      output_sha256=sha256_file(output))
    except (BootstrapError, OSError, subprocess.SubprocessError) as error:
        result["error"] = str(error)
    return result


def check_manifest(manifest):
    if manifest.get("tool") != OWNER["tool"] or manifest.get("schema") != OWNER["schema"]:
        raise BootstrapError("manifest owner/schema changed")
    pins = {a.name: asdict(a) for a in ARTIFACTS}
    for name, record in manifest["artifacts"].items():
        if name not in pins or any(record.get(k) != v for k, v in pins[name].items()):
            raise BootstrapError(f"manifest pins changed for {name}; refusing to replace dependencies")
    # A new, not-yet-downloaded input can be added without disturbing verified sources.
    manifest["pins_sha256"] = pins_digest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--offline", action="store_true", help="verify/reuse cache without any network requests")
    parser.add_argument("--download-only", action="store_true", help="skip C++/CRT preflight")
    parser.add_argument("--sysroot", type=Path, default=SYSROOT, help="read-only API-37 NDK platform sysroot")
    parser.add_argument("--clang", type=Path, default=Path("/usr/lib/llvm-22/bin/clang++"))
    parser.add_argument("--lld", type=Path, default=Path("/usr/lib/llvm-22/bin/ld.lld"))
    args = parser.parse_args()
    prepare_root(ROOT)
    lock = owned_path(ROOT, ".lock")
    with lock.open("a") as handle:
        try:
            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise BootstrapError("another nativebridge-deps bootstrap is running") from error
        path = owned_path(ROOT, "manifest.json")
        manifest = json.loads(path.read_text()) if path.exists() else {
            **OWNER, "platform": "Android 17 / API 37", "aosp_superproject": SUPERPROJECT,
            "pins_sha256": pins_digest(), "artifacts": {}, "next_steps": NEXT_STEPS,
        }
        check_manifest(manifest)
        manifest["preflight"] = {"passed": False, "scope": "not run"}
        save_manifest(ROOT, manifest)
        prior_bytes = manifest.get("network_bytes_total", manifest.get("download_bytes", 0))
        downloader = Downloader(args.offline, prior_bytes)
        try:
            for artifact in ARTIFACTS:
                print(f"{'verify' if args.offline else 'fetch/verify'} {artifact.name}", flush=True)
                previous = manifest["artifacts"].get(artifact.name)
                other_bytes = sum(a["extracted_bytes"] for n, a in manifest["artifacts"].items() if n != artifact.name)
                record = fetch_artifact(ROOT, artifact, downloader, previous,
                                        max_tar=min(MAX_TAR, MAX_EXTRACTED - other_bytes))
                manifest["artifacts"][artifact.name] = record
                total = sum(a["download_bytes"] for a in manifest["artifacts"].values())
                expanded = sum(a["extracted_bytes"] for a in manifest["artifacts"].values())
                if total > MAX_DOWNLOAD or expanded > MAX_EXTRACTED:
                    raise BootstrapError("total dependency storage budget exceeded")
                manifest["download_bytes"] = total
                manifest["extracted_bytes"] = expanded
                save_manifest(ROOT, manifest)
                print(f"  {record['download_bytes']:,} downloaded bytes; {record['extracted_bytes']:,} extracted bytes", flush=True)
            if not args.download_only:
                manifest["preflight"] = preflight(ROOT, args.sysroot.resolve(), args.clang, args.lld)
        finally:
            # Failed/aborted transfers count too, and cannot reset the cap on a rerun.
            manifest["network_bytes_this_run"] = downloader.received
            manifest["network_bytes_total"] = prior_bytes + downloader.received
            save_manifest(ROOT, manifest)
        print(f"Manifest: {path}\nNetwork this run: {downloader.received:,} bytes; cached inputs: {manifest['download_bytes']:,} bytes")
        print("Preflight: " + ("PASS (compile/link only)" if manifest["preflight"]["passed"] else "NOT PASSED / NOT RUN"))
        for step in NEXT_STEPS:
            print("- " + step)
        if not args.download_only and not manifest["preflight"]["passed"]:
            print(manifest["preflight"].get("error", "preflight not run"))
            return 1
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (BootstrapError, OSError, ValueError, tarfile.TarError, urllib.error.URLError) as error:
        print(f"nativebridge-deps: {error}", file=sys.stderr)
        raise SystemExit(1)
