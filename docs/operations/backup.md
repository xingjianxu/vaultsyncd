# 一致备份准备

当前实现入口为在线服务库 `backup::prepare`；管理CLI与HTTP导出入口由S088接入。`prepared/ready` 表示固定准备视图可供导出，不等于已备份到独立故障域或已完成恢复验证。

备份基目录固定在状态目录下的 `backups/`，仅服务账号可访问（0700），数据库/配置/清单0600。该目录所在挂载必须为CoW btrfs，不能启用NoCoW/NoDataSum，并由管理员预先配置 `user_subvol_rm_allowed`。可把专用备份文件系统挂在此位置；服务代码没有sudo、挂载、格式化或任意子卷路径接口。普通用户删除只读快照会遇到EROFS，调用清理前必须先持久进入failed/releasing，再仅对对应ID的snapshot清ro并删除。NixOS模块接入时必须保留这一权限边界，不能给主服务宽泛CAP_SYS_ADMIN。

准备时共用对象独占门，等待本服务正在进行的发布/恢复/下载/采集结束，并补完可恢复意图；未解决的pending操作、意图或GC删除阶段会阻止准备。Root注册也参加该门。SQLite源读取事务固定各Root seq、映射、逻辑表计数与对象清单，Online Backup API包含WAL。导出副本移除会话、续传及临时租约，源数据库不受此清理影响。对象reflink/校验复制至没有嵌套子卷的准备子卷，刷盘并检查后创建只读快照。

布局为 `backups/<32位ID>/snapshot/{database.sqlite,config.json,manifest.json,objects/<RootId>/<hash>}`。清单格式为 `api/backup-manifest.schema.json`；数据库、配置和每个对象均有长度与SHA-256，记录ServerId、schema、协议契约版本/摘要、产品版本及Root原映射。副本包含数据库中仍保留的历史和回收站；不包含尚未采集的外部工作目录写入。外部进程没有被暂停，不得把此视图描述为工作目录此刻全部字节。

24小时backup lease先于恢复GC建立。现阶段租约保守保护所列Root的全部对象，可能延后新对象回收；只读快照对象为独立inode的reflink/复制，不依赖共享对象文件名。失效/失败或进程中断不会变成complete；启动恢复只处理持久preparing/failed且未清理的对应ID，失败保留原因与清理重试条件。ready视图的导出、校验、释放与过期管理归S088。服务不内置定时备份；同盘只读快照仍需导出到独立故障域并验证恢复。

S087真实验收使用独立run_id `s087-backup-20261106` 的1GiB loop btrfs卷，仅该卷启用上述挂载选项。普通用户执行快照/清理；持续外部写、租约防GC、缺对象失败、嵌套拒绝和SIGKILL恢复均已通过，测试卷已卸载、loop已detach、run目录已清理。证据摘要见 `agent/tasks/S087.md`。
