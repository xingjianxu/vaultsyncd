# 开发与服务验证主机环境基线

采集时间：2026-11-05T15:48—15:50 +08:00
采集位置：`/wrd/prj/vaultsyncd/.worktrees/s001-s007`（分支 `chore/S001-S007-bootstrap`，基线 `4a2c7e9`）
采集方式：仅只读命令；未执行 rebuild、未挂载/格式化磁盘、未安装软件包、未改动系统配置，未输出密码、token、私钥或凭据内容。
证据性质：下文“实测”仅指本机本次观察到的结果，不代表其他主机、其他架构或 design 的完整支持矩阵已经验证。

## 1. 主机与工具链

| 项目 | 命令 | 实测结果 | 退出码 |
| --- | --- | --- | --- |
| 当前工作目录 | `pwd` | `/wrd/prj/vaultsyncd/.worktrees/s001-s007` | 0 |
| 发行版 | `nixos-version` | `26.11.20261102.7356cc7 (Zokor)` | 0 |
| os-release | `head -n 6 /etc/os-release` | NixOS 26.11 (Zokor) | 0 |
| 内核 | `uname -a` | `Linux wks-lab 7.3.0 #1-NixOS SMP PREEMPT_DYNAMIC ... x86_64 GNU/Linux` | 0 |
| 机器架构 | `uname -m` | `x86_64` | 0 |
| Nix | `nix --version` | `nix (Nix) 2.34.9` | 0 |
| Nix 实验特性 | `nix config show experimental-features` | `fetch-tree flakes nix-command` | 0 |
| 主机名 | `hostname` | `wks-lab` | 0 |
| CPU | `grep -m1 'model name' /proc/cpuinfo` | Intel Xeon Gold 6430；`nproc` = 128 | 0 |
| 内存 | `free -h` / `/proc/meminfo` | MemTotal 503 GiB；Swap 为 zram0 251.8 GiB，已用 0 | 0 |
| 当前用户 | `id` | `uid=1000(xingjian) gid=100(users)`；附加组含 `wheel`、`kvm`(302)、`video`、`render` 等 | 0 |
| 非交互 sudo | `sudo -n true` | 退出码 0（本次命令可无密码使用 sudo；未做任何写操作） | 0 |
| 虚拟化类型 | `systemd-detect-virt` | `none`（物理机） | 1 |
| 内核 btrfs 支持 | `grep -w btrfs /proc/filesystems` | 存在 `btrfs` | 0 |

说明：`x86_64` 是当前开发/服务验证主机的事实；本机没有 ARM64 运行证据。

## 2. 磁盘、挂载与空闲空间

### 2.1 块设备与文件系统

`lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINTS` 实测：

| 设备/分区 | 大小 | 类型 | 文件系统 | 主要挂载点 |
| --- | --- | --- | --- | --- |
| `/dev/sda` | 447.1G | disk | — | — |
| `/dev/sda1` | 512M | part | vfat | `/boot` |
| `/dev/sda2` | 446.6G | part | btrfs | `/`、`/home`、`/nix`、`/var/log`、`/.snapshots` |
| `/dev/sdb` | 447.1G | disk | — | — |
| `/dev/sdb1` | 447.1G | part | btrfs | `/wrd`（源码所在） |
| `/dev/sdc` | 21.8T | disk | — | — |
| `/dev/sdc1` | 21.8T | part | btrfs | `/data` |
| `/dev/zram0` | 251.8G | disk | swap | SWAP |

`findmnt` 实测关键挂载与选项：

| 挂载点 | 源/子卷 | 类型 | 关键选项 |
| --- | --- | --- | --- |
| `/` | `/dev/sda2[/@]` | btrfs | `rw,noatime,compress=zstd:1,ssd,discard=async,space_cache=v2,subvolid=256` |
| `/home` | `/dev/sda2[/@home]` | btrfs | 同上，`subvolid=257` |
| `/nix` | `/dev/sda2[/@nix]` | btrfs | 同上，`subvolid=259`；`/nix/store` 为只读挂载 |
| `/var/log` | `/dev/sda2[/@var_log]` | btrfs | 同上 |
| `/wrd` | `/dev/sdb1[/@wrd]` | btrfs | `rw,noatime,compress=zstd:1,ssd,discard=async,space_cache=v2,subvolid=256` |
| `/data` | `/dev/sdc1[/@data]` | btrfs | `rw,noatime,compress=zstd:1,space_cache=v2,subvolid=256` |
| `/boot` | `/dev/sda1` | vfat | 仅引导用途，不作服务端存储后端 |

