# vaultsyncd

Windows 11 与 Linux 之间的个人文件同步工具：Linux 服务端（含管理 CLI 与 Web 管理页）与 Windows 客户端（后台 agent、CLI、桌面与 CFAPI 占位）。首发范围、不变量与数据契约见 [`docs/design.md`](docs/design.md) §2—3、§6。

当前已有 Linux 服务端、管理 CLI/Web 和 Windows agent、CLI、桌面及 CFAPI 实现，处于首发验收阶段，**尚未完成首发门禁**。S101 hosted CI、S106 常规性能与 S107 正式容量仍阻塞；不能将历史安装包或单客体顺序运行结果视为当前版本发布通过。逐卡状态见 [`agent/progress.md`](agent/progress.md)，发布证据见[发布核对表](docs/operations/release.md)，平台与容量界限见[已知限制](docs/known-limitations.md)。

## 文档入口

| 文档 | 内容 |
| --- | --- |
| [`docs/design.md`](docs/design.md) | 产品与技术设计基线（唯一产品规格） |
| [`agent/implementation-plan.md`](agent/implementation-plan.md) | S001—S110 执行顺序与任务卡 |
| [`agent/progress.md`](agent/progress.md) | 执行状态表与最早 ready 卡 |
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
nix develop -c cargo xtask check    # 边界/生成代码/消费者检查 + fmt + clippy + test + 前端 typecheck/test，任一步失败即非零退出
nix develop -c cargo xtask generate # 从 HTTP 与 IPC schema 重新生成 Rust/TypeScript 契约
nix develop -c cargo xtask env      # rustc/cargo/node/pnpm 版本、workspace 成员与关键路径
nix develop -c cargo xtask help     # 子命令与退出码
```

`cargo xtask check` 实际执行的分步命令（可单独运行，顺序与输出一致）：

```bash
nix develop -c cargo xtask check-boundaries     # 三项目标依赖方向、HTTP/IPC生成一致与前端消费者
nix develop -c cargo fmt --check
nix develop -c cargo clippy --locked --all-targets -- -D warnings
nix develop -c cargo test --locked
nix develop -c pnpm install --frozen-lockfile   # 首次检出或 pnpm-lock.yaml 变更后
nix develop -c pnpm --recursive run typecheck
nix develop -c pnpm --recursive --if-present run test
```

退出码：`0` 全部通过；`1` 检查失败或前置条件不满足（例如缺少 `node_modules`，此时不跳过前端检查，而是报错并提示上面的安装命令）；`2` 用法错误；`3` 子命令尚未实现（`package`）；平台 CI job 的 `78` 表示资源缺失（blocked）。命令契约见 [`docs/architecture/interfaces.md`](docs/architecture/interfaces.md)。

## 构建与运行

一键生成全部支持平台的 release 归档（Linux x64/ARM64、Windows x64 ZIP）：

```bash
./tools/release.sh
```

在 Linux x64/Nix 主机运行，自动进入锁定环境并准备依赖。产物写入
`dist/releases/v<版本>-<提交前12位>/`，包含校验和、来源清单和日志；Windows 只生成免安装 ZIP。
单平台选择、目录设置和首次下载说明见[统一打包入口](packaging/README.md#一键生成发行归档)。

锁定开发工具链由 `flake.nix` 与 `flake.lock` 提供。服务端只支持 Linux、本地合规 btrfs 与完整普通文件；Windows 客户端只支持 Windows 11 x64/NTFS，原生 CFAPI 必须在 Windows 验证。Linux/macOS/Android 同步客户端、P2P 和增量传输不在首发范围。

| 操作 | 唯一维护入口 |
| --- | --- |
| NixOS 部署、HTTPS、健康与恢复 | [NixOS 运维](docs/operations/nixos.md) |
| 通用 Linux 包、systemd、容器 | [Linux 运维](docs/operations/linux.md) |
| Windows 安装、升级、回退与卸载 | [Windows 安装](docs/operations/windows-installation.md) |
| Windows agent、CLI 与状态 | [Windows agent](docs/operations/windows-agent.md) |
| 一致性备份、导出、校验与空实例恢复 | [备份](docs/operations/backup.md) |
| 未上传内容与异常缓存恢复 | [恢复导出](docs/operations/recovery.md) |
| Windows 原生验证与故障排查 | [实验室手册](docs/operations/windows-lab.md) |

Windows 构建使用宿主 `nix develop .#windows`；前端也在宿主构建。安装包与通用 Linux 包的实际构建命令见上表，不使用尚未实现的 `cargo xtask package`。`cargo xtask check-boundaries` 检查跨平台依赖方向和消费者；`tools/ci/README.md` 维护平台 CI job 与实验室隔离配置。交叉编译、fake 或浏览器通过均不能代替原生平台验收。
