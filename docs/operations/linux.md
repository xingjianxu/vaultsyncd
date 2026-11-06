# 通用 Linux 发行包与容器

## 构建与来源

从干净、已提交的 Git flake 构建，不使用工作树内已有 target/dist：

```sh
nix build .#portable .#portable-aarch64 --no-link --json
nix build .#container-deps .#container-deps-aarch64 --no-link --json
```

两架构分别为 x86_64-unknown-linux-gnu / aarch64-unknown-linux-gnu，Rust 服务端和管理 CLI 与同提交 Web 独立构建，再封装私有 ELF loader、递归解析的运行库及相对路径入口。Ubuntu 运行不需要 `/nix`、Node 或 Rust；需要发行版 Python 3.11+（tomllib）、btrfs-progs、findmnt、lsattr 和常规 POSIX shell。首发实际验收基线 Ubuntu 24.04。不支持把单个 bin wrapper 移出发行目录；可以整体移动目录。原始 Nix 动态产物不能直接复制冒充此包。

`runtime/share/vaultsyncd/release-manifest.json` 记录产品版本、实际源 commit/NAR、两组件来源、锁摘要、工具链/目标和每个实际文件的长度/摘要。协议版本独立。邻接 `vaultsyncd-runtime-sources.tar.gz` 提供所封装 glibc/GCC 的对应源码及锁定 Nixpkgs 配方；分发二进制/镜像时必须同时提供此归档。私有运行库许可在 `share/licenses`。清单核验失败、dirty source、缺 Web 资源均拒绝，不重标旧包。

## 普通 systemd

先核实 btrfs、CoW、目录权限、可用空间及备份卷的 `user_subvol_rm_allowed`。示例假定 state 为 `/var/lib/vaultsyncd`、Root 白名单为 `/srv/vaultsyncd-roots`；已有数据只能显式授权访问，不递归 chown/chmod。此包不会格式化磁盘、自动迁移挂载或放宽整个数据卷。容器 overlay、NOCOW、嵌套子卷及路径/挂载漂移不能充当合规 Root。

将发行目录完整安装至 `/opt/vaultsyncd`，创建专用无登录用户/组 vaultsyncd（示例容器 UID/GID 8721，普通系统应先排除冲突），显式建立并授权 state/Root。state 必须由服务用户私有持有，模式 0700。将发行目录 `share/vaultsyncd/vaultsyncd.toml` 安装至 `/etc/vaultsyncd/vaultsyncd.toml`，按实际白名单和回环监听修改。将 `share/vaultsyncd/vaultsyncd.service` 安装至 `/etc/systemd/system/vaultsyncd.service`，在修改路径时同步修改 `ReadWritePaths`/`RequiresMountsFor`。先 `systemd-analyze verify`，再 daemon-reload。

离线 owner 初始化由服务 UID 执行 `vaultsyncd-admin init --state-dir /var/lib/vaultsyncd`，密码从交互 stdin 输入；不得写在 Nix、命令参数、环境变量、Git 或日志。确认离线配置后启动 unit，核验 health/capabilities、管理 socket 同 UID 准入、样本 Root/内容/初始采集和备份，再决定 enable。服务无 sudo/CAP_SYS_ADMIN；btrfs 只读快照和删除由普通 UID 在已明确授予的卷内完成。管理 socket 仅本机私有目录，不暴露到反代。

停止使用 SIGTERM，等待120秒宽限后处理异常。升级前保留旧完整发行目录和清单、先按备份流程验证可恢复性；停止旧服务后原子切换完整代码目录，保留数据及配置，启动并核验 server_id/Root ID/seq/健康。恢复数据遵守 [离线恢复流程](recovery.md)，不能在线直接改数据库。

## 锁定 Ubuntu 镜像与 Compose

`packaging/linux/container/ubuntu-lock.json` 固定官方 Ubuntu24.04 各架构 manifest 与17个实际 .deb 的版本、URL、SHA256。构建驱动先核对完整发行目录/源码归档，再按固定输入取得依赖；Dockerfile 的安装阶段使用 `--network=none`、本地 .deb，不在镜像构建中更新仓库。示例：

```sh
nix develop .#portable-tests --command python3 packaging/linux/container/build.py \
  /实际portable输出/runtime localhost/vaultsyncd:明确来源标识 \
  --deps /实际container-deps输出 --receipt /受限记录目录/image-receipt.json
```

镜像收据包含 image ID、base、版本/源和清单/Ubuntu锁摘要。分发固定 image ID/digest；不要把浮动标签当来源证明。Podman 使用 Docker 格式构建，并用 `podman save --format docker-archive` 传递，转移前后核验 Healthcheck；OCI 格式转移可能丢弃 Docker health 配置。镜像自身不含对应源码大归档，发布渠道需邻接提供。

使用 `deploy/compose/compose.yaml`，明确设定 VAULTSYNCD_IMAGE、VAULTSYNCD_STATE、VAULTSYNCD_ROOTS（可设置 UID/GID/回环端口），预先建立符合存储契约的 bind source。YAML声明 `create_host_path:false`；Podman兼容API实测仍可能忽略此位。必须使用 `python3 deploy/compose/run.py --project-name <独立名称> up`，入口在调用引擎前核验两个绝对bind目录已存在、非符号链接，防止拼错路径被静默创建到overlay；down/stop等清理命令不要求数据路径仍存在。先以同一镜像、同 UID、同 state bind 离线 init owner，再 compose up。默认进程非root、只读镜像、无 capability、no-new-privileges、无 `/dev/fuse`，端口仅宿主127.0.0.1；生产 HTTPS 接入参照 [NixOS/TLS记录](nixos.md)。

备份需分别核对源快照完整性与导出归档完整性；prepare/verify-source 不能冒充已下载有效包。真实导出后用 `vaultsyncd-admin backup verify` 验证归档，再安全 release。断开传输、进程重启后先查询实际状态，重用原操作标识，不自动删未确认内容。

## 验收与限制

S098 使用独立 run_id 的 btrfs loop 数据、Ubuntu容器及专属 ARM64 全系统 TCG 客体，不改现有 trial/Windows 或主机正式服务。QEMU-user 对 btrfs ioctl 的真实失败被保留，未当作运行成功；ARM64 存储验收必须来自完整 Ubuntu ARM64 内核，不能以交叉编译/help 代替。TCG 证据不代表原生性能。具体完成矩阵、源 commit、包/镜像摘要及清理结果见 `agent/tasks/S098.md` 与 curated artifacts。
