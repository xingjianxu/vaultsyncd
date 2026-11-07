#!/usr/bin/env python3
"""Build from a verified portable runtime and immutable Ubuntu payload lock.
No host/global dependencies installed. Every build RUN has networking disabled.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import urllib.request

def sha(path):
    h=hashlib.sha256()
    with path.open('rb') as f:
        for b in iter(lambda:f.read(1024*1024),b''):h.update(b)
    return h.hexdigest()

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('runtime',type=Path)
    parser.add_argument('image')
    parser.add_argument('--engine',choices=['podman','docker'],default='podman')
    parser.add_argument('--deps',type=Path,help='optional previously fetched .deb cache; missing files downloaded by exact URL/hash')
    parser.add_argument('--sources',type=Path,help='matching runtime sources archive, default adjacent to runtime')
    parser.add_argument('--receipt',type=Path,required=True)
    parser.add_argument('--offline',action='store_true',help='require the exact base digest and every locked .deb already cached')
    args=parser.parse_args()
    root=args.runtime.resolve(strict=True)
    manifest_path=root/'share/vaultsyncd/release-manifest.json'
    manifest=json.loads(manifest_path.read_text())
    assert manifest['kind']=='portable-linux-private-loader'
    sources=args.sources or root.parent/manifest['corresponding_source_archive']['name']
    assert sha(sources)==manifest['corresponding_source_archive']['sha256'], 'keep the matching runtime sources archive available'
    expected={e['path'] for e in manifest['files']}|{'share/vaultsyncd/release-manifest.json'}
    actual={str(p.relative_to(root)) for p in root.rglob('*') if p.is_file()}
    assert expected==actual and not any(p.is_symlink() for p in root.rglob('*')), 'unexpected/missing runtime files'
    for e in manifest['files']:
        p=(root/e['path']).resolve(strict=True)
        assert p.is_relative_to(root) and p.stat().st_size==int(e['size']) and sha(p)==e['sha256'], 'stale/tampered runtime'
    assert manifest['target']=='x86_64-unknown-linux-gnu', 'only Linux x64 runtimes are supported'
    arch='amd64'
    lock=json.loads((root/'share/vaultsyncd/container/ubuntu-lock.json').read_text())
    base='docker.io/library/ubuntu@'+lock['base_manifests'][arch]
    cached=subprocess.run([args.engine,'image','inspect',base],capture_output=True,text=True)
    base_cached=cached.returncode==0
    if not base_cached:
        assert not args.offline, 'exact Ubuntu base digest is not cached'
        subprocess.run([args.engine,'pull','--platform','linux/'+arch,base],check=True)
        cached=subprocess.run([args.engine,'image','inspect',base],capture_output=True,text=True,check=True)
    base_facts=json.loads(cached.stdout)[0]
    digest=lock['base_manifests'][arch]
    assert base_facts['Architecture']==arch and (base_facts.get('Digest')==digest or any(
        value.endswith('@'+digest) for value in base_facts.get('RepoDigests',[]))), 'cached Ubuntu base identity mismatch'
    with tempfile.TemporaryDirectory(prefix='vaultsyncd-image-') as temp:
        context=Path(temp);shutil.copytree(root,context/'runtime')
        shutil.copy2(root/'share/vaultsyncd/container/Dockerfile',context/'Dockerfile')
        deps=context/'apt-debs';deps.mkdir()
        for e in lock['packages'][arch]:
            assert Path(e['filename']).name==e['filename']
            dest=deps/e['filename']
            cached=args.deps/e['filename'] if args.deps else None
            if cached and cached.is_file():shutil.copyfile(cached,dest)
            else:
                assert not args.offline, 'locked Ubuntu dependency is not cached: '+e['filename']
                with urllib.request.urlopen(e['url'],timeout=120) as incoming,dest.open('xb') as out:
                    shutil.copyfileobj(incoming,out)
            assert sha(dest)==e['sha256'],'Ubuntu dependency checksum mismatch'
        format_args=['--format=docker'] if args.engine=='podman' else []
        subprocess.run([args.engine,'build',*format_args,'--network=none','--pull=never','--platform','linux/'+arch,
            '--build-arg','BASE_IMAGE='+base,'--build-arg','PRODUCT_VERSION='+manifest['version'],
            '--build-arg','SOURCE_COMMIT='+manifest['sourceInfo']['commit'],'-t',args.image,str(context)],check=True)
    inspected=json.loads(subprocess.check_output([args.engine,'image','inspect',args.image],text=True))[0]
    receipt={'format':1,'image':args.image,'image_id':inspected['Id'],'architecture':arch,'base':base,
             'offline':args.offline,'base_cached_verified':base_cached,
             'sourceInfo':manifest['sourceInfo'],'version':manifest['version'],'runtime_manifest_sha256':sha(manifest_path),
             'ubuntu_lock_sha256':sha(root/'share/vaultsyncd/container/ubuntu-lock.json')}
    assert inspected['Architecture']==arch and inspected['Config']['User']=='8721:8721'
    health=inspected.get('Healthcheck') or inspected['Config'].get('Healthcheck')
    assert health and health['Test'], 'image must retain the declared health check'
    receipt['healthcheck']=health
    args.receipt.write_text(json.dumps(receipt,sort_keys=True,indent=2)+'\n')
    print(json.dumps(receipt,sort_keys=True))

if __name__=='__main__':main()
