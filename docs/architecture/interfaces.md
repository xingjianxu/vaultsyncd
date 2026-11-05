# 已实现接口摘要

本文件只记录已经实现、可被后续卡复用的工具与接口契约；产品语义、数据模型与 wire 协议以 `docs/design.md` 和 `api/` 中的契约文件为准，不在此复制规格正文。接口形态变化时同步更新本文件，并在对应 `agent/tasks/Sxxx.md` 记录原因。

## tools/context.py — 规格上下文提取

状态：S002 实现。Python 3 标准库，无第三方依赖；S003 后由 devShell 提供 `python3`（与主机同为 3.14.8）。

来源文件（相对仓库根；路径由脚本自身位置推导，可从任意工作目录调用）：

| 子命令 | 来源 |
| --- | --- |
| `card` | `agent/implementation-plan.md` |
| `design` | `docs/design.md` |

命令：

```bash
python3 tools/context.py card <CARD> [<CARD> ...]
python3 tools/context.py design <SECTION> [<SECTION> ...]
python3 tools/context.py --help
```

`<CARD>` 是任务卡编号（`S002`，大小写不敏感）或完整标题（`S002 — 建立 agent 入口与上下文提取工具`、`建立 agent 入口`）；`<SECTION>` 是 design 编号（`11.2`、`5.10`，可带 `§` 前缀或末尾点号）或完整标题/标题正文。

### 定位规则

1. 任务卡只匹配带 `S<数字>` 编号的标题；design 允许非编号标题（如附录）按标题匹配。
2. 匹配顺序：先去空白/大小写/全角破折号归一化后的精确匹配；精确多解报错。再按编号前缀（design 要求前缀后接 `.`，`5.1` 不会误配 `5.10`）或标题前缀匹配；唯一则采用，多解报错。无匹配报错并给候选。
3. 小节范围到下一个同级或更高级标题为止，并包含下属子标题。design 额外按编号判断后代关系：`5.1` 包含同级标题 `5.1.1`（编号为后代），`11.2` 在 `11.3` 前结束；下一个小节不进入输出。
4. 只有 ATX 标题（行首 1—6 个 `#` 加空白）被识别；Setext 标题与缩进代码块不参与匹配。
5. 代码围栏（行首最多 3 个空格 + 至少 3 个 `` ` `` 或 `~`）内的 `#` 行不是标题。关闭围栏要求相同字符、长度不小于开启围栏且其后无其他内容，因此更长围栏可包含短围栏示例；未闭合围栏视为延伸到文件末尾，其后各行不参与标题识别，正常仓库文档不存在该情况，临时 fixture 触发时按此规则处理。

### 输出格式

每个小节依次输出三行标记、一个空行和原文行范围：

```text
--- source: docs/design.md
--- heading: ### 11.2 一次内容修改的提交步骤
--- lines: 646-657

<原文第 646—657 行，含标题行>
```

多个小节之间空一行；重复请求的同一小节只输出一次，顺序按参数。行号范围为 1 基闭区间，末尾空行会被裁掉，因此正文为空的小节只输出标题行。来源标题与行号便于下次会话按预算取用，并避免依赖会漂移的固定行号。

### 错误与退出码

错误一律写 stderr，不向 stdout 输出半条结果：

| 退出码 | 情形 |
| --- | --- |
| 0 | 全部查询成功 |
| 2 | 用法错误（无子命令/无参数）、未知标题、前缀多解、编号不存在 |
| 1 | 来源文件缺失或不可读 |

多参数时先解析全部查询；任一失败即整体失败并列出所有错误，stdout 保持为空。未知查询附最多 8 条接近候选；多解按来源路径、行号与标题列出候选（超过 20 条时注明剩余数量）。

## flake.nix — 开发 shell

状态：S003 实现；S008 增加 `quickemu`，S009 增加 `windows`。默认 devShell 是日常 Linux 构建入口，额外 shell 按需进入；实测版本与限制见 `docs/environment.md` §10、§12、§13。

| 项目 | 契约 |
| --- | --- |
| 输入 | `github:NixOS/nixpkgs/nixos-unstable`，rev 由 `flake.lock` 固定（当前 `1d060086d60cb76a40cf37109f979d88a5bd6396`）；无 overlay、无其他 input |
| 导出 | `devShells.x86_64-linux.default`，derivation 名 `vaultsyncd-dev`；`nix develop` 即默认入口。 `devShells.default` 不是 flake schema 的合法顶层属性，按系统的 `devShells.<system>.default` 是规范写法 |
| 覆盖范围 | 默认 shell 只覆盖 Linux `x86_64-linux` 本机构建；Windows x64 交叉编译由独立的 `windows` shell 承担（S009 已实测），客体只承担原生运行与验收；分工见实施手册 §2.6 |
| 非交互调用 | `nix develop --command bash -c '<cmd>'`；`shellHook` 只导出 `VAULTSYNCD_DEV_SHELL=1` |
| 跟踪要求 | `flake.nix`/`flake.lock` 必须先被 Git 跟踪，否则 `nix flake lock`/`nix develop` 退出码 1 |
| `quickemu` shell（S008） | `devShells.x86_64-linux.quickemu`，derivation `vaultsyncd-quickemu`：quickemu 4.9.9、qemu 11.1.2、xorriso 1.5.8.pl02、wimlib 1.14.5、p7zip、openssh、curl，全部来自同一 `flake.lock`。与 `default` 分开，避免日常开发环境引入虚拟化依赖；`shellHook` 只导出 `VAULTSYNCD_QUICKEMU_SHELL=1`，调用方式 `nix develop .#quickemu --command <cmd>` |
| `windows` shell（S009） | `devShells.x86_64-linux.windows`，derivation `vaultsyncd-windows`：rustup 1.29.2、cargo-xwin 0.23.1、clang/clang-cl 21.1.8、lld-link 21.1.8、llvm-lib 21.1.8、openssh、git，全部来自同一 `flake.lock`。交叉目标固定 `x86_64-pc-windows-msvc`（理由见 `docs/environment.md` §13.2）。`shellHook` 导出 `VAULTSYNCD_WINDOWS_SHELL=1`，把 `RUSTUP_HOME`/`CARGO_HOME`/`XWIN_CACHE_DIR` 指向工作树根 `.cache/windows/`（被 `.gitignore` 忽略），并用 `rustup which cargo` 把 `rust-toolchain.toml` 指定的工具链放进 PATH；缺失时只提示 `tests/platform/windows/setup-toolchain.sh`，不隐式下载。调用方式 `nix develop .#windows --command <cmd>` |
| 缓存与清理 | `.cache/windows/{rustup,cargo,xwin}` 全部可再生，`rm -rf .cache/windows` 即清理；不写用户全局 `~/.rustup`、`~/.cargo`，也不进 Git |
| 默认 shell 的测试工具（S013） | 默认 shell 增加 `pkgs.e2fsprogs`：`mkfs.ext4`（拒绝样本）、`chattr`/`lsattr`（NOCOW 样本）、`filefrag`（reflink 共享 extent 证据）。只被 `tests/platform/linux/` 的验收脚本使用，不构成产品运行依赖；之前主机缺 `lsattr`/`chattr` 的限制（`docs/environment.md` §11.8）由此解除 |

## Cargo workspace 与 Rust 工具链

状态：S004 实现；S009 增加 `tests/windows-integration`，S010 增加 `crates/platform-windows`。成员按 design §5.3/§5.4/§5.8 逐个增补，不预建空 crate。默认 Cargo 命令在 `nix develop -c` 内执行，Windows 交叉构建在 `nix develop .#windows -c` 内执行。

| 项目 | 契约 |
| --- | --- |
| 根清单 | `Cargo.toml` 为虚拟 workspace（无 `[package]`）；`resolver = "3"`；`members`/`default-members` 只列真实存在的包。当前 `members = ["crates/domain", "crates/fs-common", "crates/platform-linux-server", "crates/platform-windows", "crates/protocol", "tests/windows-integration", "tools/xtask"]`，`default-members = ["crates/domain", "crates/fs-common", "crates/protocol"]`：两个平台适配层、只能在 Windows 上运行的测试包与仅用于编排的工具都不进默认集合；`crates/fs-common`（S017）与 `crates/protocol`（S024）只依赖纯核心，两端共用，因此进默认集合 |
| 包命名 | `crates/<目录>/` 的包名为 `vaultsyncd-<目录>`（如 `vaultsyncd-domain`）；`apps/` 入口按 design §5.7 的 `vaultsyncd-server`/`-admin`/`-agent`/`-cli` |
| 共享包字段 | `[workspace.package]`：`edition = "2024"`、`rust-version = "1.85.0"`、`version = "0.1.0"`、`publish = false`；成员用 `<字段>.workspace = true` 继承，不重复字面量 |
| lint 接线 | 成员默认写 `[lints] workspace = true`。`[workspace.lints.rust]`：`unsafe_code`、`missing_docs`、`unused` = deny；`[workspace.lints.clippy]`：`all = { level = "deny", priority = -1 }`。crate 内的 `#![forbid(unsafe_code)]` 是更硬的内层约束。**包级放行 `unsafe_code` 的唯一可行写法**（S010 实测）：省略 `[lints] workspace` 键（写 `workspace = false` 会报错“`workspace` cannot be false”，与 `workspace = true` 并存会被拒“cannot override `workspace.lints` in `lints`”），然后在包内手抄 workspace 的其余 lint（`missing_docs`/`unused` deny、`clippy::all` deny）并在 `Cargo.toml` 注释说明理由；实测 `crates/platform-windows` 的 `missing_docs` 与 `unsafe_code` 行为都符合预期。也可以不改 lint 表，改用各 target 源码里的 crate 级 `#![allow(unsafe_code)]`（`tests/windows-integration` 的做法）。S013 的 `crates/platform-linux-server` 用同一写法（包级 `unsafe_code = "allow"` + 手抄其余 lint），理由同样是必须直接调用内核 ABI |
| 工具链固定 | `rust-toolchain.toml`：`channel = "1.99.0"`、`targets = ["x86_64-pc-windows-msvc"]`（S009 起）。Linux 日常构建的 rustc/cargo/rustfmt/clippy 来自锁定 nixpkgs，不读该文件；`nix develop .#windows` 内的 rustup 按本文件安装同一 channel 与 Windows 标准库，两端版本以 `docs/environment.md` §13 为唯一基准 |
| 格式 | `rustfmt.toml` 只固定 `style_edition = "2024"`，其余用默认值；`cargo fmt --check` 为格式门禁 |
| 锁文件 | 仓库根唯一 `Cargo.lock`（生成产物，提交入库），不得忽略；新增依赖后先 `cargo generate-lockfile`，验证用 `--locked` |
| 验收命令 | `cargo metadata --locked --no-deps`、`cargo check --locked`、`cargo fmt --check`、`cargo clippy --locked --all-targets -- -D warnings`、`cargo test --locked` |
| MSRV 状态 | `rust-version = "1.85.0"` 依据 edition 2024 稳定版本声明；实测只在 1.99.0 上编译（Linux 与 Windows 交叉目标），devShell 内无 1.85.0 工具链，1.85.0 真编译复核仍未执行 |

## 前端 pnpm workspace 与 UI 包

状态：S005 建立 workspace，S024 增加 `packages/api-client`（远端 HTTP 客户端）。当前成员 `packages/api-client`、`packages/ui`；`apps/web`、`apps/desktop`、`packages/local-ipc-types` 按各自卡创建后再加入成员列表。所有 node/pnpm 命令在 `nix develop -c` 内执行（版本见 `docs/environment.md` §10）。

| 项目 | 契约 |
| --- | --- |
| 根清单 | `package.json`：`private: true`；`packageManager: "pnpm@12.9.0"`；`engines.node = "24.21.0"`、`engines.pnpm = "12.9.0"` 全部写死 devShell 实测值，版本源是 `flake.lock` 固定的 nixpkgs，不追“最新”；换版本必须先改 flake 输入 |
| 根脚本 | `typecheck` = `pnpm --recursive run typecheck`，覆盖全部成员；新成员只需提供同名 script，无需改根脚本 |
| 成员登记 | `pnpm-workspace.yaml` 只列已经真实存在的目录（当前 `packages/api-client`、`packages/ui`），不为未创建的 apps 预留占位 |
| 包命名 | `packages/<目录>/` 的包名为 `@vaultsyncd/<目录>`（`@vaultsyncd/ui`），`private: true`，version 与 Rust workspace 同为 `0.1.0` |
| 共享 TS 配置 | 仓库根 `tsconfig.base.json`：`strict`、ES2022 + DOM、`module: ESNext`、`moduleResolution: bundler`、`jsx: react-jsx`、`esModuleInterop`、`isolatedModules`、`skipLibCheck`；成员 `tsconfig.json` 只写 `extends: "../../tsconfig.base.json"` 与 `include` |
| UI 包边界 | `packages/ui/src/` 只放无平台 I/O 的展示组件与类型（design §5.6、§6）：不 fetch、不读文件、不走 IPC、不持有同步状态；状态语义由 Rust 纯核心定义，应用层映射后传文案 |
| 导出形式 | `exports: { ".": "./src/index.ts" }`，直接导出 TS 源码（当前无构建步骤、不产 `dist/`）；消费方必须能编译 TS，编译链在 S076 的应用里 |
| 依赖策略 | react/react-dom 以 `peerDependencies` 声明并由应用自带运行时，同时在 UI 包 `devDependencies` 装同版本供本地类型检查；`typescript` 只在根 `devDependencies`，pnpm 脚本会自动把根 `node_modules/.bin` 加进 PATH |
| 锁文件 | 仓库根唯一 `pnpm-lock.yaml`，提交入库、不忽略，不产生嵌套锁；pnpm 12 在同一文件的第一个 YAML 文档写 `packageManagerDependencies`（各平台 `@pnpm/exe.*`），是 `packageManager` 字段的锁定产物，不手改 |
| 验收命令 | `pnpm install --frozen-lockfile`、`pnpm --filter @vaultsyncd/ui typecheck`、根 `pnpm typecheck` |

## crates/platform-windows — Windows 平台适配层（CFAPI 原型）

状态：S010 实现 `cfapi/` 的注册、连接、占位创建、查询、注销与 `FileIdentity` 编码；S011 实现固定内容源的读取供给（`FETCH_DATA` 分段落盘）、范围请求、离线/超时明确失败与传输中断语义；S012 实现释放（`CfSetInSyncState` + `CfDehydratePlaceholder`）与释放前的写保护判定（使用保护探测 + 内容代际）；`shell/`、`files/`、`watcher/`、`ipc/` 尚未实现。包名 `vaultsyncd-platform-windows`，无第三方依赖，属于 workspace `members` 但**不在** `default-members`（Windows 目标代码只有交叉构建 + 客体运行才能验证）。宿主单测可覆盖纯逻辑部分（S011 起含确定性内容源、SHA-256 与回填循环）：`cargo test --locked -p vaultsyncd-platform-windows`。操作步骤见 `docs/operations/windows-lab.md` §16（S010）、§17（S011）、§18（S012），实测证据见 `docs/environment.md` §14、§17、§18。

### 模块与平台边界

