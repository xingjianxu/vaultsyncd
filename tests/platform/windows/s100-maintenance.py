#!/usr/bin/env python3
"""Real upgrade/rollback/uninstall of the retained S099 Windows standard user and actual CFAPI data.

Run in nix develop .#web-tests, with a same-HEAD windows-client component directory argument.
The caller must coordinate the VM before starting: two controlled restarts restore Quickemu at end; the retained password never leaves the guest.
"""
import hashlib
import json
import os
import socket
from pathlib import Path
import subprocess
import sys
import time
import uuid


ROOT = Path(__file__).resolve().parents[3]
HOST = os.environ.get("GUEST_HOST", "127.0.0.1")
USER = os.environ.get("GUEST_USER", "Quickemu")
PORT = os.environ.get("GUEST_PORT", "2222")
KEY = os.environ.get("GUEST_SSH_KEY", "/data/vaultsyncd-lab/win-lab/ssh/vaultsyncd-lab_ed25519")
OPTIONS = ["-i", KEY, "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=no",
           "-o", "BatchMode=yes", "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=5"]
SSH = ["ssh", "-p", PORT, *OPTIONS, f"{USER}@{HOST}"]
SCP = ["scp", "-P", PORT, *OPTIONS]


def checked(args, **kwargs):
    result = subprocess.run([str(x) for x in args], cwd=ROOT, text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kwargs)
    if result.returncode:
        raise RuntimeError(f"{args[0]} failed ({result.returncode}): {result.stderr[-2000:]}")
    return result.stdout


