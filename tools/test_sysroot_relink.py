#!/usr/bin/env python3
"""Offline APEX mount-point regressions for extracted Android system trees."""

import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    "sysroot_relink", Path(__file__).parent / "art/sysroot-relink.py")
relink = importlib.util.module_from_spec(spec)
spec.loader.exec_module(relink)


class RelinkTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def fixture(self):
        libc = self.root / "system/apex/com.android.runtime/lib64/bionic/libc.so"
        libc.parent.mkdir(parents=True)
        libc.write_bytes(b"runtime fixture")
        link = self.root / "system/lib64/libc.so"
        link.parent.mkdir(parents=True)
        link.symlink_to("/apex/com.android.runtime/lib64/bionic/libc.so")
        return libc, link

    def run_relink(self):
        self.assertEqual(relink.main(["sysroot-relink.py", str(self.root)]), 0)

    def test_missing_apex_mount_point_exposes_packages(self):
        libc, link = self.fixture()
        self.run_relink()
        self.assertEqual((self.root / "apex").readlink(), Path("system/apex"))
        self.assertFalse(link.readlink().is_absolute())
        self.assertEqual(link.resolve(), libc)
        self.assertEqual(link.read_bytes(), b"runtime fixture")

    def test_empty_gsi_apex_mount_point_exposes_packages_and_is_idempotent(self):
        libc, link = self.fixture()
        (self.root / "apex").mkdir()
        self.run_relink()
        self.assertEqual((self.root / "apex").readlink(), Path("system/apex"))
        target = link.readlink()
        self.run_relink()
        self.assertEqual(link.readlink(), target)
        self.assertEqual(link.resolve(), libc)

    def test_populated_mount_point_is_not_removed(self):
        self.fixture()
        apex = self.root / "apex"
        apex.mkdir()
        marker = apex / "user-owned"
        marker.write_text("keep")
        self.run_relink()
        self.assertFalse(apex.is_symlink())
        self.assertEqual(marker.read_text(), "keep")

    def test_existing_package_alias_is_retained(self):
        libc, link = self.fixture()
        apex = self.root / "apex"
        apex.symlink_to("system/apex")
        self.run_relink()
        self.assertEqual(apex.readlink(), Path("system/apex"))
        self.assertEqual(link.resolve(), libc)

    def test_no_package_tree_does_not_remove_empty_directory(self):
        apex = self.root / "apex"
        apex.mkdir()
        self.run_relink()
        self.assertTrue(apex.is_dir())
        self.assertFalse(apex.is_symlink())


if __name__ == "__main__":
    unittest.main()