| 项目 | 契约 |
| --- | --- |
| `cfapi::identity` | `FileIdentity` 编解码，纯逻辑、可在宿主测试：48 B = magic `b"VS"`(2) + schema_version(1) + 保留字节(1) + `RootId`(16) + `EntryId`(16) + 版本提示 u64 LE(8) + CRC-32/IEEE LE(4)。解码校验长度/magic/版本/保留字节/CRC，错误为 `IdentityDecodeError` 的 5 个明确变体。blob 只有定长字节与整数（结构上不含路径、令牌、服务器地址），**不是**业务真相 |
| `cfapi::prototype_layout` | 原型固定条目：`PROTOTYPE_ROOT_ID`（16 B 常量）、目录 `vaultsyncd-proto`、文件 `small-12345.bin`(12,345 B)、`large-1mib.bin`(1,048,576 B) 与 `huge-16mib.bin`(16,777,216 B，S011 增加，用于让平台发出范围请求)；`entry_id_for(name)` 由两个 FNV-1a 64 位哈希派生稳定 EntryId（原型专用，真实身份由服务端分配）；`file_identity(name)` 组装完整身份。纯逻辑，可在宿主测试 |
| `cfapi::content`（S011，纯逻辑） | 固定测试内容源：`CONTENT_SEED` + 条目名派生的位置块（SplitMix64）⇒ 内容固定、版本固定；`entry_for_path` 只认 `…/vaultsyncd-proto/<固定条目>`；`check_range` 拒绝负偏移/非正长度/越界；`fill_range` 按偏移生成任意区间（与整份切片逐字节相同）；`expected_digest` 给出整份 SHA-256；自带 SHA-256（NIST 向量锁定）；`serve_range` 是按块回填循环（`ChunkSink`），每段前检查取消、下游拒绝即停。宿主测试覆盖金样本哈希、区间切片、非法范围与取消/拒绝语义 |
| `cfapi::lifecycle` | `HResult`（`Display` 为 `0x%08X`、`succeeded()` 按符号判定）与 `StageError { stage, hr, detail }`；`format_record`/`record` 输出单行 `stage=<stage> key=value… hresult=0x…`。失败必须带阶段与原始码上报，不吞错 |
| `cfapi::guard`（S012，纯逻辑） | 释放判定：`Verdict { Clean, Dirty, Busy, Unknown }`、`ProbeOutcome { Opened, Failed(u32), Skipped }`、`UsageProbe { exclusive_read, exclusive_write, shared_read }`、`ContentCheck { Matched, Mismatched, Unverified }`、`Observation { is_placeholder, pin_state, in_sync_state, on_disk_data_size, logical_size, modified_data_size }`、`evaluate(&Inputs) -> Decision { verdict, reason }`。规则顺序即保守程度：非占位符/已脱水/pinned/探测冲突/探测未执行/内容非全本地/内容不符/内容无法核对/内容变动依次返回，只有全部通过才是 `clean`；`reason` 是稳定拼写（`content_mismatch`、`handle_held_read`、`already_dehydrated` 等），客体脚本按它断言。宿主单测 10 个覆盖每一条 |
| `cfapi::raw`（仅 Windows） | `cldapi`/`kernel32` 的最小 FFI 声明；结构体与 Windows SDK 10.0.26100 的 C 布局逐项对应，并有 `const` 尺寸断言（见 `docs/environment.md` §14.3）。只声明实际用到的函数 |
| `cfapi::prototype`（仅 Windows） | 调用的唯一序列：`CfRegisterSyncRoot` → `CfConnectSyncRoot` → `CfCreatePlaceholders`（目录先、文件后）→ `CfGetPlaceholderInfo`/`CfGetPlaceholderStateFromFindData` → `CfDisconnectSyncRoot` → `CfUnregisterSyncRoot`。传 `FILE_FLAG_OPEN_REPARSE_POINT` 之外的手改 reparse point、截断文件伪造脱水、无条件 in-sync 都被明确禁止；占位用 `CF_INSYNC_POLICY_NONE` + `CF_PLACEHOLDER_CREATE_FLAG_NONE` 创建 |
| FFI 生命周期 | 每个阶段一行日志（`register`/`connect`/`create.placeholders`/`create.placeholder`/`hydration.probe`/`inspect.entry`/`disconnect`/`cleanup.remove`/`unregister`/`sync_root.query`/`cleanup.preserved`）；句柄在 `try_observe`/占位查询内即开即关，连接键必须由同一进程 `CfDisconnectSyncRoot` 释放后再注销；注销后用 `CfGetSyncRootInfoByPath` 后置确认，不靠猜错误码 |
| `cfapi::provider`（仅 Windows，S011） | 提供方运行期：`ProviderConfig`（内容源 `available`/`unavailable`、模拟延迟 `latency_ms`、超时阈值 `timeout_ms`、分段延迟 `chunk_delay_ms`）与 `ProviderStats`（fetch/chunk/bytes/range_rejections/cancel_callbacks/aborted/timeout/unavailable/failed_acks/in_flight）。上下文是进程级 `OnceLock<Arc<ProviderContext>>`，回调**不用** `CallbackContext` 裸指针，因此不存在指向已释放内存的路径；断开后 `wait_idle` 确认没有在途回调。读取/取消/水合验证的入口见下表 |
| `cfapi::release`（仅 Windows，S012） | 取证据 + 释放：`run_protect` 只判定（不改状态），`run_release` 先判定、只有 `Verdict::Clean` 才连上提供方（`ProviderSession`）→ 用 `GENERIC_READ|GENERIC_WRITE` 句柄 `CfSetInSyncState(CF_IN_SYNC_STATE_IN_SYNC)` → `CfDehydratePlaceholder(0, 逻辑大小, CF_DEHYDRATE_FLAG_NONE, NULL)` → 复核 `on_disk_data_size==0`。`ReleaseOutcome { Released, AlreadyDehydrated, Refused, DehydrateFailed }`；`Refused` 表示**没有**调用平台写操作（`dehydrate_hresult=-`），`DehydrateFailed` 保留原始 `HRESULT` 且不报成功。探测是三次 `CreateFileW`（`GENERIC_READ`+不共享 / `GENERIC_WRITE`+不共享 / `GENERIC_READ`+全共享），只开句柄立即关闭；内容核对读两遍整份文件并比较两次 SHA-256 |
| `FetchData` 回调（S011 语义） | 按回调请求的 `Offset`/`Length` 用 `CfExecute(CF_OPERATION_TYPE_TRANSFER_DATA)` 分段回填固定内容：`ParamSize=48`、`CompletionStatus=STATUS_SUCCESS`、`Buffer` 为该段内容；每段前检查取消标志，下游 `CfExecute` 失败立即停止并记录 `failed_acks`。非法范围/非原型条目/逻辑大小不符 ⇒ `STATUS_CLOUD_FILE_INVALID_REQUEST`；源不可用 ⇒ `STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE`；延迟达到阈值 ⇒ `STATUS_IO_TIMEOUT`。**任何分支都不返回 0 字节成功**。回调表只有 `FETCH_DATA` + `CANCEL_FETCH_DATA` + `CF_CALLBACK_TYPE_NONE`，且是 `'static` |
| 登记策略（S011 变更） | `CF_HYDRATION_POLICY_PARTIAL`（只水合被请求范围）+ `CF_POPULATION_POLICY_PARTIAL`（不实现 `FETCH_PLACEHOLDERS` 时不让平台自动填充目录）。S010 用的是 `FULL`/`FULL`；S011 实测 FULL 人口策略会让别的进程按路径打开占位符时等待目录枚举而阻塞，且 hydration FULL 与 PARTIAL 在本 build 上都表现为“打开即整份水合” |
| 占位判定 | `CfGetPlaceholderInfo` 成功即视为占位符（非占位符返回 `ERROR_NOT_A_CLOUD_FILE`），并作为 `FileIdentity`/`OnDiskDataSize`/`InSyncState` 的来源；`CfGetPlaceholderStateFromFindData` 只作补充。日志用 `placeholder_source` 标明来源，避免把 `0x00000000` 的 find-data 状态误读成“非占位” |
| 非 Windows 构建 | `cfapi::raw`/`cfapi::prototype` 用 `cfg(windows)` 排除；example 在非 Windows 目标只打印提示并退出 1，误跑 Linux 产物不会假装成功 |

### 原型命令 `cfapi-prototype`（example 目标）

`crates/platform-windows/examples/cfapi_prototype.rs`，经 `cargo xwin build --examples` 交叉构建为 `cfapi-prototype.exe`。

| 子命令 | 输入 | 行为 | 退出码 |
| --- | --- | --- | --- |
| `create [--root <path>]` | 已存在的本地目录，默认 `C:\vaultsyncd-test` | 注册（已注册时按 `ERROR_ALREADY_EXISTS` 继续并记录）→ 连接 → 建目录占位 → 建三个文件占位（已存在且是占位则跳过；已存在但不是占位则拒绝覆盖）→ 断开。**S011 起不再做“读取必须失败”的探测**（水合已实现），内容验证交给 `read`/`hydrate` | 0 成功；1 失败（输出 `stage=`/`hresult=` 与原因） |
| `inspect [--root <path>] [--allow-hydrated]` | 同上 | 只读：查询注册提供方；对目录与三个文件逐条校验占位、逻辑大小、本地数据量、`in_sync_state=0`、identity 解码与期望一致；默认要求仍脱水，`--allow-hydrated` 换成“本地数据不超过逻辑大小” | 0 已注册且全部校验通过；3 未注册（`NotRegistered`，cleanup 后的预期状态）；1 校验失败 |
| `cleanup [--root <path>]` | 同上 | 连接（未注册则继续）→ 只删除确认是占位符的探针条目（`abstract`/`kept_not_placeholder` 都写日志）→ 断开 → 注销并后置确认 → 逐条打印保留内容 | 0 成功（含幂等：已清空时仍 0）；1 失败 |
| `read --name <entry> [--root] [--offset N --length M] [--source available\|unavailable] [--latency-ms N] [--timeout-ms N] [--chunk-delay-ms N] [--expect success\|failure]` | 原型条目名（`expect` 可列出） | 自带提供方会话：读取整份或区间，与固定内容版本逐字节/哈希比对，输出 `stage=read.result`（bytes/sha256/expected_sha256/digest_match/on_disk_data_size/open_ms/read_ms/elapsed_ms/outcome）与 `provider.stats`；`--expect failure` 要求“明确失败且 0 字节” | 0 观察与 `--expect` 一致；1 相反或读取失败 |
| `cancel --name <entry> [--cancel-after-ms N] [--chunk-delay-ms N] [--require-cancel-callback]` | 同上 | 读线程负责打开+读取，主线程在 `FETCH_DATA` 已开始后关闭/取消它，输出 `stage=cancel.result`（cancel_code/cancel_to_return_ms/cancel_callbacks/aborted_fetches/in_flight）；默认不强制平台回调 `CANCEL_FETCH_DATA`（本 build 未观察到） | 0 取消动作已发出且读取有界返回、无在途回调；1 悬挂或未观察到水合开始 |
| `hydrate --name <entry> [--offset N --length M] [--cancel-after-ms N] [--disconnect-after-ms N] [--chunk-delay-ms N]` | 同上 | 用只请求 `FILE_READ_ATTRIBUTES` 的句柄调 `CfHydratePlaceholder`，按**指定范围**请求水合；输出 `stage=hydrate.result`（requested/platform offset/length、hydrate_hresult、interruption、interrupt_to_return_ms、total_ms、on_disk_data_size、各计数） | 0 水合成功或中断后在 5 s 内返回且无在途回调；1 失败/悬挂 |
| `protect --name <entry> [--root]` | 同上 | 只判定：打印 `stage=guard.observe`/`guard.probe`/`guard.content`/`guard.decision`（verdict、reason、pin/in_sync、on_disk/logical/modified、content、content_stable、local/expected SHA-256、三个探测结果）；不释放、不写 in-sync、不改内容 | 0 证据收集完成（verdict 可为 dirty/busy/unknown）；1 证据收集失败（如条目路径打不开） |
| `release --name <entry> [--root] [--expect released\|refused]` | 同上 | 判定为 `clean` 才 `CfSetInSyncState(IN_SYNC)` + `CfDehydratePlaceholder`；其余打印 `outcome=refused` 并保留内容。日志追加 `release.in_sync`/`release.dehydrate`/`release.result`（outcome、in_sync_hresult、dehydrate_hresult、on_disk_after、dehydrated_after） | 0 结果与 `--expect` 一致（`released` ⊇ {released, already_dehydrated}，`refused` ⊇ {refused, dehydrate_failed}）；1 不一致或判定阶段失败；2 用法 |
| `expect [--name <entry> --offset N --length M]` | — | 平台无关：打印 `expect_entry name= size= sha256=`（或 `expect_range`），供宿主脚本与客体独立哈希比对 | 0 成功；1 内容源错误；2 用法 |
| 用法错误 | — | 缺子命令、未知子命令/参数、`--root` 缺值、`--offset` 与 `--length` 不成对 | 2 |

不变量：

- 只操作 `--root` 下的 `vaultsyncd-proto` 探针子树；不枚举、不删除其它用户内容。
- `cleanup` 删除前必须确认是占位符；不是占位符就保留并在日志里说明。探针目录非空时记
  `cleanup.remove status=kept_not_empty` 并**继续注销**（S012）：能注销优先于能删目录，
  否则无效名字会让同步根永久处于注册状态。
- 注销前必须先断开连接；注销后用查询确认，未确认不报告成功。
- 释放只走平台认可流程（`CfSetInSyncState` + `CfDehydratePlaceholder`）；不手改 reparse point、
  不截断文件伪造脱水、不无条件置 in-sync。只有 `Verdict::Clean` 才释放，其余判定都保留内容。
- `inspect` 的水合断言（S012 调整）：`in_sync_state!=0` 只在**已脱水**条目上被接受（`release`
  的 `CfSetInSyncState` 会这样留下状态）；已水合却自称 in-sync 仍判失败。

### `tests/platform/windows` 的新增入口（S010）

| 项目 | 契约 |
| --- | --- |
| `common.sh` | 新增 `PLATFORM_PACKAGE=vaultsyncd-platform-windows`、`CFAPI_PROTOTYPE_BINARY=cfapi-prototype`、`CFAPI_GUEST_DIR`（默认 `C:/vaultsyncd-lab/s010`）、`CFAPI_ROOT`（默认 `C:/vaultsyncd-test`）；`guest_scp` 支持 `GUEST_TARGET_DIR` 覆盖目标目录，S009 的 `GUEST_DIR` 默认行为不变 |
| `build-cross.sh` | 先按 S009 原样构建冒烟 exe，再构建 `-p vaultsyncd-platform-windows --examples` 并把 `cfapi-prototype.exe` 复制到 `artifacts/windows/<target>/`；该步单独加 `-C link-arg=/ignore:4099` 抑制静态 CRT 缺 PDB 的 `LNK4099` |
| `run-cfapi-prototype.sh` | 宿主 SHA-256 → 建 `CFAPI_GUEST_DIR` → scp exe 与 `guest-cfapi-prototype.ps1` → ssh 执行 → 逐项比对哈希；退出码沿用客体脚本（0 全部检查通过） |
| `guest-cfapi-prototype.ps1` | `-Directory`/`-Root`/`-Exe`；先幂等清场，放入 `user-keep.txt` 与 `user-dir\keep-note.txt`（记录 SHA-256），依次执行 `create`/`inspect`，采集 `Get-Item` 属性/长度与 `fsutil reparsepoint query`/`sparse queryflag`，用 `read` 子命令验证水合读取成功且长度正确，执行 `cleanup`，再核对用户内容哈希不变、占位已删、`inspect` 退出码 3、再次 `cleanup` 退出码 0；逐项打印 `check=pass|fail`，最后打印 `cfapi_prototype_result: OK|FAILED` |
| `common.sh`（S011 追加） | `CFAPI_HYDRATION_GUEST_DIR`（默认 `C:/vaultsyncd-lab/s011`）、`CFAPI_HYDRATION_RANGE_ENTRY/OFFSET/LENGTH`（默认 `huge-16mib.bin`、`4000000`、`1048576`），S010 的变量与 `guest_scp` 默认行为不变 |
| `run-cfapi-hydration.sh`（S011） | 宿主 SHA-256 + `cargo run --example cfapi-prototype -- expect`（及 `expect_range`）算出期望值 → scp exe 与 `guest-cfapi-hydration.ps1` → ssh 执行 → 比对 exe 哈希、`guest_file_sha256`/`guest_range_sha256` 与宿主期望 → 统计 `check=pass|fail`；任一 fail/MISMATCH 或未报告 `cfapi_hydration_result: OK` 即非零退出 |
| `common.sh`（S012 追加） | `CFAPI_RELEASE_GUEST_DIR`（默认 `C:/vaultsyncd-lab/s012`）、`CFAPI_RELEASE_ROOT`（默认 `C:/vaultsyncd-test`）、`CFAPI_RELEASE_RENAME_ROOT`（默认 `C:/vaultsyncd-test-rename`）；S009—S011 的变量不变 |
| `run-cfapi-release.sh`（S012） | 宿主 SHA-256 + `cargo run --example cfapi-prototype -- expect` → scp exe 与 `guest-cfapi-release.ps1` → ssh 执行 → 比对 exe 与每个 `guest_file_sha256` → 统计 `check=pass|fail` 与 `blocked_item=`；任一 fail/MISMATCH、未报告 `cfapi_release_result: OK` 即非零退出（`blocked_item` 只打印不失败，阻塞项由交接与 §18.6 记录） |
| `guest-cfapi-release.ps1`（S012） | `-Directory`/`-Root`/`-RenameRoot`/`-Exe`；顺序：清场 → create/inspect → 已脱水条目 protect（不为判定而水合）→ 干净样本 protect/release（`in_sync`/`dehydrate` 全 0x0、`on_disk_after=0`）→ 新进程 `inspect` 复核脱水 → 再读哈希不变 → 原地写 / 映射写 / 映射仍打开 / 独占句柄 / 共享锁五类保存（每类前后 SHA-256 与判定断言，句柄消失后可释放）→ rename-over（单独同步根，记录阻塞现象后 `cleanup`+`rd` 恢复）→ 收尾清理与用户内容哈希。逐项 `check=pass|fail`，另输出 `blocked_item=` 与 `cfapi_release_result: OK|FAILED` |
| `guest-cfapi-hydration.ps1`（S011） | `-Directory`/`-Root`/`-Exe`；按“清场 → create → 离线未水合失败（3 条目）→ 整份读取 + 哈希 → 离线读已水合 + 未水合仍失败 → 范围水合（平台范围必须是部分区间）→ 传输中断开同步根（循环早停、有界返回）→ 重置后再取消（句柄可关闭、有界返回）→ 重置后超时（明确失败、有界）→ 超时失败后再读成功 → `inspect --allow-hydrated` → cleanup 保留用户内容/注销/幂等”顺序执行，逐项 `check=pass|fail`，最后打印 `cfapi_hydration_result: OK|FAILED` |

