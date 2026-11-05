#!/usr/bin/env bash
# S008 Windows 实验室客体：宿主侧可复现引导脚本。
#
# 只放文本模板；ISO、qcow2、EFI/TPM 状态、凭据、日志全部落在源码之外的 VM 目录。
# 依赖 `nix develop .#quickemu` 提供的 quickemu/qemu/xorriso/wimlib/p7zip/openssh/curl。
#
# 用法（在仓库根执行）：
#   nix develop .#quickemu --command tools/windows-lab/bootstrap-host.sh fetch-virtio
#   nix develop .#quickemu --command tools/windows-lab/bootstrap-host.sh fetch-iso
#   nix develop .#quickemu --command tools/windows-lab/bootstrap-host.sh prepare
#   nix develop .#quickemu --command tools/windows-lab/bootstrap-host.sh start
#   nix develop .#quickemu --command tools/windows-lab/bootstrap-host.sh wait-ssh
#   nix develop .#quickemu --command tools/windows-lab/bootstrap-host.sh status
#   nix develop .#quickemu --command tools/windows-lab/bootstrap-host.sh stop
#   nix develop .#quickemu --command tools/windows-lab/bootstrap-host.sh facts
#
# VM_ROOT 可用环境变量覆盖，默认 /data/vaultsyncd-lab/win-lab。

set -euo pipefail

SCRIPT_DIR="$(dirname "$(readlink -f "$0")")"
VM_ROOT="${VM_ROOT:-/data/vaultsyncd-lab/win-lab}"
VM_NAME="${VM_NAME:-windows-11}"
LAB_USER="${LAB_USER:-Quickemu}"
SSH_PORT="${SSH_PORT:-2222}"
SPICE_PORT="${SPICE_PORT:-5930}"

# 微软 Evaluation Center 的 Windows 11 Enterprise 评估版（官方未修改镜像）。
# 消费者 ISO 端点会按来源 IP 拦截，见 docs/environment.md §12。
ISO_URL="${ISO_URL:-https://go.microsoft.com/fwlink/?linkid=2382600}"
ISO_FILE="${VM_ROOT}/${VM_NAME}/windows-11.iso"
ISO_SHA256_FILE="${VM_ROOT}/${VM_NAME}/windows-11.iso.sha256"

# virtio-win 官方稳定版驱动 ISO（quickemu 自带的下载链路返回反爬 HTML，需替换）。
VIRTIO_URL="${VIRTIO_URL:-https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/stable-virtio/virtio-win.iso}"
VIRTIO_FILE="${VM_ROOT}/${VM_NAME}/virtio-win.iso"

UNATTENDED_DIR="${VM_ROOT}/${VM_NAME}/unattended"
CREDENTIAL_FILE="${VM_ROOT}/secrets/lab-credentials.txt"
SSH_KEY="${VM_ROOT}/ssh/vaultsyncd-lab_ed25519"
KNOWN_HOSTS="${VM_ROOT}/ssh/known_hosts"
LOG_DIR="${VM_ROOT}/logs"
CONF="${VM_ROOT}/${VM_NAME}.conf"
PID_FILE="${VM_ROOT}/${VM_NAME}/${VM_NAME}.pid"

log() { printf '%s %s\n' "$(date -u +%FT%TZ)" "$*"; }
die() { log "ERROR: $*"; exit 1; }

lab_ssh() {
    ssh -p "${SSH_PORT}" -i "${SSH_KEY}" \
        -o BatchMode=yes -o IdentitiesOnly=yes -o ConnectTimeout=8 \
        -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile="${KNOWN_HOSTS}" \
        "${LAB_USER}@127.0.0.1" "$@"
}

# ISO 9660 的主卷描述符固定在第 16 扇区（偏移 32768）：1 字节类型 + "CD001"。
# 直接比这 5 个字节，避免 `head | grep -q` 在 pipefail 下因 SIGPIPE 误判。
is_iso9660() { [ "$(dd if="$1" bs=1 skip=32769 count=5 2>/dev/null)" = "CD001" ]; }

cmd_fetch_virtio() {
    mkdir -p "${VM_ROOT}/downloads"
    local tmp="${VM_ROOT}/downloads/virtio-win.iso"
    if [ -f "${tmp}" ] && is_iso9660 "${tmp}"; then
        log "virtio-win ISO 已在 ${tmp}，跳过下载"
    else
        log "下载 virtio-win 驱动 ISO：${VIRTIO_URL}"
        curl -fL --retry 5 --retry-delay 5 --continue-at - -o "${tmp}" "${VIRTIO_URL}"
    fi
    is_iso9660 "${tmp}" || die "${tmp} 不是 ISO 9660（可能是反爬 HTML 页）"
    cp -f "${tmp}" "${VIRTIO_FILE}"
    log "virtio-win ISO 就位：${VIRTIO_FILE} ($(stat -c %s "${VIRTIO_FILE}") 字节)"
}

