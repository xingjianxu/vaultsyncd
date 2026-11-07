#!/usr/bin/env python3
"""Build the supported release archives from one clean commit on Linux x64."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import shlex
import shutil
import subprocess
import sys
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
PLATFORMS = {
    "linux-x64": ("portable", "x86_64-unknown-linux-gnu"),
    "linux-arm64": ("portable-aarch64", "aarch64-unknown-linux-gnu"),
    "windows-x64": (None, "x86_64-pc-windows-msvc"),
}


def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT, text=True).strip()


def source():
    if git("status", "--porcelain", "--untracked-files=all"):
        raise ValueError("发布需要干净、已提交的源码；请先提交当前修改。")
    return {"commit": git("rev-parse", "HEAD"), "tree": git("rev-parse", "HEAD^{tree}"),
            "version": tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]}


def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def copy_file(source, destination):
    if source.is_symlink() or not source.is_file():
        raise ValueError(f"缺少普通文件：{source}")
    if destination.exists():
        raise ValueError(f"拒绝覆盖：{destination}")
    shutil.copyfile(source, destination)
    if sha(source) != sha(destination):
        raise ValueError(f"复制校验失败：{destination}")


def check_identity(record, expected, target, windows=False):
    identity = record["source"] if windows else record["sourceInfo"]
    version = identity["version"] if windows else record["version"]
    if identity["commit"] != expected["commit"] or version != expected["version"] or record["target"] != target:
        raise ValueError("组件来源、版本或平台不匹配")
    if windows and identity["tree"] != expected["tree"]:
        raise ValueError("Windows 组件源码树不匹配")
    inputs = identity["inputs"] if windows else record["inputs"]
    if not inputs or any(sha(ROOT / name) != digest for name, digest in inputs.items()):
        raise ValueError("组件锁文件/协议输入不匹配")


class Builder:
    def __init__(self, stage, expected):
        self.stage, self.expected = stage, expected
        self.commands = []
        self.env = os.environ.copy()
        # Toolchain setup stays in the repository cache, never in global homes.
        self.env.setdefault("VAULTSYNCD_WINDOWS_CACHE", str(ROOT / ".cache/windows"))

    def run(self, command, capture=False):
        number = len(self.commands) + 1
        log = self.stage / "logs" / f"{number:02}.log"
        self.commands.append({"argv": list(map(str, command)), "log": str(log.relative_to(self.stage))})
        print(f"[{number}] {shlex.join(list(map(str, command)))}\n    日志：{log}", flush=True)
        with log.open("wb") as output:
            result = subprocess.run(command, cwd=ROOT, env=self.env,
                                    stdout=subprocess.PIPE if capture else output, stderr=output)
            if capture:
                output.write(result.stdout)
        if result.returncode:
            raise RuntimeError(f"步骤 {number} 失败（退出码 {result.returncode}），日志：{log}")
        return result.stdout if capture else None

    def linux(self, name):
        attribute, target = PLATFORMS[name]
        outputs = json.loads(self.run(["nix", "build", f".#{attribute}",
                                       "--no-link", "--json"], capture=True))
        if len(outputs) != 1:
            raise ValueError("Nix 输出数量异常")
        directory = Path(outputs[0]["outputs"]["out"])
        manifest = directory / "runtime/share/vaultsyncd/release-manifest.json"
        record = json.loads(manifest.read_text())
        check_identity(record, self.expected, target)
        destination = self.stage / name
        destination.mkdir()
        archive = f"vaultsyncd-{self.expected['version']}-{target}.tar.gz"
        copy_file(directory / archive, destination / archive)
        sources = record["corresponding_source_archive"]
        if sources["name"] != "vaultsyncd-runtime-sources.tar.gz" or sha(directory / sources["name"]) != sources["sha256"]:
            raise ValueError("Linux 运行库对应源码校验失败")
        copy_file(directory / sources["name"], destination / sources["name"])
        copy_file(manifest, destination / "release-manifest.json")

    def windows(self):
        prefix = ["nix", "develop", ".#windows", "--command"]
        self.run(prefix + ["bash", "tests/platform/windows/setup-toolchain.sh"])
        # pnpm 12 fetch reads the lockfile directly; frozen is an install flag.
        self.run(prefix + ["pnpm", "fetch"])
        directory = self.stage / "work/windows"
        self.run(prefix + ["cargo", "xtask", "release", "build", "windows-zip", str(directory)])
        manifest = self.stage / "work/windows-manifest.json"
        self.run(prefix + ["cargo", "xtask", "release", "assemble", str(manifest), str(directory)])
        self.run(prefix + ["cargo", "xtask", "release", "verify", str(manifest)])
        record = json.loads((directory / "receipt.json").read_text())
        check_identity(record, self.expected, PLATFORMS["windows-x64"][1], windows=True)
        if record["component"] != "windows-zip" or list((directory / "payload").glob("*setup.exe")):
            raise ValueError("Windows ZIP 组件异常：包含安装器或类型错误")
        destination = self.stage / "windows-x64"
        destination.mkdir()
        archive = f"vaultsyncd-{self.expected['version']}-x64-unsigned.zip"
        copy_file(directory / "payload" / archive, destination / archive)
        copy_file(directory / "receipt.json", destination / "receipt.json")


def output_directory(value):
    directory = Path(value).expanduser().resolve()
    if directory.is_relative_to(ROOT):
        relative = directory.relative_to(ROOT)
        if not relative.parts or subprocess.run(["git", "check-ignore", "-q", str(relative / "release-output")], cwd=ROOT).returncode:
            raise ValueError("仓库内输出目录必须已被 Git 忽略（推荐 dist/releases）。")
    return directory


def publish(builder, selected, destination):
    if builder.expected != source():
        raise ValueError("构建期间源码发生变化，拒绝发布")
    shutil.rmtree(builder.stage / "work")
    files = [{"path": str(path.relative_to(builder.stage)), "bytes": path.stat().st_size, "sha256": sha(path)}
             for path in sorted(builder.stage.rglob("*")) if path.is_file()]
    (builder.stage / "SHA256SUMS").write_text("".join(f"{item['sha256']}  {item['path']}\n" for item in files))
    manifest = {"format": 1, "source": builder.expected, "platforms": selected,
                "signature_status": "unsigned-test", "native_acceptance": "not-run-by-this-tool",
                "commands": builder.commands, "files": files}
    (builder.stage / "release-manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n")
    if destination.exists():
        raise ValueError(f"拒绝覆盖已有发布目录：{destination}")
    builder.stage.rename(destination)


def main(argv=None):
    parser = argparse.ArgumentParser(description="在 Linux x64/Nix 上一键构建 Linux 双架构包和 Windows ZIP。")
    parser.add_argument("--platform", nargs="+", choices=["all", *PLATFORMS], default=["all"], help="默认全部；可选择一个或多个平台")
    parser.add_argument("--output-dir", default=str(ROOT / "dist/releases"), help="输出根目录，默认仓库 dist/releases")
    parser.add_argument("--plan", action="store_true", help="仅显示平台和输出根目录，不构建")
    args = parser.parse_args(argv)
    selected = list(PLATFORMS) if "all" in args.platform else list(dict.fromkeys(args.platform))
    directory = output_directory(args.output_dir)
    if args.plan:
        print(json.dumps({"platforms": selected, "output_dir": str(directory), "windows": "ZIP only; unsigned-test"}, indent=2))
        return
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise ValueError("此入口需要 Linux x86_64 主机和 Nix，目标支持 Linux x64/ARM64 与 Windows x64。")
    expected = source()
    destination = directory / f"v{expected['version']}-{expected['commit'][:12]}"
    if destination.exists():
        raise ValueError(f"拒绝覆盖已有发布目录：{destination}；可使用 --output-dir 指定新位置。")
    cache = ROOT / ".cache"
    cache.mkdir(exist_ok=True)
    with (cache / "release.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise RuntimeError("已有一键构建正在运行，请等待其完成。") from error
        directory.mkdir(parents=True, exist_ok=True)
        stage = Path(tempfile.mkdtemp(prefix=f".partial-{destination.name}-", dir=directory))
        for name in ("logs", "work"):
            (stage / name).mkdir()
        builder = Builder(stage, expected)
        try:
            for name in selected:
                builder.windows() if name == "windows-x64" else builder.linux(name)
            publish(builder, selected, destination)
        except BaseException:
            print(f"构建未完成，现场与日志保留在：{stage}", file=sys.stderr)
            raise
    print(f"构建完成：{destination}\n校验：cd {shlex.quote(str(destination))} && sha256sum -c SHA256SUMS")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, OSError, KeyError, subprocess.CalledProcessError) as error:
        print(f"release: {error}", file=sys.stderr)
        sys.exit(1)
