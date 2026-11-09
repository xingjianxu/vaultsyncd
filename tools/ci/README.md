# 本地平台检查与安装验收（S101 / T36）

通过锁定的 NixOS 工具链构建 Linux x64 服务端并交叉构建 Windows x64 客户端，
在 Windows 11/NTFS 客体原生验证安装、WebView2 与 CFAPI。构建成功不能替代原生运行证据。

命令的唯一目录为 `run.py`，通过 `cargo xtask ci <job> [参数]` 调用，
`--plan` 仅显示命令，不能作执行证据。退出0表示实际步骤均成功；失败非0；缺资源78明确blocked。
每次执行要求干净提交，记录HEAD、空porcelain、平台/架构、实际命令、每步退出码和耗时，
输出在忽略目录 `artifacts/S101/ci/<job>-<nonce>/`。不会因缺前端依赖或CFAPI环境跳过测试。
便携检查入口为 `cargo xtask check`，按 design §5.8 同时验证受影响消费者。

```sh
nix develop --command pnpm install --frozen-lockfile
nix develop --command cargo xtask ci contracts
nix develop --command cargo xtask ci frontend
nix develop --command cargo xtask ci linux-server
nix develop --command cargo xtask ci release-linux
nix develop --command python3 tools/ci/test_ci.py
```

## 独占真实实验室

特权实验仅在已配置的专用可丢弃实验室手工运行。
本地flock禁止本控制器彼此并发；执行者仍需与其他实验室使用者协调窗口。
不改trial/Caddy/SSH，不关闭Windows客体。当前只覆盖 Linux x64 与 Windows x64；不需要 ARM 模板、QEMU 或 binfmt 注册。失败保留外部专属磁盘和日志供恢复。

在源码外私有JSON中设置：`{"evidence":"/data/vaultsyncd-temp/S101-唯一run"}`。
Windows沿用`docs/operations/windows-lab.md`的密钥与客体，设置共享交叉缓存。
入口（源码须已提交，两个阶段不允许改变HEAD）：

```sh
export VAULTSYNCD_CI_LAB=1
export VAULTSYNCD_WINDOWS_CACHE=/data/vaultsyncd-temp/cache/windows
nix develop --command python3 tools/ci/acceptance.py --config /私有/run.json --phase build
nix develop .#portable-tests --command python3 tools/ci/acceptance.py --config /私有/run.json --phase lab
```

构建阶段固定同HEAD的Nix/Linux x64 便携包/Web/依赖和Windows正式未签名NSIS；lab阶段
分别核对来源、manifest和payload hash，实际跑NixOS闭包/页面、Ubuntu容器btrfs上传/提交/固定下载/
备份/新目标离线恢复/新epoch/重新登录、Windows原生CFAPI释放和跨进程e2e，
最后复用S099安装驱动真实标准用户安装/GUI+CLI/关GUI同步/重启登录同步。
`VAULTSYNCD_CI_CLEANUP=1`仅本run启用：logout删除本用户凭据，完整驻留门通过才revert并卸载provider；
核对三份文件hash和普通文件属性后，恢复原Quickemu登录，删除新测试用户/配置/探针和私有密码，并仅删除该确切新SID的自动生成任务（包括Windows SoftLanding）。
历史S099默认保留行为不变。失败不强删不完整Vault，也不删除其他卡的账户或数据。

仅非秘密结果写`artifacts/S101/public/`；完整私有现场、凭据、SQLite、VM镜像保留在受限目录。
安装报告以`execution_environment: local`标记执行环境；NixOS x64、Ubuntu x64 和 Windows 原生必须分列。

以下是历史 S101 验收记录，ARM64 已由 S111/ADR-015 移出当前范围，不能作为现行执行指令。

S101实际产品源为`fcbbb93ecff408fedc8fbba6006c81b84a151727`，所有构建与安装时HEAD相同且porcelain为空；
后续仅实验装置/交接提交不重标旧receipt。NixOS26.11包原生运行；Ubuntu24.04.5 x64容器共享NixOS7.3内核，
ARM64为完整Ubuntu6.8内核TCG。Windows11 build26300.9711/WebView2 157实际标准用户安装、GUI/CLI同实例、
关GUI上传与真实重启后上传通过，5项原生CFAPI与S074五块e2e通过，不是三台并发机器。
首次动态解释器、后续TCG单探针超时两次装置失败均保留，后以相同命令目录逐job续验通过；不能把原控制器整体exit1说成exit0。
最终装置修复有9项回归及实际静态解释器选择证明；资源复核发现并补清新SID的SoftLanding任务，驱动亦已修正。
精确平台分列、哈希、失败与恢复范围见`agent/tasks/S101*.md`和忽略的`artifacts/S101/public/`。
