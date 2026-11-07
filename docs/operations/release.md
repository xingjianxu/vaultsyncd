# 首发发布核对与证据索引

状态：**blocked，未完成首发验收**。2026-11-07。本文汇总独立证据，不将历史交接的done叠加为当前候选35项全部通过。S101 hosted、S106/S107性能容量与T02三台Windows实证缺口未解除；T29明确P1，不算首发门禁。

## 源码与证据归属

| 证据组 | 实际源与界限 |
| --- | --- |
| S074基础互通 | `88fb7be`，Windows单客体三身份顺序；S102在 `81699c411c6d976188a63927f55150bbc34c574b` 干净树复跑 |
| S101历史发行/安装 | `fcbbb93ecff408fedc8fbba6006c81b84a151727`，0.1.1 unsigned-test；本机同源，hosted未执行 |
| S103文件/挂载故障 | 产品修复 `8e4ad2b`、装置 `42ab10d`，合并 `b36e31e`；详细源见交接，不能标成当前候选重跑 |
| S104维护/恢复 | `c03114d3aa3757bbd116b8d2137c1d080935028e`，合并 `326b469`；默认关闭test-faults，maintenance5/0、T33额外1/0 |
| S105修复与原生安全 | 产品源 `b26372ec8b64e7c88b6d2aa34a5258413fe15f9c`；IPC1.6/排除重协调、NoSpace与CFAPI范围key修复，debug PE无native-acceptance；交接 `8cd3a8a`，父整合候选 `fd896faee0f407b2cf3da5ed332a88f83d184a3f`，S105 done |
| S106/S107 | `390cda0`/`dcadee5`纯文档准备审计，分别以 `cb9ed20`/`0d4fb30`整合；两卡blocked、基准未测，见[性能报告](../reports/performance.md) |

当前整合源码候选为 `fd896faee0f407b2cf3da5ed332a88f83d184a3f`，包含本批代码；其后生命周期/清单提交仅维护文档。该整合提交通过父协调者的metadata、fmt和真实HTTP安全2/2检查，不等于同源全平台发行/35P0重跑。S105交接与[安全报告](../reports/security.md)记录各原生运行与二进制来源。

每行的第一交接给出实现来源/提交，第二交接给出后续验证与平台界限。旧功能证据不能自动证明最终整合候选；发布前须选同一源码提交重建产物并取得所需平台门禁。原始大日志在主树忽略目录 `artifacts/Sxxx` 或 `artifacts/batch-S076-S100/Sxxx`，保留失败轨迹。跟踪文档可查看，忽略的本地证据需随发布审查单独归档。

## 35项P0证据目录

