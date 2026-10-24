#!/usr/bin/env python3
"""Count UTF-16/UTF-8 occurrences of strings in an APK's binary manifest.

Usage: manifestgrep.py <apk> <needle> [<needle> ...]
"""
import sys
import zipfile


def main():
    apk = sys.argv[1]
    data = zipfile.ZipFile(apk).read("AndroidManifest.xml")
    for needle in sys.argv[2:]:
        raw = needle.encode()
        utf16 = needle.encode("utf-16-le")
        print(f"{needle}: raw={data.count(raw)} utf16={data.count(utf16)}")


if __name__ == "__main__":
    main()
