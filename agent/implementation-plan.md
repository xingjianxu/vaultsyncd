# vaultsyncd 逐步实施手册

版本：S199 精简版 · 2026-11-12。实际状态、阻塞和下一 ready 只以 [进度表](progress.md) 为准。

## 1. 文件放置与使用方式

`AGENTS.md` 是执行入口；产品语义在 `docs/design.md`，接口在 `docs/architecture/interfaces.md`，环境/操作在 `docs/environment.md` 与 `docs/operations/`。本文件只保留任务关键结论和未完成卡要求，交接留在 `agent/tasks/Sxxx.md`，不重复维护设计。路径均相对于仓库根目录。

精简前完整计划：`git show 31e226214940eb0805ac9e2b6a9bff7896f923bb:agent/implementation-plan.md`。此固定版本保留原依赖、设计章节、实现要点、验收及 T01—T36/DEV 对照；历史目标不覆盖后来已确认决定。Git 外原始证据、备份和回退引用仍须独立保留，不能由该归档恢复。

## 2. 给每个 agent 的工作约定

### 2.1 一次只执行一张任务卡

按当前进度选择一张 ready 卡；状态为 todo/in_progress/blocked/done。先核对 Git diff、直接依赖短交接、相关源码和卡指定设计章节，保留他人修改。跨模块或超预算可拆 Sxxx-a/b，父卡须全部验收后才能 done；不可缩减标准。

### 2.2 上下文预算

首次建议 6,000—10,000 tokens；用 `python3 tools/context.py card Sxxx` 与 `design <小节>` 精确提取，不默认全文读长文档。续接先看任务检查点与 diff；历史“下一 ready”不代表当前状态。

### 2.3 每步统一完成条件

实现行为和错误分支，执行受影响消费者必要验证，更新本卡交接/进度与变化的接口索引。按 AGENTS.md 使用 commit skill，中文 summary 和 body；只暂存本任务文件，不推送。未实际执行的验证不能写通过；done 必须包含所需 Windows 原生证据、独立复审（卡有要求时）、交接与收尾。

### 2.4 环境、状态与数据隔离

项目命令用锁定 devShell：`nix develop`。所有开发状态、缓存、VM、测试卷、下载、证据、备份均在 `/data/vaultsyncd-dev/` 下按任务/run_id 隔离；正式 `/data/vaultsyncd`、`/var/lib/vaultsyncd` 保持。服务端工作目录/状态使用合规 btrfs；故障仅作用于已核对的 run_id 实例，不把挂载、权限或扫描故障当删除。生产/dev 边界与地址见 AGENTS.md、`docs/operations/dev-server.md`。

### 2.5 NixOS 的工程规则

项目 flake 提供工具/产物，主机配置入口需现场核对。项目所需配置与 rebuild 已授权；保存 diff/旧 generation，build、test、健康检查后 switch，保留 SSH 和恢复路径；test 会激活且没有定时自动回滚。用户最新决定（2026-11-13，S219）：通过验收的服务端/Web 代码更改只自动更新 dev（`https://wlab.132140.xyz:15401`，`tools/dev-server.sh` 指定本次验收包），完成健康验证和交接；不自动更新 prod（`https://vsyncd.132140.xyz`）。生产部署须有用户另行明确要求，既有 rebuild 授权和历史 prod/dev 同包验收不构成自动更新生产的依据；不得为任务收尾更新生产 pin、激活或重启生产服务。纯客户端/文档不触发部署，不顺手升级系统。

### 2.6 Windows 验证资源：优先 Quickemu

首发仅 Linux x64 服务端与 Windows 11 x64 客户端。宿主 `nix develop .#windows` 交叉编译 Rust/原生测试，前端同样在宿主构建；客体本地 NTFS 做真实 CFAPI、GUI、Explorer 与安装/恢复验收，不默认安装完整编译工具链。复用现有合规 Quickemu 客体；检查 KVM、容量、会话、端口与进程，保留运行现场。构建同源产物、传输后核对 SHA-256；Wine/fake/交叉成功不能替代 Windows 原生。具体命令与拓扑唯一维护在 `docs/operations/windows-lab.md` §12；缺资源记录 blocked 与恢复条件，不降低目标。

### 2.7 代码开发使用 Git worktree

涉及代码、测试、脚本、schema 或构建配置，编辑/构建前先按 AGENTS.md 的 worktree 规则和已安装 `using-git-worktrees` skill 隔离；复用本任务树，不嵌套、不自动 stash。任务树在 `.worktrees/`，分支含任务号；无法隔离则停止代码修改，可继续只读/文档。交接记录路径、分支、基线、验证与实际提交；验收交接后合 master，确认干净且已合入后清本卡本地树/分支，保留其他任务现场。

## 3. 可直接复用的启动提示词

执行一张 ready 卡：读 AGENTS.md、本手册 §1—2、当前卡、指定 design 小节与直接依赖短交接；先查状态/diff/worktree，再实现、验证、交接、提交、合并和清理。只报告真实结果、限制与下一 ready。

## 4. 任务索引

以下保留原 Sxxx 标题供 `tools/context.py` 定位，已完成卡只存关键结论和交接入口；范围被后卡覆盖时以最新已确认决定与实际验收为准。未完成卡保留原验收；当前阻塞/恢复条件见进度表及本卡交接。

## 5. 逐步任务卡

### S226 — 桌面启动自动恢复绑定与服务器悬停详情

- **状态与前置**：done（用户2026-11-13新增）；S203/S221。
- **设计**：design §18.1、§6、§24；S203普通停止策略与本机认证IPC保持。
- **范围与验收**：主桌面首次读到完整状态后，通过已有ResumeBinding恢复所有未解绑的暂停绑定；每次启动只执行一轮，手动暂停不被轮询/导航恢复，失败逐项可见且继续其它绑定。移除本地Binding表格Vault列，以Mantine HoverCard在服务器列展示地址/ServerId/VaultId/登录状态。相关类型/单测/构建、真实Chromium交互及Windows原生启动/退出/CFAPI恢复验证；无服务端变更，不部署。交接后合master并清理本卡树。
- **交接**：[S226](tasks/S226.md)；实现71df0ab，Chromium专项/绑定/布局、Windows原生130项与服务器字节及原客户端恢复通过，两个测试注册无损清理并保留原62项。

### S225 — 补齐两端同步与传输记录的文件操作

- **状态**：done；用户 2026-11-13 新增，前置 S172/S197/S202。
- **设计**：design §18.1、ADR-029、§6 不变量；接口 S172/S225。
- **范围与验收**：核实并补齐新建目录、移动/重命名、删除、历史版本恢复及回收站恢复的已提交事实，两端显示与筛选；与上传/下载分开，保留账本历史，不反推旧事件。验证迁移、事务/重试/授权、两端构建与真实浏览器和 Windows IPC 链路；仅更新 dev，保留 prod 与永久客户端。
- **交接**：[S225](tasks/S225.md)；实现91bd3b5/品牌整合7335ab0/夹具75698d4；服务端与两端浏览器、Windows原生51项及安全清理通过，仅dev更新并验证schema22/旧记录/身份/资源保持，prod未变。

### S224 — 加深透明 Logo 中央 V 并扩充主体

- **状态与前置**：done（用户新增）；S223，design §18.1。
- **范围与验收**：默认透明 SVG 中央 V 使用深色、主体贴近画布边缘且不裁切，重生七尺寸 ICO，保留黑底资源；编译完整 Windows x64 客户端，验证浏览器与 Windows 原生资源；仅部署本地 dev 并健康验证。
- **交接**：[S224](tasks/S224.md)；实现8b112ef，主体约72%→98%、V深石板蓝；纯包81项/浏览器八组、干净完整Windows release构建/清单及原生图标通过，根dist已交付；仅dev已更新并通过136资源/真实登录与展示验证。

### S223 — 统一透明背景品牌并保留黑底版本

- **状态与前置**：done（用户新增）；S183/S222，design §18.1。
- **范围与验收**：品牌集中 `assets/branding/`，保留原黑底 SVG/ICO；Web/favicon 与 Windows EXE/窗口/托盘/Explorer 使用透明资源。验证七尺寸 alpha、原版字节保留、两端构建与真实浏览器及 Windows 消费；按 S219 仅更新 dev 并验证。
- **交接**：[S223](tasks/S223.md)；实现c0cf31b/87e61a3，根类型/Web九套/两端构建、Windows严格检查/Win32七尺寸/EXE像素及真实Explorer、精确Nix包81项/浏览器八组通过；仅dev已更新，136资源和真实登录验证通过，prod保持。

### S222 — Explorer 云文件夹入口显示绑定目录名与软件图标

- **状态与前置**：done（用户新增）；S129/S183，design §14.2、§18.1。
- **范围与验收**：Windows Explorer 侧边栏同步根使用各绑定本地目录名称，不再统一显示 vaultsyncd；图标使用根 logo 的现有 Windows ICO。已有绑定随正常启动原位刷新，不注销、不改变 Binding/Vault/占位身份、驻留与未确认内容；图标不依赖便携程序原路径，不写入同步目录。宿主相关检查、Windows 交叉严格检查及真实 Windows 新旧/多绑定、Unicode 名称、持久图标和重启保留验证，独立复审；不改服务端、不部署或发行。交接后合 master 并清本卡树。
- **交接**：[S222](tasks/S222.md)；实现5571e2e，宿主43项/严格检查、Windows严格检查/agent构建、原生图标注册7项与CFAPI2项、最终精确代码三绑定/旧入口/便携移动及真实Explorer名称与Logo验收通过，独立补审APPROVED；不替换永久客户端、不部署或发行。

