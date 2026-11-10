# 发布输入与组件清单

## 一键生成发行归档

在 Linux x86_64/Nix 宿主上，从干净的已提交源码构建 Windows x64 便携 ZIP：

```sh
./tools/release.sh                                      # 仅 Windows x64 ZIP
./tools/release.sh --platform windows-x64                # 显式选择同一目标
./tools/release.sh --output-dir /data/vaultsyncd-dev/S169-实际run_id/releases
./tools/release.sh --plan                               # 只显示计划，不构建
```

`--platform all` 保留为 Windows x64 的同义参数；Linux 平台参数直接拒绝。脚本自动进入锁定 devShell，使用 Rust/MSVC 交叉工具链和冻结前端依赖，构建 `windows-zip` 组件，不调用 NSIS。统一发行入口不构建 Linux 便携包、容器或 glibc/GCC runtime sources；Linux 服务端继续通过 Nix/NixOS 部署。原 Linux 便携包与容器脚本仅保留供既有受控实验室和历史证据复验，不属于本入口或 GitHub 交付。

默认输出：

```text
/data/vaultsyncd-dev/releases/v<版本>-<提交前12位>/
  windows-x64/   vaultsyncd-<版本>-x64-unsigned.zip、receipt.json
  logs/          各步骤完整日志
  SHA256SUMS
  release-manifest.json
```

首次运行需要联网下载 Nix、Rust、Windows SDK/CRT 与 pnpm 依赖；缓存、构建目录和证据统一位于 `/data/vaultsyncd-dev/`，Windows 工具链默认复用该根下的 `vaultsyncd-temp/cache/windows`。每次使用全新 release target，不安装全局依赖、不启动 Windows VM。自定义输出与缓存仍须位于开发根下。

版本来自根 Cargo.toml；构建前后检查干净同一 HEAD，核对版本、目标和锁/协议输入，再执行 Windows receipt assemble/verify。已有输出拒绝覆盖；失败保留 `.partial-*` 和日志，不签发成功目录。进入本地构建目录执行 `sha256sum -c SHA256SUMS` 可校验 ZIP、收据及日志。

ZIP 仍是 `unsigned-test`，解压后使用 `Start vaultsyncd.cmd`；状态留当前用户 LocalAppData，运行需要 WebView2，停止使用 `Stop vaultsyncd.cmd`。本入口不执行 Windows 原生验收，也不解除性能、容量、多机和同源候选门禁。运行/数据保护见[Windows 安装与 ZIP](../docs/operations/windows-installation.md)。

## 开发构建（无版本号，直接写 dist/）

`tools/release.sh` 属发布链：要求干净已提交的源码，产物按 `v<版本>-<HEAD前12位>` 归档并带收据、校验表和版本号命名的 ZIP。只想把当前代码编译成可运行的程序目录时用另一个入口：

```sh
./tools/build.sh
```

脚本自动进入 `nix develop .#windows`，冻结锁离线准备前端、交叉编译 `x86_64-pc-windows-msvc` release（静态 CRT），每次先清空 `dist/` 再写入。产物就是无版本号 ZIP 的解压结果，可整目录拷到 Windows 11 本机普通路径后运行 `Start vaultsyncd.cmd`：

```text
dist/   四份 EXE（agent/session/CLI/desktop）、installed-files.json、zip-files.json、
        UNSIGNED-TEST.txt、Start/Stop vaultsyncd.cmd、README-ZIP.txt、build-info.txt
```

S185 起删除 `vaultsyncd-zip.exe`；任意本机普通目录直接运行 `vaultsyncd-desktop.exe` 或 Start 脚本即可。普通启动不校验 owner、程序路径、hash 或清单；旧 owner 留存忽略，状态保留在当前用户 LocalAppData。Start/Stop 调用 `vaultsyncd-agent.exe --user-session start|stop`，跨目录复用健康 agent；要切换版本先安全 Stop。清单仅供发布和显式自动更新使用，维护屏障/单 writer/IPC 用户认证保留。

`build-info.txt` 记录构建时的提交/树/版本/脏树状态与工具链版本。本入口不生成收据、不写 `SHA256SUMS`、不做干净提交检查，因此**不是发布产物**：不能用于 GitHub Release，也不改变 `unsigned-test` 语义与客户端首发门禁（仍需 Windows 原生验收）。

