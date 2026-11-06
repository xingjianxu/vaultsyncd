#!/usr/bin/env python3
"""Assemble a Nix-native component manifest; deliberately not an S093 Git worktree receipt."""
import hashlib
import json
import re
import sys
from pathlib import Path

def main():
    provenance=Path(sys.argv[1]);root=Path(sys.argv[2])
    data=json.loads(provenance.read_text())
    assert data["kind"]=="nix-component-set" and re.fullmatch("[0-9a-f]{40}",data["sourceInfo"]["commit"])
    assert data["sourceInfo"]["narHash"].startswith("sha256-")
    files=[]
    def add(path):
        digest=hashlib.sha256()
        with path.open("rb") as source:
            for block in iter(lambda:source.read(65536),b""):digest.update(block)
        files.append({"path":str(path.relative_to(root)),"size":str(path.stat().st_size),"sha256":digest.hexdigest()})
    for name in ["vaultsyncd-server","vaultsyncd-admin"]:
        path=root/"bin"/name
        assert path.is_file()
        add(path)
    web=root/"share/vaultsyncd/web"
    assert (web/"index.html").is_file() and (web/"assets").is_dir()
    for path in sorted(web.rglob("*")):
        if path.is_file():add(path)
    data["files"]=files
    (root/"share/vaultsyncd/release-manifest.json").write_text(json.dumps(data,ensure_ascii=False,sort_keys=True,indent=2)+"\n")

if __name__=="__main__":main()
