# Windows 11 实验室客体操作说明

本文件是 Windows 原生验收环境的唯一操作入口，来源事实见 `docs/environment.md` §12，模板与引导脚本在 `tools/windows-lab/`。平台与交付边界以 `docs/design.md` §2.1 为准，构建分工见 design ADR-013 与实施手册 §2.6。

## 1. 目的与边界

- 客体只承担 **Windows 原生运行、调试、安装/升级、CFAPI 与 GUI 验收**。客户端与 Windows 测试程序在 NixOS 宿主交叉编译，前端在宿主构建，产物传输进客体执行；**客体不默认安装 Rust、Visual Studio/C++ Build Tools、Windows SDK、Node/pnpm**。理由与出处：design ADR-013（2026-11-05 用户决定）、实施手册 §2.6。宿主侧交叉工具链由 S009 落实，本卡不验证。
- 本文件描述的是**一台**可重复启动的实体客体（`windows-11`）。第二台并发客体在 S107 从同一基线独立初始化 DeviceId，不在此处复制。
- 不使用共享文件夹当 NTFS 测试根：CFAPI 根必须落在客体本地 NTFS（`C:\vaultsyncd-test` 等）。`--public-dir none` 已关闭 quickemu 的宿主目录共享，测试数据一律走客体本地磁盘与 `C:\vaultsyncd-lab\`。
- Wine、交叉编译成功、Linux fake 都不能替代客体上的原生结果。

## 2. 宿主前置

| 项目 | 要求 / 实测值 |
| --- | --- |
| 架构与虚拟化 | x86_64，`/dev/kvm` 可读写（`crw-rw-rw- root kvm`），用户在 `kvm` 组；`systemd-detect-virt` = `none`（物理机） |
| 宿主容量 | 128 逻辑核、503 GiB 内存；`/data` 为本地 btrfs，可用 22 TiB（VM 目录在 `/data` 上，不在源码树内） |
| 工具链 | `nix develop .#quickemu`（flake 输出，锁定 nixpkgs rev 与默认 devShell 相同）：quickemu 4.9.9 / QEMU 11.1.2 / xorriso 1.5.8.pl02 / wimlib-imagex 1.14.5 / p7zip 17.06 / openssh 10.6p1 / curl 8.22.0 |
| 版本锁定 | 版本来自 `flake.lock` 固定的 nixpkgs，不装全局包、不跟随最新；换版本必须先改 flake 输入 |
| 宿主不改动项 | 不改 `/etc/nixos`、不执行 `nixos-rebuild`、不改宿主防火墙与宿主 SSH、不格式化任何块设备、不触碰 `/data/llm` |

## 3. 目录布局（全部在源码之外）

VM 根：`/data/vaultsyncd-lab/win-lab/`（可在 `VM_ROOT` 环境变量覆盖）。`/data/vaultsyncd-lab` 同时是 S007 的测试卷 lab 根，二者用子目录隔离。

| 路径 | 内容 | 权限 |
| --- | --- | --- |
| `windows-11.conf` | quickemu 配置（由模板渲染） | 644 |
| `windows-11/windows-11.iso` | Windows 11 Enterprise 评估版 ISO | 644 |
| `windows-11/windows-11.iso.sha256` | ISO 校验记录 | 644 |
| `windows-11/virtio-win.iso` | virtio-win 官方驱动 ISO（客体 E:） | 644 |
| `windows-11/disk.qcow2` | 系统盘（100 GiB 虚拟） | 644 |
| `windows-11/OVMF_VARS.fd` | EFI 变量（NVRAM） | 644 |
| `windows-11/tpm2-00.permall` + `windows-11.swtpm-sock` | swtpm 2.0 状态与套接字 | 640 / socket |
| `windows-11/unattended/` | patched `autounattend.xml`、`provision.ps1`、自动化公钥、SPICE msi | 700（含明文口令） |
| `windows-11/unattended.iso` | 重打后的自动安装盘（客体 F:） | 644 |
| `secrets/lab-credentials.txt` | 实验室账号口令（**只有 0600**，不进 Git/文档） | 600 |
| `ssh/vaultsyncd-lab_ed25519[.pub]` | 自动化密钥对（私钥只在 VM 目录） | 600 / 644 |
| `ssh/known_hosts` | 客体 host key 记录 | 644 |
| `logs/` | 下载、启动、`quickemu --vm` 输出日志 | 700 |
| `downloads/` | ISO 落地中间文件（可与 `windows-11/` 去重后清理） | 700 |

这些路径都在 Git 仓库之外；仓库内只有 `tools/windows-lab/` 的文本模板。

## 4. ISO 来源与校验

### 4.1 Windows 镜像

| 项目 | 值 |
| --- | --- |
| 来源 | 微软 Evaluation Center 的 Windows 11 Enterprise 评估版（官方未修改镜像），`https://go.microsoft.com/fwlink/?linkid=2382600` |
| 实际落地 URL | `https://software-static.download.prss.microsoft.com/dbazure/26300.9457.260913-1737.26h2_ge_release_svc_refresh_CLIENTENTERPRISEEVAL_OEMRET_x64FRE_en-us.iso`（302 目标，`Accept-Ranges: bytes`） |
| 版本 | Windows 11 Enterprise Evaluation，26H2 分支 build 26300，x64，en-US，卷标 `CENA_X64FREE_EN-US_DV9`，UDF 文件系统（`install.wim` > 4 GiB） |
| 大小与校验 | 8,225,329,152 字节；sha256 `bc3f24086ebadc94489066b5ad78089e2cf5c3491e90e790bb81a2b199c10e38`（下载时间 2026-11-05T09:02—09:16Z，宿主侧 `sha256sum`） |
| `install.wim` 映象 | `wimlib-imagex info` 只有一个 index：`Windows 11 Enterprise Evaluation`，x86_64，默认语言 en-US |

为什么不用消费者版：`quickget windows 11` 从 `https://www.microsoft.com/en-us/software-download/windows11` 取镜像时本机被按 IP 拦截（`WARNING! Microsoft blocked the automated download request based on your IP address.`，实测 2026-11-05）。评估版是同一厂商的官方镜像，90 天评估期、**不是产品授权**；不得改用第三方镜像、UUP dump 或修改版 ISO。

`install.wim` 只有单个映象，因此 `autounattend.xml` 的 `ImageInstall` 不需要 `ImageSelect`；安装程序直接使用唯一映像。

### 4.2 virtio-win 驱动 ISO

| 项目 | 值 |
| --- | --- |
| 来源 | `https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/stable-virtio/virtio-win.iso`（virtio-win 官方稳定版 0.1.302） |
| 大小与校验 | 877,373,440 字节；sha256 `303f7ae40dad495d6ae474fdc571df58958a4dbc5c37a522d80f9a203867949d` |
| 为什么需要 | quickemu 的 `guest_os="windows"` 使用 `virtio-blk-pci` 磁盘与 `virtio-net-pci` 网卡；没有真实驱动时安装阶段看不到磁盘、装完也没有网络 |
| 已知陷阱 | quickemu 4.9.9 自带的 `virtio-win.iso` 下载链路返回 4,466 字节的反爬 HTML（`file` 报 `HTML document`），会静默破坏安装。`bootstrap-host.sh fetch-virtio` 用 ISO 9660 魔数（偏移 32768 的 `CD001`）判断并在 `prepare` 中再次校验 |

驱动 ISO 挂为客体 E:，`autounattend.xml` 的 `DriverPaths` 使用 `E:\viostor\w10\amd64` 等路径；`unattended.iso` 挂为 F:。

## 5. unattended 改动点（相对于 `quickget windows 11` 生成的版本）

模板：`tools/windows-lab/autounattend.xml.template`（`__LAB_PASSWORD__` 由 `prepare` 用 VM 目录里的随机口令替换）。

1. **删除两处 Windows 11 Pro KMS 通用密钥** `W269N-WFGWX-YVC9B-4J6C9-T83GX`：`windowsPE` 的 `UserData/ProductKey` 与 `specialize` 的 `Shell-Setup/ProductKey`。评估版 ISO 没有 Pro 版，也不需要密钥；保留会导致“版本/密钥不匹配”安装失败。`prepare` 会做负控：残留该密钥或残留占位符即失败。
2. **账号**：保留 quickemu 的 `Quickemu`（Administrators）本地账号与 AutoLogon，但口令换成 32 位随机字母数字，只存在于 `secrets/lab-credentials.txt`（0600）与 0700 目录内的 `autounattend.xml`。获取方式：`sudo cat /data/vaultsyncd-lab/win-lab/secrets/lab-credentials.txt`（文件属主即运行 quickemu 的用户）。
3. **`DynamicUpdate` 关闭**（`<Enable>false</Enable>`）：安装期不抓 Windows Update，安装时长与结果可复现；更新由后续手动决定。
4. **FirstLogonCommands 收敛为一条**：`powershell.exe -NoProfile -ExecutionPolicy Bypass -File F:\provision.ps1`，其余动作（qemu-ga、SPICE msi、viogpudo、电源、SSH、WinRM）都在 `provision.ps1` 内分步执行并记录，单步失败不会中断整条链路。
5. 保留 quickemu 的 `EnableLUA=false`（关闭 UAC）与 `LabConfig` 的 CPU/RAM/TPM/SecureBoot 检查绕过。这是上游模板的实验室取向，**与生产 Windows 行为不同**，涉及 UAC/提权的结论必须在 S010—S012 明确记录；`secureboot="off"` 同理。

重打自动安装盘（`prepare` 内部执行，与 quickget 的 `mkisofs -quiet -J` 等价）：

