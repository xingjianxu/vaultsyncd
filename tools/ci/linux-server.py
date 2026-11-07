#!/usr/bin/env python3
"""Run Linux tests as the caller on btrfs; provision only a disposable CI loop.

On an already btrfs-backed lab, use its existing fixture base. On stock hosted
Ubuntu, bind our fresh btrfs volume to the existing test suite's hard-coded
<repo>/target/s052-lab. No production mount, root service or source file changes.
"""
import os
from pathlib import Path
import subprocess
import tempfile
import shutil

ROOT = Path(__file__).resolve().parents[2]
PACKAGES = ['vaultsyncd-platform-linux-server', 'vaultsyncd-server-store', 'vaultsyncd-server-core',
            'vaultsyncd-server', 'vaultsyncd-admin', 'vaultsyncd-server-tests']


def main():
    def checked(*command):
        return subprocess.check_output(list(map(str, command)), text=True).strip()
    target = ROOT / 'target/s052-lab'
    target.mkdir(parents=True, exist_ok=True)
    backend = checked('stat', '-f', '-c', '%T', target)
    with tempfile.TemporaryDirectory(prefix='vaultsyncd-s101-', dir='/var/tmp') as base:
        base = Path(base)
        image = base/'volume.img'
        mount = base/'mount';mount.mkdir()
        loop = None
        bound = mounted = False
        def sudo(*command):
            return checked('sudo', '-n', *command)
        try:
            if backend != 'btrfs':
                if any(target.iterdir()):
                    raise RuntimeError('refusing to hide nonempty fixture directory under a mount')
                with image.open('xb') as output:
                    output.truncate(3 * 1024**3)
                loop = sudo('losetup', '--find', '--show', image)
                checked('nix', 'shell', '--inputs-from', str(ROOT), 'nixpkgs#btrfs-progs',
                        '-c', 'sudo', '-n', 'mkfs.btrfs', '-q', loop)
                sudo('mount', '-o', 'user_subvol_rm_allowed', loop, mount);mounted=True
                sudo('chown', f'{os.getuid()}:{os.getgid()}', mount)
                fixture = mount/'fixtures';fixture.mkdir()
                sudo('mount', '--bind', fixture, target);bound=True
                temporary = mount/'tmp';temporary.mkdir(mode=0o700)
            else:
                # Keep temp-based platform tests on real btrfs too, rather than
                # silently skipping them because host /tmp happens to be tmpfs.
                temporary = base/'tmp'
                if checked('stat', '-f', '-c', '%T', base) != 'btrfs':
                    raise RuntimeError('btrfs fixture exists but /var/tmp is not btrfs; use a dedicated btrfs runner')
                temporary.mkdir(mode=0o700, exist_ok=False)
            env = {**os.environ, 'TMPDIR': str(temporary), 'VAULTSYNCD_TEST_DIR': str(temporary)}
            command = ['cargo', 'test', '--locked']
            for package in PACKAGES:
                command += ['-p', package]
            print(f'Linux test storage: backend=btrfs fixture={target} temporary={temporary}', flush=True)
            return subprocess.run(command, cwd=ROOT, env=env, timeout=1800).returncode
        finally:
            if backend == 'btrfs' and 'temporary' in locals():
                # This directory was created exclusively by this invocation;
                # never touch other fixtures or another task's target contents.
                shutil.rmtree(temporary)
            if bound:
                sudo('umount', target)
            if mounted:
                sudo('umount', mount)
            if loop:
                sudo('losetup', '-d', loop)


if __name__ == '__main__':
    raise SystemExit(main())
