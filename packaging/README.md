# 发布输入与组件清单

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
payload；拒绝 .env 和 VITE_ 覆盖。Linux server/admin 使用全新 Cargo target、locked release 构建，
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
