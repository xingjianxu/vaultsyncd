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
if(![IO.Path]::IsPathVaulted($RunDirectory)){throw 'absolute new run directory required'}
$RunDirectory=[IO.Path]::GetFullPath($RunDirectory)
if($RunDirectory.StartsWith((Join-Path $env:LOCALAPPDATA 'vaultsyncd'),[StringComparison]::OrdinalIgnoreCase)){throw 'run/program directory must be outside application state'}
$program=Join-Path $RunDirectory '中文 ZIP 程序'
$profile=Join-Path $env:LOCALAPPDATA 'vaultsyncd\agent'
$owner=Join-Path $env:LOCALAPPDATA 'vaultsyncd\zip-owner.json'
$results=Join-Path $RunDirectory 'results'
$script:sequence=0
function Write-Json($path,$value){[IO.File]::WriteAllText($path,($value|ConvertTo-Json -Depth 30),(New-Object Text.UTF8Encoding($false)))}
function Run-Zip($directory,$action,[switch]$MayFail){
  $script:sequence++
  $out=Join-Path $results ($Mode+'-'+$script:sequence+'-'+$action+'.stdout.txt')
  $err=Join-Path $results ($Mode+'-'+$script:sequence+'-'+$action+'.stderr.txt')
  $process=Start-Process -FilePath (Join-Path $directory 'vaultsyncd-zip.exe') -ArgumentList $action -RedirectStandardOutput $out -RedirectStandardError $err -PassThru
  $null=$process.Handle
  if(!$process.WaitForExit(180000)){throw 'ZIP command timeout; no process was killed; preserve logs and use safe Stop'}
  $process.WaitForExit();$process.Refresh()
  $value=@{exit=$process.ExitCode;stdout=[IO.File]::ReadAllText($out);stderr=[IO.File]::ReadAllText($err)}
  if(!$MayFail-and $value.exit-ne 0){throw ('ZIP '+$action+' failed: '+$value.stderr)}
  $value
}
function Status { (Run-Zip $program 'health').stdout|ConvertFrom-Json }
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
  if((Get-Volume -DriveLetter ([IO.Path]::GetPathVault($RunDirectory).Substring(0,1))).FileSystem-ne 'NTFS'){throw 'native NTFS required'}
  New-Item -ItemType Directory $results|Out-Null
  $before=Registration-Snapshot
  if($before.run-or $before.setup-or $before.shortcut){throw 'fresh user has prior vaultsyncd registration; preserve it and choose another user'}
  Expand-Archive -LiteralPath $ZipPath -DestinationPath $program
  $manifest=Get-Content -LiteralPath (Join-Path $program 'zip-files.json') -Raw -Encoding UTF8|ConvertFrom-Json
  if($manifest.source.commit-ne $ExpectedCommit-or $manifest.signature_status-ne 'unsigned-test'){throw 'ZIP source/signature label mismatch'}
  if(@($manifest.files).Count-ne 10-or @(Get-ChildItem -LiteralPath $program -File).Count-ne 11){throw 'unexpected ZIP inventory'}
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
  Run-Zip $program 'start'|Out-Null
  if((Agent).ProcessId-ne $pidBefore){throw 'repeat Start did not reuse existing agent'}
  Close-Desktop -RequireWindow
  if((Agent).ProcessId-ne $pidBefore){throw 'GUI close changed the agent'}
  $afterGui=Status
  if($status.snapshot_version.Split(':')[0]-ne $afterGui.snapshot_version.Split(':')[0]){throw 'GUI close changed IPC instance'}
  $config=Get-Content -LiteralPath (Join-Path $profile 'agent.json') -Raw -Encoding UTF8|ConvertFrom-Json
  Run-Zip $program 'stop'|Out-Null
  Run-Zip $program 'stop'|Out-Null # offline idempotence still verifies writer ownership
  $copy=Join-Path $RunDirectory '另一份 ZIP'
  Copy-Item -LiteralPath $program -Destination $copy -Recurse
  $refused=Run-Zip $copy 'start' -MayFail
  if($refused.exit-eq 0-or $refused.stderr-notmatch 'ZIP_OWNER_MISMATCH'){throw 'different program path was not explicitly refused'}
  $manifestPath=Join-Path $program 'zip-files.json'
  $saved=[IO.File]::ReadAllBytes($manifestPath)
  try{
    $changed=Get-Content -LiteralPath $manifestPath -Raw -Encoding UTF8|ConvertFrom-Json
    $changed.source.commit='0000000000000000000000000000000000000000'
    Write-Json $manifestPath $changed
    $sourceRefused=Run-Zip $program 'start' -MayFail
    if($sourceRefused.exit-eq 0-or $sourceRefused.stderr-notmatch 'ZIP_OWNER_MISMATCH'){throw 'different source was not explicitly refused'}
  }finally{[IO.File]::WriteAllBytes($manifestPath,$saved)}
  Run-Zip $program 'start'|Out-Null
  $restarted=Status
  if($status.snapshot_version.Split(':')[0]-eq $restarted.snapshot_version.Split(':')[0]){throw 'safe stop did not exit the original instance'}
  $nextConfig=Get-Content -LiteralPath (Join-Path $profile 'agent.json') -Raw -Encoding UTF8|ConvertFrom-Json
  if($nextConfig.device_id-ne $config.device_id){throw 'ZIP restart changed device identity'}
  $after=Registration-Snapshot
  if(($before|ConvertTo-Json -Compress)-ne ($after|ConvertTo-Json -Compress)){throw 'ZIP unexpectedly registered startup/shortcut/setup'}
  Write-Json (Join-Path $results 'fresh.json') @{ok=$true;scope='launcher smoke, NOT live sync/CFAPI acceptance';sid=$identity.User.Value;program=$program;profile=$profile;source=$ExpectedCommit;archive_sha256=$ZipSha256;first_status=$status;restarted_status=$restarted;registration=$after;device_id=$config.device_id;path_refused=$refused;source_refused=$sourceRefused}
  Write-Output 'Fresh smoke passed. Agent/GUI remain for explicit live server binding. No users/logon settings were changed.'
  exit
}
if(!(Test-Path $owner)-or !(Test-Path (Join-Path $results 'fresh.json'))){throw 'only this script Fresh-owned run may be continued'}
$fresh=Get-Content (Join-Path $results 'fresh.json') -Raw -Encoding UTF8|ConvertFrom-Json
if($fresh.sid-ne $identity.User.Value-or $fresh.program-ne $program){throw 'wrong run/user'}
if($Mode-eq 'Stop'){
  Close-Desktop
  Run-Zip $program 'stop'|Out-Null
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
Run-Zip $program 'stop'|Out-Null
if(!(Test-Path $path)-or !(Test-Path $binding.cache_path)-or !(Test-Path (Join-Path $profile 'state\client.sqlite'))){throw 'safe Stop removed user content/cache/state'}
if((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()-ne $hash){throw 'safe Stop changed file bytes'}
Run-Zip $program 'start'|Out-Null
$status=Status
foreach($item in $status.bindings){if(!$item.paused){throw 'PrepareMaintenance pause was not retained on restart'}}
Write-Json (Join-Path $results 'live.json') @{ok=$true;host_server_hash_verification='REQUIRED separately; not asserted by guest';filename=$name;sha256=$hash;bytes=(Get-Item -LiteralPath $path).Length;binding_id=$BindingId;vault=$binding.local_path;cache=$binding.cache_path;status_after_restart=$status;exports=@(Get-ChildItem -LiteralPath $profile -Directory -Filter 'zip-stop-*'|Select-Object -ExpandProperty FullName)}
Write-Output 'Live guest checks passed. Independently compare real server bytes/hash; bindings remain paused. Use Stop mode to leave this owned run safely stopped.'
