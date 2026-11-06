# 客户端未上传内容恢复

适用 Windows 11 x64 客户端。先保全未确认内容，再处理 intent 或重新开始同步。服务端整体离线恢复另见备份运维文档；本节不把客户端状态 JSON 当作可直接回滚的数据库。

## 先导出，再处理现场

1. 保留原同步目录、agent 状态目录与缓存；不要删除数据库、清空缓存、解除 CFAPI 注册或强制覆盖文件来“修复”状态。若应用仍在写文件，先保存并关闭该文件；有条件时暂停绑定。
2. 从同一 Windows 用户会话启动 agent，并确认 `vaultsyncd-cli.exe --json status` 能读取实际 IPC 状态。绑定处于恢复受阻状态时仍可请求导出。CLI 自身不打开数据库、也不在 agent 不可用时假装导出成功。介质缺失、权限不足或唯一快照无法读取时，保留现场并先恢复其可读条件。
3. 选择已有普通目录下的**尚不存在**的目的目录，位于同步目录、缓存和内部状态之外：

   ```text
   vaultsyncd-cli.exe --json export-recovery --binding-id <绑定ID> --destination D:/Recovery/case-20261107
   ```

   桌面“同步目录 → 管理与恢复 → 导出恢复包”使用同一 agent 命令。成功要求退出码0及 `management.exported_files`，且目录内有完整、可验证的 `manifest.json`。错误或只有部分文件的目录不能视为完整包；先查看错误并使用另一个新目录重试，不删除原内容来让导出通过。
4. 导出固定来源句柄、逐份读回 SHA-256，并在最后发布清单；不会清 dirty、完成 intent、确认 pending 上传或清除冲突候选。目录已存在、位于受保护树内、文件忙、来源变化或缺唯一快照均应失败。失败保留已有目的内容及源端；部分输出仅供诊断。

## 包内内容怎样对应

| 文件/来源 | 用途 |
| --- | --- |
| `manifest.json` | 格式1、绑定ID、每份文件字节数与SHA-256、来源类别/身份；不是可执行指令 |
| `state.json` | 绑定Root/epoch/模式、baseline与remote/local条目、pending、固定上传快照、未完intent、受控应用/释放现场；用于解释与对照，不直接导入在线数据库 |
| `current-<EntryId>.bin` | 实际当前完整本地字节；按state中的baseline条目身份、父目录和name了解原位置；可能比最后观察的generation更新 |
| `snapshot-<OpId>.bin` | 固定上传代际内容；与upload_resumes里的摘要/大小/身份核对。当前文件后来修改时，这份旧字节也必须保留 |
| `candidate-<身份>.bin` | 扫描发现的未入库新文件、独立暂存或冲突候选；身份文件名避免原名称冲突，不保证能从名字反推出原路径。按摘要和只读检查选择新的人工名称 |

基线保存服务器版本与revision身份，不表示该旧版本的字节必然都在本包中。当前dirty内容、旧固定上传快照和新未上传文件可能是三份不同内容，不能任选一份覆盖其他两份。对照清单的hash、来源类别以及原现场再决定取回对象。

## 独立核验与人工取回

在普通目录中操作，拒绝 reparse/symlink 包或清单引用。先核对 `format=1`、合法绑定身份、`state.binding.id`与清单一致。清单文件名只能是一个普通分量，不能含 `/`、`\`、`:`、`.`/`..`或重复名字。必须正好有一份state.json；内容项只允许current/snapshot/candidate，名字与来源kind/id相符。每项都要实际读回字节数和SHA-256；固定来源size/digest也要相等。任何缺失、不符或未知格式均停止，不报告恢复完成。

可用 PowerShell 对已核对的扁平清单逐份重算（目录与文件须为普通文件，清单来自本次保全的包）：

```powershell
$package = 'D:\Recovery\case-20261107'
$manifest = Get-Content (Join-Path $package 'manifest.json') -Raw -Encoding UTF8 | ConvertFrom-Json
if ($manifest.format -ne 1) { throw '未知格式' }
$names = @{}
foreach ($item in $manifest.files) {
  if ($item.name -match '[/\\:]' -or $item.name -in @('', '.', '..', 'manifest.json') -or $names.ContainsKey($item.name)) { throw '非法或重复文件名' }
  $names[$item.name] = $true
  $file = Get-Item -LiteralPath (Join-Path $package $item.name)
  if ($file.PSIsContainer -or ($file.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw '不是普通文件' }
  if ($file.Length -ne $item.size) { throw '长度不符' }
  if ((Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant() -ne $item.sha256) { throw '摘要不符' }
}
```

上面的循环只做文件一致性检查；前述绑定、state和来源身份检查同样必须完成。演练的原生驱动独立检查完整清单，并逐份核对已知内容。没有数字签名的清单不能证明来源真实性，请同时保留导出时的证据与包副本。

人工取回只复制选中的内容文件到**新的空普通目录**，采用CreateNew禁止覆盖。例：选中某个已核验的current项后，把该项的普通字节另存为 `my-draft.txt`；旧snapshot单独命名为 `older-upload.txt`。不要执行包中的脚本/程序，也不要把文件名、intent.source_path等当命令：

```powershell
New-Item -ItemType Directory 'D:\Recovered-empty'
$source = Join-Path $package 'current-<EntryId>.bin'
$target = 'D:\Recovered-empty\my-draft.txt'
$recoveryInput = [IO.File]::OpenRead($source)
try {
  $recoveryOutput = [IO.File]::Open($target, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
  try { $recoveryInput.CopyTo($recoveryOutput); $recoveryOutput.Flush($true) } finally { $recoveryOutput.Dispose() }
} finally { $recoveryInput.Dispose() }
Get-FileHash -LiteralPath $target -Algorithm SHA256
```

取回hash应与对应manifest项相同。原目录和包保持保留；后续要把取回文件合入同步目录，应在解决阻塞后走正常观察/冲突保留流程，不批量强制覆盖旧现场。安装、卸载、升级迁移尚由S099及后续卡验收，本演练不宣称覆盖。

## S084 原生演练入口

先在本卡独立target中构建真实服务端helper，再执行Windows runner：

```sh
nix develop --command cargo build --locked -p vaultsyncd-server-tests --example e2e-native-server
nix develop .#windows --command bash tests/windows-integration/run-unuploaded-recovery.sh
```

runner默认使用本树target；若设置CARGO_TARGET_DIR，两条命令必须指向同一独立目录。搬旧缓存后必须在当前树重编helper；runner核对fixture工作路径，拒绝带旧树内嵌路径的helper。客体使用真实NTFS、Interactive/Limited登录态与同用户IPC；仅本run上传part断连，其他请求转发真实Linux服务，绝不在线写DB。报告和hash清单保留在artifacts/S084，实际执行结果见agent/tasks/S084.md；Windows VM保持在线。