```bash
xorriso -as mkisofs -quiet -J -o "$VM_ROOT/windows-11/unattended.iso" "$VM_ROOT/windows-11/unattended/"
```

改 `autounattend.xml`/`provision.ps1` 后**必须**重打 `unattended.iso`，否则客体仍读到旧盘。

**介质只在首次安装期间存在**：quickemu 4.9.9 在“磁盘看起来已使用”时会清空 `iso`（源码注释：*If there is a disk image that appears to be used do not boot from installation media*），而 `unattended.iso` 的挂载条件是 `iso` 非空。因此安装完成后再开机，`unattended.iso` 与安装 ISO 都不再插入（实测 `info block` 为 `ide2-cd0: [not inserted]`，客体 `F:` = `MediaLoaded: False`）。`provision.ps1` 因此按**内容**在 `DriveType=5` 的盘里查找 `ssh_authorized_key.pub` 与 `guest-agent\qemu-ga-x86_64.msi`，找不到就打 `SKIP` 并继续，不会因盘符变化或介质缺失而中断。

## 6. provision.ps1 做了什么

日志：`C:\vaultsyncd-lab\provision.log`（`Start-Transcript`，带 UTC 时间戳的步骤行与失败原因）。

| 步骤 | 内容 |
| --- | --- |
| 事实记录 | OS Caption/Version/Build/Arch、计算机名、时区、Culture/UI culture/WinSystemLocale、活动代码页（按实测记录，不猜测） |
| 时区 | 设为 `China Standard Time`（与开发宿主一致，便于日志对齐） |
| 长路径 | `HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\LongPathsEnabled=1` |
| 电源 | `powercfg /hibernate off`、待机/休眠超时 0、`HiberbootEnabled=0`（关快速启动）；不关 Defender、不关防火墙整体 |
| Delivery Optimization | `DODownloadMode=0`（只走 HTTP，不做 P2P 上传/局域网发现），实验室收敛项 |
| OpenSSH Server | 先 `Add-WindowsCapability -Online -Name OpenSSH.Server*`；若仍无 `sshd.exe` 则回退官方 `PowerShell/Win32-OpenSSH` release MSI；随后 `sshd` 服务设为 Automatic 并启动 |
| SSH 认证收敛 | 改写 `%ProgramData%\ssh\sshd_config`：`PubkeyAuthentication yes`、`PasswordAuthentication no`、`KbdInteractiveAuthentication no`（插入位置在 `Match Group administrators` 之前），然后重启 `sshd` |
| 自动化公钥 | `F:\ssh_authorized_key.pub` → `%ProgramData%\ssh\administrators_authorized_keys`，`icacls /inheritance:r /grant *S-1-5-32-544:F *S-1-5-18:F`（用 SID 避免本地化名差异） |
| 防火墙 | 把入站 TCP/22 的 `OpenSSH-Server-In-TCP` 规则设为 **任意 profile**（`-Profile Any`）；Windows 把 slirp 网卡判为 `Public`，而能力自带的规则只覆盖 `Private`，不改就会静默丢 SYN。不动其他规则、不关防火墙 |
| WinRM | `Enable-PSRemoting -SkipNetworkProfileCheck -Force`，HTTP 5985，启动类型 Automatic（备选通道） |
| VirtIO 附加组件 | `E:\guest-agent\qemu-ga-x86_64.msi`、`E:\viogpudo\...\viogpudo.inf`、F: 上两个 SPICE msi；单步失败只记录 |
| 收尾 | 再次把 `Get-Volume C` 与 `fsutil fsinfo volumeinfo C:` 写进日志 |

## 7. 启动 / 停止 / 重启

所有命令在仓库根、`nix develop .#quickemu` 内执行：

```bash
nix develop .#quickemu --command tools/windows-lab/bootstrap-host.sh status
nix develop .#quickemu --command tools/windows-lab/bootstrap-host.sh start
nix develop .#quickemu --command tools/windows-lab/bootstrap-host.sh wait-ssh
nix develop .#quickemu --command tools/windows-lab/bootstrap-host.sh stop
```

`start` 实际执行（等价于手工命令）：

```bash
cd /data/vaultsyncd-lab/win-lab
setsid nohup quickemu --vm windows-11.conf \
  --display none --viewer none --access local --spice-port 5930 \
  --public-dir none >> logs/quickemu-start.log 2>&1 &
```

- 4.9.9 里 `--access local` 时 SPICE 传输取决于 `--display`：`--display none` 用 TCP `127.0.0.1:<spice-port>`；`--display spice` 用本地 unix socket（`windows-11/windows-11.sock`），`--spice-port` 在该组合下被忽略。两种都不暴露公网；本卡采用 `--display none` 以便用 `ss -ltn` 直接核对 `127.0.0.1:5930`。
- SPICE 客户端不安装在本机（宿主无桌面）：从操作电脑 `ssh -N -L 5930:127.0.0.1:5930 用户@宿主` 后连 `spice://127.0.0.1:5930`（需要 `spicy`/`remote-viewer`）。纯运维不需要 SPICE，SSH 即可。
- 重启客体：`ssh ... 'shutdown /r /t 0'`；不要用 `quickemu --kill`（SIGKILL 会让 qcow2 refcount 进入需修复状态，实测出现过 `qemu-img` 修复 refcount 的日志）。
- `--kill` 仅在客体完全无响应时使用，用后第一次启动前先做一次干净关机才不会累积不一致。

## 8. SSH 连接

| 项目 | 值 |
| --- | --- |
| 端点 | 宿主 `127.0.0.1:2222` → 客体 `22`（`ssh_port="2222"`，quickemu 生成 `hostfwd=tcp::2222-:22`） |
| 用户 / 认证 | `Quickemu`（Administrators）+ 自动化 ed25519 私钥 `ssh/vaultsyncd-lab_ed25519`（`IdentitiesOnly=yes`） |
| 命令 | `ssh -p 2222 -i /data/vaultsyncd-lab/win-lab/ssh/vaultsyncd-lab_ed25519 -o IdentitiesOnly=yes -o UserKnownHostsFile=/data/vaultsyncd-lab/win-lab/ssh/known_hosts Quickemu@127.0.0.1 'cmd /c ver'` |
| 密码 | 仅在 `secrets/lab-credentials.txt`（0600），用于 SPICE 控制台登录/排障；不作为文档内容 |
| 服务形态 | `sshd` 启动类型 Automatic；管理员公钥走 `administrators_authorized_keys`（继承已禁用，只允许 Administrators 与 SYSTEM） |
| 认证限制 | 已实施密钥独占：`PasswordAuthentication no`、`KbdInteractiveAuthentication no`、`PubkeyAuthentication yes`；用 `ssh -o PreferredAuthentications=password -o PubkeyAuthentication=no` 实测返回 `Permission denied (publickey).`，同密钥登录仍成功 |
| 已知提示 | 每次连接会打印 `WARNING: connection is not using a post-quantum key exchange algorithm`：宿主 devShell 的 OpenSSH 10.6p1 对客体 in-box OpenSSH 9.5 的提示，不影响功能；需要静音时用 `-o LogLevel=ERROR`（该警告仍可能出现）或升级客体 OpenSSH |

## 9. 网络

- 客体网卡：`virtio-net-pci`，QEMU user 网络（slirp）。客体地址 `10.0.2.15/24`，网关/DNS `10.0.2.3`，宿主在客体看来是 **`10.0.2.2`**。客体 `localhost` 不是宿主。
- 宿主 → 客体：唯一通道是 `hostfwd`（当前 `2222→22`，SPICE `127.0.0.1:5930`）。
- **暴露面**：quickemu 4.9.9 只能生成 `tcp::<port>` 形式的转发，即绑定 `0.0.0.0:2222`；宿主自身在 `ens5f0` 上有公网地址且主机防火墙放行全部 TCP/UDP，因此该端口在客体运行期间可从宿主所在网络访问。收敛手段只能是客体侧认证（密钥登录 + 随机口令）。自 2026-11-06 起客体按用户决定**保持运行**（见 §19.1），因此“用完即关机”不再是收敛手段；改为依赖密钥独占认证与随机口令，需要临时下线时用 `bootstrap-host.sh stop`。
- 客体访问外网：slirp NAT 正常（实测与 Microsoft 更新端点建立连接）；`Delivery Optimization` 的 P2P 已由 `DODownloadMode=0` 策略关闭，避免与内网其他主机建 7680 连接。
- 客体网卡的网络类别实测是 `Public`（`Get-NetConnectionProfile`）。入站 22 的防火墙规则必须是任意 profile，否则宿主看到的是 `SYN_SENT`/banner 超时；这一点在 §6 与 §14 都有记录。

## 10. 干净关机

```bash
nix develop .#quickemu --command tools/windows-lab/bootstrap-host.sh stop
```

内部顺序：`ssh ... 'cmd /c shutdown /s /t 0'` → 轮询 `windows-11/windows-11.pid` 直到 qemu 进程消失（默认 180 s）→ 检查 `ss -ltn` 不再有 `2222`/`5930`。`shutdown` 命令返回非零也可能是连接先断开，判断以 qemu 是否退出为准。**不要留下孤儿 qemu/swtpm**；若 qemu 未退出，先查 `windows-11/windows-11.log`，不要 `kill -9`。

> **运行态保持（2026-11-06 用户决定）**：测试轮次结束、卡验收完成**也不关闭客体**，保持 qemu 与 SSH 在线，供后续卡直接复用（见 §19.1）。`stop` 只在用户明确要求、需要恢复基线或主机维护时使用；不再把“结束后干净关机”当作默认收尾。启动/停止前仍建议先确认真实运行状态（`bootstrap-host.sh status`），不要重复 `start`。