## tools/xtask — 任务编排入口

状态：S006 实现 `env`/`check`，S024 实现 `generate`（`package` 仍明确未实现）。包名 `vaultsyncd-xtask`，二进制名 `xtask`，edition 2024。S024 起允许第三方解析器依赖：`serde`（derive）、`serde_yaml_ng` 0.10、`toml` 0.9，全部由根 `Cargo.lock` 锁定；不引入模板引擎，生成逻辑在本包内。它属于 workspace `members` 但**不在 `default-members`**：便携的 `cargo check --locked`/`cargo test --locked`/`cargo clippy` 只构建核心成员，不构建编排工具。

调用入口是根 `.cargo/config.toml` 的唯一别名 `xtask = "run --package vaultsyncd-xtask --"`；该文件不设置目标、镜像、链接器或网络行为。所有子命令都在 `cargo locate-project --workspace` 找到的根目录执行，因此可从任意子目录调用（构建信息由 cargo 写到 stderr）。

| 子命令 | 行为 | 退出码 |
| --- | --- | --- |
| `env` | 只读诊断：定位 workspace 根；探测 `rustc/cargo/node/pnpm --version` 的实际输出（工具缺失时标「不可用」，不写死版本）；用 `cargo tree --locked --workspace --depth 0 --prefix none` 列出成员；列出 `Cargo.toml`、`Cargo.lock`、`package.json`、`pnpm-lock.yaml`、`rust-toolchain.toml`、`node_modules` 是否存在 | 0；无法定位 workspace 时 1 |
| `check` | 先确认 `<root>/node_modules` 存在：缺失立即报错并提示 `pnpm install --frozen-lockfile`，不跳过前端检查。随后按固定顺序执行下表五步，逐步打印命令与退出码，末尾汇总失败步骤；子进程继承终端，诊断即时可见 | 全部通过 0；任一步失败 1 |
| `generate` | 从 `api/openapi.yaml` 生成 `crates/protocol/src/generated/mod.rs` 与 `packages/api-client/src/generated/*`；只写有变化的文件并打印路径。`--check` 不写盘，只逐字节比对，不一致时列出文件并退出 1。配置、契约与生成器的校验规则见下文「api/ — 远端协议契约源」 | 无 diff/写入成功 0；配置漂移、未支持构造、`--check` 有 diff 1；多余参数 2 |
| `package` | 产品打包：尚未实现，同上 | 3 |
| `help`、`--help`、`-h` | 打印子命令与退出码说明 | 0 |
| 其他 / 缺少子命令 / 多余参数 | 打印未知名或参数错误并附帮助 | 2 |

`check` 的步骤表（顺序固定）：

| 步骤名 | 命令 |
| --- | --- |
| `protocol-regenerate` | 进程内比对契约与生成代码（等价 `cargo xtask generate --check`），排在最前，失败即说明生成代码与 `api/openapi.yaml` 不同步 |
| `rust-fmt` | `cargo fmt --check` |
| `rust-clippy` | `cargo clippy --locked --all-targets -- -D warnings` |
| `rust-test` | `cargo test --locked` |
| `frontend-typecheck` | `pnpm --recursive run typecheck` |

覆盖面：`cargo fmt --check` 覆盖全部成员（含 `tools/xtask` 与生成的 Rust 文件）；clippy 与 test 按 Cargo 语义只作用于 `default-members`（当前 `crates/domain`、`crates/protocol`），xtask 自身需单独 `cargo clippy -p vaultsyncd-xtask --locked --all-targets -- -D warnings` 验证，扩展名成员后是否改为 `--workspace` 由后续卡按 §5.7/§5.8 决定。`protocol-regenerate` 的失败是非零退出，因此 CI 的单步 `cargo xtask check` 已经包含“重新生成无 diff”检查。`pnpm --filter @vaultsyncd/api-client test` 的运行时断言不在 `check` 内（`check` 只跑 typecheck），需要在协议相关卡手动执行。

约束：`env` 不硬编码会漂移的版本与成员清单；`check` 不吞输出、失败不报成功；未实现子命令不产生副作用。

## .github/workflows/checks.yml — 便携 CI

状态：S006 接入，仅 Linux runner；平台矩阵、安装包与 Windows 原生验证留给后续卡（S101）。仓库当前**无远端**（`git remote -v` 无输出），因此只有本地证据（actionlint 与命令一致性），未在 CI 上真实运行。

| 项目 | 契约 |
| --- | --- |
| 触发 | `push`（任意分支）与 `pull_request`，`permissions: contents: read` |
| 工具链来源 | YAML 不重复写版本：Rust 取 `rust-toolchain.toml`（`rustup show` 读取并在缺失时安装，含 rustfmt/clippy 组件）；Node 取 `package.json` 的 `engines.node`（`actions/setup-node` 的 `node-version-file: package.json`）；pnpm 取 `packageManager`（`pnpm/action-setup` 默认读取） |
| 缓存 | `actions/cache` 缓存 `~/.cargo/registry`、`~/.cargo/git`、`target`，键含 `Cargo.lock` 与 `rust-toolchain.toml` 哈希；前端用 `actions/setup-node` 的 `cache: pnpm` |
| 检查入口 | 先 `pnpm install --frozen-lockfile`，再用单步 `cargo xtask check`；不在 YAML 重写 fmt/clippy/test/typecheck |
| 本地验证 | `nix shell --inputs-from . nixpkgs#actionlint -c actionlint .github/workflows/checks.yml` 退出 0（actionlint 1.7.12） |

## tools/test-volume/test-volume.sh — 测试卷生命周期

状态：S007 实现（`create`/`verify`/`cleanup`/`status` 可用）。Bash（`set -eu`、`pipefail`），只做主机侧测试环境准备，不属于 Rust workspace、不参与 `cargo xtask check`，也不需要 `node_modules`。实测证据与路径规划见 `docs/environment.md` §11。

| 子命令 | 输入 | 行为 | 退出码 |
| --- | --- | --- | --- |
| `create <run_id> [大小]` | `run_id` 匹配 `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`；`大小` 为 truncate 语法，默认 `512M` | 在 lab 根内建 `runs/<run_id>/`，创建 btrfs 子卷 `volume/`、稀疏镜像 `loop.img`、挂载点 `mnt/` 与元数据 `run.env`；`mkfs.btrfs` + `mount` 后写 meta。同一 `run_id` 已存在则明确拒绝 | 0；参数/路径/工具/sudo/已存在/挂载核对失败 1 |
| `verify <run_id>` | 同上（无大小参数） | 只读：路径规范化与祖先检查（受保护路径、含 `.git`、`.worktrees`）、`findmnt -T` 后端与来源、btrfs 文件系统 UUID/subvolid、`statfs` 身份、`losetup -O BACK-FILE` 归属、`nodatacow/nodatasum` 挂载选项、run 目录下嵌套挂载白名单、`volume/` 与 `mnt/` 为空 | 0 = PASS；run 不存在/已清理 1；任何断言不符 1 |
| `cleanup <run_id>` | 同上 | 顺序：条目白名单校验 → 挂载来源核对后 `umount` → 只 detach 后端等于本 run 镜像的 loop → 核对 subvolid 与空目录后 `btrfs subvolume delete` → `rm`/`rmdir` 本 run 文件并尝试删除空的 `runs/` 与 lab 根 | 0；缺 meta、未知条目、来源不符、subvolid 变化、非空子卷、残留 1 |
| `status` | 无参数 | 只读列出 lab 根后端/挂载点/可用空间与每个 run 的 loop 设备、挂载状态；lab 根不存在时打印「未创建」 | 0；lab 根非法或 sudo 不可用 1 |
| `--help` / `-h` / `help` | 无 | 打印命令、环境变量、`run_id` 规则与 sudo 要求 | 0 |
| 其他 / 缺参数 / 多余参数 | — | 打印用法后报错 | 1（空子命令 2） |

环境变量与约束：

| 项目 | 契约 |
| --- | --- |
| `VAULTSYNCD_LAB_ROOT` | 默认 `/data/vaultsyncd-lab`。必须绝对且已规范化，必须位于 `/data` 或 `/var/lib` 之下；拒绝 `/`、`/home`、`/nix`、`/var/log`、`/.snapshots`、`/data/llm`、`/data/.snapshots`、`/wrd`、`/tmp` 等受保护路径及其内部，拒绝 Git 工作区与 `.worktrees` |
| `VAULTSYNCD_LOOP_SIZE` | loop 镜像默认大小，等价于 `create` 的可选第二参数 |
| 派生路径 | 镜像、挂载点、子卷、meta 全部由 `run_id` 派生并逐条断言落在 `runs/<run_id>/` 内，调用者不能传任意路径 |
| 提权 | 只用 `sudo -n`（`losetup`/`mkfs.btrfs`/`mount`/`umount`/`btrfs subvolume`）；失败立即退出，不提示密码，不回退到交互 sudo |
| 元数据 | `run.env` 记录 `run_id`、`created_at`、lab/run/volume/image/mount 路径、`volume_subvol_id`、`volume_fs_uuid`、`image_size(_bytes)`、`image_fs_uuid`、`loop_device`；`verify`/`cleanup` 缺字段即拒绝操作 |
| 幂等性 | `create` 对已存在 `run_id` 明确拒绝（不部分修复）；`verify` 对已清理 run 明确报不存在；`cleanup` 对已清理 run 报不存在，不静默成功 |
| 不变量 | 不格式化既有块设备、不修改 fstab、不触碰 `/data/llm` 与其他 run、不执行 `rm -rf`（只用 `rm -f` 指定文件与 `rmdir`） |

## tools/windows-lab/bootstrap-host.sh — Windows 客体宿主侧引导

状态：S008 实现。Bash（`set -euo pipefail`），只做宿主侧准备，不属于 Rust workspace、不参与 `cargo xtask check`；需要在 `nix develop .#quickemu` 内运行。模板同目录：`autounattend.xml.template`、`provision.ps1`、`windows-11.conf.template`。操作说明与实测证据见 `docs/operations/windows-lab.md` 与 `docs/environment.md` §12。

| 子命令 | 输入 | 行为 | 退出码 |
| --- | --- | --- | --- |
| `fetch-virtio` | `VIRTIO_URL`（默认 virtio-win 官方稳定版） | 下载驱动 ISO 到 `downloads/`，用 ISO 9660 魔数（偏移 32768 的 `CD001`）判定不是反爬 HTML，再复制到 `windows-11/virtio-win.iso` | 0；非 ISO/下载失败 1 |
| `fetch-iso` | `ISO_URL`（默认 Evaluation Center 链接） | HEAD 取重定向链最后一段 `Content-Length`，与本地字节数比对；不足则 `curl --continue-at -` 续传；超出或哈希与已记录值不符则报错；结果写 `windows-11/windows-11.iso.sha256` | 0；大小不符/sha 不符/无 Content-Length 1 |
| `prepare` | `secrets/lab-credentials.txt`、`ssh/vaultsyncd-lab_ed25519` | 首次生成 32 位随机口令（0600）与 ed25519 密钥；用口令渲染 `autounattend.xml`（残留 Pro KMS 密钥或残留占位符即失败）；复制 `provision.ps1` 与 `ssh_authorized_key.pub`；用 `xorriso -as mkisofs -quiet -J` 重打 `unattended.iso`；渲染 `windows-11.conf` | 0；缺 ISO/驱动 ISO、占位符未替换、密钥残留 1 |
| `start` | `windows-11.conf` | 先查 `SSH_PORT`/`SPICE_PORT` 占用（不依赖管道退出码），再 `setsid nohup quickemu --vm ... --display none --viewer none --access local --spice-port <p> --public-dir none` 后台启动并打印端口/进程状态 | 0；端口被占、conf 缺失 1 |
| `wait-ssh` | `WAIT_SSH_SECONDS`（默认 2700） | 每 15 s 用密钥执行 `ver`，成功即返回 | 0；超时 1 |
| `facts` | 无 | 通过 SSH 打印 `ver`、`Get-ComputerInfo` 版本、`Get-Volume C`、`fsutil fsinfo volumeinfo C:` | 0；SSH 失败非零 |
| `stop` | `STOP_SECONDS`（默认 180） | SSH 执行 `shutdown /s /t 0`，轮询 `windows-11/windows-11.pid` 直到 qemu 退出 | 0；qemu 未退出 1（不 kill -9） |
| `status` | 无 | 打印 pid 是否存活、`2222`/`5930` 监听、`windows-11.log` 末尾 | 0 |
| 空参数 / 未知子命令 | — | 打印用法 / 明确报错 | 2 / 1 |

环境变量与约束：

| 项目 | 契约 |
| --- | --- |
| `VM_ROOT` | 默认 `/data/vaultsyncd-lab/win-lab`，必须在源码树之外；所有派生路径都相对它，不写入仓库 |
| `VM_NAME` / `LAB_USER` / `SSH_PORT` / `SPICE_PORT` | 默认 `windows-11` / `Quickemu` / `2222` / `5930`；第二台客体改这些值，不要复用同一 `tpm2-00.permall` 或 qcow2 |
| 凭据 | 口令只在 `secrets/lab-credentials.txt`（0600，目录 0700）；私钥只在 `ssh/`（0600）；脚本不打印口令与私钥 |
| 幂等性 | `prepare` 可重复执行（复用已有口令/密钥，重打 ISO 与 conf）；`fetch-iso` 以远端字节数与已记录 sha256 判定，不重复下载完整文件；`start` 不做隐式 `--kill` |
| 不变量 | 不格式化既有块设备、不改宿主防火墙/宿主 SSH/`/etc/nixos`、不使用第三方 Windows 镜像、不在仓库内写镜像或凭据 |

## tests/windows-integration + tests/platform/windows — Windows 交叉构建与客体验证入口

状态：S009 实现，S010 在同一目录新增 CFAPI 原型的构建/传输/客体脚本（见上一节“`tests/platform/windows` 的新增入口（S010）”）。`tests/windows-integration/` 是 design §5.8 指定的 Cargo 包 `vaultsyncd-windows-tests`（workspace `members`，**不在** `default-members`）；`tests/platform/windows/` 放宿主侧脚本与客体侧 PowerShell 脚本。操作步骤见 `docs/operations/windows-lab.md` §12（S009）与 §16（S010），实测证据见 `docs/environment.md` §13、§14。

