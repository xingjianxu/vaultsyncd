# 发布输入与组件清单

## 一键生成发行归档

在 Linux x86_64 主机安装并启用 Nix flakes 后，从干净的已提交源码运行：

```sh
./tools/release.sh                                      # 默认全部三个平台
./tools/release.sh --platform windows-x64                # 只构建 Windows ZIP
./tools/release.sh --platform linux-x64 linux-arm64      # Linux 双架构
./tools/release.sh --output-dir /绝对路径/releases       # 自定义输出根目录
./tools/release.sh --plan                               # 只显示计划，不构建
```

脚本自动进入锁定 devShell，Linux 使用 `nix build .#portable/.#portable-aarch64`，包含 server、admin、Web、私有 loader/库、示例配置及 systemd 单元；运行时源码归档随包保留。Windows 在宿主使用锁定 Rust/MSVC 交叉工具链和冻结前端依赖，走 `windows-zip` 组件，不调用 NSIS、不生成安装器。默认输出：

```text
dist/releases/v<版本>-<提交前12位>/
  linux-x64/     vaultsyncd-<版本>-x86_64-unknown-linux-gnu.tar.gz
                 vaultsyncd-runtime-sources.tar.gz、release-manifest.json
  linux-arm64/   vaultsyncd-<版本>-aarch64-unknown-linux-gnu.tar.gz
                 vaultsyncd-runtime-sources.tar.gz、release-manifest.json
  windows-x64/   vaultsyncd-<版本>-x64-unsigned.zip、receipt.json
  logs/          各步骤完整日志
  SHA256SUMS
  release-manifest.json
```

只选择部分平台时仅包含实际构建的平台。首次运行需要联网下载 Nix、Rust、Windows SDK/CRT 与 pnpm 依赖；后续复用 Nix store、仓库 `.cache/windows/` 和 pnpm 缓存，Windows 每次仍使用全新 release target。可用 `VAULTSYNCD_WINDOWS_CACHE=/实际缓存目录` 指定已有工具链缓存。不会安装全局依赖或启动 Windows VM。

版本来自根 Cargo.toml；每个平台核对同一完整源码提交、版本、目标和锁/协议输入，Windows 还执行既有 receipt assemble/verify。成功后整体发布目录；已有目录拒绝覆盖，需要重复构建时指定另一输出根目录。失败退出非零，保留 `.partial-*` 现场和日志，不生成成功发布目录；不自动删除旧包。仓库内自定义输出目录须已被 Git 忽略。进入产物目录执行 `sha256sum -c SHA256SUMS` 可校验归档、平台来源记录与日志。

Windows ZIP 仍是 `unsigned-test`，解压后使用 `Start vaultsyncd.cmd`；状态留当前用户 LocalAppData，运行需要 WebView2，停止使用 `Stop vaultsyncd.cmd`。本入口只构建和校验产物，不执行 Windows/Ubuntu 原生验收，也不解除 S101/S106—S108 发布门禁；不扩展到 macOS、Windows ARM64 或 Linux 客户端。运行/数据保护说明见[Windows 安装与 ZIP](../docs/operations/windows-installation.md)和[Linux 运维](../docs/operations/linux.md)。

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
