# 实施进度

当前：S001—S113 共 **113 卡，108 done、5 blocked**；无 todo / in_progress，**当前无 ready 卡**。S113已将本机服务及正式目录切换为 `xingjian:users`，一键pure部署至generation245，固定项目6b967bd；入口与更新见 [部署文档](../docs/operations/nixos.md#s113-主机用户运行与-pure-一键部署2026-11-07)。

用户决定（2026-11-07，S111/ADR-015）：Linux ARM64 从脚本、代码、发布、编译与测试范围移除；Linux x64 与 Windows x64 保留。此前双架构记录均为历史证据，不再形成 ARM64 门禁。上一轮约定提交 `b585fc4`；S111 已实现并通过验收，原 5 张 blocked 卡恢复条件中的 ARM64 范围由本决定覆盖，其他缺口保留。

最近完成：S113支持既有主机账号与 `./tools/deploy-local.sh`。普通/专用用户模块、pure主机构建、65项包内测试、HTTPS提交下载/重启/无特权备份通过；真实一键pin更新、test/switch245、来源/UID/全部正式文件归属及旧身份/版本核对通过。主机32bfeb5已ff合入main，锁及SSH/10项服务与3应用保持。项目实现4071fd4、部署源6b967bd；交接及整合/清理见 [S113](tasks/S113.md) 与 [批次](tasks/S113-batch.md)。旧数据与离线备份保留，原首发门禁不变。

恢复顺序：先恢复 **S101 hosted CI**（缺 remote、runner 和 Actions 实证），再推进 **S106 → S107 → S108**；另须补 T02 三台独立 Windows 与最终同源发行证据。S011 的 OS 取消回调未触发仍为历史原型阻塞，保留其原生验证条件。当前不宣称首发验收完成。

S110 历史发行：S110统一入口 `./tools/release.sh`，正式源 `81340f6` 已真实构建Linux x64/ARM64与Windows x64 ZIP；包在 `dist/releases/v0.1.1-81340f689b98/`，15项SHA及归档逐文件核对通过。验收交接 `1c61929` 已ff-only合入master，任务树/分支已清理；详见 [S110](tasks/S110.md) 与 [批次](tasks/S110-batch.md)。本次只构建，不代替Windows/Ubuntu原生或S108最终门禁；S109原生历史证据仍见其交接。

资源与边界：已有 NixOS/btrfs、Windows 11 NTFS/CFAPI 及 Ubuntu 双架构历史证据；ARM64 为全系统 TCG，不代表原生性能。Windows 构建在宿主、原生验收在客体，产物 unsigned-test；S112已部署可信域名HTTPS；旧开发CA仅用于历史试用。历史版本、空间和端口不能代替现场复核。整理时 Git 仅有 master 主工作区/本地分支；下表工作树信息转入历史交接，不作为仍可进入的续接位置。

文档维护（2026-11-07）：104 张 done 父卡整理为结论与证据入口，子卡、批次记录及 5 张 blocked 卡保留；原始全文固定在 `599ffe1fe6652409c53c772ae103f193afc737b7`，可用 `git show <该提交>:<原路径>` 读取。验收标准保留在实施手册；详细接口/运维继续以 docs 为准。原始日志仍在 Git 外，Git 历史不能替代 artifacts 备份。本次没有重跑产品验收或改变任务状态。

维护验证：纯文档在 `/wrd/prj/vaultsyncd` 的 master 原地完成，初始工作区干净，基线为上述固定提交。在 `nix develop --command python3` 内核对 109 卡状态/前置与原任务卡正文一致、5 张 blocked 卡及其状态行不变、104 份原文可从固定提交完整恢复、446 个本地链接有效；子卡/批次/源码/原始证据未改动，`git diff --check` 通过。当前无 ready 卡；恢复条件仍按 S101 等 blocked 交接执行。

状态：`todo` 未开始；`in_progress` 执行中；`blocked` 缺资源/前置；`done` 沿用已记录验收。环境/前置保留原索引；已完成卡的实际验证、限制及生命周期只在交接展开。**“历史限制见交接”不表示新增当前阻塞**，后续覆盖范围须读对应卡，不自动推定解决。

| 编号 | 结果 | 环境 | 前置 | 状态 | 产出提交/工作树 | 验证摘要 | 阻塞 | 交接路径 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| S001 | 确认主机与同步环境基线 | L | 无 | done | db7ce53 | 历史验收见交接 | 无 | [S001](tasks/S001.md) |
| S002 | 建立 agent 入口与上下文提取工具 | L | S001 | done | cf993b1 | 历史验收见交接 | 无 | [S002](tasks/S002.md) |
| S003 | 建立 Nix 开发 shell | L | S001、S002 | done | 8165ee6 | 历史验收见交接 | 无 | [S003](tasks/S003.md) |
| S004 | 建立最小 Cargo workspace | L | S003 | done | e249897 | 历史验收见交接 | 无 | [S004](tasks/S004.md) |
| S005 | 建立 pnpm workspace 与工具版本 | L | S003 | done | c7df404 | 历史验收见交接 | 无 | [S005](tasks/S005.md) |
| S006 | 建立任务编排入口与基础 CI | L | S004、S005 | done | fa5dca2 | 历史验收见交接 | 历史限制见交接 | [S006](tasks/S006.md) |
| S007 | 准备独立 btrfs 测试位置 | L | S001、S003 | done | c18029a | 历史验收见交接 | 历史限制见交接 | [S007](tasks/S007.md) |
| S008 | 准备 Windows 客体与连接说明 | L | S001、S007 | done | 71ae67c | 历史验收见交接 | 历史限制见交接 | [S008](tasks/S008.md) |
| S009 | 固定 Windows 交叉编译与客体验证工具链 | X | S008、S004、S005 | done | f815f1f | 历史验收见交接 | 历史限制见交接 | [S009](tasks/S009.md) |
| S010 | CFAPI 注册与占位创建原型 | W | S009 | done | a6aed74 | 历史验收见交接 | 历史限制见交接 | [S010](tasks/S010.md) |
| S011 | CFAPI 读取、取消与离线原型 | W | S010 | blocked | 产出提交 `efb0563`（父提交 `a6aed74`；worktree `/wrd/prj/vaultsyncd/.worktrees/s011-s012`，分支 `feat/S011-S012-cfapi-read-write`） | 新增 `cfapi::content`（确定性固定内容源 + 自带 SHA-256（NIST 向量锁定）+ `serve_range` 回填循环，宿主可测）与 `cfapi::provider`（进程级 `Arc` 上下文、`FETCH_DATA` 分段 `CfExecute(TRANSFER_DATA)`、`CANCEL_FETCH_DATA` 原子取消标志、离线/超时明确失败、`wait_idle` 断开前静默）；`examples/cfapi-prototype` 新增 `read`/`cancel`/`hydrate`/`expect`，`create` 去掉“读取必须失败”探测、`inspect` 增 `--allow-hydrated`，条目集合增 16 MiB（范围请求）；登记策略改 `PARTIAL`/`PARTIAL`；新增 `tests/platform/windows/{run-cfapi-hydration.sh,guest-cfapi-hydration.ps1}` 并更新 S010 客体脚本读取探测。宿主 `cargo test -p vaultsyncd-platform-windows`(0，28 passed)、`cargo clippy --all-targets`(0)、`cargo fmt --check`(0)、`cargo xwin clippy`(0)、`build-cross.sh`(0)、`run-cfapi-hydration.sh`(0：46 项 check=pass、0 fail，两端 4 项哈希 MATCH)、`run-cfapi-prototype.sh`(0：21 项 pass)。客体证据：整份读取 `small`/`huge` 字节与 SHA-256 与宿主一致；`hydrate` 取得真正的**范围**请求（`platform_offset=3997696 platform_length=1052672`）；离线未水合以 `0xC000CF11`→`os error 388` 在 1 ms 内失败、已水合离线可读且 `fetch_requests=0`；慢源超时有界失败；传输中断开同步根后循环在 3/16 段停止且 305 ms 返回；客体已清理探针目录并干净关机 | **部分阻塞**：`CANCEL_FETCH_DATA` 在本 build（26300）上 5 条路径均未触发（CancelSynchronousIo 成功但水合继续、CancelIoEx=1168、传输中关句柄、传输中断开、跨进程请求收不到 `FETCH_DATA`）；跨进程提供方唤醒未打通。取消语义已由宿主单测的取消分支 + 传输中断实测覆盖，严格断言入口 `--require-cancel-callback` 保留 | agent/tasks/S011.md |
| S012 | CFAPI 释放与写入保护原型 | W | S011 | done | 2d90fb7 | 历史验收见交接 | 历史限制见交接 | [S012](tasks/S012.md) |
| S013 | Linux 存储准入原型 | L | S007、S004 | done | 6f05033 | 历史验收见交接 | 历史限制见交接 | [S013](tasks/S013.md) |
| S014 | Linux 普通文件发布原型 | L | S013 | done | 61a218b | 历史验收见交接 | 无 | [S014](tasks/S014.md) |
| S015 | 定义身份与有界计数类型 | L | S004 | done | b83d258、55e2252 | 历史验收见交接 | 历史限制见交接 | [S015](tasks/S015.md) |
| S016 | 定义状态维度与错误分类 | L | S015 | done | 01e36a6、a03a3d5、4db6927 | 历史验收见交接 | 无 | [S016](tasks/S016.md) |
| S017 | 路径与名称纯规则 | L | S015 | done | a67e61a、29b402e | 历史验收见交接 | 历史限制见交接 | [S017](tasks/S017.md) |
| S018 | 排除规则与解释 | L | S017 | done | 2e5310d、97737c5、bcaca8e… | 历史验收见交接 | 历史限制见交接 | [S018](tasks/S018.md) |
| S019 | 三方规划基本分支 | L | S015、S016、S017 | done | 690b106、37a0c0a | 历史验收见交接 | 无 | [S019](tasks/S019.md) |
| S020 | 首次合并规划 | L | S019 | done | 2a63f13、4edefb8 | 历史验收见交接 | 无 | [S020](tasks/S020.md) |
| S021 | 内容冲突及稳定副本身份 | L | S019 | done | ddf6b47、be8c261、4dc2bc0… | 历史验收见交接 | 无 | [S021](tasks/S021.md) |
| S022 | 移动与删除冲突规划 | L | S020、S021 | done | 00be4ec、d658f28、68795c6… | 历史验收见交接 | 无 | [S022](tasks/S022.md) |
| S023 | 规划器操作重排验证 | L | S022 | done | 0f5191f、e32d185、8523c01… | 历史验收见交接 | 历史限制见交接 | [S023](tasks/S023.md) |
| S024 | 远端协议最小契约与生成通路 | L | S015、S006 | done | fc0d551、47bf26c、8c22892 | 历史验收见交接 | 无 | [S024](tasks/S024.md) |
| S025 | 本机 IPC 最小契约与生成通路 | L | S016、S024 | done | 7da72c9、5383e3b | 历史验收见交接 | 历史限制见交接 | [S025](tasks/S025.md) |
| S026 | 服务端数据库迁移框架 | L | S015、S007 | done | fc3c240、3184949、945ca35… | 历史验收见交接 | 无 | [S026](tasks/S026.md) |
| S027 | 服务端命名空间表与约束 | L | S026 | done | 8080579、40963df、745ae2c | 历史验收见交接 | 无 | [S027](tasks/S027.md) |
| S028 | 版本、事件与幂等表 | L | S027 | done | f909832、48d6daa、e58f59e… | 历史验收见交接 | 历史限制见交接 | [S028](tasks/S028.md) |
| S029 | 上传、意图与租约表 | L | S028 | done | 1a961cb、2906a8f | 历史验收见交接 | 历史限制见交接 | [S029](tasks/S029.md) |
| S030 | 客户端数据库与状态表 | L | S015、S016、S026 | done | 0d66f36、52f0632 | 历史验收见交接 | 历史限制见交接 | [S030](tasks/S030.md) |
| S031 | 客户端事件、传输与意图持久化 | L | S030 | done | 94ab5ac、c946744 | 历史验收见交接 | 无 | [S031](tasks/S031.md) |
| S032 | Linux 安全路径访问正式接口 | L | S014、S017 | done | abd6f56、1929980、ea66765 | 历史验收见交接 | 无 | [S032](tasks/S032.md) |
| S033 | 不可变对象写入与读取 | L | S029、S032 | done | 72fd406、4a1baf0、99baf54… | 历史验收见交接 | 无 | [S033](tasks/S033.md) |
| S034 | 持久提交意图状态机 | L | S033 | done | 369fe58、1dff617、97e55df… | 历史验收见交接 | 历史限制见交接 | [S034](tasks/S034.md) |
| S035 | 内容 CAS 与现场复核 | L | S034、S028 | done | 2209f18、db04f8c、095526a… | 历史验收见交接 | 历史限制见交接 | [S035](tasks/S035.md) |
| S036 | 幂等重试与冲突内容提交 | L | S035、S021 | done | 8073e12、da8e4e3、a776c9f | 历史验收见交接 | 无 | [S036](tasks/S036.md) |
| S037 | 启动恢复与崩溃注入 | L | S036 | done | 16820b1、808b95c、a21aabc… | 历史验收见交接 | 无 | [S037](tasks/S037.md) |
| S038 | 目录创建与同 Root 移动 | L | S037、S022 | done | 2b4bf00、fb317ad、dc34fbd… | 历史验收见交接 | 无 | [S038](tasks/S038.md) |
| S039 | 可恢复删除与回收站 | L | S038 | done | dd27e3a、ad675ad、280c87f… | 历史验收见交接 | 历史限制见交接 | [S039](tasks/S039.md) |
| S040 | 历史与回收站恢复操作 | L | S039 | done | ffade51、a9ba6e7、b8bdb95… | 历史验收见交接 | 历史限制见交接 | [S040](tasks/S040.md) |
| S041 | 服务端进程与配置生命周期 | L | S037、S024 | done | 9508098、10a08b6、962c51c | 历史验收见交接 | 无 | [S041](tasks/S041.md) |
| S042 | 首次 owner 初始化与密码认证 | L | S041、S026 | done | c53f488、bcda69d、6ad456e… | 历史验收见交接 | 历史限制见交接 | [S042](tasks/S042.md) |
| S043 | 会话、刷新与 Root 授权 | L | S042 | done | 011d12f、a498ba5、931ef9c… | 历史验收见交接 | 历史限制见交接 | [S043](tasks/S043.md) |
| S044 | 管理 Root 注册与健康 API | L | S043、S032、S027 | done | add5d58、83e55a1、d4206f1… | 历史验收见交接 | 历史限制见交接 | [S044](tasks/S044.md) |
| S045 | 上传会话和固定块接收 | L | S043、S029、S033 | done | 88cc8de、e6c8533、4a88d10… | 历史验收见交接 | 历史限制见交接 | [S045](tasks/S045.md) |
| S046 | 上传 finalize 与固定版本下载 | L | S045 | done | f27565e、5205e73、f0c0f84… | 历史验收见交接 | 历史限制见交接 | [S046](tasks/S046.md) |
| S047 | 操作 HTTP 接口与错误映射 | L | S036、S038、S039、S040、S043、S046 | done | 6cc3384、ee79f3b、aacd1e0 | 历史验收见交接 | 历史限制见交接 | [S047](tasks/S047.md) |
| S048 | 一致性快照、changes 与 SSE | L | S044、S047 | done | f360a9f、5711271 | 历史验收见交接 | 无 | [S048](tasks/S048.md) |
| S049 | 外部目录初始稳定采集 | L | S044、S035 | done | ad1bf63、cfebb06、712881e… | 历史验收见交接 | 历史限制见交接 | [S049](tasks/S049.md) |
| S050 | inotify 与溢出补扫 | L | S049 | done | 7f75ec5、cecaecc、675b511… | 历史验收见交接 | 历史限制见交接 | [S050](tasks/S050.md) |
| S051 | 外部移动、删除与挂载失效 | L | S050、S038、S039 | done | 1b99313、58a90b4、d2c9035… | 历史验收见交接 | 历史限制见交接 | [S051](tasks/S051.md) |
| S052 | 服务端整合小型 HTTP 闭环 | L | S048、S051 | done | 282e5dc、5de59d6、e4e1a4c… | 历史验收见交接 | 历史限制见交接 | [S052](tasks/S052.md) |
| S053 | Rust HTTPS 客户端与认证刷新 | L | S043、S048 | done | 930fbb0、1e6376f、1a6eb95… | 历史验收见交接 | 历史限制见交接 | [S053](tasks/S053.md) |
| S054 | 上传下载续传客户端 | L | S053、S045、S046、S031 | done | 72aeb21、fd61d51、1cf4209… | 历史验收见交接 | 历史限制见交接 | [S054](tasks/S054.md) |
| S055 | agent 核心端口与确定性执行器 | L | S023、S031、S054 | done | 49d492c、2af11b0、1f02d72… | 历史验收见交接 | 历史限制见交接 | [S055](tasks/S055.md) |
| S056 | 本地 generation 与上传快照 | L | S055 | done | 96c4ab0、96bbb43、26f34e6… | 历史验收见交接 | 历史限制见交接 | [S056](tasks/S056.md) |
| S057 | 接收事件与应用进度 | L | S055、S048 | done | 1c4ef4d、41c2d8d、f869eb8… | 历史验收见交接 | 历史限制见交接 | [S057](tasks/S057.md) |
| S058 | 首次绑定与可恢复合并执行 | L | S056、S057、S020 | done | 2f43607、28dba66、a2e893c… | 历史验收见交接 | 历史限制见交接 | [S058](tasks/S058.md) |
| S059 | 本地受控应用与重启核对 | L | S058 | done | 5b826e0、ad3807d、f70947a… | 历史验收见交接 | 历史限制见交接 | [S059](tasks/S059.md) |
| S060 | 旧游标与 epoch 重建 | L | S059 | done | 6a6cc2f、6924348、ac3d662… | 历史验收见交接 | 历史限制见交接 | [S060](tasks/S060.md) |
| S061 | 暂停、模式切换与解除绑定 | L | S059、S018 | done | 520501f、5529270 | 历史验收见交接 | 历史限制见交接 | [S061](tasks/S061.md) |
| S062 | 下载优先级与重试调度 | L | S054、S059 | done | deafbe3、ab44a59、6fabd35… | 历史验收见交接 | 历史限制见交接 | [S062](tasks/S062.md) |
| S063 | 手动释放的核心安全条件 | L | S059、S061 | done | 9cf3d80、82ec864 | 历史验收见交接 | 历史限制见交接 | [S063](tasks/S063.md) |
| S064 | agent 状态、问题与恢复导出 | L | S060、S062、S063 | done | b0bb69a | 历史验收见交接 | 历史限制见交接 | [S064](tasks/S064.md) |
| S065 | Windows 安全文件与本地扫描 | W | S012、S017、S059 | done | 03055ac、1395e15 | 历史验收见交接 | 历史限制见交接 | [S065](tasks/S065.md) |
| S066 | CFAPI 正式注册与元数据更新 | W | S065、S010 | done | 20367d1 | 历史验收见交接 | 历史限制见交接 | [S066](tasks/S066.md) |
| S067 | CFAPI hydration 接入传输执行器 | W | S066、S054、S062 | done | 6e089ff、8a0e921、b9bcbce | 历史验收见交接 | 历史限制见交接 | [S067](tasks/S067.md) |
| S068 | Windows 写入检测与使用保护 | W | S066、S056、S059 | done | f5af3db、d874f54、1847d9f | 历史验收见交接 | 历史限制见交接 | [S068](tasks/S068.md) |
| S069 | Windows 手动释放与恢复 | W | S067、S068、S063 | done | 59985ea、c9b95a0、e542446… | 历史验收见交接 | 历史限制见交接 | [S069](tasks/S069.md) |
| S070 | Windows agent 进程与单实例 | W | S067、S068、S069、S064 | done | e0200e3、f5ce872、97c0735… | 历史验收见交接 | 历史限制见交接 | [S070](tasks/S070.md) |
| S071 | Windows named pipe 传输与身份 | W | S070、S025 | done | 8036466、323469f、e3a2e51… | 历史验收见交接 | 历史限制见交接 | [S071](tasks/S071.md) |
| S072 | 本机管理命令完整接入 | W | S071、S061、S064 | done | ebc76ae、798b36a、866f2fc… | 历史验收见交接 | 历史限制见交接 | [S072](tasks/S072.md) |
| S073 | Windows CLI 最小管理入口 | W | S072 | done | d7742ba、32e716b、ce7863d | 历史验收见交接 | 历史限制见交接 | [S073](tasks/S073.md) |
| S074 | 三台客户端基础互通 | X | S073、S052 | done | 5e5e98f、1452d36、0d1c673… | 历史验收见交接 | 历史限制见交接 | [S074](tasks/S074.md) |
| S075 | Linux 在线管理 socket 与 CLI | L | S043、S044、S047、S048 | done | ace6837、526bde0、1f415b8… | 历史验收见交接 | 历史限制见交接 | [S075](tasks/S075.md) |
| S076 | 前端公共组件与两应用壳 | B | S005、S024、S025 | done | cb82498 | 历史验收见交接 | 历史限制见交接 | [S076](tasks/S076.md) |
| S077 | Web 登录与会话管理 | B | S076、S043 | done | 89c7b2d | 历史验收见交接 | 历史限制见交接 | [S077](tasks/S077.md) |
| S078 | Web Root 与服务健康页面 | B | S077、S044、S051 | done | 5246a05、103a0b1 | 历史验收见交接 | 历史限制见交接 | [S078](tasks/S078.md) |
| S079 | Web 历史、冲突与回收站 | B | S078、S040、S047 | done | 4ef8174 | 历史验收见交接 | 历史限制见交接 | [S079](tasks/S079.md) |
| S080 | Tauri Windows 壳与最小 IPC 桥 | W | S076、S072 | done | 692e18f | 历史验收见交接 | 历史限制见交接 | [S080](tasks/S080.md) |
| S081 | 桌面连接与 Binding 页面 | W | S080 | done | 94dc1af | 历史验收见交接 | 历史限制见交接 | [S081](tasks/S081.md) |
| S082 | 桌面文件状态与手动操作 | W | S081、S069、S073 | done | 713d907 | 历史验收见交接 | 历史限制见交接 | [S082](tasks/S082.md) |
| S083 | 桌面历史与系统通知 | W | S082、S079、S064 | done | ecd05fa、58188c4、7c77b25 | 历史验收见交接 | 历史限制见交接 | [S083](tasks/S083.md) |
| S084 | 客户端未上传数据恢复演练 | W | S083、S064 | done | e1ba9a8 | 历史验收见交接 | 历史限制见交接 | [S084](tasks/S084.md) |
| S085 | 历史保留与对象引用标记 | L | S040、S029 | done | ae6a8c7 | 历史验收见交接 | 历史限制见交接 | [S085](tasks/S085.md) |
| S086 | GC 删除协调与临时清理 | L | S085、S037 | done | 7c79ae1、accc684 | 历史验收见交接 | 历史限制见交接 | [S086](tasks/S086.md) |
| S087 | 一致性备份快照与租约 | L | S086、S048 | done | 813cf39、84c7512 | 历史验收见交接 | 历史限制见交接 | [S087](tasks/S087.md) |
| S088 | 备份导出、校验与释放接口 | L | S087、S075 | done | 99cc17a、0bc6846、f1302ba | 历史验收见交接 | 历史限制见交接 | [S088](tasks/S088.md) |
| S089 | 离线恢复到空目录 | L | S088、S037 | done | 59663a5、fae55cc、51eecbb… | 历史验收见交接 | 历史限制见交接 | [S089](tasks/S089.md) |
| S090 | Web 备份管理入口 | B | S079、S088 | done | e73a5f6 | 历史验收见交接 | 历史限制见交接 | [S090](tasks/S090.md) |
| S091 | 协议与数据库升级演练 | X | S089、S074 | done | 2eedab8、cf987fc、515b347 | 历史验收见交接 | 历史限制见交接 | [S091](tasks/S091.md) |
| S092 | 构建依赖与协议边界检查 | L | S024、S025、S080 | done | db95ba8 | 历史验收见交接 | 历史限制见交接 | [S092](tasks/S092.md) |
| S093 | 发布版本与产物清单 | L | S052、S076、S092 | done | d59a7b2、7993e72、fbc0d27 | 历史验收见交接 | 历史限制见交接 | [S093](tasks/S093.md) |
| S094 | Linux 服务端 Nix 构建包 | L | S093、S090 | done | 83d0098、90a3f21、3b1fbcc | 历史验收见交接 | 历史限制见交接 | [S094](tasks/S094.md) |
| S095 | NixOS 服务模块 | L | S094、S007 | done | a70f755、9243d14、eeb8f66 | 历史验收见交接 | 历史限制见交接 | [S095](tasks/S095.md) |
| S096 | 在本机启用隔离服务实例 | L | S095、S042 | done | 99a7ed4 | 历史验收见交接 | 历史限制见交接 | [S096](tasks/S096.md) |
| S097 | HTTPS 与同源 Web 部署 | L | S096、S077 | done | d90a263、b615fd9、b345ac9… | 历史验收见交接 | 历史限制见交接 | [S097](tasks/S097.md) |
| S098 | 通用 Linux 包与容器配置 | L | S094、S097 | done | 048aff1、62115d6、a14848f… | 历史验收见交接 | 历史限制见交接 | [S098](tasks/S098.md) |
| S099 | Windows 安装包与后台启动 | W | S083、S084、S093 | done | ca510ab、cf1d174 | 历史验收见交接 | 历史限制见交接 | [S099](tasks/S099.md) |
| S100 | Windows 升级与卸载保护 | W | S099、S091 | done | 3a2e2aa、d0392f4 | 历史验收见交接 | 历史限制见交接 | [S100](tasks/S100.md) |
| S101 | CI 平台矩阵与安装验证 | X | S092、S098、S100 | blocked | 产品源fcbbb93；任务a6bbb56；master合并f07aab2，任务树/分支已清理 | 本机同源Linux双架构与Windows安装/CFAPI/e2e完成；portable914/0、Linux633/0，CFAPI5/5；NixOS与Ubuntu分列，ARM为全系统TCG | 仅hosted：无remote/runner，未执行Actions；需配置远端、Windows编译及独占platform-lab runner后运行工作流 | agent/tasks/S101.md（+a/b）；artifacts/S101/public/acceptance-matrix.json |
| S102 | 事件异常与旧设备集成验收 | X | S074、S089、S091 | done | 1a51f04、81699c4、60200a7 | 历史验收见交接 | 历史限制见交接 | [S102](tasks/S102.md) |
| S103 | 文件与挂载故障集成验收 | X | S074、S051、S068 | done | 8e4ad2b、81dcb16、b36e31e | 历史验收见交接 | 历史限制见交接 | [S103](tasks/S103.md) |
| S104 | GC、备份与崩溃联合验收 | X | S086、S089、S102 | done | c03114d、71013d9、326b469 | 历史验收见交接 | 历史限制见交接 | [S104](tasks/S104.md) |
| S105 | 安全与操作可达性验收 | X | S083、S090、S097、S100 | done | b26372e、8cd3a8a、81ceb6d… | 历史验收见交接 | 历史限制见交接 | [S105](tasks/S105.md) |
| S106 | 小文件时延与常规规模基准 | X | S101、S102、S103、S104、S105 | blocked | 基线c59ff19；独立只读前置/资源报告 `docs/reports/performance.md`，文档390cda0已cb9ed20合入，证据/树/分支已收尾 | devShell资源核查：双路6430/128线程、503.54GiB RAM、SSD可用143.05GiB、旋转btrfs21.58TiB；仅1台8GiB/8vCPU Windows，未联系客体、未跑小样本/常规档，p95/RSS/CPU/队列全未测 | S101 hosted无remote/runner/Actions证据；S105已fd896fa完成，hosted前置仍阻塞；需准备3独立Windows及SSD/RTT实证，容量本身未证不足，不降1万文件/约10GiB目标 | agent/tasks/S106.md；artifacts/S106 |
| S107 | 正式容量与大文件续传验证 | X | S106 | blocked | 基线cb9ed20；只读正式档预算与恢复证据要求写 `docs/reports/performance.md`，文档dcadee5已0d4fb30合入，证据/树/分支已收尾 | devShell容量/挂载/btrfs/remote均exit0；data21.5808TiB，SSD142.59GiB；有限情景含余量19.54TiB，另累计五端下载stage情景25.54TiB（差约3.96TiB）；正式10万文件/1TiB/50GiB/5设备与重传字节全未测 | S106未验收，追溯S101 hosted缺口；五独立Windows及大NTFS卷未准备，须先核实内部副本/stage/历史增长再准入容量，不降目标、不以小样本替代 | agent/tasks/S107.md；artifacts/S107 |
| S108 | 发布清单与最终交接 | X | S101、S102、S103、S104、S105、S106、S107 | blocked | 35项P0索引、README/运维入口与当前限制；文档5f485a5已abdedd3整合；S105 done/代码候选fd896fa；证据、树与分支已收尾 | 两架构各24个历史runtime文件及发布/源码tar哈希核验，b263源三PE哈希/原生启动验证；当前候选全平台包未完成 | S101 hosted、S106/S107性能容量、T02三台Windows及最终同源发行证据缺失；未宣称首发完成 | agent/tasks/S108.md；docs/operations/release.md |
| S109 | Windows 免安装 ZIP（状态留本机） | W | S099、S100 | done | 367faf5、3512865 | 历史验收见交接 | 历史限制见交接 | [S109](tasks/S109.md) |
| S110 | 一键构建多平台 release 归档 | L | S093、S098、S109 | done | d3209ab、81340f6、1c61929；master已ff整合，任务树/分支已清理 | 三目标同源打包/15项SHA；Linux各24文件、ZIP11条目/5PE；契约15/15+xtask23/23，clippy/fmt通过 | 无；新包原生验收未执行，原发布门禁不变 | [S110](tasks/S110.md) |
| S111 | 移除 Linux ARM64 构建与验证范围 | X | S110 | done | 实现9e62b03、交接ecb21dc；master已ff整合，任务树/分支已清理 | release9、ZIP8、CI8、Linux74、xtask23；clippy/fmt/边界/消费者/Nix求值/server-admin check通过 | 无；原hosted及首发原生/性能门禁保留 | [S111](tasks/S111.md) |
| S112 | 本机域名部署与NixOS功能选项 | L | S095、S096、S097、S111 | done | 项目4856738、c94aca4已ff合入master；主机9c2133f、56bd93b已合main，任务树均清理 | 包内65/0；主机build/test/switch244、可信TLS、16MiB/413、committed/下载/重启/备份、Chromium登录/SSE与既有服务检查通过 | 无部署阻塞；独立外网可达性未获证据，原hosted/规模/首发门禁保留 | [S112](tasks/S112.md) |
| S113 | 主机用户运行与 pure 一键部署 | L | S112 | done | 项目4071fd4/6b967bd，主机32bfeb5已ff合main；整合/清理见批次 | 契约6/6、两类模块、包内65/0；pure构建、迁移test、HTTPS上传/重启/备份；一键pin更新/test/switch245、归属/身份/版本/SSH/10服务/锁核对通过 | 无；原hosted/规模/首发门禁保留 | [S113](tasks/S113.md) |
