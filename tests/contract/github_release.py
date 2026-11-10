#!/usr/bin/env python3
"""Local release validation and remote failure contracts; no GitHub mutations."""
import argparse
import contextlib
import copy
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("github_release", ROOT / "packaging/github-release.py")
RELEASE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RELEASE)


class Remote(RELEASE.GitHub):
    def __init__(self, source, assets):
        super().__init__("owner/repo")
        self.source, self.assets = source, assets
        self.record = None
        self.ref = None
        self.calls = []
        self.fail_upload = None

    def api(self, suffix, missing=False):
        if suffix.startswith("commits/"):
            return {"sha": self.source["commit"]}
        if suffix.startswith("git/ref/tags/"):
            return self.ref
        if suffix.startswith("git/tags/"):
            return {"object": {"type": "commit", "sha": self.source["commit"]}}
        if suffix.startswith("releases?per_page="):
            return [copy.deepcopy(self.record)] if self.record else []
        if suffix == "releases/1":
            return copy.deepcopy(self.record)
        raise AssertionError(suffix)

    def release(self, *args):
        self.calls.append(args)
        if args[0] == "create":
            self.record = {"id": 1, "tag_name": args[1], "draft": True, "target_commitish": self.source["commit"],
                           "prerelease": "--prerelease" in args, "assets": [],
                           "body": Path(args[args.index("--notes-file") + 1]).read_text()}
        elif args[0] == "upload":
            path = Path(args[2])
            if path.name == self.fail_upload:
                raise ValueError("upload failure")
            self.record["assets"].append({"name": path.name, "size": path.stat().st_size,
                                           "state": "uploaded", "digest": "sha256:" + RELEASE.sha(path)})
        elif args[0] == "edit":
            self.record["draft"] = False
        else:
            raise AssertionError(args)


class GitHubReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        def git(*args):
            return subprocess.check_output(["git", "-C", str(self.repo), *args], stderr=subprocess.PIPE).decode().strip()
        git("init", "-q")
        (self.repo / "Cargo.toml").write_text('[workspace.package]\nversion = "0.2.0"\n')
        (self.repo / "Cargo.lock").write_text("locked fixture input\n")
        git("add", "Cargo.toml", "Cargo.lock")
        git("-c", "user.name=Release contract fixture", "-c", "user.email=fixture@example.invalid",
            "-c", "commit.gpgsign=false", "commit", "-qm", "fixture")
        git("remote", "add", "origin", "https://github.com/owner/repo.git")
        self.source = {"commit": git("rev-parse", "HEAD"), "tree": git("rev-parse", "HEAD^{tree}"), "version": "0.2.0"}
        self.directory = self.root / "bundle"
        self.directory.mkdir()
        self.manifest = {"format": 1, "source": self.source, "platforms": ["windows-x64"],
                         "signature_status": "unsigned-test", "native_acceptance": "not-run-by-this-tool", "files": []}
        inputs = {"Cargo.lock": RELEASE.sha(self.repo / "Cargo.lock")}
        self.write("windows-x64/vaultsyncd-0.2.0-x64-unsigned.zip", b"Windows fixture bytes")
        zip_path = self.directory / "windows-x64/vaultsyncd-0.2.0-x64-unsigned.zip"
        windows = {"component": "windows-zip", "source": {**self.source, "inputs": inputs},
                   "target": RELEASE.TARGETS["windows-x64"], "files": [
                       {"path": zip_path.name, "bytes": zip_path.stat().st_size, "sha256": RELEASE.sha(zip_path)}]}
        self.write("windows-x64/receipt.json", json.dumps(windows).encode())
        self.write("logs/01.log", b"local build log, must not be uploaded")
        self.save()
        self.patcher = patch.object(RELEASE, "VAULT", self.repo)
        self.patcher.start()
        self.addCleanup(self.patcher.stop)
        self.args = argparse.Namespace(tag="v0.2.0", resume=False, stable=False, draft=False, notes_file=None, title=None)
        self.stage = self.root / "stage"
        self.stage.mkdir()
        self.assets = {"payload.zip": self.directory / "windows-x64/vaultsyncd-0.2.0-x64-unsigned.zip"}
        # Real publisher uploads staged files whose basenames equal the public names.
        payload = self.stage / "payload.zip"
        payload.write_bytes(next(iter(self.assets.values())).read_bytes())
        self.assets = {payload.name: payload}
        self.remote = Remote(self.source, self.assets)

    def write(self, name, content):
        path = self.directory / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)

    def save(self):
        self.manifest["files"] = [{"path": str(p.relative_to(self.directory)), "bytes": p.stat().st_size,
                                   "sha256": RELEASE.sha(p)} for p in sorted(self.directory.rglob("*"))
                                  if p.is_file() and p.name != "SHA256SUMS" and p != self.directory / "release-manifest.json"]
        (self.directory / "release-manifest.json").write_text(json.dumps(self.manifest))
        (self.directory / "SHA256SUMS").write_text("".join(
            f"{item['sha256']}  {item['path']}\n" for item in self.manifest["files"]))

    def publish(self):
        with patch.object(RELEASE, "run", return_value=b""), contextlib.redirect_stdout(io.StringIO()):
            RELEASE.publish(self.remote, self.manifest, self.assets, self.stage, self.args)

    def test_bundle_checks_real_git_source_and_selects_only_public_assets(self):
        manifest, assets, digests = RELEASE.bundle(self.directory)
        self.assertEqual(manifest["source"], self.source)
        self.assertEqual(set(assets), {"vaultsyncd-0.2.0-x64-unsigned.zip"})
        self.assertNotIn("logs/01.log", assets)
        self.assertNotIn("vaultsyncd-runtime-sources.tar.gz", assets)
        self.assertEqual(set(assets), set(digests))
        self.assertNotIn("release-manifest.json", assets)
        self.assertNotIn("windows-x64-receipt.json", assets)

    def test_linux_only_mixed_empty_and_duplicate_platforms_are_refused_before_gh(self):
        for platforms in (["linux-x64"], ["linux-x64", "windows-x64"], [], ["windows-x64"] * 2):
            with self.subTest(platforms=platforms):
                self.manifest["platforms"] = platforms
                self.save()
                with patch.object(RELEASE, "GitHub") as api, contextlib.redirect_stdout(io.StringIO()):
                    with self.assertRaisesRegex(ValueError, "仅接受 Windows"):
                        RELEASE.main([str(self.directory), "--dry-run"])
                api.assert_not_called()

    def test_dry_run_discloses_receipts_checksums_and_logs_as_local_only(self):
        with contextlib.redirect_stdout(io.StringIO()) as output:
            RELEASE.main([str(self.directory), "--dry-run"])
        plan = json.loads(output.getvalue())
        self.assertEqual(plan["assets"], ["vaultsyncd-0.2.0-x64-unsigned.zip"])
        self.assertEqual(plan["local_only"], ["SHA256SUMS", "logs/01.log", "release-manifest.json", "windows-x64/receipt.json"])

    def test_tampered_bytes_missing_files_and_checksum_are_refused(self):
        path = self.directory / "logs/01.log"
        path.write_bytes(b"tampered")
        with self.assertRaisesRegex(ValueError, "校验失败"):
            RELEASE.bundle(self.directory)
        path.unlink()
        with self.assertRaisesRegex(ValueError, "普通文件"):
            RELEASE.bundle(self.directory)
        self.save()
        (self.directory / "SHA256SUMS").write_text("changed")
        with self.assertRaisesRegex(ValueError, "SHA256SUMS"):
            RELEASE.bundle(self.directory)

    def test_traversal_symlink_duplicate_and_unsupported_platform_are_refused(self):
        for name in ["../outside", "/absolute", "linux-x64/../outside", "linux-x64//file", "linux-x64\\file"]:
            with self.subTest(name=name), self.assertRaises(ValueError):
                RELEASE.regular(self.directory, name)
        link = self.directory / "link"
        link.symlink_to(self.directory / "windows-x64", target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "符号链接"):
            RELEASE.regular(self.directory, "link/receipt.json")
        self.manifest["files"].append(self.manifest["files"][0])
        (self.directory / "release-manifest.json").write_text(json.dumps(self.manifest))
        with self.assertRaisesRegex(ValueError, "重复路径"):
            RELEASE.bundle(self.directory)
        self.manifest["platforms"] = ["linux-arm64"]
        self.save()
        with self.assertRaisesRegex(ValueError, "仅接受 Windows"):
            RELEASE.bundle(self.directory)

    def test_changed_version_tree_and_mixed_component_sources_are_refused(self):
        for field, value in [("version", "0.3.0"), ("tree", "a" * 40)]:
            original = self.source[field]
            self.source[field] = value
            self.save()
            with self.assertRaises(ValueError):
                RELEASE.bundle(self.directory)
            self.source[field] = original
        receipt = self.directory / "windows-x64/receipt.json"
        data = json.loads(receipt.read_text())
        data["source"]["commit"] = "b" * 40
        receipt.write_text(json.dumps(data))
        self.save()
        with self.assertRaisesRegex(ValueError, "组件来源"):
            RELEASE.bundle(self.directory)

    def test_dry_run_does_not_invoke_gh_or_create_staging_directory(self):
        with contextlib.redirect_stdout(io.StringIO()) as output, patch.object(RELEASE, "GitHub") as api:
            RELEASE.main([str(self.directory), "--dry-run", "--staging-root", str(self.root / "no-stage")])
        api.assert_not_called()
        self.assertFalse((self.root / "no-stage").exists())
        plan = json.loads(output.getvalue())
        self.assertEqual(plan["repository"], "owner/repo")
        self.assertEqual(plan["tag"], "v0.2.0")
        self.assertTrue(plan["prerelease"])
        self.assertFalse(plan["latest"])

    def test_dry_run_discloses_client_channel_promotion(self):
        for arguments, expected in [(["--stable"], True), (["--stable", "--draft"], False)]:
            with contextlib.redirect_stdout(io.StringIO()) as output:
                RELEASE.main([str(self.directory), "--dry-run", *arguments])
            self.assertEqual(json.loads(output.getvalue())["latest"], expected)

    def test_cli_dry_run_from_another_directory_and_bad_arguments(self):
        directory = self.repo / "packaging"
        directory.mkdir()
        script = directory / "github-release.py"
        script.write_bytes((ROOT / "packaging/github-release.py").read_bytes())
        result = subprocess.run([RELEASE.sys.executable, str(script), str(self.directory), "--dry-run"],
                                cwd=self.root, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["source"], self.source)
        result = subprocess.run([RELEASE.sys.executable, str(script), "--invalid"],
                                cwd=self.root, capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)

    def test_tag_mismatch_and_stable_suffix_are_refused_before_remote_access(self):
        for arguments in [["--tag", "v0.3.0"], ["--stable", "--tag", "v0.2.0-rc.1"]]:
            with self.assertRaises(ValueError):
                RELEASE.main([str(self.directory), "--dry-run", *arguments])

    def test_supported_remote_urls_and_credential_rejection(self):
        for url in ["https://github.com/owner/repo.git", "git@github.com:owner/repo.git",
                    "ssh://git@github.com/owner/repo.git", "https://github.com/owner/repo/"]:
            with patch.object(RELEASE, "git", return_value=url):
                self.assertEqual(RELEASE.repository("origin"), "owner/repo")
        for url in ["https://secret@github.com/owner/repo.git", "https://example.com/owner/repo", "../local"]:
            with patch.object(RELEASE, "git", return_value=url), self.assertRaises(ValueError):
                RELEASE.repository("origin")

    def test_create_upload_verify_publish_order_and_unsigned_disclosure(self):
        self.publish()
        self.assertEqual([c[0] for c in self.remote.calls], ["create", "upload", "edit"])
        self.assertIn("--target", self.remote.calls[0])
        self.assertIn("--draft", self.remote.calls[0])
        self.assertIn("--prerelease", self.remote.calls[0])
        self.assertFalse(self.remote.record["draft"])
        self.assertIn("unsigned-test", self.remote.record["body"])
        self.assertIn("--latest=false", self.remote.calls[-1])

    def test_release_notes_include_zip_checksum_without_claiming_checksum_attachments(self):
        self.publish()
        self.assertIn("Windows x64 便携 ZIP", self.remote.record["body"])
        self.assertIn(RELEASE.sha(self.assets["payload.zip"]), self.remote.record["body"])
        self.assertNotIn("SHA256SUMS", self.remote.record["body"])

    def stable_assets(self):
        name = "vaultsyncd-0.2.0-x64-unsigned.zip"
        path = self.stage / name
        path.write_bytes(b"Windows fixture bytes")
        self.assets = {name: path}
        self.remote = Remote(self.source, self.assets)
        self.args.stable = True

    def test_stable_windows_release_is_latest_only_after_all_assets_verified(self):
        self.stable_assets()
        self.publish()
        self.assertNotIn("--prerelease", self.remote.calls[0])
        self.assertIn("--latest=false", self.remote.calls[0])
        self.assertIn("--latest=true", self.remote.calls[-1])

    def test_stable_draft_only_promotes_client_channel_when_resumed(self):
        self.stable_assets()
        self.args.draft = True
        self.publish()
        self.assertTrue(self.remote.record["draft"])
        self.assertFalse(any("--latest=true" in call for call in self.remote.calls))
        self.args.draft = False
        self.args.resume = True
        self.publish()
        self.assertIn("--latest=true", self.remote.calls[-1])

    def test_stable_missing_github_digest_keeps_draft_even_if_bytes_were_verified(self):
        self.stable_assets()
        original = self.remote.api
        def no_digest(suffix, missing=False):
            value = original(suffix, missing)
            if suffix == "releases/1":
                value["assets"][0]["digest"] = None
            return value
        with patch.object(self.remote, "api", side_effect=no_digest), patch.object(self.remote, "verify_assets", return_value=set()):
            with self.assertRaisesRegex(ValueError, "GitHub SHA-256"), contextlib.redirect_stderr(io.StringIO()):
                self.publish()
        self.assertTrue(self.remote.record["draft"])
        self.assertFalse(any(call[0] == "edit" for call in self.remote.calls))

    def test_failed_upload_keeps_draft_and_resume_skips_matching_assets(self):
        self.remote.fail_upload = "payload.zip"
        with self.assertRaisesRegex(ValueError, "upload failure"), contextlib.redirect_stderr(io.StringIO()):
            self.publish()
        self.assertTrue(self.remote.record["draft"])
        self.remote.fail_upload = None
        self.args.resume, self.args.draft = True, True
        self.publish()
        count = len(self.remote.calls)
        self.publish()
        self.assertEqual(count, len(self.remote.calls))
        self.args.draft = False
        self.publish()
        self.assertFalse(self.remote.record["draft"])
        with self.assertRaisesRegex(ValueError, "Release 已存在"):
            self.publish()

    def test_wrong_remote_tag_and_mismatched_draft_assets_are_refused(self):
        self.remote.ref = {"object": {"type": "commit", "sha": "a" * 40}}
        with self.assertRaisesRegex(ValueError, "远端标签"):
            self.publish()
        self.assertEqual(self.remote.calls, [])
        self.remote.ref = {"object": {"type": "tag", "sha": "b" * 40}}
        self.args.draft = True
        self.publish()
        self.args.resume = True
        self.remote.record["assets"][0]["digest"] = "sha256:" + "0" * 64
        with self.assertRaisesRegex(ValueError, "远端附件哈希"):
            self.publish()

    def test_http_404_is_distinct_from_authentication_network_and_server_failures(self):
        api = RELEASE.GitHub("owner/repo")
        for status in [401, 403, 500]:
            result = subprocess.CompletedProcess([], 1, f"HTTP/2.0 {status} failure\n\n{{}}", "failure")
            with patch.object(RELEASE.subprocess, "run", return_value=result), self.assertRaisesRegex(ValueError, "查询失败"):
                api.api("releases/tags/v0.2.0", missing=True)
        result = subprocess.CompletedProcess([], 1, 'HTTP/2.0 404 Not Found\n\n{}', "missing")
        with patch.object(RELEASE.subprocess, "run", return_value=result):
            self.assertIsNone(api.api("releases/tags/v0.2.0", missing=True))
        result = subprocess.CompletedProcess([], 0, 'HTTP/2.0 200 OK\nX-Test: yes\n\n{"id": 1}', "")
        with patch.object(RELEASE.subprocess, "run", return_value=result):
            self.assertEqual(api.api("releases/tags/v0.2.0"), {"id": 1})

    def test_missing_remote_commit_and_authentication_failure_never_create_release(self):
        api = self.remote.api
        with patch.object(self.remote, "api", side_effect=lambda suffix, **kw: None if suffix.startswith("commits/") else api(suffix, **kw)):
            with self.assertRaisesRegex(ValueError, "尚未推送"):
                self.publish()
        with patch.object(RELEASE, "run", side_effect=ValueError("not authenticated")):
            with self.assertRaisesRegex(ValueError, "not authenticated"):
                RELEASE.publish(self.remote, self.manifest, self.assets, self.stage, self.args)
        self.assertEqual(self.remote.calls, [])

    def test_remote_asset_without_digest_is_downloaded_and_checked(self):
        self.args.draft = True
        self.publish()
        item = self.remote.record["assets"][0]
        item["id"] = 7
        del item["digest"]
        def download(args, **kwargs):
            self.assertIn("Accept: application/octet-stream", args)
            kwargs["stdout"].write(self.assets["payload.zip"].read_bytes())
            return subprocess.CompletedProcess(args, 0)
        with patch.object(RELEASE.subprocess, "run", side_effect=download):
            self.assertEqual(self.remote.verify_assets(self.remote.record, self.assets, self.stage), {"payload.zip"})
        self.assertFalse((self.stage / "download-check").exists())

    def test_remote_mismatch_after_upload_does_not_publish(self):
        original = self.remote.verify_assets
        def verify(*args, **kwargs):
            if kwargs.get("complete"):
                raise ValueError("remote integrity failure")
            return original(*args, **kwargs)
        with patch.object(self.remote, "verify_assets", side_effect=verify), contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaisesRegex(ValueError, "remote integrity"):
                self.publish()
        self.assertTrue(self.remote.record["draft"])
        self.assertNotIn("edit", [call[0] for call in self.remote.calls])

    def test_copy_race_is_refused_before_remote_mutation(self):
        original = RELEASE.shutil.copyfile
        def corrupt(source, destination):
            original(source, destination)
            Path(destination).write_bytes(b"changed after validation")
        with patch.object(RELEASE.shutil, "copyfile", side_effect=corrupt), patch.object(RELEASE, "publish") as publish:
            with self.assertRaisesRegex(ValueError, "暂存复制"), contextlib.redirect_stdout(io.StringIO()):
                RELEASE.main([str(self.directory), "--staging-root", str(self.root / "staging")])
        publish.assert_not_called()

    def test_drafts_are_found_on_later_pages_without_using_the_published_tag_endpoint(self):
        api = RELEASE.GitHub("owner/repo")
        draft = {"id": 101, "tag_name": "v0.2.0", "draft": True}
        pages = [[{"tag_name": f"v9.0.{i}"} for i in range(100)], [draft]]
        with patch.object(api, "api", side_effect=pages) as query:
            self.assertEqual(api.find_release("v0.2.0"), draft)
        self.assertEqual([call.args[0] for call in query.call_args_list],
                         ["releases?per_page=100&page=1", "releases?per_page=100&page=2"])

    def test_recomputed_outer_manifest_does_not_hide_stale_windows_receipt(self):
        self.write("windows-x64/vaultsyncd-0.2.0-x64-unsigned.zip", b"changed archive")
        self.save()
        with self.assertRaisesRegex(ValueError, "ZIP 与组件收据"):
            RELEASE.bundle(self.directory)

    def test_staging_contains_only_zip_and_preserves_local_receipts_and_checksums(self):
        original_checksums = (self.directory / "SHA256SUMS").read_bytes()
        def inspect(api, manifest, assets, stage, args):
            name = "vaultsyncd-0.2.0-x64-unsigned.zip"
            self.assertEqual(set(assets), {name})
            self.assertEqual(list(stage.iterdir()), [stage / name])
            self.assertEqual(RELEASE.sha(assets[name]), RELEASE.sha(self.directory / "windows-x64" / name))
        with patch.object(RELEASE, "publish", side_effect=inspect) as publish, contextlib.redirect_stdout(io.StringIO()):
            RELEASE.main([str(self.directory), "--staging-root", str(self.root / "staging")])
        publish.assert_called_once()
        self.assertEqual(list((self.root / "staging").iterdir()), [])
        self.assertEqual((self.directory / "SHA256SUMS").read_bytes(), original_checksums)
        self.assertTrue((self.directory / "windows-x64/receipt.json").is_file())


if __name__ == "__main__":
    unittest.main()
