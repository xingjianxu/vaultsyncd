#!/usr/bin/env bash
# 固定已提交版本，在锁定环境内纯构建并部署到本机。
set -euo pipefail
vault=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$vault"
exec nix develop . --option pure-eval true --no-update-lock-file --no-write-lock-file \
  --command python3 deploy/nixos/deploy-local.py "$@"
