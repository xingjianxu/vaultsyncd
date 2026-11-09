# S012 guest verification: CFAPI release (dehydrate) and write protection (protect).
# Called by tests/platform/windows/run-cfapi-release.sh over scp + ssh; runs on the guest only,
# installs no toolchain and never touches a shared folder (the sync vault is local NTFS).
#
# This file is intentionally ASCII-only: the guest console code page is 437 and PowerShell 5.1
# parses .ps1 files without a BOM as ANSI, so non-ASCII comments can break the parser (observed
# while drafting this script). Keep every byte below 0x80.
#
# Exit code: 0 all checks passed, 1 any check failed.
#
# Contract consumed by the host script:
#   check=pass|fail name=<id> <detail>
#   guest_file_sha256 name=<entry> sha256=<hex>     SHA-256 of a pristine-content sample
#                                                   (computed by PowerShell, compared on the host)
#   guest_written_sha256 name=<entry> style=<x> ...  SHA-256 after an editor-style write
#                                                   (guest-only comparison, never compared on the host)
#   guard=<verdict>/<reason> <tag>                   decision summary for readability
#   blocked_item=<id> <detail>                       behaviour this environment cannot prove safe
#   cfapi_release_result: OK|FAILED
#
# Editor behaviour is simulated with PowerShell: in-place overwrite, memory-mapped write,
# FileShare.None exclusive handle, FileAccess.Read + FileShare.Read shared lock, rename-over save.
# Every scenario starts from a freshly reset, re-hydrated sample.

param(
    [Parameter(Mandatory = $true)][string]$Directory,
    [Parameter(Mandatory = $true)][string]$Vault,
    [string]$RenameVault = 'C:\vaultsyncd-test-rename',
    [string]$Exe = 'cfapi-prototype.exe'
)

$ErrorActionPreference = 'Stop'
$failed = 0
$prototypeDirName = 'vaultsyncd-proto'
$small = 'small-12345.bin'
$smallSize = 12345
$lastOut = ''
$lastExit = 0
$lastResetHash = ''

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

function Blocked([string]$name, [string]$detail) {
    Write-Output "blocked_item=$name $detail"
}

