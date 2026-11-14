param([switch]$Worker)
$ErrorActionPreference = 'Stop'
if ($Worker) {
  $agent = $null
  $desktop = $null
  $workerExit = 1
  $rootsBefore = $null
  try {
    $session = (Get-Process -Id $PID).SessionId
    $existing = @(Get-Process vaultsyncd-agent,vaultsyncd-desktop -ErrorAction SilentlyContinue | Where-Object {$_.SessionId -eq $session})
    if ($existing.Count -ne 0) { throw 'preserve existing native client processes; close them before this isolated run' }
    $env:TEMP = Join-Path $PSScriptRoot 'tmp'; $env:TMP = $env:TEMP
    $env:LOCALAPPDATA = Join-Path $PSScriptRoot 'local'
    New-Item -ItemType Directory -Path $env:TEMP,$env:LOCALAPPDATA | Out-Null
    $null = [Windows.Storage.Provider.StorageProviderSyncRootManager,Windows.Storage.Provider,ContentType=WindowsRuntime]
    $rootsBefore = @([Windows.Storage.Provider.StorageProviderSyncRootManager]::GetCurrentSyncRoots() | ForEach-Object {$_.Id} | Sort-Object)
    $rootsBefore | ConvertTo-Json | Set-Content (Join-Path $PSScriptRoot 'prior-sync-roots.json') -Encoding UTF8
    $fixture = Get-Content (Join-Path $PSScriptRoot 'fixture.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    $state = Join-Path $PSScriptRoot 'state'
    $local = Join-Path $PSScriptRoot '同步目录'
    $empty = Join-Path $PSScriptRoot '空目录'
    New-Item -ItemType Directory -Path $state,$local,$empty | Out-Null
    [IO.File]::WriteAllText((Join-Path $local 'local-only.txt'), 'S172-native-upload', (New-Object Text.UTF8Encoding($false)))
    $fixture | Add-Member -NotePropertyName hostname -NotePropertyValue ([System.Net.Dns]::GetHostName())
    $fixture | Add-Member -NotePropertyName localPath -NotePropertyValue $local
    $fixture | Add-Member -NotePropertyName emptyPath -NotePropertyValue $empty
    $fixture | Add-Member -NotePropertyName caFile -NotePropertyValue (Join-Path $PSScriptRoot 'ca.pem')
    [IO.File]::WriteAllText((Join-Path $PSScriptRoot 'fixture-runtime.json'), ($fixture | ConvertTo-Json -Depth 8), (New-Object Text.UTF8Encoding($false)))
    $config = @{server=@{id=$fixture.serverId;url=$fixture.url};device_id=[Guid]::NewGuid().ToString('N');state_dir=$state;poll_millis=250;bindings=@()}
    [IO.File]::WriteAllText((Join-Path $PSScriptRoot 'agent.json'), ($config | ConvertTo-Json -Depth 5), (New-Object Text.UTF8Encoding($false)))
    $profile = Join-Path $env:LOCALAPPDATA 'vaultsyncd/agent'
    New-Item -ItemType Directory -Path $profile -Force | Out-Null
    Copy-Item (Join-Path $PSScriptRoot 'agent.json') (Join-Path $profile 'agent.json')
    $agent = Start-Process -FilePath (Join-Path $PSScriptRoot 'vaultsyncd-agent.exe') -ArgumentList @('--config', (Join-Path $PSScriptRoot 'agent.json')) -PassThru -RedirectStandardError (Join-Path $PSScriptRoot 'agent.log')
    Start-Sleep -Seconds 3
    if ($agent.HasExited) { throw 'agent failed before activity test' }
    $env:VAULTSYNCD_S080_REPORT = Join-Path $PSScriptRoot 'webview-report.json'
    $env:VAULTSYNCD_S172_FIXTURE = Join-Path $PSScriptRoot 'fixture-runtime.json'
    $desktop = Start-Process -FilePath (Join-Path $PSScriptRoot 'vaultsyncd-desktop.exe') -PassThru -RedirectStandardError (Join-Path $PSScriptRoot 'desktop.log')
    if (!$desktop.WaitForExit(360000)) { throw 'real activity WebView timeout' }
    $report = Get-Content $env:VAULTSYNCD_S080_REPORT -Raw -Encoding UTF8 | ConvertFrom-Json
    if (!$report.ok) { throw ('activity acceptance failed: '+($report | ConvertTo-Json -Compress -Depth 8)) }
    if ($agent.HasExited) { throw 'closing activity GUI stopped agent' }
    if ((Get-Content (Join-Path $local 'server-seed.txt') -Raw) -ne 'S172-native-server-seed') { throw 'native downloaded bytes mismatch' }
    $report | Add-Member -NotePropertyName guiExitKeepsAgent -NotePropertyValue $true
    $report | Add-Member -NotePropertyName windowsVersion -NotePropertyValue ([Environment]::OSVersion.VersionString)
    $report | ConvertTo-Json -Compress -Depth 8 | Set-Content (Join-Path $PSScriptRoot 'result.json') -Encoding UTF8
    $workerExit = 0
  } catch {
    $_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'error.txt')
    $workerExit = 1
  } finally {
    if ($desktop -and !$desktop.HasExited) { Stop-Process -Id $desktop.Id }
    if ($agent -and !$agent.HasExited) {
      try {
        $stop = Start-Process (Join-Path $PSScriptRoot 'vaultsyncd-agent.exe') -ArgumentList @('--user-session','shutdown') -PassThru -RedirectStandardOutput (Join-Path $PSScriptRoot 'agent-shutdown.log') -RedirectStandardError (Join-Path $PSScriptRoot 'agent-shutdown.err')
        $null = $stop.Handle
        if (!$stop.WaitForExit(90000)) { throw 'orderly fixture shutdown timed out; do not force stop' }
        $stop.WaitForExit()
        if ($stop.ExitCode -ne 0 -or !$agent.WaitForExit(20000)) { throw 'orderly fixture shutdown failed; preserve process and state' }
      } catch {
        $_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'cleanup-error.txt')
        $workerExit = 1
      }
    }
    if ($agent -and $agent.HasExited -and $local -and $empty) {
      try {
        $env:VAULTSYNCD_ACTIVITY_CLEANUP_ROOT = $PSScriptRoot
        & (Join-Path $PSScriptRoot 'activity-cleanup.exe') --ignored --exact cleanup_only_fully_materialized_activity_fixture --nocapture 2>&1 | Set-Content (Join-Path $PSScriptRoot 'provider-cleanup.log') -Encoding UTF8
        if ($LASTEXITCODE -ne 0) { throw 'fixture provider cleanup refused; preserve its registrations and files' }
        $paths = @($local,$empty)
        $owned = @([Windows.Storage.Provider.StorageProviderSyncRootManager]::GetCurrentSyncRoots() | Where-Object {$_.Path.Path -in $paths})
        foreach ($root in $owned) { [Windows.Storage.Provider.StorageProviderSyncRootManager]::Unregister($root.Id) }
        $rootsAfter = @([Windows.Storage.Provider.StorageProviderSyncRootManager]::GetCurrentSyncRoots() | ForEach-Object {$_.Id} | Sort-Object)
        $rootsAfter | ConvertTo-Json | Set-Content (Join-Path $PSScriptRoot 'final-sync-roots.json') -Encoding UTF8
        if ($rootsBefore.Count -ne $rootsAfter.Count -or ($rootsBefore.Count -gt 0 -and @(Compare-Object -ReferenceObject $rootsBefore -DifferenceObject $rootsAfter -CaseSensitive).Count -ne 0)) { throw 'unexpected registration change; preserve evidence and do not remove other roots' }
      } catch {
        $_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'cleanup-error.txt')
        $workerExit = 1
      }
    }
  }
  $workerExit | Set-Content (Join-Path $PSScriptRoot 'exit.txt')
  exit
}
$hashes = Get-Content (Join-Path $PSScriptRoot 'hashes.json') -Raw | ConvertFrom-Json
foreach ($item in $hashes.PSObject.Properties) {
  if ((Get-FileHash (Join-Path $PSScriptRoot $item.Name) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $item.Value) { throw 'native binary transport hash mismatch' }
}
$task = 'vaultsyncd-s172-'+[Guid]::NewGuid().ToString('N')
$action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker"
$principal = New-ScheduledTaskPrincipal -UserId 'Quickemu' -LogonType Interactive -RunLevel Limited
try {
  Register-ScheduledTask -TaskName $task -Action $action -Principal $principal | Out-Null
  Start-ScheduledTask -TaskName $task
  $end = (Get-Date).AddSeconds(540)
  while (!(Test-Path (Join-Path $PSScriptRoot 'exit.txt'))) {
    if ((Get-Date) -ge $end) { throw 'interactive native activity worker timeout' }
    Start-Sleep -Milliseconds 200
  }
  if ((Get-Content (Join-Path $PSScriptRoot 'exit.txt')) -ne '0') {
    Get-Content (Join-Path $PSScriptRoot 'error.txt')
    exit 1
  }
  Get-Content (Join-Path $PSScriptRoot 'result.json')
} finally { Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue }
