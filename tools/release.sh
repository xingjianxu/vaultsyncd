#!/usr/bin/env bash
# 在锁定的开发环境中构建 Linux x64 / Windows x64，可从任意目录调用。
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
exec nix develop . --command python3 packaging/release.py "$@"