### S221 — 桌面同步目录使用添加弹窗并打开本地目录

- **状态与前置**：done（用户新增）；S190/S201，design §18.1。
- **范围与验收**：将桌面内联添加表单改为按钮触发的对话框；保留按服务器选择授权 Vault、原生目录选择、模式与创建保护。取消不创建，成功关闭，失败保留输入，忙时阻止关闭与重复操作。Binding 第一列路径可点击及键盘打开本机默认文件管理器；原生桥仅接收 BindingId，经认证 agent 状态查活动绑定，不接受任意路径/命令，失败如实提示。根类型/desktop build/activity、真实 Chromium 弹窗与回归、Windows 交叉检查和原生目录打开及绑定流程验收、独立复审；不改服务端、不部署、不发行。交接后合 master 并清理本卡树。
- **交接**：[S221](tasks/S221.md)；实现78d0110/ACL修复55f2217，根类型/桌面/宿主/交叉检查、Chromium及独立补审通过；真实Windows S081完整链路、Explorer精确目录窗口及原客户端恢复通过，无服务端更改。

### S220 — 按文件后缀显示 Web 文件类型图标

- **状态与前置**：done（用户新增）；S198/S214，design §18.1。
- **范围与验收**：仅服务端 Web 文件浏览器按文件名后缀显示常见文档、PDF、表格、演示、图片、音视频、压缩包、代码等类型图标；大小写不敏感，未知或无后缀回退默认，目录与操作图标不变。不读取文件内容，不改变预览/操作准入、协议或 schema。根类型/Web 测试/build、真实 Chromium 图标/布局/交互检查、独立复审及纯 Nix 包构建；仅更新 dev 并验证健康/静态字节和原身份，prod 不动，交接合 master 清理本卡树。
- **交接**：[S220](tasks/S220.md)；实现e29c134、浏览器测试2821833，根类型/Web九套/build/独立复审、精确包完整浏览器及专项七组通过。仅dev更新，136资源字节、身份/配置及线上浏览器通过，prod进程/system/pin保持。

### S219 — 记录服务端任务仅自动更新开发服务器的约定

- **状态与前置**：done（用户最新明确决定，2026-11-13）；纯文档维护。
- **范围与验收**：更新 agent 入口、实施手册与 prod/dev 运维文档，覆盖旧自动生产部署约定；历史部署事实保留，不改变代码或运行实例。核对文档一致性及 Git diff。
- **交接**：[S219](tasks/S219.md)；后续服务端/Web 任务仅自动更新 dev，prod 须用户另行明确要求。

### S218 — 修复文件预览关闭动画残影

- **状态与前置**：done（用户新增）；S217/S215/S210/S209，design §18.1。
- **范围与验收**：服务端Web预览关闭时立即取消在途请求并阻止迟到更新，标题与已渲染正文/布局保留至Mantine退出动画结束后释放，不以定时器猜测时长；导航、会话失效、卸载保持安全清理，快速重开不受旧回调影响。根类型/Web测试/build、真实Chromium图片/PDF/文本/Office动画中间帧与取消/重开回归、独立复审；纯Nix构建并部署prod/dev，保持身份/配置并核验健康/资源，提交交接合master清树。
- **交接**：[S218](tasks/S218.md)；实现a6a23de已部署generation291，prod/dev同包、精确包52组与18退出/重开109帧、408次字节及健康/身份核验通过，独立复审APPROVED。

### S217 — 缩紧文件预览标题与正文间距

- **状态与前置**：done（用户新增）；S216，design §18.1。
- **范围与验收**：仅调整服务端Web文件预览，缩小标题栏底部留白，加载完成时不保留空操作行；保持取消/重试/下载/关闭行为。根类型、Web测试与构建，真实Chromium图片/PDF明暗320/1440间距及文本/Office回归，部署prod/dev并核验健康、资源与身份。
- **交接**：[S217](tasks/S217.md)；实现df87e02已部署generation290，prod/dev同包，精确包三套预览17+13+16组、408次字节与健康/身份核验通过。

### S216 — 简化文件预览标题栏操作

- **状态与前置**：done（用户新增）；S215/S210/S209，design §18.1。
- **范围与验收**：移除预览正文“关闭”按钮，保留标题栏关闭叉号；原件下载改为标题栏图标按钮，保留可访问名称、悬停提示、禁用条件与原下载逻辑。核验焦点/关闭取消、长标题及窄屏布局，图片/PDF、文本、Office浏览器回归，构建后部署prod/dev并保持身份。
- **交接**：[S216](tasks/S216.md)；实现b62e45c已部署generation289，prod/dev同包，精确包三套预览回归及线上健康/身份/408次字节核对通过。

### S214 — Web 文件列表按列排序

- **状态与前置**：done（用户新增）；S213/S132/S122，design §18.1。
- **范围与验收**：名称、大小、修改日期标题切换升降序，默认名称升序；目录优先，缺失值后置，原始整数精确比较及稳定同值顺序；筛选后分页前排序，切换回第一页，目录树不受影响。验证单元、键盘/分页/筛选/导航与真实 Chromium，构建并部署 prod/dev，健康和身份保持。
- **交接**：[S214](tasks/S214.md)；实现bf8acc6已部署generation287，prod/dev同包、精确包Chromium/408次字节核对与健康通过。

### S001 — 确认主机与同步环境基线

关键结论/范围：只读核查 Git、NixOS/Nix、x86_64 架构、btrfs 挂载、空间、配置入口、端口、KVM 和权限，完成开发主机基线与全任务索引。交接与实际验证：[S001](tasks/S001.md)。

### S002 — 建立 agent 入口与上下文提取工具

关键结论/范围：建立零第三方依赖的 `tools/context.py`，按编号或标题提取任务卡/design 小节。交接与实际验证：[S002](tasks/S002.md)。

### S003 — 建立 Nix 开发 shell

关键结论/范围：建立根 `flake.nix`/`flake.lock`，锁定 nixpkgs，提供 Rust、Node/pnpm、Python、SQLite 等开发工具。交接与实际验证：[S003](tasks/S003.md)。

### S004 — 建立最小 Cargo workspace

关键结论/范围：建立最小 Cargo 虚拟 workspace，仅按实际需要加入 domain 包。交接与实际验证：[S004](tasks/S004.md)。

### S005 — 建立 pnpm workspace 与工具版本

关键结论/范围：建立根 pnpm workspace、唯一 pnpm-lock.yaml 与 UI 包类型检查入口，根 private package.json 锁定 packageManager/engines。交接与实际验证：[S005](tasks/S005.md)。

### S006 — 建立本地任务编排与检查入口

关键结论/范围：建立 `tools/xtask` 与 cargo 别名。交接与实际验证：[S006](tasks/S006.md)。

### S007 — 准备独立 btrfs 测试位置

关键结论/范围：建立源码外、按 run_id 管理的 btrfs loop 测试卷工具 `tools/test-volume/test-volume.sh`，提供 create/verify/cleanup/status。交接与实际验证：[S007](tasks/S007.md)。

### S008 — 准备 Windows 客体与连接说明

关键结论/范围：建立 quickemu devShell、`tools/windows-lab/` 模板和连接说明，使用微软官方评估版 ISO 完成 Windows 11 x64 客体安装。交接与实际验证：[S008](tasks/S008.md)。

### S009 — 固定 Windows 交叉编译与客体验证工具链

关键结论/范围：固定宿主 Windows x64 交叉编译与客体运行分工：`nix develop .#windows`、cargo-xwin、MSVC CRT/SDK、x86_64-pc-windows-msvc 及静态 CRT。交接与实际验证：[S009](tasks/S009.md)。

### S010 — CFAPI 注册与占位创建原型

关键结论/范围：建立 platform-windows CFAPI 注册/连接/占位/查询/断开/注销原型及 48 B FileIdentity 编解码。交接与实际验证：[S010](tasks/S010.md)。

### S011 — CFAPI 读取、取消与离线原型

- **环境与前置**：W；S010。
- **只读设计范围**：`docs/design.md` §13、§14.2、T06/T22；加直接前置交接及相关源文件。
- **主要落点**：crates/platform-windows/examples/；tests/platform/windows/。
- **实现要点**：用固定测试内容源实现读取回调与范围请求，固定版本；处理取消、重复请求和超时。源不可用时回报明确失败；回调生命周期不能访问已释放状态。
- **本步验收**：实际文件读取字节/哈希正确；断开源后未下载内容失败；取消后句柄可关闭，无永久卡住。
- **交接**：`agent/tasks/S011.md`，记录实际接口、验证与下一个 ready 任务。

### S012 — CFAPI 释放与写入保护原型

关键结论/范围：CFAPI 释放与写入保护原型。交接与实际验证：[S012](tasks/S012.md)。

### S013 — Linux 存储准入原型

关键结论/范围：新增 `crates/platform-linux-server` 的 `mounts/` 准入：真实 btrfs 卷上 `run-storage-admission.sh` 60/100 项通过、`run-reflink.sh` 7 项 0 失败，宿主 19 单测、clippy/fmt/metadata 全 0。交接与实际验证：[S013](tasks/S013.md)。

