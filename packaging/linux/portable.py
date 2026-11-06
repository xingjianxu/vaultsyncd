#!/usr/bin/env python3
"""Assemble relocatable ELF dependencies from a single locked component set.
Never execute input binaries, so the same builder supports cross targets.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

PROVENANCE, RUST, WEB, OUTPUT, SOURCES = map(Path, sys.argv[1:])
ROOT=OUTPUT/'runtime'
data=json.loads(PROVENANCE.read_text())
assert data['kind']=='nix-component-set' and len(data['sourceInfo']['commit'])==40
data['kind']='portable-linux-private-loader'
data['runtime_requirements']=['Linux kernel with btrfs and CoW', '/bin/sh', 'coreutils', 'Python >= 3.11', 'util-linux findmnt', 'btrfs-progs >= 6.6']
for name in ('bin','lib','libexec','share/vaultsyncd'): (ROOT/name).mkdir(parents=True,exist_ok=True)

def patchelf(*args):
    return subprocess.check_output(['patchelf',*map(str,args)],text=True).strip()

def sha(path):
    digest=hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda:f.read(1024*1024),b''):digest.update(block)
    return digest.hexdigest()

allowed=[Path(p).resolve() for p in os.environ['runtimeLibraryRoots'].split(':')]
seen={}
provenance=[]
def copy_elf(source,dest):
    original=source.resolve(strict=True)
    shutil.copy2(original,dest);dest.chmod(0o755)
    needed=patchelf('--print-needed',original).splitlines()
    paths=[Path(p) for p in patchelf('--print-rpath',original).split(':') if p]+allowed
    patchelf('--remove-rpath',dest)
    for name in needed:
        candidates=[Path(name)] if '/' in name else [p/name for p in paths]
        found=next((p.resolve() for p in candidates if p.is_file()),None)
        if found is None or not any(found.is_relative_to(p) for p in allowed):
            raise RuntimeError(f'unknown runtime dependency: {name}')
        soname=Path(name).name
        if '/' in name:patchelf('--replace-needed',name,soname,dest)
        if soname in seen:
            assert seen[soname]==sha(found),f'conflicting SONAME {soname}'
            continue
        seen[soname]=sha(found)
        provenance.append({'name':soname,'input_sha256':seen[soname]})
        copy_elf(found,ROOT/'lib'/soname)

for name in ('vaultsyncd-server','vaultsyncd-admin'):
    source=RUST/'bin'/name
    interpreter=Path(patchelf('--print-interpreter',source))
    assert any(interpreter.resolve().is_relative_to(p) for p in allowed)
    loader=interpreter.name
    if loader not in seen:
        seen[loader]=sha(interpreter);copy_elf(interpreter,ROOT/'lib'/loader)
    copy_elf(source,ROOT/'libexec'/f'{name}.elf')
    # The shell wrapper invokes the bundled loader explicitly. The ELF itself
    # keeps the standard target interpreter, never a /nix/store interpreter.
    standard='/lib64/'+loader if data['target'].startswith('x86_64-') else '/lib/'+loader
    patchelf('--set-interpreter',standard,ROOT/'libexec'/f'{name}.elf')
    wrapper=Path(os.environ['launchTemplate']).read_text().replace('@LOADER@',loader)
    (ROOT/'bin'/name).write_text(wrapper);(ROOT/'bin'/name).chmod(0o755)

shutil.copytree(WEB,ROOT/'share/vaultsyncd/web')
shutil.copy2(os.environ['preflight'],ROOT/'share/vaultsyncd/preflight.py')
shutil.copy2(os.environ['unit'],ROOT/'share/vaultsyncd/vaultsyncd.service')
shutil.copy2(os.environ['exampleConfig'],ROOT/'share/vaultsyncd/vaultsyncd.toml')
data['runtime_libraries']=provenance
data['corresponding_source_archive']={'name':SOURCES.name,'sha256':sha(SOURCES)}
data['files']=[{'path':str(p.relative_to(ROOT)),'size':str(p.stat().st_size),'sha256':sha(p)} for p in sorted(ROOT.rglob('*')) if p.is_file()]
(ROOT/'share/vaultsyncd/release-manifest.json').write_text(json.dumps(data,sort_keys=True,ensure_ascii=False,indent=2)+'\n')
assert not any(p.is_symlink() for p in ROOT.rglob('*'))
for p in [*ROOT.joinpath('lib').iterdir(),*ROOT.joinpath('libexec').iterdir()]:
    assert not patchelf('--print-rpath',p)
    assert all('/' not in name for name in patchelf('--print-needed',p).splitlines())
