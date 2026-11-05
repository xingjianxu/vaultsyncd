# 实施进度

当前阶段：S006 已完成，`tools/xtask`（`vaultsyncd-xtask`，二进制 `xtask`）提供 `env`/`check` 编排与显式未实现的 `generate`/`package`，根 `.cargo/config.toml` 提供 `cargo xtask` 别名，`.github/workflows/checks.yml` 接入 Linux 便携检查（Rust fmt/clippy/test + 前端 frozen-lockfile 安装与 typecheck，入口为单步 `cargo xtask check`）；工程实施继续到 S007。
最早 ready：**S007**（独立 btrfs 测试位置，前置 S001+S003 均已满足）；S008 依赖 S007，Windows 卡依赖 S008/S009。
平台资源核验结论（S001，2026-11-05）：开发/服务验证主机为 NixOS 26.11 x86_64、Nix 2.34.9；`/`、`/home`、`/nix`、`/var/log`、`/wrd`、`/data` 均为本地 btrfs；`/wrd` 可用 197G、`/data` 可用 22T；KVM 可用且用户在 `kvm` 组，但 QEMU/Quickemu 未安装、无现成 Windows 客体；尚无本项目独立 btrfs 测试卷。Ubuntu 24.04、Linux ARM64、Windows 原生 CFAPI、reflink 与验收规模均未核验。证据见 `docs/environment.md`。

状态图例：`todo` 未开始；`in_progress` 执行中；`blocked` 缺资源/前置；`done` 验收完成。下表共 108 行（S001—S108），编号、结果、环境、前置取自实施手册 §4 任务索引；产出/验证/阻塞列随执行更新。

