# 实施进度

当前：**S147 done**（客户端连接记录与查看，未部署）；S148自动连接已整合复验。S001—S148 共148卡：143 done、5 blocked；**下一 ready：无**。主机generation262，正式服务仍为S145源`6e58bc2`。验收见[S147](tasks/S147.md)，临时目录约定见[S146](tasks/S146.md)。

最近部署：[S145](tasks/S145.md) 源`6e58bc26fcef09faf675229889693b593f6e9c4f`、主机pin`18a7df5`、generation261；pure部署及严格TLS生产8路由匿名门禁、登录后文件浏览/刷新/退出/后退、明暗320/1440、全量资产/CSP/cookie、ServerId/schema17、10服务/3应用与主机锁均通过。不清库、不改生产内容，无schema迁移；回退入口generation260，按S113先test/健康再switch并同步pin，未实际演练。S144清库历史与留档仍见[S144](tasks/S144.md)，更早部署不代表当前现场。

恢复顺序：**S101 hosted → S106 → S107 → S108**；须补 T02 三台独立 Windows 与当前候选同源发行证据。S011 的 OS 取消回调仍需原生触发证据；首发验收未完成。各卡的历史“下一 ready”不代表当前待办。

当前平台仅 Linux x64 服务端、Windows 11 x64 客户端（S111/ADR-015）；历史 ARM64 证据保留但不构成恢复门禁。Windows 在宿主构建、客体原生验收；主机、容量、端口与旧工作树须现场复核。

## 未完成任务

以下保留原状态行和恢复条件；详细证据及执行入口见对应交接。

| 编号 | 结果 | 环境 | 前置 | 状态 | 产出提交/工作树 | 验证摘要 | 阻塞 | 交接路径 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| S011 | CFAPI 读取、取消与离线原型 | W | S010 | blocked | 产出提交 `efb0563`（父提交 `a6aed74`；worktree `/wrd/prj/vaultsyncd/.worktrees/s011-s012`，分支 `feat/S011-S012-cfapi-read-write`） | 新增 `cfapi::content`（确定性固定内容源 + 自带 SHA-256（NIST 向量锁定）+ `serve_range` 回填循环，宿主可测）与 `cfapi::provider`（进程级 `Arc` 上下文、`FETCH_DATA` 分段 `CfExecute(TRANSFER_DATA)`、`CANCEL_FETCH_DATA` 原子取消标志、离线/超时明确失败、`wait_idle` 断开前静默）；`examples/cfapi-prototype` 新增 `read`/`cancel`/`hydrate`/`expect`，`create` 去掉“读取必须失败”探测、`inspect` 增 `--allow-hydrated`，条目集合增 16 MiB（范围请求）；登记策略改 `PARTIAL`/`PARTIAL`；新增 `tests/platform/windows/{run-cfapi-hydration.sh,guest-cfapi-hydration.ps1}` 并更新 S010 客体脚本读取探测。宿主 `cargo test -p vaultsyncd-platform-windows`(0，28 passed)、`cargo clippy --all-targets`(0)、`cargo fmt --check`(0)、`cargo xwin clippy`(0)、`build-cross.sh`(0)、`run-cfapi-hydration.sh`(0：46 项 check=pass、0 fail，两端 4 项哈希 MATCH)、`run-cfapi-prototype.sh`(0：21 项 pass)。客体证据：整份读取 `small`/`huge` 字节与 SHA-256 与宿主一致；`hydrate` 取得真正的**范围**请求（`platform_offset=3997696 platform_length=1052672`）；离线未水合以 `0xC000CF11`→`os error 388` 在 1 ms 内失败、已水合离线可读且 `fetch_requests=0`；慢源超时有界失败；传输中断开同步根后循环在 3/16 段停止且 305 ms 返回；客体已清理探针目录并干净关机 | **部分阻塞**：`CANCEL_FETCH_DATA` 在本 build（26300）上 5 条路径均未触发（CancelSynchronousIo 成功但水合继续、CancelIoEx=1168、传输中关句柄、传输中断开、跨进程请求收不到 `FETCH_DATA`）；跨进程提供方唤醒未打通。取消语义已由宿主单测的取消分支 + 传输中断实测覆盖，严格断言入口 `--require-cancel-callback` 保留 | agent/tasks/S011.md |
| S101 | CI 平台矩阵与安装验证 | X | S092、S098、S100 | blocked | 产品源fcbbb93；任务a6bbb56；master合并f07aab2，任务树/分支已清理 | 本机同源Linux双架构与Windows安装/CFAPI/e2e完成；portable914/0、Linux633/0，CFAPI5/5；NixOS与Ubuntu分列，ARM为全系统TCG | 仅hosted：无remote/runner，未执行Actions；需配置远端、Windows编译及独占platform-lab runner后运行工作流 | agent/tasks/S101.md（+a/b）；artifacts/S101/public/acceptance-matrix.json |
| S106 | 小文件时延与常规规模基准 | X | S101、S102、S103、S104、S105 | blocked | 基线c59ff19；独立只读前置/资源报告 `docs/reports/performance.md`，文档390cda0已cb9ed20合入，证据/树/分支已收尾 | devShell资源核查：双路6430/128线程、503.54GiB RAM、SSD可用143.05GiB、旋转btrfs21.58TiB；仅1台8GiB/8vCPU Windows，未联系客体、未跑小样本/常规档，p95/RSS/CPU/队列全未测 | S101 hosted无remote/runner/Actions证据；S105已fd896fa完成，hosted前置仍阻塞；需准备3独立Windows及SSD/RTT实证，容量本身未证不足，不降1万文件/约10GiB目标 | agent/tasks/S106.md；artifacts/S106 |
| S107 | 正式容量与大文件续传验证 | X | S106 | blocked | 基线cb9ed20；只读正式档预算与恢复证据要求写 `docs/reports/performance.md`，文档dcadee5已0d4fb30合入，证据/树/分支已收尾 | devShell容量/挂载/btrfs/remote均exit0；data21.5808TiB，SSD142.59GiB；有限情景含余量19.54TiB，另累计五端下载stage情景25.54TiB（差约3.96TiB）；正式10万文件/1TiB/50GiB/5设备与重传字节全未测 | S106未验收，追溯S101 hosted缺口；五独立Windows及大NTFS卷未准备，须先核实内部副本/stage/历史增长再准入容量，不降目标、不以小样本替代 | agent/tasks/S107.md；artifacts/S107 |
| S108 | 发布清单与最终交接 | X | S101、S102、S103、S104、S105、S106、S107 | blocked | 35项P0索引、README/运维入口与当前限制；文档5f485a5已abdedd3整合；S105 done/代码候选fd896fa；证据、树与分支已收尾 | 两架构各24个历史runtime文件及发布/源码tar哈希核验，b263源三PE哈希/原生启动验证；当前候选全平台包未完成 | S101 hosted、S106/S107性能容量、T02三台Windows及最终同源发行证据缺失；未宣称首发完成 | agent/tasks/S108.md；docs/operations/release.md |

