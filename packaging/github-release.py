#!/usr/bin/env python3
"""Publish a verified release.py bundle through the GitHub CLI, without CI."""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
from urllib.parse import quote

VAULT = Path(__file__).resolve().parents[1]
TARGETS = {"windows-x64": "x86_64-pc-windows-msvc"}


def run(*args, **kwargs):
    result = subprocess.run(args, cwd=VAULT, capture_output=True, **kwargs)
    if result.returncode:
        raise ValueError(f"命令失败：{args[0]} {args[1]}（退出码 {result.returncode}）\n"
                         + result.stderr.decode(errors="replace"))
    return result.stdout


def git(*args):
    return run("git", *args).decode().strip()


def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def regular(root, name):
    relative = PurePosixPath(name)
    if not name or relative.is_absolute() or str(relative) != name or ".." in relative.parts or "\\" in name:
        raise ValueError(f"非法产物路径：{name}")
    path = root
    for part in relative.parts:
        path = path / part
        if path.is_symlink():
            raise ValueError(f"拒绝符号链接：{name}")
    if not path.is_file():
        raise ValueError(f"缺少普通文件：{name}")
    return path


def repository(remote):
    url = git("remote", "get-url", "--push", remote)
    match = re.fullmatch(r"(?:https://github\.com/|git@github\.com:|ssh://git@github\.com/)([\w.-]+/[\w.-]+?)(?:\.git)?/?", url)
    if not match:
        raise ValueError("远端必须是 github.com 的 HTTPS/SSH 仓库地址，且不含凭据")
    return match[1]