### S014 — Linux 普通文件发布原型

关键结论/范围：`files/` 受限句柄发布：`run-file-publish.sh` 100/100 项通过（openat2 拒绝符号链接/`..`/绝对路径、FICLONE 同卷与跨文件系统 EXDEV 回退、两步受控替换与外部竞争保留候选、对象隔离），宿主 36 单测、clippy/fmt 0。交接与实际验证：[S014](tasks/S014.md)。

### S015 — 定义身份与有界计数类型

关键结论/范围：新增 `crates/domain/src/{ids,version}.rs`：7 类不透明身份（规范文本/Debug/`FromStr` 往返、大写解析、8 条非法文本拒绝）+ `Revision`/`RootSeq`/`Generation`/`ServerEpoch` 有界计数（`i64::MAX` 与 2^53 边界往返、12 条非法文本、`next` 不回绕）。交接与实际验证：[S015](tasks/S015.md)。

### S016 — 定义状态维度与错误分类

关键结论/范围：`crates/domain/src/state.rs`：六维正交状态 + `project`/`is_synced`（`dirty+offline+materialized` 可共存，`Pending`/`BlockedByLocal`/`Conflicted` 均不判已同步）。交接与实际验证：[S016](tasks/S016.md)。

### S017 — 路径与名称纯规则

关键结论/范围：新增 `crates/fs-common`：`RelativePath::parse` 只接受规范相对分量（拒绝绝对/UNC/盘符/NUL/空分量/`.`/`..`，不自动小写不改名）。交接与实际验证：[S017](tasks/S017.md)。

### S018 — 排除规则与解释

关键结论/范围：`crates/fs-common/src/ignore.rs`（13 单测）+ `tests/ignore_rules.rs` + `fixtures/paths/`：注释/通配/根锚定/目录/否定规则并返回命中规则与被阻断规则，父目录排除+子项否定、`!build/` 重新包含、`report.docx.tmp` 排除而 `report.docx` 参与同步、相同规则的重复解析与正反查询结果确定、排除不转换成 Delete。交接与实际验证：[S018](tasks/S018.md)。

### S019 — 三方规划基本分支

关键结论/范围：新增 `crates/planner`（只依赖 domain+fs-common，无 I/O）：B/L/R 事实 + `Plan`（action/prerequisites/dependencies/local_protection）。交接与实际验证：[S019](tasks/S019.md)。

### S020 — 首次合并规划

关键结论/范围：`crates/planner/src/first_sync.rs`：`plan_first_sync` 入口 + `BindingMode`/`PreserveReason`，4×4×2 组合无一删除或覆盖、本地存在时保护永不为 `NotNeeded`、同长度不同内容与文件—目录同名保留双方、5 条 fixture 两次计算逐字段相同。交接与实际验证：[S020](tasks/S020.md)。

### S021 — 内容冲突及稳定副本身份

关键结论/范围：`crates/planner/src/conflict.rs`：服务器主条目保留 + 迟到分支副本，`Action::CreateConflictCopy`、`Dependency::ConflictCopyCommitted`。交接与实际验证：[S021](tasks/S021.md)。

### S022 — 移动与删除冲突规划

关键结论/范围：`crates/planner/src/{rename,delete}.rs`：同 Root 移动、双移动、移动+修改、跨 Root 两段任务、删除/恢复冲突与 `SubtreeToken`、旧 token 的 `SUBTREE_CHANGED` 重规划、跨 Root 目标失败不计划源删除。交接与实际验证：[S022](tasks/S022.md)。

### S023 — 规划器操作重排验证

关键结论/范围：新增 `crates/planner/tests/op_reorder.rs`（1949 行、7 项）+ `fixtures/planner/op-reorder.txt`（11 固定种子、两个初始世界）：88 个次序 / 260 检查点 / 14 种关键裁决覆盖 I02/I03/I04/I06/I07，失败贪心收缩成最短轨迹并打印 `seed` 与 `VAULTSYNCD_PLANNER_REPLAY` 重放命令。交接与实际验证：[S023](tasks/S023.md)。

### S024 — 远端协议最小契约与生成通路

关键结论/范围：`api/{openapi.yaml,generation.toml}` + `crates/protocol` + `packages/api-client` + `cargo xtask generate`/`--check`：同源生成 Rust DTO 与 TS client 双方编译、重生成无 diff。交接与实际验证：[S024](tasks/S024.md)。

### S025 — 本机 IPC 最小契约与生成通路

关键结论/范围：`api/local-ipc.schema.json`（唯一编辑源：`get_status` 闭集命令、`request_id`/`ipc_version`、响应/错误体、帧上限）+ `api/generation.toml` `format_version=2` 一次生成 5 个文件（远端 3 + 本机 2）。交接与实际验证：[S025](tasks/S025.md)。

### S026 — 服务端数据库迁移框架

关键结论/范围：服务端数据库迁移框架。交接与实际验证：[S026](tasks/S026.md)。

### S027 — 服务端命名空间表与约束

关键结论/范围：迁移 `0002_namespace.sql`（users/roots/entries、复合外键 `entries(root_id,parent_id)→entries(root_id,id)`、活跃同父名称部分唯一索引、名称/类型 CHECK）+ `roots.rs`/`entries.rs` 事务内读写接口（递归 CTE 环检查、owner scope）。交接与实际验证：[S027](tasks/S027.md)。

### S028 — 版本、事件与幂等表

关键结论/范围：版本、事件与幂等表。交接与实际验证：[S028](tasks/S028.md)。

### S029 — 上传、意图与租约表

关键结论/范围：上传、意图与租约表。交接与实际验证：[S029](tasks/S029.md)。

### S030 — 客户端数据库与状态表

关键结论/范围：新增 `crates/client-store`（独立 client schema `0001_initial.sql`：bindings/baseline/remote/local/pending_ops + 迁移与种类门禁）：同条目可同时存旧 baseline、新 remote 与本地 dirty，库种类错误拒绝。交接与实际验证：[S030](tasks/S030.md)。

### S031 — 客户端事件、传输与意图持久化

关键结论/范围：迁移 `0002_events_transfers_intents.sql`（received_changes/transfers/local_intents/notifications + `bindings.received_seq/applied_seq`）+ `events/transfers/intents/notifications.rs`：批次/远端/任务/游标同事务且游标最后推进，注入失败后事务内游标仍 0、回滚无残留。交接与实际验证：[S031](tasks/S031.md)。

### S032 — Linux 安全路径访问正式接口

关键结论/范围：`files/root.rs`（Root 句柄正式接口+快照/漂移）、`restricted.rs`（`getdents64` 句柄列举、权限/挂载边界错误分类）、`mounts/overlap.rs`（父子/别名/bind 重叠）。交接与实际验证：[S032](tasks/S032.md)。

### S033 — 不可变对象写入与读取

关键结论/范围：新增 `crates/server-core`（纯规则 + `ports::object_store` 端口，无平台 I/O）与 `platform-linux-server/examples/object-capture.rs` 组装：克隆候选→固定副本 hash/长度校验→独立暂存→持久发布。交接与实际验证：[S033](tasks/S033.md)。

### S034 — 持久提交意图状态机

关键结论/范围：`server-core/src/commit/intent.rs`（prepared/published/finalized/needs_recovery 显式 phase + before/after 对象与目标身份）与 `src/recovery/mod.rs` 恢复判定、`ports::{intent_log,target}` 端口。交接与实际验证：[S034](tasks/S034.md)。

### S035 — 内容 CAS 与现场复核

关键结论/范围：`commit/put.rs`：`PutRequest` 形状校验、`RootCommitGate` 同 Root 串行化、`commit_put` 全链路（对象持久→意图→目标暂存发布→元数据/结果事务→ACK）、`RECONCILE_REQUIRED` 重分类。交接与实际验证：[S035](tasks/S035.md)。

### S036 — 幂等重试与冲突内容提交

关键结论/范围：`commit/conflict.rs` + `ports/conflict_store.rs`：稳定 `OperationKey`/`RequestDigest` 查询，过期不同内容只造一份冲突副本并登记映射，丢响应重试逐字段相同且不新增发布/映射/意图，同 `OpId` 不同摘要→`OP_ID_REUSED`，pending 不重做，三设备三份副本各自重试拿回自己的。交接与实际验证：[S036](tasks/S036.md)。

### S037 — 启动恢复与崩溃注入

关键结论/范围：启动恢复与崩溃注入。交接与实际验证：[S037](tasks/S037.md)。

### S038 — 目录创建与同 Vault 移动

关键结论/范围：目录创建与同 Vault 移动。交接与实际验证：[S038](tasks/S038.md)。

### S039 — 可恢复删除与回收站

关键结论/范围：`server-core/src/commit/delete.rs` + `ports/{removal,trash_store}.rs` + 迁移 `0005_trash.sql`（trash_entries/tombstones）：删除复用 put/命名空间阶段机，服务端按冻结清单算 128 位 `SubtreeToken`（任一条目变化即 `subtree_changed`，T12），有界批次 ≤256 且可中断可重入。交接与实际验证：[S039](tasks/S039.md)。

### S040 — 历史与回收站恢复操作