function RunExe([string[]]$exeArgs) {
    # Redirect native stdout/stderr separately: with $ErrorActionPreference = 'Stop' PowerShell
    # treats native stderr as a terminating error.
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

function FileLength([string]$path) {
    (Get-Item -LiteralPath $path).Length
}

# Delete a directory tree with cmd's rd: Remove-Item -Recurse fails on sync-vault leftovers whose
# names the cloud filter will not enumerate. cmd stderr is merged and discarded, and the error
# preference is relaxed only for this call so a non-empty tree cannot abort the whole script.
function Remove-RootDir([string]$path) {
    $win = $path -replace '/', '\'
    if (-not (Test-Path -LiteralPath $win)) { return }
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    cmd /c "rd /s /q `"$win`"" > $null 2>&1
    $code = $LASTEXITCODE
    $ErrorActionPreference = $previous
    Write-Output "rd_exit=$code path=$win"
}

# Run the exe without letting a non-zero exit abort the script: the error preference is relaxed
# only for this call and every byte of output is discarded (the caller logs what it needs).
function Invoke-Quiet([string[]]$exeArgs) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & $script:exePath @exeArgs > $null 2>&1
    $code = $LASTEXITCODE
    $ErrorActionPreference = $previous
    return $code
}

function SamplePath([string]$vault) {
    Join-Path (Join-Path $vault $prototypeDirName) $small
}

# Reset a sync vault to "freshly created placeholders, all dehydrated".
function Reset-Vault([string]$vault) {
    if (Test-Path -LiteralPath $vault) { Invoke-Quiet @('cleanup', '--vault', $vault) | Out-Null }
    Remove-Item -Recurse -Force (Join-Path $vault $prototypeDirName) -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $vault | Out-Null
    RunExe @('create', '--vault', $vault)
    Check "reset_create_$vault" ($lastExit -eq 0) "exit=$lastExit"
}

# Reset and hydrate the small sample (an editor can only write local content).
# The resulting hash goes into script scope so it does not mix with the logged output stream.
function Reset-Hydrated([string]$vault) {
    Reset-Vault $vault
    RunExe @('read', '--vault', $vault, '--name', $small)
    Check "reset_read_$vault" (($lastExit -eq 0) -and ((Field 'read.result' 'digest_match') -eq 'true')) `
        "exit=$lastExit digest_match=$(Field 'read.result' 'digest_match')"
    $script:lastResetHash = FileSha256 (SamplePath $vault)
}

# Start a third-party process that keeps a handle open (simulates another application).
function Start-Holder([string]$body) {
    Start-Process -FilePath 'powershell' -ArgumentList '-NoProfile', '-Command', $body -PassThru -WindowStyle Hidden
}

function Wait-Holder($holder) {
    $holder | Wait-Process -Timeout 30 -ErrorAction SilentlyContinue
    return [bool]$holder.HasExited
}

$Vault = $Vault -replace '/', '\'
$RenameVault = $RenameVault -replace '/', '\'
$exePath = Join-Path $Directory $Exe
if (-not (Test-Path -LiteralPath $exePath)) {
    Write-Output "check=fail name=exe_present path=$exePath"
    Write-Output 'cfapi_release_result: FAILED'
    exit 1
}

$os = Get-CimInstance -ClassName Win32_OperatingSystem
Write-Output "guest_os: $($os.Caption)"
Write-Output "guest_build: $($os.BuildNumber)"
Write-Output "guest_exe_sha256 $Exe $((Get-FileHash -Algorithm SHA256 -Path $exePath).Hash.ToLower())"
$volume = Get-Volume -DriveLetter $Vault.Substring(0, 1)
Write-Output "guest_vault_volume filesystem=$($volume.FileSystem) drive=$($volume.DriveLetter)"

# --- reset: both the main vault and the rename-only vault start clean -------------------
Stage 'prereset'
foreach ($anyVault in @($Vault, $RenameVault)) {
    if (Test-Path -LiteralPath $anyVault) {
        Invoke-Quiet @('cleanup', '--vault', $anyVault) | Out-Null
        Remove-RootDir $anyVault
    }
}
New-Item -ItemType Directory -Force -Path $Vault | Out-Null
$keepPath = Join-Path $Vault 'user-keep.txt'
Set-Content -LiteralPath $keepPath -Value "user content must survive cleanup`r`nS012 vaultsyncd" -NoNewline -Encoding Ascii
$keepHashBefore = FileSha256 $keepPath
Write-Output "user_keep_before sha256=$keepHashBefore size=$((Get-Item -LiteralPath $keepPath).Length)"
RunExe @('create', '--vault', $Vault)
Check 'create_exit' ($lastExit -eq 0) "exit=$lastExit"

Stage 'inspect_dehydrated'
RunExe @('inspect', '--vault', $Vault)
Check 'inspect_dehydrated_exit' ($lastExit -eq 0) "exit=$lastExit"

# --- dehydrated sample: verdict unknown/already_dehydrated, no hydration side effect ---
Stage 'protect_dehydrated'
RunExe @('protect', '--vault', $Vault, '--name', $small)
$verdict = Field 'guard.decision' 'verdict'
$reason = Field 'guard.decision' 'reason'
$onDisk = Field 'guard.decision' 'on_disk_data_size'
$probe = Field 'guard.decision' 'probe_read_exclusive'
Write-Output "guard=$verdict/$reason protect_dehydrated"
Check 'protect_dehydrated_exit' ($lastExit -eq 0) "exit=$lastExit"
Check 'protect_dehydrated_verdict' (($verdict -eq 'unknown') -and ($reason -eq 'already_dehydrated')) `
    "verdict=$verdict reason=$reason"
Check 'protect_dehydrated_no_probe' (($probe -eq 'skipped') -and ($onDisk -eq '0')) `
    "probe_read_exclusive=$probe on_disk_data_size=$onDisk"

# --- clean sample: releasable, and reading it again returns the same bytes -------------
Stage 'clean_release'
$sample = SamplePath $Vault
RunExe @('read', '--vault', $Vault, '--name', $small)
$hydratedHash = FileSha256 $sample
Write-Output "guest_file_sha256 name=$small sha256=$hydratedHash"
Check 'clean_read_digest' ((Field 'read.result' 'digest_match') -eq 'true') `
    "digest_match=$(Field 'read.result' 'digest_match') on_disk_data_size=$(Field 'read.result' 'on_disk_data_size')"

RunExe @('protect', '--vault', $Vault, '--name', $small)
$verdict = Field 'guard.decision' 'verdict'
$reason = Field 'guard.decision' 'reason'
Write-Output "guard=$verdict/$reason protect_clean"
Check 'protect_clean_verdict' (($lastExit -eq 0) -and ($verdict -eq 'clean')) `
    "exit=$lastExit verdict=$verdict reason=$reason"
Check 'protect_clean_evidence' `
    (((Field 'guard.decision' 'content') -eq 'matched') -and `
        ((Field 'guard.decision' 'content_stable') -eq 'true') -and `
        ((Field 'guard.decision' 'probe_read_exclusive') -eq 'opened') -and `
        ((Field 'guard.decision' 'probe_write_exclusive') -eq 'opened') -and `
        ((Field 'guard.decision' 'probe_shared_read') -eq 'opened') -and `
        ((Field 'guard.decision' 'local_sha256') -eq (Field 'guard.decision' 'expected_sha256'))) `
    "content=$(Field 'guard.decision' 'content') stable=$(Field 'guard.decision' 'content_stable') probes=$(Field 'guard.decision' 'probe_read_exclusive')/$(Field 'guard.decision' 'probe_write_exclusive')/$(Field 'guard.decision' 'probe_shared_read')"

RunExe @('release', '--vault', $Vault, '--name', $small, '--expect', 'released')
$outcome = Field 'release.result' 'outcome'
$dehydrateHr = Field 'release.result' 'dehydrate_hresult'
$inSyncHr = Field 'release.result' 'in_sync_hresult'
$onDiskAfter = Field 'release.result' 'on_disk_after'
Write-Output "guard=$(Field 'release.result' 'verdict')/$(Field 'release.result' 'reason') release_clean"
Check 'clean_release_exit' ($lastExit -eq 0) "exit=$lastExit outcome=$outcome"
Check 'clean_release_outcome' ($outcome -eq 'released') "outcome=$outcome"
Check 'clean_release_platform_ok' (($dehydrateHr -eq '0x00000000') -and ($inSyncHr -eq '0x00000000')) `
    "in_sync_hresult=$inSyncHr dehydrate_hresult=$dehydrateHr"
