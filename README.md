# vaultsyncd

Windows 11 与 Linux 之间的个人文件同步工具：Linux 服务端（含管理 CLI 与 Web 管理页）与 Windows 客户端（后台 agent、CLI、桌面与 CFAPI 占位）。首发范围、不变量与数据契约见 [`docs/design.md`](docs/design.md) §2—3、§6。

当前状态：工程实施从 S001 开始。S001（主机与同步环境基线）、S002（agent 入口与上下文提取工具）、S003（Nix 开发 shell）已完成，尚无产品代码；下一 ready 卡为 S004（Cargo workspace）与 S005（pnpm workspace）。逐步状态见 [`agent/progress.md`](agent/progress.md)。

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

## 最小验证

现阶段没有编译产物。开发 shell 通过 `flake.nix` + `flake.lock` 提供锁定的 Rust、Node/pnpm、Python、SQLite 与 C 工具链：

```bash
nix develop                      # 进入 devShell
nix develop --command bash -c 'rustc --version; cargo --version; node --version; pnpm --version; python3 --version; sqlite3 --version'
python3 tools/context.py card S001
python3 tools/context.py design 11.2
python3 tools/context.py design 99.9; echo $?   # 预期退出码 2，并列出候选
python3 -c 'import ast,pathlib; ast.parse(pathlib.Path("tools/context.py").read_text())'
nix flake check
git diff --check
```

实测工具版本、nixpkgs rev 与实测限制见 [`docs/environment.md`](docs/environment.md) §10。Windows 侧工具链（rustup 与 MSVC）由 S009 固定，本 flake 不涵盖 Windows 交叉编译。