关键结论/范围：`server-core/src/history/{version,trash}.rs` + `ports/{restore_store,content_lease}.rs`：历史按旧不可变版本创建新 `VersionId`（旧版本字节/行不变，I12），恢复期间短期内容租约保护对象，同 OpId 重放只做一次事务与一次发布，占用时另名 `report（恢复）.docx`（T13）。交接与实际验证：[S040](tasks/S040.md)。

### S041 — 服务端进程与配置生命周期

关键结论/范围：新增 `apps/server`（包 `vaultsyncd-server`，Linux-only）+ `deploy/examples/vaultsyncd.toml`：配置加载、tracing、`server-store` 库门禁、逐 Root 恢复核对、readiness/写入门与 Axum `/v1/health` 组成一个进程，先恢复再开放写入。交接与实际验证：[S041](tasks/S041.md)。

### S042 — 首次 owner 初始化与密码认证

关键结论/范围：首次 owner 初始化与密码认证。交接与实际验证：[S042](tasks/S042.md)。

### S043 — 会话、刷新与 Vault 授权

关键结论/范围：迁移 `0006_sessions.sql` + `server-store/src/sessions.rs`、`core/src/auth/{session,scope}.rs`、`apps/server/src/auth.rs`（三个端点、Bearer/cookie、CSRF、`principal_from`/`authorize_root`）+ ADR-0001 凭据载体。交接与实际验证：[S043](tasks/S043.md)。

### S044 — 管理 Vault 注册与健康 API

关键结论/范围：管理 Vault 注册与健康 API。交接与实际验证：[S044](tasks/S044.md)。

### S045 — 上传会话和固定块接收

关键结论/范围：新 `crates/transfer`（流式 SHA-256 + 固定块几何，`server-store::ChunkGeometry` 改薄封装）+ `server-core/src/api/uploads.rs` + `apps/server/src/uploads.rs`。交接与实际验证：[S045](tasks/S045.md)。

### S046 — 上传 finalize 与固定版本下载

关键结论/范围：核心 finalize/固定版本下载（幂等重放、缺块不改状态、内容不符→abort、过期→`VersionGone` 不建租约、Range 13 类输入）+ 生成器支持二进制成功响应与 `in: header`（`SuccessBody::{Json,Bytes}`、TS `Uint8Array`、`x-part-sha256` 真 header）+ `apps/server/src/content.rs` 端点。交接与实际验证：[S046](tasks/S046.md)。

### S047 — 操作 HTTP 接口与错误映射

关键结论/范围：操作 HTTP 接口与错误映射。交接与实际验证：[S047](tasks/S047.md)。

### S048 — 一致性快照、changes 与 SSE

关键结论/范围：一致性快照、changes 与 SSE。交接与实际验证：[S048](tasks/S048.md)。

### S049 — 外部目录初始稳定采集

关键结论/范围：外部目录初始稳定采集。交接与实际验证：[S049](tasks/S049.md)。

### S050 — inotify 与溢出补扫

关键结论/范围：新增 `platform-linux-server/src/watcher/`（inotify，监视已打开目录句柄，只产出 `Changed`/`Subtree`/`Overflow`/`Gap`）+ `server-core/src/observer/{hints,intents,follow}.rs`：事件只是线索，路径现状由 `WorkTreePort` 实测，观察到不存在只记录不产生删除。交接与实际验证：[S050](tasks/S050.md)。

### S051 — 外部移动、删除与挂载失效

关键结论/范围：删除只来自完整且健康扫描（区分 `complete`/`permission_denied`/`io_error`/`mount_changed`）。交接与实际验证：[S051](tasks/S051.md)。

### S052 — 服务端整合小型 HTTP 闭环

关键结论/范围：真实监听闭环 `tests/server-integration`：登录→Root→初始导入采纳→分块上传→finalize→put 200→同 OpId 重放同 revision/root_seq/version_id→快照→固定版本下载逐字节→外部改写进 changes→优雅重启后批次/epoch/版本/hash 一致（连续 3 次 3/3）。交接与实际验证：[S052](tasks/S052.md)。

### S053 — Rust HTTPS 客户端与认证刷新

关键结论/范围：新增 `crates/http-client`：`Transport` 端口 + reqwest/rustls 生产实现、`ApiClient`、`AuthSession` 单飞刷新、超时/取消/错误分类、`CredentialSink` 端口、`testing` 替身。交接与实际验证：[S053](tasks/S053.md)。

### S054 — 上传下载续传客户端

关键结论/范围：服务端缺块清单驱动续传、块摘要回执不符重传、整份摘要流式校验、有界块并发、取消保留进度。交接与实际验证：[S054](tasks/S054.md)。

### S055 — agent 核心端口与确定性执行器

关键结论/范围：新增 `crates/agent-core`：四类互不引用端口（文件/平台、时钟、远端、状态）+ 确定性执行器（先意图后动作、按插入顺序调度、失败动作与依赖保留、重试 3 次封顶、退避期内不重试、暂停不丢队列、pending 不出队）。交接与实际验证：[S055](tasks/S055.md)。

### S056 — 本地 generation 与上传快照

关键结论/范围：generation 固定快照上传、续传持久化、响应丢失用原 OpId 查询。交接与实际验证：[S056](tasks/S056.md)。

### S057 — 接收事件与应用进度

关键结论/范围：接收批次事务持久化远端/任务/received_cursor，应用完成才推进 baseline/applied。交接与实际验证：[S057](tasks/S057.md)。

### S058 — 首次绑定与可恢复合并执行

关键结论/范围：持久化固定远端快照、本地导入清单、稳定 OpId 与临时 ID 映射。交接与实际验证：[S058](tasks/S058.md)。

### S059 — 本地受控应用与重启核对

关键结论/范围：固定版本下载到暂存 + 流式校验 + 持久意图后受控替换 + 成功后基线事务。交接与实际验证：[S059](tasks/S059.md)。

### S060 — 旧游标与 epoch 重建

关键结论/范围：发布前全状态核对（代际/dirty/residency/指纹/身份），核对后新 dirty 拒发布。交接与实际验证：[S060](tasks/S060.md)。

### S061 — 暂停、模式切换与解除绑定

关键结论/范围：持久暂停/恢复与 full 补齐任务。交接与实际验证：[S061](tasks/S061.md)。

### S062 — 下载优先级与重试调度

关键结论/范围：用户读取保留槽、metadata/hash独立限流、同entry串行。交接与实际验证：[S062](tasks/S062.md)。

### S063 — 手动释放的核心安全条件

关键结论/范围：显式释放先真实内容核对、固定版本短租约与持续使用保护，持久意图后平台脱水，成功才 CAS metadata_only。交接与实际验证：[S063](tasks/S063.md)。

### S064 — agent 状态、问题与恢复导出

关键结论/范围：同事务多维状态与full证明。交接与实际验证：[S064](tasks/S064.md)。

### S065 — Windows 安全文件与本地扫描

关键结论/范围：句柄相对 NtCreateFile no-follow/no-recall、卷+128位身份重叠、句柄元数据枚举与 CFAPI 驻留事实。交接与实际验证：[S065](tasks/S065.md)。

### S066 — CFAPI 正式注册与元数据更新

关键结论/范围：正式 CFAPI adapter 使用含 CRC 的 domain v2 身份，身份不依赖路径。交接与实际验证：[S066](tasks/S066.md)。

### S067 — CFAPI hydration 接入传输执行器

关键结论/范围：真HTTP/SQLite/CFAPI固定pin整份校验后范围供给。交接与实际验证：[S067](tasks/S067.md)。

### S068 — Windows 写入检测与使用保护

关键结论/范围：WindowsFiles 以完整摘要和物理身份驱动代际，持久平台日志保全旧对象并拒绝覆盖新对象。交接与实际验证：[S068](tasks/S068.md)。

### S069 — Windows 手动释放与恢复

关键结论/范围：真实版本租约与持续使用保护跨网络/SQLite await 保持到驻留 CAS。交接与实际验证：[S069](tasks/S069.md)。

### S070 — Windows agent 进程与单实例

关键结论/范围：独立 agent 组装真实 HTTP/store/core/Windows，SID 会话互斥与状态锁先于数据库。交接与实际验证：[S070](tasks/S070.md)。

### S071 — Windows named pipe 传输与身份

关键结论/范围：命名管道按 SID/session 隔离，受限 DACL、拒绝远程并双向核对身份。交接与实际验证：[S071](tasks/S071.md)。

### S072 — 本机管理命令完整接入

关键结论/范围：IPC1.2 闭集管理命令经单 mailbox 串行执行，SQLite 为连接/绑定权威。交接与实际验证：[S072](tasks/S072.md)。

### S073 — Windows CLI 最小管理入口

关键结论/范围：新增 apps/cli 只经命名管道、21命令映射、--json与退出码0/2/3/4/5/6、密码仅安全输入。交接与实际验证：[S073](tasks/S073.md)。

### S074 — 三台客户端基础互通

关键结论/范围：真实Windows原生最终提交退出0：21份输入missing=0、14路径收敛、lost_responses=2、conflict_copies=5。交接与实际验证：[S074](tasks/S074.md)。

### S075 — Linux 在线管理 socket 与 CLI

关键结论/范围：在线管理 Unix socket 以目录句柄和 SO_PEERCRED 保护，同有效 UID。交接与实际验证：[S075](tasks/S075.md)。

### S076 — 前端公共组件与两应用壳

关键结论/范围：公共 UI 无传输依赖。交接与实际验证：[S076](tasks/S076.md)。

