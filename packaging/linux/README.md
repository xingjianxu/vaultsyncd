# Linux Nix 组件

在干净提交运行 `nix build`，或分别构建 `.#rust`、`.#web`。
Rust server/admin 使用 Cargo.lock 的真实 crate checksum 固定抓取，构建/测试由 cargo --offline 执行；
Web 使用 fetchPnpmDeps 固定输出哈希，再在 sandbox 离线安装并构建。Rust 源过滤不包含前端应用源码，
Rust derivation 不依赖 Web derivation，纯源码单测无须先编译前端。

发行输出包含 bin/vaultsyncd-server、bin/vaultsyncd-admin、share/vaultsyncd/web、
share/vaultsyncd/release-manifest.json。运行 wrapper 只补 btrfs-progs 所需 PATH，
服务端通过显式 VAULTSYNCD_WEB_DIR 定位不可变静态资源；环境覆盖为不存在、相对或不完整资源目录时拒绝启动。
未设置该变量的裸 Rust server 保持 API-only，不在编译期寻找或内嵌 dist。未知 API 路径不退回 HTML。

manifest 是独立的 Nix 来源记录，不伪造 S093 Git工作树收据。组件来自同一锁定flake快照，记录干净提交、
整个输入NAR、锁/协议源摘要、目标、Nixpkgs revision、工具包版本、过滤源/derivation身份与实际文件hash。
构建依赖的store hash分成8字符组写入记录，避免Nix扫描器因来源记录而把Node等构建闭包拖入运行闭包。
dirty flake可构建组件帮助开发，默认发行组装明确拒绝；没有忽略S093的同源要求或接受任意外部组件。

`vaultsyncd.toml` 仅为隔离回环部署示例。此卡不改主机服务、挂载或正式实例；
NixOS模块及部署由S095/S096继续。TLS入口、生产同源部署和访问策略由S097验收。
该输出依赖Nix store动态库及资源，不冒充通用Linux单文件包（S098）。
