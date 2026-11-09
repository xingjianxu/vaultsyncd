#!/usr/bin/env python3
"""Real unsigned installation in a fresh Windows standard user, real WebView2 CDP and guest reboot.

Run in nix develop .#web-tests, with a same-HEAD windows-client component directory argument.
The caller must coordinate the VM before starting: three controlled restarts restore Quickemu at end.
"""
import hashlib
import json
import os
import re
from pathlib import Path
import subprocess
import sys
import time
import uuid

from playwright.sync_api import Error as PlaywrightError, sync_playwright

VAULT = Path(__file__).resolve().parents[3]
HOST = os.environ.get("GUEST_HOST", "127.0.0.1")
USER = os.environ.get("GUEST_USER", "Quickemu")
PORT = os.environ.get("GUEST_PORT", "2222")
KEY = os.environ.get("GUEST_SSH_KEY", "/data/vaultsyncd-dev/vaultsyncd-lab/win-lab/ssh/vaultsyncd-lab_ed25519")
OPTIONS = ["-i", KEY, "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=no",
           "-o", "BatchMode=yes", "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=5"]
SSH = ["ssh", "-p", PORT, *OPTIONS, f"{USER}@{HOST}"]
SCP = ["scp", "-P", PORT, *OPTIONS]


def checked(args, **kwargs):
    result = subprocess.run([str(x) for x in args], cwd=VAULT, text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kwargs)
    if result.returncode:
        raise RuntimeError(f"{args[0]} failed ({result.returncode}): {result.stderr[-2000:]}")
    return result.stdout