### S077 — Web 登录与会话管理

关键结论/范围：浏览器认证使用 HttpOnly/Secure/SameSite cookie。交接与实际验证：[S077](tasks/S077.md)。

### S078 — Web Vault 与服务健康页面

关键结论/范围：owner 专用 Root 管理展示现场健康与真实扫描报告，失败标 unavailable/blocked 且不推导删除。交接与实际验证：[S078](tasks/S078.md)。

### S079 — Web 历史、冲突与回收站

关键结论/范围：授权历史/冲突/回收站分页与保留时间展示。交接与实际验证：[S079](tasks/S079.md)。

### S080 — Tauri Windows 壳与最小 IPC 桥

关键结论/范围：Windows Tauri 壳以固定 agent_get_status 桥调用当前会话 IPC，内置来源/CSP 与最小权限。交接与实际验证：[S080](tasks/S080.md)。

### S081 — 桌面连接与 Binding 页面

关键结论/范围：固定桥完成连接、登录与授权 Root 绑定/首合并，配置和多维进度来自真实 agent。交接与实际验证：[S081](tasks/S081.md)。

### S082 — 桌面文件状态与手动操作

关键结论/范围：IPC1.3 分页 FileStatus 复用核心投影和安全扫描。交接与实际验证：[S082](tasks/S082.md)。

### S083 — 桌面历史与系统通知

关键结论/范围：IPC1.4 与 client 0012 支持历史恢复、Notice 及原子代际 claim。交接与实际验证：[S083](tasks/S083.md)。

### S084 — 客户端未上传数据恢复演练

关键结论/范围：真实未完意图和上传快照下强杀自有 agent，重启后先导出恢复包。交接与实际验证：[S084](tasks/S084.md)。

### S085 — 历史保留与对象引用标记

关键结论/范围：历史保留采用业务时间和默认 30 天规则，活动引用、pending 和备份保护内容。交接与实际验证：[S085](tasks/S085.md)。

### S086 — GC 删除协调与临时清理

关键结论/范围：GC 授权前同事务重核引用，以对象门串行协调发布/下载/恢复/备份。交接与实际验证：[S086](tasks/S086.md)。

### S087 — 一致性备份快照与租约

关键结论/范围：独立target四包573＋集成36，共609/0。交接与实际验证：[S087](tasks/S087.md)。

### S088 — 备份导出、校验与释放接口

关键结论/范围：store92/server54/admin11/core流2/管理恢复3。交接与实际验证：[S088](tasks/S088.md)。

### S089 — 离线恢复到空目录

关键结论/范围：五包591/0＋集成36/0。交接与实际验证：[S089](tasks/S089.md)。

### S090 — Web 备份管理入口

关键结论/范围：Web 备份复用稳定作业 ID 和固定清单，OPFS 有界暂存并校验逐项哈希。交接与实际验证：[S090](tasks/S090.md)。

### S091 — 协议与数据库升级演练

关键结论/范围：默认 ApiClient 在认证/数据副作用前协商 capabilities，major/缺能力明确拒绝。交接与实际验证：[S091](tasks/S091.md)。

### S092 — 构建依赖与协议边界检查

关键结论/范围：统一门禁校验三目标传递依赖、pnpm 消费者和生成文件一致性，阻止 GUI/CLI 绕过 IPC 访问存储。交接与实际验证：[S092](tasks/S092.md)。

### S093 — 发布版本与产物清单

关键结论/范围：根 workspace.package.version 为唯一产品版本源，协议/生成器身份独立。交接与实际验证：[S093](tasks/S093.md)。

### S094 — Linux 服务端 Nix 构建包

关键结论/范围：flake 分离 Rust/Web/default 构建与离线固定依赖，真实 sandbox 构建。交接与实际验证：[S094](tasks/S094.md)。

### S095 — NixOS 服务模块

关键结论/范围：NixOS 模块以非 root、零 capabilities、最小写路径运行。交接与实际验证：[S095](tasks/S095.md)。

### S096 — 在本机启用隔离服务实例

关键结论/范围：在实际主机 flake 锁定永久 Git 源部署隔离实例，保存原 dirty lock 和恢复 generation。交接与实际验证：[S096](tasks/S096.md)。

### S097 — HTTPS 与同源 Web 部署

关键结论/范围：Caddy TLS 回环为 Web/API 提供同源入口，覆盖块上传上限、SSE 和头部伪造负例。交接与实际验证：[S097](tasks/S097.md)。

### S098 — 通用 Linux 包与容器配置

关键结论/范围：统一来源构建 Ubuntu 可搬移包、容器与对应源码归档，记录私有 loader/依赖和完整 receipt。交接与实际验证：[S098](tasks/S098.md)。 历史范围已由 S169 覆盖，当前契约见该卡与 AGENTS.md。

### S099 — Windows 安装包与后台启动

关键结论/范围：锁定 NSIS 的 Windows 11 x64 每用户测试安装包，程序/LocalAppData 状态分离。交接与实际验证：[S099](tasks/S099.md)。 历史范围已由 S185 覆盖，当前契约见该卡与 AGENTS.md。

### S100 — Windows 升级与卸载保护

关键结论/范围：IPC1.5 维护准备先暂停/排空/导出再退出，持 writer 锁备份程序和状态后迁移，失败回退。交接与实际验证：[S100](tasks/S100.md)。

### S101 — 本地平台矩阵与安装验证

关键结论/范围：tools/ci/。交接与实际验证：[S101](tasks/S101.md)。

### S102 — 事件异常与旧设备集成验收

关键结论/范围：干净树原生六场景全过（Windows11/NTFS/CFAPI）：T14丢/重/乱序后received=applied且宿主外部写入落盘。交接与实际验证：[S102](tasks/S102.md)。

### S103 — 文件与挂载故障集成验收

关键结论/范围：七类故障实测：外部写竞争修复put静默覆盖（409 `RECONCILE_REQUIRED`、外部内容保留）。交接与实际验证：[S103](tasks/S103.md)。

### S104 — GC、备份与崩溃联合验收

关键结论/范围：故障停车仅为默认关闭的测试 helper，不新增产品故障路由。交接与实际验证：[S104](tasks/S104.md)。

### S105 — 安全与操作可达性验收

关键结论/范围：补齐 IPC1.6 排除规则真实入口与逐路径围栏，取消排除保留三方基线。交接与实际验证：[S105](tasks/S105.md)。

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

关键结论/范围：同一次 fresh release 产出 NSIS+ZIP。交接与实际验证：[S109](tasks/S109.md)。 历史范围已由 S185 覆盖，当前契约见该卡与 AGENTS.md。

### S110 — 一键构建多平台 release 归档

关键结论/范围：`tools/release.sh` 自动进 devShell，由 `packaging/release.py` 统一 Linux 双架构 portable 和 Windows x64 ZIP，默认 `dist/releases/v<版本>-<提交12位>`。交接与实际验证：[S110](tasks/S110.md)。 历史范围已由 S169 覆盖，当前契约见该卡与 AGENTS.md。

### S111 — 移除 Linux ARM64 构建与验证范围

关键结论/范围：S111-a 移除 release/Nix/容器/ABI 的 ARM64 路径。交接与实际验证：[S111](tasks/S111.md)。

### S112 — 本机域名部署与 NixOS feature 配置

关键结论/范围：正式入口 https://vsyncd.132140.xyz，主机generation244、服务端0.1.1。交接与实际验证：[S112](tasks/S112.md)。

### S113 — 主机用户运行与 pure 一键部署

关键结论/范围：模块支持既有非 vault 账号且不改写其属性。交接与实际验证：[S113](tasks/S113.md)。

### S114 — 客户端目录选择与 Vault 名称显示

关键结论/范围：本地目录通过 Windows 原生单目录选择器选择，只读显示，取消保留原选择，未选择时禁止创建。交接与实际验证：[S114](tasks/S114.md)。

### S115 — 服务端 Web 的 HeroUI React 完整重构

关键结论/范围：保持五个管理入口与现有 API/认证/恢复/备份语义，采用 HeroUI v3、Tailwind v4 与 React 重构 Web。交接与实际验证：[S115](tasks/S115.md)。

### S116 — 将当前服务端部署到本机

关键结论/范围：通过现有一键 pure 部署入口将当前已提交版本固定到 `/etc/nixos#wks-lab`，保留现有域名、账号、ServerId、VaultId 与正式数据，不更新主机依赖。交接与实际验证：[S116](tasks/S116.md)。

### S117 — Web 管理界面紧凑化与 HeroUI 控件统一

关键结论/范围：保持五个管理入口与业务语义，统一导航、面包屑、按钮、表格、表单与弹层的密度。交接与实际验证：[S117](tasks/S117.md)。

### S118 — 将最新服务端（S117 之后）部署到本机

关键结论/范围：用现有一键 pure 入口把部署时 master HEAD 固定到 `/etc/nixos#wks-lab`，使 S115/S117 上线。交接与实际验证：[S118](tasks/S118.md)。

### S119 — 服务端 Web 从 HeroUI 迁移到 Mantine

关键结论/范围：五个管理入口全部采用 Mantine，删除 HeroUI 及专用依赖/样式，保留 React、生成 API 与既有业务语义。交接与实际验证：[S119](tasks/S119.md)。

### S120 — 将 Mantine 版最新服务端部署到本机

