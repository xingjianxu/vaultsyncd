# Windows 11 x64 免安装 ZIP

本入口是**程序免安装、状态留在本机**，不是携带用户资料的便携版。当前仅提供 `unsigned-test` 未签名测试包；hash/来源清单不是数字签名。默认不添加 HKCU Run、Windows 服务或 Start Menu 快捷方式；未登录该用户或重启机器后不会自行同步。ZIP 新配置的系统通知默认关闭；用户在界面明确开启通知后，Toast 投递会注册本应用的 Start Menu 快捷方式，关闭通知不会清除问题与待办。

## 启动与前置

1. 从发布组件 receipt 核对 `vaultsyncd-<版本>-x64-unsigned.zip` 的完整 SHA-256，解压**全部**文件到新的本机普通目录，不能位于 `%LOCALAPPDATA%\vaultsyncd` 内。支持中文/空格路径；不支持 UNC、junction/reparse 程序目录。不要直接在 ZIP 浏览器内运行。
2. 必须有 Microsoft Edge **WebView2 Evergreen Runtime**，不是仅有 Edge 浏览器。入口按微软[Runtime 分发文档](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution)读取 Runtime 客户端 pv 版本，缺失时明确提示[微软下载页](https://developer.microsoft.com/microsoft-edge/webview2/)，退出且不开 GUI，不隐式下载。只支持 Windows 11 x64。
3. 双击 `Start vaultsyncd.cmd`。专用 `vaultsyncd-zip.exe start` 验证组件 hash、创建无凭据初始配置，使用固定路径 CreateProcess 启动独立 agent。只有同用户、正确程序路径的实际 IPC `GetStatus` 成功且进程仍存活后，才启动 GUI；不能把 spawn 成功视为健康。出错窗口会保留诊断，不用管理员权限“修复”。
4. 在 GUI 配置真实服务端、登录并建立绑定。关闭 GUI **仍继续同步**。再次 Start 复用同一健康 agent，不启动第二个 writer；GUI 本身已有单实例约束。ZIP 默认不注册 Start Menu AUMID 快捷方式，通知须用户显式开启；开关及注册影响在“问题与恢复”的通知面板说明。

程序文件包括原四 EXE（agent/session/CLI/desktop）、专用 ZIP EXE、`installed-files.json`、`zip-files.json`、两个双击 cmd、README 和未签名说明。不是安装器，无 `uninstall.exe`，不运行 setup。CLI 仍通过真实 IPC。

## 本机状态、同实例及升级限制

- `%LOCALAPPDATA%\vaultsyncd\agent\agent.json`：无凭据初始连接与稳定设备身份。
- 同目录 `state\`、`agent.log`：数据库/内部状态及诊断。`zip-stop-*\`：停止前按绑定导出的未确认内容。凭据仍在该 Windows 用户 Credential Manager；Vault/cache 仍在各绑定选择的位置。
- `%LOCALAPPDATA%\vaultsyncd\zip-owner.json`：持久绑定规范程序路径与完整 ZIP 组件清单摘要。普通 `--config` agent 和 session launcher 也会检查所有权；writer 锁/单用户 IPC 沿用现有机制，没有新增数据库 schema 或 IPC wire 字段。

同用户的 ZIP/安装版共享固定 IPC 与默认 LocalAppData 命名空间，**不允许两版混跑**。首次 ZIP 拒绝已有安装注册，以及任何已有/未知 agent 配置或非空状态（包括离线状态）。已有 ZIP 只允许同一来源、同一原目录复用同一个实例；另一解压目录、移动/重命名目录、修改来源清单或覆盖新版均明确拒绝。安装器和安装维护控制器亦拒绝接管 ZIP 所有权。

**本版本不支持 ZIP 升级，也不支持 ZIP/安装版互转已用状态。** 不应以覆盖 EXE、删除 `zip-owner.json`、删除 profile 或“先卸载再重装”绕过 S100 的停机/未确认内容导出/整套离线备份/迁移恢复门禁。保留原 ZIP 和目录继续使用；需要升级时先联系维护者安排受支持的迁移与恢复流程。现有安装版应继续使用[安装版 S100 升级入口](windows-installation.md#升级回退与卸载)，不能把 ZIP 当候选直接替换。限制是明确拒绝，不是自动创建新 DeviceId 丢弃旧状态。

不要把状态文件复制进 ZIP 以便迁移机器。复制程序到另一台机器/另一用户后，只会产生其本机新身份，不会带走原凭据或未确认内容。原 Vault/云端占位依赖原本机提供程序。

## 安全停止与移除程序

双击 `Stop vaultsyncd.cmd` 并选择 Y。或者在同一用户终端运行：

```powershell
& 'D:\中文 空格\vaultsyncd-zip.exe' health
& 'D:\中文 空格\vaultsyncd-zip.exe' stop
```

停止入口在实际发送请求的同一条 IPC 连接上认证对端路径，并保留执行维护准备的确切进程句柄，复用 `PrepareMaintenance`：持久暂停全部绑定、断开/排空回调、导出每份未确认内容，然后等待该 agent 正常退出并验证 writer 锁已释放。已经离线时也需成功取得锁才报告已停。绝不 `taskkill /IM`，不强杀，不注销 CFAPI，不删除 Vault/cache/dirty/state。导出不足、回调忙、权限/锁/IPC 失败都返回失败且保留诊断；可能已经部分暂停/断开，不要把失败误当“仍全部正常同步”。

**重新 Start 后，绑定保持 PAUSED。** 这是沿用维护准备语义，不自动恢复可能原先就由用户暂停的绑定。用户确认后在 GUI 或 CLI 对需要的绑定逐一 Resume：

```powershell
& 'D:\中文 空格\vaultsyncd-cli.exe' --json bindings
& 'D:\中文 空格\vaultsyncd-cli.exe' --json resume --binding-id '<绑定ID>'
```

停止后仅云端/部分占位不是完整离线文件；不能因为仍显示名称/逻辑大小就认为已下载，需重启原 ZIP 提供程序并适当 Resume。若只移除程序：关闭 GUI、完成安全 Stop 后，仅删除该解压程序目录，保留本机状态、Vault/cache/dirty/提供程序注册；建议保留原 ZIP 以便恢复原目录运行。再次需要同步前必须恢复原始程序目录。ZIP 没有数据清理或提供程序注销选项，不能用它绕过安装版的完整驻留/revert/hash 注销保护。

自动化调用 `vaultsyncd-zip.exe` 时把输出重定向到**文件**，不要接管道：入口派生并长期运行的 GUI/agent 子进程会继承调用方的标准句柄，管道要到它们退出才收到 EOF，看似 `start` 不返回（双击 cmd 与文件重定向不受影响）。这是当前已实测的限制，等待句柄继承收敛后再评估是否移除。

若存在 `agent\maintenance.json`，普通 Start/Stop 都不删除屏障或默认成功。保留原程序、状态与诊断，按实际事务的[受控恢复说明](windows-installation.md#升级回退与卸载)处理；不要混用安装事务候选接管 ZIP profile。

## 同源构建、收据及宿主检查

在已提交、干净的任务树内，准备 S009 锁定交叉工具链缓存后：

```sh
nix develop .#windows --command cargo xtask release build windows-client artifacts/S109/client-new
nix develop --command cargo xtask release assemble artifacts/S109/release-new.json artifacts/S109/client-new
nix develop --command cargo xtask release verify artifacts/S109/release-new.json
```

同一次 fresh release cross-build 生成五 EXE：NSIS 仍消费原四 EXE；额外 ZIP EXE 只进入 ZIP。不重复全新编译、不从开发 target 借 PE。`packaging/windows/zip/build.py` 使用 Python 标准库固定文件清单、归档时间与每文件 hash，并在生成后读回全部 ZIP 字节核对。`zip-files.json` 包含五 EXE、installed 清单、cmd/README/unsigned 说明的十份摘要；receipt 递归覆盖展开的十一份文件及**完整 ZIP 自身 hash**，assemble/verify 沿用同一 HEAD/全部文件完整性检查。原四 EXE 在两种包中是同一字节来源。unsigned 标记不是签名，也不保证整个 cross-build 逐字节可重现。

针对性检查：

```sh
nix develop --command python3 tests/contract/windows_zip.py
nix develop --command cargo test -p vaultsyncd-xtask
nix develop .#windows --command cargo xwin clippy --locked --target x86_64-pc-windows-msvc -p vaultsyncd-agent --all-targets -- -D warnings
nix develop .#windows --command cargo xwin test --locked --target x86_64-pc-windows-msvc -p vaultsyncd-agent --lib --no-run
```

Windows 测试 EXE 内有 `portable::tests::*` 和 `portable::webview::tests::*`。交叉编译/宿主打包测试不代表原生通过。脏树不能生成正式收据；父审查提交后才能执行最终 fresh build，不能为出包放宽该门禁。

## 自动化真实 GUI / dirty 重启验收

宿主已有 Windows 交互桌面且该会话没有旧 agent 时，可使用同源组件执行（不改登录、不重启操作系统）：

```sh
nix develop --command python3 tests/platform/windows/s109-e2e.py artifacts/S109/client
```

脚本创建本 run 无登录触发器的 Limited/Interactive task，同 worker 内运行 ZIP/GUI/CLI，隔离 LocalAppData/AppData/WebView，并用真实 Linux btrfs fixture、TLS 与上传断链代理。覆盖：

- ZIP 同源与逐文件 hash、`zip-files.json`/installed 清单来源、未签名状态；
- `start` 退出码 0、真实 agent IPC 健康、恰好一个由本 run 程序目录启动且可见/有主窗口句柄/响应的窗口；**重复 start 复用同一 agent 与同一窗口**；
- 通过同一会话的真实 CLI 完成连接/登录/Vault/绑定（不依赖调试端口），首次非空合并 synchronized，GUI 窗口与 CLI 为同一 agent 实例；
- 关闭窗口（WM_CLOSE）后 agent PID 不变，随后新建文件由后台真实上传，宿主从 Linux fixture 的 Vault 目录独立核对字节与 SHA-256；
- 上传断链代理阻止 begin 造成真实 dirty：安全 `stop` 后 agent 正常退出、未确认字节保留，重新 `start` 后 DeviceId/Vault/绑定不变、绑定仍 PAUSED、dirty 仍存在且服务端仍是旧内容；显式 `resume` 后服务端字节变为新内容并独立核对 hash；
- 默认状态快照（HKCU Run、HKLM、开始菜单/桌面快捷方式）在全程不变，通知默认关闭；`finish` 只对本 run 随机 `server_id` 做 IPC logout。

`result.json` 的 `restart_kind` 明确写“仅 agent 进程重启”，`boot` 不变，**不代表 OS 重启或登录自启动**。本机 Quickemu 关闭 UAC，Limited 任务仍是提升令牌，结果同时记录 `elevated_session=true`、`standard_user_proven=false`，不冒充标准用户；标准用户路径由下面的手工入口覆盖。

真实原生结果（`3512865`，unsigned-test ZIP `8c1dc94b…2b5`）：全部动作通过，关窗后上传、dirty 保留与 resume 后提交的宿主独立 hash 一致，默认 Run/HKLM/快捷方式未变，agent 安全停止、无强制结束；Vault/state/cache/recovery 保留在客体。

WebView2 157 起本机不再应用 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`：直接启动桌面也不出现 `--remote-debugging-port`，因此自动化不依赖 CDP，也不再操作页面表单；GUI 表单交互的原生证据仍由安装版 S099 验收覆盖，本卡不重复声明。另一个已实测限制是入口派生并长期运行的 GUI/agent 子进程会继承调用方标准句柄，自动化调用应把输出重定向到文件而不是管道。

## 手工标准用户原生验收入口

先协调 Windows 独占窗口。由操作者**手工登录新的独立标准用户**，安装好 WebView2；不得使用旧用户、不重置旧密码、不改 LSA/自动登录、不重启 VM。把同源 ZIP、receipt 和 `tests/platform/windows/s109-zip.ps1` 交给该用户可读目录，receipt 中对应完整 ZIP 项给出 SHA-256 与完整源提交。

在该用户的真实桌面 PowerShell 执行（示例路径须替换）：

```powershell
& 'C:\lab\s109-zip.ps1' -Mode Fresh `
  -RunDirectory "$env:USERPROFILE\Documents\s109-run-001" `
  -ZipPath 'C:\lab\vaultsyncd-0.1.1-x64-unsigned.zip' `
  -ZipSha256 '<receipt中完整ZIP的SHA256>' -ExpectedCommit '<完整40位commit>'
```

Fresh 要求目标目录全新、当前用户没有任何 vaultsyncd 状态/安装注册，遇到已有内容直接停止而不删除。验证完整 archive/展开文件来源、真实 cmd 中文空格路径、重复 Start 同 PID/IPC、关闭 GUI 后 IPC 存活、安全 Stop、停止幂等、另一目录拒绝、来源变化拒绝、重新启动身份稳定、无 Run/StartMenu/安装注册变化。保留原目录与另一份拒绝样本，最后 agent/GUI 留作真实服务配置；结果在该 run 的 `results/fresh.json`，它明确只是 launcher smoke。

通过真实 GUI/CLI 配置**隔离的真实服务端与独立 NTFS Vault/cache**，完成登录/绑定后，再执行 Live（不接受空配置冒充同步成功）：

```powershell
& 'C:\lab\s109-zip.ps1' -Mode Live `
  -RunDirectory "$env:USERPROFILE\Documents\s109-run-001" -BindingId '<活动绑定ID>'
```

Live 关闭确切路径的 GUI、确认同一 agent 仍运行，写入唯一内容并等待该绑定真实 synchronized；安全 Stop 后核对文件 hash、Vault/cache/state 保留，重新 Start 后验证绑定仍暂停。`results/live.json` 给出唯一文件名、字节数及 SHA-256；**宿主必须另从真实服务端读取并比对这份内容**，guest 的 synchronized 不替代独立 hash 证据。未确认 dirty/导出完整性、缺 WebView2、旧安装状态拒绝、维护屏障故障仍需原生矩阵补齐，不能以 Fresh 的空绑定 Stop 代替。

最后安全停止本 run：

```powershell
& 'C:\lab\s109-zip.ps1' -Mode Stop -RunDirectory "$env:USERPROFILE\Documents\s109-run-001"
```

脚本不注销提供程序、不删用户/profile/Vault/cache、不按进程名结束任务；失败保留所有现场。原生执行与最终交接由父协调，本说明本身不声称 Windows 验收已完成。