def bundle(directory):
    manifest_path = regular(directory, "release-manifest.json")
    manifest_bytes = manifest_path.read_bytes()
    manifest = json.loads(manifest_bytes)
    source = manifest["source"]
    commit, version = source["commit"], source["version"]
    if (manifest["format"] != 1 or not re.fullmatch(r"[0-9a-f]{40}", commit)
            or not re.fullmatch(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", version)):
        raise ValueError("不支持的清单格式或源码身份")
    if (git("rev-parse", f"{commit}^{{tree}}") != source["tree"]
            or tomllib.loads(run("git", "show", f"{commit}:Cargo.toml").decode())["workspace"]["package"]["version"] != version):
        raise ValueError("清单的源码树或版本与 Git 提交不符")
    platforms = manifest["platforms"]
    if platforms != ["windows-x64"]:
        raise ValueError("发布仅接受 Windows x64 便携 ZIP；请用 tools/release.sh 重新生成单平台构建目录")
    if manifest["signature_status"] != "unsigned-test" or manifest["native_acceptance"] != "not-run-by-this-tool":
        raise ValueError("不支持的签名或验收状态")
    listed = {}
    for item in manifest["files"]:
        name = item["path"]
        path = regular(directory, name)
        if name in listed or path.stat().st_size != item["bytes"] or sha(path) != item["sha256"]:
            raise ValueError(f"产物大小/哈希/重复路径校验失败：{name}")
        listed[name] = item
    checksum = regular(directory, "SHA256SUMS").read_text()
    expected = "".join(f"{item['sha256']}  {item['path']}\n" for item in manifest["files"])
    if checksum != expected:
        raise ValueError("构建 SHA256SUMS 与清单不符")
    assets = {}
    for name in platforms:
        receipt_name = f"{name}/receipt.json"
        receipt = json.loads(regular(directory, receipt_name).read_text())
        identity = receipt["source"]
        component_version = identity["version"]
        if identity["commit"] != commit or component_version != version or receipt["target"] != TARGETS[name]:
            raise ValueError("组件来源、版本或目标不符")
        if identity["tree"] != source["tree"] or receipt["component"] != "windows-zip":
            raise ValueError("Windows 组件类型或源码树不符")
        inputs = identity["inputs"]
        if not inputs:
            raise ValueError("组件缺少构建输入")
        for path, digest in inputs.items():
            if hashlib.sha256(run("git", "show", f"{commit}:{path}")).hexdigest() != digest:
                raise ValueError(f"组件输入与 Git 提交不符：{path}")
        archive = f"vaultsyncd-{version}-x64-unsigned.zip"
        entries = [item for item in receipt["files"] if item["path"] == archive]
        path = regular(directory, f"{name}/{archive}")
        if len(entries) != 1 or entries[0]["bytes"] != path.stat().st_size or entries[0]["sha256"] != sha(path):
            raise ValueError("Windows ZIP 与组件收据不符")
        for relative in (receipt_name, f"{name}/{archive}"):
            if relative not in listed:
                raise ValueError(f"组件文件未登记在构建清单：{relative}")
        assets[archive] = path
    digests = {name: listed[str(path.relative_to(directory))]["sha256"]
               for name, path in assets.items()}
    return manifest, assets, digests


class GitHub:
    def __init__(self, repo):
        self.repo = repo
        self.prefix = f"repos/{repo}"

    def api(self, suffix, missing=False):
        result = subprocess.run(["gh", "api", "--hostname", "github.com", "--include",
                                 f"{self.prefix}/{suffix}"], cwd=VAULT, capture_output=True, text=True)
        header, separator, body = result.stdout.partition("\n\n")
        status = re.match(r"HTTP/\S+ (\d{3})", header)
        if missing and status and status[1] == "404":
            return None
        if result.returncode or not separator or not status or status[1] != "200":
            raise ValueError(f"GitHub 查询失败：{suffix}\n{result.stderr}")
        return json.loads(body)

    def release(self, *args):
        return run("gh", "release", *args, "--repo", f"github.com/{self.repo}")

    def find_release(self, tag):
        # The tag endpoint only promises published releases. Listing with push
        # permission also includes drafts, including ones on later pages.
        matches = []
        page = 1
        while True:
            records = self.api(f"releases?per_page=100&page={page}")
            matches.extend(record for record in records if record["tag_name"] == tag)
            if len(records) < 100:
                break
            page += 1
        if len(matches) > 1:
            raise ValueError("同一标签存在多个 Release，需人工检查")
        return matches[0] if matches else None

    def tag(self, tag, commit):
        ref = self.api(f"git/ref/tags/{quote(tag, safe='')}", missing=True)
        if ref is None:
            return
        obj = ref["object"]
        for _ in range(10):
            if obj["type"] != "tag":
                break
            obj = self.api(f"git/tags/{obj['sha']}")["object"]
        if obj["type"] != "commit" or obj["sha"] != commit:
            raise ValueError("远端标签指向其他提交，拒绝移动标签")

    def verify_assets(self, release, assets, stage, complete=False):
        seen = set()
        for item in release["assets"]:
            name = item["name"]
            if name not in assets or name in seen or item["size"] != assets[name].stat().st_size or item["state"] != "uploaded":
                raise ValueError(f"远端草稿附件不匹配：{name}")
            # GitHub may omit digest on older assets; download those to verify bytes.
            digest = item.get("digest")
            if not digest:
                downloaded = stage / "download-check"
                with downloaded.open("wb") as output:
                    result = subprocess.run(["gh", "api", "--hostname", "github.com",
                                             f"{self.prefix}/releases/assets/{item['id']}",
                                             "-H", "Accept: application/octet-stream"], cwd=VAULT, stdout=output)
                if result.returncode:
                    raise ValueError(f"下载校验失败：{name}")
                digest = "sha256:" + sha(downloaded)
                downloaded.unlink()
            if digest != "sha256:" + sha(assets[name]):
                raise ValueError(f"远端附件哈希不匹配：{name}")
            seen.add(name)
        if complete and seen != set(assets):
            raise ValueError("远端附件未全部上传，保留草稿")
        return seen


def publish(api, manifest, assets, stage, args):
    if manifest["platforms"] != ["windows-x64"]:
        raise ValueError("发布仅接受 Windows x64 便携 ZIP")
    source = manifest["source"]
    commit, tag = source["commit"], args.tag
    run("gh", "auth", "status", "--hostname", "github.com")
    remote_commit = api.api(f"commits/{commit}", missing=True)
    if remote_commit is None or remote_commit["sha"] != commit:
        raise ValueError("产物源码提交尚未推送至目标仓库；请先推送对应分支")
    api.tag(tag, commit)
    existing = api.find_release(tag)
    marker = f"<!-- vaultsyncd-source:{commit} -->"
    if existing is not None:
        if (not args.resume or not existing["draft"] or marker not in (existing["body"] or "")
                or existing["target_commitish"] != commit or existing["prerelease"] != (not args.stable)):
            raise ValueError("Release 已存在；仅 --resume 可续传本脚本创建的同源同类型草稿")
    else:
        if args.resume:
            raise ValueError("--resume 未找到可续传的草稿")
        notes = args.notes_file.read_text() if args.notes_file else ""
        checksums = "".join(f"`{sha(path)}  {name}`\n" for name, path in assets.items())
        notes += (f"\n\n版本：{source['version']}\n\n源码：`{commit}`\n\n"
                  f"平台：{', '.join(manifest['platforms'])}\n\n"
                  "产物未签名（unsigned-test）。本工具未执行原生验收，不代表首发门禁通过。\n"
                  "仅提供 Windows x64 便携 ZIP；来源收据、日志及构建校验表保留在本地。\n\n"
                  "ZIP SHA-256：\n\n" + checksums + "\n" + marker + "\n")
        notes_path = stage / "notes.md"
        notes_path.write_text(notes)
        command = ["create", tag, "--target", commit, "--title", args.title or f"vaultsyncd {tag}",
                   "--notes-file", str(notes_path), "--draft", "--latest=false"]
        if not args.stable:
            command.append("--prerelease")
        api.release(*command)
        existing = api.find_release(tag)
        if existing is None:
            raise ValueError("未查询到新建草稿；请检查远端后用 --resume 恢复")
    endpoint = f"releases/{existing['id']}"
    seen = api.verify_assets(existing, assets, stage)
    try:
        for name, path in assets.items():
            if name not in seen:
                print(f"上传：{name}", flush=True)
                api.release("upload", tag, str(path))
        uploaded = api.api(endpoint)
        api.verify_assets(uploaded, assets, stage, complete=True)
        # Stable Windows releases are the tray updater's public channel. Its client
        # requires GitHub's authenticated metadata digest before executing a ZIP.
        client_update = args.stable
        if client_update:
            name = f"vaultsyncd-{source['version']}-x64-unsigned.zip"
            package = next(item for item in uploaded["assets"] if item["name"] == name)
            if package.get("digest") != "sha256:" + sha(assets[name]):
                raise ValueError("正式客户端更新包缺少 GitHub SHA-256 digest；保留草稿，未设 Latest")
        api.tag(tag, commit)
        if not args.draft:
            api.release("edit", tag, "--draft=false", "--latest=true" if client_update else "--latest=false")
    except BaseException:
        print("上传或发布未完成；远端现场保留。用相同参数加 --resume 续传草稿。", file=sys.stderr)
        raise
    print(f"{'草稿已就绪' if args.draft else '发布完成'}：https://github.com/{api.repo}/releases/tag/{tag}")


def main(argv=None):
    parser = argparse.ArgumentParser(description="校验本地构建目录，仅上传 Windows x64 便携 ZIP 到 GitHub Release。")
    parser.add_argument("directory", type=Path, help="含 release-manifest.json 的已完成构建目录")
    parser.add_argument("--remote", default="origin", help="目标 Git 远端，默认 origin")
    parser.add_argument("--tag", help="默认 v<构建版本>；也接受 v<版本>-<预发布后缀>")
    parser.add_argument("--title")
    parser.add_argument("--notes-file", type=Path)
    parser.add_argument("--draft", action="store_true", help="附件传完后保持草稿")
    parser.add_argument("--stable", action="store_true", help="发布为正式版本并设为 Latest 供客户端自动更新，默认预发布")
    parser.add_argument("--resume", action="store_true", help="校验并续传同源草稿，不覆盖已有附件")
    parser.add_argument("--dry-run", action="store_true", help="仅本地校验和显示附件，不联网、不上传")
    parser.add_argument("--staging-root", type=Path, default=Path("/data/vaultsyncd-dev/vaultsyncd-temp/cache/github-release"))
    args = parser.parse_args(argv)
    manifest, originals, digests = bundle(args.directory.expanduser().resolve())
    repo = repository(args.remote)
    base_tag = "v" + manifest["source"]["version"]
    args.tag = args.tag or base_tag
    if not re.fullmatch(re.escape(base_tag) + r"(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?", args.tag):
        raise ValueError("标签必须与产物版本一致，例如 v0.2.0 或 v0.2.0-rc.1")
    if args.stable and args.tag != base_tag:
        raise ValueError("正式版标签不接受预发布后缀")
    if args.notes_file:
        args.notes_file = args.notes_file.expanduser().resolve(strict=True)
        args.notes_file.read_text()  # Fail before any remote mutation.
    plan = {"repository": repo, "tag": args.tag, "source": manifest["source"],
            "draft": args.draft, "prerelease": not args.stable,
            "latest": args.stable and not args.draft,
            "assets": sorted(originals),
            "local_only": sorted(({"release-manifest.json", "SHA256SUMS"} |
                                   {item["path"] for item in manifest["files"]}) -
                                  {str(path.relative_to(args.directory.expanduser().resolve())) for path in originals.values()})}
    print(json.dumps(plan, ensure_ascii=False, indent=2), flush=True)
    if args.dry_run:
        return
    if "#" in str(args.staging_root.resolve()):
        raise ValueError("暂存目录不能含 #（GitHub CLI 将其解释为附件标签）")
    args.staging_root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="upload-", dir=args.staging_root) as temporary:
        stage = Path(temporary)
        assets = {}
        for name, original in originals.items():
            path = stage / name
            shutil.copyfile(original, path)
            if sha(path) != digests[name]:
                raise ValueError("暂存复制校验失败")
            assets[name] = path
        publish(GitHub(repo), manifest, assets, stage, args)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, TypeError, OSError) as error:
        print(f"github-release: {error}", file=sys.stderr)
        sys.exit(1)
