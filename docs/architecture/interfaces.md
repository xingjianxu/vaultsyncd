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
| MSRV 状态 | `rust-version = "1.85.0"` 依据 edition 2024 稳定版本声明；实测只在 1.99.0 上编译（Linux 与 Windows 交叉目标），devShell 内无 1.85.0 工具链，1.85.0 真编译复核仍未执行。resolver 3 会按 `rust-version` 做 MSRV 感知解析：S018 的 `ignore = "0.4"` 因此锁定 `ignore 0.4.30`（0.4.31 起要求 Rust 1.88），`globset` 同样停在 0.4.19 而不是 0.4.21 |

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

状态：S006 实现 `env`/`check`，S024 实现 `generate`，S092 实现 `check-boundaries`，S093 实现 `version`/`release`（`package` 仍明确未实现）。包名 `vaultsyncd-xtask`，二进制名 `xtask`，edition 2024。S024 起允许第三方解析器依赖：`serde`（derive）、`serde_yaml_ng` 0.10、`toml` 0.9，全部由根 `Cargo.lock` 锁定；不引入模板引擎，生成逻辑在本包内。它属于 workspace `members` 但**不在 `default-members`**：便携的 `cargo check --locked`/`cargo test --locked`/`cargo clippy` 只构建核心成员，不构建编排工具。

调用入口是根 `.cargo/config.toml` 的唯一别名 `xtask = "run --package vaultsyncd-xtask --"`；该文件不设置目标、镜像、链接器或网络行为。所有子命令都在 `cargo locate-project --workspace` 找到的根目录执行，因此可从任意子目录调用（构建信息由 cargo 写到 stderr）。

| 子命令 | 行为 | 退出码 |
| --- | --- | --- |
| `env` | 只读诊断：定位 workspace 根；探测 `rustc/cargo/node/pnpm --version` 的实际输出（工具缺失时标「不可用」，不写死版本）；用 `cargo tree --locked --workspace --depth 0 --prefix none` 列出成员；列出 `Cargo.toml`、`Cargo.lock`、`package.json`、`pnpm-lock.yaml`、`rust-toolchain.toml`、`node_modules` 是否存在 | 0；无法定位 workspace 时 1 |
| `check` | 先确认 `<root>/node_modules` 存在：缺失立即报错并提示 `pnpm install --frozen-lockfile`，不跳过前端检查。随后按固定顺序执行下表六步，逐步打印命令与退出码，末尾汇总失败步骤；子进程继承终端，诊断即时可见 | 全部通过 0；任一步失败 1 |
| `check-boundaries` | 生成一致、三目标metadata、pnpm依赖/源码门禁及前端消费者测试，最后运行 protocol/local-ipc Rust 契约；详见文末 S092 | 通过0；失败1；多余参数2 |
| `generate` | 从 `api/openapi.yaml` 生成 `crates/protocol/src/generated/mod.rs` 与 `packages/api-client/src/generated/*`；只写有变化的文件并打印路径。`--check` 不写盘，只逐字节比对，不一致时列出文件并退出 1。配置、契约与生成器的校验规则见下文「api/ — 远端协议契约源」 | 无 diff/写入成功 0；配置漂移、未支持构造、`--check` 有 diff 1；多余参数 2 |
| `version` | 从 Cargo workspace.package.version 核对 Rust继承和JSON镜像；--sync同步镜像，不改协议 | 成功0；失败1；参数错误2 |
| `release` | 真实build、同提交assemble与verify，参数和来源规则见文末S093 | 成功0；构建/来源/参数不满足1 |
| `package` | 完整平台安装包：尚未实现，由后续发布卡落实 | 3 |
| `help`、`--help`、`-h` | 打印子命令与退出码说明 | 0 |
| 其他 / 缺少子命令 / 多余参数 | 打印未知名或参数错误并附帮助 | 2 |

`check` 的步骤表（顺序固定）：

| 步骤名 | 命令 |
| --- | --- |
| `boundaries-and-protocol` | 进程内比对两份契约与生成代码、三个目标 metadata 及前端依赖/源码边界（详见文末 S092），不编译跨目标应用 |
| `rust-fmt` | `cargo fmt --check` |
| `rust-clippy` | `cargo clippy --locked --all-targets -- -D warnings` |
| `rust-test` | `cargo test --locked` |
| `frontend-typecheck` | `pnpm --recursive run typecheck` |
| `frontend-contract-tests` | `pnpm --recursive --if-present run test`，现含 API/IPC 解码及 Web 会话/恢复 |

覆盖面：fmt 覆盖全部成员；clippy/test 只作用于 Cargo.toml 的 default-members，不在 Linux 盲跑全 workspace all-features。xtask 本身另以 `cargo test/clippy -p vaultsyncd-xtask` 验证。首步同时包含生成一致和目标依赖边界；前端解码/页面契约已由 S092 纳入统一检查。

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
| `ids` | `ID_LEN = 16`、`ID_HEX_LEN = 32` 字节/字符常量；身份类型 `ServerId`、`UserId`、`RootId`、`BindingId`、`EntryId`、`VersionId`、`DeviceId`、`OpId`、`TxId`、`SessionId`、`UploadId`、`IntentId`、`ObjectLeaseId`、`BackupLeaseId` 各为 `[u8; 16]` newtype，接口统一为 `from_bytes`（`const fn`）、`as_bytes`、`from_hex`、`to_hex`、`generate(&mut impl IdSource)`，并实现 `Display`（32 个小写十六进制）、`Debug`（`类型名(hex)`）、`FromStr`、`Ord`、`Hash` |
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

`TxId` 由 S028 追加：一次服务端提交产生的事件批次所带的身份（design §7.2 `changes.tx_id`、§9.3），只用于把同一提交的批次与日志关联；幂等键是 (`RootId`, `DeviceId`, `OpId`)，不是 `TxId`。

`SessionId`、`UploadId`、`IntentId`、`ObjectLeaseId`、`BackupLeaseId` 由 S029 追加：分别是登录会话（design §7.2 `sessions.id`、`uploads.session_id`）、上传会话（§7.2 `uploads.id`）、工作目录发布意图（§7.2 `fs_intents.intent_id`）、下载/提交对象租约（§19.4）与备份租约（§7.2 `backup_leases.id`）。`sessions` 表由会话卡（S043）建立，本卡只按长度校验 `uploads.session_id`。

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

状态：S017 在 `crates/fs-common/src/path.rs`、`name.rs` 落地纯规则；无 I/O、无平台 API，已加入根 workspace 的 `members` 与 `default-members`（可移植核心，`cargo test/clippy --locked` 直接覆盖）。S018 增加排除规则模块 `ignore.rs` 与唯一第三方依赖 `ignore`（ripgrep 的 gitignore 实现，见下一节）；文件事实类型（`fingerprint.rs`）留给后续卡，本卡不预置占位 API。

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

## crates/fs-common — 排除规则与解释（S018）

状态：S018 在 `crates/fs-common/src/ignore.rs` 落地 `.syncignore` 的解析与解释（design §10.4、C13、T27），样本与用例表在 `fixtures/paths/`。glob 匹配交给 `ignore` crate 的 `gitignore` 实现（design §4.1“经验证的 gitignore 风格匹配库，不自行拼写 glob 正则”），这是本 crate 唯一的第三方依赖；本模块只接收文本、不读文件（`Gitignore::new`、`GitignoreBuilder::add` 不使用），因此仍属可移植纯规则。

### 模块与契约

| 项 | 契约 |
| --- | --- |
| `ignore::DEFAULT_RULES` | 内置默认排除清单文本，**可直接编辑**。只列编辑器交换/锁文件（`*~`、`*.swp`、`.*#`…、`~$*`、`*.tmp`、`*.temp`）、下载中间文件（`*.part`、`*.partial`、`*.crdownload`）与桌面缓存元数据（`.DS_Store`、`Thumbs.db`、`desktop.ini`）。不排除整个 `.git`、不排除任意隐藏文件、不排除 `.syncignore` 自身 |
| `IgnoreRules::parse_with_defaults(&str)` | 默认清单在前、调用方文本在后（后者优先，可用 `!` 覆盖默认排除）；返回 `Result<IgnoreRules, RuleIssue>` |
| `IgnoreRules::parse(&str)` | 只用给定文本，内置默认清单不参与（绑定提供完整清单时用） |
| `IgnoreRules::decide(&RelativePath, is_dir) -> Decision` | 给出裁决与解释；祖先目录按从浅到深检查 |
| `IgnoreRules::is_excluded(&RelativePath, is_dir) -> bool` | 只要布尔结果时的入口，与 `decide` 判定一致 |
| `ignore::Decision` | `Included { reinclude: Option<Hit> }` / `Excluded(Exclusion)`；实现 `Display`（中文解释，供“为什么被排除”查询） |
| `ignore::Exclusion` | `hit: Hit`（命中的规则）+ `blocked_reinclude: Option<Hit>`（路径自身命中的 `!` 规则因祖先目录被排除而不能生效） |
| `ignore::Hit` | `rule`（规则原文，保留 `!` 与目录尾 `/`）、`source: RuleSource`（`Defaults` / `Binding`）、`at: MatchedAt`（`Path` / `Ancestor(RelativePath)`） |
| `ignore::RuleIssue` | `Line { source, line, text, reason }`（行号相对各自来源文本）、`Build { reason }`；实现 `Display` 与 `std::error::Error` |

### 规则与边界

- **语义等同 Git gitignore**：`#` 整行注释、空行、`!` 否定、`*`/`?`/`**`/字符类、前导 `/` 根锚定、尾部 `/` 只匹配目录；首行 UTF-8 BOM 与 CRLF 按 Git 习惯处理。
- **父目录排除后不能重新包含子项**：`secrets/` 排除后 `!secrets/public.txt` 不生效，解释里给出被阻断的规则；`ignore` crate 的 `matched_path_or_any_parents` 是叶子优先（ripgrep 遍历视角），与 Git 的限制相反，因此本模块自己按祖先自浅到深遍历、只借用该库的 glob 匹配。
- **规则顺序决定结果**：同一路径的多条规则取最后的匹配；目录被 `!` 重新包含后子项可正常参与同步（`Included.reinclude` 记录命中的否定规则，可能位于祖先目录）。
- **默认清单可编辑、可替换**：改 `DEFAULT_RULES` 即改所有绑定默认；绑定可用 `parse_with_defaults` 追加或在 `parse` 里给完整清单。默认规则文件本身不能落到“被自己排除而无法更新”的循环（design §10.4）。
- 行号按来源分别计数：叠加默认清单后，绑定文本的解析错误仍报绑定文本自己的行号。

### 不变量

- **排除不是删除**：本模块只有“参与/不参与”两种裁决，不产生也无法产生 Delete 事件；排除的路径保留已下载的本地内容与服务器内容（design §5/§10.4、C05/C13）。
- **相同规则结果确定**：裁决只依赖规则文本、路径分量与 `is_dir`，不含哈希顺序、平台比较表或目录遍历状态；重复解析与重复查询给出同一结论。
- **默认规则不吞掉最终文件**：编辑器先写 `x.docx.tmp` 再 rename-over 成 `x.docx` 时，最终文件参与同步（T27）。

样本与验证：`fixtures/paths/syncignore-binding.syncignore`（绑定级规则样本）+ `fixtures/paths/syncignore-cases.txt`（用例表：路径 | 类型 | 期望 | 规则 | 命中位置 | 被阻断的否定规则），由 `crates/fs-common/tests/ignore_rules.rs` 逐行验证；默认清单的临时文件/最终文件边界在 `src/ignore.rs` 单测里。验证命令（devShell 内）：`cargo test --locked -p vaultsyncd-fs-common`、`cargo clippy --locked -p vaultsyncd-fs-common --all-targets -- -D warnings`、`cargo fmt --check`。

## crates/planner — 三方比较与基本计划（S019、S021）

状态：S019 在 `crates/planner/src/reconcile.rs` 落地 design §9.1 的四条基本分支；S020 增加首次合并入口 `first_sync::plan_first_sync`（design §9.2，见下一节）；S021 在 `crates/planner/src/conflict.rs` 落地 design §10.1—10.2 的内容冲突计划（服务器 head 保留主路径 + 迟到分支的稳定身份冲突副本与预算内冲突名）；S022 增加 `rename`（design §9.5、§10.1 移动行、跨 Root 两段任务）与 `delete`（design §10.1 删除行、§10.3 子树前提），见后两节。纯规则 crate：只依赖 `domain` 与 `fs-common`，不访问文件系统、网络、数据库或时钟，也不覆盖/删除任何文件；已加入根 workspace 的 `members` 与 `default-members`。`reconcile` 的 `Defer(DeferReason::MissingEntry)`/`DeferReason::LocationOrKindChanged` 是转交信号：上层按缺失方向改调首次合并、移动或删除入口。没有基线的输入转交首次合并入口。

### 模块与契约

| 模块 | 契约 |
| --- | --- |
| `reconcile::ContentFingerprint` | 内容等价判定值：`new(size: u64, sha256: [u8; 32])`、`size()`、`sha256()`；相等要求长度与摘要同时相同。摘要由调用方（传输/平台层）计算后以值传入，planner 不读字节 |
| `reconcile::EntryKind` | `File(ContentFingerprint)` / `Directory`（目录没有文件内容版本，design I12） |
| `reconcile::ServerFact` | 服务器侧事实（B 或 R）：`path`、`kind`、`entry_id`、`revision`、`version: Option<VersionId>`（文件由 `file()` 保证为 `Some`，目录为 `None`）、`observed_mtime`；构造 `file(path, content, entry_id, revision, version)` / `directory(path, entry_id, revision)` / `with_observed_mtime(mtime)` |
| `reconcile::LocalFact` | 本地事实 L：`path`、`kind`、`generation`、`observed_mtime`；构造 `file(path, content, generation)` / `directory(path, generation)` / `with_observed_mtime(mtime)` |
| `reconcile::Plan` | `path`（优先基线位置，其次本地，再其次远端）、`action`、`prerequisites: Vec<Prerequisite>`、`dependencies: Vec<Dependency>`、`local_protection: LocalProtection`；纯值，不承诺执行成功 |
| `reconcile::Action` | `NoOp`、`ApplyRemote { revision, version }`、`UploadLocal { base_revision, generation }`、`MergeSameResult { revision, version }`、`CreateConflictCopy { entry_id, op_id, generation }`（S021）、`ApplyRemoteMove { entry_id, revision, version, target }`、`UploadMove { entry_id, base_revision, target }`、`UploadMoveWithContent { entry_id, base_revision, target, generation }`、`ApplyRemoteDelete { entry_id, revision }`、`DeleteEntry { entry_id, base_revision, subtree }`（S022，见后两节）、`Defer(DeferReason)`；首次合并另有 `UploadNewEntry`、`CreateFromRemote`、`BindRemoteEntry`、`PreserveBoth`（S020，见下一节）。移动/删除动作携带路径或子树 token，因此 `Action` 只 `Clone` 不 `Copy` |
| `reconcile::DeferReason` | `FirstSync`（无基线，转交 design §9.2 的首次合并入口）、`MissingEntry`（某一侧缺失，design §10.3）、`IdentityReplaced`（远端身份已非基线身份，I03）、`LocationOrKindChanged`（移动/类型互换，design §9.5、§10.1）、`ContentConflict`（双方都变且结果不同，design §10）、`IncompleteFacts`（自称内容变化却给不出文件版本） |
| `reconcile::Prerequisite` | `BaseRevisionCurrent { expected }`（服务器修订仍等于计划基线）、`GenerationUnchanged { expected }`（本地代际未变）、`UsagePermitsChange`（无写入句柄）、`TargetPathUsable`（父目录已同步、名称可表示、无同名另一类型条目）、`ServerPathVacant`（服务器该路径仍无条目，首次合并的创建 CAS）、`LocalPathVacant`（本地该路径仍无条目，避免覆盖扫描后新建的本地内容） |
| `reconcile::Dependency` | `DownloadVersion { version }`、`ParentDirectoryReady`、`SnapshotLocalContent { generation }`、`ConflictCopyCommitted { entry_id }`（S021：冲突副本提交后才能替换主路径的本地字节）、`CrossRootTargetCommitted { entry_id }`（S022：跨 Root 目标创建确认后才允许删源） |
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

## crates/planner — 移动冲突计划（S022）

状态：S022 在 `crates/planner/src/rename.rs` 落地 design §9.5、§10.1 移动行与 T09/T10：同 Root 移动保持 `EntryId`，只改父目录关系（目录移动不重传子项），迟到分支复用 S021 的冲突身份与命名。纯规则、无 I/O：目标位置是否为空/祖先是否还在由调用方观察后以值传入（planner 不枚举同级条目）。

| 模块 | 契约 |
| --- | --- |
| `rename::plan_move(&MoveRequest)` | 返回 `Result<MoveOutcome, MoveError>`；`MoveRequest { base, local, remote, targets, late }` 的三方事实必须已绑定同一 `EntryId`（本地身份关联由客户端数据库保证） |
| `rename::MoveOutcome` | `Planned(Box<MovePlan>)`（`main: Plan` + `late: Option<ConflictCopy>`）、`Replan(ReplanReason)` |
| `rename::TargetState` / `MoveTargets` | `Vacant` / `Occupied` / `AncestorMissing`；`local` 供“本地落位”的计划判定，`server` 供“上传移动/新建迟到分支”判定 |
| `rename::LateBranch` | 迟到分支的 `entry_id`、`naming: ConflictNameRequest`、`committed: Option<&CommittedConflict>`（同一 `OpId` 重试返回原身份原路径） |
| `rename::ReplanReason` | `DirectoryCycle`（目标落在源目录子树内）、`TargetOccupied`、`TargetAncestorMissing`、`IdentityReplaced`（I03）、`DirectoryConflictRequired`（目录迟到分支需目录级冲突记录） |
| `rename::MoveError` | `NotAMove`（两侧位置都没变或类型互换，应走 `reconcile`/首次合并）、`LateBranchMissing`、`OpIdRequestChanged`、`IncompleteFacts`、`MissingSubtreeToken`/`UnexpectedSubtreeToken`/`SameRoot`（跨 Root 入口）、`Name(ConflictNameError)` |
| `rename::plan_cross_root_move(&CrossRootMoveRequest)` | 返回 `Result<CrossRootOutcome, MoveError>`：`Planned(Box<CrossRootMovePlan { create_target, delete_source }>)` 或 `Replan`。`delete_source` 必带 `Dependency::CrossRootTargetCommitted`，目标创建失败或结果未知时源删除不得计划完成 |

判定规则与不变量：

- 主条目永远是远端已提交的位置与内容（胜者只看服务器提交序，`mtime` 不参与）；只有远端变化时给 `ApplyRemoteMove`（内容未变不带 `DownloadVersion`，T09）或复用 `ApplyRemote`；只有本地变化时给 `UploadMove`（纯移动）或 `UploadMoveWithContent`（移动+修改，带 `SnapshotLocalContent`）。
- 双方都变：同位置同内容 → `MergeSameResult`（幂等归并、不造副本）；否则主条目保留已提交侧，落后一侧进入 `late`（内容与意图位置都保留）。迟到分支路径与主条目相同时才追加 §10.2 冲突后缀，不同时保留用户选定的名称。
- 主条目计划在 `late` 存在时追加 `Dependency::ConflictCopyCommitted`：迟到分支先提交，才允许把本地身份移到已提交位置或替换其内容（I02）。
- 目录环按路径分量判定（`docs` 与 `docs-old` 无关），只看移动目标；类型互换（文件↔目录）不是移动，返回 `NotAMove`。
- 跨 Root 移动不保持 `EntryId`：目标 Root 的身份由服务端另分配，源删除带基线修订与（目录）子树 token；源 Root 与目标 Root 相同是调用方错误（`SameRoot`）。
- 同一输入重复计算得到完全相同的 `MoveOutcome`（含前提与依赖顺序）；交换设备 `mtime` 不改变计划。

验证命令（devShell 内）：`cargo test --locked -p vaultsyncd-planner`、`cargo clippy --locked -p vaultsyncd-planner --all-targets -- -D warnings`、`cargo fmt --check`。

## crates/planner — 删除与恢复冲突（S022）

状态：S022 在 `crates/planner/src/delete.rs` 落地 design §10.1 删除行、§10.3 与 T11/T12：删除依据身份 + 修订，目录删除另带服务端子树 token；远端已提交删除而本地仍有不同内容时，先提交恢复冲突条目再撤旧身份。纯规则、无 I/O（不枚举子树、不读时钟、不分配身份）。

| 模块 | 契约 |
| --- | --- |
| `delete::SubtreeToken` | 服务端生成的不透明 16 字节值：`from_bytes([u8; 16])`、`as_bytes()`；planner 只比较相等，不从路径/时间/内容推导 |
| `delete::plan_delete(&DeleteRequest)` | 返回 `Result<DeleteOutcome, DeleteError>`；`DeleteRequest { base, server, local, subtree, subtree_changed, preserve }`，`server: ServerDeletion::Present(&ServerFact)` 或 `Deleted { at_revision }` |
| `delete::DeleteOutcome` | `Upload(Plan)`（本地删除待提交）、`ApplyLocally(Plan)`（远端已删、本地未变）、`Preserve(PreservePlan)`（先提交恢复条目）、`AlreadyDeleted`（同一修订幂等成功）、`ConflictRequired(ConflictReason)`（旧 Delete 不删新内容）、`Replan(DeleteReplanReason)` |
| `delete::PreservePlan` | `main: Plan`（`ApplyRemoteDelete`，依赖 `ConflictCopyCommitted`）、`preserve: ConflictCopy`（`Create` 或同一 `OpId` 的 `Reuse`）、`reason: PreservedReason`（`Modified`/`Moved`） |
| `delete::DeleteReplanReason` | `SubtreeChanged`（T12 `SUBTREE_CHANGED`）、`BaseRevisionMismatch`、`LocalChanged`、`IdentityReplaced`、`DirectoryPreserveRequired` |
| `delete::DeleteError` | `MissingSubtreeToken`、`UnexpectedSubtreeToken`、`PreserveSettingsMissing`、`OpIdRequestChanged`、`InconsistentFacts`（已提交删除的修订早于基线）、`Name(ConflictNameError)` |

判定规则与不变量：

- 远端仍是基线修订时才会生成 `Upload(Plan)`：动作 `DeleteEntry { entry_id, base_revision, subtree }`，前提 `BaseRevisionCurrent` +（有本地事实时）`GenerationUnchanged`；目录缺 token、文件带 token 都是明确错误，`subtree_changed` 一律 `Replan(SubtreeChanged)`，绝不把并发新增的子项一起删掉。
- 远端修订已前进：内容变了 → `ConflictRequired(ContentModified)`（修改已提交时旧 Delete 不删新内容）；只移动了 → `ConflictRequired(Moved)`；形状不变 → `Replan(BaseRevisionMismatch)`。
- 远端已提交删除：本地已无该条目 → `AlreadyDeleted`；本地未变 → `ApplyLocally`（`ApplyRemoteDelete`）；本地已修改或已移走 → `Preserve`，恢复条目（文件）先提交，主计划才撤旧身份（T11）。本地已移走时恢复条目保留用户选的位置，否则用 §10.2 冲突名；目录子树保留需要目录级计划，给 `Replan(DirectoryPreserveRequired)`。
- 恢复条目的幂等依据是 `OpId`：`preserve.committed` 记录存在时返回原 `entry_id`/原 `path`，请求不一致返回 `OpIdRequestChanged`。
- 同一输入重复计算得到完全相同的 `DeleteOutcome`。

验证命令（devShell 内）：`cargo test --locked -p vaultsyncd-planner`、`cargo clippy --locked -p vaultsyncd-planner --all-targets -- -D warnings`、`cargo fmt --check`。

## 认证与账号（S042）

状态：S042 实现核心用例、账号表 typed API、状态目录互斥锁与离线 `init` 命令。登录 HTTP 端点与会话/刷新凭据属 S043（`POST /v1/auth/login` 的响应是会话，本卡没有会话表）。

### `crates/server-store`：`users` 与 `state_lock`

| 项 | 契约 |
| --- | --- |
| `users::UserStatus` / `STATUS_ACTIVE` | 闭集，当前只有 `active`；读到未知状态文本报 `UserError::MalformedRow{column:status}`，不猜默认值 |
| `users::{insert_user,find_by_login,find_by_id,count_users,login_exists,user_exists}` | 事务内/连接上的读写。登录名唯一且**原样比较**（不做大小写折叠或 Unicode 规范化）；空登录名与空散列在写库前被拒；`NewUser{id,login,password_hash}` → `UserRecord{id,login,password_hash,status}` |
| `UserError` | 稳定码 `empty_login`/`empty_password_hash`/`duplicate_login`/`duplicate_user_id`/`malformed_user_row`/`user_storage_failed` |
| `state_lock::{StateLock,LOCK_FILE_NAME}` | `StateLock::try_acquire_exclusive(state_dir)` 独占 `<state_dir>/server.lock`（`fs2` 的 flock/LockFileEx，非阻塞）。`StateLockError::{Io,Busy}`（码 `state_lock_io`/`state_locked`）；锁随句柄关闭或进程退出释放，不用 pid 文件 |

### `crates/server-core`：`auth`

| 项 | 契约 |
| --- | --- |
| `auth::password::PasswordHasher` | `production()` 用 Argon2id v19、`m=19456 KiB, t=2, p=1`（常量 `M_COST_KIB`/`T_COST`/`P_COST`，RFC 9106 方向的工程基线，实测与 ADR 未做）；`hash(&str) -> PHC 文本`（每次独立随机盐）；`verify(stored,&str) -> PasswordVerdict{Matched,Mismatched,Malformed}`，只接受 `argon2id`（白名单），校验按散列自带参数；`dummy_verify` 用于不存在账号的等代价计算 |
| `auth::account::OwnerAccounts` | 端口：`count` / `find_by_login` / `insert`（均 `&mut self`，适配器可持一次写事务）；`StoredAccount{id,login,password_hash,active}`、`NewAccount`、`AccountError{Storage,DuplicateLogin,Malformed}` |
| `auth::init::prepare_owner(accounts, hasher, ids, NewOwnerRequest{login,password})` | 顺序固定：账号数为 0（否则 `AlreadyInitialized{existing}`）→ 登录名/密码形态（`MIN_PASSWORD_CHARS=12`、`MAX_PASSWORD_BYTES=1024`、登录名 ≤64 字符且无控制字符）→ Argon2id 散列 → 返回 `PreparedOwner{id,login,password_hash}`。**不写库**：写入由调用方在与 `count` 同一事务里完成 |
| `auth::login::authenticate(accounts, hasher, throttle, LoginAttempt{login,password,now})` | 先限流、再查账号、再比密码；账号不存在/密码错/账号未启用一律 `InvalidCredentials`（不存在账号也做等值代价计算）；`RateLimited{retry_after_seconds}`；`AccountBroken` 只用于已存散列损坏。成功返回 `AuthenticatedAccount{id,login}`，不发放凭据 |
| `auth::throttle::{LoginThrottle,ThrottlePolicy}` | 进程内、按登录名、按服务端时间：默认 5 次失败/300 秒窗口/锁定 300 秒；不存在的登录名同样计数；`MAX_TRACKED_LOGINS=1024` 上限（超出先清理过期条目，再淘汰最久未出现的一条） |

### `apps/server-admin`（包 `vaultsyncd-admin`）

| 项 | 契约 |
| --- | --- |
| 入口 | `vaultsyncd-admin init --state-dir <目录> [--login <登录名>]`，默认登录名 `owner`；无 `--password`（密码只从 stdin 读：终端下两遍且不回显，管道下一行） |
| 退出码 | `0` 成功；`1` 执行失败（状态目录不可用、数据库打不开、散列/写入失败）；`2` 用法或输入不合规（含两次密码不一致、弱密码、非法登录名）；`3` 状态目录被占用（服务端在运行或另一个 init）；`4` 已初始化过 |
| 输出 | 只打印 `login`、账号身份（32 位十六进制）、数据库路径与“Argon2id”这一事实；不打印密码，也不打印散列 |
| 流程 | 校验状态目录（绝对路径、已存在）→ 抢 `state_lock` → `Database::open(dir/server.sqlite)` → 一个写事务里 `prepare_owner` + 账号端口 `insert` → 提交 |

### 不变量与未接入

- **没有默认密码、没有公开注册**：写账号的唯一路径是离线 `init`；重复 init 被拒且旧散列原样保留（进程级用例断言）。
- **运行中不能 init**：服务端启动即独占 `<state_dir>/server.lock`（`StartupError::StateLock`），离线 init 抢不到锁时退出码 3；位置在打开数据库之前，因此不存在“两边都开着库再争锁”的窗口。
- 未接入（后续卡）：按来源地址限流（需要 HTTP 层与代理头策略）；Argon2id 参数的实测与 ADR；`apps/server` 尚未依赖 `apps/server-admin`（后者不需要）。会话与认证端点见下一节（S043）。

## 认证与会话（S043）

状态：S043 落地 `sessions` 表、会话用例、三个认证端点与凭据载体边界。设计依据 design §7.2、§15.2、§17.1、§17.2、I15、T31；载体边界见 `docs/adr/0001-auth-credential-carriers.md`。

### `crates/server-store`：`sessions`（迁移 0006）

| 项 | 契约 |
| --- | --- |
| 列 | `id`(16B) / `user_id` → `users` / `device_label`(非空) / `access_digest`(32B) / `refresh_digest`(32B) / `created_at` / `access_expires_at` / `refresh_expires_at` / `revoked_at` / `replaced_by` → `sessions` |
| 约束 | 两个摘要各带唯一索引；`created_at ≤ access_expires_at ≤ refresh_expires_at`；`replaced_by` 自引用后继会话 |
| `insert_session(tx,&NewSession)` | 设备标签空、有效期不自洽、身份/摘要重复都在写入前拒绝 |
| `find_by_access_digest` / `find_by_refresh_digest` / `find_session` | 按摘要或身份读回 `SessionRecord`（含 `revoked_at`、`replaced_by`） |
| `revoke_session(tx,id,at) -> bool` | 幂等：重复撤销返回 `false` 且不覆盖原撤销时间 |
| `rotate_refresh(tx,current,&successor,at) -> RotationOutcome` | 同一写事务内“抢占旧行（`revoked_at IS NULL` 条件 UPDATE）→ 插入后继 → 回填 `replaced_by`”；抢占失败返回 `Consumed`，无行返回 `Missing` |

与 design 字段清单的差异（有意）：`device_label` 代替 `device_id`（首发无设备注册表）；一行保存两个摘要（访问与刷新必须一起轮换）。

### `crates/server-core/src/auth/session.rs`

| 项 | 契约 |
| --- | --- |
| `SessionPolicy` | `standard()` = 访问 900 s / 刷新 30 天；`new(access,refresh)` 要求正且 access ≤ refresh |
| `SecretToken` | 32 字节随机数的 64 位十六进制文本；`parse`、`expose`、`digest()`（SHA-256）；`Debug`/`Display` 隐去内容 |
| `SessionStore` 端口 | `insert` / `find_by_access` / `find_by_refresh` / `rotate` / `revoke`，全部 `async` + `&mut self`（适配器可自持事务） |
| `issue(store,ids,now,user,label,policy)` | 校验设备标签（非空、≤128 字符、无控制字符）后签发；返回 `IssuedSession`（含明文凭据） |
| `authenticate(store,access,now) -> Principal` | 先判撤销、再判到期；`Expired` 在到期时刻即触发（`<=`） |
| `refresh(store,ids,now,refresh,policy)` | 已轮换的刷新凭据返回 `RefreshReused{replaced_by}`；不回放后继凭据；`rotate` 返回 `Consumed` 同样按重用拒绝 |
| `logout(store,access,now)` | 撤销当前会话；重复注销 `Ok` |
| 错误码 | `credential_invalid` / `credential_expired` / `session_revoked` / `refresh_reused` / `invalid_device_label` / `invalid_session_policy` / `session_storage_failed` |

### `crates/server-core/src/auth/scope.rs`

`RootGrant{root_id, owner_id}` + `require_root_scope(&Principal,&RootGrant) -> Result<(),ScopeError::AccessDenied>`（纯规则）。适配器必须先 `roots::root_in_scope(conn, owner, root)` 取行，因此“别人的 Root”与“不存在的 Root”都得到同一个 403 `ACCESS_DENIED`（错误码 `access_denied`），结论与日志不含绝对路径。

### `apps/server/src/auth.rs`：端点与载体

| 项 | 契约 |
| --- | --- |
| `POST /v1/auth/login` | 同源检查 → 限流 → 校验凭据 → 签发会话；返回 `SessionIssued` 并下发 `vaultsyncd_session`(`Path=/`) 与 `vaultsyncd_refresh`(`Path=/v1/auth`)，均为 `HttpOnly; Secure; SameSite=Strict` |
| `POST /v1/auth/refresh` | 凭据来自请求体 `refresh_token`（agent）；S077 浏览器改走独立 `browser/refresh`，旧端点不以 cookie 导出令牌；成功即轮换 |
| `POST /v1/auth/logout` | Bearer 或 cookie 认证 → 撤销会话 → 清两个 cookie（`Max-Age=0`）；不碰本地文件 |
| 认证载体 | `Authorization: Bearer <token>` 或 cookie；两者共用同一套会话与摘要 |
| CSRF | cookie 请求要求有效同源 `Origin` authority == `Host`，或 `Sec-Fetch-Site ∈ {same-origin, none}`；S077 起带 Cookie 且两者都缺失一律拒绝；失败 403 `ACCESS_DENIED` |
| 失败映射 | 凭据问题 401 `AUTH_EXPIRED`、越权 403 `ACCESS_DENIED`、限流 429 `RATE_LIMITED`（带 `Retry-After`）、存储失败 503 `NEEDS_RECOVERY`（记日志，不伪装成凭据过期） |
| 复用入口 | `principal_from(&AuthContext,&HeaderMap)`（认证 + 同源）、`authorize_root(&Database,&Principal,RootId)`（I15 的每次请求授权） |
| `AuthContext::new(Database)` | Argon2id 生产参数、默认限流与 `SessionPolicy::standard()`、OS 随机源；限流与随机源是进程内状态（重启清零） |

未接入（后续卡）：按来源地址限流、会话/账号的运维管理端点（重置密码后失效全部会话）与 `users.status` 的其他取值；Root 级业务端点（后续卡用 `principal_from` + `authorize_root` 接入）；客户端侧凭据保管（OS 凭据库 / 受限权限文件）。

## api/ — 远端协议契约源与生成配置（S024）

状态：S024 建立最小契约与生成通路。`api/` 是远端 HTTP 协议的唯一编辑源（design §5.6、§15.1—15.4），生成代码提交入库，`cargo xtask check` 用逐字节比对守住“契约与代码同步”。业务语义（状态机、不变量）仍以 design 为准，这里只记录契约文件的形状与生成器边界。

| 文件 | 契约 |
| --- | --- |
| `api/openapi.yaml` | OpenAPI `3.1.0`。当前端点：`GET /v1/health`（无需认证、`HealthResponse`、503 用错误信封）、`GET /v1/capabilities`（`Capabilities`、503 用错误信封）。`info.description` 会原样进生成文件表头 |
| `api/generation.toml` | 生成配置：`format_version = 6`（S090 起显式二进制流）、`generator = "vaultsyncd-xtask"`、`generator_version`（= 独立 GENERATOR_VERSION 常量）、`source`/`openapi_version`（远端）、`local_ipc_source`/`local_ipc_schema_draft`（本机 IPC）、`[outputs]` 五个路径、`[types.rust]`/`[types.typescript]` 映射表（两份契约共用） |
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
- 运行前核对 `api/generation.toml`：`format_version`、`generator`、`generator_version`（与 `env!("CARGO_PKG_VERSION")` 比）、`openapi_version`（与文档 `openapi` 字段比）、`local_ipc_schema_draft`（与本机 IPC 文档 `$schema` 比）任一不符即报错，避免配置与实现漂移。
- 可选字段在 Rust 侧带 `#[serde(skip_serializing_if = "Option::is_none")]`（S025 起）：缺省的可选字段不出现在 JSON 里，与 TypeScript 解码器“只接受缺省（`undefined`）”一致，写入方不写 `null`。已知宽容：serde 的 `Option<T>` 对显式 `null` 与缺省都解出 `None`，而 TS 解码器把 `null` 当畸形；正常链路（两侧都由生成代码写出）不会产生 `null`，样本里也没有。
- 闭合枚举与十进制校验的语义由两侧测试共同锁定：`cargo test --locked -p vaultsyncd-protocol`（13 个契约用例）+ `pnpm --filter @vaultsyncd/api-client test`（9 项运行时断言）。
- 生成文件表头标注“禁止手改”，不含时间戳或随机顺序，因此重新生成逐字节一致。
- **请求体（S043 起支持）**：`format_version = 3`。端点可有 `requestBody`，只接受 `application/json` 且必须引用具名 schema；可选请求体（`required: false`）与内联对象仍然报错。TypeScript 客户端为带请求体的端点生成 `body` 参数并设置 `content-type`，响应变量名改为 `payload` 以免与 `body` 参数重名。

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
| `src/generated/client.ts` | 生成：`FetchLike`、`ClientOptions`（`base_url`/`fetch`/`request_id`）、`ApiError`（`status` + 已解码 `body`）、`ProtocolError`（畸形响应/未知错误码）与每个端点的调用函数；非 2xx 按 `ErrorEnvelope` 解码成 `ApiError`。S046-b 起字节端点（`downloadVersionContent`）返回 `Uint8Array` 并走 `read_bytes`，`request_headers(options, init, content_type, accept)` 按响应体声明 `accept`；契约里的必填请求头参数（`x-part-sha256`）生成成函数参数并在 `request_headers(...)` 之后 `headers.set(...)`；可选请求头（`Range`）由调用方经 `init.headers` 传 |
| `src/index.ts` | 手写的稳定导出面（design §5.6）：只从这里导出类型、解码器、错误类与调用函数 |
| `tests/schema-decoders.mjs` | 运行时断言（9 项），直接以 `.mjs` 导入 `../src/generated/types.ts`（Node 24 原生剥离类型，无需构建步骤），并读取 `api/examples/` |

脚本：`typecheck` = `tsc --noEmit`（根 `pnpm typecheck` 覆盖）、`test` = `node tests/schema-decoders.mjs`。客户端不读全局 `fetch`/时钟：传输与 `x-request-id` 生成都由调用方注入，因此可在无浏览器的测试里替换。

## api/ — 本机 IPC 契约源（S025）

状态：S025 建立最小契约与生成通路；S071 增加 1.1 状态快照，S072 扩展至 1.2 管理命令（详见文末 S072）。本机 IPC 与远端 HTTP 契约**分开**：`api/local-ipc.schema.json` 是 JSON Schema（design §5.6、§15.5），不走网络、不复用 `api/openapi.yaml`；服务器管理 API 不能借这条通路控制客户端本地路径。生成代码提交入库，`cargo xtask generate --check` 逐字节比对。

