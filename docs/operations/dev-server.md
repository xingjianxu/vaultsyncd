# 生产与开发测试环境

S207（2026-11-12）：生产入口为 **https://vsyncd.132140.xyz**；开发测试与 Windows 测试机入口统一为 **https://wlab.132140.xyz:15401**。此前请求中的1540已确认应为15401。

| 环境 | 部署与进程 | 数据 |
| --- | --- | --- |
| prod | 既有 NixOS `vaultsyncd.service` 与主机 Caddy，部署仍用 `tools/deploy-local.sh` | `/var/lib/vaultsyncd`、`/data/vaultsyncd/vaults` |
| dev | `tools/dev-server.sh` 启动临时后台 supervisor、后端与独立 Caddy，无 NixOS unit，无开机自启 | `/data/vaultsyncd-dev/dev-server/` |

dev 为独立服务器，初始化产生新的 ServerId、数据库与账号；不复制生产状态或改写旧连接的 ServerId。原有一次性故障测试仍使用各自 run_id 的隔离实例，不能对持久 dev 或 prod 做满盘、回滚及崩溃注入。

## 自动更新范围（2026-11-13，S219）

用户最新决定：服务端（含服务端 Web）代码更新任务通过验收后，**只自动更新 dev** `https://wlab.132140.xyz:15401`，并记录实际包路径、健康验证和交接；**不自动更新 prod** `https://vsyncd.132140.xyz`。此约定覆盖此前“直接部署本机”及任务完成后 prod/dev 同包的自动部署要求，历史验收记录保留原文，不作为后续生产部署授权。

生产仅在用户另行明确要求时按既有 NixOS 流程部署；日常 dev 更新不运行 `tools/deploy-local.sh`，不修改生产 pin、激活或重启生产服务。保留 dev 原有账号、ServerId、数据库、Vault 和配置；更新版本不等于重置实例。纯文档或不改变服务产物的开发工具维护不触发部署。

## 域名证书

与 `../receiptd/scripts/dev.ts` 相同，直接读取本机已有文件：

```text
/var/lib/acme/wlab.132140.xyz/fullchain.pem
/var/lib/acme/wlab.132140.xyz/key.pem
```

实际签发入口 `/etc/nixos/hosts/wks-lab/default.nix`：`security.acme` 使用 lego 的 Cloudflare DNS-01，主名 `*.wlab.132140.xyz`，SAN 另含 `wlab.132140.xyz`；`environmentFile` 由 agenix 提供 `/run/agenix/cloudflare`。无需另取 token、复制私钥或把机密写入 Nix store。运行用户已在 `acme` 组，文件 `0640 acme:acme`、目录 `0750`；新加入组的会话需要重新登录。

每日 `acme-renew-wlab.132140.xyz.timer` 检查续期。必须使用 **fullchain**，证书须覆盖域名且有效；dev 启动失败时修复证书/权限，不降级到明文 HTTP。证书在进程启动时读取，续期后运行 `./tools/dev-server.sh restart` 重新加载。

## 管理入口

在仓库根目录运行，包装脚本自动进入锁定的 devShell，无需全局安装工具：

```bash
./tools/dev-server.sh init --login dev
./tools/dev-server.sh start
./tools/dev-server.sh status
./tools/dev-server.sh logs
./tools/dev-server.sh logs --follow
./tools/dev-server.sh stop
./tools/dev-server.sh restart
```

`init` 仅首次离线初始化账号，密码从终端或标准输入读取。已初始化不覆盖账号、数据库或内容。`start` 等健康检查与 ServerId 核对通过后返回，进程脱离前台；关闭启动它的终端不停止服务。`logs --follow` 为显式的前台日志查看，Ctrl+C 只退出查看。`stop` 向本实例发送有序终止信号，不按进程名批量 kill，不删除数据。停机后可再次启动；宿主重启后按需手动 start。

默认使用当前已部署服务包的二进制和同包 Web，**只复用程序，不复用生产配置与数据**。服务端/Web 代码更新验收后，在任务工作树用项目 Nix 包构建，再通过 `restart --package` 指定本次验收包路径更新 dev；不能省略 `--package` 而误用旧生产包。此流程到 dev 健康验证与交接为止，不自动推进生产；仅当用户另行明确要求生产部署时走原部署流程。

```bash
mkdir -p /data/vaultsyncd-dev/S207-manual-build
nix build .#default --out-link /data/vaultsyncd-dev/S207-manual-build/server-package
./tools/dev-server.sh restart --package /data/vaultsyncd-dev/S207-manual-build/server-package
```

