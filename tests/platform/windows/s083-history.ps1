param([switch]$Worker)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false)
if ($Worker) {
  $agent = $null; $desktop = $null
  $pref=Join-Path $env:LOCALAPPDATA 'vaultsyncd/desktop-notifications.json'
  $shortcut=Join-Path $env:APPDATA 'Microsoft/Windows/Start Menu/Programs/vaultsyncd.lnk'
  $hadPref=Test-Path $pref;$hadShortcut=Test-Path $shortcut
  if($hadPref){Copy-Item $pref (Join-Path $PSScriptRoot 'prior-prefs')}
  if($hadShortcut){Copy-Item $shortcut (Join-Path $PSScriptRoot 'prior-shortcut.lnk')}
  try {
    $fixture = Get-Content (Join-Path $PSScriptRoot 'fixture.json') -Raw | ConvertFrom-Json
    $state = Join-Path $PSScriptRoot 'state'
    $local = Join-Path $PSScriptRoot '同步 目录'
    New-Item -ItemType Directory -Path $state,$local | Out-Null
    $fixture | Add-Member -NotePropertyName localPath -NotePropertyValue $local
    $fixture | Add-Member -NotePropertyName caFile -NotePropertyValue (Join-Path $PSScriptRoot 'ca.pem')
    [IO.File]::WriteAllText((Join-Path $PSScriptRoot 'fixture-runtime.json'), ($fixture | ConvertTo-Json -Depth 8), (New-Object Text.UTF8Encoding($false)))
    $config = @{server=@{id=$fixture.serverId;url=$fixture.url};device_id=[Guid]::NewGuid().ToString('N');state_dir=$state;poll_millis=250;bindings=@()}
    [IO.File]::WriteAllText((Join-Path $PSScriptRoot 'agent.json'), ($config | ConvertTo-Json -Depth 5), (New-Object Text.UTF8Encoding($false)))
    $agent = Start-Process -FilePath (Join-Path $PSScriptRoot 'vaultsyncd-agent.exe') -ArgumentList @('--config', (Join-Path $PSScriptRoot 'agent.json')) -PassThru -RedirectStandardError (Join-Path $PSScriptRoot 'agent.log')
    Start-Sleep -Seconds 3
    if ($agent.HasExited) { throw 'agent failed before GUI test' }
    $env:VAULTSYNCD_S080_REPORT = Join-Path $PSScriptRoot 'webview-report.json'
    $env:VAULTSYNCD_S083_FIXTURE = Join-Path $PSScriptRoot 'fixture-runtime.json'
    $desktop = Start-Process -FilePath (Join-Path $PSScriptRoot 'vaultsyncd-desktop.exe') -PassThru -RedirectStandardError (Join-Path $PSScriptRoot 'desktop.log')
    if (!$desktop.WaitForExit(300000)) { throw 'S083 real recovery WebView acceptance timeout' }
    $report = Get-Content $env:VAULTSYNCD_S080_REPORT -Raw -Encoding UTF8 | ConvertFrom-Json
    if (!$report.ok) { throw ('WebView acceptance failed: '+($report | ConvertTo-Json -Compress -Depth 8)) }
    if ($agent.HasExited) { throw 'closing GUI stopped agent' }
    $report | Add-Member -NotePropertyName guiExitKeepsAgent -NotePropertyValue $true
    foreach($check in @(@($report.historyPath,'S083 history original'),@($report.trashPath,'S083 deleted content'),@('trash.txt','S083 same-name target kept'))){
      $file=Join-Path $local $check[0];$end=(Get-Date).AddSeconds(60)
      while(!(Test-Path $file)){if((Get-Date)-ge $end){throw 'restored content not materialized'};Start-Sleep -Milliseconds 200}
      if([IO.File]::ReadAllText($file)-ne $check[1]){throw ('restored bytes mismatch '+$check[0])}
    }
    $conflicts=@(Get-ChildItem $local -File|Where-Object{[IO.File]::ReadAllText($_.FullName)-eq 'S083 independent conflict'})
    if($conflicts.Count-ne 1){throw 'conflict copy missing or duplicated'}
    if(Test-Path (Join-Path $local 'secret.txt')){throw 'foreign content crossed Vault boundary'}
    $report|Add-Member -NotePropertyName actualBytesVerified -NotePropertyValue $true
    $report|Add-Member -NotePropertyName historyHash -NotePropertyValue (Get-FileHash (Join-Path $local $report.historyPath) -Algorithm SHA256).Hash.ToLowerInvariant()
    $report|Add-Member -NotePropertyName trashHash -NotePropertyValue (Get-FileHash (Join-Path $local $report.trashPath) -Algorithm SHA256).Hash.ToLowerInvariant()
    $report | ConvertTo-Json -Compress -Depth 8 | Set-Content (Join-Path $PSScriptRoot 'result.json') -Encoding UTF8
    '0' | Set-Content (Join-Path $PSScriptRoot 'exit.txt')
  } catch {
    $_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'error.txt')
    '1' | Set-Content (Join-Path $PSScriptRoot 'exit.txt')
  } finally {
    if ($desktop -and !$desktop.HasExited) { Stop-Process -Id $desktop.Id -Force }
    if ($agent -and !$agent.HasExited) { Stop-Process -Id $agent.Id -Force;$agent.WaitForExit() }
    if($hadPref){Copy-Item (Join-Path $PSScriptRoot 'prior-prefs') $pref -Force}elseif(Test-Path $pref){Remove-Item $pref}
    if($hadShortcut){Copy-Item (Join-Path $PSScriptRoot 'prior-shortcut.lnk') $shortcut -Force}elseif(Test-Path $shortcut){Remove-Item $shortcut}
  }
  exit
}
$hashes = Get-Content (Join-Path $PSScriptRoot 'hashes.json') -Raw | ConvertFrom-Json
foreach ($item in $hashes.PSObject.Properties) {
  if ((Get-FileHash (Join-Path $PSScriptRoot $item.Name) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $item.Value) { throw 'transport hash mismatch' }
}
$task = 'vaultsyncd-s083-history-'+[Guid]::NewGuid().ToString('N')
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
  Get-Content (Join-Path $PSScriptRoot 'result.json') -Encoding UTF8
} finally { Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue }
