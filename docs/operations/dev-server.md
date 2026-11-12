# 生产与开发测试环境

S207（2026-11-12）：生产入口为 **https://vsyncd.132140.xyz**；开发测试与 Windows 测试机入口统一为 **https://wlab.132140.xyz:15401**。此前请求中的1540已确认应为15401。

| 环境 | 部署与进程 | 数据 |
| --- | --- | --- |
| prod | 既有 NixOS `vaultsyncd.service` 与主机 Caddy，部署仍用 `tools/deploy-local.sh` | `/var/lib/vaultsyncd`、`/data/vaultsyncd/vaults` |
| dev | `tools/dev-server.sh` 启动临时后台 supervisor、后端与独立 Caddy，无 NixOS unit，无开机自启 | `/data/vaultsyncd-dev/dev-server/` |

dev 为独立服务器，初始化产生新的 ServerId、数据库与账号；不复制生产状态或改写旧连接的 ServerId。原有一次性故障测试仍使用各自 run_id 的隔离实例，不能对持久 dev 或 prod 做满盘、回滚及崩溃注入。

## 域名证书

与 `../receiptd/scripts/dev.ts` 相同，直接读取本机已有文件：

```text
/var/lib/acme/wlab.132140.xyz/fullchain.pem
/var/lib/acme/wlab.132140.xyz/key.pem
```

实际签发入口 `/etc/nixos/hosts/wks-lab/default.nix`：`security.acme` 使用 lego 的 Cloudflare DNS-01，主名 `*.wlab.132140.xyz`，SAN 另含 `wlab.132140.xyz`；`environmentFile` 由 agenix 提供 `/run/agenix/cloudflare`。无需另取 token、复制私钥或把机密写入 Nix store。运行用户已在 `acme` 组，文件 `0640 acme:acme`、目录 `0750`；新加入组的会话需要重新登录。

每日 `acme-renew-wlab.132140.xyz.timer` 检查续期。必须使用 **fullchain**，证书须覆盖域名且有效；dev 启动失败时修复证书/权限，不降级到明文 HTTP。证书在进程启动时读取，续期后运行 `./tools/dev-server.sh restart` 重新加载。

## 管理入口

在仓库根目录运行，包装脚本自动进入锁定的 devShell，无需全局安装工具：

```bash
./tools/dev-server.sh init --login dev
./tools/dev-server.sh start
./tools/dev-server.sh status
./tools/dev-server.sh logs
./tools/dev-server.sh logs --follow
./tools/dev-server.sh stop
./tools/dev-server.sh restart
```

`init` 仅首次离线初始化账号，密码从终端或标准输入读取。已初始化不覆盖账号、数据库或内容。`start` 等健康检查与 ServerId 核对通过后返回，进程脱离前台；关闭启动它的终端不停止服务。`logs --follow` 为显式的前台日志查看，Ctrl+C 只退出查看。`stop` 向本实例发送有序终止信号，不按进程名批量 kill，不删除数据。停机后可再次启动；宿主重启后按需手动 start。

默认使用当前已部署服务包的二进制和同包 Web，**只复用程序，不复用生产配置与数据**。需要测试新版本时，在任务工作树用项目 Nix 包构建，再通过 `--package` 指定包路径；通过后生产发布仍走原部署流程。

```bash
mkdir -p /data/vaultsyncd-dev/S207-manual-build
nix build .#default --out-link /data/vaultsyncd-dev/S207-manual-build/server-package
./tools/dev-server.sh restart --package /data/vaultsyncd-dev/S207-manual-build/server-package
```

dev 后端仅监听 `127.0.0.1:18741`，独立代理监听 HTTPS 15401，禁用代理管理接口及自动证书申请，不使用生产的8721/80/443或2019。保留协议16MiB分块上限、SSE及时刷送和后端Web CSP/nonce。证书路径不在运行目录复制。

## 数据与测试机

`/data/vaultsyncd-dev/dev-server/` 中 `state/` 为数据库与内部状态，`vaults/` 为可注册的独立 Vault 基目录，配置、运行信息与日志另存。目录应为服务运行用户拥有的私有 btrfs 目录；`--root` 仅接受开发根内的位置。修改路径前先 stop，不把 dev root 指向生产目录或其软链。

Vault 注册使用运行实例的受限管理 socket，通过 `vaultsyncd-admin vaults register`，不能直接修改在线数据库。Windows 测试机只保存 dev 的 URL/ServerId，通过认证 IPC 管理连接与绑定；不得将 prod 的绑定直接指向新服务器，也不为正常连接安装自签CA或禁用 TLS 验证。

现场验收、客户端位置、实际 ServerId/VaultId 和证据索引在 [S207交接](../../agent/tasks/S207.md)。密码只保存在 Git 外的受限文件，不写入本文、命令参数或日志。

本机 dev 账号为 `dev`，受限密码文件在 `/data/vaultsyncd-dev/S207-prod-dev-20261112/credentials/password`（0600）。Windows 11 测试机程序在 `C:\vaultsyncd-dev-client`，默认用户配置已连接 dev，运行该目录的 `Start vaultsyncd.cmd` 可打开；常规测试绑定及客户端 agent 保持运行。初始化其他测试机可在 devShell 中使用 `tests/platform/windows/run-dev-server.sh <完整客户端目录> <fixture目录> <证据目录>`；fixture格式与恢复边界见脚本，已有非 dev 配置须显式迁移，不覆盖。
