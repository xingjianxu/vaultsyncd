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

现场验收、客户端位置、实际 ServerId/VaultId 和证据索引在 [S207交接](../../agent/tasks/S207.md)。S209（2026-11-12）已用管理脚本有序restart到与生产generation282同源的浏览器PDF版包 `/nix/store/dil3kh3r7iphfsiqbgzxv6wkq21kx35x-vaultsyncd-0.1.7`；原dev数据库/Vault/ServerId保持，未重置账号或绑定。HTTPS健康、15项Web字节及身份隔离通过，详见 [S209交接](../../agent/tasks/S209.md)；S208旧包及证据仍保留用于回退。

本机 dev 账号为 `dev`，密码固定为 `1`（用户 2026-11-12 决定：开发环境即使重置，密码也是 `1`）。因此 dev 密码可写入本文；prod 密码仍只保存在 Git 外的受限文件，不写入文档、命令参数或日志。dev 受限密码文件在 `/data/vaultsyncd-dev/S207-prod-dev-20261112/credentials/password`（0600，内容已同步为 `1`）。

重建（重置）dev 实例后，`init` 的密码从非终端 stdin 读取单行，可用管道保持固定密码，不必交互输入：

```bash
./tools/dev-server.sh stop
# 停服后移除 /data/vaultsyncd-dev/dev-server（含 state/vaults），即产生新的 ServerId、数据库与账号
printf '1\n' | ./tools/dev-server.sh init --login dev
./tools/dev-server.sh start
```

重置会更换 ServerId 与 VaultId，Windows 测试机需按新 ServerId 重新绑定，不能沿用旧连接记录。

Windows 11 测试机程序在 `C:\vaultsyncd-dev-client`，默认用户配置已连接 dev，运行该目录的 `Start vaultsyncd.cmd` 可打开；常规测试绑定及客户端 agent 保持运行。初始化其他测试机可在 devShell 中使用 `tests/platform/windows/run-dev-server.sh <完整客户端目录> <fixture目录> <证据目录>`；fixture格式与恢复边界见脚本，已有非 dev 配置须显式迁移，不覆盖。

S210（2026-11-12）已再次有序restart到与生产generation283同包 `/nix/store/c9xkfn6c1gm7lzd86pkab669gk5vj1yx-vaultsyncd-0.1.7`，提供只读文本/代码与Markdown预览；原dev账号/Vault/ServerId保持，三源136文件/408字节及部署包文本13组/PDF图片17组通过，证据见 [S210交接](../../agent/tasks/S210.md)。

S211（2026-11-12）已通过 `restart --package` 有序切换至与生产generation284同包 `/nix/store/6liszy33b10wzpq4xn9aqn7rjamak7i0-vaultsyncd-0.1.7`，管理页面加载失败自动局部恢复；dev原账号/Vault/ServerId保持，与prod身份隔离。三源136文件/408字节及HTTPS/匿名门禁/CSP通过；证据见 [S211交接](../../agent/tasks/S211.md)。

## S213 Office 预览配置

开发实例可使用本机 S212 Document Server。先停止 dev，将共享 JWT 从受限来源交给配置命令；命令只保存本实例 `config/office-jwt` 的0600副本和非机密地址，不重建账号、ServerId、Vault或数据库。

```bash
tools/dev-server.sh stop
tools/dev-server.sh configure-office \
  --office-url https://wlab.132140.xyz:15402 \
  --office-jwt-file /data/vaultsyncd-dev/<受限来源>/office-jwt
tools/dev-server.sh start --package /nix/store/<本次验收包>
```

来源须为当前用户可读的私有普通文件，原始长度最多4096字节，去末尾CR/LF后至少32字节且仅可打印非空白ASCII；内容不进Git/store或日志。命令只能在本实例全部进程停止后执行，并验证原有状态/代理配置。先前生成的精确30秒反代模板在start/restart/configure-office时迁移到130秒，以覆盖120秒转换deadline；任意手改仍拒绝。关闭时stop后执行 `configure-office --disable-office`，保留密钥副本，再start。密钥轮换需同步重启OnlyOffice和vaultsyncd，并重新配置dev副本。

Office预览为PDF，原文件下载保持；支持doc/docx/xls/xlsx/ppt/pptx/odt/ods/odp。输入和PDF各限50MiB，两项活动转换准入，120秒超时；关闭界面撤销交付和临时源，不代表能撤回OnlyOffice已经读取的内容或已入队工作。取消不提前释放转换worker的准入；超时后上游仍可能继续，其执行并发由OnlyOffice自身配置约束。OnlyOffice可能修复部分损坏文件并产出合法PDF，这不改写原件，也不是修复提交。排版、分页、字体和浏览器PDF行为保持第三方边界，不提供交互工作表或在线保存。
