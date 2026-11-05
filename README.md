# vaultsyncd

Windows 11 与 Linux 之间的个人文件同步工具：Linux 服务端（含管理 CLI 与 Web 管理页）与 Windows 客户端（后台 agent、CLI、桌面与 CFAPI 占位）。首发范围、不变量与数据契约见 [`docs/design.md`](docs/design.md) §2—3、§6。

当前状态：S001—S006 已完成（环境基线、agent 入口与上下文提取工具、Nix 开发 shell、Cargo workspace、pnpm workspace、任务编排入口与便携 CI）。代码只有 Rust 领域内核骨架与前端展示组件，尚无产品功能；下一 ready 卡为 S007（独立 btrfs 测试位置）。逐步状态见 [`agent/progress.md`](agent/progress.md)。

## 文档入口

| 文档 | 内容 |
| --- | --- |
| [`docs/design.md`](docs/design.md) | 产品与技术设计基线（唯一产品规格） |
| [`agent/implementation-plan.md`](agent/implementation-plan.md) | S001—S108 执行顺序与任务卡 |
| [`agent/progress.md`](agent/progress.md) | 108 行执行状态表与最早 ready 卡 |
| [`docs/environment.md`](docs/environment.md) | 开发/服务验证主机的实测环境基线 |
| [`docs/architecture/interfaces.md`](docs/architecture/interfaces.md) | 已实现工具与接口的契约摘要 |
| [`AGENTS.md`](AGENTS.md) | 编码 agent 的入口、worktree 与提交约定 |

## 上下文提取工具

会话不应整份读取规格文档。用 `tools/context.py`（Python 3 标准库，无第三方依赖）按标题取用：

```bash
python3 tools/context.py card S002             # 单张任务卡
python3 tools/context.py card S002 S003        # 多张卡，按参数顺序输出
python3 tools/context.py design 11.2           # 单个设计小节
python3 tools/context.py design 5.2 5.9 5.11   # 多个小节
python3 tools/context.py --help
```

输出以 `--- source:`、`--- heading:`、`--- lines:` 标记开头，正文按原文行范围给出，不携带相邻卡片或小节。未知标题、前缀匹配多解或编号不存在时以退出码 2 报错并列出候选，不会静默返回空内容或整份文档。完整接口、围栏与编号边界规则见 [`docs/architecture/interfaces.md`](docs/architecture/interfaces.md)。

## 检查入口

CI（[`.github/workflows/checks.yml`](.github/workflows/checks.yml)）与本地用同一条命令；检查逻辑在 `tools/xtask`，YAML 不重复写一遍：

```bash
nix develop -c cargo xtask check    # fmt + clippy + test + 前端 typecheck，任一步失败即非零退出
nix develop -c cargo xtask env      # rustc/cargo/node/pnpm 版本、workspace 成员与关键路径
nix develop -c cargo xtask help     # 子命令与退出码
```

`cargo xtask check` 实际执行的分步命令（可单独运行，顺序与输出一致）：

```bash
nix develop -c cargo fmt --check
nix develop -c cargo clippy --locked --all-targets -- -D warnings
nix develop -c cargo test --locked
nix develop -c pnpm install --frozen-lockfile   # 首次检出或 pnpm-lock.yaml 变更后
nix develop -c pnpm --recursive run typecheck
```

退出码：`0` 全部通过；`1` 检查失败或前置条件不满足（例如缺少 `node_modules`，此时不跳过前端检查，而是报错并提示上面的安装命令）；`2` 用法错误；`3` 子命令尚未实现（`generate`、`package`）。命令契约见 [`docs/architecture/interfaces.md`](docs/architecture/interfaces.md)。

## 最小验证

现阶段可编译的产物只有 `crates/domain`（领域内核骨架）、`tools/xtask`（编排工具）与 `packages/ui`（展示组件）。开发 shell 通过 `flake.nix` + `flake.lock` 提供锁定的 Rust、Node/pnpm、Python、SQLite 与 C 工具链：

```bash
nix develop                      # 进入 devShell
nix develop --command bash -c 'rustc --version; cargo --version; node --version; pnpm --version; python3 --version; sqlite3 --version'
nix develop -c cargo xtask check  # 便携检查（CI 用同一入口）
python3 tools/context.py card S001
python3 tools/context.py design 11.2
python3 tools/context.py design 99.9; echo $?   # 预期退出码 2，并列出候选
python3 -c 'import ast,pathlib; ast.parse(pathlib.Path("tools/context.py").read_text())'
nix flake check
git diff --check
```

实测工具版本、nixpkgs rev 与实测限制见 [`docs/environment.md`](docs/environment.md) §10。Windows 客户端改为在本 NixOS 开发环境交叉编译，前端也在宿主构建，以节省 Windows 虚拟机资源；客体承担原生运行与验收。当前 flake 尚未提供交叉工具链，由 S009 落实并验证，分工见[实施手册 §2.6](agent/implementation-plan.md#26-windows-验证资源优先-quickemu)。