### 包与二进制

| 项目 | 契约 |
| --- | --- |
| 包 | `tests/windows-integration/Cargo.toml`，包名 `vaultsyncd-windows-tests`，无第三方依赖；`[lints] workspace = true`，FFI 需要的 `unsafe_code` 例外只以各 bin 的 crate 级 `#![allow(unsafe_code)]` 放行 |
| `platform-smoke` | 打印 `build_target`、`GetSystemInfo.wProcessorArchitecture`、`RtlGetVersion` 版本、`version.dll`/`kernel32.dll` 的 `VS_FIXEDFILEINFO` 版本；退出码 0 全部成功，1 任一调用失败或架构不是 AMD64 |
| `cfapi-link-smoke` | 调用 `CfGetPlatformInfo`（`#[link(name = "cldapi")]`），打印 HRESULT 与 `CF_PLATFORM_INFO` 三个字段；只读，不注册同步根；退出码 0 表示 `SUCCEEDED`，1 表示失败 |
| 非 Windows 构建 | 两个 bin 的 `main` 在非 Windows 目标上只打印提示并返回退出码 1，因此误跑 Linux 产物不会假装成功 |
| 输出约束 | 只用 ASCII：客体控制台代码页实测为 437，非 ASCII 会在转写日志中乱码 |

### `tests/platform/windows/common.sh`

被三个脚本 `. common.sh` 引入，不单独执行。定义 `TARGET_TRIPLE=x86_64-pc-windows-msvc`、`TEST_PACKAGE`、`SMOKE_BINARIES`、`ARTIFACT_SUBDIR=windows/$TARGET_TRIPLE`、客体默认连接参数（`GUEST_HOST`/`GUEST_PORT`/`GUEST_USER`/`GUEST_SSH_KEY`/`GUEST_DIR`，均可用环境变量覆盖）与 `repo_root`/`windows_cache`/`guest_ssh`/`guest_scp` 四个函数；`guest_*` 固定 `IdentitiesOnly=yes`、`BatchMode=yes`、`StrictHostKeyChecking=no`、`LogLevel=ERROR`。

### 脚本

| 脚本 | 输入 | 行为 | 退出码 |
| --- | --- | --- | --- |
| `setup-toolchain.sh` | `rust-toolchain.toml`（channel/targets） | 必须在 `nix develop .#windows` 内运行；`rustup toolchain install --profile minimal --no-self-update`（按 toolchain 文件解析），复核 `cargo`/`rustc`/`target-libdir` 下 `libstd-*.rlib` 存在，打印 cargo-xwin/clang-cl/lld-link/llvm-lib 版本与三个缓存目录大小；幂等 | 0；不在 `windows` shell、缺标准库目录 → 1 |
| `build-cross.sh` | workspace、缓存 | 必须在 `nix develop .#windows` 内运行；导出 `RUSTFLAGS="-C target-feature=+crt-static"`（客体缺 `VCRUNTIME140.dll`，见 `docs/environment.md` §13.4），执行 `cargo xwin build --release --locked --target $TARGET_TRIPLE -p $TEST_PACKAGE`，把两个 `*.exe` 复制到 `artifacts/windows/x86_64-pc-windows-msvc/` 并写 `SHA256SUMS` | 0；缺产物或 `cargo xwin` 失败 → 非零 |
| `run-on-guest.sh` | `artifacts/` 产物、客体 SSH | 计算宿主 `sha256sum`；在客体建 `GUEST_DIR`；只 scp 两个 exe 与 `guest-run.ps1`；经 SSH 执行 PowerShell 脚本；去掉 CR 后按行解析 `guest_sha256`/`guest_exit_code`，逐个与宿主哈希比对 | 0 全部执行成功且哈希一致；客体执行失败 → 客体的退出码；哈希缺失/不一致 → 1 |
| `guest-run.ps1` | `-Directory`、`-Executables`（逗号分隔） | 在客体打印 OS 版本/构建号/架构/PowerShell 版本，逐文件 `Get-FileHash`，逐个执行并记录 `guest_exit_code`；`-File` 调用方式不会把多个位置参数绑成数组，因此在脚本内按逗号拆分 | 0 全部二进制返回 0；否则 1 |

### 不变量

- 只传产物与执行脚本，不复制 `target/`、`node_modules/`、缓存或源码树；客体不安装任何编译工具链。
- 交叉编译成功不是验收证据：`run-on-guest.sh` 必须在客体原生执行并通过两端哈希比对。
- 退出码用于判定，不把“构建通过”“SCP 成功”当作运行成功。

## crates/platform-linux-server — Linux 服务端挂载准入（S013）

状态：S013 实现 `mounts/`（挂载准入、文件系统身份、NOCOW 检查、根目录遍历）；S014 实现 `files/`（受限读写、暂存、刷盘、受控替换，见后一节）；`watcher/`、`ipc/` 尚未实现。包名 `vaultsyncd-platform-linux-server`，**无第三方依赖**（内核 ABI 在 `mounts/raw.rs` 最小声明并带 `const` 布局断言），属于 workspace `members` 但**不在** `default-members`（只在 Linux 上编译）。宿主/服务端的可移植检查不会构建它，验证用 `cargo test --locked -p vaultsyncd-platform-linux-server` 与 `tests/platform/linux/` 的脚本。实测证据见 `docs/environment.md` §15。

### 模块与边界

| 项目 | 契约 |
| --- | --- |
| `mounts::raw`（crate 内可见） | `fstatfs(2)`、`statx(2)`、`ioctl(2)` 与三个**只读**请求：`FS_IOC_GETFLAGS`、`BTRFS_IOC_FS_INFO`、`BTRFS_IOC_INO_LOOKUP`。`struct statfs`/`struct statx`/两个 btrfs 结构的 LP64 布局有 `size_of`/`offset_of` `const` 断言（编译期自检）。未声明任何设置标志、清理 NOCOW、改挂载的调用 |
| `mounts::probe` | `probe_path`：`canonicalize` → `O_PATH|O_DIRECTORY|O_NOFOLLOW` 句柄 → `fstatfs`+`statx`（`STATX_INO`/`STATX_BTIME`/`STATX_MNT_ID`/`STATX_SUBVOL`）→ 第二个 `O_RDONLY` 句柄做 `ioctl` 并核对 inode 未变 → 按挂载 ID 关联本进程 `/proc/self/mountinfo` → btrfs 上取 FSID 与子卷 ID。`scan_root`：遍历目录找嵌套子卷与 NOCOW 目录。`inode_flags_of_path`：单文件/目录的 `FS_IOC_GETFLAGS` |
| `mounts::mountinfo` | `/proc/self/mountinfo` 解析：挂载 ID、父 ID、`major:minor`、`root`、挂载点、每挂载选项、可选字段、fstype、源、超级块选项；还原 `\040`/`\011`/`\012`/`\134` 转义。坏行报错而不是跳过 |
| `mounts::paths` | 按路径分量判 `Same`/`Ancestor`/`Descendant`/`Unrelated`；`/data/a` 与 `/data/abc` 无关（design §16.1 禁止字符串前缀判断） |
| `mounts::admission` | 纯逻辑：后端分类、判定顺序、拒绝原因、身份比较、子挂载检测 |
| `mounts::content` | 纯逻辑：`Empty`/`Sparse`/`FullyAllocated` 内容形态；**稀疏不是占位证据** |
| 非 Linux 目标 | `mounts` 用 `cfg(target_os = "linux")` 排除；在其它平台该 crate 是空库，不假装提供能力 |

### 判定顺序与原因码（`storage-admission inspect`）

顺序固定，只报第一个命中的原因：

| # | 检查 | `reason`（`code()`） | 设计错误码 |
| --- | --- | --- | --- |
| 1 | `fstatfs` 幻数/fstype 必须是 btrfs；拒绝 FUSE（含 `fuse.*`、`fuseblk`）、NFS、CIFS/SMB、overlay、tmpfs、ext4/xfs/zfs 及其它后端 | `unsupported_backend` | `UNSUPPORTED_STORAGE_BACKEND` |
| 2 | btrfs 上必须取得 FSID 与子卷 ID | `subvolume_identity_unavailable` | 同上 |
| 3 | Root 目录自身 `FS_IOC_GETFLAGS` 必须可读且不含 `FS_NOCOW_FL`（`0x00800000`） | `nocow_inode_flags` / `inode_flags_unavailable` | 同上 |
| 4 | 挂载/超级块选项不得含 `nodatacow`/`nodatasum` | `nocow_mount_option` | 同上 |
| 5 | 目录遍历必须完整（未触发上限），否则结论是“未完成”而不是通过 | `scan_depth_limit_reached` / `scan_entry_limit_reached`（`decision=inconclusive`，退出码 4） | — |
| 6 | Root 内不得有嵌套子卷（子卷根 inode 恒为 256，再用 subvol ID 确认） | `nested_subvolume` | `UNSUPPORTED_STORAGE_BACKEND` |
| 7 | 遍历到的目录不得带 NOCOW | `nocow_inode_flags`（带相对路径） | 同上 |
| 8 | Root 内不得有任何子挂载（bind mount、tmpfs、其它 fs 都算） | `nested_mount` | 同上 |
| 9 | 内部状态目录必须与 Root 不重叠、本身是 btrfs；与 Root 不在同一 btrfs 文件系统则拒绝 | `internal_state_overlap` / `internal_state_unsupported_backend` / `internal_state_other_filesystem` | 同上 |
| 10 | 已注册 Root：同一目录（别名）拒绝；路径重叠拒绝 | `duplicate_root` / `overlapping_root` | 同上 |

判定通过时输出 `note=` 行记录事实（`fsid`、`subvol_id`、`mount_id`、`mount_id_source`、`scan_entries`，以及内部状态目录在不同子卷/不同挂载时的 `internal_state_separate_subvolume=`/`internal_state_separate_mount=`）。运行期复核用 `admission::verify_unchanged(recorded, current)`：漂移返回 `filesystem_changed`/`subvolume_changed`/`directory_changed`，对应 `ROOT_UNAVAILABLE`；本卡只提供判定函数，重验时机属后续卡。

### 身份与别名规则

| 项目 | 契约 |
| --- | --- |
| 存储单元 | `(FSID, 子卷 ID)`。同子卷的不同路径（bind mount、`..`、符号链接）相等；`same_storage` 据此判定。`fsid` 取自 `BTRFS_IOC_FS_INFO`（无需特权），与 `btrfs filesystem show` 的 UUID 文本一致 |
| 目录身份 | `(FSID, 子卷 ID, inode)`；inode 只在所属子卷内解释（`same_directory`、`verify_unchanged`） |
| 子卷 ID 来源优先级 | ① `statx` 的 `STATX_SUBVOL`/`stx_subvol`（内核 6.7 起，**无需特权**）；② `BTRFS_IOC_INO_LOOKUP`（需要 `CAP_SYS_ADMIN`）；③ mountinfo 超级块选项 `subvolid=`（仅当该目录本身就是挂载点）。全部不可用时拒绝，并在输出里保留原始错误 |
| 挂载 ID 来源 | `statx` 的 `STATX_MNT_ID` 直接对应 mountinfo 的挂载 ID；内核不提供时退化为“覆盖该路径的最长挂载点”，并在 `mount_id_source=` 里标明，不假装是内核给出的 |
| 设备号 | btrfs 每个子卷有独立匿名设备号，`statx` 的 `stx_dev` 与 mountinfo 的 `major:minor` 本来就不同（实测 `statx_device=0:121` vs `mount_device=0:54`）；因此**不用设备号做身份或一致性校验**，只在输出里分别记录 |
| 符号链接 | `probe_path` 先 `canonicalize` 再以 `O_NOFOLLOW` 打开；别名得到同一身份。运行时以根句柄为边界，注册期 `canonicalize` 不消除之后的 TOCTOU（design §16.1） |

### 稀疏、占位与第三方 stub 边界

- `content::classify_content_shape` 只描述“逻辑大小 vs 已分配字节”。btrfs 上空洞、预分配、压缩与 CoW 共享 extent 都会让分配小于逻辑大小，这些是普通本地文件；**不能**因此判为占位/按需回源文件。
- 反向边界无法自动识别：第三方 daemon 把本地目录当占位或回源视图时，Linux 没有可依赖的统一判定方式（design §11.4）。S013 因此只用“允许的后端 + 已知标志 + 部署契约”约束；`SERVER_PLACEHOLDER_UNSUPPORTED` 不出现在本卡的自动判定结果里，留给显式部署契约。
- 是否“能读到完整字节”由内容版本确认流程验证（design §11.4），不在挂载准入里推断。

### 原型命令 `storage-admission`（example 目标）

`crates/platform-linux-server/examples/storage-admission.rs`，`cargo build --locked -p vaultsyncd-platform-linux-server --example storage-admission`。它是原型诊断入口，不是产品 CLI。输出为稳定的 `key=value` 行。

| 子命令 | 输入 | 行为 | 退出码 |
| --- | --- | --- | --- |
| `inspect PATH [--internal-state-dir PATH]… [--registered-root PATH]… [--max-scan-entries N] [--max-scan-depth N]` | 候选 Root 与可选上下文 | 打印探测事实（`backend`/`fstype`/`fs_magic`/`statfs_fsid`/`mount_id(_source)`/`mount_point`/`mount_root`/`statx_device`/`mount_device`/`inode`/`btime`/`fsid`/`subvol_id(_source)`/`inode_flags`/`nocow`、`scan*`、`submounts`）+ `decision`/`reason`/`error_code`/`detail`/`note` | 0 接受；3 拒绝；4 未完成；1 探测错误；2 用法错误 |
| `identity PATH` | 路径 | 只打印身份字段（挂载 ID、设备、inode、FSID、子卷 ID） | 0；1 探测错误 |
| `same-storage PATH_A PATH_B` | 两个路径 | 打印 `same_storage=`、`same_directory=` 与两侧 mount id | 0 同一存储；3 不同；1 探测错误 |
| `content PATH` | 文件或目录 | 打印 `logical_size`、`allocated_bytes`、`content_shape`、`inode_flags`、`nocow`、`placeholder_evidence=none` | 0；1 读元数据失败 |
| 扫描上限默认值 | — | `max_scan_entries=20000`、`max_scan_depth=32`；超出即 `decision=inconclusive`，不当作通过 | — |

### `tests/platform/linux/` 验收入口（S013）

| 脚本 | 行为 | 退出码 |
| --- | --- | --- |
| `common.sh` | 被三个脚本 source：`repo_root`、`require_dev_shell`（缺 cargo/btrfs/findmnt/losetup 即失败）、`require_sudo_noninteractive`（`sudo -n`）、`build_admission_bin`/`build_file_publish_bin`、`run_admission`/`run_file_publish`（结果统一写入 `CMD_OUT`/`CMD_STATUS`；`run_admission` 另保留 `ADMISSION_OUT`/`ADMISSION_STATUS` 别名）、`expect_decision`/`expect_field`/`expect_line`/`expect_status`、`snapshot_mounts`/`snapshot_loops`、`umount_checked`/`umount_tolerant`。失败累加并继续，末尾 `summary` 非零 | — |
| `run-storage-admission.sh` | 用 `tools/test-volume/test-volume.sh create <run_id>` 建立 run 目录，再在**该 run 目录内**做真实样本：btrfs 子卷与 loop 卷接受；bind mount/符号链接/`..` 别名同存储；`/tmp`、`/dev/shm`、`/run`（tmpfs）、`/proc`、自建 overlay、64 MiB 稀疏镜像上的 ext4 拒绝；嵌套子卷、子挂载、NOCOW 目录（`chattr +C`）与 NOCOW 文件拒绝；内部状态目录重叠/跨文件系统拒绝、同卷另一子卷接受并记录备注；已注册 Root 的别名与重叠拒绝；8 MiB 稀疏文件与 1 MiB 随机文件的内容形态；用法/探测错误退出码 | 0 全部通过；1 有用例失败或清理失败 |
| `run-reflink.sh` | 在 `/data` 卷上建两个兄弟子卷，`cp --reflink=always`（FICLONE）同卷跨子卷与同子卷克隆必须成功、`filefrag -v` 必须报告共享 extent；跨文件系统克隆必须失败（EXDEV）且不留目标文件；源改写后副本保持旧字节 | 同上 |

