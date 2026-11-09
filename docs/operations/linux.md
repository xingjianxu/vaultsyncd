# 通用 Linux 发行包与容器

## 构建与来源

从干净、已提交的 Git flake 构建，不使用工作树内已有 target/dist：

```sh
nix build .#portable --no-link --json
nix build .#container-deps --no-link --json
```

当前仅支持 x86_64-unknown-linux-gnu（S111/ADR-015 已移除 Linux ARM64 范围），Rust 服务端和管理 CLI 与同提交 Web 独立构建，再封装私有 ELF loader、递归解析的运行库及相对路径入口。Ubuntu 运行不需要 `/nix`、Node 或 Rust；需要发行版 Python 3.11+（tomllib）、btrfs-progs、findmnt、lsattr 和常规 POSIX shell。首发实际验收基线 Ubuntu 24.04。不支持把单个 bin wrapper 移出发行目录；可以整体移动目录。原始 Nix 动态产物不能直接复制冒充此包。

`runtime/share/vaultsyncd/release-manifest.json` 记录产品版本、实际源 commit/NAR、两组件来源、锁摘要、工具链/目标和每个实际文件的长度/摘要。协议版本独立。运行库只从锁定 glibc、GCC 和 SQLite 的目录解析，包含两程序实际链接的 `libsqlite3.so`；清单记录这些库的版本与摘要。邻接 `vaultsyncd-runtime-sources.tar.gz` 提供所封装 glibc/GCC 的对应源码及锁定 Nixpkgs 配方；分发二进制/镜像时必须同时提供此归档。私有运行库许可在 `share/vaultsyncd/licenses`。清单核验失败、dirty source、缺 Web 资源均拒绝，不重标旧包。

## 普通 systemd

先核实 btrfs、CoW、目录权限、可用空间及备份卷的 `user_subvol_rm_allowed`。示例假定 state 为 `/var/lib/vaultsyncd`、Vault 白名单为 `/srv/vaultsyncd-vaults`；已有数据只能显式授权访问，不递归 chown/chmod。此包不会格式化磁盘、自动迁移挂载或放宽整个数据卷。容器 overlay、NOCOW、嵌套子卷及路径/挂载漂移不能充当合规 Vault。

将发行目录完整安装至 `/opt/vaultsyncd`，创建专用无登录用户/组 vaultsyncd（示例容器 UID/GID 8721，普通系统应先排除冲突），显式建立并授权 state/Vault。state 必须由服务用户私有持有，模式 0700。将发行目录 `share/vaultsyncd/vaultsyncd.toml` 安装至 `/etc/vaultsyncd/vaultsyncd.toml`，按实际白名单和回环监听修改。将 `share/vaultsyncd/vaultsyncd.service` 安装至 `/etc/systemd/system/vaultsyncd.service`，在修改路径时同步修改 `ReadWritePaths`/`RequiresMountsFor`。先 `systemd-analyze verify`，再 daemon-reload。

离线 owner 初始化由服务 UID 执行 `vaultsyncd-admin init --state-dir /var/lib/vaultsyncd --login <服务用户名>`（未给 `--login` 时默认取当前运行用户），密码从交互 stdin 输入；不得写在 Nix、命令参数、环境变量、Git 或日志。确认离线配置后启动 unit，核验 health/capabilities、管理 socket 同 UID 准入、样本 Vault/内容/初始采集和备份，再决定 enable。服务无 sudo/CAP_SYS_ADMIN；btrfs 只读快照和删除由普通 UID 在已明确授予的卷内完成。管理 socket 仅本机私有目录，不暴露到反代。

停止使用 SIGTERM，等待120秒宽限后处理异常。升级前保留旧完整发行目录和清单、先按备份流程验证可恢复性；停止旧服务后原子切换完整代码目录，保留数据及配置，启动并核验 server_id/Vault ID/seq/健康。恢复数据遵守 [离线恢复流程](recovery.md)，不能在线直接改数据库。

## 锁定 Ubuntu 镜像与 Compose

`packaging/linux/container/ubuntu-lock.json` 固定官方 Ubuntu24.04 各架构 manifest 与17个实际 .deb 的版本、URL、SHA256。构建驱动先核对完整发行目录/源码归档，再按固定输入取得依赖；已有base须核验确切digest与架构，--offline要求base和全部.deb已缓存，缺失明确失败；Dockerfile 的安装阶段使用 `--network=none`、本地 .deb，不在镜像构建中更新仓库。示例：

