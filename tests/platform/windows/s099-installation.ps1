param([ValidateSet('Prepare','Worker','Restore')][string]$Mode)
$ErrorActionPreference='Stop'
[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false)
$public=$PSScriptRoot
if($public-notmatch '^C:\\vaultsyncd-lab\\s099-[a-zA-Z0-9-]+$'){throw 'isolated S099 run directory required'}
$results=Join-Path $public 'results'
$private=Join-Path 'C:\ProgramData\vaultsyncd-lab-private' (Split-Path $public -Leaf)
$logonPath='SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
function Write-Json($path,$value){[IO.File]::WriteAllText($path,($value|ConvertTo-Json -Depth 15),(New-Object Text.UTF8Encoding($false)))}
if($Mode-eq 'Prepare'){
  if((Get-Volume -DriveLetter C).FileSystem-ne 'NTFS'){throw 'native installation validation requires NTFS'}
  $hashes=Get-Content (Join-Path $public 'hashes.json') -Raw|ConvertFrom-Json
  foreach($item in $hashes.PSObject.Properties){if((Get-FileHash (Join-Path $public $item.Name) -Algorithm SHA256).Hash.ToLowerInvariant()-ne $item.Value){throw 'setup/script transport hash mismatch'}}
  $username='vs_s099_'+[Guid]::NewGuid().ToString('N').Substring(0,6)
  $password='VsS099!'+[Guid]::NewGuid().ToString('N')
  New-Item -ItemType Directory -Path $private -Force|Out-Null
  & icacls $private /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' |Out-Null
  if($LASTEXITCODE-ne 0){throw 'private backup ACL failed'}
  $key=[Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($logonPath,$true)
  $saved=@{}
  foreach($name in @('AutoAdminLogon','DefaultUserName','DefaultDomainName','DefaultPassword','AutoLogonCount','ForceAutoLogon')){
    if($key.GetValueNames()-contains $name){$saved[$name]=@{present=$true;kind=[int]$key.GetValueKind($name);value=$key.GetValue($name)}}else{$saved[$name]=@{present=$false}}
  }
  Write-Json (Join-Path $private 'prior-logon.json') $saved
  $user=New-LocalUser -Name $username -Password (ConvertTo-SecureString $password -AsPlainText -Force) -PasswordNeverExpires -UserMayNotChangePassword
  Add-LocalGroupMember -SID 'S-1-5-32-545' -Member $username
  Write-Json (Join-Path $private 'test-user.json') @{username=$username;password=$password;sid=$user.SID.Value}
  New-Item -ItemType Directory $results|Out-Null
  & icacls $public /grant ($username+':(OI)(CI)RX')|Out-Null
  if($LASTEXITCODE-ne 0){throw 'test user read ACL failed'}
  & icacls $results /grant ($username+':(OI)(CI)M')|Out-Null
  if($LASTEXITCODE-ne 0){throw 'test result ACL failed'}
  $task='vaultsyncd-s099-'+[Guid]::NewGuid().ToString('N')
  $action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Mode Worker"
  $principal=New-ScheduledTaskPrincipal -UserId ($env:COMPUTERNAME+'\'+$username) -LogonType Interactive -RunLevel Limited
  $trigger=New-ScheduledTaskTrigger -AtLogOn -User ($env:COMPUTERNAME+'\'+$username)
  Register-ScheduledTask -TaskName $task -Action $action -Principal $principal -Trigger $trigger|Out-Null
  Write-Json (Join-Path $public 'test-user.json') @{username=$username;sid=$user.SID.Value;task=$task}
  Write-Json (Join-Path $public 'resources-before.json') @{boot=(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToString('o');processes=@(Get-Process|Select-Object Id,ProcessName,SessionId)}
  [IO.File]::WriteAllText((Join-Path $results 'phase.txt'),'install')
  $key.SetValue('DefaultUserName',$username);$key.SetValue('DefaultDomainName',$env:COMPUTERNAME);$key.SetValue('DefaultPassword',$password);$key.SetValue('AutoAdminLogon','1');$key.SetValue('ForceAutoLogon','1')
  if($key.GetValueNames()-contains 'AutoLogonCount'){$key.DeleteValue('AutoLogonCount')}
  $key.Close()
  Write-Output 'PREPARED isolated standard user and private logon backup (secrets not exported)'
  exit
}
if($Mode-eq 'Restore'){
  if(Test-Path (Join-Path $private 'prior-logon.json')){
    $saved=Get-Content (Join-Path $private 'prior-logon.json') -Raw|ConvertFrom-Json
    $key=[Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($logonPath,$true)
    foreach($property in $saved.PSObject.Properties){$name=$property.Name;$item=$property.Value;if($item.present){$key.SetValue($name,$item.value,[Microsoft.Win32.RegistryValueKind]$item.kind)}elseif($key.GetValueNames()-contains $name){$key.DeleteValue($name)}}
    foreach($property in $saved.PSObject.Properties){$name=$property.Name;$item=$property.Value;if($item.present){if($key.GetValueKind($name)-ne [Microsoft.Win32.RegistryValueKind]$item.kind -or $key.GetValue($name).ToString()-ne $item.value.ToString()){throw 'prior automatic logon restoration validation failed'}}elseif($key.GetValueNames()-contains $name){throw 'prior automatic logon absent-value restoration failed'}}
    $key.Close()
    Remove-Item (Join-Path $private 'prior-logon.json')
  }
  if(Test-Path (Join-Path $public 'test-user.json')){
    $user=Get-Content (Join-Path $public 'test-user.json') -Raw|ConvertFrom-Json
    if(Get-ScheduledTask -TaskName $user.task -ErrorAction SilentlyContinue){Unregister-ScheduledTask -TaskName $user.task -Confirm:$false}
  }
  Write-Output 'RESTORED prior automatic logon; test account, installed application and data retained'
  exit
}

# This worker runs only in the newly logged-on standard user's real interactive session.
try{
  $identity=[Security.Principal.WindowsIdentity]::GetCurrent()
  $user=Get-Content (Join-Path $public 'test-user.json') -Raw|ConvertFrom-Json
  if($identity.User.Value-ne $user.sid){throw 'wrong user session'}
  if((New-Object Security.Principal.WindowsPrincipal($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)){throw 'installation validation must be standard user'}
  $fixture=Get-Content (Join-Path $public 'fixture.json') -Raw|ConvertFrom-Json
  $phase=[IO.File]::ReadAllText((Join-Path $results 'phase.txt'))
  $program=Join-Path $env:LOCALAPPDATA 'Programs\同步 客户端'
  $agentProfile=Join-Path $env:LOCALAPPDATA 'vaultsyncd\agent'
  $local=Join-Path $env:USERPROFILE 'Documents\同步 样本'
  $cli=Join-Path $program 'vaultsyncd-cli.exe'
  function Read-Status {
    $text=& $cli --json status
    if($LASTEXITCODE-ne 0){throw 'installed CLI IPC status failed'}
    ($text|ConvertFrom-Json).result
  }
  function Agent-Process {
    $items=@(Get-CimInstance Win32_Process -Filter "Name='vaultsyncd-agent.exe'"|Where-Object{$_.ExecutablePath-eq (Join-Path $program 'vaultsyncd-agent.exe') -and $_.CommandLine.Contains($agentProfile)})
    if($items.Count-ne 1){throw 'expected exactly one installed agent for this profile'}
    $items[0]
  }
  function Wait-Agent {
    $end=(Get-Date).AddSeconds(180)
    do{try{$script:agent=Agent-Process;$script:status=Read-Status;return}catch{if((Get-Date)-ge $end){throw};Start-Sleep -Milliseconds 300}}while($true)
  }
  if($phase-eq 'install'){
    if(Test-Path $program){throw 'program directory was not clean'}
    if(Test-Path $agentProfile){throw 'profile directory was not clean'}
    if(Get-ItemProperty 'HKCU:\Software\vaultsyncd' -ErrorAction SilentlyContinue){throw 'installation registration was not clean'}
    $hashes=Get-Content (Join-Path $public 'hashes.json') -Raw|ConvertFrom-Json
    foreach($item in $hashes.PSObject.Properties){if((Get-FileHash (Join-Path $public $item.Name) -Algorithm SHA256).Hash.ToLowerInvariant()-ne $item.Value){throw 'installer transport hash mismatch'}}
    $installer=Join-Path $public $fixture.installer
    if((Get-AuthenticodeSignature $installer).Status.ToString()-ne 'NotSigned'){throw 'expected honestly labeled unsigned installer'}
    $setup=Start-Process $installer -ArgumentList ('/S /D='+$program) -PassThru
    $null=$setup.Handle
    if(!$setup.WaitForExit(120000)){throw 'installer timeout'}
    $setup.WaitForExit();$setup.Refresh();if($setup.ExitCode-ne 0){throw ('installer exit='+$setup.ExitCode)}
    $manifest=Get-Content (Join-Path $program 'installed-files.json') -Raw -Encoding UTF8|ConvertFrom-Json
    foreach($file in $manifest.files){if((Get-FileHash (Join-Path $program $file.path) -Algorithm SHA256).Hash.ToLowerInvariant()-ne $file.sha256){throw 'installed payload hash mismatch'}}
    if($manifest.source.commit-ne $fixture.source_commit -or $manifest.signature_status-ne 'unsigned-test'){throw 'installed provenance mismatch'}
    $run=(Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run').'vaultsyncd-agent'
    if($run-ne ('"'+(Join-Path $program 'vaultsyncd-session.exe')+'"')){throw 'wrong user startup registration'}
    $shortcut=Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\vaultsyncd.lnk'
    $shell=New-Object -ComObject Shell.Application
    $folder=$shell.NameSpace((Split-Path $shortcut -Parent));$item=$folder.ParseName((Split-Path $shortcut -Leaf))
    if($item.ExtendedProperty('System.AppUserModel.ID')-ne 'org.vaultsyncd.desktop'){throw 'shortcut AUMID mismatch'}
    Wait-Agent
    $firstAgentId=$agent.ProcessId
    New-Item -ItemType Directory -Path $local|Out-Null
    $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=19299 --remote-debugging-address=127.0.0.1'
    $desktop=Start-Process (Join-Path $program 'vaultsyncd-desktop.exe') -PassThru
    Write-Json (Join-Path $results 'install-ready.json') @{ok=$true;username=$env:USERNAME;sid=$identity.User.Value;standard_user=$true;program=$program;profile=$agentProfile;local=$local;ca=(Join-Path $public 'ca.pem');agent_pid=$firstAgentId;gui_pid=$desktop.Id;cli_status=$status;source_commit=$manifest.source.commit;webview2=(Get-ItemProperty 'HKCU:\Software\vaultsyncd').WebView2Version;startup=$run;shortcut_aumid=$item.ExtendedProperty('System.AppUserModel.ID')}
  }elseif($phase-eq 'after-reboot'){
    # No launcher call here: only the installed HKCU Run entry may start the agent.
    Wait-Agent
    $prior=Get-Content (Join-Path $results 'before-reboot.json') -Raw -Encoding UTF8|ConvertFrom-Json
    $firstAgentId=$agent.ProcessId
    $config=Get-Content (Join-Path $agentProfile 'agent.json') -Raw -Encoding UTF8|ConvertFrom-Json
    if($config.device_id-ne $prior.device_id){throw 'device identity changed across restart'}
    if($status.snapshot_version.Split(':')[0]-eq $prior.cli_status.snapshot_version.Split(':')[0]){throw 'agent did not restart'}
    if(!(Test-Path (Join-Path $local 'after-gui-close.txt'))){throw 'saved client data missing after restart'}
    [IO.File]::WriteAllText((Join-Path $local 'after-reboot.txt'),'S099 sync after real guest reboot',(New-Object Text.UTF8Encoding($false)))
    $desktop=$null
    Write-Json (Join-Path $results 'reboot-ready.json') @{ok=$true;username=$env:USERNAME;sid=$identity.User.Value;program=$program;profile=$agentProfile;local=$local;agent_pid=$firstAgentId;cli_status=$status;boot=(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToString('o');device_id=$config.device_id;started_only_by_installed_run=$true}
  }else{throw 'unknown native validation phase'}
  $end=(Get-Date).AddMinutes(15)
  while((Get-Date)-lt $end){
    $request=Join-Path $results 'request.json'
    if(!(Test-Path $request)){Start-Sleep -Milliseconds 150;continue}
    $action=Get-Content $request -Raw -Encoding UTF8|ConvertFrom-Json;Remove-Item $request
    $answer=@{id=$action.id;action=$action.action;ok=$true}
    switch($action.action){
      'status' {$answer.status=Read-Status;$answer.agent_pid=(Agent-Process).ProcessId}
      'close-gui' {
        if(!$desktop){throw 'no owned GUI'}
        $desktop.Refresh();if(!$desktop.HasExited){$closed=$desktop.CloseMainWindow();if(!$closed){throw 'GUI close request rejected'};$null=$desktop.WaitForExit(15000);$desktop.Refresh()}
        if(!$desktop.HasExited -and $desktop.MainWindowHandle-ne [IntPtr]::Zero){throw 'GUI main window still open'}
        if((Agent-Process).ProcessId-ne $firstAgentId){throw 'GUI closure stopped or replaced agent'}
        [IO.File]::WriteAllText((Join-Path $local 'after-gui-close.txt'),'S099 sync continues after GUI closes',(New-Object Text.UTF8Encoding($false)))
        $answer.gui_closed=$true;$answer.agent_pid=$firstAgentId
      }
      'prepare-reboot' {
        $config=Get-Content (Join-Path $agentProfile 'agent.json') -Raw -Encoding UTF8|ConvertFrom-Json
        $answer.cli_status=Read-Status;$answer.device_id=$config.device_id;$answer.agent_pid=(Agent-Process).ProcessId
        Write-Json (Join-Path $results 'before-reboot.json') $answer
        [IO.File]::WriteAllText((Join-Path $results 'phase.txt'),'after-reboot')
      }
      'finish' {$answer.status=Read-Status;$answer.agent_pid=(Agent-Process).ProcessId}
      default {throw 'unknown fixed test action'}
    }
    Write-Json (Join-Path $results 'response.json') $answer
    if($action.action-eq 'prepare-reboot' -or $action.action-eq 'finish'){break}
  }
}catch{Write-Json (Join-Path $results 'worker-error.json') @{error=($_|Out-String);phase=$phase};exit 1}
