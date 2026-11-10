#!/usr/bin/env python3
"""Offline contract tests for the generated ARM64 intercept stubs.

These cover the two mistakes that kept breaking `dlopen` for the guest GL
libraries, both of which are invisible without reading the ELF metadata:
a missing `DT_SONAME`, and a version node whose name differs from the name the
referencing library recorded in its verneed entry.
"""
import importlib.util
from pathlib import Path
import re
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location(
    "build_guest_stubs", Path(__file__).with_name("build-guest-stubs.py")
)
stubs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(stubs)


def readelf(document):
    """Fake llvm-readelf --version-info output for `run()`."""
    return "\n".join(document) + "\n"


class VersionNodeTests(unittest.TestCase):
    def test_every_stub_node_is_a_bionic_symbol_version(self):
        for library, nodes in stubs.VERSION_NODES.items():
            self.assertIn(library, stubs.STUB_LIBRARIES)
            for name, parent in nodes:
                self.assertRegex(
                    name,
                    r"^[A-Z0-9_]+$",
                    "version node names are compared byte for byte, so %r must "
                    "match bionic's spelling" % name,
                )
                if parent is not None:
                    self.assertIn(parent, [node for node, _ in nodes])

    def test_version_map_declares_nodes_and_inheritance(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            script = stubs.version_map("libandroid", root / "libandroid.map")
            text = script.read_text(encoding="utf-8")
            self.assertIn("LIBANDROID {", text)
            self.assertIn("} LIBANDROID;", text)
            # Only the base node re-exports the whole stub surface.
            self.assertEqual(text.count("global: *;"), 1)

    def test_version_map_skips_libraries_without_nodes(self):
        with tempfile.TemporaryDirectory() as directory:
            self.assertIsNone(stubs.version_map("libEGL", Path(directory) / "x.map"))

    def test_link_line_carries_soname_and_version_script(self):
        source = Path(__file__).with_name("build-guest-stubs.py").read_text(
            encoding="utf-8"
        )
        self.assertIn('"-Wl,-soname=" + library + ".so"', source)
        self.assertIn('"-Wl,--version-script=" + str(script)', source)

    def test_defined_nodes_ignore_base_and_soname(self):
        document = [
            "Version symbols section '.gnu.version' contains 4 entries:",
            "  000:   0 (*local*)       2 (LIBANDROID)",
            "Version definition section '.gnu.version_d' contains 3 entries:",
            " Addr: 0000  Offset: 0x10  Link: 7 (.dynstr)",
            "  0x0000: Rev: 1  Flags: BASE  Index: 1  Cnt: 1  Name: libandroid.so",
            "  0x001c: Rev: 1  Flags: none  Index: 2  Cnt: 2  Name: LIBANDROID",
            "  0x0038: Rev: 1  Flags: none  Index: 3  Cnt: 1  Name: LIBANDROID_PLATFORM",
        ]
        with mock.patch.object(stubs, "run", return_value=readelf(document)):
            self.assertEqual(
                stubs.defined_version_nodes(Path("libandroid.so")),
                {"LIBANDROID", "LIBANDROID_PLATFORM"},
            )

    def test_referenced_nodes_are_scoped_to_the_requested_soname(self):
        documents = {
            "libandroid_runtime.so": [
                "Version needs section '.gnu.version_r' contains 2 entries:",
                " Addr: 0000  Offset: 0x10  Link: 6 (.dynstr)",
                "  0x0000: Version: 1  File: libGLESv1_CM.so  Cnt: 1",
                "  0x0010:   Name: LIBGLESV1_CM  Flags: none  Version: 15",
                "  0x0020: Version: 1  File: libandroid.so  Cnt: 1",
                "  0x0030:   Name: LIBANDROID  Flags: none  Version: 12",
            ],
            "libEGL.so": [
                "Version needs section '.gnu.version_r' contains 1 entry:",
                " Addr: 0000  Offset: 0x10  Link: 6 (.dynstr)",
                "  0x0000: Version: 1  File: libGLESv1_CM.so  Cnt: 1",
                "  0x0010:   Name: LIBGLESV1_CM  Flags: none  Version: 15",
            ],
        }

        def fake_run(command):
            return readelf(documents[Path(command[-1]).name])

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in documents:
                (root / name).write_bytes(b"\x7fELF")
            with mock.patch.object(stubs, "run", side_effect=fake_run):
                self.assertEqual(
                    stubs.referenced_version_nodes(root, "libGLESv1_CM.so"),
                    {"LIBGLESV1_CM"},
                )
                self.assertEqual(
                    stubs.referenced_version_nodes(root, "libandroid.so"),
                    {"LIBANDROID"},
                )
                self.assertEqual(
                    stubs.referenced_version_nodes(root, "libGLESv2.so"), set()
                )

    def test_case_mismatch_is_reported(self):
        documents = [
            "Version needs section '.gnu.version_r' contains 1 entry:",
            " Addr: 0000  Offset: 0x10  Link: 6 (.dynstr)",
            "  0x0000: Version: 1  File: libGLESv1_CM.so  Cnt: 1",
            "  0x0010:   Name: LIBGLESV1_CM  Flags: none  Version: 15",
        ]
        defined = [
            "Version definition section '.gnu.version_d' contains 2 entries:",
            "  0x0000: Rev: 1  Flags: BASE  Index: 1  Cnt: 1  Name: libGLESv1_CM.so",
            "  0x001c: Rev: 1  Flags: none  Index: 2  Cnt: 1  Name: LIBGLESv1_CM",
        ]

        def fake_run(command):
            return readelf(documents if "runtime" in command[-1] else defined)

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "libandroid_runtime.so").write_bytes(b"\x7fELF")
            stub = root / "libGLESv1_CM.so"
            stub.write_bytes(b"\x7fELF")
            with mock.patch.object(stubs, "run", side_effect=fake_run), mock.patch.object(
                stubs, "check_library"
            ), mock.patch.object(stubs, "stub_source", return_value=root):
                # `--check` must accept the installed stub's own metadata only.
                self.assertEqual(
                    stubs.referenced_version_nodes(root, "libGLESv1_CM.so")
                    - stubs.defined_version_nodes(stub),
                    {"LIBGLESV1_CM"},
                    "a lowercase version node must never satisfy an all-caps "
                    "reference, because bionic compares the names with strcmp",
                )


if __name__ == "__main__":
    unittest.main()
