# Windows 用户会话 agent（S070—S073）

`vaultsyncd-agent.exe` 可在 GUI 尚未启动时独立同步；入口是用户会话进程。S071 提供受限 named pipe 状态查询，S072 提供 1.2 管理命令与持久连接，S073 提供只经该管道工作的 `vaultsyncd-cli.exe`。自动启动注册仍由后续卡提供。先创建互不重叠的普通本地 NTFS 同步目录、内部缓存目录和状态目录。

最小 JSON 配置如下。所有 ID 换为真实的 32 位小写十六进制稳定身份；`root_id` 来自已授权服务端 Root，重启必须复用原身份及绑定路径。

```json
{
  "server": {"id": "11111111111111111111111111111111", "url": "https://sync.example.test"},
  "device_id": "22222222222222222222222222222222",
  "state_dir": "C:/vaultsyncd/state",
  "poll_millis": 1000,
  "bindings": [{
    "id": "33333333333333333333333333333333",
    "root_id": "44444444444444444444444444444444",
    "path": "C:/Sync/Work",
    "cache": "C:/vaultsyncd/cache/work",
    "mode": "full"
  }]
}
```

`mode` 支持 `full`、`on_demand`。配置最多 1 MiB，拒绝未知字段和凭据字段；轮询范围 250—60000 ms。默认要求 HTTPS；可用 `server.ca_file` 指定额外 CA 文件，仍保留系统信任。仅隔离开发环境可显式设置 `development_http: true` 使用 loopback HTTP。

在实际登录的 Windows 用户会话运行：

```text
vaultsyncd-agent.exe --config C:/vaultsyncd/agent.json --login-stdin
vaultsyncd-agent.exe --config C:/vaultsyncd/agent.json
```

首次命令从临时 stdin 读取协议 `LoginRequest` JSON（`login`、`password`、`device_label`），不把密码放在配置或命令行。认证结果仅写当前用户 Windows 凭据库；无库或持久化失败明确拒绝同步。SSH 网络 logon 不能替代交互用户凭据库。`--once` 执行一轮后退出，持久 pending 仍需后续轮次；正常长期运行会自动恢复并重试。相同会话或相同状态库的第二实例在打开 SQLite 前以 `ALREADY_RUNNING`、exit 2 拒绝。CTRL_C/CTRL_BREAK 请求取消回源、断开 CFAPI、排空工作并关闭连接池。

启动先检查 pinned 路径与单实例，再打开 SQLite；平台 publication、对应 release/namespace 意图、core ControlledApply 恢复都成功后才放行新同步。无法解释的意图保留现场并停写。确认的远端删除把完整本地对象保留到内部缓存；本卡不自动清理完整缓存。手动释放入口和显式管理动作由后续 IPC/命令卡接入。

真实 Linux 服务端与 Windows 原生验收入口：先在默认 devShell 构建 `cargo build --locked -p vaultsyncd-server-tests --example release-native-server`，再在 Windows devShell 运行 `tests/platform/windows/run-native-process.sh`。桥接只开放 guest loopback 18770，fixture 为短命合成测试身份，日志和 fixture 仅在 ignored artifacts 内；退出清理服务与 SSH tunnel。

## Explorer 占位状态（S129）

正式 agent 在启动恢复及新增绑定时注册 Explorer 同步根；旧版本仅注册 CFAPI 的目录会在新版 agent 启动时补齐，继续使用原 BindingId、RootId 和路径。注册失败明确报错，不通过注销根修复，也不重新下载或释放现有文件。退出后保留注册与占位文件。

在 Explorer 的同步目录查看“状态”列：未下载文件显示空心云朵；完整下载的文件显示绿色勾选；通过产品“释放空间”成功释放后恢复云朵。图标由 Windows 根据真实驻留和 pin 状态生成。切换到按需模式不会自动释放已下载内容。若列被隐藏，在详细信息视图的列标题菜单中显示“状态”；本程序不修改用户的 Explorer 全局显示设置。

原生复验：默认 devShell 构建 Linux helper 后，Windows devShell 执行 `VAULTSYNCD_PROCESS_TASK=S129 tests/platform/windows/run-native-process.sh`。脚本要求已登录桌面，仅打开/关闭本轮隔离根的 Explorer 窗口；证据保存在 `artifacts/S129/native-*/explorer-evidence/`。验收使用正式 agent 的启动和管理 IPC，不由测试代注册；覆盖 CFAPI-only 修复、新增绑定、下载/释放、重启、文件身份及浏览不回源。Shell 属性 `System.StorageProviderState` 按锁定 SDK 的 sparse=1、in-sync=2 核对，并保留实际 Explorer 截图。

## 本机状态 IPC（S071）