清理与隔离要求（这三个脚本共同）：

- 所有副本、loop、挂载、子卷只在 `runs/<run_id>/` 内；`create` 之后脚本把自己要写的目录 `chown` 给当前用户，子卷/挂载仍只用 `sudo -n`。
- `trap cleanup EXIT` 逆序卸载挂载、detach 自建 loop、删除自建子卷与样本路径，最后调用 `test-volume.sh cleanup`（其条目白名单会拒绝任何未预期残留）。
- 结束时核对 `losetup -a` 与 `findmnt` 快照与开始时一致，并报告 run 目录已删除。

### 不变量

- 只读：不设置/清除 inode 标志、不改挂载参数、不做任何“自动修复”。
- 不把“取不到事实”当作通过：无法确认 FSID/子卷 ID、扫描被上限截断、`FS_IOC_GETFLAGS` 失败都返回拒绝或未完成，并保留原始错误。
- 后端检查发生在服务进程自己的 mount namespace；容器内路径字符串不是后端类型证明。
- 同一子卷的多个别名不重复计数；inode 与设备号都不单独作为身份。

## crates/platform-linux-server — 普通文件发布原型（S014）

状态：S014 在 `crates/platform-linux-server/src/files/` 落地受限目录句柄读写、目标文件系统内暂存、`FICLONE`/校验复制、文件与目录刷盘、受控替换与前提核对。包结构、挂载准入与身份规则见上一节（S013）。`watcher/`、`ipc/`、数据库与 `fs_intent` 仍属后续卡；本卡不含任何占位实现。

### 模块与契约

| 模块 | 契约 |
| --- | --- |
| `files::raw`（crate 内可见） | `openat2`（`__NR_openat2=437`，经 `syscall(2)`）、`openat`、`renameat2`、`unlinkat`、`fsync`、`fchmod` 与 `FICLONE` ioctl 的最小声明；`struct open_how` 24 B 有 `size_of`/`offset_of` 断言，请求号与 `RESOLVE_*`/`RENAME_NOREPLACE` 有自检。errno 只做分类，不吞错 |
| `files::restricted` | `RestrictedDir`：`O_RDONLY|O_DIRECTORY|O_NOFOLLOW` 根句柄 + `child_dir`/`open_path`（`O_PATH`）/`open_read`/`create_exclusive`（`O_CREAT|O_EXCL`）/`rename`/`unlink`/`fsync_dir`/`identity_at`/`mount_id`。`validate_relative_path` 是纯逻辑，拒绝空、绝对路径、`.`、`..`、空段、NUL、超长段/路径。解析优先 `openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS)`：逃逸 → `escape_rejected`（`EXDEV`）、符号链接 → `symlink_rejected`（`ELOOP`）；仅 `ENOSYS` 回退逐段 `openat`+`O_NOFOLLOW`，`EPERM` 等不回退。`--resolve auto|openat2|walk` 可强制机制，输出 `resolve=openat2|component_walk` |
| `files::identity` | `FileIdentity`（`dev_major`/`dev_minor`/`inode`/`btime`/`size`/`mtime`/`ctime`/`nlink`/`mode`/`mount_id`）来自 `statx(AT_EMPTY_PATH)`；`same_object` 只比设备+inode+btime，`changed_fields` 是完整指纹差异（纯逻辑）。`to_spec`/`parse_identity_spec` 是前提文件的唯一编解码（缺字段、重复字段、非法时间戳都报错）。`ExpectedTarget{Absent,Present}` + `check_target` → `Create`/`Replace`/`Refused{target_appeared,target_missing,target_changed}` |
| `files::hash` | 零依赖 SHA-256（NIST 向量与流式边界自测）。用于校验复制的写入/读回比对与对象摘要，不用于任何认证 |
| `files::clone` | `stage_copy`：暂存在**目标目录**内创建独占文件（因此与目标同挂载，创建后核对 `staging_mount_id == target_mount_id`）。`FICLONE` 成功 → `method=ficlone, fallback=none`；`EXDEV` → `method=verified_copy, fallback=exdev`；`EOPNOTSUPP`/`ENOTTY`/`EINVAL` → `method=verified_copy, fallback=unsupported`；`EIO`/`ENOSPC`/`EDQUOT`/权限错误 → `reflink_failed`，**不退回、不重试**。校验复制后 `fsync` 目标、读回比对长度与 SHA-256、对比源前后指纹（变化即 `source_changed`）。失败时删除未验证的暂存文件并报告删除结果（`staging_removed`/`cleanup_error`） |
| `files::publish` | 两步 API：`stage_for_publish`（前提核对 → 暂存）与 `commit_staged`（替换前再核对 → `fchmod` → `renameat2` → `fsync` 目录 → 发布后核对身份/大小/权限）；`publish` 是两者的便捷组合。新建用 `RENAME_NOREPLACE`（内核在竞争时返回 `EEXIST` → `target_appeared`），替换用 0 旗标原子替换。前提不成立时目标不被修改，已建立的候选随错误返回（`RetainedCandidate`）。常量 `OBJECT_FILE_MODE=0o444`（对象只读）、`WORK_FILE_MODE=0o644` |

### 原型命令 `file-publish`（example 目标）

`crates/platform-linux-server/examples/file-publish.rs`，`cargo build --locked -p vaultsyncd-platform-linux-server --example file-publish`。角色目录在启动时 `canonicalize` 一次（模拟注册期处理），之后所有操作都以该目录句柄为边界。

| 子命令 | 输入 | 行为 | 退出码 |
| --- | --- | --- | --- |
| `identity ROOT REL` | 相对名称 | 打印 `FileIdentity::to_spec()` + `resolve=` | 0；3 拒绝（不存在按 `not_found`） |
| `hash ROOT REL` | 相对名称 | `decision=hashed`、`sha256=`、`bytes=`、`resolve=` | 0；3 拒绝 |
| `capture --source-root DIR --source REL --object-root DIR --object REL` | 工作文件 → 内容对象 | 按 `capture_object` 发布（`mode=0444`、`--expect absent`），输出 `decision`、`method`、`fallback`、`reflink_error`、`sha256`、`bytes`、`mode`、`source_mount_id`/`target_mount_id`/`staging_mount_id`、`source_inode`、`target_inode_before`/`target_inode_after`、`replaced`、`directory_fsynced`、`staging_name` | 0 成功；3 拒绝（对象已存在等）；1 错误 |
| `publish ... --target-dir REL --target-name NAME (--expect absent \| --expect-from FILE) [--race-hook CMD]` | 对象 → 工作文件 | `stage_for_publish` + `commit_staged`（`mode=0644`）；拒绝时打印 `decision=paused`、`reason`、`expected`/`observed`、`changed_fields`、`candidate`/`candidate_dir`/`candidate_sha256`。`--race-hook` 在两步之间执行一条命令，**仅用于验收外部竞争**，不是产品写入路径 | 0 发布；3 暂停；1 错误；2 用法 |
| `rename --from-root DIR --from REL --to-root DIR --to REL [--noreplace]` | 两个目录句柄 | 直接调用 `renameat2`，跨文件系统返回 `cross_device_rename`（`EXDEV`），不留目标文件 | 0 已重命名；3 拒绝；1 错误 |
| 全局 `--resolve auto\|openat2\|walk` | — | 选择解析机制并写入 `resolve=` | 2 未知取值 |

### `tests/platform/linux/run-file-publish.sh`（S014）

真实样本（`tools/test-volume/test-volume.sh create <run_id>`，全部在 `runs/<run_id>/` 内）：`work` 与 `state` 两个兄弟子卷 + loop btrfs 镜像（另一文件系统）。

1. `openat2` 与 `--resolve walk` 得到同一 inode，`hash` 与 `sha256sum` 一致。
2. 同卷跨子卷 `capture` → `method=ficlone`、`fallback=none`、`mode=0444`、`filefrag -v` 有共享 extent、追加写入对象被 `0444` 拒绝、`staging_mount_id == target_mount_id`。
3. 外部改写工作文件后，对象 `sha256` 与字节不变（与改写前副本 `cmp` 一致）。
4. 从对象发布新工作文件（`--expect absent`）与按身份前提替换已存在工作文件（`replaced=true`，目标换成暂存 inode）。
5. 外部竞争两条失败路径：暂存前已变化 → 拒绝且无候选；暂存后、替换前的注入写入（`--race-hook`）→ `decision=paused`、`reason=target_changed`、候选保留且哈希与源对象一致、目标未被覆盖。`--expect absent` 遇到已存在目标 → `target_appeared`。
6. 跨文件系统：`capture`/`publish` 走 `verified_copy` + `fallback=exdev`、`reflink_error` 保留 `EXDEV` 原文、源与目标挂载 ID 不同而暂存仍在目标挂载内、无共享 extent；跨设备 `rename` 返回 `cross_device_rename` 且源文件留在原文件系统。
7. 符号链接（末段与中间段）、`..`、绝对路径全部 fail-closed。
8. 重复采集已存在对象 → `target_appeared`，对象字节不变。

清理与隔离同 S013 三个脚本的共同要求（子卷/挂载/loop 的 `trap cleanup`、`losetup -a` 与 `findmnt` 快照核对、`test-volume.sh cleanup` 条目白名单）。

### 不变量

- 暂存永远建在目标文件系统/挂载内；跨文件系统不靠一次 `rename` 完成，也不在源文件系统里暂存后跨设备 `rename`。
- `EXDEV` 不被忽略也不被当作成功：走校验复制并在 `fallback=`/`reflink_error=` 里暴露；`EIO`/`ENOSPC`/权限错误不退回、不重试。
- 内容原样保留：对象只读（`0444`）；拒绝替换时保留候选；未验证的暂存一律删除并报告删除结果。
- 前提核对只覆盖“已观察到的身份/指纹”，不宣称消除与外部非协作写入之间的竞态（design §12.2 / C33）；检测到不一致就暂停并报告，不硬覆盖。
- 受限访问 fail-closed：`openat2` 的 `RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS` 或逐段 `O_NOFOLLOW`，逃逸/符号链接/`.`/`..` 都拒绝，且输出标明实际机制。

## crates/platform-linux-server — 安全路径访问正式接口（S032）

状态：S032 把 S014 的受限句柄原型升级为 Root 级正式接口 `files::root`，补目录列举、挂载边界复核、Root 身份漂移复核与 `mounts::overlap` 重叠判定。`ServerStorage` 的完整方法集（读完整版本、对象/历史/扫描）仍属后续卡（S033、S038、S049）；本卡不含占位实现。

### 模块与契约

| 模块 | 契约 |
| --- | --- |
| `files::root` | `RootHandle::open(path, ResolveMode)`：`probe_path` 读内核事实 → `O_DIRECTORY\|O_NOFOLLOW` 打开规范路径 → 核对句柄身份与探测事实是同一目录（否则 `root_changed_during_open`）→ 要求 `STATX_MNT_ID`（缺失即 `mount_id_unavailable`，fail-closed）。方法：`list_dir(REL)`、`open_read(REL)`、`create_exclusive(REL, mode)`、`identity()`、`verify()`、`path()`、`facts()`、`snapshot()`、`mechanism()`。空 `REL` 表示 Root 自身 |
| `files::root` 的挂载边界 | 每次列举/打开/创建都要求对象的 `STATX_MNT_ID` 等于 Root 的挂载 ID：不同 → `mount_boundary_crossed`（bind mount/子挂载），缺失 → `mount_boundary_unverified`。创建时**先打开并核对父目录**再用该句柄建文件，不在“先按路径建、事后发现跨挂载”的情况下往外部样本写入 |
| `files::root` 的身份复核 | `RootSnapshot{resolved_path, backend, fs_magic, fsid, subvolume_id, mount_id, dev_major, dev_minor, inode, btime}` 有 `to_spec`/`parse_spec`（缺字段、重复字段、非法值都报错）。`snapshot.compare(facts)` 按“后端 → FSID → 子卷 → 挂载 → 目录身份”返回首个 `RootDrift{backend_changed, filesystem_changed, subvolume_changed, mount_changed, directory_changed}`，`design_error_code()` 恒为 `ROOT_UNAVAILABLE`（运行期漂移暂停 Root，不推导删除）。`RootHandle::verify()` 同时复核“句柄自身仍是那个目录”和“路径仍指向同一存储单元与挂载” |
| `files::restricted`（S032 扩展） | `list_entries()` 用已打开目录句柄的 `getdents64(2)`（不用 `read_dir(路径)`），丢弃 `.`/`..`、按名称字节序排序、符号链接只报 `EntryKind::Symlink`；`DirEntry{name, kind, inode}`、`EntryKind{file, dir, symlink, other, unknown}`。新增错误码 `permission_denied`（`EACCES`/`EPERM`）、`read_dir_failed`、`mount_boundary_crossed`、`mount_boundary_unverified`，全部保留原始 `io::Error`。权限失败**不返回空列表** |
| `files::raw`（S032 扩展） | `getdents64(2)`（`__NR_getdents64` 按 `x86_64=217`/`aarch64=61` 声明；其它架构返回 `Unsupported`，不静默空目录）与 `linux_dirent64` 纯解析器（长度越界、名称未 NUL 结尾都报错），`d_type` 常量与头部长度 19 有自检 |
| `mounts::overlap` | `DirectoryRecord{role: root\|internal, facts}` + `find_overlaps(&[..])`：同一规范路径 → `same_path`；同一目录对象（设备+inode+btime）经不同路径 → `alias`（bind mount/别名，挂载 ID 不同也要报）；内部目录落在 Root 内 → `internal_inside_root`；Root 落在内部目录内 → `root_inside_internal`；其余父子关系 → `parent_child`。父子关系按路径分量（`/data/root` 与 `/data/rootx` 无关），只判定不决定是否允许 |
| `mounts::admission`（S032 扩展） | `BackendKind::parse(&str)` 与 `as_str()` 互逆，供 Root 快照规格读回；分类规则不变 |

### 诊断命令 `path-access`（example 目标）

`crates/platform-linux-server/examples/path-access.rs`，`cargo build --locked -p vaultsyncd-platform-linux-server --example path-access`。退出码：`0` 成功；`1` 探测/I/O 错误；`2` 用法错误；`3` 拒绝（逃逸、符号链接、跨挂载、权限不足、身份漂移、重叠）。

| 子命令 | 输出 |
| --- | --- |
| `list ROOT REL` | `decision=listed`、`root=`、`relative=`、`resolve=`、`root_mount_id=`、`entries=`、每条 `entry=NAME kind=KIND inode=N` |
| `open ROOT REL` | `decision=opened`、`resolve=`、`root_mount_id=` + `FileIdentity::to_spec()` |
| `create ROOT REL [--mode OCTAL]` | `decision=created`、`mount_id=`、`root_mount_id=`、`inode=`、`mode=`、`fsynced=true` |
| `snapshot ROOT` | `decision=snapshot` + `RootSnapshot::to_spec()`（交给 `verify --expect-from`） |
| `verify --expect-from FILE ROOT` | `decision=verified`、`verify=unchanged`；漂移时 `decision=refused`、`reason=<drift>`、`design_error=ROOT_UNAVAILABLE`，退出码 3 |
| `overlap (--root DIR \| --internal DIR)...` | `records=`、`record=N role= path= resolved= mount_id= dev= inode=`、`overlap=`、每对 `pair=I,J kind=KIND left=ROLE:PATH right=ROLE:PATH mount_ids=A:B`、`decision=clean\|overlapping`；有重叠时退出码 3 |

### `tests/platform/linux/run-path-access.sh`（S032）

真实样本（`tools/test-volume/test-volume.sh create <run_id>`）：`volume/root` 作为 Root、`volume/rootx` 作为同名前缀的兄弟目录、`external` 作为 Root 之外的外部样本（内容含标记串）、`state` 作为另一个 btrfs 子卷。

