#!/usr/bin/env python3
"""Failure and publication controls for the one-command release orchestrator."""
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("release_bundle", ROOT / "packaging/release.py")
RELEASE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RELEASE)


class ReleaseBundleTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.stage = self.root / ".partial"
        self.stage.mkdir()
        for name in ("work", "logs"):
            (self.stage / name).mkdir()
        self.identity = {"commit": "a" * 40, "tree": "b" * 40, "version": "0.1.1"}
        self.builder = RELEASE.Builder(self.stage, self.identity)

    def test_failed_command_preserves_log_without_success_manifest(self):
        with self.assertRaisesRegex(RuntimeError, "退出码 7"):
            self.builder.run(["bash", "-c", "echo build-failed >&2; exit 7"])
        self.assertIn("build-failed", (self.stage / "logs/01.log").read_text())
        self.assertFalse((self.stage / "release-manifest.json").exists())

    def test_changed_source_refuses_publication(self):
        with patch.object(RELEASE, "source", return_value={**self.identity, "commit": "c" * 40}):
            with self.assertRaisesRegex(ValueError, "源码发生变化"):
                RELEASE.publish(self.builder, ["linux-x64"], self.root / "published")
        self.assertFalse((self.root / "published").exists())

    def test_publication_is_self_contained_with_hashes_and_no_work_directory(self):
        payload = self.stage / "package.zip"
        payload.write_bytes(b"archive bytes for publication control")
        with patch.object(RELEASE, "source", return_value=self.identity):
            RELEASE.publish(self.builder, ["windows-x64"], self.root / "published")
        published = self.root / "published"
        manifest = json.loads((published / "release-manifest.json").read_text())
        self.assertEqual(manifest["source"], self.identity)
        self.assertEqual(manifest["native_acceptance"], "not-run-by-this-tool")
        for item in manifest["files"]:
            self.assertEqual(RELEASE.sha(published / item["path"]), item["sha256"])
        subprocess.run(["sha256sum", "-c", "SHA256SUMS"], cwd=published, check=True, capture_output=True)
        self.assertFalse((published / "work").exists())
        self.assertFalse(self.stage.exists())

    def test_existing_release_is_never_overwritten(self):
        published = self.root / "published"
        published.mkdir()
        (published / "original").write_text("keep")
        with patch.object(RELEASE, "source", return_value=self.identity):
            with self.assertRaisesRegex(ValueError, "拒绝覆盖"):
                RELEASE.publish(self.builder, [], published)
        self.assertEqual((published / "original").read_text(), "keep")

    def test_untracked_or_modified_source_is_rejected(self):
        with patch.object(RELEASE, "git", return_value=" M Cargo.toml"):
            with self.assertRaisesRegex(ValueError, "干净"):
                RELEASE.source()

    def test_wrong_platform_or_commit_and_stale_lock_are_rejected(self):
        lock = self.root / "Cargo.lock"
        lock.write_text("locked")
        record = {"source": {**self.identity, "inputs": {"Cargo.lock": RELEASE.sha(lock)}}, "target": "windows"}
        with patch.object(RELEASE, "ROOT", self.root):
            RELEASE.check_identity(record, self.identity, "windows", windows=True)
            for expected, target in [({**self.identity, "commit": "c" * 40}, "windows"), (self.identity, "linux")]:
                with self.assertRaises(ValueError):
                    RELEASE.check_identity(record, expected, target, windows=True)
            lock.write_text("changed")
            with self.assertRaisesRegex(ValueError, "输入不匹配"):
                RELEASE.check_identity(record, self.identity, "windows", windows=True)

    def test_output_inside_tracked_source_and_symlink_payload_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "Git 忽略"):
            RELEASE.output_directory(ROOT / "tools")
        original = self.root / "original"
        original.write_text("bytes")
        link = self.root / "link"
        link.symlink_to(original)
        with self.assertRaisesRegex(ValueError, "普通文件"):
            RELEASE.copy_file(link, self.root / "copy")


if __name__ == "__main__":
    unittest.main()
