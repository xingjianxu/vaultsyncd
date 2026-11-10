param([switch]$Worker)
$ErrorActionPreference = 'Stop'
if ($Worker) {
  $agent = $null; $desktop = $null
  try {
    $session = (Get-Process -Id $PID).SessionId
    if (@(Get-Process vaultsyncd-agent,vaultsyncd-desktop -ErrorAction SilentlyContinue | Where-Object {$_.SessionId -eq $session}).Count -ne 0) { throw 'preserve existing client processes' }
    $fixture = Get-Content (Join-Path $PSScriptRoot 'fixture.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    $state = Join-Path $PSScriptRoot 'state'
    New-Item -ItemType Directory -Path $state | Out-Null
    $fixture | Add-Member -NotePropertyName caFile -NotePropertyValue (Join-Path $PSScriptRoot 'ca.pem')
    $fixture | Add-Member -NotePropertyName phase -NotePropertyValue 'fresh'
    # The selected address must survive restarting from this original, unconfigured JSON.
    $config = @{server=@{id=[Guid]::NewGuid().ToString('N');url='https://localhost'};device_id=[Guid]::NewGuid().ToString('N');state_dir=$state;poll_millis=250;bindings=@()}
    $configPath = Join-Path $PSScriptRoot 'agent.json'
    [IO.File]::WriteAllText($configPath, ($config | ConvertTo-Json -Depth 5), (New-Object Text.UTF8Encoding($false)))
    $reports = @()
    foreach ($phase in @('fresh','restored','loggedout')) {
      $fixture.phase = $phase
      $runtime = Join-Path $PSScriptRoot ('fixture-'+$phase+'.json')
      [IO.File]::WriteAllText($runtime, ($fixture | ConvertTo-Json -Depth 8), (New-Object Text.UTF8Encoding($false)))
      $agent = Start-Process -FilePath (Join-Path $PSScriptRoot 'vaultsyncd-agent.exe') -ArgumentList @('--config',$configPath) -PassThru -RedirectStandardError (Join-Path $PSScriptRoot ('agent-'+$phase+'.log'))
      Start-Sleep -Seconds 3
      if ($agent.HasExited) { throw 'agent failed before login test' }
      $env:VAULTSYNCD_S080_REPORT = Join-Path $PSScriptRoot ('report-'+$phase+'.json')
      $env:VAULTSYNCD_S176_FIXTURE = $runtime
      $desktop = Start-Process -FilePath (Join-Path $PSScriptRoot 'vaultsyncd-desktop.exe') -PassThru -RedirectStandardError (Join-Path $PSScriptRoot ('desktop-'+$phase+'.log'))
      if (!$desktop.WaitForExit(180000)) { throw ('WebView login timeout: '+$phase) }
      $report = Get-Content $env:VAULTSYNCD_S080_REPORT -Raw -Encoding UTF8 | ConvertFrom-Json
      if (!$report.ok) { throw ('login acceptance failed: '+($report | ConvertTo-Json -Compress -Depth 8)) }
      if ($agent.HasExited) { throw 'GUI exit stopped agent' }
      $reports += $report
      Stop-Process -Id $agent.Id
      $agent.WaitForExit()
      $agent = $null
    }
    @{ok=$true;phases=$reports;windowsVersion=[Environment]::OSVersion.VersionString} | ConvertTo-Json -Depth 10 | Set-Content (Join-Path $PSScriptRoot 'result.json') -Encoding UTF8
    '0' | Set-Content (Join-Path $PSScriptRoot 'exit.txt')
  } catch {
    $_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'error.txt')
    '1' | Set-Content (Join-Path $PSScriptRoot 'exit.txt')
  } finally {
    if ($desktop -and !$desktop.HasExited) { Stop-Process -Id $desktop.Id }
    if ($agent -and !$agent.HasExited) { Stop-Process -Id $agent.Id }
  }
  exit
}
$hashes = Get-Content (Join-Path $PSScriptRoot 'hashes.json') -Raw | ConvertFrom-Json
foreach ($item in $hashes.PSObject.Properties) {
  if ((Get-FileHash (Join-Path $PSScriptRoot $item.Name) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $item.Value) { throw 'native binary transport hash mismatch' }
}
$task = 'vaultsyncd-s176-'+[Guid]::NewGuid().ToString('N')
$action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker"
$principal = New-ScheduledTaskPrincipal -UserId 'Quickemu' -LogonType Interactive -RunLevel Limited
try {
  Register-ScheduledTask -TaskName $task -Action $action -Principal $principal | Out-Null
  Start-ScheduledTask -TaskName $task
  $end = (Get-Date).AddSeconds(400)
  while (!(Test-Path (Join-Path $PSScriptRoot 'exit.txt'))) {
    if ((Get-Date) -ge $end) { throw 'interactive native login worker timeout' }
    Start-Sleep -Milliseconds 200
  }
  if ((Get-Content (Join-Path $PSScriptRoot 'exit.txt')) -ne '0') {
    Get-Content (Join-Path $PSScriptRoot 'error.txt')
    exit 1
  }
  Get-Content (Join-Path $PSScriptRoot 'result.json')
} finally { Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue }
