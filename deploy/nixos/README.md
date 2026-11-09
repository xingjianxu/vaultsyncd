# NixOS 模块（S095）

模块从 `nixosModules.default`（同别名 `vaultsyncd`）导出。以下只展示声明，不代表本卡启用了主机服务。实际主机集成见 S096，HTTPS 见 S097。

```nix
{ inputs, ... }: {
  imports = [ inputs.vaultsyncd.nixosModules.default ];
  services.vaultsyncd = {
    enable = true;
    # package 默认是该 flake 同源的 Linux server/admin/Web 发行包。
    stateDir = "/var/lib/vaultsyncd";
    vaultPaths = [ "/srv/vault-data/documents" ];
    settings.server.listen = "127.0.0.1:8721";
    settings.log = { level = "info"; format = "json"; };
    # extraGroups = [ "vault-documents" ]; # 管理员明确授予，模块不修改 Vault。
  };
}
```

`user/group` 默认 `vaultsyncd`，是专用非 vault 账号。`createUser=false` 可使用已在 NixOS 定义的普通非 vault 用户及组；模块不改写其账号类型、主组、描述或附加组，`extraGroups` 仍作为服务的 SupplementaryGroups。主机 feature 默认取 `host.user` 及该账号的 NixOS 主组，本机为 `xingjian:users`。目录所有权迁移须在服务停止时由运维显式完成。`vaultPaths` 是允许注册的基目录，同时成为 systemd `ReadWritePaths`；空列表拒绝全部 Vault 注册。Vault 与状态必须互不包含，不能选择 `/`、`/etc`、`/nix` 等系统目录。服务还要通过产品 Vault 注册流程检查 FSID、身份、CoW、嵌套挂载/子卷与扫描完整性。启动 preflight 仅只读检查顶层目录，避免把合法的备份快照当作非法嵌套子卷或递归扫描用户资料；注册准入不是由路径前缀代替的。

管理员须先准备真实 btrfs 挂载，开启 CoW/checksums，并为备份所在挂载明确设置 `user_subvol_rm_allowed`。普通服务 UID 用它创建、只读快照与释放备份；服务不使用 sudo，也没有 CAP_SYS_ADMIN/任何 capabilities。`RequiresMountsFor` 包括状态、内部 `backups` 与全部 Vault 基目录；独立挂载必须在主机 `fileSystems` 中显式声明。模块不格式化、挂载、迁移、创建用户 Vault 或递归改权。内部备份布局由产品管理：`<state>/backups/<id>/{prepared,snapshot}`，Vault 内禁止嵌套挂载/子卷。不要对现有资料或快照使用 `chattr -R`，也不要把内部状态放到 Vault 下。

默认 `/var/lib/vaultsyncd` 用 `StateDirectory` 创建，模式0700，工作目录和数据库写入只在内部状态。`/var/lib/` 下的其他绝对路径也由 StateDirectory 管理，仅能指向专用内部状态；systemd 可能纠正该内部状态的所有权，不能指向用户数据。其他 `stateDir` 必须由管理员提前创建为服务 UID 所有的0700目录；preflight 不替用户修复权限、符号链接或不合规文件系统。

对既有 Vault，管理员使用明确的目录/文件 ACL 或预先规划的组授权，并保证父路径可遍历；`extraGroups` 只把该账号加入明确的现有访问组，不授予文件权限。单个示例：`setfacl -m u:vaultsyncd:rwx /srv/vault-data/documents`，既有文件及子目录需要逐项授权或由资料拥有者明确采用组策略；这条命令不递归变更内容权限。未授权即拒绝启动/注册。服务的 `ProtectSystem=strict`、`ProtectHome=read-only`、`ReadWritePaths`、`UMask=0077`、`NoNewPrivileges` 与空 capability 集继续限制其访问，即使白名单外目录 DAC 允许写入也不可写。没有自动打开防火墙。

生成的 TOML 只含监听、日志和路径，没有自由格式 secret/auth 字段，也没有初始密码选项。owner 初始化在服务离线时执行 `sudo -u <服务用户> <package>/bin/vaultsyncd-init`（包装命令，默认 `--login $adminUser`，可再用 `--login` 覆盖），从终端或匿名 stdin 管道输入密码；不要把密码写进 Nix 表达式、环境、argv 或 store。在线管理经同 UID 的 `<state>/admin/socket`，不能直接写在线库。

## 隔离验收

`eval.nix` 是独立 NixOS 配置 fixture，根设备 `/dev/test-only` 不可拿去部署或启动。本卡先求值整个 `system.build.toplevel`，构建完整 NixOS system 及实际 unit/config/preflight（不激活），再将生成 unit 改用唯一测试名运行；不激活 fixture、不启动正式 `vaultsyncd.service`。

```sh
# 参数为已经构建的真实同源发行包和 flake.lock 对应 nixpkgs 源；不用占位程序。
nix develop --command python3 deploy/nixos/test-service.py /nix/store/…-vaultsyncd-0.1.0 /nix/store/…-source
```

脚本只为带 `vs-s095-时间-pid` 的独立1GiB loop btrfs 进行格式化/挂载，创建并最终删除独立临时 system UID 和 `/run` test unit。需 passwordless sudo、btrfs 工具和 ACL 工具。真实服务以该 UID、零 capabilities 运行，验证 Vault 授权前拒绝、CoW/挂载选项失败、可写 Vault/系统拒写、注册白名单、备份只读快照/校验/释放，密码仅 stdin，资料原 owner/bytes 保留。失败也保留结果与 journal，finally 停止本 test unit、卸载、删除 loop image 和临时账号，不改正式主机配置。

## 本机一键更新（S113）

先在项目中提交更新，再以普通用户执行 `./tools/deploy-local.sh`。默认部署项目 HEAD 至 `/etc/nixos#<本机 hostname>`，可传 `--rev <commit/tag>`、`--config <主机仓库>`、`--host wks-lab`；`--build-only` 仅构建，保留证据/配置树，不激活和改动主机分支。需要 sudo 激活权限。

脚本拒绝项目/主机未提交修改，仅在独立 host worktree 改唯一 `vaultsync.rev`。所有 Nix 构建/求值显式 `--option pure-eval true --no-update-lock-file --no-write-lock-file`，项目从主机 feature 指定的永久 Git 仓库完整 SHA 取源；禁止临时工作树作为部署源。主机依赖锁逐字节核对。build 后按相同 store path 执行 dry-activate/test/健康/switch，核对真实 HTTPS 证书、ServerId、运行包及 UID、目录 UID/GID、同 UID IPC ready；成功以中文提交版本 pin 并 ff 到原主机分支，清理临时树/分支，不推送。

证据默认 `/data/vaultsyncd-temp/evidence/deploy/local-*`，包含旧运行/启动 system GC root、锁、generation、差异及结果。`test` 立即激活，没有定时自动回滚；失败保留现场并尝试恢复旧运行及启动配置。数据库迁移仍由产品围栏保护；旧版本拒绝新 schema 时必须按备份恢复流程离线处理，不自动降级数据。脚本不清空数据、不递归改权、不更新其他应用依赖。部署前按产品备份流程保留可恢复备份，日常脚本健康检查不创建上传样本。当前路径、账号迁移与真实验收见 `docs/operations/nixos.md` 的 S113 节。