Check 'clean_release_on_disk_zero' ($onDiskAfter -eq '0') "on_disk_after=$onDiskAfter"

# Re-observe in a fresh process: the entry must still be a placeholder with the same logical
# size/identity and must be dehydrated. The find-data state bits lag inside the releasing
# process (S012 measured), so the check runs in a new process instead of trusting that cache.
Stage 'inspect_after_release'
RunExe @('inspect', '--vault', $Vault)
Write-Output "inspect_small=$(StageLine 'inspect.entry')"
Check 'inspect_after_release_exit' ($lastExit -eq 0) "exit=$lastExit"
RunExe @('protect', '--vault', $Vault, '--name', $small)
$verdict = Field 'guard.decision' 'verdict'
$reason = Field 'guard.decision' 'reason'
$inSync = Field 'guard.decision' 'in_sync_state'
Write-Output "guard=$verdict/$reason protect_released"
Check 'released_entry_marked_in_sync' ($inSync -eq '1') "in_sync_state=$inSync (set by CfSetInSyncState before dehydrate)"
Check 'released_entry_needs_nothing' (($verdict -eq 'unknown') -and ($reason -eq 'already_dehydrated')) `
    "verdict=$verdict reason=$reason"

# --- rehydration: after release the sample must read back byte-identical ---------------
Stage 'rehydrate_after_release'
RunExe @('read', '--vault', $Vault, '--name', $small)
$refetch = Field 'provider.stats' 'fetch_requests'
$rehydratedHash = FileSha256 $sample
Write-Output "guest_file_sha256 name=$small sha256=$rehydratedHash"
Check 'rehydrate_exit' ($lastExit -eq 0) "exit=$lastExit"
Check 'rehydrate_digest' ((Field 'read.result' 'digest_match') -eq 'true') `
    "digest_match=$(Field 'read.result' 'digest_match') bytes=$(Field 'read.result' 'bytes')"
