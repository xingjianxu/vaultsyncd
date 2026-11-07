#!/usr/bin/env python3
"""Create and verify the program-only ZIP from the same fresh cross-build as NSIS."""
import hashlib
import json
from pathlib import Path
import shutil
import sys
import zipfile

EXECUTABLES = (
    "vaultsyncd-agent.exe", "vaultsyncd-session.exe", "vaultsyncd-cli.exe",
    "vaultsyncd-desktop.exe", "vaultsyncd-zip.exe",
)
RESOURCES = ("Start vaultsyncd.cmd", "Stop vaultsyncd.cmd", "README-ZIP.txt")
FILES = (*EXECUTABLES, "installed-files.json", "UNSIGNED-TEST.txt", *RESOURCES)


def entry(path):
    data = path.read_bytes()
    return {"path": path.name, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}


def verify(archive, directory):
    expected = {p.name: p.read_bytes() for p in directory.iterdir() if p.is_file()}
    with zipfile.ZipFile(archive) as source:
        names = source.namelist()
        if sorted(names) != sorted(expected) or len(names) != len(set(names)):
            raise ValueError("ZIP file inventory mismatch")
        for name, data in expected.items():
            if source.read(name) != data:
                raise ValueError(f"ZIP content mismatch: {name}")
    manifest = json.loads(expected["zip-files.json"])
    if sorted(item["path"] for item in manifest["files"]) != sorted(FILES):
        raise ValueError("ZIP manifest inventory mismatch")
    for item in manifest["files"]:
        if entry(directory / item["path"]) != item:
            raise ValueError(f"ZIP manifest hash mismatch: {item['path']}")


def build(payload, launcher, resources, archive):
    directory = payload / "zip"
    if directory.exists() or archive.exists():
        raise ValueError("ZIP output exists; use a new component directory")
    directory.mkdir()
    for name in EXECUTABLES[:-1]:
        shutil.copyfile(payload / name, directory / name)
    shutil.copyfile(launcher, directory / EXECUTABLES[-1])
    for name in ("installed-files.json", "UNSIGNED-TEST.txt"):
        shutil.copyfile(payload / name, directory / name)
    for name in RESOURCES:
        shutil.copyfile(resources / name, directory / name)
    installed = json.loads((payload / "installed-files.json").read_text())
    manifest = {"format": 1, "source": installed["source"], "signature_status": "unsigned-test",
                "startup": "manual-only; no default Run or Start Menu registration; notifications opt-in",
                "state": "%LOCALAPPDATA%/vaultsyncd/agent", "files": [entry(directory / name) for name in FILES]}
    (directory / "zip-files.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    with zipfile.ZipFile(archive, "x", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as target:
        for path in sorted(directory.iterdir()):
            info = zipfile.ZipInfo(path.name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o100644 << 16
            target.writestr(info, path.read_bytes())
    verify(archive, directory)


if __name__ == "__main__":
    if len(sys.argv) != 5:
        raise SystemExit("usage: build.py <payload> <fresh-zip-exe> <resources> <new-zip>")
    build(*(Path(value) for value in sys.argv[1:]))