## 11. 基线备份与恢复

基线 = 未注册客户端身份、SSH 就绪、`C:` 为 NTFS 的干净状态（安装后、任何同步测试前）。必须同时备份以下**五类**内容，只备份磁盘会失去 EFI/TPM 状态，Windows 可能无法启动或 BitLocker/TPM 相关状态不一致：

```bash
VM=/data/vaultsyncd-lab/win-lab
# 客体必须已干净关机
tar -C "$VM" -cf /data/vaultsyncd-lab/win-lab-baseline-$(date -u +%Y%m%d).tar \
  windows-11.conf \
  windows-11/disk.qcow2 \
  windows-11/OVMF_VARS.fd \
  windows-11/tpm2-00.permall \
  windows-11/unattended.iso
sha256sum /data/vaultsyncd-lab/win-lab-baseline-*.tar
```

- `windows-11.swtpm-sock` 是运行时套接字，**不入备份**；`tpm2-00.permall` 是 TPM 状态，必须入备份。
- 本轮已生成的基线：`/data/vaultsyncd-lab/win-lab-baseline-20261105.tar`，13,082,818,560 字节（qcow2 稀疏，`--sparse` 打包），sha256 `de2fa63429617bb0eff09a1dbb0e41536220e42e3d7773599d527011c75265c6`；`tar -tvf` 列出 `windows-11.conf`、`windows-11/disk.qcow2`、`windows-11/OVMF_VARS.fd`、`windows-11/tpm2-00.permall`、`windows-11/unattended.iso` 五个条目。归档在最后一次干净关机之后生成。
- 凭据与密钥（`secrets/`、`ssh/`）单独按需备份/复制，不进同一份共享归档是默认做法；第二台客体应生成**新的**密钥与口令。
- 恢复：解包回同一 `VM_ROOT`，保持相对路径不变，再 `start`。`windows-11/disk.qcow2` 的虚拟大小固定 100 GiB，不要用 `qemu-img resize` 改成更小值。
- 第二台（S107 并发）：从基线解包到**另一个** `VM_ROOT`（例如 `/data/vaultsyncd-lab/win-lab-2/`），改 `VM_NAME`/`ssh_port`/`spice_port`，首次启动后用新密钥与口令，并让客户端生成独立 DeviceId；不要两台共用同一个 `tpm2-00.permall` 或同一个 qcow2。

## 12. 宿主交叉编译、传输与客体原生执行（ADR-013 / S009）

按 design ADR-013 与实施手册 §2.6：Windows 客户端、Windows 测试程序与前端都在 NixOS 宿主构建，客体只承担原生运行、调试、安装升级与 CFAPI/GUI 验收。本节是宿主构建 → 传输 → 客体执行的唯一操作入口；工具链版本与实测证据记录在 `docs/environment.md` §13。

### 12.1 分工与边界

1. 宿主构建：`nix develop .#windows` 提供 `x86_64-pc-windows-msvc` 交叉目标（S009）；前端同样在宿主用默认 devShell 构建。
2. 传输：`scp -P 2222 -i ... <产物> Quickemu@127.0.0.1:C:/vaultsyncd-lab/`，同时记录宿主 `sha256sum`。
3. 客体校验：`Get-FileHash -Algorithm SHA256`，与宿主哈希一致才算交付成功；哈希不一致时不得继续执行结果。
4. 客体只装运行所需：WebView2 运行时（Windows 11 自带）、必要 VC++ 运行库（如产物需要）；**不装** Rust/VS/SDK/Node/pnpm。
5. 原生运行/CFAPI/GUI 验收在客体上进行；交叉编译成功**不能**替代客体执行证据。CFAPI 行为验收属 S010—S012。
6. 不复制 `target/`、`node_modules/`、缓存或源码树进客体；只传具体产物与执行脚本。

### 12.2 三条命令（宿主侧，均在源码树内执行）

```bash
# 1) 首次准备：把固定 Rust 工具链与 Windows 标准库装进 .cache/windows/（幂等）
nix develop .#windows --command bash tests/platform/windows/setup-toolchain.sh

# 2) 交叉构建：产物写到 artifacts/windows/x86_64-pc-windows-msvc/ 并生成 SHA256SUMS
nix develop .#windows --command bash tests/platform/windows/build-cross.sh

# 3) 传输 + 客体原生执行 + 两端哈希比对（客体需先启动，见 §7）
nix develop .#windows --command bash tests/platform/windows/run-on-guest.sh
```

客体侧前置：`nix develop .#quickemu --command tools/windows-lab/bootstrap-host.sh start` 与 `wait-ssh`（实测 6 s 可用），结束后 `stop` 干净关机。`run-on-guest.sh` 把产物与 `guest-run.ps1` 传到 `C:\vaultsyncd-lab\s009\`，由 PowerShell 核对校验和、依次运行并采集退出码与输出；脚本退出码 0 才代表“客体原生执行成功且两端哈希一致”。

### 12.3 工具链来源与版本固定

| 项目 | 来源 | 固定方式 |
| --- | --- | --- |
| rustup 1.29.2、cargo-xwin 0.23.1、clang/clang-cl 21.1.8、lld-link 21.1.8、llvm-lib 21.1.8 | `flake.nix` 的 `windows` devShell（`devShells.x86_64-linux.windows`） | `flake.lock` 锁定的 nixpkgs rev `1d060086d60cb76a40cf37109f979d88a5bd6396` |
| 宿主 rustc/cargo 1.99.0 与 `x86_64-pc-windows-msvc` 标准库 | rustup 从 `static.rust-lang.org` 下载 | `rust-toolchain.toml` 的 `channel`/`targets`，`setup-toolchain.sh` 读该文件安装 |
| MSVC CRT 14.44.17.14、Windows SDK 10.0.26100（含 `cldapi.lib`、`cfapi.h`） | `cargo-xwin` 从 Microsoft 官方 manifest 下载 | `cargo-xwin` 0.23.1 的默认版本选择，缓存到 `.cache/windows/xwin` 并留 `DONE` 标记 |

- 目标三元组固定为 `x86_64-pc-windows-msvc`。**不用 GNU 目标**：锁定 nixpkgs 的 `pkgsCross.mingwW64`（mingw-w64 14.0.0）导入库中不含 `libcldapi.a`，CFAPI 无法链接；xwin 的 Windows SDK 含 `cldapi.lib`。
- 缓存目录都在工作树根的 `.cache/windows/`，已被 `.gitignore` 的 `/.cache/` 忽略，不写入 `~/.rustup`、`~/.cargo`：`rustup/`（工具链，实测 1.7 GB）、`cargo/`（cargo home）、`xwin/`（CRT+SDK，实测 1.2 GB）。
- 清理：`rm -rf .cache/windows`（下次 `setup-toolchain.sh` + `build-cross.sh` 会重新下载）。`artifacts/windows/` 同样在 `.gitignore` 忽略的 `artifacts/` 下，可单独删除。

### 12.4 产物与 CRT 链接方式

- `build-cross.sh` 用 `-C target-feature=+crt-static` 静态链接 MSVC CRT。原因：客体只有 CLR 版 `vcruntime140_clr0400.dll`，没有 `VCRUNTIME140.dll`，动态 CRT 产物在客体以 `0xC0000135`（`STATUS_DLL_NOT_FOUND`）退出（S009 实测）。静态链接后导入表只剩 `KERNEL32.dll`、`ntdll.dll`、`VERSION.dll`、`cldapi.dll`、`api-ms-win-core-synch-l1-2-0.dll`。
- 产品客户端最终用动态还是静态 CRT、是否随安装包分发 VC++ 运行库，属打包卡决定；本卡只为冒烟与测试产物选择无需客体改动的方案。
- S009 产物为 `platform-smoke.exe`（Win32 版本/架构/`version.dll` 探测）与 `cfapi-link-smoke.exe`（`CfGetPlatformInfo` 链接与运行时解析冒烟），源码在 `tests/windows-integration/`（包 `vaultsyncd-windows-tests`，不在 workspace `default-members`）。
- S010 起 `build-cross.sh` 额外构建 `cfapi-prototype.exe`（example 目标，源码 `crates/platform-windows/examples/`）。该步单独加 `-C link-arg=/ignore:4099` 抑制静态 CRT 预编译库缺 PDB 的 `LNK4099` 噪音，冒烟产物的编译参数保持 S009 原样。产物不是逐字节可复现（MSVC 链接写入调试信息签名），每次 run 以当次两端 SHA-256 比对为准。

### 12.5 首次实测证据（2026-11-05）

| 步骤 | 命令 | 结果 |
| --- | --- | --- |
| 工具链准备 | `nix develop .#windows --command bash tests/platform/windows/setup-toolchain.sh` | 退出 0：cargo 1.99.0 (5f94df478 2026-08-27)、rustc 1.99.0 (b940084d7 2026-09-28)、host `x86_64-unknown-linux-gnu`、`x86_64-pc-windows-msvc` 标准库目录存在 `libstd-*.rlib`、cargo-xwin 0.23.1、clang 21.1.8、LLD 21.1.8 |
| 交叉构建 | `nix develop .#windows --command bash tests/platform/windows/build-cross.sh` | 退出 0：首次下载 MSVC CRT/SDK `2m 7s`，`Finished` release profile in 0.29s；两个产物均为 `PE32+ executable for MS Windows 6.00 (console), x86-64`，238,592 B / 229,888 B |
| 宿主哈希 | `sha256sum -- *.exe`（产物目录） | `platform-smoke.exe` `6df177c907fcabe1d5f840b8117449392ce15ff1794b51f77cafae5f0be9d810`；`cfapi-link-smoke.exe` `acae5c6ef10b5514206916bcd92d735bc096fbd25737615243ccdbf8e38346b3` |
| 客体执行 | `nix develop .#windows --command bash tests/platform/windows/run-on-guest.sh` | 退出 0：客体 `Microsoft Windows 11 Enterprise Evaluation` 10.0.26300 build 26300、`AMD64`、PowerShell 5.1.26100.9444；两个产物 `guest_sha256` 与宿主一致（`MATCH`），退出码均为 0 |
| platform-smoke 输出 | 客体原生运行 | `build_target: x86_64-windows`、`GetSystemInfo.wProcessorArchitecture: 9`、`RtlGetVersion: 10.0 (build 26300)`、`version.dll file_version: 6.2.26100.1150`、`kernel32.dll file_version: 6.2.26100.9444`、`result: OK` |
| cfapi-link-smoke 输出 | 客体原生运行 | `CfGetPlatformInfo HRESULT: 0x00000000`、`CF_PLATFORM_INFO: build=26100 revision=9278 integration=1576`、`result: OK` |
| 前端类型检查（宿主） | `nix develop --command bash -c 'pnpm typecheck'` | 退出 0（`tsc --noEmit`，workspace 2 个项目） |
| 客体干净关机 | `bootstrap-host.sh stop` / `status` | 关机后 qemu 与 swtpm 退出、`2222`/`5930` 关闭、无 `qemu-system` 进程 |

