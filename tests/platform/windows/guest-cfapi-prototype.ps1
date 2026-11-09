# S010 客体验证：CFAPI 注册、占位创建、状态证据与“保留用户数据”的清理。
# 由 tests/platform/windows/run-cfapi-prototype.sh 经 scp + ssh 调用；只在客体运行，
# 不安装任何工具链，也不访问共享文件夹（同步根在客体本地 NTFS）。
# 输出只用 ASCII：客体控制台代码页实测为 437，非 ASCII 会转写成乱码。
# 退出码：0 所有检查通过；1 任一检查失败。

param(
    [Parameter(Mandatory = $true)][string]$Directory,
    [Parameter(Mandatory = $true)][string]$Vault,
    [string]$Exe = 'cfapi-prototype.exe'
)

$ErrorActionPreference = 'Stop'
$failed = 0
$prototypeDirName = 'vaultsyncd-proto'
$placeholderFiles = @('small-12345.bin', 'large-1mib.bin')
$expectedSizes = @{ 'small-12345.bin' = 12345; 'large-1mib.bin' = 1048576 }

function Stage([string]$name) {
    Write-Output "guest_stage: $name"
}

function Check([string]$name, [bool]$ok, [string]$detail) {
    if ($ok) {
        Write-Output "check=pass name=$name $detail"
    }
    else {
        Write-Output "check=fail name=$name $detail"
        $script:failed = 1
    }
}

$exePath = Join-Path $Directory $Exe
if (-not (Test-Path -LiteralPath $exePath)) {
    Write-Output "check=fail name=exe_present path=$exePath"
    Write-Output 'cfapi_prototype_result: FAILED'
    exit 1
}

$os = Get-CimInstance -ClassName Win32_OperatingSystem
Write-Output "guest_os: $($os.Caption)"
Write-Output "guest_version: $($os.Version)"
Write-Output "guest_build: $($os.BuildNumber)"
Write-Output "guest_architecture: $env:PROCESSOR_ARCHITECTURE"
Write-Output "guest_exe_sha256 $Exe $((Get-FileHash -Algorithm SHA256 -Path $exePath).Hash.ToLower())"

$volume = Get-Volume -DriveLetter $Vault.Substring(0, 1)
Write-Output "guest_vault_volume filesystem=$($volume.FileSystem) drive=$($volume.DriveLetter)"

