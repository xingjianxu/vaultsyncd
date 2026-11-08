# 本机 NixOS 服务部署

当前正式入口为 **https://vsyncd.132140.xyz**，后端0.1.1、仅回环8721。S118 已部署最新源码 9da5ee8（含 S115/S117 Web），运行与开机默认 generation247，并随本次部署激活 dae 分流规则调整。服务及正式目录为主机用户 `xingjian:users`；一键更新与凭据见 S113 节，当前版本及恢复见末尾 S118 节。此前各节为历史部署证据，旧试用卷仍保留。

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

## S112 当前正式域名部署（2026-11-07，generation244）

以下为 S112 验收时的历史快照；当前运行用户、版本和 generation 以 S113 节为准。

实际入口仍为 `/etc/nixos#wks-lab`。项目固定源 **4856738641ea3421c949476e6951d0a461209223**，同源包 `/nix/store/6nx4qmvrhj1rk3aa9mrjaxky56m4r7ck-vaultsyncd-0.1.1`，构建含65项包内测试通过。主机实现 `9c2133f`，挂载修正 `56bd93b09760d87f068c7a617b8a79904d14b852` 已ff合入主机main；主机锁从干净基线 `93dfcae` 保持不变。主机准确源 `/nix/store/kdr0v41db6vcd39kp9znz5b3xky170dp-source` 与旧/新system都有显式GCroot，证据在 `/data/vaultsyncd-deploy/S112-20261107T133454/`。

```nix
# /etc/nixos/hosts/wks-lab/default.nix 的 features.server 块
vaultsync = {
  enable = true;
  rev = "4856738641ea3421c949476e6951d0a461209223";
  stateDir = "/var/lib/vaultsyncd";
  rootPaths = [ "/data/vaultsyncd/roots" ];
  port = 8721;
  caddy.domain = "vsyncd.132140.xyz";
};
```

主机 `features/server/vaultsync.nix` 条件导入该提交的项目flake模块，消费其 `lib.caddyReverseProxy`。`enable/repository/rev` 必须放在主机字面features块（imports依赖静态host.features）；rev要求40位已提交SHA，未启用主机不读取本地仓库。可配置 `user/group/extraGroups/stateDir/rootPaths/port/log.{level,format}/caddy.{enable,domain,extraConfig}`；后端固定绑定回环。关闭feature会停止服务及本域名代理，保留挂载、账号和数据；`caddy.enable=false`只撤代理。主机vhost经既有 `features.server.caddy.virtualHosts` 桥接，默认不写access log，不直接暴露管理socket或文件。

| 项目 | 当前配置 |
| --- | --- |
| HTTPS | `https://vsyncd.132140.xyz`，DNS为本机115.24.115.250；HTTP308到HTTPS |
| 证书 | Let's Encrypt YE1，SAN匹配域名，TLS1.3；2026-11-07至2027-02-05，由Caddy自动续期 |
| DNS-01 | 复用Caddy既有Cloudflare插件和 `/run/agenix/cloudflare` 环境文件；无新明文token |
| 后端 | `127.0.0.1:8721`，`vaultsyncd.service`，UID/GID984，零capabilities、NoNewPrivileges |
| 正式ServerId | `69dbfa9fdca2bfb8a239b88d0fd30d53`（与旧试用实例不同，客户端连接须核对） |
| 状态 | 新btrfs子卷 `/data/vaultsyncd/state`（subvol2659），显式挂至 `/var/lib/vaultsyncd`，0700 |
| Root白名单 | `/data/vaultsyncd/roots`，初始注册 `/data/vaultsyncd/roots/documents`，服务账号所有0700 |
| 文档RootId | `cf8a15ace18abe4d9c3027ac25c30070` |
| 初始登录 | `owner`；随机密码在 `/var/lib/vaultsyncd-credentials/owner.password`，root0600，父目录0700 |

