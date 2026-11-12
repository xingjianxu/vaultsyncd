# vaultsyncd 逐步实施手册

版本：1.5 · 日期：2026-11-07 · 状态：执行计划；实际完成情况与当前阻塞见 `agent/progress.md`。

## 1. 文件放置与使用方式

用户在已有的 `vaultsyncd/` 项目目录中预置三份文件。下列路径及全文路径均相对于仓库根目录，不相对于当前 Markdown 文件：

| 仓库内路径 | 内容 |
| --- | --- |
| `AGENTS.md` | agent 的全局入口与提交约定 |
| `docs/design.md` | 已确认产品和技术设计基线 |
| `agent/implementation-plan.md` | 本文件，原计划 S001—S108，用户新增卡从 S109 续号 |
| `agent/progress.md` | S001 创建的状态表 |
| `agent/tasks/Sxxx.md` | 执行对应卡时创建的交接记录 |
| `agent/context/` | 按需创建的简短索引，不预建重复规格 |

设计、架构、ADR、环境与操作资料归 `docs/`；执行计划和会话交接归 `agent/`。同一规则只在一处维护；上下文摘要注明来源小节和对应提交，源变化后更新或删除过时摘要。S001 仅初始化进度，不一次创建 108 个空交接文件。现有项目如有旧路径文件，应迁移并更新引用，不保留两份可编辑副本。

本文细化 design 的 DEV-001—014；C/I/T 编号仍为唯一需求、不变量和验收编号。Git 记录版本，业务语义以 design 指定小节为准。

当前环境由用户明确为：开发主机是 NixOS，开发端和服务端位于同一服务器；执行 agent 已获为本项目配置环境和执行系统 rebuild 的授权。本文不会假定本次写文档的沙盒就是那台主机；所有机器事实在 S001 实测。

**本次新增的环境决定：NixOS 为主要开发与本机服务验证环境。** design v1.5 已纳入这一点；S001 核对 C35/平台矩阵和构建部署章节；Ubuntu 24.04 LTS 保留为通用 Linux 发布兼容测试环境，Linux x86_64 与 Windows 11 x86_64 的首发范围仍需验收。开发机的发行版不会改变服务端 btrfs 存储准入条件。

首发含 Linux 服务端、管理 CLI、基础 Web，以及 Windows agent、CLI、Tauri 桌面和 CFAPI。Linux 客户端/FUSE、macOS、Android、P2P、跨版本增量均不在本文执行任务内。

## 2. 给每个 agent 的工作约定

### 2.1 一次只执行一张任务卡

默认一个新会话、一个 agent、一张 Sxxx 卡。按照编号选择最早的 ready 任务；前置未完成则不能实现依赖它的行为。状态包括 `todo / in_progress / blocked / done`。`blocked` 写清缺失条件，不伪造成功；可继续最早的无依赖任务。例如没有 Windows 环境时，S010—S012 阻塞，但可推进 Linux、纯核心和前端任务。

每张卡限定一个主要结果。通常修改一个主要模块和直接相关测试；新增跨模块接口先写出输入、输出、错误和不变量，再实现。若预计超过约 6 个手写源码文件、涉及第二个独立状态机，或上下文超过预算，将卡拆为 Sxxx-a、Sxxx-b 等，写到 `agent/tasks/Sxxx-*.md`。拆卡只细化本卡范围，不减少验收；父卡需所有子卡完成才标 done。测试、生成代码和进度文件不计入这个文件数建议。

所有最终包名与目录按 design §5；首次引入包才创建 Cargo.toml/package.json，并加入对应根 workspace。临时接口尚未实现时返回明确 Unsupported/NotReady，不以空数组、默认成功或丢弃参数使测试通过。

### 2.2 上下文预算

每轮只读取：根 `AGENTS.md`（若已建立）、本手册 §1—2、本张卡、design 指定章节、直接前置步骤的短交接以及本轮真正涉及的源文件。不要读取全文任务卡、所有历史日志或整个仓库。

建议首次输入控制在 6,000—10,000 tokens，业务代码按需分段读；长任务在约 70% 上下文使用量前保存检查点。直接依赖的交接总量尽量不超过 1,000 中文字，每张最终交接建议 300—600 中文字。Token 为预算建议，以实际模型限制为准。

若章节过长，只读相关小节和不变量。所有 agent 都需知道：不丢未确认本地内容；冲突保留双方；首次同步不推导删除；挂载/权限/扫描故障不等于删除；完整缓存不自动驱逐；上传完成不等于 committed；SSE 只作提示；服务端不使用 FUSE/占位存储。

初次可用 `rg -n '^#{1,4} ' docs/design.md` 定位标题，再读取必要行段。S002 建立 `tools/context.py`，按标题精确提取本卡和设计小节，输出给下一次会话；避免依赖会变化的固定行号。

### 2.3 每步统一完成条件

1. 实现本卡结果与必要错误分支，保持平台/依赖边界。
2. 执行本卡最小验证，记录命令、环境、退出码及实际观察；已有相关测试必须通过。文档/脚手架任务只需结构与命令检查，不增加无意义单测。
3. 记录 `agent/tasks/Sxxx.md`：状态、关联需求/测试、接口决定、改动路径、验证证据、限制、下一步；涉及代码时同时记录 §2.7 的 worktree 续接信息。大日志放 `artifacts/`，交接只保留摘要和可重现命令。
4. 更新 `agent/progress.md` 的对应行和当前可用入口；新接口摘要写到 `docs/architecture/interfaces.md` 对应小节。不得每次追加整份设计副本。
5. 检查 diff，确保无真实密码、用户文件、数据库或测试卷进入 Git。按根 AGENTS.md 的 Git 规则，使用已安装且适用的 commit skill 做单独本地提交，summary 与 body 使用中文；身份或 skill 缺失则记录提交阻塞，不虚构作者、不自行绕过、不推送远端。

`done` 意味着卡中要求的验证已完成。Windows 未实测、资源不足、CI runner 不存在等情况必须标 blocked，不能用 Linux fake 或交叉编译替代。每轮回复只说明结果、验证、剩余阻塞和下一个 ready 编号。

### 2.4 环境、状态与数据隔离

| 用途 | 约定 |
| --- | --- |
| 项目源码 | 用户创建的 `vaultsyncd/`；实际绝对路径在 S001 记录，代码开发使用 §2.7 的任务 worktree |
| 临时开发状态 | `/data/vaultsyncd-dev/vaultsyncd-temp/Sxxx-<run_id>/`，独立账号/端口/配置 |
| 集成与故障测试 | `/data/vaultsyncd-dev/vaultsyncd-temp/volumes/runs/<run_id>/`；每轮独立 DB、Vault、对象与进程 |
| 正式试用实例 | 示例 `/var/lib/vaultsyncd/`，只在部署阶段启用，先用可丢弃样本 |
| 用户工作目录 | 从允许基目录显式选择，与所有内部状态目录互不重叠 |
| Windows 样本目录 | Windows 客体的独立 NTFS 目录，不使用共享文件夹模拟 NTFS |

所有位置为示例，agent 先检查占用、权限和实际挂载。源码可以在主机其他文件系统上；**服务端工作树、状态、对象、暂存必须符合 btrfs 约束**。按 design §11.5 核对 CoW、卷与子卷身份；ext4/XFS/ZFS/overlay 均不接受。子卷共享底层容量，满盘故障必须使用独立测试文件系统。没有合规测试卷时，可在空间充足且已确认属于本任务的普通文件上建立小型 btrfs loop 测试卷；不得格式化既有块设备，不把测试镜像当生产布局。

故障注入只能作用于带 run_id 的测试实例。满盘实验填充小型专用测试卷，强杀用该测试进程的已记录 PID，卸载只操作本任务挂载点。不得用主机根分区满盘或随意停止全部同名进程来测试。压力测试从小数据开始，先估算工作文件、不可变对象、历史、临时内容和余量的总空间。

### 2.5 NixOS 的工程规则

- 项目依赖由根 `flake.nix`、`flake.lock` 和 `nix develop` 管理。先适配主机现有 Nix/flake 配置，不为了项目重写整套系统配置；若 features 未启用，按实际版本配置最小所需选项。
- 项目 flake 导出 devShell、后续 packages 和 NixOS module；主机自己的 flake/configuration.nix 仍拥有主机配置。不得把项目 flake 当成主机 flake 执行 rebuild，除非已明确配置了对应 `nixosConfigurations`。
- 需要虚拟化、测试挂载或服务部署时，可以在已获授权范围内 rebuild，无需再次为普通 rebuild 请求许可。先保存配置 diff 和旧 generation，按实际入口执行 build、test、健康检查，再 switch。`test` 会激活配置，但不设置下次启动默认项；其本身不提供定时自动回滚。
- 远程操作需保留 SSH 与回滚能力；不得顺手升级全系统、关闭防火墙或清理回滚 generation。失败先恢复已知配置/旧 generation。具体主机入口、选项和命令必须来自现场检查，不猜 hostname 或 flake 路径。
- 不把密码写入 Nix 表达式或 Nix store；使用有权限控制的运行时文件、凭据输入或经验证的 systemd credentials。
- 首发只做 Windows 桌面，NixOS 上不为运行 Tauri Linux 桌面而引入 GTK/WebKit/FUSE 产品依赖。Windows 客户端（含原生壳）在本 NixOS 开发环境交叉编译，前端构建与测试也在宿主完成；产物交给 Windows 客体做原生运行验证，具体分工见 §2.6。

### 2.6 Windows 验证资源：优先 Quickemu

优先复用既有合规 Windows 11 x64 环境；没有时在 x86_64 NixOS + KVM 上用 Quickemu 快速准备客体。ARM 主机不能直接以 x86 KVM 方案执行，应记录所需 x86 Windows runner。镜像/许可或 KVM/容量缺失时标 blocked，继续无依赖任务。

2026-11-05 用户决定：Windows 客户端在本 NixOS 开发环境交叉编译，以节省虚拟机的 CPU、内存和磁盘资源。宿主承担 Rust 客户端、Windows 测试可执行文件、Tauri 壳及前端的构建；S009 在项目 devShell 中补齐并锁定目标、Rust 标准库、链接器及所需 SDK/库，实测后记录命令。客体承担运行、调试、安装/升级与 CFAPI、Explorer、GUI 原生验收。安装包生成与签名的宿主支持需在对应打包卡实测；如确有只能在 Windows 完成的步骤，记录证据与最小范围，不能据此把日常编译整体移回 VM。

1. 检查 `uname -m`、`/dev/kvm` 和当前用户读写权限；必要时在现有 NixOS 配置加入 kvm 组并 rebuild/重新登录。通过项目 devShell 或 `nix-shell -p quickemu` 提供 Quickemu，记录并锁定实际版本。
2. 在源码外的专用 VM 目录执行 `quickget windows 11`。下载接口不可用时从微软官方取得 ISO，并按安装版本文档配置；不得使用不明修改镜像。保留生成的 VirtIO、TPM、EFI 设置。
3. 首次启动前将配置中的 `cpu_cores="4"`、`ram="8G"`、`disk_size="100G"` 作为小样本起点，按宿主余量调整；修改 disk_size 不等于扩容已有分区。执行 `quickemu --vm windows-11.conf` 完成安装，修改自动安装账号默认密码。
4. 无桌面服务器可用 `--display spice --viewer none --access local --spice-port 5930` 启动，从操作电脑建立 `ssh -N -L 5930:127.0.0.1:5930 用户@服务器`，再用 SPICE 客户端连接 `spice://127.0.0.1:5930`。端口按占用调整，先核对安装版本的 help，不将 SPICE 直接暴露公网。
5. 客体按实际依赖准备 WebView2 等运行时，配置受限网络上的 OpenSSH 供 PowerShell 自动测试；不默认安装 Rust、C++ Build Tools/SDK、Node/pnpm 等完整构建工具链。宿主交付同一提交构建的 Windows 可执行文件、测试程序及运行所需资源，记录目标、工具链版本与 SHA-256，传输后校验哈希；不复制整个 target/node_modules。GUI、资源管理器与托盘测试使用已登录桌面会话。
6. CFAPI 根置于客体 NTFS，例如 `C:\vaultsyncd-test`，不用共享文件夹。默认 QEMU user 网络通常通过 `10.0.2.2` 到宿主，其他网络以实测为准；用 TCP/HTTPS 检查宿主端口，证书名称/信任必须匹配，不能禁用验证。客体 localhost 不是宿主。
7. 客体运行时与测试连接就绪后正常关机，备份完整 VM 配置、磁盘、EFI 与 TPM 状态，作为未注册客户端身份的基线；不能只恢复磁盘却忽略其他状态。第二台从基线独立初始化 DeviceId。每次测试用独立服务端实例/Vault；测试回滚与服务器状态配对，刻意陈旧客户端场景另记录。
8. 首台立即执行 S010—S012；第二台用于并发，正式规模按 S107。Wine、交叉编译、Linux fake 不能替代原生 CFAPI 结果。若已有 libvirt 环境可继续使用，不强制迁移。

