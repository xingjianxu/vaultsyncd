"""Read-only service-UID checks. Root admission remains authoritative at registration."""
import fcntl
import json
import os
from pathlib import Path
import stat
import subprocess
import sys


def fail(message):
    raise SystemExit(f"vaultsyncd preflight: {message}")


def inspect(path, *, private=False):
    p = Path(path)
    if not p.is_absolute() or p.resolve(strict=True) != p or not p.is_dir():
        fail(f"not a real canonical directory: {p}")
    info = p.stat()
    if private and (info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) & 0o077):
        fail(f"state must be owned by service UID and private (0700): {p}")
    if not os.access(p, os.R_OK | os.W_OK | os.X_OK):
        fail(f"explicit read/write/traverse access required: {p}")
    mount = json.loads(subprocess.check_output(
        [sys.argv[1], "--json", "--target", str(p), "--output", "FSTYPE,OPTIONS"]
    ))["filesystems"][0]
    options = set(mount["options"].split(","))
    if mount["fstype"] != "btrfs" or "ro" in options:
        fail(f"writable btrfs required: {p}")
    if options & {"nodatacow", "nodatasum"}:
        fail(f"CoW and checksums required: {p}")
    if "user_subvol_rm_allowed" not in options:
        fail(f"mount needs user_subvol_rm_allowed, no privileged fallback: {p}")
    fd = os.open(p, os.O_RDONLY | os.O_DIRECTORY)
    try:
        flags = bytearray(8)
        fcntl.ioctl(fd, 0x80086601, flags)  # FS_IOC_GETFLAGS, Linux x86_64
        if int.from_bytes(flags, sys.byteorder) & 0x00800000:  # FS_NOCOW_FL
            fail(f"NOCOW directory rejected: {p}")
    finally:
        os.close(fd)


if os.geteuid() == 0:
    fail("must run as a dedicated non-root service UID")
configuration = json.loads(Path(sys.argv[2]).read_text())
inspect(configuration["stateDir"], private=True)
backup = Path(configuration["stateDir"]) / "backups"
if backup.exists() or backup.is_symlink():
    inspect(str(backup), private=True)
for root in configuration["rootPaths"]:
    inspect(root)
print("vaultsyncd preflight: btrfs/CoW/access/mount options verified; no permissions changed")