关键结论/范围：用现有一键 pure 入口把部署时 master HEAD 固定到 `/etc/nixos#wks-lab`，使 S119 的 Mantine Web 与逐响应 style nonce CSP 上线。交接与实际验证：[S120](tasks/S120.md)。

### S121 — Web 顶栏分区与 48px 高度并部署到本机

关键结论/范围：`apps/web/src/App.tsx` 与 `styles.css` 将 AppShell 顶栏改为48px。交接与实际验证：[S121](tasks/S121.md)。

### S122 — Web Vault 文件浏览器与属性查看

关键结论/范围：Mantine 文件浏览器路由，工具栏 Vault 选择、目录面包屑与返回上级、刷新、当前目录名称筛选及列表分页。交接与实际验证：[S122](tasks/S122.md)。

### S124 — Web 文件浏览器左右分栏与 URL 目录定位

关键结论/范围：主体填满可用空间。交接与实际验证：[S124](tasks/S124.md)。

### S125 — 将最新服务端（S124 分栏文件浏览器）部署到本机

关键结论/范围：用 S113 的一键入口把部署时 master HEAD 固定到本机，使 S124 的左树右列表文件浏览器与 URL 目录深链上线。交接与实际验证：[S125](tasks/S125.md)。

### S126 — 精简已完成任务的过程记录

关键结论/范围：删除 126 份已完成任务的拆卡、排障和批次过程文件，保留全部父卡。交接与实际验证：[S126](tasks/S126.md)。

### S127 — 使用 Mantine Tree 重构文件浏览器目录树

关键结论/范围：从 Files.tsx 拆出 DirectoryTree.tsx、CSS module 和共享 FileIcon。交接与实际验证：[S127](tasks/S127.md)。

### S128 — 收紧文件浏览器目录树并部署生产

关键结论/范围：仅用开合文件夹图标表示展开状态，保留图标独立展开、文字导航与原生键盘语义。交接与实际验证：[S128](tasks/S128.md)。

### S129 — 接通正式 Windows agent 的 Explorer 占位状态图标

关键结论/范围：runtime 共用 assemble 在恢复核对后、切换内容供给前注册 Explorer，覆盖旧 CFAPI-only 根、新增绑定和重启。交接与实际验证：[S129](tasks/S129.md)。

### S134 — 服务端 Web 统一 Mantine Notification 操作反馈

关键结论/范围：`apps/web/src/components/Feedback.tsx` 提供 `showFeedback` 与 scoped `Feedback`。交接与实际验证：[S134](tasks/S134.md)。

### S136 — Web 文件管理器上传与右键文件操作

关键结论/范围：`#/files` 支持工具栏上传/当前目录粘贴、行与空白区右键菜单（打开/属性/复制路径/复制/剪切/粘贴/删除）、删除确认与回收站语义。交接与实际验证：[S136](tasks/S136.md)。

### S135 — 将通知版服务端部署到本机

关键结论/范围：固定 S134 已验收提交，使用现有 pure 部署入口 build → dry-activate → test → 健康 → switch，仅更新主机项目 pin。交接与实际验证：[S135](tasks/S135.md)。

### S137 — Web 文件列表重命名与目录树右键菜单

关键结论/范围：文件/目录右键重命名走原父目录CAS move，保EntryId/VersionId，重试/刷新复用原OpId与恢复日志。交接与实际验证：[S137](tasks/S137.md)。

### S138 — 将文件操作与重命名版服务端部署到本机

关键结论/范围：固定已验收源版本，使用现有 pure 部署入口 build → dry-activate → test → 健康 → switch，仅更新项目 pin。交接与实际验证：[S138](tasks/S138.md)。

### S139 — 重命名默认选区排除文件扩展名

关键结论/范围：文件仅选中最后一个非首位点号之前的名称，目录全选。交接与实际验证：[S139](tasks/S139.md)。

### S145 — 统一 Web 登录入口并部署本机

关键结论/范围：顶层会话门禁，初始化核验不显示管理壳。交接与实际验证：[S145](tasks/S145.md)。

### S146 — 清理闲置目录并集中临时任务数据

关键结论/范围：核对挂载及所有进程的cwd/exe/fd后清理36项，包括S097/S098编译缓存、退役ARM64镜像/loop测试卷、6个S075任务目录、S096试用卷及3处旧凭据/开发证书目录。交接与实际验证：[S146](tasks/S146.md)。

### S147 — 记录客户端连接并提供 Web 查看入口

关键结论/范围：持久化每次成功登录的连接、设备标签、接入类型、首次登录及最近认证请求时间。交接与实际验证：[S147](tasks/S147.md)。

### S148 — Web 登录后自动连接服务

关键结论/范围：`AppView` 在认证门禁通过后自动检查同源 `GET /v1/health`，适用于登录、刷新恢复会话及深链。交接与实际验证：[S148](tasks/S148.md)。

### S149 — 将客户端连接记录及自动连接版服务端部署到本机

关键结论/范围：固定当前已提交源d9cfee9，保全正式数据及身份，完成schema17→18升级、本机HTTPS与同源Web部署。交接与实际验证：[S149](tasks/S149.md)。

### S151 — 正式 Vault 基目录迁至 vaults

关键结论/范围：备份并短暂停服，同卷重命名基目录。交接与实际验证：[S151](tasks/S151.md)。

### S150 — 文件浏览器右键查看版本历史

关键结论/范围：文件菜单新增“版本历史”，就地弹窗显示已提交版本编号、SHA-256、大小、保留期限及当前版本。交接与实际验证：[S150](tasks/S150.md)。

### S152 — 客户端 UI 与 Web 统一为 Mantine

关键结论/范围：默认主题、48px 顶栏、240px 导航、明暗切换、表格/确认窗/通知归 packages/ui，两端实际共用。交接与实际验证：[S152](tasks/S152.md)。

### S153 — 将版本历史版服务端部署到本机

关键结论/范围：固定当前已提交源89a9cf4，部署S150文件版本历史。交接与实际验证：[S153](tasks/S153.md)。

### S154 — 精简 Vault 目录页面

关键结论/范围：健康与采集、注册目录改为工具栏按钮打开弹窗。交接与实际验证：[S154](tasks/S154.md)。

### S155 — 移除 GitHub 工作流及关联依赖

关键结论/范围：删除工作流和专用校验配置，移除触发路径表、仅供托管编译的入口、依赖工作流的测试及报告字段。交接与实际验证：[S155](tasks/S155.md)。

### S156 — 本地版本与产物上传 GitHub Release

关键结论/范围：复用现有同源归档，按精确版本/提交发布至配置远端。交接与实际验证：[S156](tasks/S156.md)。

### S157 — 将共用 Mantine 与精简 Vault 版服务端部署本机

关键结论/范围：固定最新已提交源5a71062，发布共用Mantine展示层及精简Vault工具栏。交接与实际验证：[S157](tasks/S157.md)。

### S158 — 构建并发布首个 GitHub 预发布版本

关键结论/范围：Linux x64服务端tar.gz、Windows x64未签名ZIP、运行库源码、两平台来源、根清单及两份校验表，共8附件。交接与实际验证：[S158](tasks/S158.md)。

### S159 — Vault 路径跳转与文件容量

关键结论/范围：主体表格服务器路径使用原生链接，复用 filesLocation 按 VaultId 打开根目录。交接与实际验证：[S159](tasks/S159.md)。

### S160 — 服务概览显示 Server ID

关键结论/范围：概览的服务连接卡片显示完整、只读可选中的 Server ID，沿用 GET /v1/capabilities。交接与实际验证：[S160](tasks/S160.md)。

### S161 — 历史 build/deploy 资料迁入临时根

关键结论/范围：两个目录分别迁至 `/data/vaultsyncd-temp/vaultsyncd-build`（18MiB）和 `/data/vaultsyncd-temp/vaultsyncd-deploy`（87MiB）。交接与实际验证：[S161](tasks/S161.md)。

### S162 — 托盘自动下载 GitHub Release 并安全升级重启

关键结论/范围：托盘“自动更新”后台查询本项目正式Latest，HTTPS/SemVer/仓库附件/大小/SHA-256及固定ZIP清单校验后，由候选agent排空、导出、备份、升级并重启。交接与实际验证：[S162](tasks/S162.md)。

### S163 — 开发测试目录统一迁入 dev 根

关键结论/范围：temp/lab同卷rename到dev下同名目录，旧入口无软链。交接与实际验证：[S163](tasks/S163.md)。

### S164 — 归档旧 vs 测试目录并清理已验证副本

关键结论/范围：123个旧顶层目录归入 `/data/vaultsyncd-dev/S164-archive-20261109/runs.tar.zst`，所有日志/截图/数据库/备份和S097原始恢复镜像保留。交接与实际验证：[S164](tasks/S164.md)。

### S165 — 构建并发布 v0.1.2 GitHub 预发布

关键结论/范围：0.1.2 版本同步至 Cargo/前端/Tauri，仅更新 23 个本地包锁版本，外部依赖与协议不变。交接与实际验证：[S165](tasks/S165.md)。

### S166 — 客户端按地址读取并确认 Server ID

关键结论/范围：agent 提供无凭据只读发现 IPC，桌面按地址/可选 CA 读取完整身份，确认后保存。交接与实际验证：[S166](tasks/S166.md)。

### S167 — 桌面启动时复用或启动本机会话 agent