| 文件 | 契约 |
| --- | --- |
| `api/local-ipc.schema.json` | JSON Schema `2020-12`。`$defs`：`IpcVersion`（`major`/`minor`）、`CommandName`（1.2 闭集 21 项，见 S072）、`CommandParameters`（按命令校验的字段集合）、`Command`、`Request`、`GetStatusResult`（`agent_version`/`ipc_protocol`，1.1 起可选 `bindings` 与 `snapshot_version`，1.2 起可选 `management`）、`BindingStatus`、`ManagementResult`、`BindingInfo`、`Problem`、`ConnectionInfo`、`RemoteItem`、`LocalIpcErrorCode`（1.2 稳定分类闭集）、`ErrorBody`、`ResponseKind`、`Response` |
| `x-vaultsyncd` 元数据 | `protocol`（`major`/`minor`，进生成常量 `PROTOCOL_MAJOR`/`PROTOCOL_MINOR`）与 `frame`（`header_bytes = 4`、`max_frame_bytes = 1048576`，进生成常量 `FRAME_HEADER_BYTES`/`MAX_FRAME_BYTES`）。`header_bytes` 必须是 4，`max_frame_bytes` 必须在 64 B—64 MiB 之间 |
| `api/examples/local-ipc.{request,response,error,status}.json`、`local-ipc.management.json` | 合法请求/旧成功应答/错误应答/1.1 状态增量/1.2 全管理命令样本，被 `crates/local-ipc/tests/contract.rs` 与 `packages/local-ipc-types/tests/schema-decoders.mjs` 直接读取；两侧都断言“解码后再编码等于样本” |

生成规则：与远端共用 `emit` 的 schema 渲染（对象/闭集枚举/基础类型），差别是 `$ref` 前缀为 `#/$defs/`、多出协议与帧常量、没有 HTTP 调用文件。schema 名 PascalCase、字段名 snake_case、每个 schema 与属性必须有 `description`、未知键与未支持构造（内联 enum/对象、非 `#/$defs/` 引用）一律报错——规则与失败行为见上一节。

## crates/local-ipc — 本机 IPC DTO、版本判定与消息帧（S025）

状态：S025 实现。包名 `vaultsyncd-local-ipc`，依赖 `serde`（derive），dev 依赖 `serde_json`；在 `members` 与 `default-members` 内（无平台 API，便携检查覆盖）。**不含** Windows 命名管道、ACL 与调用者身份校验（design §5.3；那些属于 S071 的平台传输）。

| 模块 | 契约 |
| --- | --- |
| `generated` | 生成代码（`api/local-ipc.schema.json` → `crates/local-ipc/src/generated/mod.rs`）：`PROTOCOL_MAJOR`/`PROTOCOL_MINOR`/`FRAME_HEADER_BYTES`/`MAX_FRAME_BYTES` 常量与全部 DTO。闭集枚举无 catch-all，未知命令/未知种类/未知错误码反序列化失败；显式关闭的对象拒绝未知字段，其余可选字段兼容旧客户端 |
| `version` | `SUPPORTED_MAJOR`/`SUPPORTED_MINOR`（= 生成常量）、`supported() -> IpcVersion`、`is_compatible(&IpcVersion) -> bool`：major 必须相同，minor 不得高于 agent 支持的版本（避免调用方假设未实现的命令存在）；更低 minor 兼容 |
| `framing` | `encode(&[u8]) -> Result<Vec<u8>, FrameError>`：4 字节大端长度头 + 负载；拒绝空负载与超过 `MAX_FRAME_BYTES` 的负载。`FrameDecoder::{new,push,next_frame,buffered_bytes,queued_frames}`：接受任意切分（含 1 字节一块），只有凑满声明长度才产出负载，截断数据永不产出短帧 |
| `service` | `Service::new(agent_version)`、`Service::handle(&Request) -> Response`；`Response::{ok,error,validate}` 与 `ResponseShapeError`。`get_status` 返回 agent 版本与该 agent 支持的 `ipc_protocol`，纯处理器不伪造持久状态（`bindings = None`）；应用组装补入核心快照；版本不兼容返回 `unsupported_version`（`retryable = false`），回显 `request_id` |

帧错误：`FrameError::{EmptyFrame, FrameTooLarge{bytes}, Backlog}`。不变式：等待中的部分帧最多 `FRAME_HEADER_BYTES + MAX_FRAME_BYTES` 字节；已完整但未取走的帧最多 `MAX_QUEUED_FRAMES = 4` 个；超过任一上限即报错而不再收数据。**报错后连接不可继续使用**（缓冲区可能仍有未消费字节），传输实现应关闭该连接。

未知命令与畸形 JSON 在产生应答之前就被拒绝：`Service` 只接收已解码的 `Request`，解码失败由传输层结束连接，因此不存在“未知命令回一个默认成功”的路径。`Response` 的 `kind` 与负载同理：只有 `ok` 带 `result`、`error` 带 `error`，构造只经 `Response::ok`/`Response::error`，`validate()` 供解码侧复核。

验证命令（devShell 内）：`cargo test --locked -p vaultsyncd-local-ipc`（契约与管理命令用例）、`cargo clippy --locked -p vaultsyncd-local-ipc --all-targets -- -D warnings`、`cargo fmt --check`、`RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps -p vaultsyncd-local-ipc`。

## packages/local-ipc-types — 本机 IPC TypeScript 类型（S025）

状态：S025 实现。包名 `@vaultsyncd/local-ipc-types`，`private: true`，无运行时依赖，`exports` 指向 `./src/index.ts`；已加入 `pnpm-workspace.yaml` 与根 `typecheck`。只给桌面端用（design §5.6），不含 Tauri invoke 或 IPC 客户端实现。

| 文件 | 内容 |
| --- | --- |
| `src/generated/types.ts` | 生成：`interfaces`、字符串字面量联合 + `<NAME>_VALUES`、`decode<Name>()` 解码器，以及 `PROTOCOL_*`/`FRAME_*`/`MAX_FRAME_BYTES` 常量；未知命令、未知应答种类、缺必填字段、非整数版本号都抛错 |
| `src/index.ts` | 手写的稳定导出面：只从这里导出类型、解码器与常量 |
| `tests/schema-decoders.mjs` | 运行时断言（9 项），直接以 `.mjs` 导入 `../src/generated/types.ts`，并读取 `api/examples/` |

脚本：`typecheck` = `tsc --noEmit`（根 `pnpm typecheck` 覆盖）、`test` = `node tests/schema-decoders.mjs`。

## crates/server-store — 服务端数据库迁移框架（S026）

状态：S026 实现。包名 `vaultsyncd-server-store`，依赖 `sqlx 0.8`（`runtime-tokio` + `sqlite`（bundled）+ `migrate` + `macros`），dev 依赖 `tokio`（`macros`/`rt-multi-thread`）；在 `members` 与 `default-members` 内（无平台 API，便携检查覆盖）。只做服务端 SQLite 的打开、门禁、迁移与事务，不含 HTTP、工作目录与对象 I/O。

### 模块与契约

| 项 | 契约 |
| --- | --- |
| `db::SERVER_APPLICATION_ID` | `0x56534E31`（ASCII `VSN1`），写入 `PRAGMA application_id`，是“这个文件是服务端库”的唯一标识；客户端库用另一个标识（由 `crates/client-store` 自己的卡定义） |
| `db::MIGRATOR` | `sqlx::migrate!("./migrations")`，编译期内嵌 `crates/server-store/migrations/*.sql`；文件名 `<版本>_<描述>.sql`，版本单调递增，已发布文件不得改写（校验和不符即拒绝启动） |
| `db::target_schema_version()` | 内嵌迁移的最大版本号（当前 `3`：0001 迁移框架基线 + 0002 命名空间表 + 0003 对象/版本/批次/操作表），即 `PRAGMA user_version` 的目标值；全部已知迁移成功后才写入 |
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
| `entries::EntryError` | `Db` / `OutOfScope` / `EntryNotFound{id}` / `ParentNotFound{id}` / `ParentNotDirectory{id}` / `CrossRootParent{entry_root,parent_root}` / `DuplicateName{parent,name}` / `DirectoryCycle{entry,parent}` / `DirectoryNotEmpty{id}` / `InvalidName{name,issue}` / `EmptyNameKey` / `DirectoryHasContentVersion` / `UnknownContentVersion{version}` / `DuplicateId{id}` / `MalformedRow{column}` |

### 表与不变量

- **owner scope**：`roots.owner_id → users(id)` 外键；每个读写接口都带 `UserId`，不属于该 owner 的 Root/条目返回 `OutOfScope` 或 `None`，不区分“不存在”与“别人的”。
- **父子同 Root**：`entries(root_id, parent_id) → entries(root_id, id)` 复合外键（引用目标由 `UNIQUE (root_id, id)` 提供），跨 Root 父项在 SQL 层即失败；写接口先给出 `CrossRootParent`。
- **活跃同父名称唯一**：部分唯一索引 `(root_id, parent_id, name_key) WHERE deleted_at IS NULL` 与 `(root_id, parent_id, name) WHERE deleted_at IS NULL`；`name_key` 由调用方按平台等价规则派生（design §7.2，本层不小写、不归一化），名称索引兜住“名字完全一样却给了不同键”。软删除后名称可复用。
- **一个 Root 一个合成根条目**：`(root_id) WHERE parent_id IS NULL AND deleted_at IS NULL` 部分唯一索引；根条目名称与比较键为空字符串、类型固定目录、`revision` 从 0 起。
- **目录环**：移动时从目标父项沿 `parent_id` 向上走递归 CTE（`UNION` 去重，坏环也终止），命中被移动条目即 `DirectoryCycle`；自己当父项同样拒绝。
- **路径与身份**：`roots.absolute_path` 全库唯一，形式检查为“非空、无 NUL、以 `/` 起始、不以 `/` 结尾”（根 `/` 例外），不用宿主 `Path::is_absolute`（本 crate 在 Windows 便携检查里也跑测试）；重叠/别名属 S032/S044。`volume_identity`/`root_file_identity` 是不透明非空字节。
- **删除不级联**：本卡只软删单条并拒绝删除非空目录；子树删除与回收站属 S039。`entries.content_version_id` 的填写规则见 S028：新建时写入的内容版本必须已记录到该条目（`UnknownContentVersion`），当前版本指针只由 `versions::record_version` 改写；指向 `versions` 的外键仍待 `entries` 重建表时再加（判据见 S028 一节）。
- **写事务**：写接口要求调用方已开 `BEGIN IMMEDIATE` 事务，检查与写入在同一次持锁事务内；只读接口不抢写锁。

验证命令（devShell 内）：`cargo test --locked -p vaultsyncd-server-store`、`cargo clippy --locked -p vaultsyncd-server-store --all-targets -- -D warnings`、`cargo fmt --check`、`RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps -p vaultsyncd-server-store`。

## crates/server-store — 版本、事件与幂等表（S028）

状态：S028 实现。表定义在 `crates/server-store/migrations/0003_versions_events.sql`（`objects`/`versions`/`changes`/`operations`），读写接口在 `src/versions.rs`、`src/changes.rs`、`src/operations.rs`，schema 目标版本随之为 `3`（S029 再追加 0004 后为 `4`）。不写 HTTP/SSE、不算哈希、不做文件系统 I/O：哈希与对象落盘由平台层完成后再交给本层（design §11.2）。

### 模块与契约

| 项 | 契约 |
| --- | --- |
| `changes::ChangeBatch` | 一条事务批次：`root`、`seq`（`RootSeq`，从 1 起）、`tx`（`TxId`）、`events_json`（非空 JSON 数组文本）、`created_at` |
| `changes::append_change(tx, owner, root, tx_id, events_json, at)` | 读 `roots.seq` → 比较并写推进（`WHERE seq = 旧值`）→ 插入批次行；返回分配到的批次。`events_json` 不是非空 JSON 数组时 `InvalidEvents` |
| `changes::changes_after(conn, owner, root, after, limit)` | 按 `seq > after` 升序取批次，`limit ∈ 1..=MAX_CHANGE_PAGE(500)`；每行是一整批，分页不拆批。Root 不在 owner scope 内返回 `OutOfScope`（不返回空列表） |
| `changes::ChangeError` | `Db` / `OutOfScope` / `SeqExhausted` / `SeqConflict{root,expected}` / `InvalidEvents` / `InvalidLimit` / `MalformedRow{column}` |
| `versions::NewObject` / `ObjectRecord` | 已持久化并校验过的内容对象：`root`、`hash`（SHA-256 32 B）、`size`、`storage_path`、`verified_at`，记录另带 `gc_state` |
| `versions::ObjectGcState` | `Live` / `Collectable`（数据库文本 `live` / `collectable`）；更细的 GC 阶段属对象回收卡 |
| `versions::record_object(tx, owner, &NewObject)` | 同一 `(root, hash)` 以相同长度与路径重复登记是幂等的（返回库里记录）；长度或路径不同 `ObjectConflict`；路径被别的对象占用 `DuplicateStoragePath` |
| `versions::object_in_scope(conn, owner, root, hash)` | 按内容哈希读对象（含已标记回收的）；owner scope 不匹配返回 `None` |
| `versions::NewVersion` / `VersionRecord` | 新版本输入（`id`、`root`、`entry`、`object_hash`、`size`、`created_seq`、`origin`、`at`）与读回记录（含 `superseded_at`） |
| `versions::VersionOrigin` | `Client` / `External`（数据库文本 `client` / `external`）；首次同步导入记 `Client`，服务端采集的外部变更记 `External`（design I14 同一流程） |
| `versions::record_version(tx, owner, &NewVersion)` | 核对条目（活跃文件）→ 对象（存在且长度一致）→ 批次（存在）→ 取代旧当前版本 → 插入新版本 → 改写条目当前版本指针；全部在调用方事务内 |
| `versions::current_version` / `version_history` | 当前版本（`superseded_at IS NULL`）与历史清单（新的在前，`limit ∈ 1..=MAX_VERSION_PAGE(500)`，已删除条目的历史仍可读） |
| `versions::VersionError` | `Db` / `OutOfScope` / `EntryNotFound{id}` / `NotAFile{id}` / `UnknownObject{hash}` / `ContentSizeMismatch{version_size,object_size}` / `SeqNotRecorded{seq}` / `ObjectConflict{hash,reason}` / `EmptyStoragePath` / `DuplicateStoragePath{path}` / `CurrentVersionConflict{entry}` / `DuplicateId{id}` / `SizeAboveDbLimit{size}` / `InvalidLimit` / `MalformedRow{column}` / `Entry(EntryError)` |
| `operations::OperationKey` | 幂等键 `root` + `device`(`DeviceId`) + `op`(`OpId`)；数据库主键 `(root_id, device_id, op_id)` |
| `operations::request_digest(fields)` | 由语义字段算 SHA-256：拒绝空/重复字段名，按字段名排序后以长度前缀编码（值长度 `u64` BE、名长度 `u32` BE）并加固定语义版本前缀。字段顺序无关，语义不同则摘要不同。调用方传语义值（规范十进制计数、身份原始字节），**不传**原始请求字节或 JSON |
| `operations::NewOperation` / `BeginOperation` / `OperationRecord` | 开始输入（键 + 摘要）；结果 `Started`（首次，已写 `pending`）或 `Replayed(OperationRecord)`（同键同摘要，直接返回记录）；记录含 `digest`、`state`、`result_json`、`root_seq`、`intent_path` |
| `operations::OperationState` | `Pending` / `Committed` / `Rejected`（数据库文本 `pending` / `committed` / `rejected`）；后两者是终态且有稳定结果 |
| `operations::begin_operation(tx, owner, &NewOperation)` | 首次写 `pending`；同键同摘要返回 `Replayed`；同键不同摘要 `DigestMismatch` |
| `operations::attach_intent(tx, owner, &key, path)` | 记下工作目录发布意图位置（design §11.2 步骤 5）；仅 `pending` 可改，空路径 `EmptyIntentPath` |
| `operations::commit_operation(tx, owner, &key, tx_id, events_json, result_json, at)` | design §11.2 步骤 7 的元数据完成事务：追加事件批次（推进 `RootSeq`）并把操作写成 `committed` + `result_json` + `root_seq`，同一事务提交。已终态报 `AlreadyFinished` |
| `operations::reject_operation(tx, owner, &key, result_json)` | 稳定拒绝：写 `rejected` + 结果，不推进 `RootSeq`；重试得到同一结果 |
| `operations::operation_in_scope(conn, owner, &key)` | 按幂等键读记录（用于 `GET /v1/roots/{root}/operations/{op}`）；owner scope 不匹配返回 `None` |
| `operations::discard_finished_operations(tx, owner, root, device)` | 设备代际失效时删除该设备的**终态**记录，返回行数；`pending` 记录保留 |
| `operations::OperationError` | `Db` / `OutOfScope` / `UnknownOperation{key}` / `DigestMismatch{key}` / `AlreadyFinished{key,state}` / `EmptyRequestField` / `DuplicateRequestField{name}` / `RequestFieldTooLong{name_len}` / `DuplicateOperation{key}` / `InvalidResult{reason}` / `EmptyIntentPath` / `Change(ChangeError)` / `MalformedRow{column}` |
| `operations::{REQUEST_DIGEST_LEN, MAX_RESULT_BYTES}` | 请求摘要 32 字节；紧凑结果上限 4096 字节 |

### 表与不变量

- **版本只引用已持久化内容**：`versions(root_id, object_hash) → objects(root_id, hash)`；`objects` 只在内容落盘且校验后才登记（`verified_at NOT NULL`），长度不一致在写入前报 `ContentSizeMismatch`（design I01/I05）。
- **版本只属于已记录的批次**：`versions(root_id, created_seq) → changes(root_id, seq)`；`operations(root_id, root_seq) → changes(root_id, seq)`（`pending`/`rejected` 的 `root_seq` 为 NULL，复合外键对含 NULL 行不生效）。
- **RootSeq 单调、批次不可拆**：`changes` 一行就是一批事件，`(root_id, seq)` 主键；追加时用比较并写推进 `roots.seq`，失败报 `SeqConflict` 而不是自增。回滚的事务既不暴露批次也不消耗序号（下一批仍从原 seq+1 开始）。
- **历史只读**：触发器 `versions_history_is_immutable` 拒绝改写版本的标识与内容引用（design I12），唯一可改的是 `superseded_at`；`objects_content_reference_is_immutable` 同理拒绝改写对象的内容地址证据。恢复历史要写新版本并让它成为当前版本。
- **每个条目至多一个当前版本**：部分唯一索引 `(root_id, entry_id) WHERE superseded_at IS NULL`；`record_version` 先取代旧当前版本再插入新行。
- **条目当前版本指针**：`entries.content_version_id` 只由 `versions::record_version` 改写（`pub(crate) entries::set_current_content_version`）；新建条目时若给了内容版本，必须先已记录到该条目（`EntryError::UnknownContentVersion`），否则会留下悬空指针。**未**重建 `entries` 加 `content_version_id → versions(id)` 外键：SQLite 追加外键只能重建表，而 `entries` 与 `versions` 互为引用（循环外键）需要在迁移事务里 `PRAGMA defer_foreign_keys`；本卡用“唯一写入路径 + 写入前存在性核对”达到同样的可见保证，待 `entries` 因其它原因重建时再补该外键。
- **pending 不是 committed**：`operations` 的 CHECK 保证 `pending` 不带结果、不占 `root_seq`；`committed` 必须有 `root_seq` 与结果；`rejected` 有结果但无 `root_seq`。
- **紧凑结果保留规则**：只有终态保存 `result_json`，必须是 JSON object 且 ≤ 4096 字节；记录与结果保留到相应设备代际失效（design §7.2、§9.4），届时用 `discard_finished_operations` 删除终态记录；`pending` 不删，否则同一 `op_id` 会被当成新命令重新执行。
- **owner scope**：所有接口都带 `UserId`，经 `roots.owner_id` 判定；`changes_after` 与 `version_history` 对越权 Root 报错而不是返回空列表，避免把权限失败误当成“已追平”。

验证命令（devShell 内）：`cargo test --locked -p vaultsyncd-server-store`、`cargo clippy --locked -p vaultsyncd-server-store --all-targets -- -D warnings`、`cargo fmt --check`、`RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps -p vaultsyncd-server-store`。

## crates/server-store — 上传、意图与租约表（S029）

状态：S029 实现。表定义在 `crates/server-store/migrations/0004_uploads_intents_leases.sql`（`uploads`/`upload_parts`/`fs_intents`/`backup_leases`/`object_leases`），读写接口在 `src/uploads.rs`、`src/intents.rs`、`src/leases.rs`，schema 目标版本随之为 `4`。不接收字节、不算哈希、不碰暂存文件、不发布对象、不执行 GC：流式接收与对象落盘分别属 S045 与 S033，GC 只把未过期租约当额外引用。

### 模块与契约

| 项 | 契约 |
| --- | --- |
| `uploads::ChunkGeometry` | 会话块几何：`new(total_size, chunk_size)`（块大小 0 报 `InvalidChunkSize`）、`part_count()`（`ceil(total/chunk)`，空内容 0 块）、`part_length(index)`（越界 `None`） |
| `uploads::UploadState` | `Open` / `Complete` / `Finalized` / `Aborted`（数据库文本 `open`/`complete`/`finalized`/`aborted`）；转换 `open→complete|aborted`、`complete→finalized|aborted`，后两者为终态 |
| `uploads::NewUpload` / `UploadRecord` | 会话输入与读回：`id`(`UploadId`)、`root`、`session`(`SessionId`)、`expected_size`、`hash`(32 B)、`chunk_size`、`expires_at`、`state`；记录另有 `geometry()` |
| `uploads::begin_upload(tx, owner, &NewUpload)` | 核对 owner scope 与块几何后写 `open` 会话；重复身份 `DuplicateId`，越界长度 `SizeAboveDbLimit` |
| `uploads::NewUploadPart` / `UploadPart` / `PartOutcome` | 块输入（`index`、`length`、`hash`、`path`）与读回；结果 `Recorded`（首次）或 `AlreadyPresent`（同索引同内容重传，幂等） |
| `uploads::record_part(tx, owner, upload, &NewUploadPart)` | 索引越界 `PartOutOfRange`、长度不符 `PartLengthMismatch`、同索引不同内容 `PartConflict`（不覆盖旧行）、空路径 `EmptyPartPath`、终态会话 `PartsClosed` |
| `uploads::upload_progress` / `parts_of` / `unfinished_uploads` | 续传依据：会话 + 已收块（按索引升序）+ 缺失块区间 `MissingRange{first_index,count}`（因此列表长度 ≤ 已收块数 + 1）；未结束上传按过期时间与身份排序 |
| `uploads::mark_upload_complete` / `mark_upload_finalized` / `abort_upload` | 缺块时 `IncompleteUpload{missing_parts}`；只能 `complete→finalized`；放弃保留已收块行作为清理证据；终态不再变化 |
| `intents::FsIntentKind` | `PutContent` / `Create` / `Move` / `Delete`（文本 `put_content`/`create`/`move`/`delete`） |
| `intents::FsIntentPhase` | `Prepared` / `Published` / `Finalized` / `NeedsRecovery`；转换 `prepared→published|needs_recovery`、`published→finalized|needs_recovery`、`needs_recovery→finalized`（唯一人工出口） |
| `intents::NewFsIntent` / `FsIntent` | 意图输入与读回：`intent_id`(`IntentId`)、`root`、`kind`、`target_path`（Root 内相对路径）、`staging_path`（可空）、`expected_before`/`desired_after`（JSON object）、`phase`、`created_at`/`updated_at` |
| `intents::begin_intent(tx, owner, &NewFsIntent, at)` | 校验路径形式与两个 JSON object 后写 `prepared`；路径逃出 Root、空/含 NUL 的暂存路径、非 object 状态、重复身份均拒绝且不写入 |
| `intents::advance_intent_phase(tx, owner, id, next, at)` | 只按上表转换；时间倒退 `TimeWentBackwards`；比较并写发现绕过写入 `PhaseConflict` |
| `intents::unfinished_intents` / `intent_in_scope` | 按 Root 列出 `phase <> 'finalized'`（含 `needs_recovery`）的意图，按 `created_at, intent_id` 升序；越权 Root 报 `OutOfScope`，不返回空列表 |
| `leases::ObjectLeasePurpose` / `NewObjectLease` / `ObjectLease` | 对象保护租约：用途 `Download` / `Commit`，字段 `id`(`ObjectLeaseId`)、`root`、`object_hash`、`expires_at` |
| `leases::lease_object` / `object_lease_in_scope` / `active_object_leases` / `release_object_lease` / `prune_expired_object_leases` | 只能保护已登记对象（`UnknownObject`），`expires_at <= now` 拒绝写入（`AlreadyExpired`）；“有效”等于 `expires_at > now`；释放幂等（第二次 `false`） |
| `leases::SnapshotSeqMap` | 备份快照映射：按 Root 身份升序的 `(RootId, RootSeq)`，`new` 拒绝空映射与重复 Root；`to_json` 产出 `{"<root hex>":"<decimal seq>"}`，`from_json(conn, text)` 用 `json_each` 解析并要求值是文本类型 |
| `leases::NewBackupLease` / `BackupLease` | `id`(`BackupLeaseId`)、`snapshot_seq_map`、`expires_at`、`manifest_ref`；跨 Root，无 `root_id`/owner 列 |
| `leases::insert_backup_lease` / `backup_lease` / `active_backup_leases` / `release_backup_lease` / `prune_expired_backup_leases` | 空清单/过期/未注册 Root/重复身份均拒绝；按 `expires_at` 过滤有效租约；释放幂等 |

### 表与不变量

- **索引唯一、重复内容才幂等**：`upload_parts` 主键 `(upload_id, part_index)`，内容（长度、哈希、路径）全同才算幂等重传；`path` 全库唯一，避免按会话清理误删别人的字节。
- **会话绑定不可变快照**：`uploads` 的 `expected_size`/`hash` 与 `chunk_size` 在创建时确定，块几何决定块数与每块期望长度；`state` 的 CHECK 只允许四个取值，块未齐不可能进入 `complete`（`mark_upload_complete` 核对）。`session_id` 只校验长度：`sessions` 表属 S043，追加外键需重建表，由该卡决定。
- **意图 phase 是持久化判据**：`fs_intents.phase` 的取值由迁移 CHECK 限定，转换由 `FsIntentPhase::allows_transition_to` 唯一决定；启动恢复读 `phase` 而不是重放普通日志（design §11.3）。
- **“当时不存在”不是 NULL**：`expected_before`/`desired_after` 都是 `NOT NULL` 的 JSON object（`json_valid` + `json_type='object'`），不存在必须写成对象字段，避免把“未知”与“不存在”混为一谈。
- **租约只保护已登记对象**：`object_leases(root_id, object_hash) → objects(root_id, hash)`，`ON DELETE RESTRICT` 让删除仍被租约保护的对象行失败；`expires_at > now` 是唯一有效期判据。
- **owner scope**：`uploads`/`fs_intents`/`object_leases` 的接口都带 `UserId`，经 `roots.owner_id` 判定；`backup_leases` 跨 Root（design §7.2），读接口不带 owner scope，管理 API 鉴权属 S087。

验证命令（devShell 内）：`cargo test --locked -p vaultsyncd-server-store`、`cargo clippy --locked -p vaultsyncd-server-store --all-targets -- -D warnings`、`cargo fmt --check`、`RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps -p vaultsyncd-server-store`。

## crates/client-store — 客户端数据库与状态表（S030）

状态：S030 实现。包名 `vaultsyncd-client-store`，依赖 `sqlx 0.8`（`runtime-tokio` + `sqlite`（bundled）+ `migrate` + `macros`）与 `vaultsyncd-domain`，dev 依赖 `tokio`；在 `members` 与 `default-members` 内（无平台 API，便携检查覆盖）。与 `crates/server-store` **没有依赖关系**：两套 schema、两个数据库文件（design §5.4、§7.3）。不做文件系统 I/O、不规划、不传输、不生成身份/指纹/generation。

### 模块与契约

| 项 | 契约 |
| --- | --- |
| `db::CLIENT_APPLICATION_ID` | `0x434C4E54`（ASCII `CLNT`），写入 `PRAGMA application_id`；与 `server_store::SERVER_APPLICATION_ID`（`VSN1`）互相拒绝 |
| `db::MIGRATOR` | `sqlx::migrate!("./migrations")` 编译期内嵌 `crates/client-store/migrations/*.sql`（`0001_initial.sql` 属 S030，`0002_events_transfers_intents.sql` 属 S031）；已发布文件不得改写（校验和不符即拒绝启动） |
| `db::target_schema_version()` | 内嵌迁移最大版本号（当前 `3`，S055 追加 `0003_upload_resumes.sql`），即 `PRAGMA user_version` 的目标值 |
| `db::migration_backup_path(db, from, to)` | `<库路径>.pre-migration-v<from>-to-v<to>.bak`，客户端升级不清库（design §19.3），回退走整套备份 |
| `db::Database::open(path)` | 固定顺序：只读探测种类/版本 → 拒绝非客户端库 → 确认 WAL → 迁移前备份 → 执行迁移 → 写回标记 → 建池；失败返回 `DbError` 且不给可写句柄。父目录须已存在 |
| `db::Database::{path, schema_version, pool, close}` | 路径、打开时确认的版本、只读查询用的连接池、关闭池 |
| `db::Database::begin_write()` | `BEGIN IMMEDIATE` 短写事务（`sqlx::Transaction<'static, Sqlite>`），等锁上限 10 s；只允许数据库语句 |
| `db::DbError` | `Io` / `NonUtf8Path` / `Sqlite` / `NotADatabase` / `ForeignDatabase{application_id}` / `SchemaTooNew{found,supported}` / `Backup{path,source}` / `BackupUnverified{path,detail}` / `WalUnavailable{journal_mode}` / `Migration{backup,source}`；取值与门禁规则同 S026 的服务端框架（方向相反） |
| `bindings::SyncMode` | `Full` / `OnDemand`（数据库文本 `full` / `on_demand`，design C34；不含单向模式） |
| `bindings::NewBinding` / `BindingRecord` | 绑定输入与读回记录：`id`(`BindingId`)、`server`(`ServerId`)、`device`(`DeviceId`)、`root`(`RootId`)、`local_path`、`cache_path`、`mode`、`rules_version`、`server_epoch` |
| `bindings::insert_binding(tx, &NewBinding)` / `binding(conn, id)` | 写入（`local_path`/`cache_path` 非空无 NUL、`rules_version` 非空、`id` 唯一）与按身份读取 |
| `bindings::BindingError` | `Db` / `InvalidField{field,reason}` / `DuplicateId{id}` / `MalformedRow{column}` |
| `entries::EntryKind` | `File` / `Directory`（数据库文本 `file` / `directory`） |
| `entries::{BaselineEntry, RemoteEntry}` | 同一组字段：`entry`、`parent: Option<EntryId>`、`name`、`kind`、`revision: Revision`、`content_version: Option<VersionId>`、`deleted: bool` |
| `entries::LocalEntry` | `entry`、`generation: Generation`、`dirty: Dirty`、`residency: Residency`、`fingerprint: Option<Vec<u8>>`、`file_identity: Option<Vec<u8>>` |
| `entries::{put_baseline,put_remote,put_local}` | 按 `(binding_id, entry_id)` upsert；写库前检查根条目形状与空字节 |
| `entries::{baseline_entry,remote_entry,local_entry}` | 只读查询（`&mut SqliteConnection`，可直接用连接池）；不存在返回 `None` |
| `entries::{remove_baseline,remove_remote,remove_local}` | 删除单行，返回是否命中；不影响另外两张表 |
| `entries::EntryError` | `Db` / `RootEntryShape{reason}` / `EmptyBytes{field}` / `MalformedRow{column}` |
| `queue::NewPendingOp` / `PendingOp` | 入队输入与读回记录：`op`(`OpId`)、`binding`、`kind`、`entry: Option<EntryId>`、`baseline_revision`、`baseline_version`、`depends_on: Option<OpId>`、`pinned_version: Option<VersionId>`、`attempts: u32`、`last_error: Option<ErrorCode>` |
| `queue::enqueue(tx, &NewPendingOp)` / `pending_op(conn, op)` / `pending_ops_for_binding(conn, binding)` | 入队（类型非空、禁止自依赖、依赖必须已在队列中）、按身份读取、按插入顺序（`rowid`）列队列 |
| `queue::record_attempt_failure(tx, op, code)` / `complete(tx, op)` | `attempts+1` 且写最近错误码（其余字段不变）/ 成功后出队（仍有依赖方时因 `ON DELETE RESTRICT` 失败） |
| `queue::QueueError` | `Db` / `EmptyKind` / `DuplicateOp{op}` / `SelfDependency{op}` / `UnknownDependency{op}` / `MalformedRow{column}` |

### 表与不变量

- **三张条目表互相独立**：`baseline_entries`、`remote_entries`、`local_entries` 之间没有外键，三张表各自只依赖 `bindings(id)`（`ON DELETE CASCADE` 只随绑定解绑）。因此同一条目能同时保存“旧基线（revision 3）+ 新远端（revision 5）+ 本地 dirty（generation 7）”，任一张表的写入或删除都不约束另外两张（design §7.3、§8.1、§8.4）。
- **无基线也能有远端/本地行**：首次同步（I07）与本地新建文件在提交前都没有共同基线，两张表都不引用 `baseline_entries`。
- **位置用身份表达**：`parent_entry_id` + `name`；根条目（`parent_entry_id IS NULL`）名称必须为空且只能是目录，与 `CHECK` 一致。
- **目录没有内容版本**：`CHECK (kind <> 'directory' OR content_version_id IS NULL)`（design §7.1、I12）。
- **`dirty`/`residency` 文本与 design §8.1 拼写一致**（`clean`/`dirty`/`snapshotting`/`awaiting_commit`、`metadata_only`/`partial`/`materialized`），由 `CHECK` 限定；`clean` 只是上次观察，不是“文件未变”的证明（design §8.4）。
- **`generation` 只由调用方推进**：本层不改写它，写入方按观察到的本地修改推进；旧上传确认不得清除新 generation 的规则属执行器。
- **队列顺序 = 插入顺序**：取队列用 `ORDER BY rowid`，不使用客户端时钟；`kind` 的取值集合留给执行器卡（S055），本层只要求非空；`depends_on` 自引用受 `CHECK (depends_on <> op_id)` 与 `ON DELETE RESTRICT` 约束。
- **种类门禁双向**：客户端拒绝 `VSN1` 服务端库（测试断言拒绝后 `application_id`、`user_version`、表结构与 `journal_mode` 均未改动，且没有建出任何客户端表）；服务端拒绝 `CLNT` 客户端库由 S026 的门禁与测试覆盖（同一 `application_id` 取值）。
- **迁移/备份/连接默认项**：与 S026 同一套规则——迁移号与 SQLx 记账同事务、迁移前 `VACUUM INTO` + `quick_check` 备份（仅非空库且确有待执行迁移）、`user_version` 与 `_sqlx_migrations` 取大拒绝旧二进制、`WAL`+`foreign_keys`+`busy_timeout=10 s` 每连接默认、池上限 4。

验证命令（devShell 内）：`cargo test --locked -p vaultsyncd-client-store`、`cargo clippy --locked -p vaultsyncd-client-store --all-targets -- -D warnings`、`cargo fmt --check`、`RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps -p vaultsyncd-client-store`、`sqlite3 :memory: < crates/client-store/migrations/0001_initial.sql`；S031 追加的事件、传输、意图与通知表见下一节。

## crates/client-store — 事件、传输、意图与通知（S031）

状态：S031 实现。表定义在 `crates/client-store/migrations/0002_events_transfers_intents.sql`（`received_changes`/`transfers`/`local_intents`/`notifications`，并给 `bindings` 追加 `received_seq`/`applied_seq` 游标列），读写接口在 `src/events.rs`、`src/transfers.rs`、`src/intents.rs`、`src/notifications.rs`；schema 目标版本随之为 `2`（S055 的 `0003_upload_resumes.sql` 再追加上传续传表，见文件末尾）。仍然不做文件系统 I/O、不拉事件、不传输、不算哈希、不读时钟：wire 变更列表在应用层映射成 `RemoteEntry` + `NewPendingOp` 后交给本层。

### 模块与契约

| 项 | 契约 |
| --- | --- |
| `events::IncomingBatch` | 一个不可拆分事务批次：`binding`、`seq`(`RootSeq`，从 1 起)、`tx`(`TxId`)、`events_json`（非空 JSON 数组文本，与 `server_store::changes::ChangeBatch` 同形）、`changes: Vec<RemoteEntry>`、`tasks: Vec<NewPendingOp>`（按依赖顺序） |
| `events::receive_batch(tx, &IncomingBatch)` | 同一事务内依次写 `received_changes` → `remote_entries` → `pending_ops` → 最后推进 `bindings.received_seq`，返回 `ReceivedBatch`。重复批次（同 seq + 同 tx + 同事件）返回已存记录且不重复入队；同 seq 不同内容 `ConflictingBatch`；seq 不大于游标 `StaleBatch`；seq 0 `InvalidSeq`；任务绑定与批次不一致 `TaskBindingMismatch`；绑定不存在 `UnknownBinding` |
| `events::ReceivedBatch` / `BatchState` | 批次记录（`seq`、`tx`、`events_json`、`state`、`last_error`）与状态 `Received` / `Applied` / `Blocked`（数据库文本 `received` / `applied` / `blocked`） |
| `events::cursors(conn, binding)` / `Cursors` | 一对游标 `received`、`applied`（`RootSeq`，0 = 还没有批次）；绑定不存在 `UnknownBinding` |
| `events::received_batch` / `unapplied_batches` | 按 `(binding, seq)` 读批次；列出状态不等于 `applied` 的批次（升序），即重启后的重放清单 |
| `events::mark_batch_applied(tx, binding, seq)` / `mark_batch_blocked(tx, binding, seq, code)` | 标记应用完成（并把 `applied_seq` 推进到“连续已应用前缀”末端）/ 标记阻塞并记原因（游标停在它之前，处理后仍可转已应用）；均返回是否命中 |
| `events::EventError` | `Db` / `UnknownBinding{binding}` / `InvalidSeq` / `InvalidEvents{reason}` / `ConflictingBatch{seq}` / `StaleBatch{seq,received}` / `TaskBindingMismatch{op}` / `Entry(EntryError)` / `Queue(QueueError)` / `MalformedRow{column}` |
| `transfers::NewTransfer` / `TransferRecord` | 入站内容传输：`binding`、`version`(`VersionId`)、`chunk_size`、`total_size`、`bitmap`（低位在前，第 i 位 = 第 i 块）、`temp_path`；记录另带 `state`、`verified`、`last_error`，并有 `chunk_count()` / `has_chunk(i)` |
| `transfers::TransferState` | `Running` / `Cancelled` / `Completed` / `Failed`（数据库文本 `running` / `cancelled` / `completed` / `failed`）；只有 `running` 接受进度更新 |
| `transfers::start_transfer(tx, &NewTransfer)` / `transfer` / `transfers_for_binding` | 开始传输（位图长度与尾部 bit 必须与块几何一致、临时路径非空无 NUL、`(binding,version)` 唯一）/ 读单条 / 列某个绑定的全部进度（按版本字节序） |
| `transfers::record_chunk(tx, binding, version, index)` | 记录第 `index` 块已收到；返回是否是新块。越界 `ChunkOutOfRange`、终态 `TransferClosed`、缺记录 `UnknownTransfer` |
| `transfers::{complete_transfer, fail_transfer, cancel_transfer, discard_transfer}` | 完成（要求位图覆盖全部块，写 `verified = 1`）/ 记失败（必须带错误码）/ 取消（保留位图与临时路径）/ 删除记录；均返回是否命中 |
| `transfers::TransferError` | `Db` / `ZeroChunkSize` / `SizeAboveDbLimit` / `BitmapSizeMismatch{expected,found}` / `BitmapTailNotZero` / `InvalidTempPath{reason}` / `DuplicateTransfer{version}` / `TransferClosed{state}` / `ChunkOutOfRange{index,chunks}` / `IncompleteContent{missing}` / `UnknownTransfer{version}` / `MalformedRow{column}` |
| `intents::NewLocalIntent` / `LocalIntent` | 可恢复本地动作：`binding`、`kind`、`entry: Option<EntryId>`、`target_path`、`source_path`、`content_hash`（SHA-256 原始字节）、`generation`；记录另带本地序号 `id`、`state`、`last_error` |
| `intents::LocalIntentKind` / `IntentState` | `Replace` / `Move` / `Release` / `Import`（数据库文本 `replace` / `move` / `release` / `import`）与 `Pending` / `Failed` |
| `intents::begin_intent(tx, &NewLocalIntent)` / `intent(conn, id)` / `intents_for_binding(conn, binding)` | 写意图并返回记录（同一绑定同一 `target_path` 唯一；路径非空无 NUL；空哈希拒绝）/ 读单条 / 按 `intent_id` 升序列出（恢复顺序） |
| `intents::record_intent_failure(tx, id, code)` / `complete_intent(tx, id)` | 记失败（保留路径、哈希与 generation）/ 完成后删行；均返回是否命中 |
| `intents::IntentError` | `Db` / `InvalidPath{field,reason}` / `EmptyBytes{field}` / `DuplicateTarget{path}` / `MalformedRow{column}` |
| `notifications::NewNotification` / `Notification` | 问题上报输入与记录：`dedupe_key`、`binding: Option<BindingId>`（`None` = 全局问题）、`entry`、`code`(`ErrorCode`)、`state`、`is_read`、`occurrences` |
| `notifications::NotificationState` | `Active` / `Resolved`（数据库文本 `active` / `resolved`） |
| `notifications::raise(tx, &NewNotification)` | 按去重键合并：首次写入 `active`/未读/次数 1；仍 `active` 时只加次数并保留已读；`resolved` 后复发回到 `active` 并置未读。同键不同错误码 `CodeMismatch` |
| `notifications::{notification, notifications, active_notifications, unread_count}` | 按去重键读 / 全部（写入序）/ 未恢复的 / 未读且未恢复的条数 |
| `notifications::{mark_read, resolve}` | 标记已读 / 标记已恢复；均返回是否命中 |
| `notifications::NotificationError` | `Db` / `InvalidDedupeKey{reason}` / `CodeMismatch{dedupe_key,existing}` / `MalformedRow{column}` |

