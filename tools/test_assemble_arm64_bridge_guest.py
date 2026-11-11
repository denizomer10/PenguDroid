#!/usr/bin/env python3
"""Offline regressions for the ARM64 guest bundle assembler; no GSI is needed.

The assembler is the gate that keeps a broken guest runtime out of
`.toolchain/guest-arm64-bridge`, so its refusals matter more than its happy
path: a stub that does not import `native_bridge_intercept_symbol` would run the
Bionic implementation instead of the host proxy, and an incomplete library
closure only fails later, as a `dlopen` error inside the game.

The libraries here are synthesized: a minimal ELF64 AArch64 writer produces just
the sections the assembler reads (`.dynsym`, `.dynstr`, `.dynamic`).
"""
import importlib.util
import os
from pathlib import Path
import struct
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    "assemble_arm64_bridge_guest", Path(__file__).with_name("assemble-arm64-bridge-guest.py"))
assemble = importlib.util.module_from_spec(spec)
spec.loader.exec_module(assemble)

SHT_DYNAMIC = 6
SHT_DYNSYM = 11
SHT_STRTAB = 3
SHN_UNDEF = 0
STB_GLOBAL = 1
DT_NEEDED = 1
DT_NULL = 0


def elf64(exports=(), imports=(), needed=()):
    """An ELF64 little-endian AArch64 image carrying the given symbols."""
    strings = bytearray(b"\0")
    offsets = {}

    def name(text):
        if text not in offsets:
            offsets[text] = len(strings)
            strings.extend(text.encode() + b"\0")
        return offsets[text]

    symbols = [(0, 0, 0, 0)]
    for text in exports:
        symbols.append((name(text), STB_GLOBAL << 4, 1, 1))
    for text in imports:
        symbols.append((name(text), STB_GLOBAL << 4, 0, SHN_UNDEF))
    dynsym = b"".join(struct.pack("<IBBHQQ", n, i, 0, shndx, 0, 0) for n, i, _, shndx in symbols)

    dynamic = b""
    for soname in needed:
        dynamic += struct.pack("<QQ", DT_NEEDED, name(soname))
    dynamic += struct.pack("<QQ", DT_NULL, 0)

    header = bytearray(64)
    header[:4] = b"\x7fELF"
    header[4] = 2  # ELFCLASS64
    header[5] = 1  # little-endian
    struct.pack_into("<H", header, 16, 3)  # ET_DYN
    struct.pack_into("<H", header, 18, 183)  # EM_AARCH64
    struct.pack_into("<H", header, 52, 64)  # e_ehsize

    body = bytearray()
    offsets_start = len(header)
    sections = []
    for kind, payload in ((SHT_STRTAB, bytes(strings)),
                          (SHT_DYNSYM, dynsym),
                          (SHT_DYNAMIC, dynamic)):
        while len(body) % 8:
            body.append(0)
        offset = offsets_start + len(body)
        sections.append([offset, len(payload)])
        body.extend(payload)
    while len(body) % 8:
        body.append(0)

    # Section 0 is the mandatory null entry; 1 = .dynstr, 2 = .dynsym,
    # 3 = .dynamic. Both tables link to the string table at index 1.
    table = bytearray(64 * 4)
    entries = [(1, sections[0], 0, 0),
               (SHT_DYNSYM, sections[1], 24, 1),
               (SHT_DYNAMIC, sections[2], 16, 1)]
    for index, (kind, (offset, size), entsize, link) in enumerate(entries, start=1):
        struct.pack_into("<IIQQQQIIQQ", table, index * 64,
                         0, kind, 0, 0, offset, size, link, 0, 8, entsize)
    shoff = offsets_start + len(body)
    struct.pack_into("<Q", header, 40, shoff)
    struct.pack_into("<HH", header, 58, 64, 4)  # e_shentsize, e_shnum
    return bytes(header) + bytes(body) + bytes(table)


def write(path, **kwargs):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(elf64(**kwargs))
    return path