## 13. 已知限制

- **评估版有效期**：Windows 11 Enterprise Evaluation 90 天评估期，到期后桌面会周期性提醒并可能限时关机；届时需要重新安装或改用合规授权镜像。它不是产品授权镜像。
- **单客体**：当前只有 `windows-11` 一台；并发与第二台从基线初始化属于 S107。
- **hostfwd 绑定地址**：见 §9，quickemu 4.9.9 不能把转发绑定到 `127.0.0.1`；依赖客体侧认证与关机纪律收敛。
- **headless 无画面**：`--display none` + qxl 下 `screendump`/SPICE 截图均为全黑（实测），排障不能靠截图；用 `disk.qcow2` 大小/mtime、`info usernet`、SSH 与 `C:\vaultsyncd-lab\provision.log` 判断进度。
- **首登耗时**：`Add-WindowsCapability OpenSSH.Server` 依赖 Windows Update，可能占满首次登录的十几分钟；期间宿主到 2222 的连接表现为 `SYN_SENT`（客体未监听）。
- **UAC 关闭**：`EnableLUA=false` 来自 quickemu 模板，实验室语义与生产不同。
- **SPICE 客户端**：本机无桌面，未装 `spicy`；SPICE 端口只用于从操作电脑经 SSH 隧道接入。
- **未验证项**：评估期到期行为、BitLocker、真实 LAN 暴露下的暴力破解压力、第二台并发（S107）均未验证。

## 14. 常见故障

| 现象 | 判断与处理 |
| --- | --- |
| 安装阶段“找不到磁盘” | `virtio-win.iso` 不是真 ISO（4,466 字节反爬 HTML）或没挂成 E:；重跑 `fetch-virtio` + `prepare` |
| 安装报“版本/密钥不匹配” | `autounattend.xml` 里仍有 Pro KMS 密钥；重新 `prepare`（脚本会拒绝残留密钥） |
| 宿主 `ssh -p 2222` 报 `Connection timed out during banner exchange` | 客体还没监听 22（安装/首登未完成）；看 `info usernet` 是否为 `SYN_SENT`，等 `wait-ssh` |
| sshd 已 `Running`、客体本地 `Get-NetTCPConnection -LocalPort 22` 在监听，宿主仍连不上 | 防火墙规则 profile 与网卡类别不匹配（能力规则只有 `Private`，网卡是 `Public`）：`Set-NetFirewallRule -Name OpenSSH-Server-In-TCP -Profile Any` |
| qcow2 出现 `ERROR cluster 5 refcount=0 reference=1` | 上一次 `--kill` 强杀导致；`qemu-img` 会自动修复，之后用 `stop` 做干净关机 |
| SPICE 端口被占 | `start` 前会检查 `ss -ltn`；用 `SPICE_PORT` 环境变量换端口并记录 |
| 首次启动后立即 100% CPU 且长时间无 SSH | Windows Update 在首登脚本里下载 FoD；按 §13 等待，必要时用 SPICE 控制台登录看进度 |
| 忘记口令 | `sudo cat /data/vaultsyncd-lab/win-lab/secrets/lab-credentials.txt`；密钥丢失则重新 `prepare` 后需要重装或经 SPICE 手工补 `administrators_authorized_keys` |
| `nix develop .#windows` 提示“工具链尚未安装” | 先跑 `tests/platform/windows/setup-toolchain.sh`；`shellHook` 用 `rustup which cargo` 解析 `rust-toolchain.toml`，不做隐式下载 |
| `cargo xwin build` 卡在 `Downloading MSVC CRT...` | 首次要取 MSVC CRT 与 Windows SDK（实测约 1.2 GB，2 分钟）；检查到 Microsoft manifest 的网络，失败后重跑即可续传 |
| 客体执行产物立刻以退出码 `-1073741515`（`0xC0000135`）结束 | 动态 CRT 缺少 `VCRUNTIME140.dll`；用 `build-cross.sh`（带 `+crt-static`）重新构建，不要往客体装编译工具链 |
| HRESULT/链接报找不到 `CfGetPlatformInfo` 或 `cldapi.lib` | 确认走的是 `cargo xwin build`（xwin SDK 才有 `cldapi.lib`）；`pkgsCross.mingwW64` 的 mingw-w64 14.0.0 不带该导入库 |
| 两端哈希判定为不一致但打印值相同 | 客体输出是 CRLF；`run-on-guest.sh` 已统一 `tr -d '\r'`，自定义脚本也要去 CR 后比对 |
| `.cache/windows` 占用过大（实测 rustup 1.7 GB + xwin 1.2 GB） | 全部可再生：`rm -rf .cache/windows` 后重跑 `setup-toolchain.sh` 与 `build-cross.sh` |

## 15. 备选通道：WinRM（5985）

`provision.ps1` 用 `Enable-PSRemoting -SkipNetworkProfileCheck -Force` 打开 WinRM 的 HTTP 监听（5985，NTLM）。quickemu 的 conf 只转发 SSH 端口，宿主要用 WinRM 时在运行时追加一条**仅本机**的转发（QEMU 11.1.2 实测可用）：

```bash
printf 'hostfwd_add nic tcp:127.0.0.1:5985-10.0.2.15:5985\n' \
  | socat - UNIX-CONNECT:/data/vaultsyncd-lab/win-lab/windows-11/windows-11-monitor.socket
curl -s -i -m 10 -X POST -H 'Content-Type: application/soap+xml;charset=UTF-8' \
  --data '<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"></s:Envelope>' \
  http://127.0.0.1:5985/wsman | head -3   # 期望 HTTP/1.1 401 + WWW-Authenticate: Negotiate
```

需要凭据与 pywinrm（NTLM）：

```bash
P=$(nix build --no-link --print-out-paths --impure --expr \
  'let f = builtins.getFlake "'"$PWD"'"; p = f.inputs.nixpkgs.legacyPackages.${builtins.currentSystem}; in p.python3.withPackages (ps: [ ps.pywinrm ])')
PYTHONPATH=$P/lib/python3.14/site-packages $P/bin/python3 your-script.py
```

注意：`hostfwd_add` 只对本次运行生效（重启即丢失），并且绑定 `127.0.0.1` 不会造成额外暴露。WinRM 只用于 SSH 不可用时的排障，不作为常规自动化通道；SSH 一旦可用就以 SSH 为准。qemu-guest-agent 服务在客体是 Running，但宿主 `windows-11-agent.sock` 上 `guest-ping` 无响应，暂不作为通道。

## 16. CFAPI 注册与占位创建原型（S010）

S010 在客体本地 NTFS 上注册测试同步根、创建真实占位符、把不透明身份写进 `FileIdentity`，并实现“保留用户数据”的清理。代码在 `crates/platform-windows/src/cfapi/`，命令是 `crates/platform-windows/examples/` 下的 example；宿主交叉编译与客体执行入口是 `tests/platform/windows/{build-cross,run-cfapi-prototype}.sh`。契约摘要见 `docs/architecture/interfaces.md`，实测环境事实见 `docs/environment.md` §14。

### 16.1 两条命令（宿主侧，客体需先启动）

```bash
# 1) 交叉构建：同时产出 S009 的两个冒烟 exe 与 cfapi-prototype.exe（artifacts/windows/x86_64-pc-windows-msvc/）
nix develop .#windows --command bash tests/platform/windows/build-cross.sh

# 2) 传输 + 客体执行完整验收（create → inspect → 第三方证据 → read 探测 → cleanup → 保留内容核对）
nix develop .#windows --command bash tests/platform/windows/run-cfapi-prototype.sh
```

