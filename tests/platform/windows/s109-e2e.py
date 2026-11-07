#!/usr/bin/env python3
"""S109 real ZIP/WebView2/IPC/background upload and dirty *process* restart.

Run: nix develop .#web-tests --command python3 tests/platform/windows/s109-e2e.py COMPONENT
Requires an already logged-on Quickemu desktop, no agent in that SID/session, NTFS,
WebView2, SSH and the existing real Linux/btrfs native fixture environment. Never
changes automatic logon, reboots the guest, installs setup, or uses native-acceptance.
Only this run's on-demand Interactive/Limited task is registered. Data/recovery
exports stay in the guest, including on failure. No global process/task cleanup.
"""
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import socket
import subprocess
import sys
import time
import uuid
import zipfile
from urllib.parse import urlparse

from playwright.sync_api import Error as PlaywrightError, sync_playwright

ROOT = Path(__file__).resolve().parents[3]
HOST = os.environ.get("GUEST_HOST", "127.0.0.1")
USER = os.environ.get("GUEST_USER", "Quickemu")
PORT = os.environ.get("GUEST_PORT", "2222")
KEY = os.environ.get("GUEST_SSH_KEY", "/data/vaultsyncd-lab/win-lab/ssh/vaultsyncd-lab_ed25519")
OPTIONS = ["-i", KEY, "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=no",
           "-o", "BatchMode=yes", "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=5"]
SSH = ["ssh", "-p", PORT, *OPTIONS, f"{USER}@{HOST}"]
SCP = ["scp", "-P", PORT, *OPTIONS]
BACKGROUND = b"S109 real background upload after actual GUI close"
DIRTY = b"S109 unconfirmed bytes preserved across safe process stop and restart"
BASELINE = b"S109 committed dirty baseline"


def checked(args, **kwargs):
    result = subprocess.run([str(x) for x in args], cwd=ROOT, text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kwargs)
    if result.returncode:
        # Never echo subprocess output: fixtures/authentication can contain credentials.
        raise RuntimeError(f"{Path(str(args[0])).name} failed, exit={result.returncode}; output suppressed")
    return result.stdout


def digest(data):
    return hashlib.sha256(data).hexdigest()


def verify_component(component):
    receipt = json.loads((component / "receipt.json").read_text())
    assert receipt["component"] == "windows-client"
    assert receipt["target"] == "x86_64-pc-windows-msvc"
    assert receipt["source"]["commit"] == checked(["git", "rev-parse", "HEAD"]).strip()
    assert receipt["source"]["tree"] == checked(["git", "rev-parse", "HEAD^{tree}"]).strip()
    payload = (component / "payload").resolve()
    recorded = set()
    for item in receipt["files"]:
        name = item["path"]
        assert name not in recorded and "\\" not in name
        assert not PurePosixPath(name).is_absolute() and ".." not in PurePosixPath(name).parts
        recorded.add(name)
        path = payload / name
        assert path.resolve().is_relative_to(payload) and not path.is_symlink()
        raw = path.read_bytes()
        assert len(raw) == item["bytes"] and digest(raw) == item["sha256"]
    assert recorded == {p.relative_to(payload).as_posix() for p in payload.rglob("*") if p.is_file()}
    archives = [payload / name for name in recorded if name.endswith("-unsigned.zip")]
    assert len(archives) == 1, "one formal unsigned ZIP required"
    archive = archives[0]
    expected = {"vaultsyncd-agent.exe", "vaultsyncd-session.exe", "vaultsyncd-cli.exe",
                "vaultsyncd-desktop.exe", "vaultsyncd-zip.exe", "installed-files.json",
                "UNSIGNED-TEST.txt", "Start vaultsyncd.cmd", "Stop vaultsyncd.cmd", "README-ZIP.txt"}
    with zipfile.ZipFile(archive) as source:
        names = source.namelist()
        assert len(names) == len(set(names)) and set(names) == expected | {"zip-files.json"}
        manifest = json.loads(source.read("zip-files.json"))
        assert manifest["source"] == receipt["source"] and manifest["signature_status"] == "unsigned-test"
        assert {item["path"] for item in manifest["files"]} == expected
        assert len(manifest["files"]) == len(expected)
        for item in manifest["files"]:
            raw = source.read(item["path"])
            assert len(raw) == item["bytes"] and digest(raw) == item["sha256"]
        installed = json.loads(source.read("installed-files.json"))
        assert installed["source"] == receipt["source"]
        assert installed["signature_status"] == "unsigned-test"
        for item in installed["files"]:
            raw = source.read(item["path"])
            assert len(raw) == item["bytes"] and digest(raw) == item["sha256"]
        # Like the formal release adapter, refuse embedded acceptance-only entry points.
        for name in expected:
            if not name.endswith(".exe"):
                continue
            raw = source.read(name)
            assert raw[:2] == b"MZ"
            pe = int.from_bytes(raw[0x3c:0x40], "little")
            assert raw[pe:pe + 6] == b"PE\0\0\x64\x86"
            for token in ["acceptance_report", "acceptance_file_control", "acceptance_fixture_phase",
                          "acceptance_toast_history", "VAULTSYNCD_S080_REPORT", "VAULTSYNCD_S081_FIXTURE",
                          "VAULTSYNCD_S082_FIXTURE", "VAULTSYNCD_S083_FIXTURE"]:
                assert token.encode() not in raw and token.encode("utf-16le") not in raw
    return receipt, archive