cmd_fetch_iso() {
    mkdir -p "${LOG_DIR}"
    log "HEAD 校验 ${ISO_URL}"
    curl -sIL --max-time 60 "${ISO_URL}" > "${LOG_DIR}/eval-iso-head.log"
    head -n 5 "${LOG_DIR}/eval-iso-head.log"

    # 重定向链上最后一段 Content-Length 才是评估版 ISO 的真实字节数（第一段是 302 的 0）。
    local expected_size
    expected_size="$(grep -i '^content-length:' "${LOG_DIR}/eval-iso-head.log" | tr -d '\r' | tail -n1 | awk '{print $2}')"
    [ -n "${expected_size}" ] || die "HEAD 输出里没有 Content-Length，妥善拒绝"
    log "远端大小：${expected_size} 字节"

    if [ -f "${ISO_FILE}" ] && [ "$(stat -c %s "${ISO_FILE}")" = "${expected_size}" ]; then
        log "本地 ISO 字节数已与远端一致，跳过下载"
    else
        log "续传下载 → ${ISO_FILE}"
        curl -fL --retry 5 --retry-delay 5 --continue-at - \
            -o "${ISO_FILE}" "${ISO_URL}" 2>> "${LOG_DIR}/eval-iso-download.log"
    fi
    [ "$(stat -c %s "${ISO_FILE}")" = "${expected_size}" ] \
        || die "ISO 字节数 $(stat -c %s "${ISO_FILE}") != 远端 ${expected_size}"

    local actual_sha
    actual_sha="$(sha256sum "${ISO_FILE}" | cut -d' ' -f1)"
    if [ -f "${ISO_SHA256_FILE}" ] && [ "$(cut -d' ' -f1 < "${ISO_SHA256_FILE}")" != "${actual_sha}" ]; then
        die "ISO sha256 与已记录值不符：$(cut -d' ' -f1 < "${ISO_SHA256_FILE}") != ${actual_sha}"
    fi
    printf '%s  %s\n' "${actual_sha}" "$(basename "${ISO_FILE}")" > "${ISO_SHA256_FILE}"
    log "ISO sha256: ${actual_sha}"
    log "ISO bytes:  $(stat -c %s "${ISO_FILE}")"
}

cmd_prepare() {
    [ -f "${ISO_FILE}" ] || die "缺 ${ISO_FILE}，先运行 fetch-iso"
    is_iso9660 "${ISO_FILE}" || die "${ISO_FILE} 不是 ISO 9660"
    is_iso9660 "${VIRTIO_FILE}" || die "缺可用 ${VIRTIO_FILE}，先运行 fetch-virtio"

    mkdir -p "${VM_ROOT}/secrets" "${VM_ROOT}/ssh" "${LOG_DIR}"
    chmod 700 "${VM_ROOT}/secrets" "${VM_ROOT}/ssh"

    if [ ! -f "${CREDENTIAL_FILE}" ]; then
        local password
        password="$(dd if=/dev/urandom bs=1024 count=1 2>/dev/null | tr -dc 'A-Za-z0-9')"
        password="${password:0:32}"
        umask 077
        printf 'user=%s\npassword=%s\n' "${LAB_USER}" "${password}" > "${CREDENTIAL_FILE}"
        log "已生成随机密码 → ${CREDENTIAL_FILE} (0600)"
    fi
    chmod 600 "${CREDENTIAL_FILE}"
    local password
    password="$(grep -m1 '^password=' "${CREDENTIAL_FILE}" | cut -d= -f2-)"
    [ -n "${password}" ] || die "${CREDENTIAL_FILE} 缺少 password="

    if [ ! -f "${SSH_KEY}" ]; then
        ssh-keygen -q -t ed25519 -N '' -C 'vaultsyncd-lab' -f "${SSH_KEY}"
        log "已生成自动化密钥 → ${SSH_KEY}"
    fi
    chmod 600 "${SSH_KEY}"
    chmod 644 "${SSH_KEY}.pub"

    mkdir -p "${UNATTENDED_DIR}"
    chmod 700 "${UNATTENDED_DIR}"
    cp -f "${SSH_KEY}.pub" "${UNATTENDED_DIR}/ssh_authorized_key.pub"
    cp -f "${SCRIPT_DIR}/provision.ps1" "${UNATTENDED_DIR}/provision.ps1"
    sed "s|__LAB_PASSWORD__|${password}|g" \
        "${SCRIPT_DIR}/autounattend.xml.template" > "${UNATTENDED_DIR}/autounattend.xml"
    chmod 600 "${UNATTENDED_DIR}/autounattend.xml"

    if grep -q 'W269N-WFGWX-YVC9B-4J6C9-T83GX' "${UNATTENDED_DIR}/autounattend.xml"; then
        die "autounattend.xml 仍含 Pro KMS 密钥，评估版会安装失败"
    fi
    if grep -q '__LAB_PASSWORD__' "${UNATTENDED_DIR}/autounattend.xml"; then
        die "autounattend.xml 仍有未替换的密码占位符"
    fi

    # 重打 unattended.iso（quickget 的原命令是 mkisofs -quiet -J；这里用 xorriso 的兼容模式）。
    xorriso -as mkisofs -quiet -J -o "${VM_ROOT}/${VM_NAME}/unattended.iso" "${UNATTENDED_DIR}/"
    log "unattended.iso 已重打：$(stat -c %s "${VM_ROOT}/${VM_NAME}/unattended.iso") 字节"

    sed -e "s|@DISK_SIZE@|100G|" -e "s|@CPU_CORES@|4|" -e "s|@RAM@|8G|" \
        -e "s|@SSH_PORT@|${SSH_PORT}|" "${SCRIPT_DIR}/windows-11.conf.template" > "${CONF}"
    log "配置写入 ${CONF}"
    cat "${CONF}"
}