仓库 `/wrd/prj/vaultsyncd` 位于 `/dev/sdb1` 的 btrfs 子卷 `/@wrd`；实测 `findmnt -T /wrd/prj/vaultsyncd` 结果为 btrfs。

### 2.2 空闲空间

`df -hT` 实测：

| 挂载点 | 类型 | 总量 | 已用 | 可用 | 使用率 |
| --- | --- | --- | --- | --- | --- |
| `/` | btrfs | 447G | 101G | 344G | 23% |
| `/home` | btrfs | 447G | 101G | 344G | 23%（与 `/` 共享 sda2） |
| `/wrd` | btrfs | 448G | 245G | 197G | 56% |
| `/data` | btrfs | 22T | 197G | 22T | 1% |
| `/boot` | vfat | 511M | 86M | 426M | 17% |

`df -B1 --output=target,size,used,avail` 实测可用字节：`/wrd` 211,436,646,400；`/data` 23,790,515,478,528；`/` 369,009,029,120。
注意：`/`、`/home`、`/nix`、`/var/log` 共享 `/dev/sda2` 的容量，不能把各自 `Avail` 相加。

## 3. btrfs 工具与测试卷状态

| 项目 | 命令 | 实测结果 | 退出码 |
| --- | --- | --- | --- |
| btrfs-progs | `btrfs --version` | `btrfs-progs v7.1` | 0 |
| mkfs 工具 | `mkfs.btrfs --version` | `mkfs.btrfs, part of btrfs-progs v7.1` | 0 |
| 文件系统列表 | `sudo -n btrfs filesystem show` | 三个 btrfs 设备：`/dev/sda2` 446.63GiB、`/dev/sdb1` 447.13GiB、`/dev/sdc1` 21.83TiB；均为单设备 | 0 |
| 子卷列表 | `sudo -n btrfs subvolume list /wrd` | `@wrd`(256)、`@wrd_snapshots`(257) 及既有快照子卷 | 0 |
| 子卷列表 | `sudo -n btrfs subvolume list /data` | `@data`(256)、`@data_snapshots`(257) 及既有快照子卷 | 0 |
| 空间分布 | `sudo -n btrfs filesystem df /wrd` | Data single 251.00GiB（used 238.20GiB），Metadata DUP 6.00GiB | 0 |
| 空间分布 | `sudo -n btrfs filesystem df /data` | Data single 197.01GiB（used 195.93GiB），Metadata DUP 1.00GiB | 0 |

说明与限制：

- 以普通用户执行 `btrfs filesystem show` 时设备路径显示 `MISSING`；以 `sudo -n` 只读复核后得到正确设备路径。工具本身可用。
- 现有 `/wrd`、`/data` 均为带快照子卷的既有数据区；本次未创建、未删除、未格式化任何测试卷。
- 尚无本项目专用 btrfs 测试卷/目录：`/var/lib/vaultsyncd-dev`、`/var/lib/vaultsyncd`、`/srv/vaultsyncd-test` 均不存在（`ls -ld` 退出码 2）。独立测试位置由 S007 准备。
- `/dev/loop-control` 为 `root:disk` `660`，当前用户不在 `disk` 组；若 S007 采用 loop 测试卷，需使用已确认可用的 `sudo -n` 或另行配置权限。本轮不创建 loop 设备。
- reflink/FICLONE 与快照隔离能力尚未实测，留给存储准入与备份相关卡片验证；不得把“文件系统为 btrfs”当成 reflink 已验收。

## 4. 系统配置入口

| 项目 | 命令/路径 | 实测结果 |
| --- | --- | --- |
| 主机配置目录 | `ls -la /etc/nixos` | 存在，且为独立 Git 仓库；含 `flake.nix`、`flake.lock`、`flake-parts/`、`hosts/`、`features/`、`modules/`、`users/`、`secrets.nix` 等 |
| 主机 flake | `/etc/nixos/flake.nix` | flake-parts 配置，`systems = [ "x86_64-linux" ]`，导入 `./flake-parts/core.nix`；`nixosConfigurations` 不在该文件直接声明，由 core.nix 生成 |
| 主机枚举 | `rg -n -e nixosConfigurations -e 'hosts/' /etc/nixos/flake-parts/core.nix` | `hostDir = ../hosts/${hostName}`；`flake.nixosConfigurations = builtins.listToAttrs (...)`（第 164 行）自动发现 `hosts/` 下主机 |
| 当前主机 | `hostname` + `/etc/nixos/hosts` | 当前为 `wks-lab`；`hosts/wks-lab/` 含 `default.nix`、`hardware-configuration.nix` 及主机私有 `.age` 文件 |
| 当前系统 | `ls -ld /run/current-system` | 指向 `/nix/store/1qjd95z5lhmbmq33lcfib086frzavj97-nixos-system-wks-lab-26.11.20261102.7356cc7` |
| 系统版本 | `cat /run/current-system/nixos-version` | `26.11.20261102.7356cc7` |
| 系统 generation | `sudo -n nix-env -p /nix/var/nix/profiles/system --list-generations` | 最新为 234（2026-11-05 11:38:00，current） |
| 主机配置 Git 状态 | `git -C /etc/nixos status --porcelain` | 存在用户既有修改 ` M flake.lock`；本轮未改动、未暂存、未提交主机配置 |
| 主机配置提交 | `git -C /etc/nixos log -1 --format='%H %s'` | `2e58ff14c722b2c2135acced531c6ccf630224ad chore(hosts): 更新 thesisd 固定提交到 9380be3` |