`run-cfapi-prototype.sh` 把 exe 与 `guest-cfapi-prototype.ps1` 传到 `C:aultsyncd-lab\s010\`（`CFAPI_GUEST_DIR` 可覆盖），同步根默认 `C:aultsyncd-test`（`CFAPI_ROOT` 可覆盖）。脚本先做一次幂等 `cleanup` 清场，再放入用户内容 `user-keep.txt` 与 `user-dir\keep-note.txt`，然后逐步执行并逐项打印 `check=pass|fail`；退出码 0 才代表全部检查通过。S009 的 `run-on-guest.sh` 用法不变。

### 16.2 原型命令

```text
cfapi-prototype create  [--root C:\vaultsyncd-test]   # 注册 → 连接 → 建占位 → 水合读取探测 → 断开
cfapi-prototype inspect [--root C:\vaultsyncd-test]   # 只读：注册状态 + 每个条目的占位/大小/身份
cfapi-prototype cleanup [--root C:\vaultsyncd-test]   # 只删本工具占位 → 断开 → 注销 → 列出保留内容
```

退出码：0 成功；1 失败（输出 `stage=`/`hresult=` 与原因）；2 用法错误；3 `inspect` 发现同步根未注册（cleanup 之后的预期状态）。固定条目集合在 `cfapi::prototype_layout`：目录 `vaultsyncd-proto` 先建，其下 `small-12345.bin`（12,345 B）与 `large-1mib.bin`（1,048,576 B）；EntryId 由条目名确定性派生，便于三个命令互相核对。

### 16.3 实测证据（2026-11-05，客体原生运行，退出码 0）

| 验收点 | 证据 |
| --- | --- |
| 系统识别为占位 | `inspect`：三个条目 `placeholder=true placeholder_source=placeholder_info`；`Get-Item` 属性 `4199968`（`REPARSE_POINT`+`RECALL_ON_DATA_ACCESS`+`SPARSE_FILE`+`OFFLINE`+`ARCHIVE`）；`fsutil reparsepoint query` 显示 `Reparse Tag Value : 0x9000001a`、`Reparse Data Length: 0xa8` |
| identity 写入系统 | reparse data 里可见 `56 53 01 00`（`b"VS"`+schema 1+保留 0）+ RootId + EntryId + 版本提示 + CRC-32，共 48 B；`inspect` 解码出的 RootId/EntryId/版本提示与 `create` 日志一致，`identity_length=48` |
| 逻辑大小正确 | `Get-Item`：`small-12345.bin` `Length=12345`、`large-1mib.bin` `Length=1048576`；`inspect` 的 `logical_size` 与设定值逐条相等 |
| 不是普通零字节文件 | `inspect`：`on_disk_data_size=0`、`dehydrated=true`、`in_sync_state=0`；`fsutil sparse queryflag` 报 “set as sparse”；提供方不在线时读取占位返回 `0x80131501`（provider not running），在线时 `FETCH_DATA` 回调被调用两次并以 `CfExecute` 回执 `0x0` + `STATUS_NOT_SUPPORTED` 结束，读取失败 `ERROR_CLOUD_FILE_UNSUCCESSFUL`（389），**没有返回空数据** |
| 清理不删除测试内容 | `cleanup` 逐条删除 3 个占位（`status=removed`），`user-keep.txt` 与 `user-dir\keep-note.txt` 的 SHA-256 前后一致（`7c1b69d9…e054`、`98b12897…134c`），`cleanup.preserved` 逐条列出保留内容 |
| 注销生效 | `CfUnregisterSyncRoot` `0x0`，随后 `CfGetSyncRootInfoByPath` 返回 `0x80070186`（`ERROR_CLOUD_FILE_NOT_UNDER_SYNC_ROOT`）；再次 `inspect` 退出码 3；再次 `cleanup` 退出码 0（幂等） |
| 生命周期日志 | 每步一行 `stage=… key=value … hresult=0x…`：`register`/`connect`/`create.placeholders`/`create.placeholder`/`hydration.probe`/`disconnect`/`cleanup.remove`/`unregister`/`sync_root.query`/`cleanup.preserved`，失败时命令以非零退出并打印 `stage` 与原始码 |

### 16.4 实测现象与故障处理

| 现象 | 判断与处理 |
| --- | --- |
| `CfExecute` 回执返回 `0x8007017C`（`ERROR_CLOUD_FILE_INVALID_REQUEST`），读取最终超时为 426 | `ParamSize` 必须是 `sizeof(CF_OPERATION_PARAMETERS)`=48（不是 `TransferData` 分支的 40），并且回执的 `Offset`/`Length` 必须与 `CF_CALLBACK_PARAMETERS::FetchData` 的请求范围一致。修正后回执 `0x0`，读取立即以 389 失败 |
| 目录占位符的 `CfGetPlaceholderStateFromFindData` 返回 `0x00000000`、`reparse_tag=0` | `FindFirstFileW` 直接查路径时读不到云占位符的 reparse tag（`Get-Item`/`fsutil` 都能看到 `0x9000001a`）。占位判定改用 `CfGetPlaceholderInfo`（非占位符返回 `ERROR_NOT_A_CLOUD_FILE`），日志里用 `placeholder_source` 标明来源 |
| `attrib <占位符>` 报 `Path not found` | 客体上 `attrib` 不能处理云占位符；证据改用 `Get-Item` 属性、`fsutil reparsepoint query`/`sparse queryflag` 与 CFAPI 查询 |
| 提供方进程退出后再枚举占位目录报 “The cloud file provider is not running.” | 这是 `CF_POPULATION_POLICY_FULL` 的预期行为：占位目录的枚举要提供方在线。原型只按绝对路径操作自己的条目，不依赖枚举 |
| 注册残留（进程中途被杀） | 用 `cfapi-prototype cleanup --root C:\vaultsyncd-test` 收尾；若连清理也失败，`Remove-Item -Recurse -Force C:\vaultsyncd-test` 前先确认 `inspect` 不再报告已注册 |
| 两端 exe 哈希与历史记录不同 | 交叉构建产物不是逐字节可复现（MSVC 链接阶段写入调试信息签名）；每个 run 都以当次宿主/客体哈希比对为准，不使用固定金样哈希 |

### 16.5 清理与边界

- 结束前必须让 `cleanup` 返回 0、`inspect` 返回 3，再删除 `C:\vaultsyncd-test`；本轮结束时客体已干净关机，无注册残留。
- 本卡不实现内容水合（S011）、释放/写保护（S012）；`FETCH_DATA` 只返回明确错误，占位始终保持脱水。
- 未标记 in-sync：`CF_INSYNC_POLICY_NONE` + `CF_PLACEHOLDER_CREATE_FLAG_NONE`，`inspect` 报告 `in_sync_state=0`，并在非 0 时失败。

## 17. CFAPI 内容供给：读取 / 范围 / 离线 / 超时 / 中断（S011）

S011 让原型真的供给内容：`FETCH_DATA` 按回调请求的 `Offset`/`Length` 从固定测试内容源分段回填
（`CfExecute(TRANSFER_DATA)`），并实现离线、超时与传输中断的明确语义。代码在
`crates/platform-windows/src/cfapi/{content,provider}.rs`，命令仍是
`crates/platform-windows/examples/` 下的 example；宿主入口是
`tests/platform/windows/run-cfapi-hydration.sh`。契约摘要见 `docs/architecture/interfaces.md`，
环境事实见 `docs/environment.md` §17。

### 17.1 两条命令（宿主侧，客体需先启动）

```bash
# 1) 交叉构建（与 S010 相同的脚本，产物含 cfapi-prototype.exe）
nix develop .#windows --command bash tests/platform/windows/build-cross.sh

# 2) 传输 + 客体执行 S011 验收（读取/范围/离线/超时/中断/重复请求 + 两端哈希比对）
nix develop .#windows --command bash tests/platform/windows/run-cfapi-hydration.sh
```

`run-cfapi-hydration.sh` 先把 exe 与 `guest-cfapi-hydration.ps1` 传到
`C:\vaultsyncd-lab\s011\`（`CFAPI_HYDRATION_GUEST_DIR` 可覆盖），同步根仍默认
`C:\vaultsyncd-test`（`CFAPI_ROOT` 可覆盖）。宿主侧期望哈希来自
`cargo run --example cfapi-prototype -- expect`（固定种子 ⇒ 固定内容与 SHA-256）；
客体侧用 PowerShell `Get-FileHash` / 自算区间哈希独立计算，两端必须一致。S010 的
`run-cfapi-prototype.sh` 保持可用（其读取探测已改为“水合读取成功且长度正确”）。

### 17.2 原型命令（S011 追加）

```text
cfapi-prototype read    --name <entry> [--offset N --length M] [--source available|unavailable]
                        [--latency-ms N] [--timeout-ms N] [--chunk-delay-ms N]
                        [--expect success|failure]
cfapi-prototype cancel  --name <entry> [--cancel-after-ms N] [--chunk-delay-ms N]
                        [--require-cancel-callback]
cfapi-prototype hydrate --name <entry> [--offset N --length M] [--cancel-after-ms N]
                        [--disconnect-after-ms N] [--chunk-delay-ms N]