Stage 'prereset'
# 上一轮留下的注册/目录会让“是否是占位符”的证据失效，因此先清理再建目录。
if (Test-Path -LiteralPath $Vault) {
    & $exePath cleanup --vault $Vault
    Write-Output "prereset_cleanup_exit=$LASTEXITCODE"
    Remove-Item -LiteralPath $Vault -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $Vault | Out-Null
Write-Output "guest_vault_path=$Vault"

Stage 'user_content'
# 清理前先放普通用户内容：同步根下的文件与子目录都必须原样保留。
$keepPath = Join-Path $Vault 'user-keep.txt'
$keepText = "user content must survive cleanup`r`nS010 vaultsyncd"
Set-Content -LiteralPath $keepPath -Value $keepText -NoNewline -Encoding Ascii
$keepHashBefore = (Get-FileHash -Algorithm SHA256 -Path $keepPath).Hash.ToLower()
$keepSizeBefore = (Get-Item -LiteralPath $keepPath).Length
Write-Output "user_keep_before path=user-keep.txt sha256=$keepHashBefore size=$keepSizeBefore"

$userDir = Join-Path $Vault 'user-dir'
New-Item -ItemType Directory -Force -Path $userDir | Out-Null
$notePath = Join-Path $userDir 'keep-note.txt'
Set-Content -LiteralPath $notePath -Value 'nested user content must survive cleanup' -NoNewline -Encoding Ascii
$noteHashBefore = (Get-FileHash -Algorithm SHA256 -Path $notePath).Hash.ToLower()
Write-Output "user_note_before path=user-dir/keep-note.txt sha256=$noteHashBefore size=$((Get-Item -LiteralPath $notePath).Length)"

Stage 'create'
& $exePath create --vault $Vault
$createExit = $LASTEXITCODE
Write-Output "create_exit=$createExit"
Check 'create_exit' ($createExit -eq 0) "exit=$createExit"

Stage 'inspect'
& $exePath inspect --vault $Vault
$inspectExit = $LASTEXITCODE
Write-Output "inspect_exit=$inspectExit"
Check 'inspect_exit' ($inspectExit -eq 0) "exit=$inspectExit (0 = registered and all entries verified)"

Stage 'placeholder_evidence'
# 与原型无关的第三方证据：Get-Item 属性/长度、attrib、fsutil reparsepoint 与 sparse 查询。
$prototypeDir = Join-Path $Vault $prototypeDirName
$dirItem = Get-Item -LiteralPath $prototypeDir -Force
Write-Output "evidence_getitem_dir path=$prototypeDirName length=$($dirItem.Length) attributes_value=$([int]$dirItem.Attributes) attributes_name=$($dirItem.Attributes)"
foreach ($name in $placeholderFiles) {
    $path = Join-Path $prototypeDir $name
    $item = Get-Item -LiteralPath $path -Force
    $attrValue = [int]$item.Attributes
    $reparseBit = ($attrValue -band 0x400) -ne 0
    $recallBit = ($attrValue -band 0x400000) -ne 0
    Write-Output "evidence_getitem path=$prototypeDirName/$name length=$($item.Length) attributes_value=$attrValue attributes_name=$($item.Attributes) reparse_bit=$reparseBit recall_on_data_access_bit=$recallBit"
    Check "getitem_length_$name" ($item.Length -eq $expectedSizes[$name]) "length=$($item.Length) expected=$($expectedSizes[$name])"
    Check "getitem_not_zero_$name" ($item.Length -gt 0) "length=$($item.Length)"
    Check "getitem_reparse_$name" $reparseBit "attributes_value=$attrValue"

    # attrib 在客体上对云占位符返回 "Path not found"（S010 实测），不能作为证据来源；
    # 占位证据统一用 Get-Item 属性、fsutil reparsepoint 与原型自己的 CFAPI 查询。
    Write-Output "evidence_reparsepoint $prototypeDirName/$name"
    & fsutil reparsepoint query $path | ForEach-Object { Write-Output "  $_" }

    Write-Output "evidence_sparse $prototypeDirName/$name"
    & fsutil sparse queryflag $path | ForEach-Object { Write-Output "  $_" }
}

Stage 'read_hydration'
# S011 起内容水合已实现：用 `read` 子命令（自己连接提供方）读取小条目，必须成功且长度正确。
# 逐字节/哈希、范围请求、离线、超时与中断的完整证据在 `guest-cfapi-hydration.ps1`（S011）。
$readTarget = Join-Path $prototypeDir $placeholderFiles[0]
& $exePath read --vault $Vault --name $placeholderFiles[0]
$readExit = $LASTEXITCODE
Write-Output "read_exit=$readExit"
Check 'read_hydrated_exit' ($readExit -eq 0) "exit=$readExit"
$readLength = (Get-Item -LiteralPath $readTarget).Length
$readHash = (Get-FileHash -Algorithm SHA256 -Path $readTarget).Hash.ToLower()
Write-Output "evidence_read path=$prototypeDirName/$($placeholderFiles[0]) outcome=hydrated length=$readLength sha256=$readHash"
Check 'read_hydrated_length' ($readLength -eq $expectedSizes[$placeholderFiles[0]]) "length=$readLength expected=$($expectedSizes[$placeholderFiles[0]])"

Stage 'cleanup'
& $exePath cleanup --vault $Vault
$cleanupExit = $LASTEXITCODE
Write-Output "cleanup_exit=$cleanupExit"
Check 'cleanup_exit' ($cleanupExit -eq 0) "exit=$cleanupExit"

Stage 'post_checks'
$keepExists = Test-Path -LiteralPath $keepPath
$keepHashAfter = if ($keepExists) { (Get-FileHash -Algorithm SHA256 -Path $keepPath).Hash.ToLower() } else { 'missing' }
Write-Output "user_keep_after path=user-keep.txt exists=$keepExists sha256=$keepHashAfter size=$((Get-Item -LiteralPath $keepPath -ErrorAction SilentlyContinue).Length)"
Check 'user_keep_exists' $keepExists "path=user-keep.txt"
Check 'user_keep_unchanged' ($keepHashAfter -eq $keepHashBefore) "before=$keepHashBefore after=$keepHashAfter"

$noteExists = Test-Path -LiteralPath $notePath
$noteHashAfter = if ($noteExists) { (Get-FileHash -Algorithm SHA256 -Path $notePath).Hash.ToLower() } else { 'missing' }
Write-Output "user_note_after path=user-dir/keep-note.txt exists=$noteExists sha256=$noteHashAfter size=$((Get-Item -LiteralPath $notePath -ErrorAction SilentlyContinue).Length)"
Check 'user_note_exists' $noteExists "path=user-dir/keep-note.txt"
Check 'user_note_unchanged' ($noteHashAfter -eq $noteHashBefore) "before=$noteHashBefore after=$noteHashAfter"

$dirExists = Test-Path -LiteralPath $prototypeDir
Write-Output "placeholder_dir_after exists=$dirExists"
Check 'placeholder_dir_removed' (-not $dirExists) "path=$prototypeDirName"
foreach ($name in $placeholderFiles) {
    $path = Join-Path $prototypeDir $name
    $exists = Test-Path -LiteralPath $path
    Write-Output "placeholder_file_after path=$prototypeDirName/$name exists=$exists"
    Check "placeholder_removed_$name" (-not $exists) "path=$prototypeDirName/$name"
}

Stage 'inspect_after_cleanup'
# 3 = 同步根未注册（cleanup 之后的预期状态）；其他非零值都是失败。
& $exePath inspect --vault $Vault
$postInspectExit = $LASTEXITCODE
Write-Output "inspect_after_cleanup_exit=$postInspectExit"
Check 'unregistered_after_cleanup' ($postInspectExit -eq 3) "exit=$postInspectExit (3 = not registered)"

Stage 'cleanup_again'
# 幂等性：没有注册也没有占位时再清理一次仍然成功，且不改动用户内容。
& $exePath cleanup --vault $Vault
$cleanupAgainExit = $LASTEXITCODE
Write-Output "cleanup_again_exit=$cleanupAgainExit"
Check 'cleanup_again_exit' ($cleanupAgainExit -eq 0) "exit=$cleanupAgainExit"
$keepHashFinal = if (Test-Path -LiteralPath $keepPath) { (Get-FileHash -Algorithm SHA256 -Path $keepPath).Hash.ToLower() } else { 'missing' }
Check 'user_keep_unchanged_after_second_cleanup' ($keepHashFinal -eq $keepHashBefore) "before=$keepHashBefore final=$keepHashFinal"

if ($failed -ne 0) {
    Write-Output 'cfapi_prototype_result: FAILED'
    exit 1
}
Write-Output 'cfapi_prototype_result: OK'
exit 0
