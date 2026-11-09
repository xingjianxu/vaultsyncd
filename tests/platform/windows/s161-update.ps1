param([switch]$Worker,[switch]$InstalledOnly)
$ErrorActionPreference='Stop'
[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false)
if($PSScriptRoot-notmatch '^C:\\vaultsyncd-lab\\s161-[a-zA-Z0-9-]+$'){throw 'isolated S161 run required'}
function Write-Json($path,$value){[IO.File]::WriteAllText($path,($value|ConvertTo-Json -Depth 30),(New-Object Text.UTF8Encoding($false)))}
if(!$Worker){
  $hashes=Get-Content (Join-Path $PSScriptRoot 'hashes.json') -Raw|ConvertFrom-Json
  foreach($f in $hashes.PSObject.Properties){if((Get-FileHash (Join-Path $PSScriptRoot $f.Name)).Hash.ToLowerInvariant()-ne $f.Value){throw 'transport hash mismatch'}}
  $task='vaultsyncd-s161-'+[Guid]::NewGuid().ToString('N')
  $workerArgs="-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker";if($InstalledOnly){$workerArgs+=' -InstalledOnly'}
  $action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument $workerArgs
  $principal=New-ScheduledTaskPrincipal -UserId 'Quickemu' -LogonType Interactive -RunLevel Limited
  try{
    Register-ScheduledTask -TaskName $task -Action $action -Principal $principal|Out-Null
    Start-ScheduledTask -TaskName $task
    $end=(Get-Date).AddSeconds(900)
    $last='';$heartbeat=Get-Date
    while(!(Test-Path (Join-Path $PSScriptRoot 'exit.txt'))){
      if((Get-Date)-ge $end){throw 'native worker timeout; test processes/evidence retained'}
      $phase=Join-Path $PSScriptRoot 'phase.txt'
      if(Test-Path $phase){$current=Get-Content $phase -Raw;if($current-ne $last){Write-Output $current.Trim();$last=$current}}
      if(((Get-Date)-$heartbeat).TotalSeconds-ge 15){Write-Output 'Waiting for current native phase; each command has a 180s deadline.';$heartbeat=Get-Date}
      Start-Sleep -Milliseconds 200
    }
    if((Get-Content (Join-Path $PSScriptRoot 'exit.txt'))-ne '0'){Get-Content (Join-Path $PSScriptRoot 'error.txt');exit 1}
    Get-Content (Join-Path $PSScriptRoot 'result.json') -Raw
  }finally{Unregister-ScheduledTask -TaskName $task -Confirm:$false}
  exit
}
Add-Type -TypeDefinition @'
using System;using System.Text;using System.Runtime.InteropServices;using System.Collections.Generic;
public static class S161Native {
 delegate bool Each(IntPtr w,IntPtr p);
 [DllImport("user32.dll")]static extern bool EnumWindows(Each c,IntPtr p);
 [DllImport("user32.dll")]static extern uint GetWindowThreadProcessId(IntPtr h,out uint p);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)]static extern int GetClassNameW(IntPtr h,StringBuilder s,int n);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)]static extern int GetWindowTextW(IntPtr h,StringBuilder s,int n);
 [DllImport("user32.dll")]static extern bool PostMessageW(IntPtr h,uint m,IntPtr w,IntPtr l);
 [DllImport("user32.dll")]static extern IntPtr SendMessageW(IntPtr h,uint m,IntPtr w,IntPtr l);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)]static extern int GetMenuStringW(IntPtr h,uint p,StringBuilder s,int n,uint f);
 [DllImport("user32.dll")]static extern uint GetMenuItemID(IntPtr h,int p);
 [DllImport("user32.dll")]static extern IntPtr GetDlgItem(IntPtr h,int id);
 [DllImport("user32.dll")]static extern int GetMenuItemCount(IntPtr h);
 [DllImport("user32.dll")]static extern bool SetCursorPos(int x,int y);
 [DllImport("user32.dll")]static extern bool GetMenuItemRect(IntPtr w,IntPtr m,uint i,out Rect r);
 [DllImport("user32.dll")]static extern void mouse_event(uint f,uint x,uint y,uint d,UIntPtr e);
 [StructLayout(LayoutKind.Sequential)]struct Rect{public int left,top,right,bottom;}
 public static IntPtr Find(uint process,string kind){IntPtr found=IntPtr.Zero;EnumWindows((h,p)=>{uint pid;GetWindowThreadProcessId(h,out pid);var s=new StringBuilder(256);GetClassNameW(h,s,256);if((process==0||pid==process)&&s.ToString()==kind){found=h;return false;}return true;},IntPtr.Zero);return found;}
 public static bool Dismiss(uint process){var h=Find(process,"#32770");if(h==IntPtr.Zero)return false;var button=GetDlgItem(h,1);if(button!=IntPtr.Zero)PostMessageW(button,0xf5,IntPtr.Zero,IntPtr.Zero);else PostMessageW(h,0x10,IntPtr.Zero,IntPtr.Zero);return true;}
 public static void Popup(IntPtr h){SetCursorPos(650,500);PostMessageW(h,6002,IntPtr.Zero,(IntPtr)0x205);}
 public static string[] Menu(IntPtr popup){var menu=SendMessageW(popup,0x1e1,IntPtr.Zero,IntPtr.Zero);var list=new List<string>();for(int i=0;i<GetMenuItemCount(menu);i++){var s=new StringBuilder(256);GetMenuStringW(menu,(uint)i,s,256,0x400);list.Add(s.ToString());}return list.ToArray();}
 public static void SelectItem(IntPtr tray,IntPtr popup,uint position,bool duplicate){var menu=SendMessageW(popup,0x1e1,IntPtr.Zero,IntPtr.Zero);uint id=GetMenuItemID(menu,(int)position);Rect r;if(!GetMenuItemRect(IntPtr.Zero,menu,position,out r))throw new Exception("menu item rectangle missing");SetCursorPos((r.left+r.right)/2,(r.top+r.bottom)/2);mouse_event(2,0,0,0,UIntPtr.Zero);mouse_event(4,0,0,0,UIntPtr.Zero);if(duplicate)PostMessageW(tray,0x111,(IntPtr)id,IntPtr.Zero);}
}
'@
$script:sequence=0;$script:checks=@();$script:dialogs=0
function Check($value,$name){if(!$value){throw $name};$script:checks+=$name}
function Read-Log($path){$stream=[IO.File]::Open($path,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::ReadWrite);$reader=New-Object IO.StreamReader($stream);try{$reader.ReadToEnd()}finally{$reader.Dispose()}}
function Exe($path,$arguments,[int]$expected=0){
  $script:sequence++;$stem=Join-Path $script:case ('command-'+$script:sequence)
  [IO.File]::WriteAllText((Join-Path $PSScriptRoot 'phase.txt'),('Step '+$script:sequence+' '+[IO.Path]::GetFileName($path)+' '+($arguments|Select-Object -First 1)))
  $args=($arguments|ForEach-Object{'"'+$_+'"'})-join ' '
  $start=@{FilePath=$path;PassThru=$true;RedirectStandardOutput=($stem+'.out');RedirectStandardError=($stem+'.err')}
  if($arguments.Count){$start.ArgumentList=$args}
  $p=Start-Process @start
  $null=$p.Handle;$end=(Get-Date).AddSeconds(180)
  while(!$p.HasExited){if([S161Native]::Dismiss($p.Id)){$script:dialogs++};if((Get-Date)-gt $end){throw ('process timeout '+$path)};Start-Sleep -Milliseconds 100;$p.Refresh()}
  $p.WaitForExit();$p.Refresh()
  if($p.ExitCode-ne $expected){throw ('command exit '+$p.ExitCode+' expected '+$expected+'; '+(Read-Log ($stem+'.err')))}
  Read-Log ($stem+'.out')
}
function Status{(Exe (Join-Path $script:program 'vaultsyncd-cli.exe') @('--json','status'))|ConvertFrom-Json}
function Close-Desktop {
  $items=@(Get-Process|Where-Object{try{$_.Path-eq (Join-Path $script:program 'vaultsyncd-desktop.exe')}catch{$false}})
  foreach($p in $items){
    $tray=[S161Native]::Find($p.Id,'tray_icon_app');if($tray-eq [IntPtr]::Zero){throw 'owned tray missing'}
    [S161Native]::Popup($tray);Start-Sleep -Milliseconds 300;$popup=[S161Native]::Find(0,'#32768')
    if($popup-eq [IntPtr]::Zero){throw 'owned quit menu missing'}
    [S161Native]::SelectItem($tray,$popup,2,$false)
    if(!$p.WaitForExit(30000)){throw 'owned tray Quit did not exit'}
  }
}
function Desktop {
  $end=(Get-Date).AddSeconds(45)
  do{$items=@(Get-Process|Where-Object{try{$_.Path-eq (Join-Path $script:program 'vaultsyncd-desktop.exe')-and $_.MainWindowHandle-ne [IntPtr]::Zero}catch{$false}});if($items.Count-eq 1-and [S161Native]::Find($items[0].Id,'tray_icon_app')-ne [IntPtr]::Zero){return $items[0]};Start-Sleep -Milliseconds 200}while((Get-Date)-lt $end)
  throw 'restarted real WebView window missing'
}
function Start-Case($name){
  $script:case=Join-Path $PSScriptRoot $name;New-Item -ItemType Directory $script:case|Out-Null
  $env:LOCALAPPDATA=Join-Path $script:case 'local';New-Item -ItemType Directory $env:LOCALAPPDATA|Out-Null
  $script:program=Join-Path $script:case '中文 程序';Expand-Archive (Join-Path $PSScriptRoot 'old.zip') $script:program
  Exe (Join-Path $script:program 'vaultsyncd-zip.exe') @('start')|Out-Null
  $null=Desktop;Close-Desktop;Exe (Join-Path $script:program 'vaultsyncd-zip.exe') @('stop')|Out-Null
  $env:VAULTSYNCD_S161_FIXTURE=$script:case
  # The seeder writes only this stopped, isolated fixture database.
  Exe (Join-Path $PSScriptRoot 'agent-tests.exe') @('--exact','auto_update::tests::seed_update_fixture','--ignored','--nocapture')|Out-Null
  $script:configHash=(Get-FileHash (Join-Path $env:LOCALAPPDATA 'vaultsyncd/agent/agent.json')).Hash
  $script:ownerBefore=[IO.File]::ReadAllText((Join-Path $env:LOCALAPPDATA 'vaultsyncd/zip-owner.json'))
  $script:before=@{};foreach($p in @(Get-ChildItem $script:case -Recurse -File|Where-Object{$_.Name-in @('unconfirmed.txt','keep-cache.bin')})){$script:before[$p.FullName]=(Get-FileHash $p.FullName).Hash}
  Exe (Join-Path $script:program 'vaultsyncd-zip.exe') @('start')|Out-Null;$null=Desktop
  $status=Status;Check ($status.result.management.bindings.Count-eq 2) ($name+' two native bindings')
  Close-Desktop
}
function Preserved($name){
  Check ((Get-FileHash (Join-Path $env:LOCALAPPDATA 'vaultsyncd/agent/agent.json')).Hash-eq $script:configHash) ($name+' device/config identity')
  foreach($p in $script:before.Keys){Check ((Get-FileHash $p).Hash-eq $script:before[$p]) ($name+' local/cache bytes '+[IO.Path]::GetFileName($p))}
  $status=Status;$bindings=@($status.result.management.bindings|Sort-Object binding_id)
  Check ($bindings.Count-eq 2-and !$bindings[0].paused-and $bindings[1].paused) ($name+' original pause states')
}
function Installed-Fixture($candidate){
  $script:case=Join-Path $PSScriptRoot 'installed';New-Item -ItemType Directory $script:case|Out-Null
  $env:LOCALAPPDATA=Join-Path $script:case 'local';New-Item -ItemType Directory $env:LOCALAPPDATA|Out-Null
  $script:program=Join-Path $script:case '中文 程序';Expand-Archive (Join-Path $PSScriptRoot 'old.zip') $script:program
  foreach($name in @('vaultsyncd-zip.exe','zip-files.json','README-ZIP.txt','Start vaultsyncd.cmd','Stop vaultsyncd.cmd')){Remove-Item -LiteralPath (Join-Path $script:program $name)}
  $registration='HKCU:\Software\vaultsyncd';$uninstall='HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\vaultsyncd';$run='HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
  Check (!(Test-Path $uninstall)) 'no existing uninstall registration'
  Check (!$((Get-ItemProperty $run -ErrorAction SilentlyContinue).'vaultsyncd-agent')) 'no existing startup value'
  $startup='"'+(Join-Path $script:program 'vaultsyncd-session.exe')+'"'
  New-Item $registration -Force|Out-Null;Set-ItemProperty $registration InstallDir $script:program;Set-ItemProperty $registration Version '0.1.0'
  New-Item $uninstall -Force|Out-Null;Set-ItemProperty $uninstall DisplayVersion '0.1.0'
  Set-ItemProperty $run 'vaultsyncd-agent' $startup
  [IO.File]::WriteAllText((Join-Path $script:program 'uninstall.exe'),'S161 retained uninstall fixture, never executed')
  $uninstallHash=(Get-FileHash (Join-Path $script:program 'uninstall.exe')).Hash
  Exe (Join-Path $script:program 'vaultsyncd-session.exe') @()|Out-Null
  $end=(Get-Date).AddSeconds(30)
  do{try{$null=Status;break}catch{if((Get-Date)-ge $end){throw};Start-Sleep -Milliseconds 200}}while($true)
  $config=Join-Path $env:LOCALAPPDATA 'vaultsyncd/agent/agent.json';$configHash=(Get-FileHash $config).Hash
  Exe (Join-Path $candidate 'vaultsyncd-agent.exe') @('--auto-update','--program-dir',$script:program)|Out-Null
  $null=Desktop;$null=Status
  Check ((Get-ItemProperty $registration).Version-eq '0.1.1') 'installed version registration upgraded'
  Check ((Get-ItemProperty $uninstall).DisplayVersion-eq '0.1.1') 'uninstall display version upgraded'
  Check ((Get-ItemProperty $run).'vaultsyncd-agent'-eq $startup) 'startup entry retained'
  Check ((Get-FileHash (Join-Path $script:program 'uninstall.exe')).Hash-eq $uninstallHash) 'uninstaller retained'
  Check (!(Test-Path (Join-Path $script:program 'zip-files.json'))-and !(Test-Path (Join-Path $env:LOCALAPPDATA 'vaultsyncd/zip-owner.json'))) 'installed profile never converted to ZIP'
  Check ((Get-FileHash $config).Hash-eq $configHash) 'installed device/config identity retained'
  Close-Desktop;$env:VAULTSYNCD_S161_FIXTURE=$script:case
  Exe (Join-Path $PSScriptRoot 'agent-tests.exe') @('--exact','auto_update::tests::stop_update_fixture','--ignored','--nocapture')|Out-Null
  Remove-ItemProperty $run 'vaultsyncd-agent';Remove-Item $registration;Remove-Item $uninstall
  Check (!(Test-Path $registration)-and !(Test-Path $uninstall)) 'only fixture registration cleaned'
}
try{
  Check ((Get-Process -Id $PID).SessionId-ne 0) 'interactive desktop session'
  $existing=@(Get-CimInstance Win32_Process|Where-Object{$_.Name-eq 'vaultsyncd-agent.exe'})
  Check ($existing.Count-eq 0) 'no existing user agent touched'
  Check (!(Test-Path 'HKCU:\Software\vaultsyncd')) 'no setup registration modified by ZIP tests'
  $candidate=Join-Path $PSScriptRoot 'candidate';Expand-Archive (Join-Path $PSScriptRoot 'new.zip') $candidate
  if($InstalledOnly){
    Installed-Fixture $candidate
    Write-Json (Join-Path $PSScriptRoot 'result.json') @{ok=$true;checks=$script:checks;scope='controlled installed-registration fixture; not NSIS release acceptance'}
    '0'|Set-Content (Join-Path $PSScriptRoot 'exit.txt');exit
  }
  Start-Case 'success'
  $oldPid=(Get-CimInstance Win32_Process|Where-Object{$_.ExecutablePath-eq (Join-Path $script:program 'vaultsyncd-agent.exe')}).ProcessId
  Exe (Join-Path $candidate 'vaultsyncd-agent.exe') @('--auto-update','--program-dir',$script:program)|Out-Null
  $gui=Desktop;Preserved 'upgrade'
  $newPid=(Get-CimInstance Win32_Process|Where-Object{$_.ExecutablePath-eq (Join-Path $script:program 'vaultsyncd-agent.exe')}).ProcessId
  Check ($oldPid-ne $newPid) 'agent process restarted'
  $manifest=Get-Content (Join-Path $script:program 'zip-files.json') -Raw|ConvertFrom-Json
  foreach($f in $manifest.files){Check ((Get-FileHash (Join-Path $script:program $f.path)).Hash.ToLowerInvariant()-eq $f.sha256) ('updated ZIP hash '+$f.path)}
  $owner=Get-Content (Join-Path $env:LOCALAPPDATA 'vaultsyncd/zip-owner.json') -Raw|ConvertFrom-Json
  Check ($owner.manifest-eq (Get-FileHash (Join-Path $script:program 'zip-files.json')).Hash.ToLowerInvariant()) 'ZIP owner committed with new manifest'
  Check (!(Test-Path (Join-Path $env:LOCALAPPDATA 'vaultsyncd/agent/maintenance.json'))-and !(Test-Path (Join-Path $env:LOCALAPPDATA 'vaultsyncd/agent/auto-update.json'))) 'completed transaction markers cleared'
  $tray=[S161Native]::Find($gui.Id,'tray_icon_app');Check ($tray-ne [IntPtr]::Zero) 'real native tray icon'
  [S161Native]::Popup($tray);Start-Sleep -Milliseconds 400
  $popup=[S161Native]::Find(0,'#32768');Check ($popup-ne [IntPtr]::Zero) 'right-click native tray menu'
  $menu=[S161Native]::Menu($popup);Write-Json (Join-Path $script:case 'tray-menu.json') $menu
  Check ($menu.Count-eq 3-and $menu[1]-eq '自动更新') 'automatic update menu entry'
  [S161Native]::SelectItem($tray,$popup,1,$true)
  $end=(Get-Date).AddSeconds(55);$dialog=$false
  do{if([S161Native]::Dismiss($gui.Id)){$dialog=$true;break};Start-Sleep -Milliseconds 200}while((Get-Date)-lt $end)
  Check $dialog 'tray command reaches real GitHub check and result dialog'
  Start-Sleep -Milliseconds 500;Check (![S161Native]::Dismiss($gui.Id)) 'duplicate tray commands coalesced'
  Preserved 'failed GitHub check'
  Close-Desktop
  Exe (Join-Path $candidate 'vaultsyncd-agent.exe') @('--maintenance','auto-upgrade','--program-dir',$script:program) 1|Out-Null
  Exe (Join-Path $script:program 'vaultsyncd-zip.exe') @('stop')|Out-Null
  Start-Case 'busy'
  $held=[IO.File]::Open((Join-Path $script:program 'vaultsyncd-cli.exe'),[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::Read)
  try{Exe (Join-Path $candidate 'vaultsyncd-agent.exe') @('--auto-update','--program-dir',$script:program) 1|Out-Null}finally{$held.Dispose()}
  $null=Desktop;Preserved 'busy rollback'
  Check ([IO.File]::ReadAllText((Join-Path $env:LOCALAPPDATA 'vaultsyncd/zip-owner.json'))-eq $script:ownerBefore) 'rollback original ZIP ownership'
  $m=Get-Content (Join-Path $script:program 'installed-files.json') -Raw|ConvertFrom-Json
  Check ($m.source.version-eq '0.1.0') 'rollback original program manifest'
  Close-Desktop;Exe (Join-Path $script:program 'vaultsyncd-zip.exe') @('stop')|Out-Null
  Start-Case 'startup-failure'
  $broken=Join-Path $PSScriptRoot 'bad-candidate';Expand-Archive (Join-Path $PSScriptRoot 'bad-start.zip') $broken
  Exe (Join-Path $broken 'vaultsyncd-agent.exe') @('--auto-update','--program-dir',$script:program) 1|Out-Null
  $null=Desktop;Preserved 'startup failure rollback'
  Check ([IO.File]::ReadAllText((Join-Path $env:LOCALAPPDATA 'vaultsyncd/zip-owner.json'))-eq $script:ownerBefore) 'startup failure restores owner'
  Close-Desktop
  # Stop at the durable prepared-trial boundary, then use the actual recovery entry.
  Exe (Join-Path $candidate 'vaultsyncd-agent.exe') @('--maintenance','auto-upgrade','--program-dir',$script:program)|Out-Null
  Check (Test-Path (Join-Path $env:LOCALAPPDATA 'vaultsyncd/agent/auto-update.json')) 'interrupted trial keeps recovery plan'
  Exe (Join-Path $script:program 'vaultsyncd-zip.exe') @('start')|Out-Null;$null=Desktop
  $trial=Status;Check (@($trial.result.management.bindings|Where-Object{!$_.paused}).Count-eq 0) 'trial startup keeps all bindings paused'
  Exe (Join-Path $script:program 'vaultsyncd-cli.exe') @('--json','resume','--binding-id','00000000000000000000000000000001') 5|Out-Null
  $trial=Status;Check (@($trial.result.management.bindings|Where-Object{!$_.paused}).Count-eq 0) 'trial refuses writes before health commit'
  Close-Desktop
  Exe (Join-Path $candidate 'vaultsyncd-agent.exe') @('--auto-update-recover','--program-dir',$script:program)|Out-Null
  $null=Desktop;Preserved 'interrupted trial recovery'
  Check ([IO.File]::ReadAllText((Join-Path $env:LOCALAPPDATA 'vaultsyncd/zip-owner.json'))-eq $script:ownerBefore) 'interrupted trial restores owner'
  Close-Desktop;Exe (Join-Path $script:program 'vaultsyncd-zip.exe') @('stop')|Out-Null
  Exe (Join-Path $PSScriptRoot 'agent-tests.exe') @('portable::tests::','--nocapture')|Out-Null
  Exe (Join-Path $PSScriptRoot 'agent-tests.exe') @('maintenance::storage::tests::','--nocapture')|Out-Null
  Exe (Join-Path $PSScriptRoot 'agent-tests.exe') @('maintenance::tests::','--nocapture')|Out-Null
  Exe (Join-Path $PSScriptRoot 'platform-tests.exe') @('native_exclusive_write','--nocapture')|Out-Null
  Write-Json (Join-Path $PSScriptRoot 'result.json') @{ok=$true;checks=$script:checks;dialogs=$script:dialogs;scope='controlled version fixtures, real production Windows PE/NTFS/IPC/WebView; not published release validation'}
  '0'|Set-Content (Join-Path $PSScriptRoot 'exit.txt')
}catch{
  $_|Out-String|Set-Content (Join-Path $PSScriptRoot 'error.txt')
  if($script:program-and (Test-Path $script:program)){
    try{Close-Desktop;Exe (Join-Path $script:program 'vaultsyncd-zip.exe') @('stop')|Out-Null}catch{$_|Out-String|Set-Content (Join-Path $PSScriptRoot 'cleanup-error.txt')}
  }
  '1'|Set-Content (Join-Path $PSScriptRoot 'exit.txt')
}
