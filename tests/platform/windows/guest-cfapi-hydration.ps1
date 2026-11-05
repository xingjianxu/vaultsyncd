# S011 客体验证：CFAPI 内容供给（读取、范围、取消/中断、超时、离线、重复请求）。
# 由 tests/platform/windows/run-cfapi-hydration.sh 经 scp + ssh 调用；只在客体运行，
# 不安装任何工具链，不访问共享文件夹（同步根在客体本地 NTFS）。
# 输出只用 ASCII：客体控制台代码页实测为 437，非 ASCII 会转写成乱码。
# 退出码：0 所有检查通过；1 任一检查失败。
#
# 契约（宿主脚本按这些行做两端比对）：
#   check=pass|fail name=<id> <detail>
#   guest_file_sha256 name=<entry> sha256=<hex>     读取后的文件 SHA-256（PowerShell 独立计算）
#   guest_range_sha256 name=<entry> offset=<n> length=<n> sha256=<hex>
#   cfapi_hydration_result: OK|FAILED

param(
    [Parameter(Mandatory = $true)][string]$Directory,
    [Parameter(Mandatory = $true)][string]$Root,
    [string]$Exe = 'cfapi-prototype.exe'
)

$ErrorActionPreference = 'Stop'
$failed = 0
$prototypeDirName = 'vaultsyncd-proto'
$small = 'small-12345.bin'
$large = 'large-1mib.bin'
$huge = 'huge-16mib.bin'
$hugeSize = 16777216
$rangeOffset = 4000000
$rangeLength = 1048576
$lastOut = ''
$lastExit = 0

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

function RunExe([string[]]$exeArgs) {
    # 原型的回调日志走 stderr；用 Start-Process 分别重定向，避免 PowerShell 把原生 stderr
    # 当成终止错误记录（那会在 Stop 偏好下中断脚本）。
    $outFile = Join-Path $Directory 'exe.out.tmp'
    $errFile = Join-Path $Directory 'exe.err.tmp'
    $process = Start-Process -FilePath $script:exePath -ArgumentList $exeArgs -NoNewWindow -Wait -PassThru `
        -RedirectStandardOutput $outFile -RedirectStandardError $errFile
    $script:lastExit = $process.ExitCode
    $script:lastOut = ((Get-Content -LiteralPath $outFile -Raw) + "`n" + (Get-Content -LiteralPath $errFile -Raw))
    Write-Output "--- exe $($exeArgs -join ' ') exit=$($script:lastExit)"
    foreach ($line in ($script:lastOut -split "`r?`n")) {
        if ($line.Trim().Length -gt 0) { Write-Output "  $line" }
    }
}

function StageLine([string]$stage) {
    ($script:lastOut -split "`r?`n" | Where-Object { $_ -like "stage=$stage *" } | Select-Object -Last 1)
}

function Field([string]$stage, [string]$name) {
    $line = StageLine $stage
    if ($line -and ($line -match "(?:^|\s)$name=(\S+)")) { return $matches[1] }
    return ''
}