def main():
    if len(sys.argv) != 2 or not __debug__:
        raise SystemExit("usage (without python -O): s109-e2e.py <same-HEAD windows-client component>")
    os.umask(0o077)
    component = Path(sys.argv[1]).resolve()
    receipt, archive = verify_component(component)
    run_id = f"s109-{int(time.time())}-{uuid.uuid4().hex[:10]}"
    run = ROOT / "artifacts/S109" / run_id
    run.mkdir(parents=True)
    guest = f"C:/vaultsyncd-lab/{run_id}"
    results = guest + "/results"
    server = proxy = tls = tunnel = None
    prepared = False
    summary = {"ok": False, "run_id": run_id, "source_commit": receipt["source"]["commit"],
               "zip_sha256": digest(archive.read_bytes()), "signature_status": "unsigned-test",
               "restart_kind": "agent process restart only; no OS reboot", "phase": "fixture"}

    def checkpoint():
        (run / "result.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2))

    def wait(label, predicate, seconds=240):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            try:
                value = predicate()
                if value:
                    return value
            except (RuntimeError, ValueError, OSError, PlaywrightError):
                pass  # Do not print exception text (Playwright can include filled secrets).
            time.sleep(.3)
        raise RuntimeError(f"timeout {label}; sensitive diagnostic text suppressed")

    def ps(mode):
        return checked([*SSH, f"powershell -NoProfile -ExecutionPolicy Bypass -File {guest}/s109-e2e.ps1 -Mode {mode}"])

    def read_guest(name):
        assert re.fullmatch(r"[a-z0-9-]+\.json", name)
        return checked([*SSH, 'powershell -NoProfile -Command "'
                        + f"[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false); if(Test-Path '{results}/{name}')"
                        + f"{{Get-Content -Raw -Encoding UTF8 '{results}/{name}'}}else{{exit 3}}" + '"'])

    def ready(name, seconds=300):
        def poll():
            try:
                error = read_guest("worker-error.json")
            except RuntimeError:
                error = ""
            if error.strip():
                raise AssertionError("native worker failed; see guest worker-error.json (no secrets exported)")
            return json.loads(read_guest(name))
        return wait(name, poll, seconds)

    def action(name):
        summary["phase"] = name
        checkpoint()
        request_id = uuid.uuid4().hex
        request = run / "request.json"
        request.write_text(json.dumps({"id": request_id, "action": name}))
        checked([*SCP, request, f"{USER}@{HOST}:{results}/request.tmp"])
        checked([*SSH, 'powershell -NoProfile -Command "'
                        + f"Move-Item -Force '{results}/request.tmp' '{results}/request.json'" + '"'])
        value = wait(name, lambda: (v if (v := ready("response.json", 10))["id"] == request_id else None), 300)
        assert value["ok"]
        summary[name] = value
        checkpoint()
        print("NATIVE_ACTION_PASSED", name, flush=True)
        return value

    def server_hash(work, name, expected):
        def poll():
            path = work / name
            return path.is_file() and digest(path.read_bytes()) == digest(expected)
        wait("independent real server work_dir hash: " + name, poll, 240)
        return digest((work / name).read_bytes())

    try:
        checked(["cargo", "build", "--locked", "-p", "vaultsyncd-server-tests", "--example", "e2e-native-server"])
        helper = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")) / "debug/examples/e2e-native-server"
        scenario = {"server_seed": [{"path": "server-seed.txt", "content": "S109 actual server baseline"},
                                     {"path": "dirty.txt", "content": BASELINE.decode()}]}
        (run / "scenario.json").write_text(json.dumps(scenario))
        server = subprocess.Popen([str(helper), str(run / "host-fixture.json"), str(run / "stop"),
                                   str(run / "scenario.json")], cwd=ROOT,
                                  stdout=(run / "server.log").open("w"), stderr=subprocess.STDOUT)
        wait("real Linux/btrfs fixture", lambda: (run / "host-fixture.json").exists() and server.poll() is None)
        fixture = json.loads((run / "host-fixture.json").read_text())
        work = Path(fixture["work_dir"]).resolve()
        assert work.is_relative_to(ROOT / "target"), "stale helper compiled in another worktree; rebuild locally"
        assert checked(["stat", "-f", "-c", "%T", work]).strip() == "btrfs"
        backend = str(urlparse(fixture["url"]).port)
        checked(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj",
                 "/CN=vaultsyncd-S109-CA", "-keyout", run / "ca.key", "-out", run / "ca.pem"])
        checked(["openssl", "req", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=127.0.0.1",
                 "-keyout", run / "server.key", "-out", run / "server.csr"])
        (run / "server.ext").write_text("subjectAltName=IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
        checked(["openssl", "x509", "-req", "-days", "1", "-in", run / "server.csr", "-CA", run / "ca.pem",
                 "-CAkey", run / "ca.key", "-CAcreateserial", "-extfile", run / "server.ext", "-out", run / "server.pem"])
        proxy = subprocess.Popen([sys.executable, str(ROOT / "tests/windows-integration/s100-upload-fault-proxy.py"), str(run), backend],
                                 cwd=ROOT, stdout=(run / "proxy.log").open("w"), stderr=subprocess.STDOUT)
        proxy_port = wait("S100 upload fault proxy", lambda: int((run / "proxy-port").read_text()))
        tls = subprocess.Popen([sys.executable, str(ROOT / "tests/platform/windows/s081-tls.py"), str(run), str(proxy_port)],
                               cwd=ROOT, stdout=(run / "tls.log").open("w"), stderr=subprocess.STDOUT)
        tls_port = wait("S081 private TLS", lambda: int((run / "tls-port").read_text()))
        fixture.pop("issued", None)  # No server bearer tokens are needed in Windows.
        fixture.update({"url": "https://127.0.0.1:18775", "serverId": uuid.uuid4().hex,
                        "source_commit": receipt["source"]["commit"], "zip": archive.name,
                        "zip_sha256": summary["zip_sha256"], "cdp_port": 19299,
                        "background_sha256": digest(BACKGROUND), "dirty_sha256": digest(DIRTY)})
        (run / "fixture.json").write_text(json.dumps(fixture))
        script = run / "s109-e2e.ps1"
        script.write_text(Path(__file__).with_suffix(".ps1").read_text(), encoding="utf-8-sig")
        (run / "hashes.json").write_text(json.dumps({p.name: digest(p.read_bytes()) for p in [archive, script]}))
        checked([*SSH, 'powershell -NoProfile -Command "' + f"New-Item -ItemType Directory '{guest}'|Out-Null" + '"'])
        checked([*SCP, archive, script, run / "fixture.json", run / "hashes.json", run / "ca.pem", f"{USER}@{HOST}:{guest}/"])
        # A fresh local CDP tunnel port avoids adopting somebody else's forwarded browser.
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            cdp_port = listener.getsockname()[1]
        tunnel = subprocess.Popen(["ssh", "-p", PORT, *OPTIONS, "-o", "ExitOnForwardFailure=yes", "-N",
                                   "-R", f"127.0.0.1:18775:127.0.0.1:{tls_port}",
                                   "-L", f"127.0.0.1:{cdp_port}:127.0.0.1:19299", f"{USER}@{HOST}"],
                                  cwd=ROOT, stdout=(run / "tunnel.log").open("w"), stderr=subprocess.STDOUT)
        time.sleep(1)
        assert tunnel.poll() is None, "isolated tunnel could not bind; no takeover attempted"
        prepared = True  # Cleanup is attempted even after a partially successful Prepare.
        ps("Prepare")
        installed = ready("ready.json")
        assert "同步 程序" in installed["program"] and "同步 样本" in installed["local"]
        assert installed["zip_sha256"] == summary["zip_sha256"] and installed["limited"]
        summary["ready"] = installed
        summary["phase"] = "real-webview-forms"
        checkpoint()
        with sync_playwright() as playwright:
            browser = wait("real formal WebView2 CDP", lambda: playwright.chromium.connect_over_cdp(f"http://127.0.0.1:{cdp_port}"), 180)
            context = browser.contexts[0]
            page = context.pages[0] if context.pages else context.wait_for_event("page", timeout=120000)
            page.wait_for_url(re.compile("tauri"), timeout=120000)
            page.set_default_timeout(60000)
            # Exercise the actual product default; the harness never writes a preference file.
            assert page.evaluate("window.__TAURI_INTERNALS__.invoke('agent_notification_settings')")["enabled"] is False
            summary["default_notifications_enabled"] = False
            page.locator('[name="server_id"]').fill(fixture["serverId"])
            page.locator('[name="server_url"]').fill(fixture["url"])
            page.get_by_text("开发证书", exact=True).click()
            page.locator('[name="ca_file"]').fill(installed["ca"])
            page.get_by_role("button", name="保存连接", exact=True).click()
            page.get_by_text("操作已由 agent 确认。", exact=True).wait_for(state="visible")
            page.locator('[name="username"]').fill(fixture["login"])
            page.locator('[name="password"]').fill(fixture["password"])
            page.get_by_role("button", name="登录", exact=True).click()
            page.get_by_text(re.compile("已登录（持有凭据")).wait_for(state="visible")
            page.get_by_role("link", name="同步目录", exact=True).click()
            page.get_by_role("button", name="读取授权 Root", exact=True).click()
            page.locator('[name="root_id"]').select_option(fixture["root"])
            page.locator('[name="local_path"]').fill(installed["local"])
            page.locator('[name="mode"]').select_option("full")
            page.get_by_role("button", name="创建绑定并开始合并", exact=True).click()
            state = wait("initial true full merge", lambda: (s if (s := page.evaluate("window.__TAURI_INTERNALS__.invoke('agent_get_status')")).get("bindings") and s["bindings"][0]["synchronized"] else None), 240)
            assert state["management"]["connection"]["server_id"] == fixture["serverId"]
            cli = action("status")
            assert state["snapshot_version"].split(":")[0] == cli["status"]["snapshot_version"].split(":")[0]
            assert cli["agent_pid"] == installed["agent_pid"]
            summary["gui_cli_same_instance"] = True
            summary["initial_gui_status"] = state
            page.screenshot(path=str(run / "real-zip-webview.png"))
            closed = action("close-gui")
            assert closed["gui_closed"] and closed["agent_pid"] == installed["agent_pid"]
            summary["background_server_sha256"] = server_hash(work, "after-gui-close.txt", BACKGROUND)
            wait("background commit reflected by real CLI", lambda: (s if (s := action("status"))["status"]["bindings"][0]["synchronized"] else None), 180)
            # WM_CLOSE above, not browser.close(), is the feature under test.
            if browser.is_connected():
                browser.close()
        (run / "block-uploads").touch()
        dirty = action("dirty")
        assert dirty["dirty_file"]["dirty"] and dirty["dirty_sha256"] == digest(DIRTY)
        wait("real blocked upload begin", lambda: (run / "blocked-upload-begins.log").exists(), 120)
        assert (work / "dirty.txt").read_bytes() == BASELINE
        stopped = action("safe-stop")
        assert stopped["agent_exited"] and stopped["dirty_sha256"] == digest(DIRTY)
        restarted = action("restart")
        assert restarted["device_id"] == dirty["device_id"]
        assert restarted["binding"] == dirty["binding"] | {"paused": True}
        assert restarted["dirty_file"]["dirty"] and restarted["dirty_sha256"] == digest(DIRTY)
        assert restarted["status"]["snapshot_version"].split(":")[0] != dirty["status"]["snapshot_version"].split(":")[0]
        assert restarted["status"]["management"]["connection"]["authenticated"]
        assert restarted["status"]["management"]["connection"]["server_id"] == fixture["serverId"]
        assert restarted["root_registered"] and restarted["boot"] == installed["boot"]
        assert (work / "dirty.txt").read_bytes() == BASELINE
        (run / "block-uploads").unlink()
        action("resume")  # Safe stop intentionally persists PAUSED; no implicit resumption.
        summary["dirty_server_sha256"] = server_hash(work, "dirty.txt", DIRTY)
        final = wait("final actually committed state", lambda: (s if (s := action("status"))["status"]["bindings"][0]["synchronized"] else None), 240)
        assert final["status"]["bindings"][0]["dirty"] == "0"
        summary["finish"] = action("finish")  # Logout only this random server ID; safe stop.
        assert summary["finish"]["defaults_unchanged"], "default Run/shortcut side effect (including toast) is a real failure"
        assert summary["finish"]["agent_exited"]
        summary["ok"] = True
        summary["phase"] = "passed"
    except Exception as error:
        # No traceback/Playwright call log: a failed password fill can include its value.
        summary["failure_kind"] = type(error).__name__
        print("FAIL", summary["phase"], summary["failure_kind"], "(sensitive exception text suppressed)", flush=True)
    finally:
        if prepared:
            try:
                ps("Cleanup")  # Signal only this worker; allow its safe stop, never task-kill.
                # Cleanup evidence is still readable when worker-error.json exists.
                summary["cleanup"] = wait("worker safe cleanup", lambda: json.loads(read_guest("cleanup.json")), 150)
            except Exception as error:
                summary["cleanup_error_kind"] = type(error).__name__
            try:
                checked([*SCP, "-r", f"{USER}@{HOST}:{results}", run / "guest-results"])
            except Exception as error:
                summary["collection_error_kind"] = type(error).__name__
        (run / "stop").touch()
        for process in [tunnel, tls, proxy]:
            if process and process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    summary["host_cleanup_incomplete"] = True
        if server:
            try:
                server.wait(timeout=15)
            except subprocess.TimeoutExpired:
                server.terminate()
                server.wait(timeout=10)
        for name in ["ca.key", "server.key", "fixture.json", "host-fixture.json"]:
            (run / name).unlink(missing_ok=True)
        summary["ok"] = (summary["ok"] and summary.get("cleanup", {}).get("ok", False)
                         and not summary.get("host_cleanup_incomplete") and not summary.get("collection_error_kind"))
        checkpoint()
        print("ARTIFACTS", run, flush=True)
        print("GUEST", guest, "(Root/state/cache/dirty/recovery retained; no OS reboot)", flush=True)
    return 0 if summary["ok"] and summary.get("cleanup", {}).get("ok") and not summary.get("host_cleanup_incomplete") else 1


if __name__ == "__main__":
    # Preflight errors also must not accidentally dump credential-bearing output.
    try:
        raise SystemExit(main())
    except Exception as error:
        print("PREFLIGHT_FAILED", type(error).__name__, "(details suppressed)", file=sys.stderr)
        raise SystemExit(1) from None
