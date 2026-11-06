# Windows每用户安装与会话启动

适用Windows 11 x64。当前安装包是**未签名首装测试产物**；没有生产签名身份，不应把清单当作数字签名。已有安装会被明确拒绝，不在首装时覆盖旧程序或清理数据；受控升级、卸载与保护选项由S100另行验收。

## 安装与运行条件

- 安装器以当前用户普通权限运行，无需安装Windows服务或完整开发工具链。默认程序目录为`%LOCALAPPDATA%/Programs/vaultsyncd`，也可选择新的普通目录；路径可含中文与空格。已存在的目的目录或`%LOCALAPPDATA%/vaultsyncd/`状态树内目录拒绝写入。
- 必须有Microsoft Edge WebView2 Evergreen Runtime。按微软[分发文档](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution)检查当前用户及32位机器EdgeUpdate客户端`{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}`的pv版本大于0.0.0.0。缺失时安装器退出20并给出[微软下载页](https://developer.microsoft.com/microsoft-edge/webview2/)，安装Runtime后重试。不会把Edge浏览器本身当Runtime，也不隐式下载未锁定二进制。
- 安装包包含GUI、agent、CLI和独立无窗口`vaultsyncd-session.exe`。GUI的Start Menu快捷方式固定AUMID `org.vaultsyncd.desktop`，与系统Toast一致；不会创建指向任意外部程序的通用桥。
- 首装只创建程序文件与当前用户启动注册。HKCU Run在登录后执行固定的session launcher；launcher通过CreateProcess启动独立agent，GUI关闭不结束同步。CLI和GUI使用同一用户的真实agent IPC。未登录Windows用户时不运行该用户的同步进程。

## 状态和诊断目录

`%LOCALAPPDATA%/vaultsyncd/agent/agent.json`保存初始无凭据配置，`state/`保存数据库/内部状态，`agent.log`追加运行诊断。初次随机生成ServerId/DeviceId，默认HTTPS localhost、无绑定；通过桌面配置连接、登录并选择Root。已有配置先验证，不覆盖坏文件或旧状态；实际连接、绑定、模式仍由数据库恢复，凭据由当前用户Windows Credential Manager持有。

程序目录和状态目录分离。复制或删除程序文件不能替代恢复流程；不要手删Root、数据库、缓存或CFAPI注册来修复启动。未上传内容先按[恢复导出操作](recovery.md)保全。首装测试包不提供伪造的升级/卸载成功入口，也不实现删除用户数据的选项。

安装失败保留已有内容和诊断现场。退出码20为缺Runtime，21为平台不满足，22为已有安装，23为目的目录不满足，24为文件/注册写入失败，25为固定快捷方式注册失败，26为首次session launcher失败。launcher成功仅表示子进程创建，运行健康以实际IPC和日志核对。

## 锁定宿主构建与来源清单

在已提交且干净的任务树、配置好S009交叉工具链缓存后执行：

```sh
nix develop .#windows --command cargo xtask release build windows-client artifacts/S099/client-new
nix develop --command cargo xtask release assemble artifacts/S099/release-new.json artifacts/S099/client-new
nix develop --command cargo xtask release verify artifacts/S099/release-new.json
```

适配器使用flake.lock锁定的NSIS（本环境实测3.12）、Rust/Node/pnpm等工具。清除旧desktop dist并冻结锁离线准备前端；在新Cargo target真实编译x64 MSVC release、静态CRT、无native-acceptance；核验PE架构并拒绝测试入口标识，再生成NSIS。收据记录完整commit/tree、版本与锁/协议源hash、工具实测版本、实际命令和全部payload文件hash。安装目录内的installed-files.json含同源身份和四个已安装EXE的摘要。

只接受同一HEAD的组件，脏树/来源变化/旧输出/缺文件均拒绝生成有效收据。合并或追加交接提交后旧组件不是新HEAD的发布产物，必须重新构建；实验记录需按实际来源提交解释，不能改写收据让它看起来来自后来提交。开发编译检查包没有发布receipt，不作为最终验收产物。

## 原生首装与重启演练

入口（先与父协调Windows独占窗口）：

```sh
nix develop .#web-tests --command python3 tests/platform/windows/s099-installation.py artifacts/S099/client-new
```

驱动会真实创建新标准用户，保存受限ACL下的原自动登录设置，执行约三次客体重启：新用户登录首装、安装后再次登录验证HKCU Run启动、恢复原Quickemu登录。原登录秘密留在客体私有目录，不进入源码或宿主证据；原LSA DefaultPassword使用机器DPAPI加密并读回校验，测试密码也只写LSA，不写注册表明文。按[微软自动登录说明](https://learn.microsoft.com/en-us/windows/win32/secauthn/protecting-the-automatic-logon-password)，LSA密码优先于注册表，必须保存与恢复两者；恢复后逐值核对并删除原登录备份。测试用户密码也只留在客体受限私有目录，便于后续保护演练。

程序、同步目录均使用中文空格路径；GUI是安装后的正式WebView2，通过[官方支持的CDP调试连接](https://playwright.dev/docs/webview2)进行页面操作，不运行浏览器fake，不编译acceptance钩子。只对这个GUI子进程设置调试环境变量，未在安装产物或系统策略中启用远程调试。验证GUI/CLI相同instance、GUI关闭后的实际上传、真实重启后无人工启动agent也继续上传；真实服务端字节与hash作为结果证据。

驱动的系统变更仅限新测试用户、独立目录、自己的登录触发任务和可恢复的自动登录设置。保留S084脏内容与注册根；测试结束保留新用户已安装程序/Root/state/cache，恢复原用户登录，保持VM在线。实际执行结果及来源提交见agent/tasks/S099.md与忽略artifacts/S099。
