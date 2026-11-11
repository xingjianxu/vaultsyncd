param([switch]$Worker)
$ErrorActionPreference = 'Stop'
$OutputEncoding = New-Object Text.UTF8Encoding($false)
[Console]::OutputEncoding = $OutputEncoding

function Assert([bool]$condition, [string]$message) { if (!$condition) { throw $message } }
function Write-Text([string]$path, [string]$text) {
  [IO.File]::WriteAllText($path, $text, (New-Object Text.UTF8Encoding($false)))
}
function Hash([string]$path) { (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() }
function Wait-Until([string]$description, [scriptblock]$test, [int]$seconds = 90) {
  $end = (Get-Date).AddSeconds($seconds)
  do {
    $answer = & $test
    if ($answer) { return $answer }
    Start-Sleep -Milliseconds 250
  } while ((Get-Date) -lt $end)
  throw "Timeout: $description"
}

if ($Worker) {
  $agent = $null; $fixture = $null; $root = $null; $ownedBindings = @(); $workerExit = 1
  $cli = Join-Path $PSScriptRoot 'vaultsyncd-cli.exe'
  $agentExe = Join-Path $PSScriptRoot 'vaultsyncd-agent.exe'
  $progress = Join-Path $PSScriptRoot 'progress.log'
  function Mark([string]$phase) { Add-Content -LiteralPath $progress -Value $phase }
  function Invoke-Cli([string[]]$arguments, [int]$expected = 0, [string]$reason = '') {
    $text = & $cli --json @arguments
    $exit = $LASTEXITCODE
    $raw = $text -join "`n"
    Add-Content -LiteralPath (Join-Path $PSScriptRoot 'cli.log') -Value "$($arguments -join ' ') exit=$exit $raw"
    $answer = $raw | ConvertFrom-Json
    Assert ($exit -eq $expected -and $answer.ok -eq ($expected -eq 0)) "CLI $($arguments[0]) exit $exit, expected $expected ($($answer.error.message))"
    if ($reason) { Assert ($answer.error.message -match [regex]::Escape($reason)) "CLI $($arguments[0]) did not report $reason ($($answer.error.message))" }
    return $answer.result
  }
  function Start-Agent([string]$phase) {
    $process = Start-Process -FilePath $agentExe -ArgumentList @('--config', ('"'+$configPath+'"')) -PassThru `
      -RedirectStandardError (Join-Path $PSScriptRoot "agent-$phase.log")
    $null = $process.Handle # Keep the native process handle so ExitCode remains observable.
    return $process
  }
  function Wait-Agent($process) {
    Wait-Until 'run-owned IPC' {
      if ($process.HasExited) { throw "run-owned agent exited $($process.ExitCode) before readiness" }
      $text = & $cli --json status
      if ($LASTEXITCODE -eq 0) { return (($text -join "`n") | ConvertFrom-Json).result }
    }
  }
  function Stop-Agent($process) {
    Assert (!$process.HasExited) 'agent exited before orderly stop'
    $null = & $agentExe --user-session stop
    Assert ($LASTEXITCODE -eq 0) 'orderly agent stop command failed'
    Assert ($process.WaitForExit(20000)) 'agent failed to exit after orderly stop'
    Assert ($process.ExitCode -eq 0) "agent orderly stop exit $($process.ExitCode)"
  }
  function Confirmed-File([string]$binding, [string]$path) {
    Wait-Until "confirmed $binding/$path" {
      $text = & $cli --json files --binding-id $binding
      $exit = $LASTEXITCODE
      $answer = ($text -join "`n") | ConvertFrom-Json
      if ($exit -ne 0 -and $answer.error.message -match 'BINDING_BLOCKED|NOT_READY') { return }
      Assert ($exit -eq 0 -and $answer.ok) "file status failed ($($answer.error.message))"
      $file = @($answer.result.management.files | Where-Object { $_.path -eq $path })
      if ($file.Count -eq 1 -and $file[0].entry_id -and $file[0].synchronized) { return $file[0] }
    }
  }
  function Create-Binding([string]$binding, [string]$path, [string]$cache, [int]$expected = 0, [string]$reason = '') {
    Invoke-Cli @('create-binding','--binding-id',$binding,'--server-id',$fixture.serverId,
      '--vault-id',$fixture.vault,'--local-path',$path,'--cache-path',$cache,'--mode','full') $expected $reason
  }
  function Assert-OldBlocked([string]$binding, [string]$entry) {
    $null = Invoke-Cli @('resume','--binding-id',$binding) 5 'BINDING_UNBOUND'
    $null = Invoke-Cli @('retry','--binding-id',$binding,'--operation-id',([Guid]::NewGuid().ToString('N'))) 5 'BINDING_BLOCKED'
    $null = Invoke-Cli @('restore','--binding-id',$binding,'--entry-id',$entry,
      '--operation-id',([Guid]::NewGuid().ToString('N')),'--base-revision','1',
      '--version-id',([Guid]::NewGuid().ToString('N'))) 5 'BINDING_BLOCKED'
  }
  function Assert-ActiveOverlap {
    $child = Join-Path $root 'child'
    if (!(Test-Path -LiteralPath $child)) { New-Item -ItemType Directory -Path $child | Out-Null }
    foreach ($path in @($root, (Split-Path $root -Parent))) {
      $null = Create-Binding ([Guid]::NewGuid().ToString('N')) $path $probeCache 5 'PATH_OVERLAP'
    }
    # The active provider may already have converted a child directory into a cloud
    # placeholder. Root admission can reject that native reparse before ancestry checking.
    $null = Create-Binding ([Guid]::NewGuid().ToString('N')) $child $probeCache 5
    $last = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'cli.log') -Tail 1
    Assert ($last -match 'PATH_OVERLAP|LOCAL_PATH_UNAVAILABLE') 'child rejection was not physical admission'

  }
  function Cache-Receipt([string]$path) {
    $receipt = @{}
    foreach ($file in Get-ChildItem -LiteralPath $path -File -Recurse -Force) {
      $receipt[$file.FullName.Substring($path.Length)] = Hash $file.FullName
    }
    Assert ($receipt.Count -gt 0) 'old cache receipt must not be empty'
    return $receipt
  }
  function Assert-Cache([string]$path, $receipt) {
    foreach ($name in $receipt.Keys) {
      $file = $path + $name
      Assert ((Test-Path -LiteralPath $file -PathType Leaf) -and (Hash $file) -eq $receipt[$name]) "old cache changed or vanished: $file"
    }
  }

  try {
    Assert (@(Get-Process vaultsyncd-agent,vaultsyncd-desktop -ErrorAction SilentlyContinue).Count -eq 0) 'preserve existing product processes'
    Assert ((Get-Volume -DriveLetter C).FileSystem -eq 'NTFS') 'guest-local NTFS is required'
    Assert ([Environment]::UserInteractive) 'interactive Windows session is required'
    $fixturePath = Join-Path $PSScriptRoot 'fixture.json'
    $fixture = Get-Content -LiteralPath $fixturePath -Raw -Encoding UTF8 | ConvertFrom-Json
    Remove-Item -LiteralPath $fixturePath -Force
    $env:LOCALAPPDATA = Join-Path $PSScriptRoot 'local'
    $env:APPDATA = Join-Path $PSScriptRoot 'roaming'
    $env:TEMP = Join-Path $PSScriptRoot 'temp'; $env:TMP = $env:TEMP
    $profile = Join-Path $env:LOCALAPPDATA 'vaultsyncd\agent'
    $statePath = Join-Path $profile 'state'
    $configPath = Join-Path $profile 'agent.json'
    $root = Join-Path $PSScriptRoot 'data\root'
    $probeRoot = Join-Path $PSScriptRoot 'probe-root'
    $probeCache = Join-Path $PSScriptRoot 'probe-cache'
    $caches = @(1..3 | ForEach-Object { Join-Path $PSScriptRoot "cache-$_" })
    New-Item -ItemType Directory -Path (@($statePath,$env:APPDATA,$env:TEMP,$root,$probeRoot,$probeCache)+$caches) | Out-Null
    $config = @{server=@{id=[Guid]::NewGuid().ToString('N');url='https://localhost'};
      device_id=[Guid]::NewGuid().ToString('N');state_dir=$statePath;poll_millis=250;bindings=@()}
    Write-Text $configPath ($config | ConvertTo-Json -Depth 5)
    Write-Text (Join-Path $root 'local-before.txt') 'S195-local-original'
    Mark 'start-login'
    $agent = Start-Agent 'initial'
    $status = Wait-Agent $agent
    Assert (@($status.management.bindings).Count -eq 0) 'fresh profile contained bindings'
    $null = Invoke-Cli @('set-connection','--server-id',$fixture.serverId,'--server-url',$fixture.url,'--ca-file',(Join-Path $PSScriptRoot 'ca.pem'))
    $line = $fixture.password | & $cli --json login --server-id $fixture.serverId --username $fixture.login
    Assert ($LASTEXITCODE -eq 0 -and ($line | ConvertFrom-Json).result.management.connection.authenticated) 'isolated HTTPS login failed'
    $fixture.password = $null

    Mark 'first-generation-download'
    $first = [Guid]::NewGuid().ToString('N'); $ownedBindings += $first
    $null = Create-Binding $first $root $caches[0]
    $seed = Confirmed-File $first 'server-seed.txt'
    $null = Confirmed-File $first 'local-before.txt'
    Assert ([IO.File]::ReadAllText((Join-Path $root 'server-seed.txt')) -ceq 'S195-server-original') 'first binding downloaded wrong bytes'
    Assert ($seed.residency -eq 'materialized') 'full binding did not completely download seed'
    Assert ([bool]((Get-Item -LiteralPath (Join-Path $root 'server-seed.txt')).Attributes -band [IO.FileAttributes]::ReparsePoint)) 'seed was not a native CFAPI placeholder'
    $seedHash = Hash (Join-Path $root 'server-seed.txt')
    $localHash = Hash (Join-Path $root 'local-before.txt')
    Assert-ActiveOverlap
    Write-Text (Join-Path $caches[0] 'retained-cache-sentinel.txt') 'S195-first-cache-retained'
    $null = Invoke-Cli @('detach','--binding-id',$first)
    $firstCache = Cache-Receipt $caches[0]
    Assert-OldBlocked $first $seed.entry_id

    Mark 'second-generation-reuse'
    $second = [Guid]::NewGuid().ToString('N'); $ownedBindings += $second
    $null = Create-Binding $second $root $caches[1]
    $secondSeed = Confirmed-File $second 'server-seed.txt'
    $null = Confirmed-File $second 'local-before.txt'
    Assert ((Hash (Join-Path $root 'server-seed.txt')) -eq $seedHash) 'seed bytes changed during handover'
    Assert ((Hash (Join-Path $root 'local-before.txt')) -eq $localHash) 'local bytes changed during handover'
    Assert-ActiveOverlap
    foreach ($protected in @($caches[0],$statePath)) {
      $null = Create-Binding ([Guid]::NewGuid().ToString('N')) $protected $probeCache 5 'PATH_OVERLAP'
      $null = Create-Binding ([Guid]::NewGuid().ToString('N')) $probeRoot $protected 5 'PATH_OVERLAP'
    }
    Write-Text (Join-Path $root 'upload-generation2.txt') 'S195-second-generation'
    $null = Confirmed-File $second 'upload-generation2.txt'
    Assert-Cache $caches[0] $firstCache

    Mark 'orderly-stop-and-restart'
    Stop-Agent $agent
    $agent = Start-Agent 'restarted'
    $null = Wait-Agent $agent
    $rows = @((Invoke-Cli @('bindings')).management.bindings)
    Assert ($rows.Count -eq 2) 'restart lost a durable binding row'
    $oldRow = @($rows | Where-Object { $_.binding_id -eq $first })
    $newRow = @($rows | Where-Object { $_.binding_id -eq $second })
    Assert ($oldRow.Count -eq 1 -and $oldRow[0].detached -and $oldRow[0].paused) 'restart revived or discarded detached binding'
    Assert ($newRow.Count -eq 1 -and !$newRow[0].detached -and $newRow[0].local_path -eq $root -and $newRow[0].cache_path -eq $caches[1]) 'restart changed current binding'
    Assert-OldBlocked $first $seed.entry_id
    $null = Invoke-Cli @('resume','--binding-id',$second)
    $null = Confirmed-File $second 'server-seed.txt'
    Write-Text (Join-Path $root 'after-restart.txt') 'S195-after-restart'
    $null = Confirmed-File $second 'after-restart.txt'

    Mark 'third-generation-reuse'
    Write-Text (Join-Path $caches[1] 'retained-cache-sentinel.txt') 'S195-second-cache-retained'
    $null = Invoke-Cli @('detach','--binding-id',$second)
    $secondCache = Cache-Receipt $caches[1]
    Assert-OldBlocked $second $secondSeed.entry_id
    $third = [Guid]::NewGuid().ToString('N'); $ownedBindings += $third
    $null = Create-Binding $third $root $caches[2]
    foreach ($name in @('server-seed.txt','local-before.txt','upload-generation2.txt','after-restart.txt')) { $null = Confirmed-File $third $name }
    Write-Text (Join-Path $root 'upload-generation3.txt') 'S195-third-generation'
    $null = Confirmed-File $third 'upload-generation3.txt'
    Assert ((Hash (Join-Path $root 'server-seed.txt')) -eq $seedHash) 'third generation changed original seed bytes'
    Assert ((Hash (Join-Path $root 'local-before.txt')) -eq $localHash) 'third generation changed original local bytes'
    Assert-Cache $caches[0] $firstCache
    Assert-Cache $caches[1] $secondCache
    $rows = @((Invoke-Cli @('bindings')).management.bindings)
    Assert ($rows.Count -eq 3 -and @($rows | Where-Object { $_.detached }).Count -eq 2) 'third generation lost old detached rows'
    Write-Text (Join-Path $PSScriptRoot 'binding-rows.json') ($rows | ConvertTo-Json -Depth 8)
    $null = Invoke-Cli @('logout','--server-id',$fixture.serverId)
    Stop-Agent $agent
    $agent = $null
    @{ok=$true;bindings=@($first,$second,$third);seedHash=$seedHash;localHash=$localHash;
      firstCache=$firstCache;secondCache=$secondCache;restart=$true;cfapi=$true;
      windowsVersion=[Environment]::OSVersion.VersionString} | ConvertTo-Json -Depth 12 |
      Set-Content -LiteralPath (Join-Path $PSScriptRoot 'result.json') -Encoding UTF8
    $workerExit = 0
  } catch {
    $_ | Out-String | Set-Content -LiteralPath (Join-Path $PSScriptRoot 'error.txt')
    if ($agent -and !$agent.HasExited) {
      foreach ($command in @('status','bindings','problems')) {
        try {
          $diagnostic = & $cli --json $command
          $diagnosticExit = $LASTEXITCODE
          Write-Text (Join-Path $PSScriptRoot "failure-$command.json") ($diagnostic -join "`n")
          Add-Content -LiteralPath (Join-Path $PSScriptRoot 'diagnostics.log') -Value "$command exit=$diagnosticExit"
        } catch { $_ | Out-String | Add-Content -LiteralPath (Join-Path $PSScriptRoot 'diagnostics.log') }
      }
      if ($ownedBindings.Count -gt 0) {
        try {
          $diagnostic = & $cli --json files --binding-id $ownedBindings[-1]
          $diagnosticExit = $LASTEXITCODE
          Write-Text (Join-Path $PSScriptRoot 'failure-files.json') ($diagnostic -join "`n")
          Add-Content -LiteralPath (Join-Path $PSScriptRoot 'diagnostics.log') -Value "files exit=$diagnosticExit"
        } catch { $_ | Out-String | Add-Content -LiteralPath (Join-Path $PSScriptRoot 'diagnostics.log') }
      }
    }
  } finally {
    $cleanupErrors = Join-Path $PSScriptRoot 'cleanup-errors.log'
    if ($agent -and !$agent.HasExited) {
      try { Stop-Agent $agent }
      catch {
        $_ | Out-String | Add-Content -LiteralPath $cleanupErrors
        $workerExit = 1
        if (!$agent.HasExited) { Stop-Process -Id $agent.Id }
      }
    }
    try {
      if ($root -and (Test-Path -LiteralPath $root)) {
        # Test-only native cleanup, after this run's writer has stopped. Refuse incomplete
        # placeholders and preserve hashes; never unregister a root containing partial data.
        Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.ComponentModel;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class S195Cleanup {
  [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
  static extern SafeFileHandle CreateFileW(string p,uint a,uint s,IntPtr x,uint d,uint f,IntPtr t);
  [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetFileSizeEx(SafeFileHandle h,out long size);
  [DllImport("cldapi.dll")] static extern int CfGetPlaceholderInfo(SafeFileHandle h,int c,byte[] b,uint n,out uint used);
  [DllImport("cldapi.dll")] static extern int CfRevertPlaceholder(SafeFileHandle h,uint flags,IntPtr ov);
  [DllImport("cldapi.dll", CharSet=CharSet.Unicode)] public static extern int CfUnregisterSyncRoot(string p);
  public static void Revert(string path,bool directory) {
    using (var handle=CreateFileW(path,0xC0000000,0,IntPtr.Zero,3,0x02200000,IntPtr.Zero)) {
      if(handle.IsInvalid) throw new Win32Exception(Marshal.GetLastWin32Error());
      byte[] info=new byte[8192]; uint used;
      int hr=CfGetPlaceholderInfo(handle,1,info,(uint)info.Length,out used);
      if(hr==unchecked((int)0x80070178)) {
        if((File.GetAttributes(path)&FileAttributes.ReparsePoint)!=0) throw new IOException("unknown reparse point");
        return;
      }
      if(hr<0) Marshal.ThrowExceptionForHR(hr);
      if(!directory) {
        long size;
        if(!GetFileSizeEx(handle,out size)) throw new Win32Exception(Marshal.GetLastWin32Error());
        if(used<64 || BitConverter.ToInt64(info,0)<size) throw new IOException("partial placeholder retained: "+path);
      }
      hr=CfRevertPlaceholder(handle,0,IntPtr.Zero);
      if(hr<0) Marshal.ThrowExceptionForHR(hr);
    }
  }
}
'@
        $canonical = [IO.Path]::GetFullPath($root)
        Assert ($canonical -eq (Join-Path $PSScriptRoot 'data\root') -and (Split-Path $PSScriptRoot -Leaf) -cmatch '^S195-[0-9]+-[0-9]+$') 'refuse cleanup outside exact run root'
        $preserved = @{}
        foreach ($file in Get-ChildItem -LiteralPath $root -File -Recurse -Force) { $preserved[$file.FullName] = Hash $file.FullName }
        $entries = @(Get-ChildItem -LiteralPath $root -Recurse -Force | Sort-Object { $_.FullName.Length } -Descending)
        foreach ($item in $entries) { [S195Cleanup]::Revert($item.FullName, $item.PSIsContainer) }
        foreach ($path in $preserved.Keys) { Assert ((Hash $path) -eq $preserved[$path]) "cleanup changed bytes: $path" }
        $hr = [S195Cleanup]::CfUnregisterSyncRoot($root)
        Assert ($hr -ge 0 -or $hr -eq [int]-2147024506) ('unregister HRESULT '+$hr)
        $null = [Windows.Storage.Provider.StorageProviderSyncRootManager,Windows.Storage.Provider,ContentType=WindowsRuntime]
        $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
        foreach ($binding in $ownedBindings) {
          $registration = "vaultsyncd!$sid!$binding"
          $matches = @([Windows.Storage.Provider.StorageProviderSyncRootManager]::GetCurrentSyncRoots() | Where-Object { $_.Id -eq $registration })
          if ($matches.Count -gt 0) { [Windows.Storage.Provider.StorageProviderSyncRootManager]::Unregister($registration) }
        }
        Write-Text (Join-Path $PSScriptRoot 'cleanup.json') (@{preserved=$preserved;bindings=$ownedBindings} | ConvertTo-Json -Depth 8)
      }
    } catch {
      $_ | Out-String | Add-Content -LiteralPath $cleanupErrors
      $workerExit = 1
    }
    if ($fixture -and $fixture.serverId -cmatch '^[0-9a-f]{32}$') { $null = & cmdkey.exe "/delete:vaultsyncd/session/$($fixture.serverId)" }
    "$workerExit" | Set-Content -LiteralPath (Join-Path $PSScriptRoot 'exit.txt')
  }
  exit $workerExit
}

$hashes = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'hashes.json') -Raw | ConvertFrom-Json
foreach ($item in $hashes.PSObject.Properties) {
  Assert ((Hash (Join-Path $PSScriptRoot $item.Name)) -eq $item.Value) "transport hash mismatch: $($item.Name)"
}
$task = 'vaultsyncd-s195-'+[Guid]::NewGuid().ToString('N')
$action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker"
$principal = New-ScheduledTaskPrincipal -UserId ([Security.Principal.WindowsIdentity]::GetCurrent().Name) -LogonType Interactive -RunLevel Limited
try {
  Register-ScheduledTask -TaskName $task -Action $action -Principal $principal | Out-Null
  Start-ScheduledTask -TaskName $task
  $null = Wait-Until 'interactive S195 acceptance' { Test-Path (Join-Path $PSScriptRoot 'exit.txt') } 900
  Get-Content -LiteralPath (Join-Path $PSScriptRoot 'progress.log') -ErrorAction SilentlyContinue
  $workerExit = [int](Get-Content -LiteralPath (Join-Path $PSScriptRoot 'exit.txt'))
  if ($workerExit -ne 0) {
    Get-Content -LiteralPath (Join-Path $PSScriptRoot 'error.txt') -ErrorAction SilentlyContinue
    Get-Content -LiteralPath (Join-Path $PSScriptRoot 'cleanup-errors.log') -ErrorAction SilentlyContinue
    exit $workerExit
  }
  $result = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'result.json') -Raw -Encoding UTF8 | ConvertFrom-Json
  Assert ($result.ok) 'worker did not produce a passing receipt'
  $result | ConvertTo-Json -Depth 12
} finally {
  Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
}
