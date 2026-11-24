#!/usr/bin/env python3
"""Print a normalized manifest of a directory tree for comparison.

Usage: manifest.py DIR [--no-mode] [--skip NAME]...

Each line is `path type [mode] details`. Names are NFD-normalized (as HFS+
stores them); top-level Finder metadata can be skipped with --skip.
"""
import hashlib
import os
import stat
import sys
import unicodedata


def main() -> None:
    args = sys.argv[1:]
    if not args:
        sys.exit(__doc__)
    root = args[0]
    no_mode = "--no-mode" in args
    skip = {args[i + 1] for i, a in enumerate(args) if a == "--skip"}
    lines = []
    for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
        rel_dir = os.path.relpath(dirpath, root)
        if rel_dir == ".":
            dirnames[:] = [d for d in dirnames if d not in skip]
            filenames = [f for f in filenames if f not in skip]
        for name in sorted(dirnames + filenames):
            path = os.path.join(dirpath, name)
            rel = os.path.normpath(os.path.join(rel_dir, name)).replace(os.sep, "/")
            rel = unicodedata.normalize("NFD", rel)
            st = os.lstat(path)
            mode = "" if no_mode else " %04o" % stat.S_IMODE(st.st_mode)
            if stat.S_ISLNK(st.st_mode):
                lines.append(f"{rel} link -> {os.readlink(path)}")
            elif stat.S_ISDIR(st.st_mode):
                lines.append(f"{rel} dir{mode}")
            else:
                h = hashlib.sha256()
                with open(path, "rb") as f:
                    for block in iter(lambda: f.read(1 << 20), b""):
                        h.update(block)
                lines.append(f"{rel} file{mode} {st.st_size} {h.hexdigest()}")
    lines.sort()
    sys.stdout.buffer.write(("\n".join(lines) + "\n").encode("utf-8"))


if __name__ == "__main__":
    main()