关键结论/范围：桌面启动先验证同用户会话的真实 agent IPC。交接与实际验证：[S167](tasks/S167.md)。

### S168 — GitHub 发布不再上传运行库对应源码归档

关键结论/范围：`packaging/github-release.py` 只把平台归档、平台来源收据、根 `release-manifest.json`、`build-SHA256SUMS` 和 `SHA256SUMS` 作为 Release 附件。交接与实际验证：[S168](tasks/S168.md)。 历史范围已由 S169 覆盖，当前契约见该卡与 AGENTS.md。

### S169 — 便携发行与 GitHub 附件收敛为 Windows ZIP

关键结论/范围：统一发行仅 Windows x64 ZIP，all 为同义参数、Linux 参数拒绝。交接与实际验证：[S169](tasks/S169.md)。

### S170 — 客户端登录 IP 与五分钟连接状态

关键结论/范围：S170-a 持久登录 IP、可选 wire 字段与迁移/来源验证。交接与实际验证：[S170](tasks/S170.md)。

### S171 — Web 右键新建目录与下载

关键结论/范围：空白区与目录（含目录树）右键新建目录，文件右键下载固定已提交版本。交接与实际验证：[S171](tasks/S171.md)。

### S172 — 客户端与服务端查看文件同步和传输记录

关键结论/范围：服务端持久记录认证连接、时间、当时文件路径、方向、版本、大小、范围与结果。交接与实际验证：[S172](tasks/S172.md)。

### S173 — 发布 0.1.3 预发布 GitHub Release

关键结论/范围：同步根 Cargo、前端/Tauri 镜像与 Cargo.lock（不改协议/schema/生成器版本）。交接与实际验证：[S173](tasks/S173.md)。

### S174 — ZIP 同目录覆盖后自动采纳归属

关键结论/范围：`portable::resolve_owner`/`adopt_owner`（同目录、`verify_zip` 自洽时重钉 `zip-owner.json` 并写 `zip-owner-adoptions.jsonl`）、`maintenance::admit_startup` 屏障优先、单测与 `s109-zip.ps1` Fresh 原生断言。交接与实际验证：[S174](tasks/S174.md)。 历史范围已由 S185 覆盖，当前契约见该卡与 AGENTS.md。

### S175 — 发布 0.1.4 预发布 GitHub Release

关键结论/范围：同步根 Cargo、前端/Tauri 镜像与 Cargo.lock（不改协议/schema/生成器版本与业务代码）。交接与实际验证：[S175](tasks/S175.md)。

### S176 — 桌面启动登录门禁与登录资料持久化

关键结论/范围：S176-a agent保存用户名及连接信息、恢复会话并准确报告重新登录状态。交接与实际验证：[S176](tasks/S176.md)。

### S177 — 发布 0.1.5 预发布 GitHub Release

关键结论/范围：同步根 Cargo、前端/Tauri 镜像与 Cargo.lock（不改协议/schema/生成器版本与业务代码）。交接与实际验证：[S177](tasks/S177.md)。

### S178 — Vault 选择框每次打开自动读取授权清单

关键结论/范围：移除“读取授权 Vault”按钮，鼠标或键盘每次打开 Mantine Select 自动调用既有 `agent.listVaults`。交接与实际验证：[S178](tasks/S178.md)。

### S179 — 发布 0.1.6 正式版 GitHub Release

关键结论/范围：同步根 Cargo、前端/Tauri 镜像与 Cargo.lock（不改协议/schema/生成器版本与业务代码）。交接与实际验证：[S179](tasks/S179.md)。

### S183 — 统一客户端应用图标与服务端网页品牌图标

关键结论/范围：保留 SVG 为唯一矢量源，生成 Windows 多尺寸 ICO。交接与实际验证：[S183](tasks/S183.md)。

### S184 — 开发构建脚本 `tools/build.sh` 输出到 `dist/`

关键结论/范围：新增 `tools/build.sh`（自动进入 `.#windows`、重建前端、静态 CRT 交叉编译五个 EXE、复用 `packaging/windows/zip/build.py` 生成两份同源清单、先清空 `dist/`）。交接与实际验证：[S184](tasks/S184.md)。

### S180 — 桌面更新进度与登录版本信息

关键结论/范围：自动更新增加独立Windows Shell进度窗口，下载显示实际百分比/字节及目标版本，检查、校验、维护、健康与回退显示活动条，主窗口关闭后继续展示。交接与实际验证：[S180](tasks/S180.md)。

### S185 — 删除 ZIP 专用入口与普通启动归属校验

关键结论/范围：S185-a 运行时移除 ZIP owner/清单/程序来源启动门禁，复用同用户会话 agent 不限定解压目录，安全 start/stop/health 并入 agent。交接与实际验证：[S185](tasks/S185.md)。

### S186 — 开发构建先清空 dist 且失败不留残骸

关键结论/范围：`tools/build.sh` 在进入 `.#windows` 之前就清空 `dist/` 与暂存目录（`$CARGO_TARGET_DIR/dev-build-stage`，默认 `target/dev-build-stage`），全部产物与清单在暂存目录组装、齐套后才一次性搬入 `dist/`，暂存目录由 `EXIT` trap 清理。交接与实际验证：[S186](tasks/S186.md)。

### S182 — 发布 0.1.7 正式版 GitHub Release

关键结论/范围：同步根 Cargo、前端/Tauri 镜像与 Cargo.lock（不改协议/schema/生成器版本与业务代码）。交接与实际验证：[S182](tasks/S182.md)。

### S188 — 多服务器连接存储与迁移

关键结论/范围：多服务器连接存储与迁移。交接与实际验证：[S188](tasks/S188.md)。

### S189 — 按服务器运行同步并路由命令

关键结论/范围：按服务器运行同步并路由命令。交接与实际验证：[S189](tasks/S189.md)。

### S190 — 桌面直接打开绑定列表

关键结论/范围：桌面直接打开绑定列表。交接与实际验证：[S190](tasks/S190.md)。

### S191 — Windows 双服务器原生验收

关键结论/范围：Windows 双服务器原生验收。交接与实际验证：[S191](tasks/S191.md)。

### S192 — 解除绑定后移出桌面相关列表

关键结论/范围：统一过滤已解除绑定，覆盖本地 Binding 表、管理卡片及文件/历史/传输记录选择器。交接与实际验证：[S192](tasks/S192.md)。

### S199 — 维护准备与部分占位重新包含

关键结论：维护恢复包如实记录排除缺失与固定版本部分占位，未确认内容仍拒绝；取消排除后缺失文件经显式下载不覆盖恢复，部分占位先保留原 pin 再协调。真实 SQLite/Windows CFAPI→HTTPS、恢复包哈希审计与独立复审通过，原客户端状态已恢复。交接/限制：[S199](tasks/S199.md)；无服务端变化，不部署、不发行。

### S196 — 统一默认排除与绑定附加规则

关键结论/范围：统一运行时同步、模式补齐、规则解释与保存校验的有效规则。交接与实际验证：[S196](tasks/S196.md)。

### S195 — 解除绑定后释放同步目录占用

关键结论/范围：解绑成功前停止排空原生 writer，按 durable detached 检查精确物理根重用。交接与实际验证：[S195](tasks/S195.md)。

### S197 — 同步与传输记录显示客户端主机名及属性

关键结论/范围：S197-a 登录声明与 schema21 持久属性、授权活动读取及可选 wire/IPC 字段。交接与实际验证：[S197](tasks/S197.md)。

### S198 — 统一 Lucide React 通用图标并保留 FileIcon

关键结论/范围：S198-a 接入锁定依赖、共享图标及两端导航/主题。交接与实际验证：[S198](tasks/S198.md)。

### S200 — 桌面程序配置与可选关闭 agent

关键结论/范围：S200-a 程序配置页面与原生设置桥。交接与实际验证：[S200](tasks/S200.md)。

### S201 — 两端 Vault 及绑定选择默认选中

关键结论/范围：桌面默认服务器、授权 Vault 初次自动读取及首项/撤权回退，文件/历史/传输绑定默认首项。交接与实际验证：[S201](tasks/S201.md)。

### S202 — 两端同步与传输记录自动刷新

关键结论/范围：共享刷新调度、两端记录页接入及直接相关测试。交接与实际验证：[S202](tasks/S202.md)。

### S203 — 桌面自动停止 agent 退出耗时修复

关键结论/范围：S203-a 普通退出使用独立认证有序停止，保留原地数据/队列/恢复状态及暂停策略。交接与实际验证：[S203](tasks/S203.md)。

### S205 — Web 连接正常时静默并默认自动连接

关键结论/范围：保留认证成功/刷新恢复后的同源自动健康检查、单请求与退出取消。交接与实际验证：[S205](tasks/S205.md)。

### S204 — 绑定文件的统一 Explorer 右键入口

关键结论/范围：绑定文件的统一 Explorer 右键入口。交接与实际验证：[S204](tasks/S204.md)。

### S206 — 两端上传下载图标改用 lucide 云传输图标

关键结论/范围：统一 Web 文件工具栏与两处右键菜单、两端「同步与传输记录」方向列、桌面文件状态与 Explorer 下载窗口入口的图标。交接与实际验证：[S206](tasks/S206.md)。

### S207 — 区分生产与开发测试服务

关键结论/范围：独立 dev 状态/Vault/身份，复用本机 ACME fullchain/key。交接与实际验证：[S207](tasks/S207.md)。