class GuestStubTests(unittest.TestCase):
    def test_a_stub_without_the_intercept_import_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            good = write(root / "system/lib64/arm64/libandroid.so",
                         imports=["native_bridge_intercept_symbol"])
            self.assertEqual(assemble.stub_files(root), [good])
            bad = write(root / "system/lib64/arm64/libEGL.so", imports=["eglGetDisplay"])
            with self.assertRaises(assemble.AssemblyError) as raised:
                assemble.stub_files(root)
            self.assertIn("native_bridge_intercept_symbol", str(raised.exception))
            self.assertIn(str(bad), str(raised.exception))

    def test_a_stub_root_must_be_a_guest_library_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(assemble.AssemblyError) as raised:
                assemble.stub_files(Path(tmp))
            self.assertIn("system/lib64/arm64", str(raised.exception))
            (Path(tmp) / "system/lib64/arm64").mkdir(parents=True)
            with self.assertRaises(assemble.AssemblyError) as raised:
                assemble.stub_files(Path(tmp))
            self.assertIn("empty", str(raised.exception))

    def test_only_the_guest_library_directory_is_read(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write(root / "system/lib64/arm64/libandroid.so",
                  imports=["native_bridge_intercept_symbol"])
            write(root / "system/bin/arm64/linker64", exports=["__libc_init"])
            names = [p.name for p in assemble.stub_files(root)]
            self.assertEqual(names, ["libandroid.so"])


class ClosureTests(unittest.TestCase):
    def test_a_missing_soname_is_pulled_from_the_apex_root_transitively(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            bundle = root / "bundle"
            write(bundle / "system/lib64/arm64/libandroid.so", needed=["libicu.so"])
            apex = root / "apex"
            write(apex / "libicu.so", needed=["libicu_data.so"])
            write(apex / "libicu_data.so")
            added = assemble.complete_closure(bundle, [apex])
            self.assertEqual(sorted(added), ["libicu.so", "libicu_data.so"])
            self.assertEqual(added["libicu.so"]["needed_by"], ["libandroid.so"])
            self.assertEqual(added["libicu_data.so"]["needed_by"], ["libicu.so"])
            staged = (bundle / "system/lib64/arm64/libicu.so").read_bytes()
            self.assertEqual(staged, (apex / "libicu.so").read_bytes())
            # A second pass has nothing left to do.
            self.assertEqual(assemble.complete_closure(bundle, [apex]), {})

    def test_an_unresolved_soname_names_the_provider_and_hints_at_apex(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            bundle = root / "bundle"
            write(bundle / "system/lib64/arm64/libandroid.so", needed=["libicu.so"])
            with self.assertRaises(assemble.AssemblyError) as raised:
                assemble.complete_closure(bundle, [])
            message = str(raised.exception)
            self.assertIn("libicu.so", message)
            self.assertIn("libandroid.so", message)
            self.assertIn("--apex-libs", message)

    def test_a_soname_the_kernel_provides_is_never_copied(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            bundle = root / "bundle"
            write(bundle / "system/lib64/arm64/libandroid.so", needed=["linux-vdso.so.1"])
            self.assertEqual(assemble.complete_closure(bundle, []), {})

    def test_a_non_aarch64_candidate_is_not_accepted(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            bundle = root / "bundle"
            write(bundle / "system/lib64/arm64/libandroid.so", needed=["libicu.so"])
            apex = root / "apex"
            apex.mkdir()
            (apex / "libicu.so").write_bytes(b"not an elf at all")
            with self.assertRaises(assemble.AssemblyError):
                assemble.complete_closure(bundle, [apex])


class AssembleTests(unittest.TestCase):
    def test_an_existing_destination_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            staging, built, dest = root / "staging", root / "built", root / "dest"
            for directory in (staging, built, dest):
                directory.mkdir()
            with self.assertRaises(assemble.AssemblyError) as raised:
                assemble.assemble(staging=staging, built=built, dest=dest)
            self.assertIn("refusing existing destination", str(raised.exception))

    def test_a_staging_tree_without_guest_runtime_directories_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            staging, built = root / "staging", root / "built"
            staging.mkdir()
            write(built / "system/lib64/arm64/libc.so", exports=["__libc_init"])
            with self.assertRaises(assemble.AssemblyError) as raised:
                assemble.assemble(staging=staging, built=built, dest=root / "dest", stubs=None)
            self.assertIn("staged guest is missing", str(raised.exception))

    def test_an_empty_native_bridge_bundle_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            staging, built = root / "staging", root / "built"
            for relative in assemble.RUNTIME_DIRS:
                (staging / relative).mkdir(parents=True)
                (staging / relative / "placeholder").write_bytes(b"")
            built.mkdir()
            with self.assertRaises(assemble.AssemblyError) as raised:
                assemble.assemble(staging=staging, built=built, dest=root / "dest", stubs=None)
            self.assertIn("is empty", str(raised.exception))

    def test_a_missing_native_bridge_bundle_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            staging = root / "staging"
            staging.mkdir()
            with self.assertRaises(assemble.AssemblyError) as raised:
                assemble.assemble(staging=staging, built=root / "absent",
                                  dest=root / "dest", stubs=None)
            self.assertIn("is missing", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
