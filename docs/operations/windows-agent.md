# Windows 用户会话 agent（S070、S071）

`vaultsyncd-agent.exe` 可在 GUI 尚未启动时独立同步；入口是用户会话进程。S071 提供只读 named pipe 状态查询；管理命令和自动启动注册由后续卡提供。先创建互不重叠的普通本地 NTFS 同步目录、内部缓存目录和状态目录。

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

## 本机状态 IPC（S071）

真实恢复完成后进程绑定 `\\\\.\\pipe\\vaultsyncd-agent-{token SID}-{token session ID}`；GUI/CLI 应复用 `vaultsyncd_platform_windows::ipc::Endpoint::current("vaultsyncd-agent").request(&Request)`，不猜 SID、不直接打开 `client.sqlite`，不初始化 CFAPI。请求仍只有 `get_status`，4 字节大端长度 + JSON，协议 1.1 向后接受 1.0 请求。应答包含版本和 `bindings` 多维核心快照；计数均十进制文本，不含本机路径与凭据。快照读取失败是失败连接，不是空列表或同步成功；每个 Binding 原子读取，不承诺跨 Binding 一致时刻。

管道只允许当前用户、拒绝远程并双向核对 token；同用户不同权限进程共享信任边界，不把本机管理员视为隔离沙箱。最多8连接，额外连接关闭；完整帧、处理、写入各5秒超时。客户端断线可重连；退出取消部分帧并在 DB 关闭前释放监听。登录专用进程不启动 IPC。

原生传输与错误状态验证：Windows devShell 执行 `tests/platform/windows/run-native-ipc.sh`（临时第二本机账号在 finally 删除）；实际进程加原 S070 产品回归：`VAULTSYNCD_PROCESS_TASK=S071 tests/platform/windows/run-native-process.sh`，先构建上文 Linux helper。S071 使用独立 artifacts/guest driver，不执行 S070 旧样本清理；VM保持在线。管理命令、版本化推送及全量恢复属于 S072，不是本卡提供的接口。
