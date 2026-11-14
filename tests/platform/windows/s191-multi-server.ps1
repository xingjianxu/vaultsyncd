param([switch]$Worker)
$ErrorActionPreference = 'Stop'

function Assert([bool]$condition, [string]$message) {
  if (!$condition) { throw $message }
}
function Wait-Until([string]$description, [scriptblock]$test, [int]$seconds = 45) {
  $end = (Get-Date).AddSeconds($seconds)
  do {
    $result = & $test
    if ($result) { return $result }
    Start-Sleep -Milliseconds 250
  } while ((Get-Date) -lt $end)
  throw "Timeout: $description"
}
function Write-Text([string]$path, [string]$text) {
  [IO.File]::WriteAllText($path, $text, (New-Object Text.UTF8Encoding($false)))
}
function Hash([string]$path) {
  (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
}

if ($Worker) {
  $agent = $null; $desktop = $null; $fixture = $null; $ownedBindings = @{}
  $cleanupTargets = @{}
  $workerExit = 1
  $progress = Join-Path $PSScriptRoot 'progress.log'
  $cli = Join-Path $PSScriptRoot 'vaultsyncd-cli.exe'
  $agentExe = Join-Path $PSScriptRoot 'vaultsyncd-agent.exe'
  $desktopExe = Join-Path $PSScriptRoot 'vaultsyncd-desktop.exe'
  function Mark([string]$phase) { Add-Content -LiteralPath $progress -Value $phase }
  function Invoke-Cli([string[]]$arguments, [int]$expected = 0, [string]$reason = '') {
    $text = & $cli --json @arguments
    $exit = $LASTEXITCODE
    $answer = ($text -join "`n") | ConvertFrom-Json
    $script:lastCliError = $answer.error
    Assert ($exit -eq $expected -and $answer.ok -eq ($expected -eq 0)) "CLI $($arguments[0]) exit $exit, expected $expected ($($answer.error.code))"
    if ($reason) { Assert ($answer.error.message -match [regex]::Escape($reason)) "CLI $($arguments[0]) did not report $reason" }
    return $answer.result
  }
  function Start-Agent([string]$phase) {
    $process = Start-Process -FilePath $agentExe -ArgumentList @('--config', ('"'+$configPath+'"')) -PassThru `
      -RedirectStandardError (Join-Path $PSScriptRoot "agent-$phase.log")
    return $process
  }
  function Wait-Agent($process) {
    Wait-Until 'run-owned agent IPC' {
      if ($process.HasExited) { throw 'run-owned agent exited before readiness' }
      $line = & $cli --json status
      if ($LASTEXITCODE -eq 0) { return ($line | ConvertFrom-Json).result }
    }
  }
  function Stop-Agent($process) {
    Assert (!$process.HasExited) 'agent exited before orderly stop'
    $null = & $agentExe --user-session stop
    Assert ($LASTEXITCODE -eq 0) 'orderly agent stop failed'
    Assert ($process.WaitForExit(15000)) 'agent did not exit after orderly stop'
  }
  function Start-WebView([string]$phase, [string]$name) {
    $env:VAULTSYNCD_S191_STARTUP = $phase
    $env:VAULTSYNCD_S080_REPORT = Join-Path $PSScriptRoot "webview-$name.json"
    Start-Process -FilePath $desktopExe -PassThru -RedirectStandardError (Join-Path $PSScriptRoot "desktop-$name.log")
  }
  function Assert-WebView($process) {
    Assert ($process.WaitForExit(40000)) 'native WebView smoke timed out'
    $report = Get-Content -LiteralPath $env:VAULTSYNCD_S080_REPORT -Raw -Encoding UTF8 | ConvertFrom-Json
    Assert ($report.ok -eq $true) 'native WebView smoke did not pass'
    Assert (!$agent.HasExited) 'closing WebView stopped the agent'
    return $report
  }
  function Assert-Vault($server) {
    $items = (Invoke-Cli @('vaults','--server-id',$server.serverId)).management.items
    Assert (@($items).Count -eq 1 -and $items[0].id -eq $server.vault) 'Vault query used the wrong server or account'
  }
  function Confirmed-File([string]$binding, [string]$path) {
    Wait-Until "confirmed $path" {
      $text = & $cli --json files --binding-id $binding
      $exit = $LASTEXITCODE
      $answer = ($text -join "`n") | ConvertFrom-Json
      if ($exit -ne 0 -and $answer.error.message -match 'BINDING_BLOCKED|NOT_READY') { return }
      Assert ($exit -eq 0 -and $answer.ok) "file status failed ($($answer.error.code))"
      $file = $answer.result.management.files | Where-Object { $_.path -eq $path }
      if ($file.entry_id -and $file.synchronized) { return $file }
    }
  }
  function Read-History([string]$binding, [string]$entry) {
    (Invoke-Cli @('history','--binding-id',$binding,'--entry-id',$entry)).management.items
  }

  try {
    Assert (@(Get-Process vaultsyncd-agent,vaultsyncd-desktop -ErrorAction SilentlyContinue).Count -eq 0) 'preserve existing product processes'
    Assert ((Get-Volume -DriveLetter C).FileSystem -eq 'NTFS') 'native NTFS volume required'
    $fixturePath = Join-Path $PSScriptRoot 'fixture.json'
    $fixture = Get-Content -LiteralPath $fixturePath -Raw -Encoding UTF8 | ConvertFrom-Json
    Remove-Item -LiteralPath $fixturePath -Force
    $env:LOCALAPPDATA = Join-Path $PSScriptRoot 'local'
    $env:APPDATA = Join-Path $PSScriptRoot 'roaming'
    $env:TEMP = Join-Path $PSScriptRoot 'temp'
    $env:TMP = $env:TEMP
    $env:WEBVIEW2_USER_DATA_FOLDER = Join-Path $PSScriptRoot 'webview-profile'
    $profilePath = Join-Path $env:LOCALAPPDATA 'vaultsyncd\agent'
    $statePath = Join-Path $profilePath 'state'
    $configPath = Join-Path $profilePath 'agent.json'
    $roots = @{ a = Join-Path $PSScriptRoot 'root-a'; b = Join-Path $PSScriptRoot 'root-b' }
    $caches = @{ a = Join-Path $PSScriptRoot 'cache-a'; b = Join-Path $PSScriptRoot 'cache-b' }
    $modes = @{ a = 'full'; b = 'on_demand' }
    New-Item -ItemType Directory -Path $statePath,$env:APPDATA,$env:TEMP,$roots.a,$roots.b,$caches.a,$caches.b | Out-Null
    $config = @{server=@{id=[Guid]::NewGuid().ToString('N');url='https://localhost'};device_id=[Guid]::NewGuid().ToString('N');state_dir=$statePath;poll_millis=250;bindings=@()}
    Write-Text $configPath ($config | ConvertTo-Json -Depth 5)

    Mark 'fresh'
    $agent = Start-Agent 'first'
    $status = Wait-Agent $agent
    Assert ($null -ne $status.management.connections -and $status.management.connections.Count -eq 0) 'fresh profile had a configured server'
    Assert ($null -ne $status.management.bindings -and $status.management.bindings.Count -eq 0) 'fresh profile had a binding'
    $desktop = Start-WebView '1' 'empty'
    $emptyReport = Assert-WebView $desktop
    $desktop = $null

    Mark 'configure-a-and-b'
    foreach ($side in @('a','b')) {
      $server = $fixture.$side
      $ca = Join-Path $PSScriptRoot "ca-$side.pem"
      $null = Invoke-Cli @('set-connection','--server-id',$server.serverId,'--server-url',$server.url,'--ca-file',$ca)
      $line = $server.password | & $cli --json login --server-id $server.serverId --username $server.login
      Assert ($LASTEXITCODE -eq 0 -and ($line | ConvertFrom-Json).result.management.connection.authenticated) "login failed for $side"
      Assert-Vault $server
    }
    $null = Invoke-Cli @('vaults') 5 'SERVER_REQUIRED'
    $desktop = Start-WebView '2' 'two'
    $twoReport = Assert-WebView $desktop
    $desktop = $null

    Mark 'bind-a-and-b'
    foreach ($side in @('a','b')) {
      $server = $fixture.$side
      $binding = [Guid]::NewGuid().ToString('N')
      $ownedBindings[$side] = $binding
      $cleanupTargets[$binding] = $roots[$side]
      $null = Invoke-Cli @('create-binding','--binding-id',$binding,'--server-id',$server.serverId,'--vault-id',$server.vault,
        '--local-path',$roots[$side],'--cache-path',$caches[$side],'--mode',$modes[$side])
    }
    $extraCache = Join-Path $PSScriptRoot 'cache-overlap-probe'
    New-Item -ItemType Directory -Path $extraCache | Out-Null
    $null = Invoke-Cli @('create-binding','--binding-id',([Guid]::NewGuid().ToString('N')),'--server-id',$fixture.b.serverId,
      '--vault-id',$fixture.b.vault,'--local-path',$roots.a,'--cache-path',$extraCache,'--mode','full') 5 'PATH_OVERLAP'
    $null = Invoke-Cli @('set-connection','--server-id',$fixture.a.serverId,'--server-url',$fixture.b.url,
      '--ca-file',(Join-Path $PSScriptRoot 'ca-b.pem')) 5
    Assert ($script:lastCliError.message.Contains($fixture.a.serverId) -and $script:lastCliError.message.Contains($fixture.b.serverId)) 'identity error did not identify both servers'
    $saved = (Invoke-Cli @('connections')).management.connections | Where-Object { $_.server_id -eq $fixture.a.serverId }
    Assert ($saved.server_url -eq $fixture.a.url -and $saved.authenticated) 'rejected identity change modified A'
    Assert-Vault $fixture.a

    Mark 'bidirectional-and-cfapi'
    $seedB = Join-Path $roots.b 'server-seed.txt'
    $fileB = Confirmed-File $ownedBindings.b 'server-seed.txt'
    Assert ($fileB.residency -eq 'metadata_only' -or $fileB.residency -eq 'partial') 'on-demand seed was already materialized before read'
    Assert ([bool]((Get-Item -LiteralPath $seedB).Attributes -band [IO.FileAttributes]::ReparsePoint)) 'on-demand seed was not a real NTFS placeholder'
    $seedHashes = @{}
    foreach ($side in @('a','b')) {
      $seed = Join-Path $roots[$side] 'server-seed.txt'
      $null = Confirmed-File $ownedBindings[$side] 'server-seed.txt'
      Assert ([IO.File]::ReadAllText($seed) -ceq "S191-server-$side-original") "incorrect downloaded bytes for $side"
      $seedHashes[$side] = Hash $seed
      Write-Text (Join-Path $roots[$side] "upload-$side.txt") "S191-upload-$side"
    }
    $historyHashes = @{}
    foreach ($side in @('a','b')) {
      $name = "upload-$side.txt"
      $uploaded = Confirmed-File $ownedBindings[$side] $name
      $history = @(Read-History $ownedBindings[$side] $uploaded.entry_id)
      $hash = Hash (Join-Path $roots[$side] $name)
      Assert (@($history | Where-Object { $_.entry_id -eq $uploaded.entry_id -and $_.hash -eq $hash }).Count -eq 1) "upload history mismatched for $side"
      $historyHashes[$side] = $hash
    }

    Mark 'restore-per-binding'
    foreach ($side in @('a','b')) {
      $binding = $ownedBindings[$side]
      $seed = Join-Path $roots[$side] 'server-seed.txt'
      $entry = (Confirmed-File $binding 'server-seed.txt').entry_id
      Write-Text $seed "S191-edited-$side"
      $editedHash = Hash $seed
      $head = Wait-Until "edited history for $side" {
        Read-History $binding $entry | Where-Object { $_.hash -eq $editedHash -and $_.version_id }
      }
      $original = Read-History $binding $entry | Where-Object { $_.hash -eq $seedHashes[$side] -and $_.version_id }
      Assert (@($original).Count -eq 1) "original history missing for $side"
      $restored = Invoke-Cli @('restore','--binding-id',$binding,'--entry-id',$entry,
        '--operation-id',([Guid]::NewGuid().ToString('N')),'--base-revision',$head.revision,'--version-id',$original.version_id)
      Assert ($restored.management.restored_path -eq 'server-seed.txt') "restore did not commit for $side"
      $null = Wait-Until "restored bytes for $side" {
        try { [IO.File]::ReadAllText($seed) -ceq "S191-server-$side-original" }
        catch [IO.IOException] { $false }
      }
    }

    Mark 'a-offline-b-synchronizes'
    Write-Text (Join-Path $PSScriptRoot 'block-a') '1'
    $null = Wait-Until 'host blocked A TLS' { Test-Path (Join-Path $PSScriptRoot 'a-offline') } 30
    $offlineFile = 'while-a-offline.txt'
    Write-Text (Join-Path $roots.b $offlineFile) 'S191-B-while-A-offline'
    $offlineAnswer = & $cli --json vaults --server-id $fixture.a.serverId
    Assert ($LASTEXITCODE -ne 0 -and !(($offlineAnswer | ConvertFrom-Json).ok)) 'A remained available during its TLS outage'
    Assert-Vault $fixture.b
    $null = Confirmed-File $ownedBindings.b $offlineFile
    Write-Text (Join-Path $PSScriptRoot 'unblock-a') '1'
    $null = Wait-Until 'host restored A TLS' { Test-Path (Join-Path $PSScriptRoot 'a-online') } 30
    Assert-Vault $fixture.a

    Mark 'logout-a-and-restart'
    $null = Invoke-Cli @('logout','--server-id',$fixture.a.serverId)
    $null = Invoke-Cli @('vaults','--server-id',$fixture.a.serverId) 4
    Assert-Vault $fixture.b
    Stop-Agent $agent
    $agent = Start-Agent 'reopened'
    $status = Wait-Agent $agent
    Assert (@($status.management.connections).Count -eq 2 -and @($status.management.bindings).Count -eq 2) 'restart lost durable connections or bindings'
    foreach ($side in @('a','b')) {
      $server = $fixture.$side
      $connection = @($status.management.connections | Where-Object { $_.server_id -eq $server.serverId })
      Assert ($connection.Count -eq 1 -and $connection[0].authenticated -eq ($side -eq 'b')) "restart changed $side authentication state"
      $row = @($status.management.bindings | Where-Object { $_.binding_id -eq $ownedBindings[$side] })
      Assert ($row.Count -eq 1 -and $row[0].server_id -eq $server.serverId -and $row[0].vault_id -eq $server.vault -and $row[0].local_path -eq $roots[$side] -and $row[0].mode -eq $modes[$side]) "restart changed $side binding route"
    }
    Assert-Vault $fixture.b
    $null = Invoke-Cli @('resume','--binding-id',$ownedBindings.b)
    Write-Text (Join-Path $roots.b 'after-restart.txt') 'S191-B-after-restart'
    $null = Confirmed-File $ownedBindings.b 'after-restart.txt'
    $null = Invoke-Cli @('logout','--server-id',$fixture.b.serverId)
    Stop-Agent $agent
    $agent = $null
    foreach ($side in @('a','b')) {
      $cleanup = & (Join-Path $PSScriptRoot 's191_cleanup.exe') $ownedBindings[$side] $roots[$side]
      Assert ($LASTEXITCODE -eq 0) "test-root provider cleanup failed for $side"
      Write-Text (Join-Path $PSScriptRoot "cleanup-$side.json") ($cleanup -join "`n")
    }
    $migration = $false
    $legacy = Join-Path $PSScriptRoot 'legacy'
    if (Test-Path -LiteralPath (Join-Path $legacy 'vaultsyncd-agent.exe')) {
      Mark 'legacy-profile-upgrade'
      $newAgent = $agentExe; $newCli = $cli
      $agentExe = Join-Path $legacy 'vaultsyncd-agent.exe'
      $cli = Join-Path $legacy 'vaultsyncd-cli.exe'
      $env:LOCALAPPDATA = Join-Path $PSScriptRoot 'legacy-local'
      $profilePath = Join-Path $env:LOCALAPPDATA 'vaultsyncd\agent'
      $statePath = Join-Path $profilePath 'state'
      $configPath = Join-Path $profilePath 'agent.json'
      $migrationRoot = Join-Path $PSScriptRoot 'legacy-root'
      $migrationCache = Join-Path $PSScriptRoot 'legacy-cache'
      New-Item -ItemType Directory -Path $statePath,$migrationRoot,$migrationCache | Out-Null
      $config = @{server=@{id=[Guid]::NewGuid().ToString('N');url='https://localhost'};device_id=[Guid]::NewGuid().ToString('N');state_dir=$statePath;poll_millis=250;bindings=@()}
      Write-Text $configPath ($config | ConvertTo-Json -Depth 5)
      $agent = Start-Agent 'legacy-v14'
      $null = Wait-Agent $agent
      $null = Invoke-Cli @('set-connection','--server-id',$fixture.a.serverId,'--server-url',$fixture.a.url,
        '--ca-file',(Join-Path $PSScriptRoot 'ca-a.pem'))
      $line = $fixture.a.password | & $cli --json login --username $fixture.a.login
      Assert ($LASTEXITCODE -eq 0 -and ($line | ConvertFrom-Json).result.management.connection.authenticated) 'legacy login failed'
      $migrationBinding = [Guid]::NewGuid().ToString('N')
      $cleanupTargets[$migrationBinding] = $migrationRoot
      $null = Invoke-Cli @('create-binding','--binding-id',$migrationBinding,'--vault-id',$fixture.a.vault,
        '--local-path',$migrationRoot,'--cache-path',$migrationCache,'--mode','full')
      $null = Confirmed-File $migrationBinding 'server-seed.txt'
      Stop-Agent $agent
      $agentExe = $newAgent; $cli = $newCli
      $agent = Start-Agent 'upgraded-v15'
      $upgraded = Wait-Agent $agent
      Assert (@($upgraded.management.connections).Count -eq 1 -and $upgraded.management.connections[0].server_id -eq $fixture.a.serverId -and $upgraded.management.connections[0].authenticated) 'migration lost the old endpoint or credentials'
      $row = @($upgraded.management.bindings | Where-Object { $_.binding_id -eq $migrationBinding })
      Assert ($row.Count -eq 1 -and $row[0].server_id -eq $fixture.a.serverId -and $row[0].local_path -eq $migrationRoot) 'migration changed the old binding'
      Assert (Test-Path -LiteralPath (Join-Path $statePath 'client.sqlite.pre-migration-v14-to-v15.bak')) 'migration backup was not retained'
      Assert-Vault $fixture.a
      $null = Invoke-Cli @('resume','--binding-id',$migrationBinding)
      $null = Confirmed-File $migrationBinding 'server-seed.txt'
      $null = Invoke-Cli @('set-connection','--server-id',$fixture.b.serverId,'--server-url',$fixture.b.url,
        '--ca-file',(Join-Path $PSScriptRoot 'ca-b.pem'))
      $null = Invoke-Cli @('logout','--server-id',$fixture.a.serverId)
      Stop-Agent $agent
      $agent = $null
      $cleanup = & (Join-Path $PSScriptRoot 's191_cleanup.exe') $migrationBinding $migrationRoot
      Assert ($LASTEXITCODE -eq 0) 'legacy test-root provider cleanup failed'
      Write-Text (Join-Path $PSScriptRoot 'cleanup-legacy.json') ($cleanup -join "`n")
      $migration = $true
    }
    Mark 'native-agent-unit-tests'
    $unitLog = Join-Path $PSScriptRoot 'native-agent-tests.log'
    foreach ($filter in @('ipc::tests','events::tests','issue_projection::tests','runtime::server::tests')) {
      & (Join-Path $PSScriptRoot 's191-agent-tests.exe') $filter --test-threads=1 2>&1 | Add-Content -LiteralPath $unitLog
      Assert ($LASTEXITCODE -eq 0) "native agent unit tests failed for $filter"
    }
    @{ok=$true;seedHashes=$seedHashes;historyHashes=$historyHashes;windowsVersion=[Environment]::OSVersion.VersionString;
      webview=@($emptyReport,$twoReport);full='a';onDemand='b';restore=$true;isolatedOutage='a';restart=$true;legacyMigration=$migration} |
      ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $PSScriptRoot 'result.json') -Encoding UTF8
    $workerExit = 0
  } catch {
    $_ | Out-String | Set-Content -LiteralPath (Join-Path $PSScriptRoot 'error.txt')
  } finally {
    if ($desktop -and !$desktop.HasExited) { Stop-Process -Id $desktop.Id }
    $cleanupErrors = Join-Path $PSScriptRoot 'cleanup-errors.log'
    if ((Test-Path (Join-Path $PSScriptRoot 'a-offline')) -and !(Test-Path (Join-Path $PSScriptRoot 'a-online'))) {
      Write-Text (Join-Path $PSScriptRoot 'unblock-a') '1'
      try { $null = Wait-Until 'restore A for cleanup' { Test-Path (Join-Path $PSScriptRoot 'a-online') } 15 }
      catch { $_ | Out-String | Add-Content -LiteralPath $cleanupErrors }
    }
    if ($agent -and !$agent.HasExited) {
      try {
        # The host fixtures and credentials still exist here. Complete only this run's files
        # before stopping, so product cleanup can retain every byte and remove registrations.
        foreach ($root in $cleanupTargets.Values) {
          Get-ChildItem -LiteralPath $root -File -Recurse | ForEach-Object { $null = [IO.File]::ReadAllBytes($_.FullName) }
        }
        Stop-Agent $agent
      } catch {
        $_ | Out-String | Add-Content -LiteralPath $cleanupErrors
        if (!$agent.HasExited) { Stop-Process -Id $agent.Id }
      }
    }
    foreach ($binding in $cleanupTargets.Keys) {
      try {
        $cleanup = & (Join-Path $PSScriptRoot 's191_cleanup.exe') $binding $cleanupTargets[$binding] 2>&1
        Assert ($LASTEXITCODE -eq 0) "safe cleanup refused $binding; retain its root for recovery"
        Write-Text (Join-Path $PSScriptRoot "cleanup-$binding.json") ($cleanup -join "`n")
      } catch {
        $_ | Out-String | Add-Content -LiteralPath $cleanupErrors
        $workerExit = 1
      }
    }
    if ($fixture) {
      foreach ($side in @('a','b')) {
        $id = $fixture.$side.serverId
        if ($id -cmatch '^[0-9a-f]{32}$') { $null = & cmdkey.exe "/delete:vaultsyncd/session/$id" }
      }
    }
    "$workerExit" | Set-Content -LiteralPath (Join-Path $PSScriptRoot 'exit.txt')
  }
  exit
}

