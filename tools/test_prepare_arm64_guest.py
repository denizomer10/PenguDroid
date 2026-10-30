#!/usr/bin/env python3
"""Synthetic offline ARM64 preparation tests; no real compiler, GSI or ART."""
import copy
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import struct
import subprocess
import tempfile
import unittest
from unittest import mock
import zipfile

spec = importlib.util.spec_from_file_location("prepare_arm64_guest", Path(__file__).with_name("prepare-arm64-guest.py"))
guest = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guest)
host = guest.load_prepare_module()


def elf(machine=183):
    header = bytearray(64)
    header[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<HHI", header, 16, 3, machine, 1)
    struct.pack_into("<H", header, 52, 64)
    return bytes(header) + b"synthetic ELF fixture\n"


def snapshot(root):
    result = {}
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix()
        if relative == guest.LOCK:
            continue
        info = path.lstat()
        if stat.S_ISLNK(info.st_mode):
            data = os.readlink(path)
        elif stat.S_ISDIR(info.st_mode):
            data = None
        else:
            data = path.read_bytes()
        result[relative] = (stat.S_IMODE(info.st_mode), data)
    return result


class GuestPreparationTests(unittest.TestCase):
    def setUp(self):
        previous_umask = os.umask(0o022)
        self.addCleanup(os.umask, previous_umask)
        self.temporary = tempfile.TemporaryDirectory(prefix="test-arm64-guest-")
        self.addCleanup(self.temporary.cleanup)
        self.work = Path(self.temporary.name)
        self.sysroot = self.work / "host"
        for relative in host.REQUIRED:
            path = self.sysroot / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"host fixture")
        (self.sysroot / "system/build.prop").write_text(self.properties())
        (self.sysroot / "system/apex/com.android.art/lib64/libart.so").write_bytes(elf(62))
        (self.sysroot / "system/bin").mkdir()
        (self.sysroot / "system/bin/app_process64").write_bytes(elf(62))
        (self.sysroot / "system/bin/linker64").write_bytes(elf(62))
        self.host_record = {"tool": host.OWNER, "schema": 1, "archive_sha256": host.SHA256,
                            "build": host.BUILD, "sdk": 37, "release": "17", "architecture": "x86_64",
                            "files": host.validate_runtime(self.sysroot)}
        self.write_host_record()
        self.root = self.work / "guest"
        for relative in guest.GUEST_ELF_REQUIRED:
            path = self.root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(elf())
            path.chmod(0o755 if relative.startswith("system/bin/") else 0o644)
        (self.root / "system/build.prop").write_text(self.properties())
        (self.root / "system/lib64/libextra.so").write_bytes(elf())
        (self.root / "system/lib64/libc.so").symlink_to("../apex/com.android.runtime/lib64/bionic/libc.so")
        (self.root / "system/lib64/libart.so").symlink_to("../apex/com.android.art/lib64/libart.so")
        self.source = self.work / "vdso_arm64.S"
        self.source.write_text(".text\n// local, deliberately unpinned fixture\n")
        self.compiler = self.work / "clang"
        self.compiler.write_bytes(b"mock compiler, never executed")
        self.compiler.chmod(0o755)
        self.retained = self.work / "retained"
        self.kwargs = {"sysroot": self.sysroot, "vdso_source": self.source,
                       "compiler": self.compiler, "retained": self.retained}
        self.run = self.enterContext(mock.patch.object(guest.subprocess, "run", side_effect=self.fake_compiler))
        self.baseline = snapshot(self.sysroot)

    @staticmethod
    def properties():
        return f"ro.build.version.sdk=37\nro.build.version.release=17\nro.build.id={guest.BUILD}\n"

    def write_host_record(self):
        (self.sysroot / host.MANIFEST).write_text(json.dumps(self.host_record))

    def fake_compiler(self, arguments, **kwargs):
        self.assertEqual(arguments[0], str(self.compiler))
        self.assertTrue(kwargs["check"])
        self.assertGreater(kwargs["timeout"], 0)
        if arguments[1:] == ["--version"]:
            return subprocess.CompletedProcess(arguments, 0, stdout="mock clang 22\n", stderr="")
        self.assertEqual(arguments[1:-3], list(guest.VDSO_FLAGS))
        self.assertEqual(arguments[-2], "-o")
        self.assertEqual(Path(arguments[-3]).read_bytes(), self.source.read_bytes())
        self.assertNotEqual(Path(arguments[-3]), self.source)
        output = Path(arguments[-1])
        output.write_bytes(elf())
        output.chmod(0o755)
        return subprocess.CompletedProcess(arguments, 0)

    def stage(self, **kwargs):
        return guest.stage(self.root, **(self.kwargs | kwargs))

    def assert_untouched(self, expected=None):
        self.assertEqual(snapshot(self.sysroot), self.baseline if expected is None else expected)
        self.assertEqual(list(self.work.glob(".prepare-arm64-*")), [])

    def record(self):
        return json.loads((self.sysroot / guest.MANIFEST).read_text())

    def assert_reuse_refused(self, record, message):
        (self.sysroot / guest.MANIFEST).write_text(json.dumps(record))
        before = snapshot(self.sysroot)
        source = mock.Mock(side_effect=AssertionError("must not extract or rebuild"))
        self.run.reset_mock()
        with self.assertRaisesRegex(guest.PreparationError, message):
            guest.install(source, **self.kwargs)
        source.assert_not_called()
        self.run.assert_not_called()
        self.assert_untouched(before)

    def test_complete_recursive_inventory_provenance_and_host_preservation(self):
        count, manifest = self.stage()
        record = self.record()
        self.assertEqual(manifest, self.sysroot / guest.MANIFEST)
        self.assertEqual(record["schema"], 2)
        self.assertEqual(count, len(record["files"]))
        self.assertTrue(set(guest.REQUIRED_OUTPUT).issubset(record["files"]))
        self.assertIn("system/lib64/arm64/bootstrap/libart.so", record["files"])
        self.assertIn("system/lib64/arm64/bootstrap", record["directories"])
        self.assertEqual(guest.inventory(self.sysroot), {key: record[key] for key in ("files", "directories")})
        build = record["vdso_build"]
        self.assertEqual(build["source_sha256"], hashlib.sha256(self.source.read_bytes()).hexdigest())
        self.assertEqual(build["compiler"]["sha256"], hashlib.sha256(self.compiler.read_bytes()).hexdigest())
        self.assertEqual(build["compiler"]["version"], "mock clang 22")
        self.assertIn("NOT pinned", build["note"])
        self.assertIn("--target=aarch64-linux-android37", build["arguments"])
        self.assertEqual(record["host_manifest_sha256"], guest.digest(self.sysroot / host.MANIFEST))
        after = snapshot(self.sysroot)
        for relative, value in self.baseline.items():
            self.assertEqual(after[relative], value, relative)
        for relative in record["files"]:
            info = (self.sysroot / relative).lstat()
            self.assertTrue(stat.S_ISREG(info.st_mode), relative)
            self.assertEqual(info.st_nlink, 1, relative)
        self.assertEqual((self.sysroot / "system/lib64/arm64/libc.so").read_bytes(), elf())

    def test_requires_complete_matching_host_manifest(self):
        path = self.sysroot / host.MANIFEST
        original = path.read_bytes()
        for key, value in (("tool", "unowned"), ("schema", 0), ("archive_sha256", "wrong"),
                           ("build", "wrong"), ("sdk", 36), ("release", "16"),
                           ("architecture", "aarch64"), ("files", {})):
            with self.subTest(key=key):
                record = self.host_record | {key: value}
                path.write_text(json.dumps(record))
                before = snapshot(self.sysroot)
                with self.assertRaises(guest.PreparationError):
                    self.stage()
                self.assert_untouched(before)
        path.unlink()
        before = snapshot(self.sysroot)
        with self.assertRaises(OSError):
            self.stage()
        self.assert_untouched(before)
        path.write_bytes(original)
        self.run.assert_not_called()

    def test_host_payload_change_or_wrong_elf_is_refused(self):
        path = self.sysroot / "system/apex/com.android.art/lib64/libart.so"
        path.write_bytes(elf(183))
        self.host_record["files"][path.relative_to(self.sysroot).as_posix()] = guest.digest(path)
        self.write_host_record()
        before = snapshot(self.sysroot)
        with self.assertRaisesRegex(guest.PreparationError, "x86_64"):
            self.stage()
        self.assert_untouched(before)
        path.write_bytes(elf(62))
        self.host_record["files"][path.relative_to(self.sysroot).as_posix()] = guest.digest(path)
        self.write_host_record()
        (self.sysroot / "system/framework/framework.jar").write_bytes(b"changed")
        before = snapshot(self.sysroot)
        with self.assertRaisesRegex(guest.PreparationError, "host runtime files changed"):
            self.stage()
        self.assert_untouched(before)
        self.run.assert_not_called()

    def test_host_required_escaping_link_and_hardlink_are_refused(self):
        outside = self.work / "outside-host"
        outside.write_bytes(b"outside")
        path = self.sysroot / "system/lib64/libc.so"
        path.unlink()
        for link in (lambda: path.symlink_to(outside), lambda: path.hardlink_to(outside)):
            with self.subTest(link=link):
                link()
                before = snapshot(self.sysroot)
                with self.assertRaises((guest.PreparationError, OSError)):
                    self.stage()
                self.assert_untouched(before)
                self.assertEqual(outside.read_bytes(), b"outside")
                path.unlink()
        self.run.assert_not_called()

    def test_destination_ancestry_cannot_be_symlinked(self):
        for path in (self.sysroot, self.sysroot / "system", self.sysroot / "system/bin",
                     self.sysroot / "system/lib64"):
            with self.subTest(path=path):
                saved = self.work / "saved"
                path.rename(saved)
                path.symlink_to(saved, target_is_directory=True)
                try:
                    with self.assertRaises(OSError):
                        self.stage()
                    self.assertTrue(path.is_symlink())
                finally:
                    path.unlink()
                    saved.rename(path)
                self.assert_untouched()
        self.run.assert_not_called()

    def test_guest_properties_required_and_not_duplicated(self):
        path = self.root / "system/build.prop"
        original = path.read_text()
        cases = [original.replace("sdk=37", "sdk=36"), original.replace("release=17", "release=16"),
                 original.replace(guest.BUILD, "other"), "", original + "ro.build.version.sdk=37\n"]
        for value in cases:
            with self.subTest(value=value):
                path.write_text(value)
                with self.assertRaises(guest.PreparationError):
                    self.stage()
                self.assert_untouched()
        self.run.assert_not_called()

    def test_every_required_guest_input_must_exist(self):
        for relative in ("system/build.prop", *guest.GUEST_ELF_REQUIRED):
            with self.subTest(relative=relative):
                path = self.root / relative
                saved = self.work / "missing-input"
                path.rename(saved)
                try:
                    with self.assertRaises(guest.PreparationError):
                        self.stage()
                finally:
                    saved.rename(path)
                self.assert_untouched()
        self.run.assert_not_called()

    def test_required_guest_directories_are_not_silently_skipped(self):
        for relative in (guest.ART_LIB, guest.BIONIC_LIB):
            with self.subTest(relative=relative):
                path = self.root / relative
                saved = self.work / "missing-directory"
                path.rename(saved)
                try:
                    with self.assertRaises(guest.PreparationError):
                        self.stage()
                finally:
                    saved.rename(path)
                self.assert_untouched()
        self.run.assert_not_called()

    def test_all_required_guest_elf_inputs_must_be_arm64(self):
        for relative in guest.GUEST_ELF_REQUIRED:
            with self.subTest(relative=relative):
                path = self.root / relative
                path.write_bytes(elf(62))
                try:
                    with self.assertRaisesRegex(guest.PreparationError, "ELF64 AArch64"):
                        self.stage()
                finally:
                    path.write_bytes(elf())
                self.assert_untouched()
        self.run.assert_not_called()

    def test_invalid_elf_headers_and_extra_x86_library_are_refused(self):
        path = self.root / "system/lib64/libextra.so"
        good = elf()
        cases = [b"", good[:20], b"not ELF" + good[7:]]
        for offset, value in ((4, 1), (5, 2), (6, 0), (16, 1), (20, 0), (52, 0)):
            header = bytearray(good)
            header[offset] = value
            cases.append(bytes(header))
        for value in cases + [elf(62)]:
            with self.subTest(value=value[:24]):
                path.write_bytes(value)
                with self.assertRaises(guest.PreparationError):
                    self.stage()
                self.assert_untouched()
        self.run.assert_not_called()

    def test_source_links_must_be_resolvable_and_confined(self):
        outside = self.work / "outside-source"
        outside.write_bytes(elf())
        path = self.root / "system/lib64/libextra.so"
        path.unlink()
        for target in (outside, self.work / "missing", Path(path.name)):
            with self.subTest(target=target):
                path.symlink_to(target)
                try:
                    with self.assertRaises(guest.PreparationError):
                        self.stage()
                finally:
                    path.unlink()
                self.assert_untouched()
        self.assertEqual(outside.read_bytes(), elf())
        self.run.assert_not_called()

    def test_source_directory_links_cannot_escape(self):
        outside = self.work / "outside-directory"
        outside.mkdir()
        (outside / "secret").write_bytes(b"private")
        (self.root / "system/lib64/escape").symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(guest.PreparationError, "escapes extracted tree"):
            self.stage()
        self.assert_untouched()
        self.run.assert_not_called()

    def test_source_hardlinks_are_refused(self):
        outside = self.work / "hardlink-source"
        outside.write_bytes(elf())
        path = self.root / "system/lib64/libandroid.so"
        path.unlink()
        path.hardlink_to(outside)
        with self.assertRaisesRegex(guest.PreparationError, "hardlinked"):
            self.stage()
        self.assert_untouched()
        self.run.assert_not_called()

    def test_unowned_and_legacy_installations_are_never_adopted(self):
        for relative in guest.OVERLAYS:
            (self.sysroot / relative).mkdir()
        victim = self.sysroot / "system/bin/app_process64"
        (self.sysroot / "system/bin/arm64/app_process64").symlink_to(victim)
        (self.sysroot / "system/lib64/arm64/libandroid.so").hardlink_to(self.sysroot / "system/lib64/libandroid.so")
        (self.sysroot / guest.MANIFEST).write_text(json.dumps({"tool": guest.OWNER, "schema": 1,
                                                              "archive_sha256": guest.SHA256}))
        before = snapshot(self.sysroot)
        # The host hardlink itself must also be rejected, even before guest checks.
        with self.assertRaises((OSError, guest.PreparationError)):
            self.stage()
        self.assert_untouched(before)
        (self.sysroot / "system/lib64/arm64/libandroid.so").unlink()
        before = snapshot(self.sysroot)
        with self.assertRaisesRegex(guest.PreparationError, "legacy"):
            self.stage()
        self.assert_untouched(before)
        self.run.assert_not_called()

    def test_partial_or_directory_alias_destinations_are_refused(self):
        path = self.sysroot / "system/bin/arm64"
        path.symlink_to(".", target_is_directory=True)
        before = snapshot(self.sysroot)
        with self.assertRaisesRegex(guest.PreparationError, "unowned/incomplete"):
            self.stage()
        self.assert_untouched(before)
        self.run.assert_not_called()

    def test_modified_owned_overlay_files_modes_and_inventory_are_refused(self):
        self.stage()
        self.run.reset_mock()
        path = self.sysroot / "system/lib64/arm64/bootstrap/libart.so"
        original = path.read_bytes()
        original_mode = stat.S_IMODE(path.stat().st_mode)
        modifications = (
            (lambda: path.write_bytes(b"changed bootstrap"), lambda: path.write_bytes(original)),
            (lambda: path.chmod(0o600), lambda: path.chmod(original_mode)),
            (lambda: (path.parent / "unexpected").mkdir(), lambda: (path.parent / "unexpected").rmdir()),
            (lambda: (path.parent / "stale.so").write_bytes(elf()), lambda: (path.parent / "stale.so").unlink()),
        )
        for change, restore in modifications:
            with self.subTest(change=change):
                change()
                before = snapshot(self.sysroot)
                try:
                    with self.assertRaisesRegex(guest.PreparationError, "changed"):
                        self.stage()
                    self.assert_untouched(before)
                finally:
                    restore()
        self.run.assert_not_called()

    def test_owned_overlay_rejects_leaf_symlinks_hardlinks_and_manifest_links(self):
        self.stage()
        self.run.reset_mock()
        outside = self.work / "outside-target"
        outside.write_bytes(elf())
        for relative in ("system/lib64/arm64/bootstrap/libart.so", guest.MANIFEST):
            path = self.sysroot / relative
            saved = path.read_bytes()
            mode = stat.S_IMODE(path.stat().st_mode)
            for kind in ("symlink", "hardlink"):
                with self.subTest(relative=relative, kind=kind):
                    path.unlink()
                    if kind == "symlink":
                        path.symlink_to(outside)
                    else:
                        path.hardlink_to(outside)
                    before = snapshot(self.sysroot)
                    with self.assertRaises((OSError, guest.PreparationError)):
                        self.stage()
                    self.assert_untouched(before)
                    self.assertEqual(outside.read_bytes(), elf())
                    path.unlink()
                    path.write_bytes(saved)
                    path.chmod(mode)
        self.run.assert_not_called()

    def test_missing_or_empty_vdso_provenance_is_refused_on_reuse(self):
        self.stage()
        valid = self.record()
        missing = copy.deepcopy(valid)
        del missing["vdso_build"]
        self.assert_reuse_refused(missing, "VDSO provenance")
        for value in (None, {}, [], "not provenance", False, 1):
            with self.subTest(value=value):
                self.assert_reuse_refused(valid | {"vdso_build": value}, "VDSO provenance")

    def test_vdso_provenance_requires_all_recorded_fields(self):
        self.stage()
        valid = self.record()
        fields = (("source_path",), ("source_sha256",), ("compiler",), ("arguments",),
                  ("environment",), ("note",), ("compiler", "path"), ("compiler", "sha256"),
                  ("compiler", "version"), ("environment", "PATH"), ("environment", "LC_ALL"))
        for fields_path in fields:
            with self.subTest(field=".".join(fields_path)):
                record = copy.deepcopy(valid)
                parent = record["vdso_build"]
                for field in fields_path[:-1]:
                    parent = parent[field]
                del parent[fields_path[-1]]
                self.assert_reuse_refused(record, "VDSO provenance")

    def test_vdso_provenance_rejects_invalid_field_types_and_values(self):
        self.stage()
        valid = self.record()
        bad_text = (None, False, 1, [], {}, "", " \t", "nul\0text")
        cases = [(("source_path",), (*bad_text, "relative/vdso_arm64.S")),
                 (("note",), bad_text), (("compiler",), (None, [], "clang", False, {})),
                 (("compiler", "path"), (*bad_text, "relative/clang")),
                 (("compiler", "version"), bad_text),
                 (("arguments",), (None, False, {}, "--target=aarch64", [],
                                   [None], [1], [False], [{}], [""], [" \t"], ["nul\0arg"])),
                 (("environment",), (None, False, [], "PATH=/bin", {},
                                     {"PATH": 1, "LC_ALL": "C"},
                                     {"PATH": [], "LC_ALL": "C"},
                                     {"PATH": "", "LC_ALL": "C"},
                                     {"PATH": "nul\0path", "LC_ALL": "C"},
                                     {"PATH": "/bin", "LC_ALL": None},
                                     {"PATH": "/bin", "LC_ALL": " \t"},
                                     {"PATH": "/bin", "LC_ALL": "C", "": "bad key"},
                                     {"PATH": "/bin", "LC_ALL": "C", "EXTRA": False}))]
        for fields_path, values in cases:
            for value in values:
                with self.subTest(field=".".join(fields_path), value=value):
                    record = copy.deepcopy(valid)
                    parent = record["vdso_build"]
                    for field in fields_path[:-1]:
                        parent = parent[field]
                    parent[fields_path[-1]] = value
                    self.assert_reuse_refused(record, "VDSO provenance")

    def test_vdso_provenance_hashes_must_be_lowercase_sha256(self):
        self.stage()
        valid = self.record()
        values = (None, False, 1, [], {}, "", "a" * 63, "a" * 65, "g" * 64,
                  "A" * 64, " " + "a" * 63, "a" * 63 + "\n")
        for fields_path in (("source_sha256",), ("compiler", "sha256")):
            for value in values:
                with self.subTest(field=".".join(fields_path), value=value):
                    record = copy.deepcopy(valid)
                    parent = record["vdso_build"]
                    for field in fields_path[:-1]:
                        parent = parent[field]
                    parent[fields_path[-1]] = value
                    self.assert_reuse_refused(record, "VDSO provenance.*sha256")

    def test_incomplete_generated_vdso_provenance_is_refused_before_publication(self):
        def no_version(arguments, **kwargs):
            result = self.fake_compiler(arguments, **kwargs)
            if arguments[1:] == ["--version"]:
                result.stdout = " \n"
            return result
        self.run.side_effect = no_version
        with self.assertRaisesRegex(guest.PreparationError, "VDSO provenance.*compiler.version"):
            self.stage()
        self.assert_untouched()

    def test_staged_only_compatibility_is_required_exactly_on_reuse(self):
        self.stage()
        valid = self.record()
        expected = {"state": "staged-only",
                    "blocker": "Stock GSI linker64 and libc are not native-bridge variants; "
                               "Digitalis requires the modified guest linker and libc."}
        self.assertEqual(valid["bridge_compatibility"], expected)
        missing = copy.deepcopy(valid)
        del missing["bridge_compatibility"]
        self.assert_reuse_refused(missing, "bridge_compatibility")
        values = (None, False, [], "staged-only", {}, {"state": "staged-only"},
                  {"blocker": expected["blocker"]}, expected | {"state": "compatible"},
                  expected | {"state": 1}, expected | {"blocker": None},
                  expected | {"blocker": ""}, expected | {"blocker": "different blocker"},
                  expected | {"extra": "not part of schema 2"})
        for value in values:
            with self.subTest(value=value):
                self.assert_reuse_refused(valid | {"bridge_compatibility": value}, "bridge_compatibility")

    def test_staged_only_warning_is_printed_for_preparation_and_reuse(self):
        with mock.patch.object(guest, "SYSROOT", self.sysroot), \
                mock.patch.object(guest, "VDSO_SOURCE", self.source), \
                mock.patch.object(guest, "COMPILER", self.compiler), \
                mock.patch.object(guest, "unpack_archive", return_value=self.root) as unpack, \
                mock.patch.object(guest.sys, "argv", ["prepare-arm64-guest", "--archive", "not-read.zip"]):
            for attempt in range(2):
                stdout, stderr = io.StringIO(), io.StringIO()
                with self.subTest(attempt=attempt), mock.patch.object(guest.sys, "stdout", stdout), \
                        mock.patch.object(guest.sys, "stderr", stderr):
                    self.assertEqual(guest.main(), 0)
                record = self.record()
                self.assertEqual(record["schema"], 2)
                self.assertEqual(record["bridge_compatibility"]["state"], "staged-only")
                self.assertEqual(stderr.getvalue().count("warning:"), 1)
                self.assertIn("staged-only", stderr.getvalue())
                self.assertIn(record["bridge_compatibility"]["blocker"], stderr.getvalue())
                self.assertIn(f"verified {len(record['files'])} ARM64 guest files", stdout.getvalue())
                if attempt == 0:
                    unpack.assert_called_once()
                    before = snapshot(self.sysroot)
                    unpack.reset_mock()
                    unpack.side_effect = AssertionError("reuse must not extract")
                    self.run.reset_mock()
                else:
                    self.assertIn("already verified; no files changed", stdout.getvalue())
                    unpack.assert_not_called()
                    self.run.assert_not_called()
                    self.assert_untouched(before)

    def test_verified_owned_installation_is_reused_without_source_or_compiler(self):
        expected = self.stage()
        before = snapshot(self.sysroot)
        self.source.unlink()
        self.compiler.unlink()
        self.run.reset_mock()
        source = mock.Mock(side_effect=AssertionError("must not extract or rebuild"))
        self.assertEqual(guest.install(source, **self.kwargs), expected)
        source.assert_not_called()
        self.run.assert_not_called()
        self.assert_untouched(before)

    def test_compiler_failure_invalid_output_or_missing_source_leave_no_overlay(self):
        def failed(arguments, **kwargs):
            if arguments[1:] == ["--version"]:
                return self.fake_compiler(arguments, **kwargs)
            raise subprocess.CalledProcessError(1, arguments)

        def wrong_arch(arguments, **kwargs):
            result = self.fake_compiler(arguments, **kwargs)
            if arguments[1:] != ["--version"]:
                Path(arguments[-1]).write_bytes(elf(62))
            return result

        for callback in (failed, wrong_arch):
            with self.subTest(callback=callback):
                self.run.side_effect = callback
                with self.assertRaises((guest.PreparationError, subprocess.SubprocessError)):
                    self.stage()
                self.assert_untouched()
        self.run.side_effect = self.fake_compiler
        with self.assertRaises(OSError):
            self.stage(vdso_source=self.work / "missing-source")
        self.assert_untouched()

    def test_required_guest_binaries_must_be_executable(self):
        for name in ("app_process64", "linker64"):
            path = self.root / "system/bin" / name
            path.chmod(0o644)
            with self.subTest(name=name):
                with self.assertRaisesRegex(guest.PreparationError, "execute permission"):
                    self.stage()
                self.assert_untouched()
            path.chmod(0o755)
        self.run.assert_not_called()

    def test_private_staging_and_retention_are_safe_with_permissive_umask(self):
        previous = os.umask(0)
        try:
            self.stage(keep_extracted=True)
        finally:
            os.umask(previous)
        for info in self.record()["files"].values():
            self.assertFalse(info["mode"] & 0o7022)
        for mode in self.record()["directories"].values():
            self.assertFalse(mode & 0o7022)
        self.assertTrue((self.retained / "root/system/build.prop").is_file())

    def test_vdso_provenance_hashes_the_compiled_snapshot_not_a_later_source(self):
        original = self.source.read_bytes()
        def mutate_source(arguments, **kwargs):
            result = self.fake_compiler(arguments, **kwargs)
            if arguments[1:] != ["--version"]:
                self.source.write_bytes(b"a later local source revision")
            return result
        self.run.side_effect = mutate_source
        self.stage()
        build = self.record()["vdso_build"]
        self.assertEqual(build["source_sha256"], hashlib.sha256(original).hexdigest())
        self.assertNotEqual(build["source_sha256"], guest.digest(self.source))

    def test_compiler_change_during_build_is_refused_before_publication(self):
        def mutate_compiler(arguments, **kwargs):
            result = self.fake_compiler(arguments, **kwargs)
            if arguments[1:] != ["--version"]:
                self.compiler.write_bytes(b"changed compiler")
            return result
        self.run.side_effect = mutate_compiler
        with self.assertRaisesRegex(guest.PreparationError, "compiler or source changed"):
            self.stage()
        self.assert_untouched()

    def test_host_change_during_build_is_not_overwritten_or_published(self):
        path = self.sysroot / "system/framework/framework.jar"
        def mutate_host(arguments, **kwargs):
            result = self.fake_compiler(arguments, **kwargs)
            if arguments[1:] != ["--version"]:
                path.write_bytes(b"external host modification")
            return result
        self.run.side_effect = mutate_host
        with self.assertRaisesRegex(guest.PreparationError, "host runtime files changed"):
            self.stage()
        self.assertEqual(path.read_bytes(), b"external host modification")
        self.assertFalse(any((self.sysroot / relative).exists() for relative in (*guest.OVERLAYS, guest.MANIFEST)))
        for relative, value in self.baseline.items():
            if relative != path.relative_to(self.sysroot).as_posix():
                self.assertEqual(snapshot(self.sysroot)[relative], value)

    def test_generated_vdso_cannot_be_a_symlink_or_hardlink(self):
        outside = self.work / "outside-vdso"
        outside.write_bytes(elf())
        for kind in ("symlink", "hardlink"):
            def linked(arguments, **kwargs):
                result = self.fake_compiler(arguments, **kwargs)
                if arguments[1:] != ["--version"]:
                    path = Path(arguments[-1])
                    path.unlink()
                    if kind == "symlink":
                        path.symlink_to(outside)
                    else:
                        path.hardlink_to(outside)
                return result
            with self.subTest(kind=kind):
                self.run.side_effect = linked
                with self.assertRaises((OSError, guest.PreparationError)):
                    self.stage()
                self.assert_untouched()
                self.assertEqual(outside.read_bytes(), elf())

    def test_copy_and_manifest_failures_roll_back_created_entries(self):
        original = guest.copy_regular
        for relative in ("system/lib64/arm64/libc.so", guest.MANIFEST):
            def fail_after_copy(source, destination, journal=None):
                result = original(source, destination, journal)
                if Path(destination) == self.sysroot / relative:
                    raise OSError("injected publication failure after file creation")
                return result
            with self.subTest(relative=relative), mock.patch.object(guest, "copy_regular", side_effect=fail_after_copy):
                with self.assertRaisesRegex(OSError, "injected publication failure"):
                    self.stage()
                self.assert_untouched()

    def test_rollback_preserves_foreign_entries_in_created_directory(self):
        original = guest.copy_regular
        foreign = self.sysroot / "system/lib64/arm64/user-file"
        def fail(source, destination, journal=None):
            result = original(source, destination, journal)
            if Path(destination) == self.sysroot / "system/lib64/arm64/libc.so":
                foreign.write_bytes(b"do not recursively delete me")
                raise OSError("injected failure")
            return result
        with mock.patch.object(guest, "copy_regular", side_effect=fail):
            with self.assertRaisesRegex(guest.PreparationError, "rollback left paths"):
                self.stage()
        self.assertEqual(foreign.read_bytes(), b"do not recursively delete me")
        self.assertFalse((self.sysroot / guest.MANIFEST).exists())
        self.assertFalse((self.sysroot / guest.OVERLAYS[0]).exists())

    def test_destination_appearing_during_build_is_preserved(self):
        marker = self.sysroot / "system/bin/arm64/user-file"
        def appeared(arguments, **kwargs):
            result = self.fake_compiler(arguments, **kwargs)
            if arguments[1:] != ["--version"]:
                marker.parent.mkdir()
                marker.write_bytes(b"unowned")
            return result
        self.run.side_effect = appeared
        with self.assertRaisesRegex(guest.PreparationError, "unowned/incomplete"):
            self.stage()
        self.assertEqual(marker.read_bytes(), b"unowned")
        self.assertFalse((self.sysroot / guest.MANIFEST).exists())
        self.assertFalse((self.sysroot / guest.OVERLAYS[1]).exists())

    def test_lock_contention_and_linked_locks_are_refused(self):
        with guest.publication_lock(self.sysroot):
            with self.assertRaisesRegex(guest.PreparationError, "holds the lock"):
                self.stage()
        lock = self.sysroot / guest.LOCK
        lock.unlink()
        outside = self.work / "outside-lock"
        outside.write_bytes(b"do not truncate")
        for kind in ("symlink", "hardlink"):
            with self.subTest(kind=kind):
                if kind == "symlink":
                    lock.symlink_to(outside)
                else:
                    lock.hardlink_to(outside)
                with self.assertRaises((OSError, guest.PreparationError)):
                    self.stage()
                self.assertEqual(outside.read_bytes(), b"do not truncate")
                lock.unlink()
        self.assert_untouched()
        self.run.assert_not_called()

    def test_overlay_mutation_during_retention_publication_is_refused_and_rolled_back(self):
        original = guest.copy_regular
        changed = self.sysroot / "system/lib64/arm64/libc.so"
        trigger = self.retained / "root/system/build.prop"
        mutations = []
        def mutate(source, destination, journal=None):
            result = original(source, destination, journal)
            if Path(destination) == trigger:
                self.assertFalse((self.sysroot / guest.MANIFEST).exists())
                changed.write_bytes(b"modified during retention publication")
                mutations.append(changed)
            return result
        with mock.patch.object(guest, "copy_regular", side_effect=mutate):
            with self.assertRaisesRegex(guest.PreparationError, "published overlay does not match staged inventory"):
                self.stage(keep_extracted=True)
        self.assertEqual(mutations, [changed])
        self.assertFalse(self.retained.exists())
        self.assert_untouched()

    def test_overlay_mutation_during_manifest_publication_is_refused_and_rolled_back(self):
        original = guest.copy_regular
        changed = self.sysroot / "system/lib64/arm64/libc.so"
        def mutate(source, destination, journal=None):
            result = original(source, destination, journal)
            if Path(destination) == self.sysroot / guest.MANIFEST:
                changed.write_bytes(b"modified during manifest publication")
            return result
        with mock.patch.object(guest, "copy_regular", side_effect=mutate):
            with self.assertRaisesRegex(guest.PreparationError, "published overlay changed during manifest publication"):
                self.stage(keep_extracted=True)
        self.assertFalse(self.retained.exists())
        self.assert_untouched()

    def test_existing_retention_is_never_replaced_even_with_old_sentinel(self):
        sentinel = self.retained / "root/system/bin/app_process64"
        sentinel.parent.mkdir(parents=True)
        sentinel.write_bytes(b"old extraction")
        (self.retained / "user-file").write_bytes(b"keep")
        before = snapshot(self.retained)
        with self.assertRaisesRegex(guest.PreparationError, "never replaced or deleted"):
            self.stage(keep_extracted=True)
        self.assertEqual(snapshot(self.retained), before)
        self.assert_untouched()
        self.run.assert_not_called()

    def test_linked_retention_destination_is_refused(self):
        outside = self.work / "outside-retention"
        outside.mkdir()
        self.retained.symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(guest.PreparationError, "existing retained extraction"):
            self.stage(keep_extracted=True)
        self.assertTrue(self.retained.is_symlink())
        self.assertEqual(list(outside.iterdir()), [])
        self.assert_untouched()
        self.run.assert_not_called()

    def test_fresh_retention_preserves_symlinks_as_data(self):
        outside = self.work / "outside-retained-link"
        outside.write_bytes(b"not read or copied")
        (self.root / "unused-link").symlink_to(outside)
        self.stage(keep_extracted=True)
        path = self.retained / "root/unused-link"
        self.assertTrue(path.is_symlink())
        self.assertEqual(os.readlink(path), str(outside))
        self.assertEqual(outside.read_bytes(), b"not read or copied")
        self.assertEqual((self.retained / "root/system/bin/app_process64").read_bytes(), elf())
        self.assertTrue((self.retained / guest.MANIFEST).is_file())
        retained_record = json.loads((self.retained / guest.MANIFEST).read_text())
        self.assertEqual(retained_record["bridge_compatibility"], self.record()["bridge_compatibility"])
        before = snapshot(self.retained)
        with self.assertRaises(guest.PreparationError):
            self.stage(keep_extracted=True)
        self.assertEqual(snapshot(self.retained), before)

    def test_retention_publication_failure_also_rolls_back_overlay(self):
        original = guest.copy_regular
        def fail(source, destination, journal=None):
            result = original(source, destination, journal)
            if Path(destination) == self.retained / "root/system/bin/app_process64":
                raise OSError("retention publication failed")
            return result
        with mock.patch.object(guest, "copy_regular", side_effect=fail):
            with self.assertRaisesRegex(OSError, "retention publication failed"):
                self.stage(keep_extracted=True)
        self.assertFalse(self.retained.exists())
        self.assert_untouched()

    def test_main_fails_closed_on_legacy_installation_before_extracting(self):
        for relative in guest.OVERLAYS:
            (self.sysroot / relative).mkdir()
        (self.sysroot / guest.MANIFEST).write_text(json.dumps({"tool": guest.OWNER, "schema": 1}))
        before = snapshot(self.sysroot)
        stderr = io.StringIO()
        with mock.patch.object(guest, "SYSROOT", self.sysroot), \
                mock.patch.object(guest, "unpack_archive") as unpack, \
                mock.patch.object(guest.sys, "argv", ["prepare-arm64-guest", "--archive", "not-read.zip"]), \
                mock.patch.object(guest.sys, "stderr", stderr):
            self.assertEqual(guest.main(), 1)
        self.assertIn("legacy", stderr.getvalue())
        self.assertIn("replacement is unsupported", stderr.getvalue())
        unpack.assert_not_called()
        self.run.assert_not_called()
        self.assert_untouched(before)

    def test_archive_hash_precedes_zip_read_and_only_exact_member_is_used(self):
        archive = self.work / "fixture.zip"
        archive.write_bytes(b"not ZIP")
        with mock.patch.object(guest.zipfile, "ZipFile") as container:
            with self.assertRaisesRegex(guest.PreparationError, "SHA-256 mismatch"):
                guest.extract_image(archive, self.work)
            container.assert_not_called()
        self.assertFalse((self.work / "system.raw").exists())
        with zipfile.ZipFile(archive, "w") as container:
            container.writestr("system.img", b"raw image fixture")
            container.writestr("../escape", b"must not extract")
        with mock.patch.object(guest, "SHA256", hashlib.sha256(archive.read_bytes()).hexdigest()):
            image = guest.extract_image(archive, self.work)
        self.assertEqual(image.read_bytes(), b"raw image fixture")
        self.assertFalse((self.work.parent / "escape").exists())
        self.run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
