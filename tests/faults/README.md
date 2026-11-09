# S102 事件异常与旧设备原生验收

此矩阵是显式 Windows 原生验收，不在宿主单测中冒充成功。入口复用 S074 的 TLS/SSH/交互会话驱动；四个隔离 DeviceId、SQLite、NTFS 根与缓存顺序启动真实 agent，首次下载全部种子并逐字节核对。只有一台 Windows 实机。

```sh
# 所有命令在本卡隔离树，先创建本 run 独占卷；只给本卷开启备份所需挂载选项。
nix develop --command bash tools/test-volume/test-volume.sh create s102-1107 1G
nix develop --command sudo -n mount -o remount,user_subvol_rm_allowed /data/vaultsyncd-dev/vaultsyncd-temp/volumes/runs/s102-1107/mnt
nix develop --command bash -c 'sudo -n chown "$(id -u):$(id -g)" /data/vaultsyncd-dev/vaultsyncd-temp/volumes/runs/s102-1107/mnt'
nix develop --command cargo build --locked -p vaultsyncd-server-tests --example old-device-native-server -p vaultsyncd-admin --bin vaultsyncd-admin
VAULTSYNCD_E2E_TASK=S102 \
VAULTSYNCD_S102_VOLUME=/data/vaultsyncd-dev/vaultsyncd-temp/volumes/runs/s102-1107/mnt \
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

## S104：GC、备份与真实进程崩溃

```sh
nix develop --command bash tests/faults/run-maintenance-crash.sh
```

入口创建独占 `s104-*` 2GiB btrfs loop，只给本卷开启 `user_subvol_rm_allowed`，普通服务进程无 sudo/mount 权限。默认退出时核对卷归属、卸载、detach、清理；原始 trace/日志保存在 ignored 的 `artifacts/S104/<run_id>/`，任何测试或清理失败均非零退出。`VAULTSYNCD_RUN_ID` 可指定新的 `s104-*` 名，`VAULTSYNCD_S104_REPORT` 可指定证据目录。

`test-faults` 为默认关闭的编译 feature；只有显式开启的 `s104-child` 可配置本 run 的私有文件控制桥。产品 HTTP/IPC 无故障路由，普通构建停车函数为空且无控制配置入口。此桥只在已完成的真实持久化动作之间刷盘停车，所有 SQLite/文件发布、租约、GC、归档和恢复仍执行产品实现。

| 场景 | 必须满足的判据 |
| --- | --- |
| T23/T24 十个 put 边界 | pending、prepared前后、publish前后、published前后、metadata前后、ACK前分别SIGKILL；真实固定版本下载已有持久租约、备份lease有效，显式GC已竞争同一对象门。无意图时仍pending/202且旧字节；已有意图恢复新版本并幂等重放，恰好一个新版本，零提前ACK。 |
| T22 固定版本下载 | 第二块请求实际等待时SIGKILL服务，TCP断开，中断返回后已完成的第一块进度落盘；重开磁盘候选/位图后只取缺失四个Range，已确认前缀重传0。篡改真实响应块、替换为另一真实版本响应均不产出可安装结果，原本地内容保留。测试传输仅将逻辑HTTPS地址映射到隔离回环HTTP，不代表TLS验收。 |
| 备份准备/释放 | preparing/lease落盘/只读snapshot/ready四点和releasing/删快照后两点SIGKILL；重启得到failed且清理或可解释的ready/released；同ID重试不生成伪完整备份/新副本，无租约泄漏。 |
| GC | mark/deleting/unlink三点SIGKILL，启动后台GC或显式GC补完同一对象；再跑零删除，所有当前引用对象完整。 |
| T24/T32 extent与恢复 | 真实不可压缩16MiB历史对象：btrfs du报告源/快照共享extent；有效备份lease禁止回收。测试GC时钟跨租约到期后允许删源，但只读快照仍保留完整extent。实际prepare/verify-source/export/verify/apply到空实例，逐个对象hash、条目/历史/回收站数量、当前文件和历史下载一致。 |
| T33 | 复用S091真实actor：合成未来迁移部分成功再失败，当前产品启动拒写，完整归档恢复到新空实例，历史/回收站/ServerId保留、新epoch；合成迁移不冒充已发布旧版。 |

固定交错时先把周期GC停在独占门之外，再让下载与put成为活跃读者，最后仅显式GC竞争同一真实对象门，避免测试无限停车与RwLock写者优先造成装置自锁。没有禁用GC/锁或降低引用校验。时间老化仅对停服持StateLock的fixture处理；测试GC时钟不改产品30天保留、1小时宽限、24小时备份lease默认值。此矩阵不声明Windows CFAPI或容量验收完成。