### S208 — Web 文件浏览器 PDF 与图片在线预览

- **状态与前置**：done（用户2026-11-12新增）；S171 固定版本下载、S145 会话门禁、S206 文件操作入口。
- **只读设计范围**：design §18.1/24（ADR-019 补充）；接口文档 S122、直接前置短交接与相关源码。
- **主要落点**：apps/web/src/features/files/、apps/web/tests/、tests/web/；锁定依赖及 Nix 离线闭包按实际更新。
- **实现要点**：名称点击及右键打开 PDF/常见图片只读查看器；固定版本/授权/大小与摘要校验；PDF 翻页缩放、图片等比显示，保留下载。只绘制像素，不执行文档内容，不放宽 CSP；读取与渲染可取消并有界，损坏/超限/认证失效如实处理。
- **本步验收**：相关类型/单测/构建，真实 Chromium+btrfs 实例验证 PDF 与图片、菜单/键盘、翻页缩放、下载、取消与迟到、错误重试、未授权/不受支持内容和明暗窄屏/CSP；独立复审、本机部署健康与静态资产核对；交接、合 master 并清理任务树。
- **交接**：[S208](tasks/S208.md)，记录资源限制、实际验证和部署/合并清理；实现4770492/交接598b407已ff合master，generation281及dev同包上线，5挂载/任务树/分支已清理。

### S209 — 使用浏览器内置 PDF 阅读器

- **状态与前置**：done（用户2026-11-12明确要求简化）；S208。
- **只读设计范围**：design §18.1/24 ADR-019补充，S208交接及文件预览/CSP接口。
- **主要落点**：Web文件预览、PDF依赖/打包、生产CSP与对应验证。
- **实现要点**：固定版本下载/摘要校验后，用 application/pdf Blob 调用浏览器内置查看器；删除 PDF.js、补丁与字体打包，图片维持现状。关闭/换页/退出释放URL，保留下载及不支持浏览器的提示。记录浏览器接管 PDF 内容/资源行为的边界变化。
- **本步验收**：根类型、Web单测/构建、真实Chromium内置查看器及图片/取消/校验/授权/CSP回归；Nix纯构建、本机prod/dev部署与健康/静态资产核对；交接、合master、清理任务树。
- **交接**：[S209](tasks/S209.md)；实现23d5c7c已部署generation282，prod/dev同包与精确包17组复验通过；实现/交接ac0243d已ff合master，7挂载及本卡树/分支已清理。

### S212 — 本机 OnlyOffice Document Server 基础服务

- **状态与前置**：done（用户2026-11-12新增）；S112/S113/S207，design §17.2。
- **范围**：在 `/etc/nixos` 引入用户指定的 `features.onlyoffilce.*`，wks-lab启用 Document Server；复用TLS、隔离内部监听、运行时JWT/secure_link密钥。不包含项目Web预览应用接入。
- **验收**：主机纯求值与build/dry/test/健康/switch、HTTPS API与真实文档转换、未签名请求拒绝、内部管理路径保护、密钥重启稳定及prod/dev健康保持；中文提交、交接和任务树清理。
- **交接**：[S212](tasks/S212.md)；主机229efe4已ff合main并清树，generation285；项目仅记录基础服务，未改Web应用。

### S213 — Web Office 文件只读预览

- **状态与前置**：done（用户2026-11-12新增）；S212/S210/S209，design §6/17.1/17.2/18.1，ADR-019补充。
- **范围**：Office固定版本经OnlyOffice转换为PDF，再以现有浏览器查看器预览；保留原文件下载，无编辑或同步内容变更，不放宽CSP。
- **实现**：schema新增认证转换端点；请求校验预期摘要/大小与授权，源完整核验后仅经随机本机临时通道交给转换器；服务端JWT、同源结果约束、输入/输出50MiB、120秒与最多2并发；UI关闭/会话/导航取消围栏与真实错误重试。
- **验收**：类型/相关单测及构建、真实OnlyOffice与Chromium+btrfs端到端、固定版本/越权/坏输入/输出/限额/取消/重试回归、独立复审、Nix纯构建、prod/dev部署健康与静态资产核对；交接合master及清树。
- **交接**：[S213](tasks/S213.md)；实现43ae33d/交接50fc007已ff合master，主机429b1cb已ff合main；generation286，prod/dev同包，六挂载及双树/分支清理完成。

## 6. 里程碑与覆盖关系

S001—S014 环境/平台；S015—S031 模型/持久化；S032—S052 服务端闭环；S053—S074 核心/Windows；S075—S091 管理/数据保护；S092—S105 部署与综合验收；S106—S108 性能、容量和最终交付。DEV 与 T01—T36 原始归属见上文固定归档及 design §21/§23；局部测试不得替代真实集成证据。S011 的 OS 取消回调、S106/S107 性能容量、T02 多台独立 Windows 和当前候选同源发行证据未补齐前，不宣称首发验收完成。

## 7. 记录模板与续接规则

### 7.1 agent/progress.md

顶部只保留当前里程碑/最早 ready/资源和集成状态；任务表提供状态、产出、验证摘要、阻塞原因、交接路径。done 行仅保留结论与索引，blocked 保留实际缺口/恢复条件；历史播报与“下一 ready”不覆盖当前状态。

### 7.2 agent/tasks/Sxxx.md

执行时记状态、依据/前置、结果、接口/路径、worktree/分支/基线、验证命令/退出码/证据、限制、实际提交和续接最小动作。检查点包括修改文件/函数、错误及重现命令；交接建议 300—600 中文字，复杂安全边界可适当增加。大日志与状态/证据留在开发根，不进 Git。

压缩已完成记录时保留原路径、结论、接口/规范入口、历史验证、限制及实际提交，并给 `git show <固定完整提交>:<原路径>`；不使用移动 HEAD。已完成子卡/批次可按 `agent/tasks/README.md` 统一 Git 归档删除；未完成检查点与恢复条件保留。Git 外 artifacts 必须独立保存；后续覆盖只引用真实验收，不因压缩重新认定 done 或降低验收目标。

## 8. 本手册采用的官方环境依据

当前工具链与环境事实见 `flake.lock`、`docs/environment.md`、`docs/operations/windows-lab.md`；历史外部依据及当时版本链接见固定归档，不重复维护。


### S210 — Web 文本与 Markdown 在线预览

- **状态与前置**：done（用户2026-11-12明确要求）；S209。
- **只读设计范围**：design §18.1/24 ADR-019，S209交接及文件预览/CSP接口。
- **主要落点**：Web文件查看器、React CodeMirror/语言加载、Markdown渲染、依赖锁与对应验收。
- **实现要点**：最新版 @uiw/react-codemirror 精确锁定、默认只读与按文件名语法高亮；Markdown默认GFM渲染可切代码，HTML/SVG源码不执行；沿用固定版本/长度/摘要与会话取消围栏。文本2MiB门禁，UTF-8/带BOM UTF-16严格解码，二进制拒绝；Markdown原始HTML跳过、链接与图片只显示文字不请求资源。无在线保存及wire/schema变化。
- **本步验收**：根类型、Web测试/构建、真实Chromium文本与Markdown/高亮/只读/安全/取消/布局及PDF图片回归，Nix纯构建、本机prod/dev部署、健康与静态资源核对；交接、合master与任务树清理。
- **交接**：[S210](tasks/S210.md)；实现e79299a已部署generation283，prod/dev同包、136文件/408字节与精确包13+17组通过；实现/交接04c703e已ff合master，六挂载及本卡树/分支已清理。

### S211 — 管理页面加载失败自动恢复

- **状态与前置**：done（用户2026-11-12明确要求）；S205/S210。
- **只读设计范围**：design §18.1，管理页面错误边界与懒加载。
- **主要落点**：Web PageBoundary、路由模块加载及对应验收。
- **实现要点**：加载失败自动重挂当前管理页，退避至30秒；隐藏/离线暂停，返回后恢复。重建被React缓存的懒加载失败；仅模块错误持续失败时允许五分钟一次整页恢复，保留hash/会话，移除手动重载按钮。无写操作重放及wire/schema变化。
- **本步验收**：根类型、Web测试/构建、真实Chromium模块故障恢复/节流/导航取消，Nix纯构建及本机prod/dev部署健康验证；交接、合master与任务树清理。
- **交接**：[S211](tasks/S211.md)。

### S215 — ONLYOFFICE 原生只读 Office 查看器

- **状态与前置**：done（用户2026-11-12授权执行）；S213/S212/S209/S210，design §6/17.1/17.2/18.1/24，ADR-019补充。
- **范围**：Office默认用ONLYOFFICE DocEditor原生只读浏览，固定版本、授权/完整摘要/原件下载保持；无原件保存，旧PDF端点兼容。受限临时源与服务端只读JWT；CSP仅新增配置服务origin的script/frame信任。
- **验收**：相关Rust/Web及生成一致性、真实OnlyOffice+Chromium+btrfs三格式/多工作表/权限/固定旧版/生命周期/故障/CSP、独立复审、纯Nix构建、prod/dev部署健康/身份/资产一致，交接合master清树。
- **交接**：[S215](tasks/S215.md)；功能182bfe1/组合发布源6a99df4/交接65aae2b已ff合master，generation288及prod/dev同包验收通过，六挂载与任务树/分支正常清理。