def main():
    component = Path(sys.argv[1]).resolve()
    receipt = json.loads((component / "receipt.json").read_text())
    assert receipt["component"] == "windows-client" and receipt["target"] == "x86_64-pc-windows-msvc"
    assert receipt["source"]["commit"] == checked(["git", "rev-parse", "HEAD"]).strip()
    run_id = f"s100-{int(time.time())}-{os.getpid()}"
    run = ROOT / "artifacts/S100" / run_id
    run.mkdir(parents=True)
    checked(["nix", "develop", "--command", "cargo", "xtask", "release", "assemble",
             run / "release-manifest.json", component])
    checked(["nix", "develop", "--command", "cargo", "xtask", "release", "verify",
             run / "release-manifest.json"])
    checked(["nix", "develop", "--command", "cargo", "build", "--locked", "-p",
             "vaultsyncd-server-tests", "--example", "e2e-native-server"])
    helper = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")) / "debug/examples/e2e-native-server"
    scenario = {"server_seed": [{"path": "downloaded.txt", "content": "S100 downloaded baseline"}, {"path": "dirty.txt", "content": "S100 dirty baseline"}, {"path": "cloud-only.bin", "content": "C" * 1048576}]}
    (run / "scenario.json").write_text(json.dumps(scenario))
    server = subprocess.Popen([str(helper), str(run / "host-fixture.json"), str(run / "stop"),
                               str(run / "scenario.json")], cwd=ROOT,
                              stdout=(run / "server.log").open("w"), stderr=subprocess.STDOUT)
    tls = tunnel = proxy = None
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
            except (RuntimeError, ValueError, OSError) as error:
                last = str(error)
            time.sleep(.3)
        raise RuntimeError(f"timeout {label}: {last}")

    def ps(mode):
        return checked([*SSH, f"powershell -NoProfile -ExecutionPolicy Bypass -File {guest}/s100-maintenance.ps1 -Mode {mode}"])

    def read_guest(path):
        return checked([*SSH, f'powershell -NoProfile -Command "[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false); if(Test-Path \'{path}\'){{Get-Content -Raw -Encoding UTF8 \'{path}\'}}else{{exit 3}}"'])

    def ready(filename):
        def poll():
            error = subprocess.run([*SSH, f'powershell -NoProfile -Command "[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false); if(Test-Path \'{results}/worker-error.json\'){{Get-Content -Raw -Encoding UTF8 \'{results}/worker-error.json\'}}"'],
                                   capture_output=True, text=True, cwd=ROOT)
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
        def answer():
            try:
                error = read_guest(results + "/worker-error.json")
            except RuntimeError:
                error = ""
            if error.strip():
                raise AssertionError("native worker: " + error)
            value = json.loads(read_guest(results + "/response.json"))
            return value if value["id"] == request_id else None
        value = wait(name, answer, 300)
        summary[name] = value
        (run / "checkpoint.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2))
        print("NATIVE_ACTION_PASSED", name, flush=True)
        return value

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
                                  cwd=ROOT, stdout=(run / "tunnel.log").open("a"), stderr=subprocess.STDOUT)
        time.sleep(.5)
        assert tunnel.poll() is None, "isolated authenticated tunnel failed"

    def reboot(label, port=None):
        before = boot()
        print(f"CONTROLLED_REBOOT {label}; prior resources and logon configuration saved", flush=True)
        # SSH disconnect is expected; a different real boot timestamp is required before continuing.
        try:
            subprocess.run([*SSH, 'powershell -NoProfile -Command "Restart-Computer -Force"'],
                           cwd=ROOT, capture_output=True, timeout=30)
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
        assert work.resolve().is_relative_to(ROOT / "target"), "rebuild stale embedded helper in this worktree"
        from urllib.parse import urlparse
        backend = str(urlparse(fixture["url"]).port)
        checked(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj",
                 "/CN=vaultsyncd-S100-CA", "-keyout", run / "ca.key", "-out", run / "ca.pem"])
        checked(["openssl", "req", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=127.0.0.1",
                 "-keyout", run / "server.key", "-out", run / "server.csr"])
        (run / "server.ext").write_text("subjectAltName=IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
        checked(["openssl", "x509", "-req", "-days", "1", "-in", run / "server.csr", "-CA", run / "ca.pem",
                 "-CAkey", run / "ca.key", "-CAcreateserial", "-extfile", run / "server.ext", "-out", run / "server.pem"])
        proxy = subprocess.Popen([sys.executable, str(ROOT / "tests/windows-integration/s100-upload-fault-proxy.py"), str(run), backend], cwd=ROOT, stdout=(run / "proxy.log").open("w"), stderr=subprocess.STDOUT)
        proxy_port = wait("actual upload-fault proxy", lambda: (int((run / "proxy-port").read_text()) if (run / "proxy-port").exists() else None))
        tls = subprocess.Popen([sys.executable, str(ROOT / "tests/platform/windows/s081-tls.py"), str(run), str(proxy_port)],
                               cwd=ROOT, stdout=(run / "tls.log").open("w"), stderr=subprocess.STDOUT)
        tls_port = wait("private TLS listener", lambda: (int((run / "tls-port").read_text()) if (run / "tls-port").exists() else None))
        setup = next((component / "payload").glob("*-unsigned-setup.exe"))
        fixture.update({"url": "https://127.0.0.1:18775", "serverId": uuid.uuid4().hex,
                        "source_commit": receipt["source"]["commit"], "installer": setup.name})
        (run / "fixture.json").write_text(json.dumps(fixture))
        script = run / "s100-maintenance.ps1"
        script.write_text((ROOT / "tests/platform/windows/s100-maintenance.ps1").read_text(), encoding="utf-8-sig")
        secret_script = run / "s099-logon-secrets.ps1"
        secret_script.write_text((ROOT / "tests/platform/windows/s099-logon-secrets.ps1").read_text(), encoding="utf-8-sig")
        (run / "hashes.json").write_text(json.dumps({path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in [setup, script, secret_script]}))
        checked([*SSH, f'powershell -NoProfile -Command "New-Item -ItemType Directory \'{guest}\' | Out-Null"'])
        checked([*SCP, setup, run / "fixture.json", run / "hashes.json", run / "ca.pem",
                 script, secret_script, f"{USER}@{HOST}:{guest}/"])
        # The backup includes real prior logon secrets and stays ACL-protected in the guest only.
        # Restore is attempted even if Prepare partially fails.
        prepared = True
        summary["prior_console_user"] = checked([*SSH, 'powershell -NoProfile -Command "(Get-CimInstance Win32_ComputerSystem).UserName"']).strip()
        assert summary["prior_console_user"]
        print(ps("Prepare"), flush=True)
        summary["retained_user_boot"] = reboot("retained S099 standard-user login", tls_port)
        summary["ready"] = ready("ready.json")
        assert summary["ready"]["username"] == "vs_s099_4fcc1e"
        enrolled = action("enroll")
        assert enrolled["cloud_metadata"][:3] == [1, 0, 1048576]
        (run / "block-uploads").touch()
        dirty = action("dirty")
        wait("actual disconnected upload begin", lambda: (run / "blocked-upload-begins.log").exists(), 90)
        assert dirty["dirty_file"]["dirty"]
        action("upgrade-fail")
        upgraded = action("upgrade")
        assert upgraded["source"]["commit"] == receipt["source"]["commit"]
        assert upgraded["cloud_metadata"][:3] == [1, 0, 1048576]
        prompt = action("uninstall-prompt")
        assert prompt["default_no"] and "cloud-only" in prompt["text"]
        monitor = Path(os.environ.get("VAULTSYNCD_QEMU_MONITOR", "/data/vaultsyncd-lab/win-lab/windows-11/windows-11-monitor.socket"))
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(5)
            connection.connect(str(monitor))
            connection.recv(4096)
            connection.sendall(f"screendump {run / 'actual-uninstall-prompt.ppm'}\n".encode())
            connection.recv(4096)
        wait("native uninstall screenshot", lambda: (run / "actual-uninstall-prompt.ppm").exists() and (run / "actual-uninstall-prompt.ppm").stat().st_size > 1000)
        program_only = action("confirm-program-only")
        assert program_only["root_registered"] and program_only["state_and_cache_retained"]
        assert program_only["cloud_metadata"][:3] == [1, 0, 1048576]
        action("reinstall")
        refused = action("provider-refusal")
        assert refused["exit_code"] == 27 and refused["cloud_metadata"][:3] == [1, 0, 1048576]
        materialized = action("materialize")
        assert materialized["cloud_hash"] == hashlib.sha256(b"C" * 1048576).hexdigest()
        removed = action("provider-uninstall")
        assert removed["exit_code"] == 0 and removed["cloud_metadata"][0] == 0 and removed["root_hresult"] < 0
        assert removed["state_retained"] and removed["cache_retained"]
        final = action("finish")
        assert final["processes"] == []
        assert (work / "dirty.txt").read_bytes() == b"S100 dirty baseline", "unconfirmed dirty is not a remote commit"
        summary["remote_dirty_baseline_unchanged"] = True
        summary["signature_status"] = "unsigned-test"
        summary["previous_client"] = "actual S099 development source cf1d174; no previous formal release claimed"
        print(ps("Restore"), flush=True)
        restored = True
        summary["restored_lab_boot"] = reboot("restore original Quickemu login")
        summary["restored_console_user"] = wait("original console user", lambda: (value if (value := checked([*SSH, 'powershell -NoProfile -Command "(Get-CimInstance Win32_ComputerSystem).UserName"']).strip()) == summary["prior_console_user"] else None), 240)
        summary["native_ok"] = True
    finally:
        if prepared and not restored:
            try:
                print(ps("Restore"), flush=True)
                reboot("restore original logon after failed validation")
                summary["restored_console_user"] = wait("original console after failure", lambda: (value if (value := checked([*SSH, 'powershell -NoProfile -Command "(Get-CimInstance Win32_ComputerSystem).UserName"']).strip()) == summary["prior_console_user"] else None), 240)
            except Exception as error:
                summary["restore_error"] = str(error)
        if prepared:
            try:
                checked([*SCP, "-r", f"{USER}@{HOST}:{guest}/results", run / "guest-results"])
            except Exception as error:
                summary["results_collection_error"] = str(error)
        (run / "result.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2))
        (run / "stop").touch()
        for process in [tunnel, tls, proxy]:
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
        print("GUEST", guest, "(VM stays online; retained user data and recovery evidence)", flush=True)
    assert summary.get("native_ok"), summary
    count = 0
    dirty_hash = hashlib.sha256(b"S100 unconfirmed dirty bytes").hexdigest()
    dirty_readable = False
    for manifest_path in (run / "guest-results/recovery-exports").rglob("manifest.json"):
        manifest = json.loads(manifest_path.read_text())
        for item in manifest["files"]:
            assert Path(item["name"]).name == item["name"] and item["name"] not in (".", "..")
            data = (manifest_path.parent / item["name"]).read_bytes()
            assert len(data) == item["size"] and hashlib.sha256(data).hexdigest() == item["sha256"]
            dirty_readable |= item["source"] is not None and item["sha256"] == dirty_hash
            count += 1
    assert dirty_readable and count > 0, "independent recovery readback lacks latest dirty bytes"
    (run / "independent-export-proof.json").write_text(json.dumps({"ok": True, "files_read_and_hashed": count, "dirty_sha256": dirty_hash}, indent=2))
    summary["ok"] = True
    summary["export_files_independently_hashed"] = count
    (run / "result.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2))
    print("PASS independent recovery package readback", count, flush=True)


if __name__ == "__main__":
    main()