cmd_start() {
    [ -f "${CONF}" ] || die "缺 ${CONF}，先运行 prepare"
    local listening
    listening=$'\n'$(ss -ltn | awk '{print $4}')$'\n'
    case "${listening}" in
        *$'\n'"127.0.0.1:${SPICE_PORT}"$'\n'*) die "SPICE 端口 ${SPICE_PORT} 已被占用" ;;
    esac
    case "${listening}" in
        *$'\n'*":${SSH_PORT}"$'\n'*) die "宿主端口 ${SSH_PORT} 已被占用" ;;
    esac
    log "启动客体（display=none + SPICE TCP 127.0.0.1:${SPICE_PORT}，无本地 viewer）"
    cd "${VM_ROOT}"
    setsid nohup quickemu --vm "${CONF}" \
        --display none --viewer none --access local --spice-port "${SPICE_PORT}" \
        --public-dir none \
        >> "${LOG_DIR}/quickemu-start.log" 2>&1 &
    sleep 5
    cat "${LOG_DIR}/quickemu-start.log"
    cmd_status
}

cmd_status() {
    if [ -r "${PID_FILE}" ]; then
        local pid; pid="$(head -n1 "${PID_FILE}")"
        if kill -0 "${pid}" 2>/dev/null; then
            log "qemu 运行中 pid=${pid}"
        else
            log "pid 文件残留但进程不存在：${PID_FILE}"
        fi
    else
        log "客体未运行（无 ${PID_FILE}）"
    fi
    ss -ltn | grep -E ":(${SSH_PORT}|${SPICE_PORT})\b" || log "端口 ${SSH_PORT}/${SPICE_PORT} 未监听"
    tail -n 5 "${VM_ROOT}/${VM_NAME}/${VM_NAME}.log" 2>/dev/null || true
}

cmd_wait_ssh() {
    local deadline=$(( SECONDS + ${WAIT_SSH_SECONDS:-2700} ))
    log "等待 SSH 可用（最长 $(( deadline - SECONDS ))s，端口 ${SSH_PORT}）"
    while [ "${SECONDS}" -lt "${deadline}" ]; do
        if lab_ssh 'ver' 2>/dev/null; then
            log "SSH 通道可用"
            return 0
        fi
        sleep 15
    done
    die "SSH 在等待期内未可用；查看 ${LOG_DIR}、${VM_ROOT}/${VM_NAME}/${VM_NAME}.log"
}

cmd_facts() {
    lab_ssh 'ver'
    lab_ssh 'powershell -NoProfile -Command "(Get-ComputerInfo).WindowsProductName; (Get-ComputerInfo).WindowsVersion; (Get-ComputerInfo).OsBuildNumber"'
    lab_ssh 'powershell -NoProfile -Command "Get-Volume C | Format-List FileSystemType,DriveType,Size,SizeRemaining"'
    lab_ssh 'fsutil fsinfo volumeinfo C:'
}

cmd_stop() {
    log "客体内软关机"
    lab_ssh 'shutdown /s /t 0' || log "shutdown 命令返回非零（可能已断开）"
    local deadline=$(( SECONDS + ${STOP_SECONDS:-180} ))
    while [ "${SECONDS}" -lt "${deadline}" ]; do
        if [ ! -r "${PID_FILE}" ] || ! kill -0 "$(head -n1 "${PID_FILE}")" 2>/dev/null; then
            log "qemu 已退出，SPICE/SSH 转发随之关闭"
            return 0
        fi
        sleep 5
    done
    die "qemu 在 ${STOP_SECONDS:-180}s 内未退出；不要 kill -9，先检查 ${VM_ROOT}/${VM_NAME}/${VM_NAME}.log"
}

usage() {
    grep -E '^#   nix develop' "$0" | sed 's/^#   /  /'
}

case "${1:-}" in
    fetch-virtio) cmd_fetch_virtio ;;
    fetch-iso)    cmd_fetch_iso ;;
    prepare)      cmd_prepare ;;
    start)        cmd_start ;;
    wait-ssh)     cmd_wait_ssh ;;
    status)       cmd_status ;;
    facts)        cmd_facts ;;
    stop)         cmd_stop ;;
    "")           usage; exit 2 ;;
    *)            die "未知子命令：$1" ;;
esac