### 表与不变量

- **事件与游标同事务，应用游标单独**：`received_changes`、`remote_entries`、`pending_ops` 与 `bindings.received_seq` 在同一 `BEGIN IMMEDIATE` 事务写入，接收游标最后推进（design §9.3、I09）；文件落盘后才通过 `mark_batch_applied` 推进 `applied_seq`，且只越过“连续已应用前缀”。任一步失败由调用方回滚，游标不会领先于批次与任务。
- **一个 RootSeq = 一个不可拆批次**：`received_changes` 主键 `(binding_id, seq)`，`seq > 0`（0 只是“还没有批次”的游标值）；`events_json` 与 `tx_id` 存服务端批次原形，`CHECK (json_valid(...) AND json_type(...) = 'array')` 兜住非 JSON 数组。
- **重复批次幂等、冲突批次报错**：同 seq 同 tx 同事件列表重复到达只返回已存记录（不再入队）；同 seq 不同内容 `ConflictingBatch`，seq 不大于游标 `StaleBatch`——不静默覆盖已收位置。
- **位图编码唯一**：`transfers.bitmap` 长度由块几何（`CHECK length = ceil(ceil(total/chunk)/8)`）约束，块数之后的 bit 必须为 0，写入前与读取时都检查；`(binding_id, version_id)` 主键，同一不可变版本只有一条进度，重启后按位图续传。
- **完成必须已校验且完整**：`CHECK (state <> 'completed' OR verified = 1)`，`complete_transfer` 还要求位图覆盖全部块；`failed` 必须带错误码；`cancelled` 是持久状态，终态只能由 `discard_transfer` 删除。
- **本地意图先于磁盘改动**：`local_intents` 在受控替换前写入，`intent_id` 是本地插入序（恢复顺序，不是 wire 身份），`UNIQUE (binding_id, target_path)` 保证同一路径同时只有一个期望；失败保留整条意图（路径、哈希、generation），完成才删行。
- **通知按“原因 + Root”去重**：`dedupe_key` 主键；重复上报只加 `occurrences` 且不重置已读，`resolved` 后复发重新成为未读的 `active`（design §18.2）；`binding_id` 可为 `NULL`（全局问题），绑定解绑时级联删除。
- **未做**：`received_changes` 没有保留期/GC（design §9.4 的事件保留期属拉取卡），`transfers` 只记入站内容（上传进度以服务端 `upload_parts` 为准），`local_intents` 不执行文件系统动作，通知不决定何时弹窗。

验证命令（devShell 内）：`cargo test --locked -p vaultsyncd-client-store`、`cargo clippy --locked -p vaultsyncd-client-store --all-targets -- -D warnings`、`cargo fmt --check`、`RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps -p vaultsyncd-client-store`、`sqlite3 :memory: < crates/client-store/migrations/0002_events_transfers_intents.sql`。

## crates/server-core — 不可变内容对象与对象端口（S033）

状态：S033 实现 `objects/` 与 `ports/`；后续卡的提交意图（S034）、CAS 复核（S035）、固定版本下载（S057）与 GC（S085/S086）在此之上编排。包名 `vaultsyncd-server-core`，无第三方依赖，属于 `members` 与 `default-members`（便携规则检查）。

### 端口与模块