Check 'rehydrate_refetched' ([int]$refetch -ge 1) "fetch_requests=$refetch"
Check 'rehydrate_hash_unchanged' ($rehydratedHash -eq $hydratedHash) `
    "before=$hydratedHash after=$rehydratedHash"

# --- editor scenario 1: in-place overwrite (same inode, overwrite in place) ------------
Stage 'in_place_write'
Reset-Hydrated $Vault
$before = $script:lastResetHash
$fs = [System.IO.File]::Open($sample, 'Open', 'ReadWrite', 'None')
try {
    $payload = New-Object byte[] 100
    for ($i = 0; $i -lt 100; $i++) { $payload[$i] = 0x5A }
    $fs.Write($payload, 0, 100)
    $fs.Flush($true)
}
finally { $fs.Dispose() }
$written = FileSha256 $sample
Write-Output "guest_written_sha256 name=$small style=in_place sha256=$written size=$(FileLength $sample)"
Check 'in_place_write_changed_content' ($written -ne $before) "before=$before after=$written"

RunExe @('protect', '--vault', $Vault, '--name', $small)
$verdict = Field 'guard.decision' 'verdict'
$reason = Field 'guard.decision' 'reason'
$modified = Field 'guard.decision' 'modified_data_size'
Write-Output "guard=$verdict/$reason protect_in_place"
Check 'in_place_verdict_dirty' (($verdict -eq 'dirty') -and ($reason -eq 'content_mismatch')) `
    "verdict=$verdict reason=$reason"
Check 'in_place_modified_size' ([int]$modified -gt 0) "modified_data_size=$modified"

RunExe @('release', '--vault', $Vault, '--name', $small, '--expect', 'refused')
$outcome = Field 'release.result' 'outcome'
Write-Output "guard=$(Field 'release.result' 'verdict')/$(Field 'release.result' 'reason') release_in_place"
Check 'in_place_release_refused' (($lastExit -eq 0) -and ($outcome -eq 'refused')) `
    "exit=$lastExit outcome=$outcome"
Check 'in_place_no_platform_call' ((Field 'release.result' 'dehydrate_hresult') -eq '-') `
    "dehydrate_hresult=$(Field 'release.result' 'dehydrate_hresult')"
