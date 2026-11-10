# 实施进度

当前：**S183 done**，根 `logo.svg` 已统一为 Windows 客户端 EXE/窗口/托盘图标及 Web favicon/登录/顶栏 logo。根 typecheck、两端 build、Web 七组测试、Windows 严格 clippy/构建、真实 Windows 图标像素/窗口及 Chromium 品牌/两端布局验证通过；实现 `c37760f` 已在本机 generation 274 上线，公网与 loopback 健康、SVG 字节/MIME/CSP 核对通过。交接 [S183](tasks/S183.md)；客户端新图标未发行，现有 v0.1.7 附件不变。**下一 ready：无**，S011/S106/S107/S108 门禁保持。

此前 **S182 done**，0.1.7 已作为 [v0.1.7](https://github.com/xingjianxu/vaultsyncd/releases/tag/v0.1.7) **正式版**发出（非草稿、非预发布、`releases/latest` 指向本版，唯一 Windows x64 ZIP `afddce55…`、16,062,202 字节，源`9a873c8`，携带 S180 桌面更新进度/登录版本与 S181 口令策略放宽）；远端 digest 与本地逐字节一致，标签指向同一提交。版本镜像`9a873c8`已推送 master；证据`/data/vaultsyncd-dev/S182-release-20261110/`，交接[S182](tasks/S182.md)。**下一ready：无**，S011/S106/S107/S108门禁不变；Latest 属发布渠道属性，不代表首发验收完成。

此前 **S180 done**，桌面自动更新提供独立原生进度窗口：下载真实百分比/字节，校验、安装、健康与回退活动条；登录框显示构建版本。同步修复S174屏障误拦截更新trial，保留普通启动屏障及未commit写入/同步门禁。根typecheck/build、host54项、Windows严格clippy/构建、浏览器及真实Windows进度/登录、ZIP与安装版更新/回退/门禁75项通过，交接[S180](tasks/S180.md)。实现`d1d47ea`、整合`afd7bcf`已ff合master，任务树/分支清理；无服务端更改，现已由0.1.7携带。**下一ready：无**，原S011/S106/S107/S108门禁保持。

并行完成：**S181 done**，按用户要求取消密码最短长度限制（`MIN_PASSWORD_CHARS=1`，只要求非空；ADR-031），并把本机服务端管理员密码离线重置为 `1`、撤销全部旧会话；新包已在 generation 273 上线，loopback 与公网 HTTPS 均验证用 `1` 登录成功。**下一ready：无**，S011/S106/S107/S108 门禁不变。

此前 **S179 done**，0.1.6 已作为 [v0.1.6](https://github.com/xingjianxu/vaultsyncd/releases/tag/v0.1.6) **正式版**发出（非草稿、非预发布、`releases/latest` 指向本版，唯一 Windows x64 ZIP `21b2c2ab…`、16,003,520 字节，源`1e073de`，含 S178 选择框自动读取）；远端 digest 与本地逐字节一致，标签指向同一提交。版本镜像`1e073de`已推送 master；证据`/data/vaultsyncd-dev/S179-release-20261110/`，交接[S179](tasks/S179.md)。**下一ready：无**，S011/S106/S107/S108门禁不变；Latest 属发布渠道属性，不代表首发验收完成。

此前 **S178 done**，Windows Vault 选择框每次打开自动读取授权清单，移除手动按钮；加载/错误/空清单、撤权清选与重开重试均覆盖。桌面build、根typecheck、Chromium14组及原布局、真实Windows S081绑定回归通过，交接[S178](tasks/S178.md)。实现`42edf8e`已ff合master，任务树/分支清理；无服务端更改，现已由0.1.6携带。

此前 **S177 done**，0.1.5 已作为 [v0.1.5](https://github.com/xingjianxu/vaultsyncd/releases/tag/v0.1.5) 预发布发出（非 Latest、唯一 Windows x64 ZIP `a25d6391…`、15,981,059 字节，源`7077fc9`，含 S176 桌面登录门禁）；标签与远端附件 digest 已核对。S179 前的正式版渠道为空（`releases/latest` 404）。证据`/data/vaultsyncd-dev/S177-release-20261110/`，交接[S177](tasks/S177.md)。**下一ready：无**，S106/S107/S108 与 S011 门禁不变。

此前 **S176 done**，桌面启动未登录只显示地址/用户名/密码登录框，持久化确认后进入正式界面；登出与重新认证收回主界面，重启自动恢复。用户名/地址保存私有client.sqlite，令牌沿用Windows凭据管理器，无明文密码/浏览器凭据。schema14/IPC2.9；host91项、根typecheck/build、Windows生产及验收严格clippy与交叉build、Chromium登录/布局、真实Windows三阶段26项及S080/S081回归通过。证据`/data/vaultsyncd-dev/S176-desktop-login-20261110/`，交接[S176](tasks/S176.md)。实现`c1ef19e`已ff合master，任务树/本地分支已清理；当时未发行，现由S177发布0.1.5携带。

此前 **S175 done**，0.1.4 已作为 [v0.1.4](https://github.com/xingjianxu/vaultsyncd/releases/tag/v0.1.4) 预发布发出（非 Latest、唯一 Windows x64 ZIP `041b293a…`，源`fe273d3`，含 S174 ZIP 归属采纳）；本机 master 与 origin/master 同步。此前 **S174 done**，ZIP 同目录覆盖改为自动采纳归属（无备份/无回滚、任意版本方向，跨路径与安装版互转仍拒绝，自动更新入口不变；ADR-030），宿主检查通过（fmt、Windows 交叉严格 clippy、测试 EXE 编译、ZIP 打包契约 8/8、生成一致），**Windows 原生断言未执行**（无运行中客体）。S175时的基线红（IPC示例命令数断言26、实际27）已由S176修正并通过12项。ZIP 未签名、未做 Windows 原生复验，S106/S107/S108 门禁不变；**下一 ready：无**。证据`/data/vaultsyncd-dev/S175-release-20261110/`（0.1.3 证据保留在 `S173-release-20261110/`）。

此前 S168（对应源码仅留本地）已由本卡发行范围覆盖；S166身份确认、S167桌面自动复用/启动agent已验收整合，具体原生及组合验证见对应交接。

最近发行是 [v0.1.7](https://github.com/xingjianxu/vaultsyncd/releases/tag/v0.1.7) **正式版/Latest**，源`9a873c8`，附件 `vaultsyncd-0.1.7-x64-unsigned.zip`（`afddce55…`）；此前 v0.1.6 正式版源`1e073de`、v0.1.5 预发布源`7077fc9`、v0.1.4 源`fe273d3`、v0.1.3 源`d2f9e26`、v0.1.2 源`d976376`保持。在线服务端为S181，运行/开机generation273、源`afbbc78`、schema20、主机pin`a6a335b`，管理员密码已按用户要求重置为弱口令`1`；Vault基目录`/data/vaultsyncd/vaults`。本轮Windows功能已原生验收，性能容量/多机与最终同源发行、线上成对升级门禁仍未验收；0.1.7 进入托盘自动更新渠道，但未做端到端原生升级演练且包未签名。


所有新开发测试目录只允许位于 `/data/vaultsyncd-dev/`；S163 的 temp/lab 迁移及97个GC roots保持。S164 已归档123个旧vs目录、39,360文件完整解包验证后清理；长期归档在 `/data/vaultsyncd-dev/S164-archive-20261109/`，不作缓存删除。

最近部署：S181（源`afbbc78`，generation273、包`5azvb3s`、主机pin`a6a335b`）pure/dry/test/健康/IPC/归属/switch通过，服务端密码策略改为只要求非空；停服后离线改散列并撤销全部37条会话，线上loopback与HTTPS用`1`登录均200、错密码401。旧system`x0sx0d9c`已留GC root；回退需恢复`/data/vaultsyncd-dev/S181-password-reset-20261110T180259/server.sqlite.before`或旧代际。细节见`docs/operations/nixos.md` S181。更早：S172 pure build、dry/test、HTTPS/IPC/身份/归属/switch通过；线上Chromium严格TLS1.3/匿名记录深链门禁/明暗320与1440、CSP及13资产与index保持。schema19→20有一致性备份，原5文件/FK/对象及10服务/3应用保持；无历史伪补录。完整19归档及第二同盘副本验证、隔离恢复升20验证通过，generation271旧system/boot GC roots保留；回退须整套19恢复，不能只降二进制。部署细节见docs/operations/nixos.md S172。

恢复顺序：**S106 → S107 → S108**；须补 T02 三台独立 Windows 与当前候选同源发行证据。S011 的 OS 取消回调仍需原生触发证据；首发验收未完成。各卡的历史“下一 ready”不代表当前待办。

当前平台仅 Linux x64 服务端、Windows 11 x64 客户端（S111/ADR-015）；历史 ARM64 证据保留但不构成恢复门禁。Windows 在宿主构建、客体原生验收；主机、容量、端口与旧工作树须现场复核。

## 未完成任务

以下保留原状态行和恢复条件；详细证据及执行入口见对应交接。

| 编号 | 结果 | 环境 | 前置 | 状态 | 产出提交/工作树 | 验证摘要 | 阻塞 | 交接路径 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| S011 | CFAPI 读取、取消与离线原型 | W | S010 | blocked | 产出提交 `efb0563`（父提交 `a6aed74`；worktree `/wrd/prj/vaultsyncd/.worktrees/s011-s012`，分支 `feat/S011-S012-cfapi-read-write`） | 新增 `cfapi::content`（确定性固定内容源 + 自带 SHA-256（NIST 向量锁定）+ `serve_range` 回填循环，宿主可测）与 `cfapi::provider`（进程级 `Arc` 上下文、`FETCH_DATA` 分段 `CfExecute(TRANSFER_DATA)`、`CANCEL_FETCH_DATA` 原子取消标志、离线/超时明确失败、`wait_idle` 断开前静默）；`examples/cfapi-prototype` 新增 `read`/`cancel`/`hydrate`/`expect`，`create` 去掉“读取必须失败”探测、`inspect` 增 `--allow-hydrated`，条目集合增 16 MiB（范围请求）；登记策略改 `PARTIAL`/`PARTIAL`；新增 `tests/platform/windows/{run-cfapi-hydration.sh,guest-cfapi-hydration.ps1}` 并更新 S010 客体脚本读取探测。宿主 `cargo test -p vaultsyncd-platform-windows`(0，28 passed)、`cargo clippy --all-targets`(0)、`cargo fmt --check`(0)、`cargo xwin clippy`(0)、`build-cross.sh`(0)、`run-cfapi-hydration.sh`(0：46 项 check=pass、0 fail，两端 4 项哈希 MATCH)、`run-cfapi-prototype.sh`(0：21 项 pass)。客体证据：整份读取 `small`/`huge` 字节与 SHA-256 与宿主一致；`hydrate` 取得真正的**范围**请求（`platform_offset=3997696 platform_length=1052672`）；离线未水合以 `0xC000CF11`→`os error 388` 在 1 ms 内失败、已水合离线可读且 `fetch_requests=0`；慢源超时有界失败；传输中断开同步根后循环在 3/16 段停止且 305 ms 返回；客体已清理探针目录并干净关机 | **部分阻塞**：`CANCEL_FETCH_DATA` 在本 build（26300）上 5 条路径均未触发（CancelSynchronousIo 成功但水合继续、CancelIoEx=1168、传输中关句柄、传输中断开、跨进程请求收不到 `FETCH_DATA`）；跨进程提供方唤醒未打通。取消语义已由宿主单测的取消分支 + 传输中断实测覆盖，严格断言入口 `--require-cancel-callback` 保留 | agent/tasks/S011.md |
| S106 | 小文件时延与常规规模基准 | X | S101、S102、S103、S104、S105 | blocked | 基线c59ff19；独立只读前置/资源报告 `docs/reports/performance.md`，文档390cda0已cb9ed20合入，证据/树/分支已收尾 | devShell资源核查：双路6430/128线程、503.54GiB RAM、SSD可用143.05GiB、旋转btrfs21.58TiB；仅1台8GiB/8vCPU Windows，未联系客体、未跑小样本/常规档，p95/RSS/CPU/队列全未测 | S101已由S155按修订范围收尾；S105已fd896fa完成；需准备3独立Windows及SSD/RTT实证，容量本身未证不足，不降1万文件/约10GiB目标 | agent/tasks/S106.md；artifacts/S106 |
| S107 | 正式容量与大文件续传验证 | X | S106 | blocked | 基线cb9ed20；只读正式档预算与恢复证据要求写 `docs/reports/performance.md`，文档dcadee5已0d4fb30合入，证据/树/分支已收尾 | devShell容量/挂载/btrfs/remote均exit0；data21.5808TiB，SSD142.59GiB；有限情景含余量19.54TiB，另累计五端下载stage情景25.54TiB（差约3.96TiB）；正式10万文件/1TiB/50GiB/5设备与重传字节全未测 | S106未验收；五独立Windows及大NTFS卷未准备，须先核实内部副本/stage/历史增长再准入容量，不降目标、不以小样本替代 | agent/tasks/S107.md；artifacts/S107 |
| S108 | 发布清单与最终交接 | X | S101、S102、S103、S104、S105、S106、S107 | blocked | 35项P0索引、README/运维入口与当前限制；文档5f485a5已abdedd3整合；S105 done/代码候选fd896fa；证据、树与分支已收尾 | 两架构各24个历史runtime文件及发布/源码tar哈希核验，b263源三PE哈希/原生启动验证；当前候选全平台包未完成 | S106/S107性能容量、T02三台Windows及最终同源发行证据缺失；未宣称首发完成 | agent/tasks/S108.md；docs/operations/release.md |

## 已完成任务

本轮原生复验结论见 [S141](tasks/S141.md)：客体开机后 bindings/files-ui/write(74/74)/history/S129 Explorer 五阶段/S091 升级演练/S099 安装演练全部通过；S100 维护演练无法复跑（保留用户的 0.1.0 客户端已在历史 S100 验收中卸载，且 S140 升版后旧客户端无法登录新服务端），成对旧/新端到端升级演练需另立卡。

以下沿用原验收结论；验证、限制与生命周期按需读取父卡，依赖与环境见实施手册。过程原文从固定 Git 版本查阅，Git 外 artifacts 保留。S119 的重复行已去重，未重新认定任何历史验收。

| 编号 | 结果 | 产出提交/整合 | 交接 |
| --- | --- | --- | --- |
| S183 | 根 SVG 统一 Windows 应用/窗口/托盘及 Web favicon/登录/顶栏品牌；Windows 原生与 Chromium 通过，generation274 部署健康 | 实现 c37760f、交接6772251已ff合master，任务树/分支清理；未发行客户端 | [S183](tasks/S183.md) |
| S182 | 发布 v0.1.7 正式版（含 S180/S181）；Windows ZIP 唯一附件、远端 digest 与本地一致，设为 Latest | 版本9a873c8已ff合master并推送，任务树/分支清理 | [S182](tasks/S182.md) |
| S181 | 取消密码最短长度限制（只要求非空，ADR-031）；本机管理员密码离线重置为 `1` 并撤销全部会话 | 实现afbbc78已ff合master；包在generation273上线，宿主/Web检查与HTTPS登录验证通过 | [S181](tasks/S181.md) |
| S180 | 桌面更新原生进度、登录版本及受控trial启动修复；Windows升级/回退75项通过 | 实现d1d47ea、整合afd7bcf已ff合master；任务树/分支清理，现已由0.1.7携带 | [S180](tasks/S180.md) |
| S179 | 发布 v0.1.6 正式版（含 S178）；Windows ZIP 唯一附件、远端 digest 与本地一致，设为 Latest | 版本1e073de已ff合master并推送，任务树/分支清理 | [S179](tasks/S179.md) |
| S178 | Vault选择框每次打开自动读取授权清单，加载/错误与选择失效处理 | 实现42edf8e已ff合master，树/分支清理；宿主及真实Windows S081通过 | [S178](tasks/S178.md) |
| S177 | 发布 v0.1.5 预发布（含 S176）；Windows ZIP 唯一附件、远端 digest 与本地一致，一次上传成功 | 版本7077fc9已ff合master并推送，任务树/分支清理 | [S177](tasks/S177.md) |
| S176 | 桌面启动登录框、持久化登录资料及重启会话恢复 | 实现c1ef19e已ff合master并清理；宿主/交叉与真实Windows26项及S080/S081回归通过 | [S176](tasks/S176.md) |
| S175 | 发布 v0.1.4 预发布（含 S174）；Windows ZIP 唯一附件、远端 digest 与本地一致，一次上传成功 | 版本fe273d3已ff合master并推送，任务树/分支清理 | [S175](tasks/S175.md) |
| S173 | 发布 v0.1.3 预发布；Windows ZIP 唯一附件、远端 SHA-256 一致，工具列表延迟误报已修 | 版本d2f9e26、修复532ec19已ff合master并推送，任务树/分支清理 | [S173](tasks/S173.md) |
| S174 | ZIP 同目录覆盖自动采纳归属（无备份/无回滚、任意版本方向；跨路径与安装版互转仍拒绝） | 实现7870992已ff合master，任务树/分支已清理；宿主检查通过，Windows 原生断言待执行 | [S174](tasks/S174.md) |
| S172 | 两端文件同步/传输记录；原生27项、真实浏览器/完整备份恢复及generation272部署通过 | 实现d7339f0、交接fef5f83已ff合master；本卡挂载/树/分支清理 | [S172](tasks/S172.md) |
| S171 | Web右键新建目录/固定版本下载；真实浏览器30项与generation271部署验收 | 实现01a45f1、交接a84d1be已ff合master；任务挂载/树/分支清理 | [S171](tasks/S171.md) |
| S170 | 登录IP、五分钟连接及分页轮询；后端/Web/真实浏览器与generation270生产验收通过 | 实现df8edc8、交接4fd79e1已ff合master，任务挂载/树/分支已清理；主机pin7787429已ff main并清理 | [S170](tasks/S170.md) |
| S169 | 便携发行/GitHub仅Windows ZIP；43项契约+更新4项、真实构建/ZIP/PE/唯一附件预检通过 | 实现91b8e7f、交接b6cde8c已ff合master；任务挂载/树/分支已清理 | [S169](tasks/S169.md) |
| S168 | 发布不再上传运行库对应源码；本地保留并按收据核验，dry-run披露local_only；契约26/26 | 实现7b2f0b9、交接135676f已ff合master；任务树/分支已清理 | [S168](tasks/S168.md) |
| S167 | 桌面自动复用或启动agent；就绪后建窗、失败可见且后台独立；Windows39/39 | 实现401c484、交接eb2f773、组合8c7ad69已ff合master；任务树/分支已清理 | [S167](tasks/S167.md) |
| S166 | 地址自动读取只读Server ID并确认；超时/身份保护、IPC/桌面与Windows功能回归通过 | 实现cbc82ff、交接a80d28c已ff合master；任务树/分支已清理 | [S166](tasks/S166.md) |
| S165 | v0.1.2 预发布；双平台同源构建、8附件远端摘要/来源/非Latest核对通过 | 源d976376已推送；交接889c19f已ff合master，任务挂载/树/分支已清理 | [S165](tasks/S165.md) |
| S164 | 123 个旧vs目录归档，39,360 文件全量恢复摘要/元数据通过后清理；开发目录仅dev根 | a57d364已ff合master；任务树/分支已清理，S134现场保留 | [S164](tasks/S164.md) |
| S160 | 概览显示完整Server ID；Web及真实浏览器读取/错误重试/取消与generation269生产验收通过 | 源85dd2bb、交接762d71d已ff合master；主机a37df31已ff合main，项目/主机任务树及分支清理 | [S160](tasks/S160.md) |
| S159 | Vault路径深链与当前文件容量；Rust/Web/两轮真实浏览器及generation268生产验收通过 | 实现911449b、交接0dba961已ff合master；项目/主机任务树及分支清理 | [S159](tasks/S159.md) |
| S158 | 首个GitHub预发布v0.1.1；同源双平台归档、Linux私有loader及8附件远端哈希通过；修复打包变量与SQLite/zlib依赖 | 源9aa091e已推送；交接f274601已ff合master，发行树/分支清理；并发同编号树保留并提示续接重新编号 | [S158](tasks/S158.md) |
| S101 | 本机同源平台矩阵/安装已验收；S155按用户最新范围取消远端执行阻塞后收尾，不代表当前候选发行通过 | 产品fcbbb93；任务a6bbb56、合并f07aab2；本轮范围修订见S155 | [S101](tasks/S101.md) |
| S152 | 两端共用 Mantine；双端构建/Web九套/桌面布局及Windows五套原生通过，服务端已S157上线；客户端发行另验 | 实现bfdf14e、整合8c1d1d8已ff合master；任务树/分支已清理 | [S152](tasks/S152.md) |
| S153 | 版本历史版服务端部署generation265；真实生产浏览器/资产/数据及既有服务验收通过 | 源89a9cf4；主机pin d4c5fe7、main整合2c96e2f；生命周期见交接 | [S153](tasks/S153.md) |
| S147 | 持久记录客户端连接；Web按账号分页查看活动/会话，schema18迁移与真实浏览器通过；未部署 | 实现5403c3c、整合S148 1e9367f、交接de1f96e已ff合master；任务树/分支已清理 | [S147](tasks/S147.md) |
| S148 | 登录/刷新恢复会话后自动检查服务；退出取消、失败重试、真实 Chromium 验收；未部署 | 实现`3885ab4`；整合/清理见交接 | [S148](tasks/S148.md) |
| S145 | 统一 Web 登录页、匿名全路由门禁、全局会话失效；真实认证/90组布局/五业务回归及generation261线上验收 | 实现6e58bc2、交接c9b092d已ff合master；主机pin18a7df5；任务树/分支已清理 | [S145](tasks/S145.md) |
| S141 | Vault 改名后 Windows 原生复验；修复 RootElement/IPC 版本等改名与升版遗留故障；bindings/files-ui/write/history/S129/S091/S099 原生通过 | 8b82e7d、ee2c863、e2f3b9a、83bc979、bf561f8、6f0bab3 已ff合master，任务树/分支已清理 | [S141](tasks/S141.md) |
| S139 | Web 重命名默认选区排除扩展名；26组真实浏览器与Web回归通过；未部署 | cfb0b11已ff合master，任务树/分支已清理 | [S139](tasks/S139.md) |
| S144 | 部署 `4ed8d9b` 到本机：generation260、schema17、清库重建、管理员 xingjian；生产严格 TLS/浏览器验收 | 源`4ed8d9b`；主机 pin`629c86d` | [S144](tasks/S144.md) |
| S143 | 对象区 `<vault_id>/<aa>/<hash>`；删除旧布局兼容；schema17；管理员默认登录名与 NixOS adminUser | 19a43a3已ff合master，兼容修复4ed8d9b；树/分支已清理 | [S143](tasks/S143.md) |
| S142 | Vault 改名：design/interfaces/operations/README 等文档与 Web 测试脚本；真实 Chromium 两轮通过 | 6b7eb52已ff合master，任务树/分支已清理 | [S142](tasks/S142.md) |
| S140 | 共享目录概念改名 Vault（代码/wire/DB/服务端/客户端通用层）；schema16 迁移、协议 major 2 | b32ce6a已ff合master，任务树/分支已清理 | [S140](tasks/S140.md) |
| S136 | Web 上传/右键复制剪切粘贴删除；修复跨 Vault 同摘要对象冲突（schema15 围栏、legacy 兼容） | 实现f0e0103/0346ec1、审查修复c2d06b0已ff合master；树/分支已清理 | [S136](tasks/S136.md) |
| S135 | 通知版服务端部署本机 generation256；线上通知与资产/健康验收通过 | 源faf7adb；主机b98e05f | [S135](tasks/S135.md) |
| S134 | Web 操作反馈统一 Mantine Notification；五套真实浏览器验收、Nix Web 构建通过 | 实现a1d225a/交接faf7adb已ff合master；树状态见卡片 | [S134](tasks/S134.md) |
| S133 | S132 部署到本机：generation255、pin31186c7、无迁移、生产 S132 断言 | 源b811f41；主机31186c7 | [S133](tasks/S133.md) |
| S132 | 文件列表取消操作列，行上 Mantine Menu 右键菜单（打开/属性/复制路径、Shift+F10、单菜单） | 实现a9f0310/交接b811f41已ff合master；树/分支已清理 | [S132](tasks/S132.md) |
| S131 | S130 部署到本机：generation254、pin1d05cfc、库迁移13→14、生产S130断言 | 源0cd403a；主机1d05cfc | [S131](tasks/S131.md) |
| S130 | 文件浏览器固定列宽/无下划线/人类可读大小/修改日期；迁移0014与快照字段 | 实现a9d9e35/交接0cd403a已ff合master；树/分支已清理 | [S130](tasks/S130.md) |
| S129 | 正式 agent 注册 Explorer；原生云朵/下载勾选/释放/重启验收 | 实现3357d89；交接/ff整合bbddb13；树/分支已清理 | [S129](tasks/S129.md) |
| S001 | 确认主机与同步环境基线 | db7ce53 | [S001](tasks/S001.md) |
| S002 | 建立 agent 入口与上下文提取工具 | cf993b1 | [S002](tasks/S002.md) |
| S003 | 建立 Nix 开发 shell | 8165ee6 | [S003](tasks/S003.md) |
| S004 | 建立最小 Cargo workspace | e249897 | [S004](tasks/S004.md) |
| S005 | 建立 pnpm workspace 与工具版本 | c7df404 | [S005](tasks/S005.md) |
| S006 | 建立任务编排入口与基础 CI | fa5dca2 | [S006](tasks/S006.md) |
| S007 | 准备独立 btrfs 测试位置 | c18029a | [S007](tasks/S007.md) |
| S008 | 准备 Windows 客体与连接说明 | 71ae67c | [S008](tasks/S008.md) |
| S009 | 固定 Windows 交叉编译与客体验证工具链 | f815f1f | [S009](tasks/S009.md) |
| S010 | CFAPI 注册与占位创建原型 | a6aed74 | [S010](tasks/S010.md) |
| S012 | CFAPI 释放与写入保护原型 | 2d90fb7 | [S012](tasks/S012.md) |
| S013 | Linux 存储准入原型 | 6f05033 | [S013](tasks/S013.md) |
| S014 | Linux 普通文件发布原型 | 61a218b | [S014](tasks/S014.md) |
| S015 | 定义身份与有界计数类型 | b83d258、55e2252 | [S015](tasks/S015.md) |
| S016 | 定义状态维度与错误分类 | 01e36a6、a03a3d5、4db6927 | [S016](tasks/S016.md) |
| S017 | 路径与名称纯规则 | a67e61a、29b402e | [S017](tasks/S017.md) |
| S018 | 排除规则与解释 | 2e5310d、97737c5、bcaca8e… | [S018](tasks/S018.md) |
| S019 | 三方规划基本分支 | 690b106、37a0c0a | [S019](tasks/S019.md) |
| S020 | 首次合并规划 | 2a63f13、4edefb8 | [S020](tasks/S020.md) |
| S021 | 内容冲突及稳定副本身份 | ddf6b47、be8c261、4dc2bc0… | [S021](tasks/S021.md) |
| S022 | 移动与删除冲突规划 | 00be4ec、d658f28、68795c6… | [S022](tasks/S022.md) |
| S023 | 规划器操作重排验证 | 0f5191f、e32d185、8523c01… | [S023](tasks/S023.md) |
| S024 | 远端协议最小契约与生成通路 | fc0d551、47bf26c、8c22892 | [S024](tasks/S024.md) |
| S025 | 本机 IPC 最小契约与生成通路 | 7da72c9、5383e3b | [S025](tasks/S025.md) |
| S026 | 服务端数据库迁移框架 | fc3c240、3184949、945ca35… | [S026](tasks/S026.md) |
| S027 | 服务端命名空间表与约束 | 8080579、40963df、745ae2c | [S027](tasks/S027.md) |
| S028 | 版本、事件与幂等表 | f909832、48d6daa、e58f59e… | [S028](tasks/S028.md) |
| S029 | 上传、意图与租约表 | 1a961cb、2906a8f | [S029](tasks/S029.md) |
| S030 | 客户端数据库与状态表 | 0d66f36、52f0632 | [S030](tasks/S030.md) |
| S031 | 客户端事件、传输与意图持久化 | 94ab5ac、c946744 | [S031](tasks/S031.md) |
| S032 | Linux 安全路径访问正式接口 | abd6f56、1929980、ea66765 | [S032](tasks/S032.md) |
| S033 | 不可变对象写入与读取 | 72fd406、4a1baf0、99baf54… | [S033](tasks/S033.md) |
| S034 | 持久提交意图状态机 | 369fe58、1dff617、97e55df… | [S034](tasks/S034.md) |
| S035 | 内容 CAS 与现场复核 | 2209f18、db04f8c、095526a… | [S035](tasks/S035.md) |
| S036 | 幂等重试与冲突内容提交 | 8073e12、da8e4e3、a776c9f | [S036](tasks/S036.md) |
| S037 | 启动恢复与崩溃注入 | 16820b1、808b95c、a21aabc… | [S037](tasks/S037.md) |
| S038 | 目录创建与同 Vault 移动 | 2b4bf00、fb317ad、dc34fbd… | [S038](tasks/S038.md) |
| S039 | 可恢复删除与回收站 | dd27e3a、ad675ad、280c87f… | [S039](tasks/S039.md) |
| S040 | 历史与回收站恢复操作 | ffade51、a9ba6e7、b8bdb95… | [S040](tasks/S040.md) |
| S041 | 服务端进程与配置生命周期 | 9508098、10a08b6、962c51c | [S041](tasks/S041.md) |
| S042 | 首次 owner 初始化与密码认证 | c53f488、bcda69d、6ad456e… | [S042](tasks/S042.md) |
| S043 | 会话、刷新与 Vault 授权 | 011d12f、a498ba5、931ef9c… | [S043](tasks/S043.md) |
| S044 | 管理 Vault 注册与健康 API | add5d58、83e55a1、d4206f1… | [S044](tasks/S044.md) |
| S045 | 上传会话和固定块接收 | 88cc8de、e6c8533、4a88d10… | [S045](tasks/S045.md) |
| S046 | 上传 finalize 与固定版本下载 | f27565e、5205e73、f0c0f84… | [S046](tasks/S046.md) |
| S047 | 操作 HTTP 接口与错误映射 | 6cc3384、ee79f3b、aacd1e0 | [S047](tasks/S047.md) |
| S048 | 一致性快照、changes 与 SSE | f360a9f、5711271 | [S048](tasks/S048.md) |
| S049 | 外部目录初始稳定采集 | ad1bf63、cfebb06、712881e… | [S049](tasks/S049.md) |
| S050 | inotify 与溢出补扫 | 7f75ec5、cecaecc、675b511… | [S050](tasks/S050.md) |
| S051 | 外部移动、删除与挂载失效 | 1b99313、58a90b4、d2c9035… | [S051](tasks/S051.md) |
| S052 | 服务端整合小型 HTTP 闭环 | 282e5dc、5de59d6、e4e1a4c… | [S052](tasks/S052.md) |
| S053 | Rust HTTPS 客户端与认证刷新 | 930fbb0、1e6376f、1a6eb95… | [S053](tasks/S053.md) |
| S054 | 上传下载续传客户端 | 72aeb21、fd61d51、1cf4209… | [S054](tasks/S054.md) |
| S055 | agent 核心端口与确定性执行器 | 49d492c、2af11b0、1f02d72… | [S055](tasks/S055.md) |
| S056 | 本地 generation 与上传快照 | 96c4ab0、96bbb43、26f34e6… | [S056](tasks/S056.md) |
| S057 | 接收事件与应用进度 | 1c4ef4d、41c2d8d、f869eb8… | [S057](tasks/S057.md) |
| S058 | 首次绑定与可恢复合并执行 | 2f43607、28dba66、a2e893c… | [S058](tasks/S058.md) |
| S059 | 本地受控应用与重启核对 | 5b826e0、ad3807d、f70947a… | [S059](tasks/S059.md) |
| S060 | 旧游标与 epoch 重建 | 6a6cc2f、6924348、ac3d662… | [S060](tasks/S060.md) |
| S061 | 暂停、模式切换与解除绑定 | 520501f、5529270 | [S061](tasks/S061.md) |
| S062 | 下载优先级与重试调度 | deafbe3、ab44a59、6fabd35… | [S062](tasks/S062.md) |
| S063 | 手动释放的核心安全条件 | 9cf3d80、82ec864 | [S063](tasks/S063.md) |
| S064 | agent 状态、问题与恢复导出 | b0bb69a | [S064](tasks/S064.md) |
| S065 | Windows 安全文件与本地扫描 | 03055ac、1395e15 | [S065](tasks/S065.md) |
| S066 | CFAPI 正式注册与元数据更新 | 20367d1 | [S066](tasks/S066.md) |
| S067 | CFAPI hydration 接入传输执行器 | 6e089ff、8a0e921、b9bcbce | [S067](tasks/S067.md) |
| S068 | Windows 写入检测与使用保护 | f5af3db、d874f54、1847d9f | [S068](tasks/S068.md) |
| S069 | Windows 手动释放与恢复 | 59985ea、c9b95a0、e542446… | [S069](tasks/S069.md) |
| S070 | Windows agent 进程与单实例 | e0200e3、f5ce872、97c0735… | [S070](tasks/S070.md) |
| S071 | Windows named pipe 传输与身份 | 8036466、323469f、e3a2e51… | [S071](tasks/S071.md) |
| S072 | 本机管理命令完整接入 | ebc76ae、798b36a、866f2fc… | [S072](tasks/S072.md) |
| S073 | Windows CLI 最小管理入口 | d7742ba、32e716b、ce7863d | [S073](tasks/S073.md) |
| S074 | 三台客户端基础互通 | 5e5e98f、1452d36、0d1c673… | [S074](tasks/S074.md) |
| S075 | Linux 在线管理 socket 与 CLI | ace6837、526bde0、1f415b8… | [S075](tasks/S075.md) |
| S076 | 前端公共组件与两应用壳 | cb82498 | [S076](tasks/S076.md) |
| S077 | Web 登录与会话管理 | 89c7b2d | [S077](tasks/S077.md) |
| S078 | Web Vault 与服务健康页面 | 5246a05、103a0b1 | [S078](tasks/S078.md) |
| S079 | Web 历史、冲突与回收站 | 4ef8174 | [S079](tasks/S079.md) |
| S080 | Tauri Windows 壳与最小 IPC 桥 | 692e18f | [S080](tasks/S080.md) |
| S081 | 桌面连接与 Binding 页面 | 94dc1af | [S081](tasks/S081.md) |
| S082 | 桌面文件状态与手动操作 | 713d907 | [S082](tasks/S082.md) |
| S083 | 桌面历史与系统通知 | ecd05fa、58188c4、7c77b25 | [S083](tasks/S083.md) |
| S084 | 客户端未上传数据恢复演练 | e1ba9a8 | [S084](tasks/S084.md) |
| S085 | 历史保留与对象引用标记 | ae6a8c7 | [S085](tasks/S085.md) |
| S086 | GC 删除协调与临时清理 | 7c79ae1、accc684 | [S086](tasks/S086.md) |
| S087 | 一致性备份快照与租约 | 813cf39、84c7512 | [S087](tasks/S087.md) |
| S088 | 备份导出、校验与释放接口 | 99cc17a、0bc6846、f1302ba | [S088](tasks/S088.md) |
| S089 | 离线恢复到空目录 | 59663a5、fae55cc、51eecbb… | [S089](tasks/S089.md) |
| S090 | Web 备份管理入口 | e73a5f6 | [S090](tasks/S090.md) |
| S091 | 协议与数据库升级演练 | 2eedab8、cf987fc、515b347 | [S091](tasks/S091.md) |
| S092 | 构建依赖与协议边界检查 | db95ba8 | [S092](tasks/S092.md) |
| S093 | 发布版本与产物清单 | d59a7b2、7993e72、fbc0d27 | [S093](tasks/S093.md) |
| S094 | Linux 服务端 Nix 构建包 | 83d0098、90a3f21、3b1fbcc | [S094](tasks/S094.md) |
| S095 | NixOS 服务模块 | a70f755、9243d14、eeb8f66 | [S095](tasks/S095.md) |
| S096 | 在本机启用隔离服务实例 | 99a7ed4 | [S096](tasks/S096.md) |
| S097 | HTTPS 与同源 Web 部署 | d90a263、b615fd9、b345ac9… | [S097](tasks/S097.md) |
| S098 | 通用 Linux 包与容器配置 | 048aff1、62115d6、a14848f… | [S098](tasks/S098.md) |
| S099 | Windows 安装包与后台启动 | ca510ab、cf1d174 | [S099](tasks/S099.md) |
| S100 | Windows 升级与卸载保护 | 3a2e2aa、d0392f4 | [S100](tasks/S100.md) |
| S102 | 事件异常与旧设备集成验收 | 1a51f04、81699c4、60200a7 | [S102](tasks/S102.md) |
| S103 | 文件与挂载故障集成验收 | 8e4ad2b、81dcb16、b36e31e | [S103](tasks/S103.md) |
| S104 | GC、备份与崩溃联合验收 | c03114d、71013d9、326b469 | [S104](tasks/S104.md) |
| S105 | 安全与操作可达性验收 | b26372e、8cd3a8a、81ceb6d… | [S105](tasks/S105.md) |
| S109 | Windows 免安装 ZIP（状态留本机） | 367faf5、3512865 | [S109](tasks/S109.md) |
| S110 | 一键构建多平台 release 归档 | d3209ab、81340f6、1c61929；master已ff整合，任务树/分支已清理 | [S110](tasks/S110.md) |
| S111 | 移除 Linux ARM64 构建与验证范围 | 实现9e62b03、交接ecb21dc；master已ff整合，任务树/分支已清理 | [S111](tasks/S111.md) |
| S112 | 本机域名部署与NixOS功能选项 | 项目4856738、c94aca4已ff合入master；主机9c2133f、56bd93b已合main，任务树均清理 | [S112](tasks/S112.md) |
| S113 | 主机用户运行与 pure 一键部署 | 项目4071fd4/6b967bd，主机32bfeb5已ff合main；整合/清理见批次 | [S113](tasks/S113.md) |
| S114 | 客户端目录选择与 Vault 名称显示 | 实现85143c6、交接bfa5204已ff合master；任务树/分支已清理 | [S114](tasks/S114.md) |
| S115 | 服务端 Web HeroUI React 完整重构 | d90b385已ff合master；任务树/分支已清理；基线5c5c797 | [S115](tasks/S115.md) |
| S116 | 将当前服务端部署到本机 | 源f3b8979、主机15cfc07；项目整合/清理见批次 | [S116](tasks/S116.md) |
| S117 | Web 管理界面紧凑化与 HeroUI 控件统一 | 实现3631c29、交接495a04d已ff合master；任务树/分支已清理；基线f3b8979 | [S117](tasks/S117.md) |
| S118 | 将最新服务端（S117 之后）部署到本机 | 源9da5ee8、主机42d99c5/b0ad622；项目整合/清理见批次 | [S118](tasks/S118.md) |
| S119 | 服务端 Web 从 HeroUI 迁移到 Mantine | 实现b16d9a1、交接b5e96f2已ff合master；任务树/分支已清理；基线e598106 | [S119](tasks/S119.md) |
| S120 | 将 Mantine 版最新服务端部署到本机 | 源b5e96f2、主机0073fd6；项目整合/清理见批次 | [S120](tasks/S120.md) |
| S121 | Web 顶栏分区与 48px 高度并部署到本机 | 实现354bbc4/交接c8d5429已ff合master，任务树/分支已清理；主机6cc497b | [S121](tasks/S121.md) |
| S122 | Web Vault 文件浏览器与属性查看 | 实现25dbf2b已ff合master，任务树/分支已清理；基线209a8a0 | [S122](tasks/S122.md) |
| S123 | 将最新服务端（S122 文件浏览器）部署到本机 | 源6395dfa、主机pin207099c；项目整合/清理见批次 | [S123](tasks/S123.md) |
| S124 | Web 文件浏览器左右分栏与 URL 目录定位 | 986c35c已ff合master；任务树/分支已清理，见批次；基线1e2d778 | [S124](tasks/S124.md) |
| S125 | 将最新服务端（S124 分栏文件浏览器）部署到本机 | 源30daa88、主机pin dc7dd42；项目整合/清理见批次；基线2f21788 | [S125](tasks/S125.md) |
| S127 | Mantine Tree 目录树重构 | 实现5ad2634/交接b0fbb0c已ff合master；任务树/分支已清理 | [S127](tasks/S127.md) |
| S128 | 收紧目录树并部署生产 | 实现f7609ad/交接9d58fd7已ff合master；主机6300214/generation253；任务树/分支已清理 | [S128](tasks/S128.md) |

## 本轮维护

| 编号 | 状态 | 产出提交 | 交接 |
| --- | --- | --- | --- |
| S184 | done | 实现`82dcddd`、交接`0ff8684`已ff合master；新增`tools/build.sh`（无版本号、输出根`dist/`便携程序目录），`release.sh`发布链未改；任务树/分支已清理 | [S184](tasks/S184.md) |
| S173 | done | 版本`d2f9e26`、工具修复`532ec19`已ff合master并推送；v0.1.3预发布上传、远端摘要核对与两任务树/分支清理完成 | [S173](tasks/S173.md) |
| S163 | done | 510d70d已ff合master；temp/lab迁入dev、97 GC roots/清单/健康/34单测及真实卷通过，任务树/分支已清理 | [S163](tasks/S163.md) |
| S162 | done | 实现7d5cb39、整合0ab9184/6891501、修复f5c4a24；验收交接e1fb945已ff合master，原生ZIP/安装版及宿主门通过；任务树/分支已清理 | [S162](tasks/S162.md) |
| S161 | done | 8d92da9已ff合master；历史build/deploy迁入temp同名目录，482文件/72 GC roots/快照身份/健康通过；任务树及分支清理 | [S161](tasks/S161.md) |
| S157 | done | 源5a71062；主机pin f7d1997/main整合a02b61e；generation266；交接3ced4a5/整合4dd1ea6已ff合master，两任务树/分支已清理 | [S157](tasks/S157.md) |
| S155 | done | 实现70983b7、整合b291440、交接41636ba已ff合master；本地5/5、xtask23/23、干净提交检查通过；任务树/分支已清理 | [S155](tasks/S155.md) |
| S156 | done | 实现1fbe353、交接/整合06d7427已ff合master，任务树/分支已清理；19+9+8项通过；gh未登录、未真实上传 | [S156](tasks/S156.md) |
| S154 | done | 实现/验收509940a已ff合master，任务树/分支已清理；已由S157部署服务端 | [S154](tasks/S154.md) |
| S151 | done | 项目63be203已ff合master；主机c20199e/main整合a3a62a9；generation264，两任务树/分支已清理 | [S151](tasks/S151.md) |
| S150 | done | 实现/验收65d56ae已ff合master，任务树/分支已清理；未部署生产 | [S150](tasks/S150.md) |
| S149 | done | 源d9cfee9；主机pin f5153d8/main整合fb293d5；交接98de5a5已ff合master，两任务树/分支已清理 | [S149](tasks/S149.md) |
| S126 | done | d50c6ac | [S126](tasks/S126.md) |
| S137 | done | 999bbee已ff合master，任务树/分支已清理 | [S137](tasks/S137.md) |
| S138 | done | 源5689be1/主机9930ec7/gen258；交接a89debb已ff，任务树/分支已清理 | [S138](tasks/S138.md) |
| S146 | done | 实现f67b8c1；主机13f0dfc已ff合main，原始清理证据在/data/vaultsyncd-temp/S146-cleanup；项目整合/清理见批次 | [S146](tasks/S146.md) |