1. 列举：`openat2` 与 `--resolve walk` 条目数一致，`entry=... kind=... inode=...` 稳定。
2. 符号链接（目录、文件、中间段）一律 `symlink_rejected`，输出没有外部样本的目录项与内容，外部样本哈希不变；按外部目录列举同样拒绝。
3. 换目标竞态：后台持续在“Root 内真实目录（含 inside.txt）”与“指向外部样本的符号链接”之间切换，前台 40 次列举全部只能是 Root 内文件或明确拒绝（实测 26 次成功、14 次拒绝、0 次泄漏）。
4. 目录别名：`mount --bind external root/alias` 后列举 → `mount_boundary_crossed` 且无目录项；打开别名里的文件 → 同码且不读内容；在别名里创建 → 同码且外部目录没有新文件。
5. 权限失败：root 属主 `0700` 子目录对当前用户不可读 → `permission_denied` + 保留原始 `Permission denied`，且**没有** `entries=` 行；打开其中文件同样拒绝；Root 其余部分仍可列举。
6. 创建：Root 内新建 `mount_id == root_mount_id`、`mode=0644`、`fsynced=true`；重复创建 `already_exists`；父目录无权限时 `permission_denied` 且不落盘。
7. 身份复核：未变化 `verify=unchanged`；同目录 bind 覆盖 → `mount_changed`；换成另一个子卷 → `subvolume_changed`；换成 tmpfs → `backend_changed`；重命名后新建同名目录 → `directory_changed`；换回原目录再次 `unchanged`。
8. 重叠：兄弟目录 `overlap=0`；同名前缀兄弟不是父子；内部目录在 Root 内 → `internal_inside_root`；两个 Root 父子 → `parent_child`；同一路径 → `same_path`；`mount --bind` 同一目录到无关路径 → `alias` 且两条路径挂载 ID 不同。
9. 用法错误退出码 2；不存在的 Root 退出码 1 且 `decision=failed`。

清理与隔离同 S013/S014 脚本的共同要求（`trap cleanup`、`losetup -a` 与 `findmnt` 快照核对、`test-volume.sh cleanup` 条目白名单；`root` 属主的 `locked` 目录用非交互 sudo 删除）。

### 不变量

- 所有访问都以 Root 句柄为边界按相对名称进行；`canonicalize` 只用于注册期打开路径，运行期不重新解析路径，也不提供“改工作目录再按绝对路径访问”的入口。
- 挂载边界是最外层拒绝条件：`STATX_MNT_ID` 不等于 Root 的挂载 ID、或内核不提供挂载 ID，都 fail-closed；创建文件前先核对父目录，避免写入 bind mount 后的外部样本。
- 目录列举不返回空目录来掩盖故障：无权限、非目录、句柄失效都上报原始错误（design §11.4：权限/扫描故障不等于删除）。
- 身份漂移一律映射 `ROOT_UNAVAILABLE` 并暂停 Root，不改写、不推导删除、不自动修复；`compare` 的顺序固定（后端 → FSID → 子卷 → 挂载 → 目录），同一输入结果稳定。
- 重叠判定同时看规范路径分量、目录对象身份与角色，不只比路径或 inode；btrfs 上不同子卷的同号 inode 不算同一目录。

## crates/domain — 身份与有界计数（S015）

状态：S015 在 `crates/domain/src/ids.rs`、`version.rs` 落地不透明身份与有界计数/UTC 时间端口。纯数据与纯规则，无第三方依赖、无 I/O；`crates/domain` 仍是身份的唯一定义处，平台层只承载字节。状态维度与错误分类见下节 S016。

### 模块与契约

| 模块 | 契约 |
| --- | --- |
| `ids` | `ID_LEN = 16`、`ID_HEX_LEN = 32` 字节/字符常量；身份类型 `ServerId`、`UserId`、`RootId`、`BindingId`、`EntryId`、`VersionId`、`DeviceId`、`OpId` 各为 `[u8; 16]` newtype，接口统一为 `from_bytes`（`const fn`）、`as_bytes`、`from_hex`、`to_hex`、`generate(&mut impl IdSource)`，并实现 `Display`（32 个小写十六进制）、`Debug`（`类型名(hex)`）、`FromStr`、`Ord`、`Hash` |
| `ids::IdParseError` | `Empty` / `Length(实际字节数)` / `NonHexDigit(0 基字节位置)`；长度不符时不继续逐字符检查，故 `0x…` 与带连字符 UUID 文本报 `Length` |
| `ids::IdSource` | 随机字节端口 `fn next_id_bytes(&mut self) -> [u8; ID_LEN]`；领域层不取随机数，生产端在应用层接线，测试端可注入计数器 |
| `version` | `DB_INTEGER_MAX = i64::MAX`；计数类型 `Revision`、`RootSeq`、`Generation`（`local_generation`）、`ServerEpoch` 各为 `u64` newtype，取值 `0..=DB_INTEGER_MAX`；接口 `ZERO`、`new`、`from_db_integer`、`to_db_integer`、`get`、`next`、`from_decimal`、`to_decimal`（`Display`/`FromStr` 同形式） |
| `version::BoundError` | `Empty` / `InvalidDigit` / `Negative` / `LeadingZero` / `AboveDbLimit`；`next` 到达 `DB_INTEGER_MAX` 返回 `AboveDbLimit`，不回绕 |
| `version::UtcTimestamp` | Unix 秒的 `i64` newtype，负值（1970 之前）返 `Negative`；`from_unix_seconds`/`from_db_integer`/`as_unix_seconds`/`to_db_integer`/`Display` |
| `version::Clock` | `fn now_utc(&self) -> Result<UtcTimestamp, BoundError>`；领域层不读时钟，测试注入固定序列 |

### 编码与边界规则

- **身份文本**：规范形式是 32 个小写十六进制字符；解析同时接受大写，因此同一值是唯一的字节表示。不做 base64/UUID 变体，wire 与日志都用 `to_hex()`；16 字节长度与 Windows CFAPI `FileIdentity`（48 B blob 中 RootId、EntryId 各 16 字节）一致，服务端 ID 可直接写入平台身份。
- **计数文本**：wire/JSON **只传十进制字符串**（design §7.1、§15.1），不传 JSON number；规范文本无符号、无前导零，`0` 写作 `"0"`，非法写法和超过 `i64::MAX` 的值一律拒绝，避免 JavaScript 在 2^53 以上静默丢精度。数据库读写用 `to_db_integer`/`from_db_integer`，负列值报 `Negative`。
- 各身份/计数类型互不兼容（不同类型不能互相赋值），避免把 `RootSeq` 当 `Revision`、把 `RootId` 当 `EntryId`。

### 不变量

- 上限在构造、解析与递增三处都检查，不依赖调用方自觉；类型内部因此不可能持有超出 SQLite `INTEGER` 的值。
- 生成身份必须经 `IdSource` 注入，领域层不含随机源或时钟，同一注入序列得到同一结果。
- 拒绝即报错：空文本、正负号、前导零、非十六进制字符、超界数值都返回明确变体，不静默取默认值或截断。

`UserId` 由 S027 追加：服务端 `users.id` 与 `roots.owner_id` 的账号身份（design §7.2、§17.1），与其它身份同为 16 字节 newtype，不与 `ServerId`/`RootId` 互换。

验证命令（devShell 内）：`cargo test --locked -p vaultsyncd-domain`、`cargo clippy --locked -p vaultsyncd-domain --all-targets -- -D warnings`、`cargo fmt --check`。

## crates/domain — 状态维度与错误分类（S016）

状态：S016 在 `crates/domain/src/state.rs`、`error.rs` 落地六个正交状态维度、界面主状态投影、“已同步”判断与 design §15.4 错误码分类。纯数据与纯函数：不读写文件、不查数据库、不读时钟、不做重试调度，也不生成状态（执行器负责更新维度值）。

### 模块与契约

| 模块 | 契约 |
| --- | --- |
| `state::Residency` | `MetadataOnly`（默认）/ `Partial` / `Materialized`；`Partial` 是下载中间态，不代表完整离线可用 |
| `state::Dirty` | `Clean`（默认）/ `Dirty` / `Snapshotting` / `AwaitingCommit`；`Clean` 只是上次观察，不是“文件未变”的证明 |
| `state::RemoteApply` | `Current`（默认）/ `Pending` / `BlockedByLocal` / `Conflicted` |
| `state::Transfer` | `Idle`（默认）/ `Queued` / `Running` / `RetryWait(ErrorCode)` / `Failed(ErrorCode)`；`error_code()` 给出重试/失败原因，其余状态返回 `None` |
| `state::Usage` | `Idle`（默认）/ `ReadHandles` / `WriteHandles` / `Unknown`；只供破坏性操作（释放、替换）准入使用，不改变主状态 |
| `state::Availability` | `Online`（默认）/ `Offline` / `MountFailed` / `RootUnavailable` |
| `state::EntryState` | 六个公开维度字段 + `Default`（仅云端、clean、current、idle、idle、online）；方法与无 I/O 投影：`project() -> EntryStatus`、`is_synced() -> bool` |
| `state::EntryStatus` | design §8.2 优先级（声明顺序）：`Unavailable` > `Failed` > `Conflict` > `LocalChanges` > `Syncing` > `Downloaded` > `CloudOnly` |
| `error::ErrorClass` | `Retryable` / `NeedsUserAction` / `Corrupt`；`is_severe()` 仅对后两者为真（design §8.2 严重失败、§18.2 问题页） |
| `error::ErrorCode` | design §15.4 的 23 个错误码（18 服务端 + 4 首发 Windows 本机 + `MOUNT_FAILED`）；`ALL`、`as_str()`（wire 规范文本）、`from_code()`、`class()`、`Display` |

### 投影与分类规则

- **主状态**：挂载失败/Root 不可用 → `Unavailable`；已停止自动推进的任务或重试队列里出现严重类别 → `Failed`；`Conflicted` → `Conflict`；任一非 `Clean` 的本地变更 → `LocalChanges`；其余 `remote_apply != Current` 或任务非 `Idle` → `Syncing`；最后按 `Residency` 给 `Downloaded`/`CloudOnly`。详情可同时显示多个维度，主状态只有一个。
- **已同步**：`is_synced()` 的成立条件是 `dirty = Clean`、`remote_apply = Current`，且任务不是 `RetryWait`/`Failed`。`Pending`/`BlockedByLocal`/`Conflicted` 与任一待上传修改都为假；`Queued`/`Running` 的下载或哈希任务不改变“基线一致”。`Residency`、`Availability` 不参与判断，界面另读维度显示“仅云端/当前离线”。
- **错误类别只由错误码决定**：`Retryable` 允许自动重新规划或退避重试；`NeedsUserAction` 停止相关操作、保留本地内容并进入问题页；`Corrupt` 不发布未通过校验的内容。一次性的前置动作由调用方在分类前完成（例如 `AUTH_EXPIRED` 先受控刷新一次）。`from_code` 对未知代码返回 `None`，不默认归入可重试。
- **wire `retryable` 与本分类不冲突**：`ROOT_UNAVAILABLE` 在 wire 上可为 `retryable: true`（允许低频复查），分类仍是 `NeedsUserAction`（只能由用户/管理员恢复挂载）。

### 不变量

- 维度可任意组合：`dirty + offline + materialized` 必须能表达，不被压成单一“冲突”标签。
- 投影与 `is_synced()` 是纯函数，对不一致组合保守处理：不在 `Current` 的远端事实、失败/重试任务与非 `Clean` 状态一律不判为已同步；`RetryWait` 携带严重类别时按失败显示。
- `Transfer::Failed` 表示已停止自动推进的失败，界面按 `Failed` 显示；可重试错误在退避期间保持 `RetryWait`，不进入问题页。
- 领域层不生成状态值、不做 I/O 或调度：重试次数、退避与 `Retry-After` 属调度器（design §13.3）。

验证命令（devShell 内）：`cargo test --locked -p vaultsyncd-domain`、`cargo clippy --locked -p vaultsyncd-domain --all-targets -- -D warnings`、`cargo fmt --check`。

## crates/fs-common — 相对路径与跨平台名称规则（S017）

状态：S017 在 `crates/fs-common/src/path.rs`、`name.rs` 落地纯规则；无第三方依赖、无 I/O、无平台 API，已加入根 workspace 的 `members` 与 `default-members`（可移植核心，`cargo test/clippy --locked` 直接覆盖）。排除规则（`ignore.rs`）、文件事实类型（`fingerprint.rs`）留给后续卡，本卡不预置占位 API。

### 模块与契约

| 模块 | 契约 |
| --- | --- |
| `name` | `MAX_COMPONENT_UTF8_BYTES = 255`（Linux `NAME_MAX`）、`MAX_COMPONENT_UTF16_UNITS = 255`（NTFS 组件上限）；`validate_name(&str) -> Result<(), NameIssue>` 与 `validate_name_bytes(&[u8]) -> Result<(), NameIssue>`（非 UTF-8 归类为 `NonUtf8`，不做有损解码） |
| `name::NameIssue` | `Empty` / `NonUtf8` / `Nul` / `Separator`（`/`、`\`，含 `\\?\`、`\\.\` 前缀）/ `CurrentDirectory` / `ParentDirectory` / `Control(char)` / `Illegal(char)`（`< > : " | ? *`）/ `TrailingDot` / `TrailingSpace` / `Reserved(ReservedName)` / `TooLong { utf8_bytes, utf16_units }`；实现 `Display` 与 `std::error::Error` |
| `name::ReservedName` | Windows 保留设备名 `CON`/`PRN`/`AUX`/`NUL`/`COM1`—`COM9`/`LPT1`—`LPT9`（含上标 `COM¹`—`COM³`、`LPT¹`—`LPT³`），`as_str()` 给出规范写法 |
| `name::NameEquivalence` | 原生名称等价端口 `fn equivalent(&self, left: &str, right: &str) -> bool`；平台层用真实比较表（Windows `CompareStringEx`、文件系统归一化）实现，端口只回答“是否同一条目”，未提供改名能力 |
| `name::PortableNameEquivalence` | 默认实现：字节相同或 Unicode 小写折叠后相同（仅比较，不改写）。不做 NFC/NFD 归一化，组合形式差异留给原生端口 |
| `name::find_collisions(&[&str], &impl NameEquivalence)` | 按 `(first_index, second_index)` 升序返回 `NameCollision { first_index, second_index, first, second }`；只报告，不选择保留哪一条 |
| `path::RelativePath` | 已校验的相对路径（非空分量序列）。`RelativePath::parse(&str)`、`FromStr`、`components() -> &[String]`、`Display`（以 `/` 连接，往返不改文本）；实现 `Eq`/`Hash`/`Ord` |
| `path::PathIssue` | `Empty` / `Absolute` / `DriveLetter(char)` / `Nul` / `Backslash` / `EmptyComponent { index }` / `Component { index, issue: NameIssue }` / `TooLong { utf8_bytes }`；实现 `Display` 与 `std::error::Error` |
| `path::relation` / `path::is_within` | 按分量判定 `PathRelation::{Same, Ancestor, Descendant, Unrelated}` 与“等于或在之下”；不使用字符串前缀 |
| `path::MAX_RELATIVE_PATH_UTF8_BYTES` | `4095`（Linux `PATH_MAX` 减 NUL）。Windows 绝对路径上限取决于 Root 挂载前缀，由平台层判定 |

### 编码与边界规则

- **固定检查顺序**（同一输入总是同一结论）：名称“空 → NUL → 分隔符 → `.`/`..` → 控制/非法字符 → 长度 → 尾部点/空格 → 保留名”；路径“空 → NUL → 绝对/设备前缀 → 盘符 → 反斜杠 → 整体长度 → 逐分量（空分量 → 名称规则）”。
- **保留名按第一个 `.` 之前的部分判定**且大小写不敏感：`con.txt`、`nul.tar.gz` 都是保留名；`CONS`、`com0` 不是。
- **长度双上限**：一个名称必须同时满足 255 UTF-8 字节与 255 UTF-16 码元，因此 100 个汉字（300 字节 / 100 码元）被报 `TooLong`。
- **不做规范化**：路径与名称解析只判定、不改写；`a//b`、`a/`、`./a`、`a/../b` 一律报错而不是折叠成另一条路径；大小写与 Unicode 组合形式原样保留。
- **重叠判定按分量**：`a` 与 `ab` 是 `Unrelated`，不是父子关系（design §16.1）。
- 文件与目录同名属于上层冲突（design §7.4，planner/store 负责）；`find_collisions` 只按名称等价分组，不区分条目类型。

