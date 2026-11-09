param([switch]$Worker)
$ErrorActionPreference = 'Stop'
if ($Worker) {
  Add-Type -AssemblyName UIAutomationClient
  $desktop = $null
  $script:agentPid = $null
  $checks = [Collections.Generic.List[string]]::new()
  $program = Join-Path $PSScriptRoot 'program'
  $desktopExe = Join-Path $program 'vaultsyncd-desktop.exe'
  $agentExe = Join-Path $program 'vaultsyncd-agent.exe'
  $cli = Join-Path $program 'vaultsyncd-cli.exe'
  function Check($ok, [string]$name) { if (!$ok) { throw $name }; $checks.Add($name) }
  function Launch([string]$name, [bool]$failure = $false) {
    $p = Start-Process $desktopExe -PassThru -RedirectStandardError (Join-Path $PSScriptRoot ($name+'.log'))
    $null = $p.Handle # Retain the original process handle so ExitCode survives a fast exit.
    $end = (Get-Date).AddSeconds(90)
    do {
      Start-Sleep -Milliseconds 100
      $p.Refresh()
      if ($p.HasExited) { throw "desktop exited before window: $name" }
    } while (($p.MainWindowHandle -eq 0 -or !$p.MainWindowTitle) -and (Get-Date) -lt $end)
    Check ($p.MainWindowHandle -ne 0 -and $p.MainWindowTitle) ($name+' visible native window')
    if ($failure) {
      $element = [Windows.Automation.AutomationElement]::FromHandle($p.MainWindowHandle)
      $items = $element.FindAll([Windows.Automation.TreeScope]::Descendants, [Windows.Automation.Condition]::TrueCondition)
      $text = @($items | ForEach-Object {$_.Current.Name}) -join ' '
      Check ($text -match 'agent') ($name+' native startup error is visible')
      $text | Set-Content (Join-Path $PSScriptRoot ($name+'-dialog.txt')) -Encoding UTF8
      $null = $p.CloseMainWindow()
      $exited = $p.WaitForExit(10000)
      $p.Refresh()
      @{exited=$exited;code=$p.ExitCode;dialog=$text} | ConvertTo-Json | Set-Content (Join-Path $PSScriptRoot ($name+'-exit.json')) -Encoding UTF8
      Check ($exited -and $p.ExitCode -eq 1) ($name+' exits with startup failure')
    }
    return $p
  }
  function Status {
    $text = & $cli --json status
    if ($LASTEXITCODE -ne 0) { throw 'real agent IPC status failed' }
    return ($text | ConvertFrom-Json)
  }
  function Agents {
    return ,@(Get-CimInstance Win32_Process -Filter "Name='vaultsyncd-agent.exe'" | Where-Object {$_.ExecutablePath -eq $agentExe -and $_.CommandLine -match '--config'})
  }
  function CloseDesktop {
    $null = $script:desktop.CloseMainWindow()
    Check ($script:desktop.WaitForExit(15000)) 'GUI closes normally'
    $script:desktop = $null
  }
  function StopAgent {
    if ($script:agentPid) {
      $p = Get-Process -Id $script:agentPid -ErrorAction SilentlyContinue
      if ($p) { Stop-Process -Id $script:agentPid; $null = $p.WaitForExit(10000) }
      $script:agentPid = $null
    }
  }
  try {
    $session = (Get-Process -Id $PID).SessionId
    $existing = @(Get-Process vaultsyncd-agent,vaultsyncd-desktop -ErrorAction SilentlyContinue | Where-Object {$_.SessionId -eq $session})
    Check ($existing.Count -eq 0) 'interactive session has no pre-existing client processes'
    $env:LOCALAPPDATA = Join-Path $PSScriptRoot 'local'
    New-Item -ItemType Directory -Path $env:LOCALAPPDATA | Out-Null
    $script:desktop = Launch 'cold-start'
    $agents = Agents
    @{expected=$agentExe; agents=@($agents | Select-Object ProcessId,ExecutablePath,CommandLine); count=$agents.Count; desktopTitle=$script:desktop.MainWindowTitle; all=@(Get-CimInstance Win32_Process | Where-Object {$_.Name -like 'vaultsyncd-*'} | Select-Object ProcessId,ExecutablePath,CommandLine)} | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $PSScriptRoot 'cold-diagnostics.json') -Encoding UTF8
    Check ($agents.Count -eq 1) 'cold desktop starts exactly one background agent'
    $script:agentPid = [int]$agents[0].ProcessId
    $initialStatus = Status
    Check (!$initialStatus.result.management.connection.authenticated) 'new profile has no invented credentials'
    $profile = Join-Path $env:LOCALAPPDATA 'vaultsyncd/agent'
    $config = Join-Path $profile 'agent.json'
    Check (Test-Path $config) 'fixed local profile created'
    $initialConfig = [IO.File]::ReadAllText($config)
    Check ($agents[0].CommandLine.Contains($config)) 'agent runs with fixed local profile'
    $duplicate = Start-Process $desktopExe -PassThru
    Check ($duplicate.WaitForExit(15000) -and $duplicate.ExitCode -eq 0) 'duplicate desktop start exits successfully'
    Check ((Agents).Count -eq 1 -and (Agents)[0].ProcessId -eq $script:agentPid) 'duplicate start preserves single agent PID'
    Check (@(Get-Process vaultsyncd-desktop | Where-Object {$_.SessionId -eq $session}).Count -eq 1) 'duplicate start preserves single desktop window'
    CloseDesktop
    Check ((Get-Process -Id $script:agentPid).HasExited -eq $false) 'GUI exit keeps agent alive'
    $null = Status
    $script:desktop = Launch 'reuse-agent'
    Check ((Agents)[0].ProcessId -eq $script:agentPid) 'new GUI reuses healthy agent without respawn'
    CloseDesktop
    StopAgent
    $script:desktop = Launch 'restart-agent'
    $script:agentPid = [int](Agents)[0].ProcessId
    $null = Status
    Check ([IO.File]::ReadAllText($config) -eq $initialConfig) 'agent process restart preserves identity and configuration'
    CloseDesktop
    StopAgent
    [IO.File]::WriteAllText($config, 'invalid-existing-config')
    $null = Launch 'invalid-config' $true
    Check ([IO.File]::ReadAllText($config) -eq 'invalid-existing-config') 'invalid existing configuration is preserved'
    Check ((Agents).Count -eq 0) 'invalid configuration does not create background agent'
    [IO.File]::WriteAllText($config, $initialConfig)
    $barrier = Join-Path $profile 'maintenance.json'
    [IO.File]::WriteAllText($barrier, '{"test":"S167-maintenance-barrier"}')
    $null = Launch 'maintenance-barrier' $true
    Check ((Agents).Count -eq 0 -and (Test-Path $barrier)) 'maintenance barrier is preserved and blocks agent startup'
    Remove-Item $barrier
    Rename-Item $agentExe 'agent-retained.exe'
    try { $null = Launch 'missing-agent' $true }
    finally { Rename-Item (Join-Path $program 'agent-retained.exe') 'vaultsyncd-agent.exe' }
    $env:LOCALAPPDATA = Join-Path $PSScriptRoot 'zip-local'
    New-Item -ItemType Directory -Path $env:LOCALAPPDATA | Out-Null
    $zipFiles = @('vaultsyncd-agent.exe','vaultsyncd-session.exe','vaultsyncd-cli.exe','vaultsyncd-desktop.exe','vaultsyncd-zip.exe','installed-files.json','UNSIGNED-TEST.txt','Start vaultsyncd.cmd','Stop vaultsyncd.cmd','README-ZIP.txt')
    function Inventory($names) {
      return @($names | ForEach-Object { @{path=$_;bytes=(Get-Item (Join-Path $program $_)).Length;sha256=(Get-FileHash (Join-Path $program $_) -Algorithm SHA256).Hash.ToLowerInvariant()} })
    }
    $manifest = @{format=1;source='S167-native-startup';signature_status='unsigned-test';files=(Inventory $zipFiles[0..3])}
    [IO.File]::WriteAllText((Join-Path $program 'installed-files.json'), ($manifest | ConvertTo-Json -Depth 6))
    $zipManifest = @{format=1;source=$manifest.source;signature_status='unsigned-test';files=(Inventory $zipFiles)}
    [IO.File]::WriteAllText((Join-Path $program 'zip-files.json'), ($zipManifest | ConvertTo-Json -Depth 6))
    $script:desktop = Launch 'zip-first-start'
    $script:agentPid = [int](Agents)[0].ProcessId
    $null = Status
    Check (Test-Path (Join-Path $env:LOCALAPPDATA 'vaultsyncd/zip-owner.json')) 'direct desktop first ZIP start claims ownership'
    CloseDesktop
    $script:desktop = Launch 'zip-reuse-agent'
    Check ((Agents)[0].ProcessId -eq $script:agentPid) 'direct ZIP desktop reuses healthy agent'
    CloseDesktop
    StopAgent
    $ownerFile = Join-Path $env:LOCALAPPDATA 'vaultsyncd/zip-owner.json'
    $owner = Get-Content $ownerFile -Raw | ConvertFrom-Json
    $owner.program = Join-Path $PSScriptRoot 'other-program'
    [IO.File]::WriteAllText($ownerFile, ($owner | ConvertTo-Json))
    $null = Launch 'zip-owner-mismatch' $true
    Check ((Agents).Count -eq 0) 'ZIP ownership mismatch does not start agent'
    @{ok=$true;checks=$checks.ToArray();count=$checks.Count;session=$session;native='Windows 11 x64 production desktop';release_published=$false} | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $PSScriptRoot 'result.json') -Encoding UTF8
    '0' | Set-Content (Join-Path $PSScriptRoot 'exit.txt')
  } catch {
    $_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'error.txt')
    '1' | Set-Content (Join-Path $PSScriptRoot 'exit.txt')
  } finally {
    if ($script:desktop -and !$script:desktop.HasExited) { Stop-Process -Id $script:desktop.Id }
    StopAgent
    Get-CimInstance Win32_Process -Filter "Name='vaultsyncd-agent.exe' OR Name='vaultsyncd-desktop.exe'" | Where-Object {$_.ExecutablePath -like ($program+'\*')} | ForEach-Object { Stop-Process -Id $_.ProcessId -ErrorAction SilentlyContinue }
  }
  exit
}
$hashes = Get-Content (Join-Path $PSScriptRoot 'hashes.json') -Raw | ConvertFrom-Json
foreach ($item in $hashes.PSObject.Properties) {
  if ((Get-FileHash (Join-Path (Join-Path $PSScriptRoot 'program') $item.Name) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $item.Value) { throw 'transport hash mismatch' }
}
$task = 'vaultsyncd-s167-'+[Guid]::NewGuid().ToString('N')
$action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker"
$principal = New-ScheduledTaskPrincipal -UserId 'Quickemu' -LogonType Interactive -RunLevel Limited
try {
  Register-ScheduledTask -TaskName $task -Action $action -Principal $principal | Out-Null
  Start-ScheduledTask -TaskName $task
  $end = (Get-Date).AddSeconds(420)
  while (!(Test-Path (Join-Path $PSScriptRoot 'exit.txt'))) {
    if ((Get-Date) -ge $end) { throw 'interactive startup worker timeout' }
    Start-Sleep -Milliseconds 200
  }
  if ((Get-Content (Join-Path $PSScriptRoot 'exit.txt')) -ne '0') { Get-Content (Join-Path $PSScriptRoot 'error.txt'); exit 1 }
  Get-Content (Join-Path $PSScriptRoot 'result.json')
} finally { Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue }
