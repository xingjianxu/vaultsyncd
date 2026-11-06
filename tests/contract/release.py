#!/usr/bin/env python3
"""Real frontend builds and release receipt failure controls; run in devShell after committing changes."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
BIN = TARGET / "debug/xtask"
OUT = ROOT / "artifacts/S093" / str(time.time_ns())
MIRRORS = ["package.json","packages/ui/package.json","packages/api-client/package.json","packages/local-ipc-types/package.json","apps/web/package.json","apps/desktop/package.json","apps/desktop/src-tauri/tauri.conf.json"]


def call(args, expected=0, contains=None):
    result = subprocess.run([str(arg) for arg in args], cwd=ROOT, text=True, capture_output=True, timeout=180)
    text = result.stdout + result.stderr
    (OUT / f"command-{len(list(OUT.glob('command-*'))):03}.log").write_text(text)
    assert result.returncode == expected, text
    if contains: assert contains in text, text
    return text


def main():
    OUT.mkdir(parents=True)
    call([BIN,"version","--check"])
    stale=ROOT/"apps/web/dist/old-release.js"; stale.parent.mkdir(parents=True,exist_ok=True); stale.write_text("stale asset")
    for component in ["web","desktop-ui"]:
        call([BIN,"release","build",component,OUT/component])
    assert not (OUT/"web/payload/old-release.js").exists()
    manifest=OUT/"manifest.json"
    call([BIN,"release","assemble",manifest,OUT/"web",OUT/"desktop-ui"])
    call([BIN,"release","verify",manifest])
    data=json.loads(manifest.read_text())
    commit=subprocess.check_output(["git","rev-parse","HEAD"],cwd=ROOT,text=True).strip()
    assert data["source"]["commit"]==commit
    for component in data["components"]:
        receipt=component["receipt"]
        assert receipt["toolchain"] and receipt["command"] and receipt["preparation"] and receipt["target"]=="browser-static"
        for name,digest in receipt["source"]["inputs"].items():
            assert hashlib.sha256((ROOT/name).read_bytes()).hexdigest()==digest
        for item in receipt["files"]:
            actual=Path(component["directory"])/"payload"/item["path"]
            assert actual.stat().st_size==item["bytes"] and hashlib.sha256(actual.read_bytes()).hexdigest()==item["sha256"]
    call([BIN,"release","build","web",OUT/"web"],1,"output exists")
    component=data["components"][0]; item=component["receipt"]["files"][0]
    payload=Path(component["directory"])/"payload"/item["path"]; old=payload.read_bytes()
    try:
        payload.write_bytes(old+b"tampered")
        call([BIN,"release","verify",manifest],1,"hash/size/file-set mismatch")
    finally: payload.write_bytes(old)
    old_manifest=manifest.read_bytes()
    try:
        data["source"]["commit"]="0"*40;manifest.write_text(json.dumps(data))
        call([BIN,"release","verify",manifest],1,"stale")
    finally: manifest.write_bytes(old_manifest)
    source=ROOT/"apps/web/src/App.tsx"; old_source=source.read_bytes()
    try:
        source.write_bytes(old_source+b"\n// resource changed\n")
        call([BIN,"release","verify",manifest],1,"clean committed")
        call([BIN,"release","build","web",OUT/"dirty"],1,"clean committed")
    finally: source.write_bytes(old_source)
    paths=[ROOT/name for name in MIRRORS+["Cargo.toml","Cargo.lock"]]
    saved={path:path.read_bytes() for path in paths}
    generated=[ROOT/name for name in ["crates/protocol/src/generated/mod.rs","crates/local-ipc/src/generated/mod.rs","packages/api-client/src/generated/types.ts","packages/api-client/src/generated/client.ts","packages/local-ipc-types/src/generated/types.ts"]]
    saved_generated={path:path.read_bytes() for path in generated}
    try:
        cargo=ROOT/"Cargo.toml";text=cargo.read_text();version=data["source"]["version"]
        pieces=version.split(".");pieces[2]=str(int(pieces[2])+1);next_version=".".join(pieces)
        cargo.write_text(text.replace('version = "'+version+'"','version = "'+next_version+'"',1))
        call([BIN,"version","--check"],1,"版本不同步")
        call([BIN,"version","--sync"])
        assert all(json.loads(path.read_text())["version"]==next_version for path in paths[:len(MIRRORS)])
        call([BIN,"release","verify",manifest],1,"clean committed")
        # Build xtask with the new product package version to prove generator identity is independent.
        call(["cargo","update","--workspace","--offline"])
        call(["cargo","build","--locked","--offline","-p","vaultsyncd-xtask"])
        call([BIN,"generate","--check"])
        assert all(path.read_bytes()==old for path,old in saved_generated.items())
    finally:
        for path,content in saved.items(): path.write_bytes(content)
        for path,content in saved_generated.items(): assert path.read_bytes()==content
    call([BIN,"release","verify",manifest])
    (OUT/"result.json").write_text(json.dumps({"passed":True,"commit":commit,"checks":["real-web-and-desktop-ui-build","source-and-lock-hashes","toolchain-and-target","stale-dist-deleted","existing-output-refused","payload-mutation-refused","old-commit-refused","changed-resource-refused","product-version-sync","generator-version-independent-after-rebuild","restored-manifest-valid"]},indent=2))
    print(f"S093 real release acceptance passed: {OUT}")


if __name__=="__main__": main()