dev 后端仅监听 `127.0.0.1:18741`，独立代理监听 HTTPS 15401，禁用代理管理接口及自动证书申请，不使用生产的8721/80/443或2019。保留协议16MiB分块上限、SSE及时刷送和后端Web CSP/nonce。证书路径不在运行目录复制。

## 数据与测试机

`/data/vaultsyncd-dev/dev-server/` 中 `state/` 为数据库与内部状态，`vaults/` 为可注册的独立 Vault 基目录，配置、运行信息与日志另存。目录应为服务运行用户拥有的私有 btrfs 目录；`--root` 仅接受开发根内的位置。修改路径前先 stop，不把 dev root 指向生产目录或其软链。

Vault 注册使用运行实例的受限管理 socket，通过 `vaultsyncd-admin vaults register`，不能直接修改在线数据库。Windows 测试机只保存 dev 的 URL/ServerId，通过认证 IPC 管理连接与绑定；不得将 prod 的绑定直接指向新服务器，也不为正常连接安装自签CA或禁用 TLS 验证。

现场验收、客户端位置、实际 ServerId/VaultId 和证据索引在 [S207交接](../../agent/tasks/S207.md)。S209（2026-11-12）已用管理脚本有序restart到与生产generation282同源的浏览器PDF版包 `/nix/store/dil3kh3r7iphfsiqbgzxv6wkq21kx35x-vaultsyncd-0.1.7`；原dev数据库/Vault/ServerId保持，未重置账号或绑定。HTTPS健康、15项Web字节及身份隔离通过，详见 [S209交接](../../agent/tasks/S209.md)；S208旧包及证据仍保留用于回退。

本机 dev 账号为 `dev`，密码固定为 `1`（用户 2026-11-12 决定：开发环境即使重置，密码也是 `1`）。因此 dev 密码可写入本文；prod 密码仍只保存在 Git 外的受限文件，不写入文档、命令参数或日志。dev 受限密码文件在 `/data/vaultsyncd-dev/S207-prod-dev-20261112/credentials/password`（0600，内容已同步为 `1`）。

重建（重置）dev 实例后，`init` 的密码从非终端 stdin 读取单行，可用管道保持固定密码，不必交互输入：

```bash
./tools/dev-server.sh stop
# 停服后移除 /data/vaultsyncd-dev/dev-server（含 state/vaults），即产生新的 ServerId、数据库与账号
printf '1\n' | ./tools/dev-server.sh init --login dev
./tools/dev-server.sh start
```

重置会更换 ServerId 与 VaultId，Windows 测试机需按新 ServerId 重新绑定，不能沿用旧连接记录。

Windows 11 测试机程序在 `C:\vaultsyncd-dev-client`，默认用户配置已连接 dev，运行该目录的 `Start vaultsyncd.cmd` 可打开；常规测试绑定及客户端 agent 保持运行。初始化其他测试机可在 devShell 中使用 `tests/platform/windows/run-dev-server.sh <完整客户端目录> <fixture目录> <证据目录>`；fixture格式与恢复边界见脚本，已有非 dev 配置须显式迁移，不覆盖。

S210（2026-11-12）已再次有序restart到与生产generation283同包 `/nix/store/c9xkfn6c1gm7lzd86pkab669gk5vj1yx-vaultsyncd-0.1.7`，提供只读文本/代码与Markdown预览；原dev账号/Vault/ServerId保持，三源136文件/408字节及部署包文本13组/PDF图片17组通过，证据见 [S210交接](../../agent/tasks/S210.md)。

S214已通过永久master入口 `restart --package /nix/store/inp5h2n5fa8kp5j0i445ya3wsq9qjva7-vaultsyncd-0.1.7` 与生产generation287同包，支持文件列表名称/大小/修改日期排序，默认名称升序。原账号/Vault/ServerId保持、与prod隔离，健康、三源136文件/408次字节及HTTPS/CSP通过；无持久测试样本，恢复证据见 [S214交接](../../agent/tasks/S214.md)。

S211（2026-11-12）已通过 `restart --package` 有序切换至与生产generation284同包 `/nix/store/6liszy33b10wzpq4xn9aqn7rjamak7i0-vaultsyncd-0.1.7`，管理页面加载失败自动局部恢复；dev原账号/Vault/ServerId保持，与prod身份隔离。三源136文件/408字节及HTTPS/匿名门禁/CSP通过；证据见 [S211交接](../../agent/tasks/S211.md)。

## S213 Office 预览配置

开发实例可使用本机 S212 Document Server。先停止 dev，将共享 JWT 从受限来源交给配置命令；命令只保存本实例 `config/office-jwt` 的0600副本和非机密地址，不重建账号、ServerId、Vault或数据库。

```bash
tools/dev-server.sh stop
tools/dev-server.sh configure-office \
  --office-url https://wlab.132140.xyz:15402 \
  --office-jwt-file /data/vaultsyncd-dev/<受限来源>/office-jwt
tools/dev-server.sh start --package /nix/store/<本次验收包>
```

来源须为当前用户可读的私有普通文件，原始长度最多4096字节，去末尾CR/LF后至少32字节且仅可打印非空白ASCII；内容不进Git/store或日志。命令只能在本实例全部进程停止后执行，并验证原有状态/代理配置。先前生成的精确30秒反代模板在start/restart/configure-office时迁移到130秒，以覆盖120秒转换deadline；任意手改仍拒绝。关闭时stop后执行 `configure-office --disable-office`，保留密钥副本，再start。密钥轮换需同步重启OnlyOffice和vaultsyncd，并重新配置dev副本。

Office预览为PDF，原文件下载保持；支持doc/docx/xls/xlsx/ppt/pptx/odt/ods/odp。输入和PDF各限50MiB，两项活动转换准入，120秒超时；关闭界面撤销交付和临时源，不代表能撤回OnlyOffice已经读取的内容或已入队工作。取消不提前释放转换worker的准入；超时后上游仍可能继续，其执行并发由OnlyOffice自身配置约束。OnlyOffice可能修复部分损坏文件并产出合法PDF，这不改写原件，也不是修复提交。排版、分页、字体和浏览器PDF行为保持第三方边界，不提供交互工作表或在线保存。

S213（2026-11-12）已通过stop/configure-office/start将持久dev切换为与生产generation286同包 `8qfvjkalrkdhfdhpig9zh7liix6kwsp8-vaultsyncd-0.1.7`，使用同机OnlyOffice；原dev身份/账号/Vault和Windows连接配置保持，应用CSP不变。最终精确包Chromium14组及prod/dev资源、密钥和HTTPS验证通过。合并清理结果见 [S213交接](../../agent/tasks/S213.md)。

## S220 文件类型图标（仅 dev 更新）

S220（2026-11-13）按S219约定仅通过永久主树入口 `restart --package /nix/store/alvghhrpzscsa2f6v0hl0169x9lm8ch6-vaultsyncd-0.1.7` 更新dev（源e29c134）；文件列表按后缀展示十四类图标，未知类型回退。原dev ServerId/账号/Vault/schema21及Office配置不变；精确包完整浏览器与专项七组、136文件线上字节、严格TLS/CSP/匿名门禁及登录后明暗320/1440通过，无持久测试样本。prod进程/启动时间/system291/主机pin未变，未更新或重启生产。回退只需dev restart至S218包，不重置数据；交接 [S220](../../agent/tasks/S220.md)。

## S218 预览关闭动画

S218（2026-11-13）dev经永久主树入口restart至与生产generation291同包 `/nix/store/9l4my652gyr3bqlr6sq0z7nrsxhc1297-vaultsyncd-0.1.7`（源a6a23de）。关闭立即取消工作，已渲染正文/资源保留到退出动画结束，不先缩成空壳；导航/认证/卸载保护与快速重开保持。原ServerId、账号/Vault/schema21、Office与dev配置不变；精确包52组及18退出/重开109帧、三源408次字节与TLS/CSP/健康通过。无持久样本，回退只需restart至S217包，不重置数据；证据及限制见 [S218](../../agent/tasks/S218.md)。

## S217 预览间距

S217（2026-11-13）dev由永久主树入口restart至与生产generation290同包 `/nix/store/8wkqy5gnqfkfsj389x5vd7ip0p2k6k9d-vaultsyncd-0.1.7`（源df87e02），缩紧预览标题至正文间距，下载/关闭/取消/重试保持。原ServerId/账号/Vault/schema21及Office配置不变，与prod隔离；精确包三套预览17+13+16组、三源408次字节/TLS/CSP/匿名门禁与健康通过。无持久样本，回退仅restart至S216包，不重置数据；证据与限制见 [S217](../../agent/tasks/S217.md)。

## S216 预览标题栏操作

