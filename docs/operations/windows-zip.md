# Windows 11 x64 免安装 ZIP

本入口是**程序免安装、状态留在本机**，不是携带用户资料的便携版。S185/ADR-032 起删除 `vaultsyncd-zip.exe`，取消普通启动的程序归属、清单和来源校验，覆盖 S109/S174 的对应限制。当前仅提供 `unsigned-test` 未签名测试包；发布清单不是数字签名。

## 启动与前置

1. 将下载的完整 ZIP 解压到**任意本机普通目录**，直接运行 `vaultsyncd-desktop.exe` 或 `Start vaultsyncd.cmd`。可移动、改名或换解压目录，不需要保持旧路径，也不需要管理员权限。不要在 ZIP 浏览器内运行；缺少真正需要的程序/DLL、Windows 不兼容或权限不足仍会有实际运行错误。
2. 运行环境为 Windows 11 x64 与 Microsoft Edge **WebView2 Evergreen Runtime**。客户端不隐式安装依赖；缺失时按错误提示安装 Runtime。
3. 普通启动**不读取/验证** `zip-owner.json`、`zip-files.json`、`installed-files.json`，不验证程序文件 hash、source 或清单版本。旧 owner 即使指向另一目录或损坏也不影响启动，保留即可，不要为解决启动问题删除任何状态。
4. 首次打开在登录框填写服务端地址、用户名与密码，确认服务器身份后登录。已有配置、DeviceId、会话与内容继续沿用，程序移动不会创建新 profile。新 ZIP 不默认注册开机自启、服务或 Start Menu；关闭窗口后后台仍同步。通知由用户明确开启后注册所需快捷方式。

## 本机状态、同实例及升级限制

