# 本机 NixOS 隔离试用实例

当前为S097的HTTPS试用 **generation240**，后端固定fd2f52d、回环TLS18443；最新来源、证书及Root恢复限制见下方“S097 当前 HTTPS 开发入口”。前半部分保留S096部署/回滚的历史证据。

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

## S097 当前 HTTPS 开发入口（generation240）

S097 已在同一真实主机执行两轮 build → dry-activate → test → 严格 TLS/健康检查 → switch，第二轮修复实际服务重启的 Root 身份误报。当前主机提交 `ebd832b7bbe751bfc87cbd9b9260ec53a1a26efe`，实际运行与默认 **generation240**：

```
/nix/store/51r012i21j0i26br4319xi8k5f0sdvym-nixos-system-wks-lab-26.11.20261102.7356cc7
```

主机原 dirty `flake.lock` 仍为前述SHA256，未更新或提交。`hosts/wks-lab/system.nix` 的后端固定永久仓库 `git+file:///wrd/prj/vaultsyncd?rev=fd2f52d2f58978b09fccaec3ba87b2377910a1eb`，实际重新构建的包是 `/nix/store/sw8v63x3hv5hql5qfapx0qyrlgc78ikn-vaultsyncd-0.1.0`；不是旧S094产物。代理配置独立固定 `d90a263d962795000aeb2eecfd430891a94a26d0` 的 `deploy/reverse-proxy/caddy.nix`。两源已在master永久可达，均不引用任务树。构建清单保留各自真实来源；本卡PE与包是0.1.0，不能重标为S100后续0.1.1。

提交后的主机配置连同原dirty锁再求值仍产生上述运行system。精确配置源`/nix/store/ky73gbi10wr9xbddjxyhf0n0df0wnk0i-source`、NAR `sha256-x0+sTSHtxF4sUhTQRZlYnzbFYbOunlktgo7+f2kLWQo=`及当前system均有证据目录中的GCroot；主机commit不能单独代替这份锁来源。

| 路径/入口 | 当前保证 |
| --- | --- |
| `https://localhost:18443`、`https://127.0.0.1:18443` | Caddy仅监听127.0.0.1，新vhost复用既有Caddy |
| Windows `https://10.0.2.2:18443` | Quickemu NAT访问同一开发入口，实际Edge与产品PE验收 |
| 后端 `http://127.0.0.1:18721` | 回环HTTP，仅供本机TLS反代与管理检查 |
| TLS运行时目录 | `/var/lib/vaultsyncd-tls-s097`，root:caddy0710；ca.key root0600，server.key root:caddy0640 |
| 公开开发CA | SHA256 `26:2F:E2:25:3A:8A:27:A0:5A:A2:1C:D1:B5:89:C6:81:12:EB:C4:79:9E:39:F9:80:6D:DF:E4:A8:47:E6:F5:7D` |
| 证据 | `/data/vaultsyncd-deploy/S097-20261107T012157-3886231`；任务artifacts只含筛选的公开材料 |

此入口使用显式信任私有CA：宿主浏览器只在临时NSS HOME导入；Windows在核对上述指纹后导入Quickemu当前用户Root，信任仍保留。叶证书SAN为localhost/127.0.0.1/10.0.2.2，有效30天，CA365天；到期须用同样受限运行时流程续签并reload，私钥不得复制到store、Git或证据。**没有公网域名、ACME、外网监听或公开部署验收**。正式对外前须另行配置真实域名证书、访问范围及证书续期；开发CA不能作为公网信任声明。

Web/API保持同源；代理保留原Host，覆盖受信转发字段并去掉客户端Forwarded，不启用跨源写入。request body上限16777216字节，真实最大块成功，+1字节413；`flush_interval -1` 与5分钟stream_close_delay用于SSE，没有整个响应体超时或自动重试非幂等写。只reverse_proxy，不提供任意file_server或Unix管理socket代理。页面、管理API仍受服务端会话/owner权限；路径探测包括admin/socket、在线DB、/etc/passwd和密码路径均404。会话cookie为Secure/HttpOnly/SameSite=Strict。

