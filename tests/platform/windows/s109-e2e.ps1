param([ValidateSet('Prepare','Worker','Cleanup')][string]$Mode)
$ErrorActionPreference='Stop'
[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false)
$public=$PSScriptRoot
if($public-notmatch '^C:\\vaultsyncd-lab\\s109-[a-zA-Z0-9-]+$'){throw 'isolated S109 run directory required'}
$results=Join-Path $public 'results'
$taskName='vaultsyncd-'+(Split-Path $public -Leaf)+'-e2e'
# Ordered progress marks make a stall in a *real* GUI/IPC step diagnosable without
# serializing any credential or exception text. Never throws.
function Mark([string]$step){
  try{
    if(Test-Path -LiteralPath $results){
      [IO.File]::AppendAllText((Join-Path $results 'stages.txt'),((Get-Date).ToString('o')+' '+$step+"`n"),(New-Object Text.UTF8Encoding($false)))
    }
  }catch{}
}
function Write-Json($path,$value){
  $temporary=$path+'.writing'
  [IO.File]::WriteAllText($temporary,($value|ConvertTo-Json -Depth 25 -Compress),(New-Object Text.UTF8Encoding($false)))
  Move-Item -LiteralPath $temporary -Destination $path -Force
}
function Check-Transport {
  $hashes=Get-Content (Join-Path $public 'hashes.json') -Raw -Encoding UTF8|ConvertFrom-Json
  foreach($item in $hashes.PSObject.Properties){
    if($item.Name-ne [IO.Path]::GetFileName($item.Name)){throw 'unsafe transport path'}
    if((Get-FileHash (Join-Path $public $item.Name) -Algorithm SHA256).Hash.ToLowerInvariant()-ne $item.Value){throw 'ZIP/script transport hash mismatch'}
  }
}
if($Mode-eq 'Prepare'){
  Check-Transport
  if((Get-Volume -DriveLetter C).FileSystem-ne 'NTFS'){throw 'real NTFS required'}
  $console=(Get-CimInstance Win32_ComputerSystem).UserName
  if(!$console){throw 'already logged-on interactive console required; no logon/reboot is attempted'}
  $sid=(New-Object Security.Principal.NTAccount($console)).Translate([Security.Principal.SecurityIdentifier]).Value
  if($sid-ne [Security.Principal.WindowsIdentity]::GetCurrent().User.Value){throw 'SSH user must be the logged-on console user'}
  if(Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue){throw 'refuse existing run task'}
  if(Test-Path $results){throw 'refuse reused run results'}
  New-Item -ItemType Directory $results|Out-Null
  Write-Json (Join-Path $public 'worker.json') @{sid=$sid;console=$console;task=$taskName}
  $action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Mode Worker"
  $principal=New-ScheduledTaskPrincipal -UserId $console -LogonType Interactive -RunLevel Limited
  # The worker owns its deadline/finally; Scheduler must not force-kill maintenance.
  $settings=New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew
  # Deliberately no trigger, AtLogOn, stored password, new user, or logon registry write.
  Register-ScheduledTask -TaskName $taskName -Action $action -Principal $principal -Settings $settings|Out-Null
  Start-ScheduledTask -TaskName $taskName
  Write-Output 'STARTED only the current run Interactive/Limited on-demand worker'
  exit
}
if($Mode-eq 'Cleanup'){
  # Unregistering a task does not interrupt its running action. Signal the worker first;
  # it closes only its own GUI and calls the product safe-stop entry, never TerminateProcess.
  if(Test-Path $results){[IO.File]::WriteAllText((Join-Path $results 'cancel'),'safe cleanup requested')}
  # Let the worker observe the cancel and run its own safe stop first: unregistering a task
  # while its action still runs can terminate it and lose the evidence.
  $deadline=(Get-Date).AddSeconds(120)
  while((Get-Date)-lt $deadline -and !(Test-Path -LiteralPath (Join-Path $results 'worker-done'))){Start-Sleep -Milliseconds 500}
  $task=Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
  if($task){
    $expected="-NoProfile -ExecutionPolicy Bypass -File `"$(Join-Path $public 's109-e2e.ps1')`" -Mode Worker"
    if($task.Actions.Count-ne 1 -or $task.Actions[0].Arguments-ne $expected -or $task.Principal.RunLevel-ne 'Limited' -or $task.Triggers.Count-ne 0){throw 'refuse task cleanup: ownership differs'}
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false
  }
  Write-Output 'REMOVED only this run task; worker safe cleanup requested; all local data retained'
  exit
}

# Every GUI, CLI and ZIP entry call below originates in this one Limited desktop worker.
$script:ownsProfile=$false
$script:desktops=@()
$script:completed=$false
$stage='worker-preflight'
try{
  Check-Transport
  Mark 'prepare-transport'
  $expected=Get-Content (Join-Path $public 'worker.json') -Raw -Encoding UTF8|ConvertFrom-Json
  $identity=[Security.Principal.WindowsIdentity]::GetCurrent()
  $session=(Get-Process -Id $PID).SessionId
  if($identity.User.Value-ne $expected.sid -or $session-eq 0){throw 'wrong user/session'}
  # This lab VM has UAC disabled, so even a Limited task yields an elevated token. Record the
  # actual elevation instead of pretending to be a standard user, and require that the product
  # never writes machine-wide state (HKLM/startup services) below.
  $script:elevated=(New-Object Security.Principal.WindowsPrincipal($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
  Mark 'preflight-explorer'
  $explorers=@(Get-CimInstance Win32_Process -Filter "Name='explorer.exe'"|Where-Object{$_.SessionId-eq $session})
  if(!$explorers.Count){throw 'worker is not in an existing Explorer desktop session'}
  foreach($explorer in $explorers){if((Invoke-CimMethod -InputObject $explorer -MethodName GetOwnerSid).Sid-ne $identity.User.Value){throw 'Explorer SID differs'}}
  # LOCALAPPDATA is NOT IPC isolation: endpoints are scoped to the token SID + SessionId.
  # Refuse all old session clients before changing env, calling CLI, or touching any profile.
  $prior=@(Get-CimInstance Win32_Process|Where-Object{$_.SessionId-eq $session -and $_.Name-in @('vaultsyncd-agent.exe','vaultsyncd-session.exe','vaultsyncd-zip.exe','vaultsyncd-desktop.exe')})
  if($prior.Count){throw 'old session client present; preserved without stopping or connecting'}
  Add-Type -TypeDefinition @'
using System;using System.Runtime.InteropServices;
public static class S109Native {
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)]static extern bool WaitNamedPipeW(string n,uint t);
 [DllImport("cldapi.dll",CharSet=CharSet.Unicode)]static extern int CfGetSyncRootInfoByPath(string p,int c,IntPtr b,uint n,out uint r);
 public static void RefusePipe(string name){if(WaitNamedPipeW(name,0))throw new Exception("existing SID/session IPC endpoint");int e=Marshal.GetLastWin32Error();if(e!=2)throw new Exception("SID/session IPC not demonstrably absent: "+e);}
 public static bool Root(string path){IntPtr b=Marshal.AllocHGlobal(8192);try{uint r;return CfGetSyncRootInfoByPath(path,0,b,8192,out r)>=0;}finally{Marshal.FreeHGlobal(b);}}
}
'@
  $pipe='\\.\pipe\vaultsyncd-agent-'+$identity.User.Value+'-'+$session
  [S109Native]::RefusePipe($pipe)
  Mark 'preflight-pipe'
  $fixture=Get-Content (Join-Path $public 'fixture.json') -Raw -Encoding UTF8|ConvertFrom-Json
  if(Get-NetTCPConnection -LocalPort $fixture.cdp_port -State Listen -ErrorAction SilentlyContinue){throw 'CDP port already occupied; no takeover'}
  $originalAppData=$env:APPDATA
  $program=Join-Path $public '同步 程序'
  $local=Join-Path $public '同步 样本'
  $isolatedLocal=Join-Path $public '用户 LocalAppData'
  $isolatedRoaming=Join-Path $public '用户 AppData'
  $webview=Join-Path $public 'WebView 数据'
  foreach($path in @($program,$local,$isolatedLocal,$isolatedRoaming,$webview)){if(Test-Path -LiteralPath $path){throw 'run program/state/root must be new'}}
  foreach($path in @($local,$isolatedLocal,$isolatedRoaming,$webview)){New-Item -ItemType Directory -Path $path|Out-Null}
  $env:LOCALAPPDATA=$isolatedLocal
  $env:APPDATA=$isolatedRoaming
  $env:WEBVIEW2_USER_DATA_FOLDER=$webview
  $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port='+$fixture.cdp_port+' --remote-debugging-address=127.0.0.1'
  # No notification setting file is written. Exercise ZIP's actual opt-in default honestly.
  $profile=Join-Path $isolatedLocal 'vaultsyncd\agent'
  $configPath=Join-Path $profile 'agent.json'
  $cli=Join-Path $program 'vaultsyncd-cli.exe'
  $zipEntry=Join-Path $program 'vaultsyncd-zip.exe'
  $boot=(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToString('o')

  function Registry-Snapshot([string]$path){
    $key=Get-Item -LiteralPath $path -ErrorAction SilentlyContinue
    if(!$key){return [ordered]@{path=$path;present=$false}}
    $values=@();foreach($name in @($key.GetValueNames()|Sort-Object)){
      $values+=[ordered]@{name=$name;kind=[int]$key.GetValueKind($name);value=$key.GetValue($name,$null,[Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)}
    }
    $children=@();foreach($child in @(Get-ChildItem -LiteralPath $path|Sort-Object Name)){$children+=Registry-Snapshot ($path+'\'+$child.PSChildName)}
    [ordered]@{path=$path;present=$true;values=$values;children=$children}
  }
  $shortcutDirectories=@(
    (Join-Path $originalAppData 'Microsoft\Windows\Start Menu'),
    (Join-Path $isolatedRoaming 'Microsoft\Windows\Start Menu'),
    (Join-Path $env:USERPROFILE 'Desktop'),
    [Environment]::GetFolderPath('DesktopDirectory'),
    [Environment]::GetFolderPath('CommonDesktopDirectory'),
    [Environment]::GetFolderPath('Programs'),
    [Environment]::GetFolderPath('CommonPrograms'),
    [Environment]::GetFolderPath('Startup'),
    [Environment]::GetFolderPath('CommonStartup')
  )|Where-Object{$_}|Sort-Object -Unique
  function Defaults-Snapshot {
    $links=@();foreach($directory in $shortcutDirectories){
      if(Test-Path -LiteralPath $directory){
        foreach($file in @(Get-ChildItem -LiteralPath $directory -File -Recurse|Where-Object{$_.Extension-in @('.lnk','.url','.appref-ms')}|Sort-Object FullName)){
          $links+=[ordered]@{path=$file.FullName;sha256=(Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()}
        }
      }
    }
    # Snapshot only; no Set/Remove-ItemProperty, restore, or editing old HKCU entries.
    [ordered]@{run=(Registry-Snapshot 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run');install=(Registry-Snapshot 'HKCU:\Software\vaultsyncd');machine=(Registry-Snapshot 'HKLM:\Software\vaultsyncd');shortcuts=$links}
  }
  function Canonical($value){$value|ConvertTo-Json -Depth 25 -Compress}
  function Hash([string]$path){(Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()}
  Mark 'preflight-env'
  $defaultsBefore=Defaults-Snapshot
  # Raw registry snapshots remain guest-only, outside collected results.
  Write-Json (Join-Path $public 'defaults-before.json') $defaultsBefore
  function Check-Defaults {
    $after=Defaults-Snapshot
    Write-Json (Join-Path $public 'defaults-after.json') $after
    # Ordered maps and sorted value/file names make comparisons deterministic.
    $beforeRun=Canonical $defaultsBefore.run;$afterRun=Canonical $after.run
    $beforeInstall=Canonical $defaultsBefore.install;$afterInstall=Canonical $after.install
    $beforeLinks=Canonical $defaultsBefore.shortcuts;$afterLinks=Canonical $after.shortcuts
    $beforeMachine=Canonical $defaultsBefore.machine;$afterMachine=Canonical $after.machine
    $unchanged=($beforeRun-eq $afterRun -and $beforeInstall-eq $afterInstall -and $beforeMachine-eq $afterMachine -and $beforeLinks-eq $afterLinks)
    Write-Json (Join-Path $results 'defaults-check.json') @{unchanged=$unchanged;notifications_not_pre_disabled=$true}
    return $unchanged
  }
  function Poll([string]$label,[scriptblock]$test,[int]$seconds=180){
    $end=(Get-Date).AddSeconds($seconds)
    do{$value=& $test;if($value){return $value};Start-Sleep -Milliseconds 300}while((Get-Date)-lt $end)
    throw ('timeout '+$label)
  }
  function Invoke-Cli([string[]]$arguments){
    $text=& $cli @arguments 2>&1
    if($LASTEXITCODE-ne 0){throw 'CLI IPC request failed (output suppressed)'}
    ($text|ConvertFrom-Json).result
  }
  function Status {Invoke-Cli @('--json','status')}
  function Binding($status){
    $items=@($status.management.bindings|Where-Object{!$_.detached})
    if($items.Count-ne 1 -or $items[0].root_id-ne $fixture.root -or $items[0].local_path-ne $local){throw 'expected one exact Root/path binding'}
    return $items[0]
  }
  function Files([string]$binding){(Invoke-Cli @('--json','files','--binding-id',$binding)).management.files}
  function Agent {
    $items=@(Get-CimInstance Win32_Process -Filter "Name='vaultsyncd-agent.exe'"|Where-Object{$_.SessionId-eq $session})
    if($items.Count-ne 1 -or $items[0].ExecutablePath-ne (Join-Path $program 'vaultsyncd-agent.exe') -or !$items[0].CommandLine.Contains($configPath)){throw 'one exact run-owned agent required'}
    $items[0]
  }
  function Invoke-Zip([string]$operation){
    # Bounded child wait: a *successful* Start must return after it launches the GUI, so an
    # unbounded pipeline wait here would hide real evidence as a hang. Output goes to per-run
    # files; only exit status and length are reported (no credential text).
    $out=Join-Path $results ('zip-'+$operation+'-out.txt')
    $err=Join-Path $results ('zip-'+$operation+'-err.txt')
    # The launcher's long-lived GUI/agent children inherit the caller's standard handles, so a
    # pipe would only reach EOF when they exit. Redirect to files (never pipes), let a batch file
    # capture the real exit code, and bound the wait.
    $code=Join-Path $results ('zip-'+$operation+'-exit.txt')
    # The command line travels as Unicode arguments (never inside a codepage-bound .cmd file,
    # which would corrupt the Chinese program path), and output goes to files, not pipes.
    $line='"'+$zipEntry+'" '+$operation+' > "'+$out+'" 2> "'+$err+'" & echo !ERRORLEVEL! > "'+$code+'"'
    $process=Start-Process -FilePath $env:ComSpec -ArgumentList @('/v:on','/c',$line) -PassThru -WindowStyle Hidden
    if(!$process.WaitForExit(180000)){throw ('product ZIP entry did not return: '+$operation)}
    $process.WaitForExit()
    if(!(Test-Path -LiteralPath $code)){throw ('product ZIP entry produced no exit code: '+$operation)}
    $exit=[int]((Get-Content -Raw -LiteralPath $code).Trim())
    if($exit-ne 0){throw ('product ZIP entry failed: '+$operation+' exit='+$exit)}
    if(!(Test-Path -LiteralPath $out)){return ''}
    return (Get-Content -Raw -Encoding UTF8 $out)
  }
  function Close-Gui {
    # Owned = exact ExecutablePath under this run's extracted program directory, even when an
    # earlier failure left a window that was never added to the tracked list.
    $owned=@(Get-CimInstance Win32_Process -Filter "Name='vaultsyncd-desktop.exe'"|Where-Object{$_.SessionId-eq $session -and $_.ExecutablePath-eq (Join-Path $program 'vaultsyncd-desktop.exe')})
    foreach($item in $owned){
      $process=Get-Process -Id $item.ProcessId -ErrorAction SilentlyContinue
      if($process -and !$process.HasExited){
        if(!$process.CloseMainWindow()){throw 'owned GUI rejected WM_CLOSE'}
        if(!$process.WaitForExit(15000)){throw 'owned GUI did not exit after WM_CLOSE; no force kill attempted'}
      }
    }
    foreach($desktop in $script:desktops){
      $desktop.Refresh()
      if(!$desktop.HasExited){
        if(!$desktop.CloseMainWindow()){throw 'owned GUI rejected WM_CLOSE'}
        if(!$desktop.WaitForExit(15000)){throw 'owned GUI did not exit after WM_CLOSE; no force kill attempted'}
      }
    }
    $script:desktops=@()
  }
  function Start-Zip {
    [S109Native]::RefusePipe($pipe)
    $script:ownsProfile=$true # Even partial startup may leave our exact agent needing safe stop.
    Mark 'zip-start-enter'
    $null=Invoke-Zip 'start' # Real shipped launcher from ZIP, not direct agent or fake IPC.
    Mark 'zip-start-returned'
    $s=Poll 'healthy exact ZIP agent' {try{$null=Agent;Status}catch{$null}} 90
    Mark 'agent-poll-ok'
    $gui=Poll 'real ZIP GUI process' {
      $items=@(Get-CimInstance Win32_Process -Filter "Name='vaultsyncd-desktop.exe'"|Where-Object{$_.SessionId-eq $session -and $_.ExecutablePath-eq (Join-Path $program 'vaultsyncd-desktop.exe')})
      if($items.Count-eq 1){Get-Process -Id $items[0].ProcessId}
    } 60
    $script:desktops+=@($gui)
    Mark 'gui-poll-ok'
    $health=Invoke-Zip 'health'|ConvertFrom-Json
    if($health.snapshot_version.Split(':')[0]-ne $s.snapshot_version.Split(':')[0]){throw 'ZIP health and CLI differ'}
    return $s
  }
  function Stop-Zip {
    $p=Agent
    $null=Invoke-Zip 'stop' # Real maintenance export/drain/writer-lock stop, not task/process kill.
    if(Get-Process -Id $p.ProcessId -ErrorAction SilentlyContinue){throw 'safe stop left the captured agent alive'}
    [S109Native]::RefusePipe($pipe)
    if(!(Test-Path (Join-Path $profile 'state\client.sqlite'))){throw 'safe stop lost persistent state'}
  }

  Mark 'defaults-snapshot'
  $stage='extract-and-launch'
  $archive=Join-Path $public $fixture.zip
  if((Hash $archive)-ne $fixture.zip_sha256){throw 'archive hash differs from verified component receipt'}
  Mark 'archive-hash-ok'
  Expand-Archive -LiteralPath $archive -DestinationPath $program
  $manifest=Get-Content (Join-Path $program 'zip-files.json') -Raw -Encoding UTF8|ConvertFrom-Json
  if($manifest.source.commit-ne $fixture.source_commit -or $manifest.signature_status-ne 'unsigned-test'){throw 'ZIP provenance mismatch'}
  foreach($file in $manifest.files){
    if($file.path-ne [IO.Path]::GetFileName($file.path)){throw 'ZIP manifest unsafe path'}
    $path=Join-Path $program $file.path
    if((Hash $path)-ne $file.sha256 -or (Get-Item -LiteralPath $path).Length-ne $file.bytes){throw 'extracted ZIP payload hash/size mismatch'}
  }
  Mark 'manifest-verified'
  if((Get-AuthenticodeSignature $zipEntry).Status.ToString()-ne 'NotSigned'){throw 'expected honestly unsigned test ZIP'}
  # A setup-managed HKCU InstallDir is intentionally not deleted or bypassed. Product
  # refusal is a blocked environment, not permission to modify the old installation.
  Mark 'signed-check'
  $initial=Start-Zip
  $firstAgent=(Agent).ProcessId
  Write-Json (Join-Path $results 'ready.json') @{ok=$true;limited=(!$script:elevated);elevated=$script:elevated;sid=$identity.User.Value;session=$session;program=$program;local=$local;profile=$profile;ca=(Join-Path $public 'ca.pem');zip_sha256=(Hash $archive);agent_pid=$firstAgent;gui_pid=$script:desktops[0].Id;status=$initial;boot=$boot}

  Mark 'ready'
  $stage='actions'
  $end=(Get-Date).AddMinutes(35)
  while((Get-Date)-lt $end){
    if(Test-Path (Join-Path $results 'cancel')){break}
    $request=Join-Path $results 'request.json'
    if(!(Test-Path $request)){Start-Sleep -Milliseconds 150;continue}
    $action=Get-Content $request -Raw -Encoding UTF8|ConvertFrom-Json
    Remove-Item -LiteralPath $request
    $stage=$action.action
    $answer=@{id=$action.id;action=$action.action;ok=$true}
    Mark ('action:'+$action.action)
    switch($action.action){
      'status' {$answer.status=Status;$answer.agent_pid=(Agent).ProcessId}
      'close-gui' {
        Close-Gui
        if((Agent).ProcessId-ne $firstAgent){throw 'closing GUI replaced or stopped the agent'}
        [IO.File]::WriteAllText((Join-Path $local 'after-gui-close.txt'),'S109 real background upload after actual GUI close',(New-Object Text.UTF8Encoding($false)))
        if((Hash (Join-Path $local 'after-gui-close.txt'))-ne $fixture.background_sha256){throw 'background local hash mismatch'}
        $answer.gui_closed=$true;$answer.agent_pid=(Agent).ProcessId
      }
      'dirty' {
        $s=Status;$binding=Binding $s
        if(!$s.bindings[0].synchronized){throw 'initial/background work must be committed first'}
        [IO.File]::WriteAllText((Join-Path $local 'dirty.txt'),'S109 unconfirmed bytes preserved across safe process stop and restart',(New-Object Text.UTF8Encoding($false)))
        $s=Poll 'actual dirty projection' {$s=Status;if([long]$s.bindings[0].dirty-gt 0){return $s}}
        $dirtyFile=Files $binding.binding_id|Where-Object path -eq 'dirty.txt'
        if(!$dirtyFile.dirty){throw 'per-file actual dirty absent'}
        $dirtyHash=Hash (Join-Path $local 'dirty.txt')
        if($dirtyHash-ne $fixture.dirty_sha256){throw 'dirty bytes differ'}
        $beforeDevice=(Get-Content $configPath -Raw -Encoding UTF8|ConvertFrom-Json).device_id
        $beforeBinding=Binding $s
        $beforeInstance=$s.snapshot_version.Split(':')[0]
        $answer.device_id=$beforeDevice;$answer.binding=$beforeBinding;$answer.status=$s;$answer.dirty_file=$dirtyFile;$answer.dirty_sha256=$dirtyHash
      }
      'safe-stop' {
        if(!$beforeDevice -or !$dirtyHash){throw 'real dirty evidence required before stop'}
        Stop-Zip
        $answer.agent_exited=$true;$answer.dirty_sha256=Hash (Join-Path $local 'dirty.txt')
        if($answer.dirty_sha256-ne $dirtyHash){throw 'safe stop changed dirty bytes'}
        # Independently read actual product recovery exports, including unconfirmed bytes.
        $exports=@();$foundDirty=$false
        foreach($directory in @(Get-ChildItem -LiteralPath $profile -Directory -Filter 'zip-stop-*')){
          foreach($manifestPath in @(Get-ChildItem -LiteralPath $directory.FullName -File -Filter manifest.json -Recurse)){
            $export=Get-Content $manifestPath.FullName -Raw -Encoding UTF8|ConvertFrom-Json
            foreach($file in $export.files){
              if($file.name-ne [IO.Path]::GetFileName($file.name)){throw 'unsafe recovery export name'}
              $path=Join-Path $manifestPath.Directory.FullName $file.name
              if((Hash $path)-ne $file.sha256 -or (Get-Item $path).Length-ne $file.size){throw 'recovery export readback hash mismatch'}
              if($file.sha256-eq $dirtyHash){$foundDirty=$true}
            }
            $exports+=@{directory=$manifestPath.Directory.FullName;manifest=$export}
          }
        }
        if(!$foundDirty){throw 'safe stop did not export actual dirty bytes'}
        $answer.exports=$exports;$answer.dirty_export_verified=$true
      }
      'restart' {
        if(!$beforeDevice){throw 'dirty/stop sequence required'}
        $s=Start-Zip
        $binding=Binding $s
        $device=(Get-Content $configPath -Raw -Encoding UTF8|ConvertFrom-Json).device_id
        if($device-ne $beforeDevice -or $s.snapshot_version.Split(':')[0]-eq $beforeInstance){throw 'restart lost DeviceId or did not create a new instance'}
        foreach($field in @('binding_id','root_id','local_path','cache_path','mode','detached')){if($binding.$field-ne $beforeBinding.$field){throw 'restart lost binding identity/control'}}
        if(!$binding.paused){throw 'safe stop must retain explicit paused control'}
        $dirtyFile=Files $binding.binding_id|Where-Object path -eq 'dirty.txt'
        if(!$dirtyFile.dirty -or [long]$s.bindings[0].dirty-le 0){throw 'restart lost dirty projection'}
        $answer.dirty_sha256=Hash (Join-Path $local 'dirty.txt')
        if($answer.dirty_sha256-ne $dirtyHash){throw 'restart changed unconfirmed bytes'}
        $answer.device_id=$device;$answer.binding=$binding;$answer.status=$s;$answer.dirty_file=$dirtyFile;$answer.agent_pid=(Agent).ProcessId
        $answer.root_registered=[S109Native]::Root($local);$answer.boot=(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToString('o')
        if(!$answer.root_registered -or $answer.boot-ne $boot){throw 'Root lost or unexpected OS reboot'}
        Close-Gui
      }
      'resume' {
        $binding=Binding (Status)
        $null=Invoke-Cli @('--json','resume','--binding-id',$binding.binding_id)
        $answer.status=Status
      }
      'finish' {
        $s=Status;$binding=Binding $s
        if(!$s.bindings[0].synchronized -or $s.bindings[0].dirty-ne '0'){throw 'finish requires actually committed state'}
        if($s.management.connection.server_id-ne $fixture.serverId){throw 'refuse credential cleanup for a different server'}
        # The only credential operation is authenticated IPC logout of this random server ID.
        $null=Invoke-Cli @('--json','logout')
        $answer.credential_logout_this_server=$true
        Close-Gui
        Stop-Zip
        $answer.agent_exited=$true;$answer.defaults_unchanged=Check-Defaults
        $answer.status_before_logout=$s
        $script:completed=$true
      }
      default {throw 'unknown fixed native action'}
    }
    Mark ('action-done:'+$action.action)
    Write-Json (Join-Path $results ($action.action+'.json')) $answer
    Write-Json (Join-Path $results 'response.json') $answer
    if($action.action-eq 'finish'){break}
  }
  if(!$script:completed -and !(Test-Path (Join-Path $results 'cancel'))){throw 'worker action deadline expired'}
}catch{
  # Do not serialize exception/CLI/fixture text; authentication secrets never enter host logs.
  Write-Json (Join-Path $results 'worker-error.json') @{ok=$false;stage=$stage;exception_type=$_.Exception.GetType().FullName;details='suppressed; inspect the retained run locally'}
}finally{
  $cleanup=@{ok=$true;completed=$script:completed;data_retained=$true;no_force_kill=$true}
  try{
    if($script:ownsProfile){
      Close-Gui
      $live=@(Get-CimInstance Win32_Process -Filter "Name='vaultsyncd-agent.exe'"|Where-Object{$_.SessionId-eq $session -and $_.ExecutablePath-eq (Join-Path $program 'vaultsyncd-agent.exe') -and $_.CommandLine.Contains($configPath)})
      if($live.Count-eq 1){Stop-Zip}elseif($live.Count-gt 1){throw 'ambiguous owned agent; no kill attempted'}
      $cleanup.defaults_unchanged=Check-Defaults
      if(!$cleanup.defaults_unchanged){$cleanup.ok=$false}
    }
  }catch{$cleanup.ok=$false;$cleanup.failure_kind=$_.Exception.GetType().FullName}
  # Secrets stay out of result files/logs. Only this run's fixture copy is discarded.
  if(Test-Path (Join-Path $public 'fixture.json')){Remove-Item -LiteralPath (Join-Path $public 'fixture.json')}
  if(Test-Path $results){
    Write-Json (Join-Path $results 'cleanup.json') $cleanup
    Mark 'worker-finished'
    [IO.File]::WriteAllText((Join-Path $results 'worker-done'),'done')
  }
}
