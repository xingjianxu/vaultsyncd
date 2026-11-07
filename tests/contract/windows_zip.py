#!/usr/bin/env python3
"""Host tests for complete ZIP inventories, corruption and deterministic resource packaging."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("windows_zip", ROOT / "packaging/windows/zip/build.py")
ZIP = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ZIP)


class WindowsZip(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="s109 中文 空格 ")
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.payload = self.base / "payload"
        self.payload.mkdir()
        for name in ZIP.EXECUTABLES[:-1]:
            (self.payload / name).write_bytes(b"fake packaging input, NOT a PE: " + name.encode())
        (self.payload / "installed-files.json").write_text(json.dumps({"source": {"commit": "test-only"}}))
        (self.payload / "UNSIGNED-TEST.txt").write_text("unsigned-test\n")
        self.launcher = self.base / "fresh.exe"
        self.launcher.write_bytes(b"test-only launcher")
        self.archive = self.payload / "unsigned.zip"

    def build(self):
        ZIP.build(self.payload, self.launcher, ROOT / "packaging/windows/zip", self.archive)

    def test_complete_archive_and_every_file_hash(self):
        self.build()
        ZIP.verify(self.archive, self.payload / "zip")
        with zipfile.ZipFile(self.archive) as archive:
            self.assertEqual(set(archive.namelist()), {*ZIP.FILES, "zip-files.json"})
            self.assertNotIn("uninstall.exe", archive.namelist())
        manifest = json.loads((self.payload / "zip/zip-files.json").read_text())
        self.assertEqual(manifest["signature_status"], "unsigned-test")
        self.assertEqual(manifest["source"]["commit"], "test-only")
        self.assertEqual(len(manifest["files"]), 10)

    def test_same_bytes_produce_identical_zip_despite_output_path(self):
        self.build()
        second = self.base / "second 输出"
        second.mkdir()
        for name in (*ZIP.EXECUTABLES[:-1], "installed-files.json", "UNSIGNED-TEST.txt"):
            (second / name).write_bytes((self.payload / name).read_bytes())
        archive = second / "unsigned.zip"
        ZIP.build(second, self.launcher, ROOT / "packaging/windows/zip", archive)
        self.assertEqual(archive.read_bytes(), self.archive.read_bytes())

    def test_missing_input_never_becomes_valid_archive(self):
        (self.payload / ZIP.EXECUTABLES[0]).unlink()
        with self.assertRaises(FileNotFoundError):
            self.build()
        self.assertFalse(self.archive.exists())

    def test_old_output_is_refused_without_overwriting(self):
        self.archive.write_bytes(b"retain old artifact")
        with self.assertRaisesRegex(ValueError, "output exists"):
            self.build()
        self.assertEqual(self.archive.read_bytes(), b"retain old artifact")

    def test_mutated_unpacked_payload_is_refused(self):
        self.build()
        (self.payload / "zip/vaultsyncd-cli.exe").write_bytes(b"tamper")
        with self.assertRaisesRegex(ValueError, "content mismatch"):
            ZIP.verify(self.archive, self.payload / "zip")

    def test_archive_injection_is_refused(self):
        self.build()
        with zipfile.ZipFile(self.archive, "a") as archive:
            archive.writestr("extra.exe", b"unexpected")
        with self.assertRaisesRegex(ValueError, "inventory mismatch"):
            ZIP.verify(self.archive, self.payload / "zip")

    def test_manifest_hash_corruption_is_refused(self):
        self.build()
        directory = self.payload / "zip"
        manifest = json.loads((directory / "zip-files.json").read_text())
        manifest["files"][0]["sha256"] = "0" * 64
        (directory / "zip-files.json").write_text(json.dumps(manifest))
        # Rebuild the outer archive so this specifically exercises the inner manifest check.
        self.archive.unlink()
        with zipfile.ZipFile(self.archive, "w") as archive:
            for path in directory.iterdir():
                archive.write(path, path.name)
        with self.assertRaisesRegex(ValueError, "manifest hash mismatch"):
            ZIP.verify(self.archive, directory)

    def test_double_click_entries_quote_own_directory_and_do_not_kill(self):
        resources = ROOT / "packaging/windows/zip"
        for name in ("Start vaultsyncd.cmd", "Stop vaultsyncd.cmd"):
            text = (resources / name).read_text()
            self.assertIn('"%~dp0vaultsyncd-zip.exe"', text)
            self.assertNotIn("taskkill", text.lower())
            self.assertNotIn("powershell", text.lower())
            self.assertNotIn("start ", text.lower().splitlines()[2])


if __name__ == "__main__":
    unittest.main()