在受信终端用 `sudo cat /var/lib/vaultsyncd-credentials/owner.password` 读取初始密码。服务启动不读此文件，不会重置owner；改密后需独立管理凭据，文件不代表永远有效的当前密码。初始化经离线admin匿名stdin完成，不通过argv/env/store传递。真实密码、token、cookie、私钥未进入Git或证据；运行日志、argv、生成TOML核对不含初始密码。

只创建了不存在的新目录/子卷，未格式化既有设备、未递归改权既有资料。挂载CoW、数据校验与压缩保留；`user_subvol_rm_allowed`须同时配置 `/data` 首次挂载和状态挂载，这是**整个btrfs文件系统的选项**，允许普通UID在原有目录权限约束下删除自己拥有的子卷，不限于状态子卷。首次test因仅配置后续子卷挂载而被preflight拒绝，失败日志保留；修正首次挂载、实际remount并核对findmnt后重跑通过。行为依据 [btrfs挂载文档](https://btrfs.readthedocs.io/en/stable/Administration.html)。服务没有sudo/CAP_SYS_ADMIN。Root内仍禁止嵌套子卷/挂载，注册及恢复围栏未降低。

旧镜像 `/data/vaultsyncd-trial-s096/volume.img`、挂载、UID985、全部历史试用状态及 `/var/lib/vaultsyncd-trial-credentials/owner.password` 保留；正式实例不复用旧DB。旧localhost:18443 vhost已撤下，开发CA与私钥未删除，也不用于公网信任。

实际先保存原diff/锁/generation/服务PID与SSH，构建包及主机；dry-activate仅重启本服务/polkit、reload Caddy/dbus，sshd配置逐字节不变。修正后执行以下真实命令，最后设为开机默认；test立即激活且不提供自动定时回滚：

```sh
nix build /etc/nixos#nixosConfigurations.wks-lab.config.system.build.toplevel --no-update-lock-file --no-write-lock-file --out-link /data/vaultsyncd-deploy/S112-20261107T133454/host-corrected
sudo nixos-rebuild dry-activate --store-path /nix/store/jg54j3w2arc95lc3asx071w2b75z95kq-nixos-system-wks-lab-26.11.20261102.7356cc7
sudo nixos-rebuild test --store-path /nix/store/jg54j3w2arc95lc3asx071w2b75z95kq-nixos-system-wks-lab-26.11.20261102.7356cc7
# 中间完成下述HTTPS/浏览器/存储/既有应用健康验收
sudo nixos-rebuild switch --store-path /nix/store/jg54j3w2arc95lc3asx071w2b75z95kq-nixos-system-wks-lab-26.11.20261102.7356cc7
```

主机构建日志在证据目录。当前运行/default system均为上述jg54j3w2路径，切换后重新核对原ServerId、Root健康、committed状态与固定版本SHA，结果见 `switch-health.json`。更新只改已构建项目提交与feature配置，依次build/test/健康/switch；不更新主机依赖。

验收：严格系统CA/主机名的HTTPS健康和capabilities，16MiB上传→finalize→committed，固定版本下载SHA与磁盘一致；+1字节413；重启后原ServerId/版本/同OpId重放；同UID IPC及普通UID btrfs备份prepare/verify/release通过。新Chromium配置无私有CA/不忽略TLS，真实Web登录、Secure/HttpOnly/SameSite=Strict cookies、SSE初始seq及16秒心跳通过；敏感文件/DB/socket探测404。首轮脚本把16MiB按默认分块发送而失败，改为明确协商16MiB后通过；浏览器心跳等待改为真实流等待与页面观测后通过，失败日志不冒充通过。

`deploy/nixos/verify-public.py` 与 `verify-public-browser.py` 为显式部署验收脚本，前者会在指定Root创建新样本并重启指定服务，**不是日常健康探测**。只对本次专用Root执行；使用新run_id，拒绝覆盖旧样本/证据。两脚本从受限运行时文件读密码，只输出公开状态、哈希及Cookie属性，并注销自身会话。证据 `acceptance-test2.json`、`browser.json`、`existing-services-test.json`。日常只用：

```sh
curl --fail https://vsyncd.132140.xyz/v1/health
sudo -u vaultsyncd /data/vaultsyncd-deploy/S112-package/bin/vaultsyncd-admin status --socket /var/lib/vaultsyncd/admin/socket --json
sudo journalctl -u vaultsyncd --no-pager
```

10个既有服务（含sshd/Caddy/PostgreSQL及应用）的PID/InvocationID保持，3200/3300/15312返回200，SSH banner与主机锁摘要不变。未重启宿主/Windows；这次不是Windows原生验收或S108首发门禁。域名HTTPS与证书通过本机使用公网域名验证；独立外部Web抓取工具返回不可访问，**不宣称已有独立外网客户端连通证据**。Caddy自动续期已配置，未等待证书实际临近到期再验收。

恢复入口为部署前generation243：`/nix/store/4q8hm31xrd6wfqslddb5211k6y2birxa-nixos-system-wks-lab-26.11.20261102.7356cc7`。必要时 `sudo nixos-rebuild test --store-path <该旧路径>` 立即恢复旧试用服务及Caddy配置；正式数据/凭据保留，正式状态挂载可能卸载，不要清理它。确认健康后再决定对旧路径switch；也需同步回退主机源码，否则常规flake rebuild会重新启用正式实例。本卡没有激活旧generation演练回退，不把S096的历史演练当作本卡测试。旧版本不得打开已升级的新库，恢复旧试用实例用其原库。

## S113 主机用户运行与 pure 一键部署（2026-11-07）

本机入口 `/etc/nixos#wks-lab`，feature 的 `user` 默认读主机 `host.user`，`group` 默认读 `config.users.users.<user>.group`，当前为 **xingjian:users（UID1000/GID100）**。项目模块 `createUser=false` 不更改普通用户类型/主组/描述/附加组。单元保留零 capabilities、NoNewPrivileges、0700 状态、严格系统写保护与 Root 白名单。

正式 `/data/vaultsyncd`、`/var/lib/vaultsyncd`（挂载源 `/data/vaultsyncd/state`）、`/data/vaultsyncd/roots` 及其 documents 目录均为 xingjian:users、0700；内部文件及原样本已离线改权。`/var/lib/vaultsyncd-credentials` 同归该用户且0700，`owner.password` 为0600，可在受信终端用 `cat /var/lib/vaultsyncd-credentials/owner.password` 读取。保留原登录及 ServerId/RootId，服务重启不会重置密码。没有删除正式数据、没有修改历史试用卷或其账号。

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

一次性账号迁移证据 `/data/vaultsyncd-deploy/S113-20261107T221130/`：停服后的只读 `state-before` btrfs 快照与 `roots-before` reflink 副本、旧归属和文件摘要均保留在 Git 外。旧运行/启动 generation244 为 `/nix/store/jg54j3w2arc95lc3asx071w2b75z95kq-nixos-system-wks-lab-26.11.20261102.7356cc7`。**回到 S112 账号配置前，先停服并把专用正式状态及 Root 的归属恢复为 UID/GID984（旧配置会重建该账号）；仅切换旧 system 不能同时恢复归属。** 备份快照不改权；不要对用户其他目录递归改权，也不要用旧程序直接写新 schema。

初次迁移固定项目4071fd4、主机75d6fb5，系统k92lxnm03xg828lzfzivp0aij810gpns先test；原正式 ServerId `69dbfa9fdca2bfb8a239b88d0fd30d53`、文档 RootId `cf8a15ace18abe4d9c3027ac25c30070` 和旧样本摘要保留。65项包内测试、普通/专用用户模块、aliweb关闭feature的pure完整求值通过。真实HTTPS16MiB上传/413/committed/固定下载/服务重启与同OpId重放、同UID IPC与普通用户备份prepare/verify/release通过；10项既有服务状态/PID/InvocationID保持。最终一键更新提交、generation与switch后证据见 [S113交接](../../agent/tasks/S113.md)。本卡不代替Windows、hosted CI及首发性能门禁。

S113 最终一键部署已通过：项目固定 **6b967bda32378da5e74e4c328fcaa3bcd72e75cf**，主机 **32bfeb54f9c76d5ed781a80800fdce86179c7d6a** 已 ff 合入 `/etc/nixos` main；运行/开机默认 **generation245**，system `/nix/store/mwra49kbdbgppwmc5p7j7rz0dbfbbk4w-nixos-system-wks-lab-26.11.20261102.7356cc7`，包 `/nix/store/180dcgy5j2hx801vg59g2bk15whp3p7j-vaultsyncd-0.1.1`。该源复用已通过65项测试的相同Rust组件；本次重新构建发行provenance与主机。真实脚本从4071fd4更新pin到6b967bd，显式pure构建后dry/test/健康/switch，通过后自动提交pin并清理临时树。主机main的pure toplevel重新求值得到同一system。

最终脚本证据 `/data/vaultsyncd-deploy/local-20261107T221615-1805073/`，完整日志在上方S113证据的 `one-click.log`；`switch-health.json`确认旧ServerId/RootId/committed版本及磁盘/下载摘要、全部正式文件UID1000/GID100、10项既有服务PID/InvocationID及SSH保持，3200/3300/15312跟随重定向后200、主机锁逐字节不变。10服务比较使用保留的S112历史基线，7个已启用服务另有本轮迁移前快照。最终没有注入生产失败或实际激活旧generation演练回退，也没有再次运行Windows原生/浏览器矩阵或独立外网客户端探测。


## S116 当前版本部署（2026-11-08，generation246）

按用户要求执行 `./tools/deploy-local.sh --rev f3b8979`，固定项目完整 SHA `f3b897983087af1b6d075d89000c0a5ebc235aeb`，包含 S114 服务端 Root 名称及 S115 HeroUI Web。主机仅改版本 pin，提交 `15cfc075031ba6d9fab4bf040e1adbe5d68be939` 已 ff 合入 `/etc/nixos` main，临时主机树与分支自动清理。主工作区未提交 skills 文件保留；部署从干净任务树执行。

运行与开机默认均为 **generation246**，system `/nix/store/22bvy0jasjkb02vd8j2wm6lcmgn5aci9-nixos-system-wks-lab-26.11.20261102.7356cc7`；包 `/nix/store/yd63mh9prpasa8f8647g076mx2m2wjdv-vaultsyncd-0.1.1`，manifest 与固定源一致。域名、owner、ServerId `69dbfa9fdca2bfb8a239b88d0fd30d53`、文档 RootId `cf8a15ace18abe4d9c3027ac25c30070` 保持；状态与 Root 仍为 btrfs、xingjian:users、0700。密码读取方式沿用 S113。

实际验收：部署契约6/6，Nix 包内66/0；完整 pure 构建及同一 store path 的 dry-activate/test/HTTPS与同UID IPC、实例身份/目录归属/switch通过。10项既有服务的PID/InvocationID、SSH配置与banner、3应用200及主机锁保持；线上HTML/JS/CSS共6文件逐字节匹配发行包。真实 Chromium 严格公共CA/TLS1.3、五路由及390/768/1440无溢出、主题、owner登录与Secure/HttpOnly/SameSite=Strict cookie、原Root健康、SSE初始seq及16秒心跳通过，无页面异常及CSP违规，敏感路径404。浏览器脚本前三轮因定位歧义、缺同源证据的APIRequestContext和字符串wait_for_function触发unsafe-eval限制失败；分别限定main、使用页面同源fetch和函数谓词后完整复验通过，未修改产品代码或放宽安全策略。

部署证据 `/data/vaultsyncd-deploy/local-20261108T124809-2457084/` 保存 result.json、旧generation、旧system/boot GC roots、原锁和pin diff；额外核对与浏览器脚本/失败日志/成功JSON/截图在 `/data/vaultsyncd-deploy/S116-20261108-local/`。恢复入口为部署前generation245：`/nix/store/mwra49kbdbgppwmc5p7j7rz0dbfbbk4w-nixos-system-wks-lab-26.11.20261102.7356cc7`，按S113恢复流程先test及健康检查，再决定switch并同步回退源码pin；没有实际激活旧generation演练。未运行生产上传/恢复/备份写入或故障注入、未重启宿主/Windows、未做独立外网客户端验收，原首发门禁保持。交接与任务树整合见 [S116](../../agent/tasks/S116.md) 与 [批次](../../agent/tasks/S116-batch.md)。


## S118 最新版本部署（2026-11-08，generation247）

按用户要求执行 `./tools/deploy-local.sh --rev 9da5ee8`，固定项目完整 SHA `9da5ee8f3fe3f64cf517ab6f52d6f1969b1dc353`（含 S115 服务端与 S117 Web 紧凑化）。主机侧先提交用户要求的 dae 分流规则调整 `42d99c5`（实际写法 `!dip(geoip:cn) -> proxy`；并行会话另以 `9cd3956` 记录了该语法坑），再由脚本以 `9cd3956` 为基线只改版本 pin，pin 提交 `b0ad622809158369075b0065d8dd6e1a4c8e4838` 已 ff 合入 `/etc/nixos` main，临时主机树与分支自动清理。部署从干净任务树执行，主工作区未提交 skills 文件保留。

运行与开机默认均为 **generation247**，system `/nix/store/cwrpmxw805bg9nrn08a54jmny2lxcg4w-nixos-system-wks-lab-26.11.20261102.7356cc7`；包 `/nix/store/2vqjynvrjjxcldl9svlka49mrn3rzwk6-vaultsyncd-0.1.1`，manifest 来源与固定 SHA 一致。域名、owner、ServerId `69dbfa9fdca2bfb8a239b88d0fd30d53`、文档 RootId `cf8a15ace18abe4d9c3027ac25c30070` 保持；状态与 Root 仍为 btrfs、xingjian:users、0700，IPC `phase=ready`。

实际验收：clean 检查、pure 求值与构建、manifest 与主机锁核对、dry-activate/test/HTTPS/IPC/归属/switch 全部通过；线上 `index.html` 与两个主资产与包内文件 sha256 逐字节一致（资产名已更新为新构建）。真实 Chromium 严格公共 CA/TLS1.3、五路由 390/768/1440 无溢出、主题、owner 登录与 Secure/HttpOnly/SameSite=Strict cookie、原 Root 健康、SSE seq 与 16 秒心跳、无页面异常与 CSP 违规、敏感路径 404 均通过。10 项既有服务中 9 项 PID/InvocationID 不变，`dae` 因规则变更重启后路由集 17/1024、google 与 baidu 均 200；3 应用 200、SSH banner 与主机锁不变。

部署证据 `/data/vaultsyncd-deploy/local-20261108T133746-2533091/` 保存 result.json、旧 generation、旧 system/boot GC roots、原锁与 pin diff；本轮核对与浏览器脚本/JSON/截图/静态文件副本在 `/data/vaultsyncd-deploy/S118-20261108-local/`。恢复入口为部署前 generation246：`/nix/store/22bvy0jasjkb02vd8j2wm6lcmgn5aci9-nixos-system-wks-lab-26.11.20261102.7356cc7`，按 S113 流程先 test 与健康检查再决定 switch，并同步回退源码 pin；如需还原分流规则须同时回退 `42d99c5`。未实际激活旧 generation 演练回退，未运行生产上传/恢复/备份写入或故障注入，未重启宿主/Windows，原首发门禁保持。交接与整合见 [S118](../../agent/tasks/S118.md) 与 [批次](../../agent/tasks/S118-batch.md)。
