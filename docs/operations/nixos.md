# 本机 NixOS 服务部署

## S218 文件预览关闭动画修复（2026-11-13）

源 `a6a23dea700231c731bed8727491cbb603c8d6da` 已经pure build/dry/test/健康/switch部署generation291，包 `/nix/store/9l4my652gyr3bqlr6sq0z7nrsxhc1297-vaultsyncd-0.1.7`，system `9gshvkd3kwf5xb18wzjgr9wzqn5z20f0`，主机pin `6a90aa5c3d7b674e940b00c688509489a8392655` 已ff合main并清部署树。仅关闭生命周期修复，无schema迁移；主机锁、prod/dev原身份/账号/Vault/schema21与Office配置保持，dev同步同包。精确包52组、18退场/重开109帧及408次资产字节/TLS/CSP/匿名门禁通过。旧generation290 system/boot GC roots在 `/data/vaultsyncd-dev/S218-preview-exit-20261113/deploy/local-20261113T090049-2686437/` 保留，未实际回滚；交接与限制见 [S218](../../agent/tasks/S218.md)。

## S207 环境入口（2026-11-12）

本文描述 **prod**：`https://vsyncd.132140.xyz`，既有 NixOS 部署方式、状态与 Vault 路径保持。**dev** 与 Windows 测试机改用 `https://wlab.132140.xyz:15401`，临时后台服务由 `tools/dev-server.sh` 管理，数据在 `/data/vaultsyncd-dev/dev-server/`，不注册 NixOS unit。证书与操作见 [开发测试服务](dev-server.md)，实际验收见 [S207](../../agent/tasks/S207.md)。

当前开发路径以 S163 为准：所有开发测试目录只能位于 `/data/vaultsyncd-dev/`。本文件历史 temp/lab 路径中的 `/data/` 须替换为 `/data/vaultsyncd-dev/`；更早 build/deploy 先按 S161 映射到 temp 内，再替换此前缀。历史日志与验收记录保留原文，执行旧命令前须替换路径，详见 [S163](../../agent/tasks/S163.md)。

历史 `/data/vs-*` 目录已由 [S164](../../agent/tasks/S164.md) 归档至 `/data/vaultsyncd-dev/S164-archive-20261109/runs.tar.zst`，成员保留旧目录名；该目录的 README、SHA256SUMS 和 evidence/path-map.json 提供恢复步骤、摘要与路径映射。包括 S097 离线恢复镜像在内的原始数据长期保留，不能按缓存清理；历史命令不得重新建立 `/data` 顶层开发目录。

## S197 当前客户端主机属性版部署（generation275，2026-11-11）

固定源72c25ca，主机pin295ed47已ff合main；schema20→21，显式请求扩展保持旧严格DTO兼容。部署pure/dry/test/健康/身份/归属/switch及公网15资源、匿名门禁、DB完整性/FK通过，ServerId/Vault路径保持。迁移前schema20完整归档已verify，备份/旧system/boot GC roots在 `/data/vaultsyncd-dev/S197-client-hostname-20261111/` 长期保留；同盘归档不代表独立故障域。回退须整套配对恢复及新epoch，未执行生产回退。部署及客户端验证入口见[S197](../../agent/tasks/S197.md)，历史小节保留当时证据。

## S157 当前共用 Mantine 与精简 Vault 版部署（generation266，2026-11-09）

正式入口 **https://vsyncd.132140.xyz**。固定源`5a71062da284d4841ea6ca6ad690a5ba7d93c651`，上线S152共用Mantine及S154精简Vault工具栏；schema18、`/data/vaultsyncd/vaults`、账号与ServerId保持。运行/开机generation266，system `/nix/store/641j5vxmwbcarwgfgnvippxgmacsbf9j-nixos-system-wks-lab-26.11.20261102.7356cc7`，包`rr1zw3qmm2qbnbdhjcyz1g0flf1pd2a2`。精确主机配置`f7d19978171cb2311e3cb9c642d0db9f480518ea`（main合并`a02b61e`），其纯求值精确复现运行系统；main待激活依赖锁保持，不用main当前锁冒充运行配置。

一键pure构建含73项测试，dry/test/HTTPS与IPC/UID归属/switch通过。生产真实Chromium覆盖全量资产、登录门禁/自动连接/客户端刷新、共用48px顶栏/240px导航、Vault工具栏与两弹窗、历史/键盘、明暗320/1440及CSP/cookie；账号、1Vault/7条目/5版本/4对象、5工作文件和10服务/3应用保持。生产不注册样本或修改同步内容。

完整备份`/data/vaultsyncd-temp/S157-deploy-20261109/backups/predeploy.vsbk`及异盘`/wrd/prj/vaultsyncd/artifacts/S157/backups/`均verify，原始证据/截图/脚本/旧system GC roots在同run的evidence永久保留。无schema迁移；回退用旧generation265的`/nix/store/0jnii77r4410z7ffq2ak1s820b021iq4-nixos-system-wks-lab-26.11.20261102.7356cc7`，按S113先test与健康核对，再switch并同步pin至d4c5fe7；备份恢复仍遵守空目标及换epoch。未实际回退或重跑Windows/性能容量，不替代剩余首发门禁。详细交接见[S157](../../agent/tasks/S157.md)。

## S151 当前正式 Vault 基目录（generation264，2026-11-09）

用户授权将 `/data/vaultsyncd/roots` 改为 **`/data/vaultsyncd/vaults`**，实际采用停服同卷重命名，全部原数据保留；现有 Vault 为 `/data/vaultsyncd/vaults/documents`，旧 roots 不保留别名。此节覆盖历史部署记录的正式基目录，历史证据不改写。

主机运行及开机 generation264，精确配置提交 `c20199ecf2d2db6dfceb4fe352f897aaae1fbe3f`（main合并 `a3a62a9`），system `/nix/store/qdd7vqh5k24dwlrxwzcgsn6mp9f75k0y-nixos-system-wks-lab-26.11.20261102.7356cc7`。主机 main 尚未激活的依赖锁升级继续保留；重现本次运行使用 c20199e。服务包仍为 S149 的 d9cfee9/schema18，S150 功能未随本卡部署。

迁移前完成管理 IPC 的 prepare/verify-source/export/verify/release，停服并独占 `/var/lib/vaultsyncd/server.lock` 后 checkpoint，另备份整个状态和工作目录。因身份围栏包含 resolved_path，原位 rename 后同时更新 `vaults.absolute_path` 及 `vault_file_identity` 第一行；卷/子卷/inode/btime、VaultId/epoch/seq 和其余全部数据库值保持。两目录权限为 `xingjian:users 0700`，NixOS allowed_bases、ReadWritePaths 和 RequiresMountsFor 同步指向新路径。28表副本演练及正式对比、4工作文件/3对象摘要与 inode、线上健康/IPC/历史、10项其他服务身份及 pure build/dry/test/switch 均通过。

证据和备份根 `/data/vaultsyncd-temp/S151-vault-base-20261109T1850/`，`backups/pre-move.vsbk` 为完整已校验归档；`backups/state` 与 `backups/roots` 为停服副本；`evidence/migrate.py` 和 offline/online/rollback-rehearsal 结果记录本次操作，旧system保留 GC root。该脚本固定旧路径和空新目标，**不可在已迁移实例重跑**。

回退路径：先停服、独占相同状态锁并新建本次回退的完整备份；确认仍是同一目录身份且旧 roots 不存在，再将 vaults 原位改回 roots，并在当前数据库的一个事务内逆转上述两处路径前缀，保留迁移后产生的其他数据；检查完整性/FK、逐表仅路径变化、文件身份与哈希后，激活保留的旧system并核对健康。副本已实际验证逆向更新与原库28表完全一致，生产未执行回退。不能只切旧generation，也不要用旧数据库单文件覆盖当前状态；卷或目录身份已变化时，改按完整备份恢复流程处理并更换epoch。

当前正式入口为 **https://vsyncd.132140.xyz**，后端0.1.1、仅回环8721。S157已部署源`5a71062`，运行/开机generation266、schema18。服务及正式目录仍为`xingjian:users`，Vault基目录保持vaults；当前构建、备份与回退见上方S157，历史章节保留当时证据。

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
| 样本VaultId | `58e698e16616ff0162055df963f9c1fa` |
| 服务账号 | `vaultsyncd-trial`，UID/GID985，非vault、零capabilities、NoNewPrivileges |
| 新建普通镜像 | `/data/vaultsyncd-trial-s096/volume.img`，2GiB稀疏文件，vault0600 |
| 独立 btrfs 挂载 | `/data/vaultsyncd-trial-s096/mnt`，fstab显式loop/nofail/user_subvol_rm_allowed/noatime，依赖 `/data` |
| 内部状态 | `.../mnt/state`，服务UID0700，包含DB、对象、staging、admin/socket和backups |
| 唯一允许Vault基目录 | `.../mnt/samples`，仅可丢弃 `keep.txt` 与 `s096-test.txt`/`s096-switch.txt` |
| owner初始密码文件 | `/var/lib/vaultsyncd-trial-credentials/owner.password`，vault0600，父目录vault0700 |

镜像由本卡确认路径不存在后，以 `O_EXCL` 创建普通文件，再显式执行一次 `mkfs.btrfs`；未格式化既有设备。NixOS 声明只负责挂载，**不会创建或格式化镜像**。`/data` 原有挂载和选项完全不变，只有专用镜像允许普通UID删除备份子卷。它是容量受限的试用布局，不代表正式生产磁盘规划；已有挂载点为空不能代替验证实际镜像已挂载。

状态和用户Vault分开，Vault由用户拥有并逐项ACL授权，既有资料目录没有被递归chown/chmod。服务没有sudo/CAP_SYS_ADMIN；备份准备区的子卷、只读快照和删除均由服务UID通过正常btrfs权限完成。Vault内不允许嵌套挂载/子卷，原身份/CoW/扫描准入仍由产品执行。

首次初始化在服务离线、空状态上，以服务账号执行包内 `vaultsyncd-init`（等价于 `vaultsyncd-admin init --state-dir <state> --login <adminUser>`，`adminUser` 默认等于服务用户 `xingjian`），密码经匿名stdin管道输入；没有密码参数、Nix secret选项或默认密码。本卡随机生成的密码只在受限运行时文件与初始化/登录进程内存出现；没有写进store、项目Git或证据日志。该文件保存的是**初始**凭据，之后改密需由运维同步管理；服务启动不读取它、不自动重置owner。不要将其复制到artifacts或Git。

## 已执行的发布与验收顺序

先保存旧配置diff、锁、generation列表、SSH banner与既有服务PID/InvocationID、HTTP状态，并为旧 system 建GC root。未改动配置求值输出与运行 system 完全相同后，构建新源项目包、准备专用卷/样本/owner，新增主机文件并 `git add`（否则flake看不到新文件）。

```sh
# 从上述证据目录运行，result也是该目录内的构建链接。
sudo nixos-rebuild build --flake /etc/nixos#wks-lab --no-update-lock-file --no-write-lock-file
sudo nixos-rebuild dry-activate --store-path /nix/store/2qayb90x692izny4wi4wrg6l0dg8dhsq-nixos-system-wks-lab-26.11.20261102.7356cc7
sudo nixos-rebuild test --store-path /nix/store/2qayb90x692izny4wi4wrg6l0dg8dhsq-nixos-system-wks-lab-26.11.20261102.7356cc7
```

`test` **立即激活**，不更改下次启动默认generation，也没有定时自动回滚。dry-activate确认SSH配置逐字节相同，仅新增项目单元，以及新增账号导致的polkit重启/dbus reload，没有网络/既有应用/VM重启。以下验收脚本通过真实HTTP和同UID IPC操作唯一试用Vault，密码只从vault受限文件读入内存：

```sh
nix develop --command python3 deploy/nixos/verify-instance.py /nix/store/fnc7wrxq8y48gza4i52l598k1k12slma-vaultsyncd-0.1.0 /data/vaultsyncd-deploy/S096-20261107T010323-3856550 test
```

完成本地观察 → 上传分块与finalize → committed → 固定版本下载；重启服务后复核ServerId、内容字节、版本与同OpId幂等重放，以及无特权备份prepare/verify-source/release。`pending`不算成功。每个phase只创建一次带固定测试名的新样本；存在则拒绝，日常健康检查不反复运行该写入验收。

随后实际回退旧system做 `test`：试用服务inactive、18721拒绝连接、镜像卸载；SSH及10个既有关键服务的PID/InvocationID保持。再次对新system执行 `test`，`verify-instance.py ... recovery` 核对重挂载后原ServerId/Vault/固定版本SHA256与持久Op状态，才执行：

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

这是generation237的已知健康配置；旧system及准确新配置源的GC root都保留。若健康后决定让旧配置成为持久默认，再对同一旧store路径执行 `nixos-rebuild switch --store-path ...`（该持久旧配置switch没有在本卡执行，实际演练为test回退与返回新配置）。磁盘镜像、状态和凭据不随回退删除。主机源码还包含启用声明，恢复后需明确回退/禁用 `ba1837a` 中的该声明再做普通flake rebuild；否则普通rebuild会再次启用试用实例。

SSH一直运行，回退不依赖项目服务或密码。日志/原diff/完整配置源/新旧system路径在证据目录，`rollback.sh`也保留已知旧路径。TLS、同源代理与对外入口由S097单独验收；当前HTTP例外严格限定回环试用端口。

## S097 当前 HTTPS 开发入口（generation240）

S097 已在同一真实主机执行两轮 build → dry-activate → test → 严格 TLS/健康检查 → switch，第二轮修复实际服务重启的 Vault 身份误报。当前主机提交 `ebd832b7bbe751bfc87cbd9b9260ec53a1a26efe`，实际运行与默认 **generation240**：

```
/nix/store/51r012i21j0i26br4319xi8k5f0sdvym-nixos-system-wks-lab-26.11.20261102.7356cc7
```