cfapi-prototype inspect [--root <path>] [--allow-hydrated]
cfapi-prototype expect  [--name <entry> --offset N --length M]
```

固定内容条目：`small-12345.bin`（12,345 B）、`large-1mib.bin`（1 MiB）、
`huge-16mib.bin`（16 MiB，S011 新增）。整份 SHA-256 由 `expect` 打印，宿主/客体两端比对。

### 17.3 实测证据（2026-11-05，客体原生运行，退出码 0）

| 验收点 | 证据 |
| --- | --- |
| 读取字节与哈希正确 | `read small-12345.bin`：`bytes=12345`、`sha256` 与 `expected_sha256` 一致、`on_disk_data_size=12345`；客体 `Get-FileHash` = `a82033db…4bcd`，与宿主 `expect` 相同（`MATCH`） |
| 大条目分段供给 | `read huge-16mib.bin`：`chunks_sent=256`、`bytes_served=16,777,216`、`sha256=fb58d81c…f2ed` 与宿主一致 |
| 范围请求 | `hydrate huge-16mib.bin --offset 4000000 --length 1048576`：平台按页对齐要求 `platform_offset=3997696 platform_length=1052672`（≠ 整份 16 MiB）；客体按区间自算的哈希 = 宿主 `expect_range`；`on_disk_data_size=1052672` |
| 离线 + 未水合 | `read --source unavailable`：`FETCH_DATA` 被拒 `ack_status=0xC000CF11`（`STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE`），读取失败 `os error 388`、`bytes=0`、`elapsed_ms=1`，三个条目都如此；`unavailable_fetches≥1` |
| 离线 + 已水合 | 先 `read` 成功水合 `small-12345.bin`，再 `read --source unavailable`：`outcome=matched`、`digest_match=true`、`fetch_requests=0`（完全本地命中）；同一次运行里 `large-1mib.bin` 仍脱水，读取仍明确失败 |
| 超时 | `read --latency-ms 1200 --timeout-ms 600 --expect failure`：`timeout_fetches=1`、读取失败、`elapsed_ms` 远小于阈值×2（等待被截断到阈值）；1 MiB 条目同样有界失败 |
| 传输中断（无永久卡住） | `hydrate large-1mib.bin --chunk-delay-ms 150 --disconnect-after-ms 400`：`CfDisconnectSyncRoot` 成功后回填循环在 `chunks_sent=3/16` 就停下（后续 `CfExecute` 失败记入 `failed_acks=1`），`CfHydratePlaceholder` 以 `0x8007016A` 在 `total_ms≈305` 返回，`in_flight=0` |
| 取消路径（句柄可关闭） | `cancel large-1mib.bin --cancel-after-ms 300 --chunk-delay-ms 100`：`CancelSynchronousIo` 成功（`cancel_code=0`），读取在 `cancel_to_return_ms≈1300` 内返回、句柄正常关闭、`in_flight=0`；**平台未回调 `CANCEL_FETCH_DATA`**（`cancel_callbacks=0`，见 §17.4） |
| 重复请求幂等 | 离线失败时同一次读取里系统重试 `FETCH_DATA` 两次、两次回执相同状态且都不返回数据；超时失败后再次 `read` 成功且哈希与首次水合一致（`retry_after_timeout_hash` 一致） |
| 回调生命周期 | 上下文是进程级 `OnceLock<Arc<ProviderContext>>`（回调不解引用 `CallbackContext` 裸指针）；断开后 `provider.drain in_flight=0` 才上报结果；`cancel`/`hydrate` 的中断路径同样如此 |
| 清理不误删用户内容 | `cleanup` 删除探针子树后 `user-keep.txt` SHA-256 不变，`inspect` 退出 3（未注册），再次 `cleanup` 退出 0 |

### 17.4 实测现象与故障处理（S011）

| 现象 | 判断与处理 |
| --- | --- |
| 打开带读权限的占位符即触发**整份**水合并阻塞 | `open_ms≈993`（1 MiB、每段 60 ms）；甚至只读 4 KiB 或 1 MiB 以上条目时平台也要求 `required_offset=0 required_length=<整份>`。`CF_HYDRATION_POLICY_FULL` 与 `PARTIAL` 在本 build（26300）上表现一致；因此“同进程中途取消一次读取”没有可取消的用户态 I/O |
| 想拿到真正的**范围**请求 | 用只请求 `FILE_READ_ATTRIBUTES` 的句柄调 `CfHydratePlaceholder(handle, offset, length, …)`：平台按页对齐请求部分区间（详见 §17.3）。直接读文件做不到 |
| `CANCEL_FETCH_DATA` 未观察到 | 试过 5 条路径：① `CancelSynchronousIo(读线程)` 返回成功但水合继续完成；② `CancelIoEx(文件句柄)` 返回 `ERROR_NOT_FOUND`(1168)；③ 传输中关闭句柄——`CfHydratePlaceholder` 立刻返回但平台仍把 17/17 段要完；④ 传输中 `CfDisconnectSyncRoot`——在途分段被拒（`CfExecute` 0x80070057）、循环立即停止，但没发 cancel 回调；⑤ 跨进程读取（PowerShell 或 `--no-connect` 的 hydrate）时提供方**完全收不到** `FETCH_DATA`，请求方一直阻塞。结论：本 build 在该 NTFS 卷上没有发出过 `CANCEL_FETCH_DATA`；取消路径的“尽快退出”由纯逻辑单测（`serve_range` 的 `aborted` 分支）+ 中断实测（④）覆盖，严格断言用 `--require-cancel-callback` 保留 |
| 跨进程读取不到提供方回调 | 别的进程读同一同步根时，本进程（`CfConnectSyncRoot` 持有者）收不到 `FETCH_DATA`，请求方无限等待；因此取消/中断验证都在同一进程内完成。跨进程唤醒需要进一步排查（可能与连接归属/卷级重定向器状态有关），记录为限制 |
| `CF_POPULATION_POLICY_FULL` 下其他进程按路径打开占位符阻塞 | 改为 `CF_POPULATION_POLICY_PARTIAL`（不实现 `FETCH_PLACEHOLDERS` 时不应让平台自动填目录）。S010 的“枚举占位目录报 provider not running”现象随策略改变而不同，S010 脚本不依赖枚举 |
| 宿主 `cargo run --example cfapi-prototype -- expect` | 在 `nix develop .#windows` 内可用：`expect` 子命令与内容源都是平台无关的纯逻辑，宿主侧期望值因此不依赖客体 |

### 17.5 清理与边界

- 结束前必须让两个脚本各自返回 0（`cfapi_hydration_result: OK` / `cfapi_prototype_result: OK`），再清理探针目录与同步根，并干净关机。
- S011 仍未实现：释放/写保护（S012，`CfDehydratePlaceholder`/`CfSetInSyncState`/写保护）、Explorer 展示、崩溃后注册残留恢复。
- 内容源是确定性的固定版本（`CONTENT_SEED`），不是服务端对象存储，也不做块级哈希校验；真实实现按 `entry_id + version_id` 取不可变对象并逐块校验。

## 18. CFAPI 释放与写入保护（S012）

S012 让原型能做释放（dehydrate）并先做写入保护（protect）：先取“已观察 generation + 使用保护”
的复合并给出 `clean`/`dirty`/`busy`/`unknown`，只有 `clean` 才按系统认可的流程
`CfSetInSyncState(IN_SYNC)` → `CfDehydratePlaceholder(0, 逻辑大小)` 释放；其余判定一律拒绝
并保留本地内容。代码在 `crates/platform-windows/src/cfapi/{guard,release}.rs`，命令仍是
`crates/platform-windows/examples/` 下的 example；宿主入口是
`tests/platform/windows/run-cfapi-release.sh`。契约摘要见 `docs/architecture/interfaces.md`，
环境事实见 `docs/environment.md` §18。

### 18.1 两条命令（宿主侧，客体需先启动）

```bash
# 1) 交叉构建（与 S010/S011 相同的脚本，产物含 cfapi-prototype.exe）
nix develop .#windows --command bash tests/platform/windows/build-cross.sh

# 2) 传输 + 客体执行 S012 验收（protect/release + 四类编辑器保存 + 两端哈希比对）
nix develop .#windows --command bash tests/platform/windows/run-cfapi-release.sh
```

`run-cfapi-release.sh` 把 exe 与 `guest-cfapi-release.ps1` 传到 `C:\vaultsyncd-lab\s012\`
（`CFAPI_RELEASE_GUEST_DIR` 可覆盖），主同步根 `C:\vaultsyncd-test`（`CFAPI_RELEASE_ROOT`），
rename-over 专用根 `C:\vaultsyncd-test-rename`（`CFAPI_RELEASE_RENAME_ROOT`）。rename-over 会把
目标名字留在“无法按路径打开”的状态（见 §18.6），因此单独用一个根，不污染主根后续检查。

### 18.2 原型命令（S012 追加）

```text
cfapi-prototype protect --name <entry> [--root <path>]
                        # 只判定、不改状态（不释放、不写 in-sync）
cfapi-prototype release --name <entry> [--root <path>] [--expect released|refused]
                        # 判定 clean 才释放；其余拒绝并保留本地内容
