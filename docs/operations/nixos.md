# 本机 NixOS 隔离试用实例

## 实际入口和来源（S096，2026-11-07）

主机 `wks-lab`，配置入口 **`/etc/nixos#wks-lab`**，通过既有 `flake-parts/core.nix` 自动装配 `hosts/wks-lab/system.nix`。项目 flake 只提供包与模块，不能拿它代替主机 flake 执行 rebuild。

主机新增文件的本地提交 `ba1837aeb2a1905e6bc406adfdd6602ca450e6e1`，其中固定：

```nix
builtins.getFlake "git+file:///wrd/prj/vaultsyncd?rev=3640f3a3f72e09ba1302ffd19459266c2c7a23b6"
```

这是永久主仓库中的已提交来源，包含 S091 的 capabilities 生产路由和默认协商链；删除任务 worktree 不影响重建。包为 `/nix/store/fnc7wrxq8y48gza4i52l598k1k12slma-vaultsyncd-0.1.0`，实际 Rust 组件 `rzjzvi87mzwl66l1yvl73f32clw8682d-vaultsyncd-rust-0.1.0`。其 Nix manifest 的 commit/NAR/锁摘要与组件身份都来自同一固定 flake。新包 sandbox 构建并运行包内64项测试通过，线上 `/v1/capabilities` 真实返回 protocol1.0 与功能集合；不是旧S094包。

主机在本卡开始时已有一份未提交的 `flake.lock`，其内容恰好就是运行中 generation237 的依赖。本卡逐字节保留，未更新、暂存或提交：SHA256 `1ee4edafe554a98ef13c27f52d215592e9e32c52f10a0f95217577a967cbb095`。因此 **主机 Git commit 本身并不包含这份既有锁变更**。准确配置证据（原锁、前后diff、元数据、日志）保留在：

`/data/vaultsyncd-deploy/S096-20261107T010323-3856550/`

包含该锁和新主机文件的完整配置源是 `/nix/store/yybg4ycnfyi1kvpnk9773yhf9m4mjzgp-source`，NAR `sha256-THW01vsoYoZLR7KnXGOhY71cst1c7HVBHtYyJizc1cM=`；该目录与旧 system 均有上述证据目录下的显式 GC root。保存源的纯求值也核对同一 system 输出，避免把“host commit+不同锁”宣称为同一部署。

## 隔离存储和凭据

| 项目 | 实际值 |
| --- | --- |
| 服务 | `vaultsyncd.service`，enabled/active |
| HTTP | **`127.0.0.1:18721`**，仅本机试用；未开放公网入口 |
| ServerId | `9e9d06c5d0d371a43c2cb4043de02cd8`，回滚恢复与重启后均相同 |
| 样本RootId | `58e698e16616ff0162055df963f9c1fa` |
| 服务账号 | `vaultsyncd-trial`，UID/GID985，非root、零capabilities、NoNewPrivileges |
| 新建普通镜像 | `/data/vaultsyncd-trial-s096/volume.img`，2GiB稀疏文件，root0600 |
| 独立 btrfs 挂载 | `/data/vaultsyncd-trial-s096/mnt`，fstab显式loop/nofail/user_subvol_rm_allowed/noatime，依赖 `/data` |
| 内部状态 | `.../mnt/state`，服务UID0700，包含DB、对象、staging、admin/socket和backups |
| 唯一允许Root基目录 | `.../mnt/samples`，仅可丢弃 `keep.txt` 与 `s096-test.txt`/`s096-switch.txt` |
| owner初始密码文件 | `/var/lib/vaultsyncd-trial-credentials/owner.password`，root0600，父目录root0700 |

镜像由本卡确认路径不存在后，以 `O_EXCL` 创建普通文件，再显式执行一次 `mkfs.btrfs`；未格式化既有设备。NixOS 声明只负责挂载，**不会创建或格式化镜像**。`/data` 原有挂载和选项完全不变，只有专用镜像允许普通UID删除备份子卷。它是容量受限的试用布局，不代表正式生产磁盘规划；已有挂载点为空不能代替验证实际镜像已挂载。

状态和用户Root分开，Root由用户拥有并逐项ACL授权，既有资料目录没有被递归chown/chmod。服务没有sudo/CAP_SYS_ADMIN；备份准备区的子卷、只读快照和删除均由服务UID通过正常btrfs权限完成。Root内不允许嵌套挂载/子卷，原身份/CoW/扫描准入仍由产品执行。

首次初始化在服务离线、空状态上，以服务账号执行包内 `vaultsyncd-admin init --state-dir <state>`，密码经匿名stdin管道输入；没有密码参数、Nix secret选项或默认密码。本卡随机生成的密码只在受限运行时文件与初始化/登录进程内存出现；没有写进store、项目Git或证据日志。该文件保存的是**初始**凭据，之后改密需由运维同步管理；服务启动不读取它、不自动重置owner。不要将其复制到artifacts或Git。