主机原 dirty `flake.lock` 仍为前述SHA256，未更新或提交。`hosts/wks-lab/system.nix` 的后端固定永久仓库 `git+file:///wrd/prj/vaultsyncd?rev=fd2f52d2f58978b09fccaec3ba87b2377910a1eb`，实际重新构建的包是 `/nix/store/sw8v63x3hv5hql5qfapx0qyrlgc78ikn-vaultsyncd-0.1.0`；不是旧S094产物。代理配置独立固定 `d90a263d962795000aeb2eecfd430891a94a26d0` 的 `deploy/reverse-proxy/caddy.nix`。两源已在master永久可达，均不引用任务树。构建清单保留各自真实来源；本卡PE与包是0.1.0，不能重标为S100后续0.1.1。

提交后的主机配置连同原dirty锁再求值仍产生上述运行system。精确配置源`/nix/store/ky73gbi10wr9xbddjxyhf0n0df0wnk0i-source`、NAR `sha256-x0+sTSHtxF4sUhTQRZlYnzbFYbOunlktgo7+f2kLWQo=`及当前system均有证据目录中的GC root；主机commit不能单独代替这份锁来源。

| 路径/入口 | 当前保证 |
| --- | --- |
| `https://localhost:18443`、`https://127.0.0.1:18443` | Caddy仅监听127.0.0.1，新vhost复用既有Caddy |
| Windows `https://10.0.2.2:18443` | Quickemu NAT访问同一开发入口，实际Edge与产品PE验收 |
| 后端 `http://127.0.0.1:18721` | 回环HTTP，仅供本机TLS反代与管理检查 |
| TLS运行时目录 | `/var/lib/vaultsyncd-tls-s097`，vault:caddy0710；ca.key vault0600，server.key vault:caddy0640 |
| 公开开发CA | SHA256 `26:2F:E2:25:3A:8A:27:A0:5A:A2:1C:D1:B5:89:C6:81:12:EB:C4:79:9E:39:F9:80:6D:DF:E4:A8:47:E6:F5:7D` |
| 证据 | `/data/vaultsyncd-deploy/S097-20261107T012157-3886231`；任务artifacts只含筛选的公开材料 |

此入口使用显式信任私有CA：宿主浏览器只在临时NSS HOME导入；Windows在核对上述指纹后导入Quickemu当前用户Vault，信任仍保留。叶证书SAN为localhost/127.0.0.1/10.0.2.2，有效30天，CA365天；到期须用同样受限运行时流程续签并reload，私钥不得复制到store、Git或证据。**没有公网域名、ACME、外网监听或公开部署验收**。正式对外前须另行配置真实域名证书、访问范围及证书续期；开发CA不能作为公网信任声明。

Web/API保持同源；代理保留原Host，覆盖受信转发字段并去掉客户端Forwarded，不启用跨源写入。request body上限16777216字节，真实最大块成功，+1字节413；`flush_interval -1` 与5分钟stream_close_delay用于SSE，没有整个响应体超时或自动重试非幂等写。只reverse_proxy，不提供任意file_server或Unix管理socket代理。页面、管理API仍受服务端会话/owner权限；路径探测包括admin/socket、在线DB、/etc/passwd和密码路径均404。会话cookie为Secure/HttpOnly/SameSite=Strict。

Linux Chromium和Windows Edge实际完成HTTPS页面登录、16MiB上传、finalize、committed、固定版本下载摘要与磁盘一致，以及SSE初始seq、15秒心跳和新提交提示。新浏览器不信任私有CA时失败，信任正确CA后错主机证书仍失败。正式receipt PE源cf1d174（agent SHA256 `c25e36a7236edcdc5bcef405760cf1df1df6d599de05b7b426c629c0683a890c`）在Quickemu Interactive/Limited任务中验证默认CA拒绝、显式ca_file登录与Credential Manager、同用户CLI IPC logout。SSH会话1312只说明缺交互凭据，不算TLS失败。密码经同SID+SYSTEM受限命名管道/匿名stdin，不写Windows临时文件；本卡自身agent、计划任务、Edge profile及临时监听已清理，没有重启客体或触碰其他测试用户。

早期Playwright异常格式曾在临时输出中带两枚本次测试cookie，已立即用对应会话logout吊销并清理原值，失败日志不归档；没有输出密码/私钥。最终脚本只记录错误类别与源码位置，finally吊销自身会话，禁止导出HAR/cookie/profile。Windows首轮外层500秒控制器在人工CA提示时超时，内层实际worker退出0且随后Edge矩阵通过；最终900秒控制器改动仅原生语法校验，不声称重跑了整条原生链。

## S097 Vault 身份修复与恢复限制

旧注册快照含mount_id1031/dev0:128；当前服务namespace现场为mount916/dev0:110，但FSID `a3f322713ab8458f9af3666696536cf7`、subvol5、目录inode258及birth `1793984671.665929329`完全相同。原管理健康消费者直接比较历史瞬态字段而误报mount_changed；实际上传仍成功。现在启动时完整核对持久身份、CoW/访问权/嵌套准入，再为本进程建立共享VaultHandle；诊断、观察、执行与启动重放均使用此表。历史blob不修改，新注册继续严格比较当前围栏，活进程bind/卸载/权限变化不会自动重新准入。目录枚举使用固定fd的openat点目录取得独立游标，避免并发扫描污染偏移造成假空目录。

`fence-acceptance.py`以独立NixOS同策略单元、`/data/vs-s097-fences-*`新普通btrfs镜像验证；离线`fence-boundary`示例先取StateLock再通过真实适配器持久化边界，不往生产加故障开关。最终run `...025123-4077079`真实测得namespace4026534527→4026534585、mount1033→1170，同dev199、Vault/blob/seq4/内容保持健康；活进程同目录bind blocked且4条目/seq不变，撤销覆盖恢复。Prepared同dev服务重启redo committed；卷重挂实测dev199→201后Published仅补元数据，文件inode/birth/摘要不变。旧Prepared的FileKey没有birth，dev201→199时**保持pending、Prepared和全局needs_recovery/503**，两份内容与对象保留；不能忽略旧dev冒充强身份或自动覆盖。离线未完成现场保留，单元/keeper/loop均已停止、卸载、分离。并发8线程×16轮×3000文件枚举完整。

新包部署后真实试用服务又执行一次systemctl restart，严格Chromium页面确认“挂载核对通过/本进程初始采集完成”；原ServerId/Vault/seq18、历史blob、全部样本字节、先前Linux/Edge committed操作及固定版本摘要不变。服务仍UID985、零capability、NoNewPrivileges=1；10个既有服务和SSH的PID/InvocationID保持，3200/3300/15312均200。上述故障注入只作用隔离run。

S097前的generation239（保留TLS、旧Vault诊断行为）为 `/nix/store/bcwryfrqcc8sajf5kk7469q9j8g8r31d-nixos-system-wks-lab-26.11.20261102.7356cc7`；generation238不启用本卡TLS。S097没有再次激活239作回滚演练，S096实际旧代际回滚证据仍按上一节。需要恢复旧配置时可用明确store path执行`nixos-rebuild test --store-path ...`，它立即激活且**不定时回滚**；保留SSH和旧路径，确认健康后再决定switch。正常flake重建还要同步回退host文件固定源，不能只更改运行generation。试用卷/凭据不随配置回退删除。

## S112 当前正式域名部署（2026-11-07，generation244）

以下为 S112 验收时的历史快照；当前运行用户、版本和 generation 以 S113 节为准。

实际入口仍为 `/etc/nixos#wks-lab`。项目固定源 **4856738641ea3421c949476e6951d0a461209223**，同源包 `/nix/store/6nx4qmvrhj1rk3aa9mrjaxky56m4r7ck-vaultsyncd-0.1.1`，构建含65项包内测试通过。主机实现 `9c2133f`，挂载修正 `56bd93b09760d87f068c7a617b8a79904d14b852` 已ff合入主机main；主机锁从干净基线 `93dfcae` 保持不变。主机准确源 `/nix/store/kdr0v41db6vcd39kp9znz5b3xky170dp-source` 与旧/新system都有显式GC root，证据在 `/data/vaultsyncd-deploy/S112-20261107T133454/`。

```nix
# /etc/nixos/hosts/wks-lab/default.nix 的 features.server 块
vaultsync = {
  enable = true;
  rev = "4856738641ea3421c949476e6951d0a461209223";
  stateDir = "/var/lib/vaultsyncd";
  vaultPaths = [ "/data/vaultsyncd/vaults" ];
  port = 8721;
  caddy.domain = "vsyncd.132140.xyz";
};
```

主机 `features/server/vaultsync.nix` 条件导入该提交的项目flake模块，消费其 `lib.caddyReverseProxy`。`enable/repository/rev` 必须放在主机字面features块（imports依赖静态host.features）；rev要求40位已提交SHA，未启用主机不读取本地仓库。可配置 `user/group/extraGroups/stateDir/vaultPaths/port/log.{level,format}/caddy.{enable,domain,extraConfig}`；后端固定绑定回环。关闭feature会停止服务及本域名代理，保留挂载、账号和数据；`caddy.enable=false`只撤代理。主机vhost经既有 `features.server.caddy.virtualHosts` 桥接，默认不写access log，不直接暴露管理socket或文件。

| 项目 | 当前配置 |
| --- | --- |
| HTTPS | `https://vsyncd.132140.xyz`，DNS为本机115.24.115.250；HTTP308到HTTPS |
| 证书 | Let's Encrypt YE1，SAN匹配域名，TLS1.3；2026-11-07至2027-02-05，由Caddy自动续期 |
| DNS-01 | 复用Caddy既有Cloudflare插件和 `/run/agenix/cloudflare` 环境文件；无新明文token |
| 后端 | `127.0.0.1:8721`，`vaultsyncd.service`，UID/GID984，零capabilities、NoNewPrivileges |
| 正式ServerId | `69dbfa9fdca2bfb8a239b88d0fd30d53`（与旧试用实例不同，客户端连接须核对） |
| 状态 | 新btrfs子卷 `/data/vaultsyncd/state`（subvol2659），显式挂至 `/var/lib/vaultsyncd`，0700 |
| Vault白名单 | `/data/vaultsyncd/vaults`，初始注册 `/data/vaultsyncd/vaults/documents`，服务账号所有0700 |
| 文档VaultId | `cf8a15ace18abe4d9c3027ac25c30070` |
| 初始登录 | `owner`；随机密码在 `/var/lib/vaultsyncd-credentials/owner.password`，vault0600，父目录0700 |

在受信终端用 `sudo cat /var/lib/vaultsyncd-credentials/owner.password` 读取初始密码。服务启动不读此文件，不会重置owner；改密后需独立管理凭据，文件不代表永远有效的当前密码。初始化经离线admin匿名stdin完成，不通过argv/env/store传递。真实密码、token、cookie、私钥未进入Git或证据；运行日志、argv、生成TOML核对不含初始密码。