S216（2026-11-12）dev由永久主树入口有序restart至与生产generation289同包 `/nix/store/lb4av1rszg555rdkcfhp8baz5s7dbcs3-vaultsyncd-0.1.7`（源b62e45c）。预览删除正文关闭按钮，下载原件改为标题栏图标；原ServerId/账号/Vault/schema21与Office配置保持。最终精确包图片/PDF17组、文本13组、Office16组及线上408次资源字节/TLS/CSP验证通过，无持久测试样本。旧包保留，回退只需管理器restart至S215包，不重置数据；交接见 [S216](../../agent/tasks/S216.md)。

## S215 原生Office只读查看器

S215（2026-11-12）dev已由永久入口有序restart至与生产generation288同包 `/nix/store/v5x74fbkhv7wk1xqmhvkd61xfl1r49ca-vaultsyncd-0.1.7`，发布源6a99df4包含S214排序。原ServerId/账号/Vault保持，Office运行时配置和密钥副本沿用S213；不重置实例、不写入测试样本。

覆盖S213的默认Office→PDF路径：九格式按固定版本直接以ONLYOFFICE只读查看，保留原件下载，XLSX可切工作表；不提供原件编辑/保存。配置签名、权限/源完整校验、50MiB门禁保持；临时源120秒、两准入、关闭撤销，已取得副本和上游已入队任务不能硬收回。CSP精确信任配置服务API.js/frame，空事件体哈希例外不允许任意inline/eval。最终精确包Office16组与prod/dev身份、健康、资源、匿名门禁/CSP核验通过，完整证据见 [S215](../../agent/tasks/S215.md)。


## S223 透明品牌图标（仅 dev 更新）

S223（2026-11-13）经永久入口 `restart --package /nix/store/zxn0vlr9yha8p2b2259rpsjvcrdrv90y-vaultsyncd-0.1.7` 更新dev（源87e61a3）。Web登录、顶栏及favicon使用透明Logo，原黑底资源保留在源码 `assets/branding/`。136项HTTPS资源字节/严格TLS/CSP/MIME、真实登录退出及明暗320/1440八组透明度通过。原ServerId/账号/Vault/Office配置保持，仅instance包引用变化；prod PID/启动时间/system保持，不更新生产。回退仅需restart至S220包 `/nix/store/alvghhrpzscsa2f6v0hl0169x9lm8ch6-vaultsyncd-0.1.7`，不重置数据。Windows永久客户端未替换；证据与交接见 [S223](../../agent/tasks/S223.md)。


## S224 深色 V 与紧凑 Logo（仅 dev 更新）

S224（2026-11-13）经永久入口 `restart --package /nix/store/5z68d5bq95w566ahfyvc9kz9lg553jvl-vaultsyncd-0.1.7` 更新dev（源8b112ef）。中央V使用深石板蓝，主体占比约98%；136资源字节/TLS/CSP/MIME与真实登录退出、明暗320/1440八组颜色和边界通过。原身份/账号/Vault/Office保持，仅instance包引用变化；生产进程与system保持，不部署生产。回退可restart到S223包 `/nix/store/zxn0vlr9yha8p2b2259rpsjvcrdrv90y-vaultsyncd-0.1.7`，不重置数据。完整Windows客户端已编译交付根dist，客体永久客户端未替换；交接 [S224](../../agent/tasks/S224.md)。


## S225 文件操作记录升级（2026-11-13）

仅 dev 已更新到 `/nix/store/ig2vap2j3dymx5hypfjd228fvnkpfac2-vaultsyncd-0.1.7`，来源整合提交 `7335ab0`，包含 S224 最新品牌与 S225 五类已提交文件操作；通过永久主树 `tools/dev-server.sh restart --package` 启动，运行不依赖任务工作树。schema21→22；原 ServerId、账号、Vault、Office配置和90条旧活动记录保持，137资源字节与严格TLS/CSP、真实登录退出/四类型筛选通过。生产进程及system保持，未更新生产。两端操作记录说明见接口 S225、交接 [S225](../../agent/tasks/S225.md)。

升级前停机整套备份在 `/data/vaultsyncd-dev/S225-activity-events-20261113/predeploy-backup/`（state/vaults/config、70文件SHA256），自动迁移前数据库备份另在dev state内。由于schema已升22，**不能只重启S224旧包回退**；须有序停机、保留当前22整套状态后，按该备份恢复配套schema21状态/内容/配置，再用S224包启动并核验。此操作会回到备份时点，升级后新增事实需先独立保留；本卡未实际回退或重置实例，备份为长期证据，不按缓存清理。

## S227 新云同步 Logo（2026-11-13）

