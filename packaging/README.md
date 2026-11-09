# 发布输入与组件清单

## 一键生成发行归档

在 Linux x86_64 主机安装并启用 Nix flakes 后，从干净的已提交源码运行：

```sh
./tools/release.sh                                      # 默认 Linux x64 与 Windows x64
./tools/release.sh --platform windows-x64                # 只构建 Windows ZIP
./tools/release.sh --platform linux-x64                  # 只构建 Linux x64
./tools/release.sh --output-dir /绝对路径/releases       # 自定义输出根目录
./tools/release.sh --plan                               # 只显示计划，不构建
```

脚本自动进入锁定 devShell，Linux 使用 `nix build .#portable`，包含 server、admin、Web、私有 loader/库、示例配置及 systemd 单元；运行时源码归档随包保留。Windows 在宿主使用锁定 Rust/MSVC 交叉工具链和冻结前端依赖，走 `windows-zip` 组件，不调用 NSIS、不生成安装器。默认输出：

```text
dist/releases/v<版本>-<提交前12位>/
  linux-x64/     vaultsyncd-<版本>-x86_64-unknown-linux-gnu.tar.gz
                 vaultsyncd-runtime-sources.tar.gz、release-manifest.json
  windows-x64/   vaultsyncd-<版本>-x64-unsigned.zip、receipt.json
  logs/          各步骤完整日志
  SHA256SUMS
  release-manifest.json
```

只选择部分平台时仅包含实际构建的平台。首次运行需要联网下载 Nix、Rust、Windows SDK/CRT 与 pnpm 依赖；后续复用 Nix store、仓库 `.cache/windows/` 和 pnpm 缓存，Windows 每次仍使用全新 release target。可用 `VAULTSYNCD_WINDOWS_CACHE=/实际缓存目录` 指定已有工具链缓存。不会安装全局依赖或启动 Windows VM。

版本来自根 Cargo.toml；每个平台核对同一完整源码提交、版本、目标和锁/协议输入，Windows 还执行既有 receipt assemble/verify。成功后整体发布目录；已有目录拒绝覆盖，需要重复构建时指定另一输出根目录。失败退出非零，保留 `.partial-*` 现场和日志，不生成成功发布目录；不自动删除旧包。仓库内自定义输出目录须已被 Git 忽略。进入产物目录执行 `sha256sum -c SHA256SUMS` 可校验归档、平台来源记录与日志。

Windows ZIP 仍是 `unsigned-test`，解压后使用 `Start vaultsyncd.cmd`；状态留当前用户 LocalAppData，运行需要 WebView2，停止使用 `Stop vaultsyncd.cmd`。本入口只构建和校验产物，不执行 Windows/Ubuntu 原生验收，也不解除 S101/S106—S108 发布门禁；不扩展到 macOS、Windows ARM64 或 Linux 客户端。运行/数据保护说明见[Windows 安装与 ZIP](../docs/operations/windows-installation.md)和[Linux 运维](../docs/operations/linux.md)。

## 发布到 GitHub Release

本地发布脚本不依赖 GitHub 工作流。`gh` 已加入锁定 devShell；首次使用先登录有目标仓库写权限的账号：

```sh
nix develop --command gh auth login --hostname github.com
```

先提交版本和代码，再运行上面的 `tools/release.sh`。版本唯一源是根 `Cargo.toml`；需要升版时按下一节同步镜像和锁文件，然后提交、重新构建。将产物对应的源码提交推送到 `origin`（例如 `git push origin master`），再执行：

```sh
# 将路径替换为实际已完成构建目录；可在任意当前目录调用脚本。
./tools/publish-release.sh /data/vaultsyncd-temp/S156-实际run_id/releases/v0.2.0-实际提交前12位 --dry-run
./tools/publish-release.sh /data/vaultsyncd-temp/S156-实际run_id/releases/v0.2.0-实际提交前12位
```