$hashes = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'hashes.json') -Raw | ConvertFrom-Json
foreach ($item in $hashes.PSObject.Properties) {
  Assert ((Hash (Join-Path $PSScriptRoot $item.Name)) -eq $item.Value) "native transport hash mismatch for $($item.Name)"
}
$task = 'vaultsyncd-s191-'+[Guid]::NewGuid().ToString('N')
$action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker"
$principal = New-ScheduledTaskPrincipal -UserId ([Security.Principal.WindowsIdentity]::GetCurrent().Name) -LogonType Interactive -RunLevel Limited
try {
  Register-ScheduledTask -TaskName $task -Action $action -Principal $principal | Out-Null
  Start-ScheduledTask -TaskName $task
  $null = Wait-Until 'interactive native acceptance' { Test-Path (Join-Path $PSScriptRoot 'exit.txt') } 600
  Get-Content -LiteralPath (Join-Path $PSScriptRoot 'progress.log') -ErrorAction SilentlyContinue
  if ((Get-Content -LiteralPath (Join-Path $PSScriptRoot 'exit.txt')) -ne '0') {
    Get-Content -LiteralPath (Join-Path $PSScriptRoot 'error.txt')
    Get-Content -Path (Join-Path $PSScriptRoot 'agent-*.log') -ErrorAction SilentlyContinue
    exit 1
  }
  Get-Content -LiteralPath (Join-Path $PSScriptRoot 'result.json')
} finally {
  Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
}