Linux Chromium和Windows Edge实际完成HTTPS页面登录、16MiB上传、finalize、committed、固定版本下载摘要与磁盘一致，以及SSE初始seq、15秒心跳和新提交提示。新浏览器不信任私有CA时失败，信任正确CA后错主机证书仍失败。正式receipt PE源cf1d174（agent SHA256 `c25e36a7236edcdc5bcef405760cf1df1df6d599de05b7b426c629c0683a890c`）在Quickemu Interactive/Limited任务中验证默认CA拒绝、显式ca_file登录与Credential Manager、同用户CLI IPC logout。SSH会话1312只说明缺交互凭据，不算TLS失败。密码经同SID+SYSTEM受限命名管道/匿名stdin，不写Windows临时文件；本卡自身agent、计划任务、Edge profile及临时监听已清理，没有重启客体或触碰其他测试用户。

早期Playwright异常格式曾在临时输出中带两枚本次测试cookie，已立即用对应会话logout吊销并清理原值，失败日志不归档；没有输出密码/私钥。最终脚本只记录错误类别与源码位置，finally吊销自身会话，禁止导出HAR/cookie/profile。Windows首轮外层500秒控制器在人工CA提示时超时，内层实际worker退出0且随后Edge矩阵通过；最终900秒控制器改动仅原生语法校验，不声称重跑了整条原生链。

## S097 Root 身份修复与恢复限制

旧注册快照含mount_id1031/dev0:128；当前服务namespace现场为mount916/dev0:110，但FSID `a3f322713ab8458f9af3666696536cf7`、subvol5、目录inode258及birth `1793984671.665929329`完全相同。原管理健康消费者直接比较历史瞬态字段而误报mount_changed；实际上传仍成功。现在启动时完整核对持久身份、CoW/访问权/嵌套准入，再为本进程建立共享RootHandle；诊断、观察、执行与启动重放均使用此表。历史blob不修改，新注册继续严格比较当前围栏，活进程bind/卸载/权限变化不会自动重新准入。目录枚举使用固定fd的openat点目录取得独立游标，避免并发扫描污染偏移造成假空目录。

`fence-acceptance.py`以独立NixOS同策略单元、`/data/vs-s097-fences-*`新普通btrfs镜像验证；离线`fence-boundary`示例先取StateLock再通过真实适配器持久化边界，不往生产加故障开关。最终run `...025123-4077079`真实测得namespace4026534527→4026534585、mount1033→1170，同dev199、Root/blob/seq4/内容保持健康；活进程同目录bind blocked且4条目/seq不变，撤销覆盖恢复。Prepared同dev服务重启redo committed；卷重挂实测dev199→201后Published仅补元数据，文件inode/birth/摘要不变。旧Prepared的FileKey没有birth，dev201→199时**保持pending、Prepared和全局needs_recovery/503**，两份内容与对象保留；不能忽略旧dev冒充强身份或自动覆盖。离线未完成现场保留，单元/keeper/loop均已停止、卸载、分离。并发8线程×16轮×3000文件枚举完整。

新包部署后真实试用服务又执行一次systemctl restart，严格Chromium页面确认“挂载核对通过/本进程初始采集完成”；原ServerId/Root/seq18、历史blob、全部样本字节、先前Linux/Edge committed操作及固定版本摘要不变。服务仍UID985、零capability、NoNewPrivileges=1；10个既有服务和SSH的PID/InvocationID保持，3200/3300/15312均200。上述故障注入只作用隔离run。

S097前的generation239（保留TLS、旧Root诊断行为）为 `/nix/store/bcwryfrqcc8sajf5kk7469q9j8g8r31d-nixos-system-wks-lab-26.11.20261102.7356cc7`；generation238不启用本卡TLS。S097没有再次激活239作回滚演练，S096实际旧代际回滚证据仍按上一节。需要恢复旧配置时可用明确store path执行`nixos-rebuild test --store-path ...`，它立即激活且**不定时回滚**；保留SSH和旧路径，确认健康后再决定switch。正常flake重建还要同步回退host文件固定源，不能只更改运行generation。试用卷/凭据不随配置回退删除。
