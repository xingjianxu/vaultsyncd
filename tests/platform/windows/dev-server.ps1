# Configure the persistent Windows lab client against the independent development server.
# fixture.json contains only public IDs/sample bytes; password is a separate short-lived file.
param([switch]$Worker)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$utf8 = New-Object Text.UTF8Encoding($false)
function Save-Json($Path, $Value) { [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 15), $utf8) }
if ($Worker) {
  try {
    $fixture = Get-Content (Join-Path $PSScriptRoot 'fixture.json') -Raw | ConvertFrom-Json
    $url = 'https://wlab.132140.xyz:15401'
    if ($fixture.url -ne $url -or $fixture.serverId -notmatch '^[0-9a-f]{32}$' -or $fixture.vaultId -notmatch '^[0-9a-f]{32}$') { throw 'invalid development fixture' }
    $prod = Invoke-RestMethod 'https://vsyncd.132140.xyz/v1/capabilities'
    $dev = Invoke-RestMethod ($url+'/v1/capabilities')
    if ($dev.server_id -ne $fixture.serverId -or $dev.server_id -eq $prod.server_id) { throw 'development/production identity isolation failed' }
    $program = 'C:\vaultsyncd-dev-client'
    if (!(Test-Path $program)) { New-Item -ItemType Directory $program | Out-Null }
    $hashes = Get-Content (Join-Path $PSScriptRoot 'hashes.json') -Raw | ConvertFrom-Json
    foreach ($file in $hashes.PSObject.Properties) {
      $source = Join-Path (Join-Path $PSScriptRoot 'program') $file.Name
      $target = Join-Path $program $file.Name
      if ((Get-FileHash $source -Algorithm SHA256).Hash.ToLowerInvariant() -ne $file.Value) { throw 'transport hash mismatch' }
      if (Test-Path $target) {
        if ((Get-FileHash $target -Algorithm SHA256).Hash.ToLowerInvariant() -ne $file.Value) { throw 'existing persistent program differs; preserve it for explicit replacement' }
      } else { Copy-Item $source $target }
    }
    $agent = Join-Path $program 'vaultsyncd-agent.exe'
    $cli = Join-Path $program 'vaultsyncd-cli.exe'
    function Invoke-AgentCli([string[]]$Arguments) {
      $text = & $cli --json @Arguments
      if ($LASTEXITCODE -ne 0) { throw ('CLI failed: '+($text -join "`n")) }
      $answer = $text | ConvertFrom-Json
      if (!$answer.ok) { throw 'CLI rejected request' }
      return $answer.result
    }
    $profile = Join-Path $env:LOCALAPPDATA 'vaultsyncd\agent'
    $configPath = Join-Path $profile 'agent.json'
    if (!(Test-Path $configPath)) {
      if (Test-Path (Join-Path $profile 'state\client.sqlite')) { throw 'existing state without config must be recovered explicitly' }
      New-Item -ItemType Directory -Force (Join-Path $profile 'state') | Out-Null
      # A fresh profile starts directly on dev, avoiding any saved production/placeholder connection.
      $config = @{server=@{id=$fixture.serverId;url=$url};device_id=[Guid]::NewGuid().ToString('N');state_dir=(Join-Path $profile 'state');poll_millis=2000;bindings=@()}
      Save-Json $configPath $config
    }
    $config = Get-Content $configPath -Raw | ConvertFrom-Json
    if ($config.server.id -ne $fixture.serverId -or $config.server.url -ne $url) { throw 'existing profile is not dev; preserve it and migrate explicitly' }
    $device = $config.device_id
    & $agent --user-session start
    if ($LASTEXITCODE -ne 0) { throw 'agent startup failed' }
    $end = (Get-Date).AddSeconds(30)
    do {
      try { $status = Invoke-AgentCli -Arguments @('status'); break } catch { if ((Get-Date) -ge $end) { throw }; Start-Sleep -Milliseconds 250 }
    } while ($true)
    $connections = @((Invoke-AgentCli -Arguments @('connections')).management.connections)
    if ($connections.Count -ne 1 -or $connections[0].server_id -ne $fixture.serverId -or $connections[0].server_url -ne $url) { throw 'client must have exactly one development connection' }
    $passwordFile = Join-Path $PSScriptRoot 'password'
    $password = [IO.File]::ReadAllText($passwordFile).TrimEnd("`r", "`n")
    try {
      $loginText = $password | & $cli --json login --username $fixture.username --server-id $fixture.serverId
      if ($LASTEXITCODE -ne 0 -or !($loginText | ConvertFrom-Json).ok) { throw 'development login failed' }
    } finally { $password = $null; Remove-Item $passwordFile -Force }
    $vaults = Invoke-AgentCli -Arguments @('vaults','--server-id',$fixture.serverId)
    if (!@($vaults.management.items | Where-Object { $_.id -eq $fixture.vaultId -or $_.vault_id -eq $fixture.vaultId }).Count) { throw 'development vault not authorized' }
    $bindingId = [Guid]::NewGuid().ToString('N')
    $local = Join-Path $program ('vault-'+$fixture.runId)
    $cache = Join-Path $env:LOCALAPPDATA ('vaultsyncd\binding-cache\'+$bindingId)
    if (Test-Path $local) { throw 'run directory already exists' }
    New-Item -ItemType Directory -Path $local,$cache | Out-Null
    [IO.File]::WriteAllText((Join-Path $local 's207-upload.txt'), $fixture.uploadContent, $utf8)
    $null = Invoke-AgentCli -Arguments @('create-binding','--binding-id',$bindingId,'--vault-id',$fixture.vaultId,'--local-path',$local,'--cache-path',$cache,'--mode','full','--server-id',$fixture.serverId)
    $end = (Get-Date).AddSeconds(120)
    do {
      $files = @((Invoke-AgentCli -Arguments @('files','--binding-id',$bindingId)).management.files)
      $upload = @($files | Where-Object { $_.path -eq 's207-upload.txt' -and $_.synchronized -and $_.entry_id })
      $download = Join-Path $local 's207-download.txt'
      if ($upload.Count -eq 1 -and (Test-Path $download)) {
        if ([IO.File]::ReadAllText($download) -eq $fixture.downloadContent) { break }
      }
      if ((Get-Date) -ge $end) { throw 'development upload/download did not converge' }
      Start-Sleep -Milliseconds 500
    } while ($true)
    $status = Invoke-AgentCli -Arguments @('status')
    $connections = @((Invoke-AgentCli -Arguments @('connections')).management.connections)
    if ($connections.Count -ne 1 -or $connections[0].server_id -ne $fixture.serverId -or !$connections[0].authenticated) { throw 'final client connection isolation failed' }
    if ((Get-Content $configPath -Raw | ConvertFrom-Json).device_id -ne $device) { throw 'device identity changed' }
    $prodAfter = Invoke-RestMethod 'https://vsyncd.132140.xyz/v1/capabilities'
    $prodHealth = Invoke-RestMethod 'https://vsyncd.132140.xyz/v1/health'
    if ($prodAfter.server_id -ne $prod.server_id) { throw 'production identity changed' }
    Save-Json (Join-Path $PSScriptRoot 'result.json') @{ok=$true;url=$url;serverId=$fixture.serverId;productionServerId=$prodAfter.server_id;productionHealth=$prodHealth;deviceId=$device;bindingId=$bindingId;localPath=$local;cachePath=$cache;program=$program;configPath=$configPath;connections=$connections;status=$status;uploadEntryId=$upload[0].entry_id;uploadSha256=(Get-FileHash (Join-Path $local 's207-upload.txt') -Algorithm SHA256).Hash.ToLowerInvariant();downloadSha256=(Get-FileHash $download -Algorithm SHA256).Hash.ToLowerInvariant();sessionId=(Get-Process -Id $PID).SessionId;programHashes=$hashes}
    [IO.File]::WriteAllText((Join-Path $PSScriptRoot 'exit.txt'), '0', $utf8)
  } catch {
    [IO.File]::WriteAllText((Join-Path $PSScriptRoot 'error.txt'), ($_ | Out-String), $utf8)
    [IO.File]::WriteAllText((Join-Path $PSScriptRoot 'exit.txt'), '1', $utf8)
  } finally {
    $secret = Join-Path $PSScriptRoot 'password'
    if (Test-Path $secret) { Remove-Item $secret -Force }
    Remove-Item (Join-Path $PSScriptRoot 'fixture.json') -Force -ErrorAction SilentlyContinue
  }
  exit
}
$task = 'vaultsyncd-s207-'+[Guid]::NewGuid().ToString('N')
$action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker"
$principal = New-ScheduledTaskPrincipal -UserId 'Quickemu' -LogonType Interactive -RunLevel Limited
try {
  Register-ScheduledTask -TaskName $task -Action $action -Principal $principal | Out-Null
  Start-ScheduledTask -TaskName $task
  $end = (Get-Date).AddSeconds(240)
  while (!(Test-Path (Join-Path $PSScriptRoot 'exit.txt'))) {
    if ((Get-Date) -ge $end) { throw 'interactive development setup timed out; preserve agent and evidence' }
    Start-Sleep -Milliseconds 250
  }
  if ((Get-Content (Join-Path $PSScriptRoot 'exit.txt')) -ne '0') { Get-Content (Join-Path $PSScriptRoot 'error.txt'); exit 1 }
  Get-Content (Join-Path $PSScriptRoot 'result.json')
} finally { Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue }
