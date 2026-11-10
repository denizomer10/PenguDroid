#!/usr/bin/env python3
"""Offline tests for the host GL forwarder generator and the guest GL stubs."""

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True

HERE = Path(__file__).with_name


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, HERE(filename))
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


forwarders = load("gl_forwarders", "gl-forwarders.py")


def stub_source(*functions, variables=()):
    lines = ["#include \"native_bridge_support.h\""]
    for function in functions:
        lines.append("DEFINE_INTERCEPTABLE_STUB_FUNCTION(%s);" % function)
    for variable in variables:
        lines.append("DEFINE_INTERCEPTABLE_STUB_VARIABLE(%s);" % variable)
    return "\n".join(lines) + "\n"


class StubSymbolTests(unittest.TestCase):
    def test_reads_functions_and_variables(self):
        functions, variables = forwarders.stub_symbols(
            stub_source("eglGetDisplay", "eglInitialize", variables=("glString",))
        )
        self.assertEqual(functions, ["eglGetDisplay", "eglInitialize"])
        self.assertEqual(variables, ["glString"])

    def test_rejects_source_without_functions(self):
        with self.assertRaises(forwarders.ForwarderError):
            forwarders.stub_symbols("int main(void) { return 0; }")

    def test_rejects_duplicates(self):
        with self.assertRaises(forwarders.ForwarderError) as caught:
            forwarders.stub_symbols(stub_source("eglInitialize", "eglInitialize"))
        self.assertIn("eglInitialize", str(caught.exception))


class AssemblyTests(unittest.TestCase):
    def test_emits_one_jump_per_symbol_and_a_slot_table(self):
        text = forwarders.assembly("libEGL", ["eglGetDisplay", "eglInitialize"])
        self.assertEqual(text.count("jmp *"), 2)
        self.assertIn("eglGetDisplay", text)
        self.assertIn("eglInitialize", text)
        self.assertIn("libEGL", text)

    def test_stubs_are_callable_through_rip_relative_slots(self):
        # A `call rel32` stub would clobber neither registers nor stack, but the
        # generated stub must also not need a data relocation per symbol.
        text = forwarders.assembly("libEGL", ["eglGetDisplay"])
        self.assertIn("(%rip)", text)


class InitializerTests(unittest.TestCase):
    def test_looks_up_sonames_in_order_and_never_uses_rtld_default(self):
        _, sonames = forwarders.HOST_MODULES["libEGL"]
        text = forwarders.initializer("libEGL", ["eglGetDisplay"], sonames)
        # The shim must come first so its fake display wins over Mesa's.
        self.assertEqual(sonames[0], "libpengudroid_gleshim.so")
        self.assertIn("libpengudroid_gleshim.so", text)
        # Looking names up in the global scope would find this library's own stub
        # and recurse forever; only explicit handles may be searched.
        self.assertNotIn("dlsym(RTLD_DEFAULT", text)
        self.assertIn("dlsym(handles[i]", text)
        # Only the handle of the module being filled may be searched, because the
        # forwarder's own stubs are in the default scope.
        self.assertIn("dlopen", text)
        self.assertIn("dlsym", text)

    def test_never_lists_the_module_own_soname(self):
        # `dlopen("libEGL.so")` from inside `libEGL.so` returns this module, and
        # `dlsym` on it hands back the stub that jumps through the slot being
        # filled: every call then spins on a jump to itself, with no syscall, no
        # fault and no log. Only versioned host sonames may be listed.
        for module, (_, sonames) in forwarders.HOST_MODULES.items():
            self.assertNotIn(module + ".so", sonames, module)
            for soname in sonames[1:]:
                self.assertRegex(soname, r"\.so\.\d+$", f"{module}: {soname}")

    def test_rejects_symbols_that_are_this_modules_own_stubs(self):
        _, sonames = forwarders.HOST_MODULES["libEGL"]
        text = forwarders.initializer("libEGL", ["eglGetDisplay"], sonames)
        # The guard is what makes a misconfigured soname harmless rather than an
        # infinite loop, so it must stay in the generated constructor.
        self.assertIn("dladdr((void *)&pd_libEGL_slots[0]", text)
        self.assertIn("is_own_symbol(symbol)", text)
        self.assertIn("info.dli_fbase == g_own_base", text)


class GenerateTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="test-gl-forwarders-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.stubs = self.root / "stubs"
        self.stubs.mkdir()
        (self.stubs / "libEGL_stub.c").write_text(stub_source("eglGetDisplay", "eglInitialize"))
        (self.stubs / "libGLESv1_CM_stub.c").write_text(stub_source("glAlphaFunc", "glClearColor"))
        (self.stubs / "libGLESv2_stub.c").write_text(stub_source("glClear", "glDrawArrays"))
        (self.stubs / "libGLESv3_stub.c").write_text(stub_source("glDispatchCompute"))

    def source(self, library):
        stem = {
            "libEGL": "libEGL",
            "libGLESv1_CM": "libGLESv1_CM",
            "libGLESv2": "libGLESv2",
            "libGLESv3": "libGLESv3",
        }[library]
        return (self.stubs / (stem + "_stub.c")).read_text()

    def test_generates_one_source_per_host_module(self):
        written = forwarders.generate(self.root / "out", self.source)
        names = sorted(path.name for path in written)
        self.assertEqual(
            names,
            [
                "Android.bp",
                "forward_libEGL.S",
                "forward_libEGL.c",
                "forward_libGLESv1_CM.S",
                "forward_libGLESv1_CM.c",
                "forward_libGLESv2.S",
                "forward_libGLESv2.c",
                "forward_libGLESv3.S",
                "forward_libGLESv3.c",
            ],
        )
        for path in written:
            if path.suffix == ".S":
                self.assertIn("jmp *", path.read_text())
        self.assertIn("libEGL", (self.root / "out" / "Android.bp").read_text())

    def test_rejects_a_stub_library_that_declares_variables(self):
        # The proxy resolves data symbols through the same dlsym path, but a
        # forwarder cannot alias another module's storage, so failing loudly is
        # better than emitting a table entry that points at nothing.
        (self.stubs / "libGLESv3_stub.c").write_text(
            stub_source("glDispatchCompute", variables=("glVersion",))
        )
        with self.assertRaises(forwarders.ForwarderError) as caught:
            forwarders.generate(self.root / "out", self.source)
        self.assertIn("libGLESv3", str(caught.exception))


class GuestStubTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.stubs = load("build_guest_stubs", "build-guest-stubs.py")

    def test_covers_every_bionic_library_the_guest_needs(self):
        # libandroid is not a GL library but shares the stub mechanism: the asset
        # manager crash of run 11 was fixed by routing it through the bridge too.
        self.assertEqual(
            self.stubs.STUB_LIBRARIES,
            ("libEGL", "libGLESv1_CM", "libGLESv2", "libGLESv3", "libandroid"),
        )

    def test_every_stub_library_has_a_declared_source(self):
        for library in self.stubs.STUB_LIBRARIES:
            source = self.stubs.stub_source(library)
            # The export floor is the number of interceptable stubs in the
            # translation unit, so a truncated or renamed source cannot pass.
            self.assertGreater(self.stubs.declared_symbols(source), 0, library)

    def test_stub_source_lives_in_the_library_directory(self):
        source = self.stubs.stub_source("libGLESv2")
        self.assertEqual(source.name, "stubs_arm64.cc")
        self.assertEqual(source.parent.name, "libGLESv2")


if __name__ == "__main__":
    unittest.main()
