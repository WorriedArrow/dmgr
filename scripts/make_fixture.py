#!/usr/bin/env python3
"""Create a deterministic directory tree used to test dmgr on every platform.

Usage: make_fixture.py OUT_DIR [--no-symlinks] [--big] [--many N]
"""
import os
import random
import sys


def main() -> None:
    args = sys.argv[1:]
    if not args:
        sys.exit(__doc__)
    out = args[0]
    symlinks = "--no-symlinks" not in args
    big = "--big" in args
    many = 3000
    if "--many" in args:
        many = int(args[args.index("--many") + 1])
    windows = os.name == "nt"
    rnd = random.Random(1234)

    def write(rel: str, data: bytes, mode: int = 0o644) -> None:
        path = os.path.join(out, *rel.split("/"))
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "wb") as f:
            f.write(data)
        if not windows:
            os.chmod(path, mode)

    def mkdir(rel: str) -> None:
        os.makedirs(os.path.join(out, *rel.split("/")), exist_ok=True)

    os.makedirs(out, exist_ok=True)

    # An application bundle.
    write("Fixture.app/Contents/Info.plist",
          b'<?xml version="1.0" encoding="UTF-8"?>\n<plist version="1.0"><dict>'
          b"<key>CFBundleExecutable</key><string>Fixture</string></dict></plist>\n")
    macho = bytes([0xCF, 0xFA, 0xED, 0xFE]) + rnd.randbytes(1 << 20)
    write("Fixture.app/Contents/MacOS/Fixture", macho, 0o755)
    write("Fixture.app/Contents/Resources/run.sh", b"#!/bin/sh\necho fixture\n", 0o755)
    write("Fixture.app/Contents/Resources/random.bin", rnd.randbytes(5 << 20))
    write("Fixture.app/Contents/Resources/zeros.bin", bytes(8 << 20))
    write("Fixture.app/Contents/Resources/text.txt",
          b"".join(b"line %d of a compressible text file\n" % i for i in range(100_000)))
    write("Fixture.app/Contents/Frameworks/Lib.framework/Versions/A/Lib", b"\xca\xfe\xba\xbe" + rnd.randbytes(70_000), 0o755)

    # Names exercising Unicode, spaces and hidden files.
    write("README.txt", b"Read me.\n")
    write("Ünïcödé/café.txt", "café".encode())
    write("日本語/ファイル.txt", "ファイル".encode())
    write("emoji 🎉.txt", "🎉".encode())
    write("Ελληνικά.TXT", b"greek")
    write("한국어.txt", b"hangul")
    write("with spaces/and more spaces.txt", b"spaces")
    write(".hidden-file", b"hidden")
    write(".hidden-dir/inner.txt", b"inner")
    write("empty-file", b"")
    mkdir("empty-dir")
    write("readonly.txt", b"read only", 0o444)
    write("setuid-like", b"mode bits", 0o4750)
    long_len = 120 if windows else 255
    write("L" * (long_len - 4) + ".txt", b"long name")

    # Many files to force a multi-level catalog B-tree.
    for i in range(many):
        write(f"many/file-{i:05d}.txt", f"file {i}\n".encode())

    # Deep nesting.
    deep = "/".join(f"level{i}" for i in range(24))
    write(f"deep/{deep}/leaf.txt", b"deep leaf")

    if big:
        write("big/random-64m.bin", rnd.randbytes(64 << 20))
        write("big/sparse-256m.bin", bytes(256 << 20))

    if symlinks:
        def link(target: str, rel: str) -> None:
            path = os.path.join(out, *rel.split("/"))
            os.makedirs(os.path.dirname(path), exist_ok=True)
            os.symlink(target, path)

        link("README.txt", "link-to-readme")
        link("/Applications", "link-absolute")
        link("does/not/exist", "link-dangling")
        link("A", "Fixture.app/Contents/Frameworks/Lib.framework/Versions/Current")
        link("Versions/Current/Lib", "Fixture.app/Contents/Frameworks/Lib.framework/Lib")


if __name__ == "__main__":
    main()