也可直接传既有 `dist/releases/v<版本>-<提交前12位>` 目录。`--dry-run` 校验本地 Git 对象、版本、树、组件输入、所有登记文件的大小/SHA-256 和构建校验表，显示仓库、标签、附件；不联网、不上传、无需登录。脚本不要求当前 HEAD 等于包源，始终使用清单中的完整提交，避免把交接提交或新代码重标为旧包来源。默认目标取 `origin` 的 push 地址，支持 github.com HTTPS/SSH；可用 `--remote <名称>` 选择其他已配置远端。

实际发布前核对 GitHub 登录、源码提交存在于目标仓库、已有远端标签指向同一提交。版本标签默认为 `v<构建版本>`，不存在时由 GitHub Release 按精确源码 SHA 创建；不会推送分支或移动已有标签。默认发布为预发布且不设 Latest；S162起 `--stable` 含 Windows x64 时，在完整附件及 GitHub SHA-256 digest 核验后设为 Latest，供客户端托盘自动更新。草稿不推广；仅Linux包保持非Latest，避免切断Windows更新渠道。其他参数：

```sh
./tools/publish-release.sh /实际构建目录 --tag v0.2.0-rc.1 --notes-file /实际发布说明.md
./tools/publish-release.sh /实际构建目录 --draft           # 上传后保留草稿
./tools/publish-release.sh /实际构建目录 --resume          # 续传/发布同源草稿
./tools/publish-release.sh /实际构建目录 --stable          # 正式版本；标签必须为 v<版本>
```

自定义说明文件保留实际换行；工具追加源码、平台、未签名及未由本工具运行原生验收的说明。`--title` 可定制标题。正式/预发布只是 GitHub 的展示属性，不代表 S106—S108 门禁通过。

客户端只检查本项目公开仓库的最新正式 Release；发布更高版本并使用 `--stable` 才会进入自动升级渠道。保留精确附件名 `vaultsyncd-<版本>-x64-unsigned.zip`，ZIP内所有程序/清单须同源，不能只上传单个EXE。客户端仍按SemVer拒绝同版/降版；没有正式Release、仅有预发布或缺少SHA-256时提示实际原因。升级前客户端自动保留程序/整套状态备份，下载校验失败不启动维护。此卡未执行真实GitHub发布。

上传先创建草稿，逐项上传并核对远端大小/SHA-256，全部就绪后才发布。失败保留远端草稿，用相同构建目录、标签及 `--stable` 类型参数加 `--resume` 恢复；已存在且哈希相符的附件跳过，缺少摘要的旧附件会下载校验。不覆盖附件，不修改已发布 Release；远端含异源/不匹配/多余附件时退出非零，需人工检查该草稿。创建响应丢失也可通过 `--resume` 找回；发布响应丢失时先查看 GitHub 页面，已发布版本不会被自动改写。

附件包括实际选中平台的归档、Linux 对应运行库源码、各平台来源清单（加平台前缀避免重名）、根 `release-manifest.json`、`build-SHA256SUMS` 及 `SHA256SUMS`；不上传 `logs/`。GitHub 附件为平铺布局，下载到同一目录后执行 `sha256sum -c SHA256SUMS`。`build-SHA256SUMS` 保留原构建目录的校验表，路径包含平台目录及未上传的本地日志，不能直接用于平铺附件。清单与哈希不是数字签名。

暂存默认 `/data/vaultsyncd-temp/cache/github-release/upload-*`，完成或失败后清理本次复制；原构建目录保持。其他主机可传 `--staging-root <已有授权的缓存根>`。本入口只发布已有归档，不自动编译、不启动 VM、不改变线上服务。验收入口：`nix develop --command python3 tests/contract/github_release.py`（本地 Git 与 API 契约，不写真实 GitHub）。CLI 行为依据：[创建 Release](https://cli.github.com/manual/gh_release_create)、[上传附件](https://cli.github.com/manual/gh_release_upload)、[发布草稿](https://cli.github.com/manual/gh_release_edit)。

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
