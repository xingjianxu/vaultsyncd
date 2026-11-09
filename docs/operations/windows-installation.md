# Windows每用户安装与会话启动

适用Windows 11 x64。当前安装包是**未签名测试产物**；没有生产签名身份，不应把清单当作数字签名。S100产品版本0.1.1用于从S099 0.1.0开发包真实升级，不宣称存在旧正式发行版；远端协议及数据库schema未随产品版本递增。本机维护命令独立使用IPC1.5。

免安装、状态留在本机的替代入口见[Windows ZIP](windows-zip.md)。同一次`windows-client`构建同时生成两种包，但两者不能接管彼此已用状态；ZIP所有权存在时setup明确拒绝，不允许覆盖绕过S100。

## 安装与运行条件

- 安装器以当前用户普通权限运行，无需安装Windows服务或完整开发工具链。默认程序目录为`%LOCALAPPDATA%/Programs/vaultsyncd`，也可选择新的普通目录；路径可含中文与空格。已存在的目的目录或`%LOCALAPPDATA%/vaultsyncd/`状态树内目录拒绝写入。
- 必须有Microsoft Edge WebView2 Evergreen Runtime。按微软[分发文档](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution)检查当前用户及32位机器EdgeUpdate客户端`{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}`的pv版本大于0.0.0.0。缺失时安装器退出20并给出[微软下载页](https://developer.microsoft.com/microsoft-edge/webview2/)，安装Runtime后重试。不会把Edge浏览器本身当Runtime，也不隐式下载未锁定二进制。
- 安装包包含GUI、agent、CLI和独立无窗口`vaultsyncd-session.exe`。GUI的Start Menu快捷方式固定AUMID `org.vaultsyncd.desktop`，与系统Toast一致；不会创建指向任意外部程序的通用桥。
- 首装只创建程序文件与当前用户启动注册。HKCU Run在登录后执行固定的session launcher；launcher通过CreateProcess启动独立agent，GUI关闭不结束同步。CLI和GUI使用同一用户的真实agent IPC。未登录Windows用户时不运行该用户的同步进程。

## 状态和诊断目录

`%LOCALAPPDATA%/vaultsyncd/agent/agent.json`保存初始无凭据配置，`state/`保存数据库/内部状态，`agent.log`追加运行诊断。初次随机生成ServerId/DeviceId，默认HTTPS localhost、无绑定；通过桌面配置连接、登录并选择Vault。已有配置先验证，不覆盖坏文件或旧状态；实际连接、绑定、模式仍由数据库恢复，凭据由当前用户Windows Credential Manager持有。

程序目录和状态目录分离。复制或删除程序文件不能替代恢复流程；不要手删Vault、数据库、缓存或CFAPI注册来修复启动。未上传内容先按[恢复导出操作](recovery.md)保全。控制面板卸载默认仅移除程序；Vault、状态、缓存和未确认内容全部保留。程序卸载不等于云端内容已下载，不实现删除已下载用户文件的清理选项。

安装失败保留已有内容和诊断现场。退出码20为缺Runtime，21为平台不满足，22为同版/降版或安装注册不匹配，23为目的目录不满足，24为文件/注册写入失败，25为固定快捷方式注册失败，26为session launcher失败，27为维护拒绝/失败（保留回退与恢复现场）。launcher成功仅表示子进程创建，运行健康以实际IPC和日志核对。

## 锁定宿主构建与来源清单

在已提交且干净的任务树、配置好S009交叉工具链缓存后执行：

```sh
nix develop .#windows --command cargo xtask release build windows-client artifacts/S099/client-new
nix develop --command cargo xtask release assemble artifacts/S099/release-new.json artifacts/S099/client-new
nix develop --command cargo xtask release verify artifacts/S099/release-new.json
```

适配器使用flake.lock锁定的NSIS（本环境实测3.12）、Rust/Node/pnpm等工具。清除旧desktop dist并冻结锁离线准备前端；在新Cargo target真实编译x64 MSVC release、静态CRT、无native-acceptance；核验PE架构并拒绝测试入口标识，再生成NSIS和程序免安装ZIP（额外专用ZIP入口在同一次编译生成）。收据记录完整commit/tree、版本与锁/协议源hash、工具实测版本、实际命令和全部payload文件hash。安装目录内的installed-files.json含同源身份和四个已安装EXE的摘要。

只接受同一HEAD的组件，脏树/来源变化/旧输出/缺文件均拒绝生成有效收据。合并或追加交接提交后旧组件不是新HEAD的发布产物，必须重新构建；实验记录需按实际来源提交解释，不能改写收据让它看起来来自后来提交。开发编译检查包没有发布receipt，不作为最终验收产物。

## S099原生首装与重启演练（历史来源0.1.0）

入口（先与父协调Windows独占窗口）：

```sh
nix develop .#web-tests --command python3 tests/platform/windows/s099-installation.py artifacts/S099/client-new
```

驱动会真实创建新标准用户，保存受限ACL下的原自动登录设置，执行约三次客体重启：新用户登录首装、安装后再次登录验证HKCU Run启动、恢复原Quickemu登录。原登录秘密留在客体私有目录，不进入源码或宿主证据；原LSA DefaultPassword使用机器DPAPI加密并读回校验，测试密码也只写LSA，不写注册表明文。按[微软自动登录说明](https://learn.microsoft.com/en-us/windows/win32/secauthn/protecting-the-automatic-logon-password)，LSA密码优先于注册表，必须保存与恢复两者；恢复后逐值核对并删除原登录备份。测试用户密码也只留在客体受限私有目录，便于后续保护演练。

程序、同步目录均使用中文空格路径；GUI是安装后的正式WebView2，通过[官方支持的CDP调试连接](https://playwright.dev/docs/webview2)进行页面操作，不运行浏览器fake，不编译acceptance钩子。只对这个GUI子进程设置调试环境变量，未在安装产物或系统策略中启用远程调试。验证GUI/CLI相同instance、GUI关闭后的实际上传、真实重启后无人工启动agent也继续上传；真实服务端字节与hash作为结果证据。

驱动的系统变更仅限新测试用户、独立目录、自己的登录触发任务和可恢复的自动登录设置。保留S084脏内容与注册根；测试结束保留新用户已安装程序/Vault/state/cache，恢复原用户登录，保持VM在线。实际执行结果及来源提交见agent/tasks/S099.md与忽略artifacts/S099。

本次已验收源`cf1d174c3bd47b4babc2db053514619b2e733b07`的0.1.0未签名包，实际Runtime156.0.4314.58；新标准用户`vs_s099_4fcc1e`，run `s099-1793986849-3965593`。四个安装EXE来源/hash、AUMID、Run、真实页面绑定与GUI/CLI同实例、关闭后同步、真实重启后同步均通过；三次不同boot时间戳，最后回Quickemu且无agent后台进程。测试用户安装/状态/Vault保留给S100，测试服务和隧道已停止；完整摘要见`agent/tasks/S099.md`。交接追加提交后的新HEAD发布需重新构建，不能重标该实验receipt。


## 升级、回退与卸载

从已安装的S099未签名开发源`cf1d174c3bd47b4babc2db053514619b2e733b07`升级时，使用新版本正式同源安装器。升级目标固定为原程序目录，不允许同版/降版覆盖。关闭GUI和占用程序文件的终端；维护控制器验证已安装/候选的四EXE来源hash，删除本程序Run项以停止新登录启动，暂停所有绑定并导出每份未确认内容，再停止确切的同用户IPC对端。支持IPC1.5的agent会断开并排空回调、导出后正常退出；仅上述明确S099开发源允许在无传输、已暂停且导出成功后停止该已认证进程，不冒称旧正式客户端通用升级。

agent维护入口独占writer锁后，将包括WAL的整个固定状态及原程序写入`%LOCALAPPDATA%\vaultsyncd\agent\maintenance\<事务>/`并刷盘，再打开新数据库迁移器及替换程序。保留原DeviceId、凭据、Vault注册、缓存和待办。迁移/程序替换失败时恢复整套原状态与程序；恢复后不调用新迁移器再次升级旧库，仅恢复已知暂停控制。回退不是down migration。资源不足、忙文件或备份损坏时不报告成功，保留屏障、原内容和候选程序。

若`%LOCALAPPDATA%\vaultsyncd\agent\maintenance.json`仍存在，普通agent与登录launcher拒绝写入，不自行删除该屏障。按安装器所示保留候选路径，在同一Windows用户会话运行：

```powershell
& '候选目录\vaultsyncd-agent.exe' --maintenance recover --program-dir '原程序目录'
& '原程序目录\vaultsyncd-session.exe'
```

恢复只消费该事务已有hash清单；备份校验失败时先修复空间/权限/备份来源，保留现场，不清空数据库或Vault。S099原程序本身没有新维护命令，必须使用保留的新候选。密码不重置，Windows Credential Manager不删除。

卸载器默认选择“仅移除程序”：暂停、导出、停机后移除固定程序文件、Run与快捷方式，保留全部Vault/state/cache/dirty。完整文件仍有完整字节；仅云端和部分占位仍是占位，提供程序不再运行后不能假定可离线打开，需重装同一用户客户端或事先下载完整。默认不注销Vault，不调用会删除不完整占位的系统API。

显式选择“同时注销提供程序”（静默自动化对应`uninstall.exe /S /PROVIDER`）要求每个Vault扫描完整且每个文件已完整下载，任何仅云端/partial/未知项都拒绝，并保留程序与恢复现场。用户先在GUI切完整模式或下载全部条目，再重试。控制器只在实际完整驻留且连续句柄身份稳定时，逐文件比较SHA256、调用CfRevertPlaceholder、再次比较hash；确认没有任何占位后才注销CFAPI/Explorer并解除本地管理。完整和dirty文件、缓存与远端数据都保留。

依据微软[CfUnregisterSyncRoot说明](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfunregistersyncroot)，系统注销会删除不完整占位，不能直接用于默认卸载；[CfRevertPlaceholder](https://learn.microsoft.com/en-us/windows/win32/api/cfapi/nf-cfapi-cfrevertplaceholder)会尝试回源不完整文件，因此维护前置必须依据真实驻留检查，不能拿逻辑大小或零填充当完整内容。S100原生矩阵已按下述分段验收。

S100原生入口（需先协调Windows独占窗口）：`nix develop .#web-tests --command python3 tests/platform/windows/s100-maintenance.py <同HEAD正式windows-client组件>`。复用保留用户，不重置密码；先保存原LSA/注册表，再两次受控重启进入该用户、最后恢复Quickemu在线。仅有用例代理对上传begin制造真实断链，以便保留未确认dirty，其他请求均转发真实服务；无伪造成功或在线DB编辑。静默卸载自动化按[NSIS说明](https://nsis.sourceforge.io/When_I_use_ExecWait_uninstaller.exe_it_doesn%27t_wait_for_the_uninstaller%3F)先复制字节相同卸载器到独立目录，再以末尾不加引号的`_?=<原程序目录>`运行，取得实际退出码并允许删除原卸载器；正常控制面板入口仍使用NSIS标准临时副本。

若安装器后续注册/程序删除阶段失败，而准备阶段已移除maintenance.json，完整状态/程序备份及不可变plan.json仍保留。在同一用户确认agent已经停止后，选择**该次失败事务**（核对plan.action、program及备份清单）的plan.json，以不覆盖现有文件的方式复制为agent/maintenance.json，再运行该次保留候选的recover命令。不要编辑plan内容、混搭事务或直接改SQLite；恢复器会持writer锁、验证全部备份hash后恢复。程序清理只操作固定文件，未知文件保留。complete.json表示agent维护阶段结束，整个安装/卸载仍以安装器退出码和实际文件/注册核对为准。

## S100实际分段验收

产品PE/NSIS来源`17947522f73d06b934e704457cb6c01af659dd05`，0.1.1未签名测试包；旧客户端为实际S099开发源cf1d174的0.1.0，不冒充旧正式版。主run `s100-1793990401-4038267`实际通过enroll、dirty、busy程序升级失败自动整套回退、正常升级、原生默认No卸载提示、仅程序卸载、重装、provider不完整拒绝八段。该命令后续自动full任务因故意保留dirty而等待，整体exit1记录保留；没有把它改成成功。

已验证完整缓存`C:\Users\vs_s099_4fcc1e\AppData\Local\vaultsyncd\cache-s100-1793990401-4038267\download-98b7c0e401f406ca330a2779eca650ab`，1,048,576字节，SHA256 `11030261d987f0966338a7afb2fb76b1503b1683d72ffc4ffacd111bc298722f`。人工文件恢复后是实际完整普通文件，不宣称网络下载。首个续验run `s100-cont-1793991690-4089398`在写入/完整hash检查后因装置固定hash列表漏cloud而报错，该失败记录亦保留；最终续验`s100-cont-1793992025-4102900`动态核对全部六份文件、四个真实完整占位经revert后注销、普通文件可读/hash、原dirty仍未确认、state/cache保留，以及708份恢复文件宿主独立读取/hash全部通过。普通文件无CFAPI OnDisk字段，原生观察元组中的0不是“零字节内容”，完整性另以实际读取长度及hash证明。

续验入口`tests/platform/windows/s100-continuation.py <主run目录>`消费明确的历史1794752组件，验证其Git树和每份payload及实际已安装清单，不把历史receipt重标为当前HEAD发行；续验脚本实际运行副本/hash保存在各run。标准用户、Vault/state/cache/备份与WinCred保留，原LSA/注册表每轮恢复后删除原秘密备份，最后回Quickemu、无agent、VM在线。大证据在忽略artifacts/S100，精确范围见final-matrix.json及S100任务交接。

## S101 同提交安装与可选CI清理

S101实际源`fcbbb93ecff408fedc8fbba6006c81b84a151727`的0.1.1未签名NSIS为7,829,067字节；Windows11实际build26300.9711、NTFS、WebView2 157.0.4341.42。新标准用户真实安装、GUI/CLI同实例、关GUI仍同步、HKCU Run真实重启后同步、三份内容hash通过，原Quickemu登录恢复。安装器与四PE完整SHA-256在`agent/tasks/S101-b.md`和`artifacts/S101/public/windows-client-receipt.json`，不重标交接后的新HEAD产物。

CI显式设置`VAULTSYNCD_CI_CLEANUP=1`时，S099驱动仅对本run新账号执行logout、完整provider卸载（复用S100实际完整驻留/revert门）、普通文件hash核对，再删除新用户/profile/私有凭据/根/探针及准确新SID的系统任务。Windows为新用户生成的SoftLanding任务也须核对SID后清理，不用任务名通配符影响其他用户。默认未设该变量仍按历史S099保留现场。S101新SID清理独立probe通过；原S099/S100账号、数据和WinCred未触碰，VM在线且agent0。

`tools/ci/README.md`是矩阵操作入口。Windows hosted原生编译仍因无remote/runner blocked；本机证据为锁定Linux cross构建后的Windows客体原生运行，不能改称Windows本机编译。
