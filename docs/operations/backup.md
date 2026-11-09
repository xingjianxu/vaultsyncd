# 一致备份准备

在线准备、源校验、导出与释放均由运行中的服务处理，CLI通过同UID管理socket调用；Web/远端接口使用唯一active owner会话。`prepared/ready` 表示固定准备视图可供导出，不等于已备份到独立故障域或已完成恢复验证。

备份基目录固定在状态目录下的 `backups/`，仅服务账号可访问（0700），数据库/配置/清单0600。该目录所在挂载必须为CoW btrfs，不能启用NoCoW/NoDataSum，并由管理员预先配置 `user_subvol_rm_allowed`。可把专用备份文件系统挂在此位置；服务代码没有sudo、挂载、格式化或任意子卷路径接口。普通用户删除只读快照会遇到EROFS，调用清理前必须先持久进入failed/releasing，再仅对对应ID的snapshot清ro并删除。NixOS模块接入时必须保留这一权限边界，不能给主服务宽泛CAP_SYS_ADMIN。

准备时共用对象独占门，等待本服务正在进行的发布/恢复/下载/采集结束，并补完可恢复意图；未解决的pending操作、意图或GC删除阶段会阻止准备。Vault注册也参加该门。SQLite源读取事务固定各Vault seq、映射、逻辑表计数与对象清单，Online Backup API包含WAL。导出副本移除会话、续传及临时租约，源数据库不受此清理影响。对象reflink/校验复制至没有嵌套子卷的准备子卷，刷盘并检查后创建只读快照。

布局为 `backups/<32位ID>/snapshot/{database.sqlite,config.json,manifest.json,objects/<VaultId>/<hash>}`（归档成员始终按 VaultId+摘要，与源对象是新 `roots/<VaultId>/objects/...` 还是历史 `objects/...` 路径无关；清单的 `storage_path` 保留源登记原值）。清单格式为 `api/backup-manifest.schema.json`；数据库、配置和每个对象均有长度与SHA-256，记录ServerId、schema、协议契约版本/摘要、产品版本及Vault原映射。副本包含数据库中仍保留的历史和回收站；不包含尚未采集的外部工作目录写入。外部进程没有被暂停，不得把此视图描述为工作目录此刻全部字节。

24小时backup lease先于恢复GC建立。现阶段租约保守保护所列Vault的全部对象，可能延后新对象回收；只读快照对象为独立inode的reflink/复制，不依赖共享对象文件名。失效/失败或进程中断不会变成complete；启动恢复只处理持久preparing/failed且未清理的对应ID，失败保留原因与清理重试条件。ready视图由S088持久作业导出、校验、释放；启动及运行中会重放已接受作业并释放过期视图。服务不内置定时备份；同盘只读快照仍需导出到独立故障域并验证恢复。

S087真实验收使用独立run_id `s087-backup-20261106` 的1GiB loop btrfs卷，仅该卷启用上述挂载选项。普通用户执行快照/清理；持续外部写、租约防GC、缺对象失败、嵌套拒绝和SIGKILL恢复均已通过，测试卷已卸载、loop已detach、run目录已清理。证据摘要见 `agent/tasks/S087.md`。


## 分步导出与校验

`vaultsyncd-admin`使用如下步骤（将socket和输出改为实际绝对路径，ID为32位小写hex）：

```sh
vaultsyncd-admin backup prepare --id 0123456789abcdef0123456789abcdef --socket /var/lib/vaultsyncd/admin/socket
vaultsyncd-admin backup status 0123456789abcdef0123456789abcdef --socket /var/lib/vaultsyncd/admin/socket
vaultsyncd-admin backup verify-source 0123456789abcdef0123456789abcdef --socket /var/lib/vaultsyncd/admin/socket
vaultsyncd-admin backup export 0123456789abcdef0123456789abcdef --output /safe-backup/example.vsbk --socket /var/lib/vaultsyncd/admin/socket
vaultsyncd-admin backup verify /safe-backup/example.vsbk
vaultsyncd-admin backup release 0123456789abcdef0123456789abcdef --socket /var/lib/vaultsyncd/admin/socket
```

prepare可省略`--id`以生成随机ID；若可能中断，预先选择稳定ID便于status查询。重复prepare复用既有成功，同ID失败保留证据，新的准备使用新ID。prepare/verify-source/release先持久入队，CLI轮询至终态；取消或连接超时后用status查询，不假定工作已取消。普通管理请求每阶段10秒截止，归档流每次写30秒空闲截止，不用10秒总时长中止长导出。HTTP入口`/v1/admin/backup-snapshots`及各ID子资源的契约在OpenAPI；新增作业返回202，目录固定100条分页。

归档格式与收据唯一源为`api/backup-export.schema.json`。CLI以64KiB块流式写入新建0600文件，拒绝覆盖已有输出；收据`<归档>.verification.json`同为0600，先持久incomplete，全部接收校验并fsync后只标exported_unverified。保留归档与收据，显式verify会先撤销旧verified，再重读全部归档字节与终止标记，成功后原子更新verified；取消、错hash、缺项或截断保留incomplete与原因。已有损坏输出不能直接覆盖，改用新的输出路径重新export。

verify-source校验只读源全部对象及SQLite quick_check、外键、ServerId/schema/Vault seq、逻辑表数量；其verified只说明服务端源，不能替代客户端持久导出verify，也不代表完成恢复演练。归档保留配置、原Vault映射、历史、回收站和受限的密码PHC，所有会话已在副本失效；须按认证备份材料保护，不能公开分享。HTTP每次读取都要求会话及唯一active owner，只允许清单内文件/对象，禁止任意路径或全局hash访问。