$afterRefusal = FileSha256 $sample
Check 'in_place_content_preserved' ($afterRefusal -eq $written) "before=$written after=$afterRefusal"
Check 'in_place_still_hydrated' (((FileLength $sample) -eq $smallSize) -and ((Field 'release.result' 'on_disk_after') -eq "$smallSize")) `
    "size=$(FileLength $sample) on_disk_after=$(Field 'release.result' 'on_disk_after')"

# --- editor scenario 2: memory-mapped write (mapping closed afterwards) ---------------
Stage 'mapped_write'
Reset-Hydrated $Vault
$before = $script:lastResetHash
$payload = New-Object byte[] 64
for ($i = 0; $i -lt 64; $i++) { $payload[$i] = 0xA5 }
$mmf = $null
$view = $null
$mmapError = ''
try {
    $mmf = [System.IO.MemoryMappedFiles.MemoryMappedFile]::CreateFromFile($sample, [System.IO.FileMode]::Open)
    $view = $mmf.CreateViewAccessor([int64]0, [int64]0, [System.IO.MemoryMappedFiles.MemoryMappedFileAccess]::ReadWrite)
    $view.WriteArray(0, $payload, 0, 64)
    $view.Flush()
}
catch {
    $inner = $_.Exception.InnerException; if (-not $inner) { $inner = $_.Exception }
    $mmapError = "0x$('{0:X8}' -f $inner.HResult) $($inner.Message.Trim())"
}
finally {
    if ($view) { $view.Dispose() }
    if ($mmf) { $mmf.Dispose() }
}
Check 'mapped_write_api_available' ($mmapError -eq '') "error=$mmapError"
$written = FileSha256 $sample
Write-Output "guest_written_sha256 name=$small style=mapped sha256=$written size=$(FileLength $sample)"
Check 'mapped_write_changed_content' ($written -ne $before) "before=$before after=$written"

RunExe @('protect', '--vault', $Vault, '--name', $small)
$verdict = Field 'guard.decision' 'verdict'
$reason = Field 'guard.decision' 'reason'
Write-Output "guard=$verdict/$reason protect_mapped"
Check 'mapped_write_verdict_dirty' (($verdict -eq 'dirty') -and ($reason -eq 'content_mismatch')) `
    "verdict=$verdict reason=$reason"

RunExe @('release', '--vault', $Vault, '--name', $small, '--expect', 'refused')
$outcome = Field 'release.result' 'outcome'
Write-Output "guard=$(Field 'release.result' 'verdict')/$(Field 'release.result' 'reason') release_mapped"
Check 'mapped_write_release_refused' (($lastExit -eq 0) -and ($outcome -eq 'refused')) `
    "exit=$lastExit outcome=$outcome"
$afterRefusal = FileSha256 $sample
Check 'mapped_write_content_preserved' ($afterRefusal -eq $written) "before=$written after=$afterRefusal"
Check 'mapped_write_still_hydrated' ((FileLength $sample) -eq $smallSize) "size=$(FileLength $sample)"

# --- editor scenario 3: release attempted while a writable mapping is still open ------
Stage 'mapped_held_write'
Reset-Hydrated $Vault
$before = $script:lastResetHash
$holdCode = "`$m=[System.IO.MemoryMappedFiles.MemoryMappedFile]::CreateFromFile('$sample',[System.IO.FileMode]::Open); " +
    "`$v=`$m.CreateViewAccessor([int64]0,[int64]0,[System.IO.MemoryMappedFiles.MemoryMappedFileAccess]::ReadWrite); " +
    "`$b=New-Object byte[] 32; `$v.WriteArray(0,`$b,0,32); `$v.Flush(); Start-Sleep -Seconds 6; `$v.Dispose(); `$m.Dispose()"
$holder = Start-Holder $holdCode
Start-Sleep -Milliseconds 2000
Write-Output "holder_running=$(-not $holder.HasExited)"
RunExe @('protect', '--vault', $Vault, '--name', $small)
$verdict = Field 'guard.decision' 'verdict'
$reason = Field 'guard.decision' 'reason'
$probeRead = Field 'guard.decision' 'probe_read_exclusive'
Write-Output "guard=$verdict/$reason protect_mapped_held"
Check 'mapped_held_verdict_busy' (($verdict -eq 'busy') -and ($reason -eq 'handle_held_read')) `
    "verdict=$verdict reason=$reason"
Check 'mapped_held_probe_conflict' ($probeRead -eq 'error:32') "probe_read_exclusive=$probeRead"

RunExe @('release', '--vault', $Vault, '--name', $small, '--expect', 'refused')
$outcome = Field 'release.result' 'outcome'
Write-Output "guard=$(Field 'release.result' 'verdict')/$(Field 'release.result' 'reason') release_mapped_held"
Check 'mapped_held_release_refused' (($lastExit -eq 0) -and ($outcome -eq 'refused')) `
    "exit=$lastExit outcome=$outcome"
