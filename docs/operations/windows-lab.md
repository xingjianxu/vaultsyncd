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
- **暴露面**：quickemu 4.9.9 只能生成 `tcp::<port>` 形式的转发，即绑定 `0.0.0.0:2222`；宿主自身在 `ens5f0` 上有公网地址且主机防火墙放行全部 TCP/UDP，因此该端口在客体运行期间可从宿主所在网络访问。收敛手段只能是客体侧认证（密钥登录 + 随机口令）与及时关机；启动/关闭用 `bootstrap-host.sh start/stop`，不使用客体时保持关机。
- 客体访问外网：slirp NAT 正常（实测与 Microsoft 更新端点建立连接）；`Delivery Optimization` 的 P2P 已由 `DODownloadMode=0` 策略关闭，避免与内网其他主机建 7680 连接。
- 客体网卡的网络类别实测是 `Public`（`Get-NetConnectionProfile`）。入站 22 的防火墙规则必须是任意 profile，否则宿主看到的是 `SYN_SENT`/banner 超时；这一点在 §6 与 §14 都有记录。

## 10. 干净关机

```bash
nix develop .#quickemu --command tools/windows-lab/bootstrap-host.sh stop
```

内部顺序：`ssh ... 'cmd /c shutdown /s /t 0'` → 轮询 `windows-11/windows-11.pid` 直到 qemu 进程消失（默认 180 s）→ 检查 `ss -ltn` 不再有 `2222`/`5930`。`shutdown` 命令返回非零也可能是连接先断开，判断以 qemu 是否退出为准。**不要留下孤儿 qemu/swtpm**；若 qemu 未退出，先查 `windows-11/windows-11.log`，不要 `kill -9`。

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

## 12. 客体工具链与产物交付（ADR-013）

1. 宿主构建：在 NixOS 上交叉编译 Windows x64 目标（S009 固定工具链），前端在宿主构建；不把 `target/`、`node_modules/` 复制进客体。
2. 传输：`scp -P 2222 -i ... <产物> Quickemu@127.0.0.1:C:/vaultsyncd-lab/`，同时记录宿主 `sha256sum`。
3. 客体校验：`powershell -c "Get-FileHash C:\vaultsyncd-lab\<file> -Algorithm SHA256"`，与宿主哈希一致才算交付成功。
4. 客体只装运行所需：WebView2 运行时（Windows 11 自带）、必要 VC++ 运行库（如产物需要）；不装 Rust/VS/SDK/Node/pnpm。
5. 原生运行/CFAPI/GUI 验收在客体上进行；交叉编译成功不能替代客体执行证据。

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

## 16. 当前状态（2026-11-05 本轮结束）

- 客体**已干净关机**（`shutdown /s /t 0` 后 qemu 与 swtpm 退出，`2222`/`5930` 已关闭，无孤儿进程）。下次使用：`nix develop .#quickemu --command tools/windows-lab/bootstrap-host.sh start`，再 `wait-ssh`（实测从启动到 SSH 可用约 20 秒）。
- 系统：Windows 11 Enterprise Evaluation 26H2 / build 26300.9457，`C:` 为 NTFS（98.6 GiB，可用约 78.7 GiB），8 vCPU / 8 GiB / 100 GiB VirtIO GPT。
- 通道：SSH 密钥登录（`Quickemu@127.0.0.1:2222`，密钥独占认证）+ SPICE `127.0.0.1:5930` + WinRM 5985（按 §15 临时转发）。
- 基线归档：`/data/vaultsyncd-lab/win-lab-baseline-20261105.tar`（含 conf/qcow2/OVMF_VARS/TPM 状态/unattended.iso）。
- 客体未安装任何编译工具链；S009 之后的产物按 §12 传输并在客体原生执行。
- `provision.ps1` 在已安装客体上整份重跑过一次（10:29:21Z—10:29:35Z）：所有步骤 `BEGIN/END` 成对、无 `FAILED`，依赖介质的四步输出 `SKIP: ... (expected after installation)`；控制台代码页首登实测为 437（`Active codepage: 437`），因此脚本内的错误文本统一用英文，避免转写日志出现乱码。
