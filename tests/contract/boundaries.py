#!/usr/bin/env python3
"""S092 executable negative controls. Run in an isolated worktree inside devShell.

Build xtask first, then: python3 tests/contract/boundaries.py
Mutations are local fixtures restored byte for byte, including Cargo.lock.
"""
import json
import os
from pathlib import Path
import subprocess

VAULT = Path(__file__).resolve().parents[2]
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", VAULT / "target"))
XTASK = TARGET / "debug/xtask"


def run(args):
    return subprocess.run([str(a) for a in args], cwd=VAULT, capture_output=True, text=True, timeout=180)


def main():
    gitdir = run(["git", "rev-parse", "--absolute-git-dir"])
    common = run(["git", "rev-parse", "--path-format=absolute", "--git-common-dir"])
    assert gitdir.returncode == common.returncode == 0 and gitdir.stdout != common.stdout, "requires isolated linked worktree"
    assert XTASK.is_file(), "build xtask first"
    artifacts = VAULT / "artifacts/S092"
    artifacts.mkdir(parents=True, exist_ok=True)
    lock = VAULT / "Cargo.lock"

    def negative(label, relative, transform, expected, cargo=False):
        path = VAULT / relative
        old, old_lock = path.read_bytes(), lock.read_bytes()
        try:
            path.write_text(transform(old.decode()))
            if cargo:
                resolved = run(["cargo", "metadata", "--offline", "--format-version", "1", "--filter-platform", "x86_64-pc-windows-msvc"])
                assert resolved.returncode == 0, resolved.stderr
            result = run([XTASK, "check-boundaries"])
            output = result.stdout + result.stderr
            (artifacts / (label + ".log")).write_text(output)
            assert result.returncode == 1, f"{label}: expected failure, got {result.returncode}"
            assert expected in output, f"{label}: wrong failure: {output[-2000:]}"
            print(f"negative control passed: {label}")
        finally:
            path.write_bytes(old)
            lock.write_bytes(old_lock)
            assert path.read_bytes() == old and lock.read_bytes() == old_lock

    dependency = 'vaultsyncd-client-store = { path = "../../crates/client-store" }'
    negative("server-client", "apps/server/Cargo.toml",
             lambda text: text.replace("\n[dependencies]\n", "\n[dependencies]\n" + dependency + "\n", 1),
             "vaultsyncd-server -> vaultsyncd-client-store", cargo=True)
    negative("optional-server-client", "apps/server/Cargo.toml",
             lambda text: text.replace("\n[dependencies]\n", "\n[dependencies]\nvaultsyncd-client-store = { path = \"../../crates/client-store\", optional = true }\n", 1),
             "vaultsyncd-server -> vaultsyncd-client-store", cargo=True)
    negative("windows-only-server-client", "apps/server/Cargo.toml",
             lambda text: text + "\n[target.'cfg(windows)'.dependencies]\n" + dependency + "\n",
             "边界违规 [x86_64-pc-windows-msvc]", cargo=True)
    negative("cli-store", "apps/cli/Cargo.toml",
             lambda text: text.replace("\n[dependencies]\n", "\n[dependencies]\n" + dependency + "\n", 1),
             "vaultsyncd-cli -> vaultsyncd-client-store", cargo=True)

    def ui_alias(text):
        manifest = json.loads(text)
        manifest["dependencies"] = {"boundary-alias": "npm:@tauri-apps/api@2.12.4"}
        return json.dumps(manifest)
    negative("ui-tauri-alias", "packages/ui/package.json", ui_alias, "@vaultsyncd/ui -> @tauri-apps/api")
    negative("cli-sql-write", "apps/cli/src/main.rs",
             lambda text: text + '\nconst BOUNDARY_NEGATIVE: &str = "INSERT INTO local VALUES (1)";\n',
             "GUI/CLI 数据库访问违规")
    negative("remote-generated", "packages/api-client/src/generated/types.ts",
             lambda text: text + "\n// negative generated drift\n", "重新生成有 diff")
    negative("ipc-generated", "crates/local-ipc/src/generated/mod.rs",
             lambda text: text + "\n// negative generated drift\n", "重新生成有 diff")
    negative("ts-consumer", "packages/local-ipc-types/src/index.ts",
             lambda text: text + "\nexport type BoundaryNegative = MissingBoundaryType;\n",
             "MissingBoundaryType")
    generated = [
        VAULT / "packages/api-client/src/generated/types.ts",
        VAULT / "packages/api-client/src/generated/client.ts",
        VAULT / "crates/protocol/src/generated/mod.rs",
        VAULT / "packages/local-ipc-types/src/generated/types.ts",
        VAULT / "crates/local-ipc/src/generated/mod.rs",
    ]
    before = {path: path.read_bytes() for path in generated}
    assert run([XTASK, "generate"]).returncode == 0
    assert all(path.read_bytes() == old for path, old in before.items()), "generation must be reproducible"
    final = run([XTASK, "check-boundaries"])
    (artifacts / "restored-positive.log").write_text(final.stdout + final.stderr)
    assert final.returncode == 0, final.stdout + final.stderr
    print("9 negative controls, byte-exact restoration, reproducible generation and restored positive gate passed")


if __name__ == "__main__":
    main()