## 发布到 GitHub Release

本地发布脚本不依赖 GitHub 工作流。`gh` 已加入锁定 devShell；首次使用先登录有目标仓库写权限的账号：

```sh
nix develop --command gh auth login --hostname github.com
```

先提交版本和代码，再运行上面的 `tools/release.sh`。版本唯一源是根 `Cargo.toml`；需要升版时按下一节同步镜像和锁文件，然后提交、重新构建。将产物对应的源码提交推送到 `origin`（例如 `git push origin master`），再执行：

```sh
# 将路径替换为实际已完成构建目录；可在任意当前目录调用脚本。
./tools/publish-release.sh /data/vaultsyncd-dev/vaultsyncd-temp/S156-实际run_id/releases/v0.2.0-实际提交前12位 --dry-run
./tools/publish-release.sh /data/vaultsyncd-dev/vaultsyncd-temp/S156-实际run_id/releases/v0.2.0-实际提交前12位
```

也可直接传既有 `dist/releases/v<版本>-<提交前12位>` 目录。`--dry-run` 校验本地 Git 对象、版本、树、组件输入、所有登记文件的大小/SHA-256 和构建校验表，显示仓库、标签、附件；不联网、不上传、无需登录。脚本不要求当前 HEAD 等于包源，始终使用清单中的完整提交，避免把交接提交或新代码重标为旧包来源。默认目标取 `origin` 的 push 地址，支持 github.com HTTPS/SSH；可用 `--remote <名称>` 选择其他已配置远端。

实际发布前核对 GitHub 登录、源码提交存在于目标仓库、已有远端标签指向同一提交。版本标签默认为 `v<构建版本>`，不存在时由 GitHub Release 按精确源码 SHA 创建；不会推送分支或移动已有标签。默认发布为预发布且不设 Latest；`--stable` 在 ZIP 及 GitHub SHA-256 digest 核验后设为 Latest，供客户端托盘自动更新；草稿不推广。其他参数：

```sh
./tools/publish-release.sh /实际构建目录 --tag v0.2.0-rc.1 --notes-file /实际发布说明.md
./tools/publish-release.sh /实际构建目录 --draft           # 上传后保留草稿
./tools/publish-release.sh /实际构建目录 --resume          # 续传/发布同源草稿
./tools/publish-release.sh /实际构建目录 --stable          # 正式版本；标签必须为 v<版本>
```

自定义说明文件保留实际换行；工具追加源码、平台、未签名及未由本工具运行原生验收的说明。`--title` 可定制标题。正式/预发布只是 GitHub 的展示属性，不代表 S106—S108 门禁通过。

S185 新布局删除 ZIP 专用 EXE 后，旧 v0.1.7 更新器会拒绝新十文件包，首次发布该布局必须在发行说明写明一次性手工下载、旧版安全停止、从任意新目录直开并保留原状态；不得宣称旧 Latest 能直接托盘升级。既有发行和标签不改写，本卡不发行；新客户端后续自动更新使用新集合。若 GUI 与现有 agent 来自不同目录，托盘更新先提示安全 Stop 后重开目标目录，不下载或进入错误目标恢复。

客户端只检查本项目公开仓库的最新正式 Release；发布更高版本并使用 `--stable` 才会进入自动升级渠道。保留精确附件名 `vaultsyncd-<版本>-x64-unsigned.zip`，ZIP内所有程序/清单须同源，不能只上传单个EXE。客户端仍按SemVer拒绝同版/降版；没有正式Release、仅有预发布或缺少SHA-256时提示实际原因。升级前客户端自动保留程序/整套状态备份，下载校验失败不启动维护。此卡未执行真实GitHub发布。

GitHub Release 只上传一个附件：`vaultsyncd-<版本>-x64-unsigned.zip`。来源收据、根清单、`SHA256SUMS` 和日志保留在本地；`--dry-run` 的 `assets` 列出唯一 ZIP，`local_only` 列出其余本地文件。发布说明记录 ZIP 的 SHA-256，远端上传仍逐字节摘要校验；哈希不是数字签名。GitHub 页面自动生成的仓库源码下载链接不属于脚本上传附件。

