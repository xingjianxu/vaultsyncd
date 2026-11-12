#!/usr/bin/env python3
"""Temporary dev server only; production service and data are never managed here."""

import argparse
import fcntl
import http.client
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import socket
import ssl
import stat
import subprocess
import sys
import time

DEV_BASE = Path("/data/vaultsyncd-dev")
DEFAULT_ROOT = DEV_BASE / "dev-server"
HOST = "wlab.132140.xyz"
PORT = 15401
BACKEND = 18741
CERT = Path("/var/lib/acme/wlab.132140.xyz/fullchain.pem")
KEY = CERT.with_name("key.pem")


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def canonical(path):
    path = Path(path)
    require(path.is_absolute() and path.resolve() == path, f"需要无软链的绝对规范路径：{path}")
    return path


def safe_root(value, create=False):
    require(os.geteuid() != 0, "请以普通开发用户运行")
    base = canonical(DEV_BASE)
    require(base.is_dir() and os.access(base, os.W_OK | os.X_OK), "开发根尚未建立或未授权")
    root = canonical(value)
    require(root != base and root.is_relative_to(base), f"目录必须位于 {base} 之下")
    # Every ancestor must be real, including dangling symlinks which resolve() detects.
    for item in [root, *root.parents]:
        require(not item.is_symlink(), f"目录含软链：{item}")
    if create:
        root.mkdir(mode=0o700, parents=True, exist_ok=True)
    require(root.is_dir(), "尚未初始化，请先运行 init")
    info = root.stat()
    require(info.st_uid == os.geteuid() and not stat.S_IMODE(info.st_mode) & 0o077,
            "开发实例目录必须由当前用户拥有且权限为 0700")
    for name in ("state", "vaults", "config", "runtime", "logs"):
        path = root / name
        require(not path.is_symlink() and path.resolve() == path, f"拒绝目录软链：{path}")
        if create:
            path.mkdir(mode=0o700, exist_ok=True)
        require(path.is_dir() and path.stat().st_uid == os.geteuid()
                and not stat.S_IMODE(path.stat().st_mode) & 0o077,
                f"需要当前用户私有目录：{path}")
    # Never follow a replaced database, log or control file outside this instance.
    for directory in ("state", "config", "runtime", "logs"):
        for parent, dirs, files in os.walk(root / directory, followlinks=False):
            for name in dirs + files:
                require(not (Path(parent) / name).is_symlink(), f"拒绝内部软链：{Path(parent) / name}")
    return root


def read_json(path):
    return json.loads(path.read_text()) if path.exists() else None


def write_json(path, value):
    temporary = path.with_name(path.name + ".new")
    require(not temporary.is_symlink(), f"拒绝软链：{temporary}")
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")
    temporary.replace(path)


def process_identity(pid):
    try:
        directory = Path(f"/proc/{pid}")
        fields = (directory / "stat").read_text().rsplit(")", 1)[1].split()
        if fields[0] == "Z":
            return None
        return {"pid": pid, "boot_id": Path("/proc/sys/kernel/random/boot_id").read_text().strip(),
                "starttime": fields[19], "uid": directory.stat().st_uid,
                "argv": (directory / "cmdline").read_bytes().decode().rstrip("\0").split("\0")}
    except (FileNotFoundError, ProcessLookupError):
        return None


def alive(identity):
    if not identity:
        return False
    actual = process_identity(identity["pid"])
    return actual is not None and all(actual[k] == identity[k] for k in ("boot_id", "starttime", "uid"))


def terminate(identity):
    """Pin the process first, then verify its recorded boot/starttime before signalling."""
    if not identity:
        return
    try:
        fd = os.pidfd_open(identity["pid"])
    except ProcessLookupError:
        return
    try:
        require(identity["uid"] == os.geteuid(), "拒绝向其他用户进程发信号")
        if alive(identity):
            signal.pidfd_send_signal(fd, signal.SIGTERM)
    finally:
        os.close(fd)


def discover_package(value):
    value = value or os.environ.get("DEV_SERVER_PACKAGE")
    if not value:
        text = subprocess.check_output(["systemctl", "show", "vaultsyncd.service", "--property=ExecStart", "--value"], text=True)
        # The deployment wrapper retains the Web package and btrfs PATH, unlike /proc/exe.
        marker = "path="
        require(marker in text, "无法发现现有包，请显式传 --package")
        value = text.split(marker, 1)[1].split(" ;", 1)[0].strip()
        value = str(Path(value).parent)
    package = Path(value).resolve(strict=True)
    if package.name == "bin":
        package = package.parent
    for name in ("vaultsyncd-server", "vaultsyncd-admin"):
        require(os.access(package / "bin" / name, os.X_OK), f"包缺少可执行文件：{name}")
    return package