```sh
nix develop .#portable-tests --command python3 packaging/linux/container/build.py \
  /实际portable输出/runtime localhost/vaultsyncd:明确来源标识 \
  --offline --deps /实际container-deps输出 --receipt /受限记录目录/image-receipt.json
```

镜像收据包含 image ID、base、版本/源和清单/Ubuntu锁摘要。分发固定 image ID/digest；不要把浮动标签当来源证明。Podman 使用 Docker 格式构建，并用 `podman save --format docker-archive` 传递，转移前后核验 Healthcheck；OCI 格式转移可能丢弃 Docker health 配置。镜像自身不含对应源码大归档，发布渠道需邻接提供。

使用 `deploy/compose/compose.yaml`，明确设定 VAULTSYNCD_IMAGE、VAULTSYNCD_STATE、VAULTSYNCD_VAULTS（可设置 UID/GID/回环端口），预先建立符合存储契约的 bind source。YAML声明 `create_host_path:false`；Podman兼容API实测仍可能忽略此位。必须使用 `python3 deploy/compose/run.py --project-name <独立名称> up`，入口在调用引擎前核验两个绝对bind目录已存在、非符号链接，防止拼错路径被静默创建到overlay；down/stop等清理命令不要求数据路径仍存在。先以同一镜像、同 UID、同 state bind 离线 init owner，再 compose up。默认进程非vault、只读镜像、无 capability、no-new-privileges、无 `/dev/fuse`，端口仅宿主127.0.0.1；生产 HTTPS 接入参照 [NixOS/TLS记录](nixos.md)。

备份需分别核对源快照完整性与导出归档完整性；prepare/verify-source 不能冒充已下载有效包。真实导出后用 `vaultsyncd-admin backup verify` 验证归档，再安全 release。断开传输、进程重启后先查询实际状态，重用原操作标识，不自动删未确认内容。

## 验收与限制

以下 S098/S101 为历史验收记录；ARM64 不属于当前构建、部署或测试支持范围。

S098 使用独立 run_id 的 btrfs loop 数据、Ubuntu容器及专属 ARM64 全系统 TCG 客体，不改现有 trial/Windows 或主机正式服务。QEMU-user 对 btrfs ioctl 的真实失败被保留，未当作运行成功；ARM64 存储验收必须来自完整 Ubuntu ARM64 内核，不能以交叉编译/help 代替。TCG 证据不代表原生性能。具体完成矩阵、源 commit、包/镜像摘要及清理结果见 `agent/tasks/S098.md` 与 curated artifacts。


S098最终实测：产品0.1.1、发行源09d73f9双架构便携包/离线镜像在Ubuntu24.04通过server/admin/Web、overlay拒绝、btrfs bind上传与固定内容下载、备份源与归档分别验证、release和重启幂等；Compose必须使用上述run.py入口，实际缺源拒绝。ARM64另以普通Ubuntu原样systemd unit通过备份/重启/Chromium，服务payload与最终发行逐文件相同（测试源67，差异仅镜像构建脚本）。两处实际目标兼容修复为ARM64目录UAPI位和旧内核INO_LOOKUP的固定根inode查询模式；无capability例外。37份curated元数据及截图见artifacts/S098，敏感/数据盘在外部私有run目录离线保留，专属VM和临时binfmt已停止/移除。没有改主机正式配置或Windows VM。

## S101 同提交平台复核

S101在干净源`fcbbb93ecff408fedc8fbba6006c81b84a151727`重建NixOS安装包、双架构便携包和匹配Ubuntu镜像，不复用旧S098收据。NixOS26.11/kernel7.3是原生包/闭包与页面验证；Ubuntu24.04.5 x64为容器用户态、共享宿主内核；ARM64为完整Ubuntu24.04.5/kernel6.8.0-147的TCG客体，不代表原生ARM性能。三类环境的os-release、kernel、来源与结果分列在`artifacts/S101/public/`。

容器验收除真实普通UID8721/caps0的btrfs上传/提交/固定下载/完整备份/重启，还以归档在**另一组新空目标**执行restore validate/apply，实际启动恢复服务、重新登录、下载同一固定版本并核对新epoch。两架构均通过，不能用源verify或重启旧DB冒称恢复。入口与缺资源78语义在`tools/ci/README.md`。动态qemu解释器和TCG早期SSH单探针失败保留，后按同一命令目录逐job同源续验通过；不声称原整体控制器exit0。

本run实验挂载/loop/容器/binfmt/隧道已清理；停止的ARM复制盘和钥匙已删除，原S098模板、原trial loop0及宿主服务配置不变。外部S101目录保留发行GC roots与非秘密元数据，完整来源/hash/恢复证据和限制见`agent/tasks/S101*.md`。
