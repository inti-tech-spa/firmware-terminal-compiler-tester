#!/usr/bin/env python3
"""Create a deterministic tar.xz containing one directory tree."""

import pathlib
import stat
import sys
import tarfile


destination = pathlib.Path(sys.argv[1]).resolve()
root = pathlib.Path(sys.argv[2]).resolve()

with tarfile.open(destination, "w:xz", format=tarfile.GNU_FORMAT) as archive:
    paths = [root, *sorted(root.rglob("*"), key=lambda path: path.as_posix())]
    for path in paths:
        metadata = path.lstat()
        if not (stat.S_ISDIR(metadata.st_mode) or stat.S_ISREG(metadata.st_mode)):
            raise SystemExit(f"unsupported package entry: {path}")
        info = archive.gettarinfo(str(path), arcname=str(path.relative_to(root.parent)))
        info.uid = 0
        info.gid = 0
        info.uname = "root"
        info.gname = "wheel"
        info.mtime = 0
        if info.isfile():
            with path.open("rb") as source:
                archive.addfile(info, source)
        else:
            archive.addfile(info)