| ID | 场景 | 实现/历史交接 | 后续交接 | 真实证据与剩余边界 |
| --- | --- | --- | --- | --- |
| T01 | 非空初次绑定 | [S074](../../agent/tasks/S074.md) | [S102](../../agent/tasks/S102.md) | 真实Windows非空导入、21份输入全覆盖；单客体顺序身份 |
| T02 | 三端离线冲突 | [S074](../../agent/tasks/S074.md) | [S102](../../agent/tasks/S102.md) | 仅三隔离身份；**三台Windows实证缺失** |
| T03 | 丢ACK幂等 | [S074](../../agent/tasks/S074.md) | [S104](../../agent/tasks/S104.md) | 真实丢响应/同OpId重放与进程强杀，无重复版本 |
| T04 | 上传中再次保存 | [S056](../../agent/tasks/S056.md) | [S074](../../agent/tasks/S074.md) | 代际/ACK核对及集成；发布同候选仍需复核 |
| T05 | 已下载离线修改 | [S074](../../agent/tasks/S074.md) | [S102](../../agent/tasks/S102.md) | 旧基线冲突与旧设备dirty保留 |
| T06 | 离线读占位 | [S067](../../agent/tasks/S067.md) | [S105](../../agent/tasks/S105.md) | Windows原生离线/坏块拒绝；S105最终原生矩阵通过，见S105交接 |
| T07 | 拒绝释放未上传 | [S069](../../agent/tasks/S069.md) | [S105](../../agent/tasks/S105.md) | 原生dirty/busy/确认后变化、完整内容保留 |
| T08 | 空间不足不驱逐 | [S069](../../agent/tasks/S069.md) | [S105](../../agent/tasks/S105.md) | SQLite pager及独立NTFS VHD full均有原生通过证据，见S105交接 |
| T09 | 占位重命名 | [S074](../../agent/tasks/S074.md) | [S101](../../agent/tasks/S101.md) | 原生身份不变/on_disk0，无内容下载 |
| T10 | 移动与修改 | [S022](../../agent/tasks/S022.md) | [S074](../../agent/tasks/S074.md) | 规划/服务端与基础互通证据；多实机并发未独立补测 |
| T11 | 删除与修改 | [S074](../../agent/tasks/S074.md) | [S102](../../agent/tasks/S102.md) | 旧Entry退役、dirty新Entry与回收站内容保留 |
| T12 | 目录删与新增 | [S039](../../agent/tasks/S039.md) | [S052](../../agent/tasks/S052.md) | 真实服务端子树token并发拒绝；不冒充三机运行 |
| T13 | 恢复路径占用 | [S052](../../agent/tasks/S052.md) | [S079](../../agent/tasks/S079.md) | 服务端另名恢复及Web真实入口，来源见交接 |
| T14 | SSE异常 | [S102](../../agent/tasks/S102.md) | [S102](../../agent/tasks/S102.md) | 真实loss/duplicate/reorder与持久changes收敛 |
| T15 | 旧设备与保留过期 | [S102](../../agent/tasks/S102.md) | [S102](../../agent/tasks/S102.md) | 真实410/GC过期、断网重建保留旧基线与dirty |
| T16 | 恢复旧服务端 | [S102](../../agent/tasks/S102.md) | [S104](../../agent/tasks/S104.md) | 真实备份恢复新epoch、旧pending delete隔离 |
| T17 | 外部文件操作 | [S103](../../agent/tasks/S103.md) | [S103](../../agent/tasks/S103.md) | 真实工作目录采集、rename/截断、历史可恢复 |
| T18 | 持续写竞争 | [S103](../../agent/tasks/S103.md) | [S103](../../agent/tasks/S103.md) | 409 RECONCILE_REQUIRED，未采集外部字节不被覆盖 |
| T19 | 卸载露空目录 | [S103](../../agent/tasks/S103.md) | [S103](../../agent/tasks/S103.md) | 真实umount-l、ROOT_UNAVAILABLE、零删除/版本 |
| T20 | 队列溢出与重启 | [S103](../../agent/tasks/S103.md) | [S103](../../agent/tasks/S103.md) | 内核inotify overflow补扫、权限失败非删除 |
| T21 | 容量与CoW失败 | [S103](../../agent/tasks/S103.md) | [S103](../../agent/tasks/S103.md) | 独立btrfs双run的数据/元数据ENOSPC与reflink失败 |
| T22 | 下载断线错块混版本 | [S104](../../agent/tasks/S104.md) | [S105](../../agent/tasks/S105.md) | 真实TCP补4 Range、前缀0重传；CFAPI证据分别列账 |
| T23 | 逐持久点强杀 | [S104](../../agent/tasks/S104.md) | [S104](../../agent/tasks/S104.md) | 十put边界、真实SQLite/btrfs/HTTP，无提前ACK |
| T24 | GC/提交/备份 | [S104](../../agent/tasks/S104.md) | [S104](../../agent/tasks/S104.md) | 六快照/三GC边界，活跃引用与共享extent保持 |
| T25 | 路径越界与别名 | [S065](../../agent/tasks/S065.md) | [S103](../../agent/tasks/S103.md) | Windows junction/symlink、Linux别名/挂载拒绝 |
| T26 | 名称碰撞 | [S065](../../agent/tasks/S065.md) | [S103](../../agent/tasks/S103.md) | 真实Windows大小写/Unicode/保留名/长路径拒绝 |
| T27 | rename-over保存 | [S068](../../agent/tasks/S068.md) | [S103](../../agent/tasks/S103.md) | 后续原生保存矩阵；不据S012早期失败判当前全失败 |
| T28 | 应用写与保护 | [S068](../../agent/tasks/S068.md) | [S103](../../agent/tasks/S103.md) | 真实映射写/共享锁/原子保存/受控替换矩阵 |
| T30 | 模式排除解绑 | [S100](../../agent/tasks/S100.md) | [S105](../../agent/tasks/S105.md) | 真实GUI/CLI与规则运行时修复，最终原生矩阵通过，见S105交接 |
| T31 | 鉴权与WebView | [S101](../../agent/tasks/S101.md) | [S105](../../agent/tasks/S105.md) | 跨owner资源、撤销/CSRF/恶意WebView/IPC原生拒绝 |
| T32 | 完整空环境恢复 | [S089](../../agent/tasks/S089.md) | [S104](../../agent/tasks/S104.md) | 4对象逐hash、条目/历史/回收站、dangling0 |
| T33 | 升级失败回退 | [S091](../../agent/tasks/S091.md) | [S104](../../agent/tasks/S104.md) | 真实actor未来schema拒写与完整归档回退，合成版本明确 |
| T34 | 时延与规模 | [S106](../../agent/tasks/S106.md) | [S107](../../agent/tasks/S107.md) | **blocked，全部性能/正式容量未测** |
| T35 | 不合规后端拒绝 | [S013](../../agent/tasks/S013.md) | [S103](../../agent/tasks/S103.md) | 真实卸载/替换/子挂载/bind别名/NOCOW与准入 |
| T36 | 发布平台与安装 | [S098](../../agent/tasks/S098.md) | [S101](../../agent/tasks/S101.md) | 历史同源三平台本机安装；hosted/当前候选未完成 |