未来如需本项目服务/NixOS 模块，应按实施手册 §2.5 从上述现有入口执行 `nixos-rebuild build/test/switch`；不得把 vaultsyncd 项目 flake 当作主机 flake。本轮仅核对入口，未执行 rebuild。

## 5. 已占用监听端口

`ss -ltn`（用户态，退出码 0）与 `sudo -n ss -ltnp`（只读，补充进程归属，退出码 0）实测。局域网/公网监听与仅本机监听均列出：

| 端口 | 监听地址 | 进程/服务 |
| --- | --- | --- |
| 22 | `0.0.0.0`、`[::]` | `sshd` |
| 53 | `127.0.0.53%lo`、`127.0.0.54` | `systemd-resolved` |
| 80、443 | `*` | `caddy` |
| 139、445 | `0.0.0.0`、`[::]` | `smbd` |
| 2019 | `127.0.0.1` | `caddy`（管理接口） |
| 1688 | `0.0.0.0` | `vlmcsd` |
| 3200、3300 | `*` | `next-server` |
| 5355 | `0.0.0.0`、`[::]` | `systemd-resolved` |
| 5357 | 多个地址（`10.200.0.110`、`115.24.115.250`、链路本地、`[::]`） | `wsdd` |
| 5432 | `0.0.0.0`、`[::]` | PostgreSQL |
| 6464、6465 | `127.0.0.1` | `xray` |
| 11010、15888、33657 | `0.0.0.0` / `*` | `easytier-core` |
| 11434 | `127.0.0.1` | `ollama` |
| 15100、15110、15310 | `127.0.0.1` | `process-compose` |
| 15101、15102、15111、15311、15312 | `127.0.0.1` 或 `*` | `next-server` |
| 39309 | `127.0.0.1` | `code-server` |
| 50052 | `0.0.0.0` | `bun` |

未占用/可用端口未逐一扫描；后续选择 vaultsyncd 开发服务端口时应避开上表，并再次用 `ss -ltn` 核对。`ss -lun` 有 128 行输出，本轮未逐项归类（UDP 不是首发 HTTP 服务的直接冲突面）。

## 6. KVM 与 Windows 客体资源

| 项目 | 命令 | 实测结果 | 退出码 |
| --- | --- | --- | --- |
| KVM 设备 | `ls -l /dev/kvm` | `crw-rw-rw- 1 root kvm 10, 232` | 0 |
| 当前用户 KVM 访问 | `test -r /dev/kvm -a -w /dev/kvm` | 可读写（退出码 0） | 0 |
| kvm 组 | `getent group kvm` | `kvm:x:302:xingjian` | 0 |
| KVM 模块 | `lsmod` | 输出含 `kvm_intel`、`kvm` 行 | 0 |
| QEMU | `command -v qemu-system-x86_64` | 未找到 | 1 |
| Quickemu/Quickget | `command -v quickemu quickget` | 均未安装 | 1 |
| libvirt | `command -v virsh`；`ls /var/lib/libvirt` | `virsh` 未找到；目录不存在 | 1 / 2 |
| 既有 Windows 客体文件 | `find /wrd /data /home/xingjian /var/lib/libvirt -maxdepth 3 \( -iname '*.qcow2' -o -iname '*.vmdk' -o -iname '*.vdi' -o -iname 'windows*.conf' \)` | 未发现 | 1 |
| 物理机标志 | `systemd-detect-virt` | `none` | 1 |

结论：本机具备 Intel VT/KVM 与足够内存/磁盘余量，但尚未安装 QEMU/Quickemu，也没有现成 Windows 客体。Windows 原生 CFAPI 验收所需的客体、工具链与网络连接方式尚未建立，属于未核验项，由 S008/S009 及后续 Windows 卡完成；在此之前不得声称 Windows 能力已验证。

## 7. Git 与工作树基线

