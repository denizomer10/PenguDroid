#!/usr/bin/env python3
"""Expand APEX/CAPEX payloads in an extracted Android system tree, without mounts.

Usage: unpack-apex.py ROOT [--erofs /path/to/fsck.erofs]
Existing expanded packages are retained; incomplete destinations are rejected.
Raw Android archives stay local to the supplied runtime tree.
"""
import argparse
from pathlib import Path
import shutil
import subprocess
import tempfile
import zipfile


def copy_member(archive, member, output):
    with archive.open(member) as source, output.open("wb") as destination:
        shutil.copyfileobj(source, destination, 4 * 1024 * 1024)


def expand(root, erofs):
    base = root.resolve() / "system" / "apex"
    if not base.is_dir():
        raise RuntimeError(f"missing APEX directory: {base}")
    debugfs = shutil.which("debugfs")
    expanded = retained = 0
    for archive in sorted(base.iterdir()):
        if archive.suffix not in (".apex", ".capex"):
            continue
        destination = base / archive.stem
        if destination.exists():
            if not (destination / "apex_manifest.pb").is_file():
                raise RuntimeError(f"incomplete destination; inspect it before retrying: {destination}")
            retained += 1
            continue
        with tempfile.TemporaryDirectory(prefix=".pengudroid-apex-", dir=base) as temporary:
            temporary = Path(temporary)
            original = archive
            if archive.suffix == ".capex":
                original = temporary / "original.apex"
                with zipfile.ZipFile(archive) as container:
                    copy_member(container, "original_apex", original)
            payload = temporary / "payload.img"
            with zipfile.ZipFile(original) as container:
                copy_member(container, "apex_payload.img", payload)
            output = temporary / "expanded"
            output.mkdir()
            with payload.open("rb") as image:
                image.seek(1024)
                is_erofs = image.read(4) == bytes.fromhex("e2e1f5e0")
            if is_erofs:
                if not erofs:
                    raise RuntimeError(f"{archive.name} needs fsck.erofs; pass --erofs")
                command = [erofs, f"--extract={output}", "--no-preserve-owner", str(payload)]
            else:
                if not debugfs:
                    raise RuntimeError("ext4 APEX extraction needs debugfs")
                quoted = str(output).replace("\\", "\\\\").replace('"', '\\"')
                if any(c in quoted for c in "\n\r\0"):
                    raise RuntimeError("unsupported control character in runtime path")
                command = [debugfs, "-R", f'rdump / "{quoted}"', str(payload)]
            result = subprocess.run(command, capture_output=True, text=True)
            # debugfs may exit zero even if it could not open the filesystem.
            if result.returncode or not (output / "apex_manifest.pb").is_file():
                raise RuntimeError(f"{archive.name}: {result.stdout[-2000:]}{result.stderr[-2000:]}")
            output.rename(destination)
            expanded += 1
            print(f"expanded {archive.stem} ({'erofs' if is_erofs else 'ext4'})", flush=True)
    print(f"APEX packages: {expanded} expanded, {retained} existing packages retained")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--erofs", default=shutil.which("fsck.erofs"))
    args = parser.parse_args()
    expand(args.root, args.erofs)


if __name__ == "__main__":
    main()
