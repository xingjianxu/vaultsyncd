#!/usr/bin/env python3
"""S075 real btrfs + isolated SSH/CLI acceptance. Run inside nix develop after cargo build.

Creates only /data/vaultsyncd-temp/vaultsyncd-s075-<run_id>; never opens SQLite. SSH keys are ephemeral,
known_hosts is derived from our own host key (no insecure host-key bypass). Logs stay
outside Git. Requires passwordless sudo for the isolated sshd and nobody UID test.
"""
import getpass
import hashlib
import json
import os
from pathlib import Path
import secrets
import shlex
import signal
import socket
import struct
import subprocess
import time
import urllib.error
import urllib.request

REPO = Path(__file__).resolve().parents[3]
SERVER = REPO / "target/debug/vaultsyncd-server"
ADMIN = REPO / "target/debug/vaultsyncd-admin"
RUN = Path("/data/vaultsyncd-temp") / f"vaultsyncd-s075-{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"


def call(args, **kwargs):
    return subprocess.run([str(x) for x in args], capture_output=True, text=True, **kwargs)


def checked(args, **kwargs):
    result = call(args, **kwargs)
    if result.returncode:
        raise AssertionError(f"command failed ({result.returncode}): {args}: {result.stderr}")
    return result.stdout


def wait_for(function, label, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = function()
        if result:
            return result
        time.sleep(0.2)
    raise AssertionError(f"timed out: {label}")


def frame(command):
    data = json.dumps(command).encode()
    return struct.pack("!I", len(data)) + data


def raw(command):
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(12)
        stream.connect(str(RUN / "state/admin/socket"))
        stream.sendall(frame(command))
        header = stream.recv(4)
        assert len(header) == 4
        size = struct.unpack("!I", header)[0]
        data = bytearray()
        while len(data) < size:
            chunk = stream.recv(size - len(data))
            assert chunk
            data.extend(chunk)
        return json.loads(data)


def main():
    checked(["sudo", "-n", "true"])
    checked(["sudo", "-n", "mkdir", "-m", "755", RUN])
    checked(["sudo", "-n", "chown", f"{os.getuid()}:{os.getgid()}", RUN])
    assert checked(["findmnt", "-no", "FSTYPE", "-T", RUN]).strip() == "btrfs"
    state, vault = RUN / "state", RUN / "vaults/work"
    state.mkdir(mode=0o700)
    vault.mkdir(parents=True)
    sample = vault / "sample.txt"
    sample.write_bytes(b"S075 first preserved bytes\n")
    first_hash = hashlib.sha256(sample.read_bytes()).hexdigest()
    password = secrets.token_urlsafe(32)
    checked([ADMIN, "init", "--state-dir", state], input=password + "\n")
    config = RUN / "server.toml"
    config.write_text(f'[server]\nlisten = "127.0.0.1:0"\n[state]\ndir = "{state}"\n[vaults]\nallowed_bases = ["{vault.parent}"]\n[log]\nformat = "json"\nlevel = "info"\n')
    log = (RUN / "server.log").open("w")
    server = subprocess.Popen([str(SERVER), "--config", str(config)], stdout=log, stderr=log)
    sshd = None
    outputs = []
    try:
        def listening():
            for line in (RUN / "server.log").read_text().splitlines():
                record = json.loads(line).get("fields", {})
                if record.get("message") == "服务端开始监听":
                    return record["listen"]
            assert server.poll() is None, "server exited before listening"
        address = wait_for(listening, "server listening")
        uds = state / "admin/socket"
        assert (state / "admin").stat().st_mode & 0o777 == 0o700
        assert uds.stat().st_mode & 0o777 == 0o600
        print("PASS btrfs state/Vault + socket 0700/0600")

        # A separate SSH daemon: no changes to host sshd, ~/.ssh or system credentials.
        for name in ["host", "client"]:
            checked(["/run/current-system/sw/bin/ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", RUN / name])
        authorized = RUN / "authorized_keys"
        authorized.write_bytes((RUN / "client.pub").read_bytes())
        authorized.chmod(0o600)
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        sshconfig = RUN / "sshd_config"
        sshconfig.write_text(f'Port {port}\nListenAddress 127.0.0.1\nHostKey {RUN}/host\nPidFile {RUN}/sshd.pid\nAuthorizedKeysFile {authorized}\nAllowUsers {getpass.getuser()}\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPubkeyAuthentication yes\nUsePAM no\nPermitRootLogin no\nLogLevel ERROR\n')
        sshlog = (RUN / "sshd.log").open("w")
        sshd = subprocess.Popen(["sudo", "-n", "/run/current-system/sw/bin/sshd", "-D", "-e", "-f", str(sshconfig)], stdout=sshlog, stderr=sshlog)
        wait_for(lambda: (RUN / "sshd.pid").exists(), "isolated sshd")
        known = RUN / "known_hosts"
        known.write_text(f"[127.0.0.1]:{port} " + (RUN / "host.pub").read_text())
        ssh = ["ssh", "-F", "/dev/null", "-p", str(port), "-i", str(RUN / "client"), "-o", f"UserKnownHostsFile={known}", "-o", "StrictHostKeyChecking=yes", "-o", "BatchMode=yes", f"{getpass.getuser()}@127.0.0.1"]

        def cli(*command, ok=True):
            args = [ADMIN, *command, "--socket", uds, "--json"]
            result = call([*ssh, shlex.join([str(x) for x in args])])
            assert (result.returncode == 0) == ok, (result.returncode, result.stdout, result.stderr)
            value = json.loads(result.stdout)
            outputs.append(value)
            return value

        assert cli("status")["result"]["phase"] == "ready"
        registered = cli("vaults", "register", vault)["result"]
        vault_id = registered["vault_id"]
        assert registered["health"] == "healthy"
        assert hashlib.sha256(sample.read_bytes()).hexdigest() == first_hash
        assert cli("vaults")["result"]["vaults"][0]["vault_id"] == vault_id
        assert cli("status")["result"]["vault_count"] == "1"
        assert cli("vaults", "register", "/etc", ok=False)["error"]["code"] == "ACCESS_DENIED"
        print("PASS SSH CLI status/Vault registration/list + out-of-base rejection, original bytes preserved")

        def api(method, path, body=None, token=None):
            data = json.dumps(body).encode() if body is not None else None
            headers = {"Content-Type": "application/json"}
            if token:
                headers["Authorization"] = "Bearer " + token
            request = urllib.request.Request("http://" + address + path, data=data, headers=headers, method=method)
            try:
                with urllib.request.urlopen(request, timeout=15) as response:
                    return json.load(response)
            except urllib.error.HTTPError as error:
                raise AssertionError((error.code, error.read().decode())) from error

        session = api("POST", "/v1/auth/login", {"login": "owner", "password": password, "device_label": "S075-isolated-acceptance"})
        token = session["access_token"]
        prefix = f"/v1/vaults/{vault_id}"

        def snapshot():
            snap = api("POST", prefix + "/snapshots", {}, token)
            page = api("GET", prefix + f'/snapshots/{snap["snapshot_id"]}/entries?limit=100', token=token)
            return snap, page["entries"]

        def imported():
            snap, entries = snapshot()
            return next(((snap, entry) for entry in entries if entry["name"] == "sample.txt"), None)

        snap, entry = wait_for(imported, "real observer import")
        entry_id = entry["entry_id"]
        history = cli("history", vault_id, entry_id)["result"]["items"]
        assert history and history[0]["size"] == str(sample.stat().st_size)
        sample.write_bytes(b"S075 second version bytes\n")
        wait_for(lambda: len(cli("history", vault_id, entry_id)["result"]["items"]) >= 2, "second real committed version")
        snap, entries = snapshot()
        entry = next(e for e in entries if e["entry_id"] == entry_id)
        deleted = api("POST", prefix + "/operations", {
            "op_id": secrets.token_hex(16), "device_id": secrets.token_hex(16), "kind": "delete",
            "server_epoch": snap["server_epoch"], "entry_id": entry_id, "base_revision": entry["revision"],
            "base_version_id": entry["version_id"], "local_generation": "1"
        }, token)
        assert deleted["state"] == "committed", deleted
        assert not sample.exists()
        trash = cli("trash", vault_id)["result"]["items"]
        assert trash and trash[0]["entry_id"] == entry_id and trash[0]["expires_at"] > trash[0]["deleted_at"]
        assert len(cli("history", vault_id, entry_id)["result"]["items"]) >= 2
        assert cli("trash", "00" * 16, ok=False)["error"]["code"] == "ACCESS_DENIED"
        print("PASS real observer committed history + real delete transaction/trash + deleted-entry history")

        # Mount/path failure becomes an actual unavailable Vault, not an empty success.
        vault.rename(vault.with_name("offline"))
        wait_for(lambda: any(p["kind"] == "vault_health" for p in cli("problems")["result"]["items"]), "unavailable Vault problem")
        assert cli("problems", "1")["result"]["items"] == []
        print("PASS real unavailable Vault reported in problems; bounded live pages")

        denied = call(["sudo", "-n", "-u", "nobody", ADMIN, "status", "--socket", uds, "--json"])
        assert denied.returncode != 0
        print("PASS unauthorized OS UID rejected by private socket path")
        # Also bypass only this run's filesystem ACL to prove the independent SO_PEERCRED check.
        state.chmod(0o755)
        (state / "admin").chmod(0o755)
        uds.chmod(0o666)
        try:
            code = "import socket; s=socket.socket(socket.AF_UNIX); s.settimeout(3); s.connect(" + repr(str(uds)) + "); assert s.recv(1) == b''"
            checked(["sudo", "-n", "-u", "nobody", "python3", "-c", code])
        finally:
            uds.chmod(0o600)
            (state / "admin").chmod(0o700)
            state.chmod(0o700)
        print("PASS unauthorized UID rejected by SO_PEERCRED even with deliberately permissive test-only ACL")
        for size in [0, 1024 * 1024 + 1]:
            with socket.socket(socket.AF_UNIX) as stream:
                stream.settimeout(3)
                stream.connect(str(uds))
                stream.sendall(struct.pack("!I", size))
                assert stream.recv(1) == b""
        assert raw(["v999", "status"])["status"] == 400
        assert raw(["v1", "trash", vault_id, "01"])["status"] == 400
        assert raw(["v1", "not-a-command"])["status"] == 400
        assert cli("status")["result"]["vault_count"] == "1"
        print("PASS oversized/zero frames + malformed commands fail; service remains responsive")
        captured = json.dumps(outputs) + (RUN / "server.log").read_text()
        for secret in [password, session["access_token"], session["refresh_token"], "$argon2", "password_hash"]:
            assert secret not in captured
        (RUN / "cli-results.json").write_text(json.dumps(outputs, ensure_ascii=False, indent=2))
        print("PASS machine JSON and service logs contain no password/hash/access/refresh credentials")
        server.send_signal(signal.SIGTERM)
        assert server.wait(timeout=15) == 0
        assert not uds.exists(), "graceful stop must remove only its own socket"
        server = subprocess.Popen([str(SERVER), "--config", str(config)], stdout=log, stderr=log)
        wait_for(lambda: uds.exists(), "socket after restart")
        assert cli("status")["result"]["vault_count"] == "1"
        assert cli("trash", vault_id)["result"]["items"][0]["entry_id"] == entry_id
        print("PASS graceful socket cleanup + restart preserves online management state")
        print(f"PASS S075 acceptance artifacts: {RUN}")
    finally:
        if server.poll() is None:
            server.send_signal(signal.SIGTERM)
            server.wait(timeout=20)
        if sshd is not None and (RUN / "sshd.pid").exists():
            checked(["sudo", "-n", "kill", "-TERM", (RUN / "sshd.pid").read_text().strip()])
            sshd.wait(timeout=10)
        # Ephemeral private keys are not needed after acceptance.
        for name in ["host", "client"]:
            (RUN / name).unlink(missing_ok=True)
        log.close()
        print(f"Artifacts retained outside Git: {RUN}")


if __name__ == "__main__":
    main()