def server_config(root):
    return (f'[server]\nlisten = "127.0.0.1:{BACKEND}"\n[state]\ndir = {json.dumps(str(root / "state"))}\n'
            f'[vaults]\nallowed_bases = [{json.dumps(str(root / "vaults"))}]\n'
            '[log]\nlevel = "info"\nformat = "json"\n')


def caddy_config():
    return f'''{{
  admin off
  auto_https off
}}
https://{HOST}:{PORT} {{
  tls {CERT} {KEY}
  request_body {{
    max_size 16MiB
  }}
  header {{
    X-Content-Type-Options nosniff
    Referrer-Policy no-referrer
    -Server
  }}
  reverse_proxy 127.0.0.1:{BACKEND} {{
    flush_interval -1
    stream_close_delay 5m
    header_up Host {{hostport}}
    header_up -Forwarded
    header_up X-Forwarded-For {{remote_host}}
    header_up X-Forwarded-Proto https
    header_up X-Forwarded-Host {{hostport}}
    header_up X-Real-IP {{remote_host}}
    transport http {{
      dial_timeout 5s
      response_header_timeout 30s
    }}
  }}
}}
'''


def settings(root):
    data = read_json(root / "config" / "instance.json")
    require(data and data.get("environment") == "dev" and data.get("root") == str(root), "实例配置缺失或根目录不匹配")
    require((root / "config" / "server.toml").read_text() == server_config(root), "服务端配置已变化，拒绝启动越界配置")
    require((root / "config" / "Caddyfile").read_text() == caddy_config(), "代理配置已变化，请核对并重新生成")
    return data


def preflight(root):
    subprocess.run([sys.executable, str(Path(__file__).resolve().parent.parent / "nix/modules/preflight.py"),
                    shutil.which("findmnt") or "findmnt", "--state", str(root / "state")], check=True)


def dev_environment(root):
    environment = os.environ.copy()
    environment.pop("VAULTSYNCD_CONFIG", None)
    environment.pop("VAULTSYNCD_WEB_DIR", None)
    environment["TMPDIR"] = str(root / "runtime")
    environment["XDG_DATA_HOME"] = str(root / "runtime" / "xdg-data")
    environment["XDG_CONFIG_HOME"] = str(root / "runtime" / "xdg-config")
    environment["XDG_CACHE_HOME"] = str(root / "runtime" / "xdg-cache")
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    return environment


def probe(path, tls=False):
    connection = http.client.HTTPConnection("127.0.0.1", BACKEND, timeout=2)
    if tls:
        connection = http.client.HTTPSConnection(HOST, PORT, timeout=2, context=ssl.create_default_context())
        connection.sock = ssl.create_default_context().wrap_socket(
            socket.create_connection(("127.0.0.1", PORT), timeout=2), server_hostname=HOST)
    try:
        connection.request("GET", path)
        response = connection.getresponse()
        require(response.status == 200, f"健康接口 {path} 返回 {response.status}")
        return json.loads(response.read())
    finally:
        connection.close()


def check_health(expected=None):
    require(probe("/v1/health").get("status") == "ok", "后端健康检查失败")
    local = probe("/v1/capabilities")
    require(local.get("server_id") and "vaults" in local.get("features", []), "后端能力响应不完整")
    require(probe("/v1/health", tls=True).get("status") == "ok", "TLS 健康检查失败")
    require(probe("/v1/capabilities", tls=True) == local, "HTTPS 入口与开发后端身份不一致")
    require(not expected or local["server_id"] == expected, "开发服务 ServerId 与上次启动不一致")
    return local["server_id"]


def runtime(root):
    return read_json(root / "runtime" / "processes.json") or {}


def stop(root):
    data = runtime(root)
    supervisor = data.get("supervisor")
    if alive(supervisor):
        terminate(supervisor)
    else:
        # Recover children after a supervisor crash, only with an exact identity match.
        for name in ("caddy", "server"):
            terminate(data.get(name))
    deadline = time.monotonic() + 45
    while any(alive(data.get(name)) for name in ("supervisor", "server", "caddy")):
        require(time.monotonic() < deadline, "优雅停止尚未完成；保留进程记录和数据，请检查日志，未强杀")
        time.sleep(0.15)
    print("dev 服务已停止；状态与 Vault 数据保留")


def ensure_port_available(address, port):
    with socket.socket() as listener:
        # Match server/Caddy reuse semantics: a stopped connection in TIME_WAIT
        # must not block restart, while an actual listener still refuses the bind.
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind((address, port))