| 项目 | 契约 |
| --- | --- |
| `ports::object_store::ObjectBlobPort` | 对象字节层端口，由平台适配器实现：`stage_working_copy(source, staging)`、`publish_staged(staged, object)`、`discard_staged(staging)`、`verify_object(object)`、`read_object(object)`。核心只依赖这些签名，不做平台 I/O（design §5.5） |
| 端口报告的实测事实 | `StagedBlob { staging, source: FileKey, staged: FileKey, digest, method, fallback }`、`PublishedBlob { object, already_present }`、`StoredFacts { bytes, digest }`、`StoredBytes { data, bytes, digest }`。`FileKey { device, inode, links, bytes }` 是“同 inode/有别名”的唯一判据；`CopyMethod = reflink \| verified_copy`，`CopyFallback = exdev \| unsupported` 必须如实暴露 |
| 端口错误 | `BlobError::{SourceUnreadable, Storage{stage, failure, detail}, Verification, ObjectMissing, InvalidRequest}`；`StorageFailure::{NoSpace, QuotaExceeded, Io, PermissionDenied, ReadOnly, Other}`，`code()` 给出稳定短名（`no_space` 等）。`EIO`/`ENOSPC`/权限错误不得报成“不支持 reflink”或成功（design §11.5） |
| `objects::ContentDigest` | SHA-256 原始 32 字节；`from_hex` 只接受 64 位小写十六进制，`to_hex`/`fanout`（前 2 位）与 `objects.hash` 同形 |
| `objects::ObjectLayout` | 对象区布局：暂存 `<root>/staging/<名字>`，已发布对象 `<root>/objects/<aa>/<64 位 hex>`；`relative_object_path` 是写进 `objects.storage_path` 的规范相对路径。暂存名必须是单个文件名（空、`.`/`..`、含 `/`/`\`/NUL 一律拒绝），核心在调用端口前就失败 |
| `objects::capture_object(port, layout, CaptureRequest{source, staging_name, ExpectedContent{digest, bytes}})` | 顺序固定：建立私有副本 → 判据检查 → 按内容地址发布（不覆盖）→ 发布后复核。成功返回 `CapturedObject{digest, bytes, storage_path, method, fallback, reuse, staging}`，`ReuseOutcome = freshly_published \| existing_verified` |
| 采集判据（顺序即错误优先级） | 1) 副本与源不同 inode（否则 `shared_writable_inode`）；2) 副本无额外硬链接（`staging_alias`）；3) 长度等于期望（`length_mismatch`）；4) SHA-256 等于期望（`digest_mismatch`）；5) 内容地址已有对象时复核既有内容（`existing_object_mismatch`）；6) 发布后复核（`post_publish_mismatch`）。任一不成立都不返回对象 |
| 失败模型 | `CaptureFailure { reason: CaptureFailureReason, staging: DiscardOutcome }`，`staging ∈ {not_needed, discarded, failed}`；`is_committable()` 恒为 `false`。端口报错时核心不重复丢弃（端口契约要求自己清理未验证的暂存文件）；发布失败与复用路径由核心丢弃并报告结果 |
| `objects::read_object(port, layout, digest, expected_bytes)` | 按内容地址读取并核对元数据长度与 SHA-256，返回 `ObjectBytes{digest, data}`；失败为 `ReadFailure`（`invalid_digest`/`object_missing`/`length_mismatch`/`digest_mismatch`），不把截断或改写后的字节当成这个版本（I12） |
| 依赖边界 | `server-core` 不依赖 `platform-linux-server`；Linux 适配层库也不依赖 `server-core`，只有入口（当前是 `crates/platform-linux-server/examples/object-capture.rs`，S033 的本地组装 wrapper）用 `dev-dependencies` 把 `files::clone`/`files::restricted` 包装成端口 |

### Linux 适配与验收入口（S033）

| 项目 | 契约 |
| --- | --- |
| `examples/object-capture.rs` | 子命令 `capture --objects <dir> --source <file> --staging <name> --digest <hex> --bytes <n>` 与 `read --objects <dir> --digest <hex> --bytes <n>`；输出稳定 `key=value` 行（`decision=`、`digest=`、`method=`、`fallback=`、`reuse=`、`staging_cleanup=`、`code=`、`observed=`），成功退出 0、被拒绝 1、用法错误 2 |
| 适配器行为 | 暂存与发布都在对象区（`<objects>/staging` → `<objects>/objects/<aa>/<hex>`），发布用 `RENAME_NOREPLACE` 且不覆盖既有对象、成功后刷目录；对象权限 `0444`；`FICLONE` 优先，`EXDEV`/不支持退回 `stage_copy` 的校验复制并暴露原因 |
| 验收脚本 | `VAULTSYNCD_RUN_ID=s033-… bash tests/platform/linux/run-objects.sh`：同卷 reflink 采集、改写/截断/删除工作文件后对象不变、错误摘要与长度不产生对象、跨卷 `exdev` 校验复制、小 tmpfs 满盘 `no_space`、缺失对象明确失败；结束清理样本目录并核对 |

## crates/server-core — 提交意图状态机与恢复判定（S034）

状态：S034 实现 `commit/`、`recovery/` 与 `ports::{intent_log, target}`；完整内容 CAS（S035）、幂等/冲突（S036）、create/move/delete（S038/S039）与全 Root 启动恢复（S037）在其上扩展。包名 `vaultsyncd-server-core`，依赖只有 `vaultsyncd-domain`（身份与时间戳的唯一来源），仍属 `members`/`default-members`。

### 端口与模块

| 项目 | 契约 |
| --- | --- |
| `ports::target::TargetPort` | Root 工作目录字节层端口，由平台适配器实现：`observe(target_path) -> TargetObservation`、`publish(&PublishRequest) -> PublishedTarget`。`TargetObservation::{Absent, Present{identity: FileKey, digest}}` 把“确认不存在”与“读不到”分开：读不到（权限/挂载/IO）必须报 `TargetError::Unreadable`，不得返回 `Absent`。`PublishRequest{target_path, staging_path, digest, size}` 要求先建目标子卷内暂存文件再受控替换，失败必须清理自己的暂存文件 |
| `ports::target::{TargetError, PublishedTarget}` | `TargetError::{Unreadable{detail}, Storage{stage, failure: StorageFailure, detail}, InvalidRequest{detail}}`，`code()` 给稳定短名（`target_unreadable`、`no_space` 等）；`PublishedTarget{identity, digest}` 是发布后实测事实，短写不得报成功 |
| `ports::intent_log::IntentLog` | 异步意图日志端口（`async fn`，泛型使用、不做 dyn 对象）：`begin_put(&NewPutIntent, at) -> PutIntent`、`advance_put(id, PutPhase, at) -> PutIntent`、`load_put(id) -> Option<PutIntent>`、`unfinished_puts(root) -> Vec<PutIntent>`（写入顺序，恢复入口）。实现方是服务端存储层的 `fs_intents` 适配（S035 落地），越权必须报错而不是返回空列表 |
| `ports::intent_log::IntentLogError` | `Storage{detail}` / `OutOfScope` / `UnknownIntent{id}` / `DuplicateIntent{id}` / `IllegalTransition{from,to}` / `Rejected{code,detail}` / `MalformedIntent{detail}`，`code()` 给稳定短名 |
| `commit::intent::PutPhase` | `Prepared` / `Published` / `Finalized` / `NeedsRecovery`，文本与 `fs_intents.phase` 同一套取值；`allows_transition_to` 只允许 `prepared→published`、`published→finalized`、两个开放阶段→`needs_recovery`、`needs_recovery→finalized` |
| `commit::intent::{TargetBefore, TargetAfter}` | 意图前提与目标：`TargetBefore::{Absent, Present(TargetSnapshot{entry, revision, digest, identity: FileKey})}`、`TargetAfter{digest, size}`。`to_json()` 是写库的唯一编码，`from_json()` 是恢复的唯一解码；键顺序固定，计数与字节一律十进制**字符串**（不用 JSON number），未知键、缺字段、类型不符、`null`、转义与非 object 顶层一律 `StateJsonError`（`state_not_object`/`state_unknown_field`/`state_missing_field`/`state_type_mismatch`/`state_invalid_value`） |
| `commit::intent::{NewPutIntent, PutIntent}` | 意图身份、Root、`target_path`（Root 内相对路径）、`staging_path`（单文件名，核心在 I/O 前用 `ObjectLayout::validate_staging_name` 校验）、before/after；`PutIntent` 另带 `phase`，`NewPutIntent::prepared()` 给出刚写库的形态 |
| `commit::intent::publish_put(log, target, at, &NewPutIntent)` | 顺序固定：现场复核（前提不符 → `before_mismatch`，不写意图不动磁盘）→ 写 prepared 意图（失败不发布）→ 发布工作文件 → 发布后复核实测摘要与字节数 → 记 `published`。返回 `PublishedPut{intent, target}` |
| `commit::intent::finalize_put(log, id, at)` | 把已发布意图推进到 `finalized`；S035 在写 entries/versions/changes 的同一事务里调用，未发布就收尾被阶段规则拒绝（`illegal_intent_transition`） |
| `commit::intent::PutFailure` | `reason: PutFailureReason`、`intent_id`、`phase`；`is_committed()` 恒为 `false`，已写入的意图行不删除也不回退。`PutFailureReason::{InvalidRequest, TargetUnreadable, BeforeMismatch{expected, observed}, Intent(IntentLogError), Publish(TargetError), PublishedTargetMismatch{expected, observed}}`，`code()` 给稳定短名；编排函数返回 `Box<PutFailure>` |
| `recovery::classify_put(intent, observation)` | 按持久阶段与现场给出 `RecoveryDecision::{AlreadyFinished, Redo, Complete, Reconcile{reason}, NeedsRecovery{reason}}`：`prepared`+满足前提 → `redo`；现场已是目标内容 → `complete`；第三种现场 → `prepared_target_changed`/`published_target_changed`/`published_target_reverted`（保留双方、等协调）；目标不可读或意图已标 `needs_recovery` → `needs_recovery`（`pauses_writes()` 为真） |
| `recovery::plan_root(intents, observe)` | 按意图写入顺序产出 `RootRecovery{intents: Vec<ClassifiedIntent{intent_id, target_path, phase, decision}>, pauses_writes}`；任一意图拿不到可证明现场即 `pauses_writes = true`（写入必须保持关闭） |

### 表与不变量

- **先持久意图再动磁盘**：`publish_put` 的任何写文件动作都在 `begin_put` 成功之后；意图写不进去就没有发布。
- **阶段推进必须有证据**：发布后实测摘要/长度不等于 `desired_after` 时不记 `published`；元数据事务未提交不得 `finalized`。
- **错误不是 committed**：所有失败返回 `PutFailure`，`is_committed()` 恒 `false`，且不删除已持久化的意图行（它是恢复证据）。
- **故障不是删除**：目标不可读只走向 `needs_recovery` 与暂停写入，不解释为“目标不存在”后重做。
- **证据自描述**：`expected_before`/`desired_after` 的编码与解码在核心一处维护；存储层的 `fs_intents.expected_before/desired_after` 只用这段文本，不另造一套形态（真实 SQLite 迁移与 CHECK 约束下的往返见 `crates/server-store/src/intents.rs` 的 S034 测试）。

## crates/server-core — 内容 CAS 与现场复核（S035）

状态：S035 在 S034 的意图状态机上实现单文件内容 put 的完整 CAS 与 ACK 边界（`commit::put`、`ports::put_store`）。幂等/冲突（S036）、全 Root 恢复（S037）、create/move/delete（S038/S039）仍在其上扩展；服务端存储层的真实适配（SQLite 事务 + `fs_intents` 收尾）尚未落地，见本节末的“未接入”项。

### 端口与模块

| 项目 | 契约 |
| --- | --- |
| `commit::put::RootCommitGate` | 进程内 Root 提交串行化：`acquire(root) -> RootCommitTicket`、`release(ticket)`、`is_held(root)`。同一 Root 同时只允许一次提交，第二个得到 `root_commit_busy`（不排队、不阻塞）；不同 Root 互不影响。票据只能由 `acquire` 产生，释放不失败（不匹配即编程错误）。只覆盖单进程，跨进程共享数据库的串行化属服务端组装卡 |
| `commit::put::PutRequest` | `intent_id`、`root`、`entry`、`expected_epoch`、`expected_revision`、`expected_parent`、`expected_name`、`target_path`、`staging_path`、`content: ExpectedContent`、`before: TargetBefore`。形状校验在任何端口调用之前：暂存名必须是单个文件名（I10），`target_path` 必须是 Root 内相对路径且最后一段等于 `expected_name`，`before` 的条目/revision 必须与请求基线一致 |
| `commit::put::commit_put(gate, store, log, target, at, &PutRequest)` | 顺序固定：请求形状 → Root 票据 → epoch → 命名空间基线 → 对象持久 → 现场指纹 → `publish_put`（意图 → 发布 → 记 published）→ `apply_put`（元数据/changes/结果事务）→ 核对事务证据 → ACK。任何出口都会释放 Root 票据。成功返回 `CommittedPut{record, target}`，`record.phase` 必须是 `finalized` |
| `commit::put::CommittedPut` | `record: PutRecord` + `target: PublishedTarget`；`intent_id()` 给出已收尾的意图。只有元数据事务提交后才存在（I01：发布完成不等于 committed） |
| `ports::put_store::PutCommitStore` | 存储端口（异步，泛型使用）：`root_facts(root) -> RootFacts{epoch}`、`entry_baseline(root, entry) -> EntryBaseline{entry, root, parent, name, revision, kind}`、`object_state(root, digest) -> ObjectState::{Persisted{bytes}, Missing}`、`apply_put(&PutWrite) -> PutRecord`。`apply_put` 必须在一个事务里做：比较并写 `expected_revision`、写 `versions`/`entries.revision`/当前版本、追加 `changes` 批次、写操作结果，并把 `fs_intents` 从 `published` 收尾到 `finalized`；意图不是 `published` 时返回 `IllegalIntentTransition` |
| `ports::put_store::PutRecord` | `intent_id`、`phase`、`version`、`revision`、`seq: RootSeq`、`tx: TxId`、`digest`、`size`；`phase` 必须是 `finalized`、`digest`/`size` 等于请求内容、`revision` 严格大于基线，否则核心按 `store_contract_violation` 停写（不返回 ACK） |
| `ports::put_store::PutStoreError` | `Storage{detail}` / `OutOfScope` / `UnknownRoot{root}` / `EntryMissing{entry}` / `RevisionChanged{expected, actual}` / `IllegalIntentTransition{from, to}` / `Rejected{code, detail}` / `MalformedIntent{detail}`，`code()` 给稳定短名（`revision_changed` 等） |
| `PutFailureReason` 新增原因与 wire 映射 | `EpochChanged → epoch_changed / EPOCH_CHANGED`、`BaseMismatch → base_mismatch / BASE_MISMATCH`（过期 revision）、`SubtreeChanged → subtree_changed / SUBTREE_CHANGED`（父目录/名称变化）、`Reconcile → reconcile_required / RECONCILE_REQUIRED`（现场指纹不符）、`UnsupportedEntry → unsupported_entry / UNSUPPORTED_ENTRY`、`ObjectNotPersisted`/`ObjectSizeMismatch`（无 wire 码）、`Gate`（`root_commit_busy`）、`Store`（透传存储码）、`StoreContract → store_contract_violation / NEEDS_RECOVERY`。`PutFailure::error_code() -> Option<ErrorCode>` 是 API 层唯一的 wire 映射，不在上层重写字符串 |

### 不变量

- **过期基线不覆盖**：epoch、revision、父目录、名称、现场指纹五项都在动磁盘之前核对；前四项分别得到 `EPOCH_CHANGED`/`BASE_MISMATCH`/`SUBTREE_CHANGED`，现场不符得到 `RECONCILE_REQUIRED`（design §11.2 步骤 4、I04）。
- **对象先于意图**：内容地址上没有已登记对象（或登记长度不符）时一个字节都不发布，也不写意图（design §11.2 步骤 3）。
- **失败永远不是 committed**：`PutFailure::is_committed()` 恒为 `false`；发布失败停在 `prepared`，事务失败停在 `published`，两者都不删除意图行（恢复证据）。
- **现场复核两次不宣称消除竞态**：CAS 预检查与 `publish_put` 的发布前复核各观察一次现场；两者之间仍可能被外部写入，按 design §12.2 不承诺保留每一次非协作瞬态写入，只保证“发现不符就拒绝并保留候选版本”。
- **证据不一致就停写**：存储层声称事务成功但返回的证据（意图阶段、内容摘要/长度、revision 推进）与请求不符，按 `store_contract_violation`（`NEEDS_RECOVERY`）处理，不把适配器缺陷当成成功。

### 未接入（后续卡）

- `PutCommitStore` 尚无真实实现：`root_facts`/`entry_baseline`/`object_state`/`apply_put` 要接到 `crates/server-store` 的 `roots`/`entries`/`versions`/`changes`/`operations`/`intents`，并把 `IntentLog` 的真实 `fs_intents` 适配一并落地；在此之前 `apps/server` 的提交入口没有可组装的存储实现。

## crates/server-core — 冲突内容提交与 `OpId` 幂等（S036）

状态：S036 在 S035 的意图/发布顺序上实现 design §10.1—10.2、T02/T03、I01/I06 的服务端部分（`commit::conflict`、`ports::conflict_store`）：过期基线上的不同内容成为一个稳定身份的冲突条目与一条映射，同一 `OpId` 的重试返回原结果。真实存储适配与恢复入口仍未落地（S037 与组装卡）。

| 项目 | 契约 |
| --- | --- |
| `commit::conflict::ConflictRequest` | `intent_id`、`op: OperationKey{root, device, op}`、`digest: RequestDigest`、`entry`（客户端想覆盖的 head 条目）、`expected_epoch`、`expected_revision`（客户端的过期基线）、`target_path`（§10.2 冲突路径，Root 内相对）、`staging_path`（单文件名）、`content: ExpectedContent`。形状在任何端口调用前检查 |
| `commit::conflict::commit_conflict(gate, store, log, target, ids, at, &ConflictRequest)` | 顺序固定：请求形状 → Root 票据 → `conflict_head`（epoch + 源条目现状）→ epoch → `operation` 记录（重放 / `OP_ID_REUSED` / pending 不重做）→ “过期 + 不同内容”判定 → 目标空缺预检 → `begin_conflict`（登记 pending）→ `publish_put`（意图 → 发布 → `published`）→ `apply_conflict`（条目/版本/映射/事件/批次结果/收尾）→ 核对证据 → ACK。任何出口都释放 Root 票据 |
| `commit::conflict::CommittedConflict` | `op: OpId`、`source`、`sibling`（服务器为该 `OpId` 分配的冲突条目）、`path`、`content: ExpectedContent`、`revision`、`seq`。首次提交与重放返回同一份；它也是 `operations` 里该 `OpId` 的终态结果 |
| `ports::conflict_store::OperationKey` / `RequestDigest` | `OperationKey{root, device, op}` 是 design §7.2 的幂等键；`RequestDigest` 是 32 字节规范化请求摘要，核心只比较不重算——规范化编码只有一处实现（`vaultsyncd_server_store::operations::request_digest`），摘要必须覆盖源条目、基线 revision、冲突路径与内容摘要/长度 |
| `ports::conflict_store::ConflictCommitStore` | 异步存储端口（泛型使用）：`conflict_head(root, source) -> ConflictHead{epoch, source: Option<HeadSource{entry, revision, kind, content}>}`、`operation(key) -> Option<RecordedOperation{digest, result}>`、`begin_conflict(key, digest) -> ConflictBegin::{Started, Recorded}`（原子插入或读 `pending`，已存在时绝不改写摘要或结果）、`apply_conflict(&ConflictWrite) -> ConflictRecord{intent_id, phase, conflict, tx}`。`apply_conflict` 必须在同一事务里：创建 `sibling` 条目（挂在 `source` 的父目录下、名称取 `path` 最后一段）、写版本（内容等于 `content`）、写 `conflicts` 映射与 `operations` 终态结果、追加 `changes` 批次、把 `fs_intents` 从 `published` 收尾到 `finalized`；路径已被占用返回 `PathOccupied` |
| `ports::conflict_store::ConflictStoreError` | `Storage{detail}` / `OutOfScope` / `UnknownRoot{root}` / `PathOccupied{path}` / `DuplicateOperation{op}` / `IllegalIntentTransition{from,to}` / `Rejected{code,detail}` / `MalformedRow{detail}`，`code()` 给稳定短名（`target_path_occupied`、`malformed_conflict_row` 等） |
| `PutFailureReason` 新增原因与 wire 映射 | `OpIdReused → op_id_reused / OP_ID_REUSED`（同一 `OpId` 不同请求）、`OperationPending → operation_pending / NEEDS_RECOVERY`（pending 不重做）、`ConflictNotStale → conflict_not_stale / RECONCILE_REQUIRED`（head revision 仍是基线）、`ContentAlreadyCommitted → content_already_committed / RECONCILE_REQUIRED`（同结果只归并）、`ConflictStore`（透传存储码） |

不变量：

- **同一个 `OpId` 只有一个结果**：`operation` 记录先于“过期/不同内容”判断（重放时 head 必然已前进），摘要不同即 `OP_ID_REUSED`，摘要相同但记录的源条目/路径/内容不同按 `store_contract_violation`（`NEEDS_RECOVERY`）停写核对。
- **过期且不同内容才造副本**：head revision 仍是请求基线 → `conflict_not_stale`；head 内容已等于请求内容 → `content_already_committed`（§10.1 第二行不额外造副本）。两者都让客户端重新拉事实再规划。
- **冲突也是受恢复保护的提交**：冲突条目同样走意图 → 发布 → `published` → 元数据事务；发布失败停在 `prepared`、事务失败停在 `published`，意图不删除，`recovery::classify_put` 可直接分类。
- **pending 不是 committed**：`begin_conflict` 在动磁盘前登记；重试看到 `pending` 返回 `operation_pending`，不重做也不新建第二份副本。目标被占用这类可重新规划的拒绝在登记之前发生，不把 `OpId` 锁死。
- **名称不是身份**：`target_path` 由 `planner::conflict::conflict_entry_path` 生成（§10.2 只有一处实现），核心只校验形状；副本身份是 `sibling`（服务器分配）并与结果同事务持久化，重试不改名、不换 id。

未接入（后续卡）：`ConflictCommitStore` 同样只有测试假实现；真实实现要接 `crates/server-store` 的 `entries`/`versions`/`changes`/`operations`/`conflicts` 与 `fs_intents` 收尾，`conflicts` 表本身也尚未建（S028 只落了 `operations`）。崩溃后 `pending` 记录的收尾依赖 S037 的全 Root 恢复；跨进程并发仍靠数据库唯一约束与 Root 票据（同 S035 限制）。


## crates/server-core — 启动恢复与写入门（S037）

状态：S037 在 S034 的判定之上实现 design §11.3、I13、T23 的启动恢复执行与“先恢复再开放写入口”，并新建 design §5.8 的集成测试包 `tests/server-integration/`（`vaultsyncd-server-tests`）。真实 SQLite/文件端口适配仍未落地。

| 项目 | 契约 |
| --- | --- |
| `recovery::startup::recover_root(log, target, store, at, root) -> Result<RecoveryReport, RecoveryError>` | 按 `IntentLog::unfinished_puts` 的写入顺序处理每条未完成 put 意图：现场 == 前 hash → 重做发布 + 补元数据事务；现场 == 后 hash（阶段 `prepared`/`published`）→ 先按现场证据补记 `published`（若还停在 `prepared`），再补元数据事务；现场是第三种内容 → 保留双方、不覆盖不提交；目标不可读/已标 `needs_recovery` → 落盘 `needs_recovery` 并停写；恢复动作失败 → 不假装完成，Root 保持关闭。`RecoveryError::Intent(IntentLogError)` 表示读不出未完成清单（越权/存储失败），调用方必须保持关闭 |
| `recovery::startup::RecoveryReport` | `root`、`intents: Vec<RecoveredIntent{intent: PutIntent, decision: RecoveryDecision, outcome: RecoveryOutcome}>`；`pauses_writes()`（任一 `NeedsRecovery`/`Unresolved`）、`needs_reconciliation()`（任一 `Preserved`）、`committed_versions()`、`outcome_for(id)`。报告是恢复输出与候选版本证据（前 hash 在 `intent.before`，后 hash 在 `intent.after`） |
| `recovery::startup::RecoveryOutcome` | `Redone{version, published}` / `Completed{version}` / `Preserved{reason: ReconcileReason}` / `NeedsRecovery{reason: NeedsRecoveryReason}` / `Unresolved{code, detail}`；`is_committed()` 只对前两者为真，`version()`、`pauses_writes()`、`as_str()`（稳定短名） |
| `recovery::startup::WriteAdmission` / `RootAdmission` / `AdmissionState` / `WriteRefusal` | 逐 Root 写入门：新建时所有 Root 为 `AwaitingRecovery`（默认拒绝），只有 `record(RecoveryReport)` 能推进到 `Open` 或 `Paused(report)`，没有直接打开的入口。`admit_write(root) -> Result<(), WriteRefusal>`，拒绝码 `recovery_pending`（恢复未完成）与 `needs_recovery`（恢复暂停）；`state(root)` 给 readiness 用；`report(root)` 给诊断/API 输出暂停原因 |
| `tests/server-integration/`（包 `vaultsyncd-server-tests`） | `src/lib.rs` 是文件型假端口与边界注入装置（`CrashStore`/`StoreHandle`/`Boundary`/`run_commit`/`run_recovery`/`start_with_recovery`），`src/bin/crash-child.rs` 是被强杀的子进程，`tests/recovery.rs` 是验收矩阵。不进 `default-members`，用 `cargo test --locked -p vaultsyncd-server-tests` 执行 |

不变量：

- **先恢复再开放写入**：提交入口必须先 `WriteAdmission::admit_write`；默认状态是拒绝，报告 `pauses_writes()` 为真时永远只能是 `Paused`。
- **只重放可证明的操作**：恢复只用意图里的 `expected_before`/`desired_after` 与现场实测比较；元数据事务只从 `expected_before` 的快照取条目身份与基线 revision，不猜身份。
- **事务声称成功也要核对证据**：`apply_put` 返回的 `phase` 不是 `finalized`、内容不等于 `desired_after` 或 revision 没推进，都按 `store_contract_violation` 停写。
- **保留旧对象与冲突候选**：`Preserved`/`NeedsRecovery` 不删除对象、不改工作文件；`Redone`/`Completed` 只新增一个版本，旧内容对象仍被旧版本引用。

未接入（后续卡）：create/move/delete 的意图日志（S038/S039）要接进同一条启动顺序；`expected_before` 为 `Absent` 的意图（新建条目）当前返回 `Unresolved{code: "missing_entry_baseline"}` 并保持 Root 关闭，因为意图里没有条目身份可收尾；真实存储适配仍待落地（`apps/server` 的 readiness 组装已由 S041 落地，见下一节）；装置里的摘要不是 SHA-256（假端口），仅用于区分前后内容。

## crates/server-core — 目录创建与同 Root 移动（S038）

状态：S038 把目录创建与同 Root 移动接进 put 已有的“先持久意图再动磁盘”顺序（design §9.5、§11.2—11.3、T09/T10）。移动只改父关系与名称：`EntryId` 不变、不写版本行、不改内容引用、内容流量为零。真实 SQLite/文件端口适配与提交入口在进程内的装配仍未落地（S041 落的是进程生命周期与配置）。

| 项目 | 契约 |
| --- | --- |
| `commit::namespace::commit_namespace(gate, store, log, ns, at, &NamespaceRequest) -> Result<CommittedNamespace, Box<NamespaceFailure>>` | 顺序：请求形状（名称/路径/目录环/自指）→ Root 串行化 → epoch → 条目基线（新建要求身份不存在；移动要求 revision/父/名一致）→ 目标父目录存在且是目录 → 源与目标现场 → 意图 `prepared` → `mkdir`/`rename` 并复核 → `published` → 元数据事务 → ACK。失败都不是 committed，已持久化意图保留为恢复证据 |
| `NamespaceRequest` | `CreateDirectory(CreateDirectoryRequest{intent_id, root, entry, expected_epoch, parent, name, target_path})` 或 `Move(MoveRequest{intent_id, root, entry, expected_epoch, expected_revision, from: NamespaceEndpoint, to: NamespaceEndpoint})`；`NamespaceEndpoint{parent, name, path}` 的路径最后一段必须等于名称 |
| `CommittedNamespace` | `record: NamespaceRecord`（`phase` 必须是 `finalized`、`entry` 等于意图条目、新建 revision 0、移动严格大于基线）与 `target: PathState`（提交后目标实测现场） |
| `NamespaceFailure` / `NamespaceFailureReason` | 稳定码：`invalid_request`、`root_commit_busy`、`epoch_changed`、`base_mismatch`、`subtree_changed`、`entry_already_exists`、`target_unreadable`、`target_occupied`、`target_ancestor_missing`、`source_changed`、意图/动作错误码、`applied_state_mismatch`、存储错误码、`store_contract_violation`。wire 映射：`EPOCH_CHANGED`、`BASE_MISMATCH`、`SUBTREE_CHANGED`（占用/祖先缺失/基线变化）、`RECONCILE_REQUIRED`（源与基线不符）、`NEEDS_RECOVERY`（存储契约违约） |
| `commit::namespace::NamespaceBefore` / `NamespaceAfter` | `fs_intents` 的 `expected_before`/`desired_after`。before：`{"present":false}`（新建）或 `{"present":true,"entry","revision","parent","path_hex","kind","size","device","inode","links"}`；after：`{"present":true,"entry","parent","path_hex","kind"}`。路径按 UTF-8 十六进制编码（同一编码器不接受转义序列），移动的 before 记录源 inode 作为“移动已落地”的证据 |
| `commit::namespace::classify_namespace(&NamespaceIntent, &ObservedNamespace) -> NamespaceRecoveryDecision` | `ObservedNamespace{target, source}` 原样携带观察错误。判定：现场满足前提 → `Redo`；现场已是目标（移动还要求目标 inode 与 before 相同、源已空）→ `Complete`；目标被占/祖先缺失/源与目标都不见了/`published` 却目标回退 → `Reconcile`；读不到或已标 `needs_recovery` → `NeedsRecovery` |
| `commit::namespace::recover_namespace_root(log, ns, store, at, root) -> Result<NamespaceRecoveryReport, NamespaceRecoveryError>` | 按 `NamespaceIntentLog::unfinished_namespaces` 顺序处理：`Redo` 重做动作并补事务、`Complete` 只补事务（阶段还停在 `prepared` 时先补记 `published`）、`Reconcile` → `Preserved`（不覆盖不提交）、`NeedsRecovery` → 落盘 `needs_recovery`、动作失败 → `Unresolved` 保持关闭。报告提供 `pauses_writes()`、`needs_reconciliation()`、`committed()` |
| `ports::namespace::NamespacePort` | `inspect(path) -> PathState{Absent\|AncestorMissing\|File{identity}\|Directory{identity}}`、`create_directory(path)`、`rename(from, to)`。契约：不存在只在父目录可访问且确认没有该名字时报告；读不到是 `Unreadable`（不是“不存在”）；`rename` 必须是同一次改名（inode 不变），不得退化成复制再删除；目标已存在时动作必须失败，不覆盖 |
| `ports::namespace_store::NamespaceCommitStore` | `root_facts`、`entry_baseline`（复用 `RootFacts`/`EntryBaseline`/`EntryKind`）、`apply_namespace(&NamespaceWrite) -> NamespaceRecord`。事务契约：移动只改 `parent_id`/`name`/`name_key`/`revision`，不写版本行、不改 `content_version_id`；新建只插入 `kind = directory` 且无内容版本的条目（revision 0）；比较并写 `expected_revision`，目标同父活跃名称被占返回 `SlotOccupied`；`published → finalized` 与 `entries`/`changes` 同一事务 |
| `ports::intent_log::NamespaceIntentLog` | `begin_namespace`、`advance_namespace`、`load_namespace`、`unfinished_namespaces`。与 `IntentLog` 共用 `fs_intents` 表与 `PutPhase` 阶段机（`kind = 'create'`/`'move'`、`staging_path` NULL、`target_path` 是目标路径） |

不变量：

- **`EntryId` 不随路径变**：所有路径都拿基线身份写事务；移动后的 `NamespaceRecord.entry` 必须等于意图条目，否则按 `store_contract_violation` 停写。
- **移动不搬内容**：命名空间端口只有观察/`mkdir`/`rename`，没有内容方法；事务输入也没有内容字段。移动后内容版本引用必须与移动前一致（假端口测试断言）。
- **不覆盖目标**：目标被占、祖先缺失、目标落在自己子树里（目录环）、目标父目录就是自己，一律在动磁盘之前拒绝，不动目标也不动源。
- **动作要能复核**：`rename` 之后必须证明目标 inode 就是源那一个且源位置已空；`mkdir` 之后必须是目录。不符就不推进阶段，意图停在 `prepared`。
- **恢复先于写入开放**：装配时必须先跑 `recover_namespace_root` 与 put 的 `recover_root`，两者的报告都不暂停，才记录 `WriteAdmission::record` 开放写入口。

未接入（后续卡）：真实端口适配（`NamespacePort` 的 Linux 实现、`NamespaceIntentLog`/`NamespaceCommitStore` 的 SQLx 实现）与提交入口的进程内装配；delete 的意图与回收站（S039）；`tests/server-integration/` 的强杀矩阵只覆盖 put 边界，命名空间边界目前由 `crates/server-core` 的假端口用例覆盖；目标祖先的逐级存在性由适配器报告，核心只核对目标父目录条目与端口现场。

## crates/server-core + crates/server-store — 可恢复删除与回收站（S039）

状态：S039 把删除接进 put/命名空间同一套持久意图阶段机（design §10.3、T11/T12）。核心给编排与判定，存储层给回收站清单/墓碑表与有界批次软删除；真实 `NamespacePort`/`RemovalPort`/`DeleteIntentLog`/`DeleteStore` 的 SQLx 与 Linux 适配仍未落地（随 S041 之后的装配），本卡证明的是端口契约 + 编排 + 表约束。

| 项目 | 契约 |
| --- | --- |
| `commit::delete::commit_delete(gate, store, log, ns, at, &DeleteRequest) -> Result<CommittedDelete, Box<DeleteFailure>>` | 顺序：请求形状（路径）→ Root 串行化 → epoch → 条目基线 revision → 子树忙 → 现场种类 → 冻结清单与 token → 意图 `prepared` → 有界批次移除并复核 → `published` → 单一事务公布 → ACK。`ns` 同时实现 `NamespacePort`（观察）与 `RemovalPort`（移除） |
| `DeleteRequest` | `{intent_id, root, entry, expected_epoch, expected_revision, subtree_token: Option<SubtreeToken>, path}`。目录删除必须带服务端 token，文件删除必须不带（混用即 `invalid_request`） |
| `CommittedDelete` | `record: DeleteRecord`（`phase` 必须 `finalized`、`trashed` 等于清单长度）、`target`（必须是 `Absent`）、`removed`（工作目录删掉的目录项数）、`batches` |
| `SubtreeToken` | 服务端按冻结清单算出的 128 位摘要（FNV-1a），任一条目的身份/父/名/种类/revision/内容版本变化或成员增删都会换值。`to_hex()` 是 32 个小写十六进制字符；它是并发检测，不是安全凭据 |
| `SubtreeImage` / `SubtreeEntry` / `DeleteBatch` | `SubtreeImage::new(root, entries)` 要求非空、根条目恰好一条且带父目录、身份不重复、路径合法、非根成员的父目录在清单里。`token()`、`batches(limit)`（每批 ≤ `MAX_DELETE_BATCH_ENTRIES = 256`）、`find(entry)` |
| `SubtreeBusy` / `check_write(root, path)` | 子树忙来自**持久事实**：未收尾的删除意图。`SubtreeBusy::from_intents(&[DeleteIntent])` 后，命中 `path == 子树路径` 或其后代的写入返回 `SubtreeBusyConflict{entry, path}`。装配层应在 put/新建/移动/恢复之前调用它 |
| `DeleteFailure` / `DeleteFailureReason` | 稳定码：`invalid_request`、`root_commit_busy`、`epoch_changed`、`base_mismatch`、`subtree_changed`、`subtree_busy`、`target_unreadable`、移除错误码（`path_in_use`、`target_unreadable`、存储分类）、`removal_incomplete`、`manifest_mismatch`、意图/存储错误码、`store_contract_violation`。`is_committed()` 恒为 `false`。wire 映射：`EPOCH_CHANGED`、`BASE_MISMATCH`、`SUBTREE_CHANGED`（子树变化/忙/移除未完成）、`NEEDS_RECOVERY`（存储契约违约）；其余无 wire 码 |
| `DeleteSnapshot` / `DeleteAfter` | `fs_intents` 的 `expected_before`/`desired_after`（`kind = 'delete'`、`staging_path` NULL）。before：`{"present":true,"entry","revision","parent","path_hex","kind","size","device","inode","links","token","entries"}`；after：`{"present":false,"entries"}` |
| `classify_delete(&DeleteIntent, &ObservedDelete) -> DeleteRecoveryDecision` / `recover_delete_root(log, ns, store, at, root)` | 判定：`Finalized` → 无动作；`needs_recovery` → 停写；路径读不到 → `NeedsRecovery`；路径不存在 → `Complete`；现场还是前提那个条目（同种类同 inode）→ `Redo`；被换成别的条目 → `Reconcile{target_occupied}`；祖先缺失 → `Reconcile{target_unavailable}`。`Complete`/`Redo` 补事务前要求重读的清单 token 与条目数仍等于意图值，否则 `Unresolved("subtree_changed")` 停写；清单根已不存在 → `Preserved{subtree_already_deleted}` |
| `ports::removal::RemovalPort` | `remove_file(path)`、`remove_directory_batch(path, max_entries) -> RemovalProgress{removed, complete}`。契约：目录按批推进、可被打断后重复调用（已删完报 `finished(0)`）；失败用 `RemovalError{Unreadable\|InUse\|Storage\|InvalidRequest}`，不得报成成功 |
| `ports::trash_store::DeleteStore` | `root_facts`、`entry_baseline`、`subtree_image(root, entry) -> SubtreeImage`（目标自身 + 全部活跃后代，顺序稳定）、`apply_delete(&DeleteApply) -> DeleteRecord`。事务契约：按 `batches` 逐批软删除（每批 ≤ 256）、每条写回收站清单与墓碑、不写 `objects`/`versions`、不改 `content_version_id`；revision 比较并写失败 → `RevisionChanged`；清单与库不一致 → `ManifestMismatch`；`published → finalized` 与批次同一事务 |
| `ports::trash_store::TrashManifestEntry` | `{entry, parent, path, kind, revision, content_version, deleted_at, expires_at}`；目录 `content_version` 恒为 `None`，`expires_at = deleted_at + TRASH_RETENTION_SECONDS (30 天)` |
| `ports::intent_log::DeleteIntentLog` | `begin_delete`、`advance_delete`、`load_delete`、`unfinished_deletes`。与 put/命名空间共用 `fs_intents` 与 `PutPhase`（`kind = 'delete'`）。`unfinished_deletes` 是子树忙的事实来源 |
| `crates/server-store::trash` | `soft_delete_batch(tx, owner, root, &[SoftDelete{entry, revision}], at) -> usize`（有界、比较并写）、`record_trash_entry`、`insert_tombstone`、`trash_entries`、`trash_entry`、`tombstones`、`tombstone`；错误码 `out_of_scope`、`already_in_trash`、`duplicate_tombstone`、`invalid_batch`、`batch_mismatch`、`invalid_path`、`directory_with_content`、`invalid_retention`、`malformed_trash_row` |
| 迁移 `0005_trash.sql` | `trash_entries(entry_id PK, root_id, parent_id, path, kind, revision, content_version_id, deleted_at, expires_at)`（`(root_id, entry_id)` 外键指向 `entries`，目录不得带内容版本，`expires_at >= deleted_at`）；`tombstones(root_id, entry_id)` 主键 + 路径/种类/时间/保留期。schema 版本 5 |

不变量：

- **删除前提可证明**：目录删除带 token；并发新增/改名/删改使重算的 token 与请求不一致 → `subtree_changed`，不写意图、不动磁盘（T12）。
- **工作目录失败不能 ACK**：任一移除错误或“报告完成但路径还在”都停在 `prepared`，不写元数据事务；`is_committed()` 恒 `false`（design §10.3 第三条）。
- **逻辑批次一次公布**：回收站清单、墓碑、分批软删除、`changes` 事件批次与意图收尾在同一事务；大树不是一条语句，但对外只有“全有或全无”。
- **删除不动内容与历史**：事务不写 `objects`/`versions`、不改 `content_version_id`；删除后 `version_history` 与内容对象仍可读，回收站行保留原路径与内容版本供 30 天内恢复（S040）。
- **忙是持久状态**：未收尾的删除意图让该子树的其他写入得到 `subtree_busy`（wire `SUBTREE_CHANGED`），重启后仍然有效。

未接入（后续卡）：`RemovalPort` 的 Linux 实现、`DeleteIntentLog`/`DeleteStore` 的 SQLx 适配与提交入口装配；`SubtreeBusy::check_write` 在 put/新建/移动路径上的调用点；回收站恢复与历史恢复（S040）；回收站过期的内容回收与墓碑压缩（同步协议卡，design §10.3 末条）。强杀矩阵仍只覆盖 put 边界，删除的崩溃与中断由假端口的阶段/现场组合覆盖。

## apps/server — 服务端进程与配置生命周期（S041）

状态：S041 实现。包名 `vaultsyncd-server`（`apps/server/`），属于 workspace `members` 但**不在** `default-members`：非 Linux 目标直接 `compile_error!`，便携检查不构建它。依赖 `axum 0.8`（HTTP/1 + JSON）、`tokio`、`tracing`/`tracing-subscriber`、`serde`/`toml`，以及 `vaultsyncd-{domain,protocol,server-core,server-store}`；不含 FUSE/CFAPI/Windows 依赖（`cargo tree -p vaultsyncd-server` 无相关 crate）。示例配置在 `deploy/examples/vaultsyncd.toml`。

### 进程契约

| 项 | 契约 |
| --- | --- |
| 入口 | `vaultsyncd-server [--config <路径>]`；路径优先级 `--config` > 环境变量 `VAULTSYNCD_CONFIG` > `/etc/vaultsyncd/vaultsyncd.toml`。未知参数、`--config` 重复或缺值都是用法错误 |
| 退出码 | `0` 正常停止（SIGTERM/SIGINT 完成优雅停止）；`1` 启动/运行失败（数据库、恢复读不出结论、监听绑定）；`2` 用法或配置错误。常量在 `vaultsyncd_server::{EXIT_OK,EXIT_FAILURE,EXIT_CONFIG}` |
| 信号 | 只注册 SIGTERM/SIGINT；注册失败则拒绝启动（没有优雅停止能力就不接请求）。注册在组装依赖之前完成 |
| 日志 | `tracing` 到 stderr；级别与格式来自 `[log]`（`trace/debug/info/warn/error`、`text/json`），管道/文件不写 ANSI 转义 |

### 配置（`vaultsyncd_server::config`）

| 项 | 契约 |
| --- | --- |
| `Config::load(path)` / `Config::parse(text, path)` | 严格 TOML：`deny_unknown_fields`（拼错的键必须报错，不能被默认值掩盖）。返回校验后的 `Config{listen, state_dir, database, log}` |
| `[server] listen` | IP:端口；缺省 `127.0.0.1:8721`（`DEFAULT_LISTEN`，只监听回环地址；对外由同机反向代理以 HTTPS 进入，design §17.2） |
| `[state] dir` | 必填、绝对、**启动时必须已存在且是目录**（服务不替运维决定内部目录落在哪个文件系统；内部目录准入属后续存储卡）。数据库固定为 `dir/server.sqlite`（`DATABASE_FILE_NAME`） |
| `[log] level/format` | 闭集；`level` 缺省 `info`，`format` 缺省 `text` |
| `ConfigError` | 稳定码 `config_unreadable` / `config_invalid` / `config_invalid_value`；`Display` 中文，含字段名与路径 |

### 生命周期与 readiness（`vaultsyncd_server::lifecycle`）

| 项 | 契约 |
| --- | --- |
| `Phase` | `Starting` / `Ready` / `NeedsRecovery` / `Draining`；`as_str()`、`is_ready()`（只有 `Ready` 为真） |
| `Lifecycle` | `new()` 起于 `Starting`，所有 Root 的写入默认关闭；`record_recovery(RecoveryReport)` 是唯一的开放入口（转调核心 `WriteAdmission`）；`mark_ready()` 只在 `Starting` 时生效；`mark_needs_recovery(detail)` 之后 `mark_ready()` 不再生效；`begin_shutdown()` 进入 `Draining` 且不复位 |
| `Lifecycle::admit_write(root)` | 先看是否 `Draining`（`SubmissionRefusal::ShuttingDown`，码 `shutting_down`），再转调核心 `WriteAdmission::admit_write`（`recovery_pending` / `needs_recovery`） |
| `root_state(root)` / `recovery_detail()` | 诊断与健康探测输出；`recovery_detail` 只含 Root 身份与意图条数，不含服务器绝对路径 |

### 启动恢复编排与 HTTP（`vaultsyncd_server::{recovery,http,bootstrap}`）

| 项 | 契约 |
| --- | --- |
| `recovery::assess_registered_roots(db, &lifecycle)` | 用 `server_store::roots::all_roots` 取全部已注册 Root（系统级查询，不依赖 owner scope），逐 Root 调 `server_store::intents::unfinished_intents`：空清单 → 记一份空 `RecoveryReport`（开放写入）；非空 → 记入 `RecoveryAssessment::blocked_roots` 并保持关闭。返回 `RecoveryAssessment{open_roots, blocked_roots, is_complete(), detail()}` |
| `recovery::count_unfinished_intents(db)` | 全部 Root 下未收尾意图条数；停止流程用它打印 `unfinished_intents=N` 作为“证据仍保留”的账目 |
| `RecoveryAssessmentError` | `Roots(RootError)` / `Intents{root, source}`：读不出清单就**不启动**（`IntentLog` 契约：不能假设没有未完成意图） |
| `bootstrap::prepare(config)` | 打开数据库（种类/版本/WAL 门禁在 `server-store`）→ 评估恢复 → 全部完成才 `mark_ready` → 绑定监听。错误为 `StartupError::{Database,Recovery,Bind}` |
| `bootstrap::Service::{local_addr,lifecycle,assessment,serve}` | `serve(shutdown)`：打印 `listen/state_dir/schema_version/phase/open_roots/blocked_roots` → `axum::serve(...).with_graceful_shutdown(...)`；收到停止信号先 `begin_shutdown()`，服务循环结束后统计并打印未完成意图条数，再 `database.close()` |
| `GET /v1/health` | 用 `vaultsyncd_protocol::generated::{HealthResponse,ErrorEnvelope}` 的 DTO，不手写 JSON：`Ready` → 200 `{"status":"ok"}`；`Draining` → 200 `{"status":"degraded"}`（读可用、新提交已拒绝）；`Starting` → 503 `NEEDS_RECOVERY` + `retryable=true`；`NeedsRecovery` → 503 `NEEDS_RECOVERY` + `retryable=false`。错误信封的 `request_id` 为 `srv-<pid>-<序号>` |

### 不变量与未接入

- **恢复没完成就不开放写入**：`app` 里没有任何直接打开写门的入口；未完成意图只被报告，不被当成已完成。
- **重启不是恢复证据**：证据仍留在 `fs_intents` 时，重启后的 readiness 依旧为假（进程级用例断言）。
- **停止不丢任务**：`Draining` 先于 HTTP 收尾；停止流程不删除意图行/暂存/已发布内容，只打印保留条数并关闭数据库。
- **配置不猜默认**：未知键、非法地址、缺失/相对状态目录一律退出码 2，且不创建数据库。

未接入（后续卡）：未完成意图的真实重放需要 `PutCommitStore`/`IntentLog` 的真实存储适配（S035/S037 的已知缺口，见下）；内部状态目录与 Root 的 btrfs 准入（`crates/platform-linux-server::mounts`）尚未接到启动路径；`GET /v1/capabilities`（需要 ServerId/epoch 的真实来源）、认证与其余业务端点属 S042—S044；跨进程单实例互斥由 S042 的状态目录锁提供（见「认证与账号（S042）」），两个进程不再能同时占用同一状态目录。

## crates/server-core + apps/server — Root 注册与清单（S044）

状态：S044 实现。核心侧 `vaultsyncd_server_core::api::roots`（纯规则，无 I/O），应用侧 `vaultsyncd_server::roots`（HTTP + 平台探测 + 写库装配），契约在 `api/openapi.yaml`。

### 核心规则（`vaultsyncd_server_core::api::roots`）

| 项 | 契约 |
| --- | --- |
| `AllowedBases::new(iter)` | 允许基目录必须非空、绝对、不含 `..`（`AllowedBaseError::{Empty,NotAbsolute,NotNormalized}`）；重复项去重。空集合是配置错误，不是“不限制” |
| `AllowedBases::{contains,len,is_empty}` | `contains` 按**路径分量**判断“在基目录内或就是基目录”，不用字符串前缀（`/data/root` 不含 `/data/rootx`）；相对路径、含 `..` 的路径一律不在范围内（本层不折叠路径） |
| `CandidateFacts` | 平台层探测结论：`Accepted{volume_identity,root_identity}` / `NotADirectory{reason,detail}` / `UnsupportedStorage{reason,detail}` / `Inconclusive{reason,detail}`；核心不重复做挂载判定 |
| `evaluate_registration(&bases, resolved, facts)` | 先判规范路径仍在允许基目录内，再按事实给出 `RegistrationVerdict::{Accepted,Rejected}` |
| `RegistrationRejection` | `design_error_code()`：越界 `ACCESS_DENIED`，其余（不是目录、存储准入拒绝、扫描不完整）`UNSUPPORTED_STORAGE_BACKEND`（design §11.4 的配置阶段拒绝）；`http_status()` 403/422；`reason()` 进错误信封 `details.reason`（越界为 `outside_allowed_bases`，**不回显允许目录**）；`retryable()` 恒 false |

### 端点与配置（`vaultsyncd_server::{roots,config,http,bootstrap}`）

| 项 | 契约 |
| --- | --- |
| `POST /v1/admin/roots` | 需要有效会话（首发只有一位 owner，没有账号注册接口，因此“仅 owner 管理”＝必须有会话）。请求 `RootRegisterRequest{path}`；成功 200 `RootRegistered{root_id,health,pending_import}`，不回显路径。顺序：允许基目录（按请求文本，越界直接 403 且**不探测**，避免成为任意路径探针）→ `RootHandle::open` + `scan_root` + `admission::evaluate`（候选 vs 内部状态目录、已注册 Root、本进程挂载表）→ 按规范路径再判一次允许基目录 → `roots::insert_root` 写行与合成根条目 |
| 拒绝映射 | 403 `ACCESS_DENIED`（越界/未配置基目录）；422 `UNSUPPORTED_STORAGE_BACKEND`（FUSE/占位视图、非 btrfs、NOCOW、嵌套子卷/子挂载、内部目录重叠、已注册父子/别名重叠、扫描不完整、不是目录）。存储失败 503 `NEEDS_RECOVERY` |
| `GET /v1/roots` | 需要有效会话。`RootList{roots:[RootSummary{root_id,health}]}`，只含身份与健康状态（`healthy`/`unavailable`），**不含绝对路径**；按 `root_id` 排序（库内 `ORDER BY absolute_path` 只用于运维） |
| `[roots] allowed_bases` | 配置项（`Option<AllowedBases>`）：缺省为 `None` ＝ 所有注册 403；出现非法项时启动前报 `config_invalid_value` / `roots.allowed_bases` |
| `RootContext` | `vaultsyncd_server::roots::RootContext{auth, bases, state_dir, scan, ids}`；内部状态目录当前取 `[state] dir`（SQLite 与后续对象/历史/暂存/备份都将在其下），挂载探测在 `spawn_blocking` 里做（`statfs`/`statx`/`getdents64` 是阻塞调用） |
| 身份字节 | `volume_identity` = btrfs FSID 的 UUID 文本字节；`root_file_identity` = `RootSnapshot::of_facts(facts).to_spec()` 字节。两者都是平台层编码，存储层只存不解释 |

### 不变量与未接入

- **注册不改写目录内容**：流程只做只读探测（句柄打开、目录遍历、挂载表读取）与 SQLite 写入；既有文件不导入、不清空、不改名（端点用例断言样本字节不变）。
- **启动不以空库覆盖既有文件**：注册是显式管理动作；启动恢复（S037）只处理未完成意图，不扫描、不写入 Root 内容。
- **两段允许基目录检查**：符号链接不能把 Root 带出允许范围；越界路径连“是否存在”都不回答。
- **普通客户端看不到绝对路径**：清单与注册响应都不含服务器路径；日志里才有 `path=`（服务端日志属运维面）。

未接入（后续卡）：`pending_import` 目前恒为 `true` 且**没有持久化列**——初始采集（S049）完成时如何把“待导入”推进为“已导入”需要 `roots` 追加迁移（design §7.2 的 roots 列不含该状态，SQLite 改 CHECK 需重建表，故留给采集卡一次做对）；Root 健康状态只在注册时写入，运行期挂载漂移复核（`RootHandle::verify` + `RootSnapshot::compare` → `ROOT_UNAVAILABLE` 暂停）尚未接到后台巡检；`epoch` 写 `ServerEpoch::ZERO`（进程还没有独立的 epoch 来源，`GET /v1/capabilities` 属后续卡）；多账号角色区分（admin 与普通成员）需要账号卡先加角色列。

## apps/server + server-admin — Linux 在线管理（S075）

- **契约源**：`api/admin-socket.schema.json`，动态 JSON 文档，不引入手写 wire DTO 或改动 Windows IPC。平台边界在 `platform_linux_server::ipc`，服务内管理适配器在 `apps/server/src/admin.rs`；CLI 在线模块不打开 SQLite。离线 `init` 继续独占状态锁。
- **地址与身份**：服务启动时创建 `<state.dir>/admin/socket`；状态目录必须归服务有效 UID 且不可被组/其他用户写入；`admin` 必须是真实目录、归相同 UID、权限恰为 `0700`，socket 为 `0600`。创建/绑定/chmod/清理锚定 `O_DIRECTORY|O_NOFOLLOW` 打开的目录 fd，避免父路径替换导致操作其它位置。服务读取请求前用 Linux `SO_PEERCRED` 核验相同有效 UID；不信任请求内 UID、login、环境变量或 bearer。CLI 同样核验父目录/socket 类型、权限、所有者、连接前后 inode/dev 与对端 UID。管理员用 `sudo -u <服务账号>`，不开放管理组或 root 特例。只接受唯一 active owner；零个/多个返回 409 `OWNER_REQUIRED`，不猜账号、不签发临时或长期会话。
- **帧与生命周期**：每连接一请求一响应；4 字节大端无符号长度 + UTF-8 JSON，长度 `1..=1048576`，分配前拒绝零长/越界；截断失败。请求为 `["v1", "命令", ...]`，未知版本、命令、参数或非规范页号返回 400。最多 16 个在途连接，每请求 10 秒，CLI 总截止 15 秒。响应 `{"status":200,"result":{...}}` 或 `{"status":4xx/5xx,"error":{"code":...,"message":...}}`；失败不伪装空清单。服务停止前收尾请求，再关数据库；仅清理自身 socket inode。崩溃恢复由启动顺序保证：`bootstrap::prepare` 先取得 `<state>/server.lock` 独占锁，之后才调用 `AdminSocket::bind_after_state_lock`，因此持锁时已存在的 `admin/socket` 只能是上一个持锁进程被 SIGKILL 后留下的 inode，可自动回收。回收仍只在 `O_DIRECTORY|O_NOFOLLOW` 锚定的 admin 目录 fd 内、经 `lstat` 确认类型为 socket 且属主为服务有效 UID 时 unlink；普通文件、符号链接、他人属主一律明确失败且不删除。 服务端 bind 经 `/proc/self/fd/<dirfd>/socket`，本身不受 AF_UNIX `sun_path` 108 字节限制；CLI `--socket` 按字面路径连接，超过 107 字节时 `ipc::connect` 返回明确的 `InvalidInput`（提示缩短状态目录或 `--socket`），不退化成内核裸 `SUN_LEN`。第二个活着的服务端在抢锁阶段即返回 `state_locked`，到不了回收路径，不会覆盖其他实例的实时 socket；绝不删除数据库或恢复证据。

| `vaultsyncd-admin <命令> --socket <绝对路径> [--json]` | 真实用例与输出 |
| --- | --- |
| `status` | 当前 `phase`、owner 的 `root_count`（十进制字符串）、启动 `recovery_detail` |
| `roots [页号]` | 复用 Root 清单用例；`roots` + `next_page`，只给身份与数据库健康值，不给绝对路径；store 新增 `roots_of_owner_page`，按 ID 读取至多 101 条（100 + 下一页探针） |
| `roots register <绝对路径>` | 与 HTTP **共用** `roots::register_for_owner`；允许基目录、btrfs/NOCOW/重叠/完整扫描准入与注册事务不变。仅 ready 时接受；原样返回 `RootRegistered` 或现有错误信封，不直接写在线 DB。超时/断线结果未知，先查询 Root 清单核对，不自动重放 |
| `history <RootId> <EntryId> [页号]` | 服务内 scope 检查后只读真实 committed `versions`，包括软删除条目；版本 ID、字节数、seq、来源、被取代时间，不读取对象内容或账号材料 |
| `trash <RootId> [页号]` | 真实 `trash_entries`；原相对路径、种类、版本（目录为 null）、删除与到期时间；未知/越权 Root 为 403，而非空成功。恢复写操作仍走既有 operation API |
| `problems [页号]` | 持久 conflicts 映射、数据库非健康 Root，以及逐 Root 复用 `verify_root_mount` 的**实时**卷/目录身份、挂载与可读性故障；启动恢复说明另带 `recovery_detail`。探测不更新数据库健康值、不推导删除。内部探测路径/spec 在输出前剔除；不冒称有尚未持久化的逐文件观察器问题目录 |

分页命令固定最多 100 条，页号 `0..=1000000`，`next_page=null` 结束；都是实时偏移分页，不提供一致性快照承诺（需要一致清单用既有 snapshots API）。`problems` 的页按候选 Root/冲突计数，健康 Root 过滤后可能空页但仍有 `next_page`，调用方须跟随游标。JSON 不含密码、PHC、access/refresh 材料；成功退出 0，执行/协议失败 1，用法错误 2。远端仅 SSH 登录主机使用本 CLI，或既有认证 HTTPS；本 socket 不是 HTTP 反代入口、不接受 TCP/URL/`--insecure`。

实机回归：`nix develop --command bash -c 'cargo build --offline -p vaultsyncd-server -p vaultsyncd-admin && python3 tests/platform/linux/run-admin-socket.py'`，在源码外独立 btrfs run_id、隔离 loopback SSH daemon 验证；不改宿主 SSH 配置、Windows VM、其它进程或卷。证据与限制见 `agent/tasks/S075.md`。

## crates/server-core — 历史版本与回收站恢复（S040，子树恢复 S040-b）

主要落点 `crates/server-core/src/history/`（`version`、`trash`），端口在 `ports/restore_store.rs` 与
`ports/content_lease.rs`。设计依据 design §10.3、I12、T13。

### 端口与模块

| 项 | 契约 |
| --- | --- |
| `history::version::VersionRestoreRequest` | `intent_id`（重试复用）、`op: OperationKey`、`digest: RequestDigest`（覆盖条目/基线/版本/路径）、`entry`、`version`（被恢复的旧版本）、`expected_epoch`、`expected_revision`、`target_path`、`staging_path`、`before: TargetBefore` |
| `history::version::restore_version(gate, store, log, target, leases, at, &request)` | 顺序：请求形状 → Root 票据 → epoch → `OpId` 记录 → 版本事实（归属/保留期/种类/基线/名称）→ 内容租约 → 现场指纹 → `publish_put` → `RestoreStore::apply_version_restore` → 核对证据 → 解除租约。返回 `RestoredVersion{ result, lease }` |
| `history::version::CommittedVersionRestore` | `op`、`root`、`entry`、`version`（**新**当前版本）、`restored_from`（旧版本）、`content`、`revision`、`seq`、`path`；`version != restored_from`、`revision > 基线` 是核心核对项（I12） |
| `history::trash::TrashRestoreRequest` | `intent_seed`（意图种子，重试复用）、`op`、`digest`、`entry`（文件或子树根）、`expected_epoch`、`expected_revision`（清单里删除时的 revision）；**不含** `staging_path`（暂存名由服务端派生） |
| `history::trash::restore_trash(gate, &mut TrashRestorePorts{store,log,target,namespace,leases}, at, &request)` | 顺序：epoch → `OpId` 记录 → 清单（存在/逐行未过期/种类自洽/revision 一致）→ 父目录是活目录 → 根的实际名称（占用则另名）→ `plan_subtree` 规划整棵子树（结构不一致在此停写，尚未登记 pending）→ 登记 pending → 按深度 `mkdir` 目录 → 逐文件“租约 + 意图 + 发布” → `apply_trash_restore`（全量 `entries` + 全量 `intents`）→ 核对证据 → 解除全部租约。返回 `RestoredTrash{ result, lease }`；单条目就是长度为 1 的子树 |
| `trash::plan_subtree(&TrashSubtree, root_path)`（内部纯函数） | 清单 → `Vec<PlannedEntry{entry,parent,path,name,kind,content_version,revision}>`，按（路径深度, 路径）排序；根用调用方给的实际路径，后代=“根新路径 + 清单相对后缀”（另名只换前缀）；不一致→`store_contract_violation` |
| `history::trash::intent_for(seed, index)` | 第 `index` 个被发布文件的意图身份：`index==0` 就是种子，其后把种子最后 8 字节当大端 u64 递增；溢出→`invalid_request`（不回绕） |
| `history::trash::staging_name(intent)` | 服务端派生的暂存文件名 `.restore-<意图十六进制>`：稳定、可重入、操作内唯一、单个文件名 |
| `trash::create_restored_directory`（内部） | 恢复目标目录必须 `inspect` 为 `Absent` 才能 `mkdir`；已存在→`subtree_changed`（不把清单内容合并进别人的目录） |
| `history::trash::CommittedTrashRestore` | `op`、`root`、`entry`（身份不变）、`path`、`name`（可能另名）、`renamed`、`revision`、`seq`、`entries`（本批恢复的条目数，含根与后代） |
| `history::trash::alternate_name` | `名称（恢复）.ext`、`名称（恢复 2）.ext`…扩展名保留；`.gitignore` 这类名称整段算原干；上限 `MAX_RESTORE_NAME_ATTEMPTS = 32` |
| `history::RestoreFailure` / `RestoreFailureReason` | 稳定码 + `error_code()`（design §15.4）+ `intent_id`/`phase`（意图证据，发布阶段失败也带）+ `lease_expires_at`（遗留租约）；`is_committed()` 恒为 `false` |
| `history::release_all` / `history::release_after_failure` | 整批租约的清理：成功路径报告最晚未解除的保护时刻（`LeaseDisposition::Expires{at}`），失败路径只记 `lease_expires_at`、不改判主因 |
| `ports::content_lease::ContentLeasePort` | `acquire(&ContentLeaseRequest{root,digest,purpose,expires_at}) -> ContentLease{id,purpose,expires_at}`、`release(&lease)`；`purpose` 与 `object_leases.purpose`（`download`/`commit`）同一套取值；只保护已登记对象 |
| `ports::restore_store::RestoreStore` | `root_facts`、`operation`、`begin_restore`、`version_facts -> VersionFacts{entry,content,retained_until}`、`apply_version_restore(&VersionRestoreWrite) -> VersionRestoreRecord` |
| `ports::restore_store::TrashRestoreStore` | `root_facts`、`operation`、`begin_restore`、`trash_subtree -> Option<TrashSubtree>`、`parent_kind`、`name_occupant`（只算活跃条目）、`content_facts(root, version)`、`apply_trash_restore(&TrashRestoreWrite) -> TrashRestoreRecord` |
| 幂等 | `RestoreBegin::{Started, Recorded}`；同一 `OpId` 同摘要 → 重放首次结果（不再建租约、不再写事务），不同摘要 → `op_id_reused`，登记未收尾 → `operation_pending`（wire `NEEDS_RECOVERY`） |

### 不变量与未接入

- **历史不可变（I12）**：恢复只追加新版本行并引用旧版本的同一内容对象；端口契约禁止修改旧版本行，
  核心核对 `result.version != restored_from` 与 `result.content == 旧版本内容`。
- **不覆盖现有内容（T13）**：原名称被活跃条目占用时另名并回报实际 `path`/`name`；现场必须空缺才能发布，
  事务内再核一次占用（`NameOccupied`）。
- **失败不是 committed**：任何失败都返回 `RestoreFailure`（`is_committed()==false`），已写意图停在
  `prepared`/`published`，`pending` 记录保留；租约解除失败只记 `lease_expires_at`，不改判结果。批量恢复
  任一文件失败就整批停下：元数据事务从未开始（没有任何条目被标成已恢复），返回底层稳定码，已建目录、
  已发布文件与已持久化意图留作 `fs_intents` 恢复证据，旧条目与内容对象不变；成功与失败路径都解除本批租约。
- **子树恢复契约（S040-b）**：清单是多行时整棵恢复，不再返回 `restore_subtree_unsupported`；顺序是
  “按（路径深度, 路径）建目录 → 逐文件租约/意图/发布 → 单一事务收尾全部 `entries` 与 `intents`”。根另名
  时后代路径整体移位、名称不变；某后代的名称在事务内被并发占用仍整批失败（`NameOccupied`），不逐个另名。
- **真实适配器未落地**：`RestoreStore`/`TrashRestoreStore`/`ContentLeasePort` 的 SQLx 实现与进程内装配
  仍未写（与 S035/S039 的同类缺口一致），本卡证明的是核心编排、端口契约与证据核对。

## crates/transfer — 固定块几何与流式摘要（S045）

状态：S045 实现。包名 `vaultsyncd-transfer`，唯一依赖 `sha2`，在 `default-members` 内；不做任何 I/O。

| 模块 | 契约 |
| --- | --- |
| `chunk::ChunkGeometry` | `new(total_size, chunk_size) -> Result<_, GeometryError>`（块大小 0 拒绝）、`total_size()`、`chunk_size()`、`part_count()`（`ceil(total/chunk)`，空内容 0 块）、`part_length(index) -> Option<u64>`（越界返回 `None`，与“空块”可区分）；边界用 `checked_mul`/`checked_sub` |
| `hash` | `DIGEST_BYTES = 32`；`ContentHasher::{new,update,hashed_bytes,finish}`、`digest_of(&[u8])`、`verify(actual, expected) -> Result<(), DigestMismatch>`（带两侧摘要）、`to_hex(&[u8;32])` |

不变量：**块几何与摘要算法只有这一份实现**——`crates/server-store::uploads::ChunkGeometry` 是它在
存储层的薄封装（`UploadRecord::geometry()` 因此返回 `Result`），`crates/server-core::api::uploads`
直接用它。摘要以原始 32 字节交付，不新增第三个摘要包装类型（`server-core::objects::ContentDigest`
与 `server-store` 的 `ObjectHash` 各自保留自己的类型）。续传位图与传输端口不在此 crate。

## crates/server-core — 上传会话与固定块接收（S045）

状态：S045 实现。

| 接口 | 契约 |
| --- | --- |
| `api::uploads::UploadPolicy` | `standard()`：默认块 4 MiB、块上限 16 MiB、总大小上限 64 GiB |
| `api::uploads::plan_begin(policy, &BeginRequest)` | 总大小/块大小超限分别报 `TotalTooLarge`/`ChunkTooLarge`；`chunk_size = 0` 取策略默认值；返回 `BeginPlan{chunk_size, geometry}` |
| `api::uploads::receive_part(store, staging, upload, index, declared, bytes)` | 读会话 → 状态必须 `open` → 几何给期望长度 → 摘要核对 → 已收块比较 → `PartStaging::store_part`（返回即已 fsync）→ `record_part`；返回 `PartReceipt{write,index,length,hash,phase}` |
| `api::uploads::UploadFailure` | `TotalTooLarge`/`ChunkTooLarge`/`IndexOutOfRange`/`LengthMismatch`/`DigestMismatch`/`PartConflict`/`PartsClosed`/`UnknownUpload`/`OutOfScope`/`MalformedSession`/`Storage{detail}`（满盘为 `detail = "no_space"`） |
| `ports::upload_session::UploadSessionStore` | `load`/`existing_part`/`record_part`：全部限定 owner scope，`load` 对“不存在/别人的 Root/别人的上传”统一 `UnknownUpload` |
| `ports::upload_session::PartStaging` | `store_part(upload,index,bytes) -> String`（返回前必须写全并 fsync；失败不得留下可登记路径） |

## crates/server-core — 上传 finalize 与固定版本下载（S046）

状态：S046 实现核心规则与端口，S046-b 接上契约与 HTTP 端点（见下文「apps/server — 内容端点」）。

| 接口 | 契约 |
| --- | --- |
| `api::content::finalize_upload(store, publisher, objects, upload, staging_name, now)` | 读会话 → 读块行 → `AssembledParts::verify`（块数/索引连续/长度等于几何）→ `open` 时 `mark_complete` → `publish_parts` → 核对总长度与总 SHA-256 → `record_published` → `mark_finalized`；返回 `FinalizedContent{upload, root, content: ContentReference{digest,size,storage_path}, part_count, reuse, replayed}`。**不写 entries/versions/changes/operations** |
| `api::content::ContentFailure` | `Upload(UploadFailure)`/`IncompleteUpload{expected,present}`/`ContentMismatch{expected,actual,expected_size,actual_size}`（并使会话转 `aborted`）/`Publish(PublishFailure)`/`VersionGone`/`EntryUnavailable`/`RangeNotSatisfiable{size,requested}`/`InvalidRange{requested,detail}`/`Read{detail,no_space}`/`Lease(ContentLeaseError)`/`Storage{detail}` |
| `api::content::parse_range(header, size) -> Result<ByteSlice, _>` | `bytes=start-end`/`bytes=start-`/`bytes=-suffix`；终点超出收敛到末字节、后缀长于对象等于整份；多区间/非 `bytes=`/反向/非规范十进制 → `InvalidRange`；起点越界、`-0`、空对象 → `RangeNotSatisfiable`。`ByteSlice::content_range(size)` 给 `bytes start-end/total` |
| `api::content::open_download(store, leases, root, &DownloadRequest{entry,version,range}, lease_expires_at)` | 解析 `(root, entry, version)` 固定内容地址 → 收敛 Range → `acquire(Download)` 租约；返回 `DownloadHandle{content,slice,lease}`。版本不存在/已清理 → `VersionGone`，事实与本请求不符 → `EntryUnavailable` |
| `api::content::read_download(port, layout, &handle)` | `read_object_range` 读 `slice`；短读 → `ContentMismatch`；覆盖整个对象时核对整份 SHA-256（部分切片由客户端按 design §13.2 校整文件） |
| `api::content::finish_download(leases, handle)` | 解除下载租约；解除失败只报告，不改判已读出的字节 |
| `ports::content_publish::ContentPublisher::publish_parts(upload, staging_name, parts)` | 按索引升序拼装进对象区暂存副本、边写边算摘要、不覆盖发布；返回 `PublishedContent{digest,size,storage_path,reuse}`（实测事实）。`PublishFailure` 含 `PartUnreadable`/`PartLength`/`PartMissing`/`Storage{stage,no_space,detail}`/`Verification`/`ObjectConflict`/`StorageLayer` |
| `ports::content_publish::ContentObjectStore::record_published(root, &content, verified_at)` | 登记 `objects` 行；同一 `(root,hash)` 相同登记幂等，不同长度/路径 → `ObjectConflict` |
| `ports::version_content::VersionContentStore::version_content(root, entry, version)` | 返回 `VersionContent{root,entry,version,digest,size}`；`OutOfScope`/`EntryNotFound`/`NotAFile`/`VersionGone`/`ObjectUnavailable`，owner scope 由适配器保证（I15） |
| `ports::upload_session::UploadSessionStore` 增量 | `parts(upload)`（索引升序）、`mark_complete`（适配器须先确认无缺失块）、`mark_finalized`、`abort` |
| `ports::object_store::ObjectBlobPort::read_object_range(object, offset, length)` | 返回 `StoredSlice{data,bytes}`；短读是错误，不补零；`offset+length` 越界为错误 |

适配器落地情况（S046-b）：`apps/server::uploads::DbUploads` 已实现四个新增会话方法（`parts_of`/
`mark_upload_complete`/`mark_upload_finalized`/`abort_upload`，各自独立写事务）；
`apps/server::content` 实现 `FsAssembler`（块拼装 + `objects::capture_object` 发布语义）、
`DbObjects`（`versions::record_object`）、`DbVersionContent`（`versions::current_version` +
`versions::version_history`）、`DbLeases`（`object_leases` 的 download 租约）与 `FsObjectPort`
（对象区整份/区间读）；`crates/platform-linux-server/examples/object-capture.rs` 的
`LinuxObjectPort` 同样已实现区间读。

## apps/server — 上传端点（S045）

| 端点 | 语义 |
| --- | --- |
| `POST /v1/roots/{root}/uploads` | 建会话：`principal_from` → `authorize_root` → `plan_begin` → `uploads::begin_upload`（单次写事务）；201 返回 `UploadSession`；会话绑定签发它的登录会话，到期 24 小时 |
| `GET /v1/roots/{root}/uploads/{upload}` | 会话状态 + **服务端真实已收块** + 缺失区间；Root 不符/不属于该登录会话 → 404 `UNSUPPORTED_ENTRY`（不泄露存在性） |
| `PUT /v1/roots/{root}/uploads/{upload}/parts/{index}` | 块体 `application/octet-stream`、块摘要头 `x-part-sha256`（64 位小写十六进制，缺失或不合法 → 400 `INVALID_REQUEST`）；载荷上限 `max_chunk_size + 1`，由上传路由上的 `DefaultBodyLimit` 按策略设置（默认 16 MiB + 1，覆盖 Axum 自带的 2 MiB——否则默认块大小 4 MiB 无法上传）；成功 200 `UploadPartAck{already_present}` |

失败映射（S045-b 的 HTTP 验收用例 `apps/server/tests/uploads.rs` 逐条实测）：401 `AUTH_EXPIRED`、
403 `ACCESS_DENIED`、404 `UNSUPPORTED_ENTRY`（会话不存在、Root 不符或**不属于该登录会话**）、
400 `INVALID_REQUEST`（形状/越界索引/长度不符/缺或错的 `x-part-sha256`/超载荷）、
409 `PART_CONFLICT`（同索引不同字节或会话已不接收块）、422 `HASH_MISMATCH`、
413（请求体超过策略上限，由路由层在处理器之前返回，无错误信封）、503 `NEEDS_RECOVERY`、
507 `INSUFFICIENT_STORAGE`。暂存落在 `state_dir/uploads/<upload_id>/<index>`（临时文件 → fsync →
rename → fsync 目录）；越界索引、长度不符、摘要不符与 413 都在写盘之前返回，不留暂存文件。
`x-part-sha256` 自 S046-b 起是契约里的真实请求头参数（生成器 `format_version 4` 支持 `in: header`）；
契约里它是 `putUploadPart` 的必填参数，生成的 TS 客户端把它作为 `x_part_sha256` 参数并在
`request_headers(...)` 之后 `headers.set("x-part-sha256", …)`。
**已知缺口**：`INVALID_REQUEST`/`PART_CONFLICT` 是本次扩展的错误码，design §15.4 需补行（等用户确认措辞）。

## apps/server — 内容端点（S046-b）

`POST /v1/roots/{root}/uploads/{upload}/finalize`（JSON）与
`GET /v1/roots/{root}/entries/{entry}/versions/{version}/content`（二进制/区间）都先
`auth::principal_from` 再 `auth::authorize_root`，然后才动存储。

| 端点 | 语义与实测返回码 |
| --- | --- |
| `POST …/uploads/{upload}/finalize` | 200 `FinalizedContent{upload_id, root_id, hash, size, storage_path, part_count, reuse, replayed}`（可提交内容引用，**不改条目 head**）；块未收齐 400 `INVALID_REQUEST`（不改会话状态）；拼装内容与声明的总长度/总哈希不符 422 `HASH_MISMATCH`（会话转 `aborted`、不登记对象）；会话已放弃 409 `PART_CONFLICT`；重复调用 200 且 `replayed=true`（不重新发布）；缺凭据 401 `AUTH_EXPIRED`、跨 Root 403 `ACCESS_DENIED`、会话不存在/不属于该登录会话 404 `UNSUPPORTED_ENTRY`、满盘 507 `INSUFFICIENT_STORAGE`、其余存储失败 503 `NEEDS_RECOVERY` |
| `GET …/entries/{entry}/versions/{version}/content` | 整份 200 `application/octet-stream` + `Content-Length` + `x-content-sha256`（该版本的整份内容地址）+ `Accept-Ranges: bytes`；带 `Range: bytes=start-end`/`bytes=start-`/`bytes=-suffix` 时 206 + `Content-Range: bytes start-end/total`；区间语法非法/越界/多区间 400 `INVALID_REQUEST`；版本不存在、已清理、不属于该条目或该 Root 410 `VERSION_GONE`（不回退成当前内容）；缺凭据 401、跨 Root 403、读取或租约失败 503 |

对象区布局：`state_dir/objects/objects/<前 2 位十六进制>/<64 位十六进制>`（已发布）与
`state_dir/objects/staging/<名字>`（未发布）；块暂存仍是 `state_dir/uploads/<upload_id>/<index>`。
下载读取前建立 `object_leases` 的 `download` 租约（`ttl 900 s`），读完解除；解除失败只记录日志，
不改判已读出的字节。生成侧：`MediaType.schema` 可选、成功响应折叠成 `SuccessBody::{Json,Bytes}`
（同一端点可同时声明 200 与 206，形状必须一致），字节响应的 TS 函数返回 `Uint8Array` 并走
`read_bytes`（非 2xx 仍按错误信封 `read_json`）。

## crates/server-core + apps/server — 操作命令端点（S047）

### 核心（`vaultsyncd_server_core::api::operations`）

- **`OperationRequest`**：put/create/move/delete/restore 的强类型形状（`op`/`device`/`kind`/
  `expected_epoch` + 各命令自己的 `entry`/`base_revision`/`base_version`/`upload`/
  `local_generation`/`conflict_policy`/`parent`/`name`/`expected_absent`/`subtree_token`/`version`）。
  `validate()` 按命令固定前提（put 要 entry+base_revision+upload；create 要 parent+name 且
  `expected_absent=true`、不得自带 entry；move 要 entry+base_revision+目标 parent/name；delete 要
  entry+base_revision；restore 要 entry+base_revision），混用两套命令语义一律 `INVALID_REQUEST`。
- **`admit(RootFacts, &OperationRequest)`** 的固定顺序：Root 可写性 → epoch → 命令形状。因此不可写
  Root 先得到 503 `ROOT_UNAVAILABLE`，旧 epoch 得到 410 `EPOCH_CHANGED`，都不会先去看操作日志。
- **`digest_fields()`** 只给语义字段（种类/身份/基线/上传/本地代际/冲突策略/命名空间目标）；
  `op_id`、`device_id`（幂等键本身）与 `server_epoch`（客户端事实基础）**不参与**摘要，避免“重试时
  带来新 epoch”被误判成 `OP_ID_REUSED`。摘要编码仍由 `server-store::operations::request_digest`
  唯一实现。
- **`decide_replay(RecordedOperation, digest)`**：同摘要 → 已记录结果；不同摘要 → 409
  `OP_ID_REUSED`。**`RecordedOutcome::{Committed, Pending, Rejected}`** 的 `http_status()`：200/202/200，
  `is_synced()` 只对 committed 为真（I01）。
- **`OperationFailure`** 自带 `design_error_code()`/`http_status()`/`is_retryable()`：
  `ROOT_UNAVAILABLE` 503(可重试)、`EPOCH_CHANGED` 410、`OP_ID_REUSED`/`BASE_MISMATCH`/`SUBTREE_CHANGED`/
  `RECONCILE_REQUIRED` 409、`INVALID_REQUEST` 400、`INVALID_NAME`/`UNSUPPORTED_ENTRY` 422、
  `UNKNOWN_OPERATION` 404、`NEEDS_RECOVERY` 503、`INSUFFICIENT_STORAGE` 507。

### 端点（`apps/server::operations`，契约 `api/openapi.yaml`）

`POST /v1/roots/{root}/operations`（JSON）与 `GET /v1/roots/{root}/operations/{op}` 都先
`auth::principal_from` 再 `auth::authorize_root`。幂等键是 `(root, device_id, op_id)`，请求里的
`device_id` 由客户端安装实例自报（首发没有设备注册表，不作为授权依据）。结果查询只带 OpId：
记录里的设备由库给出（`server-store::operations::operation_of_root_in_scope`，同 Root 同 OpId 多行
视为坏数据）。

| 情况 | 实际返回 |
| --- | --- |
| 首次见到该键（**本版**） | 503 `SERVICE_NOT_READY`（`retryable=true`），**不写** `operations` 行、不追加事件批次 |
| 同键同摘要、记录 committed | 200 `OperationResult{state=committed, root_seq, revision, entry_id, version_id, echo_local_generation, replayed=true}` |
| 同键同摘要、记录 pending | 202 `OperationResult{state=pending, replayed=true}`，**不带** `revision`/`root_seq`/`version_id`/`entry_id` |
| 同键不同摘要 | 409 `OP_ID_REUSED`（`retryable=false`） |
| `server_epoch` 不是 Root 当前 epoch | 410 `EPOCH_CHANGED` |
| Root health 不是 healthy | 503 `ROOT_UNAVAILABLE`（可重试） |
| 记录 rejected | 409 + 记录里的 `error_code`/`error_message`（终态结论稳定） |
| 查询不存在/不属于该 Root/不属于该会话 | 404 `UNSUPPORTED_ENTRY`（三者不区分） |
| 形状/身份文本不合法 | 400 `INVALID_REQUEST`；名称不可用 422 `INVALID_NAME` |

`SERVICE_NOT_READY` 是本卡新加的 wire 错误码（design §15.4 闭集里没有“执行器未装配”，`NEEDS_RECOVERY`
另有“意图无法解释、需人工介入”的专义），**待与 design §15.4 同步**。

### 未接入（属 S052 服务端整合闭环；已由 S052/S052-b 接入，见「服务端提交执行器与观察运行时（S052）」）

- 命令执行适配器：`PutCommitStore`/`TargetPort`/`NamespaceStore`/`RemovalPort`/`RestoreStore`/
  `ConflictCommitStore` 的真实 SQLx + 文件系统实现、`begin_operation` → 执行 → `commit_operation`/
  `reject_operation` 的接线；装上后“首次见到该键”才应改回 202/200。
- 生成器（S047 顺带补齐）：可选 `decimal_u64` 字段用 `crate::decimal::option`，可选字段一律加
  `#[serde(default)]`（否则带自定义读写实现的可选字段在缺失时会变成必填）。

