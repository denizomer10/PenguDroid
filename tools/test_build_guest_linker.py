#!/usr/bin/env python3
"""Offline tests for the guest bootstrap modules, closure, and safe bundle layout."""

import contextlib
import copy
import importlib.util
import io
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "build_guest_linker", Path(__file__).with_name("build-guest-linker.py"))
assert spec is not None and spec.loader is not None
build = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = build
spec.loader.exec_module(build)


def elf_header(machine=183, elf_type=3):
    return (b"\x7fELF\x02\x01" + bytes(10) + elf_type.to_bytes(2, "little")
            + machine.to_bytes(2, "little"))


class BootstrapTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="guest-linker-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name) / "build"
        self.root.mkdir()
        patches = {
            "ROOT": self.root, "BUNDLE": self.root / "bundle",
            "LINKER_ARTIFACT": self.root / "out/native_bridge_guest_linker.native_bridge",
            "LIBC_ARTIFACT": self.root / "out/libnative_bridge_guest_libc.native_bridge",
            "APP_PROCESS": self.root / "out/native_bridge_guest_app_process.native_bridge",
            "VDSO_FILE": self.root / "inputs/libnative_bridge_vdso.so",
            "LD_CONFIG": self.root / "inputs/ld.config.arm64.txt",
        }
        for name, value in patches.items():
            patcher = mock.patch.object(build, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.artifacts = build.bootstrap_artifacts()
        for label, path in self.artifacts.items():
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(elf_header() + label.encode())
        build.LD_CONFIG.write_text("namespace.default.search.paths = /system/lib64/arm64\n")
        sonames = {"linker": "ld-android.so", "vdso": "linux-vdso.so.1",
                   "libc": "libc.so", "libdl": "libdl.so", "libm": "libm.so"}
        exports = {"linker": build.REQUIRED_EXPORTS,
                   "vdso": ("native_bridge_post_init", "native_bridge_intercept_symbol"),
                   "libc": build.REQUIRED_LIBC_EXPORTS,
                   "libdl": ("dlopen", "dlsym"), "libm": build.REQUIRED_LIBM_EXPORTS,
                   "app_process64": ()}
        needed = {"linker": [], "vdso": [], "libm": ["libc.so"],
                  "libdl": ["ld-android.so"],
                  "libc": ["linux-vdso.so.1", "ld-android.so", "libdl.so"],
                  "app_process64": ["linux-vdso.so.1", "libc.so"]}
        imports = {"app_process64": ["native_bridge_post_init", "__libc_init"],
                   "libc": ["native_bridge_intercept_symbol"],
                   "libdl": ["__loader_dlopen"]}
        self.evidence = {
            label: {"soname": [sonames[label]] if label in sonames else [],
                    "interpreter": [build.GUEST_INTERPRETER] if label == "app_process64" else [],
                    "entry": 0x4000 if label == "app_process64" else 0,
                    "pie": label == "app_process64", "needed": needed[label],
                    "exports": list(exports[label]), "imports": imports.get(label, []),
                    "required_missing": [], "path": str(path)}
            for label, path in self.artifacts.items()}
        self.by_path = {path: label for label, path in self.artifacts.items()}
        inspector = mock.patch.object(
            build, "inspect_runtime", side_effect=lambda path: copy.deepcopy(
                self.evidence[self.by_path[Path(path)]]))
        self.inspect = inspector.start()
        self.addCleanup(inspector.stop)

    def test_complete_closure_resolves_resident_sonames_without_duplicate_files(self):
        closure = build.validate_bootstrap_closure(self.artifacts)
        self.assertTrue(closure["validated"])
        self.assertEqual(closure["resident_sonames"], {
            "ld-android.so": "system/bin/arm64/linker64",
            "linux-vdso.so.1": "system/lib64/arm64/libnative_bridge_vdso.so"})
        self.assertEqual(closure["needed_resolution"]["libm"], {
            "libc.so": "system/lib64/arm64/libc.so"})

    def test_missing_artifact_is_not_accepted(self):
        self.artifacts["libm"].unlink()
        with self.assertRaisesRegex(build.BuildError, "missing libm"):
            build.validate_bootstrap_closure(self.artifacts)

    def test_transitive_missing_dependency_is_not_an_ndk_fallback(self):
        self.evidence["libdl"]["needed"].append("libc++_shared.so")
        with self.assertRaisesRegex(build.BuildError, "missing DT_NEEDED libc\\+\\+_shared.so"):
            build.validate_bootstrap_closure(self.artifacts)

    def test_wrong_soname_and_library_interpreter_are_rejected(self):
        for key, value in (("soname", ["libc.so"]), ("interpreter", ["/host/linker"])):
            with self.subTest(key=key):
                old = self.evidence["libdl"][key]
                self.evidence["libdl"][key] = value
                with self.assertRaisesRegex(build.BuildError, "SONAME/PT_INTERP"):
                    build.validate_bootstrap_closure(self.artifacts)
                self.evidence["libdl"][key] = old

    def test_resident_objects_may_not_gain_dependencies(self):
        self.evidence["vdso"]["needed"] = ["libc.so"]
        with self.assertRaisesRegex(build.BuildError, "preloaded vDSO"):
            build.validate_bootstrap_closure(self.artifacts)

    def test_app_process_uses_canonical_not_installed_interpreter(self):
        self.evidence["app_process64"]["interpreter"] = ["/system/bin/arm64/linker64"]
        with self.assertRaisesRegex(build.BuildError, "PT_INTERP /system/bin/linker64"):
            build.validate_bootstrap_closure(self.artifacts)

    def test_app_process_requires_pie_entry_and_post_init_import(self):
        for key, value in (("pie", False), ("entry", 0), ("imports", ["__libc_init"])):
            with self.subTest(key=key):
                artifact = copy.deepcopy(self.evidence["app_process64"])
                artifact[key] = value
                with self.assertRaises(build.BuildError):
                    build.validate_runtime_module("app_process", artifact)

    def test_app_process_drops_unused_libm_and_libdl(self):
        for dependency in ("libm.so", "libdl.so", "libandroid_runtime.so"):
            with self.subTest(dependency=dependency):
                artifact = copy.deepcopy(self.evidence["app_process64"])
                artifact["needed"].append(dependency)
                with self.assertRaisesRegex(build.BuildError, "unused runtime libraries"):
                    build.validate_runtime_module("app_process", artifact)

    def test_unresolved_strong_import_fails_even_when_all_needed_exist(self):
        self.evidence["libc"]["imports"].append("missing_native_bridge_hook")
        with self.assertRaisesRegex(build.BuildError, "unresolved bootstrap imports in libc"):
            build.validate_bootstrap_closure(self.artifacts)

    def test_unrelated_bundled_libm_cannot_mask_a_missing_app_import(self):
        self.evidence["app_process64"]["imports"].append("cos")
        with self.assertRaisesRegex(build.BuildError, "unresolved bootstrap imports in app_process64"):
            build.validate_bootstrap_closure(self.artifacts)

    def test_libm_requires_real_math_exports(self):
        self.evidence["libm"]["exports"].remove("sqrt")
        with self.assertRaisesRegex(build.BuildError, "missing required exports.*sqrt"):
            build.validate_bootstrap_closure(self.artifacts)

    def test_bundle_layout_hashes_permissions_and_idempotence(self):
        result = build.assemble_bundle()
        expected = set(build.BOOTSTRAP_PATHS.values()) | {"system/etc/ld.config.arm64.txt"}
        self.assertEqual(set(result["files"]), expected)
        self.assertTrue(result["manifest_free"])
        for relative, details in result["files"].items():
            path = build.BUNDLE / relative
            mode = 0o755 if relative.startswith("system/bin/") else 0o644
            self.assertEqual(path.stat().st_mode & 0o777, mode)
            self.assertEqual(details["sha256"], build.sha256(path))
        self.assertFalse((build.BUNDLE / "system/lib64/arm64/ld-android.so").exists())
        self.assertFalse((build.BUNDLE / "system/lib64/arm64/linux-vdso.so.1").exists())
        # Equal content must not leave old non-executable permissions intact.
        (build.BUNDLE / build.BOOTSTRAP_PATHS["app_process64"]).chmod(0o644)
        self.assertEqual(build.assemble_bundle()["files"], result["files"])

    def test_invalid_closure_does_not_touch_existing_bundle(self):
        result = build.assemble_bundle()
        self.evidence["libdl"]["needed"].append("absent.so")
        with self.assertRaises(build.BuildError):
            build.assemble_bundle()
        for relative, details in result["files"].items():
            self.assertEqual(build.sha256(build.BUNDLE / relative), details["sha256"])

    def test_unknown_bundle_files_are_preserved_and_refused(self):
        build.BUNDLE.mkdir()
        unknown = build.BUNDLE / "keep-me"
        unknown.write_text("user work")
        with self.assertRaisesRegex(build.BuildError, "not removed"):
            build.assemble_bundle()
        self.assertEqual(unknown.read_text(), "user work")
        self.assertFalse((build.BUNDLE / "system").exists())

    def test_symlink_bundle_directory_and_leaf_are_refused(self):
        outside = self.root / "outside"
        outside.mkdir()
        victim = outside / "victim"
        victim.write_text("unchanged")
        build.BUNDLE.symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(build.BuildError, "unsafe output"):
            build.assemble_bundle()
        build.BUNDLE.unlink()
        destination = build.BUNDLE / build.BOOTSTRAP_PATHS["app_process64"]
        destination.parent.mkdir(parents=True)
        destination.symlink_to(victim)
        with self.assertRaisesRegex(build.BuildError, "unsafe output"):
            build.assemble_bundle()
        self.assertEqual(victim.read_text(), "unchanged")

    def test_missing_config_does_not_create_bundle(self):
        build.LD_CONFIG.unlink()
        with self.assertRaisesRegex(build.BuildError, "missing ld_config"):
            build.assemble_bundle()
        self.assertFalse(build.BUNDLE.exists())

    def test_output_paths_must_stay_in_owned_root(self):
        with self.assertRaisesRegex(build.BuildError, "owned build root"):
            build.check_output_path(self.root.parent / "outside")
        with self.assertRaises(build.BuildError):
            build.check_output_path(self.root / ".." / "outside")

    def test_failed_link_or_acceptance_preserves_existing_artifact(self):
        original = build.APP_PROCESS.read_bytes()
        runner = mock.Mock(report={})
        native = build.GuestBuild(mock.Mock(), runner)
        for link_error in (True, False):
            with self.subTest(link_error=link_error), \
                    mock.patch.object(build, "inspect_runtime", side_effect=build.BuildError("bad ELF")):
                runner.run.side_effect = build.BuildError("link failed") if link_error else None
                with self.assertRaises(build.BuildError):
                    native.link_bootstrap("app_process64", ["ld.lld"], build.APP_PROCESS, [], [])
                self.assertEqual(build.APP_PROCESS.read_bytes(), original)
                self.assertEqual(list(build.APP_PROCESS.parent.glob(build.APP_PROCESS.name + ".*")), [])

    def test_new_modules_use_built_guest_inputs_not_ndk_dsos(self):
        native = build.GuestBuild(mock.Mock(), mock.Mock())
        with mock.patch.object(native, "build", side_effect=AssertionError("no libc rebuild")):
            paths = native.bootstrap_shared_inputs({"shared_libs": ["libnative_bridge_vdso"]})
            self.assertEqual(paths, list(map(str, [build.VDSO_FILE, build.LIBC_ARTIFACT,
                                                  self.root / "lib/libm.so", self.root / "lib/libdl.so"])))
            self.assertEqual(native.bootstrap_shared_inputs({"system_shared_libs": []}), [])
            build.LIBC_ARTIFACT.unlink()
            with self.assertRaisesRegex(build.BuildError, "missing built guest libc"):
                native.bootstrap_shared_inputs({})

    def test_existing_libc_option_still_builds_libc_and_bundles(self):
        native = mock.Mock()
        native.generate.return_value = {}
        outputs = {"libdl": self.artifacts["libdl"],
                   "libnative_bridge_guest_libc": self.artifacts["libc"],
                   "libm": self.artifacts["libm"],
                   "native_bridge_guest_app_process": self.artifacts["app_process64"]}
        native.build.side_effect = lambda name, *args: {
            "output": str(outputs[name]), "soname": "libdl.so" if name == "libdl" else "libc.so",
            "bytes": outputs[name].stat().st_size, "sha256": build.sha256(outputs[name])}
        with mock.patch.object(build, "prepare_root"), mock.patch.object(build, "Blueprints"), \
                mock.patch.object(build, "GuestBuild", return_value=native), \
                mock.patch.object(build, "Runner"), \
                mock.patch.object(build, "inspect_libc", return_value={"required_missing": [], "checks": {"ok": True}}), \
                mock.patch.object(sys, "argv", ["build-guest-linker.py", "--module", "libnative_bridge_guest_libc"]), \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(build.main(), 0)
        self.assertEqual(native.build.call_args_list, [mock.call("libdl", "shared"),
                         mock.call("libnative_bridge_guest_libc", "shared"),
                         mock.call("libm", "shared"), mock.call("native_bridge_guest_app_process")])
        native.generate.assert_called_once()
        self.assertTrue((build.BUNDLE / build.BOOTSTRAP_PATHS["libm"]).is_file())

    def test_verify_module_does_not_generate_or_build(self):
        native = mock.Mock()
        with mock.patch.object(build, "prepare_root"), mock.patch.object(build, "Blueprints"), \
                mock.patch.object(build, "GuestBuild", return_value=native), \
                mock.patch.object(build, "Runner"), \
                mock.patch.object(sys, "argv", ["build-guest-linker.py", "--module", "app_process",
                                                "--stage", "verify", "--bundle"]), \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(build.main(), 0)
        native.generate.assert_not_called()
        native.build.assert_not_called()
        report = json.loads((self.root / "report.json").read_text())
        self.assertTrue(report["verified"])
        self.assertTrue(report["bundle"]["closure"]["validated"])
        self.assertTrue((Path(report["run"]) / "report.json").is_file())


class ModuleTests(unittest.TestCase):
    def test_existing_and_new_cli_options(self):
        args = build.parse_args([])
        self.assertIsNone(args.module)
        self.assertEqual(args.stage, "all")
        self.assertFalse(args.bundle)
        self.assertEqual(build.parse_args(["--module", "libnative_bridge_guest_libc"]).module,
                         "libnative_bridge_guest_libc")
        for module in ("app_process", "libm"):
            args = build.parse_args(["--module", module, "--stage", "verify", "--bundle", "--seconds", "60"])
            self.assertEqual(args.module, module)
            self.assertEqual(args.seconds, 60)
            self.assertTrue(args.bundle)

    def test_invalid_cli_combinations_fail_before_building(self):
        for argv in (["--bundle"], ["--module", "app_process", "--stage", "rust"],
                     ["--module", "libm", "--stage", "compile"], ["--seconds", "59"],
                     ["--module", "framework_app_process"]):
            with self.subTest(argv=argv), contextlib.redirect_stderr(io.StringIO()), \
                    self.assertRaises(SystemExit):
                build.parse_args(argv)

    def test_new_app_source_is_indexed_without_framework_sources(self):
        self.assertIn(build.NBS / "android_api/app_process/Android.bp", build.BP_FILES)
        self.assertFalse(any("frameworks/base" in str(path) for path in build.BP_FILES))

    def test_cpp_compilation_has_no_exceptions_or_rtti(self):
        native = build.GuestBuild(mock.Mock(), mock.Mock())
        with mock.patch.object(native, "include_flags", return_value=[]):
            argv = native.compile_command("native_bridge_guest_app_process", {},
                                          Path("app_process.cc"), Path("app_process.o"))
        for flag in (build.TARGET, "-fPIC", "-fno-exceptions", "-fno-rtti"):
            self.assertIn(flag, argv)

    def test_app_link_uses_arm64_crt_canonical_interpreter_and_as_needed(self):
        model = mock.Mock()
        model.kind.return_value = "cc_binary"
        native = build.GuestBuild(model, mock.Mock())
        source = build.NBS / "android_api/app_process/app_process.cc"
        with mock.patch.object(native, "_sources", return_value=[source]), \
                mock.patch.object(native, "object_for", return_value=Path("app.o")), \
                mock.patch.object(native, "bootstrap_crt", return_value=(Path("begin.o"), Path("end.o"))) as crt, \
                mock.patch.object(native, "bootstrap_shared_inputs", return_value=["vdso.so", "libc.so", "libm.so"]), \
                mock.patch.object(native, "link_bootstrap", return_value={}) as link:
            native._build_app_process("native_bridge_guest_app_process", {"enabled": True})
        crt.assert_called_once_with("crtbegin_dynamic", "crtend_android")
        argv = link.call_args.args[1]
        self.assertIn("-pie", argv)
        self.assertIn("--entry=_start", argv)
        self.assertIn("--dynamic-linker=/system/bin/linker64", argv)
        self.assertLess(argv.index(Path("begin.o")), argv.index(Path("app.o")))
        self.assertLess(argv.index("--as-needed"), argv.index("vdso.so"))
        self.assertLess(argv.index("libm.so"), argv.index(Path("end.o")))
        self.assertNotIn("--no-as-needed", argv)
        self.assertEqual(link.call_args.args[2], build.APP_PROCESS)

    def test_libm_reuses_static_objects_only_when_source_rules_match(self):
        model = mock.Mock()
        model.props.return_value = {"cflags": ["-same"]}
        model.canonical.return_value = "libm.arm64.map"
        native = build.GuestBuild(model, mock.Mock())
        props = {"cflags": ["-same"], "version_script": ":libm.arm64.map"}

        with mock.patch.object(native, "_sources", return_value=[Path("math.c")]), \
                mock.patch.object(native, "compile_command", side_effect=lambda name, props, *rest: props["cflags"]), \
                mock.patch.object(native, "bootstrap_static_inputs", return_value=["libm.a", "optimized.a"]) as archives, \
                mock.patch.object(native, "bootstrap_crt", return_value=("begin.o", "end.o")), \
                mock.patch.object(native, "bootstrap_shared_inputs", return_value=["libc.so"]), \
                mock.patch.dict(build.VERSION_SCRIPTS, {"libm.arm64.map": Path("libm.map")}), \
                mock.patch.object(build, "version_script_symbols", return_value=["cos", "sqrt"]), \
                mock.patch.object(native, "link_bootstrap", return_value={}) as link:
            result = native._build_libm_shared(props)
            self.assertTrue(result["reused_static_variant"])
            archives.assert_called_once_with("libm", {"cflags": ["-same"]})
            argv = link.call_args.args[1]
            self.assertIn("--whole-archive", argv)
            self.assertEqual(argv.count("libm.a"), 1)
            self.assertEqual(argv.count("optimized.a"), 1)
            self.assertIn("--version-script=libm.map", argv)
            self.assertIn("--as-needed", argv)
            archives.reset_mock()
            with self.assertRaisesRegex(build.BuildError, "shared compile flags differ"):
                native._build_libm_shared({**props, "cflags": ["-different"]})
            archives.assert_not_called()

    def test_archive_reuse_requires_exact_nonduplicate_member_contents(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "lib").mkdir()
            archive = root / "lib/libm.a"
            archive.write_bytes(b"fixture archive")
            obj = root / "math.o"
            obj.write_bytes(elf_header(elf_type=1))
            native = build.GuestBuild(mock.Mock(), mock.Mock(report={}))
            native.root = root
            for members, contents, reused in (("math.o\n", obj.read_bytes(), True),
                                              ("math.o\nmath.o\n", b"", False),
                                              ("math.o\n", b"stale", False)):
                with self.subTest(members=members, contents=contents), \
                        mock.patch.object(native, "_sources", return_value=[Path("math.c")]), \
                        mock.patch.object(native, "object_for", return_value=obj), \
                        mock.patch.object(build.subprocess, "run", side_effect=[
                            subprocess.CompletedProcess([], 0, members),
                            subprocess.CompletedProcess([], 0, contents)]):
                    self.assertEqual(native.bootstrap_static_inputs("libm", {}),
                                     [archive] if reused else [obj])
                    self.assertEqual(native.runner.report["bootstrap_archive_reuse"]["libm"]["reused"], reused)

    def test_bootstrap_crt_refuses_wrong_architecture(self):
        with tempfile.TemporaryDirectory() as directory:
            crt = Path(directory)
            (crt / "crtbegin_dynamic.o").write_bytes(elf_header(machine=62, elf_type=1))
            (crt / "crtend_android.o").write_bytes(elf_header(elf_type=1))
            with mock.patch.object(build, "CRT", crt), self.assertRaisesRegex(build.BuildError, "AArch64"):
                build.GuestBuild.bootstrap_crt("crtbegin_dynamic", "crtend_android")

    def test_runtime_inspection_rejects_wrong_arch_and_ignores_weak_imports(self):
        text = """
  Entry point address: 0x4500
      [Requesting program interpreter: /system/bin/linker64]
  0x0000000000000001 (NEEDED) Shared library: [libc.so]
  0x000000006ffffffb (FLAGS_1) PIE
   Num: Value Size Type Bind Vis Ndx Name
     0: 0 0 NOTYPE LOCAL DEFAULT UND
     1: 0 0 FUNC GLOBAL DEFAULT UND __libc_init@LIBC
     2: 0 0 FUNC WEAK DEFAULT UND optional_hook
     3: 0 4 FUNC GLOBAL DEFAULT 12 exported@@LIBC
"""
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "elf"
            path.write_bytes(elf_header())
            with mock.patch.object(build.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, text)):
                result = build.inspect_runtime(path)
                self.assertEqual(result["imports"], ["__libc_init"])
                self.assertEqual(result["exports"], ["exported"])
                self.assertEqual(result["entry"], 0x4500)
                self.assertTrue(result["pie"])
                self.assertEqual(result["interpreter"], [build.GUEST_INTERPRETER])
                path.write_bytes(elf_header(machine=62))
                with self.assertRaisesRegex(build.BuildError, "AArch64"):
                    build.inspect_runtime(path)


if __name__ == "__main__":
    unittest.main()
