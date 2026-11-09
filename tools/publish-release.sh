#!/usr/bin/env bash
# 上传 tools/release.sh 生成的归档；可从任意目录调用。
set -euo pipefail
vault=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
exec nix develop "$vault" --command python3 "$vault/packaging/github-release.py" "$@"