## crates/server-core + apps/server — 外部目录初始稳定采集（S049）

### 核心（`vaultsyncd_server_core::observer`）

- **端口**：`ports::work_tree::WorkTreePort`（`list_dir`/`observe`，指纹 `FileStamp` = 设备/inode/链接数/大小 + btime/mtime/ctime；读不到是 `WorkTreeError::{Unreadable,RootUnavailable,InvalidPath}`，确认并发消失才是 `Gone`/`Absent`，**不得**把故障报告成不存在）；`ports::external_store::ExternalAdoptStore`（`lookup_path` + 一个事务 `apply_external`：登记对象/写版本/推进 revision/追加 `changes` 批次；`Update` 用比较并写，**不触碰工作目录**）。候选与对象沿用 `ObjectBlobPort`，新增 `objects::capture_observed_object`（期望内容取自候选读回值，且要求候选长度等于建副本前的源长度）。
- **`scan::scan_tree(port, ScanLimits, ScanCursor)`**：BFS、目录内按名称字节序；预算按目录项计、可在目录中途停下并精确续扫。结论 `ScanCompletion::{Complete, Inconclusive{resume, reasons}}`：预算耗尽、目录/项读不了、Root 不可用都是 `Inconclusive`，已枚举部分**不是**完整快照；枚举不产生任何删除结论。符号链接/特殊文件/非 UTF-8 名进入 `skipped`，不静默消失。
- **`scan::StabilityTracker`**：窗口默认 1000 ms（设计 500—1000 ms）；首次看到总等待完整窗口；指纹任一字段变化（含只有 ctime 变化的保留 mtime 改写）重新计时。只降低读到中间态的概率，不证明写入结束（C33）。
- **`capture::capture_external`**：稳定指纹 → 观察前核对 → 私有候选（reflink/校验复制）+ 读回校验 → 观察后要求与观察前指纹**完全相同** → `ExternalProposal`。变化 → `Requeue(RequeueReason)`；路径消失/非普通文件单列，不是删除。
- **`capture::adopt_external(gate, store, work, proposal, at)`**：与 put 共用 `RootCommitGate`（忙则 `RootBusy`，不阻塞）→ 采纳前再次核对指纹 → 路径查询（库里内容相同则 `UnchangedContent`，不产生版本；目录冲突 `Blocked`）→ `apply_external` → 核对证据（摘要/长度/条目/revision 推进，不符是 `StoreContract`，不当成已采纳）。
- **`import::ExternalObserver::run_pass`**：一轮 = 续扫（有游标则继续，否则从起点）→ 对仍待处理但本轮未扫到的文件逐个复查 → 稳定判断 → 每轮最多 `max_captures` 个采集+采纳。`history_protected(path)` 仅在采纳成功（或库里已有相同内容）后为真；`initial_import_complete` 需要“一轮完整扫描 + 当时无待处理”，一旦为真保持。账本只是进程内报告视图，重启后靠库查询避免重复版本。
- **采纳后的现场**：事务提交后现场仍可能再被改写，不阻止也不宣称阻止；新内容下一轮成为下一版本。放弃采纳时已发布的候选对象只是无引用的内容地址文件，由对象 GC 回收。

### Linux 适配（`vaultsyncd_server::observer`，`vaultsyncd_platform_linux_server::files`）

- `LinuxWorkTree`（`RootHandle` 上的受限枚举/`identity_at`；每列一个目录先 `RootHandle::verify`，漂移 = `RootUnavailable`）与 `LinuxObjectPort`（源经 Root 句柄读取，候选建在对象区 staging，复制期间源变化映射为 `SourceUnreadable`）。
- 平台修复：`RestrictedDir::list_entries` 现在先 `lseek` 倒回目录句柄起点；此前重复列举同一句柄（Root 句柄每轮扫描都列一次）第二次起得到空列表。新增 `RootHandle::identity_at`。

### 未接入（后续卡）

- `ExternalAdoptStore`/`PutCommitStore` 的真实 SQLx 适配（含补建父目录条目、`changes` 事件内容）；`roots.pending_import` 仍是注册时的常量，初始导入完成标志尚未落库（需要 `roots` 迁移，与存储适配一起做）；
- 后台调度（周期轮次、`apps/server` 里按 Root 启动观察器）已由 S050 提供可驱动的 `FollowDriver`，进程入口按 Root 起后台任务仍未做；已采纳文件被外部删除/改名的事件管线。

## crates/platform-linux-server::watcher + server-core::observer — inotify 与溢出补扫（S050）

### 平台（`vaultsyncd_platform_linux_server::watcher`）

- **`DirWatcher::open(&RootHandle, WatchConfig) -> (DirWatcher, Vec<WatchSignal>)`**：递归登记每个目录一个 inotify watch（先登记后列目录，不留缺口）；监视对象是已打开的目录句柄（`/proc/self/fd/N`，`RootHandle::open_dir`/`proc_fd_path`），不按路径字符串重新解析。`poll(&RootHandle, timeout)`/`drain(&RootHandle)` 读取事件并整理成 `WatchSignal`：
  - `Changed{path}`：该路径（文件或目录）上发生过事件，只是“值得再看一眼”的线索，**不**表示删除/写完；同一次读取内同路径合并，一次读取最多 `max_signals_per_poll` 条；
  - `Subtree{path}`：新出现（或权限恢复后第一次可监视）的目录，监视已登记，必须补扫整个子树；
  - `Overflow`：内核队列溢出（`IN_Q_OVERFLOW`），已丢弃整张监视表并重建；消费者必须立即全量补扫；
  - `Gap{path, reason, detail}`：该目录没有被监视（`PermissionDenied`/`WatchLimit`/`Vanished`/`Other`），只能靠周期扫描覆盖；不是删除。
- 目录被移走/删除时按路径前缀移除其整棵监视，不再以旧路径上报；非 UTF-8 名只上报所在目录。不识别“谁写的”，也不做改名配对（cookie 未用）。

### 核心（`vaultsyncd_server_core::observer`）

- **`hints::{WatchHint, HintQueue, HintLimits}`**：线索（`Changed/Subtree/Overflow/Gap`）的有界去抖队列。同路径合并，静默满 `debounce_ms` 才由 `take_due(now_ms)` 交出；子树线索吸收其内的路径线索；容量满则整体退化为一次全量补扫（`FullScanCause::HintQueueFull`），不丢线索也不无限增长；**`Overflow` 不经去抖，下一次取用立即要求全量补扫**并丢弃已排队的路径线索。
- **`intents::{OwnWriteIntent, OwnWriteLedger}`**（经 `ExternalObserver::expect_own_write/release_own_write`）：本服务写入 Root 之前登记（设备+inode+内容长度/摘要）。观察到该路径文件时 **inode 相同且内容摘要相同才忽略**（`FileState::OwnWrite`，不采纳）；inode 不同 = 不是它（意图保留，服务发布可能尚未发生）；同 inode 但内容不同 = 外部原地修改（意图作废并正常采纳）。无任何时间窗口；账本有容量上限，满了拒绝登记（`IntentError::LedgerFull`，调用方暂缓该写入），不淘汰旧意图。服务提交完成/放弃后调用方必须 `release_own_write`。
- **`ExternalObserver::run_hinted_pass(ports, paths, subtrees, limits, clock)`**：只观察线索指向的路径/子树，并推进所有待处理文件（窗口要靠重新观察才推进）；观察到不存在只记入 `HintedReport.absent`，**不产生删除结论**；读不了进 `unreadable` 保持原状；子树扫描被预算截断置 `subtree_truncated`。与全量扫描共用稳定窗口、采集与采纳，不另开写入通道。
- **`ExternalObserver::run_content_review(ports, batch, limits, clock)`**：对已采纳路径按路径顺序分批重读内容（采纳基线现含内容摘要），与基线摘要比对；不同 = 指纹没发现的改写，撤下基线按首次看到重新等待窗口（`ReviewReport.content_changed`）；读不了/不存在不动基线。`wrapped` 表示一圈走完。
- **`follow::{FollowDriver, FollowConfig}`**：`push_hint` 记线索，`step(ports, limits, clock)` 一步 = 全量补扫（若需要）→ 线索观察（若需要）→ 内容复核（若到期）。全量补扫原因 `FullScanCause`：`Startup`（第一次 step 必做）、`QueueOverflow`、`HintQueueFull`、`SubtreeTruncated`、`Continuation`（上次预算耗尽，立即续扫）、`Retry`（上次有读不了的项/Root 不可用，`retry_interval_ms` 后）、`Periodic`（`metadata_interval_ms`）。`FollowConfig::recommended()` = 去抖 300 ms、周期元数据补扫 60 s、重试 5 s、每 10 s 复核 64 个文件（design §23）。

### Linux 适配（`vaultsyncd_server::observer`）

- `hint_of(WatchSignal) -> WatchHint`（一一对应）；`RootWatcher::open(Rc<RootHandle>, WatchConfig) -> (RootWatcher, Vec<WatchHint>)` 与 `pump(&mut FollowDriver, now_ms, timeout)`：**先**建立监视、**再**让驱动器做启动补扫。进程入口按 Root 起后台任务、监视出错后的重建尚未做。

### 限制（后续卡）

- 已采纳文件的外部删除/改名管线（rename 配对、回收站）不在本卡：观察到不存在只记录、不推导；自写意图尚无调用方（服务端写入发布路径接入时登记/释放）；内容复核用候选采集（reflink/复制 + 哈希 + 发布同内容对象）实现，开销较大，后续可在端口上增加只读哈希；挂载变化后需重建监视（`RootUnavailable` 之后）由进程入口负责。

## crates/server-core + server-store + apps/server — 快照、changes 与 SSE（S048）

契约在 `api/openapi.yaml`（生成器 format v5：`in: query` 标量参数、`text/event-stream` 成功响应）。四个端点
都先 `auth::principal_from` 再 `auth::authorize_root`；查询串只接受声明的参数，未知/重复/非规范十进制 400。

| 端点 | 语义 |
| --- | --- |
| `POST /v1/roots/{root}/snapshots` → 201 `Snapshot` | 同一写事务读 `roots.seq`（`high_water_seq`）并复制活跃条目成清单；TTL 600 s（`api::snapshots::SNAPSHOT_TTL_SECONDS`） |
| `GET …/snapshots/{snapshot}/entries?cursor&limit` → 200 `SnapshotPage` | **只读清单**，按 EntryId 升序；`next_cursor` 缺省=末页；`limit` 1–1000（缺省 500），越界 400；快照过期/未知/他人的 **410 `CURSOR_EXPIRED`**（`details.reason=snapshot_expired`），不返回空页 |
| `GET …/changes?after&limit` → 200 `ChangeList` | 严格 seq 升序；`limit` 1–500（缺省 100）只决定在哪个**事务边界**截断：第 `limit` 个批次与后续批次 `tx_id` 相同且 seq 连续则延伸取完；`has_more`/`next_after`/`head_seq`/`server_epoch` 同读事务取得；`after` 早于保留窗口（默认 90 天）或晚于当前头 → **410 `CURSOR_EXPIRED`**（`details.reason=before_retention_window｜ahead_of_head` + `expired_through｜head_seq`） |
| `GET …/events` → 200 `text/event-stream` | `event: seq`，`id`/`data`=`SeqHint{root_id,seq}`；新连接立即给当前头，之后头变化才提示（可合并跳号）；**只提示、不带事件内容**；draining 时流主动结束 |

- **存储**：迁移 `0007_snapshots.sql`——`snapshots`（元数据）/`snapshot_entries`（创建时的值拷贝，无外键引用 `entries`，
  `ON DELETE CASCADE`）与 `changes_by_time` 索引。`server-store::snapshots::{create_snapshot,snapshot_page}`：
  创建时回收过期快照并把每 Root 的未过期快照压到 `MAX_LIVE_SNAPSHOTS_PER_ROOT`(8)；翻页元数据与条目同一读事务。
  `server-store::changes` 增补 `change_window`（头、epoch、保留下沿 = “创建时间早于截止的最新批次”的 seq，索引寻址）与
  `changes_after_whole_transactions`（不拆事务）。`SnapshotId`（`domain::ids`）是快照身份类型。
- **核心规则**（纯函数，`server_core::api::{events,snapshots}`）：`judge_cursor`、`retention_cutoff`、`changes_limit`/
  `snapshot_limit`（越界拒绝而非截断）、`page_progress`、`next_hint`、`ChangesFailure`（400/410/503 + 错误码 + 可重试）。
- **`apps/server::sync`**：`SyncContext::{new,with_config}`（`SyncConfig{event_retention_seconds, sse_poll_interval,
  sse_keepalive}`）；`http::router` 与 `bootstrap` 增加 `SyncContext` 参数。SSE 流自己轮询 `roots.seq`（间隔默认 1 s），
  不依赖进程内发布者，所以任何写者推进 seq 都会被提示。
- **客户端约定**：SSE 可丢/重/乱序，客户端以 `changes` 的游标与 seq 去重修复；断线只需轮询 `changes`；410 时重新取快照并
  保留本地基线与未上传队列（design §9.4）。`events_json` 是该事务全部条目变化的 JSON 数组文本，整批原子应用。

### 未接入 / 限制

- `events_json` 的事件内容形状由提交协调（S052 起）写入；本卡只保证“整批取出、不拆开”。
- 事件保留期（90 天）目前只是 `SyncConfig` 默认值，尚未进入 `[config]` TOML；物理清理 `changes` 行受
  `versions.created_seq`/`operations.root_seq` 的外键约束，保留窗口是逻辑下沿，不删行。
- `GET …/events` 需要 `Authorization` 头，浏览器原生 `EventSource` 不能直接用（TS 客户端 `subscribeEvents` 返回原始 `Response`）。
- 未覆盖 `ServerEpoch` 变更后旧游标的显式 410（`after` 不带 epoch；`ahead_of_head` 与 `before_retention_window` 覆盖
  回退场景的常见形态，epoch 绑定留给重建流程卡）。

## crates/http-client — 共用 HTTPS 客户端与认证刷新（S053）

状态：S053 实现。包名 `vaultsyncd-http-client`，在 `default-members` 内，Linux/Windows 客户端共用；只依赖 wire 契约（`vaultsyncd-protocol` 的生成 DTO）、`reqwest`（rustls + 系统信任库）、`tokio` 与 `serde_json`，不做本地状态与平台 I/O。设计依据 design §5.4、§13.3、§17.3；刷新语义来自 S043 的服务端轮换（旧刷新凭据用一次即失效）。

| 模块 | 契约 |
| --- | --- |
| `transport` | 端口 `Transport::send(TransportRequest) -> TransportFuture<Result<TransportResponse, TransportError>>`（装箱 future，可作 trait object）。`TransportRequest{method,url,headers,body}`、`TransportResponse{status,headers,body}`、`Method{Get,Post,Put,Patch,Delete}`、`TransportFuture<'a,T>`。生产实现 `ReqwestTransport`（`pub(crate) new(&ValidatedConfig)`）：rustls + 操作系统信任库、`connect_timeout`、**不跟随重定向**（避免 `Authorization` 跨主机）、`user-agent: vaultsyncd-client/<版本>` |
| `config` | `ClientConfig`（`new(base_url)` + `with_timeouts` / `with_trust` / `allow_plaintext_http`），由 `ApiClient::new` 校验：只接受 `https`（明文 `http` 必须显式 `allow_plaintext_http`，只用于本机/隔离测试）、拒绝 URL 里的用户信息、查询串与片段、超时必须非零。`Timeouts{request,connect}`（默认 30 s / 10 s）、`TlsTrust::{SystemRoots, SystemRootsWithAdditionalRoots(Vec<PEM>)}`（`system_roots_with_pem` / `system_roots_with_pem_file`）、`ConfigError{kind,message}`、`ConfigErrorKind{Url,Scheme,PlaintextForbidden,Timeout,Tls,Io}` |
| `auth` | `Credentials{session_id,access_token,refresh_token,access_expires_at,refresh_expires_at}`（`from_issued(&SessionIssued)` 校验 32 位小写十六进制会话身份、非空令牌、规范十进制到期时间、访问不晚于刷新；`Debug` 隐去两个令牌）。`AuthSession`（克隆共享状态）：`install`/`clear`/`snapshot`/`generation`/`needs_reauth`/`has_credentials`；内部 `refresh(observed_generation, refresher)` 用一把异步锁做**单飞合并**——拿到锁后先比代际，别人已刷新就直接复用，不再发请求。`CredentialSink{credentials_stored,credentials_erased}` 是持久化端口（agent 用 OS 凭据库/受限权限文件，见 ADR 0001），本 crate 不写文件 |
| `client` | `ApiClient<T: Transport>`（`new(ClientConfig)` → 生产传输；`with_transport(config, Arc<T>)` → 测试替身；`with_auth_session` 注入会话；`Clone` 共享会话与连接池）。`ApiRequest::{new,get,post}` + `with_auth`/`with_json`/`with_body`/`with_header`/`with_timeout`/`with_cancellation`；`ApiResponse{status,headers,body}` + `header`/`retry_after`/`json`。`ApiClient::{send,send_json,login,logout}` |
| `cancel` | `Cancellation`：可克隆的取消开关（`cancel`/`is_cancelled`/`cancelled`），基于 `tokio::sync::watch`；只取消等待，不取消别人共享的刷新 |
| `error` | `ClientError::{InvalidRequest,Transport,Timeout{phase},Cancelled,NotAuthenticated,ReauthRequired{reason,cause},Api(Box<ApiFailure>),UnexpectedResponse{status},Protocol}`；`is_retryable()`、`retry_after()`、`certificate_problem()`、`api_failure()`。`TransportErrorKind::{Connect,Timeout,Tls,Certificate(CertificateProblem),Io,Protocol}`；`CertificateProblem::{UnknownIssuer,Expired,NotYetValid,NameMismatch,Revoked,Other}`；`ApiFailure{status,body,retry_after}`（`body` 是生成 `ErrorBody`）；`TimeoutPhase::{Request,Refresh}`、`ReauthReason::{RefreshRejected,RetriedUnauthorized}` |
| `testing`（仅 `cfg(test)`/`testing` feature） | `FakeTransport::new(handler)`（记录每次实际发出的 `TransportRequest`，`calls`/`call_count`）、`json_response`/`error_response`/`issued` 构造器。产品构建里不存在该模块 |

### 认证与请求规则（不可绕过）

- **401 一次受控刷新**：`AuthMode::Bearer` 的请求收到 401 后，用会话里的刷新凭据换新凭据并重试**一次**；同一会话的并发 401 由单飞锁合并成**一次**刷新。重试后仍 401 → `ReauthRequired{RetriedUnauthorized}`，**不再刷新**。
- **刷新失败分两类**：刷新接口 401 → `ReauthRequired{RefreshRejected}` 并置 `needs_reauth`，凭据与队列都保留；刷新遇到连接/超时/证书问题 → 原样返回该错误（可重试），凭据、代际与队列都**不变**。`NotAuthenticated` 在本地没有凭据时于发送前返回，不碰传输。
- **TLS 不可降级**：只有系统信任库或“系统信任库 + 显式追加的 PEM”；追加根证书是加法，不会放行其它不受信任的证书，仓库里没有关闭校验的配置项或环境变量。证书问题单独分类、不可重试、不触发刷新。
- **超时与取消**：每次请求的 `timeout` 默认取 `Timeouts::request`，可按请求覆盖，覆盖传输发送与读响应体全过程；取消返回 `Cancelled`，不是失败状态。
- **错误分类**：服务端错误信封 → `Api`（`is_retryable` 取 `body.retryable`，`retry_after` 只解析秒数形式的 `Retry-After`）；非信封的非 2xx（代理/网关）→ `UnexpectedResponse`（5xx 可重试）；2xx 但响应体不合契约 → `Protocol`（不可重试）。
- **请求装配**：路径必须是 `/` 开头的绝对路径且不含方案、查询串、片段或空白（查询串编码留给需要它的端点卡）；`Authorization`、`Content-Type`、`Content-Length` 由客户端生成，调用方手写会被 `InvalidRequest` 拒绝；请求头名称限 ASCII 字母数字与 `-`，值不允控制字符；`base_url` 的路径前缀（如 `/vault/`）对每个请求保留。

### 不变量与未接入

- 本 crate 不读时钟、不取随机数、不写数据库/文件；刷新是否能成功只由服务端判定，客户端不做“提前过期”猜测。
- 普通网络故障的指数退避与 `Retry-After` 调度属于 `crates/agent-core`（后续卡）：这里只给 `is_retryable`/`retry_after` 结论。
- 未接入（后续卡）：Root 清单（S054 起）、changes/SSE 拉取、上传会话、固定版本内容下载（design §5.4 的 `roots.rs`/`changes.rs`/`uploads.rs`/`content.rs`）；查询串编码；浏览器 cookie 载体（Rust 客户端只走 Bearer）。
- 验收命令（devShell 内）：`cargo test --locked -p vaultsyncd-http-client`（36 项 + 1 doctest）、`cargo clippy --locked -p vaultsyncd-http-client --all-targets -- -D warnings`、`cargo fmt --check`。证书用例用本地 rustls 服务与自签 CA 做真实握手。

## crates/server-core + platform-linux-server — 外部移动、删除与挂载失效（S051）

### 核心（`vaultsyncd_server_core::observer`）

- **扫描健康结论（`scan`）**：`ScanReport::health() -> ScanHealth`，取值 `Complete` / `PermissionDenied{paths}` / `IoError{paths}` / `MountChanged{detail}` / `Incomplete{remaining_dirs}`（预算耗尽单列）。优先级：挂载 > 权限 > IO > 未走完；完整原因仍在 `ScanCompletion::Inconclusive.reasons`。`InconclusiveReason::{DirectoryUnreadable,EntryUnreadable}` 增 `failure: ReadFailure{PermissionDenied,IoError}`；`WorkTreeError::PermissionDenied{path,detail}` 与 `Unreadable` 是两个错误（`code()` 分别为 `work_tree_permission_denied` / `work_tree_unreadable`）。**只有 `ScanHealth::Complete` 允许推导删除。**
- **移动识别（`reconcile::pair_moves`）**：证据只认文件对象身份（设备 + inode + 创建时间，`MissingEntry::same_object`），内容哈希不参与配对。一对一才配对：两份同内容文件互换名字时各自 inode 仍能正确对上；一个缺失对多个新路径（硬链接/复制）或多个缺失共用一个身份 → 歧义，不猜，按独立新增/删除处理。移动目标必须尚未采纳；目标路径在库里已有条目 → 不移动（不覆盖别人的条目）。
- **删除与保护（`ReconcileLimits`）**：`ReconcileLimits{max_deletes_per_cycle, max_delete_percent, protection_capacity}`（`DEFAULT` = 32 / 10% / 8，`new` 拒绝 0 与 >100%）。`protection_reason` 按单轮上限与占比判定，占比规则在已保护条目 < `SHARE_MIN_TOTAL`(16) 时不生效（小集合只看数量）。命中即整批进 `ProtectedBatch{id, reason, protected_total, entries:[ProtectedEntry{path,entry,revision,identity,btime,content}], directories:[ProtectedDirectory{path,identity,error,missing_count}]}`，队列满时逐个记 `held{code=protection_queue_full}`，不静默丢弃。
- **`ExternalObserver`**：`run_pass` 在 `scan_complete && !root_unavailable` 且扫描健康时、于 `handle_files` **之前**做核对（否则移动目标会被当成新文件建新条目），整轮持有 `RootCommitGate` 票据（忙则 `ReconcileReport::deferred`，下一轮再试）。一次完整扫描可跨多轮续扫，核对用累计的 `scan_files`。`PassReport.reconcile: ReconcileReport{health, ran, moves, ambiguous, deletions, held, protected, deferred}`。
- **互换名字（`identity_swap_needs_review`）**：两条路径都还在、但文件对象换了位置时，单向移动表达不了，按路径重新采纳又会把两个 EntryId 绑到对方的文件上；因此保留双方基线、把现场记进 `held`/`ambiguous`，既不删除也不重绑，`handle_files` 对该路径返回 `PendingReason::Held{code}`。人工处理后才恢复。
- **放行**：`protected_batches()` 只读查看；`release_protected(id)` 移出队列并把（路径, 条目身份）记成一次性放行，下一轮只对这些身份传播删除（路径换成别的条目时重新受保护）。`with_reconcile_limits(limits)` 设定限额。
- **端口**：`ports::external_store::ExternalAdoptStore` 增 `apply_external_move(ExternalMove) -> ExternalMoveRecord`（只改父目录与名称、保留 EntryId、不写版本/对象）与 `apply_external_delete(ExternalDelete) -> ExternalDeleteRecord`（可恢复删除：回收站 + 墓碑 + 事件 + 保留历史），两者都比较并写 `expected_revision` 并返回提交后事实；`ports::work_tree::WorkTreePort` 增 `observe_dir(dir) -> Result<Option<DirIdentity>, _>`（保护队列取目录身份）。`AdoptOutcome::UnchangedContent` 现带回 `entry`/`revision`，写进采纳基线。

### 平台（`vaultsyncd_platform_linux_server::mounts::health`）

- `verify_root_mount(path, &RootSnapshot) -> Result<MountHealth, MountChange>`：重新 `probe_path` → 与注册快照比较（后端/FSID/子卷/挂载 ID/目录身份）→ 检查 Root 之下是否有子挂载（按路径分量，`/data/root2` 不是 `/data/root` 的子挂载）→ 确认根目录仍可列出（读一项）。`MountChange::{Probe,Drift,Submount,PermissionDenied,Unreadable,MountInfo}`，`design_error_code()` 恒为 `ROOT_UNAVAILABLE`，`detail_code()` 给出 `mount_changed`/`submount_detected`/`permission_denied` 等细分。权限不足在探测阶段也单独分类。
- `RootHandle::verify()` 现在调用它（`RootSpecVerifyError::Mount`），因此所有共享根句柄的路径（`LinuxWorkTree::list_dir`/`observe_dir`）都受同一复核；“卸载后路径下露出空目录”命中的是 `DirectoryChanged`/`MountChanged`，即 Root 不可用。
- `submounts_under(entries, root)` 是纯函数（可单测）；`apps/server::observer::map_restrict` 把 `RestrictError::PermissionDenied` 映射为 `WorkTreeError::PermissionDenied`。

### 不变量与未接入

- 扫描不完整（预算）或健康结论不是 `complete` 时**零删除、零移动**；Root 不可用直接中止整轮。
- 真实 `umount` 需要特权，未在测试里执行：验收用真实目录“改名 + 原路径建空目录”复现卸载后的现场（对 Root 句柄而言与卸载等价），并在平台层用真实目录验证身份漂移与权限回收。
- 未接入（S052 及后续）：`apply_external_move`/`apply_external_delete` 的 SQLx 适配与回收站落库；进程入口按 Root 运行后台任务；目录级移动识别（`WorkTreePort` 只提供文件指纹，目录改名时其内文件按 inode 逐个保持身份，目录条目本身由存储层补建）；硬链接去重（当前按独立条目处理）。

## crates/transfer — 续传位图与上传/下载执行（S054）

状态：S054 实现。包名 `vaultsyncd-transfer`，仍在 `default-members` 内，**依旧不做 I/O**：不读文件、不建连接、不碰数据库、不看时钟。执行器只通过 `src/ports.rs` 的端口碰外部世界，适配器在别处（`crates/http-client` 的 `uploads`/`content`，`crates/agent-core` 的本地临时文件与进度表）。设计依据 design §13.1—13.3；服务端事实来自 S045/S046 的上传与内容端点。