上传先创建草稿，核对远端大小/SHA-256，完成后才发布。创建草稿后若 Release 列表尚未可见（实测 GitHub 有短暂延迟），脚本退避重查最多 10 次、间隔 1 秒再报错；失败保留草稿，用相同构建目录、标签及 `--stable` 类型参数加 `--resume` 恢复；匹配 ZIP 跳过，缺摘要的旧附件下载校验。只接受 Windows 单平台构建目录；旧 Linux/混合构建目录需要重新构建，不删文件或手改来源清单冒充。旧草稿若带清单、校验表或 Linux 等额外附件，续传会拒绝，需人工核对后另开草稿或移除额外附件。已发布 Release 不改写，旧发行与对应源码归档继续保留。

暂存默认 `/data/vaultsyncd-dev/vaultsyncd-temp/cache/github-release/upload-*`，完成或失败后清理本次复制；原构建目录保持。其他主机可传 `--staging-root <已有授权的缓存根>`。本入口只发布已有归档，不自动编译、不启动 VM、不改变线上服务。验收入口：`nix develop --command python3 tests/contract/github_release.py`（本地 Git 与 API 契约，不写真实 GitHub）。CLI 行为依据：[创建 Release](https://cli.github.com/manual/gh_release_create)、[上传附件](https://cli.github.com/manual/gh_release_upload)、[发布草稿](https://cli.github.com/manual/gh_release_edit)。

## 组件来源收据

产品版本唯一源是根 Cargo.toml 的 workspace.package.version。Rust 包继承该值；
`cargo xtask version --sync` 同步根/前端 package.json 与 Tauri version，`version --check`
及统一 check 拒绝漂移。产品版本只接受规范 major.minor.patch；修改后运行
`cargo update --workspace --offline` 更新 Cargo.lock（不升级外部锁定依赖），并按根 pnpm 工作区检查锁文件。
协议/API/schema 和生成器各自维护版本；产品升版不要求改动任何 wire 版本或生成表头。

所有项目命令在 devShell 内运行。先完成检查并提交，确保工作树无未提交/未跟踪输入。
使用新的、被忽略的产物目录，例如：

```sh
cargo xtask release build web artifacts/release-web
cargo xtask release build desktop-ui artifacts/release-desktop-ui
cargo xtask release assemble artifacts/release.json artifacts/release-web artifacts/release-desktop-ui
cargo xtask release verify artifacts/release.json
```

build 真实执行固定构建入口，不接收手写占位产物。Web/desktop-ui 每次先清除本应用 dist，再构建复制到
payload；先用冻结锁离线准备依赖并记录步骤，拒绝 .env 和 VITE_ 覆盖。Linux server/admin 使用全新 Cargo target、locked release 构建，
记录宿主目标。Windows 安装和 Linux 发行包组装仍由对应平台发布卡扩展，不把静态 desktop-ui 当原生客户端。

receipt.json 记录产品版本、完整 HEAD/tree、两工作区与 Nix/工具链锁及协议源摘要、
实测 rustc/cargo/node/pnpm、实际命令、目标和逐文件大小/SHA-256。构建前后核对源码，失败不签发记录。
assemble/verify 再核当前提交、版本/锁和 payload 的完整文件集；只接受严格同一提交，不接受未经验证的混搭。
清单只描述实际提供的组件集合，不代表完整首发交付。已有输出不覆盖，符号链接/特殊文件拒绝。

清单不是签名或完全隔离构建的证明。当前适配器在本地可信工具链和已安装锁定依赖下构建；
验证依赖本机源码与组件目录，因此复制到发布包的自包含布局由 S094/S099 延伸。
合并、追加提交、修改版本或资源后旧记录不能用于新提交，必须重新构建。
日志与大产物均写忽略 artifacts；验收：`python3 tests/contract/release.py`
（先 build xtask、提交本卡代码；使用实际双前端构建与可还原负例，不占 Windows VM）。


S099增加`windows-client`组件：在`nix develop .#windows`内执行相同release build入口，使用新target真实构建GUI/agent/CLI/session launcher及未签名NSIS测试包（S100增加受控升级与默认保留数据的卸载）；继承严格同HEAD、前后来源核对和完整payload清单。用户状态与启动、原生验收条件见[Windows安装操作](../docs/operations/windows-installation.md)。安装验证须真实Windows，不以组件收据代替。