- `%LOCALAPPDATA%\vaultsyncd\agent\agent.json`：配置与稳定设备身份。
- 同目录 `state\`、`agent.log`：数据库/内部状态与日志。地址、CA 与上次成功登录名保存在客户端数据库；凭据沿用 Windows Credential Manager。Vault/cache 仍在绑定指定的位置，不随程序目录迁移。
- 历史 `%LOCALAPPDATA%\vaultsyncd\zip-owner.json` 与 `zip-owner-adoptions.jsonl`：不再参与启动，也不会被新启动流程重写或自动删除。
- `installed-files.json`、`zip-files.json`：发布与**显式自动更新**使用的清单，普通启动可没有这些文件；保留它们便于走自动更新，但不能将其恢复或完整性要求变成启动门禁。

同用户会话保持单实例和单 writer。已有健康 agent 时，从另一目录启动桌面也会复用它，而不是拒绝路径或启动第二个 writer。因此移动后仍运行的是旧进程版本时，应先关窗口、安全 Stop，再从所需目录重开。IPC 仍校验真实用户身份，数据/schema/Vault 的保护仍在。

手工替换和移动没有程序或整套状态的备份/回滚，启动也不再替用户发现混搭清单或篡改程序。可从任何方向替换文件，但旧二进制可能不能打开新 schema。托盘“自动更新”仍为 Latest 正式版下载与候选校验、排空/导出、离线程序/状态备份和恢复事务；无需先认领 ZIP owner。安装版的卸载注册表只由真实安装路径维护，不因从另一个目录运行免安装版而接管。若 GUI 来自 B、后台仍在 A，托盘更新会在联网下载/退出 GUI 前说明原因并要求安全 Stop 后从要更新的目录重新打开，不尝试更新 B 后再错误恢复 A。

`agent\maintenance.json` / `agent\auto-update.json` 表示未完成事务，仍会阻止普通启动，不能删除屏障绕过。按实际事务的[恢复说明](windows-installation.md#升级回退与卸载)恢复；取消程序归属校验不等于允许在线覆盖状态或丢弃未确认内容。

## 安全停止与移除程序

双击 `Stop vaultsyncd.cmd` 并选择 Y，或在终端运行：

```powershell
& 'D:\任意目录\vaultsyncd-agent.exe' --user-session health
& 'D:\任意目录\vaultsyncd-agent.exe' --user-session stop
```

Start/Stop 脚本都调用同目录 agent，不再调用 ZIP 专用 EXE。stop 在同次 IPC 连接上认证真实对端用户并固定进程句柄，执行 `PrepareMaintenance`，暂停绑定、保全未确认内容、排空回调，等该 agent 退出并确认 writer 锁释放；可以从另一解压目录安全停止正在运行的实例。不按进程名强杀，不注销 CFAPI、不删除 Vault/cache/dirty/state。失败时保留诊断，不将部分暂停误报为仍在正常同步。

**重新 Start 后绑定保持 PAUSED**。在 GUI 或 CLI 对需要的绑定明确 Resume：

```powershell
& 'D:\任意目录\vaultsyncd-cli.exe' --json bindings
& 'D:\任意目录\vaultsyncd-cli.exe' --json resume --binding-id '<绑定ID>'
```

仅删除程序目录不会删除状态，也不等于安全停用；只有安全停止后才移除不用的程序目录。仅在线占位仍依赖客户端，不能误认完整离线文件。复制程序到另一机器/用户不会携带原状态与凭据，不要复制状态绕过恢复。自动化调用 start/health/stop 时将输出重定向到文件，避免长期子进程继承管道句柄造成等待 EOF。

## 同源构建、收据及宿主检查

开发目录用 `./tools/build.sh`，输出根 `dist/`，四份 EXE（agent/session/CLI/desktop）、两份清单、两个 cmd、README、unsigned 说明和 build-info。发布从已提交干净树走 `./tools/release.sh`，生成四 EXE 同源 Windows ZIP；保留本地收据、完整 ZIP SHA-256、构建日志与发布校验。构建/下载候选检查不是客户端普通启动前置。

```sh
nix develop --command python3 tests/contract/windows_zip.py
nix develop --command cargo test -p vaultsyncd-xtask
nix develop .#windows --command cargo xwin clippy --locked --target x86_64-pc-windows-msvc -p vaultsyncd-agent -p vaultsyncd-desktop --all-targets -- -D warnings
```

打包集合为九项文件加 `zip-files.json` 本身（共十文件），无 `vaultsyncd-zip.exe`。宿主打包契约检查字节/清单/确定性；自动更新解压与候选校验同步适配此集合，仍拒绝路径穿越、链接、重复条目或危险归档。**格式迁移（明确的一次性手工步骤）：已发布 v0.1.7 及更早的旧更新器硬编码旧 11 文件集合，会拒绝本卡新的 10 文件包，不能直接通过旧托盘升级到新布局。** 复现证据见 S185 的 `review-legacy-update/`。不要为兼容旧更新器重新放回用户要求删除的 EXE；从发布页手工下载新完整包，关闭旧界面并安全停止旧 agent（旧 Stop 脚本仍调用其旧专用入口），再从任意新目录打开新 desktop；本机原状态继续沿用。旧发行包/标签不改写，本卡不发布新 Release；未来首次发行新布局必须在发布说明明确提示此次手工迁移，不得宣称旧 Latest 可直接托盘升级。迁移后的新客户端恢复使用新集合进行后续托盘自动更新。

## 自动化真实 GUI / dirty 重启验收

`tests/platform/windows/run-native-startup.sh` 现在运行 `s185-direct-startup.ps1`，在已登录的实验室 Windows 桌面、隔离 LOCALAPPDATA 下验证：无清单首次直开、旧损坏 owner 与清单不影响启动、第二目录复用健康实例、跨目录安全停止、冷启动与移动改名保留身份、Start cmd、单实例、无效配置与维护屏障保留。不得用交叉编译替代这份原生结果。

同步内容/CFAPI 仍使用既有真实绑定验收入口，不因本卡启动改动宣称已经同步：S167/S080/S081 的绑定与文件验证要求仍保留；需真实隔离服务端与客体本地 NTFS Vault。标准用户长期 Fresh/Live/Stop 入口为 `tests/platform/windows/s109-zip.ps1`（已按新 agent CLI 更新），Live 必须配置绑定并由宿主独立核验文件哈希。所有故障与导出只在带 run_id 的实验室实例进行，不清理用户状态。

## 手工标准用户原生验收入口

在手工登录的独立标准用户桌面，解压完整包到带空格/中文目录，直接打开桌面、关闭窗口后确认同步继续；安全停止后移动目录再开，核对原 DeviceId/绑定/未确认内容。另开一个解压副本验证复用同一健康 agent，不应报 `ZIP_OWNER_MISMATCH`。验证新程序不创建 owner，已有 owner 保持原字节；测试只改隔离样本，不编辑真实在线状态。原生 CFAPI、多机、性能容量与最终同源发行仍为独立首发门禁。