function FileSha256([string]$path) {
    (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLower()
}

function RangeHash([string]$path, [long]$offset, [long]$length) {
    $stream = [System.IO.File]::OpenRead($path)
    try {
        $stream.Position = $offset
        $buffer = New-Object byte[] $length
        $read = 0
        while ($read -lt $length) {
            $n = $stream.Read($buffer, $read, $length - $read)
            if ($n -le 0) { break }
            $read += $n
        }
        $sha = [System.Security.Cryptography.SHA256]::Create()
        try {
            $hash = $sha.ComputeHash($buffer, 0, $read)
        }
        finally { $sha.Dispose() }
        return ([BitConverter]::ToString($hash) -replace '-', '').ToLower()
    }
    finally { $stream.Dispose() }
}

$exePath = Join-Path $Directory $Exe
if (-not (Test-Path -LiteralPath $exePath)) {
    Write-Output "check=fail name=exe_present path=$exePath"
    Write-Output 'cfapi_hydration_result: FAILED'
    exit 1
}

$os = Get-CimInstance -ClassName Win32_OperatingSystem
Write-Output "guest_os: $($os.Caption)"
Write-Output "guest_build: $($os.BuildNumber)"
Write-Output "guest_exe_sha256 $Exe $((Get-FileHash -Algorithm SHA256 -Path $exePath).Hash.ToLower())"
$volume = Get-Volume -DriveLetter $Root.Substring(0, 1)
Write-Output "guest_root_volume filesystem=$($volume.FileSystem) drive=$($volume.DriveLetter)"

Stage 'prereset'
if (Test-Path -LiteralPath $Root) {
    & $exePath cleanup --root $Root | Out-Null
    Write-Output "prereset_cleanup_exit=$LASTEXITCODE"
    Remove-Item -LiteralPath $Root -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $Root | Out-Null
$keepPath = Join-Path $Root 'user-keep.txt'
Set-Content -LiteralPath $keepPath -Value "user content must survive cleanup`r`nS011 vaultsyncd" -NoNewline -Encoding Ascii
$keepHashBefore = FileSha256 $keepPath
Write-Output "user_keep_before sha256=$keepHashBefore size=$((Get-Item -LiteralPath $keepPath).Length)"

Stage 'create'
RunExe @('create', '--root', $Root)
Check 'create_exit' ($lastExit -eq 0) "exit=$lastExit"

Stage 'inspect_dehydrated'
RunExe @('inspect', '--root', $Root)
Check 'inspect_dehydrated_exit' ($lastExit -eq 0) "exit=$lastExit"

# --- 离线：未水合条目必须明确失败，不能返回 0 字节成功 -------------------------------
Stage 'offline_dehydrated'
foreach ($name in @($small, $large, $huge)) {
    RunExe @('read', '--root', $Root, '--name', $name, '--source', 'unavailable', '--expect', 'failure')
    $outcome = Field 'read.result' 'outcome'
    $bytes = Field 'read.result' 'bytes'
    $readError = Field 'read.result' 'error'
    $unavailable = Field 'provider.stats' 'unavailable_fetches'
    Check "offline_read_fails_$name" ($lastExit -eq 0 -and $outcome -eq 'failed' -and $bytes -eq '0' -and $readError -ne '-') `
        "exit=$lastExit outcome=$outcome bytes=$bytes error=$readError"
    Check "offline_source_unavailable_$name" ([int]$unavailable -ge 1) "unavailable_fetches=$unavailable"
}

# --- 读取整份内容：字节与哈希必须等于固定内容版本 -----------------------------------
Stage 'read_small'
$smallPath = Join-Path (Join-Path $Root $prototypeDirName) $small
RunExe @('read', '--root', $Root, '--name', $small)
$bytes = Field 'read.result' 'bytes'
$digestMatch = Field 'read.result' 'digest_match'
$onDisk = Field 'read.result' 'on_disk_data_size'
$elapsed = Field 'read.result' 'elapsed_ms'
Check 'read_small_exit' ($lastExit -eq 0) "exit=$lastExit"
Check 'read_small_bytes' ($bytes -eq '12345') "bytes=$bytes"
Check 'read_small_digest' ($digestMatch -eq 'true') "digest_match=$digestMatch elapsed_ms=$elapsed"
Check 'read_small_on_disk' ($onDisk -eq '12345') "on_disk_data_size=$onDisk"
$smallHash = FileSha256 $smallPath
Write-Output "guest_file_sha256 name=$small sha256=$smallHash"
Check 'read_small_length' ((Get-Item -LiteralPath $smallPath).Length -eq 12345) "length=$((Get-Item -LiteralPath $smallPath).Length)"

# --- 离线读取已水合数据：本地命中，不应再向提供方请求内容 ---------------------------
Stage 'offline_hydrated'
RunExe @('read', '--root', $Root, '--name', $small, '--source', 'unavailable')
$outcome = Field 'read.result' 'outcome'
$digestMatch = Field 'read.result' 'digest_match'
$fetches = Field 'provider.stats' 'fetch_requests'
Check 'offline_hydrated_read_ok' ($lastExit -eq 0 -and $outcome -eq 'matched' -and $digestMatch -eq 'true') `
    "exit=$lastExit outcome=$outcome digest_match=$digestMatch"
Check 'offline_hydrated_no_fetch' ($fetches -eq '0') "fetch_requests=$fetches"
RunExe @('read', '--root', $Root, '--name', $large, '--source', 'unavailable', '--expect', 'failure')
Check 'offline_unhydrated_still_fails' ($lastExit -eq 0 -and (Field 'read.result' 'outcome') -eq 'failed') `
    "exit=$lastExit outcome=$(Field 'read.result' 'outcome') error=$(Field 'read.result' 'error')"

# --- 范围水合：平台应按页对齐请求一段而不是整份 -----------------------------------
Stage 'hydrate_range'
$hugePath = Join-Path (Join-Path $Root $prototypeDirName) $huge
RunExe @('hydrate', '--root', $Root, '--name', $huge, '--offset', "$rangeOffset", '--length', "$rangeLength")
$platformOffset = Field 'hydrate.result' 'platform_offset'
$platformLength = Field 'hydrate.result' 'platform_length'
$hydrateHr = Field 'hydrate.result' 'hydrate_hresult'
$onDisk = Field 'hydrate.result' 'on_disk_data_size'
Check 'hydrate_range_exit' ($lastExit -eq 0) "exit=$lastExit hydrate_hresult=$hydrateHr"
Check 'hydrate_range_is_partial' ($platformOffset -ne '-' -and [int]$platformLength -lt $hugeSize -and [int]$platformLength -gt 0) `
    "platform_offset=$platformOffset platform_length=$platformLength file_size=$hugeSize"
Check 'hydrate_range_on_disk' ([int]$onDisk -ge $rangeLength) "on_disk_data_size=$onDisk"
$rangeHash = RangeHash $hugePath $rangeOffset $rangeLength
Write-Output "guest_range_sha256 name=$huge offset=$rangeOffset length=$rangeLength sha256=$rangeHash"

# --- 大条目整份读取：分段回填（每段 64 KiB）后字节与哈希仍必须正确 -----------------
Stage 'read_huge_full'
RunExe @('read', '--root', $Root, '--name', $huge)
$bytes = Field 'read.result' 'bytes'
$chunks = Field 'provider.stats' 'chunks_sent'
Check 'read_huge_exit' ($lastExit -eq 0) "exit=$lastExit"
Check 'read_huge_bytes' ($bytes -eq "$hugeSize") "bytes=$bytes expected=$hugeSize"
Check 'read_huge_digest' ((Field 'read.result' 'digest_match') -eq 'true') "digest_match=$(Field 'read.result' 'digest_match')"
Check 'read_huge_chunked' ([int]$chunks -ge 2) "chunks_sent=$chunks"
$hugeHash = FileSha256 $hugePath
Write-Output "guest_file_sha256 name=$huge sha256=$hugeHash"

# --- 传输中断开同步根：回填循环必须尽快停止，调用有界返回 ---------------------------
Stage 'abort_disconnect'
RunExe @('hydrate', '--root', $Root, '--name', $large, '--offset', '0', '--length', '1048576', '--chunk-delay-ms', '150', '--disconnect-after-ms', '400')
$interruption = Field 'hydrate.result' 'interruption'
$chunksSent = Field 'hydrate.result' 'chunks_sent'
$failedAcks = Field 'hydrate.result' 'failed_acks'
$totalMs = Field 'hydrate.result' 'total_ms'
$inFlight = Field 'hydrate.result' 'in_flight'
Check 'abort_interruption_kind' ($interruption -eq 'disconnect') "interruption=$interruption"
Check 'abort_loop_stopped_early' ([int]$chunksSent -lt 16 -and [int]$chunksSent -ge 0) "chunks_sent=$chunksSent of 16"
Check 'abort_platform_rejected_ack' ([int]$failedAcks -ge 1) "failed_acks=$failedAcks"
Check 'abort_bounded_return' ([int]$totalMs -le 5000) "total_ms=$totalMs"
Check 'abort_no_inflight' ($inFlight -eq '0') "in_flight=$inFlight"

# --- 重置为脱水状态，验证“句柄可关闭、无永久卡住”的取消路径 -------------------------
Stage 'reset_placeholders'
& $exePath cleanup --root $Root | Out-Null
Write-Output "reset_cleanup_exit=$LASTEXITCODE"
Remove-Item -LiteralPath (Join-Path $Root $prototypeDirName) -Recurse -Force -ErrorAction SilentlyContinue
RunExe @('create', '--root', $Root)
Check 'reset_create_exit' ($lastExit -eq 0) "exit=$lastExit"

Stage 'cancel_close_handle'
# 1 MiB 条目 + 100 ms/段：整份水合约 1.6 s，取消点在 300 ms，足以验证“取消后读取有界返回”。
RunExe @('cancel', '--root', $Root, '--name', $large, '--cancel-after-ms', '300', '--chunk-delay-ms', '100')
$cancelCode = Field 'cancel.result' 'cancel_code'
$returnMs = Field 'cancel.result' 'cancel_to_return_ms'
$cancelCallbacks = Field 'cancel.result' 'cancel_callbacks'
$abortedFetches = Field 'cancel.result' 'aborted_fetches'
$inFlight = Field 'cancel.result' 'in_flight'
Check 'cancel_exit' ($lastExit -eq 0) "exit=$lastExit"
Check 'cancel_issued' ($cancelCode -eq '0') "cancel_code=$cancelCode"
Check 'cancel_bounded_return' ([int]$returnMs -le 5000) "cancel_to_return_ms=$returnMs"
Check 'cancel_no_inflight' ($inFlight -eq '0') "in_flight=$inFlight"
# 平台是否回调 CANCEL_FETCH_DATA 单独报告（该 build 未观察到，见 S011 交接）。
Write-Output "cancel_callback_observed=${cancelCallbacks} aborted_fetches=${abortedFetches}"

# --- 超时：慢源超过阈值必须明确失败且有界返回，不能悬挂 -----------------------------
Stage 'reset_for_timeout'
& $exePath cleanup --root $Root | Out-Null
Write-Output "timeout_reset_cleanup_exit=$LASTEXITCODE"
Remove-Item -LiteralPath (Join-Path $Root $prototypeDirName) -Recurse -Force -ErrorAction SilentlyContinue
RunExe @('create', '--root', $Root)
Check 'timeout_reset_create_exit' ($lastExit -eq 0) "exit=$lastExit"

Stage 'timeout_short'
RunExe @('read', '--root', $Root, '--name', $small, '--latency-ms', '1200', '--timeout-ms', '600', '--expect', 'failure')
$outcome = Field 'read.result' 'outcome'
$readError = Field 'read.result' 'error'
$elapsed = Field 'read.result' 'elapsed_ms'
$timeouts = Field 'provider.stats' 'timeout_fetches'
Check 'timeout_read_fails' ($lastExit -eq 0 -and $outcome -eq 'failed' -and $readError -ne '-') "exit=$lastExit outcome=$outcome error=$readError"
Check 'timeout_source_counted' ([int]$timeouts -ge 1) "timeout_fetches=$timeouts"
Check 'timeout_bounded' ([int]$elapsed -le 3000) "elapsed_ms=$elapsed"

Stage 'timeout_large'
RunExe @('read', '--root', $Root, '--name', $large, '--latency-ms', '1500', '--timeout-ms', '500', '--expect', 'failure')
Check 'timeout_large_fails_bounded' ($lastExit -eq 0 -and (Field 'read.result' 'outcome') -eq 'failed') `
    "exit=$lastExit outcome=$(Field 'read.result' 'outcome') elapsed_ms=$(Field 'read.result' 'elapsed_ms') error=$(Field 'read.result' 'error')"

# --- 重复请求：超时失败后再次读取必须成功且哈希一致（同一固定版本，无混版本） -----
Stage 'retry_after_timeout'
RunExe @('read', '--root', $Root, '--name', $small)
$fetches = Field 'provider.stats' 'fetch_requests'
$chunks = Field 'provider.stats' 'chunks_sent'
Check 'retry_after_timeout_ok' ($lastExit -eq 0 -and (Field 'read.result' 'digest_match') -eq 'true') `
    "exit=$lastExit digest_match=$(Field 'read.result' 'digest_match') fetch_requests=$fetches chunks_sent=$chunks"
$retryHash = FileSha256 (Join-Path (Join-Path $Root $prototypeDirName) $small)
Check 'retry_after_timeout_hash' ($retryHash -eq $smallHash) "before=$smallHash after=$retryHash"

# --- 收尾：只删本工具占位，用户内容保留，注销生效 ----------------------------------
Stage 'inspect_hydrated'
RunExe @('inspect', '--root', $Root, '--allow-hydrated')
Check 'inspect_hydrated_exit' ($lastExit -eq 0) "exit=$lastExit"

Stage 'cleanup'
RunExe @('cleanup', '--root', $Root)
Check 'cleanup_exit' ($lastExit -eq 0) "exit=$lastExit"
$dirExists = Test-Path -LiteralPath (Join-Path $Root $prototypeDirName)
Check 'cleanup_removed_probe_dir' (-not $dirExists) "path=$prototypeDirName"
$keepHashAfter = FileSha256 $keepPath
Check 'cleanup_preserved_user_content' ($keepHashAfter -eq $keepHashBefore) "before=$keepHashBefore after=$keepHashAfter"
RunExe @('inspect', '--root', $Root)
Check 'unregistered_after_cleanup' ($lastExit -eq 3) "exit=$lastExit (3 = not registered)"
RunExe @('cleanup', '--root', $Root)
Check 'cleanup_again_exit' ($lastExit -eq 0) "exit=$lastExit"

if ($failed -ne 0) {
    Write-Output 'cfapi_hydration_result: FAILED'
    exit 1
}
Write-Output 'cfapi_hydration_result: OK'
exit 0