## 已完成任务

本轮原生复验结论见 [S141](tasks/S141.md)：客体开机后 bindings/files-ui/write(74/74)/history/S129 Explorer 五阶段/S091 升级演练/S099 安装演练全部通过；S100 维护演练无法复跑（保留用户的 0.1.0 客户端已在历史 S100 验收中卸载，且 S140 升版后旧客户端无法登录新服务端），成对旧/新端到端升级演练需另立卡。

以下沿用原验收结论；验证、限制与生命周期按需读取父卡，依赖与环境见实施手册。过程原文从固定 Git 版本查阅，Git 外 artifacts 保留。S119 的重复行已去重，未重新认定任何历史验收。

| 编号 | 结果 | 产出提交/整合 | 交接 |
| --- | --- | --- | --- |
| S147 | 持久记录客户端连接；Web按账号分页查看活动/会话，schema18迁移与真实浏览器通过；未部署 | 实现5403c3c，整合S148 1e9367f；目标整合/清理待执行 | [S147](tasks/S147.md) |
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
| S126 | done | d50c6ac | [S126](tasks/S126.md) |
| S137 | done | 999bbee已ff合master，任务树/分支已清理 | [S137](tasks/S137.md) |
| S138 | done | 源5689be1/主机9930ec7/gen258；交接a89debb已ff，任务树/分支已清理 | [S138](tasks/S138.md) |
| S146 | done | 实现f67b8c1；主机13f0dfc已ff合main，原始清理证据在/data/vaultsyncd-temp/S146-cleanup；项目整合/清理见批次 | [S146](tasks/S146.md) |