Check 'mapped_held_not_dehydrated' ((Field 'release.result' 'on_disk_after') -eq "$smallSize") `
    "on_disk_after=$(Field 'release.result' 'on_disk_after')"
$holderExited = Wait-Holder $holder
$afterHolder = FileSha256 $sample
Write-Output "guest_written_sha256 name=$small style=mapped_held sha256=$afterHolder size=$(FileLength $sample)"
Check 'mapped_held_writer_completed' ($holderExited -and ($afterHolder -ne $before)) `
    "holder_exited=$holderExited before=$before after=$afterHolder"
Check 'mapped_held_content_kept' ((FileLength $sample) -eq $smallSize) "size=$(FileLength $sample)"

# --- editor scenario 4a: exclusive handle (FileShare.None) must block release ---------
Stage 'exclusive_lock'
Reset-Hydrated $Vault
$before = $script:lastResetHash
$holdCode = "`$f=[System.IO.File]::Open('$sample','Open','ReadWrite','None'); Start-Sleep -Seconds 6; `$f.Dispose()"
$holder = Start-Holder $holdCode
Start-Sleep -Milliseconds 1500
Write-Output "holder_running=$(-not $holder.HasExited)"
RunExe @('protect', '--vault', $Vault, '--name', $small)
$verdict = Field 'guard.decision' 'verdict'
$reason = Field 'guard.decision' 'reason'
Write-Output "guard=$verdict/$reason protect_exclusive"
Check 'exclusive_verdict_busy' (($verdict -eq 'busy') -and ($reason -eq 'handle_held_read')) `
    "verdict=$verdict reason=$reason"
Check 'exclusive_all_probes_conflict' `
    (((Field 'guard.decision' 'probe_read_exclusive') -eq 'error:32') -and `
        ((Field 'guard.decision' 'probe_write_exclusive') -eq 'error:32') -and `
        ((Field 'guard.decision' 'probe_shared_read') -eq 'error:32')) `
    "probes=$(Field 'guard.decision' 'probe_read_exclusive')/$(Field 'guard.decision' 'probe_write_exclusive')/$(Field 'guard.decision' 'probe_shared_read')"

RunExe @('release', '--vault', $Vault, '--name', $small, '--expect', 'refused')
$outcome = Field 'release.result' 'outcome'
Write-Output "guard=$(Field 'release.result' 'verdict')/$(Field 'release.result' 'reason') release_exclusive"
Check 'exclusive_release_refused' (($lastExit -eq 0) -and ($outcome -eq 'refused')) `
    "exit=$lastExit outcome=$outcome"
Check 'exclusive_no_platform_call' ((Field 'release.result' 'dehydrate_hresult') -eq '-') `
    "dehydrate_hresult=$(Field 'release.result' 'dehydrate_hresult')"
Check 'exclusive_not_dehydrated' ((Field 'release.result' 'on_disk_after') -eq "$smallSize") `
    "on_disk_after=$(Field 'release.result' 'on_disk_after')"
$holderExited = Wait-Holder $holder
$afterHolder = FileSha256 $sample
Check 'exclusive_content_preserved' ($holderExited -and ($afterHolder -eq $before)) `
    "holder_exited=$holderExited before=$before after=$afterHolder"

# Once the handle is gone the same content must be releasable: this proves the earlier refusal
# came from usage protection and not from some unrelated error.
Stage 'exclusive_released_after_holder_gone'
RunExe @('protect', '--vault', $Vault, '--name', $small)
Check 'after_holder_verdict_clean' ((Field 'guard.decision' 'verdict') -eq 'clean') `
    "verdict=$(Field 'guard.decision' 'verdict') reason=$(Field 'guard.decision' 'reason')"