**T29为P1**：Linux客户端/FUSE导入不在首发交付范围，不用移除P0测试来扩大通过比例。除T29外恰有35行；“已有证据”不表示该行已对当前候选通过最终门禁。

## 运维入口

| 任务 | 执行依据 |
| --- | --- |
| NixOS部署/TLS/健康/Root身份恢复 | [nixos.md](nixos.md)；先检查实际主机入口，保留恢复generation与SSH |
| 通用Linux/systemd/容器 | [linux.md](linux.md)；合规btrfs、普通文件、不要求FUSE |
| Windows安装/升级/默认保留数据卸载 | [windows-installation.md](windows-installation.md)；完整provider注销前先验证全部内容驻留 |
| 一致性备份与空实例恢复 | [backup.md](backup.md)；prepare→verify→export→verify→release，离线恢复至新空目录 |
| 异常缓存/未上传内容 | [recovery.md](recovery.md)；保留源现场并核对恢复包 |
| agent/CLI与原生故障定位 | [windows-agent.md](windows-agent.md)、[windows-lab.md](windows-lab.md)；不直接修改在线SQLite |

## 产物与哈希

本轮不制作发布包。S105在精确源码 `b26372ec8b64e7c88b6d2aa34a5258413fe15f9c` 重建了Windows debug PE（IPC1.6、未启用native-acceptance、未签名、无NSIS）；本卡实际读取三份字节并核对provenance的大小/hash：

| PE | 字节 | 本卡实际SHA-256 |
| --- | --- | --- |
| `vaultsyncd-agent.exe` | 24083456 | `2901c0ced058bfd3d32df5f7ef4105f7c2a2753d7d8bd2cc3f95c4b74c72dc37` |
| `vaultsyncd-cli.exe` | 2147328 | `1367c77b7ccbd238b3e563d2fd75a8705727514960936d1186c562ad11a17c6f` |
| `vaultsyncd-desktop.exe` | 13698048 | `3056b72bd3344a22f0b3ce6b77d7e6ade551251c385a1175fbe00c2e85d9a2fb` |

对应 `artifacts/S105/production/` 与 `provenance.json`。原生验收日志记录的是各自测试二进制的来源/哈希，不能将后来重建PE称为已通过全套安装器或hosted验收。

`artifacts/S108/artifact-index.json`保留S101安全公开清单文件的实际SHA-256及历史清单所声明的payload哈希，二者严格区分；旧包源固定为 `fcbbb93...`，不能用于声称当前修复已发布。0.1.1历史Windows安装器声明SHA-256为 `805a22c52ef89b9d135a775521a52c10f533579ad85462954fc98817f188aa94`；本卡不声称重新读取了该安装器字节。本卡重新读取现存历史Nix store输出：两架构各24份runtime文件hash均匹配公开manifest，两份源码归档均匹配 `23c6906f6d914f62eaf442bd9327904e7d61647b55ed5d012179380c3de87dce`。实际发布归档如下（均为历史源，非新候选）：

| 归档 | 字节 | 本卡实际SHA-256 |
| --- | --- | --- |
| `vaultsyncd-0.1.1-x86_64-unknown-linux-gnu.tar.gz` | 11591687 | `63cdc2fec2eb0e88150eb19f35f74ef37248467b9cd76cc1e4238f911b556599` |
| `vaultsyncd-0.1.1-aarch64-unknown-linux-gnu.tar.gz` | 10867397 | `a60eb73e2178e95d370b08758df01bd400c485ddc279b94e3a811981f7093a89` |

完整24项文件hash、源码归档、组件来源与Nix store路径见各架构公开release-manifest和本卡artifact-index。

## 恢复发布验收

1. 补真实远端、hosted工作流与runner证据，完成S101；保留本地/hosted平台区别。
2. 选包含S102—S105修复的最终源码提交，记录dirty状态、锁文件和构建工具链；为Linux x64、Web、Windows包生成新的来源及payload SHA-256，不重标历史receipt。
3. 完成S106三设备/SSD/RTT条件下的小样本及常规档，再按S107完整容量预算准备五台独立Windows设备和NTFS数据卷，执行真实10万文件/1TiB/50GiB传输并记录重传字节与资源曲线。
4. 在同候选下复核本表35项证据缺口（含T02真实三台），安装/升级/备份恢复和安全入口；记录所有失败与恢复，不将ignored原生测试当已实跑。
5. 无blocked P0后才能标记首发验收完成。签名、证书/公网部署与未测平台如实披露；本任务不授权推送远端或公网发布。

S108卡引用“附录B”，当前 `docs/design.md` 只有附录A；`context.py design 附录B`退出2。此处按§21、§25与实际交接核对，不替用户补造规格。
