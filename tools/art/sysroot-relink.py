#!/usr/bin/env python3
"""Make an extracted Android tree self-contained by relativising symlinks.

`debugfs rdump` preserves Android's absolute symlinks (`/apex/...`,
`/system/...`), which point at mount points that do not exist on the host.
This rewrites every such link to a path *relative to the link*, resolved
inside the extracted root, so the tree works when used in place.

    sysroot-relink.py <root>

It also creates `<root>/apex -> system/apex` when the image stores the APEXes
under `/system/apex` (Waydroid/system-as-root images do).
"""
import os
import sys


def main(argv):
    if len(argv) != 2:
        print(__doc__)
        return 2
    root = os.path.abspath(argv[1])

    # Android normally has /apex as a mount point; the image keeps the APEXes
    # under /system/apex, so expose them at the top level too.
    apex = os.path.join(root, "apex")
    if not os.path.lexists(apex) and os.path.isdir(os.path.join(root, "system", "apex")):
        os.symlink("system/apex", apex)

    rewritten = broken = 0
    for dirpath, dirnames, filenames in os.walk(root):
        for name in dirnames + filenames:
            path = os.path.join(dirpath, name)
            if not os.path.islink(path):
                continue
            target = os.readlink(path)
            if not target.startswith("/"):
                continue
            mapped = os.path.join(root, target.lstrip("/"))
            if not os.path.exists(mapped):
                broken += 1
                continue
            rel = os.path.relpath(mapped, os.path.dirname(path))
            os.unlink(path)
            os.symlink(rel, path)
            rewritten += 1
    print(f"relinked {rewritten} absolute symlink(s); {broken} target(s) missing in the tree")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