```

`protect`/`release` 都会打印 `stage=guard.observe` / `guard.probe` / `guard.content` /
`guard.decision`；`release` 追加 `release.in_sync` / `release.dehydrate` / `release.result`
（`outcome=released|already_dehydrated|refused|dehydrate_failed`）。`--expect` 只判断
“内容是否已经不在本地”：`released` 接受 `released`/`already_dehydrated`，`refused` 接受
`refused`/`dehydrate_failed`；`dehydrate_failed` 表示判定为 `clean` 但平台拒绝释放，
原始 `HRESULT` 必须出现在日志里，不能当成功。

### 18.3 判定规则（只有 `clean` 允许释放）

顺序即保守程度顺序，任一条无法证明就立刻返回，`release` 不调用任何平台写操作。

| 判定 | 条件（按检查顺序） | `release` 处置 |
| --- | --- | --- |
| `unknown` | 不是占位符 | 拒绝（不碰文件） |
| `unknown` | `on_disk_data_size=0`（已脱水）→ 也直接跳过三个探测与内容核对，避免“为了判断而先水合” | 报告 `already_dehydrated`（无本地内容可丢） |
| `unknown` | `CF_PIN_STATE_PINNED`/`PINNED_RECURSIVE`（不替用户解除 pin） | 拒绝 |
| `busy` | 独占探测失败且错误码为 `ERROR_SHARING_VIOLATION`(32)/`ERROR_LOCK_VIOLATION`(33)：有人持有不允许读/写共享的句柄 | 拒绝，`dehydrate_hresult=-` |
| `unknown` | 探测因其它错误码失败，或三个探测有任一项没执行 | 拒绝 |
| `unknown` | 本地数据少于逻辑大小（只有部分区间在本地） | 拒绝 |
| `dirty` | 本地内容 SHA-256 ≠ 固定内容版本的期望值（有未上传的本地修改） | 拒绝，`dehydrate_hresult=-` |
| `unknown` | 内容无法核对（读取失败、非原型条目） | 拒绝 |
| `busy` | 两次内容哈希不一致（观察窗口内内容被改动） | 拒绝 |
| `clean` | 以上全部通过 | `CfSetInSyncState(IN_SYNC)` + `CfDehydratePlaceholder(0, size)`，复核 `on_disk_data_size=0` 后才报 `released` |

三个探测都是“只开句柄、立即关闭”（`GENERIC_READ`+`dwShareMode=0`、`GENERIC_WRITE`+
`dwShareMode=0`、`GENERIC_READ`+全共享），不读不写内容。

### 18.4 实测证据（2026-11-05，客体原生运行，`run-cfapi-release.sh` 退出 0）

`cfapi-prototype.exe` SHA-256 `5219ad5b…502a6`（两端一致）；**74 项 `check=pass`、0 fail、
1 blocked**（rename-over，见 §18.6）。逐项日志在 `artifacts/s012/s012-release-final.log`
（`artifacts/` 不入 Git）。

| 验收点 | 证据 |
| --- | --- |
| 干净样本可释放 | 先 `read small-12345.bin`（`on_disk_data_size=12345`，哈希 = 宿主 `expect` = `a82033db…bcd`）→ `protect`：`verdict=clean reason=content_matches_committed_version content=matched content_stable=true probes=opened/opened/opened` → `release`：`outcome=released in_sync_hresult=0x00000000 dehydrate_hresult=0x00000000 on_disk_after=0` |
| 释放后再读字节仍正确 | 新进程 `inspect` 退出 0（仍是占位、逻辑大小 12345、身份不变、`dehydrated=true`）；`protect` 报 `in_sync_state=1 reason=already_dehydrated`（`CfSetInSyncState` 的确生效）；`read` 重新触发 `FETCH_DATA`（`fetch_requests=1`）、`digest_match=true`，客体 `Get-FileHash` 与释放前完全一致 |
| 原地写（同 inode 覆盖写）不可释放 | PowerShell 以 `FileShare.None` 覆盖前 100 B → 哈希 `e01501b7…497ce` → `protect`：`verdict=dirty reason=content_mismatch modified_data_size=4096` → `release --expect refused`：`outcome=refused dehydrate_hresult=-`，释放后哈希不变、`on_disk_after=12345`（仍水合） |
| 映射写（`MemoryMappedFile` 写后关闭）不可释放 | 映射写入 64 B → 哈希 `5e81b022…302224e` → `verdict=dirty content=mismatched modified_data_size=12345` → `outcome=refused`，哈希与大小不变 |
| 映射仍打开时不可释放 | 另一进程建可写映射并写 32 B 后保持 6 s → `verdict=busy reason=handle_held_read`（三个探测 `error:32`）→ `outcome=refused dehydrate_hresult=- on_disk_after=12345`；持有者退出后哈希变成 `ea0d7e1a…`（映射写入确实落地），文件仍在 |
| 独占句柄（`FileShare.None`）不可释放 | 另一进程持有读写独占句柄 → 三个探测全部 `error:32` → `verdict=busy` → `outcome=refused`、`dehydrate_hresult=-`、哈希不变；**持有者退出后同一内容 `verdict=clean` 且 `outcome=released`**（证明拒绝来自使用保护而非别的原因） |
| 共享锁（`FileAccess.Read`+`FileShare.Read`）不可释放 | 探测 `probe_read_exclusive=error:32`（其它两项 `opened`）→ `verdict=busy reason=handle_held_read` → `outcome=refused`，哈希不变 |
| 已脱水条目不被“判定”打扰 | 对刚 `create` 的条目直接 `protect`：`verdict=unknown reason=already_dehydrated`，三个探测都是 `skipped`、内容 `unverified`（不为判定而水合） |
| 用户内容与注销不受影响 | 收尾 `cleanup` 删探针子树、`user-keep.txt` SHA-256 前后一致、`inspect` 退出 3（未注册）、再次 `cleanup` 退出 0 |

### 18.5 边界表：已观察 generation 与使用保护

“本地是 clean”只是**上次观察**（design §8.4），因此原型把“能观察到什么”和“能证明什么”分开记录：

| 证据 | 来源 | 能证明 | 不能证明 |
| --- | --- | --- | --- |
| 内容 SHA-256（两次） | 读本地文件 + 固定内容版本期望值 | 本地字节等于已确认版本；两次一致说明观察窗口内没变 | 之后是否被再次改写；未落到文件系统的编辑器内存 |
| `on_disk_data_size`/`logical_size` | `CfGetPlaceholderInfo(CF_PLACEHOLDER_INFO_STANDARD)` | 是否整份在本地（部分水合不能代表整份内容） | —— |
| `in_sync_state` | 同上 | `1` 表示本提供方在释放前用 `CfSetInSyncState` 声明过“内容已确认”；写入后系统把它清回 0 | `0` 不等于 dirty（本原型从不自动置位，只能作为“未被确认”的弱信号） |
| `pin_state` | 同上 | `PINNED`/`PINNED_RECURSIVE` 时必须拒绝（不替用户解 pin） | —— |
| `modified_data_size` | 同上 | 原地写后为 4096（页粒度）、映射写后为 12345，是有修改的**旁证** | 不能替代内容哈希（判定仍以哈希为准） |
| `FileIdentity.version_hint` | `CfGetPlaceholderInfo` 的 blob | 条目身份与版本提示在释放前后不变 | 提示是原型固定值，不代表真实版本推进 |
| 共享模式探测（3 组） | `CreateFileW` + `GetLastError` | 有人持有不允许读/写共享的句柄（实测 `FileShare.None` 与 `FileShare.Read` 都是 `error:32`） | **完全共享**的读写者看不到；句柄之间没有持有者时也可能有内存映射/延迟写 |
| 平台 dehydration 门 | `CfSetInSyncState`/`CfDehydratePlaceholder` 返回值 | 系统在拒绝时给出明确 `HRESULT`，最终以 `on_disk_data_size=0` 复核 | —— |

### 18.6 实测限制与阻塞

| 项 | 现象（本 build 26300 / `C:` NTFS） | 结论与恢复条件 |
| --- | --- | --- |
| **rename-over 保存**（T27/T28） | ① 同步根**内**新建临时文件失败：占位目录下返回 `0x801F0005`（“An invalid name request…”），同步根顶层返回 `0x8007016A`（“The cloud file provider is not running.”），即使另一进程正持有连接也一样；② 改在同步根外用同卷临时文件 `Move-Item -Force` 覆盖目标：命令报告成功（`rename_over_move=ok`），但目标名字随后**无法按路径打开**（`0x801F0005`），`protect` 因此以 `stage=query.placeholder` 失败（退出 1），内容与身份都无法核对 | **阻塞**：本原型无法证明 rename-over 保存安全，也未观察“新内容落到目标名下”。恢复条件：提供方需要在被保存进程可见的范围内在线（打通 S011 记录的跨进程唤醒）并处理平台的重命名/删除通知，再重跑本项断言 |
| rename-over 后的残留 | 目标名字出现在 `FindFirstFileW` 里但 `open_attributes` 失败，目录枚举也报错；`del`/`rd /s /q` 在**注册状态**下分别返回 “provider exited unexpectedly”/“目录不是空的” | 恢复步骤（已实测）：先 `cleanup`（S012 起对“探针目录非空”记 `status=kept_not_empty` 并继续注销）→ `inspect` 退出 3 → `rd /s /q` 成功。因此 `cleanup` 的“必须能注销”优先于“必须删掉探针目录” |
| `dehydrated_after=false` 但 `on_disk_after=0` | 释放进程内紧跟 dehydrate 的 `FindFirstFileW` 仍带 `PARTIALLY_ON_DISK` 位（同进程 find-data 缓存），新进程 `inspect` 报 `dehydrated=true` | 复核用**新进程** `inspect`（脚本已如此），不用释放进程内的状态位 |
| 释放前的干净判定需要读内容 | 内容哈希必须读整份本地内容，因此一个已脱水条目在“判定→释放”过程中会被先水合再脱水 | 原型限制（真实实现要靠已记录的内容哈希/代际与短期内容租约，design §8.4）；已脱水条目直接短路，不再读内容 |
| 完全共享的写者不可见 | share 模式允许读写删除的第三方句柄三个探测都通过 | 这是 design §14.4 说“锁/共享标志只是证据的一部分”；原型靠内容哈希 + 平台的 dehydrate 门兜底，真实实现仍需补稳定窗口/批处理 |

### 18.7 清理与边界

- 结束前必须让 `run-cfapi-release.sh` 返回 0 且 `blocked_item=` 只列出已知项，再清理 `C:\vaultsyncd-test`、`C:\vaultsyncd-test-rename`、`C:\vaultsyncd-lab\s012` 探针目录并干净关机。
- 未实现：真实内容租约与版本绑定（内容源仍是固定版本）、Explorer 展示、崩溃后注册残留恢复、真实编辑器（Word/VS Code）保存路径。

## 19. 当前状态（2026-11-05 本轮结束）

- 客体**已干净关机**（`shutdown /s /t 0` 后 qemu 与 swtpm 退出，`2222`/`5930` 已关闭，无孤儿进程）。下次使用：`nix develop .#quickemu --command tools/windows-lab/bootstrap-host.sh start`，再 `wait-ssh`（实测从启动到 SSH 可用约 20 秒）。
- 系统：Windows 11 Enterprise Evaluation 26H2 / build 26300.9457，`C:` 为 NTFS（98.6 GiB，可用约 78.7 GiB），8 vCPU / 8 GiB / 100 GiB VirtIO GPT。
- 通道：SSH 密钥登录（`Quickemu@127.0.0.1:2222`，密钥独占认证）+ SPICE `127.0.0.1:5930` + WinRM 5985（按 §15 临时转发）。
- 基线归档：`/data/vaultsyncd-lab/win-lab-baseline-20261105.tar`（含 conf/qcow2/OVMF_VARS/TPM 状态/unattended.iso）。
- 客体未安装任何编译工具链。S009 用 `nix develop .#windows` 交叉编译 `platform-smoke.exe` / `cfapi-link-smoke.exe`，传输后客体原生执行退出码均 0、两端 SHA-256 一致（见 §12.5），因此“宿主构建 → 客体运行”通道已可用。
- S010（2026-11-05）在客体上注册了测试同步根、创建了目录 + 12,345 B + 1 MiB 三个真实占位符、把 48 B 的 `FileIdentity` 交给系统保存，并按“只删自己的占位”完成清理与注销；`run-cfapi-prototype.sh` 退出 0、两端 SHA-256 一致（见 §16.3）。结束后已清理 `C:\vaultsyncd-test` 里的占位、注销同步根（`inspect` 退出码 3）并干净关机，没有注册残留。内容水合（S011）与释放/写保护（S012）仍未实现。
- S011（2026-11-05）在客体上实现了真正的按需内容供给：`read` 读到的字节与 SHA-256 与宿主固定内容版本一致（12,345 B / 1 MiB / 16 MiB 三个条目，含 1 MiB 范围水合），离线读未水合条目以 `ERROR_CLOUD_FILE_NETWORK_UNAVAILABLE`(388) 在 1 ms 内明确失败、已水合条目离线可读且不再请求内容，慢源超时有界失败，传输中断开同步根后回填循环在 3/16 段停下并有界返回；`run-cfapi-hydration.sh` 退出 0（46 项 `check=pass`、0 fail，两端 4 项哈希 `MATCH`），S010 的 `run-cfapi-prototype.sh` 仍退出 0（21 项 pass）。`CANCEL_FETCH_DATA` 在该 build 上未观察到（见 §17.4）。结束后已清理探针目录、注销同步根并干净关机。
- `provision.ps1` 在已安装客体上整份重跑过一次（10:29:21Z—10:29:35Z）：所有步骤 `BEGIN/END` 成对、无 `FAILED`，依赖介质的四步输出 `SKIP: ... (expected after installation)`；控制台代码页首登实测为 437（`Active codepage: 437`），因此脚本内的错误文本统一用英文，避免转写日志出现乱码。
- S012（2026-11-05）在客体上实现了释放与写保护判定：干净样本 `CfSetInSyncState`+`CfDehydratePlaceholder` 全 0x0、`on_disk_after=0`，新进程复核仍脱水、再读哈希不变（`fetch_requests=1`）；原地写、映射写（关闭后与仍打开）、独占句柄、共享锁五类场景都以 `dirty`/`busy` 拒绝释放并保留内容与哈希，句柄消失后同一内容可正常释放；`run-cfapi-release.sh` 退出 0（74 项 `check=pass`、0 fail、1 `blocked_item`），S010/S011 的脚本仍退出 0（21/46 项 pass）。**rename-over 保存列为阻塞**：同步根内建不了临时文件，同步根外改名后目标名字无法按路径打开（`0x801F0005`），见 §18.6。结束后已清理两个同步根与探针目录并干净关机。