RunExe @('release', '--vault', $Vault, '--name', $small, '--expect', 'released')
Check 'after_holder_release_ok' (($lastExit -eq 0) -and ((Field 'release.result' 'outcome') -eq 'released')) `
    "exit=$lastExit outcome=$(Field 'release.result' 'outcome') on_disk_after=$(Field 'release.result' 'on_disk_after')"

# --- editor scenario 4b: shared lock (FileAccess.Read + FileShare.Read) ---------------
Stage 'shared_read_lock'
Reset-Hydrated $Vault
$before = $script:lastResetHash
$holdCode = "`$f=[System.IO.File]::Open('$sample','Open','Read','Read'); Start-Sleep -Seconds 6; `$f.Dispose()"
$holder = Start-Holder $holdCode
Start-Sleep -Milliseconds 1500
Write-Output "holder_running=$(-not $holder.HasExited)"
RunExe @('protect', '--vault', $Vault, '--name', $small)
$verdict = Field 'guard.decision' 'verdict'
$reason = Field 'guard.decision' 'reason'
Write-Output "guard=$verdict/$reason protect_shared_read"
Check 'shared_read_verdict_busy' (($verdict -eq 'busy') -and ($reason -eq 'handle_held_read')) `
    "verdict=$verdict reason=$reason"
Check 'shared_read_probe_conflict' ((Field 'guard.decision' 'probe_read_exclusive') -eq 'error:32') `
    "probe_read_exclusive=$(Field 'guard.decision' 'probe_read_exclusive')"
RunExe @('release', '--vault', $Vault, '--name', $small, '--expect', 'refused')
$outcome = Field 'release.result' 'outcome'
Write-Output "guard=$(Field 'release.result' 'verdict')/$(Field 'release.result' 'reason') release_shared_read"
Check 'shared_read_release_refused' (($lastExit -eq 0) -and ($outcome -eq 'refused')) `
    "exit=$lastExit outcome=$outcome"
Check 'shared_read_not_dehydrated' ((Field 'release.result' 'on_disk_after') -eq "$smallSize") `
    "on_disk_after=$(Field 'release.result' 'on_disk_after')"
$holderExited = Wait-Holder $holder
$afterHolder = FileSha256 $sample
Check 'shared_read_content_preserved' ($holderExited -and ($afterHolder -eq $before)) `
    "holder_exited=$holderExited before=$before after=$afterHolder"

# --- editor scenario 5: rename-over save (temp file renamed onto the target) ----------
# Measured on this build: a temp file cannot be created inside the sync vault (new names fail
# with 0x801F0005 / "provider is not running"), so the temp file is created outside the vault on
# the same volume and renamed over the target. The rename itself reports success, but the target
# name can afterwards no longer be opened by path (0x801F0005), so its content and identity
# cannot be verified. This prototype therefore cannot prove rename-over saves safe: recorded as
# blocked with the observed error codes and the recovery steps.
Stage 'rename_over'
Reset-Vault $RenameVault
RunExe @('read', '--vault', $RenameVault, '--name', $small)
$renameSample = SamplePath $RenameVault
$renameBefore = FileSha256 $renameSample
Write-Output "guest_file_sha256 name=$small sha256=$renameBefore"
RunExe @('protect', '--vault', $RenameVault, '--name', $small)
Check 'rename_over_baseline_clean' ((Field 'guard.decision' 'verdict') -eq 'clean') `
    "verdict=$(Field 'guard.decision' 'verdict') reason=$(Field 'guard.decision' 'reason')"

# An editor normally writes its temp file next to the target. Measure whether that is possible
# inside a registered sync vault on this build before falling back to an out-of-vault temp file.
$insideTemp = Join-Path (Join-Path $RenameVault $prototypeDirName) 'rename-inside.tmp'
$insideError = ''
try {
    [System.IO.File]::WriteAllBytes($insideTemp, [System.Text.Encoding]::ASCII.GetBytes('inside-vault-temp'))
}
catch {
    $inner = $_.Exception.InnerException; if (-not $inner) { $inner = $_.Exception }
    $insideError = "0x$('{0:X8}' -f $inner.HResult)"
}
Write-Output "rename_over_inside_temp_error=$insideError"
Check 'rename_over_inside_temp_blocked' ($insideError -ne '') `
    "error=$insideError (temp file cannot be created inside the registered sync vault)"

