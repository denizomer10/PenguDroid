#!/usr/bin/env python3
"""Offline tests for the restricted Blueprint reader and bounded native runner."""

import importlib.util
import json
import os
import resource
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("build_nativebridge", Path(__file__).with_name("build-nativebridge.py"))
assert spec is not None and spec.loader is not None
build = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = build
spec.loader.exec_module(build)


class BuildTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        build.prepare_root()

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="test-", dir=build.ROOT / "work")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def model(self, text):
        source = self.root / "source"
        source.mkdir()
        (source / "Android.bp").write_text(text)
        return build.Blueprints({"fixture": source})

    def test_defaults_variables_and_android_guest_arch_selection(self):
        model = self.model('''
            flags = ["-DBASE"]
            extra = flags + ["-DSECOND"]
            cc_defaults { name: "common", cflags: extra,
                arch: { x86_64: { cflags: ["-DGUEST_LP64"] },
                        arm64: { cflags: ["-DWRONG_HOST"] } }, }
            cc_library_static { name: "entry", defaults: ["common"], host_supported: true,
                target: { android: { srcs: ["NativeBridgeItf.cc"] },
                          linux_glibc: { srcs: ["host_only.cc"] } },
                arch: { x86_64: { cflags: ["-DNATIVE_BRIDGE_GUEST_ARCH_ARM64"] } }, }
        ''')
        props = model.props("entry")
        self.assertEqual(props["cflags"], ["-DBASE", "-DSECOND", "-DGUEST_LP64", "-DNATIVE_BRIDGE_GUEST_ARCH_ARM64"])
        self.assertEqual([Path(p).name for p in props["srcs"]], ["NativeBridgeItf.cc"])
        self.assertNotIn("host_only.cc", str(props))
        self.assertIn("common", model.used)
        self.assertEqual(len(model.used["entry"]["blueprint_sha256"]), 64)

    def test_filegroup_preserves_input_order(self):
        model = self.model('''
            filegroup { name: "inputs", srcs: ["b.json", "a.json"] }
            filegroup { name: "combined", srcs: [":inputs", "c.json"] }
        ''')
        for name in ("a.json", "b.json", "c.json"):
            (self.root / "source" / name).write_text("{}")
        result = model.files(model.props("combined")["srcs"])
        self.assertEqual([Path(p).name for p in result], ["b.json", "a.json", "c.json"])

    def test_unresolved_defaults_fail_with_dependency_chain(self):
        model = self.model('cc_library_static { name: "entry", defaults: ["missing"] }')
        with self.assertRaisesRegex(build.BuildError, "missing; required by entry"):
            model.props("entry")

    def test_default_cycles_fail_closed(self):
        model = self.model('''
            cc_defaults { name: "a", defaults: ["b"] }
            cc_defaults { name: "b", defaults: ["a"] }
        ''')
        with self.assertRaisesRegex(build.BuildError, "cyclic Blueprint defaults"):
            model.props("a")

    def test_unsupported_syntax_is_not_silently_discarded(self):
        with self.assertRaisesRegex(build.BuildError, "unsupported/unresolved Blueprint value"):
            build.Values(build.tokenize('{ srcs: select(foo(), {default: []}) }')).value()
        self.assertIn("(", build.tokenize('select(foo())'))
        with self.assertRaisesRegex(build.BuildError, "unsupported/unresolved"):
            build.Values(["unknown"]).value()

    def test_ndk_stub_declaration_is_not_a_platform_library(self):
        model = self.model('''cc_library {name: "liblog", export_include_dirs: ["include"]}
                              ndk_library {name: "liblog", symbol_file: "liblog.map.txt"}''')
        self.assertEqual(model.modules["liblog"][0], "cc_library")
        self.assertEqual(model.props("liblog")["export_include_dirs"], [str(self.root / "source/include")])

    def test_duplicate_modules_are_rejected(self):
        with self.assertRaisesRegex(build.BuildError, "ambiguous module"):
            self.model('cc_defaults {name:"a"} cc_defaults {name:"a"}')

    def test_disabled_android_variant_is_not_host_fallback(self):
        model = self.model('''cc_library_static { name: "disabled", host_supported: true,
                              target: { android: { enabled: false } } }''')
        with self.assertRaisesRegex(build.BuildError, "Android x86_64 variant disabled"):
            model.props("disabled")

    def test_namespaced_header_module_and_exports(self):
        model = self.model('''
            cc_library_headers { name: "underlying", export_include_dirs: ["include"] }
            cc_library_headers { name: "outer", header_libs: ["underlying"],
                                 export_header_lib_headers: ["underlying"] }
        ''')
        runner = mock.Mock(root=self.root)
        native = build.NativeBuild(model, runner, self.root)
        exported = native.exports("//fixture:outer")
        self.assertEqual(exported, [str(self.root / "source/include")])
        runner.run.assert_not_called()

    def test_python_generator_command_comes_from_blueprint(self):
        model = self.model('''
            python_binary_host { name: "generator", main: "generator.py", srcs: ["generator.py"] }
            genrule { name: "header", tools: ["generator"], out: ["sub/generated.h"],
                      cmd: "$(location generator) $(out) $(in)", srcs: ["input.json"] }
        ''')
        (self.root / "source/generator.py").write_text("import sys\nfrom pathlib import Path\nPath(sys.argv[1]).write_text(Path(sys.argv[2]).read_text())\n")
        (self.root / "source/input.json").write_text("// generated input\n")
        for directory in ("logs", "gen", "tmp"):
            (self.root / directory).mkdir()
        report = {"commands": []}
        runner = build.Runner(self.root, report, 20)
        native = build.NativeBuild(model, runner, self.root)
        generated = native.generate("header")
        self.assertEqual((generated / "sub/generated.h").read_text(), "// generated input\n")
        self.assertEqual(native.generate("header"), generated)
        self.assertEqual(len(report["commands"]), 1)
        self.assertIn("generator.py", " ".join(report["commands"][0]["argv"]))
        self.assertFalse(list((self.root / "source").glob("__pycache__")))

    def test_non_python_host_tool_not_omitted(self):
        model = self.model('cc_binary_host { name: "required_generator", srcs: ["generator.cc"] }')
        native = build.NativeBuild(model, mock.Mock(root=self.root), self.root)
        with self.assertRaisesRegex(build.BuildError, "unimplemented host codegen tool"):
            native.python_tool("required_generator")

    def test_runner_limits_and_failure_log(self):
        for name in ("logs", "tmp"):
            (self.root / name).mkdir()
        report = {"commands": []}
        runner = build.Runner(self.root, report, 20)
        command = [sys.executable, "-B", "-c", "import resource; print(resource.getrlimit(resource.RLIMIT_AS)); raise SystemExit(7)"]
        with self.assertRaisesRegex(build.BuildError, "failed \\(7\\)"):
            runner.run(command, "expected failure")
        record = report["commands"][0]
        self.assertEqual(record["returncode"], 7)
        self.assertIn(str(build.MEMORY), Path(record["log"]).read_text())
        self.assertEqual(record["argv"], command)
        self.assertEqual(resource.getrlimit(resource.RLIMIT_AS)[0], resource.RLIM_INFINITY)

    def test_wall_time_budget(self):
        runner = build.Runner(self.root, {"commands": []}, 1)
        runner.deadline = time.monotonic() - 1
        with self.assertRaisesRegex(build.BuildError, "wall-time budget"):
            runner.run([sys.executable, "-V"], "never start")

    def test_unowned_build_directory_is_not_replaced(self):
        root = self.root / "user"
        root.mkdir()
        (root / "mine").write_text("keep")
        with mock.patch.object(build, "ROOT", root), self.assertRaisesRegex(build.BuildError, "unowned"):
            build.prepare_root()
        self.assertEqual((root / "mine").read_text(), "keep")
        self.assertFalse((root / ".owner.json").exists())

    def test_pinned_soong_device_selector_order(self):
        model = self.model('''cc_library_static { name: "entry", cflags: ["base"],
            target: {
                linux: {cflags: ["linux"]}, bionic: {cflags: ["bionic"]},
                android: {cflags: ["android"]}, android64: {cflags: ["device64"]},
                host: {cflags: ["WRONG"]}, not_windows: {cflags: ["WRONG"]},
                linux_glibc: {cflags: ["WRONG"]}, linux_bionic: {cflags: ["WRONG"]},
                linux_x86_64: {cflags: ["linux_arch"]},
                bionic_x86_64: {cflags: ["bionic_arch"]},
                android_x86_64: {cflags: ["android_arch"]},
            }, arch: {x86_64: {cflags: ["arch"]}},
            multilib: {lib64: {cflags: ["lib64"]}, lib32: {cflags: ["WRONG"]}},
        }''')
        self.assertEqual(model.props("entry")["cflags"],
                         ["base", "linux", "bionic", "android", "device64", "arch", "lib64",
                          "linux_arch", "bionic_arch", "android_arch"])

    def test_defaults_are_merged_before_variant_scalar_overrides(self):
        model = self.model('''
            cc_defaults {name: "d", enabled: false,
                         arch: {x86_64: {cpp_std: "gnu++23"}}}
            cc_library_static {name: "entry", defaults: ["d"], enabled: true, cpp_std: "gnu++17"}
        ''')
        self.assertEqual(model.props("entry")["cpp_std"], "gnu++23")
        self.assertTrue(model.props("entry")["enabled"])

    def test_inactive_host_include_dirs_are_not_resolved(self):
        model = self.model('''cc_library_static { name: "entry",
            target: {linux_glibc: {include_dirs: ["not/downloaded/host"]}} }''')
        self.assertNotIn("include_dirs", model.props("entry"))

    def test_explicit_static_and_shared_header_variants(self):
        model = self.model('''cc_library {name: "entry", export_include_dirs: ["common"],
            static: {export_include_dirs: ["static"], cflags: ["-DSTATIC"]},
            shared: {export_include_dirs: ["shared"], cflags: ["-DSHARED"]}}''')
        with self.assertRaisesRegex(build.BuildError, "explicit static/shared"):
            model.props("entry")
        for linkage in ("static", "shared"):
            props = model.props("entry", linkage=linkage)
            self.assertEqual([Path(p).name for p in props["export_include_dirs"]], ["common", linkage])
            self.assertEqual(props["cflags"], ["-D" + linkage.upper()])

    def test_32_bit_only_module_fails_instead_of_falling_back(self):
        model = self.model('cc_library_static {name:"entry", compile_multilib:"32"}')
        with self.assertRaisesRegex(build.BuildError, "not an x86_64 module"):
            model.props("entry")

    def test_unknown_property_and_arch_features_fail_closed(self):
        model = self.model('''cc_library_static {name:"unknown", imaginary_property:true}
            cc_library_static {name:"feature", arch:{x86_64:{avx2:{cflags:["-mavx2"]}}}}''')
        for name, field in (("unknown", "imaginary_property"), ("feature", "avx2")):
            with self.subTest(name=name), self.assertRaisesRegex(build.BuildError, field):
                model.props(name)

    def test_sanitizer_properties_are_explicitly_inert_not_silently_applied(self):
        model = self.model('''cc_library_static {name:"entry", export_include_dirs:["include"],
                              sanitize:{misc_undefined:["integer"]}}''')
        self.assertIn("sanitize", build.METADATA)
        native = build.NativeBuild(model, mock.Mock(root=self.root), self.root)
        self.assertEqual(native.exports("entry"), [str(self.root / "source/include")])
        props = model.props("entry")
        argv = native.compile_command(props, "source.cc", self.root / "out.o", [])
        self.assertFalse(any(argument.startswith("-fsanitize") for argument in argv))

    def test_unknown_variant_selectors_fail_closed(self):
        model = self.model('''cc_library_static {name:"target_typo",target:{android_x86_65:{cflags:["WRONG"]}}}
            cc_library_static {name:"arch_typo",arch:{x86_65:{cflags:["WRONG"]}}}
            cc_library_static {name:"multilib_typo",multilib:{lib128:{cflags:["WRONG"]}}}''')
        for name in ("target_typo", "arch_typo", "multilib_typo"):
            with self.subTest(name=name), self.assertRaisesRegex(build.BuildError, "unsupported.*selectors"):
                model.props(name)

    def test_sdk_variant_is_not_silently_compiled_as_platform(self):
        model = self.model('cc_library_static {name:"entry",sdk_version:"29"}')
        runner = mock.Mock(root=self.root)
        with self.assertRaisesRegex(build.BuildError, "not the API37 platform variant"):
            build.NativeBuild(model, runner, self.root).build("entry")
        runner.run.assert_not_called()

    def test_product_variables_affecting_headers_cannot_be_ignored(self):
        model = self.model('''
            cc_library_headers {name:"compile_flags", product_variables:{debuggable:{cflags:["-DDEBUG"]}}}
            cc_library_headers {name:"header_paths",
                                product_variables:{debuggable:{export_include_dirs:["debug"]}}}
            cc_library_headers {name:"config", soong_config_variables:{translation_arch:{conditions_default:{}}}}
        ''')
        self.assertIn("product_variables", model.props("compile_flags", headers_only=True))
        for name in ("header_paths", "config"):
            with self.subTest(name=name), self.assertRaisesRegex(build.BuildError, "unsupported properties"):
                model.props(name, headers_only=True)
        with self.assertRaisesRegex(build.BuildError, "product_variables"):
            model.props("compile_flags")

    def test_filegroup_cycles_and_exclusions(self):
        model = self.model('''filegroup {name:"cycle",srcs:[":cycle"]}
            filegroup {name:"selected",srcs:["a.cc","b.cc"],exclude_srcs:["a.cc"]}''')
        for name in ("a.cc", "b.cc"):
            (self.root / "source" / name).touch()
        with self.assertRaisesRegex(build.BuildError, "cyclic filegroup"):
            model.files([":cycle"])
        self.assertEqual([Path(p).name for p in model.files([":selected"])], ["b.cc"])

    def test_undeclared_header_reexports_fail_closed(self):
        model = self.model('''cc_library_headers {name:"entry",export_header_lib_headers:["dep"]}
                              cc_library_headers {name:"dep",export_include_dirs:["include"]}''')
        native = build.NativeBuild(model, mock.Mock(root=self.root), self.root)
        with self.assertRaisesRegex(build.BuildError, "undeclared reexport"):
            native.exports("entry")

    def test_header_reexport_cycles_fail_closed(self):
        model = self.model('''cc_library_headers {name:"a",header_libs:["b"],export_header_lib_headers:["b"]}
                              cc_library_headers {name:"b",header_libs:["a"],export_header_lib_headers:["a"]}''')
        with self.assertRaisesRegex(build.BuildError, "cyclic header reexport"):
            build.NativeBuild(model, mock.Mock(root=self.root), self.root).exports("a")

    def test_whole_static_header_reexports_are_valid(self):
        model = self.model('''cc_library_static {name:"entry",whole_static_libs:["dep"],
                                                export_static_lib_headers:["dep"]}
                              cc_library_static {name:"dep",export_include_dirs:["include"]}''')
        native = build.NativeBuild(model, mock.Mock(root=self.root), self.root)
        self.assertEqual(native.exports("entry"), [str(self.root / "source/include")])

    def test_wrong_namespace_and_output_path_names_fail_closed(self):
        model = self.model('cc_library_headers {name:"entry"}')
        for name in ("//missing:entry", "../escape", "/tmp/entry"):
            with self.subTest(name=name), self.assertRaises(build.BuildError):
                model.canonical(name)

    def test_duplicate_properties_truncation_and_missing_commas(self):
        for text in ('{name:"a",name:"b"}', '{srcs:["a"', '{srcs:["a" "b"]}', '{name:"a" enabled:true}'):
            with self.subTest(text=text), self.assertRaises(build.BuildError):
                build.Values(build.tokenize(text)).value()

    def test_generator_rejects_shell_and_undeclared_programs(self):
        model = self.model('''
            python_binary_host {name:"generator",main:"generator.py",srcs:["generator.py"]}
            genrule {name:"shell",tools:["generator"],out:["out.h"],
                     cmd:"$(location generator) $(out); touch forbidden"}
            genrule {name:"unlisted",out:["out.h"],cmd:"touch $(out)"}
            genrule {name:"escape",tools:["generator"],out:["../escape.h"],
                     cmd:"$(location generator) $(out)"}
        ''')
        (self.root / "source/generator.py").write_text("pass\n")
        runner = mock.Mock(root=self.root)
        native = build.NativeBuild(model, runner, self.root)
        for name, error in (("shell", "shell genrule"), ("unlisted", "declared tool"), ("escape", "unsafe generated")):
            with self.subTest(name=name), self.assertRaisesRegex(build.BuildError, error):
                native.generate(name)
        runner.run.assert_not_called()

    def test_runner_removes_host_compiler_and_python_injection(self):
        with mock.patch.dict(os.environ, {"CPATH":"/host", "CPLUS_INCLUDE_PATH":"/host-cpp",
                                         "LIBRARY_PATH":"/host-lib", "PYTHONPATH":"/host-python",
                                         "CCC_OVERRIDE_OPTIONS":"-stdlib=libstdc++", "LD_PRELOAD":"/host.so"}):
            env = build.Runner(self.root, {"commands":[]}, 10).env
        for key in ("CPATH", "CPLUS_INCLUDE_PATH", "LIBRARY_PATH", "PYTHONPATH", "CCC_OVERRIDE_OPTIONS", "LD_PRELOAD"):
            self.assertNotIn(key, env)
        self.assertEqual(env["PYTHONDONTWRITEBYTECODE"], "1")
        self.assertEqual(env["TMPDIR"], str(self.root / "tmp"))

    def test_runner_kills_timed_out_command(self):
        for name in ("logs", "tmp"):
            (self.root / name).mkdir()
        report = {"commands": []}
        runner = build.Runner(self.root, report, 10)
        runner.deadline = time.monotonic() + 0.05
        with self.assertRaisesRegex(build.BuildError, "timed out"):
            runner.run([sys.executable, "-B", "-c", "import time; time.sleep(5)"], "bounded sleeper")
        self.assertTrue(report["commands"][0]["timed_out"])
        self.assertLess(report["commands"][0]["elapsed_seconds"], 2)

    def test_c_cpp_and_assembly_flags_do_not_leak_between_languages(self):
        native = build.NativeBuild(None, mock.Mock(root=self.root), self.root / "sysroot")
        props = {"cflags":["-DMODULE"], "conlyflags":["-DC_ONLY"],
                 "cppflags":["-DCPP_ONLY"], "asflags":["-DASM_ONLY"]}
        commands = {suffix: native.compile_command(props, "source" + suffix, self.root / "out.o", [])
                    for suffix in (".c", ".cc", ".S")}
        for suffix, argv in commands.items():
            self.assertIn("--target=x86_64-linux-android37", argv)
            self.assertIn("--no-default-config", argv)
            self.assertIn("-DANDROID", argv)
            self.assertIn("-DMODULE", argv)
            self.assertFalse(any("NATIVE_BRIDGE_GUEST_ARCH" in flag for flag in argv))
            self.assertEqual(Path(argv[0]).name, "clang++" if suffix == ".cc" else "clang")
        self.assertIn("-std=gnu17", commands[".c"])
        self.assertIn("-std=gnu++23", commands[".cc"])
        self.assertIn("-nostdinc++", commands[".cc"])
        self.assertNotIn("-nostdinc++", commands[".c"])
        self.assertFalse(any(flag.startswith("-std=") for flag in commands[".S"]))
        for suffix, flag in ((".c","-DC_ONLY"), (".cc","-DCPP_ONLY"), (".S","-DASM_ONLY")):
            for other, argv in commands.items():
                self.assertEqual(flag in argv, suffix == other)
        cpp = commands[".cc"]
        platform = str(build.DEPS / "toolchain/libcxx/android/x86_64/include/c++/v1")
        common = str(build.DEPS / "toolchain/libcxx/include/c++/v1")
        self.assertLess(cpp.index(platform), cpp.index(common))
        with self.assertRaisesRegex(build.BuildError, "unsupported source language"):
            native.compile_command({}, "source.mm", self.root / "out.o", [])

    def test_nondefault_stl_and_system_library_replacement_fail_closed(self):
        model = self.model('''cc_library_static {name:"ndk",stl:"c++_shared"}
                              cc_library_shared {name:"no_system",system_shared_libs:[]}''')
        runner = mock.Mock(root=self.root)
        native = build.NativeBuild(model, runner, self.root)
        for name, error in (("ndk", "non-default STL"), ("no_system", "non-default system libraries")):
            with self.subTest(name=name), self.assertRaisesRegex(build.BuildError, error):
                native.build(name)
        runner.run.assert_not_called()

    def test_missing_link_dependencies_are_not_stubbed(self):
        model = self.model('cc_library_shared {name:"entry",static_libs:["missing_dep"]}')
        runner = mock.Mock(root=self.root)
        native = build.NativeBuild(model, runner, self.root)
        with self.assertRaisesRegex(build.BuildError, "unresolved Blueprint module missing_dep"):
            native.build("entry")
        runner.run.assert_not_called()

    def test_extra_downloads_reuse_legacy_cache_and_preserve_previous_records(self):
        legacy = {"name":"libnativehelper", "download_sha256":"retained"}
        (self.root / "extra-manifest.json").write_text(json.dumps(legacy))
        (self.root / "extras-manifest.json").write_text(json.dumps({"keep":{"name":"keep"}}))
        downloader = mock.Mock(received=0)
        calls = []
        def fetch(root, artifact, transport, previous):
            self.assertEqual(root, self.root)
            self.assertIs(transport, downloader)
            calls.append((artifact.name, previous))
            return previous or {"name":artifact.name}
        with mock.patch.object(build, "ROOT", self.root), mock.patch.object(build.BOOT, "Downloader", return_value=downloader) as factory, mock.patch.object(build.BOOT, "fetch_artifact", side_effect=fetch):
            records, received = build.fetch_extras(offline=True)
            factory.assert_called_once_with(offline=True)
            self.assertEqual(calls[0], ("libnativehelper", legacy))
            calls.clear()
            build.fetch_extras(offline=True)
        self.assertEqual(received, 0)
        self.assertIn("keep", records)
        self.assertTrue(all(previous is not None for _, previous in calls))
        self.assertEqual({name for name, _ in calls}, {a.name for a in build.EXTRAS})
        self.assertFalse((self.root / "extras-manifest.tmp").exists())

    def test_elf_validation_rejects_other_architectures_classes_and_types(self):
        elf = bytearray(20)
        elf[:6] = b"\x7fELF\x02\x01"
        elf[16:18] = (1).to_bytes(2, "little")
        elf[18:20] = (62).to_bytes(2, "little")
        path = self.root / "fixture.o"
        path.write_bytes(elf)
        build.verify_elf(path, 1)
        with self.assertRaises(build.BuildError):
            build.verify_elf(path, 3)
        for index, value in ((4, 1), (5, 2), (18, 183)):
            invalid = bytearray(elf)
            invalid[index] = value
            path.write_bytes(invalid)
            with self.subTest(index=index), self.assertRaisesRegex(build.BuildError, "ELF64 x86_64"):
                build.verify_elf(path, 1)

    def platform_fixture(self):
        root = self.root / "platform"
        (root / "system/lib64").mkdir(parents=True)
        (root / "system/build.prop").write_text("ro.build.id=" + build.GSI_BUILD + "\nro.build.version.sdk=37\nro.build.version.release=17\n")
        elf = bytearray(20)
        elf[:6] = b"\x7fELF\x02\x01"
        elf[16:18] = (3).to_bytes(2, "little")
        elf[18:20] = (62).to_bytes(2, "little")
        for specification in build.PLATFORM_SHARED.values():
            (root / specification["relative"]).write_bytes(elf)
        record = {"tool":"PenguDroid API-37 runtime preparation", "schema":1,
                  "build":build.GSI_BUILD, "sdk":37, "release":"17", "architecture":"x86_64",
                  "archive_sha256":build.GSI_SHA256,
                  "files":{relative:build.BOOT.sha256_file(root / relative) for relative in
                           ["system/build.prop", *(s["relative"] for s in build.PLATFORM_SHARED.values())]}}
        (root / build.RUNTIME_MANIFEST).write_text(json.dumps(record))
        log = self.root / "readelf.log"
        def inspect(argv, label):
            name = next(n for n, s in build.PLATFORM_SHARED.items() if Path(argv[-1]).name == Path(s["relative"]).name)
            symbols = build.PLATFORM_SHARED[name]["required_exports"]
            text = "0x0 (SONAME) Library soname: [" + name + ".so]\n0x0 (NEEDED) Shared library: [libc.so]\n"
            text += "".join(f"{i}: 0000000000001000 16 FUNC GLOBAL DEFAULT 10 {symbol}@@LIBANDROID\n"
                            for i, symbol in enumerate(symbols, 1))
            log.write_text(text)
            return {"log":str(log)}
        runner = mock.Mock(root=self.root)
        runner.run.side_effect = inspect
        # Synthetic fixtures exercise validation, not genuine GSI provenance or runtime behavior.
        return root, record, runner

    def test_platform_import_requires_completed_manifest_not_a_staging_tree(self):
        root = self.root / "platform"
        root.mkdir()
        records = {}
        imports = build.PlatformImports(root, mock.Mock(root=self.root), records)
        self.assertFalse(imports.probe())
        self.assertEqual(set(records), set(build.PLATFORM_SHARED))
        self.assertTrue(all("missing completed" in r["blocker"] for r in records.values()))
        self.assertFalse(list(root.iterdir()))

    def test_platform_import_checks_hashes_soname_defined_exports_and_is_read_only(self):
        root, manifest, runner = self.platform_fixture()
        before = {p.relative_to(root):build.BOOT.sha256_file(p) for p in root.rglob("*") if p.is_file()}
        records = {}
        imports = build.PlatformImports(root, runner, records)
        self.assertTrue(imports.probe())
        self.assertEqual({p.relative_to(root):build.BOOT.sha256_file(p) for p in root.rglob("*") if p.is_file()}, before)
        for name, record in records.items():
            self.assertEqual(record["sha256"], manifest["files"][build.PLATFORM_SHARED[name]["relative"]])
            self.assertEqual(record["soname"], name + ".so")
            self.assertFalse(record["needed_closure_verified"])
            self.assertFalse(record["runtime_tested"])

    def test_wrong_gsi_api_build_or_architecture_cannot_fall_back(self):
        root, original, runner = self.platform_fixture()
        for key, value in (("sdk",33), ("release","13"), ("build","OTHER"), ("architecture","arm64"),
                           ("archive_sha256","0" * 64), ("tool","unverified extraction")):
            manifest = {**original, key:value}
            (root / build.RUNTIME_MANIFEST).write_text(json.dumps(manifest))
            with self.subTest(key=key), self.assertRaisesRegex(build.BuildError, "no fallback"):
                build.PlatformImports(root, runner, {}).inspect("libandroid")
        runner.run.assert_not_called()

    def test_unrecorded_platform_library_is_not_a_guessed_stub(self):
        root, manifest, runner = self.platform_fixture()
        del manifest["files"]["system/lib64/libandroid.so"]
        (root / build.RUNTIME_MANIFEST).write_text(json.dumps(manifest))
        with self.assertRaisesRegex(build.BuildError, "must pin files.*libandroid.so"):
            build.PlatformImports(root, runner, {}).inspect("libandroid")
        runner.run.assert_not_called()

    def test_modified_platform_library_fails_before_tool_execution(self):
        root, _, runner = self.platform_fixture()
        (root / "system/lib64/libandroid.so").write_bytes(b"changed")
        with self.assertRaisesRegex(build.BuildError, "platform file hash mismatch"):
            build.PlatformImports(root, runner, {}).inspect("libandroid")
        runner.run.assert_not_called()

    def test_platform_symlinks_allow_only_resolved_files_inside_completed_root(self):
        root, _, runner = self.platform_fixture()
        library = root / "system/lib64/libandroid.so"
        apex = root / "system/apex/fixture/lib64"
        apex.mkdir(parents=True)
        library.rename(apex / "libandroid.so")
        library.symlink_to("../apex/fixture/lib64/libandroid.so")
        self.assertEqual(build.PlatformImports(root, runner, {}).inspect("libandroid"), (apex / "libandroid.so").resolve())
        library.unlink()
        outside = self.root / "outside.so"
        outside.write_bytes((apex / "libandroid.so").read_bytes())
        library.symlink_to(outside)
        with self.assertRaisesRegex(build.BuildError, "escapes the extracted runtime"):
            build.PlatformImports(root, runner, {}).inspect("libandroid")

    def test_platform_rejects_wrong_soname_undefined_export_and_host_cpp(self):
        root, _, runner = self.platform_fixture()
        log = self.root / "invalid-readelf.log"
        cases = (
            ("0x0 (SONAME) Library soname: [libother.so]\n", "SONAME"),
            ("0x0 (SONAME) Library soname: [libandroid.so]\n1: 0 0 FUNC GLOBAL DEFAULT UND ANativeActivity_finish\n", "missing defined exports"),
            ("0x0 (SONAME) Library soname: [libandroid.so]\n0x0 (NEEDED) Shared library: [libc++_shared.so]\n", "host/NDK"),
        )
        runner.run.side_effect = None
        runner.run.return_value = {"log":str(log)}
        for text, error in cases:
            log.write_text(text)
            with self.subTest(error=error), self.assertRaisesRegex(build.BuildError, error):
                build.PlatformImports(root, runner, {}).inspect("libandroid")

    def test_platform_detects_manifest_changed_during_inspection(self):
        root, _, runner = self.platform_fixture()
        inspect = runner.run.side_effect
        def changed(argv, label):
            result = inspect(argv, label)
            with (root / build.RUNTIME_MANIFEST).open("a") as stream:
                stream.write(" ")
            return result
        runner.run.side_effect = changed
        with self.assertRaisesRegex(build.BuildError, "changed during inspection"):
            build.PlatformImports(root, runner, {}).inspect("libandroid")

    def early_fixture(self, suffix):
        run = self.root / ("run-" + suffix)
        (run / "lib").mkdir(parents=True)
        artifacts = []
        for name in build.EARLY_GOALS:
            directory = run / "obj" / name
            directory.mkdir(parents=True)
            obj = directory / "fixture.o"
            elf = bytearray(20)
            elf[:6] = b"\x7fELF\x02\x01"
            elf[16:18] = (1).to_bytes(2, "little")
            elf[18:20] = (62).to_bytes(2, "little")
            obj.write_bytes(elf)
            shared = name == "libberberis_exec_region"
            output = run / "lib" / (name + (".so" if shared else ".a"))
            if shared:
                elf[16:18] = (3).to_bytes(2, "little")
            output.write_bytes(elf if shared else b"!<arch>\n")
            artifacts.append({"module":name,"output":str(output),"objects":[str(obj)],
                              "bytes":output.stat().st_size,"sha256":build.BOOT.sha256_file(output)})
        return {**build.OWNER, "run":str(run), "early_steps_passed":True, "requested_goals_passed":False,
                "driver_sha256":"0" * 64,"dependencies_manifest_sha256":"1" * 64,
                "complete_translator":False,"artifacts":artifacts,"commands":[],"blocker":"later goal blocked"}

    def test_failed_platform_probe_preserves_last_successful_early_artifacts(self):
        milestone = self.early_fixture("old")
        build.atomic_json(self.root / "report.json", milestone)
        with mock.patch.object(build, "ROOT", self.root):
            saved = build.load_early_checkpoint()
            before = (self.root / "last-successful-early.json").read_bytes()
            run = self.root / "run-probe"
            run.mkdir()
            probe = {**build.OWNER,"run":str(run),"early_steps_passed":False,"requested_goals_passed":False,
                     "artifacts":[],"last_successful_early":saved,"blocker":"platform not ready"}
            build.publish_report(probe)
        latest = json.loads((self.root / "report.json").read_text())
        self.assertEqual(latest["artifacts"], [])
        self.assertFalse(latest["early_steps_passed"])
        self.assertEqual(latest["last_successful_early"]["artifacts"], milestone["artifacts"])
        self.assertFalse(latest["last_successful_early"]["reused_for_current_run"])
        self.assertEqual((self.root / "last-successful-early.json").read_bytes(), before)
        self.assertEqual(latest, json.loads((run / "report.json").read_text()))

    def test_towards_bridge_failure_still_checkpoints_completed_early_goals(self):
        milestone = self.early_fixture("new")
        with mock.patch.object(build, "ROOT", self.root):
            build.publish_report(milestone)
            saved = build.load_early_checkpoint()
        self.assertEqual(saved["run"], milestone["run"])
        self.assertEqual(len(saved["artifacts"]), 4)
        self.assertFalse(milestone["requested_goals_passed"])
        self.assertFalse(milestone["complete_translator"])

    def test_partial_build_does_not_replace_successful_early_checkpoint(self):
        old = self.early_fixture("old")
        partial = self.early_fixture("partial")
        partial["early_steps_passed"] = False
        partial["artifacts"] = partial["artifacts"][:2]
        with mock.patch.object(build, "ROOT", self.root):
            build.publish_report(old)
            partial["last_successful_early"] = build.load_early_checkpoint()
            build.publish_report(partial)
            self.assertEqual(build.load_early_checkpoint()["run"], old["run"])
        self.assertEqual(len(partial["artifacts"]), 2)

    def test_tampered_retained_artifact_is_not_silently_reused_or_forgotten(self):
        milestone = self.early_fixture("old")
        with mock.patch.object(build, "ROOT", self.root):
            build.publish_report(milestone)
            before = (self.root / "last-successful-early.json").read_bytes()
            Path(milestone["artifacts"][0]["output"]).write_bytes(b"tampered")
            with self.assertRaisesRegex(build.BuildError, "retained early artifact changed"):
                build.load_early_checkpoint()
        self.assertEqual((self.root / "last-successful-early.json").read_bytes(), before)

    def test_early_checkpoint_rejects_duplicate_modules_and_path_escapes(self):
        milestone = self.early_fixture("old")
        with mock.patch.object(build, "ROOT", self.root):
            for field in ("duplicate", "output", "object"):
                candidate = json.loads(json.dumps(milestone))
                if field == "duplicate":
                    candidate["artifacts"][1] = candidate["artifacts"][0]
                elif field == "output":
                    candidate["artifacts"][0]["output"] = str(self.root / "not-owned-by-run.so")
                else:
                    candidate["artifacts"][0]["objects"] = [str(self.root / "not-owned-by-module.o")]
                with self.subTest(field=field), self.assertRaises(build.BuildError):
                    build.early_snapshot(candidate)

    def test_standalone_blueprint_uses_verified_original_source_directory(self):
        source = self.root / "metadata"
        source.mkdir()
        blueprint = source / "Android.bp"
        blueprint.write_text('cc_library_headers {name:"headers",export_include_dirs:["platform"]}')
        canonical = self.root / "verified-slice/libc"
        (canonical / "platform").mkdir(parents=True)
        model = build.Blueprints({"metadata":source}, {blueprint:canonical})
        self.assertEqual(model.props("headers")["export_include_dirs"], [str(canonical / "platform")])
        self.assertEqual(model.used["headers"]["blueprint"], str(blueprint))
        self.assertEqual(model.used["headers"]["source_directory"], str(canonical))

    def test_only_proven_packaging_custom_rules_can_project_headers(self):
        model = self.model('''
            soong_config_module_type {name:"packaging_cc",module_type:"cc_library",config_namespace:"ANDROID",
                                      bool_variables:["release_flag"],properties:["apex_available"]}
            packaging_cc {name:"safe",export_include_dirs:["include"],
                          soong_config_variables:{release_flag:{apex_available:["platform"],conditions_default:{apex_available:["runtime"]}}}}
            soong_config_module_type {name:"flag_cc",module_type:"cc_library",bool_variables:["flag"],properties:["cflags"]}
            flag_cc {name:"unsafe",export_include_dirs:["include"]}
            packaging_cc {name:"undeclared",soong_config_variables:{unknown:{apex_available:[]}}}
            packaging_cc {name:"header_change",soong_config_variables:{release_flag:{conditions_default:{export_include_dirs:["wrong"]}}}}
        ''')
        self.assertEqual(model.props("safe", linkage="shared", headers_only=True)["export_include_dirs"],
                         [str(self.root / "source/include")])
        with self.assertRaisesRegex(build.BuildError, "unsupported Blueprint rule"):
            model.props("safe", linkage="shared")
        for name in ("unsafe", "undeclared", "header_change"):
            with self.subTest(name=name), self.assertRaises(build.BuildError):
                model.props(name, linkage="shared", headers_only=True)

    def test_whole_static_cycles_fail_before_archiving(self):
        model = self.model('''cc_library_static {name:"a",whole_static_libs:["b"]}
                              cc_library_static {name:"b",whole_static_libs:["a"]}''')
        runner = mock.Mock(root=self.root)
        native = build.NativeBuild(model, runner, self.root)
        with self.assertRaisesRegex(build.BuildError, "cyclic native dependency"):
            native.build("a")
        runner.run.assert_not_called()
        self.assertEqual(native.building, [])
        self.assertFalse(native.built)

    def test_explicit_native_linkage_is_required_and_never_substituted(self):
        model = self.model('''cc_library {name:"both"} cc_library_shared {name:"shared"}''')
        native = build.NativeBuild(model, mock.Mock(root=self.root), self.root)
        with self.assertRaisesRegex(build.BuildError, "explicit static/shared build variant"):
            native.build("both")
        with self.assertRaisesRegex(build.BuildError, "static variant unavailable"):
            native.build("shared", "static")

    def test_generated_sources_do_not_omit_a_native_host_tool(self):
        model = self.model('''cc_library_static {name:"entry",generated_sources:["generated"]}
            genrule {name:"generated",tools:["native_tool"],out:["generated.cc"],cmd:"$(location native_tool) $(out)"}
            cc_binary_host {name:"native_tool",srcs:["generator.cc"]}''')
        runner = mock.Mock(root=self.root)
        with self.assertRaisesRegex(build.BuildError, "module is not host-supported: native_tool"):
            build.NativeBuild(model, runner, self.root).build("entry")
        runner.run.assert_not_called()

    def test_nonempty_link_flags_are_not_silently_dropped(self):
        model = self.model('cc_library_shared {name:"entry",ldflags:["-Wl,--some-unsupported-flag"]}')
        runner = mock.Mock(root=self.root)
        with self.assertRaisesRegex(build.BuildError, "no flags discarded"):
            build.NativeBuild(model, runner, self.root).build("entry")
        runner.run.assert_not_called()

    def test_genrule_defaults_keep_declared_outputs_tools_and_input_order(self):
        model = self.model('''
            genrule_defaults {name:"common",out:["one.h","two.h"],tools:["generator"],
                              cmd:"$(location generator) $(out) $(in)",srcs:["first.json"]}
            genrule {name:"generated",defaults:["common"],srcs:["second.json"]}
        ''')
        props = model.props("generated")
        self.assertEqual(props["out"], ["one.h", "two.h"])
        self.assertEqual(props["tools"], ["generator"])
        self.assertEqual([Path(p).name for p in props["srcs"]], ["first.json", "second.json"])
        self.assertIn("$(out) $(in)", props["cmd"])

    def test_codegen_defaults_cannot_be_used_as_cc_defaults_or_vice_versa(self):
        model = self.model('''genrule_defaults {name:"gen_defaults",out:["out.cc"]}
            cc_defaults {name:"cc_defaults",cflags:["-DWRONG"]}
            genrule {name:"bad_gen",defaults:["cc_defaults"]}
            cc_library_static {name:"bad_cc",defaults:["gen_defaults"]}''')
        for name in ("bad_gen", "bad_cc"):
            with self.subTest(name=name), self.assertRaisesRegex(build.BuildError, "unsupported defaults module"):
                model.props(name)

    def test_resource_and_goal_policy(self):
        self.assertGreaterEqual(build.MEMORY, 6_000_000_000)
        self.assertLessEqual(build.MEMORY, 8_000_000_000)
        self.assertIn("libberberis_native_bridge_arm64", build.EARLY_GOALS)
        self.assertNotIn("berberis_all", build.EARLY_GOALS)
        self.assertNotIn("linux_glibc", build.AXES)
        self.assertEqual(json.loads(json.dumps(build.OWNER))["schema"], 1)