## 已执行的发布与验收顺序

先保存旧配置diff、锁、generation列表、SSH banner与既有服务PID/InvocationID、HTTP状态，并为旧 system 建GCroot。未改动配置求值输出与运行 system 完全相同后，构建新源项目包、准备专用卷/样本/owner，新增主机文件并 `git add`（否则flake看不到新文件）。

```sh
# 从上述证据目录运行，result也是该目录内的构建链接。
sudo nixos-rebuild build --flake /etc/nixos#wks-lab --no-update-lock-file --no-write-lock-file
sudo nixos-rebuild dry-activate --store-path /nix/store/2qayb90x692izny4wi4wrg6l0dg8dhsq-nixos-system-wks-lab-26.11.20261102.7356cc7
sudo nixos-rebuild test --store-path /nix/store/2qayb90x692izny4wi4wrg6l0dg8dhsq-nixos-system-wks-lab-26.11.20261102.7356cc7
```

`test` **立即激活**，不更改下次启动默认generation，也没有定时自动回滚。dry-activate确认SSH配置逐字节相同，仅新增项目单元，以及新增账号导致的polkit重启/dbus reload，没有网络/既有应用/VM重启。以下验收脚本通过真实HTTP和同UID IPC操作唯一试用Root，密码只从root受限文件读入内存：

```sh
nix develop --command python3 deploy/nixos/verify-instance.py /nix/store/fnc7wrxq8y48gza4i52l598k1k12slma-vaultsyncd-0.1.0 /data/vaultsyncd-deploy/S096-20261107T010323-3856550 test
```

完成本地观察 → 上传分块与finalize → committed → 固定版本下载；重启服务后复核ServerId、内容字节、版本与同OpId幂等重放，以及无特权备份prepare/verify-source/release。`pending`不算成功。每个phase只创建一次带固定测试名的新样本；存在则拒绝，日常健康检查不反复运行该写入验收。

随后实际回退旧system做 `test`：试用服务inactive、18721拒绝连接、镜像卸载；SSH及10个既有关键服务的PID/InvocationID保持。再次对新system执行 `test`，`verify-instance.py ... recovery` 核对重挂载后原ServerId/Root/固定版本SHA256与持久Op状态，才执行：

```sh
sudo nixos-rebuild switch --store-path /nix/store/2qayb90x692izny4wi4wrg6l0dg8dhsq-nixos-system-wks-lab-26.11.20261102.7356cc7
```

当前默认与运行中均为 **generation238**，输出 `2qayb90x...7356cc7`。`verify-instance.py ... switch` 新一轮完整样本闭环通过，包括服务重启；SSH与既有sshd/caddy/postgresql/memtrack-web/grpc/receiptd/labtrack/thesisd/dae/networkd进程未变，3200/3300/15312均200。没有重启宿主或Windows客体。包运行闭包无Node/pnpm/TypeScript；日志及生成TOML已实际核对不含初始密码。

## 健康、更新与恢复

```sh
curl --fail http://127.0.0.1:18721/v1/health
curl --fail http://127.0.0.1:18721/v1/capabilities
sudo -u vaultsyncd-trial /nix/store/fnc7wrxq8y48gza4i52l598k1k12slma-vaultsyncd-0.1.0/bin/vaultsyncd-admin status --socket /data/vaultsyncd-trial-s096/mnt/state/admin/socket --json
sudo journalctl -u vaultsyncd.service --no-pager
```

更新必须先在项目提交并构建新包，再仅更新主机文件里的项目commit，重走build/test/健康/switch；不顺手更新主机nixpkgs，不引用 `.worktrees`。迁移前按备份/离线恢复规程留一致包；不要直接复制在线SQLite或把宿主对loop镜像的普通快照冒充一致备份。不直接写在线数据库。

本卡**实际验证过**的旧system恢复命令：

```sh
sudo nixos-rebuild test --store-path /nix/store/h2wjnyc8qi268a1z6lylxmqld05y0a8n-nixos-system-wks-lab-26.11.20261102.7356cc7
```

这是generation237的已知健康配置；旧system及准确新配置源的GCroot都保留。若健康后决定让旧配置成为持久默认，再对同一旧store路径执行 `nixos-rebuild switch --store-path ...`（该持久旧配置switch没有在本卡执行，实际演练为test回退与返回新配置）。磁盘镜像、状态和凭据不随回退删除。主机源码还包含启用声明，恢复后需明确回退/禁用 `ba1837a` 中的该声明再做普通flake rebuild；否则普通rebuild会再次启用试用实例。

SSH一直运行，回退不依赖项目服务或密码。日志/原diff/完整配置源/新旧system路径在证据目录，`rollback.sh`也保留已知旧路径。TLS、同源代理与对外入口由S097单独验收；当前HTTP例外严格限定回环试用端口。
