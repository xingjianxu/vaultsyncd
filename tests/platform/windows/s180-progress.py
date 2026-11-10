#!/usr/bin/env python3
"""Inspect the real Shell progress dialog in the interactive Windows lab session."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sys
import time

spec = importlib.util.spec_from_file_location('update_lab', Path(__file__).with_name('s162-update.py'))
lab = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lab)

def main():
    binaries = Path(sys.argv[1]).resolve()
    run_id = 's180-' + str(time.time_ns())
    artifact = Path(os.environ['VAULTSYNCD_NATIVE_ARTIFACTS']) / run_id
    artifact.resolve().relative_to('/data/vaultsyncd-dev')
    artifact.mkdir(parents=True)
    test = max((binaries / 'deps').glob('vaultsyncd_platform_windows-*.exe'), key=lambda p: p.stat().st_mtime)
    (artifact / 'platform-tests.exe').write_bytes(test.read_bytes())
    (artifact / 's180-progress.ps1').write_text(Path(__file__).with_suffix('.ps1').read_text(), encoding='utf-8-sig')
    (artifact / 'hashes.json').write_text(json.dumps({p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in artifact.iterdir()}))
    guest = 'C:/vaultsyncd-lab/' + run_id
    lab.remote(f"New-Item -ItemType Directory -Path '{guest}' | Out-Null")
    lab.run([*lab.SCP, *artifact.iterdir(), f'Quickemu@127.0.0.1:{guest}/'])
    try:
        lab.execute_test(f"& '{guest}/s180-progress.ps1'", artifact / 'native.log')
    finally:
        lab.run([*lab.SCP, f'Quickemu@127.0.0.1:{guest}/*.json', f'Quickemu@127.0.0.1:{guest}/*.out', f'Quickemu@127.0.0.1:{guest}/*.err', str(artifact)])
        print('EVIDENCE', artifact, flush=True)

if __name__ == '__main__':
    main()