@unittest.skipUnless((build.DEPS / "manifest.json").is_file()
                     and (build.LLVM / "clang++").is_file()
                     and (build.ROOT / "extra/platform-headers/cutils/log.h").is_file(),
                     "requires the existing verified API37/platform-libc++ bootstrap and cached extras")
class PinnedIntegrationTests(unittest.TestCase):
    def setUp(self):
        build.prepare_root()
        temporary = tempfile.TemporaryDirectory(prefix="integration-test-", dir=build.ROOT / "work")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        for name in ("source", "logs", "tmp", "lib", "gen", "obj"):
            (self.root / name).mkdir()

    def test_real_api37_c_cpp_assembly_and_platform_libcxx_shared_link(self):
        source = self.root / "source"
        (source / "Android.bp").write_text('''cc_library_shared {name:"fixture",srcs:["a.c","b.cc","c.S"]}''')
        (source / "a.c").write_text('''#include <stdint.h>
#if !defined(ANDROID) || !defined(__BIONIC__) || !defined(__x86_64__) || __ANDROID_API__ != 37
#error wrong platform
#endif
_Static_assert(sizeof(void*) == 8, "LP64");
int c_probe(void) { return 37; }
''')
        (source / "b.cc").write_text('''#include <string>
extern "C" size_t cpp_probe(const char* value) { return std::string(value).size(); }
''')
        (source / "c.S").write_text('''.text
.globl asm_probe
.type asm_probe,@function
asm_probe:
ret
.section .note.GNU-stack,"",@progbits
''')
        manifest = json.loads((build.DEPS / "manifest.json").read_text())
        report = {"commands":[]}
        runner = build.Runner(self.root, report, 30)
        native = build.NativeBuild(build.Blueprints({"fixture":source}), runner, Path(manifest["preflight"]["sysroot"]))
        result = native.build("fixture")
        self.assertEqual(len(result["objects"]), 3)
        self.assertEqual(result["elf_machine"], "x86_64")
        self.assertFalse(result["runtime_tested"])
        symbols = runner.run([str(build.LLVM / "llvm-nm"), "--undefined-only", result["output"]], "check platform ABI")
        text = Path(symbols["log"]).read_text()
        self.assertIn("St3__1", text)
        self.assertNotIn("__ndk1", text)
        self.assertNotIn("GLIBCXX", text)
        dynamic = runner.run([str(build.LLVM / "llvm-readelf"), "--dynamic", result["output"]], "check dependencies")
        text = Path(dynamic["log"]).read_text()
        self.assertIn("[libc++.so]", text)
        self.assertNotIn("libc++_shared", text)
        self.assertNotIn("libstdc++", text)
        link = next(c["argv"] for c in report["commands"] if c["label"] == "link fixture")
        libdir = Path(manifest["preflight"]["sysroot"]) / "usr/lib/x86_64-linux-android/37"
        self.assertIn(str(libdir / "crtbegin_so.o"), link)
        self.assertIn(str(libdir / "crtend_so.o"), link)
        self.assertIn(str(build.DEPS / "toolchain/libclang_rt.builtins-x86_64-android.a"), link)
        self.assertIn("--no-undefined", link)
        self.assertIn("--threads=1", link)

    def test_real_readelf_platform_import_gate_on_synthetic_android_fixture(self):
        source = self.root / "source"
        (source / "Android.bp").write_text('cc_library_shared {name:"fixture",srcs:["probe.c"]}')
        (source / "probe.c").write_text("int platform_gate_probe(void) { return 37; }\n")
        bootstrap = json.loads((build.DEPS / "manifest.json").read_text())
        runner = build.Runner(self.root, {"commands":[]}, 30)
        native = build.NativeBuild(build.Blueprints({"fixture":source}), runner, Path(bootstrap["preflight"]["sysroot"]))
        artifact = native.build("fixture")
        platform = self.root / "platform"
        library = platform / "system/lib64/fixture.so"
        library.parent.mkdir(parents=True)
        Path(artifact["output"]).rename(library)
        props = platform / "system/build.prop"
        props.write_text("ro.build.id=" + build.GSI_BUILD + "\nro.build.version.sdk=37\nro.build.version.release=17\n")
        # This receipt is synthetic. The test validates real ELF inspection, not a GSI or runtime.
        manifest = {"tool":"PenguDroid API-37 runtime preparation", "schema":1, "build":build.GSI_BUILD,
                    "sdk":37,"release":"17","architecture":"x86_64","archive_sha256":build.GSI_SHA256,
                    "files":{"system/build.prop":build.BOOT.sha256_file(props),
                             "system/lib64/fixture.so":build.BOOT.sha256_file(library)}}
        (platform / build.RUNTIME_MANIFEST).write_text(json.dumps(manifest))
        records = {}
        with mock.patch.object(build, "PLATFORM_SHARED", {"fixture":{"relative":"system/lib64/fixture.so",
                                                                     "required_exports":["platform_gate_probe"]}}):
            self.assertEqual(build.PlatformImports(platform, runner, records).inspect("fixture"), library)
        self.assertEqual(records["fixture"]["status"], "verified link input only")
        self.assertFalse(records["fixture"]["needed_closure_verified"])

    def test_generated_cpp_source_is_actually_compiled(self):
        source = self.root / "source"
        (source / "Android.bp").write_text('''
            python_binary_host {name:"generator",main:"generator.py",srcs:["generator.py"]}
            genrule {name:"generated",tools:["generator"],out:["generated.cc"],cmd:"$(location generator) $(out)"}
            cc_library_static {name:"entry",generated_sources:["generated"]}
        ''')
        (source / "generator.py").write_text('import sys\nfrom pathlib import Path\nPath(sys.argv[1]).write_text(\'extern "C" int generated_answer() { return 37; }\\n\')\n')
        bootstrap = json.loads((build.DEPS / "manifest.json").read_text())
        report = {"commands":[]}
        runner = build.Runner(self.root, report, 30)
        native = build.NativeBuild(build.Blueprints({"fixture":source}), runner, Path(bootstrap["preflight"]["sysroot"]))
        artifact = native.build("entry")
        self.assertEqual(len(artifact["objects"]), 1)
        compile = next(c["argv"] for c in report["commands"] if c["label"].startswith("compile entry"))
        self.assertIn(str(self.root / "gen/generated/generated.cc"), compile)
        record = runner.run([str(build.LLVM / "llvm-nm"), "--defined-only", artifact["output"]], "check generated object")
        self.assertIn("T generated_answer", Path(record["log"]).read_text())
        self.assertFalse(list(source.rglob("__pycache__")))

    def test_whole_archive_duplicates_and_transitive_static_link_are_preserved(self):
        source = self.root / "source"
        (source / "a").mkdir()
        (source / "b").mkdir()
        (source / "a/same.c").write_text("int left(void) { return 10; }\n")
        (source / "b/same.c").write_text("int unreferenced_right(void) { return 20; }\n")
        (source / "ordinary.c").write_text("extern int leaf(void); int ordinary(void) { return leaf(); }\n")
        (source / "leaf.c").write_text("#ifndef STATIC_VARIANT\n#error wrong linkage\n#endif\nint leaf(void) { return 27; }\n")
        (source / "entry.c").write_text("extern int left(void); extern int ordinary(void); int entry(void) { return left()+ordinary(); }\n")
        (source / "Android.bp").write_text('''
            cc_library_static {name:"left",srcs:["a/same.c"]}
            cc_library_static {name:"right",srcs:["b/same.c"]}
            cc_library_static {name:"bundle",whole_static_libs:["left","right"]}
            cc_library_static {name:"ordinary",srcs:["ordinary.c"],static_libs:["leaf"]}
            cc_library {name:"leaf",srcs:["leaf.c"],static:{cflags:["-DSTATIC_VARIANT"]}}
            cc_library_shared {name:"entry",srcs:["entry.c"],static_libs:["ordinary"],whole_static_libs:["bundle"]}
        ''')
        bootstrap = json.loads((build.DEPS / "manifest.json").read_text())
        report = {"commands":[]}
        runner = build.Runner(self.root, report, 30)
        native = build.NativeBuild(build.Blueprints({"fixture":source}), runner, Path(bootstrap["preflight"]["sysroot"]))
        bundle = native.build("bundle")
        record = runner.run([str(build.LLVM / "llvm-ar"), "t", bundle["output"]], "check flattened members")
        members = Path(record["log"]).read_text().splitlines()
        self.assertEqual(members, ["000-same.o", "000-same.o"])
        before = len(report["commands"])
        self.assertIs(native.build("bundle"), bundle)
        self.assertEqual(len(report["commands"]), before)
        artifact = native.build("entry")
        self.assertEqual([a["module"] for a in artifact["static_archives"]], ["ordinary", "leaf"])
        record = runner.run([str(build.LLVM / "llvm-nm"), "--dynamic", "--defined-only", artifact["output"]], "check whole-static retention")
        symbols = Path(record["log"]).read_text()
        for name in ("left", "unreferenced_right", "ordinary", "leaf", "entry"):
            self.assertIn("T " + name, symbols)
        self.assertIn("leaf", {a["module"] for a in report["dependency_artifacts"]})

    def test_exact_power_alias_and_platform_header_exports(self):
        if not (build.ROOT / "extra/hardware-interfaces/power/aidl/Android.bp").is_file():
            self.skipTest("requires the pinned power/platform header slices")
        roots = self.platform_header_roots()
        model = build.Blueprints(roots, {build.ROOT / "extra/bionic-libc/Android.bp":build.DEPS / "sources/bionic/libc"})
        props = model.props("android.hardware.power-ndk_shared")
        self.assertEqual(props["shared_libs"], ["android.hardware.power-V7-ndk"])
        self.assertNotIn("export_shared_lib_headers", props)
        audit = build.describe_platform_sources(model)
        self.assertEqual(audit["libandroid"]["unresolved_source_defaults"], [])
        self.assertEqual(audit["libdl_android"]["unresolved_source_defaults"], [])
        native = build.NativeBuild(model, mock.Mock(root=self.root), self.root)
        android = native.exports("libandroid", linkage="shared")
        self.assertEqual(set(android), {str(build.ROOT / "extra/frameworks-base/native/android/include_platform"),
                                       str(build.ROOT / "extra/frameworks-native/libs/arect/include"),
                                       str(build.ROOT / "extra/npumanager/ndk/include")})
        dl = native.exports("libdl_android", linkage="shared")
        self.assertIn(str(build.DEPS / "sources/bionic/libc/platform"), dl)
        self.assertTrue(all(Path(p).is_dir() for p in android + dl))

    def platform_header_roots(self):
        return {"frameworks/base":build.ROOT / "extra/frameworks-base",
                "frameworks/native":build.ROOT / "extra/frameworks-native",
                "hardware/interfaces":build.ROOT / "extra/hardware-interfaces",
                "packages/modules/NpuManager":build.ROOT / "extra/npumanager",
                "build/soong":build.ROOT / "extra/soong",
                "libnativehelper":build.ROOT / "extra/libnativehelper",
                "bionic":build.DEPS / "sources/bionic",
                "bionic-libc":build.ROOT / "extra/bionic-libc",
                "bionic-libdl":build.ROOT / "extra/bionic-libdl"}

    def test_link_uses_genuine_verified_api37_platform_libraries(self):
        if not (build.PLATFORM_ROOT / build.RUNTIME_MANIFEST).is_file():
            self.skipTest("requires the completed verified API37 runtime")
        source = self.root / "source"
        (source / "Android.bp").write_text('''
            cc_library_static {name:"facade",srcs:["probe.cc"],export_include_dirs:["include"],
                header_libs:["jni_headers"],export_header_lib_headers:["jni_headers"],
                shared_libs:["libandroid","libdl_android"]}
            cc_library_shared {name:"platform_link_probe",srcs:["entry.cc"],static_libs:["facade"]}
        ''')
        (source / "include").mkdir()
        (source / "include/probe.h").write_text('#include <android/native_activity.h>\nextern "C" void probe(ANativeActivity*);\n')
        (source / "entry.cc").write_text('#include "probe.h"\nextern "C" void entry(ANativeActivity* a) { probe(a); }\n')
        (source / "probe.cc").write_text('''#include "probe.h"
#include <android/dlext_private.h>
extern "C" void probe(ANativeActivity* activity) {
  ANativeActivity_finish(activity);
  android_set_16kb_appcompat_mode(false);
}
''')
        roots = self.platform_header_roots()
        roots["fixture"] = source
        bootstrap = json.loads((build.DEPS / "manifest.json").read_text())
        report, records = {"commands":[]}, {}
        runner = build.Runner(self.root, report, 30)
        imports = build.PlatformImports(build.PLATFORM_ROOT, runner, records)
        model = build.Blueprints(roots, {build.ROOT / "extra/bionic-libc/Android.bp":build.DEPS / "sources/bionic/libc"})
        native = build.NativeBuild(model, runner, Path(bootstrap["preflight"]["sysroot"]), imports)
        artifact = native.build("platform_link_probe")
        self.assertEqual(set(records), {"libandroid", "libdl_android"})
        self.assertEqual([a["module"] for a in artifact["static_archives"]], ["facade"])
        self.assertEqual(artifact["shared_inputs"], [records[n]["path"] for n in ("libandroid", "libdl_android")])
        self.assertFalse(any("usr/lib/x86_64-linux-android/37/libandroid" in p for p in artifact["shared_inputs"]))
        record = runner.run([str(build.LLVM / "llvm-readelf"), "--dynamic", artifact["output"]], "check genuine platform NEEDED")
        dynamic = Path(record["log"]).read_text()
        self.assertIn("[libandroid.so]", dynamic)
        self.assertIn("[libdl_android.so]", dynamic)
        self.assertFalse(artifact["runtime_tested"])

    def test_pinned_platform_providers_are_real_libraries_not_ndk_declarations(self):
        if not (build.ROOT / "extra/frameworks-base/native/android/Android.bp").is_file():
            self.skipTest("requires cached provider Blueprints")
        model = build.Blueprints({"frameworks/base":build.ROOT / "extra/frameworks-base",
                                  "bionic-libdl":build.ROOT / "extra/bionic-libdl"})
        audit = build.describe_platform_sources(model)
        self.assertEqual(model.modules["libandroid"][0], "cc_library_shared")
        provider = audit["libandroid"]
        self.assertEqual(provider["revision"], "94b4c163b7dfe5ce3607f7bb8456f9573f7de57d")
        self.assertEqual(provider["git_blob"], "6c172b98e7f80ee21fa53fc4b440adec4657bb50")
        self.assertEqual(provider["unresolved_source_defaults"], ["android.hardware.power-ndk_shared"])
        common = provider["declarations"]["libandroid_common"]["properties"]
        self.assertTrue({"libandroid_runtime","libbinder","libgui"} <= set(common["shared_libs"]))
        self.assertEqual(common["export_include_dirs"], ["include_platform"])
        self.assertIn("libandroid_com.android.npumanager", common["export_static_lib_headers"])
        with self.assertRaisesRegex(build.BuildError, "android.hardware.power-ndk_shared"):
            model.props("libandroid", linkage="shared", headers_only=True)
        dl = audit["libdl_android"]["declarations"]["libdl_android"]
        self.assertEqual(dl["kind"], "bionic_cc_library")
        self.assertEqual(dl["properties"]["shared_libs"], ["ld-android"])
        self.assertTrue(dl["properties"]["nocrt"])

    def test_pinned_digitalis_guest_flags_and_bridge_source_selection(self):
        manifest = json.loads((build.DEPS / "manifest.json").read_text())
        self.assertEqual(manifest["artifacts"]["digitalis"]["revision"], "2d03335e3c6bee947ace72f0aa717ff4f8db138b")
        source = build.DEPS / "sources/digitalis"
        model = build.Blueprints({"frameworks/libs/binary_translation":source})
        for name in build.EARLY_GOALS[:3]:
            flags = model.props(name)["cflags"]
            self.assertIn("-DBERBERIS_GUEST_LP64=1", flags)
            self.assertFalse(any("NATIVE_BRIDGE_GUEST_ARCH" in flag for flag in flags))
        props = model.props("libberberis_native_bridge_arm64")
        self.assertIn("-DNATIVE_BRIDGE_GUEST_ARCH_ARM64", props["cflags"])
        self.assertFalse(any("RISCV" in flag for flag in props["cflags"]))
        paths = model.files(props["srcs"])
        self.assertEqual([str(Path(p).relative_to(source / "native_bridge")) for p in paths],
                         ["native_bridge.cc", "arm64/native_bridge.cc", "arm64/digitalis_app_support.cc"])
        self.assertIn("libberberis_native_bridge_arm64", model.props("libberberis_arm64")["whole_static_libs"])


if __name__ == "__main__":
    unittest.main()