只创建了不存在的新目录/子卷，未格式化既有设备、未递归改权既有资料。挂载CoW、数据校验与压缩保留；`user_subvol_rm_allowed`须同时配置 `/data` 首次挂载和状态挂载，这是**整个btrfs文件系统的选项**，允许普通UID在原有目录权限约束下删除自己拥有的子卷，不限于状态子卷。首次test因仅配置后续子卷挂载而被preflight拒绝，失败日志保留；修正首次挂载、实际remount并核对findmnt后重跑通过。行为依据 [btrfs挂载文档](https://btrfs.readthedocs.io/en/stable/Administration.html)。服务没有sudo/CAP_SYS_ADMIN。Vault内仍禁止嵌套子卷/挂载，注册及恢复围栏未降低。

旧镜像 `/data/vaultsyncd-trial-s096/volume.img`、挂载、UID985、全部历史试用状态及 `/var/lib/vaultsyncd-trial-credentials/owner.password` 保留；正式实例不复用旧DB。旧localhost:18443 vhost已撤下，开发CA与私钥未删除，也不用于公网信任。

实际先保存原diff/锁/generation/服务PID与SSH，构建包及主机；dry-activate仅重启本服务/polkit、reload Caddy/dbus，sshd配置逐字节不变。修正后执行以下真实命令，最后设为开机默认；test立即激活且不提供自动定时回滚：

```sh
nix build /etc/nixos#nixosConfigurations.wks-lab.config.system.build.toplevel --no-update-lock-file --no-write-lock-file --out-link /data/vaultsyncd-deploy/S112-20261107T133454/host-corrected
sudo nixos-rebuild dry-activate --store-path /nix/store/jg54j3w2arc95lc3asx071w2b75z95kq-nixos-system-wks-lab-26.11.20261102.7356cc7
sudo nixos-rebuild test --store-path /nix/store/jg54j3w2arc95lc3asx071w2b75z95kq-nixos-system-wks-lab-26.11.20261102.7356cc7
# 中间完成下述HTTPS/浏览器/存储/既有应用健康验收
sudo nixos-rebuild switch --store-path /nix/store/jg54j3w2arc95lc3asx071w2b75z95kq-nixos-system-wks-lab-26.11.20261102.7356cc7
```

主机构建日志在证据目录。当前运行/default system均为上述jg54j3w2路径，切换后重新核对原ServerId、Vault健康、committed状态与固定版本SHA，结果见 `switch-health.json`。更新只改已构建项目提交与feature配置，依次build/test/健康/switch；不更新主机依赖。

验收：严格系统CA/主机名的HTTPS健康和capabilities，16MiB上传→finalize→committed，固定版本下载SHA与磁盘一致；+1字节413；重启后原ServerId/版本/同OpId重放；同UID IPC及普通UID btrfs备份prepare/verify/release通过。新Chromium配置无私有CA/不忽略TLS，真实Web登录、Secure/HttpOnly/SameSite=Strict cookies、SSE初始seq及16秒心跳通过；敏感文件/DB/socket探测404。首轮脚本把16MiB按默认分块发送而失败，改为明确协商16MiB后通过；浏览器心跳等待改为真实流等待与页面观测后通过，失败日志不冒充通过。

`deploy/nixos/verify-public.py` 与 `verify-public-browser.py` 为显式部署验收脚本，前者会在指定Vault创建新样本并重启指定服务，**不是日常健康探测**。只对本次专用Vault执行；使用新run_id，拒绝覆盖旧样本/证据。两脚本从受限运行时文件读密码，只输出公开状态、哈希及Cookie属性，并注销自身会话。证据 `acceptance-test2.json`、`browser.json`、`existing-services-test.json`。日常只用：

```sh
curl --fail https://vsyncd.132140.xyz/v1/health
sudo -u vaultsyncd /data/vaultsyncd-deploy/S112-package/bin/vaultsyncd-admin status --socket /var/lib/vaultsyncd/admin/socket --json
sudo journalctl -u vaultsyncd --no-pager
```

10个既有服务（含sshd/Caddy/PostgreSQL及应用）的PID/InvocationID保持，3200/3300/15312返回200，SSH banner与主机锁摘要不变。未重启宿主/Windows；这次不是Windows原生验收或S108首发门禁。域名HTTPS与证书通过本机使用公网域名验证；独立外部Web抓取工具返回不可访问，**不宣称已有独立外网客户端连通证据**。Caddy自动续期已配置，未等待证书实际临近到期再验收。

恢复入口为部署前generation243：`/nix/store/4q8hm31xrd6wfqslddb5211k6y2birxa-nixos-system-wks-lab-26.11.20261102.7356cc7`。必要时 `sudo nixos-rebuild test --store-path <该旧路径>` 立即恢复旧试用服务及Caddy配置；正式数据/凭据保留，正式状态挂载可能卸载，不要清理它。确认健康后再决定对旧路径switch；也需同步回退主机源码，否则常规flake rebuild会重新启用正式实例。本卡没有激活旧generation演练回退，不把S096的历史演练当作本卡测试。旧版本不得打开已升级的新库，恢复旧试用实例用其原库。

## S113 主机用户运行与 pure 一键部署（2026-11-07）

本机入口 `/etc/nixos#wks-lab`，feature 的 `user` 默认读主机 `host.user`，`group` 默认读 `config.users.users.<user>.group`，当前为 **xingjian:users（UID1000/GID100）**。项目模块 `createUser=false` 不更改普通用户类型/主组/描述/附加组。单元保留零 capabilities、NoNewPrivileges、0700 状态、严格系统写保护与 Vault 白名单。

正式 `/data/vaultsyncd`、`/var/lib/vaultsyncd`（挂载源 `/data/vaultsyncd/state`）、`/data/vaultsyncd/vaults` 及其 documents 目录均为 xingjian:users、0700；内部文件及原样本已离线改权。`/var/lib/vaultsyncd-credentials` 同归该用户且0700，`owner.password` 为0600，可在受信终端用 `cat /var/lib/vaultsyncd-credentials/owner.password` 读取。保留原登录及 ServerId/VaultId，服务重启不会重置密码。没有删除正式数据、没有修改历史试用卷或其账号。

主机配置仍以完整项目 Git SHA 固定 `features.server.vaultsync.rev`，永久地址 `git+file:///wrd/prj/vaultsyncd`，不依赖会清理的 worktree。更新入口：

```sh
cd /wrd/prj/vaultsyncd
# 先提交项目更新，项目和 /etc/nixos 均须干净
./tools/deploy-local.sh
# 或部署指定的已提交版本
./tools/deploy-local.sh --rev <完整Git提交>
# 仅构建，不激活；会保留证据和配置工作树
./tools/deploy-local.sh --build-only
```

默认主机名为本机 hostname，可用 `--host wks-lab`，`--config` 默认 `/etc/nixos`。普通用户运行，激活需要 sudo。脚本只在隔离主机 worktree 更新版本，显式启用 `pure-eval=true`、禁止锁更新/写入；根据固定源构建完整主机并核对发行 manifest SHA，按相同 store path dry-activate → test → HTTPS/IPC/ServerId/运行包/UID/目录归属 → switch，再核对开机与运行 system。成功以中文提交版本 pin、ff 回主机原分支并清理临时树，不推送。纯求值固定引用的依据见 [Nix getFlake 手册](https://nix.dev/manual/nix/2.31/language/builtins)；激活已构建路径使用已安装 nixos-rebuild 的 `--store-path`。

证据在 `/data/vaultsyncd-deploy/local-*`：旧运行/启动 system 的 GC root、generation、锁、pin 差异与 result.json；`--build-only` 或失败树保留，检查后按结果中的 worktree/branch 清理。每次部署只操作当前配置，既有未提交修改不会自动 stash 或搬运。`test` 立即激活，不提供定时回滚。失败会尝试恢复旧运行/启动 system；若迁移后旧程序拒绝新 schema，需要离线恢复产品备份，脚本不降级数据库。正常更新前按产品备份流程留可恢复副本。

一次性账号迁移证据 `/data/vaultsyncd-deploy/S113-20261107T221130/`：停服后的只读 `state-before` btrfs 快照与 `vaults-before` reflink 副本、旧归属和文件摘要均保留在 Git 外。旧运行/启动 generation244 为 `/nix/store/jg54j3w2arc95lc3asx071w2b75z95kq-nixos-system-wks-lab-26.11.20261102.7356cc7`。**回到 S112 账号配置前，先停服并把专用正式状态及 Vault 的归属恢复为 UID/GID984（旧配置会重建该账号）；仅切换旧 system 不能同时恢复归属。** 备份快照不改权；不要对用户其他目录递归改权，也不要用旧程序直接写新 schema。

初次迁移固定项目4071fd4、主机75d6fb5，系统k92lxnm03xg828lzfzivp0aij810gpns先test；原正式 ServerId `69dbfa9fdca2bfb8a239b88d0fd30d53`、文档 VaultId `cf8a15ace18abe4d9c3027ac25c30070` 和旧样本摘要保留。65项包内测试、普通/专用用户模块、aliweb关闭feature的pure完整求值通过。真实HTTPS16MiB上传/413/committed/固定下载/服务重启与同OpId重放、同UID IPC与普通用户备份prepare/verify/release通过；10项既有服务状态/PID/InvocationID保持。最终一键更新提交、generation与switch后证据见 [S113交接](../../agent/tasks/S113.md)。本卡不代替Windows、首发性能门禁。

S113 最终一键部署已通过：项目固定 **6b967bda32378da5e74e4c328fcaa3bcd72e75cf**，主机 **32bfeb54f9c76d5ed781a80800fdce86179c7d6a** 已 ff 合入 `/etc/nixos` main；运行/开机默认 **generation245**，system `/nix/store/mwra49kbdbgppwmc5p7j7rz0dbfbbk4w-nixos-system-wks-lab-26.11.20261102.7356cc7`，包 `/nix/store/180dcgy5j2hx801vg59g2bk15whp3p7j-vaultsyncd-0.1.1`。该源复用已通过65项测试的相同Rust组件；本次重新构建发行provenance与主机。真实脚本从4071fd4更新pin到6b967bd，显式pure构建后dry/test/健康/switch，通过后自动提交pin并清理临时树。主机main的pure toplevel重新求值得到同一system。

最终脚本证据 `/data/vaultsyncd-deploy/local-20261107T221615-1805073/`，完整日志在上方S113证据的 `one-click.log`；`switch-health.json`确认旧ServerId/VaultId/committed版本及磁盘/下载摘要、全部正式文件UID1000/GID100、10项既有服务PID/InvocationID及SSH保持，3200/3300/15312跟随重定向后200、主机锁逐字节不变。10服务比较使用保留的S112历史基线，7个已启用服务另有本轮迁移前快照。最终没有注入生产失败或实际激活旧generation演练回退，也没有再次运行Windows原生/浏览器矩阵或独立外网客户端探测。


## S116 当前版本部署（2026-11-08，generation246）

按用户要求执行 `./tools/deploy-local.sh --rev f3b8979`，固定项目完整 SHA `f3b897983087af1b6d075d89000c0a5ebc235aeb`，包含 S114 服务端 Vault 名称及 S115 HeroUI Web。主机仅改版本 pin，提交 `15cfc075031ba6d9fab4bf040e1adbe5d68be939` 已 ff 合入 `/etc/nixos` main，临时主机树与分支自动清理。主工作区未提交 skills 文件保留；部署从干净任务树执行。

运行与开机默认均为 **generation246**，system `/nix/store/22bvy0jasjkb02vd8j2wm6lcmgn5aci9-nixos-system-wks-lab-26.11.20261102.7356cc7`；包 `/nix/store/yd63mh9prpasa8f8647g076mx2m2wjdv-vaultsyncd-0.1.1`，manifest 与固定源一致。域名、owner、ServerId `69dbfa9fdca2bfb8a239b88d0fd30d53`、文档 VaultId `cf8a15ace18abe4d9c3027ac25c30070` 保持；状态与 Vault 仍为 btrfs、xingjian:users、0700。密码读取方式沿用 S113。

实际验收：部署契约6/6，Nix 包内66/0；完整 pure 构建及同一 store path 的 dry-activate/test/HTTPS与同UID IPC、实例身份/目录归属/switch通过。10项既有服务的PID/InvocationID、SSH配置与banner、3应用200及主机锁保持；线上HTML/JS/CSS共6文件逐字节匹配发行包。真实 Chromium 严格公共CA/TLS1.3、五路由及390/768/1440无溢出、主题、owner登录与Secure/HttpOnly/SameSite=Strict cookie、原Vault健康、SSE初始seq及16秒心跳通过，无页面异常及CSP违规，敏感路径404。浏览器脚本前三轮因定位歧义、缺同源证据的APIRequestContext和字符串wait_for_function触发unsafe-eval限制失败；分别限定main、使用页面同源fetch和函数谓词后完整复验通过，未修改产品代码或放宽安全策略。

部署证据 `/data/vaultsyncd-deploy/local-20261108T124809-2457084/` 保存 result.json、旧generation、旧system/boot GC roots、原锁和pin diff；额外核对与浏览器脚本/失败日志/成功JSON/截图在 `/data/vaultsyncd-deploy/S116-20261108-local/`。恢复入口为部署前generation245：`/nix/store/mwra49kbdbgppwmc5p7j7rz0dbfbbk4w-nixos-system-wks-lab-26.11.20261102.7356cc7`，按S113恢复流程先test及健康检查，再决定switch并同步回退源码pin；没有实际激活旧generation演练。未运行生产上传/恢复/备份写入或故障注入、未重启宿主/Windows、未做独立外网客户端验收，原首发门禁保持。交接与任务树整合见 [S116](../../agent/tasks/S116.md) 与 [批次（历史归档）](../../agent/tasks/README.md)。


## S118 最新版本部署（2026-11-08，generation247）

按用户要求执行 `./tools/deploy-local.sh --rev 9da5ee8`，固定项目完整 SHA `9da5ee8f3fe3f64cf517ab6f52d6f1969b1dc353`（含 S115 服务端与 S117 Web 紧凑化）。主机侧先提交用户要求的 dae 分流规则调整 `42d99c5`（实际写法 `!dip(geoip:cn) -> proxy`；并行会话另以 `9cd3956` 记录了该语法坑），再由脚本以 `9cd3956` 为基线只改版本 pin，pin 提交 `b0ad622809158369075b0065d8dd6e1a4c8e4838` 已 ff 合入 `/etc/nixos` main，临时主机树与分支自动清理。部署从干净任务树执行，主工作区未提交 skills 文件保留。

运行与开机默认均为 **generation247**，system `/nix/store/cwrpmxw805bg9nrn08a54jmny2lxcg4w-nixos-system-wks-lab-26.11.20261102.7356cc7`；包 `/nix/store/2vqjynvrjjxcldl9svlka49mrn3rzwk6-vaultsyncd-0.1.1`，manifest 来源与固定 SHA 一致。域名、owner、ServerId `69dbfa9fdca2bfb8a239b88d0fd30d53`、文档 VaultId `cf8a15ace18abe4d9c3027ac25c30070` 保持；状态与 Vault 仍为 btrfs、xingjian:users、0700，IPC `phase=ready`。

实际验收：clean 检查、pure 求值与构建、manifest 与主机锁核对、dry-activate/test/HTTPS/IPC/归属/switch 全部通过；线上 `index.html` 与两个主资产与包内文件 sha256 逐字节一致（资产名已更新为新构建）。真实 Chromium 严格公共 CA/TLS1.3、五路由 390/768/1440 无溢出、主题、owner 登录与 Secure/HttpOnly/SameSite=Strict cookie、原 Vault 健康、SSE seq 与 16 秒心跳、无页面异常与 CSP 违规、敏感路径 404 均通过。10 项既有服务中 9 项 PID/InvocationID 不变，`dae` 因规则变更重启后路由集 17/1024、google 与 baidu 均 200；3 应用 200、SSH banner 与主机锁不变。

部署证据 `/data/vaultsyncd-deploy/local-20261108T133746-2533091/` 保存 result.json、旧 generation、旧 system/boot GC roots、原锁与 pin diff；本轮核对与浏览器脚本/JSON/截图/静态文件副本在 `/data/vaultsyncd-deploy/S118-20261108-local/`。恢复入口为部署前 generation246：`/nix/store/22bvy0jasjkb02vd8j2wm6lcmgn5aci9-nixos-system-wks-lab-26.11.20261102.7356cc7`，按 S113 流程先 test 与健康检查再决定 switch，并同步回退源码 pin；如需还原分流规则须同时回退 `42d99c5`。未实际激活旧 generation 演练回退，未运行生产上传/恢复/备份写入或故障注入，未重启宿主/Windows，原首发门禁保持。交接与整合见 [S118](../../agent/tasks/S118.md) 与 [批次（历史归档）](../../agent/tasks/README.md)。


## S120 Mantine 版最新服务端部署（2026-11-08，generation248）

按用户要求执行 `./tools/deploy-local.sh --rev b5e96f2`，固定项目完整 SHA `b5e96f23268ef9dd0a34574e81bcb87718d9a552`（S119 实现提交 `b16d9a1`），使 Mantine 9.7.5 AppShell/默认主题与逐响应 style nonce CSP 上线。主机仅改版本 pin，pin 提交 `0073fd638392658eb56ef01cf30ceadc9229631a` 已 ff 合入 `/etc/nixos` main，临时主机树与分支自动清理；部署从干净任务树执行，主工作区未提交 skills 文件保留。

运行与开机默认均为 **generation248**，system `/nix/store/i1ydyql7wcb0gs8q5dr1xxygm4zj18h6-nixos-system-wks-lab-26.11.20261102.7356cc7`；包 `/nix/store/9h1r3jvn1r1nsnrdal2h169n4h5w9hdm-vaultsyncd-0.1.1`，manifest 来源与固定 SHA 一致。域名、owner、ServerId `69dbfa9fdca2bfb8a239b88d0fd30d53`、文档 VaultId `cf8a15ace18abe4d9c3027ac25c30070` 保持；状态与 Vault 仍为 btrfs、xingjian:users、0700，IPC `phase=ready`。

实际验收：clean 检查、pure 求值与构建、manifest 与主机锁核对、dry-activate/test/HTTPS/IPC/归属/switch 全部通过；线上 `index.html`（剥离逐响应 nonce 后）与 `assets/index-Ckcbaj1u.js`、`assets/index-BX8_IVCu.css` 的 sha256 与包内一致；CSP 为 `style-src 'self' 'nonce-<32位hex>'`、header 与页面 meta 同值、无 `unsafe-inline`。真实 Chromium 严格公共 CA/TLS1.3、Mantine 主题切换、五路由 390/768/1440（窄屏经 Burger）无溢出、owner 登录与 Secure/HttpOnly/SameSite=Strict cookie、原 Vault 健康、SSE seq 与 16 秒心跳、无页面异常与 CSP 违规、敏感路径 404 均通过。10 项既有服务 PID/InvocationID 全部不变，3 应用 200、SSH banner 与主机锁不变。

部署证据 `/data/vaultsyncd-deploy/local-20261108T142445-2602881/` 保存 result.json、旧 generation、旧 system/boot GC roots、原锁与 pin diff；本轮核对与浏览器脚本/JSON/截图/响应头/静态文件副本在 `/data/vaultsyncd-deploy/S120-20261108-local/`。恢复入口为部署前 generation247：`/nix/store/cwrpmxw805bg9nrn08a54jmny2lxcg4w-nixos-system-wks-lab-26.11.20261102.7356cc7`，按 S113 流程先 test 与健康检查再决定 switch，并同步回退源码 pin。未实际激活旧 generation 演练回退，未运行生产上传/恢复/备份写入或故障注入，未重启宿主/Windows，原首发门禁保持。交接与整合见 [S120](../../agent/tasks/S120.md) 与 [批次（历史归档）](../../agent/tasks/README.md)。


## S121 48px 顶栏部署（2026-11-08，generation249）

固定实现源`354bbc479a4ef7b68418aa061d5a7f5a4d5e488c`，部署入口与S113一致，从干净任务树执行。48px顶栏、独立Logo分区/侧栏对齐边线、居左面包屑已上线。主机pin `6cc497bd4915674b9b27a6e0e62bf1c7a6394bd7`已ff合main并清理临时主机树/分支；运行及开机system均为`/nix/store/ds2gqqb5lng9pwwjjffj2z2qf3iavy4n-nixos-system-wks-lab-26.11.20261102.7356cc7`，包`/nix/store/v6zxzvx08p7m6lh6n6mi54y44f1yd25x-vaultsyncd-0.1.1`。

pure构建/manifest/锁/dry/test/HTTPS/IPC/归属/switch通过；线上HTML剥nonce后与包内一致，`index-ClZcVBwm.js`、`index-Mvsj7_ia.css`字节/哈希一致。真实Chromium线上50组布局和TLS/CSP/登录cookie/原Vault健康/SSE/敏感路径六项通过。域名、owner、ServerId、VaultId、数据与xingjian:users归属保持；10项服务PID/InvocationID、3应用200、SSH及主机锁保持。

证据`/data/vaultsyncd-deploy/local-20261108T144944-2637582/`与`/data/vaultsyncd-deploy/S121-20261108-local/`；恢复入口generation248（system `i1ydyql7wcb0gs8q5dr1xxygm4zj18h6`），先按S113 test/健康再switch并同步pin，未实际回滚。未做生产内容写入/故障或Windows/规模验收，原首发门禁保持。交接与生命周期见[S121](../../agent/tasks/S121.md)与[批次（历史归档）](../../agent/tasks/README.md)。

## S123 最新版本（S122 文件浏览器）部署（2026-11-08，generation250）

固定项目完整 SHA `6395dfaf91eae84445695484e65ee740003faec0`（部署时 master HEAD，含 S122 实现 `25dbf2b` 的只读文件浏览器 `#/files` 与 S121 顶栏），入口与 S113 相同，从干净任务树执行 `./tools/deploy-local.sh --rev HEAD`。主机仅改版本 pin，pin 提交 `207099c690eedf4d991efe9bd519a63d2e4b25c3` 已 ff 合 `/etc/nixos` main，临时主机树与分支自动清理；部署契约 6/6 通过，主工作区未提交 skills 文件保留。

运行与开机默认均为 **generation250**，system `/nix/store/ivwnixw752n1n1bbk57iwrdx5p7nl25f-nixos-system-wks-lab-26.11.20261102.7356cc7`；包 `/nix/store/z7lcr1c1rz3ia0ixpk646594l5dk2cy9-vaultsyncd-0.1.1`，manifest 来源与固定 SHA 一致，包内 Rust 组件重建时 67/67 测试通过，Web 组件与 S122 `nix build .#web` 同一 store path。域名、owner、ServerId `69dbfa9fdca2bfb8a239b88d0fd30d53`、文档 VaultId `cf8a15ace18abe4d9c3027ac25c30070` 保持；状态与 Vault 仍为 btrfs、xingjian:users（UID1000/GID100）、0700，IPC `phase=ready`、`vault_count=1`。

实际验收：clean 检查、pure 求值与构建、manifest 与主机锁核对、dry-activate/test/HTTPS/IPC/归属/switch 全部通过；线上 `index.html` 剥 nonce 后与包内逐字节一致，`assets/index-DQ7KusXO.js`、`assets/index-BtUIqXZy.css` 与懒加载块 `assets/Files-V7htNGV1.js` 的 sha256 与包内一致；CSP 为逐响应 nonce、无 `unsafe-inline`。真实 Chromium 严格公共 CA/TLS1.3 七项通过：CSP/主题、六路由（含 `#/files`）在 390/768/1440 无溢出、owner 登录安全 cookie、原 Vault 健康、文件浏览器在 `documents` Vault 读到 2 项并打开属性弹窗/返回上级、认证 SSE seq 与 16 秒心跳、无 pageerror/CSP 违规、敏感路径 404。文件浏览器请求审计只有 `GET /v1/vaults`、`POST /v1/vaults/<vault>/snapshots`（design §9.3 元数据快照）、`GET …/entries`、`GET …/events` 与 logout，无内容/上传/操作写入；本轮在生产 Vault 生成快照 `01bf172d1156e91785681e4812faf5a3`，不修改文件内容。10 项既有服务 PID/InvocationID、3 应用 200、SSH banner 与主机锁（`1ee4edaf…`）不变。

证据 `/data/vaultsyncd-deploy/local-20261108T153312-2697471/`（result、旧 generation、旧 system/boot GC roots、锁、pin diff）与 `/data/vaultsyncd-deploy/S123-20261108-local/`（final-state/assets/production-smoke 脚本与 JSON、截图、web 副本）；脚本与结果另存 `artifacts/S123/`。恢复入口为部署前 generation249 `ds2gqqb5lng9pwwjjffj2z2qf3iavy4n`，按 S113 先 test 与健康检查再决定 switch 并同步回退 pin；本轮未实际激活旧 generation。未做生产上传/恢复/备份写入、故障注入、Windows 或规模验收，原首发门禁保持。交接与生命周期见 [S123](../../agent/tasks/S123.md) 与 [批次（历史归档）](../../agent/tasks/README.md)。

## S125 最新服务端（S124 分栏文件浏览器）部署（2026-11-08，generation251）

固定项目完整 SHA `30daa88febd53115b126a08cf062b37a217b117e`（部署时 master HEAD，含 S124 实现 `986c35c` 的左侧 Vault/目录树、右侧工具栏/列表与 `#/files/<vault_id>/<相对目录>` 深链），入口与 S113 相同，从干净任务树执行 `./tools/deploy-local.sh --rev HEAD`。主机仅改版本 pin，pin 提交 `dc7dd421dadbcf0cba11ab59f4801aefe7d4e3c5` 已 ff 合 `/etc/nixos` main，临时主机树/分支自动清理；部署契约 6/6 通过，主工作区未提交 skills 文件保留。

运行与开机默认均为 **generation251**，system `/nix/store/9nmw4ilaxg5i5q50n93fbn59q95dfv03-nixos-system-wks-lab-26.11.20261102.7356cc7`；包 `/nix/store/jd5xcmh6p5x0v7zsv844fxm70898zfy2-vaultsyncd-0.1.1`，manifest 来源与固定 SHA 一致，主机锁 `1ee4edaf…` 逐字节未变；本次重建 9 个派生（含重新构建并运行 checkPhase 的 `vaultsyncd-rust-0.1.1`）。域名、owner、ServerId `69dbfa9fdca2bfb8a239b88d0fd30d53`、运行用户 `xingjian:users` 保持。

**按用户明确要求，本轮不执行部署后验证**：没有 Chromium/浏览器矩阵、没有线上资产 sha256 比对、没有既有服务 PID/应用/SSH/锁基线复核，也没有生产上传/恢复/备份写入或故障注入。实际通过的门禁只有部署机制自身的 clean 检查、pure 求值与构建、manifest 与主机锁核对、`dry-activate` → `test` → HTTPS 健康（ServerId 不变）→ 同 UID IPC `phase=ready` → `stateDir`/Vault 归属 → `switch` → 运行/开机 system 一致性；切换前后两次健康检查均通过。因此线上 Web 资产与 UI 的最新证据仍来自 S123（generation250），本卡不构成任何新的生产 UI/安全/服务影响结论。

证据 `/data/vaultsyncd-deploy/local-20261108T160744-2745069/`（result、旧 generation、旧 system/boot GC roots、锁、host-before diff、pin diff）与 `/data/vaultsyncd-deploy/S125-20261108-local/`（契约测试与部署日志），筛选副本在 `artifacts/S125/`。恢复入口为部署前 generation250 `ivwnixw752n1n1bbk57iwrdx5p7nl25f`，按 S113 先 test 与健康检查再决定 switch 并同步回退 pin；本轮未实际激活旧 generation。原首发门禁（性能容量、最终发行、T02 Windows）不变。


## S128 目录树间距与开合图标部署（2026-11-08，generation253）

固定项目 `f7609ad12f58c1ef08f6d5322f1896cdf061a86c`，从隔离任务树执行现有 `tools/deploy-local.sh --rev <SHA>`。现场部署前已为 generation252、项目7582962/主机2362d9a；进度页原 generation251 仅为历史记录。本次仅更新主机项目 pin，主机提交 `63002140b0af2f1d922f0a97aa1aea81d433f97e` 已 ff 合 main、临时主机树/分支已清理；锁逐字节保持。

pure build、manifest 来源、dry-activate/test、HTTPS/IPC/ServerId/归属、switch 全通过；运行与开机均为 `/nix/store/q92sr3wjc5vidsafwr1n1bhvqn50syfh-nixos-system-wks-lab-26.11.20261102.7356cc7`，包 `/nix/store/395v9igpkc8gnh9bfnna0fgkblk821ml-vaultsyncd-0.1.1`。线上 index 剥 nonce 与全量资产逐字节匹配包内；真实 Chromium 严格 TLS1.3、owner 登录、明暗×320/1440 目录树、无页面溢出/CSP/pageerror、注销200通过。既有10服务 PID/InvocationID、3应用200、ServerId不变；只读浏览仅请求元数据，无文件内容写入。

证据 `/data/vaultsyncd-deploy/local-20261108T164823-2812385/`、永久仓库 `artifacts/S128/` 与 `/data/vs-s128-20261108T164620-2810111/`。恢复入口为 generation252 `/nix/store/iqdq51l51lndpk90898kgn7cxjb8kqh1-nixos-system-wks-lab-26.11.20261102.7356cc7`，按 S113 先 test/健康再 switch 并同步 pin；未实际激活旧 generation。Windows、规模及原首发门禁保持，详见 [S128](../../agent/tasks/S128.md)。


## S133 S132 右键菜单服务端部署（2026-11-08，generation255）

按用户要求（S132 完成后“部署服务端到本机”）执行 `./tools/deploy-local.sh --rev b811f41ce862fa4e34ac5776d333c2037b6bfd30`，
固定源为 S132 实现 `a9f0310` 与交接 `b811f41`（两者已 ff 合入项目 master）。本卡无 Rust 与迁移改动，主机仅改版本 pin：
pin 提交 `31186c77525206a78bd3256c611d8a8142e16649` 已 ff 合 `/etc/nixos` main，临时主机树/分支自动清理，主机依赖锁
逐字节保持 `1ee4edaf…`，主工作区未提交改动未被触碰。

运行与开机默认均为 **generation255**，system `/nix/store/a86anl2i29i5q3kcx9jqay6p2a2a6ngc-nixos-system-wks-lab-26.11.20261102.7356cc7`，
包 `/nix/store/vzwp4yzlbrli90ih9avbqq39pm30zk92-vaultsyncd-0.1.1`（manifest 来源与固定源一致）。服务以 xingjian:users 运行、
`phase=ready`、`/v1/health` 正常；ServerId `69dbfa9fdca2bfb8a239b88d0fd30d53` 与文档 VaultId 未变；库保持 `user_version=14`、
14 条迁移、`entries.modified_at` 仍在（本次不迁移）。10 项无关服务 PID/InvocationID、3 个本机应用 200 与主机锁逐字节保持。

生产验收（`artifacts/S133/verify-production.py`，exit0，`production.json` passed=true）：线上全量 `assets/*` 与包内逐字节一致，
`index.html` 除 CSP nonce 外一致、CSP 无 `unsafe-inline`，TLS 1.3，owner 登录成功、注销 200。S132 断言：表头恰为
`名称/大小/修改日期`（无“操作”列）、`table-layout: fixed`、三列 574.1/129.6/222.3 px（份额 62/14/24 ±2%）、数据行三格、
不存在“查看…的属性”行尾按钮；右键首行菜单在光标处打开（±12/16 px）且项为 `打开/属性/复制路径`，Esc 关闭后
`Shift+F10` 重开同一菜单，“复制路径”写入剪贴板 `/新建文件夹` 且提示 4 秒后消失；明暗×320/1440 目录树 14px 图标、无页面溢出、
无 CSP/页面错误。生产浏览审计只有 4 个请求（`GET /v1/vaults`、`POST …/snapshots`、`GET …/entries?limit=1000`、`POST /v1/auth/logout`），
无内容或操作写入。

证据 `/data/vaultsyncd-deploy/local-20261108T194501-3012226/`（result、旧 generation、旧 system/boot GC roots、锁、pin diff）；
筛选副本在 `artifacts/S133/`。恢复入口为部署前 generation254
`/nix/store/6xq77p0sdy0zh5il41mqqbi3kva8jsp5-nixos-system-wks-lab-26.11.20261102.7356cc7`：本卡不改库结构，回退只需
`sudo nixos-rebuild test --store-path <该旧路径>` 并同步回退主机 pin，不必恢复数据库；未实际激活旧 generation。
限制：生产只读浏览，未做生产内容写入/恢复/故障注入，未做 Windows 原生与规模验收；升级前已存在的条目 `modified_at` 仍为 NULL，
界面显示 `—`。详见 [S133](../../agent/tasks/S133.md)。

## S135 Mantine Notification 服务端部署（2026-11-08，generation256）

固定项目 `faf7adb6dc3b32e97d7384842899c307cde1464b`（S134 实现 `a1d225a` 与交接），执行现有 `tools/deploy-local.sh --rev <SHA>`：pure build、manifest、dry-activate/test、HTTPS/IPC/ServerId/归属、switch 全通过。主机仅更新版本 pin，提交 `b98e05fec08fb88e333a155a5bd9d02e98a3ea20` 已 ff main，临时主机树/分支已清理；锁 `1ee4edaf…` 不变。

运行及开机默认 generation256，system `/nix/store/w9jrk25vqz0iqqzsrq1v40bn8zw9vahf-nixos-system-wks-lab-26.11.20261102.7356cc7`，包 `/nix/store/vmnd65vsc2ja1fblbdf36zbdr5vh3zfh-vaultsyncd-0.1.1`；无 Rust/迁移改动，库保持14，ServerId/Vault/归属保持。线上资产与包内逐字节一致、index 除 nonce 外一致；严格 TLS1.3 Chromium 登录、右键/Shift+F10 复制路径的 Notification、四秒自动消失、Enter 关闭、明暗×320/1440 无溢出/布局位移/CSP错误通过。10项无关服务 PID/InvocationID 和3应用200保持。生产仅只读元数据浏览及剪贴板操作。

证据 `artifacts/S135/` 与 `/data/vaultsyncd-deploy/local-20261108T201005-3057280/`，passed/cleaned=true，含旧system/boot GC roots。恢复入口 generation255 `/nix/store/a86anl2i29i5q3kcx9jqay6p2a2a6ngc-nixos-system-wks-lab-26.11.20261102.7356cc7`，先 `sudo nixos-rebuild test --store-path <旧路径>` 核对健康，再决定 switch，并同步回退主机 pin；无数据库迁移，不必恢复库。本次未实际回退演练、未做生产内容写入或故障注入，不替代 Windows和规模门禁。详见 [S135](../../agent/tasks/S135.md)。

## S138 文件操作与重命名版部署（generation258，2026-11-08）

固定源`5689be16b8bc389ae3781e96c2ff734f7d6761ae`包含S136文件操作、S137重命名/树菜单及连续目录改名观察器防误删。现场原已为源7f50cc3/generation257/schema15，纠正此前文档仍写S135的滞后；本次15→15不迁移。

部署前经同UID管理socket完成完整prepare/verify-source/export/verify/release，归档`/wrd/prj/vaultsyncd/artifacts/S138/predeploy.vsbk`与0600收据10文件verified，目录0700。源/data(sdc1)与导出/wrd(sdb1)异盘、仍同主机；不宣称异地备份或实际恢复演练。原状态、Vault与配置保留；历史14→15SQLite备份已只读核验，不代替整套归档。

现有脚本pure build→dry→test→健康→switch全部通过，主机pin`9930ec77046abed5744a35f1b672593270bc485e`已ff main并清理临时树/分支，锁不变。运行/开机generation258，system`/nix/store/g16gzr5crzcz29nfpjnjg3j2r7ga25pa-nixos-system-wks-lab-26.11.20261102.7356cc7`，包`/nix/store/hr90dm2vbghdwa9vq8i62ipsn4fqa206-vaultsyncd-0.1.1`。严格TLS1.3生产Chromium全量资产、列表与树右键/键盘/取消改名/Vault保护、明暗320/1440、通知与CSP通过；旧布局固定版本内容长度及SHA256匹配，8对象映射/字节、schema15/ServerId/归属与10无关服务/3应用保持。生产只读元数据/固定版本与取消弹窗，无文件操作提交。

永久证据`artifacts/S138/`及`/data/vaultsyncd-deploy/local-20261108T224057-3442489/`（passed/cleaned=true，旧system/boot GC roots）。直接回退为generation257：`/nix/store/r3fvjjgnknna7rm5y7nc9aqifnxmbslc-nixos-system-wks-lab-26.11.20261102.7356cc7`，按S113先`sudo nixos-rebuild test --store-path <旧路径>`、健康后再switch并同步pin；本次无迁移，不降schema。未实际回退、生产恢复或故障注入；原Windows/规模门禁保持，详见[S138](../../agent/tasks/S138.md)。

## S145 统一登录入口部署（generation261，2026-11-09）

当前固定源 `6e58bc26fcef09faf675229889693b593f6e9c4f`，主机pin `18a7df5e9ef087ba7bc65d83f1c98b919ebbc5eb`；由S113入口完成pure build、dry-activate、test、HTTPS/IPC/身份/归属、switch，主机临时树/分支自动清理。运行和开机generation261，system `/nix/store/ygn41yw7w7fhqkg7xd6djgn09rqqq53x-nixos-system-wks-lab-26.11.20261102.7356cc7`，包 `/nix/store/cqm1vcah29i9vklv5fhhmah5fr28frlg-vaultsyncd-0.1.1`。

服务端Web未登录仅显示独立登录页，管理导航与业务页不挂载，登录后恢复原hash/目录深链；退出/改密/撤销/最终401收回整个界面。静态登录资产与公开health/capabilities端点仍开放，业务API保持服务端授权。真实生产Chromium（禁QUIC以固定TLS1.3验证，不跳过证书校验）通过8路由匿名门禁、登录后文件浏览及刷新、退出后后退仍拦截、cookie/CSP、明暗320/1440；全量线上静态资产与包逐字节匹配，无页面/CSP错误。ServerId `9395d9cacd7d3fb4b746592806a8b5cb`、schema17、管理员xingjian、10项服务/3应用及主机锁保持，不清库不改生产内容。

证据 `artifacts/S145/` 与 `/data/vaultsyncd-deploy/local-20261109T145920-429779/`（passed/cleaned=true）。回退入口S144的generation260 `/nix/store/rq6p480986lr6hhv87qa0fvapr14g6yj-nixos-system-wks-lab-26.11.20261102.7356cc7`，先按S113 test/健康再switch，并同步主机pin；本卡无迁移、不需降库，未实际演练回退。S144清库历史见[交接](../../agent/tasks/S144.md)，本次不重做清库；Windows/规模门禁保持。整合与清理见[S145](../../agent/tasks/S145.md)。


## S146 闲置目录清理与临时数据集中（2026-11-09）

后续路径覆盖：S161 已将本节历史 build/deploy 目录迁入临时根，当前映射及验收见文末 S161；下文旧路径为当时证据。

用户要求清理闲置目录，今后临时任务数据、测试卷、下载与缓存统一在 `/data/vaultsyncd-temp`，按任务/run_id或cache用途分目录。根目录本机为xingjian:users 0711，私密证据与备份子目录0700；不可把整个临时根当成单次任务的删除目标。新部署证据默认 `/data/vaultsyncd-temp/evidence/deploy`。历史 `vaultsyncd-build` / `vaultsyncd-deploy` 仍保留原路径的原始验收资料、备份与GC roots，后续不在这些历史目录新建任务。

清理前逐项核对挂载和进程打开的路径；清理36项、`du`合计11,084,029,952字节（约10.3GiB），包括S097/S098编译缓存、退役ARM64虚拟机存储和测试镜像、6个S075测试目录、S096试用镜像与3处旧凭据/开发证书目录。S075原始材料先私密归档。历史build目录剩约18MiB、deploy约87MiB；所有原始日志/结果及发布GC roots、S113快照、S144清库前归档和当前初始化凭据保留。`df`前后未显示相应物理空间下降，/data存在Snapper快照；本次不删除系统/数据快照，不声称释放10.3GiB独占物理空间。

主机提交 `13f0dfc` 已ff合main，撤下旧试用挂载和账号，pure build、dry、test、HTTPS/IPC健康、switch通过；运行与开机generation262、system `/nix/store/ybwlrf3l0fmir63fjz3y1inbwd3m142h-nixos-system-wks-lab-26.11.20261102.7356cc7`。项目服务pin保持 `6e58bc2`、原包/ServerId与正式挂载保持，vaultsyncd/Caddy/SSH的PID和InvocationID不变；整个 `/data/vaultsyncd-lab`（含win-lab、基线、关联缓存）不进入操作清单，VM PID1894045及启动时间保持。

证据 `/data/vaultsyncd-temp/S146-cleanup/` 保存完整删除清单、脚本、验证JSON、容量记录、主机构建/激活日志及旧generation261 GC roots。新目录下真实256MiB btrfs卷create/verify/cleanup及旧lab拒绝检查通过。旧generation只用于配置排障；已删除S096试用数据不能通过切换generation恢复，旧试用恢复流程退役。正式实例/备份保留，本次未实际回退、未重跑Windows原生能力验收，原5张blocked门禁不变。详见[S146](../../agent/tasks/S146.md)。

## S149 客户端连接记录与自动连接版部署（generation263，2026-11-09）

固定项目源`d9cfee975ba2656feffd560b8cf0be46ba516442`，精确主机构建提交`f5153d8a4459ca17f753566ac2f9467b420a37fc`；运行/开机system `/nix/store/7h9r5qm6aw6k0931ldmxyk41xmn08ycw-nixos-system-wks-lab-26.11.20261102.7356cc7`，包`/nix/store/56c15crxzpz7ahharpvwggpmzjl191iz-vaultsyncd-0.1.1`。本轮从13f0dfc纯求值复现旧generation262后，使用其运行依赖执行S113脚本，完成pure构建（73项包内测试）、dry/test/HTTPS/IPC/目录归属/switch；schema17→18成功，保留17迁移备份。

主机main此前d87f48e已提交全量锁更新但尚未激活。为避免顺带升级整机，本轮使用隔离的旧配置，仅把新pin通过`fb293d54877ea20b0de4ab3c875d90ee7036e893`合回main；main锁逐字节保持。**准确复现本次system应引用f5153d8，当前main求值会同时带入尚未验收的主机依赖更新。** 不要把main现有锁误报为运行锁；后续整机升级需独立build/test/健康/switch。

正式Chromium通过严格TLS1.3、9路由匿名门禁、客户端当前连接/浏览器类型/刷新保持身份、登录与刷新自动连接且换页不重复、文件浏览、退出/后退、CSP/cookie和全部静态资产逐字节一致；明暗320/1440无页面横向溢出。ServerId `9395d9cacd7d3fb4b746592806a8b5cb`、账号、Vault/条目/版本/对象计数及正式文件摘要保持，10服务PID/InvocationID、3应用200保持。

升级前完整归档`/data/vaultsyncd-temp/S149-deploy-20261109T095905Z/backups/pre-upgrade.vsbk`已校验并在另一文件系统`/wrd/prj/vaultsyncd/artifacts/S149/backups/`保存校验副本；旧包已restore validate/apply到隔离空目录，schema17/FK及对象/工作文件摘要匹配。回退不能只切旧generation262：须按备份文档恢复整套schema17到新空状态/Vault目录、更换epoch和重新登录，再更新配置及pin。未实际回退生产。部署日志、before/final/result、截图、恢复校验与旧system GC roots保留在该run的`evidence/`；详见[S149](../../agent/tasks/S149.md)。原5张blocked卡不变，无下一ready。


## S153 版本历史版服务端部署（generation265，2026-11-09）

源`89a9cf4ef47b9b2af1ee2cedff6b82c10c082205`包含S150文件右键版本历史，生产入口 https://vsyncd.132140.xyz 。沿用S151已验收路径`/data/vaultsyncd/vaults`，schema18不变。现有deploy-local入口以c20199e隔离主机配置pure构建，73包内测试、dry/test/HTTPS/IPC/身份/归属/switch全部通过，运行与开机generation265，system `/nix/store/0jnii77r4410z7ffq2ak1s820b021iq4-nixos-system-wks-lab-26.11.20261102.7356cc7`。主机pin `d4c5fe7099f168bcdf4433da4b78a986ad730b21`已由main `2c96e2fed546f62a80d4a85623aee729b193bced`整合，main未激活依赖锁升级保留；精确复现本次运行应使用d4c5fe7，而非main当前锁。

生产真实Chromium的TLS1.3、全量资产、匿名门禁、登录/自动连接/客户端页/退出、文件版本号与摘要、刷新/键盘/Escape、明暗320/1440、CSP/cookie全通过。ServerId、schema/FK、Vault路径/历史、4工作文件和3对象摘要及10既有服务/3应用保持；未写生产同步内容。完整备份`/data/vaultsyncd-temp/S152-deploy-20261109T103922Z/backups/predeploy.vsbk`及异盘`artifacts/S152/backups/`副本/收据均verified。原始脚本/结果/日志/截图、部署result及GC roots在该run的`evidence/`。本卡因并行S152桌面任务而改编号S153，既有运行目录名保留。

回退入口generation264 `/nix/store/qdd7vqh5k24dwlrxwzcgsn6mp9f75k0y-nixos-system-wks-lab-26.11.20261102.7356cc7`，可按S113先`sudo nixos-rebuild test --store-path <该旧路径>`并确认HTTPS/IPC健康，再switch并同步主机pin至c20199e的旧源。此次无schema迁移，保留S151的vaults路径；若要恢复备份仍须按备份协议恢复到空目录并更换epoch。未实际回退、生产恢复或Windows/规模复验；原首发门禁保持。交接与清理见[S153](../../agent/tasks/S153.md)。


## S159 Vault 路径与容量版部署（generation268，2026-11-09）

当前服务源911449b、schema18，服务器路径可点击打开对应Vault根目录，“文件容量”统计当前已提交文件的逻辑总大小。运行/开机system为pwcp1j07c6mx6ln9a0z6n8whabhds0jk，主机配置2cfc2bf（main）纯求值复现；公网TLS/资产/浏览器、HTTPS/IPC及正式数据保持均通过。本轮期间其他主机任务已完成全系统更新和修复，当前基线3461249取代S153/S157的旧锁复现入口。

完整备份、日志、截图、部署result及旧system/boot GC roots永久保留在`/data/vaultsyncd-temp/S159-20261109/`，异盘校验备份在`artifacts/S159/backups/`。本次无schema迁移；回退先按S113对旧system `/nix/store/8mjzl3ql0chn39mba7cv2529rf0h0n3n-nixos-system-wks-lab-26.11.20261109.5e6fbb3`执行test并核对HTTPS/IPC，再switch并同步主机pin至3461249。不要退回旧全系统锁而撤销另一任务的主机升级；若恢复备份仍需按备份协议恢复到空目录并换epoch。本次未实际回退，详细证据、验证及清理见[S159](../../agent/tasks/S159.md)。


## S160 服务概览 Server ID 部署（generation269，2026-11-09）

源`85dd2bb9ed427a13ec2437bea69038f8300718ec`在概览服务连接卡片显示完整Server ID，窄屏自动换行，供选中复制。沿用GET /v1/capabilities，无wire/schema迁移。以实际主机干净main `2cfc2bf`执行既有`tools/deploy-local.sh --rev HEAD --evidence-base /data/vaultsyncd-temp/S160-20261109/evidence/deploy`，pure build、manifest、dry/test、HTTPS/IPC/身份/归属与switch通过，result的passed/cleaned=true。主机pin `a37df3131590f01e61e4cd1532f9d910d974e3ac`已ff main且任务树/分支清理；锁74319cf7…保持。运行与开机generation269，system `/nix/store/p744ig0hmksv3nw68bfb6vhiy3z1abpc-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，包`/nix/store/k33fl5g1mjfir3sn6wfb5s0cvfkgydaa-vaultsyncd-0.1.1`。

生产真实Chromium验证编号与API一致、完整选择/只读、刷新、明暗320/1440完整显示与无横向溢出、严格TLS1.3/CSP/cookie/逐字节全量资产，登录退出正常，无pageerror。正式ServerId、schema18/FK、5工作文件摘要、10既有服务PID/InvocationID及3应用保持。原始脚本、日志、截图、result与部署记录永久保存`/data/vaultsyncd-temp/S160-20261109/evidence/`，部署run为`deploy/local-20261109T201823-1530268/`。完整备份及收据在该任务`backups/`，异盘`/wrd/prj/vaultsyncd/artifacts/S160/backups/`已独立verify。

旧system/boot GC roots保留；直接回退入口generation268 `/nix/store/pwcp1j07c6mx6ln9a0z6n8whabhds0jk-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，按S113先test核对HTTPS/IPC再switch，并同步主机pin至2cfc2bf。本卡未实际回退、未做Windows或容量复验；原blocked首发门禁不变。项目合并清理见[S160](../../agent/tasks/S160.md)。

## S161 历史 build/deploy 资料迁移（2026-11-09）

当前归档位置为 `/data/vaultsyncd-temp/vaultsyncd-build`（原 `/data/vaultsyncd-build`）与 `/data/vaultsyncd-temp/vaultsyncd-deploy`（原 `/data/vaultsyncd-deploy`）；旧目录和兼容软链均不存在。历史命令、日志和收据不批量改写，查阅或恢复时仅替换这两个根前缀。这些归档仍含长期证据、备份、发布/回退 GC roots，不能按普通临时缓存删除。

同文件系统rename保留全部482个普通文件的SHA256/元数据和目录inode，S113只读btrfs快照仍ID2663，UUID与只读标志保持；72个GC roots先临时固定再登记新路径，验证后移除旧登记及临时根。首次核对发现67个根链接属主被登记命令改变，恢复属主后639项完整清单一致。服务进程身份、当前system及HTTP/严格TLS的HTTPS健康保持，无重启/rebuild；不触碰正式目录和lab。

原始plan、前后清单、登记日志、快照/服务身份、健康结果及result.json（passed=true）在 `/data/vaultsyncd-temp/S161-relocate-20261109T124412Z/evidence/`。Nix print-roots实际识别72个新根；枚举附带移除4个既有失效auto链接及2个陈旧temproots文件，未删除store产物或实际系统回退。操作记录与限制见 [S161](../../agent/tasks/S161.md)。

## S163 开发根迁移（2026-11-09）

当前 `/data/vaultsyncd-*` 顶层仅保留 `/data/vaultsyncd-dev`，temp/lab整树在其同名子目录中，旧入口无兼容软链；S161中的历史build/deploy归档也随temp迁入。所有新开发测试目录（含VM、缓存、部署证据与备份）只能位于dev根，长期证据不得按cache清理。正式 `/data/vaultsyncd`、`/var/lib/vaultsyncd` 和当前system/boot不改，无服务部署或rebuild。

97个GC roots登记新路径并由Nix识别；归档只读子卷ID2663、UUID与ro=true保持，243269项清单及5399稳定文件摘要通过，服务/VM进程身份与HTTP/HTTPS、客体SSH/monitor健康保持。原始证据与检查边界见 `/data/vaultsyncd-dev/S163-relocate-20261109/evidence/` 及 [S163](../../agent/tasks/S163.md)。

主机旧部署任务 `chore/S113-deploy-local-20261109T172110-662492` 的worktree现为 `/data/vaultsyncd-dev/vaultsyncd-temp/evidence/deploy/local-20261109T172110-662492/host`；已执行git worktree repair。其 `hosts/wks-lab/default.nix` 未提交修改原样保留，不代表当前运行配置，不自动合并或清理。


## S170 登录 IP 与五分钟连接状态部署（generation270，2026-11-10）

源`df8edc893d370f4a843c922f4222b45a36215d32`，沿用`tools/deploy-local.sh --rev <SHA> --evidence-base /data/vaultsyncd-dev/S170-client-ip-20261110/evidence/deploy`。主机现场基线a37df31、generation269，主机锁74319cf7…保持；pure package/主机build、dry-activate/test、HTTPS/IPC/身份/归属及switch均通过。主机仅更新pin，提交`7787429ac677dbe335a1e693c599f5e2f25c71eb`已ff main，主机树/分支清理；result passed/cleaned=true。

运行与开机generation270，system `/nix/store/z9mh48x55z908gn8fv2v8dszbd2l7ba5-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，包`/nix/store/g7mm5zrdzv2y98xyfd45p42dmz612r23-vaultsyncd-0.1.2`。schema18→19新增首次登录IP；已有连接IP保持未知，重新登录记录新连接来源。既有Caddy显式覆盖X-Forwarded-For为remote_host，后端仅信任loopback提供的单个合法地址，其他直连用TCP来源；IP不用于授权。有效会话五分钟内（含300秒）认证活动显示已连接，否则未连接，每30秒刷新已加载分页，不表示同步完成。

证据根`/data/vaultsyncd-dev/S170-client-ip-20261110/`：evidence/backend.log（229 passed/1既有ignored、clippy/生成/边界）、frontend.log、browser-accepted.log、auth-browser.log；客户端真实53记录与四张截图在S170-browser-20261110T092144-2202626。线上evidence/result.json passed=true，Chromium验证登录IP与API一致、Caddy覆盖伪造转发头、自动轮询、本次会话、明暗320/1440、TLS1.3/CSP/cookie及全部11资产；ServerId、5工作文件摘要、schema/FK、10既有服务与3应用保持。无生产内容写入。

部署前完整备份经prepare/verify-source/export/verify/release，backups/predeploy.vsbk及backups-copy副本均verified（同一主机/data盘，不是异盘或异地），新包也能verify旧归档。自动迁移备份`/var/lib/vaultsyncd/server.sqlite.pre-migration-v18-to-v19.bak`只读核验版本18/quick_check通过。直接回退入口为generation269 `/nix/store/p744ig0hmksv3nw68bfb6vhiy3z1abpc-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，旧system/boot GC roots在本卡deploy run保留；旧二进制拒绝schema19，回退须按离线恢复流程恢复整套18归档及配对映射，不能只降二进制或替换库，再同步主机pin至a37df31。未实际回退、未做Windows/性能容量验收，原blocked门禁保持。项目提交与合并清理见[S170](../../agent/tasks/S170.md)。


## S171 Web 右键新建目录与下载部署（generation271，2026-11-10）

固定源`01a45f1edbd9b7e9b4e3a0ada4791956e41e7e9f`，执行`tools/deploy-local.sh --rev <SHA> --evidence-base /data/vaultsyncd-dev/S171-web-create-download-20261110/evidence/deploy`。现场主机干净main基线7787429；pure package/主机构建、dry-activate/test、HTTPS/IPC/ServerId/归属与switch通过。主机只改pin，提交`1513b07fe05f48064b51eb6c94342fdb2463be6b`已ff main并清理任务树/分支；主机锁74319cf7…保持，result passed/cleaned=true。

运行/开机generation271，system `/nix/store/vgv5l88qmyj537w7xmz7pfwmzf6x5lg2-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，包`/nix/store/vi7ji8hpg8jf7sxh2b48k09x9l7gaqkz-vaultsyncd-0.1.2`。无wire/schema变化，库保持19，Vault基目录仍为`/data/vaultsyncd/vaults`。Web空白区/目录树/目录行可新建子目录，文件可下载点击时版本并在长度/SHA-256校验后按原名另存；目录不提供打包下载，最终Blob受浏览器资源约束。

证据根`/data/vaultsyncd-dev/S171-web-create-download-20261110/`：基线、根类型/Web七套与构建、真实Chromium+btrfs30项（含新建取消/无效/同名/目标、丢响应原OpId查询、空/Unicode/跨4MiB下载和损坏拒绝）、四张新建明暗320/1440截图；Nix包内74项测试。`web-artifacts.json`核对Nix Web与浏览器验收dist的12文件一致；线上`evidence/result.json` passed=true，严格TLS1.3/匿名文件深链门禁/明暗窄屏、全部11资产和CSP通过，ServerId/schema19/FK/对象/5文件/10服务/3应用保持。历史凭据路径已不存在，未重置密码，生产未复跑已认证文件操作；业务经独立实例实证。

部署前通过prepare/verify-source/export/verify/release取得整套19备份，`backups/predeploy.vsbk`与`backups-copy/`重读验证通过（同机同/data盘，非异地）。恢复入口为generation270 `/nix/store/z9mh48x55z908gn8fv2v8dszbd2l7ba5-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，旧system/boot GC roots在部署run长期保留；可先test旧system并核对健康，再switch及同步主机pin至7787429，无schema变化无需仅因降二进制恢复库。未实际生产回退，未做Windows/性能容量或64GiB浏览器下载；原首发门禁保持。项目整合/清理见[S171](../../agent/tasks/S171.md)。


## S172 同步与传输记录部署（generation272，2026-11-10）

固定源`d7339f06f89527c03c150bc69a1d316cbcdd9605`，以`tools/deploy-local.sh --rev <完整SHA> --evidence-base /data/vaultsyncd-dev/S172-sync-records-20261110/evidence/deploy`完成pure build/dry-activate/test/HTTPS与IPC/身份/归属/switch。主机基线1513b07，pin提交`7a74069b3641d83172322864161e86a05878a406`已ff main并清理任务树/分支；锁74319cf7…保持，结果passed/cleaned=true。未更新全系统依赖。

运行及开机generation272，system `/nix/store/x0sx0d9cp8dmr43z64318ry53nam1l3d-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，包`/nix/store/s3ampibvg57zdd274z0k1dcjx6lxdy6b-vaultsyncd-0.1.2`，内含75测试通过。schema19→20新增活动与受理来源，IPC2.8，capabilities新增file_activity；Vault基目录和身份保持。Web `#/activity`及客户端对应入口可看时间/客户端/路径/方向/版本/大小/区间/结果，仅从本版开始记录，不把served当客户端已收到。

证据根`/data/vaultsyncd-dev/S172-sync-records-20261110/`：两端测试、真实浏览器活动/OPFS备份、Windows26300原生27项及IPC版本门禁；原生628源码输入与提交树一致但候选是在dirty基线上构建，未公开发行。schema20非空账本真实导出与独立1GiB btrfs故障恢复通过，loop已卸载且原始镜像保留。线上`evidence/result.json` passed=true：严格TLS1.3、匿名记录深链门禁/接口401、明暗320/1440、CSP、13资产及index，Nix与验收Web14文件逐字节一致；schema20/FK/对象、5工作文件及10服务/3应用保持。生产无当前凭据，未重置账号，已认证业务只在独立实例验收；生产活动表初始0，未伪补旧记录。

升级前prepare/verify-source/export/verify/release取得整套schema19归档`backups/predeploy.vsbk`及第二同盘副本并重读verify；另隔离恢复该归档、升级20、健康/FK/5文件摘要通过（`evidence/restore19-preview.json`）。正式库自动一致备份`server.sqlite.pre-migration-v19-to-v20.bak`自检与版本19通过，但不能代替整套恢复。generation271旧system/boot GC roots在deploy run保留；旧system为`/nix/store/vgv5l88qmyj537w7xmz7pfwmzf6x5lg2-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，旧pin1513b07。旧二进制拒绝schema20，回退须停服、按离线恢复流程将已验证19归档恢复到新空状态/Vault映射并换epoch，再配置旧pin/路径、test健康后switch；不能只降二进制或手改schema。未实际生产回退，原性能容量/多机/首发门禁保持。项目整合与清理见[S172](../../agent/tasks/S172.md)。

## S181 密码策略放宽与管理员密码重置部署（generation273，2026-11-10）

用户要求取消密码最短长度限制并把本机管理员密码重置为 `1`。固定源`afbbc78e48059d8f504673c7d315beed99c7f15d`，执行现有`tools/deploy-local.sh --rev afbbc78`：pure 构建、manifest 来源核对、dry-activate/test/健康/IPC/UID归属/switch 通过，主机仅改版本 pin（提交`a6a335b`，`/etc/nixos` main ff 合入，锁保持`74319cf7…`）。

运行/开机generation273，system `/nix/store/60mnizya7nhgbn8v88ybhbjj8v5kxpw6-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，包`/nix/store/5azvb3s7bfnm2aimsygjabh8w8c8sps8-vaultsyncd-0.1.6`（`sourceInfo.commit=afbbc78`）。无 wire/schema 变化，库保持schema20；ServerId `9395d9ca…`、Vault 基目录与既有业务数据不变。部署证据`/data/vaultsyncd-dev/vaultsyncd-temp/evidence/deploy/local-20261110T180022-2783874/`（`passed=true`、`cleaned=true`，旧 system `x0sx0d9c…` 已留GC root）。

密码重置按用户选择的一次性离线改库执行，证据`/data/vaultsyncd-dev/S181-password-reset-20261110T180259/`：用新包内`vaultsyncd-admin init`在隔离状态目录生成 `1` 的 Argon2id 散列（生产参数`m=19456,t=2,p=1`，临时库不参与生产），停服后整库备份`server.sqlite.before`（sha256`c6579c3c…`），单事务改写1条账号散列并把37条有效会话置`revoked_at`；`PRAGMA integrity_check=ok`，改后 sha256`46ecd34e…`。启动后`/v1/health`在loopback与`https://vsyncd.132140.xyz`均200，错密码401，用`1`登录200并`/v1/auth/account`返回`xingjian`，验证会话已logout，最终无有效会话。

限制：该决定有意接受弱口令风险，不替代首发安全门禁；如需可审计的在线重置入口须另立卡。回退用旧代际`x0sx0d9c…`（先test/健康再switch并同步pin）或上述停服备份，二者都不降级数据库。

## S202 记录自动刷新部署（generation278，2026-11-12）

固定项目源`50b518584f69cd59ad2e27e945ad5dd967c97823`，主机pin`9e948a484ec0d4f1ca63fdd2fd6107625dfccb57`已ff合main并清树。沿用`tools/deploy-local.sh --rev <SHA> --evidence-base /data/vaultsyncd-dev/S202-activity-live-20261111/deploy`完成pure build/dry/test/HTTPS与IPC/身份和归属/switch；运行及开机generation278，system`/nix/store/k4bxgcd0yflgd1l7zx2nkpbmfkm3cjp1-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，包`/nix/store/bbqjq3qd0adjdj6x6z5r5sxsav18lsp2-vaultsyncd-0.1.7`。本卡无服务端schema变化，锁、ServerId、Vault路径保持。

两源health/capabilities及28项静态资产/index比对通过；index按实际CSP nonce精确复原，资源原始字节/SHA256一致。匿名活动页门禁、CSP/style nonce及nosniff通过，未登录或修改生产文件。证据及截图见该开发根下`result.json`、`verification/`、`deploy/local-20261112T081942-881429/`（passed/cleaned=true）。旧generation277的system/boot GC roots保留；直接回退可先test `/nix/store/f4w0a9j1al2ssfjpqc7i839khi91aaqz-nixos-system-wks-lab-26.11.20261109.5e6fbb3`、核对健康后switch并同步主机pin至f7ee4dd；本次未实际回退。完整验收与客户端交付见`agent/tasks/S202.md`。

## S205 Web 正常连接静默部署（generation279，2026-11-12）

固定项目源`5e33611e6256d8654e15b2645c4aaebd2545693d`，主机pin`71c9af4dc2bac7896e2ad87fdc55f68b6c1f5797`已ff main并清树。执行`tools/deploy-local.sh --rev <SHA> --evidence-base /data/vaultsyncd-dev/S205-web-connection-20261112/deploy`，pure构建/dry-activate/test/HTTPS与IPC/身份/归属/switch全部通过；运行及开机generation279，system`/nix/store/4ncbab1h79c32kv4yqq240l4vq5kmmhz-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，包`/nix/store/jgpaa4vv914is74akx8bdpw878226lyz-vaultsyncd-0.1.7`。本次无wire/schema迁移，主机锁、ServerId、Vault路径保持。

两源health/capabilities与14项资产/index共28次精确字节比对通过（index只剥离实际CSP nonce meta），匿名活动页门禁/CSP/style nonce/nosniff通过；未登录或修改生产文件。部署包Web精确字节配合同包后端，隔离btrfs上再次通过完整Chromium认证/自动健康检查/静默/异常重试/退出取消/明暗窄屏验收。证据`/data/vaultsyncd-dev/S205-web-connection-20261112/verification/`、`S205-auth-20261112T093812-1016812/`及`deploy/local-20261112T093441-1002616/`（passed/cleaned=true）。旧generation278的system/boot GC roots保留；如需回退，先test `/nix/store/k4bxgcd0yflgd1l7zx2nkpbmfkm3cjp1-nixos-system-wks-lab-26.11.20261109.5e6fbb3`、验证健康再switch，并同步主机pin至9e948a4；本次未实际回退。完整交接见`agent/tasks/S205.md`。

## S206 云传输图标部署（generation280，2026-11-12）

固定项目源`13dbfa776286f4014acc51d522a0ed9222818c4c`，主机pin`652f4744bca7a31c8b9267c76adbbcafff2048f5`已ff main并清树。执行`tools/deploy-local.sh --rev HEAD --evidence-base /data/vaultsyncd-dev/S206-cloud-transfer-icons-20261112094306/deploy`，pure构建/dry-activate/test/HTTPS与IPC/身份/归属/switch全部通过；运行及开机generation280，system`/nix/store/ffw13m972vxi7flkz7hipjy9i1vlx86r-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，包`/nix/store/pp9w0qwapqcjap4yincym58kh6wx80pl-vaultsyncd-0.1.7`。本次仅前端图标资产变化，无wire/schema迁移，主机锁、ServerId`9395d9cacd7d3fb4b746592806a8b5cb`、Vault路径保持。

健康/`vaultsyncd-admin status`（phase=ready）、13项Web资产抽查（index JS/CSS与Files分块）线上与包内SHA-256一致，CSP/style nonce、nosniff正常，匿名`/v1/vaults`仍返回401，未登录或修改生产文件。证据`/data/vaultsyncd-dev/S206-cloud-transfer-icons-20261112094306/deploy/local-20261112T094622-1031794/`（passed/cleaned=true）及同根`icon-browser-20261112T095936-1047003/`（部署包精确字节Web+同包后端Chromium云图标断言与截图）。旧generation279的system/boot GC roots保留；如需回退，先test `/nix/store/4ncbab1h79c32kv4yqq240l4vq5kmmhz-nixos-system-wks-lab-26.11.20261109.5e6fbb3`、验证健康再switch，并同步主机pin至`71c9af4`；本次未实际回退。完整交接见`agent/tasks/S206.md`。

## S208 PDF 与图片预览部署（generation281，2026-11-12）

项目源 `4770492975d61b045d64090da77d05ec4ab36207`，主机 pin `3597d0b1aa9e5d879df7a73ac1c935bf4444a4c7` 已 ff 合 `/etc/nixos` main 并清树；主机依赖锁不变。`tools/deploy-local.sh --rev <完整SHA> --evidence-base /data/vaultsyncd-dev/S208-file-preview-20261112/deploy` 完成 pure 构建、dry-activate、test、HTTPS/IPC/ServerId/目录归属核对及 switch。运行/开机 generation281，system `/nix/store/lv1k5g2vd5sb6al22ac12c8kr6nxdidv-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，包 `/nix/store/9kn3cqsqrcnyqfd7pmmhd13kahc53hd2-vaultsyncd-0.1.7`；无 wire/schema 变化，生产 ServerId `9395d9cacd7d3fb4b746592806a8b5cb` 保持。

生产 HTTPS、开发 HTTPS 与生产 loopback 的201文件共603次逐字节核对通过（index只移除实际nonce meta）；health、身份隔离、CSP/nonce/nosniff、匿名Vault 401及prod/dev真实Chromium匿名深链/窄屏均通过。部署包精确Web+同包后端在独立btrfs再次通过全部16组预览浏览器验收；未登录或修改生产文件。dev 经管理脚本有序 restart 到同包，保留原数据库/Vault/ServerId `fe616633a01bb594d574b8d08626efc3`，不重置账号或重建绑定；生产仍为既有NixOS部署，dev仍非NixOS服务。

证据 `/data/vaultsyncd-dev/S208-file-preview-20261112/`：`deploy/local-20261112T151823-1391598/result.json`（passed/cleaned=true）、`evidence/online-verification.json`、`evidence/online-browser/`、`S208-file-preview-20261112T152147-1404072/evidence/`。旧generation280的system/boot GC roots保留；回退先test `/nix/store/ffw13m972vxi7flkz7hipjy9i1vlx86r-nixos-system-wks-lab-26.11.20261109.5e6fbb3`、健康再switch，并同步主机pin至 `652f474`；dev可显式restart至旧包。未实际回退，完整交接与50MiB/像素限制见 `agent/tasks/S208.md`。

## S209 浏览器内置 PDF 部署（generation282，2026-11-12）

项目源 `23d5c7c2ffb0195245f106bea6115b1fffcf5ac9`，主机pin `bf4857dd4bfc0d4b90449de65911a5174775ad92` 已ff main并清树；主机锁不变。`tools/deploy-local.sh --rev <SHA> --evidence-base /data/vaultsyncd-dev/S209-native-pdf-20261112/deploy` 的pure构建、dry-activate、test、HTTPS/IPC/身份/归属与switch通过。运行/开机generation282，system `/nix/store/xhnglrd8swyda39xqifl83a3sqqji441-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，包 `/nix/store/dil3kh3r7iphfsiqbgzxv6wkq21kx35x-vaultsyncd-0.1.7`；无wire/schema迁移。PDF.js/字体/补丁已删除，PDF由浏览器处理，Web CSP仅新增 `frame-src blob:`；行为与限制见 [S209交接](../../agent/tasks/S209.md)。

prod/dev HTTPS及prod loopback的15文件共45次字节核对、health/capabilities身份保持、CSP/nonce/nosniff、匿名401通过，prod/dev Chromium匿名深链320/1440通过；部署精确包独立btrfs再次通过17组预览验收。dev经管理器restart至同包，原数据库/Vault/账号/ServerId保持。证据同任务根，`deploy/local-20261112T155408-1442682/result.json` 的passed/cleaned=true；未登录或修改生产文件。旧generation281的system/boot GC roots保留；回退先test上节system `lv1k5g2vd5sb6al22ac12c8kr6nxdidv`、验证健康再switch，同步主机pin至 `3597d0b`，dev可restart至上节旧包。未实际回退。


## S210 文本与 Markdown 预览部署（generation283，2026-11-12）

固定源 `e79299a2c88edde05e76a85131268aa94c717f48`，主机pin `aae429264a71479b336ebf3c288b6ff382a94c3f` 已ff main并清树、主机锁不变。`tools/deploy-local.sh --rev <SHA> --evidence-base /data/vaultsyncd-dev/S210-text-preview-20261112/deploy` pure构建/dry/test/HTTPS/IPC/身份与归属/switch全部通过，Nix包内75项测试通过。运行/开机generation283，system `/nix/store/y37pxj5akkip23jmc79qbiy74ycl4q7z-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，包 `/nix/store/c9xkfn6c1gm7lzd86pkab669gk5vj1yx-vaultsyncd-0.1.7`。无wire/schema变化，生产ServerId保持，CSP策略未放宽；CodeMirror样式携带已有nonce。

prod/dev HTTPS及prod loopback 136文件/408次精确字节核对、健康/身份隔离、匿名401/CSP/nonce/nosniff与线上Chromium深链320/1440通过；部署包独立btrfs文本13组、PDF/图片17组全部通过。未登录或修改生产文件。dev管理器有序restart至同包，原数据库/Vault/账号/ServerId保持。证据同任务根 `evidence/` 与 `deploy/local-20261112T161807-1493237/result.json`（passed/cleaned=true），边界和完整验收见 [S210交接](../../agent/tasks/S210.md)。

旧generation282的system/boot GC roots保留；回退先test上节system `xhnglrd8swyda39xqifl83a3sqqji441`、验证健康后switch，并同步主机pin至 `bf4857d`；dev可显式restart至上节旧包。未实际回退。

## S211 管理页面自动恢复部署（generation284，2026-11-12）

项目源 `5a85e64db1e31dc7a11c99120c34b84bfd191347`，沿用 `tools/deploy-local.sh`，pure构建/包内75项/dry/test/HTTPS/IPC/身份与归属/switch通过。主机pin `716e45384da7c91f50ce7239e540b3a5695d1a37` 已ff main并清树，依赖锁保持；运行/开机generation284，system `/nix/store/q08fphxvymbbv91gygcmsfigs1zbszwf-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，包 `/nix/store/6liszy33b10wzpq4xn9aqn7rjamak7i0-vaultsyncd-0.1.7`。无wire/schema变化。prod/dev同包，136文件/408字节、健康/身份隔离/匿名401/CSP与严格HTTPS Chromium320/1440通过；原generation283的system/boot GC roots保留，未实际回退。证据及恢复策略见 [S211交接](../../agent/tasks/S211.md)。

## S212 OnlyOffice 基础服务（generation285，2026-11-12）

主机 `/etc/nixos` 新增 `features.onlyoffilce.*`，实现提交 `229efe4190bcb2f37f5fe8bbf24ee1d8d66feeac` 已ff合main，任务树/分支正常清理；初始化忽略规则提交 `7b3c1f2`。选项及日常维护唯一入口为 `/etc/nixos/README.md` 的「wks-lab：OnlyOffice Document Server」。运行/开机generation285，system `/nix/store/hyqksj0k5r3iwzarnap6wm35wxgjvwp7-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，主机锁和原vaultsyncd的S211源/包保持。

Document Server 为 `https://wlab.132140.xyz:15402`，健康路径 `/healthcheck`，API `/web-apps/apps/api/documents/api.js`，复用wlab证书；内部nginx15403、docservice15404与RabbitMQ/epmd均限回环。JWT为运行时 `/var/lib/onlyoffice-secrets/jwt`，nonce同目录，0640 root:onlyoffice；仅供后端签发授权请求，不把密钥交给前端。默认首次生成并保留，停用feature不删除服务数据。示例服务关闭，根地址的welcome跳转不代表提供可上传的Office网站。

纯构建/dry/test/健康/switch通过；生成的最小DOCX及官方DOCX/XLSX/PPTX样本均完成PDF转换和HTTPS下载，无签名/错误签名拒绝，管理路径大小写/版本前缀/编码403、相对重定向、资源加载、重启密钥稳定与prod/dev健康通过。补齐上游忽略监听host和缺失字体索引配置；首次失败的构建/启动/转换日志及两份core保留在 `/data/vaultsyncd-dev/S212-onlyoffice-20261112/`，不作缓存清理。应用Web尚未接入Office预览，也未进行浏览器Office渲染验收。

回退目标为generation284/system `/nix/store/q08fphxvymbbv91gygcmsfigs1zbszwf-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，其GC root为证据根的 `old-system-root`。先运行该system的 `bin/switch-to-configuration test`，确认SSH与prod/dev健康后将 `/nix/var/nix/profiles/system` 设回旧system并执行其switch；同步撤销主机OnlyOffice配置，避免下次rebuild重新开启。此卡未改项目协议/数据库，不需回退项目数据；新建的OnlyOffice数据库/缓存/密钥仍应保留。未实际执行回退。

## S213 Office 预览部署（generation286，2026-11-12）

项目源 `43ae33dfccbfe1ffcabf5303caf13811dd9d33a4`，主机功能提交 `53c2db05eb1e7386ec0966f9135c67bb1c3cfde5`、pin `429b1cbda7c8644637757aa823032bf6160fc6cc`。`tools/deploy-local.sh --rev <SHA> --config /etc/nixos/.worktrees/S213-office-preview --evidence-base /data/vaultsyncd-dev/S213-office-preview-20261112/deploy` pure构建/dry/test/HTTPS及IPC/身份/归属/switch通过，result passed/cleaned=true。运行/开机generation286，system `/nix/store/4k7a8grgpv5c2d0pij0yliz86gz1cch6-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，包 `/nix/store/8qfvjkalrkdhfdhpig9zh7liix6kwsp8-vaultsyncd-0.1.7`，Rust `494y5s1...`、Web `kj1iy2h...`；主机和项目依赖锁未更新版本（Cargo仅新增服务端直接依赖接线），无数据库迁移或IPC版本变化。

wks-lab通过 `features.server.vaultsync.office.enable` 桥接项目 `services.vaultsyncd.office`，使用既有 `https://wlab.132140.xyz:15402`。systemd LoadCredential代读 `/var/lib/onlyoffice-secrets/jwt`，TOML只指 `/run/credentials/vaultsyncd.service/office-jwt`，Requires/After保证生成完成；没有扩大服务用户组或秘密ACL。生产进程命名空间中的凭据、dev私有副本与实际OnlyOffice密钥均一致。反代头超时130秒、服务停止期限150秒覆盖转换120秒deadline。

OnlyOffice独立与内嵌FileConverter均以失败即停止构建的最小补丁关闭文档network/privateIP，并将allowList置空；Node仍可读取应用的随机回环源。真实旧serializeOptions可信JWT输出true/true、新双副本false/false的调用对照通过。七种外链样本未获阳性对照（FHS缺curl），不能用零请求声称完整网络阻断实证；补丁策略与此证据边界已独立复审。OnlyOffice已入队转换不能撤回，HTTP关闭不会提前释放worker准入，deadline后上游仍可能继续。

server117项、包内80项、根类型/Web九套/构建、生成器21项/生成一致、dev管理器11项、模块credential fixture和最终精确包Chromium14组全部通过。三类真实Office→原生PDF、原件精确下载、授权/固定旧版/故障/重试/取消/CSP/窄屏证据在任务根，最终run `S213-office-20261112T210937-1816154`。prod/dev同包且各自原ServerId保持，136文件在prod/dev HTTPS及prod loopback共408字节比较、匿名新API401、严格TLS浏览器320/1440与CSP通过。没有向生产/持久dev写入验收文档或直接修改在线数据库。

dev经管理器stop/configure-office/start配置同包，保留账号/Vault/身份与本机普通目录；收尾已由永久master入口再次有序启动，原身份和健康保持，supervisor不再引用任务路径。完整日志、截图、样本、core映射及GC roots在 `/data/vaultsyncd-dev/S213-office-preview-20261112/`，不按缓存删除。

回退：证据根 `evidence/old-system-root` 保留原generation285/system `hyqksj0k5r3iwzarnap6wm35wxgjvwp7`；deploy子目录另留补丁过渡test system `1p31kwwy...` 与原boot root。先对明确旧system执行test并核对SSH/prod健康，再设回启动profile/switch，同步主机源pin。回退到S211须在dev停止后 `configure-office --disable-office` 再启动旧包，旧二进制严格拒绝新[office]配置；仅功能/协议增加，无须修改或回退同步数据库。旧OnlyOffice系统会撤掉本次文档网络补丁，回退时同时关闭Office功能；保留服务数据/密钥。未实际执行回退。

S213收尾：项目实现43ae33d/交接50fc007已ff合master，主机53c2db0/429b1cb已ff合main；六挂载与双任务树/分支正常清理，既有其他任务树保持。清理后prod/dev/OnlyOffice健康通过，恢复与原始证据独立保留。

## S214 文件列表列排序部署（generation287）

源 `bf8acc627387523a203d9d105bd1776f87141076`，主机pin `e5caced011456cb5ca682640c2e3f04df25655eb` 已ff合main。既有 `tools/deploy-local.sh --rev <SHA> --evidence-base /data/vaultsyncd-dev/S214-file-sorting-20261112/deploy` 完成pure/dry/test/HTTPS、IPC、ServerId及归属核对/switch，result passed/cleaned=true，主机锁不变。运行/开机generation287，system `/nix/store/mdy6yg7jvhnyjprg39lbg8fc9smh6v74-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，包 `/nix/store/inp5h2n5fa8kp5j0i445ya3wsq9qjva7-vaultsyncd-0.1.7`。仅展示排序，无数据库迁移、文件写入或协议变化。

根类型/Web九套/构建、独立复审、Nix包内80项与精确包Chromium全套文件浏览器通过。dev经永久入口有序restart同包，原数据/身份和prod隔离保持；三源136文件/408次字节核对、严格TLS/匿名401/CSP及最终双环境health=ok。未向持久实例写测试样本。完整证据及旧generation286 system/boot GC roots在任务根，回退可使用其中previous-system/previous-boot；dev按管理器restart指定原S213包，保留现有数据，不重置配置。交接见 [S214](../../agent/tasks/S214.md)。

## S215 ONLYOFFICE原生只读查看器部署（generation288）

发布源 `6a99df4158bab3c3fcc3dca5f2f9196b3df0a2dd`（功能182bfe1，含S214排序），主机pin `65316dc4d48ee0dbedfde0846f0ccd39f537db3e` 已ff合main。既有部署脚本完成pure/dry/test/HTTPS、IPC、ServerId及UID/归属核对/switch，result passed/cleaned=true；主机锁不变。运行/开机generation288，system `/nix/store/38xqv31y8xxgpha1qikk9bia2i29xzh9-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，包 `/nix/store/v5x74fbkhv7wk1xqmhvkd61xfl1r49ca-vaultsyncd-0.1.7`。只增加内存viewer会话与HTTP契约，无数据库或IPC迁移。

最终精确包Office真实Chromium16组（含XLSX工作表0→1→0、签名与明文提权真实拒绝）和包内81项通过，server121项/根类型/Web九套/构建/生成一致及21项生成器/独立复审通过；PDF图片17组、文本13组回归通过。prod/dev同二进制，原账号/Vault/ServerId保持；136文件三源408次字节、严格TLS/匿名POST及DELETE401/精确CSP与凭据接线通过。dev使用永久入口restart，无任务路径依赖。没有向持久实例写测试文档。

Office默认直接DocEditor只读浏览；CSP按配置精确加入OnlyOffice来源，API.js为受信任代码，仅空事件体哈希例外兼容上游Safari属性，不允许非空inline/eval。临时源120秒/两准入，关闭撤销，但已取得的副本/上游缓存不可收回，源准入不等于上游执行并发。旧PDF转换API继续兼容。

证据 `/data/vaultsyncd-dev/S215-office-viewer-20261112/`，最终精确包run `S215-office-20261112T215042-1928363`；部署 `deploy/local-20261112T215205-1930595/` 保存generation287的previous-system/previous-boot GC roots。回退先对该旧system执行test并核对SSH/prod健康，再恢复启动profile/switch及主机pin；dev由管理器restart至S214包 `/nix/store/inp5h2n5fa8kp5j0i445ya3wsq9qjva7-vaultsyncd-0.1.7`，保留账号/Vault/配置，无需回退数据库。未实际执行回退；不删除旧roots或实验原始证据。交接见 [S215](../../agent/tasks/S215.md)。

S215收尾：项目功能182bfe1/组合源6a99df4/交接65aae2b已ff合master；六挂载与本地S215树/分支正常清理，主机pin65316dc已ff合main并自动清理本次部署树。永久dev入口及runtime supervisor保留，证据和generation287回退roots长期保留。

## S217 文件预览间距部署（generation290，2026-11-13）

源 `df87e02bff7231aa19dc686b0905f5a18736404b`，主机pin `9b88c9846a3d55cfd46aeeb0abd05befdf92b174` 已ff合main且部署树/分支清理，锁未变。运行及开机generation290，system `/nix/store/bspn2gwq18a3q25cn3ikil180m93zhac-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，包 `/nix/store/8wkqy5gnqfkfsj389x5vd7ip0p2k6k9d-vaultsyncd-0.1.7`。pure/dry/test/HTTPS/IPC/UID归属/switch通过；dev由永久入口restart同包，原ServerId/账号/Vault/schema21与Office配置保持。精确包图片/PDF17组、文本13组、Office16组及136文件三源408次字节、TLS/匿名门禁/CSP与320/1440通过，无持久验收样本。

证据 `/data/vaultsyncd-dev/S217-preview-spacing-20261112/`（实际2026-11-13），部署目录 `deploy/local-20261113T082210-2590107/` 的previous-system/previous-boot保留generation289。回退可先test旧system `yaiwki5p97qn9x5889vp392vj6hlff5y`，核验SSH/健康后switch，主机pin恢复4e06276；dev通过永久管理器restart到S216包 `lb4av1rszg555rdkcfhp8baz5s7dbcs3`，不回退数据库或重置身份。本轮未实际回退/重启宿主，不发行推送或重打包Windows；交接 [S217](../../agent/tasks/S217.md)。

## S216 文件预览标题栏部署（generation289）

源 `b62e45c7589e2dd157a6a186eab1ccbb89b148ba`，主机pin `4e06276bdbd7dad45f38309c5466eb4e4490a00c` 已ff合main且临时树/分支清理。运行/开机generation289，system `/nix/store/yaiwki5p97qn9x5889vp392vj6hlff5y-nixos-system-wks-lab-26.11.20261109.5e6fbb3`，包 `/nix/store/lb4av1rszg555rdkcfhp8baz5s7dbcs3-vaultsyncd-0.1.7`；主机锁未变，无schema迁移。pure build/dry/test/HTTPS/IPC/UID归属/switch通过。dev从永久入口restart同包，各自ServerId/账号/Vault与dev Office配置保持；136文件三源408次字节、严格TLS/匿名门禁/CSP及最终精确包三套预览17+13+16组通过。

证据 `/data/vaultsyncd-dev/S216-preview-header-20261112/`；`deploy/local-20261112T223357-1987644/previous-system` 与 `previous-boot` 保留generation288 system `38xqv31y8xxgpha1qikk9bia2i29xzh9`。需回退时先对该旧system执行test，核对SSH/健康后再switch并将主机pin回65316dc；dev通过永久管理器restart至S215包 `v5x74fbkhv7wk1xqmhvkd61xfl1r49ca`，不回退数据库或重置身份。本轮未实际回退。交接 [S216](../../agent/tasks/S216.md)，无本卡部署阻塞，不发行推送或重打包Windows。