### 不变量

- 名称与路径的接受/拒绝完全由输入决定，不含哈希表顺序、平台比较表或当前目录状态，可移植核心与平台层得到相同判定。
- 任何无法无损跨端表示的名称都带明确原因返回，绝不静默小写、归一化、替换字符或截断。
- 碰撞只报告不消解：接入原生等价端口只会扩大检测范围，不会产生改名或覆盖行为。

验证命令（devShell 内）：`cargo test --locked -p vaultsyncd-fs-common`、`cargo clippy --locked -p vaultsyncd-fs-common --all-targets -- -D warnings`、`cargo fmt --check`。

## crates/planner — 三方比较与基本计划（S019、S021）

状态：S019 在 `crates/planner/src/reconcile.rs` 落地 design §9.1 的四条基本分支；S020 增加首次合并入口 `first_sync::plan_first_sync`（design §9.2，见下一节）；S021 在 `crates/planner/src/conflict.rs` 落地 design §10.1—10.2 的内容冲突计划（服务器 head 保留主路径 + 迟到分支的稳定身份冲突副本与预算内冲突名）。纯规则 crate：只依赖 `domain` 与 `fs-common`，不访问文件系统、网络、数据库或时钟，也不覆盖/删除任何文件；已加入根 workspace 的 `members` 与 `default-members`。移动/删除矩阵由后续卡补齐，遇到这些输入只输出延期裁决。没有基线的输入转交首次合并入口。

### 模块与契约

| 模块 | 契约 |
| --- | --- |
| `reconcile::ContentFingerprint` | 内容等价判定值：`new(size: u64, sha256: [u8; 32])`、`size()`、`sha256()`；相等要求长度与摘要同时相同。摘要由调用方（传输/平台层）计算后以值传入，planner 不读字节 |
| `reconcile::EntryKind` | `File(ContentFingerprint)` / `Directory`（目录没有文件内容版本，design I12） |
| `reconcile::ServerFact` | 服务器侧事实（B 或 R）：`path`、`kind`、`entry_id`、`revision`、`version: Option<VersionId>`（文件由 `file()` 保证为 `Some`，目录为 `None`）、`observed_mtime`；构造 `file(path, content, entry_id, revision, version)` / `directory(path, entry_id, revision)` / `with_observed_mtime(mtime)` |
| `reconcile::LocalFact` | 本地事实 L：`path`、`kind`、`generation`、`observed_mtime`；构造 `file(path, content, generation)` / `directory(path, generation)` / `with_observed_mtime(mtime)` |
| `reconcile::Plan` | `path`（优先基线位置，其次本地，再其次远端）、`action`、`prerequisites: Vec<Prerequisite>`、`dependencies: Vec<Dependency>`、`local_protection: LocalProtection`；纯值，不承诺执行成功 |
| `reconcile::Action` | `NoOp`、`ApplyRemote { revision, version }`、`UploadLocal { base_revision, generation }`、`MergeSameResult { revision, version }`、`CreateConflictCopy { entry_id, op_id, generation }`（S021）、`Defer(DeferReason)`；首次合并另有 `UploadNewEntry`、`CreateFromRemote`、`BindRemoteEntry`、`PreserveBoth`（S020，见下一节） |
| `reconcile::DeferReason` | `FirstSync`（无基线，转交 design §9.2 的首次合并入口）、`MissingEntry`（某一侧缺失，design §10.3）、`IdentityReplaced`（远端身份已非基线身份，I03）、`LocationOrKindChanged`（移动/类型互换，design §9.5、§10.1）、`ContentConflict`（双方都变且结果不同，design §10）、`IncompleteFacts`（自称内容变化却给不出文件版本） |
| `reconcile::Prerequisite` | `BaseRevisionCurrent { expected }`（服务器修订仍等于计划基线）、`GenerationUnchanged { expected }`（本地代际未变）、`UsagePermitsChange`（无写入句柄）、`TargetPathUsable`（父目录已同步、名称可表示、无同名另一类型条目）、`ServerPathVacant`（服务器该路径仍无条目，首次合并的创建 CAS）、`LocalPathVacant`（本地该路径仍无条目，避免覆盖扫描后新建的本地内容） |
| `reconcile::Dependency` | `DownloadVersion { version }`、`ParentDirectoryReady`、`SnapshotLocalContent { generation }`、`ConflictCopyCommitted { entry_id }`（S021：冲突副本提交后才能替换主路径的本地字节） |
| `reconcile::LocalProtection` | `NotNeeded`（无操作）、`KeepLocalContent`（字节不动，只在基线写入后推进状态）、`SnapshotBeforeReplace { generation }`、`UploadSnapshot { generation }` |
| `reconcile::reconcile(base, local, remote)` | 三项 `Option<&…>`，`None` 表示该侧没有该条目（删除事实）；返回 `Result<Plan, PlanError>`，三项都缺失返回 `PlanError::NoFacts` |
| `reconcile::PlanError` | `NoFacts`（没有可比对的条目）、`PathMismatch`（首次合并两侧事实路径不同，调用方没有按规范路径配对） |

### 判定规则

- **比较对象**：内容（长度 + SHA-256）、位置（名称/父目录）、类型与“该侧是否存在”；`mtime` 只随事实记录，任何分支都不参与判定（I04）。长度与摘要相同、仅 `mtime` 相同或位置相同都不足以归并。
- **“R=B”按 `Revision`**：远端修订与基线相同且形状相同才算无变化；“L=B”只能按本地观察到的位置、类型与内容判定。任一证据显示变化即按“有变化”处理。
- **四条基本分支**：`L=B、R=B` → `NoOp`；`L=B、R≠B` → `ApplyRemote`（前提 `BaseRevisionCurrent`、`GenerationUnchanged`、`UsagePermitsChange`、`TargetPathUsable`；内容变化时有 `DownloadVersion` 依赖并要求替换前快照，仅元数据变化时不需要下载、本地字节不动）；`L≠B、R=B` → `UploadLocal`（携带基线 `Revision`，依赖父目录就绪与本地快照，保护说明为上传快照）；`L≠B、R≠B` 且位置/类型/内容都相同 → `MergeSameResult`（不传输内容、不造副本，只在基线写入后推进状态）。
- **延期裁决**：无基线、某一侧缺失、远端身份被替换、位置或类型变化、双方结果不同、事实不完整都输出 `Action::Defer`；这类计划没有前提与依赖，`local_protection` 固定为 `KeepLocalContent`，不允许执行器删除或覆盖本地内容（I02、I04、I07）。

### 冲突计划（S021）

| 契约 | 形状 |
| --- | --- |
| `conflict::plan_content_conflict(&ContentConflictRequest)` | 返回 `Result<ConflictPlan, ConflictPlanError>`；输入 `base`/`local`/`remote` 事实、`conflict_entry_id`（服务端为该 `OpId` 首次分配的身份）、`committed: Option<&CommittedConflict>`（幂等记录）、`naming` |
| `conflict::ConflictPlan` | `main: Plan`（保留服务器 head 的 `ApplyRemote`，依赖含 `ConflictCopyCommitted`，保护为 `SnapshotBeforeReplace`）、`copy: ConflictCopy` |
| `conflict::ConflictCopy` | `Create(Plan)`（`Action::CreateConflictCopy { entry_id, op_id, generation }`，同一父目录下的冲突名）、`Reuse(CommittedConflict)`（同一 `OpId` 的既有结果，不造第二份） |
| `conflict::CommittedConflict` | 幂等记录：`op_id`、`entry_id`、`path`（首次生成的路径，重试不得改名）、`content: ContentFingerprint` |
| `conflict::conflict_entry_path(&RelativePath, &ConflictNameRequest)` | 按 §10.2 生成同父目录的冲突路径 `Result<RelativePath, ConflictNameError>`；名称形如 `<原名干>（冲突-<设备短标识>-<服务器时间>-<op 短标识>）<扩展名>` |
| `conflict::NameBudget` | 目标文件系统的名称预算 `new(utf8_bytes, utf16_units)`；上限为 `fs-common` 的 255/255（`NameBudget::CROSS_PLATFORM`），更紧时原名干按字符边界截断 |
| `conflict::ShortIdLen` | 名称里短标识的十六进制长度（`DEFAULT` = 8，范围 8—32，`extended()` 撞名时翻倍）；长度不合规返回 `ShortIdLenError` |
| `conflict::ConflictPlanError` | `NotContentConflict`（应走 `reconcile` 的其它分支）、`IncompleteHead`（head 是文件却无内容版本）、`OpIdRequestChanged`（同一 `OpId` 的记录与本次请求不同，I06）、`Name(ConflictNameError)` |
| `conflict::ConflictNameError` | `NameMissing`、`SuffixExceedsBudget { required_utf8_bytes, required_utf16_units }`、`Unrepresentable(NameIssue)`（设备短标识违规等）、`Path(PathIssue)`（组合后相对路径超长） |

- **只接一种输入**：路径/类型/身份未变、两侧都相对基线改了内容、且两侧 `ContentFingerprint` 不同；其它形态一律 `NotContentConflict`，交回 `reconcile` 对应分支，不抢移动/删除矩阵的裁决。
- **胜者只看服务器提交序**：保留哪一份由 `remote.revision` 表达，`mtime` 不进入任何判定；交换设备时钟得到同一计划。
- **同内容归并只看长度与 SHA-256**：`ContentFingerprint` 相同就不创建副本；名称相同、路径相同或 `mtime` 相同都不足以归并。
- **幂等依据是 `OpId`**：冲突身份与路径由服务端为该操作分配一次后持久化；重试传入 `committed` 时返回原 `entry_id` 与原 `path`，即使设备短标识或服务器时间已变也不改名，不会不断新增副本；同 `OpId` 不同内容/身份返回 `OpIdRequestChanged`。
- **名称不承担幂等**：短标识撞名只能加长后换名（`ShortIdLen::extended`），不改变身份。

### 不变量

- 同一三方输入重复计算得到完全相同的 `Plan`（含前提与依赖顺序：按 `Prerequisite`/`Dependency` 声明顺序）；冲突计划同样对同一输入重复得到相同的 `ConflictPlan`（含冲突身份、路径与名称）。
- 只输出计划、不执行：不覆盖文件、不删除文件、不推导删除、不做 I/O。
- 缺少必需事实（如内容变化却无 `VersionId`）不降级成“只推进元数据”，而是延期裁决。
- 冲突名称只由输入（原名、设备短标识、服务器时间、`OpId`、预算）决定；不带哈希表顺序与客户端时钟，同一输入得到同一名称且严格命中名称预算。

验证命令（devShell 内）：`cargo test --locked -p vaultsyncd-planner`、`cargo clippy --locked -p vaultsyncd-planner --all-targets -- -D warnings`、`cargo fmt --check`。

## crates/planner — 首次合并计划（S020）

状态：S020 在 `crates/planner/src/first_sync.rs` 落地 design §9.2 第 4—7 步（C22、I07）。与 `reconcile` 的区分是结构性的：本入口**不接受基线参数**，因此不会把“另一端没有”当成删除；已经同步过、只是游标过期的设备必须带旧基线走 `reconcile`（design §9.2 末段），不得调用本入口。纯规则、无 I/O；`reconcile` 的 `Action`/`Prerequisite`/`PlanError` 随之扩充但没有改变已有分支的行为。

| 模块 | 契约 |
| --- | --- |
| `first_sync::BindingMode` | `Full`（远端独有的文件先下载完整内容再呈现）、`OnDemand`（先建占位，内容按需下载）；客户端 Binding 配置映射到此枚举，不保留第二套拼写 |
| `first_sync::PreserveReason` | `ContentDiffers`（同类型不同内容：服务器条目保留名称，本地另成冲突条目）、`KindDiffers`（文件与目录同名：保留双方类型，本地一方另名并告知用户） |
| `first_sync::plan_first_sync(local, remote, mode)` | 两侧 `Option<&…>` 是**同一规范路径**上的事实；返回 `Result<Plan, PlanError>`。两侧都缺失 → `PlanError::NoFacts`；两侧路径不同 → `PlanError::PathMismatch` |
| `reconcile::Action::UploadNewEntry { generation }` | 仅本地存在：服务器新建条目；前提 `GenerationUnchanged`、`TargetPathUsable`、`ServerPathVacant`；依赖 `ParentDirectoryReady` +（文件）`SnapshotLocalContent`；保护 `UploadSnapshot`（目录为 `KeepLocalContent`） |
| `reconcile::Action::CreateFromRemote { entry_id, revision, version, mode }` | 仅远端存在：本地建立条目；前提 `BaseRevisionCurrent`（快照修订）、`TargetPathUsable`、`LocalPathVacant`；依赖 `ParentDirectoryReady` +（`Full` 模式的文件）`DownloadVersion`；保护 `NotNeeded` |
| `reconcile::Action::BindRemoteEntry { entry_id, revision, version }` | 同路径、同类型、同内容：只绑定远端 `EntryId` 与基线，无依赖，保护 `KeepLocalContent` |
| `reconcile::Action::PreserveBoth(PreserveReason)` | 同路径无法证明同一结果：双方都保留，无依赖（副本身份与名称归冲突规则），保护 `KeepLocalContent` |

判定规则与不变量：

- 内容相同只按长度 + SHA-256（`EntryKind` 相等）判定，不看名称相似度或 `mtime`；目录之间按名称合并（`BindRemoteEntry`，`version: None`）。
- 任何输入组合都不产生删除或覆盖：允许的动作只有 `UploadNewEntry`、`CreateFromRemote`、`BindRemoteEntry`、`PreserveBoth` 与 `Defer`；本地存在时 `local_protection` 永不为 `NotNeeded`（I02）。
- `Full` 模式下远端文件给不出 `VersionId` 时输出 `Defer(IncompleteFacts)`，不降级成“只建占位”；`OnDemand` 模式仍可占位。
- 同一输入多次计算得到完全相同的 `Plan`（含前提与依赖顺序）。
- 未做（后续卡/上层）：服务器一致性元数据快照（`snapshot_id + epoch + high_water_seq`）与本地导入清单归扫描/会话层；快照期间事件的接续与上传前 CAS 的执行归执行器；冲突副本名称、`OpId` 身份与长度预算归 S021。

验证命令（devShell 内）：`cargo test --locked -p vaultsyncd-planner`、`cargo clippy --locked -p vaultsyncd-planner --all-targets -- -D warnings`、`cargo fmt --check`。

## api/ — 远端协议契约源与生成配置（S024）

状态：S024 建立最小契约与生成通路。`api/` 是远端 HTTP 协议的唯一编辑源（design §5.6、§15.1—15.4），生成代码提交入库，`cargo xtask check` 用逐字节比对守住“契约与代码同步”。业务语义（状态机、不变量）仍以 design 为准，这里只记录契约文件的形状与生成器边界。

| 文件 | 契约 |
| --- | --- |
| `api/openapi.yaml` | OpenAPI `3.1.0`。当前端点：`GET /v1/health`（无需认证、`HealthResponse`、503 用错误信封）、`GET /v1/capabilities`（`Capabilities`、503 用错误信封）。`info.description` 会原样进生成文件表头 |
| `api/generation.toml` | 生成配置：`format_version = 1`、`generator = "vaultsyncd-xtask"`、`generator_version`（= 包版本）、`source`、`openapi_version`、`[outputs]` 三个路径、`[types.rust]`/`[types.typescript]` 映射表 |
| `api/examples/*.response.json` | 合法响应样本（capabilities/health/error），被 `crates/protocol/tests/contract.rs` 与 `packages/api-client/tests/schema-decoders.mjs` 直接读取；改契约时同步更新 |

