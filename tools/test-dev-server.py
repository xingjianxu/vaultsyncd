#!/usr/bin/env python3
"""Manager lifecycle tests; these do not replace the real server/Windows acceptance."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import socket
import sys
import tempfile
import time
import unittest
from unittest import mock

SOURCE = Path(__file__).with_name("dev-server.py")
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("dev_server", SOURCE)
manager = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(manager)


class DevServerTests(unittest.TestCase):
    def setUp(self):
        self.previous_mask = os.umask(0o077)
        self.temporary = tempfile.TemporaryDirectory(prefix="S207-manager-test-", dir=manager.DEV_BASE)
        self.root = manager.safe_root(Path(self.temporary.name), create=True)
        self.children = []

    def tearDown(self):
        for child in self.children:
            if child.poll() is None:
                child.terminate()
            child.wait(timeout=5)
        self.temporary.cleanup()
        os.umask(self.previous_mask)

    def child(self):
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        self.children.append(child)
        return child

    def test_rejects_production_root(self):
        with self.assertRaisesRegex(RuntimeError, "必须位于"):
            manager.safe_root("/var/lib/vaultsyncd", create=True)

    def test_rejects_symlink_state_before_any_database_access(self):
        (self.root / "state").rmdir()
        (self.root / "state").symlink_to("/var/lib/vaultsyncd", target_is_directory=True)
        with self.assertRaisesRegex(RuntimeError, "软链"):
            manager.safe_root(self.root)

    def test_stale_pid_identity_does_not_signal_live_process(self):
        child = self.child()
        identity = manager.process_identity(child.pid)
        stale = dict(identity, starttime=str(int(identity["starttime"]) + 1))
        self.assertFalse(manager.alive(stale))
        manager.terminate(stale)
        self.assertIsNone(child.poll())
        manager.terminate(identity)
        child.wait(timeout=5)

    def test_stop_recovers_children_after_supervisor_exit(self):
        child = self.child()
        manager.write_json(self.root / "runtime/processes.json", {"phase": "ready", "server": manager.process_identity(child.pid)})
        manager.stop(self.root)
        child.wait(timeout=5)
        self.assertFalse(manager.alive(manager.runtime(self.root)["server"]))

    def test_final_health_failure_stops_just_started_supervisor(self):
        child = self.child()
        manager.write_json(self.root / "runtime/processes.json", {"phase": "ready", "token": "this-start"})
        with mock.patch.object(manager, "check_health", side_effect=RuntimeError("TLS failed")):
            with self.assertRaisesRegex(RuntimeError, "TLS failed"):
                manager.wait_started(self.root, child, "this-start", None)
        self.assertIsNotNone(child.poll())

    def test_start_timeout_waits_for_supervisor_cleanup(self):
        child = self.child()
        with mock.patch.object(manager.time, "monotonic", side_effect=[0, 36]):
            with self.assertRaisesRegex(RuntimeError, "启动超时"):
                manager.wait_started(self.root, child, "this-start", None)
        self.assertIsNotNone(child.poll())

    def test_port_check_rejects_real_listener(self):
        with socket.socket() as listener:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            with self.assertRaises(OSError):
                manager.ensure_port_available("127.0.0.1", listener.getsockname()[1])

    def test_port_check_allows_restart_after_time_wait(self):
        with socket.socket() as listener:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
            listener.listen()
            with socket.create_connection(("127.0.0.1", port)) as client:
                connection, _ = listener.accept()
                connection.close()
                self.assertEqual(client.recv(1), b"")
        manager.ensure_port_available("127.0.0.1", port)

    def office_fixture(self):
        (self.root / "config/server.toml").write_text(manager.server_config(self.root))
        (self.root / "config/Caddyfile").write_text(manager.caddy_config())
        manager.write_json(self.root / "config/instance.json", {
            "environment": "dev", "root": str(self.root), "initialized": True})
        key = self.root / "source-jwt"
        key.write_text("a" * 64)
        key.chmod(0o600)
        return key

    def test_office_configuration_copies_private_key_and_preserves_identity(self):
        key = self.office_fixture()
        manager.configure_office(self.root, "https://office.example.test", key)
        data = manager.settings(self.root)
        self.assertTrue(data["initialized"])
        self.assertEqual(data["office"]["jwt_secret_file"], str(self.root / "config/office-jwt"))
        self.assertEqual((self.root / "config/office-jwt").read_bytes(), key.read_bytes())
        self.assertEqual((self.root / "config/office-jwt").stat().st_mode & 0o777, 0o600)
        self.assertIn("[office]", (self.root / "config/server.toml").read_text())
        manager.configure_office(self.root, None, None, disable=True)
        self.assertNotIn("office", manager.settings(self.root))
        self.assertTrue((self.root / "config/office-jwt").is_file())

    def test_office_configuration_rejects_remote_http_and_running_instance(self):
        key = self.office_fixture()
        before = (self.root / "config/server.toml").read_bytes()
        with self.assertRaisesRegex(RuntimeError, "根 URL"):
            manager.configure_office(self.root, "http://192.0.2.1", key)
        self.assertEqual((self.root / "config/server.toml").read_bytes(), before)
        key.write_text("short")
        with self.assertRaisesRegex(RuntimeError, "32至4096"):
            manager.configure_office(self.root, "https://office.example.test", key)
        key.write_text("a" * 64)
        child = self.child()
        manager.write_json(self.root / "runtime/processes.json", {"server": manager.process_identity(child.pid)})
        with self.assertRaisesRegex(RuntimeError, "停止"):
            manager.configure_office(self.root, "https://office.example.test", key)
        self.assertEqual((self.root / "config/server.toml").read_bytes(), before)

    def test_failed_proxy_start_cleans_up_other_child(self):
        package = self.root / "test-package"
        (package / "bin").mkdir(parents=True)
        server = package / "bin/vaultsyncd-server"
        server.write_text(f"#!{sys.executable}\nimport time\ntime.sleep(60)\n")
        server.chmod(0o700)
        failure = package / "proxy-failure"
        failure.write_text(f"#!{sys.executable}\nraise SystemExit(19)\n")
        failure.chmod(0o700)
        (self.root / "config/server.toml").write_text(manager.server_config(self.root))
        (self.root / "config/Caddyfile").write_text(manager.caddy_config())
        manager.write_json(self.root / "config/instance.json", {"environment": "dev", "root": str(self.root)})
        process = subprocess.run([sys.executable, str(SOURCE), "_supervise", "--root", str(self.root),
                                  "--package", str(package), "--caddy", str(failure), "--token", "unit-test"],
                                 capture_output=True, text=True, timeout=10)
        self.assertEqual(process.returncode, 0, process.stderr)
        record = manager.runtime(self.root)
        self.assertEqual(record["phase"], "failed")
        self.assertIn("子进程", record["error"])
        self.assertFalse(manager.alive(record["server"]))
        self.assertFalse(manager.alive(record["caddy"]))


if __name__ == "__main__":
    unittest.main()
