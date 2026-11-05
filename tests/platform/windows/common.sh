# tests/platform/windows 下脚本的共享常量与函数（S009）。被 setup-toolchain.sh、
# build-cross.sh、run-on-guest.sh 以 `. common.sh` 引入，不单独执行。
# 操作说明与实测证据见 docs/operations/windows-lab.md §12。

# 交叉目标：与 rust-toolchain.toml 的 targets 项一致。首发只有 x64 MSVC：mingw-w64
# 14.0.0 不带 cldapi 导入库，CFAPI 无法链接（S009 实测）。
TARGET_TRIPLE="x86_64-pc-windows-msvc"
# design §5.8 指定的 Windows 原生测试包（不进 workspace default-members）。
TEST_PACKAGE="vaultsyncd-windows-tests"
# 本卡冒烟二进制：产物、哈希清单与客体执行共用这一处拼写。
SMOKE_BINARIES=("platform-smoke" "cfapi-link-smoke")
# 产物落在仓库内被 .gitignore 忽略的 artifacts/ 下，按目标三元组分目录。
ARTIFACT_SUBDIR="windows/${TARGET_TRIPLE}"

# 客体（S008 交付的 Windows 11 实验室）默认连接参数，可用环境变量覆盖。
GUEST_HOST="${GUEST_HOST:-127.0.0.1}"
GUEST_PORT="${GUEST_PORT:-2222}"
GUEST_USER="${GUEST_USER:-Quickemu}"
GUEST_SSH_KEY="${GUEST_SSH_KEY:-/data/vaultsyncd-lab/win-lab/ssh/vaultsyncd-lab_ed25519}"
# 客体侧工作目录：只在 C:\vaultsyncd-lab 之下，不复制 target/ 或 node_modules/。
GUEST_DIR="${GUEST_DIR:-C:/vaultsyncd-lab/s009}"

# 仓库根：脚本按工作树布局推算，避免把绝对路径写死进 Git。
repo_root() {
    git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel
}

# 宿主侧缓存目录，与 flake.nix `windows` devShell 的 shellHook 使用同一默认值。
windows_cache() {
    printf '%s' "${VAULTSYNCD_WINDOWS_CACHE:-$(repo_root)/.cache/windows}"
}

# 经 SSH 在客体执行一条命令；密钥独占认证，禁用交互提示。
guest_ssh() {
    ssh -p "$GUEST_PORT" -i "$GUEST_SSH_KEY" \
        -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes \
        -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST" "$@"
}

# 把文件复制到客体工作目录。
guest_scp() {
    scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" \
        -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes \
        -o LogLevel=ERROR "$@" "$GUEST_USER@$GUEST_HOST:$GUEST_DIR/"
}
