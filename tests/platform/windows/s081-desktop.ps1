param([switch]$Worker)
$ErrorActionPreference = 'Stop'
if ($Worker) {
  $agent = $null; $desktop = $null; $picker = $null; $explorer = $null; $workerExit = 1
  try {
    $fixture = Get-Content (Join-Path $PSScriptRoot 'fixture.json') -Raw | ConvertFrom-Json
    $state = Join-Path $PSScriptRoot 'state'
    $local = Join-Path $PSScriptRoot '同步 目录'
    New-Item -ItemType Directory -Path $state,$local | Out-Null
    [IO.File]::WriteAllText((Join-Path $local 'local-only.txt'), 'S081-local-before-import', (New-Object Text.UTF8Encoding($false)))
    $fixture | Add-Member -NotePropertyName localPath -NotePropertyValue $local
    $fixture | Add-Member -NotePropertyName caFile -NotePropertyValue (Join-Path $PSScriptRoot 'ca.pem')
    [IO.File]::WriteAllText((Join-Path $PSScriptRoot 'fixture-runtime.json'), ($fixture | ConvertTo-Json -Depth 8), (New-Object Text.UTF8Encoding($false)))
    $config = @{server=@{id=$fixture.serverId;url=$fixture.url};device_id=[Guid]::NewGuid().ToString('N');state_dir=$state;poll_millis=250;bindings=@()}
    [IO.File]::WriteAllText((Join-Path $PSScriptRoot 'agent.json'), ($config | ConvertTo-Json -Depth 5), (New-Object Text.UTF8Encoding($false)))
    $agent = Start-Process -FilePath (Join-Path $PSScriptRoot 'vaultsyncd-agent.exe') -ArgumentList @('--config', (Join-Path $PSScriptRoot 'agent.json')) -PassThru -RedirectStandardError (Join-Path $PSScriptRoot 'agent.log')
    Start-Sleep -Seconds 3
    if ($agent.HasExited) { throw 'agent failed before GUI test' }
    $env:VAULTSYNCD_S080_REPORT = Join-Path $PSScriptRoot 'webview-report.json'
    $env:VAULTSYNCD_S081_FIXTURE = Join-Path $PSScriptRoot 'fixture-runtime.json'
    $picker = Start-Process powershell.exe -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File', (Join-Path $PSScriptRoot 's114-picker.ps1'), '-Path', ('"'+$local+'"')) -PassThru -RedirectStandardError (Join-Path $PSScriptRoot 'picker-error.log')
    # Native evidence, separate from the WebView's bridge acknowledgment: observe
    # a real Explorer window opened on this run's exact managed directory.
    $explorer = Start-Job -ArgumentList $local -ScriptBlock {
      param($expectedPath)
      $ErrorActionPreference = 'Stop'
      $shell = New-Object -ComObject Shell.Application
      $existing = @($shell.Windows() | ForEach-Object { $_.HWND })
      foreach ($window in @($shell.Windows())) {
        try { $initialPath = $window.Document.Folder.Self.Path } catch { continue }
        if ([String]::Equals($initialPath, $expectedPath, [StringComparison]::OrdinalIgnoreCase)) { throw 'test directory was already open before the directory action' }
      }
      $deadline = (Get-Date).AddSeconds(300)
      while ((Get-Date) -lt $deadline) {
        foreach ($window in @($shell.Windows())) {
          try { $actualPath = $window.Document.Folder.Self.Path } catch { continue }
          if (![String]::Equals($actualPath, $expectedPath, [StringComparison]::OrdinalIgnoreCase)) { continue }
          $evidence = [pscustomobject]@{ path = $actualPath; hwnd = $window.HWND; executable = $window.FullName; observed = $true; newWindow = ($existing -notcontains $window.HWND) }
          if ([IO.Path]::GetFileName($evidence.executable) -ine 'explorer.exe') { throw 'directory window is not Windows Explorer' }
          # Windows may reuse an existing window/tab. Never close such a window.
          if ($evidence.newWindow) { $window.Quit() }
          return $evidence
        }
        Start-Sleep -Milliseconds 150
      }
      throw 'no Explorer window observed for the managed directory'
    }
    $desktop = Start-Process -FilePath (Join-Path $PSScriptRoot 'vaultsyncd-desktop.exe') -PassThru -RedirectStandardError (Join-Path $PSScriptRoot 'desktop.log')
    if (!$desktop.WaitForExit(300000)) { throw 'S081 real WebView acceptance timeout' }
    $report = Get-Content $env:VAULTSYNCD_S080_REPORT -Raw -Encoding UTF8 | ConvertFrom-Json
    if (!$report.ok) { throw ('WebView acceptance failed: '+($report | ConvertTo-Json -Compress -Depth 8)) }
    if (!$picker.WaitForExit(10000)) { throw 'native picker automation timeout' }
    $picked = Get-Content (Join-Path $PSScriptRoot 'picker-result.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($picked.selected -ne $local -or !$picked.cancelled) { throw 'native picker automation failed' }
    if (!(Wait-Job $explorer -Timeout 10)) { throw 'Explorer observation did not finish' }
    $opened = Receive-Job $explorer -ErrorAction Stop
    if (!$opened.observed -or $opened.path -ine $local -or !$report.directoryOpenAcknowledged) { throw 'native binding directory open was not verified' }
    $opened | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $PSScriptRoot 'explorer-window.json') -Encoding UTF8
    $report | Add-Member -NotePropertyName explorerWindowObserved -NotePropertyValue $true
    if ($agent.HasExited) { throw 'closing GUI stopped agent' }
    $report | Add-Member -NotePropertyName guiExitKeepsAgent -NotePropertyValue $true
    # Actual downloaded bytes after first merge, and new upload after the GUI has exited.
    $seed = Join-Path $local 'server-seed.txt'
    if ((Get-Content $seed -Raw) -ne 'S081-server-before-import') { throw 'server seed did not materialize' }
    [IO.File]::WriteAllText((Join-Path $local 'after-gui.txt'), 'S081-agent-after-gui-exit', (New-Object Text.UTF8Encoding($false)))
    $report | Add-Member -NotePropertyName seedHash -NotePropertyValue (Get-FileHash $seed -Algorithm SHA256).Hash.ToLowerInvariant()
    $report | Add-Member -NotePropertyName localHash -NotePropertyValue (Get-FileHash (Join-Path $local 'local-only.txt') -Algorithm SHA256).Hash.ToLowerInvariant()
    Start-Sleep -Seconds 8
    $report | ConvertTo-Json -Compress -Depth 8 | Set-Content (Join-Path $PSScriptRoot 'result.json') -Encoding UTF8
    $workerExit = 0
  } catch {
    $_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'error.txt')
    $workerExit = 1
  } finally {
    if ($explorer) { Stop-Job $explorer; Remove-Job $explorer }
    if ($picker -and !$picker.HasExited) { Stop-Process -Id $picker.Id -Force }
    if ($desktop -and !$desktop.HasExited) { Stop-Process -Id $desktop.Id -Force }
    if ($agent -and !$agent.HasExited) {
      # Never terminate an agent: use the authenticated current-session shutdown
      # and wait for this exact fixture process to release its writer lock.
      & (Join-Path $PSScriptRoot 'vaultsyncd-agent.exe') --user-session shutdown 2>&1 | Set-Content (Join-Path $PSScriptRoot 'agent-shutdown.log')
      if ($LASTEXITCODE -ne 0 -or !$agent.WaitForExit(20000)) {
        'orderly fixture agent shutdown failed; process retained, no force kill attempted' | Set-Content (Join-Path $PSScriptRoot 'cleanup-error.txt')
        $workerExit = 1
      }
    }
  }
  # Publish completion only after the isolated agent has stopped safely.
  $workerExit | Set-Content (Join-Path $PSScriptRoot 'exit.txt')
  exit
}
$hashes = Get-Content (Join-Path $PSScriptRoot 'hashes.json') -Raw | ConvertFrom-Json
foreach ($item in $hashes.PSObject.Properties) {
  if ((Get-FileHash (Join-Path $PSScriptRoot $item.Name) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $item.Value) { throw 'transport hash mismatch' }
}
$task = 'vaultsyncd-s081-'+[Guid]::NewGuid().ToString('N')
$action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker"
$principal = New-ScheduledTaskPrincipal -UserId 'Quickemu' -LogonType Interactive -RunLevel Limited
try {
  Register-ScheduledTask -TaskName $task -Action $action -Principal $principal | Out-Null
  Start-ScheduledTask -TaskName $task
  $end = (Get-Date).AddSeconds(350)
  while (!(Test-Path (Join-Path $PSScriptRoot 'exit.txt'))) {
    if ((Get-Date) -ge $end) { throw 'interactive acceptance worker timeout' }
    Start-Sleep -Milliseconds 200
  }
  if ((Get-Content (Join-Path $PSScriptRoot 'exit.txt')) -ne '0') {
    Get-Content (Join-Path $PSScriptRoot 'error.txt')
    Get-Content (Join-Path $PSScriptRoot 'desktop.log') -ErrorAction SilentlyContinue
    Get-Content (Join-Path $PSScriptRoot 'agent.log') -ErrorAction SilentlyContinue
    exit 1
  }
  Get-Content (Join-Path $PSScriptRoot 'result.json')
} finally { Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue }
