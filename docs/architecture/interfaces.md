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

状态：S003 实现。唯一 devShell 入口，后续卡（S004/S005/S006）在此内构建与验证；实测版本与限制见 `docs/environment.md` §10。

| 项目 | 契约 |
| --- | --- |
| 输入 | `github:NixOS/nixpkgs/nixos-unstable`，rev 由 `flake.lock` 固定（当前 `1d060086d60cb76a40cf37109f979d88a5bd6396`）；无 overlay、无其他 input |
| 导出 | `devShells.x86_64-linux.default`，derivation 名 `vaultsyncd-dev`；`nix develop` 即默认入口。 `devShells.default` 不是 flake schema 的合法顶层属性，按系统的 `devShells.<system>.default` 是规范写法 |
| 覆盖范围 | 仅 Linux `x86_64-linux`；Windows 工具链由 S009 用 rustup+MSVC 固定，不做交叉编译 |
| 非交互调用 | `nix develop --command bash -c '<cmd>'`；`shellHook` 只导出 `VAULTSYNCD_DEV_SHELL=1` |
| 跟踪要求 | `flake.nix`/`flake.lock` 必须先被 Git 跟踪，否则 `nix flake lock`/`nix develop` 退出码 1 |

## Cargo workspace 与 Rust 工具链

状态：S004 实现。`crates/domain` 是当前唯一成员；后续卡按 design §5.4 增补成员，不预建空 crate。所有 Cargo 命令在 `nix develop -c` 内执行。

| 项目 | 契约 |
| --- | --- |
| 根清单 | `Cargo.toml` 为虚拟 workspace（无 `[package]`）；`resolver = "3"`；`members`/`default-members` 只列真实存在的包 |
| 包命名 | `crates/<目录>/` 的包名为 `vaultsyncd-<目录>`（如 `vaultsyncd-domain`）；`apps/` 入口按 design §5.7 的 `vaultsyncd-server`/`-admin`/`-agent`/`-cli` |
| 共享包字段 | `[workspace.package]`：`edition = "2024"`、`rust-version = "1.85.0"`、`version = "0.1.0"`、`publish = false`；成员用 `<字段>.workspace = true` 继承，不重复字面量 |
| lint 接线 | 成员必须写 `[lints] workspace = true`。`[workspace.lints.rust]`：`unsafe_code`、`missing_docs`、`unused` = deny；`[workspace.lints.clippy]`：`all = { level = "deny", priority = -1 }`。crate 内的 `#![forbid(unsafe_code)]` 是更硬的内层约束 |
| 工具链固定 | `rust-toolchain.toml`：`channel = "1.99.0"`、`components = ["rustfmt", "clippy"]`。Linux 的 rustc/cargo 来自锁定 nixpkgs，rustup 不读该文件；Windows 用 `rustup toolchain install 1.99.0-x86_64-pc-windows-msvc` 取同一版本，两端版本以本节为唯一基准 |
| 格式 | `rustfmt.toml` 只固定 `style_edition = "2024"`，其余用默认值；`cargo fmt --check` 为格式门禁 |
| 锁文件 | 仓库根唯一 `Cargo.lock`（生成产物，提交入库），不得忽略；新增依赖后先 `cargo generate-lockfile`，验证用 `--locked` |
| 验收命令 | `cargo metadata --locked --no-deps`、`cargo check --locked`、`cargo fmt --check`、`cargo clippy --locked --all-targets -- -D warnings`、`cargo test --locked` |
| MSRV 状态 | `rust-version = "1.85.0"` 依据 edition 2024 稳定版本声明；实测只在 1.99.0 上编译，devShell 内无 1.85.0 工具链，1.85.0 真编译复核待 S006/S009 |

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
