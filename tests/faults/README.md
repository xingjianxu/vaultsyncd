# S102 事件异常与旧设备原生验收

此矩阵是显式 Windows 原生验收，不在宿主单测中冒充成功。入口复用 S074 的 TLS/SSH/交互会话驱动；四个隔离 DeviceId、SQLite、NTFS 根与缓存顺序启动真实 agent，首次下载全部种子并逐字节核对。只有一台 Windows 实机。

```sh
# 所有命令在本卡隔离树，先创建本 run 独占卷；只给本卷开启备份所需挂载选项。
nix develop --command bash tools/test-volume/test-volume.sh create s102-1107 1G
nix develop --command sudo -n mount -o remount,user_subvol_rm_allowed /data/vaultsyncd-lab/runs/s102-1107/mnt
nix develop --command bash -c 'sudo -n chown "$(id -u):$(id -g)" /data/vaultsyncd-lab/runs/s102-1107/mnt'
nix develop --command cargo build --locked -p vaultsyncd-server-tests --example old-device-native-server -p vaultsyncd-admin --bin vaultsyncd-admin
VAULTSYNCD_E2E_TASK=S102 \
VAULTSYNCD_S102_VOLUME=/data/vaultsyncd-lab/runs/s102-1107/mnt \
VAULTSYNCD_ADMIN_BIN="$PWD/target/debug/vaultsyncd-admin" \
VAULTSYNCD_RELEASE_SERVER="$PWD/target/debug/examples/old-device-native-server" \
VAULTSYNCD_WINDOWS_CACHE=/wrd/prj/vaultsyncd/.cache/windows \
nix develop .#windows --command bash tests/platform/windows/run-native-e2e.sh
```

## 判据（全部满足才退出 0）

| 场景 | 故障与必要证据 |
| --- | --- |
| T14-loss/duplicate/reorder | 分别武装仅改真实上游 SSE chunk 的丢失/重复/乱序规则，必须有产品 SSE 请求且规则真正触发，报告保存上/下游帧；宿主两次真实外部写入经持久 changes 最终落到客户端 NTFS、received/applied相等且推进。仅轮询成功不算此项完成。 |
| T15-cursor-expired | 保留旧 baseline、离线 dirty，服务端提交删除；停服持 StateLock 将本 fixture 的事件时间设为 1 后重启，产品默认 90 天不变；真实 changes 410、无 clean 复活、dirty 字节保留、重建日志及旧 baseline 保留。 |
| T15-history-expired | 独立旧客户端；停服持锁将历史 superseded_at/删除时间及到期时间设为 1/2，真实 GC 两轮跨测试时钟宽限（mark/复核/删除）回收过期对象；真实固定版本下载410证明过期，不能错误要求过期元数据从清单隐藏。重建分类 clean=RetiredClean、dirty=RecoveryCandidate；同上内容判据。不是改在线 DB 或关闭 GC。 |
| T16-old-backup | 初始真实 prepare/verify-source/export/verify/release；客户端真实删除请求在上游收到前断网，停 agent 核对持久 pending；实际 CLI 离线恢复旧归档到全新空 btrfs 目标、重新登录；新 epoch、恢复文件不被删、dirty 保留、旧 Delete 不自动发送且已隔离、持久 rollback 重建日志。服务器拒绝重放不等于客户端没有盲重放。 |

`report.json` 为逐场景 checks/evidence；任一缺证或失败退出 1，不把 fail 改为 skip。不手改客户端 DB；只在 agent 已停止时以 SQLite read_only 查询计数与重建日志。控制器 `old-device-native-server` 只在本 run 回环测试桥提供固定动作，不进入产品服务路由；恢复、故障时间、目标限定本 run。

成功和已完成矩阵的失败都会停止自有 agent、注销自有 CFAPI 根、清理测试凭据；驱动仍按既有规则在退出 1 时保留 driver 日志，归档后只删除报告指明的本 run 目录。若基础设施错误中断，应先核对已启动的自有进程/根再收尾；不得误删其它卡现场。宿主 helper 优雅停止并删除本 run Lab；最后使用 test-volume cleanup 清卷。交接见 `agent/tasks/S102.md`。