经永久主树管理入口仅dev更新到 `/nix/store/hk86ijg337lsmz6y89a46jlk2d5v7rhl-vaultsyncd-0.1.7`（源b176726）。Web favicon/登录/顶栏采用用户新云朵双箭头SVG，保留源图白底。137项HTTPS资源字节/严格TLS/CSP/MIME、真实登录退出及明暗320/1440八组通过；原ServerId/账号/Vault/Office配置保持，instance仅包引用变化，生产进程/system保持。回退可restart至S225包 `/nix/store/ig2vap2j3dymx5hypfjd228fvnkpfac2-vaultsyncd-0.1.7`，schema22保持，不重置数据。完整Windows客户端已交付根dist，客体永久客户端未替换；交接 [S227](../../agent/tasks/S227.md)。

## S228 新 Logo 透明背景（2026-11-13）

仅dev经永久主树入口更新到 `/nix/store/5h9igr23zl2if3kqrl6cx51gxg756p1s-vaultsyncd-0.1.7`（源d7e3e2e）。默认SVG移除白底，Web favicon/登录/顶栏透明；图形/渐变/画布保持，Windows七尺寸ICO同步透明。137资源字节/严格TLS/CSP/MIME、真实登录退出与明暗320/1440八组透明度通过；prod/dev身份健康与原配置保持，instance仅包引用变化，生产进程/system保持。回退可restart至原dev包 `/nix/store/3793434gfaxdyb6pc5fvdsc7bvfy3g4b-vaultsyncd-0.1.7`，不重置schema22实例。新完整Windows客户端已交付根dist，永久客体客户端未替换；详见 [S228](../../agent/tasks/S228.md)。

## S229 放大品牌图标（2026-11-13）

仅dev经永久主树入口更新到 `/nix/store/rnbz64qyy7ak79ndn56qzrig9yy6j89m-vaultsyncd-0.1.7`（源c90af28）。SVG主体横向约99%、等比放大约6%，Web顶栏36px、登录48px，透明背景保持；七尺寸Windows ICO同步重生，完整客户端已交付根dist。137资源/严格TLS/CSP/MIME、真实登录退出及明暗320/1440八组尺寸/透明度通过；原prod/dev身份健康、dev账号/Vault/Office配置保持，instance仅包引用变化，生产进程/system保持。回退可restart至S228包 `/nix/store/5h9igr23zl2if3kqrl6cx51gxg756p1s-vaultsyncd-0.1.7`，不重置schema22实例。永久Windows客户端未替换；详见 [S229](../../agent/tasks/S229.md)。

## S230 浅蓝色品牌 Logo（2026-11-13）

仅dev经永久主树入口更新到 `/nix/store/wz9vmywir3wqp4qbhg2xw4pv65qnd30z-vaultsyncd-0.1.7`（源3fd5104）。云朵改为浅蓝#8ACFFF/#66BDF4/#47A7E9，双箭头/透明背景/放大尺寸保持；七尺寸Windows ICO同步重生，完整客户端已交付根dist。137资源/严格TLS/CSP/MIME、真实登录退出及明暗320/1440八组浅蓝/透明度/尺寸通过；prod/dev身份健康、dev账号/Vault/Office配置保持，instance仅包引用变化，生产进程/system保持。回退可restart至S229包 `/nix/store/rnbz64qyy7ak79ndn56qzrig9yy6j89m-vaultsyncd-0.1.7`，不重置schema22实例。永久Windows客户端未替换；详见 [S230](../../agent/tasks/S230.md)。

## S232 浅蓝品牌略加深（2026-11-14）

仅dev经永久主树入口更新到 `/nix/store/vmp9f5vnw6iihcj6b7kfi5yq9mncd874-vaultsyncd-0.1.7`（源d87c1c5）。云朵浅蓝亮度轻降约4个百分点，渐变#76C7FF/#53B5F3/#359EE7；双箭头/透明区域/路径/尺寸保持，七尺寸Windows ICO同步重生，完整客户端交付根dist。137资源/严格TLS/CSP/MIME、真实登录退出与八组颜色/透明度/尺寸通过；原prod/dev健康身份、dev账号/Vault/Office配置保持，instance仅包引用变化，生产进程/system保持。回退可restart至S230包 `/nix/store/wz9vmywir3wqp4qbhg2xw4pv65qnd30z-vaultsyncd-0.1.7`，不重置schema22实例。永久Windows客户端未替换；详见 [S232](../../agent/tasks/S232.md)。