真实恢复完成后进程绑定 `\\\\.\\pipe\\vaultsyncd-agent-{token SID}-{token session ID}`；GUI/CLI 应复用 `vaultsyncd_platform_windows::ipc::Endpoint::current("vaultsyncd-agent").request(&Request)`，不猜 SID、不直接打开 `client.sqlite`，不初始化 CFAPI。协议 1.2 的 21 项命令闭集与参数见 `api/local-ipc.schema.json`；请求为 4 字节大端长度 + JSON，1.2 向后接受更低的 minor。`get_status` 应答包含版本、`bindings` 多维核心快照与 `management` 结果；计数均十进制文本，不含本机路径与凭据。快照读取失败是失败连接，不是空列表或同步成功；每个 Binding 原子读取，不承诺跨 Binding 一致时刻。

管道只允许当前用户、拒绝远程并双向核对 token；同用户不同权限进程共享信任边界，不把本机管理员视为隔离沙箱。最多8连接，额外连接关闭；完整帧、处理、写入各5秒超时。客户端断线可重连；退出取消部分帧并在 DB 关闭前释放监听。登录专用进程不启动 IPC。

原生传输与错误状态验证：Windows devShell 执行 `tests/platform/windows/run-native-ipc.sh`（临时第二本机账号在 finally 删除）；实际进程回归按 `VAULTSYNCD_PROCESS_TASK=S071|S072|S073 tests/platform/windows/run-native-process.sh` 选择（先构建上文 Linux helper）；各卡使用独立 artifacts/guest driver，不执行 S070 旧样本清理。S073 模式的测试与 CLI 见下节。

## Windows CLI（S073）

`vaultsyncd-cli.exe` 是同一用户会话里的最小管理入口，只调用运行中 agent 的 named pipe，不打开 `client.sqlite`、不启动 CFAPI、不自己执行同步。常用命令：

```text
vaultsyncd-cli.exe --json status
vaultsyncd-cli.exe --json pause --binding-id <32位小写十六进制>
vaultsyncd-cli.exe --json download --binding-id <ID> --entry-id <ID>
vaultsyncd-cli.exe --json release --binding-id <ID> --entry-id <ID>
vaultsyncd-cli.exe --json export-recovery --binding-id <ID> --destination C:/recovery
vaultsyncd-cli.exe --json roots
vaultsyncd-cli.exe --json history --binding-id <ID> --entry-id <ID>
vaultsyncd-cli.exe --json restore --binding-id <ID> --entry-id <ID> --operation-id <ID> --base-revision <N>
```

`--help` 列出全部命令。登录只能用 `vaultsyncd-cli.exe login --username <账号>`；密码从终端安全输入，或由脚本经标准输入首行提供，**不接受命令行密码**，不会进入 shell 历史、输出或日志。

`--json` 成功/失败均为单行 JSON；没有 `--json` 时输出简短中文摘要，错误写 stderr。退出码固定为 `0` 成功、`2` 用法/输入错误、`3` 无法连接 agent、`4` 未登录/需重新认证、`5` agent 拒绝或执行失败、`6` 协议/应答不兼容。脚本应同时检查退出码与 JSON `error.code`；空结果与失败不能混为一谈。

原生验收：构建 Linux `release-native-server` helper 后，在 Windows devShell 执行 `VAULTSYNCD_PROCESS_TASK=S073 tests/platform/windows/run-native-process.sh`。测试使用真实 agent/CLI/HTTP/SQLite/NTFS，并以进程句柄枚举证明样例管理请求期间 CLI 没有打开 `client.sqlite`；测试只清理自己的 `run-cli-*` 目录与凭据。

## 删除与离线修改并发（S074 实测行为）

- 服务端 `delete` 事件现在携带被删版本的 `version_id`/`hash`/`size`（客户端删除与外部删除一致）。客户端同 revision 的已确认删除基线因此能与事件事实相等，不会整批拒绝或把旧条目复活。
- 远端删除落在“本地已修改但尚未上传”的同一路径时，agent 先保留本地未确认内容；上传协调完成后可用**新 EntryId** 重新占用同名路径。旧 EntryId 不复活，原内容在回收站/历史可查。
- 被腾空的路径代际不会清零：本地 preserve-both 改名后远端版本在原路径安装会得到更大的 generation，避免与已持久化证据冲突。
- CFAPI `adopt_complete` 只允许把**完整本地内容**（`on_disk >= size`，同 Binding/Root）重新绑定到新条目；不完整占位仍拒绝换身份，避免丢弃另一个条目的云状态。

复现入口见 `docs/operations/windows-lab.md` §20 与 `tests/platform/windows/run-native-e2e.sh`；三个客户端是同一台客体的三个隔离身份、agent 顺序运行。
