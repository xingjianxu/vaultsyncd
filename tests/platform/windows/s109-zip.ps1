# Run in a manually logged-on, NEW standard user's desktop terminal. No logon/user changes.
param(
  [ValidateSet('Fresh','Live','Stop')][string]$Mode='Fresh',
  [Parameter(Mandatory=$true)][string]$RunDirectory,
  [string]$ZipPath,
  [string]$ZipSha256,
  [string]$ExpectedCommit,
  [string]$BindingId
)
$ErrorActionPreference='Stop'
[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false)
$identity=[Security.Principal.WindowsIdentity]::GetCurrent()
if((New-Object Security.Principal.WindowsPrincipal($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)){throw 'Use a new STANDARD user, never an existing application user or administrator'}
if((Get-Process -Id $PID).SessionId-eq 0){throw 'Run in the manually logged-on interactive desktop, not session 0'}
if(![IO.Path]::IsPathRooted($RunDirectory)){throw 'absolute new run directory required'}
$RunDirectory=[IO.Path]::GetFullPath($RunDirectory)
if($RunDirectory.StartsWith((Join-Path $env:LOCALAPPDATA 'vaultsyncd'),[StringComparison]::OrdinalIgnoreCase)){throw 'run/program directory must be outside application state'}
$program=Join-Path $RunDirectory '中文 ZIP 程序'
$profile=Join-Path $env:LOCALAPPDATA 'vaultsyncd\agent'
$owner=Join-Path $env:LOCALAPPDATA 'vaultsyncd\zip-owner.json'
$results=Join-Path $RunDirectory 'results'
$script:sequence=0
function Write-Json($path,$value){[IO.File]::WriteAllText($path,($value|ConvertTo-Json -Depth 30),(New-Object Text.UTF8Encoding($false)))}
function Run-Session($directory,$action){
  $script:sequence++
  $out=Join-Path $results ($Mode+'-'+$script:sequence+'-'+$action+'.stdout.txt')
  $err=Join-Path $results ($Mode+'-'+$script:sequence+'-'+$action+'.stderr.txt')
  $process=Start-Process -FilePath (Join-Path $directory 'vaultsyncd-agent.exe') -ArgumentList @('--user-session',$action) -RedirectStandardOutput $out -RedirectStandardError $err -PassThru
  $null=$process.Handle
  if(!$process.WaitForExit(180000)){throw 'user-session command timeout; no process was killed; preserve logs and use safe Stop'}
  $process.WaitForExit();$process.Refresh()
  $value=@{exit=$process.ExitCode;stdout=[IO.File]::ReadAllText($out);stderr=[IO.File]::ReadAllText($err)}
  if($value.exit-ne 0){throw ('user-session '+$action+' failed: '+$value.stderr)}
  $value
}
function Status { (Run-Session $program 'health').stdout|ConvertFrom-Json }
function Agent {
  $items=@(Get-CimInstance Win32_Process -Filter "Name='vaultsyncd-agent.exe'"|Where-Object{$_.ExecutablePath-eq (Join-Path $program 'vaultsyncd-agent.exe') -and $_.SessionId-eq (Get-Process -Id $PID).SessionId})
  if($items.Count-ne 1){throw 'expected one exact ZIP agent in current session'}
  $items[0]
}
function Close-Desktop([switch]$RequireWindow) {
  if($RequireWindow){
  $windowDeadline=(Get-Date).AddSeconds(45)
  do{
    $visible=@(Get-Process|Where-Object{try{$_.Path-eq (Join-Path $program 'vaultsyncd-desktop.exe') -and $_.SessionId-eq (Get-Process -Id $PID).SessionId -and $_.MainWindowHandle-ne [IntPtr]::Zero}catch{$false}})
    if($visible.Count){break}
    Start-Sleep -Milliseconds 200
  }while((Get-Date)-lt $windowDeadline)
  if(!$visible.Count){throw 'no real GUI window observed; cannot claim GUI-close acceptance'}
  }
  $items=@(Get-Process|Where-Object{try{$_.Path-eq (Join-Path $program 'vaultsyncd-desktop.exe') -and $_.SessionId-eq (Get-Process -Id $PID).SessionId}catch{$false}})
  foreach($item in $items){
    $item.Refresh()
    if(!$item.HasExited-and $item.MainWindowHandle-ne [IntPtr]::Zero){
      if(!$item.CloseMainWindow()){throw 'owned GUI close request was rejected'}
      $end=(Get-Date).AddSeconds(30)
      do{Start-Sleep -Milliseconds 200;$item.Refresh()}while(!$item.HasExited-and $item.MainWindowHandle-ne [IntPtr]::Zero-and (Get-Date)-lt $end)
      if(!$item.HasExited-and $item.MainWindowHandle-ne [IntPtr]::Zero){throw 'GUI still open; no force kill attempted'}
    }
  }
}
function Registration-Snapshot {
  $runKey=Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -ErrorAction SilentlyContinue
  @{run=if($runKey){$runKey.'vaultsyncd-agent'}else{$null};setup=[bool](Test-Path 'HKCU:\Software\vaultsyncd');shortcut=[bool](Test-Path (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\vaultsyncd.lnk'))}
}
if($Mode-eq 'Fresh'){
  if(Test-Path $RunDirectory){throw 'run directory exists: choose a new isolated directory'}
  if(Test-Path (Join-Path $env:LOCALAPPDATA 'vaultsyncd')){throw 'application state already exists: STOP, do not delete it; use a new test user'}
  if(Test-Path 'HKCU:\Software\vaultsyncd'){throw 'setup-managed client exists: use a NEW standard test user'}
  if($ZipSha256-notmatch '^[0-9a-fA-F]{64}$'-or $ExpectedCommit-notmatch '^[0-9a-f]{40}$'){throw 'receipt-derived SHA256 and full source commit are required'}
  if((Get-FileHash -LiteralPath $ZipPath -Algorithm SHA256).Hash-ne $ZipSha256){throw 'archive transport hash mismatch'}
  if((Get-Volume -DriveLetter ([IO.Path]::GetPathRoot($RunDirectory).Substring(0,1))).FileSystem-ne 'NTFS'){throw 'native NTFS required'}
  New-Item -ItemType Directory $results|Out-Null
  $before=Registration-Snapshot
  if($before.run-or $before.setup-or $before.shortcut){throw 'fresh user has prior vaultsyncd registration; preserve it and choose another user'}
  Expand-Archive -LiteralPath $ZipPath -DestinationPath $program
  $manifest=Get-Content -LiteralPath (Join-Path $program 'zip-files.json') -Raw -Encoding UTF8|ConvertFrom-Json
  if($manifest.source.commit-ne $ExpectedCommit-or $manifest.signature_status-ne 'unsigned-test'){throw 'ZIP source/signature label mismatch'}
  if(@($manifest.files).Count-ne 9-or @(Get-ChildItem -LiteralPath $program -File).Count-ne 10){throw 'unexpected ZIP inventory'}
  if(Test-Path (Join-Path $program 'vaultsyncd-zip.exe')){throw 'removed ZIP launcher must not be shipped'}
  foreach($file in $manifest.files){
    if($file.path-match '[/\\:]'-or $file.path-eq '.'-or $file.path-eq '..'){throw 'unsafe ZIP manifest path'}
    $path=Join-Path $program $file.path
    if((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()-ne $file.sha256-or (Get-Item -LiteralPath $path).Length-ne $file.bytes){throw ('ZIP file hash mismatch: '+$file.path)}
  }
  # Exercise the real double-click cmd path, with spaces and Chinese characters.
  $start=Join-Path $program 'Start vaultsyncd.cmd'
  $entry=Start-Process -FilePath $env:ComSpec -ArgumentList ('/d /s /c ""'+$start+'""') -PassThru
  $null=$entry.Handle
  if(!$entry.WaitForExit(120000)){throw 'double-click Start timed out; inspect the visible error; nothing was force killed'}
  $entry.WaitForExit();$entry.Refresh();if($entry.ExitCode-ne 0){throw 'double-click entry failed'}
  $status=Status;$pidBefore=(Agent).ProcessId
  Run-Session $program 'start'|Out-Null
  if((Agent).ProcessId-ne $pidBefore){throw 'repeat Start did not reuse existing agent'}
  Close-Desktop -RequireWindow
  if((Agent).ProcessId-ne $pidBefore){throw 'GUI close changed the agent'}
  $afterGui=Status
  if($status.snapshot_version.Split(':')[0]-ne $afterGui.snapshot_version.Split(':')[0]){throw 'GUI close changed IPC instance'}
  $config=Get-Content -LiteralPath (Join-Path $profile 'agent.json') -Raw -Encoding UTF8|ConvertFrom-Json
  Run-Session $program 'stop'|Out-Null
  Run-Session $program 'stop'|Out-Null # offline idempotence still verifies writer ownership
  if(Test-Path $owner){throw 'fresh startup must not create a ZIP owner'}
  # An old, even invalid owner is inert historical data, not a program/state adoption gate.
  $legacyOwner='S185 legacy invalid owner; preserve these exact bytes'
  [IO.File]::WriteAllText($owner,$legacyOwner,(New-Object Text.UTF8Encoding($false)))
  $legacyOwnerHash=(Get-FileHash -LiteralPath $owner -Algorithm SHA256).Hash
  $manifestPath=Join-Path $program 'zip-files.json'
  $installedPath=Join-Path $program 'installed-files.json'
  $savedManifest=[IO.File]::ReadAllBytes($manifestPath)
  $savedInstalled=[IO.File]::ReadAllBytes($installedPath)
  $originalProgram=$program
  $copy=Join-Path $RunDirectory '另一份 ZIP'
  try{
    # Neither manifest participates in ordinary startup, even when present but malformed.
    [IO.File]::WriteAllText($manifestPath,'invalid legacy ZIP manifest')
    [IO.File]::WriteAllText($installedPath,'invalid legacy installed manifest')
    Run-Session $program 'start'|Out-Null
    $livePid=(Agent).ProcessId
    Close-Desktop -RequireWindow
    Copy-Item -LiteralPath $program -Destination $copy -Recurse
    Run-Session $copy 'start'|Out-Null
    if((Agent).ProcessId-ne $livePid){throw 'another folder did not reuse the running agent'}
    $program=$copy
    Close-Desktop -RequireWindow
    Run-Session $program 'stop'|Out-Null
    # Cold start from another folder must keep the original user's config and state.
    Remove-Item -LiteralPath (Join-Path $copy 'zip-files.json')
    Remove-Item -LiteralPath (Join-Path $copy 'installed-files.json')
    Run-Session $program 'start'|Out-Null
    $null=Agent
    $null=Status
    $copyConfig=Get-Content -LiteralPath (Join-Path $profile 'agent.json') -Raw -Encoding UTF8|ConvertFrom-Json
    if($copyConfig.device_id-ne $config.device_id){throw 'cross-directory start changed device identity'}
    if((Get-FileHash -LiteralPath $owner -Algorithm SHA256).Hash-ne $legacyOwnerHash){throw 'startup changed inert legacy owner'}
    Close-Desktop -RequireWindow
    Run-Session $program 'stop'|Out-Null
  }finally{
    $program=$originalProgram
    [IO.File]::WriteAllBytes($manifestPath,$savedManifest)
    [IO.File]::WriteAllBytes($installedPath,$savedInstalled)
  }
  Run-Session $program 'start'|Out-Null
  $restarted=Status
  if($status.snapshot_version.Split(':')[0]-eq $restarted.snapshot_version.Split(':')[0]){throw 'safe stop did not exit the original instance'}
  $nextConfig=Get-Content -LiteralPath (Join-Path $profile 'agent.json') -Raw -Encoding UTF8|ConvertFrom-Json
  if($nextConfig.device_id-ne $config.device_id){throw 'ZIP restart changed device identity'}
  if((Get-FileHash -LiteralPath $owner -Algorithm SHA256).Hash-ne $legacyOwnerHash){throw 'restart changed inert legacy owner bytes'}
  $after=Registration-Snapshot
  if(($before|ConvertTo-Json -Compress)-ne ($after|ConvertTo-Json -Compress)){throw 'ZIP unexpectedly registered startup/shortcut/setup'}
  Write-Json (Join-Path $results 'fresh.json') @{ok=$true;scope='launcher smoke, NOT live sync/CFAPI acceptance';sid=$identity.User.Value;program=$program;profile=$profile;source=$ExpectedCommit;archive_sha256=$ZipSha256;first_status=$status;restarted_status=$restarted;registration=$after;device_id=$config.device_id;cross_directory_reuse=$true;cross_directory_cold_start=$true;manifests_not_required=$true;legacy_owner_sha256=(Get-FileHash -LiteralPath $owner -Algorithm SHA256).Hash.ToLowerInvariant()}
  Write-Output 'Fresh smoke passed. Agent/GUI remain for explicit live server binding. No users/logon settings were changed.'
  exit
}
if(!(Test-Path (Join-Path $results 'fresh.json'))){throw 'only this script Fresh-owned run may be continued'}
$fresh=Get-Content (Join-Path $results 'fresh.json') -Raw -Encoding UTF8|ConvertFrom-Json
if($fresh.sid-ne $identity.User.Value-or $fresh.program-ne $program){throw 'wrong run/user'}
if(!(Test-Path $owner)-or (Get-FileHash -LiteralPath $owner -Algorithm SHA256).Hash.ToLowerInvariant()-ne $fresh.legacy_owner_sha256){throw 'owned run legacy owner fixture was changed'}
if($Mode-eq 'Stop'){
  Close-Desktop
  Run-Session $program 'stop'|Out-Null
  if((Get-FileHash -LiteralPath $owner -Algorithm SHA256).Hash.ToLowerInvariant()-ne $fresh.legacy_owner_sha256){throw 'safe Stop changed legacy owner bytes'}
  Write-Json (Join-Path $results 'stopped.json') @{ok=$true;state_preserved=(Test-Path $profile);owner_preserved=(Test-Path $owner)}
  exit
}
# Live requires explicit GUI/CLI enrollment against a real isolated server FIRST. It is never
# called a success for an empty profile. Host must independently verify the returned file hash.
if($BindingId-notmatch '^[0-9a-f]{32}$'){throw 'Live requires an explicit configured binding ID'}
$cli=Join-Path $program 'vaultsyncd-cli.exe'
$text=& $cli --json bindings
if($LASTEXITCODE-ne 0){throw 'binding list IPC failed'}
$binding=@(($text|ConvertFrom-Json).result.management.bindings|Where-Object{$_.binding_id-eq $BindingId})
if($binding.Count-ne 1-or $binding[0].paused){throw 'Live requires one explicit active binding'}
$binding=$binding[0]
$agentPid=(Agent).ProcessId
Close-Desktop -RequireWindow
$name='s109-after-gui-close-'+[Guid]::NewGuid().ToString('N')+'.txt'
$path=Join-Path $binding.local_path $name
[IO.File]::WriteAllText($path,('S109 real synchronization after closing GUI '+[Guid]::NewGuid().ToString('N')),(New-Object Text.UTF8Encoding($false)))
# Allow at least one normal discovery poll; the host's independent server hash is still
# mandatory, rather than treating a previously synchronized stale snapshot as proof of upload.
Start-Sleep -Seconds 4
$end=(Get-Date).AddMinutes(3)
do{
  Start-Sleep -Milliseconds 500
  $status=Status
  $observed=@($status.bindings|Where-Object{$_.binding_id-eq $BindingId})
  if((Get-Date)-gt $end){throw 'live synchronization timeout; preserve content and logs'}
}while($observed.Count-ne 1-or !$observed[0].synchronized)
if((Agent).ProcessId-ne $agentPid){throw 'agent was replaced after GUI close'}
$hash=(Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
Run-Session $program 'stop'|Out-Null
if(!(Test-Path $path)-or !(Test-Path $binding.cache_path)-or !(Test-Path (Join-Path $profile 'state\client.sqlite'))){throw 'safe Stop removed user content/cache/state'}
if((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()-ne $hash){throw 'safe Stop changed file bytes'}
Run-Session $program 'start'|Out-Null
$status=Status
foreach($item in $status.bindings){if(!$item.paused){throw 'PrepareMaintenance pause was not retained on restart'}}
if((Get-FileHash -LiteralPath $owner -Algorithm SHA256).Hash.ToLowerInvariant()-ne $fresh.legacy_owner_sha256){throw 'live stop/restart changed legacy owner bytes'}
Write-Json (Join-Path $results 'live.json') @{ok=$true;host_server_hash_verification='REQUIRED separately; not asserted by guest';filename=$name;sha256=$hash;bytes=(Get-Item -LiteralPath $path).Length;binding_id=$BindingId;vault=$binding.local_path;cache=$binding.cache_path;status_after_restart=$status;exports=@(Get-ChildItem -LiteralPath $profile -Directory -Filter 'session-stop-*'|Select-Object -ExpandProperty FullName)}
Write-Output 'Live guest checks passed. Independently compare real server bytes/hash; bindings remain paused. Use Stop mode to leave this owned run safely stopped.'
