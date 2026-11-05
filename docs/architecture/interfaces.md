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

## Cargo workspace 与 Rust 工具链

状态：S004 实现；S009 增加 `tests/windows-integration`，S010 增加 `crates/platform-windows`。成员按 design §5.3/§5.4/§5.8 逐个增补，不预建空 crate。默认 Cargo 命令在 `nix develop -c` 内执行，Windows 交叉构建在 `nix develop .#windows -c` 内执行。

| 项目 | 契约 |
| --- | --- |
| 根清单 | `Cargo.toml` 为虚拟 workspace（无 `[package]`）；`resolver = "3"`；`members`/`default-members` 只列真实存在的包。当前 `members = ["crates/domain", "crates/platform-windows", "tests/windows-integration", "tools/xtask"]`，`default-members = ["crates/domain"]`：Windows 平台适配层、只能在 Windows 上运行的测试包与仅用于编排的工具都不进默认集合 |
| 包命名 | `crates/<目录>/` 的包名为 `vaultsyncd-<目录>`（如 `vaultsyncd-domain`）；`apps/` 入口按 design §5.7 的 `vaultsyncd-server`/`-admin`/`-agent`/`-cli` |
| 共享包字段 | `[workspace.package]`：`edition = "2024"`、`rust-version = "1.85.0"`、`version = "0.1.0"`、`publish = false`；成员用 `<字段>.workspace = true` 继承，不重复字面量 |
| lint 接线 | 成员默认写 `[lints] workspace = true`。`[workspace.lints.rust]`：`unsafe_code`、`missing_docs`、`unused` = deny；`[workspace.lints.clippy]`：`all = { level = "deny", priority = -1 }`。crate 内的 `#![forbid(unsafe_code)]` 是更硬的内层约束。**包级放行 `unsafe_code` 的唯一可行写法**（S010 实测）：省略 `[lints] workspace` 键（写 `workspace = false` 会报错“`workspace` cannot be false”，与 `workspace = true` 并存会被拒“cannot override `workspace.lints` in `lints`”），然后在包内手抄 workspace 的其余 lint（`missing_docs`/`unused` deny、`clippy::all` deny）并在 `Cargo.toml` 注释说明理由；实测 `crates/platform-windows` 的 `missing_docs` 与 `unsafe_code` 行为都符合预期。也可以不改 lint 表，改用各 target 源码里的 crate 级 `#![allow(unsafe_code)]`（`tests/windows-integration` 的做法） |
| 工具链固定 | `rust-toolchain.toml`：`channel = "1.99.0"`、`targets = ["x86_64-pc-windows-msvc"]`（S009 起）。Linux 日常构建的 rustc/cargo/rustfmt/clippy 来自锁定 nixpkgs，不读该文件；`nix develop .#windows` 内的 rustup 按本文件安装同一 channel 与 Windows 标准库，两端版本以 `docs/environment.md` §13 为唯一基准 |
| 格式 | `rustfmt.toml` 只固定 `style_edition = "2024"`，其余用默认值；`cargo fmt --check` 为格式门禁 |
| 锁文件 | 仓库根唯一 `Cargo.lock`（生成产物，提交入库），不得忽略；新增依赖后先 `cargo generate-lockfile`，验证用 `--locked` |
| 验收命令 | `cargo metadata --locked --no-deps`、`cargo check --locked`、`cargo fmt --check`、`cargo clippy --locked --all-targets -- -D warnings`、`cargo test --locked` |
| MSRV 状态 | `rust-version = "1.85.0"` 依据 edition 2024 稳定版本声明；实测只在 1.99.0 上编译（Linux 与 Windows 交叉目标），devShell 内无 1.85.0 工具链，1.85.0 真编译复核仍未执行 |

## 前端 pnpm workspace 与 UI 包

状态：S005 实现。当前唯一成员 `packages/ui`；`apps/web`、`apps/desktop`、`packages/api-client`、`packages/local-ipc-types` 按各自卡创建后再加入成员列表。所有 node/pnpm 命令在 `nix develop -c` 内执行（版本见 `docs/environment.md` §10）。