def wait_started(root, process, token, expected):
    identity = process_identity(process.pid)
    try:
        deadline = time.monotonic() + 35
        while time.monotonic() < deadline:
            current = runtime(root)
            if current.get("token") == token and current.get("phase") == "ready":
                return check_health(expected)
            require(process.poll() is None, "后台启动失败，请查看 logs --service supervisor 及 server/caddy 日志")
            time.sleep(0.15)
        raise RuntimeError("后台启动超时，请检查 supervisor/server/caddy 日志")
    except BaseException as error:
        terminate(identity)
        try:
            process.wait(timeout=35)
        except subprocess.TimeoutExpired:
            raise RuntimeError("启动未成功，已请求优雅停止但仍在等待；保留进程记录，请检查 status/logs，未强杀") from error
        raise


def start(root, package_override=None):
    data = settings(root)
    current = runtime(root)
    if alive(current.get("supervisor")):
        require(current.get("phase") == "ready", "开发服务正在启动或停止，请查看 status/logs")
        require(not package_override or str(discover_package(package_override)) == data["package"], "请先停止服务再切换包")
        check_health(data.get("server_id"))
        print(f"dev 已运行：https://{HOST}:{PORT}")
        return
    require(not any(alive(current.get(name)) for name in ("server", "caddy")), "发现遗留子进程，请先运行 stop")
    if package_override:
        data["package"] = str(discover_package(package_override))
        write_json(root / "config" / "instance.json", data)
    package = discover_package(data["package"])
    preflight(root)
    # Decode a real certificate and verify key pairing/validity before starting either child.
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(CERT, KEY)
    certificate = ssl._ssl._test_decode_cert(str(CERT))
    require(ssl.cert_time_to_seconds(certificate["notBefore"]) <= time.time()
            < ssl.cert_time_to_seconds(certificate["notAfter"]), "开发证书尚未生效或已经过期")
    require(any(kind == "DNS" and name == HOST for kind, name in certificate["subjectAltName"]), "证书不覆盖开发域名")
    caddy = shutil.which("caddy")
    require(caddy, "devShell 缺少 caddy")
    subprocess.run([caddy, "validate", "--config", str(root / "config" / "Caddyfile"), "--adapter", "caddyfile"],
                   check=True, stdout=subprocess.DEVNULL, env=dev_environment(root), cwd=root)
    for address, port in (("127.0.0.1", BACKEND), ("0.0.0.0", PORT)):
        ensure_port_available(address, port)
    # Keep the supervisor independent of an ephemeral Git worktree and terminal.
    supervisor_script = root / "runtime" / "supervisor.py"
    shutil.copyfile(Path(__file__).resolve(), supervisor_script)
    token = secrets.token_hex(16)
    with (root / "logs" / "supervisor.log").open("ab", buffering=0) as log:
        process = subprocess.Popen([sys.executable, str(supervisor_script), "_supervise", "--root", str(root), "--token", token,
                                    "--package", str(package), "--caddy", caddy], stdin=subprocess.DEVNULL,
                                   stdout=log, stderr=subprocess.STDOUT, start_new_session=True, close_fds=True,
                                   env=dev_environment(root), cwd=root)
    data["server_id"] = wait_started(root, process, token, data.get("server_id"))
    write_json(root / "config" / "instance.json", data)
    print(f"dev 已后台启动：https://{HOST}:{PORT}（ServerId {data['server_id']}）")