依据：[Quickemu Windows](https://github.com/quickemu-project/quickemu/wiki/04-Create-Windows-virtual-machines)、[配置](https://github.com/quickemu-project/quickemu/blob/master/docs/quickemu_conf.5.md)、[QEMU 网络](https://www.qemu.org/docs/master/system/devices/net.html)。

### 2.7 代码开发使用 Git worktree

涉及代码改动时，执行根 `AGENTS.md` 的“Git worktree”约定，该处为规则的唯一维护入口；用户已授权此模式，无需再次确认。当前已安装 `using-git-worktrees`，入口为 `.agents/skills/using-git-worktrees/SKILL.md`。先读取 skill 并检查已有隔离，再准备任务工作树与基线；不要等到 S002 才开始遵守。

纯文档维护可留在当前工作区；一旦任务扩展到代码，先完成 worktree 准备。源码 worktree 不替代 §2.4 的服务状态与故障测试隔离，也不改变服务端工作目录的文件系统准入条件。交接按 §7.2 记录路径、分支、基线和提交；后续会话从该 worktree 续接。

## 3. 可直接复用的启动提示词

首次执行：

```text
你在 vaultsyncd 仓库根目录工作。本次只执行 agent/implementation-plan.md 的 S001。
先读取该文件 §1—2 和 S001，再按卡片读取 docs/design.md 指定章节。
如需改动代码，先按 AGENTS.md 使用已安装的 using-git-worktrees skill 准备隔离工作树。
用户已授权本项目所需 NixOS 配置与 rebuild。当前只完成这一张卡，
记录事实、验证、进度和交接，不实现后续卡，也不访问真实业务数据。
```

后续执行：

```text
在 vaultsyncd 仓库执行最早的 ready 任务，一次仅一张卡。
先读 AGENTS.md、implementation-plan.md §1—2、progress.md 的未完成状态，
再读本卡及直接依赖交接、design.md 指定小节和相关代码。
涉及代码改动时，先按 AGENTS.md 使用 using-git-worktrees skill，复用或创建任务 worktree，
检查基线后在其中编辑、验证和提交；用户已授权此模式，不重复询问。
若卡片过大，按 §2.1 拆分；若资源缺失，写明阻塞并选择无依赖任务。
实现后执行最小验证，更新任务记录、接口摘要与进度。
不要用“编译成功”代替平台验收，不要将 planned 命令报告为已执行。
```

## 4. 任务索引

- S185 — 删除 ZIP 专用入口与普通启动归属校验（用户最新要求；前置 S167/S174/S184，done；见任务卡与 `agent/tasks/S185.md`）。

编号顺序是推荐执行顺序，依赖列决定可执行性。表中的 L=NixOS/Linux，W=Windows 11，B=浏览器/前端（工具链可在 NixOS），X=跨平台组合。`+` 表示列出的前置任务均完成；单纯章节引用不是执行依赖。

| 步骤 | 结果 | 环境 | 前置 |
| --- | --- | --- | --- |
| S001 | 确认主机与同步环境基线 | L | 无 |
| S002 | 建立 agent 入口与上下文提取工具 | L | S001 |
| S003 | 建立 Nix 开发 shell | L | S001、S002 |
| S004 | 建立最小 Cargo workspace | L | S003 |
| S005 | 建立 pnpm workspace 与工具版本 | L | S003 |
| S006 | 建立本地任务编排与检查入口 | L | S004、S005 |
| S007 | 准备独立 btrfs 测试位置 | L | S001、S003 |
| S008 | 准备 Windows 客体与连接说明 | L | S001、S007 |
| S009 | 固定 Windows 交叉编译与客体验证工具链 | X | S008、S004、S005 |
| S010 | CFAPI 注册与占位创建原型 | W | S009 |
| S011 | CFAPI 读取、取消与离线原型 | W | S010 |
| S012 | CFAPI 释放与写入保护原型 | W | S011 |
| S013 | Linux 存储准入原型 | L | S007、S004 |
| S014 | Linux 普通文件发布原型 | L | S013 |
| S015 | 定义身份与有界计数类型 | L | S004 |
| S016 | 定义状态维度与错误分类 | L | S015 |
| S017 | 路径与名称纯规则 | L | S015 |
| S018 | 排除规则与解释 | L | S017 |
| S019 | 三方规划基本分支 | L | S015、S016、S017 |
| S020 | 首次合并规划 | L | S019 |
| S021 | 内容冲突及稳定副本身份 | L | S019 |
| S022 | 移动与删除冲突规划 | L | S020、S021 |
| S023 | 规划器操作重排验证 | L | S022 |
| S024 | 远端协议最小契约与生成通路 | L | S015、S006 |
| S025 | 本机 IPC 最小契约与生成通路 | L | S016、S024 |
| S026 | 服务端数据库迁移框架 | L | S015、S007 |
| S027 | 服务端命名空间表与约束 | L | S026 |
| S028 | 版本、事件与幂等表 | L | S027 |
| S029 | 上传、意图与租约表 | L | S028 |
| S030 | 客户端数据库与状态表 | L | S015、S016、S026 |
| S031 | 客户端事件、传输与意图持久化 | L | S030 |
| S032 | Linux 安全路径访问正式接口 | L | S014、S017 |
| S033 | 不可变对象写入与读取 | L | S029、S032 |
| S034 | 持久提交意图状态机 | L | S033 |
| S035 | 内容 CAS 与现场复核 | L | S034、S028 |
| S036 | 幂等重试与冲突内容提交 | L | S035、S021 |
| S037 | 启动恢复与崩溃注入 | L | S036 |
| S038 | 目录创建与同 Vault 移动 | L | S037、S022 |
| S039 | 可恢复删除与回收站 | L | S038 |
| S040 | 历史与回收站恢复操作 | L | S039 |
| S041 | 服务端进程与配置生命周期 | L | S037、S024 |
| S042 | 首次 owner 初始化与密码认证 | L | S041、S026 |
| S043 | 会话、刷新与 Vault 授权 | L | S042 |
| S044 | 管理 Vault 注册与健康 API | L | S043、S032、S027 |
| S045 | 上传会话和固定块接收 | L | S043、S029、S033 |
| S046 | 上传 finalize 与固定版本下载 | L | S045 |
| S047 | 操作 HTTP 接口与错误映射 | L | S036、S038、S039、S040、S043、S046 |
| S048 | 一致性快照、changes 与 SSE | L | S044、S047 |
| S049 | 外部目录初始稳定采集 | L | S044、S035 |
| S050 | inotify 与溢出补扫 | L | S049 |
| S051 | 外部移动、删除与挂载失效 | L | S050、S038、S039 |
| S052 | 服务端整合小型 HTTP 闭环 | L | S048、S051 |
| S053 | Rust HTTPS 客户端与认证刷新 | L | S043、S048 |
| S054 | 上传下载续传客户端 | L | S053、S045、S046、S031 |
| S055 | agent 核心端口与确定性执行器 | L | S023、S031、S054 |
| S056 | 本地 generation 与上传快照 | L | S055 |
| S057 | 接收事件与应用进度 | L | S055、S048 |
| S058 | 首次绑定与可恢复合并执行 | L | S056、S057、S020 |
| S059 | 本地受控应用与重启核对 | L | S058 |
| S060 | 旧游标与 epoch 重建 | L | S059 |
| S061 | 暂停、模式切换与解除绑定 | L | S059、S018 |
| S062 | 下载优先级与重试调度 | L | S054、S059 |
| S063 | 手动释放的核心安全条件 | L | S059、S061 |
| S064 | agent 状态、问题与恢复导出 | L | S060、S062、S063 |
| S065 | Windows 安全文件与本地扫描 | W | S012、S017、S059 |
| S066 | CFAPI 正式注册与元数据更新 | W | S065、S010 |
| S067 | CFAPI hydration 接入传输执行器 | W | S066、S054、S062 |
| S068 | Windows 写入检测与使用保护 | W | S066、S056、S059 |
| S069 | Windows 手动释放与恢复 | W | S067、S068、S063 |
| S070 | Windows agent 进程与单实例 | W | S067、S068、S069、S064 |
| S071 | Windows named pipe 传输与身份 | W | S070、S025 |
| S072 | 本机管理命令完整接入 | W | S071、S061、S064 |
| S073 | Windows CLI 最小管理入口 | W | S072 |
| S074 | 三台客户端基础互通 | X | S073、S052 |
| S075 | Linux 在线管理 socket 与 CLI | L | S043、S044、S047、S048 |
| S076 | 前端公共组件与两应用壳 | B | S005、S024、S025 |
| S077 | Web 登录与会话管理 | B | S076、S043 |
| S078 | Web Vault 与服务健康页面 | B | S077、S044、S051 |
| S079 | Web 历史、冲突与回收站 | B | S078、S040、S047 |
| S080 | Tauri Windows 壳与最小 IPC 桥 | W | S076、S072 |
| S081 | 桌面连接与 Binding 页面 | W | S080 |
| S082 | 桌面文件状态与手动操作 | W | S081、S069、S073 |
| S083 | 桌面历史与系统通知 | W | S082、S079、S064 |
| S084 | 客户端未上传数据恢复演练 | W | S083、S064 |
| S085 | 历史保留与对象引用标记 | L | S040、S029 |
| S086 | GC 删除协调与临时清理 | L | S085、S037 |
| S087 | 一致性备份快照与租约 | L | S086、S048 |
| S088 | 备份导出、校验与释放接口 | L | S087、S075 |
| S089 | 离线恢复到空目录 | L | S088、S037 |
| S090 | Web 备份管理入口 | B | S079、S088 |
| S091 | 协议与数据库升级演练 | X | S089、S074 |
| S092 | 构建依赖与协议边界检查 | L | S024、S025、S080 |
| S093 | 发布版本与产物清单 | L | S052、S076、S092 |
| S094 | Linux 服务端 Nix 构建包 | L | S093、S090 |
| S095 | NixOS 服务模块 | L | S094、S007 |
| S096 | 在本机启用隔离服务实例 | L | S095、S042 |
| S097 | HTTPS 与同源 Web 部署 | L | S096、S077 |
| S098 | 通用 Linux 包与容器配置 | L | S094、S097 |
| S099 | Windows 安装包与后台启动 | W | S083、S084、S093 |
| S100 | Windows 升级与卸载保护 | W | S099、S091 |
| S101 | 本地平台矩阵与安装验证 | X | S092、S098、S100 |
| S102 | 事件异常与旧设备集成验收 | X | S074、S089、S091 |
| S103 | 文件与挂载故障集成验收 | X | S074、S051、S068 |
| S104 | GC、备份与崩溃联合验收 | X | S086、S089、S102 |
| S105 | 安全与操作可达性验收 | X | S083、S090、S097、S100 |
| S106 | 小文件时延与常规规模基准 | X | S101、S102、S103、S104、S105 |
| S107 | 正式容量与大文件续传验证 | X | S106 |
| S108 | 发布清单与最终交接 | X | S101、S102、S103、S104、S105、S106、S107 |
| S109 | Windows 免安装 ZIP（状态留本机） | W | S099、S100 |
| S110 | 一键构建多平台 release 归档 | L | S093、S098、S109 |
| S111 | 移除 Linux ARM64 构建与验证范围 | X | S110 |
| S112 | 本机域名部署与 NixOS feature 配置 | L | S095、S096、S097、S111 |
| S113 | 主机用户运行与 pure 一键部署 | L | S112 |
| S115 | 服务端 Web HeroUI React 完整重构 | B/L | S077、S078、S079、S090、S112 |
| S116 | 将当前服务端部署到本机 | L/B | S113、S114、S115 |
| S117 | Web 管理界面紧凑化与 HeroUI 控件统一 | B | S115 |
| S118 | 将最新服务端（S117 之后）部署到本机 | L | S116、S117 |
| S119 | 服务端 Web 从 HeroUI 迁移到 Mantine | B/L | S117、S118 |
| S120 | 将 Mantine 版最新服务端部署到本机 | L | S118、S119 |
| S121 | Web 顶栏分区与 48px 高度并部署到本机 | B/L | S119、S120 |
| S122 | Web Vault 文件浏览器与属性查看 | B/L | S048、S078、S119、S121 |
| S124 | Web 文件浏览器左右分栏与 URL 目录定位 | B | S122、S123 |
| S125 | 将最新服务端（S124 分栏文件浏览器）部署到本机 | L/B | S113、S123、S124 |
| S126 | 精简已完成任务的过程记录 | D | 用户直接要求；独立文档维护 |
| S128 | 收紧文件浏览器目录树并部署生产 | B/L | S127、S125 |

## 5. 逐步任务卡

每张卡的验收加上 §2.3 的统一完成条件。文中的示例路径内部文件可随已存在接口调整；变更接口要更新短摘要与消费者，不能自行扩大产品范围。

### S001 — 确认主机与同步环境基线

- **环境与前置**：L；无。
- **只读设计范围**：`docs/design.md` §0—3、§5.10、C35/C39；加直接前置交接及相关源文件。
- **主要落点**：docs/environment.md；docs/design.md；agent/progress.md。
- **实现要点**：只读检查 pwd、Git 状态、NixOS/Nix 版本、架构、磁盘/挂载、空闲空间、现有服务端口和系统配置入口。记录可用权限与 Windows/KVM 资源，不输出秘密。将 NixOS 主要开发验证环境写入 design，保留 Ubuntu 发布兼容验证；建立所有任务的 todo 索引。
- **本步验收**：记录均有实际命令证据；没有把当前开发系统架构误写成已支持全部架构。
- **交接**：`agent/tasks/S001.md`，记录实际接口、验证与下一个 ready 任务。

### S002 — 建立 agent 入口与上下文提取工具

- **环境与前置**：L；S001。
- **只读设计范围**：`docs/design.md` §5、§23；加直接前置交接及相关源文件。
- **主要落点**：AGENTS.md；README.md；tools/context.py；docs/architecture/interfaces.md。
- **实现要点**：核对用户预置 AGENTS.md，按本仓库实际工具补充入口，不覆盖其中 worktree、commit skill/中文提交规则；探测并记录已安装 worktree 与 commit skill 的真实名称和位置。修改 context.py 前按 §2.7 准备任务 worktree。context.py 按完整标题提取本卡和指定设计小节，到同级/更高级标题结束；未知或歧义标题报错，代码围栏内的 # 不当标题。README 链接两份文档。
- **本步验收**：提取 S001 和 design §11.2，确认边界正确且不带其他卡；无关章节未自动全部注入；交接包含实际 worktree 路径、分支或 detached HEAD、基线提交与检查结果，并可从该路径续接。
- **交接**：`agent/tasks/S002.md`，记录实际接口、验证与下一个 ready 任务。

### S003 — 建立 Nix 开发 shell

- **环境与前置**：L；S001、S002。
- **只读设计范围**：`docs/design.md` §4.1、§5.7；加直接前置交接及相关源文件。
- **主要落点**：flake.nix；flake.lock；docs/environment.md。
- **实现要点**：锁定 nixpkgs，devShell 提供 Rust/Cargo/rustfmt/clippy、C 编译工具、pkg-config、Node/pnpm、Python、Git、SQLite 与基本诊断工具。优先使用锁定 nixpkgs 工具链，实际记录版本；仅在明确缺能力时引入额外 overlay。
- **本步验收**：nix develop 内输出工具版本，编译临时 Rust 程序并运行前端包管理器；新终端可复现，不依赖临时全局安装。
- **交接**：`agent/tasks/S003.md`，记录实际接口、验证与下一个 ready 任务。

### S004 — 建立最小 Cargo workspace

- **环境与前置**：L；S003。
- **只读设计范围**：`docs/design.md` §5.4、§5.7；加直接前置交接及相关源文件。
- **主要落点**：Cargo.toml；Cargo.lock；crates/domain/；rust-toolchain.toml；.gitignore。
- **实现要点**：根为虚拟 workspace，只加入真实存在的 domain 包；edition/MSRV/resolver 组合实际验证。rust-toolchain 与 Nix 提供版本保持一致，说明 Nix 与 Windows 如何取同版本。配置格式/lint，忽略 target、artifacts、运行数据。
- **本步验收**：cargo metadata、cargo check --locked、cargo fmt --check 成功；只有一个 Cargo.lock。
- **交接**：`agent/tasks/S004.md`，记录实际接口、验证与下一个 ready 任务。

### S005 — 建立 pnpm workspace 与工具版本

- **环境与前置**：L；S003。
- **只读设计范围**：`docs/design.md` §5.6—5.7；加直接前置交接及相关源文件。
- **主要落点**：package.json；pnpm-workspace.yaml；pnpm-lock.yaml；packages/ui/。
- **实现要点**：创建最小有实际导出的 TS UI 包及类型检查入口；仅加入已创建成员。锁定 packageManager，建立 React/TS 基础配置，应用后续加入。
- **本步验收**：pnpm install --frozen-lockfile 和包 typecheck 可运行；无嵌套锁文件。
- **交接**：`agent/tasks/S005.md`，记录实际接口、验证与下一个 ready 任务。

### S006 — 建立本地任务编排与检查入口

- **环境与前置**：L；S004、S005。
- **只读设计范围**：`docs/design.md` §5.7—5.8；加直接前置交接及相关源文件。
- **主要落点**：tools/xtask/；.cargo/config.toml。
- **实现要点**：xtask 先提供环境信息与已有检查编排；不存在的 generate/package 命令明确报未实现。先执行便携 Rust 与 TS 检查，平台测试以后接入；本地命令写 README。
- **本步验收**：错误子命令非零退出；故意引入格式错误能使检查失败；检查结果保留本地执行证据。
- **交接**：`agent/tasks/S006.md`，记录实际接口、验证与下一个 ready 任务。

### S007 — 准备独立 btrfs 测试位置

- **环境与前置**：L；S001、S003。
- **只读设计范围**：`docs/design.md` §11.4—11.5、§16.1；加直接前置交接及相关源文件。
- **主要落点**：docs/environment.md；tools/test-volume/。
- **实现要点**：检查候选目录及底层挂载；优先现有合规测试卷。必要时用小型新建普通镜像文件建立 btrfs loop，校验路径/设备归属并记录清理命令。规划 dev/test/正式实例状态与 vault 的互斥路径。
- **本步验收**：findmnt/statfs 证据证明实际后端；测试目录为空且与源码/用户数据无重叠；清理不触碰其他挂载。
- **交接**：`agent/tasks/S007.md`，记录实际接口、验证与下一个 ready 任务。

### S008 — 准备 Windows 客体与连接说明

- **环境与前置**：L；S001、S007。
- **只读设计范围**：`docs/design.md` §2.1、§14.2、§21.1；加直接前置交接及相关源文件。
- **主要落点**：docs/environment.md；docs/operations/windows-lab.md。
- **实现要点**：检查既有 Windows；否则按 §2.6 在已具备镜像和资源时用 Quickemu 配置 KVM VM，并记录启动/关闭/恢复/网络访问方式。必要系统修改先 build/test 后 switch，保留 SSH；客体 NTFS 和测试网络独立。缺镜像/许可/虚拟化条件则 blocked，并记录继续 Linux 任务的路径。
- **本步验收**：Windows 版本、NTFS、宿主可达地址有证据；尚无客体不得 done，不以 Wine 代替。
- **交接**：`agent/tasks/S008.md`，记录实际接口、验证与下一个 ready 任务。

### S009 — 固定 Windows 交叉编译与客体验证工具链

- **环境与前置**：X（NixOS 宿主构建 + Windows 11 客体运行）；S008、S004、S005。
- **只读设计范围**：`docs/design.md` §5.7、§14.2；加直接前置交接及相关源文件。
- **主要落点**：flake.nix/flake.lock；docs/operations/windows-lab.md；tests/platform/windows/。
- **实现要点**：按 §2.6 在 NixOS devShell 固定 Windows x64 交叉目标、Rust 标准库、链接器及所需 SDK/库，验证后记录具体方案与版本；Node/pnpm 留在宿主。客体只准备实际运行时与自动测试入口，不默认安装完整编译工具链。交付同一提交的 Windows 产物与测试资源并核对哈希，避免共享 target/node_modules；写最小宿主构建、传输和客体执行命令。
- **本步验收**：在宿主交叉编译最小 Rust 示例，传入 Windows 原生运行成功并核对产物 SHA-256；在宿主执行 pnpm 类型检查通过；记录宿主构建工具、目标和客体运行时版本。交叉编译成功不能替代客体执行证据，CFAPI 仍由 S010—S012 原生验收；尚未创建的完整客户端与安装包在对应卡继续验证。
- **交接**：`agent/tasks/S009.md`，记录实际接口、验证与下一个 ready 任务。

### S010 — CFAPI 注册与占位创建原型

- **环境与前置**：W；S009。
- **只读设计范围**：`docs/design.md` §14.1—14.2、I03/I05；加直接前置交接及相关源文件。
- **主要落点**：crates/platform-windows/examples/；tests/platform/windows/。
- **实现要点**：在专用 NTFS 目录注册测试 sync vault，建立目录/文件真实 placeholder，将不透明 EntryId/VersionId 编码进受限 identity。实现注销与保留用户数据的清理；FFI 记录生命周期。
- **本步验收**：系统可识别占位、文件逻辑大小正确；不是普通零字节文件；清理不删除测试内容。
- **交接**：`agent/tasks/S010.md`，记录实际接口、验证与下一个 ready 任务。

### S011 — CFAPI 读取、取消与离线原型

- **环境与前置**：W；S010。
- **只读设计范围**：`docs/design.md` §13、§14.2、T06/T22；加直接前置交接及相关源文件。
- **主要落点**：crates/platform-windows/examples/；tests/platform/windows/。
- **实现要点**：用固定测试内容源实现读取回调与范围请求，固定版本；处理取消、重复请求和超时。源不可用时回报明确失败；回调生命周期不能访问已释放状态。
- **本步验收**：实际文件读取字节/哈希正确；断开源后未下载内容失败；取消后句柄可关闭，无永久卡住。
- **交接**：`agent/tasks/S011.md`，记录实际接口、验证与下一个 ready 任务。

### S012 — CFAPI 释放与写入保护原型

- **环境与前置**：W；S011。
- **只读设计范围**：`docs/design.md` §8.3—8.4、§14.4、T07/T27/T28；加直接前置交接及相关源文件。
- **主要落点**：crates/platform-windows/examples/；tests/platform/windows/。
- **实现要点**：测试编辑器原地写、rename-over、共享锁、映射写与释放。建立已观察 generation 与使用保护的边界记录；不能证明安全时返回 Busy/Unknown。记录原生能力实际限制。
- **本步验收**：dirty/使用中内容不可释放；已确认干净样本可释放再读；每类编辑器保存有哈希证据，失败列为阻塞。
- **交接**：`agent/tasks/S012.md`，记录实际接口、验证与下一个 ready 任务。

### S013 — Linux 存储准入原型

- **环境与前置**：L；S007、S004。
- **只读设计范围**：`docs/design.md` §11.4—11.5、§16.1、T35；加直接前置交接及相关源文件。
- **主要落点**：crates/platform-linux-server/src/mounts/；tests/platform/linux/。
- **实现要点**：创建 Linux 适配包，通过目录句柄 fstatfs 与 mountinfo 检查类型/挂载身份。拒绝 FUSE/NFS/SMB/不支持后端；检查内部状态目录与子挂载。普通稀疏文件不等同 placeholder。
- **本步验收**：btrfs 和合规 bind mount 接受，其他后端拒绝；核对 FSID/子卷 ID 与别名重叠，拒绝嵌套子卷及子挂载、NOCOW/nodatasum，验证同卷跨子卷 reflink；记录无法普遍识别第三方 stub 的边界。
- **交接**：`agent/tasks/S013.md`，记录实际接口、验证与下一个 ready 任务。

### S014 — Linux 普通文件发布原型

- **环境与前置**：L；S013。
- **只读设计范围**：`docs/design.md` §11.1—11.3、C33；加直接前置交接及相关源文件。
- **主要落点**：crates/platform-linux-server/src/files/；tests/platform/linux/。
- **实现要点**：通过受限目录句柄读写，建立目标文件系统内暂存、文件和目录刷盘、受控替换原型。同卷用 FICLONE 到目标子卷内 staging，跨文件系统先校验复制到目标 staging；验证旧独立快照不随工作文件写入改变。
- **本步验收**：工作文件外部改写后对象旧字节仍在；EXDEV 不被忽略；外部竞争不能保证原子时暂停并报告。
- **交接**：`agent/tasks/S014.md`，记录实际接口、验证与下一个 ready 任务。

### S015 — 定义身份与有界计数类型

- **环境与前置**：L；S004。
- **只读设计范围**：`docs/design.md` §7.1、§6；加直接前置交接及相关源文件。
- **主要落点**：crates/domain/src/ids.rs；version.rs。
- **实现要点**：实现不透明 ID newtype、Revision/VaultSeq/Generation/Epoch；数据库 i64 上限检查，wire 大整数为十进制字符串；注入 ID/时间来源供确定性测试。
- **本步验收**：超过 JS 精度边界可往返；负数、溢出、格式不合法被拒绝。
- **交接**：`agent/tasks/S015.md`，记录实际接口、验证与下一个 ready 任务。

### S016 — 定义状态维度与错误分类

- **环境与前置**：L；S015。
- **只读设计范围**：`docs/design.md` §8.1—8.3、§15.4、§18.2；加直接前置交接及相关源文件。
- **主要落点**：crates/domain/src/state.rs；error.rs。
- **实现要点**：驻留、dirty、remote_apply、transfer、usage、availability 分开建模；区分可重试/需用户处理/损坏。实现纯状态投影函数，不写文件。
- **本步验收**：同时 dirty+offline+materialized 可表达；pending 不能投影成已同步。
- **交接**：`agent/tasks/S016.md`，记录实际接口、验证与下一个 ready 任务。

### S017 — 路径与名称纯规则

- **环境与前置**：L；S015。
- **只读设计范围**：`docs/design.md` §16.1—16.2、§7.4；加直接前置交接及相关源文件。
- **主要落点**：crates/fs-common/src/path.rs；name.rs。
- **实现要点**：建立相对路径分量校验、Windows 保留名/尾部字符和长度问题分类；不做自动小写或静默 Unicode 改名。给原生名称等价检查留端口。
- **本步验收**：穿越、绝对路径、NUL、文件目录同名与碰撞 fixture 得到明确错误；不把字符串前缀当目录重叠判断。
- **交接**：`agent/tasks/S017.md`，记录实际接口、验证与下一个 ready 任务。

### S018 — 排除规则与解释

- **环境与前置**：L；S017。
- **只读设计范围**：`docs/design.md` §10.4、C13；加直接前置交接及相关源文件。
- **主要落点**：crates/fs-common/src/ignore.rs；fixtures/paths/。
- **实现要点**：实现 .syncignore 的注释、通配、根锚定、目录和否定规则，返回命中的规则与原因；默认清单可编辑。排除表示不同步，绝不转换成 Delete。
- **本步验收**：父目录排除/子项否定、临时文件 rename-over 最终文件均有案例；相同规则结果确定。
- **交接**：`agent/tasks/S018.md`，记录实际接口、验证与下一个 ready 任务。

### S019 — 三方规划基本分支

- **环境与前置**：L；S015、S016、S017。
- **只读设计范围**：`docs/design.md` §9.1、I02/I04；加直接前置交接及相关源文件。
- **主要落点**：crates/planner/src/reconcile.rs。
- **实现要点**：定义 B/L/R 事实与 Plan，不访问 I/O；实现双方无变、仅本地变、仅远端变、同结果归并。输出动作前提、依赖和需要保护本地内容的说明。
- **本步验收**：表驱动输入得确定计划；mtime 相同但内容不同不被归并。
- **交接**：`agent/tasks/S019.md`，记录实际接口、验证与下一个 ready 任务。

### S020 — 首次合并规划

- **环境与前置**：L；S019。
- **只读设计范围**：`docs/design.md` §9.2、T01；加直接前置交接及相关源文件。
- **主要落点**：crates/planner/src/first_sync.rs。
- **实现要点**：无基线时本地独有/远端独有均保留；同路径同内容关联远端 ID；异内容与文件目录同名输出保留双方计划。首次合并与旧设备重建用不同入口。
- **本步验收**：任意缺项不输出删除；多次计算同一输入计划稳定。
- **交接**：`agent/tasks/S020.md`，记录实际接口、验证与下一个 ready 任务。

### S021 — 内容冲突及稳定副本身份

- **环境与前置**：L；S019。
- **只读设计范围**：`docs/design.md` §10.1—10.2、T02/T03；加直接前置交接及相关源文件。
- **主要落点**：crates/planner/src/conflict.rs。
- **实现要点**：输出服务器主条目保留与迟到分支新条目计划；冲突身份关联 OpId，名称受长度预算约束。同内容归并必须比较长度与 hash，不凭名字去重。
- **本步验收**：重试不会不断新增副本；交换设备时钟不改变版本判定。
- **交接**：`agent/tasks/S021.md`，记录实际接口、验证与下一个 ready 任务。

### S022 — 移动与删除冲突规划

- **环境与前置**：L；S020、S021。
- **只读设计范围**：`docs/design.md` §9.5、§10.1—10.3、T09—T13；加直接前置交接及相关源文件。
- **主要落点**：crates/planner/src/rename.rs；delete.rs。
- **实现要点**：实现移动+修改、双移动、删改和删除目录并发子项的矩阵；EntryId 不随路径变。目录环/目标冲突/旧 subtree_token 给重规划结果；跨 Vault 拆目标创建确认再源删除。
- **本步验收**：矩阵各分支有小 fixture；跨 Vault 目标失败不得计划完成源删除。
- **交接**：`agent/tasks/S022.md`，记录实际接口、验证与下一个 ready 任务。

### S023 — 规划器操作重排验证

- **环境与前置**：L；S022。
- **只读设计范围**：`docs/design.md` §6、§21.1；加直接前置交接及相关源文件。
- **主要落点**：crates/planner/tests/；fixtures/。
- **实现要点**：用固定种子枚举少量设备编辑/移动/删改/重试次序；检查唯一未确认内容总有保留去处。故障保存最短操作轨迹，不一次引入大型模型检查框架。
- **本步验收**：覆盖 I02/I03/I04/I06/I07；失败能按种子单独重放。
- **交接**：`agent/tasks/S023.md`，记录实际接口、验证与下一个 ready 任务。

### S024 — 远端协议最小契约与生成通路

- **环境与前置**：L；S015、S006。
- **只读设计范围**：`docs/design.md` §15.1—15.4、§5.6；加直接前置交接及相关源文件。
- **主要落点**：api/openapi.yaml；api/generation.toml；crates/protocol/；packages/api-client/。
- **实现要点**：先定义 capabilities、错误信封、长整数字符串与健康接口。小样本验证生成器能同时产 Rust DTO 与 TS client，锁版本并实现 xtask generate。业务 domain 到 wire 显式转换。
- **本步验收**：同源生成双方编译；重新生成无 diff；未知枚举不默认成功；此后按功能逐步扩展契约。
- **交接**：`agent/tasks/S024.md`，记录实际接口、验证与下一个 ready 任务。

### S025 — 本机 IPC 最小契约与生成通路

- **环境与前置**：L；S016、S024。
- **只读设计范围**：`docs/design.md` §15.5、§5.6；加直接前置交接及相关源文件。
- **主要落点**：api/local-ipc.schema.json；crates/local-ipc/；packages/local-ipc-types/。
- **实现要点**：定义 request_id、版本、命令/结果/错误、长度限制；先实现 GetStatus。生成 Rust/TS 类型，framing 有上限和部分读取处理；不把 Windows 传输实现放 DTO 包。
- **本步验收**：截断/超长/未知命令拒绝，Rust 与 TS 样本一致；本机 IPC 与服务器管理 API 分开。
- **交接**：`agent/tasks/S025.md`，记录实际接口、验证与下一个 ready 任务。

### S026 — 服务端数据库迁移框架

- **环境与前置**：L；S015、S007。
- **只读设计范围**：`docs/design.md` §7.2、§19.3；加直接前置交接及相关源文件。
- **主要落点**：crates/server-store/src/db.rs；migrations/。
- **实现要点**：独立 server 数据库种类标识、schema 版本、外键、WAL 与短事务接口。迁移前一致性备份；失败禁止写服务启动；旧二进制读到过新 schema 拒绝写。
- **本步验收**：从空库与旧 fixture 升级；中断迁移不产生可写半成品；客户端库误传被拒绝。
- **交接**：`agent/tasks/S026.md`，记录实际接口、验证与下一个 ready 任务。

### S027 — 服务端命名空间表与约束

- **环境与前置**：L；S026。
- **只读设计范围**：`docs/design.md` §7.2—7.4；加直接前置交接及相关源文件。
- **主要落点**：crates/server-store/src/entries.rs；vaults.rs；migrations/。
- **实现要点**：建 vaults/entries 及 owner scope，活跃同父名称唯一、父子同 Vault、目录环检查；提供事务内读写接口，网络代码不能随意 raw SQL。
- **本步验收**：非法跨 Vault 父项、重复名称和目录环失败，原数据保持。
- **交接**：`agent/tasks/S027.md`，记录实际接口、验证与下一个 ready 任务。

### S028 — 版本、事件与幂等表

- **环境与前置**：L；S027。
- **只读设计范围**：`docs/design.md` §7.2、I06/I09；加直接前置交接及相关源文件。
- **主要落点**：crates/server-store/src/versions.rs；operations.rs；changes.rs。
- **实现要点**：建 versions/objects/changes/operations，统一事务更新 seq 与操作结果；digest 基于规范化请求语义。相同 OpId 不同 digest 拒绝，紧凑结果保留规则明确。
- **本步验收**：回滚不暴露事件；重复操作返回同结果；VaultSeq 单调且事务批次不拆分。
- **交接**：`agent/tasks/S028.md`，记录实际接口、验证与下一个 ready 任务。

### S029 — 上传、意图与租约表

- **环境与前置**：L；S028。
- **只读设计范围**：`docs/design.md` §7.2、§11.3、§19.4；加直接前置交接及相关源文件。
- **主要落点**：crates/server-store/src/intents.rs；uploads.rs；leases.rs。
- **实现要点**：增加 uploads/parts/fs_intents/backup_leases 以及下载/提交对象保护所需租约模型；phase 有明确转换与恢复字段。持久化 phase 不靠普通日志推断。
- **本步验收**：重启能列出未完成意图、缺块与有效租约；重复 part 索引不产生两条记录。
- **交接**：`agent/tasks/S029.md`，记录实际接口、验证与下一个 ready 任务。

### S030 — 客户端数据库与状态表

- **环境与前置**：L；S015、S016、S026。
- **只读设计范围**：`docs/design.md` §7.3、§8.4；加直接前置交接及相关源文件。
- **主要落点**：crates/client-store/src/；migrations/。
- **实现要点**：建立独立 client schema 和迁移路径，先包含 bindings、baseline、remote、local、pending_ops；generation 与基线独立。不得复用 server schema 或数据库文件。
- **本步验收**：同条目能同时保存旧 baseline、新 remote 和本地 dirty；库种类错误拒绝。
- **交接**：`agent/tasks/S030.md`，记录实际接口、验证与下一个 ready 任务。

### S031 — 客户端事件、传输与意图持久化

- **环境与前置**：L；S030。
- **只读设计范围**：`docs/design.md` §7.3、§9.3、I09；加直接前置交接及相关源文件。
- **主要落点**：crates/client-store/src/events.rs；transfers.rs；intents.rs。
- **实现要点**：补 received_changes、transfers、local_intents、notifications；事件与接收游标同事务，应用游标单独。队列依赖可恢复，不能先推进游标再写任务。
- **本步验收**：任意事务失败后游标与任务一致；重启保留未应用事件和下载位图。
- **交接**：`agent/tasks/S031.md`，记录实际接口、验证与下一个 ready 任务。

### S032 — Linux 安全路径访问正式接口

- **环境与前置**：L；S014、S017。
- **只读设计范围**：`docs/design.md` §11.4—11.5、§16.1、T25；加直接前置交接及相关源文件。
- **主要落点**：crates/platform-linux-server/src/files/；mounts/。
- **实现要点**：封装根句柄下无符号链接逃逸的遍历/打开/创建，复核文件和挂载身份；检查 Vault 及内部目录的父子/别名/bind 重叠。真实 syscall 错误分类保留原因。
- **本步验收**：符号链接换目标与目录别名攻击不访问外部样本；权限失败不返回空目录。
- **交接**：`agent/tasks/S032.md`，记录实际接口、验证与下一个 ready 任务。

### S033 — 不可变对象写入与读取

- **环境与前置**：L；S029、S032。
- **只读设计范围**：`docs/design.md` §11.1—11.2、I12；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/objects/；ports/。
- **实现要点**：创建 server-core，按 §11.5 克隆候选、对固定副本做 hash/长度校验、独立对象暂存和持久发布；覆盖克隆后源写入/截断/删除、跨卷复制和错误分类；对象与可变工作文件不能共享可写 inode。端口定义在核心，入口用本地 wrapper 组装避免循环依赖。
- **本步验收**：改写工作文件不影响已保存对象；错 hash、短写、ENOSPC 不产生可提交引用。
- **交接**：`agent/tasks/S033.md`，记录实际接口、验证与下一个 ready 任务。

### S034 — 持久提交意图状态机

- **环境与前置**：L；S033。
- **只读设计范围**：`docs/design.md` §11.2—11.3；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/commit/intent.rs；recovery/。
- **实现要点**：为 put 定义 prepared/published/finalized 等明确 phase，记录 before/after、对象和目标身份。先持久意图再发布；只实现单文件 happy path 与可恢复状态，不扩展所有操作。
- **本步验收**：每个 phase 的数据库/磁盘证据可重建；错误不得变成 committed。
- **交接**：`agent/tasks/S034.md`，记录实际接口、验证与下一个 ready 任务。

### S035 — 内容 CAS 与现场复核

- **环境与前置**：L；S034、S028。
- **只读设计范围**：`docs/design.md` §11.2、§12.2、I01/I04；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/commit/put.rs。
- **实现要点**：Vault 内序列化提交，检查 epoch、revision、父目录、名称和现场指纹；现场不同返回 RECONCILE_REQUIRED。对象持久→意图→目标 staging 发布→元数据/changes/结果事务→ACK。
- **本步验收**：正常内容提交字节一致；过期 revision/外部现场变化不能覆盖；发布失败保持 pending/failed。
- **交接**：`agent/tasks/S035.md`，记录实际接口、验证与下一个 ready 任务。

### S036 — 幂等重试与冲突内容提交

- **环境与前置**：L；S035、S021。
- **只读设计范围**：`docs/design.md` §10.1—10.2、T02/T03；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/commit/conflict.rs。
- **实现要点**：接入稳定 OpId/digest 查询；过期不同内容生成一次冲突条目和映射，重复请求返回原结果。明确相同 OpId 不同请求错误；冲突也是正常受恢复保护的提交。
- **本步验收**：丢失响应后重试不多造副本；多设备不同内容均可按 hash 找到。
- **交接**：`agent/tasks/S036.md`，记录实际接口、验证与下一个 ready 任务。

### S037 — 启动恢复与崩溃注入

- **环境与前置**：L；S036。
- **只读设计范围**：`docs/design.md` §11.3、T23；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/recovery/；tests/server-integration/。
- **实现要点**：先恢复再开放写入口；按前/后 hash 区分未发布、已发布、已又被外部改写、无法解释。无法解释置 NEEDS_RECOVERY。注入每一持久化边界强杀测试子进程。
- **本步验收**：重启得到可证明旧/新结果或明确暂停；无“假成功”；旧对象与冲突候选保留。
- **交接**：`agent/tasks/S037.md`，记录实际接口、验证与下一个 ready 任务。

### S038 — 目录创建与同 Vault 移动

- **环境与前置**：L；S037、S022。
- **只读设计范围**：`docs/design.md` §9.5、§10.1、T09/T10；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/commit/namespace.rs。
- **实现要点**：创建目录与 move 使用同一意图/恢复机制；保持 EntryId，检查目标、目录环、父目录状态和基线。目录移动只变父关系，不重新上传子内容。
- **本步验收**：移动后对象引用不变，内容流量为零；移动中崩溃可恢复，目标冲突不覆盖。
- **交接**：`agent/tasks/S038.md`，记录实际接口、验证与下一个 ready 任务。

### S039 — 可恢复删除与回收站

- **环境与前置**：L；S038。
- **只读设计范围**：`docs/design.md` §10.3、T11/T12；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/commit/delete.rs；crates/server-store/src/trash.rs。
- **实现要点**：建立 subtree token、墓碑、trash 清单；大目录删除作为有界批次任务，受影响子树写入明确 busy，最终逻辑批次完整公布。工作目录移除失败不能成功 ACK。
- **本步验收**：并发新增使前提失败；中断可继续；删改保留修改内容，历史可读。
- **交接**：`agent/tasks/S039.md`，记录实际接口、验证与下一个 ready 任务。

### S040 — 历史与回收站恢复操作

- **环境与前置**：L；S039。
- **只读设计范围**：`docs/design.md` §10.3、I12、T13；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/history/。
- **实现要点**：按旧不可变版本创建新当前版本；回收站恢复文件/目录清单，目标占用则另名并返回实际 ID/路径。恢复使用内容租约与提交协调。
- **本步验收**：旧版本 hash 不变；同名目标未被覆盖；重复恢复 OpId 结果稳定。
- **交接**：`agent/tasks/S040.md`，记录实际接口、验证与下一个 ready 任务。

### S041 — 服务端进程与配置生命周期

- **环境与前置**：L；S037、S024。
- **只读设计范围**：`docs/design.md` §4.2、§17.2；加直接前置交接及相关源文件。
- **主要落点**：apps/server/src/；deploy/examples/vaultsyncd.toml。
- **实现要点**：建立 Linux-only 入口，加载配置、日志和依赖；恢复完成后 readiness 才为真。优雅停止停止新提交、保留任务证据。开发默认回环地址和独立端口。
- **本步验收**：启动/停止/重启样本正常；无 Windows/FUSE 依赖；损坏配置或恢复未完成不开放写入。
- **交接**：`agent/tasks/S041.md`，记录实际接口、验证与下一个 ready 任务。

### S042 — 首次 owner 初始化与密码认证

- **环境与前置**：L；S041、S026。
- **只读设计范围**：`docs/design.md` §17.1、C27；加直接前置交接及相关源文件。
- **主要落点**：apps/server-admin/；crates/server-core/src/auth/；api/openapi.yaml。
- **实现要点**：实现一次性离线 init：空状态、独占锁、交互/stdin 读密码、Argon2id；没有默认密码。登录与统一错误/限流契约；离线 init 复用 core 用例，不直接散落修改表。
- **本步验收**：重复 init 拒绝覆盖；错误密码失败；argv/日志/Nix store 无密码；运行中 init 被锁拒绝。
- **交接**：`agent/tasks/S042.md`，记录实际接口、验证与下一个 ready 任务。

### S043 — 会话、刷新与 Vault 授权

- **环境与前置**：L；S042。
- **只读设计范围**：`docs/design.md` §17.1、§15.2、T31；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/auth/；crates/server-store/src/sessions.rs。
- **实现要点**：令牌只存摘要；实现到期、注销、刷新轮换和并发刷新语义；每次资源请求按 owner/vault scope 检查。浏览器采用同源 HttpOnly cookie 会话并防 CSRF，agent 使用安全保管的凭据；ADR 说明两者边界。
- **本步验收**：跨 Vault、到期、撤销和旧刷新凭据拒绝；cookie 写请求校验 Origin/CSRF；响应不泄露绝对路径。
- **交接**：`agent/tasks/S043.md`，记录实际接口、验证与下一个 ready 任务。

### S044 — 管理 Vault 注册与健康 API

- **环境与前置**：L；S043、S032、S027。
- **只读设计范围**：`docs/design.md` §11.4、§15.2；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/api/vaults.rs；api/openapi.yaml。
- **实现要点**：只接受预配置允许基目录中的普通目录；身份与重叠检查通过后注册。启动不自动以空库覆盖既有文件；返回待导入/健康状态，普通客户端看不到绝对路径。
- **本步验收**：合规目录注册；父子/别名/FUSE/不合规内部目录拒绝；实际数据未改写。
- **交接**：`agent/tasks/S044.md`，记录实际接口、验证与下一个 ready 任务。

### S045 — 上传会话和固定块接收

- **环境与前置**：L；S043、S029、S033。
- **只读设计范围**：`docs/design.md` §13.1—13.2、§15.2；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/api/uploads.rs；crates/transfer/src/。
- **实现要点**：按授权 Vault 建 upload，流式接收指定索引/长度/hash 的块，写完持久化再记完成；总大小/并发有界。重复块字节不一致拒绝，状态由服务器给出。
- **本步验收**：断开重连可获取真实已收块；越界索引/超大载荷/错块/跨会话访问拒绝。
- **交接**：`agent/tasks/S045.md`，记录实际接口、验证与下一个 ready 任务。

### S046 — 上传 finalize 与固定版本下载

- **环境与前置**：L；S045。
- **只读设计范围**：`docs/design.md` §13.2、I01/I05/I15；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/api/content.rs；api/openapi.yaml。
- **实现要点**：完整拼装流式校验后发布对象，finalize 只返回可提交引用；下载按 vault/entry/version 与 Range 返回固定字节，下载租约防 GC，不能按路径替换成新 head。
- **本步验收**：finalize 后尚无条目提交；下载中 head 改变仍为旧版；过期版本明确 VERSION_GONE。
- **交接**：`agent/tasks/S046.md`，记录实际接口、验证与下一个 ready 任务。

### S047 — 操作 HTTP 接口与错误映射

- **环境与前置**：L；S036、S038、S039、S040、S043、S046。
- **只读设计范围**：`docs/design.md` §15.2—15.4；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/api/operations.rs；api/openapi.yaml。
- **实现要点**：暴露 put/create/move/delete/restore 和按 OpId 查询；扩展 schema 然后生成双方 DTO。pending 用 202，CAS/子树冲突按契约返回，完整错误保留 request_id。
- **本步验收**：HTTP 级重试、旧 epoch、相同 OpId 不同 digest 与响应丢失可验证；无提前已同步。
- **交接**：`agent/tasks/S047.md`，记录实际接口、验证与下一个 ready 任务。

### S048 — 一致性快照、changes 与 SSE

- **环境与前置**：L；S044、S047。
- **只读设计范围**：`docs/design.md` §9.3—9.4；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/api/snapshots.rs；events.rs。
- **实现要点**：生成固定 snapshot_id/high_water_seq 的短期元数据清单；分页只读该清单。changes 有序事务批次，限额不能拆事务；SSE 只通知有新 seq，游标过期明确 410。
- **本步验收**：边写边翻页仍是一致快照；SSE 丢/重/乱序后 changes 可修复；过期游标不返回假空集。
- **交接**：`agent/tasks/S048.md`，记录实际接口、验证与下一个 ready 任务。

### S049 — 外部目录初始稳定采集

- **环境与前置**：L；S044、S035。
- **只读设计范围**：`docs/design.md` §12.1—12.2、C33；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/observer/scan.rs。
- **实现要点**：枚举普通已有文件，稳定窗口后 reflink 私有候选并对候选校验，采集前后核对身份/指纹；跨卷走校验复制；不把克隆原子性解释为应用保存完成；通过同一提交事件管线采纳外部内容。首次未采集成功的条目不标历史已保护。
- **本步验收**：SSH 建文件/改内容产生版本和事件；持续写入重排队；测试不声称保留未观察瞬态。
- **交接**：`agent/tasks/S049.md`，记录实际接口、验证与下一个 ready 任务。

### S050 — inotify 与溢出补扫

- **环境与前置**：L；S049。
- **只读设计范围**：`docs/design.md` §12.1、T20/T27；加直接前置交接及相关源文件。
- **主要落点**：crates/platform-linux-server/src/watcher/；crates/server-core/src/observer/。
- **实现要点**：实现监视、去抖和有界队列，事件只是变化线索；溢出/重启立即补扫，周期元数据扫描与分批内容复核补漏。忽略本服务写入要按身份/意图核对，不能按时间窗口全部忽略。
- **本步验收**：rename-over 最终内容被采集；注入溢出和保留 mtime 改写最终发现；权限失败不推删除。
- **交接**：`agent/tasks/S050.md`，记录实际接口、验证与下一个 ready 任务。

### S051 — 外部移动、删除与挂载失效

- **环境与前置**：L；S050、S038、S039。
- **只读设计范围**：`docs/design.md` §9.5、§12.3、T17/T19/T35；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/observer/；crates/platform-linux-server/src/mounts/。
- **实现要点**：rename 优先 inode/配对事件证据，同 hash 不猜唯一身份；完整健康扫描才推断删除。每次挂载变化检查身份和子挂载，异常批量删除按设计暂停核对。
- **本步验收**：卸载后露出空目录置 unavailable，零删除事件；多个同内容文件移动不串 ID。
- **交接**：`agent/tasks/S051.md`，记录实际接口、验证与下一个 ready 任务。

### S052 — 服务端整合小型 HTTP 闭环

- **环境与前置**：L；S048、S051。
- **只读设计范围**：`docs/design.md` §21、T03/T17/T21/T22/T23；加直接前置交接及相关源文件。
- **主要落点**：tests/server-integration/；api/examples/。
- **实现要点**：实际启动隔离 server，经登录、Vault、上传、提交、快照、下载、外部修改形成闭环；补齐剩余契约示例，故障分别作用工作盘与状态盘。
- **本步验收**：重启后版本/hash/事件一致；两种满盘均不提前 committed；测试无需 Windows。
- **交接**：`agent/tasks/S052.md`，记录实际接口、验证与下一个 ready 任务。

### S053 — Rust HTTPS 客户端与认证刷新

- **环境与前置**：L；S043、S048。
- **只读设计范围**：`docs/design.md` §13、§17.3；加直接前置交接及相关源文件。
- **主要落点**：crates/http-client/src/。
- **实现要点**：按生成 DTO 包装认证、超时、取消和错误分类；TLS 正常校验，开发 CA 显式信任；同会话刷新合并，失败保留本地队列。提供 transport 测试替身。
- **本步验收**：401 刷新一次后重试；无无限刷新；证书错误明确，不能全局禁用证书校验。
- **交接**：`agent/tasks/S053.md`，记录实际接口、验证与下一个 ready 任务。

### S054 — 上传下载续传客户端

- **环境与前置**：L；S053、S045、S046、S031。
- **只读设计范围**：`docs/design.md` §13.1—13.3；加直接前置交接及相关源文件。
- **主要落点**：crates/transfer/src/；crates/http-client/src/。
- **实现要点**：客户端固定上传快照/下载 VersionId，服务端缺块清单驱动续传；持久位图、长度/hash，有限并发与取消恢复。整文件 hash 流式计算，不读全文件入内存。
- **本步验收**：断线只补缺块；错块重传；混版本不发布；不是跨版本块增量。
- **交接**：`agent/tasks/S054.md`，记录实际接口、验证与下一个 ready 任务。

### S055 — agent 核心端口与确定性执行器

- **环境与前置**：L；S023、S031、S054。
- **只读设计范围**：`docs/design.md` §5.5、§14.1；加直接前置交接及相关源文件。
- **主要落点**：crates/agent-core/src/ports/；executor/。
- **实现要点**：建立文件/平台、时钟、远端和状态端口；动作先写意图再执行，定义依赖调度和暂停。测试用 fake 端口仅在测试构建中启用，不生成 Linux 同步客户端产品。
- **本步验收**：失败动作及依赖保留；重试次数有界；端口之间无循环依赖。
- **交接**：`agent/tasks/S055.md`，记录实际接口、验证与下一个 ready 任务。

### S056 — 本地 generation 与上传快照

- **环境与前置**：L；S055。
- **只读设计范围**：`docs/design.md` §8.3—8.4、T04；加直接前置交接及相关源文件。
- **主要落点**：crates/agent-core/src/executor/upload.rs。
- **实现要点**：本地变更提升 generation；快照读取前后复核，固定内容到服务器 committed 且 baseline 落库才允许清理。ACK 只清对应 generation，冲突保存 ID 映射。
- **本步验收**：上传时二次保存仍 dirty；旧 ACK 不清新版本；响应丢失用原 OpId 查询。
- **交接**：`agent/tasks/S056.md`，记录实际接口、验证与下一个 ready 任务。

### S057 — 接收事件与应用进度

- **环境与前置**：L；S055、S048。
- **只读设计范围**：`docs/design.md` §9.3、I09；加直接前置交接及相关源文件。
- **主要落点**：crates/agent-core/src/engine/events.rs。
- **实现要点**：接收批次事务写 remote/任务/received_cursor，应用完成后更新 baseline/applied_cursor。SSE 唤醒加周期 pull；关联任务失败不阻塞无依赖条目。
- **本步验收**：进程各持久化点中断不丢事件；重复/乱序提示不重复应用。
- **交接**：`agent/tasks/S057.md`，记录实际接口、验证与下一个 ready 任务。

### S058 — 首次绑定与可恢复合并执行

- **环境与前置**：L；S056、S057、S020。
- **只读设计范围**：`docs/design.md` §9.2、T01；加直接前置交接及相关源文件。
- **主要落点**：crates/agent-core/src/bindings/import.rs。
- **实现要点**：验证本地根/模式，固定远端快照，保存本地导入清单，执行规划并映射临时 ID；后续 changes 从 high_water 接续。上传仍带 CAS。
- **本步验收**：两端非空目录中断后继续；同名异内容两份保留；无基线不会删除。
- **交接**：`agent/tasks/S058.md`，记录实际接口、验证与下一个 ready 任务。

### S059 — 本地受控应用与重启核对

- **环境与前置**：L；S058。
- **只读设计范围**：`docs/design.md` §8.4、T05/T23；加直接前置交接及相关源文件。
- **主要落点**：crates/agent-core/src/executor/apply.rs；recovery/。
- **实现要点**：下载到暂存并校验，持久意图后调用平台替换，成功后 baseline。崩溃重启对不能证明 clean 的真实内容重新核对，unknown 禁止破坏操作。
- **本步验收**：文件已换/DB 未更新可恢复；本地再次修改导致停止替换；旧 clean 记录不能覆盖新字节。
- **交接**：`agent/tasks/S059.md`，记录实际接口、验证与下一个 ready 任务。

### S060 — 旧游标与 epoch 重建

- **环境与前置**：L；S059。
- **只读设计范围**：`docs/design.md` §9.4、T15/T16；加直接前置交接及相关源文件。
- **主要落点**：crates/agent-core/src/recovery/resync.rs。
- **实现要点**：保留旧 baseline、脏内容和队列；旧 clean 远端缺失不重新上传，脏缺失生成恢复候选。epoch 改变隔离旧操作、拉新快照，不盲目重放旧 Delete。
- **本步验收**：日志/历史过期与恢复旧备份两种场景结果不同且可解释；无自动复活历史删除。
- **交接**：`agent/tasks/S060.md`，记录实际接口、验证与下一个 ready 任务。

### S061 — 暂停、模式切换与解除绑定

- **环境与前置**：L；S059、S018。
- **只读设计范围**：`docs/design.md` §10.4、T30；加直接前置交接及相关源文件。
- **主要落点**：crates/agent-core/src/bindings/。
- **实现要点**：full/on_demand 切换形成可恢复任务；full 必须下载所需完整内容后才完成。排除变更保留现有内容并记录 excluded，解除绑定按驻留/dirty 提示恢复选择。
- **本步验收**：模式切换不清脏文件；排除/解绑不生成全局 Delete；操作中断可恢复。
- **交接**：`agent/tasks/S061.md`，记录实际接口、验证与下一个 ready 任务。

### S062 — 下载优先级与重试调度

- **环境与前置**：L；S054、S059。
- **只读设计范围**：`docs/design.md` §13.3、§18.2；加直接前置交接及相关源文件。
- **主要落点**：crates/agent-core/src/engine/scheduler.rs。
- **实现要点**：用户打开优先、后台传输有限并发；退避带 jitter，上限和取消持久化；断网状态可见且静默。磁盘满不自动驱逐完整缓存/历史。
- **本步验收**：持续失败不忙循环；用户读取不会被大文件队列无限饿死；重启仍保留队列。
- **交接**：`agent/tasks/S062.md`，记录实际接口、验证与下一个 ready 任务。

### S063 — 手动释放的核心安全条件

- **环境与前置**：L；S059、S061。
- **只读设计范围**：`docs/design.md` §8.4、T07/T08；加直接前置交接及相关源文件。
- **主要落点**：crates/agent-core/src/executor/release.rs。
- **实现要点**：释放前确认无 dirty/冲突候选、generation 未变、平台可保护，服务器版本仍可读并持有短租约。full 模式按设计拒绝释放；平台成功后才改驻留状态。
- **本步验收**：条件任一变化拒绝；检查后 GC 竞争仍保全版本；没有自动 LRU。
- **交接**：`agent/tasks/S063.md`，记录实际接口、验证与下一个 ready 任务。

### S064 — agent 状态、问题与恢复导出

- **环境与前置**：L；S060、S062、S063。
- **只读设计范围**：`docs/design.md` §18、§19.2；加直接前置交接及相关源文件。
- **主要落点**：crates/agent-core/src/status/；recovery/export.rs。
- **实现要点**：汇总多维状态和子任务，问题按 vault/原因去重；ExportRecovery 导出脏内容、基线和清单至指定目录，路径安全且不覆盖。日志脱敏有大小上限。
- **本步验收**：pending 不显示同步完成；断网无弹窗风暴；恢复包包含未上传字节且可校验。
- **交接**：`agent/tasks/S064.md`，记录实际接口、验证与下一个 ready 任务。

### S065 — Windows 安全文件与本地扫描

- **环境与前置**：W；S012、S017、S059。
- **只读设计范围**：`docs/design.md` §14.2、§16、T25/T26；加直接前置交接及相关源文件。
- **主要落点**：crates/platform-windows/src/files/；watcher/。
- **实现要点**：实现 Windows 文件身份、reparse/symlink 边界、卷身份/重叠检查、监视与补扫；枚举 placeholder 元数据不意外 hydration。原生名称等价验证落实。
- **本步验收**：保留名/大小写/长路径给明确错误；扫描不下载全盘；权限/监视溢出不是删除。
- **交接**：`agent/tasks/S065.md`，记录实际接口、验证与下一个 ready 任务。

### S066 — CFAPI 正式注册与元数据更新

- **环境与前置**：W；S065、S010。
- **只读设计范围**：`docs/design.md` §14.1—14.2、T09；加直接前置交接及相关源文件。
- **主要落点**：crates/platform-windows/src/cfapi/；shell/。
- **实现要点**：把原型整理为 adapter：注册生命周期、稳定 identity、占位枚举/更新/移动；每回调绑定正确 Binding/Entry/Version。注销不删除文件。
- **本步验收**：跨重启 ID 稳定；占位 rename 不传内容；陈旧/损坏 identity 明确拒绝。
- **交接**：`agent/tasks/S066.md`，记录实际接口、验证与下一个 ready 任务。

### S067 — CFAPI hydration 接入传输执行器

- **环境与前置**：W；S066、S054、S062。
- **只读设计范围**：`docs/design.md` §13、§14.2、T06/T22；加直接前置交接及相关源文件。
- **主要落点**：crates/platform-windows/src/cfapi/；apps/agent/src/bootstrap.rs。
- **实现要点**：将实际回调请求映射到固定版本下载和取消，线程边界不阻塞 UI/不持数据库事务；部分下载维持 partial 状态，离线明确错误。
- **本步验收**：打开真实占位获得准确内容；取消后可重读；中途 head 更新不混版。
- **交接**：`agent/tasks/S067.md`，记录实际接口、验证与下一个 ready 任务。

### S068 — Windows 写入检测与使用保护

- **环境与前置**：W；S066、S056、S059。
- **只读设计范围**：`docs/design.md` §8.4、§14.4、T04/T27/T28；加直接前置交接及相关源文件。
- **主要落点**：crates/platform-windows/src/files/；watcher/。
- **实现要点**：正式实现原型验证过的保存/替换路径；unknown/busy 保留本地内容，监视事件是线索，替换前重新核对 generation 与现场。不可证明绝对互斥的情况记录真实边界。
- **本步验收**：编辑器原地/原子保存、共享锁、映射写同时远端更新不静默丢数据；失败保留版本。
- **交接**：`agent/tasks/S068.md`，记录实际接口、验证与下一个 ready 任务。

### S069 — Windows 手动释放与恢复

- **环境与前置**：W；S067、S068、S063。
- **只读设计范围**：`docs/design.md` §14.2、T07/T08/T30；加直接前置交接及相关源文件。
- **主要落点**：crates/platform-windows/src/cfapi/release.rs。
- **实现要点**：核心校验后调用正式 dehydration，平台错误还原/保持状态；释放中崩溃用 local_intent 核对。缓存过期服务器不可读时保留唯一旧内容。
- **本步验收**：真实占用/dirty 拒绝；成功释放后元数据保留；网络/磁盘故障不误报成功。
- **交接**：`agent/tasks/S069.md`，记录实际接口、验证与下一个 ready 任务。

### S070 — Windows agent 进程与单实例

- **环境与前置**：W；S067、S068、S069、S064。
- **只读设计范围**：`docs/design.md` §4.2、§5.3；加直接前置交接及相关源文件。
- **主要落点**：apps/agent/src/。
- **实现要点**：组装 core/store/Windows/HTTP，用户会话内单实例，启动恢复先于同步；实现信号/退出、日志和最小运行配置，凭据放 Windows 凭据库。
- **本步验收**：重启自动恢复队列；启动第二实例不抢同一数据库；GUI 尚不存在也能同步。
- **交接**：`agent/tasks/S070.md`，记录实际接口、验证与下一个 ready 任务。

### S071 — Windows named pipe 传输与身份

- **环境与前置**：W；S070、S025。
- **只读设计范围**：`docs/design.md` §15.5、I15；加直接前置交接及相关源文件。
- **主要落点**：crates/platform-windows/src/ipc/。
- **实现要点**：受限 named pipe ACL，检查本机用户和消息长度；部分帧、并发请求、连接关闭处理有界。GUI/CLI 仅消费 IPC 模块，不启用 CFAPI 回调。
- **本步验收**：其他用户/畸形消息拒绝；同一用户可查询状态；断连不终止 agent。
- **交接**：`agent/tasks/S071.md`，记录实际接口、验证与下一个 ready 任务。

### S072 — 本机管理命令完整接入

- **环境与前置**：W；S071、S061、S064。
- **只读设计范围**：`docs/design.md` §15.5、§18.1；加直接前置交接及相关源文件。
- **主要落点**：api/local-ipc.schema.json；crates/agent-core/src/；apps/agent/。
- **实现要点**：逐项扩展 Get/List/Create/Pause/Resume/Download/Release/Problems/Retry/Detach/ExportRecovery；补连接登录、模式设置和历史操作所需命令。生成 DTO，执行器复核参数，状态推送带版本和全量恢复。
- **本步验收**：每项 P0 操作有处理器/错误/样本；省略命令不能靠 UI 隐藏；丢推送可重新查询。
- **交接**：`agent/tasks/S072.md`，记录实际接口、验证与下一个 ready 任务。

### S073 — Windows CLI 最小管理入口

- **环境与前置**：W；S072。
- **只读设计范围**：`docs/design.md` §5.1、§18.1；加直接前置交接及相关源文件。
- **主要落点**：apps/cli/src/。
- **实现要点**：建立 vaultsyncd-cli，命令映射 IPC，提供机器可读输出和稳定退出码；密码从安全输入传给 agent，不写命令历史。加入下载/释放/恢复导出等入口。
- **本步验收**：真实 agent 上查询/暂停/恢复/释放失败说明准确；CLI 不直接开 client.sqlite。
- **交接**：`agent/tasks/S073.md`，记录实际接口、验证与下一个 ready 任务。

### S074 — 三台客户端基础互通

- **环境与前置**：X；S073、S052。
- **只读设计范围**：`docs/design.md` §21、T01—T05/T09—T14；加直接前置交接及相关源文件。
- **主要落点**：tests/e2e/；tests/scenarios/。
- **实现要点**：建立真实跨进程 runner，经网络和 IPC 驱动三个 Windows 测试身份/环境；分别非空导入、离线编辑、重命名、删改、响应丢失。记录拓扑，不把多个进程冒充独立实机。
- **本步验收**：每份输入内容都能在当前/冲突/历史找到；各端最终收敛；结果有提交和系统版本。
- **交接**：`agent/tasks/S074.md`，记录实际接口、验证与下一个 ready 任务。

### S075 — Linux 在线管理 socket 与 CLI

- **环境与前置**：L；S043、S044、S047、S048。
- **只读设计范围**：`docs/design.md` §5.3、§15、§17.2；加直接前置交接及相关源文件。
- **主要落点**：apps/server-admin/src/；crates/platform-linux-server/src/ipc/。
- **实现要点**：受限 Unix socket 复用管理用例，校验 socket 权限和调用者身份。提供 vaults/status/problems/history/trash 等命令；远程管理走已认证 HTTPS，不直接改在线 DB。
- **本步验收**：SSH 下 CLI 管理真实隔离服务；无权限用户失败；JSON 输出不含长期凭据。
- **交接**：`agent/tasks/S075.md`，记录实际接口、验证与下一个 ready 任务。

### S076 — 前端公共组件与两应用壳

- **环境与前置**：B；S005、S024、S025。
- **只读设计范围**：`docs/design.md` §5.3、§5.6、§18.1；加直接前置交接及相关源文件。
- **主要落点**：packages/ui/；apps/web/；apps/desktop/。
- **实现要点**：创建 React/TS 应用和路由，复用按钮/表格/状态组件；Vite 构建并固定版本。desktop 暂用明确的 IPC 测试替身，web 用 API 客户端；不把 Tauri 依赖放共享 UI。
- **本步验收**：两个应用可独立类型检查/构建；未连接显示未连接，不展示假同步成功。
- **交接**：`agent/tasks/S076.md`，记录实际接口、验证与下一个 ready 任务。

### S077 — Web 登录与会话管理

- **环境与前置**：B；S076、S043。
- **只读设计范围**：`docs/design.md` §17、§18.1；加直接前置交接及相关源文件。
- **主要落点**：apps/web/src/features/auth/；services/。
- **实现要点**：同源 cookie 会话、登录/退出/过期处理、CSRF；限制开发代理 origin。账号基础管理至少包含 owner 改密和会话撤销，契约先补齐并复用 auth 用例。
- **本步验收**：浏览器登录/改密/注销后受保护请求行为正确；token 不进入 localStorage 或页面日志。
- **交接**：`agent/tasks/S077.md`，记录实际接口、验证与下一个 ready 任务。

### S078 — Web Vault 与服务健康页面

- **环境与前置**：B；S077、S044、S051。
- **只读设计范围**：`docs/design.md` §18.1；加直接前置交接及相关源文件。
- **主要落点**：apps/web/src/features/vaults/。
- **实现要点**：显示允许基目录范围、注册校验结果、Vault 状态/采集进度/错误原因；路径管理仅 owner。不能将暂未扫描显示为空目录已同步。
- **本步验收**：普通目录可注册；重叠/不支持后端/挂载失效页面给可执行原因。
- **交接**：`agent/tasks/S078.md`，记录实际接口、验证与下一个 ready 任务。

### S079 — Web 历史、冲突与回收站

- **环境与前置**：B；S078、S040、S047。
- **只读设计范围**：`docs/design.md` §18.1、§10.3；加直接前置交接及相关源文件。
- **主要落点**：apps/web/src/features/history/；trash/；problems/。
- **实现要点**：分页列出版本/冲突副本/到期时间，调用 Restore 并显示实际恢复位置。关闭通知不删除副本；恢复 pending 单独显示，失败可重试同一操作。
- **本步验收**：同名恢复保留目标；页面和服务器结果一致；客户端断网重试不产生重复恢复。
- **交接**：`agent/tasks/S079.md`，记录实际接口、验证与下一个 ready 任务。

### S080 — Tauri Windows 壳与最小 IPC 桥

- **环境与前置**：W；S076、S072。
- **只读设计范围**：`docs/design.md` §5.3、§17.3；加直接前置交接及相关源文件。
- **主要落点**：apps/desktop/src-tauri/。
- **实现要点**：src-tauri 加入根 Cargo workspace，最小 capabilities/CSP、托盘和 typed commands。桥接 named pipe，前端不拿令牌/DB 句柄，拒绝任意 shell/path 命令。
- **本步验收**：Windows 真壳读取 agent 状态；恶意 command 被拒；只有根 Cargo.lock。
- **交接**：`agent/tasks/S080.md`，记录实际接口、验证与下一个 ready 任务。

### S081 — 桌面连接与 Binding 页面

- **环境与前置**：W；S080。
- **只读设计范围**：`docs/design.md` §18.1、§15.5；加直接前置交接及相关源文件。
- **主要落点**：apps/desktop/src/features/connections/；bindings/。
- **实现要点**：通过桥实现登录、选 Vault/本地目录/模式、导入进度、暂停恢复。登录凭据由 agent 存储；页面关闭不销毁 agent。
- **本步验收**：创建绑定实际启动合并；错误路径/证书错误可见；关闭 GUI 后文件仍同步。
- **交接**：`agent/tasks/S081.md`，记录实际接口、验证与下一个 ready 任务。

### S082 — 桌面文件状态与手动操作

- **环境与前置**：W；S081、S069、S073。
- **只读设计范围**：`docs/design.md` §18.1、§8.2；加直接前置交接及相关源文件。
- **主要落点**：apps/desktop/src/features/files/；problems/。
- **实现要点**：提供下载、释放、重试、模式设置、解绑和恢复导出；多维状态投影与执行器统一。释放提示本地内容/离线影响，UI 和后端双重检查。
- **本步验收**：dirty/忙文件按钮解释原因且后端拒绝；pending 不显示已同步；解绑不全局删除。
- **交接**：`agent/tasks/S082.md`，记录实际接口、验证与下一个 ready 任务。

### S083 — 桌面历史与系统通知

- **环境与前置**：W；S082、S079、S064。
- **只读设计范围**：`docs/design.md` §18、T31；加直接前置交接及相关源文件。
- **主要落点**：apps/desktop/src/features/history/；src-tauri/src/。
- **实现要点**：通过 agent 管理历史/回收站恢复，后端仍验证权限；关键故障去重系统通知，普通断网仅状态。通知关闭时问题页仍完整，恢复后更新状态。
- **本步验收**：磁盘满/冲突/挂载变化进入问题页；无重复通知风暴；恢复结果正确。
- **交接**：`agent/tasks/S083.md`，记录实际接口、验证与下一个 ready 任务。

### S084 — 客户端未上传数据恢复演练

- **环境与前置**：W；S083、S064。
- **只读设计范围**：`docs/design.md` §19.2、T05/T07/T28；加直接前置交接及相关源文件。
- **主要落点**：tests/windows-integration/；docs/operations/recovery.md。
- **实现要点**：构造 agent 异常、dirty 文件、未完成 intent，先做 ExportRecovery；校验导出内容和基线，验证从恢复包人工取回路径。安装卸载尚未实现时不伪造该验证。
- **本步验收**：每份未确认数据都有可读导出和 hash；导出失败不删除源。
- **交接**：`agent/tasks/S084.md`，记录实际接口、验证与下一个 ready 任务。

### S085 — 历史保留与对象引用标记

- **环境与前置**：L；S040、S029。
- **只读设计范围**：`docs/design.md` §19.4、C07；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/gc/mark.rs。
- **实现要点**：实现当前/历史30天/回收站/未完成操作/上传下载和备份租约的引用枚举，时间注入；过期依据 superseded/delete 时间，不按对象文件 mtime。
- **本步验收**：当前老文件永不因年龄过期；删除前的保留窗口正确；active lease 被标记保护。
- **交接**：`agent/tasks/S085.md`，记录实际接口、验证与下一个 ready 任务。

### S086 — GC 删除协调与临时清理

- **环境与前置**：L；S085、S037。
- **只读设计范围**：`docs/design.md` §19.4、T24；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/gc/sweep.rs。
- **实现要点**：标记后宽限、重新核对引用、与发布协调再删；过期未完成上传独立清理。只清可重建临时内容，不清客户端 dirty/完整缓存；磁盘不足不缩短历史。
- **本步验收**：提交和 GC 交错不删新引用对象；崩溃重启可继续；满盘状态不触发越界回收。
- **交接**：`agent/tasks/S086.md`，记录实际接口、验证与下一个 ready 任务。

### S087 — 一致性备份快照与租约

- **环境与前置**：L；S086、S048。
- **只读设计范围**：`docs/design.md` §19.1、T24；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/backup/prepare.rs。
- **实现要点**：暂停本服务新提交/GC并收敛意图，选择每 Vault seq，Online Backup API 导出 DB、建立完整对象清单和租约；按 §19.1 建立无嵌套子卷的备份准备区，克隆/复制清单对象并创建只读快照后恢复提交/GC，租约继续生效。覆盖准备失败、重启遗留清理与最小快照权限；不赋予服务任意挂载/格式化权限。声明外部未采集写入不在快照保证内。
- **本步验收**：持续外部写入时仍得到已确认一致快照；租约期间对象不被 GC；失败不会标 complete。
- **交接**：`agent/tasks/S087.md`，记录实际接口、验证与下一个 ready 任务。

### S088 — 备份导出、校验与释放接口

- **环境与前置**：L；S087、S075。
- **只读设计范围**：`docs/design.md` §19.1、§15.2；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/backup/export.rs；apps/server-admin/；api/openapi.yaml。
- **实现要点**：实现 manifest/对象授权读取、流式导出、verify/release；包含配置/schema/Vault 映射/历史回收站，认证材料受限且会话默认失效。CLI 按 prepare/export/verify/release 分步调用。
- **本步验收**：缺对象/错 hash 标 incomplete；快照释放中断可恢复且不误删对象；完整导出可逐项校验；不内置定时备份。
- **交接**：`agent/tasks/S088.md`，记录实际接口、验证与下一个 ready 任务。

### S089 — 离线恢复到空目录

- **环境与前置**：L；S088、S037。
- **只读设计范围**：`docs/design.md` §19.2、T16/T32；加直接前置交接及相关源文件。
- **主要落点**：crates/server-core/src/backup/restore.rs；apps/server-admin/。
- **实现要点**：restore validate 检查清单、空间、兼容和新 Vault 映射；停止服务并持同一独占锁才可恢复。重建 DB/普通工作文件后校验，沿用 ServerId、更换 epoch、会话失效；复用 core 而非直接任意 SQL。
- **本步验收**：恢复空测试实例字节/历史/回收站一致；非空目标和在线实例拒绝；旧客户端操作被隔离。
- **交接**：`agent/tasks/S089.md`，记录实际接口、验证与下一个 ready 任务。

### S090 — Web 备份管理入口

- **环境与前置**：B；S079、S088。
- **只读设计范围**：`docs/design.md` §18.1、§19.1；加直接前置交接及相关源文件。
- **主要落点**：apps/web/src/features/backup/。
- **实现要点**：呈现 prepare/进度/清单/完整性/释放，支持安全导出入口。离线 restore 由 CLI 执行，页面说明停止服务与 restore validate 流程，不在运行中调用恢复写库。
- **本步验收**：页面可完成一致备份并验证；下载缺失可重试；不会显示未完成包为有效备份。
- **交接**：`agent/tasks/S090.md`，记录实际接口、验证与下一个 ready 任务。

### S091 — 协议与数据库升级演练

- **环境与前置**：X；S089、S074。
- **只读设计范围**：`docs/design.md` §19.3、T33；加直接前置交接及相关源文件。
- **主要落点**：tests/contract/；tests/server-integration/；tests/windows-integration/。
- **实现要点**：固定已知旧 schema/协议 fixture，验证 major 拒绝、minor/capabilities、两端迁移备份和失败暂停；旧程序看到新 schema 拒绝写。回退使用完整一致备份。
- **本步验收**：失败后原数据可恢复；升级后历史和队列不丢；没有可用旧正式客户端时注明 fixture 覆盖范围。
- **交接**：`agent/tasks/S091.md`，记录实际接口、验证与下一个 ready 任务。

### S092 — 构建依赖与协议边界检查

- **环境与前置**：L；S024、S025、S080。
- **只读设计范围**：`docs/design.md` §5.5—5.8；加直接前置交接及相关源文件。
- **主要落点**：tools/xtask/src/；tests/contract/。
- **实现要点**：实现 check-boundaries：server 无客户端/FUSE/Tauri依赖，GUI/CLI 不直写 DB，生成代码可重现。按 target 检查 Cargo metadata，pnpm 消费者也参加契约检查。
- **本步验收**：故意引入违规依赖或手改 generated 能检查失败；Linux 不盲跑全 workspace all-features。
- **交接**：`agent/tasks/S092.md`，记录实际接口、验证与下一个 ready 任务。

### S093 — 发布版本与产物清单

- **环境与前置**：L；S052、S076、S092。
- **只读设计范围**：`docs/design.md` §5.7.1；加直接前置交接及相关源文件。
- **主要落点**：tools/xtask/src/；packaging/。
- **实现要点**：统一产品版本来源并同步 package manifests，协议/schema 独立；记录提交、目标、工具链、锁文件摘要、hash。仅使用同一提交或明确验证过的组合，拒绝陈旧前端资源。
- **本步验收**：组装清单能追溯所有输入；修改版本/资源后旧产物不能静默被复用。
- **交接**：`agent/tasks/S093.md`，记录实际接口、验证与下一个 ready 任务。

### S094 — Linux 服务端 Nix 构建包

- **环境与前置**：L；S093、S090。
- **只读设计范围**：`docs/design.md` §5.9、§17.2；加直接前置交接及相关源文件。
- **主要落点**：nix/package.nix；flake.nix；packaging/linux/。
- **实现要点**：分别构建 Rust server/admin 与 Web 静态资源，再组装发布目录；Nix derivation 使用固定依赖输入/哈希和离线构建，不以 fake hash 当最终值。Web 路径显式配置，缺资源报错。
- **本步验收**：干净 nix build 得到可运行 server/admin/web；运行时无需 Node；源码构建单测仍不依赖前端构建。
- **交接**：`agent/tasks/S094.md`，记录实际接口、验证与下一个 ready 任务。

### S095 — NixOS 服务模块

- **环境与前置**：L；S094、S007。
- **只读设计范围**：`docs/design.md` §17.2、§5.10；加直接前置交接及相关源文件。
- **主要落点**：nix/modules/vaultsyncd.nix；deploy/nixos/。
- **实现要点**：导出 services.vaultsyncd 的 enable/package/stateDir/settings 等选项；专用非 vault 用户、工作目录访问白名单、StateDirectory 和必要写路径。外部 Vault 目录权限显式授予，不递归改用户现有权限。核对 btrfs 挂载依赖、CoW、子卷布局和最小快照权限；不自动格式化或迁移已有目录。
- **本步验收**：nixos 配置 evaluation/build 成功；服务用户能访问样本 vault，无法任意写系统目录；凭据不进入 store。
- **交接**：`agent/tasks/S095.md`，记录实际接口、验证与下一个 ready 任务。

### S096 — 在本机启用隔离服务实例

- **环境与前置**：L；S095、S042。
- **只读设计范围**：`docs/design.md` §17.2、本文§2.5；加直接前置交接及相关源文件。
- **主要落点**：主机既有 NixOS 配置；docs/operations/nixos.md。
- **实现要点**：接入模块与开发/试用独立端口/状态，保存系统配置 diff 和旧 generation；按现场入口 build/test/健康检查/switch。使用 CLI 安全初始化 owner，试用样本不指向真实文档。
- **本步验收**：重启服务后样本闭环成功；SSH/既有服务正常；记录回滚命令和配置来源，无需再次询问 rebuild 权限。
- **交接**：`agent/tasks/S096.md`，记录实际接口、验证与下一个 ready 任务。

### S097 — HTTPS 与同源 Web 部署

- **环境与前置**：L；S096、S077。
- **只读设计范围**：`docs/design.md` §17.2、C28/C29；加直接前置交接及相关源文件。
- **主要落点**：deploy/reverse-proxy/；docs/operations/nixos.md。
- **实现要点**：使用已有或新最小反向代理，TLS 入口与后端回环绑定；Web 与 API 同源，SSE/上传限制正确。开发可用显式信任的私有 CA，公网域名/证书缺失则记录所缺信息。
- **本步验收**：浏览器与 Windows 经 HTTPS 登录/上传/SSE；错误证书失败；代理不意外暴露管理 socket 或任意路径。
- **交接**：`agent/tasks/S097.md`，记录实际接口、验证与下一个 ready 任务。

### S098 — 通用 Linux 包与容器配置

- **环境与前置**：L；S094、S097。
- **只读设计范围**：`docs/design.md` §5.9、C35/C39；加直接前置交接及相关源文件。
- **主要落点**：packaging/linux/systemd/；packaging/linux/container/；deploy/compose/。
- **实现要点**：提供普通 Linux systemd 和容器构建，运行状态/Vault bind mount 到合规后端；Nix 构建产物不可未经处理直接当 Ubuntu 可运行包，使用经验证的通用构建/依赖封装。无 /dev/fuse。
- **本步验收**：Ubuntu 环境实际运行 server/admin/web；容器默认 overlay 状态被拒绝或用合规 bind 挂载；仅完成交叉构建不算运行通过。
- **交接**：`agent/tasks/S098.md`，记录实际接口、验证与下一个 ready 任务。

### S099 — Windows 安装包与后台启动

- **环境与前置**：W；S083、S084、S093。
- **只读设计范围**：`docs/design.md` §5.9、§19.3；加直接前置交接及相关源文件。
- **主要落点**：packaging/windows/；apps/desktop/src-tauri/。
- **实现要点**：安装包包含 GUI/agent/CLI 和 WebView2 条件；用户会话启动 agent，GUI 关闭不停止它。程序目录与 LocalAppData 状态分离，路径带空格/中文可用。签名资料不存在时标为未签名测试产物。
- **本步验收**：干净 Windows 安装、登录后 agent 启动、CLI/GUI 同实例；重启后同步继续。
- **交接**：`agent/tasks/S099.md`，记录实际接口、验证与下一个 ready 任务。

### S100 — Windows 升级与卸载保护

- **环境与前置**：W；S099、S091。
- **只读设计范围**：`docs/design.md` §19.3、T30/T33；加直接前置交接及相关源文件。
- **主要落点**：packaging/windows/；tests/windows-integration/。
- **实现要点**：升级先停新任务保全内容，再迁移/替换程序；卸载区分程序、完整内容、仅云端条目，默认不删用户数据/dirty。确保卸载 provider 后文件处置与 UI 提示一致。
- **本步验收**：带 dirty/占位/已下载样本升级/卸载演练有 hash；失败可恢复，不因卸载留下被误称为完整文件的假内容。
- **交接**：`agent/tasks/S100.md`，记录实际接口、验证与下一个 ready 任务。

### S101 — 本地平台矩阵与安装验证

- **环境与前置**：X；S092、S098、S100。
- **只读设计范围**：`docs/design.md` §5.8、T36；加直接前置交接及相关源文件。
- **主要落点**：tools/ci/；tools/xtask/。
- **实现要点**：接通 Linux core/server、宿主交叉构建及 Windows 原生运行、前端、契约、e2e/release 本地命令；按 design §5.8 检查消费者。Linux x64 与 Windows x64 按 ADR-015 执行；缺本地实验资源则明确 blocked（S155/ADR-024取消远端执行要求）。
- **本步验收**：同一提交两端构建安装；Linux x64 启动/存储/恢复实测；Windows CFAPI 非 fake；NixOS 与 Ubuntu 证据分列。
- **交接**：`agent/tasks/S101.md`，记录实际接口、验证与下一个 ready 任务。

### S102 — 事件异常与旧设备集成验收

- **环境与前置**：X；S074、S089、S091。
- **只读设计范围**：`docs/design.md` T14/T15/T16；加直接前置交接及相关源文件。
- **主要落点**：tests/e2e/；tests/faults/。
- **实现要点**：分别注入 SSE 丢重乱、游标过期、历史过期、恢复旧服务器备份；保留旧 baseline/脏内容验证重建流程。通过可控时钟/小保留配置缩短测试，不改产品默认要求。
- **本步验收**：无清洁旧文件复活、无脏内容丢失、无旧 epoch Delete 自动重放；报告逐场景判据。
- **交接**：`agent/tasks/S102.md`，记录实际接口、验证与下一个 ready 任务。

### S103 — 文件与挂载故障集成验收

- **环境与前置**：X；S074、S051、S068。
- **只读设计范围**：`docs/design.md` T17—T21/T25—T28/T35；加直接前置交接及相关源文件。
- **主要落点**：tests/server-integration/；tests/windows-integration/。
- **实现要点**：在专用卷/客体分开运行外部写竞争、卸载、溢出、独立 btrfs 卷的数据/元数据 ENOSPC、CoW 分配失败、卷/子卷替换、symlink/reparse 和编辑器保存矩阵。一个会话只执行一类故障，必要时拆子卡。
- **本步验收**：每类结果保留内容和明确错误；外部瞬态保证不夸大；真实用户目录未参与故障注入。
- **交接**：`agent/tasks/S103.md`，记录实际接口、验证与下一个 ready 任务。

### S104 — GC、备份与崩溃联合验收

- **环境与前置**：X；S086、S089、S102。
- **只读设计范围**：`docs/design.md` T22/T23/T24/T32/T33；加直接前置交接及相关源文件。
- **主要落点**：tests/e2e/；tests/faults/。
- **实现要点**：组合活跃下载、提交、备份租约与 GC，逐个提交持久化点强杀；恢复到空实例再比对所有引用对象。覆盖只读快照准备/释放中断和共享 extent 仍被快照持有的空间统计；保存最小失败轨迹，避免一次运行所有排列。
- **本步验收**：无仍被引用对象缺失；backup complete 可实际恢复；中断结果可解释且幂等。
- **交接**：`agent/tasks/S104.md`，记录实际接口、验证与下一个 ready 任务。

### S105 — 安全与操作可达性验收

- **环境与前置**：X；S083、S090、S097、S100。
- **只读设计范围**：`docs/design.md` T06—T08/T30/T31；加直接前置交接及相关源文件。
- **主要落点**：tests/contract/；tests/e2e/。
- **实现要点**：检查每项 P0 操作的 GUI/CLI/Web 入口；越权版本/备份访问、恶意 IPC/WebView、会话撤销、CSRF、日志脱敏。覆盖释放与解绑的最后时刻状态变化。
- **本步验收**：接口实际拒绝非法行为；所有必需操作能通过真实入口完成，UI 禁用不代替后端校验。
- **交接**：`agent/tasks/S105.md`，记录实际接口、验证与下一个 ready 任务。

### S106 — 小文件时延与常规规模基准

- **环境与前置**：X；S101、S102、S103、S104、S105。
- **只读设计范围**：`docs/design.md` §20、T34；加直接前置交接及相关源文件。
- **主要落点**：tools/bench/；docs/reports/performance.md。
- **实现要点**：先小样本测稳定保存→服务端确认→另一客户端可见各阶段；再1万文件/约10GiB/3客户端。记录硬件/网络/配置、p95、RSS/CPU/队列，调参每次只改一个变量。
- **本步验收**：给出真实数据与瓶颈；满足定义条件才判断1—3秒目标；无误报已同步。
- **交接**：`agent/tasks/S106.md`，记录实际接口、验证与下一个 ready 任务。

### S107 — 正式容量与大文件续传验证

- **环境与前置**：X；S106。
- **只读设计范围**：`docs/design.md` C36、§20、T34；加直接前置交接及相关源文件。
- **主要落点**：tools/bench/；docs/reports/performance.md。
- **实现要点**：先估算完整副本+对象+历史+staging容量，资源够才运行10万文件/1TiB/50GiB/5 Windows设备验收。用确定性流式生成内容，不以稀疏空洞或元数据伪装真实传输。
- **本步验收**：实际规模与断点重传字节被记录；不足则 blocked 并报告差距，不能下调目标后标通过。
- **交接**：`agent/tasks/S107.md`，记录实际接口、验证与下一个 ready 任务。

### S108 — 发布清单与最终交接

- **环境与前置**：X；S101、S102、S103、S104、S105、S106、S107。
- **只读设计范围**：`docs/design.md` §21、§25、附录B；加直接前置交接及相关源文件。
- **主要落点**：agent/progress.md；docs/operations/；docs/known-limitations.md；README.md。
- **实现要点**：汇总 T01—T36 中35项P0，T29明确P1；逐项关联真实证据和候选提交。整理NixOS部署、Windows安装、备份恢复、升级、故障排查与限制；版本产物带hash，留给用户可复核。
- **本步验收**：无 blocked P0 才声明首发验收完成；未签名/未测平台/容量欠缺如实列出；不自动发布公网或推送远端。
- **交接**：`agent/tasks/S108.md`，记录实际接口、验证与下一个 ready 任务。

### S109 — Windows 免安装 ZIP（状态留本机）

- **来源与前置**：用户在本轮明确确认新增 ZIP 分发，优先于原卡序；W；S099、S100 已 done。本轮只执行此卡，不变更 S101/S106—S108 的阻塞结论。
- **只读设计范围**：`docs/design.md` §5.9、§19.3、§24；S099/S100 短交接及启动、IPC、维护和发布相关源码。
- **主要落点**：`packaging/windows/`、`apps/agent/`、`tools/xtask/src/release/` 及 Windows 原生验收。
- **实现要点**：同源 ZIP 解压启动，GUI/agent/CLI 配套；配置、状态、缓存及凭据继续留当前 Windows 用户，不做随盘跨机迁移。默认不创建登录自启/快捷方式，不安装服务；GUI 关闭后 agent 可继续同步，另提供明确安全停止入口。检查 WebView2、中文空格路径与真实 IPC；不绕过迁移备份、维护屏障及未确认内容保护。不得承诺 CFAPI 零注册或删除程序等于安全停用。
- **本步验收**：发布来源/hash、ZIP 集合与篡改拒绝；真实 Windows 解压启动、GUI/CLI 同实例、关 GUI 后 agent 存活/同步、安全停止和重启状态不丢、默认无 Run/快捷方式、路径带中文空格；已有安装或不同来源的状态不得混跑。新增包未签名必须明确标记；缺平台证据标 blocked。
- **交接**：`agent/tasks/S109.md`；记录实际 worktree、基线、产出、验证、包路径/hash、限制及恢复条件。

### S110 — 一键构建多平台 release 归档

- **来源与前置**：用户要求统一一键编译各支持平台 release，Windows 只要 ZIP；L；S093、S098、S109 已 done，可独立执行，不改变原发布门禁。
- **只读设计范围**：design §5.7.1、§5.9、§5.10、§24；直接依赖短交接及既有平台构建器。
- **主要落点**：`tools/release.sh`、`packaging/release.py`、`tools/xtask/src/release/`。
- **实现要点**：默认 Linux x64 服务端含 Web（原 ARM64 范围已由 S111/ADR-015 移除）、Windows x64 客户端 ZIP；自动进入锁定环境和准备依赖。按版本/提交归档到忽略目录，记录来源/哈希/日志，拒绝覆盖与混源；失败保留现场。沿用 ZIP 的现有启动/状态语义，不更改产品平台范围。
- **本步验收**：真实一键构建当前两个 x64 目标（S110 历史为三个目标，S111 更新平台范围），核对同源收据与 SHA256SUMS、Windows 无安装器；验证失败/脏源码/重复输出拒绝，现有 ZIP 与受影响 xtask 检查通过。构建证据不冒充 Windows/Ubuntu 原生验收。
- **交接**：`agent/tasks/S110.md`；记录源码与产物路径、实际验证、worktree/提交/合并清理；原 blocked 卡保持。

### S111 — 移除 Linux ARM64 构建与验证范围

- **来源与前置**：2026-11-07 用户确认 Linux ARM64 不再考虑，包含脚本、代码与测试；S110 已 done，本卡 ready，可独立执行。
- **设计范围**：design §2.1、§24（ADR-015）；保留 Linux x64 服务端及 Windows x64 客户端，历史证据不重写。
- **实现**：S111-a 移除 release/Nix/容器/ABI 的 ARM64 路径；S111-b 移除 CI/客体验收与相关测试，统一当前文档；同卡顺序执行，不启动其他卡。
- **验收**：release 默认与 all 只含两个 x64 目标，旧 ARM64 选项明确拒绝；Nix 输出、CI 目录和执行控制器不再依赖 ARM64、QEMU/binfmt/ARM模板；保留 x64 失败/来源/隔离校验，相关契约与 Linux 平台测试、xtask/fmt/clippy 通过，Nix 求值通过。不要求本卡重新发行或代替 Windows 原生验收。
- **交接**：`agent/tasks/S111.md`；记录实际命令、限制、提交、master 集成及工作树清理。

## 6. 里程碑与覆盖关系

下列范围是交付分组，不改变任务卡依赖；Windows 原型早期缺失时可以推进 Linux 工作，但 Windows 正式适配与发布不得绕过原型验证。

| 阶段 | 任务 | 可观察结果 |
| --- | --- | --- |
| 环境和平台可行性 | S001—S014 | 可复现 Nix 工具链、隔离测试卷、Windows 原生原型 |
| 纯模型和持久化 | S015—S031 | 确定性规划、两套数据库、可生成契约 |
| Linux 同步服务 | S032—S052 | 普通文件、不可变版本、可恢复提交及外部采集的 HTTP 闭环 |
| 客户端通用核心 | S053—S064 | 可恢复队列、三方同步、模式/释放/状态逻辑 |
| Windows 客户端 | S065—S074 | 真实 CFAPI 与多客户端同步 |
| 管理与交互 | S075—S084 | Linux 管理 CLI/Web 与 Windows GUI/CLI |
| 生命周期与数据保护 | S085—S091 | GC、完整备份恢复、迁移与兼容证据 |
| 部署发布构建 | S092—S101 | NixOS 本机服务、通用 Linux 包、Windows 安装/升级 |
| 综合验收 | S102—S108 | 故障、容量与完整发布证据 |

### 6.1 与 design 中 DEV 任务的对应

| 设计任务 | 本手册主要步骤 |
| --- | --- |
| DEV-001 | S001—S009、S024—S025、S092 |
| DEV-002 | S010—S011 |
| DEV-003 | S012、S056、S059、S068 |
| DEV-004 | S007、S013—S014、S032 |
| DEV-005 | S015—S023 |
| DEV-006 | S024—S031；契约随后随功能补全 |
| DEV-007 | S033—S040 |
| DEV-008 | S042—S048、S053—S054 |
| DEV-009 | S055—S064 |
| DEV-010 | S038—S039、S049—S051、S060 |
| DEV-011 | S040、S085—S091 |
| DEV-012 | S065—S070 |
| DEV-013 | S071—S083、S090 |
| DEV-014 | S084、S092—S108 |

### 6.2 验收编号的最小证据归属

任务卡中的测试是局部证明；下面的归属要求最终存在真实集成证据，不能用规划器测试替代 Windows/磁盘恢复实测。

| 设计验收 | 主要实现/验收步骤 |
| --- | --- |
| T01 | S020、S058、S074 |
| T02 | S021、S036、S074 |
| T03 | S036、S047、S052、S074 |
| T04 | S056、S068、S074 |
| T05 | S059、S074、S084 |
| T06 | S011、S067、S105 |
| T07 | S063、S069、S084、S105 |
| T08 | S062、S069、S105 |
| T09 | S038、S066、S074 |
| T10 | S022、S038、S074 |
| T11 | S039、S074 |
| T12 | S039、S074 |
| T13 | S040、S079、S074 |
| T14 | S048、S057、S102 |
| T15 | S060、S102 |
| T16 | S060、S089、S102 |
| T17 | S049—S051、S103 |
| T18 | S014、S049、S103 |
| T19 | S051、S103 |
| T20 | S050、S103 |
| T21 | S052、S103 |
| T22 | S046、S054、S067、S104 |
| T23 | S037、S059、S104 |
| T24 | S086—S088、S104 |
| T25 | S032、S065、S103 |
| T26 | S017、S065、S103 |
| T27 | S012、S050、S068、S103 |
| T28 | S012、S068、S084、S103 |
| T29 | P1，不执行、不计入首发已通过项 |
| T30 | S061、S069、S082、S100、S105 |
| T31 | S043、S071、S080、S105 |
| T32 | S089、S104 |
| T33 | S026、S091、S100、S104 |
| T34 | S106—S107 |
| T35 | S013、S051、S103 |
| T36 | S098—S101、S108 |

### S112 — 本机域名部署与 NixOS feature 配置

- **依据**：2026-11-07 用户要求将服务端部署至 `vsyncd.132140.xyz`，通过 `/etc/nixos` 的 `features.server.vaultsync.*` 启停与配置；design §11.5、§17.2、§19.3。
- **前置**：S095、S096、S097、S111 均 done；不依赖规模测试，不能据此宣称首发验收。
- **实现**：复用 flake 包与服务模块；导出可交给 Caddy 自动 ACME 的同源代理；主机 feature 固定永久提交、专用账号与 btrfs 存储，保留旧试用数据、凭据及恢复 generation。
- **本步验收**：Nix 求值、当前包构建与包内测试，实际主机 build → dry-activate → test → HTTPS/认证/上传 committed/固定版本下载/重启与备份健康 → switch；可信域名证书与续期配置，核对 SSH/既有应用及锁未改变。仅报告实际外网观测能力。
- **交接**：`agent/tasks/S112.md`，记录两仓库提交、配置/凭据路径、回滚、限制和工作树清理。

### S113 — 主机用户运行与 pure 一键部署

- **依据**：2026-11-07 用户要求本机服务默认使用 `/etc/nixos` 的主机 user，目录同属该用户；允许清空旧数据，版本固定 Git SHA，更新后一键 pure rebuild 部署。
- **前置**：S112 done；独立于性能验收。
- **实现**：模块支持既有非 vault 账号且不改写其属性；主机默认 `host.user` 和其主组；永久仓库完整 SHA 固定。`tools/deploy-local.sh` 在隔离主机配置树中更新版本，强制 pure、禁止锁更新，build/dry/test/健康/switch 后提交版本配置，保留失败现场与恢复入口。
- **验收**：专用及普通用户模块求值/构建，部署失败关闭契约；实际主机 pure build → test → HTTPS/同 UID IPC/目录归属/上传下载/重启/备份 → switch；锁及 SSH/既有应用健康保持，真实运行一键脚本。
- **交接**：`agent/tasks/S113.md` 与批次记录，两仓库提交、旧 system、实际 UID/路径、更新/恢复命令及清理结果；不改变原首发门禁。

### S114 — 客户端目录选择与 Vault 名称显示

- **依据**：2026-11-07 用户要求本地目录通过选择器选择、远端 Vault 显示目录名；design §15.1、§16.1、§18.1、§19.3，ADR-016。
- **前置**：S081、S111 done；独立于性能与部署卡，本轮新增 ready。
- **实现**：S114-a 接入仅内置窗口可调用的 Windows 原生单目录选择器，只读展示已选绝对路径，取消保留原选择；S114-b 从 OpenAPI 可选名称字段生成 DTO，服务端仅返回末级目录名，agent 传递名称，UUID 继续用于身份。
- **验收**：前端类型/构建、生成一致性、Vault 名称与旧响应兼容及身份/授权/路径隔离回归、Windows x64 交叉构建与真实 WebView/选择器选择及取消；不以 fake 或交叉编译替代原生证据。
- **交接**：`agent/tasks/S114.md`；记录兼容行为、实际验证、工作树/提交/整合清理及未解决限制。原首发门禁不变。

### S115 — 服务端 Web 的 HeroUI React 完整重构

- 依据：2026-11-08 用户要求；design §18.1、§17.1、§19、§24；前置 S077、S078、S079、S090、S112 已完成。
- 结果：保持五个管理入口与现有 API/认证/恢复/备份语义，采用 HeroUI v3、Tailwind v4 与 React 重构 Web；共享桌面 UI 不迁移。
- 子卡：S115-a 基础依赖、Web 专用组件、主题与响应式导航/概览；S115-b 账号、会话与 Vault 管理；S115-c 历史、冲突与回收站；S115-d 备份与整体验收。子卡只细化本卡，不减少原验收目标。
- 执行：subagent 分别负责子卡，最多同时运行 3 个；基础公共接口先约定，页面可并行，依赖锁与公共 UI 只有一个写入者，主 agent 集成、验证、提交及整合。
- 验证：Web 原有测试/生产构建、根工作区 typecheck；真实 Chromium 账号/Vault/恢复/备份流程与桌面初始渲染回归；移动布局、主题、键盘和弹层焦点验证；更新并验证 Nix pnpm 固定依赖与 Web 离线构建。
- 边界：不修改 wire DTO/数据库/同步状态机，不将 pending 或未知显示为成功；不访问正式数据、不默认部署或推送。不替代既有规模/Windows 首发门禁。
- 交接：`agent/tasks/S115.md`；拆卡与批次原文见 `agent/tasks/README.md` 的固定 Git 归档。

### S116 — 将当前服务端部署到本机

- 依据：2026-11-08 用户要求“部署服务端到本机”；design §17.2、§11.5；前置 S113、S114、S115 done。
- 结果：通过现有一键 pure 部署入口将当前已提交版本固定到 `/etc/nixos#wks-lab`，保留现有域名、账号、ServerId、VaultId 与正式数据，不更新主机依赖。
- 验证：部署契约、固定源 manifest 与包内检查、build/dry-activate/test/HTTPS/IPC/身份/UID及目录归属/switch；真实 Chromium 新版页面/登录/Vault 健康/TLS/CSP/SSE，以及既有服务、SSH、应用与主机锁核对。
- 交接：`agent/tasks/S116.md`；记录两仓库提交、generation、证据、恢复入口与清理。仅部署本机，不改变 S101/S106—108 与 T02 首发门禁。

### S117 — Web 管理界面紧凑化与 HeroUI 控件统一

- 依据：2026-11-08 用户要求美化服务端 Web，尽量采用现成 HeroUI 控件、紧凑布局和较小圆角；design §18.1、§17.1、§19.1、ADR-017；前置 S115 done。S116 部署独立进行，本卡不接管。
- 结果：保持五个管理入口与业务语义，统一导航、面包屑、按钮、表格、表单与弹层的密度；优先使用锁定 HeroUI 3.2.7 实际提供的组件，不为布局升级依赖。
- 分工：主 agent 负责壳、概览、公共控件及主题；subagent 负责四个业务页面的控件/间距收紧；只执行父卡 S117。
- 验收：Web 原有测试、生产构建与根 typecheck；真实 Chromium 五路由、明暗主题、窄屏、键盘、紧凑尺寸与生产 CSP；受影响账号/Vault/恢复/备份真实隔离服务浏览器流程。
- 边界：不改协议、数据库与状态机，不接触正式数据或默认部署。交接 `agent/tasks/S117.md` 与批次记录，验收后本地提交并整合 master。

### S118 — 将最新服务端（S117 之后）部署到本机

- 依据：2026-11-08 用户要求“将服务端最新版部署到本机”；design §17.2、§11.5；前置 S116、S117 done。
- 结果：用现有一键 pure 入口把部署时 master HEAD 固定到 `/etc/nixos#wks-lab`，使 S115/S117 上线；保留域名、账号、ServerId、VaultId、正式数据与主机依赖锁，不改产品代码与协议。用户要求的 dae 分流规则先单独提交并随本次部署激活。
- 验收：clean/pure 构建/manifest/主机锁/dry-activate/test/HTTPS/IPC/身份/UID与归属/switch；线上静态文件与包内 sha256、真实 Chromium TLS/CSP/五路由响应式/登录 cookie/Vault 健康/SSE；dae 重启后的服务状态与出网，以及既有服务、SSH、应用与主机锁核对。
- 交接：`agent/tasks/S118.md`；记录两仓库提交、generation、证据、恢复入口、记录偏差与清理。仅部署本机，不改变 S101/S106—108 与 T02 首发门禁。

### S119 — 服务端 Web 从 HeroUI 迁移到 Mantine

- 依据：2026-11-08 用户要求移除 HeroUI、用 Mantine 默认主题与 AppShell 重实现服务端 Web，尽可能采用现成组件，保留 Tailwind 作少量自定义样式，并先计划、由 subagent 完成各子任务。前置 S117、S118 done；design §18.1、§17.1、§19.1、§24。
- 结果：五个管理入口全部采用 Mantine，删除 HeroUI 及专用依赖/样式，保留 React、生成 API 与既有业务语义。Windows 桌面共享 UI 不在本卡范围。
- 分工：S119-a 基础依赖、默认主题、AppShell/导航/概览及现成组件组合；S119-b 账号、Vault、恢复、备份页面与 Mantine 表单；S119-c CSP、测试、真实浏览器与 Nix 离线构建。三个 subagent 在同一隔离树内按文件边界合作；主 agent 负责计划、ADR/接口、审查、交接与整合，只执行父卡 S119。
- 验收：原 Web 测试、生产构建、根 typecheck、Nix Web 离线构建；必要服务端测试/fmt/clippy；真实 Chromium 五路由、明暗主题、320/390/768/1440/1920 响应式、键盘/弹层与生产 CSP，以及真实隔离 btrfs 服务的账号/Vault/恢复/备份流程。测试适配组件 DOM，不删减业务断言。
- 边界：不改 wire DTO、数据库或状态机，不接触正式数据，不默认部署。用户最新决定由 ADR-018 取代 ADR-017 的组件选择；CSP 不增加 unsafe-inline。交接 `agent/tasks/S119.md` 及三个子卡，验收提交后合入 master 并清理本地任务树/分支。

### S120 — 将 Mantine 版最新服务端部署到本机

- 依据：2026-11-08 用户再次要求“将服务端最新版部署到本机”；前置 S118、S119 done；design §17.2、§11.5、§18.1。
- 结果：用现有一键 pure 入口把部署时 master HEAD 固定到 `/etc/nixos#wks-lab`，使 S119 的 Mantine Web 与逐响应 style nonce CSP 上线；保留域名、账号、ServerId、VaultId、正式数据与主机依赖锁。
- 验收：clean/pure 构建/manifest/主机锁/dry-activate/test/HTTPS/IPC/身份/UID与归属/switch；线上 HTML（剥离 nonce 后）与主资产 sha256、CSP nonce 与无 unsafe-inline、真实 Chromium 五路由含 Burger 响应式/主题/登录 cookie/Vault 健康/SSE；既有服务、SSH、应用与主机锁核对。
- 交接：`agent/tasks/S120.md`；记录两仓库提交、generation、证据、恢复入口与清理。仅部署本机，不改变 S101/S106—108 与 T02 首发门禁。

### S121 — Web 顶栏分区与 48px 高度并部署到本机

- 依据：2026-11-08 用户要求 topbar 降至 48px、面包屑居左、Logo 独立分区且边线与左栏对齐，参考 Mantine UI navbars；追加要求完成后部署本机。前置 S119、S120 done；design §18.1、§17.2。
- 结果：组合现有 AppShell 与 Group 调整布局，保留窄屏折叠菜单、主题和五路由；通过现有 pure 一键部署入口上线，保留账号/数据/身份及主机依赖锁。
- 验收：Web test/build、根 typecheck；真实 Chromium 五路由明暗主题与 320/390/768/1440/1920 宽度，实测 48px、面包屑左对齐、Logo/侧栏分隔线、无横向溢出与导航可用；部署 build/dry/test/健康/switch、manifest/资产一致、生产 CSP/TLS/登录/Vault/SSE 与既有服务核对。
- 交接：`agent/tasks/S121.md` 及批次；只涉及布局与本机版本 pin，不改变协议/数据库或首发门禁。

### S122 — Web Vault 文件浏览器与属性查看

- 依据：2026-11-08 用户要求服务端 Web 浏览某个 Vault 的文件目录、查看基本属性，并在文件管理器工具栏切换 Vault；前置 S048、S078、S119、S121 done。design §15.1—15.2、§18.1、ADR-019。
- 结果：Mantine 文件浏览器路由，工具栏 Vault 选择、目录面包屑与返回上级、刷新、当前目录名称筛选及列表分页；文件/目录属性展示名称、类型、相对路径、大小、身份、revision、内容版本与 SHA-256。不提供内容预览或写入操作。
- 实现边界：复用授权 Vault 与固定快照 API，完整读取分页后构造目录树；取消与过期/中途错误不当成空目录，切换 Vault 丢弃旧响应；单 Vault 最多 100,000 条快照元数据，每页展示 100 子项，超限明确报错。不新增 wire/持久格式，不把未采集或 pending 当 committed。
- 验收：Web test/build、根 typecheck、受影响 api-client 测试与 Nix Web 构建；Chromium 连接独立 btrfs 服务，验证多 Vault 切换、嵌套/空目录、返回/面包屑、属性/精确大小/特殊文件名、筛选/分页/刷新、断网/失效/切换竞态、明暗/窄屏/CSP。单测验证分页不一致/坏游标/坏树/中途失败/取消。
- 交接：`agent/tasks/S122.md` 及批次。部署不在本卡范围；既有首发与 Windows 门禁保持。

### S124 — Web 文件浏览器左右分栏与 URL 目录定位

- 依据：2026-11-08 用户要求重构文件浏览器；前置 S122、S123 done；design §18.1、ADR-019。
- 结果：左侧 Vault 选择与目录树，右侧顶部刷新/前进/后退/上级/相对地址栏/筛选/属性工具栏，下方文件列表；移除 Vault 列表刷新按钮、页内重复标题和说明。路径 `#/files/<vault_id>/<逐段编码的相对目录>` 可直接打开、刷新与历史导航；不存在或无权目录明确报错，不回退成根目录。
- 规范：将 “Avoid unnecessary secondary labels and descriptions. Let the layout do the explaining.” 写入 design §18.1 和 agent 入口，适用于后续所有 UI 任务。
- 验收：Web test/build、根 typecheck、Nix Web 构建；真实 Chromium + 独立 btrfs 服务验证左树右列表、Vault 切换、URL 深链/重载/特殊名称/错误、地址栏与历史、筛选/分页/属性、网络失败/迟到响应、明暗/窄屏/CSP；保留 S122 只读边界。
- 交接：`agent/tasks/S124.md` 与批次。仅本卡，不默认部署，不改变协议/数据库或原首发门禁。

### S125 — 将最新服务端（S124 分栏文件浏览器）部署到本机

- 依据：2026-11-08 用户要求“部署服务端到本机，部署完不要验证”；前置 S113（一键部署入口）、S121（generation249）、S123（generation250）、S124 done。design §17.2、§11.5。
- 结果：用 S113 的一键入口把部署时 master HEAD 固定到本机，使 S124 的左树右列表文件浏览器与 URL 目录深链上线。只改主机版本 pin，不改产品代码、协议、schema 或数据库；域名、owner、正式数据、ServerId、VaultId 与运行用户保持。
- 范围：按用户明确要求，本轮不执行部署后验证（不跑 Chromium/浏览器矩阵、不比对线上资产哈希、不重铺服务 PID/应用/SSH/主机锁基线）。部署脚本自身的 clean 检查、pure 构建、manifest 与主机锁核对、dry-activate → test → HTTPS/IPC/身份/归属 → switch 门禁照常执行，失败即恢复旧 system。
- 交接：`agent/tasks/S125.md` 与批次。原 S106/S107 性能容量、S108 最终发行与 T02 Windows 门禁不变。

### S126 — 精简已完成任务的过程记录

- 依据：2026-11-08 用户要求精简或删除以后不会用到的过程文件；独立文档维护，无产品前置；design §24 的范围与数据语义保持。
- 结果：保留计划、当前进度、父卡短交接及全部 blocked 恢复条件；删除已完成拆卡/批次过程文件，用固定 Git 提交提供原文入口，清理失效引用、过时播报与重复状态行。
- 验证：核对被删除文件可逐字节从固定提交恢复；历史卡状态和 blocked 原文/状态行保持；本地链接、上下文提取及 diff 检查通过。保留用户未提交修改，不重跑或重认定产品验收，不清理 Git 外证据。
- 交接：`agent/tasks/S126.md`；纯文档在 master 原地维护；完成后无 ready 卡，沿用 blocked 恢复顺序。

### S127 — 使用 Mantine Tree 重构文件浏览器目录树

- 依据：2026-11-08 用户指定 https://mantine.dev/core/tree/；前置 S124 done；design §18.1。
- 结果：将目录树整理为独立 Mantine Tree/useTree 组件，统一节点、层级线、展开图标与选中样式；目录标签导航、箭头独立展开，空目录不显示展开按钮。保持 Vault、URL、历史、快照读取和只读边界。
- 验收：Web test/build、根 typecheck；真实 Chromium + 独立 btrfs 服务验证展开/收起不导航、空目录、选中与深链/历史同步、键盘与 Vault 隔离、明暗/窄屏/CSP，以及既有文件浏览器回归。
- 交接：`agent/tasks/S127.md`。仅重构目录树，不默认部署；原 blocked 恢复条件保持。

### S128 — 收紧文件浏览器目录树并部署生产

- 依据：2026-11-08 用户指定 Mantine files tree 示例，要求移除展开箭头、缩短折线与目录图标间距，完成后部署生产；前置 S127、S125 done；design §18.1、§17.2。
- 结果：仅用开合文件夹图标表示展开状态，保留图标独立展开、文字导航与原生键盘语义；空目录不留箭头占位。用现有 pure 入口将固定提交部署到本机生产域名。
- 验收：Web test/build、根 typecheck；隔离 btrfs Chromium 检查开合图标、4px 折线间距、14px 图标、6px 文字间距、键盘/深链/主题与窄屏；部署 build/dry/test/健康/switch 与线上资产一致性。
- 交接：`agent/tasks/S128.md` 与批次。只更新主机项目 pin，保留数据、身份、锁与现有首发门禁。

### S129 — 接通正式 Windows agent 的 Explorer 占位状态图标

- 依据：2026-11-08 用户要求 Explorer 显示未下载占位文件的空心云朵；前置 S066、S070、S072 done；design §14.2、§6。
- 结果：启动与新增绑定共同注册 WinRT 同步根，修复旧 CFAPI-only 根；在固定线程内管理 WinRT 生命周期，注册策略与适配器对齐。保留文件驻留、pin、同步状态与现有数据；失败明确返回，不注销根。
- 验收：相关 Rust 测试/fmt/clippy 与 Windows 交叉构建；真实 Windows 正式 agent 新增绑定、启动修复、重复重启、下载/释放状态切换，以及 Explorer 可见云朵截图；验证浏览不触发下载、退出保留注册和文件。
- 交接：`agent/tasks/S129.md`，记录隔离、实际命令、原生证据、限制和 master 整合/清理。

### S134 — 服务端 Web 统一 Mantine Notification 操作反馈

- 依据：用户指定 https://mantine.dev/core/notification/，所有服务端 Web feedback（含复制路径成功）统一使用 Mantine Notification；前置 S132、S133 done；design §18.1、§6。
- 结果：全局 Mantine 通知区与统一反馈入口，覆盖账号、连接检查、Vault、文件浏览、恢复、备份的成功/失败与校验反馈；持续状态、风险说明、操作记录保留原位，不将 pending 当 committed 或下载当验证。
- 验收：根 typecheck、Web test/build、Nix Web sandbox 构建；真实 Chromium + 隔离 btrfs 服务覆盖复制成功/失败、自动/手动关闭、页面错误清理、明暗/窄屏/长路径/CSP，以及账号/Vault/文件/恢复/备份回归。
- 交接：`agent/tasks/S134.md`。无协议/数据库变更；验收合并后由 S135 单独部署本机。

### S136 — Web 文件管理器上传与右键文件操作

- 依据：用户要求服务端 Web 文件管理器支持上传文件，右键复制、剪切、粘贴、删除；前置 S134/S135 done；design §6、§10.3、§15.3、§18.1、§24。
- 结果：沿用授权 API 的上传/finalize/create、固定版本复制、CAS 移动与回收站删除；提供文件/目录上下文菜单和当前目录粘贴入口。
- 安全：同名不覆盖，目录不可移入自身后代；删除确认绑定 revision/子树 token；pending/未知结果保留操作身份；批量部分完成如实展示，不伪造原子性。
- 验证：服务层单测、相关 Web 回归、typecheck/build，独立 btrfs 服务与真实 Chromium 上传/复制/移动/回收站/竞态/CSP 验收。
- 拆分：S136-a 操作编排与单测，S136-b UI 与真实浏览器验收，S136-c 修复真实跨 Vault 同摘要上传暴露的对象登记冲突，S136-d 合并后审查修复（离线恢复原生验证、布局契约与测试、租约顺序、运维处置）；统一在本卡验收和交接。不包含本机生产部署。
- 交接：`agent/tasks/S136.md`；如 API 限制需要显式记录，不改同步核心或降低不变量。

### S135 — 将通知版服务端部署到本机

- 依据：用户追加“完成后，部署到本机”；前置 S134 done；design §19.3，部署规程见 `docs/operations/nixos.md` S113。
- 结果：固定 S134 已验收提交，使用现有 pure 部署入口 build → dry-activate → test → 健康 → switch，仅更新主机项目 pin。
- 验收：主机旧 generation/锁/身份基线，真实 HTTPS/IPC/服务身份与线上资产一致性；生产浏览器复制路径 Notification、关闭/自动消失、明暗/窄屏/CSP，无生产内容写入。
- 交接：`agent/tasks/S135.md`，保留回退路径与真实证据；不降低原 blocked 首发门禁。

### S137 — Web 文件列表重命名与目录树右键菜单

- 依据：用户要求文件列表右键新增重命名，左侧目录树也支持右键菜单；前置 S136 done；design §6、§18.1、§24。
- 结果：文件/目录重命名使用同 Vault CAS move，保留 EntryId/VersionId，重试沿用原请求；树与列表共享打开/属性/复制路径/复制/剪切/粘贴/重命名/删除菜单，Vault 禁止改名/剪切/删除，粘贴落到右键目标目录。
- 验证：操作层单测、Web 回归/typecheck/build、独立 btrfs 服务与真实 Chromium，覆盖取消/无效名/同名/过期基线/未知结果、目录改名后的地址同步、键盘与 CSP。
- 拆分：S137-a Web 操作/菜单；S137-b 真实验收暴露的观察器旧路径误删修复（连续目录改名后保留子项并恢复后续外部观察）。父卡须两项都验收，不降低 I03/I08/I14。
- 交接：`agent/tasks/S137.md`、`S137-a.md`、`S137-b.md`。无协议/数据库变更；验收合并后执行 S138 本机部署。

### S138 — 将文件操作与重命名版服务端部署到本机

- 依据：用户追加“完成任务后，部署服务端到本机”；前置 S137 done；design §19.3 与 S113 部署规程。
- 结果：固定已验收源版本，使用现有 pure 部署入口 build → dry-activate → test → 健康 → switch，仅更新项目 pin。
- 验证：以现场 schema15/generation257 为基线，核对既有14→15备份及旧对象读取兼容；本卡15→15更新，验证 HTTPS/IPC/身份/归属与既有服务稳定；生产 Chromium 验证线上资产、文件/树菜单、重命名取消、明暗/窄屏/CSP。
- 交接：`agent/tasks/S138.md`，保留旧 generation 与整套回退路径；不降低原5张 blocked 首发门禁。

### S139 — 重命名默认选区排除文件扩展名

- 依据：用户要求服务端 Web 重命名弹窗默认选中的文件名不要包括扩展名；前置 S137 done；design §18.1。
- 结果：文件仅选中最后一个非首位点号之前的名称，目录全选；无扩展名及仅首位点号的隐藏文件全选，不改实际名称或提交协议。
- 验证：Web test/typecheck/build，真实 Chromium 验证默认焦点与选区、输入保留扩展名、多点号/隐藏文件/无扩展名/Unicode/带点目录及取消无提交。
- 交接：`agent/tasks/S139.md`；不包含生产部署。

### S145 — 统一 Web 登录入口并部署本机

- 依据：用户要求“添加统一的登录页面，不登录无法查看服务端 web 界面。完成后部署”；前置 S077、S144 done；design §17.1、§18.1、§19.3。
- 范围：顶层会话门禁，初始化核验不显示管理壳；未登录仅独立登录页，登录后恢复原 hash 深链；退出、改密、撤销及受保护请求最终失效后立即回登录。沿用同源 HttpOnly cookie、CSRF、401 单飞刷新，不变更协议/持久格式。
- 验收：Web 单测/typecheck/build；真实 Chromium 核验所有管理路由与深链的匿名门禁、错误登录、登录/刷新/过期/退出/撤销/改密、网络核验失败与重试、明暗窄屏、CSP；复用 pure 部署入口 build → dry → test → 健康 → switch，线上登录门禁与资产/实例保持复验。
- 交接：`agent/tasks/S145.md`；生产不清库、不改用户内容，不降低原 blocked 首发门禁。

### S146 — 清理闲置目录并集中临时任务数据

- 依据：用户要求清理上述闲置目录，今后临时任务数据与缓存统一放在 `/data/vaultsyncd-temp`；win-lab 相关目录不动。前置 S144、S145 done；design §6、§11.5、§19.3。
- 范围：核对进程/挂载后清理旧测试状态、可再生编译产物、退役试用卷与旧凭据；保留生产状态、Vault、备份、原始验收证据和整个 `/data/vaultsyncd-lab`。更新临时目录约定与直接创建数据的脚本默认路径，历史记录不批量改写。
- 验收：保存清理清单、容量、旧 generation 与 GC roots；主机撤试用挂载经 pure build → dry → test/健康 → switch；相关脚本语法/CI检查、真实独立测试卷 create/verify/cleanup；生产身份、挂载、SSH 与运行中 VM 保持。
- 交接：`agent/tasks/S146.md`，记录保留路径、清理结果与后续位置；不改变原5张 blocked 首发门禁。

### S147 — 记录客户端连接并提供 Web 查看入口

- 依据：用户要求“可以记录和查看连接服务端的客户端的情况”；前置 S043、S078、S145 done；design §6、§17.1、§18.1、§24。
- 范围：持久化每次成功登录的连接、设备标签、接入类型、首次登录及最近认证请求时间；凭据轮换保持连接身份。管理端分页查看当前账号记录，标识当前连接、最近活动/未活跃、已撤销/已过期；无心跳，不能称实时在线或同步完成。旧数据活动时间和类型未知，不猜测。
- 拆分：S147-a 存储迁移、轮换及认证记录、只读协议/API；S147-b Web 页面及端到端验收；同一父卡验收，不增加客户端审批或远控，不包含生产部署。
- 验收：空库及 schema17 升级/备份恢复、轮换/注销/改密/重启、无效凭据不更新活动、账号隔离与稳定分页；生成一致性、相关 Rust/前端回归、typecheck/build、独立 btrfs 服务真实 Chromium 验证明暗窄屏、会话门禁、错误重试和转义/CSP。
- 交接：`agent/tasks/S147.md`；保留原5卡 blocked 门禁。

### S148 — Web 登录后自动连接服务

- 依据：用户要求“登录web管理端后，自动连接服务”；前置 S145 done；design §17.1、§18.1。
- 范围：登录成功与刷新恢复会话后自动执行同源健康检查；所有管理路由显示真实连接状态，保留手动检查/重试。未登录不自动检查，退出取消在途请求，旧响应不影响新会话；不增加轮询或改变认证/同步语义。
- 验收：Web test、根 typecheck、Web build；真实 Chromium + 隔离 btrfs 服务验证匿名无检查、登录/刷新自动检查、导航不重复、离线失败/手动恢复与退出取消/CSP。
- 交接：`agent/tasks/S148.md`；本卡不含生产部署，保留 S147 独立工作树。

### S149 — 将客户端连接记录及自动连接版服务端部署到本机

- 依据：用户2026-11-09要求“部署服务端到本机”；前置S147、S148 done；design §19.1—19.3。
- 范围：固定当前已提交源d9cfee9，保全正式数据及身份，完成schema17→18升级、本机HTTPS与同源Web部署；保持运行系统依赖，主机尚未激活的锁升级单独保留。
- 验收：升级前完整导出并校验可恢复备份；pure build→dry-activate→test→HTTPS/IPC/身份/目录归属→switch；生产客户端连接页、登录后自动连接、认证门禁与静态资源一致性；既有应用/SSH健康保持。
- 交接：`agent/tasks/S149.md`；原5张blocked卡恢复门禁保持。

### S151 — 正式 Vault 基目录迁至 vaults

- 依据：用户要求 `/data/vaultsyncd/roots` 改为 `/data/vaultsyncd/vaults`，无法迁移时允许放弃旧数据；前置 S149 done；design §11.5、§19.1、§24。优先同卷保留原数据，不扩大到主机锁升级或应用版本部署。
- 范围：备份并短暂停服，同卷重命名基目录；离线同步登记路径及目录身份中的 resolved_path，保留 FSID/子卷/inode/btime、VaultId、epoch、历史、账号和对象；调整主机白名单、权限及挂载依赖。
- 验收：运行配置基线复现，pure build/dry/test/健康/switch；离线数据库完整性、除两处路径外逐表一致，文件及对象哈希/目录身份一致；线上 HTTPS/IPC Vault healthy、历史可查询、新路径生效及旧路径不再存在；备份与回退证据保留。
- 交接：`agent/tasks/S151.md`，原 blocked 首发门禁保持。

### S150 — 文件浏览器右键查看版本历史

- 依据：2026-11-09 用户要求服务端 Web 文件右键菜单提供版本历史对话框；前置 S079、S137、S149 done；design §6（I03/I12/I15）、§18.1、§24。
- 结果：文件菜单新增“版本历史”，就地弹窗显示已提交版本编号、SHA-256、大小、保留期限及当前版本；目录不显示该入口。
- 边界：复用授权 `listEntryHistory`，按原 VaultId/EntryId 分页，上一页/下一页、刷新、失败重试；关闭、切目录/Vault及退出取消请求，迟到响应不污染新弹窗。不新增恢复写入、DTO或schema；本卡不部署生产。
- 验收：根 typecheck、Web test/build，真实 Chromium + 独立 btrfs 服务验证多版本、特殊名称、键盘菜单、分页、空态、网络错误/重试、关闭/切换竞态、明暗窄屏及CSP；分页/空态等若用受控响应，证据须明确区分。
- 交接：`agent/tasks/S150.md` 及批次；原5张blocked首发门禁保持。

### S152 — 客户端 UI 与 Web 统一为 Mantine

- 依据：2026-11-09 用户要求按已讨论方案执行，允许 subagent；S151 编号已有用途，本卡续 S152。前置 S076/S119/S141/S150 done；design §5.6、§18.1、§24。
- 结果：客户端沿用 Tauri/agent IPC，页面采用 Mantine 默认主题及现成组件，统一 48px 顶栏、响应式导航、明暗切换、表单/表格/确认窗/通知；主题和纯展示组件由 packages/ui 提供，Web 共用。
- 拆分：S152-a 共享层和 Web 接入、S152-b 桌面功能页、S152-c 浏览器/Windows 原生验收，主 agent 集成入口及 CSP；不改同步/认证/数据库/wire 语义，不含生产部署。
- 验收：根 typecheck、两端 build、Web test、Nix Web 构建；真实 Chromium 明暗×320/390/768/1440/1920、五桌面路由、断开状态/键盘/确认窗；Windows 原生连接/绑定、文件及历史/通知回归，必要 Rust 检查。
- 交接：`agent/tasks/S152.md` 和子卡；须所有验证完成后标 done、合 master 并清理。

### S153 — 将版本历史版服务端部署到本机

- 依据：用户2026-11-09要求“部署服务端”；前置 S150、S151 done；design §19.1—19.3。
- 范围：固定当前已提交源89a9cf4，部署S150文件版本历史；保留正式Vault路径、数据、身份与schema18，沿用c20199e的运行依赖锁，保留main尚未激活的锁升级。
- 验收：部署前完整备份并verify；运行配置pure求值复现，pure build→dry→test→HTTPS/IPC/身份/归属→switch；生产版本历史只读浏览、静态资产一致及既有服务健康。
- 交接：`agent/tasks/S153.md`；原5张blocked首发门禁保持。

### S154 — 精简 Vault 目录页面

- 状态：done（用户2026-11-09直接要求）；前置 S078、S119、S134 done。
- 依据：design §18.1、§6，沿用管理 Vault API，不改数据语义。
- 范围：健康与采集、注册目录改为工具栏按钮打开弹窗；允许基目录在工具栏右对齐；主体保留目录清单，移除重复说明，保留实际错误与恢复建议。
- 验收：Web typecheck/test/build；真实 Chromium 注册、错误、健康刷新/挂载故障恢复、键盘焦点与明暗窄屏布局；不部署生产。
- 交接：`agent/tasks/S154.md`，原 blocked 首发门禁保持。

### S155 — 移除 GitHub 工作流及关联依赖

- 依据：用户要求去除 GitHub workflow 功能，包括文档和相关代码脚本；前置 S006、S101 的本机验证；design §5.8、§24。
- 范围：删除工作流和专用校验配置，移除触发路径表、仅供托管编译的入口、依赖工作流的测试及报告字段；保留本地 xtask 检查、同源发行与独占实验室入口。同步规格、操作文档和受影响门禁，旧验收事实仍由固定 Git 记录追溯。
- 验收：相关 Python/xtask 测试、契约/边界检查、本地命令规划和缺资源拒绝语义；工作流及活动引用清零，其他任务修改保持。仅开发工具变化，不改变在线服务产物。
- 交接：`agent/tasks/S155.md`；同批记录用户“服务端更改验收后直接部署本机”的持续授权。

### S156 — 本地版本与产物上传 GitHub Release

- 依据：2026-11-09 用户已配置 GitHub 仓库，要求编写版本及产物发布脚本；前置 S093、S110、S111、S155 done；design §5.9、§24、§25。
- 范围：复用现有同源归档，按精确版本/提交发布至配置远端；本地预检、草稿上传/校验/续传、标签围栏、发布说明及依赖使用指引。仅 Linux x64 与 Windows x64，不恢复工作流，不修改首发门禁，不代替原生验收；本轮编写及验证，不执行真实远端发布。
- 验收：相关本地契约/回归、包装入口与锁定 CLI 参数检查；覆盖坏 hash、路径逃逸/链接、混源、版本/标签不符、API 失败、上传中断及续传、不覆盖已发布内容。真实写 GitHub 不属于脚本开发验收。
- 交接：`agent/tasks/S156.md`；记录工作树、提交、合并清理、实际验证及登录/实际发布限制。

### S157 — 将共用 Mantine 与精简 Vault 版服务端部署本机

- 依据：用户要求部署服务端；前置 S152、S154、S155 done，design §19.1—19.3。
- 范围：固定最新已提交源5a71062，发布共用Mantine展示层及精简Vault工具栏；沿用generation265的主机依赖锁与vaults基目录，不混入主机main尚未激活升级。
- 验收：完整备份导出并verify及异盘副本；运行配置pure求值复现，pure build→dry/test→HTTPS/IPC/身份/目录归属→switch；生产登录、共用界面、Vault工具栏/弹窗、版本历史、静态资产与既有服务及数据保持。
- 交接：`agent/tasks/S157.md`；S156独立任务现场保持，原生/性能容量及最终发行门禁保持。

### S158 — 构建并发布首个 GitHub 预发布版本

- 依据：2026-11-09 用户明确要求“帮我发布一版”；前置 S110、S111、S152、S154、S155、S156 done，design §5.9、§24、§25。
- 范围：从当前干净源码构建 0.1.1 的 Linux x64 服务端和 Windows x64 ZIP，通过 S156 脚本上传至 origin；默认预发布、非 Latest，源码提交推送属于本次发布授权。修复实测发现的 Linux 打包变量名不一致及 SQLite 运行库缺漏；保留旧归档，不扩大到 ARM64、不修改版本或首发门禁。
- 验收：打包/上传契约测试、同源双平台构建及 SHA256SUMS、本地 dry-run、GitHub 草稿上传与逐附件哈希核对、最终标签/源码/预发布属性确认。未执行的 Windows/Ubuntu 原生及性能容量验收如实保留。
- 交接：`agent/tasks/S158.md`；记录精确源、Release URL、产物/永久证据、提交及工作树合并清理；仅发行操作，不重复部署相同服务端代码。

### S159 — Vault 路径跳转与文件容量

- 依据：2026-11-09 用户要求点击 Vault 目录表格的服务器路径后打开对应文件浏览器；前置 S122、S154 done；design §18.1。
- 范围：主体表格服务器路径使用原生链接，复用 filesLocation 按 VaultId 打开根目录；增加“文件容量”列。用户确认口径为当前已提交且未删除文件的逻辑总大小，按文件计数，不含历史和回收站；管理清单新增兼容可选 file_bytes 字段，旧服务缺省显示未知，不改 DB schema 或文件数据。
- 验收：Rust 聚合/授权和协议生成一致性；Web typecheck/test/build；真实 Chromium 验证非首个 Vault、根目录、键盘和返回、刷新、容量与明暗窄屏；验收后直接部署本机并验证 HTTPS、版本及界面行为。
- 交接：`agent/tasks/S159.md`；S158 发行现场和原 blocked 首发门禁保持。

### S160 — 服务概览显示 Server ID

- 状态：done；依据：2026-11-09 用户明确要求，design §18.1；前置 S076/S077/S148 done。
- 范围：概览的服务连接卡片显示完整、只读可选中的 Server ID，沿用 GET /v1/capabilities；独立处理读取错误、重试与卸载取消，不改协议、凭据或客户端配置。
- 验收：Web typecheck/test/build、既有认证浏览器回归；真实 Chromium 核对 API 一致性、失败/重试、退出取消、明暗窄屏；通过后部署本机并验证 HTTPS/身份/界面。
- 交接：`agent/tasks/S160.md`；原 blocked 首发门禁保持。

### S161 — 历史 build/deploy 资料迁入临时根

- 依据：2026-11-09 用户要求将 `/data/vaultsyncd-build`、`/data/vaultsyncd-deploy` 集中到临时根；前置 S146 done，design §6、§19.3。
- 范围：同文件系统重命名到 `/data/vaultsyncd-temp/` 下的同名目录，保留证据、备份、权限、btrfs 快照身份；迁移 Nix GC roots，历史记录通过新旧路径映射追溯。
- 验收：运行与挂载引用检查、迁移前后文件摘要及元数据、GC roots 登记与目标存在、快照身份、服务进程/系统及 HTTP/HTTPS 健康；不触碰正式数据、lab 或重建服务。
- 交接：`agent/tasks/S161.md`；原 blocked 首发门禁保持。

### S162 — 托盘自动下载 GitHub Release 并安全升级重启

- 依据：2026-11-09 用户新增并明确选择自动升级及重启；前置 S100、S109、S156 done；design §6、§18.1、§19.3、§24。
- 范围：托盘右键“自动更新”，后台检查最新正式 Release、下载 Windows x64 ZIP 并核对版本/来源/大小/SHA-256；提取固定程序集合，复用 agent 维护屏障、排空、恢复导出、全状态/程序备份及失败回退；ZIP 归属在受控事务中更新，禁止普通覆盖/不同路径接管。
- 子步骤：S162-a 网络与校验；S162-b ZIP/安装版维护与重启；S162-c 宿主及 Windows 原生验收。属于同张用户功能卡，不展开其他 ready 工作。
- 验收：版本/下载负例与安全解压；Windows 原生 ZIP 升级、归属/状态/暂停保持、重启与失败恢复、托盘入口/重复点击；交叉构建及受影响既有维护测试。真实 GitHub 无可访问正式 Release 时如实记录，使用受控附件验证不得称为线上升级。
- 交接：`agent/tasks/S162.md`、progress、interfaces；全部验收后合 master 并清理；不修改服务端，不触发部署。


### S163 — 开发测试目录统一迁入 dev 根

- 依据：2026-11-09 用户要求所有 `/data/vaultsyncd-*` 目录迁入 `/data/vaultsyncd-dev`，以后开发测试目录只能位于该根；前置 S146、S161 done，design §6、§19.3、§24。本卡 ready，覆盖此前 lab 保持原位的约定。
- 范围：同文件系统搬迁 temp/lab 两棵现有目录并保留内部布局、证据、权限、btrfs 身份；修复 GC roots、活跃引用、开发脚本默认路径与当前操作约定。正式数据与运行服务产物保持。
- 验收：迁移前后元数据/稳定文件摘要、GC roots有效性、子卷身份与挂载、VM/SSH/socket及生产HTTP/HTTPS健康；脚本语法、受影响现有单测、真实测试卷生命周期与旧根拒绝、Windows缓存可用。历史证据只记前缀映射，不批量改写。
- 交接：`agent/tasks/S163.md`；全部验收后合master并清理任务树，原4张blocked首发门禁保持。


### S164 — 归档旧 vs 测试目录并清理已验证副本

- 依据：2026-11-09 用户明确要求归档 `/data/vs-*` 所需证据、核实重复及可再生数据后删除，并再次确认以后开发目录只能放在 `/data/vaultsyncd-dev/`；前置 S163 done，design §6、§19.3。
- 范围：检查进程、挂载、loop、单元和 GC 引用；保存完整文件清单、摘要、权限及路径映射，归档所有原始证据和 S097 离线恢复镜像。完整解包验证通过后才删除源树和验证副本，仅排除无活动占用的可再生 socket；修正仍直接在 `/data` 建目录的两个旧脚本。
- 验收：归档完整性测试、解包后逐文件 SHA-256/权限/属主/mtime/xattrs 一致、删除前重验源与引用、旧目录无残留、服务和 VM 身份及 HTTP/严格 TLS HTTPS 健康保持；脚本语法与路径守卫检查。历史证据不批量改写，不重建服务。
- 交接：`agent/tasks/S164.md`；归档、清单和操作日志长期保留，不作为缓存自动删除；原四张 blocked 首发门禁保持。

### S165 — 构建并发布 v0.1.2 GitHub 预发布

- 依据：2026-11-09 用户要求新 GitHub Release，并确认沿用预发布；前置 S158、S159、S160、S162、S163、S164 done，design §5.9、§24、§25。
- 范围：产品升至 0.1.2，同步版本镜像与工作区锁，从同一干净提交重建 Linux x64 服务端及 Windows x64 ZIP，发布到 origin 为预发布、非 Latest；必要源码推送属于本次发布授权。保留旧发行及开发根中的完整证据，不改首发门禁。
- 验收：版本一致、发布契约、同源双平台构建与归档/运行库/PE 检查、SHA256SUMS、本地 dry-run、草稿上传及远端逐附件大小/摘要、最终标签/源码/预发布属性核对。未重跑的原生与性能容量实证如实记录。
- 交接：`agent/tasks/S165.md`；记录精确源、Release URL、永久证据、提交、合并及清理。仅发行版本与既有功能，不触发无关主机部署。

### S166 — 客户端按地址读取并确认 Server ID

- 依据：2026-11-09 用户要求仅填写服务器地址，自动读取 Server ID 供确认；前置 S081、S160 done；design §17.2、§18.1、§24。
- 范围：agent 提供无凭据只读发现 IPC，桌面按地址/可选 CA 读取完整身份，确认后保存；编辑输入和迟到请求不得复用旧身份。保存前重新读取并比对，有绑定时仍拒绝切换身份；保持 HTTPS 验证、原配置及凭据。
- 验收：IPC 生成一致性/兼容与参数负例、桌面 typecheck/build、Windows 交叉检查及真实 WebView/agent/HTTPS 发现、确认、重试、证书/身份错误和已有绑定保护；GUI 关闭后同步继续。所有证据位于开发根。
- 执行分段：S166-a 为 agent/IPC 发现与保存核验，S166-b 为桌面确认交互及原生验收；仍只交付本卡一个结果。
- 交接：`agent/tasks/S166.md`；通过后合 master 并清理。客户端变更不触发服务端部署或发行。

### S167 — 桌面启动时复用或启动本机会话 agent

- 依据：2026-11-09 用户明确要求客户端桌面程序启动时自动启动 agent；前置 S080、S099、S109、S162 done；design §5.9、§18.1、§24。本卡 ready。
- 范围：桌面启动先验证同用户会话的真实 agent IPC；未运行时通过同目录程序和固定用户配置启动，等待健康后开窗。复用安装版/ZIP 的配置、归属与维护门禁，关窗不终止 agent，启动错误明确显示。
- 验收：宿主相关单测、Windows x64 交叉编译与严格 clippy；Windows 原生桌面冷启动、已有 agent 复用、重复启动、GUI退出后 agent存活，以及启动失败/配置与维护保护。
- 交接：`agent/tasks/S167.md`；客户端更改不触发无关服务端部署，原四张 blocked 首发门禁保持。

### S168 — GitHub 发布不再上传运行库对应源码归档

- 依据：2026-11-09 用户明确要求发布 Release 时不上传 sources 包，并确认所指为 `vaultsyncd-runtime-sources.tar.gz`、该归档仅留在本地构建目录；前置 S156、S165 done；无 design 接口改变（§25 发行只限制产物，不要求该附件上网）。本卡 ready。
- 范围：`packaging/github-release.py` 只上传平台归档、平台来源清单、根清单与两校验表；Linux 运行库对应源码仍随构建目录保留、继续按组件收据核验，dry-run 明确披露为 `local_only`，发布说明注明另行提供。同步发布契约测试、`packaging/README.md`、`docs/operations/linux.md` 与 `docs/architecture/interfaces.md`。
- 验收：`tests/contract/github_release.py` 全绿（含“不上传对应源码”“本地哈希不符拒绝”“说明披露按平台区分”负例/正例）；`release.py`、`nix/portable.nix` 与容器入口构建/校验对应源码的既有行为不变；不改线上服务、不改已发布 Release。
- 交接：`agent/tasks/S168.md`；通过后合 master 并清理。许可说明为“对应源码不再作为 GitHub 附件，分发二进制者须另行提供本地归档”。

### S169 — 便携发行与 GitHub 附件收敛为 Windows ZIP

- 依据：2026-11-10 用户明确要求便携部署只考虑 Windows，GitHub Release 只发布 Windows 便携 ZIP；前置 S110/S156/S168 done，design §5.9/24/25、ADR-027。本卡 ready。
- 范围：统一 release 仅构建 Windows x64 ZIP，拒绝 Linux 参数；上传仅接受 Windows 单平台目录、只传 ZIP，来源/日志/校验表留本地并核验。Linux 服务端继续 Nix/NixOS，历史 Linux 实验室入口/发行保留；不修改已发布 Release。
- 验收：release_bundle、github_release、windows_zip 契约及包装入口通过，核对默认/显式/all 仅 Windows、Linux/混合拒绝、唯一附件及来源/完整性/续传/Latest 保护；真实同源 Windows ZIP 构建及本地 dry-run。无新 CFAPI 行为，不以编排测试声称原生验收。
- 交接：`agent/tasks/S169.md`；验收后合 master 并清理，本卡不发新版，不触发无关服务端部署。

### S170 — 客户端登录 IP 与五分钟连接状态

- 依据：2026-11-10 用户明确要求服务管理/客户端统计登录 IP 与当前连接（五分钟内检测到连接）；前置 S147、S160 done，design §17.1/18.1/24。本卡 ready。
- 范围：S170-a 持久登录 IP、可选 wire 字段与迁移/来源验证；S170-b 客户端表格及自动刷新、真实浏览器验收、本机部署与交接。属于同一功能卡，不展开其他任务。
- 验收：IPv4/IPv6、受信本机代理/直连及伪造转发头、旧库备份迁移恢复、刷新身份/IP保持；五分钟边界、撤销/过期、分页自动更新/取消/错误；生成一致、受影响消费者检查、真实 Chromium 与部署健康。
- 交接：`agent/tasks/S170.md`；不改变同步或 Windows 原生能力，不补造旧 IP。

### S171 — Web 右键新建目录与下载

- 依据：2026-11-10 用户要求 Web 文件管理器右键新建目录与下载；前置 S136/S137/S170 done，design §15.3/18.1/24。本卡 ready。
- 范围：空白区与目录（含目录树）右键新建目录，文件右键下载固定已提交版本；复用授权 API、expected_absent 和原操作恢复。下载分段读取并核验大小/摘要，保留原名，失败不另存损坏内容。
- 验收：同名/非法名、取消、目录目标、pending/丢响应恢复；空文件/Unicode/分段/版本固定、错误/取消/认证；Web 检查、真实 Chromium/btrfs、本机部署与线上健康。
- 交接：`agent/tasks/S171.md`；不改变 wire/schema、同步或发行；验收后合 master 并清理。

### S172 — 客户端与服务端查看文件同步和传输记录

- 依据：用户2026-11-10要求查看哪个时间、哪个客户端同步哪个文件及上传/下载等属性；前置S147/S152/S170/S171 done；design §6、§7.2、§13.2、§15.5、§18.1、§24。
- 结果：服务端持久记录认证连接、时间、当时文件路径、方向、版本、大小、范围与结果；Web与客户端经各自API/agent IPC查看授权Vault的分页记录。
- 拆分：S172-a 服务端schema20、事务内同步提交账本与传输事实、只读API；S172-b 客户端IPC/Tauri与记录界面；S172-c Web界面、真实浏览器与本机部署。共用同一父卡验收，子agent只分工，不降低门禁。
- 边界：从新版本开始记录，不伪造历史；上传内容已校验不等于同步已提交；下载记录表示服务端提供固定版本内容/区间，不能推导客户端接收、校验或落盘。连接身份与声明的DeviceId分开，保留当时路径和客户端名称，不记录凭据。
- 验收：升级备份与旧库门禁、授权隔离/严格分页、提交幂等/恢复/失败及固定版本Range；两端类型/构建与界面错误/空态/筛选/分页；Windows原生IPC及桌面验证；本机部署、健康与旧数据保持。
- 交接：`agent/tasks/S172.md`，记录分工、实际命令与证据、源提交、部署、合并及工作树清理；原四张blocked门禁保持。

### S173 — 发布 0.1.3 预发布 GitHub Release

- 依据：2026-11-10 用户要求发布新的 GitHub Release 并确认按预发布上传；前置 S169/S172 done。`v0.1.2` 标签与 Release 已指向旧提交 `889c19f`，本卡升版 0.1.3 后发布。
- 范围：同步根 Cargo、前端/Tauri 镜像与 Cargo.lock（不改协议/schema/生成器版本）；推送 master；用 `tools/release.sh` 构建 Windows x64 便携 ZIP；`tools/publish-release.sh` 以预发布上传唯一 ZIP，不设 Latest。
- 验收：`cargo xtask version --check`、`pnpm install --frozen-lockfile`、真实构建的 SHA256SUMS 校验、`--dry-run` 计划与远端附件大小/SHA-256 核对；不声称 Windows 原生复验或首发门禁通过。
- 结果：0.1.3 版本镜像与工具重查修复 `d2f9e26`、`532ec19` 已推送 master；Windows ZIP `e9db7e56…` 由 `v0.1.3` 预发布发出（非 Latest、唯一附件），远端大小与 SHA-256 复核一致。未做 Windows 原生复验。
- 交接：`agent/tasks/S173.md`；不改动已发布 Release，不触发服务端部署。

### S174 — ZIP 同目录覆盖后自动采纳归属

- 依据：2026-11-10 用户在实际使用中把新版本 ZIP 解压覆盖原程序目录后遇到 `ZIP_OWNER_MISMATCH` 无法启动，明确要求“不要要求这么严格，直接替换或者通过程序自动更新都允许”；知情选择为**自动采纳且不做备份**、**允许任意版本方向**、**只放宽覆盖而不改自动更新入口**。前置 S109/S162/S167 done；design §19.3、§24（ADR-030）。
- 范围：`portable::resolve_owner`/`adopt_owner`（同目录、`verify_zip` 自洽时重钉 `zip-owner.json` 并写 `zip-owner-adoptions.jsonl`）、`maintenance::admit_startup` 屏障优先、单测与 `s109-zip.ps1` Fresh 原生断言；同步 design ADR-030、接口、ZIP 运维文档与包内 README。跨目录/安装版互转、维护事务严格前置、自动更新范围不变。
- 验收：交叉编译 + 严格 clippy + 测试 EXE 编译通过；Windows 原生 `s109-zip.ps1 -Mode Fresh` 新增采纳断言通过（客体可用时）；不声称可回滚或无备份风险已消除。
- 交接：`agent/tasks/S174.md`；无服务端产物变化，不触发部署。

### S175 — 发布 0.1.4 预发布 GitHub Release

- 依据：2026-11-10 用户要求发布新的 GitHub Release 并确认按预发布上传；前置 S174 done。`v0.1.3` 标签与 Release 已指向旧提交 `d2f9e26`，且 master 领先 `origin/master` 两个提交，本卡升版 0.1.4 后发布。
- 范围：同步根 Cargo、前端/Tauri 镜像与 Cargo.lock（不改协议/schema/生成器版本与业务代码）；推送 master；用 `tools/release.sh` 构建 Windows x64 便携 ZIP；`tools/publish-release.sh` 以预发布上传唯一 ZIP，不设 Latest。
- 验收：`cargo xtask version --check`、`pnpm install --frozen-lockfile`、真实构建的 SHA256SUMS 校验、`--dry-run` 计划与远端附件大小/digest 核对；不声称 Windows 原生复验或首发门禁通过。
- 结果：版本镜像 `fe273d3` 已推送 master；Windows ZIP `041b293a…` 由 `v0.1.4` 预发布发出（非 Latest、唯一附件），远端大小与 digest 复核一致，标签指向 `fe273d3`。未做 Windows 原生复验。
- 交接：`agent/tasks/S175.md`；不改动已发布 Release，不触发服务端部署。

### S176 — 桌面启动登录门禁与登录资料持久化

- 依据：用户2026-11-10要求桌面未登录时弹出服务器地址、用户名和密码登录框，登录后进入主界面并保存登录信息及凭据；前置S166/S167/S172/S175 done，design §6/17/18.1/24。本卡 ready。
- 范围：S176-a agent保存用户名及连接信息、恢复会话并准确报告重新登录状态；S176-b 桌面统一登录框与主界面门禁；同卡顺序实施，不扩大同步或发行范围。
- 验收：前端检查、IPC生成一致、客户端迁移恢复与旧版拒写、Windows交叉严格检查；真实Windows WebView/agent/HTTPS登录失败与成功、登出门禁、重启恢复和凭据不进入浏览器存储。不改变服务端，无部署。
- 交接：`agent/tasks/S176.md`；验收与交接完成后合master并清理。

### S177 — 发布 0.1.5 预发布 GitHub Release

- 依据：2026-11-10 用户要求“github release 发版”，确认版本号 0.1.5、按预发布上传（非 Latest）、推送 master 到 origin；前置 S176 done，master 领先 `origin/master` 两个提交。
- 范围：同步根 Cargo、前端/Tauri 镜像与 Cargo.lock（不改协议/schema/生成器版本与业务代码）；推送 master；用 `tools/release.sh` 构建 Windows x64 便携 ZIP；`tools/publish-release.sh` 以预发布上传唯一 ZIP，不设 Latest。
- 验收：`cargo xtask version --check`、`pnpm install --frozen-lockfile`、真实构建的 SHA256SUMS 校验、`--dry-run` 计划与远端附件大小/digest 及标签指向核对；不声称 Windows 原生复验或首发门禁通过。
- 结果：版本镜像 `7077fc9` 已推送 master；Windows ZIP `a25d6391…`（15,981,059 字节）由 `v0.1.5` 预发布发出（非 Latest、唯一附件），远端 digest 一致，标签指向 `7077fc9`；`releases/latest` 404，无正式版渠道条目。
- 交接：`agent/tasks/S177.md`；不改动已发布 Release，不触发服务端部署。

### S178 — Vault 选择框每次打开自动读取授权清单

- 依据：2026-11-10 用户要求取消 Windows 客户端手动“读取授权 Vault”，每次使用选择控件自动读取；前置 S081/S114/S152/S176 done，design §6（I15）、§18.1。本卡 ready。
- 范围：同步目录页使用 Mantine Select 的打开事件读取，移除单独读取按钮；加载、错误、空清单、重复打开与选择失效处理；更新直接消费者验收脚本。无 IPC/schema/服务端变更。
- 验收：桌面 typecheck/build、Chromium 鼠标/键盘打开与刷新、错误重试/空清单/授权变化/重复请求保护；真实 Windows S081 WebView/agent/HTTPS 绑定回归。无服务端部署或发行。
- 交接：`agent/tasks/S178.md`；完成后合 master 并清理任务工作树。

### S179 — 发布 0.1.6 正式版 GitHub Release

- 依据：2026-11-10 用户要求“github release 发版”，明确版本号 0.1.6 且按正式版发布（`--stable`、设为 Latest）；前置 S178 done，master 领先 `origin/master` 两个提交。
- 范围：同步根 Cargo、前端/Tauri 镜像与 Cargo.lock（不改协议/schema/生成器版本与业务代码）；推送 master；用 `tools/release.sh` 构建 Windows x64 便携 ZIP；`tools/publish-release.sh --stable` 上传唯一 ZIP 并设 Latest，成为托盘自动更新渠道的正式版来源。
- 验收：`cargo xtask version --check`、`cargo fmt --check`、`cargo xtask generate --check`、`pnpm install --frozen-lockfile`、真实构建的 SHA256SUMS 校验、`--dry-run` 计划与远端附件大小/digest、Latest 归属及标签指向核对；不声称 Windows 原生复验或首发门禁通过。
- 交接：`agent/tasks/S179.md`；不改动已发布 Release，不触发服务端部署。

### S183 — 统一客户端应用图标与服务端网页品牌图标

- 依据：2026-11-10 用户要求将根目录 `logo.svg` 用作客户端 app icon、服务端网页 favicon 与 logo；前置 S180、S182 done；design §18.1。
- 范围：保留 SVG 为唯一矢量源，生成 Windows 多尺寸 ICO；替换 Tauri EXE/窗口/托盘图标；Web 登录与顶栏展示同源 SVG，favicon 与 logo 共用构建资源；补齐 Nix Web 源过滤。
- 验收：根 typecheck、Web/desktop build、Web 现有测试、fmt、Windows 严格 clippy/构建；Chromium 明暗/窄宽屏图标与 favicon 取源核对，真实 Windows EXE/窗口图标核验；验收后本机部署及公网健康/资源核对。
- 交接：`agent/tasks/S183.md`；不新增发行或改变同步/认证协议，既有首发门禁保持。

### S184 — 开发构建脚本 `tools/build.sh` 输出到 `dist/`

- 依据：2026-11-10 用户要求清空根 `dist/`，把 `./tools/release.sh` 产物路径设为 `dist/` 且不带版本号、只编译当前代码；并允许在 release.sh 必须保留版本号时另建 build 脚本。用户选定产物为完整便携程序目录。
- 范围：新增 `tools/build.sh`（自动进入 `.#windows`、重建前端、静态 CRT 交叉编译五个 EXE、复用 `packaging/windows/zip/build.py` 生成两份同源清单、先清空 `dist/`）；不改 `tools/release.sh`、`packaging/release.py` 与任何 wire/schema/发布语义；同步 `packaging/README.md`、`docs/operations/windows-lab.md` §12.2 与 interfaces S184。
- 验收：任务树内实跑退出 0；五份 PE32+ x64 导入表无 MSVC 运行库；`installed-files.json`/`zip-files.json` 同源且逐文件 hash/字节一致；Windows 原生运行不在本卡范围。
- 交接：`agent/tasks/S184.md`；开发构建不是发布产物，不改动已发布 Release 与首发门禁。

## 7. 记录模板与续接规则

### 7.1 agent/progress.md

保持一个紧凑状态表：`任务 | 状态 | 产出提交或工作树 | 验证摘要 | 阻塞原因 | 交接路径`。顶部只放当前里程碑、最早 ready 任务、平台资源状态和最近一次集成结果。初始所有步骤为 todo，不能因创建了目录就完成业务卡。

已完成行只保留状态、产出索引与短交接入口，不重复详细验证和历史工作树；blocked 行保留实际缺口与恢复条件。过时的批次播报和“下一 ready”从顶部移除，以批次记录及 Git 历史追溯；当前没有 ready 卡时明确写无，不从历史交接推导待办。

S001 初始化完整编号；S002 以后工具可以仅输出未完成行及直接依赖，避免后期每次读取整份表。遇到产品规格冲突记录在对应卡并按 design 规则处理；普通实现选择自行解决并简短记录。

### 7.2 agent/tasks/Sxxx.md

执行中使用下列模板记录检查点。完成后可整理为约 300—600 中文字的短交接，复杂接口/恢复边界按实际需要保留更多信息：结果、关键接口/规范入口、历史验证、限制、实际提交与隔离信息。父卡保留原文件路径。已完成拆卡和批次可在父卡保留结论、验证、限制与生命周期后删除，查阅入口统一见 `agent/tasks/README.md` 的固定 Git 归档，并更新引用；未完成卡的检查点与恢复条件仍保留当前文件。仅在需要时展开原始命令与排障过程。

压缩前选定包含完整原文的固定 Git 提交，短交接写明 `git show <完整提交>:agent/tasks/Sxxx.md`，禁止使用会变化的 HEAD 或“以分支 HEAD 为准”代替。设计、接口、ADR、运维资料继续以 docs 为维护入口，不复制到历史摘要；Git 外原始证据仍须单独保存，不能声称可由 Git 恢复日志。历史限制注明当时范围，后续覆盖只引用已有验收；本次文档整理不重新认定 done，不缩减原任务卡验收要求。blocked 卡保留完整恢复条件。

```markdown
# Sxxx — 任务标题
状态：todo / in_progress / blocked / done
依据：design 的具体小节、C/I/T 编号
前置：编号与实际可用接口
结果：本轮实际完成的行为
接口：新增/修改的函数、DTO、错误、迁移或命令
文件：主要改动路径
工作树：绝对路径、分支或 detached HEAD、基线提交；纯文档原地维护注明不适用
基线：准备与检查命令、退出码、已有失败或缺失条件
提交：实际 hash；未提交则注明原因及保留位置
验证：环境、命令、退出码、实际观察、报告路径
限制：尚未验证/失败项，禁止写成已通过
续接：下一张 ready 卡；或本卡剩余最小动作
```

检查点另写当前正在修改的文件/函数、编译错误和重现命令。下一 agent 先按交接定位任务 worktree，再读检查点和 Git diff，不重建已有骨架、不覆盖未提交工作。无法访问前次大日志时按记录命令复现，不能猜结果。

## 8. 本手册采用的官方环境依据

下列链接用于工具和平台实现查阅；执行时按已锁定版本核对，不把页面上的最新版本自动当项目版本。产品需求与验收语义已在 design 和本文说明。

- NixOS 手册：系统配置、服务与 rebuild：https://nixos.org/manual/nixos/stable/
- Nix `develop`：项目 devShell：https://nix.dev/manual/nix/2.35/command-ref/new-cli/nix3-develop
- Tauri 2 Windows 前置条件：https://v2.tauri.app/start/prerequisites/
- Tauri 2 Windows 打包：https://v2.tauri.app/distribute/windows-installer/

开始时只执行 S001。之后每轮按依赖选择一张 ready 卡；直到 S108 满足全部首发门禁，才将项目状态改为“首发验收完成”。


### S180 — 桌面更新进度与登录版本信息

- 依据：2026-11-10 用户要求桌面客户端更新提供进度条，登录界面显示版本信息；前置 S162/S176/S179 done，design §6、§18.1、§24，本卡 ready。
- 范围：S180-a 共用 Windows 原生进度窗口与下载/校验/维护/重启阶段；S180-b 登录框构建版本；S180-c 修复组合验收暴露的 S174 屏障拦截更新 trial 回归并整体验收。同卡顺序实施，保留普通启动屏障、更新来源校验、事务与回退。
- 验收：根 typecheck/desktop build；受影响 Rust 宿主测试与 Windows 严格 clippy/交叉构建；浏览器登录/明暗窄宽布局；真实 Windows 进度条及自动更新成功/失败恢复回归。百分比仅取下载实际字节，无总量阶段用活动条，下载完成不冒充安装完成。
- 交接：`agent/tasks/S180.md`；验收交接后合 master 并清理；无服务端改动、无发行推送。


### S185 — 删除 ZIP 专用入口与普通启动归属校验

- 依据：用户明确要求“删除 vaultsyncd-zip.exe，不需要校验任何东西，要保证下载完放到任意目录，即可执行”；前置 S167/S174/S184 done，design §19.3/§24 ADR-032，覆盖 ADR-014/030 的启动归属限制。
- 范围：S185-a 运行时移除 ZIP owner/清单/程序来源启动门禁，复用同用户会话 agent 不限定解压目录，安全 start/stop/health 并入 agent；S185-b 删除专用 EXE 的打包/更新依赖、同步契约和运维文档；S185-c Windows 原生跨目录、移动、旧 owner/清单无关启动和单实例/维护屏障回归。子批次同卡顺序整合，不降低验收。
- 边界：保留真实运行必要条件、用户认证/单 writer/状态/schema/Vault 保护和未完成维护事务屏障；发布和显式自动更新候选校验不作为普通启动门禁；不删除旧 profile/owner，不改同步协议或服务端。
- 验收：ZIP 打包契约、xtask/自动更新消费者测试、Windows 严格 clippy/交叉构建和真实 Windows 上跨目录/无清单/旧 owner/移动重启/安全停止；按实际结果记录，不声称可执行损坏或缺依赖的 EXE。
- 交接：`agent/tasks/S185.md`；验收与交接后合 master 并清理本卡工作树；重建开发 dist，不推送/发行，不触发服务端部署。

### S186 — 开发构建先清空 dist 且失败不留残骸

- 依据：2026-11-11 用户要求“build.sh 构建产物前，先清空 dist 目录”；前置 S184/S185 done，`agent/tasks/S185.md` 的后缀构建留下 `dist/.stage` 证明现有清空位置与暂存位置不够严。
- 范围：`tools/build.sh` 在进入 `.#windows` 之前就清空 `dist/` 与暂存目录（`$CARGO_TARGET_DIR/dev-build-stage`，默认 `target/dev-build-stage`），全部产物与清单在暂存目录组装、齐套后才一次性搬入 `dist/`，暂存目录由 `EXIT` trap 清理；不改发布链（`tools/release.sh`、`packaging/release.py`、`cargo xtask release`）与任何 wire/schema/产物内容。
- 验收：任务树内实跑构建退出 0 且 `dist/` 只剩预期 11 个文件（无 `.stage`、无 `unsigned.zip`）；预置 `dist/` 旧文件与暂存目录残留后重跑，旧文件与残留均消失；模拟早期失败（pnpm shim 退出 3）时退出码透传、`dist/` 不存在、暂存目录被清掉。
- 交接：`agent/tasks/S186.md`；开发构建仍不是发布产物，不触发服务端部署与发行推送。

### S182 — 发布 0.1.7 正式版 GitHub Release

- 依据：2026-11-10 用户要求“发布到 github release”，明确版本号 0.1.7 且按正式版发布（`--stable`、设为 Latest）；前置 S180、S181 done，`v0.1.6` 已占用且不含这两项。
- 范围：同步根 Cargo、前端/Tauri 镜像与 Cargo.lock（不改协议/schema/生成器版本与业务代码）；推送 master；用 `tools/release.sh` 构建 Windows x64 便携 ZIP；`tools/publish-release.sh --stable` 上传唯一 ZIP 并设 Latest，成为托盘自动更新渠道的正式版来源。
- 验收：`cargo xtask version --check`、`cargo fmt --check`、`cargo xtask generate --check`、`pnpm install --frozen-lockfile --offline`、真实构建的 SHA256SUMS 校验、`--dry-run` 计划与远端附件大小/digest、Latest 归属及标签指向核对；不声称 Windows 原生复验或首发门禁通过。
- 交接：`agent/tasks/S182.md`；不改动已发布 Release，不触发服务端部署。

### S188 — 多服务器连接存储与迁移

- 用户要求 Windows 客户端同时连接不同服务器，见 design §17.3/§18.1/§24 ADR-033。
- 按 ServerId 保存连接；从 schema 14 单连接迁移，复用现有 Windows Credential Manager 键和账号资料，保留已有绑定、游标与待办。
- 验收覆盖空库、旧库迁移、重复启动、故障重试、旧版写入围栏与备份恢复。
- 交接记录在 `agent/tasks/S188.md`。

### S189 — 按服务器运行同步并路由命令

- 同用户 agent 内按 ServerId 管理连接、会话及同步循环；连接命令显式选定服务器，文件、历史、恢复等由 BindingId 路由；本机 IPC 2.10 与 CLI/Tauri 桥同步更新。
- 验收覆盖身份复核、服务器故障与认证隔离、全局目录重叠、绑定身份不变；生成一致、宿主测试、Windows 严格检查。
- 交接记录在 `agent/tasks/S189.md`。

### S190 — 桌面直接打开绑定列表

- Mantine 主界面始终可用，连接设置按服务器添加/登录/退出/编辑，绑定选择服务器与已授权 Vault。
- 验收覆盖零连接启动、两台服务器选择、认证过期不退回登录、切换后丢弃迟到结果；桌面构建、浏览器及真实 WebView 验证。
- 交接记录在 `agent/tasks/S190.md`。

### S191 — Windows 双服务器原生验收

- 同一 agent 与两个隔离 HTTPS 服务端使用独立 NTFS 目录，覆盖双向同步、按需 CFAPI、重启、单服务器离线和登出。
- 验收按字节验证各服务器与本机的来源隔离，验证单服务器故障时另一台仍同步，核对旧状态迁移与正常路径。
- 交接记录在 `agent/tasks/S191.md`。

### S192 — 解除绑定后移出桌面相关列表

- 依据：2026-11-11 用户要求 Windows 客户端「同步目录 / 本地 Binding」解除绑定后从相关列表删除；前置 S190/S191 done。
- 设计入口：design §6、§18.1；沿用解绑保留数据语义，不删除持久恢复记录、本地内容或服务器文件，不改 wire/schema。
- 实现：统一过滤已解除绑定，覆盖本地 Binding 表、管理卡片及文件/历史/传输记录选择器；绑定集合变化时清空失效选择和页面缓存，解绑保留报告在卡片卸载后仍可见。
- 验收：根 typecheck、桌面 build/activity、Chromium 取消/失败/成功/刷新/外部解绑与迟到读取回归；Windows S082 真 WebView→IPC→HTTPS/CFAPI 验证列表移除及未确认内容/恢复包/服务器数据保留。
- 交接：`agent/tasks/S192.md`；验收后本地提交合 master 并清理本卡树/分支，重建开发客户端；无服务端部署或远端发行。

### S199 — 维护准备与部分占位重新包含

- 依据：S196 原生验收发现，存在「已跟踪但本地已删除的排除条目」时，维护准备（`PrepareMaintenance`，即有序停止）返回 `LOCAL_CHANGED`，停服与导出被阻塞；按需绑定的部分水合占位在取消排除后同样只保留围栏。
- 设计：design §6、§10.4；导出必须如实覆盖未确认内容，不能因排除条目缺失而静默跳过，也不能删除队列或伪造成功。
- 实现：区分「排除而停放」「本地已删除且未恢复」「部分占位」与真正的未确认内容，给出显式恢复入口与准确原因；恢复仍需固定版本、代际/身份与三方核对，不覆盖未确认数据，不推导删除。
- 验收：真实 SQLite 与 Windows 原生覆盖已删除排除条目/部分占位下的有序停止与导出、显式恢复、失败原因可见；真实 agent→CFAPI→HTTPS 回归；独立复审。
- 交接：`agent/tasks/S199.md`，通过后提交、合 master、清理本卡树/分支；无服务端产物更改不部署，不发行。

### S196 — 统一默认排除与绑定附加规则

- 依据：2026-11-11 用户要求默认加载内置清单，桌面原「完整排除清单」只编辑额外规则；直接前置 S018/S061/S082 已完成。
- 设计：design §6、§10.4、§18.1；内置规则在前、绑定附加规则在后，空文本仍排除 `~$*` 等临时文件，保留 `!` 覆盖及父目录限制。
- 实现：统一运行时同步、模式补齐、规则解释与保存校验的有效规则；已有绑定安全采用默认策略，排除不删除两端数据，取消排除仍经版本围栏和三方重协调；桌面明确「额外排除规则」，schema 契约与生成代码同步。
- 验收：纯规则与真实 SQLite 回归覆盖默认临时文件、最终文件、附加/否定、已有绑定/重启、父目录及未确认内容保护；宿主/前端检查、Windows 交叉检查与可用客体原生验收，独立复审。
- 交接：`agent/tasks/S196.md`，通过后提交、合 master、清理本卡树/分支；无服务端产物更改不部署，不发行。

### S195 — 解除绑定后释放同步目录占用

- 依据：2026-11-11 用户报告 Windows 已解绑目录重绑仍 PATH_OVERLAP；直接前置 S192/S193/S194 已完成。
- 设计：design §6、§16.1、ADR-034；活动根与持久恢复记录分离，不删除旧缓存/记录或放宽活动/内部路径隔离。
- 实现：解绑成功前停止排空原生 writer，按 durable detached 检查精确物理根重用；落库前核对并无损转换完整旧占位、注销旧注册；部分/外来/未知内容拒绝且不落新行；启动、释放和维护消费者一致，旧恢复写入口围栏。
- 验收：宿主相关测试、Windows 交叉严格检查、真实 NTFS/CFAPI 回归（完整/部分、活动父子/相等、旧缓存/状态、重启）与真实 agent→IPC→HTTPS 新绑定同步、独立复审。
- 交接：`agent/tasks/S195.md`，验收后提交、合 master、清理本卡树/分支，并重建开发客户端；无服务端部署或远端发行。

### S197 — 同步与传输记录显示客户端主机名及属性

- 依据：2026-11-11 用户要求客户端显示运行主机名，操作系统、IP 为客户端属性；前置 S172/S170/S191 done，design §18.1/§24。
- 范围：S197-a 登录声明与 schema21 持久属性、授权活动读取及可选 wire/IPC 字段；S197-b Windows 原生采集与两端详情；S197-c 同源验证、部署和交接。顺序实施同卡。
- 边界：主机名/系统是非可信客户端声明，IP 为服务端观察首次登录来源；刷新保持快照，旧数据及浏览器缺少主机名显示未知，不猜服务器主机名、不反写历史。
- 验收：迁移备份/恢复及旧二进制围栏、认证/授权/刷新/账本回归、生成一致、两端类型构建及浏览器；Windows 原生采集及 agent 登录→HTTPS→账本→IPC；服务端验收后本机部署健康验证。
- 交接：agent/tasks/S197.md；本地提交、合 master 并清理，不发行推送。

### S198 — 统一 Lucide React 通用图标并保留 FileIcon

- 依据：2026-11-11 用户要求采用 Lucide React，保留现有 FileIcon，上传和下载用图标代替；前置 S183/S197 done，design §18.1。
- 范围：S198-a 接入锁定依赖、共享图标及两端导航/主题；S198-b 保留文件目录 FileIcon，将文件通用操作及两端传输方向/客户端下载入口改用 Lucide。顺序实施同卡，不改同步、授权或协议语义。
- 验收：根 typecheck、两端 build、Web 既有测试/桌面 activity、Chromium 文件操作与两端布局/传输方向，检查中文可访问名称、提示、禁用状态及 CSP；更新 Nix 前端依赖摘要并部署本机 Web，验证健康与静态资源。
- 状态：done；交接 agent/tasks/S198.md。验收完成并部署本机 generation276；本地中文提交、合 master 并清理本卡树/分支，不远端发行推送。

### S200 — 桌面程序配置与可选关闭 agent

- 依据：2026-11-11 用户要求专门的程序配置页面，关闭主窗口可选择最小化到托盘（默认）或完全退出，完全退出可选择自动关闭 agent（默认开）；直接前置 S167/S185/S190 已完成，本卡 ready。
- 设计：design §6、§18.1、§24 ADR-035；配置由桌面壳按用户持久化，不加入同步协议或在线数据库。托盘显式退出遵循停止 agent 偏好，自动更新保留原有受控退出流程。
- 范围：S200-a 程序配置页面与原生设置桥；S200-b 原生关闭、托盘恢复、有序停止及失败保留现场；S200-c 浏览器与 Windows 原生验收、开发客户端交付。子批次同卡顺序整合。
- 验收：默认值/保存重启/损坏和写失败、agent 离线可配置、两种关闭及停止开关、并发退出与失败可见；宿主 Rust、类型/桌面构建、严格 Windows 交叉检查和真实交互会话窗口/托盘/agent 存活与停止，不用强杀伪造成功。
- 交接：`agent/tasks/S200.md`；验收交接完成后本地提交合 master 并清理；无服务端产物变化不部署，不发行。
- 状态：done；宿主/浏览器及 Windows 生产客户端 106 项配置关闭验收、62 项启动回归通过，验证与限制见交接。

### S201 — 两端 Vault 及绑定选择默认选中

- 依据：2026-11-11 用户要求 Windows 客户端与网页需要选择 Vault 的控件默认选中一个；前置 S178/S190/S192/S198/S200 done，本轮优先处理明确请求。
- 设计：design §18.1；按列表顺序默认首项，保留有效手选；无授权/加载失败保持空与操作围栏，不选择其他服务器的旧结果，不改 wire/schema。
- 实现：桌面默认服务器、授权 Vault 初次自动读取及首项/撤权回退，文件/历史/传输绑定默认首项；Web 文件浏览清单就绪即选首项，保留显式 URL，复验已有恢复/传输默认选择。
- 验收：根类型、两端构建、相关前端与 Chromium 回归、Windows 原生默认读取/绑定及本机网页部署健康；交接 `agent/tasks/S201.md`。

### S202 — 两端同步与传输记录自动刷新

- 依据：2026-11-11 用户要求网页及桌面记录无需手动刷新；前置 S172/S197/S201 done，优先本次明确请求。
- 设计：design §18.1；可见时串行 2 秒轮询当前页，恢复可见/聚焦立即刷新，隐藏暂停；保留手动首页刷新、详情和失败前结果，隔离迟到响应，不改变观察事实或授权。
- 范围：共享刷新调度、两端记录页接入及直接相关测试；无 API/IPC/schema 变化。
- 验收：调度单测、根类型及两端构建、Chromium 新记录/分页/详情/错误恢复/取消与 Windows 原生记录页；部署本机 Web 并健康验证，交接 `agent/tasks/S202.md`。
- 状态：done；调度/浏览器及Windows原生记录页通过，generation278上线，开发客户端已交付；验收与限制见交接，本地中文提交合master清理，不发行推送。

### S203 — 桌面自动停止 agent 退出耗时修复

- 依据：2026-11-11 用户报告开启自动关闭 agent 后长期停在「正在停止 agent」。前置 S200/S201 done，本轮明确请求优先；design §6、§18.1、§24。
- 范围：S203-a 普通退出使用独立认证有序停止，保留原地数据/队列/恢复状态及暂停策略；升级/卸载和显式 stop 继续完整导出。S203-b 桌面等待上限及诊断；S203-c Windows 原生自动退出/数据保全与开发客户端交付。
- 验收：协议生成/契约、宿主退出监督、Windows 严格 clippy/构建、真实 Windows 桌面退出与有绑定未确认数据重启保全；不强杀 agent、不伪称已停机。仅客户端产物，无服务端部署与远端发行。
- 交接：`agent/tasks/S203.md`；验收后中文提交、合 master 并清理任务树/分支。

### S204 — 绑定文件的统一 Explorer 右键入口

- 依据：2026-11-12 用户要求绑定目录下文件提供一级 vaultsyncd、二级查看文件历史/释放空间等常用操作，授权子 agent；直接前置 S069/S083/S185/S190/S203 done，本轮明确请求优先，ready。
- 设计：design §6、§18.1、§24；仅当前用户注册，按绑定路径边界筛选单文件；路径为数据，启动固定桌面程序，不经脚本解释器。操作重新从认证 agent 解析 BindingId/EntryId，拒绝解绑、身份变化和越界；不直接访问在线数据库。
- 子批次：S204-a 原生菜单注册/清理与路径限定；S204-b 文件专用窗口和现有 IPC 安全操作；S204-c 宿主/浏览器、Windows Explorer 原生验证与开发客户端交付。
- 验收：绑定内/外、同前缀目录、特殊字符、移动/解绑/重启/程序目录移动、历史分页、下载、释放确认与 dirty/busy/full 保护，现有根类型与桌面构建、Windows 严格检查。用户已选择免安装兼容的Windows 11「显示更多选项」，不用注册表值检查代替 Explorer 实际展示。
- 交接：`agent/tasks/S204.md`；本地中文提交，验收交接后合 master 并清理本卡树/分支；仅客户端改动，无服务端部署、远端发行。

- S204状态：done；最终干净f840e78交付字节Windows98项、原生路径4项和Chromium8组等通过，证据及限制见交接；开发dist已交付，不远端发行/部署。
