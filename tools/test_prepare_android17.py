#!/usr/bin/env python3
"""Offline runtime preparation regressions; no Android image is downloaded."""
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import struct
import tempfile
import unittest
from unittest import mock
import zipfile
import zlib

spec = importlib.util.spec_from_file_location("prepare_android17", Path(__file__).with_name("prepare-android17.py"))
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)


def sparse(chunks, blocks, checksum=0, **kwargs):
    file_header = kwargs.get("file_header", 28)
    chunk_header = kwargs.get("chunk_header", 12)
    header = struct.pack("<I4H4I", prepare.SPARSE_MAGIC, kwargs.get("major", 1), 0,
                         file_header, chunk_header, 4, blocks, len(chunks), checksum)
    image = header + bytes(max(0, file_header - 28))
    for kind, count, data in chunks:
        image += struct.pack("<2H2I", kind, 0, count, chunk_header + len(data))
        image += bytes(max(0, chunk_header - 12)) + data
    return image


class SparseTests(unittest.TestCase):
    def expand(self, source):
        with tempfile.TemporaryFile() as output:
            size = prepare.expand_sparse(io.BytesIO(source), output)
            output.seek(0)
            data = output.read()
            self.assertEqual(size, len(data))
            return data

    def test_raw_fill_holes_and_both_crc_checks(self):
        expected = b"ABCD" + b"1234" * 2 + bytes(8) + b"last"
        crc = zlib.crc32(expected)
        image = sparse([(0xCAC1, 1, b"ABCD"), (0xCAC2, 2, b"1234"),
                        (0xCAC3, 2, b""), (0xCAC1, 1, b"last"),
                        (0xCAC4, 0, struct.pack("<I", crc))], 6, checksum=crc)
        self.assertEqual(self.expand(image), expected)

    def test_trailing_hole_is_materialized_in_file_length(self):
        self.assertEqual(self.expand(sparse([(0xCAC3, 3, b"")], 3)), bytes(12))

    def test_extended_headers(self):
        self.assertEqual(self.expand(sparse([(0xCAC1, 1, b"test")], 1, file_header=32, chunk_header=16)), b"test")

    def test_rejects_corrupt_chunk_lengths_counts_and_types(self):
        cases = [sparse([(0xCAC1, 2, b"tiny")], 2),
                 sparse([(0xCAC2, 1, b"")], 1),
                 sparse([(0xCAC3, 1, b"bad!")], 1),
                 sparse([(0xCAC4, 1, b"crc!")], 1),
                 sparse([(0xCAFE, 1, b"data")], 1),
                 sparse([(0xCAC1, 1, b"data")], 0),
                 sparse([], 1)]
        for image in cases:
            with self.subTest(image=image):
                with self.assertRaises(prepare.PreparationError):
                    self.expand(image)

    def test_rejects_unsupported_or_excessive_image(self):
        for image in (sparse([], 0, major=2), sparse([], 0, file_header=27),
                      sparse([], 0, chunk_header=11)):
            with self.assertRaises(prepare.PreparationError):
                self.expand(image)
        with mock.patch.object(prepare, "MAX_IMAGE", 16):
            with self.assertRaisesRegex(prepare.PreparationError, "oversized"):
                self.expand(sparse([], 5))

    def test_rejects_truncation_or_trailing_data(self):
        good = sparse([(0xCAC1, 1, b"data")], 1)
        for image in (good[:12], good[:-1], good + b"extra"):
            with self.assertRaises(prepare.PreparationError):
                self.expand(image)

    def test_rejects_wrong_crc(self):
        for image in (sparse([(0xCAC1, 1, b"data")], 1, checksum=1),
                      sparse([(0xCAC1, 1, b"data"), (0xCAC4, 0, struct.pack("<I", 1))], 1)):
            with self.assertRaisesRegex(prepare.PreparationError, "checksum|CRC32"):
                self.expand(image)

    def test_large_fill_is_streamed_across_buffer_boundaries(self):
        with mock.patch.object(prepare, "BUFFER", 8):
            self.assertEqual(self.expand(sparse([(0xCAC2, 7, b"abcd")], 7)), b"abcd" * 7)


class RuntimeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.work = Path(self.temporary.name)

    def fixture(self):
        root = self.work / "runtime"
        for relative in prepare.REQUIRED:
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"fixture")
        (root / "system/build.prop").write_text(
            "ro.build.version.sdk=37\nro.build.version.release=17\nro.build.id=" + prepare.BUILD + "\n")
        header = bytearray(20)
        header[:6] = b"\x7fELF\x02\x01"
        struct.pack_into("<H", header, 18, 62)
        (root / "system/apex/com.android.art/lib64/libart.so").write_bytes(header)
        return root

    def test_identity_and_owned_runtime_reuse_without_archive(self):
        root = self.fixture()
        hashes = prepare.validate_runtime(root)
        (root / prepare.MANIFEST).write_text(json.dumps({"tool": prepare.OWNER, "archive_sha256": prepare.SHA256, "files": hashes}))
        prepare.prepare(self.work / "missing.zip", root)

    def test_missing_bootclasspath_or_art_is_not_faked(self):
        root = self.fixture()
        (root / "system/etc/classpaths/bootclasspath.pb").unlink()
        with self.assertRaisesRegex(prepare.PreparationError, "bootclasspath.pb"):
            prepare.validate_runtime(root)

    def test_rejects_older_sdk_wrong_release_and_build(self):
        root = self.fixture()
        prop = root / "system/build.prop"
        original = prop.read_text()
        for before, after in (("sdk=37", "sdk=33"), ("release=17", "release=16"), (prepare.BUILD, "different")):
            prop.write_text(original.replace(before, after))
            with self.assertRaisesRegex(prepare.PreparationError, "no runtime fallback"):
                prepare.validate_runtime(root)

    def test_rejects_duplicate_platform_property(self):
        root = self.fixture()
        prop = root / "system/build.prop"
        prop.write_text(prop.read_text() + "ro.build.version.sdk=37\n")
        with self.assertRaisesRegex(prepare.PreparationError, "duplicate"):
            prepare.validate_runtime(root)

    def test_rejects_arm64_art_even_with_x86_64_directories(self):
        root = self.fixture()
        art = root / "system/apex/com.android.art/lib64/libart.so"
        header = bytearray(art.read_bytes())
        struct.pack_into("<H", header, 18, 183)
        art.write_bytes(header)
        with self.assertRaisesRegex(prepare.PreparationError, "x86_64"):
            prepare.validate_runtime(root)

    def test_rejects_links_outside_runtime(self):
        root = self.fixture()
        outside = self.work / "outside"
        outside.write_bytes(b"outside")
        path = root / "system/lib64/libc.so"
        path.unlink()
        path.symlink_to(outside)
        with self.assertRaisesRegex(prepare.PreparationError, "escapes"):
            prepare.validate_runtime(root)

    def test_never_overwrites_unowned_changed_or_linked_runtime(self):
        root = self.fixture()
        with self.assertRaisesRegex(prepare.PreparationError, "unowned"):
            prepare.prepare(self.work / "missing.zip", root)
        hashes = prepare.validate_runtime(root)
        (root / prepare.MANIFEST).write_text(json.dumps({"tool": prepare.OWNER, "archive_sha256": prepare.SHA256, "files": hashes}))
        (root / "system/framework/framework.jar").write_bytes(b"changed")
        with self.assertRaisesRegex(prepare.PreparationError, "changed"):
            prepare.prepare(self.work / "missing.zip", root)
        alias = self.work / "alias"
        alias.symlink_to(root)
        with self.assertRaisesRegex(prepare.PreparationError, "symlink"):
            prepare.prepare(self.work / "missing.zip", alias)

    def test_checksum_is_verified_before_any_archive_extraction(self):
        archive = self.work / "untrusted.zip"
        archive.write_bytes(b"not even a ZIP")
        with self.assertRaisesRegex(prepare.PreparationError, "SHA-256 mismatch"):
            prepare.image_from_archive(archive, self.work)
        self.assertFalse((self.work / "system.raw").exists())

    def test_reads_only_exact_image_member_not_arbitrary_zip_paths(self):
        image = bytearray(4096)
        image[1080:1082] = b"\x53\xef"
        archive = self.work / "fixture.zip"
        with zipfile.ZipFile(archive, "w") as container:
            container.writestr("system.img", image)
            container.writestr("../escape", b"bad")
        with mock.patch.object(prepare, "SHA256", hashlib.sha256(archive.read_bytes()).hexdigest()):
            result = prepare.image_from_archive(archive, self.work)
        self.assertEqual(result.read_bytes(), image)
        self.assertEqual(prepare.filesystem(result), "ext4")
        self.assertFalse((self.work.parent / "escape").exists())

    def test_detects_erofs_and_rejects_unknown_filesystem(self):
        image = self.work / "image"
        data = bytearray(4096)
        data[1024:1028] = bytes.fromhex("e2e1f5e0")
        image.write_bytes(data)
        self.assertEqual(prepare.filesystem(image), "erofs")
        image.write_bytes(bytes(4096))
        with self.assertRaises(prepare.PreparationError):
            prepare.filesystem(image)


if __name__ == "__main__":
    unittest.main()