### schema 子集与生成规则

| 契约写法 | Rust | TypeScript |
| --- | --- | --- |
| 具名 `type: object` + `properties` + `required` | `pub struct`（`Debug`/`Clone`/`PartialEq`/`Eq`/`serde`），字段按名字排序，非必填为 `Option<T>` | `interface` + `decode<Name>()`，缺必填字段抛错 |
| 具名 `type: string` + `enum` | `pub enum` + `#[serde(rename = "...")]`，无 catch-all | 字符串字面量联合 + `<NAME>_VALUES` + `decode<Name>()` 抛未知取值 |
| `$ref: "#/components/schemas/X"` | `X` | `X` + `decodeX()` |
| `type: string` / `boolean` / `integer` | `String` / `bool` / `u32`（`minimum`/`maximum` 只进文档） | `string` / `boolean` / `number`（整数校验） |
| `type: string, format: decimal_u64` | `String` + `#[serde(with = "crate::decimal")]`：只接受规范十进制文本 | `string` + `expect_decimal()`：正则 `^(0|[1-9][0-9]*)$` |
| `type: array` + `items` | `Vec<T>` | `Array<T>` + 逐元素解码 |
| `type: object` + `additionalProperties: {type: string}` | `BTreeMap<String, String>` | `Record<string, string>` |

### 生成器边界（失败优于猜测）

- 未知/未实现构造一律报错退出：内联 `enum`、内联对象、`oneOf`/`anyOf`/`allOf`、`default` 状态码、非 `#/components/schemas/…` 的 `$ref`、映射值不是 string、`format` 不是 `decimal_u64`、根文档与 schema 的未知键（`serde(deny_unknown_fields)`）。
- 契约卫生由生成器强制：每个具名 schema 与每个属性都必须有 `description`（生成的公开类型/字段要满足 `missing_docs`，TS 侧对应 JSDoc）；schema 名必须是 PascalCase ASCII 标识符；字段名必须是 snake_case 且不与 Rust 关键字冲突；`required` 不能列出未定义属性；每个端点必须有 `operationId`（lowerCamelCase，也是生成的函数名）、恰好一个 2xx 响应、所有非 2xx 响应引用 `ErrorEnvelope`。
- 运行前核对 `api/generation.toml`：`format_version`、`generator`、`generator_version`（与 `env!("CARGO_PKG_VERSION")` 比）、`openapi_version`（与文档 `openapi` 字段比）任一不符即报错，避免配置与实现漂移。
- 闭合枚举与十进制校验的语义由两侧测试共同锁定：`cargo test --locked -p vaultsyncd-protocol`（13 个契约用例）+ `pnpm --filter @vaultsyncd/api-client test`（9 项运行时断言）。
- 生成文件表头标注“禁止手改”，不含时间戳或随机顺序，因此重新生成逐字节一致。

## crates/protocol — 远端 wire 契约（S024）

状态：S024 实现。包名 `vaultsyncd-protocol`，依赖 `serde`（derive）与 `vaultsyncd-domain`，dev 依赖 `serde_json`；在 `default-members` 内，Windows 与 Linux 共用。

| 模块 | 契约 |
| --- | --- |
| `generated` | 生成代码（`api/openapi.yaml` → `crates/protocol/src/generated/mod.rs`）：`Capabilities`、`ProtocolVersion`、`CapabilityLimits`、`HealthResponse`、`HealthStatus`、`ErrorEnvelope`、`ErrorBody`、`ErrorCode`。闭合枚举无 catch-all，未知取值反序列化失败；未知字段被忽略（前向兼容） |
| `decimal` | `DB_INTEGER_MAX = i64::MAX`、`DecimalTextError`（`Empty`/`Negative`/`LeadingZero`/`InvalidDigit`/`AboveDbLimit`）、`validate(&str) -> Result<u64, DecimalTextError>`，以及给 `#[serde(with = "crate::decimal")]` 用的 `deserialize`/`serialize`（JSON number、`"01"`、`"-1"`、超界文本都拒绝） |
| `domain_map` | `server_epoch_to_wire`/`server_epoch_from_wire`（`ServerEpoch` ↔ 十进制文本）、`server_id_to_wire`/`server_id_from_wire`（`ServerId` ↔ 32 位十六进制）；只做显式转换，不实现 `From`/`Into` |

不变量：wire DTO 不依赖 domain 类型（转换集中在 `domain_map`）；本 crate 不做 I/O、不读时钟、不取随机数；`decimal` 的规则与 `vaultsyncd_domain::version` 的规范十进制一致，`tests/contract.rs` 用同一份取值矩阵比对两处结论。

验证命令（devShell 内）：`cargo test --locked -p vaultsyncd-protocol`、`cargo clippy --locked -p vaultsyncd-protocol --all-targets -- -D warnings`、`cargo fmt --check`、`RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps -p vaultsyncd-protocol`。

## packages/api-client — 远端 HTTP 客户端（S024）

状态：S024 实现。包名 `@vaultsyncd/api-client`，`private: true`，无运行时依赖，`exports` 指向 `./src/index.ts`；已加入 `pnpm-workspace.yaml` 与根 `typecheck`。

| 文件 | 内容 |
| --- | --- |
| `src/generated/types.ts` | 生成：类型 + 解码器 + 按需生成的 `expect_*` 辅助函数；未知枚举、缺字段、非法十进制文本都抛错 |
| `src/generated/client.ts` | 生成：`FetchLike`、`ClientOptions`（`base_url`/`fetch`/`request_id`）、`ApiError`（`status` + 已解码 `body`）、`ProtocolError`（畸形响应/未知错误码）与每个端点的 `getCapabilities`/`getHealth`；非 2xx 按 `ErrorEnvelope` 解码成 `ApiError` |
| `src/index.ts` | 手写的稳定导出面（design §5.6）：只从这里导出类型、解码器、错误类与调用函数 |
| `tests/schema-decoders.mjs` | 运行时断言（9 项），直接以 `.mjs` 导入 `../src/generated/types.ts`（Node 24 原生剥离类型，无需构建步骤），并读取 `api/examples/` |

脚本：`typecheck` = `tsc --noEmit`（根 `pnpm typecheck` 覆盖）、`test` = `node tests/schema-decoders.mjs`。客户端不读全局 `fetch`/时钟：传输与 `x-request-id` 生成都由调用方注入，因此可在无浏览器的测试里替换。

## crates/server-store — 服务端数据库迁移框架（S026）

状态：S026 实现。包名 `vaultsyncd-server-store`，依赖 `sqlx 0.8`（`runtime-tokio` + `sqlite`（bundled）+ `migrate` + `macros`），dev 依赖 `tokio`（`macros`/`rt-multi-thread`）；在 `members` 与 `default-members` 内（无平台 API，便携检查覆盖）。只做服务端 SQLite 的打开、门禁、迁移与事务，不含 HTTP、工作目录与对象 I/O。

### 模块与契约

| 项 | 契约 |
| --- | --- |
| `db::SERVER_APPLICATION_ID` | `0x56534E31`（ASCII `VSN1`），写入 `PRAGMA application_id`，是“这个文件是服务端库”的唯一标识；客户端库用另一个标识（由 `crates/client-store` 自己的卡定义） |
| `db::MIGRATOR` | `sqlx::migrate!("./migrations")`，编译期内嵌 `crates/server-store/migrations/*.sql`；文件名 `<版本>_<描述>.sql`，版本单调递增，已发布文件不得改写（校验和不符即拒绝启动） |
| `db::target_schema_version()` | 内嵌迁移的最大版本号（当前 `2`：0001 迁移框架基线 + 0002 命名空间表），即 `PRAGMA user_version` 的目标值；全部已知迁移成功后才写入 |
| `db::migration_backup_path(db, from, to)` | `<库路径>.pre-migration-v<from>-to-v<to>.bak`：迁移前一致性备份的位置，留给管理员整套恢复 |
| `db::Database::open(path)` | 固定顺序：只读探测种类/版本 → 拒绝非服务端库 → 确认 WAL → 迁移前备份 → 执行迁移 → 写回标记 → 建池；任一步失败返回 `DbError` 且不给出可写句柄。父目录须已存在 |
| `db::Database::{path, schema_version, pool, close}` | 路径、打开时确认的版本、只读查询用的连接池、关闭池 |
| `db::Database::begin_write()` | `BEGIN IMMEDIATE` 的短写事务（`sqlx::Transaction<'static, Sqlite>`），等锁上限 10 s；只允许数据库语句 |
| `db::DbError` | `Io` / `NonUtf8Path` / `Sqlite` / `NotADatabase` / `ForeignDatabase{application_id}` / `SchemaTooNew{found,supported}` / `Backup{path,source}` / `BackupUnverified{path,detail}` / `WalUnavailable{journal_mode}` / `Migration{backup,source}` |

### 门禁与不变量

- **种类**：`application_id` 等于服务端标识放行；`0` 且库里没有任何对象（新文件/空库）允许被初始化为服务端库；`0` 但非空、或其它任何标识一律 `ForeignDatabase`。拒绝路径用独立只读探测连接（不设 journal_mode/外键、不创建文件），因此不会改动对方文件。
- **版本**：取 `user_version` 与 `_sqlx_migrations` 最大版本的较大者，超过 `target_schema_version()` 即 `SchemaTooNew`。两个来源都看，是因为“迁移已提交、标记未写”之间中断的库只有迁移记账记得新版本；这类半完成库不会被旧二进制当成可写库。
- **迁移**：每条迁移与 SQLx 记账在同一事务内（单条失败整体回滚）；多条迁移中途失败时库停在最后一个成功版本，错误携带迁移前备份路径，恢复走整套备份，不做自动 down。
- **备份**：仅在“确实有待执行迁移且库非空”时做，用 `VACUUM INTO`（读事务快照，含 `-wal` 里已提交的数据）+ `PRAGMA quick_check` 自检；同名备份已存在则保留不覆盖（更接近迁移前原始状态）。
- **连接默认项**：`journal_mode=WAL`（打开后读回实际值校验，切换不到 WAL 就拒绝启动，覆盖 SMB/NFS/只读存储）、`foreign_keys=ON`、`busy_timeout=10 s`、池上限 4（只服务并发读；写由 `BEGIN IMMEDIATE` 串行化）。
- **事务只包数据库**：文件系统 I/O、网络与等待不得放进写事务（design §6）。

验证命令（devShell 内）：`cargo test --locked -p vaultsyncd-server-store`、`cargo clippy --locked -p vaultsyncd-server-store --all-targets -- -D warnings`、`cargo fmt --check`、`RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps -p vaultsyncd-server-store`。

## crates/server-store — 服务端命名空间表（S027）

状态：S027 实现。表定义在 `crates/server-store/migrations/0002_namespace.sql`（`users`/`roots`/`entries`），读写接口在 `src/roots.rs`、`src/entries.rs`。不写 HTTP、不做挂载探测或路径 I/O；路径重叠与身份比较留给平台层（S032）与注册流程（S044）。

### 模块与契约

| 项 | 契约 |
| --- | --- |
| `roots::NewRoot` / `RootRecord` | 注册输入（`id`、`entry`、`owner`、`absolute_path`、`volume_identity`、`root_file_identity`、`epoch`）与读回记录（含 `health`、`seq`、`epoch`） |
| `roots::RootHealth` | `Healthy` / `Unavailable`（数据库文本 `healthy` / `unavailable`）；挂载消失、身份不匹配、权限失败都是 `unavailable`，不推导删除 |
| `roots::insert_root(tx, &NewRoot)` | 一个事务内写 `roots` 一行 + 该 Root 的合成根条目一行；返回读回的 `RootRecord` |
| `roots::root_in_scope(conn, owner, id)` | 按 owner scope 读 Root；不属于该 owner 或不存在都返回 `None` |
| `roots::roots_of_owner(conn, owner)` | 列出账号名下的 Root，按 `absolute_path` 排序 |
| `roots::set_health(tx, owner, id, health)` | 只更新自己名下的 Root；0 行受影响返回 `OutOfScope` |
| `roots::RootError` | `Db` / `InvalidPath{path,reason}` / `EmptyIdentity{field}` / `UnknownOwner{id}` / `DuplicatePath{path}` / `DuplicateId{id}` / `OutOfScope` / `Entry(EntryError)` / `MalformedRow{column}` |
| `entries::NewEntry` / `EntryRecord` | 新建输入（`id`、`root`、`parent`、`name`、`name_key`、`kind`、`revision`、`content_version`）与读回记录（含 `deleted_at`） |
| `entries::EntryKind` | `File` / `Directory`（数据库文本 `file` / `directory`） |
| `entries::insert_entry(tx, owner, &NewEntry)` | 检查 owner scope、父目录、名称与同父唯一性后写一行；返回读回记录 |
| `entries::move_entry(tx, owner, id, target_parent, name, name_key, revision)` | 同 Root 内移动/改名；检查目标父目录与目录环 |
| `entries::soft_delete_entry(tx, owner, id, at)` | 写 `deleted_at`；目录仍有活跃子项时 `DirectoryNotEmpty` |
| `entries::entry_in_scope` / `children` / `root_entry` | 只读查询（接收 `&mut SqliteConnection`，可直接用连接池）：按 owner scope 取单条（含已删）、取活跃子项（按 `name_key`、`name`、`id` 排序）、取合成根条目 |
| `entries::EntryError` | `Db` / `OutOfScope` / `EntryNotFound{id}` / `ParentNotFound{id}` / `ParentNotDirectory{id}` / `CrossRootParent{entry_root,parent_root}` / `DuplicateName{parent,name}` / `DirectoryCycle{entry,parent}` / `DirectoryNotEmpty{id}` / `InvalidName{name,issue}` / `EmptyNameKey` / `DirectoryHasContentVersion` / `DuplicateId{id}` / `MalformedRow{column}` |

### 表与不变量

- **owner scope**：`roots.owner_id → users(id)` 外键；每个读写接口都带 `UserId`，不属于该 owner 的 Root/条目返回 `OutOfScope` 或 `None`，不区分“不存在”与“别人的”。
- **父子同 Root**：`entries(root_id, parent_id) → entries(root_id, id)` 复合外键（引用目标由 `UNIQUE (root_id, id)` 提供），跨 Root 父项在 SQL 层即失败；写接口先给出 `CrossRootParent`。
- **活跃同父名称唯一**：部分唯一索引 `(root_id, parent_id, name_key) WHERE deleted_at IS NULL` 与 `(root_id, parent_id, name) WHERE deleted_at IS NULL`；`name_key` 由调用方按平台等价规则派生（design §7.2，本层不小写、不归一化），名称索引兜住“名字完全一样却给了不同键”。软删除后名称可复用。
- **一个 Root 一个合成根条目**：`(root_id) WHERE parent_id IS NULL AND deleted_at IS NULL` 部分唯一索引；根条目名称与比较键为空字符串、类型固定目录、`revision` 从 0 起。
- **目录环**：移动时从目标父项沿 `parent_id` 向上走递归 CTE（`UNION` 去重，坏环也终止），命中被移动条目即 `DirectoryCycle`；自己当父项同样拒绝。
- **路径与身份**：`roots.absolute_path` 全库唯一，形式检查为“非空、无 NUL、以 `/` 起始、不以 `/` 结尾”（根 `/` 例外），不用宿主 `Path::is_absolute`（本 crate 在 Windows 便携检查里也跑测试）；重叠/别名属 S032/S044。`volume_identity`/`root_file_identity` 是不透明非空字节。
- **删除不级联**：本卡只软删单条并拒绝删除非空目录；子树删除与回收站属 S039。`entries.content_version_id` 暂只有长度与“目录无内容版本”约束，指向 `versions` 的外键要到 S028 建表后才能加。
- **写事务**：写接口要求调用方已开 `BEGIN IMMEDIATE` 事务，检查与写入在同一次持锁事务内；只读接口不抢写锁。

验证命令（devShell 内）：`cargo test --locked -p vaultsyncd-server-store`、`cargo clippy --locked -p vaultsyncd-server-store --all-targets -- -D warnings`、`cargo fmt --check`、`RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps -p vaultsyncd-server-store`。
