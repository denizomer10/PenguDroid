#!/usr/bin/env python3
"""Offline tests; temporary files stay inside the bootstrap's owned work directory."""

import base64
import hashlib
import importlib.util
import io
import shutil
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock

spec = importlib.util.spec_from_file_location("nativebridge_deps", Path(__file__).with_name("nativebridge-deps.py"))
assert spec is not None and spec.loader is not None
deps = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = deps
spec.loader.exec_module(deps)


class PinTests(unittest.TestCase):
    def test_immutable_revision_and_content_pins(self):
        self.assertEqual(len(deps.ARTIFACTS), 17)
        self.assertEqual(len({a.name for a in deps.ARTIFACTS}), 17)
        self.assertEqual(len({a.destination for a in deps.ARTIFACTS}), 17)
        for artifact in deps.ARTIFACTS:
            with self.subTest(artifact=artifact.name):
                self.assertRegex(artifact.revision, r"^[0-9a-f]{40}$")
                self.assertTrue(artifact.url.startswith("https://"))
                self.assertIn(artifact.revision, artifact.url)
                self.assertNotIn("refs/heads", artifact.url)
                self.assertTrue(artifact.tree or artifact.blob or artifact.sha256)
                if artifact.tree or artifact.blob:
                    self.assertRegex(artifact.tree or artifact.blob, r"^[0-9a-f]{40}$")
                if artifact.sha256:
                    self.assertRegex(artifact.sha256, r"^[0-9a-f]{64}$")
                deps.relative_parts(artifact.destination)
        digitalis = deps.ARTIFACTS[0]
        self.assertEqual(digitalis.revision, "2d03335e3c6bee947ace72f0aa717ff4f8db138b")
        self.assertEqual(digitalis.sha256, "dd2e63f14581c6baa756c6000a939c204658a606764af756b1e708e7c8559f1f")
        self.assertEqual(deps.MAX_DOWNLOAD, 150_000_000)

    def test_pins_fingerprint_changes_with_revision(self):
        original = deps.pins_digest()
        altered = deps.Artifact("test", "bionic", "0" * 40, "sources/test", tree="1" * 40)
        with mock.patch.object(deps, "ARTIFACTS", (altered,)):
            self.assertNotEqual(original, deps.pins_digest())

    def test_manifest_cannot_change_an_already_recorded_pin(self):
        record = deps.asdict(deps.ARTIFACTS[0])
        manifest = {**deps.OWNER, "artifacts": {"digitalis": record}}
        deps.check_manifest(manifest)
        self.assertEqual(manifest["pins_sha256"], deps.pins_digest())
        record["revision"] = "0" * 40
        with self.assertRaisesRegex(deps.BootstrapError, "pins changed"):
            deps.check_manifest(manifest)

    def test_no_runtime_or_full_toolchain_downloads(self):
        for artifact in deps.ARTIFACTS:
            self.assertNotIn("sys-img", artifact.url)
            self.assertNotIn("sysroot", artifact.url)
            if artifact.project == deps.CLANG_PROJECT:
                self.assertTrue(artifact.subtree.startswith(deps.CLANG_PATH + "/"))
                self.assertNotIn("/bin/", artifact.subtree)
        self.assertIn("__ANDROID_API__ == 37", deps.SMOKE_SOURCE)
        self.assertIn("std::__1::vector", deps.SMOKE_SOURCE)
        self.assertTrue(any("Not built" in step for step in deps.NEXT_STEPS))


class WorkspaceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        deps.prepare_root(deps.ROOT)

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="test-", dir=deps.ROOT / "work")
        self.addCleanup(self.temporary.cleanup)
        self.work = Path(self.temporary.name)
        self.serial = 0

    def archive(self, members):
        self.serial += 1
        output = self.work / f"archive-{self.serial}.tar.gz"
        with tarfile.open(output, "w:gz") as container:
            for name, kind, value in members:
                member = tarfile.TarInfo(name)
                member.mode = 0o644
                if kind == "file":
                    member.size = len(value)
                    container.addfile(member, io.BytesIO(value))
                elif kind == "exec":
                    member.mode = 0o755
                    member.size = len(value)
                    container.addfile(member, io.BytesIO(value))
                else:
                    member.type = {"link": tarfile.SYMTYPE, "hardlink": tarfile.LNKTYPE,
                                   "dir": tarfile.DIRTYPE, "fifo": tarfile.FIFOTYPE}[kind]
                    member.linkname = value
                    container.addfile(member)
        return output

    def sparse_archive(self, logical_size, name="sparse"):
        """A PAX GNU-sparse member: a tiny tar that reconstructs a huge file."""
        self.serial += 1
        output = self.work / f"archive-{self.serial}.tar.gz"

        def record(key, value):
            length = 1
            while True:
                body = f"{length} {key}={value}\n"
                if len(body) == length:
                    return body.encode()
                length = len(body)

        payload = record("GNU.sparse.map", "0,0") + record("GNU.sparse.size", str(logical_size))
        with tarfile.open(output, "w:gz") as container:
            header = tarfile.TarInfo("pax")
            header.type = tarfile.XHDTYPE
            header.mode = 0o644
            header.size = len(payload)
            container.addfile(header, io.BytesIO(payload))
            member = tarfile.TarInfo(name)
            member.type = tarfile.REGTYPE
            member.mode = 0o644
            member.size = 0  # on-tape size; the payload is all reconstructed holes
            container.addfile(member)
        return output

    def assert_rejected(self, members, **kwargs):
        destination = self.work / f"destination-{self.serial}"
        with self.assertRaises(deps.BootstrapError):
            deps.safe_extract(self.archive(members), destination, **kwargs)
        self.assertFalse(destination.exists())

    def test_safe_files_modes_relative_links_and_licenses(self):
        archive = self.archive([("LICENSE", "file", b"keep this license"),
                                ("bin/tool", "exec", b"executable"),
                                ("include/license", "link", "../LICENSE")])
        destination = self.work / "safe"
        deps.safe_extract(archive, destination)
        self.assertEqual((destination / "LICENSE").read_bytes(), b"keep this license")
        self.assertTrue((destination / "bin/tool").stat().st_mode & 0o111)
        self.assertEqual((destination / "include/license").read_bytes(), b"keep this license")
        self.assertEqual((destination / "include/license").readlink(), Path("../LICENSE"))

    def test_absolute_parent_windows_and_control_paths(self):
        for name in ("/escape", "../escape", "a/../../escape", "a\\escape", "C:/escape", "a/\nfile", ".git/config"):
            with self.subTest(name=name):
                self.assert_rejected([(name, "file", b"bad")])
        self.assertFalse((self.work / "escape").exists())

    def test_duplicate_members(self):
        self.assert_rejected([("same", "file", b"one"), ("./same", "file", b"two")])

    def test_symlinks_cannot_escape_or_be_absolute(self):
        for target in ("../escape", "/tmp/escape", "C:/escape", "a\\b"):
            with self.subTest(target=target):
                self.assert_rejected([("link", "link", target)])

    def test_member_cannot_write_through_symlink_in_either_order(self):
        members = [("link", "link", "real"), ("link/child", "file", b"bad")]
        self.assert_rejected(members)
        self.assert_rejected(list(reversed(members)))

    def test_member_cannot_write_through_regular_file(self):
        self.assert_rejected([("file/child", "file", b"bad"), ("file", "file", b"bad")])

    def test_symlink_chain_cycle(self):
        self.assert_rejected([("a", "link", "b"), ("b", "link", "a")])

    def test_dotdot_after_symlink_is_resolved_physically(self):
        # Lexically reducing a/../escape would miss that a -> . escapes the root.
        self.assert_rejected([("a", "link", "."), ("b", "link", "a/../escape")])

    def test_safe_symlink_chain(self):
        archive = self.archive([("actual", "file", b"safe"), ("a", "link", "actual"), ("b", "link", "a")])
        destination = self.work / "chain"
        deps.safe_extract(archive, destination)
        self.assertEqual((destination / "b").read_bytes(), b"safe")

    def test_pinned_formatting_link_can_only_be_preserved_as_inert_text(self):
        target = deps.INERT_LINKS[".clang-format"]
        archive = self.archive([(".clang-format", "link", target)])
        destination = self.work / "inert"
        inert = deps.safe_extract(archive, destination, inert_links=deps.INERT_LINKS)
        self.assertEqual(inert, {".clang-format": target})
        self.assertFalse((destination / ".clang-format").is_symlink())
        self.assertEqual((destination / ".clang-format").read_text(), target)
        expected = self.work / "original"
        expected.mkdir()
        (expected / ".clang-format").symlink_to(target)
        self.assertEqual(deps.tree_digest(destination, inert), deps.tree_digest(expected))
        self.assertNotEqual(deps.tree_digest(destination), deps.tree_digest(expected))
        self.assert_rejected([(".clang-format", "link", "../../different")], inert_links=deps.INERT_LINKS)
        self.assert_rejected([(".clang-format", "link", target), (".clang-format/child", "file", b"bad")],
                             inert_links=deps.INERT_LINKS)

    def test_nested_inert_link_git_hash(self):
        target = deps.INERT_LINKS["rust/rustfmt.toml"]
        archive = self.archive([("rust/rustfmt.toml", "link", target)])
        destination = self.work / "nested"
        inert = deps.safe_extract(archive, destination, inert_links=deps.INERT_LINKS)
        expected = self.work / "original"
        (expected / "rust").mkdir(parents=True)
        (expected / "rust/rustfmt.toml").symlink_to(target)
        self.assertEqual(deps.tree_digest(destination, inert), deps.tree_digest(expected))
        self.assertFalse((destination / "rust/rustfmt.toml").is_symlink())

    def test_hardlinks_and_special_files_rejected(self):
        for kind in ("hardlink", "fifo"):
            with self.subTest(kind=kind):
                self.assert_rejected([("special", kind, "target")])

    def test_expanded_bytes_and_member_limits(self):
        self.assert_rejected([("large", "file", b"x" * 4096)], max_tar=100)
        self.assert_rejected([("a", "file", b"a"), ("b", "file", b"b")], max_members=1)

    def test_sparse_members_cannot_expand_beyond_the_extraction_budget(self):
        # Python's tarfile reports this as sparse and reconstructs `logical_size`
        # zero bytes from an on-tape size of 0, so neither the gzip size nor the
        # raw-tar cap bounds the bytes actually written to disk.
        archive = self.sparse_archive(16 * 1024 * 1024)
        self.assertLess(archive.stat().st_size, 4096)
        with tarfile.open(archive, "r:gz") as container:
            member = next(m for m in container if m.isfile())
            self.assertTrue(member.issparse())
            self.assertEqual(len(container.extractfile(member).read()), 16 * 1024 * 1024)
        destination = self.work / "sparse"
        with self.assertRaises(deps.BootstrapError):
            deps.safe_extract(archive, destination, max_tar=1 << 20, max_extracted=1 << 20)
        self.assertFalse(destination.exists())

    def test_logical_extraction_budget_is_enforced_before_writing(self):
        archive = self.archive([("a", "file", b"x" * 4096), ("b", "file", b"y" * 4096)])
        destination = self.work / "budget"
        with self.assertRaises(deps.BootstrapError):
            deps.safe_extract(archive, destination, max_tar=1 << 20, max_extracted=5000)
        self.assertFalse(destination.exists())

    def test_github_prefix_is_exact(self):
        self.assert_rejected([("wrong/file", "file", b"bad")], prefix="expected")
        archive = self.archive([("expected", "dir", ""), ("expected/file", "file", b"good")])
        destination = self.work / "prefix"
        deps.safe_extract(archive, destination, prefix="expected")
        self.assertEqual((destination / "file").read_bytes(), b"good")

    def test_existing_destination_is_never_replaced(self):
        destination = self.work / "user"
        destination.mkdir()
        (destination / "mine").write_text("leave me")
        with self.assertRaisesRegex(deps.BootstrapError, "replace"):
            deps.safe_extract(self.archive([("new", "file", b"new")]), destination)
        self.assertEqual((destination / "mine").read_text(), "leave me")

    def test_existing_destination_symlink_is_rejected(self):
        destination = self.work / "alias"
        destination.symlink_to(self.work, target_is_directory=True)
        with self.assertRaises(deps.BootstrapError):
            deps.safe_extract(self.archive([("file", "file", b"bad")]), destination)

    def test_unowned_root_is_not_adopted_or_modified(self):
        root = self.work / "user"
        root.mkdir()
        (root / "mine").write_text("leave me")
        with self.assertRaisesRegex(deps.BootstrapError, "unowned"):
            deps.prepare_root(root)
        self.assertEqual([p.name for p in root.iterdir()], ["mine"])

    def test_root_and_output_symlinks_are_rejected(self):
        alias = self.work / "alias"
        alias.symlink_to(self.work, target_is_directory=True)
        with self.assertRaises(deps.BootstrapError):
            deps.prepare_root(alias)
        with self.assertRaises(deps.BootstrapError):
            deps.owned_path(self.work, "alias/file")

    def test_empty_git_tree_and_file_hash(self):
        empty = self.work / "empty"
        empty.mkdir()
        self.assertEqual(deps.tree_digest(empty), "4b825dc642cb6eb9a060e54bf8d69288fbee4904")
        (empty / "file").write_bytes(b"")
        entry = b"100644 file\0" + bytes.fromhex("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391")
        expected = hashlib.sha1(b"tree " + str(len(entry)).encode() + b"\0" + entry).hexdigest()
        self.assertEqual(deps.tree_digest(empty), expected)

    def cached_source(self, tree="", checksum=""):
        root = self.work / "owned"
        deps.prepare_root(root)
        artifact = deps.Artifact("toy", "toy", "0" * 40, "sources/toy", tree=tree, sha256=checksum)
        archive = self.archive([("LICENSE", "file", b"license"), ("file", "file", b"original")])
        shutil.copyfile(archive, root / "downloads/toy.tar.gz")
        return root, artifact

    def test_pinned_archive_checksum_mismatch(self):
        root, artifact = self.cached_source(checksum="0" * 64)
        with self.assertRaisesRegex(deps.BootstrapError, "SHA256 mismatch"):
            deps.fetch_artifact(root, artifact, deps.Downloader(offline=True))
        self.assertFalse((root / artifact.destination).exists())

    def test_pinned_tree_mismatch_is_not_published(self):
        root, artifact = self.cached_source(tree="0" * 40)
        with self.assertRaisesRegex(deps.BootstrapError, "Git tree mismatch"):
            deps.fetch_artifact(root, artifact, deps.Downloader(offline=True))
        self.assertFalse((root / artifact.destination).exists())

    def test_offline_reuse_and_tampering_detection(self):
        root, artifact = self.cached_source()
        downloader = deps.Downloader(offline=True)
        first = deps.fetch_artifact(root, artifact, downloader)
        second = deps.fetch_artifact(root, artifact, downloader, first)
        self.assertEqual(first, second)
        self.assertEqual(downloader.received, 0)
        self.assertTrue(first["license_files"])
        (root / artifact.destination / "file").write_text("user modification")
        with self.assertRaisesRegex(deps.BootstrapError, "modified dependency"):
            deps.fetch_artifact(root, artifact, downloader, first)
        self.assertEqual((root / artifact.destination / "file").read_text(), "user modification")

    def test_raw_git_blob_pin(self):
        root = self.work / "owned"
        deps.prepare_root(root)
        data = b"upstream license\n"
        artifact = deps.Artifact("notice", "toolchain", "0" * 40, "toolchain/NOTICE",
                                 blob=deps.git_hash(b"blob", data).hex(), subtree="NOTICE")
        (root / "downloads/notice.b64").write_bytes(base64.b64encode(data))
        record = deps.fetch_artifact(root, artifact, deps.Downloader(offline=True))
        self.assertEqual((root / artifact.destination).read_bytes(), data)
        self.assertEqual(record["content_sha256"], hashlib.sha256(data).hexdigest())

    def test_missing_api37_never_falls_back(self):
        sysroot = self.work / "sysroot"
        old = sysroot / "usr/lib/x86_64-linux-android/36"
        old.mkdir(parents=True)
        (old / "crtbegin_so.o").write_bytes(b"not API37")
        result = deps.preflight(self.work, sysroot, Path("/no/compiler"), Path("/no/linker"))
        self.assertFalse(result["passed"])
        self.assertIn("/37/", result["error"])
        self.assertEqual(result["commands"], [])

    def test_download_limit_and_offline_network_guard(self):
        class Response(io.BytesIO):
            def __init__(self, data):
                super().__init__(data)
                self.headers = {}

            def geturl(self):
                return "https://example.invalid/pinned"

        downloader = deps.Downloader()
        downloader.opener = mock.Mock()
        downloader.opener.open.return_value = Response(b"12345")
        with mock.patch.object(deps, "MAX_DOWNLOAD", 3), self.assertRaisesRegex(deps.BootstrapError, "byte limit"):
            downloader.fetch("https://example.invalid/pinned", self.work / "limited", 10)
        self.assertEqual(downloader.received, 3)
        offline = deps.Downloader(offline=True)
        offline.opener = mock.Mock()
        with self.assertRaisesRegex(deps.BootstrapError, "offline cache miss"):
            offline.fetch("https://example.invalid/pinned", self.work / "offline", 10)
        offline.opener.open.assert_not_called()
        exhausted = deps.Downloader(already_received=deps.MAX_DOWNLOAD)
        exhausted.opener = mock.Mock()
        with self.assertRaisesRegex(deps.BootstrapError, "cumulative download budget"):
            exhausted.fetch("https://example.invalid/pinned", self.work / "exhausted", 10)
        exhausted.opener.open.assert_not_called()

    def test_http_and_redirect_downgrade_are_rejected(self):
        with self.assertRaisesRegex(deps.BootstrapError, "HTTPS"):
            deps.Downloader().fetch("http://example.invalid/file", self.work / "no-http", 10)
        with self.assertRaisesRegex(deps.BootstrapError, "HTTPS"):
            deps.HTTPSRedirects().redirect_request(None, None, 302, "", {}, "http://example.invalid/file")


if __name__ == "__main__":
    unittest.main()
