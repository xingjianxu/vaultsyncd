#!/usr/bin/env python3
"""Exclusive same-commit installation controller for a pre-provisioned lab.

Config JSON: evidence (new absolute /data directory), arm64_template (stopped
S098 ubuntu-arm64-tcg directory). Windows uses the documented SSH environment.
Never modifies the template, an existing guest disk, or host production services.
Run with VAULTSYNCD_CI_LAB=1; restore/cleanup failures fail the run.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

from run import plan

ROOT = Path(__file__).resolve().parents[2]


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
        print('BLOCKED: provision a private lab config with evidence and arm64_template paths')
        return 78
    config = json.loads(Path(args.config).read_text())
    evidence = Path(config["evidence"])
    assert evidence.is_absolute() and evidence.is_relative_to('/data') and not evidence.is_symlink()
    evidence.mkdir(mode=0o700, exist_ok=True)
    # All local controllers use the same lock; hosted workflow concurrency is additional.
    with Path('/tmp/vaultsyncd-platform-lab.lock').open('a') as lock:
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
            for job in ('release-linux', 'release-arm64'):
                for command in plan(job, {}):
                    outputs.extend(json.loads(run(command, evidence/(job+'.log'))))
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
                packages['arm64' if target.startswith('aarch64') else 'amd64'] = path
            elif (path/'share/vaultsyncd/release-manifest.json').is_file():
                packages['nixos'] = path
        receipt = json.loads((component/'receipt.json').read_text())
        assert receipt['source']['commit'] == commit
        run(['nix','develop','--command','cargo','xtask','release','assemble',evidence/'windows-release.json',component],evidence/'assemble.log')
        run(['nix','develop','--command','cargo','xtask','release','verify',evidence/'windows-release.json'],evidence/'verify.log')
        # Only our own handler and interpreter; never replace a pre-existing entry.
        handler = 'vs-s101-' + str(os.getpid())
        binfmt = Path('/proc/sys/fs/binfmt_misc')
        mounted = not (binfmt/'register').exists()
        registered = False
        vm_started = False
        vm = evidence/'ubuntu-arm64-tcg'
        sudo = ['sudo','-n']
        def privileged(*command):
            subprocess.run([*sudo,*map(str,command)],check=True,timeout=120)
        try:
            if mounted:
                privileged('mount','-t','binfmt_misc','binfmt_misc',binfmt)
            qemu = shutil.which('qemu-aarch64')
            assert qemu and os.access(qemu,os.X_OK), 'use nix develop .#portable-tests for lab phase'
            privileged('mkdir','-m','755','/run/'+handler)
            privileged('cp',qemu,'/run/'+handler+'/qemu-aarch64')
            registration = (f':{handler}:M::\\x7fELF\\x02\\x01\\x01\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x02\\x00\\xb7\\x00:'
                            f'\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\x00\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xfe\\xff\\xff\\xff:/run/{handler}/qemu-aarch64:F')
            subprocess.run([*sudo,'tee',str(binfmt/'register')],input=registration,text=True,check=True,stdout=subprocess.DEVNULL,timeout=30)
            registered=True
            images={}
            for arch in ('amd64','arm64'):
                package=packages[arch]
                deps=next(p for p in paths if p.name.endswith('vaultsyncd-ubuntu-'+arch+'-dependencies'))
                image=f'localhost/vaultsyncd-s098:s101-{commit[:12]}-{arch}'
                run(['python3','packaging/linux/container/build.py',package/'runtime',image,'--offline','--deps',deps,
                     '--receipt',evidence/f'image-{arch}.json'],evidence/f'image-{arch}.log')
                images[arch]=image
            run(['python3','tools/ci/run.py','nixos-install','--package',packages['nixos']],evidence/'nixos-install.log')
            run(['python3','tools/ci/run.py','ubuntu-install','--image',images['amd64'],'--evidence',evidence],evidence/'ubuntu-install.log')
            # Copy-on-write copies of the stopped, established ARM64 lab. This run
            # owns the copies only; provisioning a new guest is not CI logic.
            template=Path(config['arm64_template']).resolve(strict=True)
            meta=json.loads((template/'vm.json').read_text())
            assert not Path(f'/proc/{meta["pid"]}').exists(), 'template VM must be stopped'
            vm.mkdir(mode=0o700)
            for name in ['root.qcow2','data.raw','vars.fd','seed.iso','id_ed25519','known_hosts']:
                subprocess.run(['cp','--reflink=auto','--sparse=always',str(template/name),str(vm/name)],check=True,timeout=120)
            meta['args']=[v.replace(str(template),str(vm)) for v in meta['args']]
            # Record before launch for recovery even if controller is interrupted.
            (vm/'vm.json').write_text(json.dumps(meta,indent=2))
            run(meta['args'],evidence/'arm64-launch.log',120)
            meta['pid']=int((vm/'qemu.pid').read_text());vm_started=True
            (vm/'vm.json').write_text(json.dumps(meta,indent=2))
            ssh=['ssh','-o','BatchMode=yes','-o','ConnectTimeout=5','-o','StrictHostKeyChecking=yes',
                 '-o','UserKnownHostsFile='+str(vm/'known_hosts'),'-i',str(vm/'id_ed25519'),'-p',str(meta['ssh_port']),'ubuntu@127.0.0.1']
            for _ in range(180):
                probe=subprocess.run([*ssh,'uname -m'],capture_output=True,timeout=10)
                if probe.returncode==0:
                    assert probe.stdout.strip()==b'aarch64'
                    break
                time.sleep(2)
            else:
                raise RuntimeError('ARM64 guest SSH not ready')
            run(['python3','tools/ci/run.py','arm64-install','--vm',vm,'--image',images['arm64'],'--evidence',evidence],evidence/'arm64-install.log')
            # Fixture binaries are rebuilt at the exact source commit before the
            # Windows cross/native drivers (not copied from an old target).
            run(['nix','develop','--command','cargo','build','--locked','-p','vaultsyncd-server-tests',
                 '--example','e2e-native-server','--example','release-native-server'],evidence/'fixtures.log')
            for job in ['windows-cfapi','windows-e2e']:
                run(['nix','develop','.#windows','--command','cargo','xtask','ci',job],evidence/(job+'.log'),env=env)
            run(['nix','develop','.#web-tests','--command','python3','tests/platform/windows/s099-installation.py',component],evidence/'windows-install.log',env={**os.environ,'VAULTSYNCD_CI_CLEANUP':'1'})
            public=ROOT/'artifacts/S101/public';public.mkdir(parents=True,exist_ok=True)
            result={'status':'passed','source':source,'arm64':'full-system Ubuntu ARM64 TCG emulation, not native performance',
                    'windows':'Linux cross build / Windows 11 native install, WebView2 and CFAPI',
                    'hosted_ci':'blocked: no git remote or configured runners','products':products,
                    'windows_payload':{p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in (component/'payload').iterdir() if p.is_file()}}
            (public/'installation.json').write_text(json.dumps(result,ensure_ascii=False,indent=2))
        finally:
            try:
                if vm_started:
                    subprocess.run([*ssh,'sudo -n systemctl poweroff'],check=True,timeout=30)
                    for _ in range(90):
                        if not Path(f'/proc/{meta["pid"]}').exists():
                            break
                        time.sleep(2)
                    else:
                        raise RuntimeError('owned ARM64 VM did not shut down; preserve evidence')
            finally:
                if registered:
                    subprocess.run([*sudo,'tee',str(binfmt/handler)],input='-1',text=True,check=True,stdout=subprocess.DEVNULL,timeout=30)
                privileged('rm','-rf','/run/'+handler)
                if mounted:
                    privileged('umount',binfmt)
    return 0


if __name__=='__main__':
    sys.exit(main())