| 编号 | 结果 | 环境 | 前置 | 状态 | 产出提交/工作树 | 验证摘要 | 阻塞 | 交接路径 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| S001 | 确认主机与同步环境基线 | L | 无 | done | 本卡单次本地提交（父提交 4a2c7e9；worktree `/wrd/prj/vaultsyncd/.worktrees/s001-s007`，分支 `chore/S001-S007-bootstrap`） | 只读复核 pwd/Git/NixOS/Nix/架构/挂载/空间/配置入口/端口/btrfs/KVM/权限；见 docs/environment.md | 无 | agent/tasks/S001.md |
| S002 | 建立 agent 入口与上下文提取工具 | L | S001 | done | 本卡单次本地提交（父提交 db7ce53；worktree `/wrd/prj/vaultsyncd/.worktrees/s001-s007`，分支 `chore/S001-S007-bootstrap`） | `python3 tools/context.py card S001` 与 `design 11.2` 边界人工核对；未知/歧义/不存在编号退出 2 并列候选；围栏 fixture 验证；`git diff --check` 清洁 | 无 | agent/tasks/S002.md |
| S003 | 建立 Nix 开发 shell | L | S001、S002 | done | 本卡单次本地提交（父提交 cf993b1；worktree `/wrd/prj/vaultsyncd/.worktrees/s001-s007`，分支 `chore/S001-S007-bootstrap`） | `nix flake lock` 锁定 nixpkgs rev 1d060086（2026-11-03）；devShell 内输出版本全部退出码 0；临时 Rust 编译运行、cargo test/fmt/clippy（含负控）、C+SQLite 链接、pnpm 全部通过；`env -i` 新终端复现一致；`nix flake check` 通过 | 无 | agent/tasks/S003.md |
| S004 | 建立最小 Cargo workspace | L | S003 | done | 本卡单次本地提交（父提交 8165ee6；worktree `/wrd/prj/vaultsyncd/.worktrees/s001-s007`，分支 `chore/S001-S007-bootstrap`） | 虚拟 workspace 仅含 `vaultsyncd-domain`；`cargo metadata/check/fmt/clippy/test` 全部退出 0（含 fmt、`unsafe_code`、clippy::all 三项负控）；唯一 `Cargo.lock`；edition 2024 + resolver 3 + rust-version 1.85.0 | 无 | agent/tasks/S004.md |
| S005 | 建立 pnpm workspace 与工具版本 | L | S003 | done | 本卡单次本地提交（父提交 e249897；worktree `/wrd/prj/vaultsyncd/.worktrees/s001-s007`，分支 `chore/S001-S007-bootstrap`） | 根 private `package.json`（packageManager/engines 锁 pnpm 12.9.0、node 24.21.0）；`pnpm-workspace.yaml` 只含 `packages/ui`；`packages/ui` 导出 `StatusBadge` 与类型、`tsc --noEmit` 通过；`pnpm install`(0)、`--frozen-lockfile`(0)、filter/根 `typecheck`(0)、类型错误负控(1→复原 0)；唯一 `pnpm-lock.yaml`，无嵌套锁 | 无 | agent/tasks/S005.md |
| S006 | 建立任务编排入口与基础 CI | L | S004、S005 | done | 本卡单次本地提交（父提交 c7df404；worktree `/wrd/prj/vaultsyncd/.worktrees/s001-s007`，分支 `chore/S001-S007-bootstrap`） | `tools/xtask`（无第三方依赖，members 非 default-members）+ `.cargo/config.toml` 别名；`env`/`check` 退出 0（check 实跑 fmt/clippy/test/typecheck 四步）；`generate`/`package` 退出 3、未知子命令`check bogus`/无子命令退出 2；负控 fmt 与 TS 类型错误均退出 1 并定位阶段，`cmp` 复原后回 0；`cargo check/metadata --locked` 0 且 default-members 仅 domain；actionlint 1.7.12 校验 `checks.yml` 通过（无远端，未真实运行 CI） | 无（CI 未在远端执行，见交接） | agent/tasks/S006.md |
| S007 | 准备独立 btrfs 测试位置 | L | S001、S003 | todo | — | 未执行 | — | agent/tasks/S007.md（执行时创建） |
| S008 | 准备 Windows 客体与连接说明 | L | S001、S007 | todo | — | 未执行 | — | agent/tasks/S008.md（执行时创建） |
| S009 | 固定 Windows 构建工具链 | W | S008、S004、S005 | todo | — | 未执行 | — | agent/tasks/S009.md（执行时创建） |
| S010 | CFAPI 注册与占位创建原型 | W | S009 | todo | — | 未执行 | — | agent/tasks/S010.md（执行时创建） |
| S011 | CFAPI 读取、取消与离线原型 | W | S010 | todo | — | 未执行 | — | agent/tasks/S011.md（执行时创建） |
| S012 | CFAPI 释放与写入保护原型 | W | S011 | todo | — | 未执行 | — | agent/tasks/S012.md（执行时创建） |
| S013 | Linux 存储准入原型 | L | S007、S004 | todo | — | 未执行 | — | agent/tasks/S013.md（执行时创建） |
| S014 | Linux 普通文件发布原型 | L | S013 | todo | — | 未执行 | — | agent/tasks/S014.md（执行时创建） |
| S015 | 定义身份与有界计数类型 | L | S004 | todo | — | 未执行 | — | agent/tasks/S015.md（执行时创建） |
| S016 | 定义状态维度与错误分类 | L | S015 | todo | — | 未执行 | — | agent/tasks/S016.md（执行时创建） |
| S017 | 路径与名称纯规则 | L | S015 | todo | — | 未执行 | — | agent/tasks/S017.md（执行时创建） |
| S018 | 排除规则与解释 | L | S017 | todo | — | 未执行 | — | agent/tasks/S018.md（执行时创建） |
| S019 | 三方规划基本分支 | L | S015、S016、S017 | todo | — | 未执行 | — | agent/tasks/S019.md（执行时创建） |
| S020 | 首次合并规划 | L | S019 | todo | — | 未执行 | — | agent/tasks/S020.md（执行时创建） |
| S021 | 内容冲突及稳定副本身份 | L | S019 | todo | — | 未执行 | — | agent/tasks/S021.md（执行时创建） |
| S022 | 移动与删除冲突规划 | L | S020、S021 | todo | — | 未执行 | — | agent/tasks/S022.md（执行时创建） |
| S023 | 规划器操作重排验证 | L | S022 | todo | — | 未执行 | — | agent/tasks/S023.md（执行时创建） |
| S024 | 远端协议最小契约与生成通路 | L | S015、S006 | todo | — | 未执行 | — | agent/tasks/S024.md（执行时创建） |
| S025 | 本机 IPC 最小契约与生成通路 | L | S016、S024 | todo | — | 未执行 | — | agent/tasks/S025.md（执行时创建） |
| S026 | 服务端数据库迁移框架 | L | S015、S007 | todo | — | 未执行 | — | agent/tasks/S026.md（执行时创建） |
| S027 | 服务端命名空间表与约束 | L | S026 | todo | — | 未执行 | — | agent/tasks/S027.md（执行时创建） |
| S028 | 版本、事件与幂等表 | L | S027 | todo | — | 未执行 | — | agent/tasks/S028.md（执行时创建） |
| S029 | 上传、意图与租约表 | L | S028 | todo | — | 未执行 | — | agent/tasks/S029.md（执行时创建） |
| S030 | 客户端数据库与状态表 | L | S015、S016、S026 | todo | — | 未执行 | — | agent/tasks/S030.md（执行时创建） |
| S031 | 客户端事件、传输与意图持久化 | L | S030 | todo | — | 未执行 | — | agent/tasks/S031.md（执行时创建） |
| S032 | Linux 安全路径访问正式接口 | L | S014、S017 | todo | — | 未执行 | — | agent/tasks/S032.md（执行时创建） |
| S033 | 不可变对象写入与读取 | L | S029、S032 | todo | — | 未执行 | — | agent/tasks/S033.md（执行时创建） |
| S034 | 持久提交意图状态机 | L | S033 | todo | — | 未执行 | — | agent/tasks/S034.md（执行时创建） |
| S035 | 内容 CAS 与现场复核 | L | S034、S028 | todo | — | 未执行 | — | agent/tasks/S035.md（执行时创建） |
| S036 | 幂等重试与冲突内容提交 | L | S035、S021 | todo | — | 未执行 | — | agent/tasks/S036.md（执行时创建） |
| S037 | 启动恢复与崩溃注入 | L | S036 | todo | — | 未执行 | — | agent/tasks/S037.md（执行时创建） |
| S038 | 目录创建与同 Root 移动 | L | S037、S022 | todo | — | 未执行 | — | agent/tasks/S038.md（执行时创建） |
| S039 | 可恢复删除与回收站 | L | S038 | todo | — | 未执行 | — | agent/tasks/S039.md（执行时创建） |
| S040 | 历史与回收站恢复操作 | L | S039 | todo | — | 未执行 | — | agent/tasks/S040.md（执行时创建） |
| S041 | 服务端进程与配置生命周期 | L | S037、S024 | todo | — | 未执行 | — | agent/tasks/S041.md（执行时创建） |
| S042 | 首次 owner 初始化与密码认证 | L | S041、S026 | todo | — | 未执行 | — | agent/tasks/S042.md（执行时创建） |
| S043 | 会话、刷新与 Root 授权 | L | S042 | todo | — | 未执行 | — | agent/tasks/S043.md（执行时创建） |
| S044 | 管理 Root 注册与健康 API | L | S043、S032、S027 | todo | — | 未执行 | — | agent/tasks/S044.md（执行时创建） |
| S045 | 上传会话和固定块接收 | L | S043、S029、S033 | todo | — | 未执行 | — | agent/tasks/S045.md（执行时创建） |
| S046 | 上传 finalize 与固定版本下载 | L | S045 | todo | — | 未执行 | — | agent/tasks/S046.md（执行时创建） |
| S047 | 操作 HTTP 接口与错误映射 | L | S036、S038、S039、S040、S043、S046 | todo | — | 未执行 | — | agent/tasks/S047.md（执行时创建） |
| S048 | 一致性快照、changes 与 SSE | L | S044、S047 | todo | — | 未执行 | — | agent/tasks/S048.md（执行时创建） |
| S049 | 外部目录初始稳定采集 | L | S044、S035 | todo | — | 未执行 | — | agent/tasks/S049.md（执行时创建） |
| S050 | inotify 与溢出补扫 | L | S049 | todo | — | 未执行 | — | agent/tasks/S050.md（执行时创建） |
| S051 | 外部移动、删除与挂载失效 | L | S050、S038、S039 | todo | — | 未执行 | — | agent/tasks/S051.md（执行时创建） |
| S052 | 服务端整合小型 HTTP 闭环 | L | S048、S051 | todo | — | 未执行 | — | agent/tasks/S052.md（执行时创建） |
| S053 | Rust HTTPS 客户端与认证刷新 | L | S043、S048 | todo | — | 未执行 | — | agent/tasks/S053.md（执行时创建） |
| S054 | 上传下载续传客户端 | L | S053、S045、S046、S031 | todo | — | 未执行 | — | agent/tasks/S054.md（执行时创建） |
| S055 | agent 核心端口与确定性执行器 | L | S023、S031、S054 | todo | — | 未执行 | — | agent/tasks/S055.md（执行时创建） |
| S056 | 本地 generation 与上传快照 | L | S055 | todo | — | 未执行 | — | agent/tasks/S056.md（执行时创建） |
| S057 | 接收事件与应用进度 | L | S055、S048 | todo | — | 未执行 | — | agent/tasks/S057.md（执行时创建） |
| S058 | 首次绑定与可恢复合并执行 | L | S056、S057、S020 | todo | — | 未执行 | — | agent/tasks/S058.md（执行时创建） |
| S059 | 本地受控应用与重启核对 | L | S058 | todo | — | 未执行 | — | agent/tasks/S059.md（执行时创建） |
| S060 | 旧游标与 epoch 重建 | L | S059 | todo | — | 未执行 | — | agent/tasks/S060.md（执行时创建） |
| S061 | 暂停、模式切换与解除绑定 | L | S059、S018 | todo | — | 未执行 | — | agent/tasks/S061.md（执行时创建） |
| S062 | 下载优先级与重试调度 | L | S054、S059 | todo | — | 未执行 | — | agent/tasks/S062.md（执行时创建） |
| S063 | 手动释放的核心安全条件 | L | S059、S061 | todo | — | 未执行 | — | agent/tasks/S063.md（执行时创建） |
| S064 | agent 状态、问题与恢复导出 | L | S060、S062、S063 | todo | — | 未执行 | — | agent/tasks/S064.md（执行时创建） |
| S065 | Windows 安全文件与本地扫描 | W | S012、S017、S059 | todo | — | 未执行 | — | agent/tasks/S065.md（执行时创建） |
| S066 | CFAPI 正式注册与元数据更新 | W | S065、S010 | todo | — | 未执行 | — | agent/tasks/S066.md（执行时创建） |
| S067 | CFAPI hydration 接入传输执行器 | W | S066、S054、S062 | todo | — | 未执行 | — | agent/tasks/S067.md（执行时创建） |
| S068 | Windows 写入检测与使用保护 | W | S066、S056、S059 | todo | — | 未执行 | — | agent/tasks/S068.md（执行时创建） |
| S069 | Windows 手动释放与恢复 | W | S067、S068、S063 | todo | — | 未执行 | — | agent/tasks/S069.md（执行时创建） |
| S070 | Windows agent 进程与单实例 | W | S067、S068、S069、S064 | todo | — | 未执行 | — | agent/tasks/S070.md（执行时创建） |
| S071 | Windows named pipe 传输与身份 | W | S070、S025 | todo | — | 未执行 | — | agent/tasks/S071.md（执行时创建） |
| S072 | 本机管理命令完整接入 | W | S071、S061、S064 | todo | — | 未执行 | — | agent/tasks/S072.md（执行时创建） |
| S073 | Windows CLI 最小管理入口 | W | S072 | todo | — | 未执行 | — | agent/tasks/S073.md（执行时创建） |
| S074 | 三台客户端基础互通 | X | S073、S052 | todo | — | 未执行 | — | agent/tasks/S074.md（执行时创建） |
| S075 | Linux 在线管理 socket 与 CLI | L | S043、S044、S047、S048 | todo | — | 未执行 | — | agent/tasks/S075.md（执行时创建） |
| S076 | 前端公共组件与两应用壳 | B | S005、S024、S025 | todo | — | 未执行 | — | agent/tasks/S076.md（执行时创建） |
| S077 | Web 登录与会话管理 | B | S076、S043 | todo | — | 未执行 | — | agent/tasks/S077.md（执行时创建） |
| S078 | Web Root 与服务健康页面 | B | S077、S044、S051 | todo | — | 未执行 | — | agent/tasks/S078.md（执行时创建） |
| S079 | Web 历史、冲突与回收站 | B | S078、S040、S047 | todo | — | 未执行 | — | agent/tasks/S079.md（执行时创建） |
| S080 | Tauri Windows 壳与最小 IPC 桥 | W | S076、S072 | todo | — | 未执行 | — | agent/tasks/S080.md（执行时创建） |
| S081 | 桌面连接与 Binding 页面 | W | S080 | todo | — | 未执行 | — | agent/tasks/S081.md（执行时创建） |
| S082 | 桌面文件状态与手动操作 | W | S081、S069、S073 | todo | — | 未执行 | — | agent/tasks/S082.md（执行时创建） |
| S083 | 桌面历史与系统通知 | W | S082、S079、S064 | todo | — | 未执行 | — | agent/tasks/S083.md（执行时创建） |
| S084 | 客户端未上传数据恢复演练 | W | S083、S064 | todo | — | 未执行 | — | agent/tasks/S084.md（执行时创建） |
| S085 | 历史保留与对象引用标记 | L | S040、S029 | todo | — | 未执行 | — | agent/tasks/S085.md（执行时创建） |
| S086 | GC 删除协调与临时清理 | L | S085、S037 | todo | — | 未执行 | — | agent/tasks/S086.md（执行时创建） |
| S087 | 一致性备份快照与租约 | L | S086、S048 | todo | — | 未执行 | — | agent/tasks/S087.md（执行时创建） |
| S088 | 备份导出、校验与释放接口 | L | S087、S075 | todo | — | 未执行 | — | agent/tasks/S088.md（执行时创建） |
| S089 | 离线恢复到空目录 | L | S088、S037 | todo | — | 未执行 | — | agent/tasks/S089.md（执行时创建） |
| S090 | Web 备份管理入口 | B | S079、S088 | todo | — | 未执行 | — | agent/tasks/S090.md（执行时创建） |
| S091 | 协议与数据库升级演练 | X | S089、S074 | todo | — | 未执行 | — | agent/tasks/S091.md（执行时创建） |
| S092 | 构建依赖与协议边界检查 | L | S024、S025、S080 | todo | — | 未执行 | — | agent/tasks/S092.md（执行时创建） |
| S093 | 发布版本与产物清单 | L | S052、S076、S092 | todo | — | 未执行 | — | agent/tasks/S093.md（执行时创建） |
| S094 | Linux 服务端 Nix 构建包 | L | S093、S090 | todo | — | 未执行 | — | agent/tasks/S094.md（执行时创建） |
| S095 | NixOS 服务模块 | L | S094、S007 | todo | — | 未执行 | — | agent/tasks/S095.md（执行时创建） |
| S096 | 在本机启用隔离服务实例 | L | S095、S042 | todo | — | 未执行 | — | agent/tasks/S096.md（执行时创建） |
| S097 | HTTPS 与同源 Web 部署 | L | S096、S077 | todo | — | 未执行 | — | agent/tasks/S097.md（执行时创建） |
| S098 | 通用 Linux 包与容器配置 | L | S094、S097 | todo | — | 未执行 | — | agent/tasks/S098.md（执行时创建） |
| S099 | Windows 安装包与后台启动 | W | S083、S084、S093 | todo | — | 未执行 | — | agent/tasks/S099.md（执行时创建） |
| S100 | Windows 升级与卸载保护 | W | S099、S091 | todo | — | 未执行 | — | agent/tasks/S100.md（执行时创建） |
| S101 | CI 平台矩阵与安装验证 | X | S092、S098、S100 | todo | — | 未执行 | — | agent/tasks/S101.md（执行时创建） |
| S102 | 事件异常与旧设备集成验收 | X | S074、S089、S091 | todo | — | 未执行 | — | agent/tasks/S102.md（执行时创建） |
| S103 | 文件与挂载故障集成验收 | X | S074、S051、S068 | todo | — | 未执行 | — | agent/tasks/S103.md（执行时创建） |
| S104 | GC、备份与崩溃联合验收 | X | S086、S089、S102 | todo | — | 未执行 | — | agent/tasks/S104.md（执行时创建） |
| S105 | 安全与操作可达性验收 | X | S083、S090、S097、S100 | todo | — | 未执行 | — | agent/tasks/S105.md（执行时创建） |
| S106 | 小文件时延与常规规模基准 | X | S101、S102、S103、S104、S105 | todo | — | 未执行 | — | agent/tasks/S106.md（执行时创建） |
| S107 | 正式容量与大文件续传验证 | X | S106 | todo | — | 未执行 | — | agent/tasks/S107.md（执行时创建） |
| S108 | 发布清单与最终交接 | X | S101、S102、S103、S104、S105、S106、S107 | todo | — | 未执行 | — | agent/tasks/S108.md（执行时创建） |