| 模块 | 契约 |
| --- | --- |
| `bitmap` | `PartRange{first_index,count}`（`new` 拒绝 0 块、`indices`、`end_index`）；`PartBitmap::{empty,from_bytes,geometry,contains,mark,count,is_complete,missing,bytes,into_bytes}`。第 i 位 = 第 i 块、低位在前；`from_bytes` 校验长度与尾部 bit，`mark` 越界返回 `IndexOutOfRange`；`BitmapError::{SizeMismatch,TailNotZero,IndexOutOfRange,Unrepresentable}` |
| `ports` | `PortFuture<'a,T>`；`PortError::{Unavailable{detail,retry_after},ReauthRequired{detail},Cancelled,Rejected{code,detail},Protocol{detail},Local{kind,detail}}` + `is_retryable/is_cancelled/needs_reauth/is_no_space/code`；`LocalErrorKind::{Read,Write,NoSpace,Missing}` |
| `ports::SnapshotSource` | `identity()`、`total_size()`、`read(offset,length)->Vec<u8>`（短读是错误） |
| `ports::UploadTarget` | `begin(&SessionSpec{total_size,digest,chunk_size})->UploadSessionInfo`、`status(session)->UploadProgressInfo{session,received,missing}`、`put_part(session,index,&digest,bytes)->PartAck{index,length,digest,already_present}`、`finalize(session)->FinalizedContent{digest,size,part_count,replayed,storage_path}`；`UploadSessionState::{Open,Complete,Finalized,Aborted}` |
| `ports::VersionedContent` / `VersionPin` | `read_range(&pin,offset,length)`；`VersionPin{binding,entry,version,digest,size}`（`new` 拒绝空身份/控制字符） |
| `ports::TempContent` | `path()`、`ensure_capacity(needed)`、`write_at(offset,&bytes)`、`read_at(offset,length)`、`sync()`、`discard()` |
| `ports::DownloadProgressStore` / `StoredProgress` / `DownloadKey` | `load/save/discard`；进度记录 = 块大小 + 总大小 + 临时路径 + 位图（构造时校验几何一致）；键 = `binding/entry/version`（不同版本永不共用位图） |
| `concurrency` | `CancelToken::{new,cancel,is_cancelled}`（克隆共享）；内部 `bounded_join`（最多 `limit` 个块在飞、遇错停止启动新块并丢弃在飞的 future） |
| `upload` | `DEFAULT_CHUNK_SIZE=4 MiB`、`DEFAULT_PART_CONCURRENCY=4`、`DEFAULT_PART_ATTEMPTS=2`；`UploadOptions{chunk_size,part_concurrency,part_attempts,cancel}`；`UploadSnapshot{identity,total_size,digest}`、`UploadResume{snapshot,session}`；`run(source,target,Option<&UploadResume>,&UploadOptions)->Result<UploadOutcome,UploadFailure>`；`UploadOutcome{content: ContentReference, session, uploaded_parts, reused_parts, resumed}`；`UploadErrorKind::{InvalidOptions,InvalidSnapshot,ReadSnapshot,SnapshotChanged,SessionMismatch,SessionClosed{state},PartRejected{index,expected,actual},FinalizedMismatch,Cancelled,Port}`；`UploadFailure{kind, resume: Option<Box<UploadResume>>}`（`resume()` 给出可直接持久化的续传信息） |
| `download` | `DEFAULT_CONTENT_ATTEMPTS=2`；`DownloadOptions{chunk_size,part_concurrency,content_attempts,cancel}`；`run(content,temp,progress,&VersionPin,&DownloadOptions)->Result<DownloadOutcome,DownloadError>`；`DownloadOutcome{temp_path,size,digest,part_count,downloaded_parts,resumed,attempts}`；`DownloadError::{InvalidOptions,NoSpace{needed},Cancelled,ContentMismatch{expected,actual},Port}` |
| `hash` 增量 | `from_hex(&str)->Result<[u8;32],HexError>`：只接受 64 位**小写**十六进制（大写在契约里不是同一份摘要） |

### 执行顺序与不变量

- **上传**：流式哈希整份快照（窗口 ≤ 块大小）→ 续传先比对快照身份/大小/摘要（任一变化 → `SnapshotChanged`，不碰旧会话）→ 会话事实（总大小、整份摘要、块数）必须等于快照，否则 `SessionMismatch` → 按服务端 `missing` 补块（还核对“缺失区间 = 已收块的补集”）→ `put_part` 的回执索引/长度/摘要都要一致，不符则重读快照重传（`part_attempts`），持续不符 `PartRejected` 且不 finalize → `finalize` 再核对 hash/size/part_count。**finalize 结果只是可提交内容引用，不写 entries/versions/changes/operations**（design §15.2、I01）。
- **下载**：固定 `VersionPin` → 载入位图（块几何/总大小/临时路径不符即作废）→ `ensure_capacity`（不足 `NoSpace`，不写不删）→ 只取缺失块并写入临时偏移 → `sync` → 流式读回临时文件算整份摘要 → 相符才返回；不符则作废位图与临时内容、有限次重下，仍不符 `ContentMismatch` 且不安装。安装/发布意图/状态更新属上层。
- **混版本**：摘要相同即内容相同，因此会话摘要与快照摘要相等时复用旧会话安全；不等时立即拒绝。下载键含 VersionId，且整份摘要不符绝不安装——服务端若供给另一版本，结局是失败而不是发布。
- **不是跨版本块增量**：没有“按块跨版本复用/内容定义分块/增量专用 GC”的入口；位图只在同一次传输的有效性内使用。

验收命令（devShell 内）：`cargo test --locked -p vaultsyncd-transfer`（59 项）、`cargo clippy --locked -p vaultsyncd-transfer --all-targets -- -D warnings`、`cargo fmt --all -- --check`。执行器用内存端口替身验证断线续传、错块重传、取消保留进度与并发上限。

## crates/http-client — 上传会话与固定版本内容端点（S054）

状态：S054 实现。新增两个模块，均实现上面的 `crates/transfer` 端口，错误分类沿用 S053 的 `ClientError` → `PortError` 映射；不引入第二套 DTO（响应类型来自 `vaultsyncd_protocol::generated`）。

| 模块 | 契约 |
| --- | --- |
| `uploads::UploadEndpoint<T: Transport>` | `new(client, root)`（Root 非 32 位小写十六进制 → `ClientError::InvalidRequest`，触网前失败）、`client()`、`root()`；实现 `UploadTarget`：`POST /v1/roots/{root}/uploads`、`GET …/uploads/{upload}`、`PUT …/uploads/{upload}/parts/{index}`（`content-type: application/octet-stream` + 必发 `x-part-sha256`）、`POST …/uploads/{upload}/finalize`（无请求体）。`PART_DIGEST_HEADER = "x-part-sha256"` |
| `content::ContentEndpoint<T: Transport>` | `new(client, root)`、`client()`、`root()`；实现 `VersionedContent`：`GET /v1/roots/{root}/entries/{entry}/versions/{version}/content` + `Range: bytes=start-end`。`CONTENT_DIGEST_HEADER = "x-content-sha256"` |
| `content::parse_content_range` / `ContentRange` | `bytes start-end/total` → `{first,last,total}` + `length()`；非 `bytes ` 前缀、非规范十进制、起点大于终点 → 协议错误 |

### 请求与校验规则（不可绕过）

- **身份只接受 32 位小写十六进制**：Root、上传会话、条目、版本在拼 URL 之前校验（`is_identity`/`require_identity`），杜绝把 `../` 之类拼进路径。
- **块摘要一定要发**：`x-part-sha256` 是 64 位小写十六进制；服务端在写盘前用它核对字节。
- **区间响应必须自洽**：带 `Content-Range` 时起点/长度/总长都要与请求和 `VersionPin.size` 一致；整份 200 响应只接受“从 0 开始且长度等于版本大小”；缺少 `Content-Range` 的 206 是协议错误；响应体长度不等于请求长度是协议错误（短读不补零）。
- **整份摘要提前核对**：响应带 `x-content-sha256` 且与固定版本摘要不同 → 协议错误（不用下完整份才发现是别的版本）。
- **错误分类**：`ClientError` → `PortError`：`Timeout`/可重试 `Transport`/5xx 非信封/`Api{retryable:true}`（带 `Retry-After`）→ `Unavailable`；`Api{retryable:false}` → `Rejected{code}`；401/刷新失败/无凭据 → `ReauthRequired`；证书、协议、请求构造 → `Protocol`（不可重试）；`Cancelled` → `Cancelled`。
- **没有“提交条目”的入口**：`finalize` 只返回内容引用；提交与 committed 确认由 agent 执行器用 `OpId` 完成。

验收命令（devShell 内）：`cargo test --locked -p vaultsyncd-http-client`（60 项 + 1 doctest，其中 `flow_tests` 用传输替身当契约服务端跑通断线续传/错块重传/固定版本下载）、`cargo clippy --locked -p vaultsyncd-http-client --all-targets -- -D warnings`。


## crates/agent-core — 客户端端口与确定性操作执行器（S055）

状态：S055 实现。包名 `vaultsyncd-agent-core`，位于 `crates/agent-core/`，在 `members` 与
`default-members` 内。依赖 `vaultsyncd-domain`、`vaultsyncd-fs-common`、`vaultsyncd-planner`、
`vaultsyncd-protocol`、`vaultsyncd-transfer`、`vaultsyncd-client-store` 与 SQLx（只用于适配器的连接池
与短事务）；**不依赖** server-core/server-store/Tauri，不做平台 I/O（design §5.5）。假端口只在
`cfg(test)` 编译；产品代码不依赖 tokio。Linux 同步客户端产品不在本包生成。

### 端口（`ports/`，互不引用）

| 端口 | 契约 |
| --- | --- |
| `clock::Clock` | 对 `domain::version::Clock` 的 `Send + Sync` 包装（`impl<T: DomainClock + Send + Sync> Clock for T`），时间点只用 `UtcTimestamp`。`clock::now(&dyn Clock)` 把时钟错误转成 `PortError::Local{Read}`。端口不 sleep：执行器只报告到期时间。 |
| `local::LocalPort` | `capabilities() -> LocalCapabilities`（按需读取/脱水/使用保护/稳定身份/原子替换/名称约束）、`observe(&RelativePath) -> LocalFacts`（种类、generation、大小、file identity、`Usage`）、`snapshot(path, generation) -> Arc<dyn transfer::SnapshotSource>`、`apply(&ApplyRequest)`、`move_entry(&MoveRequest)`、`remove(&RemoveRequest)`、`release(&ReleaseRequest) -> ApplyReceipt{stage: Applied/Unchanged, generation, file_identity}`。代际不符必须 `LOCAL_CHANGED`，不强行继续。 |
| `remote::RemotePort` | `submit(&protocol::OperationRequest) -> OperationResult`、`operation(binding, OpId) -> OperationResult`（响应丢失后按原 OpId 查询）、`upload(binding) -> Arc<dyn UploadTarget>`、`content(binding) -> Arc<dyn VersionedContent>`。直接复用 wire DTO 与 S053/S054 端点，不另造操作形状；`pending` 原样返回，不由适配器包装成成功。 |
| `state::OperationStore` | `pending_ops(binding)`、`record_failure(op, code)`（attempts+1、写错误码，其余不变）、`complete(op)`；队列表类型复用 `client_store::queue::PendingOp`（别名 `QueuedOperation`）。 |
| `state::IntentStore` | `begin_intent(&ActionIntent) -> i64`（同 binding+target_path 的相同意图复用同一条，字段不同拒绝）、`fail_intent(id, code)`、`complete_intent(id)`、`pending_intents(binding) -> Vec<StoredIntent>`。`ActionIntent{op, binding, kind: LocalWriteKind{Replace,Move,Release,Import}, entry, target_path: RelativePath, source_path, content_hash: Option<[u8;32]>, generation}`。 |
| `state::UploadResumeStore` | `load_resume(op)`、`save_resume(&StoredUploadResume)`、`discard_resume(op)`；`StoredUploadResume{op,binding,snapshot_identity,total_size,digest,session}` 与 `transfer::upload::{UploadResume,UploadSnapshot}` 互转（坏行报错，不当成“没有续传”）。 |
| `ports::Ports` | 唯一组装点：`local`/`clock`/`remote`/`operations`/`intents`/`upload_resumes` 六个 `Arc<dyn …>`，只负责组装，不规定构造顺序。 |

端口依赖表写在 `ports/mod.rs` 模块文档；测试逐字校验各叶模块不出现其它端口 trait 名，并要求只有
组装点同时提到全部端口。端口失败统一用 `vaultsyncd_transfer::ports::PortError` 分类（本地 SQLite
失败按 `Local{Write}`/`Local{Read}`）。

### 执行器（`executor/`）

| 项 | 契约 |
| --- | --- |
| `OperationKind` | `pending_ops.kind` 的闭集：`upload_content`/`upload_new_entry`/`upload_move`/`upload_move_with_content`/`create_conflict_copy`/`apply_remote`/`apply_remote_move`/`apply_remote_delete`/`create_from_remote`/`delete_entry`/`bind_remote_entry`/`merge_baseline`/`release_local`。`from_action(&planner::Action)` 是规划器动作的唯一映射（`NoOp`/`Defer`/`PreserveBoth` 无可执行动作）；`local_write()` 给出需要先落意图的四种（apply/replace、move、release、create_from_remote→replace；`apply_remote_delete` 无 `local_intents` 种类）。未知文本不猜，阻塞该操作。 |
| `ActionHandler` | S056/S059 接入的缝：`plan_intent(op) -> Option<ActionIntent>`（不得在此改本地内容）与 `execute(ports, op, intent)`；返回 `ActionOutcome::{Completed, Pending{poll_after}}` 或 `ActionFailure{code, detail}`。顺序由执行器保证。 |
| `Executor<H>::run(&RunRequest)` | 暂停时只返回原因、不读队列；否则按插入顺序依次过“依赖就绪 → 尝试次数 → 退避到期 → 种类可解析”闸。就绪操作先 `IntentStore::begin_intent` 再执行；成功则完成意图并出队；`Pending` 不出队（I01）并记轮询到期；失败调用 `record_failure`，可重试错误（`ErrorCode::class() == Retryable`）退避后重试，用尽或 `NeedsUserAction`/`Corrupt` 时标记意图失败并阻塞。**任何失败都不删除操作与其依赖方**。 |
| `RetryPolicy` | 默认 `max_attempts=3`、`base=1s`、`max=60s`；`new` 拒绝 0 次、>16 次、0 延迟与上限小于起点。`delay_after(n)=min(base*2^(n-1), max)`，不加抖动（S062）。 |
| `RunRequest` / `RunReport` | `RunRequest{binding, pause: Option<PauseReason>, max_ops, pending_poll_interval}`；`PauseReason::{UserRequested,Offline,RootUnavailable,BindingDisabled}`。`RunReport{binding, paused, ops: Vec<OpReport>, next_due}`，`OpReport{op, kind, outcome}`，`OpOutcome::{Completed, Pending{poll_after}, Retrying{code,attempts,due}, Blocked(BlockReason)}`，`BlockReason::{DependencyIncomplete{on},RetriesExhausted{code,attempts},NeedsUserAction{code},Corrupt{code},UnknownKind{kind},CyclicDependency,BackoffPending{due},RunLimitReached}`。 |
| `ExecutorError` | `Port(PortError)`（本轮中止，队列原样）与 `DuplicateOp{op}`（数据损坏，不执行任何一个操作）。 |

调度不使用客户端时钟排序；退避到期时间只存在进程内（`Mutex<HashMap<OpId, UtcTimestamp>>`），避免同一
进程内连续 `run` 立刻烧光尝试次数；跨重启的退避持久化属 S062。依赖成环时只标记真正成环的一段，
环的前驱报告 `DependencyIncomplete`。

### 适配器（`adapters::client_store`）

`ClientStorePorts::new(Arc<client_store::db::Database>)` 同时实现 `OperationStore`、`IntentStore`、
`UploadResumeStore` 与 `transfer::ports::DownloadProgressStore`：队列/意图/续传表直接映射；下载进度
用 `transfers` 的 `chunk_size`/`total_size`/`temp_path`/`bitmap`，位图只增不回退、几何或临时路径变化
即作废旧进度、位图完整时 `complete_transfer`（verified=1），已完成记录重复保存幂等；`cancelled`/
`failed` 终态不返回可用进度。上传续传键为 `OpId`，操作出队由外键级联删除。

验收命令（devShell 内）：`cargo test --locked -p vaultsyncd-agent-core`（28 项）、
`cargo clippy --locked -p vaultsyncd-agent-core --all-targets -- -D warnings`、`cargo fmt --all -- --check`。

## crates/client-store — 上传续传会话（S055，迁移 0003）

`crates/client-store/migrations/0003_upload_resumes.sql` 新增 `upload_resumes`：

| 项 | 契约 |
| --- | --- |
| `upload_resumes::NewUploadResume` / `UploadResume` | `op: OpId`、`binding`、`snapshot_identity`、`total_size`、`digest: [u8;32]`、`session`；`matches_snapshot(identity,total,digest)` 判断会话是否仍可用于这份快照。 |
| `upload_resumes::save(tx, &NewUploadResume)` | `INSERT … ON CONFLICT (op_id) DO UPDATE`：同一操作的快照与会话一起覆盖，不存在“旧会话配新快照”。快照身份/会话非空无 NUL，长度不超 `INTEGER` 上限，操作不存在（外键）报 `UnknownOperation`。 |
| `upload_resumes::load(conn, op)` / `discard(tx, op)` | 读一条 / 删除并返回是否命中；坏行（短身份、非法摘要长度、空字段）读时拒绝。 |

表不变量：`op_id` 主键并外键 `pending_ops(op_id) ON DELETE CASCADE`（操作成功出队即删除续传记录）；
不存块位图（已收块以服务端 `upload_parts` 为准，design §9.3）。schema 目标版本随之为 `3`。

## crates/agent-core — generation 上传动作（S056）

路径：`crates/agent-core/src/executor/upload.rs`。`UploadActionHandler` 按注册的 `OpId`/`UploadSpec`
执行 `LocalPort::observe → snapshot → transfer::upload::run → observe`：前后 generation、大小及稳定
file identity 必须仍对应固定快照，否则返回 `LOCAL_CHANGED`，不提交旧内容。上传续传由
`UploadResumeStore` 保存；finalize 只得到内容引用，仍不是 committed。

| 接口 | 契约 |
| --- | --- |
| `UploadSpec` | 本地相对路径、固定 `Generation`、可选 create/move 目标信息；generation 由观察/规划层在每次可观察保存时单调推进，处理器不使用 mtime 生成结论。 |
| `UploadCommitStore::commit_upload` | 输入原 `OpId`、绑定、generation、快照身份、finalize 内容引用与 `OperationResult`；实现必须在短事务中按 generation 条件写 baseline/dirty，并保存 `OperationResult::conflict` 的稳定主条目/副本 EntryId 映射。返回 `Applied` 或 `StaleGeneration`；后者只确认旧远端提交，不得清理新 generation 的 dirty 内容。 |
| `UploadActionHandler` | 上传提交请求携带原 `OpId`、`local_generation`、基线 revision/version、设备身份与 server epoch。`pending` 留队列和续传记录；提交响应不可用时使用 `RemotePort::operation(binding, op)` 查询同一 OpId；只有 committed 且 `UploadCommitStore` 成功后才清理续传记录并返回 `Completed`。 |

旧 ACK 不以 mtime 决胜，也不覆盖未确认本地内容；冲突结果必须由 `UploadCommitStore` 与本地快照/基线一起持久化。
验收命令：`cargo test --locked -p vaultsyncd-agent-core`（30 项），并执行 agent-core clippy/fmt。

## crates/agent-core — 事件接收与应用进度（S057）

`engine::events::EventReceiver<S,D,A>` 是绑定作用域的事件编排器，位于
`crates/agent-core/src/engine/events.rs`。`ChangeSource` 只负责按 `RootId + after + limit` 拉取
协议 `ChangeList`；SSE/SeqHint 不进入持久状态，只作为唤醒参数，周期调用 `run_cycle(None)` 仍会
执行 pull 兜底。`EventDecoder` 同时解码 wire 批次和重启后 `received_changes` 的原始
`EventEnvelope`，不得把未知事件静默变成空任务。

| 接口 | 契约 |
| --- | --- |
| `EventApplier::apply(binding, changes, tasks)` | 对同批次任务执行本地应用；实现应按显式依赖独立推进无关条目，返回 `ApplyResult::Applied` 或带 `ErrorCode` 的 `Blocked`。 |
| `EventReceiver::run_cycle(hint)` | 先恢复未应用批次，再按 `changes` 严格 seq 拉取；重复/过旧提示不重复应用，页有空洞或乱序时报协议错误且不越过空洞。 |
| 接收事务 | `client_store::events::receive_batch` 在一个短事务中按批次 → remote → pending tasks → received cursor 写入；提交后才调用应用器。 |
| 应用事务 | 应用器完成后，在独立短事务中写 `baseline_entries` 并调用 `mark_batch_applied`，只推进连续已应用前缀；失败标记 `blocked`，批次与接收游标保留，重启时由 `resume_pending` 重放。 |

`ReceiveError::{Source,Decode,Store,Apply,Protocol}` 不吞底层错误；批次解码或应用事务失败时不伪造
应用完成。`ReceiveReport` 区分 `received`、`applied`、`blocked` 与重复提示，便于测试和诊断。

## crates/http-client — changes 拉取（S057）

`crates/http-client/src/changes.rs::list` 使用生成的 `ChangeList`，通过 `ApiRequest::with_query_param`
对 `after`/`limit` 做独立百分号编码，并强制 Bearer 会话；Root 身份和规范十进制游标在发送前校验。
`ApiRequest` 禁止调用方直接拼接查询串，查询参数按 RFC 3986 编码。SSE 仍只由上层作为唤醒来源，
不被当作事件正文来源。

### 服务端提交执行器与观察运行时（S052）

S047 的操作端点在“首次见到该幂等键”时只回答 503 `SERVICE_NOT_READY`；S049/S050 的观察规则也缺少生产适配器与后台任务。S052 把它们接上（实现见 `apps/server/src/{executor,external}.rs`）：

- **内容 put 执行链路**：`ExecutorContext::{execute_put}`。顺序是
  1. 读库确定命名空间事实：条目（必须活跃文件）、`entries::path_of` 得相对路径、父目录与名称；
  2. `begin_operation`（`operations` 表写 `pending`，幂等键 `(root, device, op)`）；
  3. `commit_put` + 端口实现：`SqlIntentLog`（`fs_intents`，证据 JSON 用核心的
     `TargetBefore/After::to_json`）、`WorkTreeTarget`（`RootHandle` 观察 + 受控发布）、`SqlPutStore`；
  4. `SqlPutStore::apply_put` 在一个 `BEGIN IMMEDIATE` 里 `commit_operation`（事件批次 + 终态）
     →`versions::record_version`→`entries::advance_entry_revision`→意图 `finalized`。
  ACK 只在事务提交后返回；`pending` 不是 committed。
- **`WorkTreeTarget` 的替换前提**：`observe(target_path)` 记下实测身份（`RestrictedDir::identity_at`）
  与 SHA-256；`publish` 用这份实测身份作 `ExpectedTarget::{Present,Absent}`，因此“观察之后目标被外部
  改写”会在发布前被拒绝；发布走 `platform_linux_server::files::publish`（FICLONE/校验复制 +
  `renameat2` + 目录刷盘 + 发布后核对）。`ENOSPC`/`EDQUOT` 映射 507 `INSUFFICIENT_STORAGE`，
  其它存储故障 503 `NEEDS_RECOVERY`。
- **事件内容（`changes.events_json`）**：固定为对象数组，字段
  `kind`（`put`/`external`/`move`/`delete`）、`entry_id`、`path`（有路径时）、`revision`、
  内容变化另带 `version_id`/`hash`/`size`，以及 `origin`（`client`/`external`）；全部计数与摘要都是
  十进制/十六进制文本。一个 RootSeq = 一条完整批次，客户端整批原子应用。
- **重试语义**：端点与执行器共用同一份 `request_digest` 判定重放；响应丢失后的重试返回同一
  `revision`/`root_seq`/`version_id`。结果 JSON 里没有 `root_seq` 时以 `operations.root_seq` 补齐
  （执行器写结果时批次序号尚未分配）。
- **失败与稳定拒绝**：任何失败都不返回 committed；只有读证据成功且该 Root 没有未完成意图时，才尝试
  写稳定 `rejected`。已经有意图的失败保留 `pending`；读不出意图也不得推测成稳定拒绝。所有命令在共享
  提交门内检查未完成意图，存在或读取失败即停写。观察器同样不采纳有未完成意图的 Root。
- **命令适配（S052-b）**：`commands::{SqlFsIntentLog, WorkTreeNamespace,
  SqlNamespaceStore, SqlDeleteStore}` 接入 create/move/delete；移动保持 EntryId/内容版本，删除写清单、墓碑
  并保留历史。`restore_ops::{SqlRestoreStore, SqlTrashRestoreStore}` 接入历史版本与回收站（文件/目录）恢复；
  `conflict_ops::SqlConflictStore` 在迟到 put 的 preserve_both 下提交独立副本，映射唯一键为
  `(RootId, DeviceId, OpId)`，不覆盖 head。冲突名称复用 planner，恢复/冲突的真实 RootSeq 在同一事务内写回结果。
- **持久操作关联（迁移 0009）**：`execution_journal(root,device,op → kind, context_json)` 在**发布前**与
  意图同一事务写入（`journal::Binding::{new,linked,persist,attach}`）；`intent_operations(intent → 完整操作键)`
  让启动补完不靠暂存名、mtime 或最新操作行推断。`kind ∈ put | namespace | delete | version_restore |
  conflict | trash_restore`；`context_json` 携带 entry/revision、restored_from、冲突 sibling 身份、
  `local_generation`，回收站整组恢复则携带冻结清单与全部意图身份。`trash_membership(entry → 删除根/删除批次)`
  由删除事务写入。升级旧库时遗留的、没有关联的意图原样保留（不编造关联），启动后保持停写。
- **回收站整组恢复**：`TrashRestoreStore::begin_restore(key, digest, &plan)` 在一个事务里写 `operations`
  pending、`trash_restore` 计划与全部目录的 `prepared` Create 意图，**之后**才 mkdir/发布；根位置空缺核对
  在登记之前（已存在的外部目录按 `SUBTREE_CHANGED` 拒绝，无 pending、无意图）。子树范围 = 根的删除批次
  （`trash_membership.deletion_id`）内按父子身份取闭包，不用旧路径前缀；没有归属的行（外部采纳的单条删除）
  只恢复它自己。位置跟随原父目录**当前**路径（`TrashRestoreStore::parent_path`）；根名称被活跃条目占用则
  另名（`名称（恢复）`），不合并。`TrashRestoreWrite` 增 `directory_intents`。保留期已过 → 410 `VERSION_GONE`。
- **启动补完（`replay::registered`，先于 `recovery::assess` 与监听）**：对每条未完成意图读取持久关联，
  put/version_restore/conflict 用 `classify_put`（前像重做 / 后像补完 / 其它停写），namespace/delete 用核心
  `recover_*_root`，trash_restore 按持久计划逐条目补建目录/文件意图后用同一事务收尾（一次整组只补一次）。
  无法证明的现场（第三种内容、前像被外部改动、计划外内容、意图损坏、关联缺失）返回错误：意图/证据/
  文件原样保留，Root 保持停写；现场还原后下一次启动补完**同一**操作，不产生第二个版本/副本/事件。
- **删除不丢未确认内容**：`commands::verify_delete_scope`——文件现场哈希必须等于当前版本；目录现场每项必须在
  冻结清单里且种类相同、文件长度等于当前版本长度（清单有而现场没有的不算问题）。前向路径不通过 → 409
  `RECONCILE_REQUIRED`（意图之前拒绝）；重启补完不通过 → 保持停写。限制：目录内同名同长度的原地改写不被该
  检查发现（观察器周期采纳兜底）。
- **错误与重放一致性**：`OperationFailure::VersionGone`（410 `VERSION_GONE`）；`commands::{namespace,delete,
  restore}_failure` 与 `executor::map_put_failure` 保留类型化的 `EpochChanged{expected,actual}`；恢复与冲突
  命令在没有意图时把失败记成稳定拒绝。`journal::with_echo` 把客户端 `local_generation` 写进持久结果，
  所有命令的重放、状态查询与重启补完都回显同一值。各 `apply_*` 事务内用 `journal::ensure_digest` 再核对
  操作摘要与 owner scope。
- **自写关联**：put 登记 `expect_own_write`（设备 + inode + 内容）；其余命令靠共享提交门 + 提交后现场与
  数据库一致，由进程级测试 `own_commands_never_come_back_as_external_changes` 证明命令结果不以
  `origin=external` 重新出现（5 s 内多轮观察）。
- **外部采纳（`external::SqlExternalStore`）**：`lookup_path` 用 `entries::entry_by_path`（逐分量按
  原始名称）+ 当前版本；`apply_external` 在补建缺失父目录后，按 Create/Update（比较并写
  `expected_revision`）在一个事务里追加批次、幂等登记对象、写版本（`origin=external`）、推进 revision；
  `apply_external_move` 只改父目录与名称（EntryId 不变）；`apply_external_delete` 推进 revision、软删除、
  写回收站清单与墓碑（保留历史与对象），三者都不触碰工作目录。
- **观察运行时（`external::ObserverRuntime`）**：每轮从库里枚举 Root（新注册的下一轮自动纳入），
  为每个健康 Root 用 `RootHandle + LinuxWorkTree + LinuxObjectPort` 跑一轮
  `ExternalObserver::run_pass`（周期扫描是 S050 三层兜底的正确性层；**未接 inotify**，只影响时延）。
  put 发布通过 `expect_own_write` 登记（设备 + inode + 内容）；其余命令的自写关联仍待验证。
  HTTP 执行器实际复用观察器的同一个 gate（锁序 gate → roots），不是只复用 gate 的类型。
  运行时用独立线程 + current-thread runtime（端口与 `RootHandle` 不要求 `Send`），优雅停止时先停
  观察循环再关闭数据库。
- **新增存储函数**：`entries::entry_by_path(conn, owner, root, path)`、
  `entries::path_of(conn, owner, entry)`、`entries::advance_entry_revision(tx, owner, entry, expected, next)`
  （比较并写，变化报 `EntryError::RevisionChanged`）；`RootHandle::root_dir()` 暴露已核对的根目录句柄。
- **恢复验收入口**：`cargo test -p vaultsyncd-server-tests --test {commands,restore,conflict,dir_restore,recovery_boundaries,kill_boundaries}`；`kill_boundaries` 用 `server-child` 真实子进程（触发器卡住写事务后 SIGKILL）。
- **验收入口**：`cargo test -p vaultsyncd-server-tests --test http_loop`（真实监听 + 真实 SQLite +
  btrfs 工作目录）。满盘用例需要小型专用 btrfs 卷（`tools/test-volume/test-volume.sh create <run> 192M`），
  用 `VAULTSYNCD_S052_WORK_VOLUME` / `VAULTSYNCD_S052_STATE_VOLUME` 指定卷内可写子目录
  （准入要求状态目录与 Root 同一 btrfs 文件系统）。

## crates/agent-core — 首次绑定与可恢复合并（S058）

路径：`crates/agent-core/src/bindings/{import,store}.rs`；`bindings::import::FirstImport` 只用于
没有共同基线的新绑定。已有 baseline、remote/local 状态、事件、意图、队列或非零游标时拒绝
重新首次合并，旧设备必须走带基线重建。编排器不做平台文件 I/O，不生成 Linux 客户端。

| 接口 | 契约 |
| --- | --- |
| `FirstImport::new(database, Ports, ImportLocal, ImportSnapshot)` / `run(binding, ids, max_items)` | 绑定已由配置层持久化；按根准入 → 固定快照及分页检查点 → 完整本地扫描 → S020 逐路径规划 → 持久清单与稳定 OpId → 有界执行推进。`max_items=0` 可只保存清单；恢复复用原快照、清单、改名目标与身份，不依赖内存注册表。返回 `ImportReport{completed, high_water, complete}`。 |
| `ImportLocal: NameEquivalence` | `validate(binding, other_paths)` 用平台规范路径/根句柄验证普通健康根、缓存/内部状态/其它绑定不重叠、名称可表示，并给跨重启根身份；`scan(binding)` 必须完整，文件含大小、SHA-256 与 generation；坏扫描报错，不能返回空清单代表失败。原生名称等价由平台注入，同目录非同拼写的等价名称明确拒绝，不静默改名。 |
| `ImportLocal::preserve(binding, op, from, to, generation)` | 平台先持久本地意图，核对代际与目标未占用，幂等另名保留原内容；目录冲突连同子树另名，子项后续通过最终父目录身份上传。重放必须验证现场身份/内容，不能仅凭目标存在就成功。 |
| `ImportLocal::install(&ImportInstall) -> Generation` | 输入持久 install OpId、目标、完整 `SnapshotEntry` 与模式。必须核对目标仍为空，固定 VersionId 下载/校验/发布，Full 不得降级为占位；OnDemand 才可建占位。崩溃窗口由同一持久意图核对，不覆盖扫描之后的本地新内容。该端口是 S059/Windows 适配接线点。 |
| `ImportSnapshot::begin(root) -> SnapshotSeed` / `page(root, snapshot, cursor)` | 直接复用生成的 Snapshot/SnapshotPage；seed 另带认证响应的服务器观察时间，只用于提前固定的冲突标签，不伪造提交时间、不以客户端时钟决胜。每页检查相同 snapshot_id/high_water、严格 EntryId 顺序、前进游标、完整计数和父目录树；到期/错误不能悄悄换快照。 |
| 内部 `ImportStore: UploadCommitStore` | 仅处理持久导入清单中的 OpId。文件复用 S056 固定 generation 上传，目录 create 不带内容；两者均 `expected_absent=true`、固定 server_epoch，不以旧快照覆盖新创建。committed ACK、临时→稳定 EntryId、baseline/remote、本地 generation 条件清 dirty 与日志同一事务；旧代际 ACK 保留新 dirty。pending/finalize 不算完成。 |

- 同路径同内容只绑定远端 EntryId；内容/类型不同先保留本地另名并上传，再安装原路径的固定远端版本。
  首次导入没有删除动作，不以 mtime 决胜。新建服务器同名条目的并发变化仍由 create CAS 拒绝。
- 清单执行完成后，合成根 baseline、`received_seq=applied_seq=snapshot.high_water_seq` 与 complete
  在一个事务中发布；不是用上传回执中的较新 root_seq 初始化游标。S057 `run_cycle` 与直接
  `resume_pending` 在导入未完成时拒绝接收/应用，完成后继续按原 high_water 拉 changes。
- 运行者用日志 revision 条件更新防止旧内存覆盖 ACK。应用入口应在导入期间独占该绑定的通用队列
  调度；导入自行从日志重建 UploadSpec，不能让另一 S055 执行器抢跑尚未注册规格的上传。
- 并发提交若返回 `OperationResult::conflict`，持久化主/副本完整回执及稳定副本映射，但响应不含
  最终路径，不能猜名称写 clean；清单保持未完成，等待取得最终路径后恢复。普通首次合并的
  同名异内容已经通过固定冲突目标 + create CAS 完整执行，不受这条并发异常限制影响。

### 客户端迁移 0004 与恢复证据

`crates/client-store/migrations/0004_first_import.sql` 追加 `binding_imports`（binding 主键、内部版本化
JSON 日志、revision、complete）和 `import_id_map`（binding+temporary_id 主键、稳定 entry_id、
唯一 op_id、原始结果 JSON）。外键随 binding 级联；不改写 0001—0003，不给旧绑定自动制造首次
导入状态。0003→0004 测试验证旧 pending_ops/upload_resumes 保留及升级前一致性备份。

`bindings/import/{tests,safety_tests}.rs` 用真实客户端 SQLite 与磁盘账本，在独立进程中分别于
快照分页、远端 committed 未回 ACK、保留改名后、固定版本安装后直接退出（71/73/72/74），
随后重开数据库完成。验证非空目录、同内容归并、内容与两种类型冲突、子树与临时 ID 映射、
CAS、旧代际 ACK、未定位冲突保留，以及从 high_water=7 接收/应用其后 5 个完整事件批次。
测试专用磁盘适配只在 cfg(test) 编译，不能替代 Windows 原生能力验收。

未接线：生产 HTTPS 快照/服务器时间适配、Windows 根扫描与意图/安装、生产 EventDecoder 仍由应用
入口提供；导入专用 ACK 适配不冒充所有 S056 上传的通用 baseline/冲突适配。当前 S052 的 create
运行时仍显式未装配，本卡未修改服务端路径；本卡证据是 L 环境真实持久恢复，不宣称产品端到端可用。

## crates/agent-core — 本地受控内容应用与重启核对（S059）

路径：`executor/apply.rs`、`executor/apply/store.rs`、`recovery/mod.rs`。平台与 HTTP 由入口组装，
纯核心不进行文件 I/O。只处理完整文件内容安装，不生成 Linux 客户端，不冒充 Windows 原生验证。

| 接口 | 契约 |
| --- | --- |
| `ControlledLocal::observe_content(path)` | 健康根的 `Absent`，或 `File{LocalFacts, Arc<SnapshotSource>}`。固定快照对应活动文件代际；读取错误、挂载失效、扫描中均返回错误，不返回缺失。稳定身份不可空，使用状态必须 Idle；平台不能只凭 mtime 或数据库 clean 维护 generation。 |
| `recovery::inspect(local, path)` | 流式读取固定快照（窗口 ≤256 KiB），读取后再次核对活动路径、类型、身份、generation 与使用状态。返回 `ContentEvidence::{Absent, File{generation,identity,digest,size}}`；未知不产生破坏授权。每次应用都调用，重启不缓存旧 clean 结论。删除/释放适配器也必须先完成真实核对，未扫描条目保持 unknown。 |
| `ControlledLocal::replace_verified(request, before)` | 只在持久意图存在后调用。在平台使用保护/独占句柄内重新核对完整 before，包括真实内容、身份和 generation，再安装重新校验的暂存并持久化文件/父目录。Absent 要求不覆盖创建；不能用无保护的 observe+rename 实现。任何二次修改返回 LOCAL_CHANGED。 |
| `ApplySpec` | 固定 OpId、BindingId、路径、比较并写的旧 baseline（首次为 None）、已提交新 baseline、VersionPin（版本/摘要/长度）、原确认内容证据。拒绝非文件、删除、版本/身份不匹配和倒退 revision；不接受上传 finalize/pending 作为提交证明。 |
| `ApplySpec::for_import_file(&ImportInstall)` | S058 Full 文件安装桥接，固定 SnapshotEntry 版本/摘要/大小，expected=Absent；可在 ImportLocal::install 中交给同一 ControlledApply。目录和 OnDemand 占位明确 Unsupported，不静默降级 Full。 |
| `ControlledApply::apply(spec, content, temp, progress, options)` | 重哈希真实前置内容 → S054 固定版本下载/暂存同步/流式整份校验 → 同事务写意图及重放上下文 → 再核对并平台条件替换 → 再核对真实新内容 → baseline/local/完成记录同事务。失败保留队列、旧基线、暂存与意图；相同 OpId 输入不同拒绝 OP_ID_REUSED。 |
| `ControlledApply::recover(binding)` | 从 SQLite 按 intent_id 恢复，不依赖原内存规格或网络。现场等于目标摘要/长度只补基线；仍等于 before 才可条件重做；第三种内容、占用、故障或新 DB generation/dirty 保留现场。返回逐操作 Result；读清单失败或存在未归属的旧/其它平台意图返回整体 Err，不得宣布启动恢复完成。 |
| `ControlledApply::specification(op)` | 只读返回校验过的持久固定输入，包括已完成记录；应用 ACK 重启不从已变化的 head/基线猜旧 spec。 |
| `ControlledApply::apply_after_confirmed_copy(..., copy_op)` | 仅用于 preserve-both 的原 head 恢复：copy 必须是已完成同 binding、不同 entry 的固定版本发布，摘要/长度等于 source expected；原 dirty 的物理身份、generation、fingerprint 和真实内容仍逐项 CAS。证明随 journal 持久，恢复重新校验；不能作为通用强制覆盖。 |

客户端迁移 `0005_controlled_apply.sql` 追加 `controlled_applies`：OpId 主键、绑定、目标、唯一
local_intents 外键、完整 JSON 重放上下文、completed。开放目标唯一；意图与日志同事务创建，
baseline/local 的 generation/身份/摘要与 completed、意图删除同事务完成。完成记录保留以防
导入/事件在下一层确认前重放旧安装；再次调用也先检查现场，不覆盖新字节。0001—0004 未改写，
0004→0005 实迁移测试保留旧意图、导入/ACK 映射及迁移前一致性备份。

消费者围栏：S058 finish_item 保留已有 local 行（含受控发布的指纹/身份或新 dirty），不倒退
baseline；S057 应用批次不回写更旧 revision，同 revision 不同事实报错，applied_seq 仍只在
全部动作与基线成功后推进。S056 上传 ACK 的 generation 条件事务及 pending 非 committed 语义不变。
接 S055 的 handler 应令 plan_intent 返回 None，由 ControlledApply 校验下载后保存完整意图；
成功才 Completed/出队，失败不清意图。入口仍须串行调度同一 binding，启动恢复和逐条核对完成前
不得放开破坏动作；本接口不自动替用户处理未知旧意图。

验收：`executor/apply/{tests,consumer_tests}.rs` 使用真实 SQLite/WAL、磁盘暂存/内容与 rename/fsync，
独立进程在意图后、替换后 DB 未更新、基线事务后直接退出 81/82/83，后续进程重开核对恢复。
另覆盖旧 clean 同大小新内容、二次修改/创建、暂存篡改、操作复用、未知/旧意图、旧 ACK/新 dirty、
首次安装收尾与事件 received/applied 游标。测试平台端口仅 cfg(test)，不提供生产 Windows 使用保护。
生产平台、目录/保留移动、按需占位及事件 DTO 解码仍由相应适配/后续卡接线。

## crates/agent-core — 旧游标与服务器代际重建（S060）

路径：`recovery/resync.rs`、`recovery/resync/store.rs`；客户端迁移 `0006_resync.sql`（`binding_resyncs`、
`resync_quarantined_ops`，0001—0005 未改）。与 S058 首次导入是两个入口：已有旧基线的绑定只能走本入口，
首次导入 `FirstImport::run`、事件接收、`ClientStorePorts` 取队列/入队及 `ControlledApply` 都经
`ensure_unblocked/ensure_operation` 读取持久围栏，重建未完成时拒绝。无旧基线的新绑定调用重建报错。

| 接口 | 契约 |
| --- | --- |
| `Resync::new(db, ImportLocal, ControlledLocal, ImportSnapshot)` | 只用健康根准入/完整扫描与 S059 真实内容核对；不调用 preserve/install，不做 Linux 产品客户端。 |
| `Resync::run(binding, ResyncReason)` | 认证协议错误触发。`CursorExpired`（同 epoch 历史被回收）或 `ServerRollback{new_epoch}`（备份恢复，相同 seq 属新 epoch，高水位可低于旧值）。第一笔事务写围栏、保存旧 baseline/remote/事件，旧队列操作永久隔离。 |
| `Resync::resume_pending(binding)` / `latest_report(binding)` | 从 SQLite 续接未完成重建（重新真实核对，不信任旧 clean）/ 只读最近持久结果，不重扫。 |
| `ResyncReport{prepared, complete, items}` | `prepared` 不是 applied。有任何 `RecoveryCandidate/Reconcile/Review` 时 `complete=false`，游标、旧基线、队列不动。 |
| `Disposition` | `Current`、`RetiredClean`（远端已无的历史 clean，保留本地、不重传）、`RetiredAbsent`（本地/远端皆无，不推导 Delete）、`RecoveryCandidate`（脏缺失，或 `ServerRollback` 下远端缺失、本地完整副本失去确认；不复用原 EntryId）、`Reconcile`、`Review`（unknown/占位/来源不明/同路径新身份）。 |

结果对照（同一份本地内容、远端均缺失）：`CursorExpired` → `RetiredClean`，可 complete；`ServerRollback` →
`RecoveryCandidate`，不 complete，需用户/后续恢复入口处理，二者都不上传复活、不重放旧 Delete。
发布（`store::finish`）单事务：核对每条本地行与分类时 `LocalStamp`（generation/dirty/residency/指纹/身份）完全一致，
否则保留围栏；通过后同事务写新 remote/baseline、server_epoch、received_seq=applied_seq=快照 high_water、
quarantine 旧操作、完成日志。历史 tombstone 与同路径新身份按身份而非路径比较，不折叠。

限制：生产端口（Windows CFAPI、HTTP 快照分页）、恢复候选的处理 UI/入口、`resume` 调度由后续卡接入；测试磁盘端口仅 cfg(test)。

## crates/agent-core — 绑定控制与可恢复模式任务（S061）

`bindings::control::BindingControls::new(Arc<Database>)` 提供 `set_paused`、`change_mode`、
`resume_mode`、`mode_progress`、`change_rules`、`acknowledge_reconciled`、`preview_unbind` 和
`unbind_keep_data`。每绑定入口串行组装；平台与网络不进入 SQLite 事务。

| 接口/结果 | 契约 |
| --- | --- |
| `execution_gate(binding, Option<EntryId>) -> ExecutionGate` | 启动新动作前调用；仅 `Ready` 可执行，`Paused/Unbound/Excluded/ReconcileRequired` 保留队列。含 excluded/reconcile 后代的目录也阻塞，不能从本地缺项推导父目录 Delete。存在规则但目标位置未知时保守要求协调。 |
| `change_mode` / `resume_mode(..., &dyn FullMaterializer, budget)` | full→on_demand 只改配置；on_demand→full 先落持久任务，按预算核对/补齐。逐文件保存与远端版本、路径、本地 generation/dirty/驻留/身份/摘要绑定的证据；全部未排除文件通过才 complete。新本地/远端事实使旧证据失效。 |
| `FullMaterializer::ensure_full(&FullFile) -> LocalEntry` | 入口适配器用 S059 下载/受控安装固定版本，已有完整内容也须实际核对内容/身份/代际。脏内容只能核对保留。返回后复核数据库相同事实与已应用基线；仅下载、旧 clean 或未应用新远端版本都不能完成。 |
| `change_rules(binding, version, text)` | 接收完整规则文本，复用 S018 gitignore 解析。记录已有条目 excluded；取消排除变为 reconcile_required。规则版本永不复用，当前相同请求幂等；旧版本 ACK 不放开新规则。协调完成后调用 `acknowledge_reconciled`。 |
| `UnbindReport` / `unbind_keep_data` | 提示 unconfirmed 条目、incomplete 驻留及恢复导出选择；仅 detached+paused，不删除 bindings/本地行/基线/队列/文件，不发全局 Delete。恢复不能隐式重新绑定。 |

客户端迁移 **0007**：`binding_controls`、`binding_participation`、`binding_full_proofs`、
`binding_rules_history`，旧迁移不变。中断后原任务/证据保留；独立进程退出 101/102/103
分别证明意图后、单文件完成后、整任务完成后恢复。6→当前升级保留 awaiting_commit/队列并创建一致性备份。
生产 Windows materializer 和管理 IPC 入口在后续平台/应用卡组装；测试磁盘适配器仅 cfg(test)。

## S062：持久优先级调度与上传错误证据

来源：design §13.3、§18.2；实现`crates/agent-core/src/engine/scheduler{.rs,/model.rs,/store.rs}`、迁移`crates/client-store/migrations/0008_scheduler.sql`。

- `Scheduler::new(Arc<Database>, Ports, ScheduledHandler, Arc<dyn DispatchGate>, Arc<dyn Jitter>, Limits)`：数据库与队列端口必须同源；单进程共享一个调度器，各绑定共享资源上限。`run_round(binding)`可并发调用以利用用户保留槽；按`RoundReport::next_due`或外部唤醒再次运行，不立即空转。默认content=4（后台最多3）、metadata=1、hash=2，同entry跨类别互斥，无entry保守串行该绑定；优先级不使用mtime或大小。
- `prioritize(op, Priority::UserRead)`仅允许下载，不能用metadata/hash绕过内容上限；metadata来自明确元数据操作，`hash_content`是仅由实现它的ScheduledHandler处理的内部队列任务。原OpId/版本pin/依赖不因重试改变；前置仍在任何绑定队列即不启动依赖方，只有真实Committed后解除依赖并出队。
- `DispatchGate`已有`BindingControls`生产实现：仅`ExecutionGate::Ready`可启动；暂停/排除/解绑/重协调均保留任务。管线进入、动作意图落库后各复核一次，后者变化返回Deferred而非Cancelled。更底层原子平台动作仍承担使用保护与代际CAS；取消已提交动作不能撤销既有Committed或清新dirty。
- `ScheduledHandler: ActionHandler`沿用`plan_intent→持久意图→动作`，额外接收与S054完全相同的`CancelToken`及返回`TaskError`。`UploadActionHandler`已实现并直连分块取消；`ActionAdapter<H>`兼容旧handler，但仅可在动作开始前取消。需活动取消的下载handler应直接实现新端口并传token给DownloadOptions。
- `ActionFailure::scheduling: FailureScheduling`默认为ByCode；Unavailable携带Retry-After，Cancelled不累计失败，LocalPermission区别于HTTP403。上传端口与分块失败保留这些类型化证据，NoSpace映射InsufficientStorage；不解析detail、不保存敏感诊断到调度表。
- `operation_schedule`与pending_ops同寿命，持久priority/due_at/cancelled/halted/offline；失败attempts/last_error与调度due/halted/offline同一事务。指数上限默认1—60秒，取半区间正jitter；Retry-After为下界（可超过60秒），UTC秒存储向上取整。停止/取消不删队列、续传、意图或本地字节；显式resume不重置失败次数。普通不可用只在TaskStatus可见，严重错误由needs_attention区分；通知/去重归S064。
- `Outcome::Pending`始终保留队列，等待至少1秒；重启只重建进程内活动槽，不把in-flight或已传完内容当Committed。v7→v8迁移前备份保留暂停、dirty、失败队列，旧schema校验和不变，旧二进制拒绝v8。

## S065：Windows 安全元数据与通知端口

实现：`crates/platform-windows/src/files/`、`watcher/`。平台 I/O 不进入纯核心；只提供观察事实与补扫要求，不直接生成全局 Delete。

| 接口 | 输入/输出与安全契约 |
| --- | --- |
| `RootHandle::open(Path)`、`verify()` | 仅现有本地驱动器绝对目录；逐分量 `NtCreateFile(RootDirectory=已持有父句柄)`，`FILE_OPEN_REPARSE_POINT + FILE_OPEN_NO_RECALL`，持有祖先句柄并禁止 delete 共享。类型中性打开后核对目录；不使用本客体返回参数错误的 `FILE_DIRECTORY_FILE` 组合。verify 重新核对身份/路径及当前 ACL（无 backup intent 的相对重开），旧句柄授权不代表新 ACL 仍可访问。 |
| `FileIdentity{volume,file}`、`RootHandle::overlaps` | 真实卷 serial + 128 位文件 ID；以祖先/自身目录身份判父子与别名，不能用文本 starts_with；subst 原生证明同一物理目录仍重叠。此身份不替代协议 EntryId。 |
| `inspect(relative)`、`scan()` | 直接复用 fs-common `RelativePath` 和名称边界；句柄相对 no-follow，拒绝未知 reparse/junction/symlink 与跨卷；原名不改写。目录使用 `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` 从句柄枚举，不按路径递归，不读内容；迭代扫描限制深目录持有句柄数量。 |
| `EntryMetadata` / `CloudMetadata` | 文件 ID、逻辑/分配大小、attributes/tag、links、时间戳；CFAPI 的 on_disk/modified/pin/in_sync/provider identity 来自同一句柄 `CfGetPlaceholderInfo`。CFAPI 过滤器可能隐藏 reparse 属性，不能仅靠 tag 判断占位。identity <=4 KiB；查询失败明确报错。mtime/in_sync 不作为业务 committed 证据。 |
| `ScanReport{root,health,entries}` | 保留局部问题；`Complete/PermissionDenied/RootUnavailable/Incomplete`。任一未知对象、硬链接、名称碰撞或枚举失败使 `permits_missing_comparison=false`；只有完整健康扫描可以进入核心的缺失核对，仍不直接授权删除。 |
| `NativeNames: NameEquivalence` | Windows `CompareStringOrdinal(ignoreCase=true)`，API 失败保守报冲突；真实 NTFS case/Unicode 创建验证。不归一化、不静默选择赢家，大小写敏感目录仍保守报问题。 |
| `DirectoryWatcher::new(&RootHandle, buffer_bytes)` / `poll(timeout_ms)` | 用相同父句柄相对重开并核对 root，`ReadDirectoryChangesW` 原生异步监视；缓冲限16 B..64 KiB、poll <=60s，回传 `Idle/Changes/Rescan`，删除/rename 通知只是提示。溢出/失联/畸形记录均需要全量补扫；drop 取消并等待完成后才释放 OVERLAPPED/缓冲。filter 不含 last-access，避免扫描反馈。 |
| `ReconcileGate` | 启动/非 Idle 提示即需要补扫，周期元数据扫描覆盖通知丢失和重装间隙；不健康扫描保留围栏。调度/持久基线/受控发布由 agent 入口及后续卡接线。 |

原生验收入口：`VAULTSYNCD_WINDOWS_CACHE=/wrd/prj/vaultsyncd/.cache/windows nix develop .#windows --command tests/platform/windows/run-native-files.sh`。仅在实验室 NTFS run_id 目录/临时盘符操作，并核对 exe 传输 SHA-256；52 个客体测试包括已 pin 空目录真正原地设置 junction、ACL 变化、真实内核缓冲溢出与三个脱水占位符扫描前后 0 本地内容字节。环境分工、VM 保持在线规则见 `docs/operations/windows-lab.md` §12、§19.1。