def main():
    component = Path(sys.argv[1]).resolve()
    receipt = json.loads((component / "receipt.json").read_text())
    assert receipt["component"] == "windows-client" and receipt["target"] == "x86_64-pc-windows-msvc"
    assert receipt["source"]["commit"] == checked(["git", "rev-parse", "HEAD"]).strip()
    run_id = f"s099-{int(time.time())}-{os.getpid()}"
    run = VAULT / "artifacts/S099" / run_id
    run.mkdir(parents=True)
    checked(["nix", "develop", "--command", "cargo", "xtask", "release", "assemble",
             run / "release-manifest.json", component])
    checked(["nix", "develop", "--command", "cargo", "xtask", "release", "verify",
             run / "release-manifest.json"])
    checked(["nix", "develop", "--command", "cargo", "build", "--locked", "-p",
             "vaultsyncd-server-tests", "--example", "e2e-native-server"])
    helper = Path(os.environ.get("CARGO_TARGET_DIR", VAULT / "target")) / "debug/examples/e2e-native-server"
    scenario = {"server_seed": [{"path": "server-seed.txt", "content": "S099 server before installation"}]}
    (run / "scenario.json").write_text(json.dumps(scenario))
    server = subprocess.Popen([str(helper), str(run / "host-fixture.json"), str(run / "stop"),
                               str(run / "scenario.json")], cwd=VAULT,
                              stdout=(run / "server.log").open("w"), stderr=subprocess.STDOUT)
    tls = tunnel = None
    prepared = False
    restored = False
    guest = f"C:/vaultsyncd-lab/{run_id}"
    results = guest + "/results"
    summary = {"ok": False, "source_commit": receipt["source"]["commit"], "run_id": run_id}

    def wait(label, predicate, seconds=240):
        end = time.monotonic() + seconds
        last = None
        while time.monotonic() < end:
            try:
                result = predicate()
                if result:
                    return result
            except (RuntimeError, ValueError, OSError, PlaywrightError) as error:
                last = str(error)
            time.sleep(.3)
        raise RuntimeError(f"timeout {label}: {last}")

    def ps(mode):
        return checked([*SSH, f"powershell -NoProfile -ExecutionPolicy Bypass -File {guest}/s099-installation.ps1 -Mode {mode}"])

    def read_guest(path):
        return checked([*SSH, f'powershell -NoProfile -Command "[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false); if(Test-Path \'{path}\'){{Get-Content -Raw -Encoding UTF8 \'{path}\'}}else{{exit 3}}"'])

    def ready(filename):
        def poll():
            error = subprocess.run([*SSH, f'powershell -NoProfile -Command "[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false); if(Test-Path \'{results}/worker-error.json\'){{Get-Content -Raw -Encoding UTF8 \'{results}/worker-error.json\'}}"'],
                                   capture_output=True, text=True, cwd=VAULT)
            if error.stdout.strip():
                raise AssertionError("native worker: " + error.stdout)
            return json.loads(read_guest(f"{results}/{filename}"))
        return wait(filename, poll, 420)

    def action(name):
        request_id = uuid.uuid4().hex
        request = run / "request.json"
        request.write_text(json.dumps({"id": request_id, "action": name}))
        checked([*SCP, request, f"{USER}@{HOST}:{results}/request.tmp"])
        checked([*SSH, f'powershell -NoProfile -Command "Move-Item -Force \'{results}/request.tmp\' \'{results}/request.json\'"'])
        return wait(name, lambda: (value if (value := json.loads(read_guest(results + "/response.json")))["id"] == request_id else None), 120)

    def boot():
        return checked([*SSH, 'powershell -NoProfile -Command "(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToString(\'o\')"']).strip()

    def connect_tunnel(port):
        nonlocal tunnel
        if tunnel and tunnel.poll() is None:
            tunnel.terminate()
            tunnel.wait(timeout=10)
        tunnel = subprocess.Popen(["ssh", "-p", PORT, *OPTIONS, "-o", "ExitOnForwardFailure=yes", "-N",
                                   "-R", f"127.0.0.1:18775:127.0.0.1:{port}",
                                   "-L", "127.0.0.1:19299:127.0.0.1:19299", f"{USER}@{HOST}"],
                                  cwd=VAULT, stdout=(run / "tunnel.log").open("a"), stderr=subprocess.STDOUT)
        time.sleep(.5)
        assert tunnel.poll() is None, "isolated authenticated tunnel failed"

    def reboot(label, port=None):
        before = boot()
        print(f"CONTROLLED_REBOOT {label}; prior resources and logon configuration saved", flush=True)
        # SSH disconnect is expected; a different real boot timestamp is required before continuing.
        try:
            subprocess.run([*SSH, 'powershell -NoProfile -Command "Restart-Computer -Force"'],
                           cwd=VAULT, capture_output=True, timeout=30)
        except subprocess.TimeoutExpired:
            pass  # Only a changed real boot timestamp below permits continuation.
        after = wait(label + " actual boot", lambda: (value if (value := boot()) != before else None), 420)
        if port:
            connect_tunnel(port)
        return {"before": before, "after": after}

    try:
        wait("real server", lambda: (run / "host-fixture.json").exists() and server.poll() is None)
        fixture = json.loads((run / "host-fixture.json").read_text())
        work = Path(fixture["work_dir"])
        assert work.resolve().is_relative_to(VAULT / "target"), "rebuild stale embedded helper in this worktree"
        from urllib.parse import urlparse
        backend = str(urlparse(fixture["url"]).port)
        checked(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj",
                 "/CN=vaultsyncd-S099-CA", "-keyout", run / "ca.key", "-out", run / "ca.pem"])
        checked(["openssl", "req", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=127.0.0.1",
                 "-keyout", run / "server.key", "-out", run / "server.csr"])
        (run / "server.ext").write_text("subjectAltName=IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
        checked(["openssl", "x509", "-req", "-days", "1", "-in", run / "server.csr", "-CA", run / "ca.pem",
                 "-CAkey", run / "ca.key", "-CAcreateserial", "-extfile", run / "server.ext", "-out", run / "server.pem"])
        tls = subprocess.Popen([sys.executable, str(VAULT / "tests/platform/windows/s081-tls.py"), str(run), backend],
                               cwd=VAULT, stdout=(run / "tls.log").open("w"), stderr=subprocess.STDOUT)
        tls_port = wait("private TLS listener", lambda: (int((run / "tls-port").read_text()) if (run / "tls-port").exists() else None))
        setup = next((component / "payload").glob("*-unsigned-setup.exe"))
        fixture.update({"url": "https://127.0.0.1:18775", "serverId": uuid.uuid4().hex,
                        "source_commit": receipt["source"]["commit"], "installer": setup.name,
                        "ci_cleanup": os.environ.get("VAULTSYNCD_CI_CLEANUP") == "1"})
        (run / "fixture.json").write_text(json.dumps(fixture))
        script = run / "s099-installation.ps1"
        script.write_text((VAULT / "tests/platform/windows/s099-installation.ps1").read_text(), encoding="utf-8-sig")
        secret_script = run / "s099-logon-secrets.ps1"
        secret_script.write_text((VAULT / "tests/platform/windows/s099-logon-secrets.ps1").read_text(), encoding="utf-8-sig")
        picker = run / "s114-picker.ps1"
        picker.write_text((VAULT / "tests/platform/windows/s114-picker.ps1").read_text(), encoding="utf-8-sig")
        (run / "hashes.json").write_text(json.dumps({path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in [setup, script, secret_script, picker]}))
        checked([*SSH, f'powershell -NoProfile -Command "New-Item -ItemType Directory \'{guest}\' | Out-Null"'])
        checked([*SCP, setup, run / "fixture.json", run / "hashes.json", run / "ca.pem",
                 script, secret_script, picker, f"{USER}@{HOST}:{guest}/"])
        # The backup includes real prior logon secrets and stays ACL-protected in the guest only.
        # Restore is attempted even if Prepare partially fails.
        prepared = True
        summary["prior_console_user"] = checked([*SSH, 'powershell -NoProfile -Command "(Get-CimInstance Win32_ComputerSystem).UserName"']).strip()
        assert summary["prior_console_user"]
        print(ps("Prepare"), flush=True)
        summary["first_login_boot"] = reboot("fresh standard-user login", tls_port)
        installed = ready("install-ready.json")
        assert installed["standard_user"] and "同步 客户端" in installed["program"]
        assert not installed["profile"].lower().startswith(installed["program"].lower())
        summary["installed"] = installed
        with sync_playwright() as playwright:
            browser = wait("actual installed WebView2 CDP", lambda: playwright.chromium.connect_over_cdp("http://127.0.0.1:19299"), 180)
            context = browser.contexts[0]
            page = context.pages[0] if context.pages else context.wait_for_event("page", timeout=120000)
            # Sync Playwright properties do not pump navigation events while Python sleeps.
            page.wait_for_url(re.compile("tauri"), timeout=120000)
            page.set_default_timeout(60000)
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
            page.get_by_role("button", name="读取授权 Vault", exact=True).click()
            page.locator('[name="vault_id"]').select_option(fixture["vault"])
            # The installed client only accepts a path from the real native folder picker.
            action("pick-directory")
            page.get_by_role("button", name="选择目录…", exact=True).click()

            def picked():
                value = page.locator('[name="local_path"]').input_value()
                if value == installed["local"]:
                    return True
                if value:
                    raise AssertionError(f"picker returned {value!r}, expected {installed['local']!r}")
                return False

            wait("native directory picker", picked, 180)
            page.locator('[name="mode"]').select_option("full")
            page.get_by_role("button", name="创建绑定并开始合并", exact=True).click()

            def gui_status():
                return page.evaluate("window.__TAURI_INTERNALS__.invoke('agent_get_status')")

            state = wait("installed full initial merge", lambda: (s if (s := gui_status()).get("bindings") and s["bindings"][0]["synchronized"] else None), 180)
            assert state["management"]["connection"]["server_id"] == fixture["serverId"]
            cli = action("status")
            assert state["snapshot_version"].split(":")[0] == cli["status"]["snapshot_version"].split(":")[0]
            assert cli["agent_pid"] == installed["agent_pid"]
            summary["gui_cli_same_instance"] = True
            summary["binding_id"] = state["bindings"][0]["binding_id"]
            summary["initial_gui_status"] = state
            page.screenshot(path=str(run / "installed-webview.png"))
            closed = action("close-gui")
            assert closed["gui_closed"] and closed["agent_pid"] == installed["agent_pid"]
            summary["gui_close_keeps_agent"] = True
            wait("upload after actual GUI close", lambda: (work / "after-gui-close.txt").exists() and (work / "after-gui-close.txt").read_bytes() == b"S099 sync continues after GUI closes", 180)
            summary["after_gui_close_cli"] = wait("post GUI committed status", lambda: (r if (r := action("status"))["status"]["bindings"][0]["synchronized"] else None), 90)
            summary["before_reboot"] = action("prepare-reboot")
            if browser.is_connected():
                try:
                    browser.close()
                except PlaywrightError as error:
                    summary["cdp_cleanup_after_real_WM_CLOSE"] = str(error)
        summary["sync_boot"] = reboot("installed HKCU Run agent restart", tls_port)
        restarted = ready("reboot-ready.json")
        assert restarted["started_only_by_installed_run"] and restarted["sid"] == installed["sid"]
        wait("real upload after guest restart", lambda: (work / "after-reboot.txt").exists() and (work / "after-reboot.txt").read_bytes() == b"S099 sync after real guest reboot", 180)
        final = wait("committed after reboot", lambda: (r if (r := action("status"))["status"]["bindings"][0]["synchronized"] else None), 90)
        assert final["status"]["management"]["connection"]["server_id"] == fixture["serverId"]
        assert final["status"]["management"]["connection"]["authenticated"]
        summary["restarted"] = restarted
        summary["final_cli"] = action("finish")
        summary["real_server_hashes"] = {name: hashlib.sha256((work / name).read_bytes()).hexdigest()
                                         for name in ["server-seed.txt", "after-gui-close.txt", "after-reboot.txt"]}
        summary["runtime"] = "actual Microsoft WebView2; formal binary without native-acceptance"
        summary["signature_status"] = "unsigned-test"
        print(ps("Restore"), flush=True)
        restored = True
        summary["restored_lab_boot"] = reboot("restore original Quickemu login")
        summary["restored_console_user"] = wait("original console user", lambda: (value if (value := checked([*SSH, 'powershell -NoProfile -Command "(Get-CimInstance Win32_ComputerSystem).UserName"']).strip()) == summary["prior_console_user"] else None), 240)
        if fixture["ci_cleanup"]:
            assert summary["final_cli"]["ci_cleanup"]["provider_uninstall_exit"] == 0
            assert summary["final_cli"]["ci_cleanup"]["ordinary_file_hashes"] == summary["real_server_hashes"]
            checked([*SCP, "-r", f"{USER}@{HOST}:{guest}/results", run / "guest-results"])
            username, sid = installed["username"], installed["sid"]
            assert re.fullmatch(r"vs_s099_[0-9a-f]{6}", username) and re.fullmatch(r"S-1-5-21-[0-9-]+", sid)
            checked([*SSH, 'powershell -NoProfile -Command "'
                     + f"$u=Get-LocalUser -Name '{username}'; if($u.SID.Value-ne '{sid}'){{throw 'test SID mismatch'}}; "
                     + f"Get-CimInstance Win32_UserProfile -Filter \\\"SID='{sid}'\\\" | Remove-CimInstance; "
                     + f"Get-ScheduledTask | Where-Object {{$_.Principal.UserId -eq '{sid}'}} | Unregister-ScheduledTask -Confirm:$false; Remove-LocalUser -Name '{username}'; "
                     + f"Remove-Item -LiteralPath '{guest}' -Recurse -Force; Remove-Item -LiteralPath 'C:/ProgramData/vaultsyncd-lab-private/{run_id}' -Recurse -Force"
                     + '"'])
            summary["ci_resources_removed"] = True
        summary["ok"] = True
    finally:
        if prepared and not restored:
            try:
                print(ps("Restore"), flush=True)
                reboot("restore original logon after failed validation")
                summary["restored_console_user"] = wait("original console after failure", lambda: (value if (value := checked([*SSH, 'powershell -NoProfile -Command "(Get-CimInstance Win32_ComputerSystem).UserName"']).strip()) == summary["prior_console_user"] else None), 240)
            except Exception as error:
                summary["restore_error"] = str(error)
        if prepared and not summary.get("ci_resources_removed"):
            try:
                checked([*SCP, "-r", f"{USER}@{HOST}:{guest}/results", run / "guest-results"])
            except Exception as error:
                summary["results_collection_error"] = str(error)
        (run / "result.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2))
        (run / "stop").touch()
        for process in [tunnel, tls]:
            if process and process.poll() is None:
                process.terminate()
                process.wait(timeout=10)
        try:
            server.wait(timeout=15)
        except subprocess.TimeoutExpired:
            server.terminate()
            server.wait(timeout=10)
        for name in ["ca.key", "server.key"]:
            (run / name).unlink(missing_ok=True)
        print("ARTIFACTS", run, flush=True)
        resources = "this run's test user/profile/credentials removed" if summary.get("ci_resources_removed") else "installed test-user data retained"
        print("GUEST", guest, "(VM stays online; " + resources + ")", flush=True)
    assert summary["ok"], summary


if __name__ == "__main__":
    main()