| 项目 | 命令 | 实测结果 | 退出码 |
| --- | --- | --- | --- |
| 当前 HEAD | `git rev-parse HEAD` | `4a2c7e9b2d7af954d9b5ba445cd4ffb671dc689a` | 0 |
| 分支 | `git rev-parse --abbrev-ref HEAD` | `chore/S001-S007-bootstrap` | 0 |
| 工作树 | `git worktree list` | 主工作区 `/wrd/prj/vaultsyncd`（master，`4a2c7e9`）；本任务工作树 `/wrd/prj/vaultsyncd/.worktrees/s001-s007`（`chore/S001-S007-bootstrap`，`4a2c7e9`） | 0 |
| 工作树隔离 | `cat .git` | `gitdir: /wrd/prj/vaultsyncd/.git/worktrees/s001-s007`（linked worktree） | 0 |
| 工作树忽略 | `git -C /wrd/prj/vaultsyncd check-ignore -v .worktrees/s001-s007` | 匹配 `.gitignore:2:/.worktrees/` | 0 |
| 工作区状态 | `git status --porcelain` | 采集时为空 | 0 |
| 提交身份 | `git config --get user.name` / `user.email` / `commit.gpgsign` | 姓名与邮箱均已配置（具体值不写入环境文档）；`commit.gpgsign` 未设置 | 0 / 0 / 1 |

## 8. 与 design 声明支持矩阵的核对

### 8.1 本机实测事实（可验证部分）

- 本机为 NixOS 26.11（Zokor），x86_64，内核 7.3.0；Nix 2.34.9，flake 可用。
- 源码工作区、`/home`、`/nix`、`/var/log`、`/wrd`、`/data` 均为本地 btrfs 子卷；当前挂载表中没有 ext4/XFS/ZFS/overlay 数据挂载。
- KVM 可用且当前用户在 `kvm` 组；未安装 Quickemu/QEMU，未创建 Windows 客体。
- 本机可用空间充足：`/wrd` 197G、`/data` 22T；但尚无本项目独立测试卷。
- 已有服务占用 22/53/80/139/443/445/1688/2019/3200/3300/5355/5357/5432/6464/6465/11010/11434/15100/15101/15102/15110/15111/15310/15311/15312/15888/33657/39309/50052 等端口；后续服务端口需避让。

### 8.2 design 已声明的支持矩阵（目标，不是本机实测结论）

- C35：Windows 11 x86_64 客户端；Linux x86_64/ARM64 服务端；NixOS 为主要开发与同机服务验证环境；Ubuntu 24.04 LTS 为发布兼容验证环境；Linux 客户端后置。
- C39：首发强制本机块设备上的 btrfs（工作目录、SQLite、对象、历史、暂存、备份准备目录），允许后端合规的普通 bind mount；不支持远程 SMB/NFS、FUSE、占位符和按需回源目录。
- design §5.11 与 ADR-009 同样写明 NixOS 同机开发/服务、Ubuntu 24.04 与 Linux ARM64 发布兼容验证。

### 8.3 核对结论

- C35 已包含“NixOS 为主要开发与同机服务验证环境”和“Ubuntu 24.04 LTS 为发布兼容验证环境”，与本次实测的开发主机身份一致；**无需修改 design**。
- C39 已与实测的本地 btrfs 存储事实一致；未发现需要改写的缺失项；**无需修改 design**。
- 仍需区分的未核验项：Ubuntu 24.04 发布兼容环境、Linux ARM64 服务端、Windows 11 原生 CFAPI、独立 btrfs 测试卷、reflink/快照隔离、10 万文件/1 TiB 规模。它们属于后续卡片，不得用本机 x86_64 结果外推为“全部架构/平台已支持”。

## 9. 未核验项与后续入口

| 未核验项 | 现状 | 后续卡片 |
| --- | --- | --- |
| Ubuntu 24.04 LTS 发布兼容 | 本机无该环境 | S098/S101 及发布验证 |
| Linux ARM64 服务端 | 本机仅 x86_64 | 发布兼容验证 |
| Windows 11 x86_64 原生 CFAPI | 无 QEMU/Quickemu、无客体 | S008—S012 |
| 独立 btrfs 测试卷 | 未创建，仅有既有 `/wrd`、`/data` | S007 |
| reflink/FICLONE 与只读快照隔离 | 未实测 | S007/S013/备份卡 |
| 验收规模（10 万文件、1 TiB、50 GiB 单文件、5 客户端） | 未实测 | S106/S107 |
| 服务端口与部署地址 | 尚未选型；本机多个端口已占用 | S041/S096/S097 |

本文件只记录 S001 的只读基线；不替代实施手册 §2.4 的环境/状态隔离规则，也不把设计目标当作已达成事实。
