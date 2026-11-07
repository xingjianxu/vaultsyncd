#!/usr/bin/env python3
"""Exclusive same-commit installation controller for a pre-provisioned lab.

Config JSON: evidence (new absolute /data directory).
Windows uses the documented SSH environment. Never modifies host production services.
Run with VAULTSYNCD_CI_LAB=1; restore/cleanup failures fail the run.
Only Linux x64 and Windows x64 are in the current platform scope.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

from run import plan

ROOT = Path(__file__).resolve().parents[2]
EVIDENCE_ROOT = Path("/data")
LAB_LOCK = Path("/tmp/vaultsyncd-platform-lab.lock")


def run(command, log, timeout=3600, env=None):
    print("ACCEPTANCE_COMMAND " + json.dumps(list(map(str, command))), flush=True)
    with log.open("w") as output:
        result = subprocess.run(list(map(str, command)), cwd=ROOT, stdout=subprocess.PIPE,
                                stderr=output, text=True, timeout=timeout, env=env)
        output.write(result.stdout)
    if result.returncode:
        raise RuntimeError(f"exit {result.returncode}: {log}")
    return result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True)
    parser.add_argument("--phase", choices=["build", "lab", "all"], default="all")
    args = parser.parse_args()
    if os.environ.get("VAULTSYNCD_CI_LAB") != "1":
        print('BLOCKED: reserve the disposable lab and set VAULTSYNCD_CI_LAB=1')
        return 78
    if not Path(args.config).is_file():
        print('BLOCKED: provision a private lab config with an evidence path')
        return 78
    config = json.loads(Path(args.config).read_text())
    evidence = Path(config["evidence"])
    assert evidence.is_absolute() and evidence.is_relative_to(EVIDENCE_ROOT) and not evidence.is_symlink()
    evidence.mkdir(mode=0o700, exist_ok=True)
    # All local controllers use the same lock; hosted workflow concurrency is additional.
    with LAB_LOCK.open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
        porcelain = subprocess.check_output(['git', 'status', '--porcelain'], cwd=ROOT, text=True)
        assert porcelain == '', 'clean same-commit source required'
        source = {'commit': commit, 'porcelain': porcelain, 'host': os.uname().sysname,
                  'kernel': os.uname().release, 'architecture': os.uname().machine}
        (evidence/'source.json').write_text(json.dumps(source, indent=2))
        products_file = evidence/'products.json'
        component = ROOT/'artifacts/S101/windows-client'
        env = {**os.environ, 'VAULTSYNCD_WINDOWS_CACHE': os.environ.get('VAULTSYNCD_WINDOWS_CACHE', '/wrd/prj/vaultsyncd/.cache/windows')}
        if args.phase in ('build', 'all'):
            outputs = []
            for command in plan('release-linux', {}):
                outputs.extend(json.loads(run(command, evidence/'release-linux.log')))
            products = {'commit': commit, 'outputs': outputs, 'component': str(component)}
            products_file.write_text(json.dumps(products, indent=2))
            run(['nix', 'develop', '.#windows', '--command', 'cargo', 'xtask', 'ci', 'release-windows',
                 '--component', component], evidence/'windows-build.log', env=env)
        if args.phase == 'build':
            return 0
        products = json.loads(products_file.read_text())
        assert products['commit'] == commit
        paths = [Path(item['outputs']['out']) for item in products['outputs']]
        packages = {}
        for path in paths:
            if (path/'runtime').is_dir():
                manifest = json.loads((path/'runtime/share/vaultsyncd/release-manifest.json').read_text())
                assert manifest['sourceInfo']['commit'] == commit
                target = manifest['target']
                assert target == 'x86_64-unknown-linux-gnu', 'unsupported Linux target'
                packages['amd64'] = path
            elif (path/'share/vaultsyncd/release-manifest.json').is_file():
                packages['nixos'] = path
        receipt = json.loads((component/'receipt.json').read_text())
        assert receipt['source']['commit'] == commit
        run(['nix','develop','--command','cargo','xtask','release','assemble',evidence/'windows-release.json',component],evidence/'assemble.log')
        run(['nix','develop','--command','cargo','xtask','release','verify',evidence/'windows-release.json'],evidence/'verify.log')
        package=packages['amd64']
        deps=next(p for p in paths if p.name.endswith('vaultsyncd-ubuntu-amd64-dependencies'))
        image=f'localhost/vaultsyncd-s098:s101-{commit[:12]}-amd64'
        run(['python3','packaging/linux/container/build.py',package/'runtime',image,'--offline','--deps',deps,
             '--receipt',evidence/'image-amd64.json'],evidence/'image-amd64.log')
        run(['python3','tools/ci/run.py','nixos-install','--package',packages['nixos']],evidence/'nixos-install.log')
        run(['python3','tools/ci/run.py','ubuntu-install','--image',image,'--evidence',evidence],evidence/'ubuntu-install.log')
        # Fixture binaries are rebuilt at the exact source commit before the
        # Windows cross/native drivers (not copied from an old target).
        run(['nix','develop','--command','cargo','build','--locked','-p','vaultsyncd-server-tests',
             '--example','e2e-native-server','--example','release-native-server'],evidence/'fixtures.log')
        for job in ['windows-cfapi','windows-e2e']:
            run(['nix','develop','.#windows','--command','cargo','xtask','ci',job],evidence/(job+'.log'),env=env)
        run(['nix','develop','.#web-tests','--command','python3','tests/platform/windows/s099-installation.py',component],evidence/'windows-install.log',env={**os.environ,'VAULTSYNCD_CI_CLEANUP':'1'})
        public=ROOT/'artifacts/S101/public';public.mkdir(parents=True,exist_ok=True)
        result={'status':'passed','source':source,'linux':'NixOS x64 and Ubuntu x64 container',
                'windows':'Linux cross build / Windows 11 native install, WebView2 and CFAPI',
                'hosted_ci':'blocked: no git remote or configured runners','products':products,
                'windows_payload':{p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in (component/'payload').iterdir() if p.is_file()}}
        (public/'installation.json').write_text(json.dumps(result,ensure_ascii=False,indent=2))
    return 0


if __name__=='__main__':
    sys.exit(main())
