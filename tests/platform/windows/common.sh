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

# S010：CFAPI 原型包与 example 二进制。独立于上面的模拟冒烟入口，S009 的用法保持不变。
PLATFORM_PACKAGE="vaultsyncd-platform-windows"
CFAPI_PROTOTYPE_BINARY="cfapi-prototype"
# 原型在客体上的工作目录与专用同步根（客体本地 NTFS，不用共享文件夹）。
CFAPI_GUEST_DIR="${CFAPI_GUEST_DIR:-C:/vaultsyncd-lab/s010}"
CFAPI_VAULT="${CFAPI_VAULT:-C:/vaultsyncd-test}"

# S011：水合验证用独立的客体工作目录与范围参数（与 S010 的原型脚本互不影响）。
CFAPI_HYDRATION_GUEST_DIR="${CFAPI_HYDRATION_GUEST_DIR:-C:/vaultsyncd-lab/s011}"
# 范围水合的条目与区间：16 MiB 条目上取 1 MiB，足以让平台按范围（而非整份）请求。
CFAPI_HYDRATION_RANGE_ENTRY="${CFAPI_HYDRATION_RANGE_ENTRY:-huge-16mib.bin}"
CFAPI_HYDRATION_RANGE_OFFSET="${CFAPI_HYDRATION_RANGE_OFFSET:-4000000}"
CFAPI_HYDRATION_RANGE_LENGTH="${CFAPI_HYDRATION_RANGE_LENGTH:-1048576}"

# S012：释放/写保护验证用独立的客体工作目录、主同步根与 rename 专用同步根。rename-over 会
# 把目标名字留在无法按路径打开的状态，因此单独用一个根，避免污染主根的后续检查。
CFAPI_RELEASE_GUEST_DIR="${CFAPI_RELEASE_GUEST_DIR:-C:/vaultsyncd-lab/s012}"
CFAPI_RELEASE_VAULT="${CFAPI_RELEASE_VAULT:-C:/vaultsyncd-test}"
CFAPI_RELEASE_RENAME_VAULT="${CFAPI_RELEASE_RENAME_VAULT:-C:/vaultsyncd-test-rename}"

# 客体（S008 交付的 Windows 11 实验室）默认连接参数，可用环境变量覆盖。
GUEST_HOST="${GUEST_HOST:-127.0.0.1}"
GUEST_PORT="${GUEST_PORT:-2222}"
GUEST_USER="${GUEST_USER:-Quickemu}"
GUEST_SSH_KEY="${GUEST_SSH_KEY:-/data/vaultsyncd-dev/vaultsyncd-lab/win-lab/ssh/vaultsyncd-lab_ed25519}"
# 客体侧工作目录：只在 C:\vaultsyncd-lab 之下，不复制 target/ 或 node_modules/。
GUEST_DIR="${GUEST_DIR:-C:/vaultsyncd-lab/s009}"

# 仓库根：脚本按工作树布局推算，避免把绝对路径写死进 Git。
repo_root() {
    git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel
}

# 宿主侧缓存目录，与 flake.nix `windows` devShell 的 shellHook 使用同一默认值。
windows_cache() {
    printf '%s' "${VAULTSYNCD_WINDOWS_CACHE:-/data/vaultsyncd-dev/vaultsyncd-temp/cache/windows}"
}

# 经 SSH 在客体执行一条命令；密钥独占认证，禁用交互提示。
guest_ssh() {
    ssh -p "$GUEST_PORT" -i "$GUEST_SSH_KEY" \
        -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes \
        -o LogLevel=ERROR "$GUEST_USER@$GUEST_HOST" "$@"
}

# 把文件复制到客体工作目录。目标目录默认 GUEST_DIR；S010 的原型脚本用
# GUEST_TARGET_DIR 指向自己的目录，从而不改动 S009 的默认路径。
guest_scp() {
    local target_dir="${GUEST_TARGET_DIR:-$GUEST_DIR}"
    scp -P "$GUEST_PORT" -i "$GUEST_SSH_KEY" \
        -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o BatchMode=yes \
        -o LogLevel=ERROR "$@" "$GUEST_USER@$GUEST_HOST:$target_dir/"
}