release等待读者退出，先持久releasing，再仅删除该ID快照，最后释放租约并记released；与原对象区无删除关联。重复release幂等，失败/强杀后重启继续。未知工件保留并报告原因，不强制清理；管理员核对本ID故障证据后再恢复。到期释放仅收尾租约，不创建新备份。把归档与收据放到独立故障域并验证后再主动release。

S088专用run `s088-1106` 的真实CLI/HTTP矩阵覆盖完整逐项导出、当前/历史/回收站、坏hash/缺对象、取消、跨10秒长流和释放SIGKILL恢复；服务始终普通用户运行，所有共享对象hash保持不变，测试卷已卸载且loop已detach。详见`agent/tasks/S088.md`。


## 离线恢复到新空目录

停止目标实例后，以服务同一UID预建0700状态目录和各Vault新空目录。它们必须在同一合规CoW btrfs文件系统，互不重叠；不能使用符号链接/目录身份别名，不能与清单原Vault路径重叠。CLI不会自动停止服务，也不覆盖非空目录。保留归档及其固定摘要收据，即使先前export/verify取消，validate仍会重新验证全部字节，不能把收据状态直接当成功。

```sh
vaultsyncd-admin restore validate --archive /safe-backup/example.vsbk --state-dir /srv/restored/state --vault 0123456789abcdef0123456789abcdef=/srv/restored/work
vaultsyncd-admin restore apply --archive /safe-backup/example.vsbk --state-dir /srv/restored/state --vault 0123456789abcdef0123456789abcdef=/srv/restored/work
```

有多个Vault时重复`--vault ID=绝对路径`，每个清单Vault恰好映射一次；无Vault备份省略该参数。validate检查固定清单/完整流终止标记、所有文件hash、SQLite完整性/FK/ServerId/schema/Vault seq/逻辑计数、协议版本、活跃命名空间与真实空间预算。支持备份schema12至当前版本，未知未来schema/协议拒绝；产品patch与契约内容摘要不代替独立版本号。容量预算是复制和元数据余量估算，不是配额预留；写入失败不会报告restored。

apply全过程持服务/init同一server.lock，在线实例明确拒绝。先持久`restore.incomplete`；正式`server.sqlite`保存独立种类的恢复护栏，源DB仅写`restore.candidate.sqlite`，所以不认识新标记的旧服务也会沿用数据库种类门拒绝半成品。对象按安全布局导入，活跃文件重建为独立inode的普通文件，历史/回收站留在DB与对象区，不把已删项写回工作目录。短事务更新新物理映射并分配全新随机63位非零epoch，沿用ServerId、Vault seq及逻辑历史，所有会话失效。随机代际排除旧值，反复从同一旧备份恢复不会确定复用相同epoch；旧epoch操作在读取旧幂等结果前返回410。

原配置字节保存在0600的`backup-config.json`；新的`server.toml`保留listen/log设置，替换状态目录与Vault允许路径，默认仅允许已映射Vault。零Vault时不配置新允许路径，后续注册前由管理员明确配置。核对端口及新目录后可用该配置启动服务。只有全部对象/工作文件hash、DB/映射/epoch和目录刷盘完成，且护栏身份未变，才原子发布候选DB、写`restore.complete.json`并持久移除未完成标记。报告格式唯一源为`api/backup-restore.schema.json`。

失败或取消保留`restore.incomplete`、护栏/候选及已写文件，服务和init在打开数据库前拒绝。不要手动删标记来绕过检查；保留现场与归档，改用另一组新空目标重新apply。已有目录的数据合并归首次合并流程，不用离线恢复强制覆盖。旧客户端按epoch变化进入重同步，保留未确认本地内容，不自动重放旧操作。

S089专用run `s089-1106` 验证实际CLI完整恢复、真实服务启动/历史下载/回收站、旧session及旧epoch隔离、重复恢复、零Vault、坏归档、实际空间不足、非法/在线目标和apply SIGKILL后的双重屏障；22.40秒最终矩阵通过。测试卷已卸载、loop detach及删除，证据见`agent/tasks/S089.md`。


## 升级前检查与完整回退

升级前先完成prepare、导出到独立故障域并verify，保留原状态目录、Vault工作目录和启动配置。迁移自身的SQLite备份用于诊断和保全原记录，不能代替包含对象、配置、Vault映射和历史的完整备份。迁移在启动写服务前完成；失败保留备份位置并停止启动。部分迁移已提交时，不让旧二进制继续写，也不手改schema版本或删除迁移记录绕过拒绝。

回退按上节将已验证的整套归档恢复至新空目标，核对当前文件、历史下载与回收站后再切换服务配置。沿用ServerId并更换epoch，重新登录；旧客户端保留本地未确认内容，按新epoch重同步。客户端升级同样先保全本地DB、工作内容与完整缓存，保留绑定身份、dirty代际和队列；迁移失败不注销Vault或清空缓存。安装及原生回调切换的停止/重启流程由Windows安装验收负责，不能只回退SQLite后让旧回调继续访问新状态。

S091固定server12/client11历史SQL校验和及远端1.0/IPC1.2开发fixture；没有旧正式发行客户端，不将固定迁移上界模拟的旧程序当作前一正式版。真实服务整套回退演练在专用btrfs卷上从当前schema13完整备份，经显式合成未来14成功/15失败、当前服务拒写，再恢复到新空目标并实际下载历史内容；测试命令、Windows原生范围及证据见agent/tasks/S091.md。
