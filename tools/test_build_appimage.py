#!/usr/bin/env python3
"""Tests for the one-file AppImage bundle manager."""

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True

REPO = Path(__file__).resolve().parent.parent
SCRIPT = REPO / "tools" / "build-appimage.sh"
RELEASE = REPO / "target" / "release"
ARTIFACTS = (
    "pengudroid",
    "pengudroid-gui",
    "libpengudroid_shim.so",
    "libpengudroid_gleshim.so",
)


def run(*arguments, env=None, check=False):
    return subprocess.run(
        ["sh", str(SCRIPT), *arguments],
        cwd=REPO,
        env=env,
        capture_output=True,
        text=True,
        check=check,
    )


class ScriptTests(unittest.TestCase):
    def test_parses_with_a_posix_shell(self):
        result = subprocess.run(
            ["sh", "-n", str(SCRIPT)], capture_output=True, text=True
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_rejects_an_unknown_option(self):
        result = run("--definitely-not-an-option")
        self.assertEqual(result.returncode, 2)
        self.assertIn("unknown option", result.stderr)


class AppDirTests(unittest.TestCase):
    """Assemble a real AppDir from stand-ins for the release binaries."""

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="test-appimage-")
        self.addCleanup(self.temporary.cleanup)
        self.fake = Path(self.temporary.name)
        # The script resolves its artifacts under <repo>/target, so run a copy of
        # the tree layout it expects; copying the repo is too expensive, so only
        # the paths the script touches are mirrored.
        self.appdir = REPO / "target" / "appimage" / "PenguDroid.AppDir"

    def test_assembled_layout(self):
        if not all((RELEASE / name).is_file() for name in ARTIFACTS):
            self.skipTest("release artifacts are not built")
        result = run("--skip-build", "--no-image", "--output", str(self.fake / "out"))
        self.assertEqual(result.returncode, 0, result.stderr)

        self.assertIn("assembled", result.stdout)
        self.assertEqual(
            sorted(path.name for path in (self.appdir / "usr/bin").iterdir()),
            sorted(ARTIFACTS),
        )
        for name in ARTIFACTS:
            self.assertTrue(os.access(self.appdir / "usr/bin" / name, os.X_OK), name)

        apprun = (self.appdir / "AppRun").read_text()
        self.assertTrue(apprun.startswith("#!/bin/sh"))
        self.assertIn('exec "$HERE/usr/bin/pengudroid-gui"', apprun)
        # An explicit choice must survive the AppRun defaults: a developer
        # testing a freshly prepared translator relies on it.
        self.assertIn('[ -z "${PENGUDROID_NATIVE_BRIDGE_LIB:-}" ]', apprun)
        self.assertIn('[ -z "${PENGUDROID_SYSROOT:-}" ]', apprun)

    def test_desktop_entry_launches_the_shell_and_accepts_apk_files(self):
        if not all((RELEASE / name).is_file() for name in ARTIFACTS):
            self.skipTest("release artifacts are not built")
        run("--skip-build", "--no-image")
        entry = (self.appdir / "pengudroid.desktop").read_text()
        self.assertIn("Type=Application", entry)
        self.assertIn("Name=PenguDroid", entry)
        self.assertIn("Exec=pengudroid-gui", entry)
        self.assertIn("Icon=pengudroid", entry)
        # The "+" button installs through the CLI, so the file manager should be
        # able to hand an APK to the shell as well.
        self.assertIn("MimeType=application/vnd.android.package-archive", entry)
        self.assertTrue((self.appdir / "pengudroid.png").is_file())
        self.assertEqual(
            (self.appdir / "pengudroid.desktop").read_bytes(),
            (self.appdir / "usr/share/applications/pengudroid.desktop").read_bytes(),
        )

    def test_bundled_cli_reports_its_version(self):
        if not all((RELEASE / name).is_file() for name in ARTIFACTS):
            self.skipTest("release artifacts are not built")
        run("--skip-build", "--no-image")
        binary = self.appdir / "usr/bin/pengudroid"
        env = dict(os.environ)
        # The CLI looks for its host libraries beside itself.
        env["LD_LIBRARY_PATH"] = str(binary.parent)
        result = subprocess.run(
            [str(binary), "--version"], capture_output=True, text=True, env=env
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("pengudroid", result.stdout.lower())


if __name__ == "__main__":
    unittest.main()
