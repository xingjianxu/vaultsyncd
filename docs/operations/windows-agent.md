# Windows 用户会话 agent（S070）

`vaultsyncd-agent.exe` 可在 GUI 尚未启动时独立同步；本卡入口是用户会话进程，自动启动注册和 named pipe 管理由后续卡提供。先创建互不重叠的普通本地 NTFS 同步目录、内部缓存目录和状态目录。

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