| 项目 | 契约 |
| --- | --- |
| 根清单 | `package.json`：`private: true`；`packageManager: "pnpm@12.9.0"`；`engines.node = "24.21.0"`、`engines.pnpm = "12.9.0"` 全部写死 devShell 实测值，版本源是 `flake.lock` 固定的 nixpkgs，不追“最新”；换版本必须先改 flake 输入 |
| 根脚本 | `typecheck` = `pnpm --recursive run typecheck`，覆盖全部成员；新成员只需提供同名 script，无需改根脚本 |
| 成员登记 | `pnpm-workspace.yaml` 只列已经真实存在的目录（当前 `packages/ui`），不为未创建的 apps 预留占位 |
| 包命名 | `packages/<目录>/` 的包名为 `@vaultsyncd/<目录>`（`@vaultsyncd/ui`），`private: true`，version 与 Rust workspace 同为 `0.1.0` |
| 共享 TS 配置 | 仓库根 `tsconfig.base.json`：`strict`、ES2022 + DOM、`module: ESNext`、`moduleResolution: bundler`、`jsx: react-jsx`、`esModuleInterop`、`isolatedModules`、`skipLibCheck`；成员 `tsconfig.json` 只写 `extends: "../../tsconfig.base.json"` 与 `include` |
| UI 包边界 | `packages/ui/src/` 只放无平台 I/O 的展示组件与类型（design §5.6、§6）：不 fetch、不读文件、不走 IPC、不持有同步状态；状态语义由 Rust 纯核心定义，应用层映射后传文案 |
| 导出形式 | `exports: { ".": "./src/index.ts" }`，直接导出 TS 源码（当前无构建步骤、不产 `dist/`）；消费方必须能编译 TS，编译链在 S076 的应用里 |
| 依赖策略 | react/react-dom 以 `peerDependencies` 声明并由应用自带运行时，同时在 UI 包 `devDependencies` 装同版本供本地类型检查；`typescript` 只在根 `devDependencies`，pnpm 脚本会自动把根 `node_modules/.bin` 加进 PATH |
| 锁文件 | 仓库根唯一 `pnpm-lock.yaml`，提交入库、不忽略，不产生嵌套锁；pnpm 12 在同一文件的第一个 YAML 文档写 `packageManagerDependencies`（各平台 `@pnpm/exe.*`），是 `packageManager` 字段的锁定产物，不手改 |
| 验收命令 | `pnpm install --frozen-lockfile`、`pnpm --filter @vaultsyncd/ui typecheck`、根 `pnpm typecheck` |

## crates/platform-windows — Windows 平台适配层（CFAPI 原型）

状态：S010 实现 `cfapi/` 的注册、连接、占位创建、查询、注销与 `FileIdentity` 编码；S011 实现固定内容源的读取供给（`FETCH_DATA` 分段落盘）、范围请求、离线/超时明确失败与传输中断语义；S012 实现释放（`CfSetInSyncState` + `CfDehydratePlaceholder`）与释放前的写保护判定（使用保护探测 + 内容代际）；`shell/`、`files/`、`watcher/`、`ipc/` 尚未实现。包名 `vaultsyncd-platform-windows`，无第三方依赖，属于 workspace `members` 但**不在** `default-members`（Windows 目标代码只有交叉构建 + 客体运行才能验证）。宿主单测可覆盖纯逻辑部分（S011 起含确定性内容源、SHA-256 与回填循环）：`cargo test --locked -p vaultsyncd-platform-windows`。操作步骤见 `docs/operations/windows-lab.md` §16（S010）、§17（S011）、§18（S012），实测证据见 `docs/environment.md` §14、§15、§16。

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

状态：S006 实现（`env`、`check` 可用；`generate`/`package` 明确未实现）。包名 `vaultsyncd-xtask`，二进制名 `xtask`，edition 2024，无第三方依赖。它属于 workspace `members` 但**不在 `default-members`**：便携的 `cargo check --locked`/`cargo test --locked`/`cargo clippy` 只构建 `crates/domain` 等核心成员，不构建编排工具。

调用入口是根 `.cargo/config.toml` 的唯一别名 `xtask = "run --package vaultsyncd-xtask --"`；该文件不设置目标、镜像、链接器或网络行为。所有子命令都在 `cargo locate-project --workspace` 找到的根目录执行，因此可从任意子目录调用（构建信息由 cargo 写到 stderr）。

| 子命令 | 行为 | 退出码 |
| --- | --- | --- |
| `env` | 只读诊断：定位 workspace 根；探测 `rustc/cargo/node/pnpm --version` 的实际输出（工具缺失时标「不可用」，不写死版本）；用 `cargo tree --locked --workspace --depth 0 --prefix none` 列出成员；列出 `Cargo.toml`、`Cargo.lock`、`package.json`、`pnpm-lock.yaml`、`rust-toolchain.toml`、`node_modules` 是否存在 | 0；无法定位 workspace 时 1 |
| `check` | 先确认 `<root>/node_modules` 存在：缺失立即报错并提示 `pnpm install --frozen-lockfile`，不跳过前端检查。随后按固定顺序执行下表四步，逐步打印命令与退出码，末尾汇总失败步骤；子进程继承终端，诊断即时可见 | 全部通过 0；任一步失败 1 |
| `generate` | 协议/代码生成：尚未实现，打印「未实现（Unsupported）」且不执行任何动作 | 3 |
| `package` | 产品打包：尚未实现，同上 | 3 |
| `help`、`--help`、`-h` | 打印子命令与退出码说明 | 0 |
| 其他 / 缺少子命令 / 多余参数 | 打印未知名或参数错误并附帮助 | 2 |

`check` 的步骤表（顺序固定）：

| 步骤名 | 命令 |
| --- | --- |
| `rust-fmt` | `cargo fmt --check` |
| `rust-clippy` | `cargo clippy --locked --all-targets -- -D warnings` |
| `rust-test` | `cargo test --locked` |
| `frontend-typecheck` | `pnpm --recursive run typecheck` |

覆盖面：`cargo fmt --check` 覆盖全部成员（含 `tools/xtask`）；clippy 与 test 按 Cargo 语义只作用于 `default-members`（当前 `crates/domain`），xtask 自身需单独 `cargo clippy -p vaultsyncd-xtask --locked --all-targets -- -D warnings` 验证，扩展名成员后是否改为 `--workspace` 由后续卡按 §5.7/§5.8 决定。

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
