#!/usr/bin/env python3
"""Independently read the actual NTFS package copied to the host, without executing its contents."""
import hashlib
import json
import pathlib
import sys

run = pathlib.Path(sys.argv[1])
report = json.loads((run / "result.json").read_text())
package = run / "recovery-package"
manifest = json.loads((package / "manifest.json").read_text())
assert report["ok"] and manifest["format"] == 1
assert manifest["binding"] == report["binding"]
assert not package.is_symlink()
names = set()
for item in manifest["files"]:
    name = item["name"]
    assert name not in names and name not in ("", ".", "..", "manifest.json")
    assert not any(part in name for part in "/\\:")
    names.add(name)
    file = package / name
    assert file.is_file() and not file.is_symlink()
    data = file.read_bytes()
    assert len(data) == item["size"]
    assert hashlib.sha256(data).hexdigest() == item["sha256"]
    source = item.get("source")
    if source:
        assert source["kind"] in ("current", "snapshot", "candidate")
        assert len(source["id"]) == 32 and all(c in "0123456789abcdef" for c in source["id"])
        assert name == f'{source["kind"]}-{source["id"]}.bin'
        assert source.get("size") in (None, item["size"])
        assert source.get("digest") in (None, item["sha256"])
    else:
        assert name == "state.json"
assert "state.json" in names
state = json.loads((package / "state.json").read_text())
assert state["binding"]["id"].lower() == manifest["binding"]
assert state["local_intents"] and state["upload_resumes"] and state["pending_ops"]
assert len(report["manual_recovered"]) == 4
for recovered in report["manual_recovered"]:
    name = pathlib.PureWindowsPath(recovered["restored_path"]).name
    file = run / "manual-recovered" / name
    assert file.is_file() and not file.is_symlink()
    assert hashlib.sha256(file.read_bytes()).hexdigest() == recovered["sha256"]
    source = package / recovered["package_name"]
    assert source.read_bytes() == file.read_bytes()
summary = {"ok": True, "verified_files": len(names), "manual_recovered": 4,
           "durable_intents": len(state["local_intents"]),
           "fixed_upload_resumes": len(state["upload_resumes"]),
           "roles": [item["kind"] for item in report["manual_recovered"]]}
(run / "independent-proof.json").write_text(json.dumps(summary, indent=2))
print("PASS independent package and manual retrieval:", json.dumps(summary))