$tempPath = Join-Path $Directory "$small.tmp"
[System.IO.File]::WriteAllBytes($tempPath, [System.Text.Encoding]::ASCII.GetBytes('rename-over-payload-s012-' * 40))
$tempHash = FileSha256 $tempPath
Write-Output "guest_written_sha256 name=$small style=rename_over_temp sha256=$tempHash size=$(FileLength $tempPath)"
$moveReported = ''
try {
    Move-Item -LiteralPath $tempPath -Destination $renameSample -Force -ErrorAction Stop
    $moveReported = 'ok'
}
catch {
    $inner = $_.Exception.InnerException; if (-not $inner) { $inner = $_.Exception }
    $moveReported = "error:0x$('{0:X8}' -f $inner.HResult)"
}
Write-Output "rename_over_move=$moveReported"
RunExe @('protect', '--vault', $RenameVault, '--name', $small)
$verdict = Field 'guard.decision' 'verdict'
$protectExit = $lastExit
$openError = ''
$targetOpens = $false
try {
    $probe = [System.IO.File]::Open($renameSample, 'Open', 'Read', 'ReadWrite')
    $targetOpens = $true
    $probe.Dispose()
}
catch {
    $inner = $_.Exception.InnerException; if (-not $inner) { $inner = $_.Exception }
    $openError = "0x$('{0:X8}' -f $inner.HResult)"
}
Write-Output "rename_over_target_opens=$targetOpens open_error=$openError protect_exit=$protectExit"
Blocked 'rename_over' "inside-vault temp blocked ($insideError), target name unopenable after rename (open_error=$openError), protect_exit=$protectExit; cannot prove the save safe"
Check 'rename_over_limitation_recorded' ((-not $targetOpens) -and ($protectExit -eq 1)) `
    "target_opens=$targetOpens open_error=$openError protect_exit=$protectExit"

# Recovery: unregister the sync vault first (cleanup keeps the invalid name and still unregisters),
# then delete the directory.
Stage 'rename_vault_recovery'
RunExe @('cleanup', '--vault', $RenameVault)
Check 'rename_vault_cleanup_exit' ($lastExit -eq 0) "exit=$lastExit"
Check 'rename_vault_cleanup_kept_leftover' `
    ((($script:lastOut -split "`r?`n") | Where-Object { $_ -like '*status=kept_not_empty*' } | Measure-Object).Count -ge 1) `
    "cleanup reported kept_not_empty and still unregistered"
RunExe @('inspect', '--vault', $RenameVault)
Check 'rename_vault_unregistered' ($lastExit -eq 3) "exit=$lastExit (3 = not registered)"
Remove-RootDir $RenameVault
Check 'rename_vault_removed' (-not (Test-Path -LiteralPath $RenameVault)) "path=$RenameVault"

# --- final cleanup: user content preserved, unregister verified ------------------------
Stage 'final_cleanup'
RunExe @('cleanup', '--vault', $Vault)
Check 'cleanup_exit' ($lastExit -eq 0) "exit=$lastExit"
Check 'cleanup_removed_probe_dir' (-not (Test-Path -LiteralPath (Join-Path $Vault $prototypeDirName))) `
    "path=$prototypeDirName"
$keepHashAfter = FileSha256 $keepPath
Check 'cleanup_preserved_user_content' ($keepHashAfter -eq $keepHashBefore) "before=$keepHashBefore after=$keepHashAfter"
RunExe @('inspect', '--vault', $Vault)
Check 'unregistered_after_cleanup' ($lastExit -eq 3) "exit=$lastExit (3 = not registered)"
RunExe @('cleanup', '--vault', $Vault)
Check 'cleanup_again_exit' ($lastExit -eq 0) "exit=$lastExit"

if ($failed -ne 0) {
    Write-Output 'cfapi_release_result: FAILED'
    exit 1
}
Write-Output 'cfapi_release_result: OK'
exit 0
