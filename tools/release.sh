#!/usr/bin/env bash
# 在锁定的开发环境中构建 Windows x64 便携 ZIP，可从任意目录调用。
set -euo pipefail
vault=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$vault"
exec nix develop . --command python3 packaging/release.py "$@"