## 19.1 运行态保持约定（2026-11-06 用户决定）

- **规则**：卡验收完成、甚至整批任务结束后，测试用 Windows 客体**不关闭**，qemu/swtpm 与 SSH(`127.0.0.1:2222`)、SPICE(`127.0.0.1:5930`) 保持在线，供下一张 Windows 卡直接复用，避免重复启动与状态重建。
- **允许关闭的唯一场景**：用户明确要求关机；需要恢复到 §11 的基线归档；主机维护/重建；或客体不可恢复。上述场景都要在 `agent/tasks/Sxxx.md` 记录关闭原因与再次启动方法。
- **收尾检查从“已关机”改为“在线且可用”**：轮次结束前用 `bootstrap-host.sh status` 与 `ssh ... 'cmd /c ver'` 确认客体运行可连；探针目录与同步根仍要按各卡要求清理/注销，不留测试残留。
- **安全收敛**：因为长期在线，`2222` 的 `0.0.0.0` 绑定（见 §9）不能靠关机收敛，必须依赖客体侧密钥独占认证（`PasswordAuthentication no`）与随机口令；不要在客体上开放其他入站端口，也不要为此关闭防火墙。
- **当前状态**：S013—S050 是 Linux 侧任务，不使用客体，因此本次没有启动它；客体仍为 2026-11-05 收尾时的干净关机状态。一旦后续 Windows 卡（S065 起）启动客体，就按本节保持运行，不再在卡结束时执行 `stop`。

## 20. 三客户端基础互通（S074）

### 20.1 拓扑与资源

- 仓库只有**一台** Windows 11 客体（第二台按计划属 S107）。S074 的三个“客户端”是同一实机上的三个隔离身份/环境：独立 DeviceId、状态目录与 `client.sqlite`、同步根、缓存和 `vaultsyncd-agent.exe` 进程。产品不变量“每个 Windows 用户会话/状态库只允许一个 agent”使三个 agent **顺序**运行；报告显式写 `physical_windows_guests=1`、`concurrent_agent_processes=1`，不把多进程冒充独立实机。
- 每个客户端在启动前用 `--login-stdin` 登录（同一测试账号、同一 Credential Manager blob），运行配置使用测试 TLS 入口 `https://127.0.0.1:<临时端口>` 与 `server.ca_file=<run>/tls/ca.pem`；不使用 `development_http`。
- 宿主真实服务端由 `vaultsyncd-server-tests` 的 `e2e-native-server` example 启动：隔离 btrfs Root、按 `tests/scenarios/three-clients.json` 的 `server_seed` 播种非空内容。客体经 **本次 run 私有**的 SSH 回环隧道 `127.0.0.1:18770` 访问宿主服务端；测试 TLS 入口在客体上终结 TLS，客户端仍做完整证书校验。
- 每个客户端环境在 `C:/vaultsyncd-lab/s074-driver-<pid>/run-e2e-<pid>/clients/{alpha,bravo,charlie}/` 下（`root`、`cache`、`state`）。成功时编排器注销 CFAPI 根并删除这些目录，只保留日志/报告；失败时保留客体目录供诊断，宿主记录警告。

### 20.2 入口与命令

宿主在默认 devShell 先构建真实服务端 fixture，再在 Windows devShell 运行：

```bash
cd /wrd/prj/vaultsyncd
nix develop --command cargo build --locked -p vaultsyncd-server-tests --example e2e-native-server
VAULTSYNCD_WINDOWS_CACHE=/wrd/prj/vaultsyncd/.cache/windows \
  nix develop .#windows --command bash tests/platform/windows/run-native-e2e.sh
```

脚本执行顺序：启动 fixture 与反向隧道 → `cargo xwin build` 产品 `vaultsyncd-agent.exe` 与 `vaultsyncd-e2e.exe` → 校验宿主/客体 SHA-256 → 在交互用户会话的 scheduled task 中运行编排器（CFAPI/WinCred/named pipe 需要真实会话）→ 取回 `report.json`、代理事件与 agent 日志到 `artifacts/S074/e2e-<timestamp>-<pid>/` → 用 `final_entries` 复核 btrfs Root 工作目录字节 → 只删除本 run 的 `s074-driver-*`。客体保持在线。

### 20.3 预期证据

- 日志依次出现 `block_done` T01/T02/T09/T11/T03，`every_input_content_found inputs=<n>`，`clients_converged`，`server_work_matches_committed_namespace entries=<n>`，`s074_native_e2e`，退出码 0。
- `report.json` 含：场景 commit 与工作树是否脏、Windows/服务端/agent 版本、三端 DeviceId/Binding、拓扑、每 block 证据、输入内容覆盖（当前/冲突/历史/回收站来源）、收敛计数、代理操作事件（含两次响应丢失与对应查询/重试）和 `final_entries`。
- `tests/scenarios/README.md` 说明场景 JSON 形状与约束；新数据不改编排器。

### 20.4 当前限制

- 不是三台并发实机：顺序 agent 只验证跨客户端最终收敛与冲突保留，不验证同时在线竞争；并发与事件异常/旧设备矩阵属 S102。
- 生产 HTTPS 反向代理部署属 S097；本卡使用测试 TLS 入口 + 认证 SSH 回环隧道，证书、主机名与校验是真实的，但不是生产代理配置。
- `kill_boundaries`（S052 子进程强杀恢复）在 S075 加入管理 socket 后存在基线已有的 `AdminSocket` 残留 socket 导致 `AddrInUse` 失败；已在 S074 交接记录为既有问题，与本卡改动无关。
