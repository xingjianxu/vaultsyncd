#!/usr/bin/env python3
"""Single command catalogue for GitHub jobs and local `cargo xtask ci` runs.

Missing lab inputs exit 78 (blocked), never a successful/empty test run.
Only explicitly provisioned, disposable runners may execute privileged lab jobs.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
CARGO = ["cargo", "xtask"]
# Conservative consumer closure: any implementation/shared input runs the whole matrix.
# Kept identical in platform.yml; test_ci.py guards additions and consumer examples.
PATHS = [".github/**", ".cargo/**", "api/**", "apps/**", "crates/**", "packages/**",
         "tests/**", "fixtures/**", "tools/**", "packaging/**", "deploy/**", "nix/**",
         "Cargo.*", "package.json", "pnpm-*.yaml", "rust-toolchain.toml", "flake.*"]
JOBS = {
    "portable": [CARGO + ["check"]],
    "contracts": [CARGO + ["generate", "--check"], CARGO + ["version", "--check"],
                  CARGO + ["check-boundaries"]],
    "frontend": [["pnpm", "--recursive", "run", "typecheck"],
                 ["pnpm", "--recursive", "--if-present", "run", "test"],
                 ["pnpm", "--recursive", "--if-present", "run", "build"]],
    "linux-server": [["cargo", "test", "--locked", "-p", "vaultsyncd-platform-linux-server",
                      "-p", "vaultsyncd-server-store", "-p", "vaultsyncd-server-core",
                      "-p", "vaultsyncd-server", "-p", "vaultsyncd-admin",
                      "-p", "vaultsyncd-server-tests"]],
    "windows-build": [["pnpm", "--filter", "@vaultsyncd/desktop", "build"],
                      ["cargo", "build", "--locked", "--release", "--target", "x86_64-pc-windows-msvc",
                       "-p", "vaultsyncd-agent", "-p", "vaultsyncd-cli", "-p", "vaultsyncd-desktop", "--bins"],
                      ["cargo", "test", "--locked", "--target", "x86_64-pc-windows-msvc",
                       "-p", "vaultsyncd-windows-tests", "--no-run"]],
    "release-linux": [["nix", "build", ".#default", ".#portable", ".#container-deps", "--no-link", "--json"]],
    "release-arm64": [["nix", "build", ".#portable-aarch64", ".#container-deps-aarch64", "--no-link", "--json"]],
    "release-windows": [CARGO + ["release", "build", "windows-client", "{component}"]],
    "windows-e2e": [["bash", "tests/platform/windows/run-native-e2e.sh"]],
    "windows-cfapi": [["bash", "tests/platform/windows/run-native-release.sh"]],
    "windows-install": [["python3", "tests/platform/windows/s099-installation.py", "{component}"]],
    "ubuntu-install": [["python3", "packaging/linux/test-container.py", "{image}", "amd64", "{evidence}"]],
    "arm64-install": [["python3", "packaging/linux/test-arm64-vm.py", "{vm}", "{image}", "{evidence}"]],
    "nixos-install": [["python3", "packaging/linux/test-nix-runtime.py", "{package}"]],
}
LAB = {"windows-e2e", "windows-cfapi", "windows-install", "ubuntu-install", "arm64-install", "nixos-install"}


def plan(job, values):
    return [[arg.format_map(values) for arg in command] for command in JOBS[job]]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("job", choices=sorted(JOBS))
    parser.add_argument("--plan", action="store_true", help="print commands; not execution evidence")
    for name in ["component", "image", "evidence", "vm", "package"]:
        parser.add_argument("--" + name)
    args = parser.parse_args()
    values = {k: v for k, v in vars(args).items() if v is not None}
    try:
        commands = plan(args.job, values)
    except KeyError as error:
        print(json.dumps({"status": "blocked", "reason": "missing --" + error.args[0]}))
        return 78
    if args.plan:
        print(json.dumps({"execution": False, "commands": commands}, indent=2))
        return 0
    report = {"job": args.job, "status": "failed", "platform": platform.platform(),
              "machine": platform.machine(), "steps": [], "commands": commands}
    report["commit"] = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    report["porcelain"] = subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True)
    if report["porcelain"]:
        print(json.dumps({**report, "reason": "clean source commit required"}))
        return 1
    if args.job in LAB and os.environ.get("VAULTSYNCD_CI_LAB") != "1":
        print(json.dumps({**report, "status": "blocked", "reason": "set VAULTSYNCD_CI_LAB=1 only in the reserved disposable lab"}))
        return 78
    if args.job == "windows-build" and sys.platform != "win32":
        print(json.dumps({**report, "status": "blocked", "reason": "native Windows build requires a Windows runner; use release-windows for the locked Linux cross adapter"}))
        return 78
    directory = ROOT / "artifacts/S101/ci" / (args.job + "-" + str(time.time_ns()))
    directory.mkdir(parents=True)
    code = 0
    try:
        for index, command in enumerate(commands):
            print("CI_COMMAND " + json.dumps(command), flush=True)
            start = time.monotonic()
            with (directory / f"{index}.log").open("w") as log:
                process = subprocess.run(command, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, timeout=3600)
            report["steps"].append({"exit": process.returncode, "seconds": round(time.monotonic() - start, 3)})
            print((directory / f"{index}.log").read_text(), end="", flush=True)
            if process.returncode:
                code = process.returncode
                break
        report["status"] = "passed" if code == 0 else "failed"
    except (OSError, subprocess.TimeoutExpired) as error:
        report["error"] = str(error)
        code = 1
    finally:
        report["porcelain_after"] = subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True)
        if report["porcelain_after"]:
            report["status"] = "failed"
            code = 1
        (directory / "result.json").write_text(json.dumps(report, ensure_ascii=False, indent=2))
        print("CI_RESULT " + json.dumps({"status": report["status"], "report": str(directory / "result.json")}))
    return code


if __name__ == "__main__":
    sys.exit(main())
