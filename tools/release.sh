#!/usr/bin/env bash
# 在锁定的开发环境中运行统一发布入口，可从任意目录调用。
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
exec nix develop . --command python3 packaging/release.py "$@"
