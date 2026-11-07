param([switch]$Worker)
$ErrorActionPreference = 'Stop'
if ($Worker) {
  $agent = $null; $desktop = $null
  try {
    $fixture = Get-Content (Join-Path $PSScriptRoot 'fixture.json') -Raw | ConvertFrom-Json
    $state = Join-Path $PSScriptRoot 'state'
    $local = Join-Path $PSScriptRoot '同步 目录'
    New-Item -ItemType Directory -Path $state,$local | Out-Null
    [IO.File]::WriteAllText((Join-Path $local 'local-only.txt'), 'S081-local-before-import', (New-Object Text.UTF8Encoding($false)))
    $fixture | Add-Member -NotePropertyName localPath -NotePropertyValue $local
    $fixture | Add-Member -NotePropertyName exportPath -NotePropertyValue (Join-Path $PSScriptRoot 'recovery-export')
    $fixture | Add-Member -NotePropertyName caFile -NotePropertyValue (Join-Path $PSScriptRoot 'ca.pem')
    [IO.File]::WriteAllText((Join-Path $PSScriptRoot 'fixture-runtime.json'), ($fixture | ConvertTo-Json -Depth 8), (New-Object Text.UTF8Encoding($false)))
    $config = @{server=@{id=$fixture.serverId;url=$fixture.url};device_id=[Guid]::NewGuid().ToString('N');state_dir=$state;poll_millis=250;bindings=@()}
    [IO.File]::WriteAllText((Join-Path $PSScriptRoot 'agent.json'), ($config | ConvertTo-Json -Depth 5), (New-Object Text.UTF8Encoding($false)))
    $agent = Start-Process -FilePath (Join-Path $PSScriptRoot 'vaultsyncd-agent.exe') -ArgumentList @('--config', (Join-Path $PSScriptRoot 'agent.json')) -PassThru -RedirectStandardError (Join-Path $PSScriptRoot 'agent.log')
    Start-Sleep -Seconds 3
    if ($agent.HasExited) { throw 'agent failed before GUI test' }
    $env:VAULTSYNCD_S080_REPORT = Join-Path $PSScriptRoot 'webview-report.json'
    $env:VAULTSYNCD_S082_FIXTURE = Join-Path $PSScriptRoot 'fixture-runtime.json'
    $desktop = Start-Process -FilePath (Join-Path $PSScriptRoot 'vaultsyncd-desktop.exe') -PassThru -RedirectStandardError (Join-Path $PSScriptRoot 'desktop.log')
    if (!$desktop.WaitForExit(300000)) { throw 'S082 real WebView acceptance timeout' }
    $report = Get-Content $env:VAULTSYNCD_S080_REPORT -Raw -Encoding UTF8 | ConvertFrom-Json
    if (!$report.ok) { throw ('WebView acceptance failed: '+($report | ConvertTo-Json -Compress -Depth 8)) }
    if ($agent.HasExited) { throw 'closing GUI stopped agent' }
    $report | Add-Member -NotePropertyName guiExitKeepsAgent -NotePropertyValue $true
    # Actual downloaded bytes after first merge, and new upload after the GUI has exited.
    $seed = Join-Path $local 'server-seed.txt'
    if ((Get-Content $seed -Raw) -ne 'S081-server-before-import') { throw 'server seed did not materialize' }
    [IO.File]::WriteAllText((Join-Path $local 'after-detach.txt'), 'S082-after-detach-local', (New-Object Text.UTF8Encoding($false)))
    $report | Add-Member -NotePropertyName seedHash -NotePropertyValue (Get-FileHash $seed -Algorithm SHA256).Hash.ToLowerInvariant()
    $report | Add-Member -NotePropertyName localHash -NotePropertyValue (Get-FileHash (Join-Path $local 'local-only.txt') -Algorithm SHA256).Hash.ToLowerInvariant()
    if ((Get-Content (Join-Path $local 'local-only.txt') -Raw) -ne 'S082-unconfirmed-local') { throw 'detach changed dirty bytes' }
    if ((Get-Content (Join-Path $local 'S105-last-moment.txt') -Raw) -ne 'S105 unique content written after detach confirmation opened') { throw 'last-moment detach content lost' }
    $report | Add-Member -NotePropertyName detachConfirmationRace -NotePropertyValue $true
    $expected = @( 'S082-unconfirmed-local', 'S082-pending-local' )
    foreach ($value in $expected) {
      $sha=[Security.Cryptography.SHA256]::Create();$digest=([BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($value)))).Replace('-','').ToLowerInvariant();
      $matches=@(Get-ChildItem $fixture.exportPath -Recurse -File | Where-Object { (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant() -eq $digest });
      if ($matches.Count -eq 0) { throw 'unconfirmed bytes missing in export' }
    }
    $report | Add-Member -NotePropertyName recoveryBytesVerified -NotePropertyValue $true
    Start-Sleep -Seconds 8
    $report | ConvertTo-Json -Compress -Depth 8 | Set-Content (Join-Path $PSScriptRoot 'result.json') -Encoding UTF8
    '0' | Set-Content (Join-Path $PSScriptRoot 'exit.txt')
  } catch {
    $_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'error.txt')
    '1' | Set-Content (Join-Path $PSScriptRoot 'exit.txt')
  } finally {
    if ($desktop -and !$desktop.HasExited) { Stop-Process -Id $desktop.Id -Force }
    if ($agent -and !$agent.HasExited) { Stop-Process -Id $agent.Id -Force }
  }
  exit
}
$hashes = Get-Content (Join-Path $PSScriptRoot 'hashes.json') -Raw | ConvertFrom-Json
foreach ($item in $hashes.PSObject.Properties) {
  if ((Get-FileHash (Join-Path $PSScriptRoot $item.Name) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $item.Value) { throw 'transport hash mismatch' }
}
$task = 'vaultsyncd-s082-'+[Guid]::NewGuid().ToString('N')
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