def supervise(args):
    root = safe_root(args.root)
    children = {}
    record = {"token": args.token, "phase": "starting", "supervisor": process_identity(os.getpid())}
    stopping = False

    def request_stop(_signal, _frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, request_stop)
    signal.signal(signal.SIGINT, request_stop)
    with (root / "runtime" / "supervisor.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            write_json(root / "runtime" / "processes.json", record)
            environment = dev_environment(root)
            web = Path(args.package) / "share/vaultsyncd/web"
            if web.is_dir():
                environment["VAULTSYNCD_WEB_DIR"] = str(web.resolve())
            commands = {"server": [str(Path(args.package) / "bin/vaultsyncd-server"), "--config", str(root / "config/server.toml")],
                        "caddy": [args.caddy, "run", "--config", str(root / "config/Caddyfile"), "--adapter", "caddyfile"]}
            for name, command in commands.items():
                with (root / "logs" / f"{name}.log").open("ab", buffering=0) as log:
                    children[name] = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=log,
                                                      stderr=subprocess.STDOUT, env=environment, cwd=root,
                                                      start_new_session=True, close_fds=True)
                record[name] = process_identity(children[name].pid)
                write_json(root / "runtime" / "processes.json", record)
            deadline = time.monotonic() + 25
            while not stopping:
                require(all(child.poll() is None for child in children.values()), "子进程已退出")
                try:
                    server_id = check_health(settings(root).get("server_id"))
                    break
                except (OSError, ValueError, RuntimeError, http.client.HTTPException):
                    require(time.monotonic() < deadline, "健康检查超时")
                    time.sleep(0.2)
            if not stopping:
                record.update(phase="ready", server_id=server_id)
                write_json(root / "runtime" / "processes.json", record)
                print(f"dev ready {server_id}", flush=True)
            while not stopping:
                require(all(child.poll() is None for child in children.values()), "子进程意外退出，正在停止其余进程")
                time.sleep(0.25)
        except Exception as error:
            record["error"] = str(error)
            print(f"dev supervisor: {error}", flush=True)
        finally:
            record["phase"] = "stopping"
            write_json(root / "runtime" / "processes.json", record)
            for name in ("caddy", "server"):
                if name in children and children[name].poll() is None:
                    # Popen owns and reaps this child; the PID cannot be reused before wait().
                    children[name].terminate()
            # Never escalate to SIGKILL: unconfirmed server work retains its shutdown protection.
            for child in children.values():
                child.wait()
            record["phase"] = "failed" if "error" in record else "stopped"
            write_json(root / "runtime" / "processes.json", record)


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description="prod 保持 NixOS 部署；本脚本仅管理独立的临时 dev 服务")
    parser.add_argument("command", choices=("init", "start", "stop", "restart", "status", "logs", "_supervise"))
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    parser.add_argument("--package", help="包含 bin/vaultsyncd-server 和 bin/vaultsyncd-admin 的目录，或该 bin 目录")
    parser.add_argument("--login", default=os.environ.get("USER", "owner"))
    parser.add_argument("--service", choices=("server", "caddy", "supervisor", "all"), default="all")
    parser.add_argument("--follow", action="store_true")
    parser.add_argument("--lines", type=int, default=100)
    parser.add_argument("--token", help=argparse.SUPPRESS)
    parser.add_argument("--caddy", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.command == "_supervise":
        supervise(args)
        return
    root = safe_root(args.root, create=args.command == "init")
    if args.command == "logs":
        require(args.lines > 0, "--lines 必须为正数")
        names = ("server", "caddy", "supervisor") if args.service == "all" else (args.service,)
        files = [str(root / "logs" / f"{name}.log") for name in names if (root / "logs" / f"{name}.log").exists()]
        require(files, "尚无运行日志")
        os.execvp("tail", ["tail", "-n", str(args.lines)] + (["-F"] if args.follow else []) + files)
    with (root / "runtime" / "operation.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if args.command == "init":
            require(not alive(runtime(root).get("supervisor")), "请先停止开发服务")
            existing = read_json(root / "config" / "instance.json")
            require(not existing or not existing.get("initialized"), "实例已初始化，拒绝重置 owner")
            package = discover_package(args.package)
            preflight(root)
            (root / "config" / "server.toml").write_text(server_config(root))
            (root / "config" / "Caddyfile").write_text(caddy_config())
            data = {"environment": "dev", "root": str(root), "package": str(package), "initialized": False}
            write_json(root / "config" / "instance.json", data)
            subprocess.run([str(package / "bin/vaultsyncd-admin"), "init", "--state-dir", str(root / "state"), "--login", args.login],
                           check=True, env=dev_environment(root), cwd=root)
            data["initialized"] = True
            write_json(root / "config" / "instance.json", data)
            print(f"dev 已初始化：{root}；运行 start 后台启动")
        elif args.command in ("start", "restart"):
            require(settings(root).get("initialized"), "owner 尚未初始化")
            if args.command == "restart":
                stop(root)
            start(root, args.package)
        elif args.command == "stop":
            stop(root)
        else:
            data = runtime(root)
            result = {"environment": "dev", "url": f"https://{HOST}:{PORT}", "root": str(root),
                      "phase": data.get("phase", "stopped"), "server_id": data.get("server_id"),
                      "processes": {name: {"pid": (data.get(name) or {}).get("pid"), "running": alive(data.get(name))}
                                    for name in ("supervisor", "server", "caddy")}}
            if data.get("phase") in ("ready", "starting", "stopping") and not any(p["running"] for p in result["processes"].values()):
                result["phase"] = "stale"
            print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"dev-server: {error}", file=sys.stderr)
        sys.exit(1)