API 依据：[NtCreateFile](https://learn.microsoft.com/en-us/windows/win32/api/winternl/nf-winternl-ntcreatefile)、[CreateFileW no-follow/no-recall](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew)、[ReadDirectoryChangesW 溢出语义](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-readdirectorychangesw)。

## crates/agent-core — 显式受控释放与版本租约（S063）

`executor::release::ManualRelease::new(Arc<Database>, Arc<dyn ReleaseLocal>, Arc<dyn ReleaseRemote>)`
提供 `release(&ReleaseSpec)` 与仅恢复开放日志的 `recover(binding)`。没有缓存挑选或自动淘汰入口。

| 接口/事实 | 契约 |
| --- | --- |
| `ReleaseSpec` | 固定 OpId/BindingId/EntryId/RelativePath/VersionPin；pin 的版本须为本地共同基线确认内容。复用 OpId 必须保持整个请求，原队列同 OpId 只能是同目标/基线/固定版本的 `release_local`。 |
| `ReleaseRemote::acquire(pin) -> Box<dyn VersionLease>` | 服务端原子确认版本可读并建立短租约，保护完整条目/版本/摘要/大小的内容对象免于 GC。核心比对完整 pin，`ensure_valid` 在安全点确认或续约；过期、断网、版本丢失都失败。持有者不得在完成前主动撤销保护。 |
| `ReleaseLocal::protect(path) -> Box<dyn ReleaseProtection>` | 从取得 guard 到 DB 驻留 CAS 完成，持续禁止并发写、替换和删除；无法保护则明确拒绝，短暂共享模式探测不能代替持续保护。 |
| `ReleaseProtection::content` / `released` | 前者供 S059 `inspect` 固定快照流式核对实际内容/代际/身份；后者只查元数据，返回可信固定 pin/身份/代际的 `ReleasedFile`，不得意外 hydration。未知/部分证据报错。 |
| `dehydrate_verified(expected, lease)` | 在同一使用保护内重新核对完整真实 before 及租约有效性，平台成功并持久化后才返回已释放证据。guard 和 lease 保持到核心完成 SQLite 条件事务。 |
| 持久恢复 | 0009 `controlled_releases` 与 local_intents 同事务保存请求、前态与证据。平台已脱水但 DB 旧 materialized：仅按身份/generation/pin 补 DB；内容/dirty/基线/epoch/绑定控制变化均保留意图并拒绝。完成日志不允许旧请求清除新本地内容。 |

释放前检查 S061 门禁，full 精确返回 `MODE_REQUIRES_LOCAL_COPY`，其它暂停/排除/解绑保持阻塞。
任何 dirty、冲突/待办、未知指纹/身份、部分驻留或未恢复意图均不授权释放。成功事务只将驻留改为
metadata_only，保留内容基线与原代际，不生成远端 Delete。`controlled_releases.completed=0` 始终是未完成状态。
原生 CFAPI 使用保护与服务器租约适配由 S069/应用入口接线；测试磁盘适配器仅 cfg(test)。

## S064：状态、Root问题与恢复导出

`status::snapshot(&Database, BindingId)` 在同一SQLite读事务汇总多维状态；S061 `mode_progress_in` 仅crate内部复用，public API不变。`BindingStatus::synchronized` 必须无dirty/队列/传输/事件差/未知/开放意图/恢复围栏且full证明完整。metadata_only不表示同步完成，完整缓存也不自动淘汰。

`status::issues::report` 按Root+ErrorCode持久合并严重原因，第一次/恢复后复发才返回notify；不绑定单个Binding外键，解绑不删其他绑定的问题。普通网络原因不创建通知，用户打开失败仍返回explain_open。`resolve_root` 仅在Root级所有相关绑定同原因均恢复后由调用方调用。`diagnostics::bounded` 只导出类型化身份、错误码、耗时整行，限制总字节，不接受自由文本/凭据/内容。

`recovery::export::ExportRecovery::run(binding, destination, ExportSource, ExportDestination)` 通过端口固定快照并stream导出，`Target` 区分current/candidate和snapshot；当前dirty与旧固定上传快照分别保存。源discover必须完整或报错，末尾复核新增候选/每个来源与SQLite状态。目的create_new必须使用安全目录句柄、拒绝reparse和覆盖、保持身份、拒绝同步/缓存重叠；ExportDirectory保证create_new/顺序写入/读回/fsync。核心不做平台I/O；测试磁盘适配器不构成生产Windows实现。

文件命名仅kind+稳定身份，state.json包含绑定/epoch/模式/游标、三视图、待办、上传快照事实、恢复日志和隔离操作（排除会话token）；清单带文件长度与SHA-256。写完读回校验、状态未变化后最后写manifest.json，并持久化后validate。validate要求格式、有效绑定、唯一state.json、合法名字/来源、固定大小摘要与state绑定一致；失败返回明确错误，保留部分目录、不清dirty、不覆盖、不宣称完整。缺失快照、部分不可恢复内容同样明确失败。生产Windows快照与安全目的目录适配、IPC接线由应用入口卡承担。

### S066：正式 Windows CFAPI 与 Explorer 生命周期

`platform-windows::cfapi::catalog::{Identity,Catalog}` 使用 domain 的 BindingId/RootId/EntryId/VersionId。
正式 identity v2 固定 72 B：`VS/2/0` + 四个 16 B 身份 + CRC32；v1 原型保持独立，生产解码拒绝旧版、损坏、外根、未知 Entry 与陈旧 Version。Catalog 由应用从持久状态重建，更新后显式发布；CRC 不能代替业务授权。

`cfapi::adapter::Adapter::connect(path,Catalog,Arc<dyn Callbacks>)` 为每个连接持有独立上下文，不用原型进程全局 provider。`Callbacks::fetch(ReadDemand)->Result<(),NTSTATUS>` 只入队；demand 包含完整身份、文件大小、固定范围与 `Reply`（真实 connection/transfer/request keys、拥有的 correlation vector）；`cancel(Reply)` 仅取消对应传输，`disconnect()` 发停止信号。`Reply::transfer` 只在连接存活期间回执，异步工作须由消费者在关闭前停止；下载/整份校验/有界队列由 S067 实现。`MetadataOnly` 明确返回 Unsupported，不能冒充水合成功。

`create` 使用 pinned parent 相对 CREATE_NEW + CfConvertToPlaceholder + CfUpdatePlaceholder；不覆盖本地文件，不手改 reparse。创建和空占位更新用排他共享句柄；只有无本地内容、无修改/保留意图且预期物理/逻辑元数据相符时更新版本/逻辑大小。mark-in-sync 仅表示已成功安装的远端空占位元数据，不能替代核心 committed 基线；hydrated/dirty/pinned 文件保留给 S068。`move_entry` 使用 NtSetInformationFile 的目标 RootDirectory 相对 rename，拒绝覆盖，不打开内容流；Windows rename 可清除 OS in-sync，不能据此清业务基线。核心调用方负责先持久 local_intent、校验 generation，再调用平台并持久回执。失败留下实际中间状态，不能假定完成。

`files::RootHandle` 只允许同步根末分量为已识别 cloud reparse；其祖先仍拒绝所有 reparse。新增 crate 内相对 create/open 工具复用 S065 身份围栏。断开的 partially-populated 目录在 Windows build 26300 可返回 FLT_INVALID_NAME_REQUEST；扫描明确 incomplete、禁止推导删除，连接存活时元数据扫描零内容供给。

`platform-windows::shell::{register,registered,unregister}` 通过 WinRT StorageProviderSyncRootManager 管理 Explorer；ID 为 `vaultsyncd!当前SID!BindingId`，Context 为 Binding+Root，必须先初始化 WinRT。注册先核对 CFAPI 根身份，拒绝接管外根。注销与 Adapter 同样先拒绝任意占位/不完整扫描。**CfUnregisterSyncRoot 本身会永久删除未水合文件和未完全填充目录**（[微软行为契约](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfunregistersyncroot)）；生产适配器要求先显式物化并 revert 全部占位，当前卡不实现该流程，不能把拒绝当已注销。`Drop`/`disconnect` 从不注销、不删文件；连接断开失败保留回调存储，避免悬垂。

原生入口 `tests/platform/windows/run-native-adapter.sh` 在 Windows devShell 交叉构建包内测试、校验两端 SHA-256、在真实 Windows NTFS 运行；日志只落 `artifacts/S066/`。S066-a/b 是本卡内身份/适配器与原生/Explorer 验收拆分，父卡全部验收后才 done。

### S068：Windows 内容观察、持续保护与可恢复发布

`platform-windows::files::controlled` 是同步 blocking 平台 API，不依赖 agent-core/store。应用在 worker 中调用并映射 `ControlledLocal`/`LocalPort`，不持数据库事务：

- `WindowsFiles::open(root,state)`：两个既有、同卷、不重叠的安全 pinned 目录；`seed(path,Evidence)` 从 durable local 状态载入旧代际，下一次仍重新哈希真实内容。
- `observe(path) -> ObservedContent::{Absent,File(Arc<FixedSnapshot>)}`：健康父目录才能证明 Absent；文件在独占 RH oplock 下流式固定，`Evidence{generation,volume,file,digest,size}`；新字节或新物理身份递增代际，mtime 不授权。`FixedSnapshot::read_at` 最多 1 MiB，句柄拒绝外部修改/删除。
- `protect(path,writable) -> (ProtectedFile,EntryMetadata)`：元数据 no-follow 后取得正式 CFAPI oplock，再核对身份。`ProtectedFile::lease -> ProtectedLease`，`lease.file()` 为借用 Win32 句柄；不得 close/duplicate。oplock 已破坏、部分内容、可写映射/使用中、未知对象均报错。S069 可在 lease 内再次复核版本/摘要后调用 CFAPI。
- `replace(op_hex,target,staged_path,before:Option<&Evidence>,digest) -> Evidence`：调用前先持久 core local_intent。内部 stage 固定并再次在 oplock 保护中核对；平台日志绑定 OpId、目标、根物理身份及前后身份/摘要，sync 后才移动旧对象到独立 state，随后不覆盖发布。两段发布存在可恢复间隙；用户填入间隙时拒覆盖，旧内容/新stage/用户文件均保留。
- `recover_publications() -> Vec<RecoveryOutcome>` 必须先于 core recovery 和同步；后像须身份及摘要同时匹配才 Published，原对象完整且目标不存在才 Restored；第三种内容 Blocked，损坏日志/路径/根身份错误报错且保留。`acknowledge_publication(op)` 仅在 core baseline 事务完成后清对应日志；preserved 原对象及快照由显式恢复/保留策略处理，不自动清除。

应用以同 OpId 关联两层意图，不能只因平台返回 Published 就推进应用游标或宣称 committed。发布出的普通完整文件的业务 CFAPI identity 由应用根据已确认 Entry/Version 安装，平台不猜服务器身份。S068-a/b 记录原生能力边界；原型 `cfapi::release` 三次探测即关句柄不构成正式持续保护授权。

## S069-b 固定版本内容短租约

- Wire唯一源 `api/openapi.yaml`：`POST /v1/roots/{root}/entries/{entry}/versions/{version}/leases` 与 `/{lease}/renew`、`/{lease}/release`，body为稳定 `lease_id`、整份 `sha256`、`size`。响应完整回显pin、UTC到期及剩余秒数、closed。仅Bearer/授权Root可操作。
- `server-store::version_leases`：同BEGIN IMMEDIATE核对固定版本与live对象，使用既有`object_leases`作为GC保护；0010请求关联记住固定输入，ID复用换输入409，失效/释放410；TTL120秒，acquire重放不延长，renew仅活跃，release幂等。活跃对象租约阻止collectable标记，GC仍按既有租约与引用集工作。
- 服务端租约已提交后才检查对象可读和长度，失败503并保留有界保护，绝不返回可释放证明；客户端`LeaseEndpoint`精确校验Root/Entry/Version/hash/size/id/TTL/closed。调用方必须保留本地副本直到有效租约及持续平台保护均成立；传输成功不代表租约有效。
- 平台释放与S063薄wrapper由S069-a/c完成，见下节；本子卡自身不冒充Windows原生或完整进程组装。

## S067 — CFAPI hydration 与传输/调度组装

- `agent-core::engine::scheduler::{UserReads,UserReadGuard}` 由生产 `Scheduler::user_reads()` 提供，复用内容并发池、per-entry 排他及持久绑定门禁。`try_acquire(request,binding,entry)` 返回 guard/忙/明确拒绝，不写同步队列、基线或 committed。调用方有界等待且响应取消。
- `apps/agent::bootstrap::{connect_hydration,HydrationConfig,PinnedVersion}` 真实装配 HTTP `ContentEndpoint<ReqwestTransport>`、SQLite `ClientStorePorts`、上述共享资源与 S066 `Adapter`。输入为已恢复 DB、认证客户端、Catalog、固定版本 size/SHA-256、独立内部stage目录、超时/块/在途上限。`HydrationCallbacks::register_version` 先登记不可变版本，再发布Catalog；同一版本改变事实或外来Root/Binding拒绝。在途请求持有原pin。
- callback只入有界队列/置取消；独立runtime先整个固定版本下载/读回SHA-256验证，再按请求范围64KiB分段 `Reply::transfer`。错误、取消、超时不返回假字节，部分进度保留；`cancel_hydration`/真实 `CANCEL_FETCH_DATA` 共用token。取消通过内容端口返回执行器，必须先落已完成位图，不能直接drop整份下载future。`disconnect`停准入并取消，`wait_idle`供启动/退出围栏，CFAPI回执键和correlation始终取S066拥有副本。
- `platform-windows::files::staging::StagingFile` 是内部目录的安全单文件 TempContent 实现：no-follow相对句柄、普通单链接、独占读写、写后sync、剩余容量检查；拒绝与同步Root重叠。已校验stage最多保留16个独占句柄供连续范围请求复用；未命中/重启重新核对整份摘要，关闭句柄不删除内容。只丢可重建暂存，用户完整缓存不自动回收。驻留在下载开始后保持partial，不凭HTTP下载完成改写baseline或清dirty，后续扫描证明完整驻留。
- 状态查询 `HydrationStats` 分列成功供给、取消、实际OS取消回调、失败及字节；显式应用取消验证不能替代OS回调触发证明。S011原始build26300的OS取消触发限制继续保留。进程、凭据和启动恢复由S070组装。

## S070-b0 应用组装基础适配

- `http-client::sync::{snapshot,snapshot_page,submit,operation}` 仅实现既有schema端点：固定身份核对，原Op重试，pending不得携带committed事实。snapshot同时返回响应HTTP Date秒数；错误不替换快照、不取客户端时间。
- `RootHandle::ensure_directory(relative)` 在no-follow父句柄下创建/核对普通目录，现有文件和越界拒绝。`WindowsFiles::preserve(from,to,&Evidence)` 持续保护、完整核对后按句柄不覆盖移动；调用方先落持久意图，重启核对相同物理身份。文件字节和代际不变。
- S070-a `InstanceGuard` 必须在DB打开前获取并保留至连接池关闭；SID来自实际token，会话mutex加state独占锁。`CredentialVault` 保存current-user WinCred blob，网络logon无库时明确失败；配置/日志不含凭据。

## S069 Windows显式释放与恢复

- `files::controlled::ProtectedFile::into_owned_lease() -> OwnedProtection`：持续CfReference，Drop先release再close；`file()`仅借用Win32句柄，不能关闭或复制。S063从protect到网络校验/驻留CAS均持此对象，跨await的真实新writer原生测试被阻止。
- `cfapi::release::verified::ReleaseHandle::open(RootHandle,path,Identity,generation,size,empty_intent)`：安全相对路径+物理/domain身份；`evidence`实际整份hash；`is_released/released_identity`只查元数据，partial/未知报错；`dehydrate(Evidence)`持续保护内再核对，CFAPI实际成功并no-recall复核后才可收尾。调用者必须先持当前固定版本GC租约，不能凭clean标记调用；失败保留核心意图，pin恢复失败也如实返回。
- `apps/agent::release::NativeReleaseLocal::new(root,Arc<Database>,Identity,RelativePath,VersionPin)`：仅固定目标，保护前读当前代际/物理身份，供`ManualRelease`的ReleaseLocal端口。`HttpReleaseRemote::new(ApiClient<ReqwestTransport>,RootId)`：真实LeaseEndpoint，20秒heartbeat、低于30秒保守余量拒绝；失败不得清本地字节，Drop主动解除或留有界TTL。
- 验收入口：先`nix develop --command cargo build --locked -p vaultsyncd-server-tests --example release-native-server`，再Windows shell运行`tests/platform/windows/run-native-release.sh`。专用`apps/agent/tests/native_release.rs` target使用真实Linux生产bootstrap服务/真实租约/Windows NTFS；bridge manifest含短命测试token，仅0600忽略artifacts及临时客体文件，脚本清理tunnel/server/manifest/driver。宿主桥接example也可供S070真实无GUI验收复用。
- SQLite FULL证据来自专用测试DB的真实pager容量上限，未填满系统卷；后续物理卷故障与生产GC worker仍按计划验收。S070启动必须先平台publication恢复再core apply/release恢复，之后连接同步。

## S070 Windows 用户会话生产进程

- `vaultsyncd-agent --config <JSON> [--login-stdin|--once]` 组装真实 HTTP/store/core/Windows；凭据来自 current-user WinCred，GUI 不参与同步。配置和实际命令见 `docs/operations/windows-agent.md`。`--once` 不代表所有 pending 已完成；后台轮询复用原 OpId、续传与调度状态。
- 恢复顺序：SID mutex/state 独占锁先于 DB；平台 publication → 实际存在的受控 release → 应用所有权可证明的 Move/Import → core ControlledApply → 新同步。应用 namespace 恢复核对固定动作、物理身份、generation/hash、目标不覆盖，并在短事务一起写 baseline、完成意图及解除该动作依赖；未知/失败/歧义意图停写。
- client 0010 的 `agent_evidence` 保存不可变物理内容证据，`agent_versions` 保存固定版本 pin，`agent_actions` 保存原 Op 规格及 ACK；baseline/local/remote/queue 仍是各自当前状态唯一事实来源。新版升级有备份，旧 v9 writer 在写前拒绝；derived event spec 可由持久 received_changes 重建。
- namespace 与内容发现共用单次健康 metadata scan。收到完整确认删除时 `WindowsFiles::quarantine` 只把同一真实对象保留到内部状态；dirty 内容不被替换。目录删除需真实 subtree token 且前后固定快照与已知子树一致。历史自 ACK/旧事件不重新安装用户已移动或删除的文件。dirty 等待本身上传协调是 Pending，不作永久 halted。
- 正式 Adapter 目录 population=ALWAYS_FULL(3)，应用通过 snapshot/events 维护所有名字；内容仍使用固定版本按需回源。protected Win32 句柄的读使用存活 OVERLAPPED/event 并等待真实完成，rename 对 STATUS_PENDING 保留 IOSB/缓冲和 CfReference 至完成；不把 pending 当成功、不把异步句柄交给 std 同步读取。
- 验收 `run-native-process.sh` 使用真实 Linux bootstrap 与 Windows 实际进程，覆盖无 GUI 双向同步、原 Op 重启、冲突双方、namespace、开放 Move 后进程终止恢复、SQLite 三类工作收敛、第二实例和退出。测试 cleanup 缺失配置时只允许显式精确 run allowlist 且同步根已空；未知现场保留。S071 提供 named pipe，S072 提供管理命令；S074/S102 扩展多客户端及故障矩阵。

## S071 Windows 本机 named pipe

- `platform-windows::ipc::Endpoint::current(scope)` 从 process token 取 SID/session，生产 scope 固定 `vaultsyncd-agent`，名称 `\\\\.\\pipe\\{scope}-{SID}-{session}`；不接受外部路径/用户身份。`connect/request` 只连本机，校验 server PID 的 token SID，默认 identification-only SQOS；GUI/CLI 消费此模块不启动 CFAPI。
- `Server::bind(Endpoint, Fn(Request)->Future<io::Result<Response>>)` 首实例独占、拒绝远程、非继承句柄、protected DACL 仅当前 SID 完全控制；每个完整请求通过管道 impersonation 读取 thread token SID，立即 RevertToSelf，跨 await 不保留 impersonation。Revert 失败 fail-closed 终止，不能把借用身份交还线程池。
- 4 字节大端长度 + JSON，沿用 local-ipc schema 上限（1 MiB）；空/超长/畸形/未知命令/截断均不产成功答复。每连接顺序执行、跨连接最多 8 个请求执行任务（额外一个待连接 listener），超额立即关闭；完整帧、handler、write 各 5 秒 deadline，不因碎片到达重置。断连仅销毁该连接。`shutdown` 取消并等待所有连接；Drop 为紧急取消；`is_finished` 供应用检查 listener 故障。
- `apps/agent/src/ipc.rs::start(Arc<Database>,Vec<BindingId>)` 在真实恢复完成后、同步循环前绑定；调用 `agent_core::status::snapshot`，逐 Binding 一读事务，读取失败关闭请求连接，不用空数组/默认绿色替代。`get_status` 1.1 在 1.0 身份信封上加可选 `bindings`：dirty/三类驻留/pending/failed/offline/transfers/intents/recovery/event_lag/unknown 均十进制字符串，mode_pending/paused/detached 与核心 synchronized 分别展示。不返回路径/凭据，不承诺多 Binding 原子快照。真实进程始终填 `bindings`；纯 `Service` 未接状态源时省略。版本 1.0 请求继续兼容，管理命令/版本化推送仍属 S072。
- runtime 持有 Server 到退出，在关闭 DB 前 `shutdown` 取消并等待 IPC；监听异常结束会使主循环明确失败。`--login-stdin` 不启动状态 IPC。
- 原生入口 `tests/platform/windows/run-native-ipc.sh`：交叉构建后 Windows 11 真 named pipe + 临时第二本机账号验 ACL；同时验证 token 身份、分片/流水线、并发、有界积压/慢读写/handler、畸形/断连后可继续请求，以及真实 SQLite 状态缺失不得假成功/版本先检。测试账号在 finally 删除，不碰其他样本。
- 真实进程验收：先构建 `release-native-server`，再 `VAULTSYNCD_PROCESS_TASK=S071 tests/platform/windows/run-native-process.sh`（Windows devShell）。复用 S070 产品回归和三项 IPC 断言：离线 dirty/pending 明确未同步、畸形/部分帧断连后能继续查、恢复后 pending/intents/lag 收敛到0且退出释放管道。S071 模式独立 driver/artifacts，仅运行实际进程用例，不执行 S070 旧样本 cleanup。

## S072 本机管理命令与授权恢复读取

- `api/local-ipc.schema.json` 1.2 提供 21 项 `CommandName` 闭集：状态/订阅、绑定列举/创建/暂停/恢复/解绑/模式、问题列举与重试、文件下载/释放、恢复包导出、连接读取/设置/登录/登出、Root/历史/回收站列举与恢复。`CommandParameters` 由 `crates/local-ipc::commands::validate` 按命令复核必填项、拒绝多余参数、非规范身份、相对路径、越界文本和非规范修订；副作用仍由执行器复核，不能只信界面。`ManagementResult` 按命令只填实际结果；稳定错误码为 `invalid_argument`/`not_found`/`blocked`/`unavailable`/`unauthenticated`/`conflict`/`internal`，同步原因仍放在 `Problem.code`。
- `GetStatusResult.snapshot_version` 为进程身份加单调计数；`subscribe_status(after_version)` 长轮询推送全量快照，进程变化或游标丢失立即返回全量，不追加增量算法。`Problem` 聚合 pending/事件/意图/传输/通知/管理恢复的真实持久原因，`retry_problem` 只恢复已持久操作并在问题已消失时幂等返回。
- `apps/agent/src/runtime.rs` 以 SQLite `agent_connection` 和 `bindings` 为权威加载连接/绑定，允许未登录启动；管理命令进入单 mailbox 串行执行，创建/解绑/模式切换/连接切换与同步生命周期交错时重新核对执行门。登录凭据只写 current-user Credential Manager，状态/结果/日志不含 token 或口令。`FullMaterializer` 从持久 mode 取值，真实读取固定内容并核对代际/身份/摘要；恢复包导出使用不覆盖的安全目录/文件句柄。完整模式任务只在本地发现完成且无未决 pending/intent 后补齐证据，避免离线上传或删除竞争被误判。
- `crates/client-store` 迁移 0011 增加无凭据 `agent_connection(singleton,server_id,server_url,ca_file)`；10→11 升级保留未确认内容与队列、生成备份、拒绝旧 writer，初始建库包含该表。
- `GET /v1/roots/{root}/entries/{entry}/history` 与 `GET /v1/roots/{root}/trash` 需要 Root 授权，按稳定身份 `after` 分页、最多 100 项，返回 `RecoveryPage`；未知/重复参数 400、越权 403、条目不存在 404。回收站恢复事件携带提交后的 revision、parent、version、hash、size。`api/openapi.yaml` 与 `packages/api-client` 同步生成；`packages/local-ipc-types` 导出 1.2 类型和解码器。
- 原生验收入口：`VAULTSYNCD_PROCESS_TASK=S072 tests/platform/windows/run-native-process.sh`，在真实 Windows 用户会话启动产品 agent，逐项调用 21 个命令并验证参数负例、版本拒绝、Credential Manager 登录、真实 HTTP/SQLite/文件副作用、问题重试、恢复包与凭据脱敏。S071 的 `VAULTSYNCD_PROCESS_TASK=S071` 真实进程回归继续复用。

## S073 Windows CLI 最小管理入口

- 新包 `apps/cli`（`vaultsyncd-cli`，产物 `vaultsyncd-cli.exe`）只依赖 `vaultsyncd-local-ipc`、`vaultsyncd-platform-windows`、Tokio、`rpassword` 与 `rand_core`；**不依赖** `vaultsyncd-client-store`、SQLx 或 agent 核心。它无法直接打开 `client.sqlite`，也不自行判断暂停/释放/恢复是否安全，所有副作用仍由运行中的 agent 执行器复核。该包在 `members` 中、不在 `default-members`（Windows 用户会话入口，宿主只跑参数/输出单元测试）。
- 命令映射：`status`→`get_status`，`subscribe-status`→`subscribe_status`，`connection`/`set-connection`/`login`/`logout`，`roots`，`bindings`/`create-binding`/`pause`/`resume`/`mode`/`detach`，`download`/`release`，`problems`/`retry`，`history`/`trash`/`restore`，`export-recovery`。`download`/`release` 缺省生成规范 32 位十六进制 `operation_id`；ID、路径、修订和模式仍由 `crates/local-ipc::commands::validate` 与 agent 复核。
- 全局 `--json`/`-j` 输出单行信封：成功 `{ok:true,command,result:GetStatusResult}`，失败 `{ok:false,command,error:{code,message,retryable},exit_code}`。成功应答缺少该命令必需结果（Binding/问题/条目/连接/导出计数/状态快照）时以 `protocol_error` 失败，不把缺失字段显示成空列表或成功。输出与日志不再打印请求，不含密码、token 或命名管道 SID。
- 稳定退出码：`0` 成功；`2` CLI 用法/本地输入错误（未知命令/选项、缺参数、`--password`、空密码）；`3` 无法连接 agent（管道不存在/拒绝/断连/超时，JSON code `agent_unavailable`）；`4` 未登录或需重新认证（agent `unauthenticated`）；`5` agent 拒绝或执行失败（其余稳定错误码）；`6` 协议版本或应答不兼容（`unsupported_version`/`protocol_error`）。
- 登录密码只从标准输入读取：终端使用 `rpassword` 不回显，管道只读首行；`login` 不接受 `--password`，密码不进入 argv、shell 历史、JSON、stderr 或日志。请求中的瞬时密码随请求释放，agent 仅写 current-user Credential Manager。
- 为让 CLI 失败说明准确，agent 的 IPC 错误映射补充：`AUTH_EXPIRED` 归为 `unauthenticated`（需登录/刷新，不再误报 `blocked`）；`BINDING_UNBOUND`（已解绑后恢复/切模式）归为 `blocked`，不再落入 `unavailable`。
- 原生验收入口：先构建 Linux helper，再在 Windows devShell 运行 `VAULTSYNCD_PROCESS_TASK=S073 tests/platform/windows/run-native-process.sh`。`apps/cli/tests/native_cli.rs` 启动真实产品 agent/CLI，覆盖查询、未登录、错误密码、设置连接/登录、创建/暂停/恢复/解绑、下载/释放成功与暂停/完整模式/不存在条目的失败、模式切换、问题/历史/回收站、恢复导出及重复导出失败、登出与 agent 停止后的连接失败；用 `NtQuerySystemInformation` 枚举 CLI 进程句柄，断言样例请求期间没有任何 `client.sqlite` 文件句柄，并核对 JSON/agent 日志不含测试密码或 token。

## S074 三客户端跨进程端到端场景

- 新包 `tests/e2e`（`vaultsyncd-e2e`）是**跨进程场景编排器**：Windows 客体上运行，持有真实产品 `vaultsyncd-agent.exe` 子进程并通过 named pipe 读取真实状态；远端是宿主真实 Linux 服务端（真实 HTTP/SQLite/btrfs Root），中间是本机测试 TLS 入口 + 认证 SSH 回环隧道。该包在 `members`、不在 `default-members`：宿主 `cargo test -p vaultsyncd-e2e` 只执行场景数据/代理规则测试，Windows 执行用 `nix develop .#windows` 交叉编译。
- 入口：宿主先构建 `cargo build --locked -p vaultsyncd-server-tests --example e2e-native-server`，再运行 `VAULTSYNCD_WINDOWS_CACHE=<cache> nix develop .#windows --command bash tests/platform/windows/run-native-e2e.sh`。脚本启动真实服务端 fixture（按场景 `server_seed` 播种非空 Root）、反向隧道、交叉编译 agent/编排器、校验 SHA-256、在交互用户会话的 scheduled task 中执行，并把 `report.json`/代理事件/agent 日志取回 `artifacts/S074/`。只清理本 run 的 `s074-driver-*` 与 `run-e2e-*`。
- `tests/scenarios/three-clients.json` 是数据驱动描述（schema 1，`serde(deny_unknown_fields)`）：`clients`（id/label/mode）、`server_seed`、按序 `blocks`（`import`/`offline_edit`/`rename`/`delete_modify`/`lost_response`）与每块 `requirements`。编排器固定每类 block 的验收判据，JSON 只提供路径/内容/参与者；`tests/scenarios/README.md` 是形状说明。新增同类数据不改代码。
- 拓扑与隔离：仓库只有**一台** Windows 11 客体（第二台属 S107）。三个客户端是同一实机的三个隔离身份/环境：独立 DeviceId、状态目录与 `client.sqlite`、同步根、缓存、agent 进程；产品“每用户会话单实例”不变量使 agent **顺序**执行（报告 `physical_windows_guests=1`、`concurrent_agent_processes=1`，不宣称并发实机）。每个客户端在启动前经 `--login-stdin` 登录（Credential Manager 只保存一份当前账号凭据）。
- 客户端运行配置使用 `server.url=https://127.0.0.1:<测试入口>` 与 `server.ca_file=<run>/tls/ca.pem`（不设 `development_http`）；证书由 rcgen 现场生成，客户端仍做完整校验。测试入口按请求 `Connection: close` 转发到隧道上游，可对指定 `kind` 的 `POST /v1/roots/{root}/operations` **先转发、再丢弃响应**，并记录每个操作的 `op_id`、`status`、`state`、`replayed`、`revision`、`version_id`、`root_seq`、冲突副本与原始请求体；GET `/operations/{op}` 查询也记为 `kind=query`。响应丢失场景先验证产品按原 OpId 查询得到同一结果，再用记录的请求体按同一 OpId 重试，要求 `replayed=true` 且 revision/version/root_seq 相同。
- 断言与报告：每份场景输入内容计算 SHA-256，必须在“当前快照条目 / 冲突副本（当前条目）/ 回收站 / 条目历史”中找到；每端最终命名空间必须与服务端快照一致，完整模式逐字节核对，按需端只允许 `on_disk=0` 占位且已水合内容仍须匹配固定版本；宿主收尾再核对 btrfs Root 工作目录字节与报告 `final_entries` 一致。报告含场景 commit/工作树是否脏、Windows/服务端/agent 版本、三端 DeviceId/Binding、拓扑、各 block 证据、代理操作事件、输入内容覆盖与收敛计数。
- 场景暴露并修复的三处产品缺陷：① 客户端删除 ACK 保留被删版本，而服务端 delete 事件原先缺 `version_id`（`hash`/`size` 也缺），客户端按“同 revision 不同事实”拒绝整批；现 `apply_delete` 从 `version_facts` 补全三者（外部删除同样补全）。② `WindowsFiles::preserve` 曾删除被腾空路径的代际历史，首次导入 preserve-both 后远端版本在原路径安装会复用 generation 1 并撞上已有 evidence；现在保留被腾空路径的代际，路径代际只单调递增。③ `Adapter::adopt_complete` 曾拒绝把完整本地文件重新绑定到同 Binding/Root 的新条目；删除与离线修改并发时 ACK 后的 adopt 因此永远失败。现在仅当同一 Binding/Root 且 `on_disk >= size`（完整本地内容）时允许换身份，不完整占位仍拒绝。
- 失败诊断：编排器在客户端未收敛时把当前 `bindings`/`problems`/代理事件写到 run 目录 `failure.json`，并停掉 agent 用 `--once` 复跑一次把产品 `detail` 打进日志；脚本把失败现场取回 artifacts，客体目录保留供诊断。

## S076 公共 UI 与前端应用壳

- `@vaultsyncd/ui` 稳定入口导出 Button、Dialog（挂载时 showModal，onClose 由父移除）、Table<T>（columns/rows/rowKey/emptyMessage）、EmptyState、PageShell（受控 activePath/items/status）及原有 StatusBadge。`@vaultsyncd/ui/styles.css` 为独立样式入口；无 HTTP/IPC/Tauri 依赖。共享 CSS 变量 `--vs-accent`/`--vs-border`。
- `@vaultsyncd/web`、`@vaultsyncd/desktop` 均提供 dev/typecheck/build/preview；各自 dist，Vite 8.3.6、React 19.3.1、根 pnpm 锁。开发/预览仅绑定 127.0.0.1；默认端口冲突明确失败。Hash 路由不要求服务端 history fallback。
- Web `services/server.ts::checkServer(options?, signal?) -> Promise<HealthResponse>` 默认同源、浏览器 fetch、随机 request_id；只经 `@vaultsyncd/api-client` 稳定导出请求健康，无令牌存储。默认路由 `#/overview`，另有 `#/roots` 的未读取反馈；未知路由显式显示不存在。
- 桌面 `services/agent.ts::AgentClient.getStatus() -> Promise<GetStatusResult>` 用本机 IPC 稳定类型；S080 后默认 `nativeAgent` 调用 Tauri 固定状态桥，`disconnectedAgent` 仅保留为明确失败的测试替身。Tauri 不进入共享 UI。默认 `#/connections`，另有 `#/bindings`/`#/problems` 未读取反馈，无伪造同步完成状态。
- 验证入口：`pnpm --filter @vaultsyncd/web test`（两应用 SSR 默认未连接与传输正负例），两应用各自 `typecheck`/`build`；无浏览器或 Windows 原生证据。

## 对象引用标记（S085）

- `server-core::gc::mark::{ObjectKey, Reference, Reason}` 表达 Root 内内容地址、排他截止点与八类保护来源；`mark(now, references)` 返回每个对象的原因集合，重复引用合并，已过期原因不能抵消有效引用。`retained_until(at)` 固定加 30 天并饱和处理时间上界；不读取时钟/mtime，不做 I/O。
- `server-store::gc::references(tx, owner, root, now)` 从调用方同一 SQLite 事务枚举：活跃当前版本永久保护，历史从取代时刻起 30 天，删除末版本从删除时刻起 30 天，回收站截止至少为删除后 30 天，未结束及 finalized 上传在会话有效期内保护总哈希，下载/提交/备份租约以 `expires_at > now` 保护。`Error::{OutOfScope,InvalidFact,Storage,Lease}` 均应中止本次回收；条目/回收站悬空版本按坏事实拒绝。
- pending 操作或 `phase != finalized` 意图保守保护 Root 全部已登记对象；备份租约的 `SnapshotSeqMap` 涵盖本 Root 时，在冻结对象清单建立前同样保护整个 Root。此策略允许多保护，不允许遗漏；不把 seq 映射误当对象清单。store 生产依赖 core 的引用类型，core 不依赖 store。
- 本接口只产出标记事实，不改变数据库 `gc_state`，不清理历史行、临时文件或内容对象。S086 必须在宽限期后重新核对并与发布/租约使用统一协调，禁止凭过期快照直接删除。真实 SQLite 与核心共 6 项新增测试；相关包 lib 回归 311/84 通过。

## S080 Windows Tauri 壳与状态桥

- `apps/desktop/src-tauri` 是根 Cargo workspace 的 `vaultsyncd-desktop`，共用根锁；Tauri 仅 Windows 目标构建。默认构建内嵌 `../dist`，先在默认 devShell 执行桌面 `build`，再在 windows devShell 用 `cargo xwin build --locked --target x86_64-pc-windows-msvc -p vaultsyncd-desktop` 构建。Windows shell 增加锁定 Python 供原生验收哈希清单。
- S080 初始正式 command `agent_get_status() -> Result<GetStatusResult, {code,message}>`；无参数，仅连接 `Endpoint::current("vaultsyncd-agent")`，继承 SID/session/服务端身份核验和传输上限；不接受调用方指定路径/管道/系统命令，不直接打开 DB 或返回凭据。错误码为 `unavailable/agent_rejected/invalid_response`，消息不回显内部路径。
- `AppManifest` 显式命令清单和 `capabilities/main.json` 仅允许内置 `main` 窗口的 `allow-agent-get-status`。无远程 capability、shell/fs/SQL 插件；CSP 与导航仅内置应用来源。托盘打开/退出仅影响 GUI，agent 不归壳进程管理。前端成功读取后显示 agent 已连接，不把它表示为文件同步成功。
- 原生验收 `tests/platform/windows/run-native-desktop.sh`：先复核 EXE SHA-256，再在 Interactive/Limited 用户会话启动独立 agent 与真 WebView，实际状态/拒绝命令/GUI 退出不停止 agent。`native-acceptance` feature 才包含测试脚本、报告 command 和动态测试 capability；正式构建清理遗留测试权限元数据，未编译报告入口。报告与测试状态在 run_id 目录，VM 保持在线。


## Web 会话与账号管理（S077）

- 唯一 wire 源 `api/openapi.yaml`：`POST /v1/auth/browser/login` 接 LoginRequest，`POST /v1/auth/browser/refresh` 无请求体，均返回 BrowserSession（session_id/access_expires_at），令牌只在 HttpOnly/Secure/SameSite=Strict cookie。agent 原登录/带正文刷新保持兼容。全部认证响应 `Cache-Control: no-store`。
- `GET /v1/auth/account` 返回当前 AccountInfo（user_id/login/session_id）；`POST /v1/auth/password` 接 PasswordChangeRequest，复用初始化密码规则和登录限流，旧散列 CAS 更新与全部会话撤销同一 SQLite 写事务；`POST /v1/auth/revoke-all` 撤销当前账号全部会话。后二者返回 AccountChanged 的撤销计数并清 cookie，成功后重新登录。错误当前/新密码 400 INVALID_REQUEST，失效身份 401，越权来源 403，限流 429，存储错误 503。
- `AuthContext.account_gate` 在进程状态锁的单服务约束下串行登录、刷新、改密和全部撤销，防止旧密码已核验但晚于撤销再签发会话。`server-core::auth::manage` 仅做密码规则，`server-store::sessions::{change_password,revoke_all}` 负责原子存储，不新增 schema。
- Web `BrowserAuth` 只用同源 credentials/no-store 请求；受保护调用复用 `request`，401 并发单飞刷新一次，重复 401 回登录，网络失败保留可重试错误。账号页面可登录、改密、退出、撤销全部；不使用 localStorage，不读 cookie 凭据。
- Vite 仅回环监听、禁用 CORS，`VAULTSYNCD_DEV_API` 只接受无凭据/路径的本机 HTTP(S) 地址，保留原 Host 让后端校验浏览器 Origin。浏览器验收使用锁定 `nix develop .#web-tests` 的 Playwright Chromium；HTTP localhost 专用测试不代表 S097 生产 HTTPS 已部署。

## S078 owner Root 健康与采集页面

- `GET /v1/admin/roots -> ManagedRootList{allowed_bases,roots}`：仅唯一 active owner，`Cache-Control:no-store`。普通 `GET /v1/roots` 保持无路径；注册也验证同一管理边界。未登录401、非唯一owner403、读取失败503，不返回假空清单。
- `ManagedRoot{root_id,path,health,reason,next_step,root_seq,collection}`：每次请求按注册 `RootSnapshot` 现场复核挂载/身份/权限，失败 health=unavailable、collection=blocked，绝不推导删除；root_seq仅已提交序号。路径仅在该管理通道暴露。
- `RootCollection{state,examined,pending,detail}` 来自同一 `ObserverRuntime` 的最近真实 `PassReport`；状态闭集 not_scanned/scanning/pending/complete/blocked，数量为十进制字符串。进程重启后无历史完成观测；只有完整扫描、初始采集完成、无pending/扫描故障才显示complete，不代表客户端同步。Root读取/意图读取/扫描失败会使旧完成状态失效。
- `AllowedBases::paths()` 仅供管理员配置枚举；`RootContext::with_observer` 由入口装配。Web `RootsPanel` 使用生成API与BrowserAuth；显式刷新，注册不自动重放，网络/5xx结果未知时要求核对清单。`tests/web/roots-browser.py` 使用锁定Playwright及独立run_id btrfs服务，实际bind挂载故障与恢复均验收，测试挂载最终已卸载。

## 对象删除协调与过期上传清理（S086）

- `server-core::gc::sweep::{GRACE_SECONDS, Decision, decide}`：固定一小时标记宽限；新引用优先 Keep，时钟回退 Wait。满盘不参与过期判据，不缩短30天保留或宽限。
- `server-store::gc::sweep::{mark_root,candidates,authorize_delete,finish_delete}`：调用方传短事务；0011迁移新增 `gc_candidates(root_id,object_hash,marked_at,phase)`，marked仍可撤销，deleting与objects.collectable同事务持久授权且不能复活。授权前再次读取同事务全部引用；仅无保护的到期内容可移除历史/回收站/过期租约，删除末版本时清已删除条目的content指针、保留墓碑。候选每Root每轮最多100项，deleting优先；引用枚举目前读取Root全部事实。
- `Database::{object_activity,object_maintenance}` 是克隆句柄共享的读/独占对象门。锁序：对象门 → Root commit gate → observer roots → SQLite短事务；不嵌套获取、不升级读锁、不跨文件I/O持SQLite写事务。同一实例必须复用Database克隆，服务状态独占锁仍为多进程边界。备份在S087使用同一独占对象门，不能另建平行协调器。
- 服务HTTP submit整体持读门（含restore/conflict/create/put），finalize/download/uploads和观察器run_once持同一读门；版本租约写入由SQLite事务与collectable触发器共同围栏。SQL同时拒绝collectable的新版本、租约、上传和备份租约，幂等对象登记不能复活deleting。过期上传status/put/finalize按不可用会话404回答，禁止重放已经失去保护的旧finalize引用。
- `apps/server::gc::run_once(database,state_dir,now) -> io::Result<Report>` 在独占门下先清过期未完成上传，再标记/复核/删对象；外部观察后台每轮释放读门后调用。候选storage_path必须等于ObjectLayout规范路径，目录用RestrictedDir/fd锚定，拒绝符号链接；已有字节核对SHA-256与单链接，unlink+父目录fsync成功后才数据库收尾。marked/deleting/unlink边界被SIGKILL后重复继续；坏路径、坏内容、存储失败保留证据并明确报错。
- 临时清理只处理过期aborted上传的登记块、规范未登记块、`.index.pid.tmp` 和该UploadId的`.assemble-*`/`finalize-*`暂存名。块和会话元数据在相应删除刷盘后清理，缺目录按已不存在幂等收尾；有效及finalized上传、未知名字/类型、用户Root、客户端dirty与完整缓存不参与。Report区分objects/parts/temporary_files；未匹配任何已知过期会话的孤立暂存不凭mtime猜测删除。

## S081 桌面连接与绑定管理

- `apps/desktop/src-tauri/src/commands.rs` 提供八个固定 command：`agent_get_status/agent_set_connection/agent_login/agent_logout/agent_list_roots/agent_create_binding/agent_pause_binding/agent_resume_binding`。每项同时进入 AppManifest 和仅内置主窗口 capability；内部 `exchange` 先复用 IPC 参数校验，后连当前 SID/session 的固定 agent 管道。前端没有任意 command、管道、shell、文件读取或数据库入口。
- TS `AgentClient` 对应八个类型化方法；连接和创建输入结构拒绝未知字段。`ConnectionInput` 只含 serverId/serverUrl/caFile，服务器身份由管理员提供（当前协议无公开身份发现端点）；`BindingInput` 只含规范 bindingId/rootId/localPath/mode。缓存由桥派生在 `%LOCALAPPDATA%/vaultsyncd/binding-cache/<bindingId>`，agent 复核实际物理路径与范围。密码只在瞬时登录请求内，页面提交后清空；凭据仍由 agent WinCred 保存，结果不返回 token。
- 连接页提供地址、账号、显式附加CA；绑定页读取授权Root、选择现有本机目录与full/on_demand、创建合并、暂停恢复。每两秒获取全量真实快照（防重叠），操作串行；读取失败保留旧快照并明确错误，未读取不冒充空清单。同步完成只看核心 synchronized，多维dirty/pending/transfers/unknown/failed/event_lag直接展示。
- agent 保留管理连接证书错误 `CERTIFICATE_INVALID`（blocked/不可自动重试）和目录错误 `LOCAL_PATH_UNAVAILABLE`（invalid_argument），返回核对名称/CA或现有NTFS目录的说明，不回显底层自由错误或凭据。Root/登录仍走现有认证用例，IPC schema 无变更。
- `run-native-bindings.sh` 复用真实 Linux btrfs 实例，私有测试TLS+认证SSH回环隧道，Windows真React表单经桥/管道操作。先不信任CA而失败，再显式CA成功；验证错误目录、双方非空首合并、暂停恢复、关闭GUI后新文件上传与服务端真实字节。脚本/fixture注入仅 native-acceptance feature，正式程序不包含。生产HTTPS部署仍由S097验收。

## S079 Web 历史、冲突与回收站

- 新增授权 `GET /v1/roots/{root}/recovery` 与 `/conflicts`，均返回 `RecoveryPage`，固定每页100项，规范 `after` 身份游标；复用 Root 授权，不暴露服务端绝对路径。recovery 返回活跃条目身份/相对路径/当前版本；conflicts 只接受持久映射里的活跃副本，不按名称猜测。关闭提示没有远端删除动作。
- `RecoveryItem` 新增可选 `expires_at`（UTC Unix秒文本、最早可回收下限）与 `conflict_source`（原条目身份）。历史按 superseded+30天，删除后不少于delete+30天；回收站使用实际 expires_at。当前活跃版本省略到期字段。旧客户端可忽略新可选字段，无迁移。
- Web `features/history/recovery.ts::RestoreSession` 按用户/Root 保存标签页 sessionStorage 原请求（无凭据），先保存再调用；未知/pending 不另起操作。resume 先 GET 原Op，404 才 POST 原请求，同 OpId/device/epoch/CAS/version；committed 才查询 recovery 的实际当前相对路径。不会把 pending/网络失败显示成恢复成功。
- `#/recovery` 复用 BrowserAuth。入口按 Root 选择文件历史、冲突和回收站；按服务端返回游标翻页，到期禁用恢复；原Op持久到同标签页刷新，跨标签/清除存储不承诺自动找回。Restore 的最终安全检查继续属于服务端。
- 验收：`pnpm --filter @vaultsyncd/web test` 含恢复会话故障单测；`nix develop .#web-tests --command python3 tests/web/recovery-browser.py` 使用真实服务/Chromium/btrfs，丢提交响应后刷新原Op核对、同名另名恢复/目标保留、历史落盘与真实分页。pending 由注入单测覆盖，不宣称该浏览器脚本制造了真实后端pending。

## S092 构建依赖与协议消费者门禁

- `tools/xtask/src/boundaries.rs` 实现 `cargo xtask check-boundaries`。先 verify 两份生成源，再逐个读取 `cargo metadata --locked --filter-platform <target> --all-features --format-version 1`；target 固定 Linux x64/ARM64、Windows x64。这里 all-features 仅影响依赖图，不编译整个 workspace。遍历 normal/build 传递边并报告消费者到违规包的链，dev 边不作为生产耦合。
- 策略保护纯核心/wire、server/client、GUI/CLI/store/planner/提交客户端与共享 UI 边界；server-store→server-core 适配方向合法，Windows desktop→Tauri 合法，Linux desktop 不应引入 Tauri runtime。通用基础库 windows-sys 不等于 CFAPI；客户端 Windows 适配器和 Windows API主包另受所属层规则限制。
- pnpm 从实际 workspace 成员清单核对五个消费者，合并 package.json 的普通/dev/optional/peer 声明、npm别名与已安装传递依赖图。GUI/CLI/Web/UI 源码另扫常见 SQL 库及写入语句；这是可审查静态门禁，不能证明任意动态执行均无数据库 I/O。
- command 随后跑前端 recursive typecheck、recursive --if-present test，以及 `cargo test --locked -p vaultsyncd-protocol -p vaultsyncd-local-ipc`。缺工具、缺依赖、元数据失败或任一步失败均返回1，多余参数2；不静默跳过消费者。统一 `cargo xtask check` 复用 verify，并由默认Rust成员测试及前端测试覆盖消费者，总共六步。
- `tests/contract/boundaries.py` 为宿主 devShell 可执行负例，不新增空 Cargo 包，不复制已有 Rust wire 契约测试。需先 build xtask，且只在隔离树无其他命令并发改该树时运行；九类真实修改在 finally 逐字节还原原文件及锁。日志写忽略 artifacts/S092；负例后主动确认重新生成一致和完整正例。平台 metadata 不代替原生运行验收。

## S093 产品版本与组件来源清单

- 唯一产品版本源 `Cargo.toml [workspace.package].version`，当前只接受规范 major.minor.patch。所有 Rust 成员必须 version.workspace=true；JSON镜像为根/五个前端包 package.json 和 Tauri config.version。`cargo xtask version [--check|--sync]` 核对/同步，边界与统一check也核对。产品升版后需更新工作区锁；不绑定 API/schema 版本。生成器身份使用独立 `generate::GENERATOR_VERSION`，配置/表头不随产品包版本变化。
- `cargo xtask release build <web|desktop-ui|server|admin> <new-directory>` 要求干净提交，输出 payload 和 receipt.json。前端先 frozen-lockfile/offline 准备依赖，再移除dist后调用固定构建；拒绝 .env/VITE_ 覆盖。Linuxserver/admin 使用当前Linux宿主三元组与全新target进行locked release构建；不把静态desktop-ui当Windows客户端。
- receipt format1 含component/target、产品版本、完整commit/tree、Cargo/pnpm/Nix/工具链锁与协议源SHA256、实测工具版本、准备/构建命令、逐文件路径/大小/hash。构建前后来源不一致、特殊文件/符号链接或空输出都不签发记录；已有目录不覆盖。
- `release assemble <new-manifest.json> <artifact-directory>...` 仅组装严格同当前提交且来源/文件集一致的实际组件；拒绝重复component/target。manifest嵌入records和组件目录，`release verify <manifest>` 再核当前HEAD/版本/锁及完整产物hash。后续提交/合并使旧实验记录过期，必须重新构建；未开放未经验证的混合提交组合。
- 机制不是签名/完全隔离构建，自包含发行布局及Windows安装由S094/S099延伸；完整package命令仍明确Unsupported。验收 `python3 tests/contract/release.py` 用真实Web与desktop-ui构建，包含产品升版后重编生成器而wire不变、旧dist/旧提交/资源/产物修改拒绝及完整还原。说明见 packaging/README.md。

## 一致备份准备与租约（S087）

- `server-core::backup::prepare::{Plan,FrozenRoot,FrozenObject,ready}` 约束清单身份/规范路径与全部ready证据；不做I/O。`server-store::backup::online_backup(source,destination)` 用安全rusqlite Online Backup API，在同一源读取事务取得Root序号/原映射/全部对象/逻辑表计数并导出WAL一致数据库；目标独占创建，清除导出副本会话/临时续传/租约后quick_check、foreign_key_check及fsync，源库不改。
- `apps/server::backup::prepare(database,config,owner,id,now) -> io::Result<Prepared>` 只能从持服务状态锁的在线进程调用；唯一active owner与每次资源鉴权仍由管理入口保证。函数取共享`object_maintenance`门，完成可恢复意图，未解决pending/intent或collectable对象则明确拒绝。Root注册HTTP/管理socket共用入口现在也取`object_activity`门。禁止另开Database实例绕过该门，也不能在持读门时调用prepare。
- 0012迁移 `backup_preparations(id,owner_id,phase,created_at,expires_at,snapshot_seq_map,manifest_hash,detail,cleaned_at)`，阶段preparing/ready/failed/releasing/released；ready必须有manifest_hash。`Prepared`含id、只读snapshot路径、固定manifest与其SHA-256；外部格式唯一源 `api/backup-manifest.schema.json`。ready仅表示prepare完成，不代表导出verify通过。
- `platform-linux-server::backup::Arena::{open,create,freeze,is_read_only,remove,snapshot_path}` 只管理基目录内规范32位ID的prepared/snapshot；拒绝NoCoW/NoDataSum、非btrfs、嵌套子卷/不完整扫描、未知条目及路径逃逸。基目录为state/backups、0700；管理员预置user_subvol_rm_allowed挂载，服务本身不sudo、不挂载或格式化，不需要CAP_SYS_ADMIN。清理ro快照时须调用方先持久failed/releasing，再仅该snapshot清ro后删除。
- 在恢复GC前安装24小时backup lease，当前按Root保守保护（包含后来新增对象，允许延后回收）。对象复制使用stage_copy（FICLONE/校验复制），复核摘要/大小、独立inode和刷盘；清单列出DB/配置摘要、schema/protocol契约版本/摘要、ServerId、原Root映射、逻辑表计数与全部对象。外部尚未采集的工作目录变化明确excluded。
- `recover_incomplete(database,state_dir,now)` 在bootstrap独占状态锁后调用，只清持久preparing/failed未收尾ID、释放对应租约并留失败原因；ready不在此清理范围。没有遗留时不要求启用备份后端。S088负责管理命令/HTTP、导出/verify/release，不得绕过当前准备视图或把ready映射为complete。
- 原生验收入口 `cargo run -p vaultsyncd-server --example backup-prepare-native -- <独立测试卷内尚不存在的run目录>`；测试不在服务中提权。S087独立target四包与集成共609/0，Windows MSVC core/store all-targets检查通过；实际btrfs持续写218次时固定seq2，历史/回收站/哈希/ro/租约、缺对象拒绝、嵌套拒绝、SIGKILL恢复与pending保护均通过。运维条件见 `docs/operations/backup.md`。

## S082 文件清单与桌面手动管理

- 本机IPC **1.3**新增`list_files(binding_id,cursor?)`，1.2调用该命令明确UnsupportedVersion，其余既有命令保持兼容。schema唯一源生成Rust/TS `FileStatus`及`ManagementResult.files`；CLI提供`files --binding-id ID [--cursor ID]`，缺结果不是空成功。共享样本`api/examples/local-ipc.files.json`验证pending/dirty和六维信息不丢失，错误布尔值/缺维度两端拒绝。
- `agent::management_listing::list`只经实际安全扫描与客户端持久状态读模型投影，复用domain `EntryState::project`。涵盖驻留、dirty、远端应用、任务、保护忙/未知、调度离线/Root不可用；online仅表示当前无已记录离线事实，不承诺网络连通。未知/未导入文件无entry_id，不能执行远端动作。扫描或挂载故障不作删除；缺完整内容凭据、物理修改、忙、待办、full或门禁阻塞均禁止UI释放。提示不是授权，S069执行器仍重核真实身份/完整hash/代际/版本租约与句柄。
- 每页最多100项，排序/游标为相对路径SHA256前16字节的规范hex，碰撞明确失败；返回next_cursor续读。实时分页不承诺跨页一致快照，刷新回第一页，界面搜索明确仅当前页。未读/失败不显示伪造空清单；旧页保留但动作禁用，慢响应不覆盖另一绑定页。
- 桌面固定桥增加`agent_list_files/agent_download_entry/agent_release_entry/agent_list_problems/agent_retry_problem/agent_set_mode/agent_detach_binding/agent_export_recovery`，AppManifest和主窗口capability逐项授权，无通用任意命令/文件/SQL入口。副作用OpId在失败后保留供同动作重试。释放必须确认本地移除/远端保留/离线不可读；绑定管理提供模式、恢复导出和保留数据解绑的独立确认，完成只认agent返回的实际计数/报告。问题重试仅对agent标记retryable项目启用，仍调用既有执行器。
- `run-native-files-ui.sh`在真Windows WebView操作页面，用只编译于native-acceptance的固定样本句柄/写入控制验证busy/dirty双重保护。实际下载、释放/重新下载、模式、pending不绿、导出dirty/未导入新文件hash、解绑后服务器原字节与本地数据均保留已验证。正式无测试feature构建清理两项测试自动权限，包含测试fixture/控制命令的二进制拒绝作为正式证据。VM保持在线，测试服务/隧道与本run进程清理。


### S088 — 固定快照导出与持久长作业

`backup::jobs::Context`由bootstrap唯一组装，HTTP与同UID管理socket共享。0013为源完整性（unverified/verified/incomplete）与prepare/verify/release持久队列；连接10秒截止不取消已接受的作业。启动及1秒pump重放queued/running/releasing，过期租约入队释放，不自动创建备份。重复verify重新读所有项、quick_check/FK/ServerId/schema/Root seq和逻辑表计数；ready不代表verified。释放先持久releasing，只删除该ID的Arena快照后才释放备份租约，失败保留恢复证据。

锁次序：每备份ID写门（prepare/verify/release）→对象维护独占门（prepare/release）→S087 Root协调→短SQL事务；导出只持每ID读门，不持对象门。严禁在已有object_activity或object_maintenance门内调用prepare/release。不会在SQL事务持锁期间做文件复制或等待流读者。源租约在导出完成后继续存在，需release或到期清理。

OpenAPI提供固定清单、仅清单内的DB/config/Root对象读取、整体流式export与job status/list。全部重新校验唯一active owner与请求会话，使用锚定RestrictedDir拒绝任意路径；备份包含受限PHC认证材料但sessions失效，不能公开分享。Unix协议特殊export先framed200握手再VSBK001原始字节，逐次写30秒空闲截止；普通请求仍10秒。流规则与0600收据唯一源`api/backup-export.schema.json`，CLI显式prepare/export/verify/release；本地verify不打开在线数据库。损坏或缺终止标记只能incomplete，服务源verified与客户端持久导出verified分别记录。

S088原生最终证据：专属btrfs卷实际CLI/HTTP49.88秒矩阵通过，包含11秒后继续完整流与release排队跨10秒后SIGKILL重放；取消verify先撤销旧verified。CLI本地verify只检查归档全部字节和固定清单，SQLite语义由verify-source及离线恢复入口检查。归档与固定摘要收据需一起保管，后续S089可复用纯`Index`/`ArchiveVerifier`和store只读`verify_database`，不可把未verified收据当成功。

## S083 桌面历史恢复与系统通知

- 本机IPC **1.4**新增 `claim_notification(notification_key,notification_generation)`，1.3及之前明确拒绝该新命令；状态返回 `management.notices`（Root/原因/active或resolved/十进制代际/严重度/是否已领取）。client迁移0012增加generation与delivery_claimed，旧库原已读、次数及恢复状态保留，有迁移前备份，旧writer拒绝新schema。恢复后复发代际递增并重置claim，原GUI旧代际不能领取新通知；claim不等于已读或显示成功。
- 严重问题复用agent-core问题端口，按Root和原因聚合全部绑定之后才解决；普通网络失败仅offline状态。真实提交冲突ACK和收到conflict变化只产生一次持久原因，现有冲突副本不会被开关/清除系统通知移除。问题快照包含持久问题和运行期投影；暂停不能证明故障已恢复。
- 桌面新增固定 `agent_list_history/agent_list_trash/agent_restore_entry` 与通知设置命令，AppManifest及capability同步最小授权，无任意HTTP/数据库入口。历史/回收站分页含到期下限和冲突来源；恢复输入先持久到agent，再GET原Op，404才POST原请求，只有committed成功。服务端仍核对Root/权限/CAS，同名恢复保留现目标；返回授权快照内实际相对位置，位置尚未取得时明确待核对。UI未知/pending保留原操作身份供核对，不显示已恢复。
- WinRT Toast固定AUMID `org.vaultsyncd.desktop`，只创建/更新本应用StartMenu快捷方式，拒绝同名他应用和symlink。先原子claim再尝试Show，保证同一活动代际最多尝试一次；Windows禁用/失败时问题仍完整，页面明确投递失败。布尔开关单独保存在用户应用设置，无凭据。微软[桌面AUMID要求](https://learn.microsoft.com/en-us/windows/win32/shell/enable-desktop-toast-with-appusermodelid)及[发送示例](https://learn.microsoft.com/en-us/windows/win32/shell/quickstart-sending-desktop-toast)为注册依据；首次未打包应用Setting可能缺行的[平台问题记录](https://github.com/MicrosoftDocs/windows-dev-docs/issues/1431)对应0x80070490，仅该特定错误允许实际Show，其余错误不默认成功。
- `run-native-notifications.sh` 使用真实Windows WebView及系统History API验证中心记录、清中心/开关不清问题、重复报告与进程重启不风暴、恢复和复发。磁盘满/挂载/冲突的这组输入由离线核心端口夹具产生，只证明问题投影与真实OS投递，不冒充实际卷故障；真实冲突/恢复另由Linux btrfs服务与Windowsagent真壳验证。所有报告/fixture/Toast历史测试桥只在native-acceptance编译，正式构建清理全部四项测试权限。

## S090 Web 备份作业与有界导出

- `#/backup` 使用生成 BackupSnapshotStatus/List 与 BrowserAuth。prepare先存稳定ID、未知结果重试同ID，作业分页并轮询；pending、源integrity、浏览器归档状态相互独立。failed/released/expired可显式使用新ID；释放经确认，只影响该快照/租约，不改本地归档和共享对象。
- OpenAPI manifest/export两个操作声明 `x-vaultsyncd-stream-response: true`；仅允许application/octet-stream，生成器format6/独立GENERATOR_VERSION0.2.0返回原Response，不arrayBuffer整份归档；其他字节端点不变。HTTP流格式与本机IPC wire语义没有变化，Rust生成改动只有来源表头。
- `features/backup/archive.ts` 是schema约束的内部Index/流验证器，不是第二套wire DTO。与Rust Index/ArchiveVerifier使用相同VSBK001帧、固定manifest摘要、授权路径/顺序/长度/hash和最终零标记；拒绝BOM路径、重复、缺项、截断及额外数据。清单最大256MiB，内容处理/OPFS写入每步≤64KiB；SHA256来自根锁定@noble/hashes2.4.0。
- `LocalExport` 写OPFS新文件，每次重试保留旧尝试，不自动删除。元信息仅sessionStorage按用户隔离；下载失败/取消为incomplete，完成写入仍exported_unverified，显式重读才verified。页面重开撤回历史成功，需重新核对。OPFS缺能力明确失败并提示CLI；不把浏览器暂存当独立故障域。
- 另存归档和收据分别提供下载；仅验证OPFS时收据标exported_unverified，重新选择另存文件验证后才可生成verified收据。收据可交S088 CLI backup verify重读；浏览器无法设置POSIX0600，页面要求管理员私有保存位置，严格权限用CLI。离线restore仅说明停服务及CLI validate，无在线restore入口。
- 验收 `pnpm --filter @vaultsyncd/web test`、`nix develop .#web-tests --command python3 tests/web/backup-browser.py`。真实专用btrfs/Chromium覆盖14场景，包含CLI验证浏览器下载的真实归档；测试归档/收据0600，finally清服务、loop和挂载。后续HTTPS/离线恢复依各自卡验收。

## S084 未上传内容恢复原生演练

- 不新增产品wire或命令，复用现有`export_recovery(binding_id,destination)`及未ready绑定打开Local的路径。新增`tests/windows-integration/tests/unuploaded_recovery.rs`，仅Windows dev依赖，不把SQLite接入CLI/GUI。真实agent IPC与真实Linux API，数据库仅只读核验；上传part断连只作用于本run私有转发器，非fake成功响应或直接写在线DB。
- 实际Move被NTFS共享锁阻止产生持久intent；强杀重启后先导出。验证current最新dirty、snapshot旧固定上传、candidate新未观察文件及intent源各自可读/hash，人工CreateNew取回至新空目录；导出前后intent/resume/dirty计数保留，baseline版本/revision及服务端已确认字节不变。同名目的/受保护目录/忙源导出失败不清源、不发布manifest。
- runner校验NTFS、EXE传输hash、同用户Interactive/Limited登录态与helper实际工作路径，拒绝搬cache中带旧树内嵌路径的helper。恢复包与人工取回副本同步到忽略artifacts后由独立Python重核22份清单文件。只停止记录且核对过exe/config的自有进程；注册根与源数据有意保留，不把注销/卸载作为清理。操作步骤及包角色说明见`docs/operations/recovery.md`；安装/卸载另由S099。
