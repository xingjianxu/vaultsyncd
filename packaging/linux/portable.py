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
import tarfile

PROVENANCE, RUST, WEB, OUTPUT, SOURCES = map(Path, sys.argv[1:])
VAULT=OUTPUT/'runtime'
data=json.loads(PROVENANCE.read_text())
assert data['kind']=='nix-component-set' and len(data['sourceInfo']['commit'])==40
data['kind']='portable-linux-private-loader'
data['runtime_requirements']=['Linux kernel with btrfs and CoW', '/bin/sh', 'coreutils', 'Python >= 3.11', 'util-linux findmnt', 'btrfs-progs >= 6.6']
for name in ('bin','lib','libexec','share/vaultsyncd'): (VAULT/name).mkdir(parents=True,exist_ok=True)

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
        copy_elf(found,VAULT/'lib'/soname)

for name in ('vaultsyncd-server','vaultsyncd-admin'):
    source=RUST/'bin'/name
    interpreter=Path(patchelf('--print-interpreter',source))
    assert any(interpreter.resolve().is_relative_to(p) for p in allowed)
    loader=interpreter.name
    if loader not in seen:
        seen[loader]=sha(interpreter);copy_elf(interpreter,VAULT/'lib'/loader)
    copy_elf(source,VAULT/'libexec'/f'{name}.elf')
    # The shell wrapper invokes the bundled loader explicitly. The ELF itself
    # keeps the standard target interpreter, never a /nix/store interpreter.
    standard='/lib64/'+loader if data['target'].startswith('x86_64-') else '/lib/'+loader
    patchelf('--set-interpreter',standard,VAULT/'libexec'/f'{name}.elf')
    wrapper=Path(os.environ['launchTemplate']).read_text().replace('@LOADER@',loader)
    (VAULT/'bin'/name).write_text(wrapper);(VAULT/'bin'/name).chmod(0o755)

shutil.copytree(WEB,VAULT/'share/vaultsyncd/web')
shutil.copy2(os.environ['preflight'],VAULT/'share/vaultsyncd/preflight.py')
shutil.copy2(os.environ['unit'],VAULT/'share/vaultsyncd/vaultsyncd.service')
shutil.copy2(os.environ['exampleConfig'],VAULT/'share/vaultsyncd/vaultsyncd.toml')
shutil.copytree(os.environ['containerTemplates'],VAULT/'share/vaultsyncd/container')
licenses=VAULT/'share/vaultsyncd/licenses';licenses.mkdir()
for variable,names in [('glibcSource',['COPYINGv2','COPYINGv3','COPYING.LIB','COPYING.LESSERv2']),('gccSource',['COPYING3','COPYING.RUNTIME'])]:
    with tarfile.open(os.environ[variable]) as archive:
        for name in names:
            matches=[m for m in archive.getmembers() if len(Path(m.name).parts)==2 and Path(m.name).name==name]
            assert len(matches)==1
            with archive.extractfile(matches[0]) as incoming,(licenses/(variable+'-'+name)).open('wb') as dest:
                shutil.copyfileobj(incoming,dest)
data['runtime_toolchain']=json.loads(os.environ['runtimeVersions'])
data['runtime_libraries']=provenance
data['corresponding_source_archive']={'name':SOURCES.name,'sha256':sha(SOURCES)}
data['files']=[{'path':str(p.relative_to(VAULT)),'size':str(p.stat().st_size),'sha256':sha(p)} for p in sorted(VAULT.rglob('*')) if p.is_file()]
(VAULT/'share/vaultsyncd/release-manifest.json').write_text(json.dumps(data,sort_keys=True,ensure_ascii=False,indent=2)+'\n')
assert not any(p.is_symlink() for p in VAULT.rglob('*'))
for p in [*VAULT.joinpath('lib').iterdir(),*VAULT.joinpath('libexec').iterdir()]:
    assert not patchelf('--print-rpath',p)
    assert all('/' not in name for name in patchelf('--print-needed',p).splitlines())
