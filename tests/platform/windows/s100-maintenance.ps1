param([ValidateSet('Prepare','Worker','Restore')][string]$Mode)
$ErrorActionPreference='Stop'
[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false)
$public=$PSScriptRoot
if($public-notmatch '^C:\\vaultsyncd-lab\\s100-[a-zA-Z0-9-]+$'){throw 'isolated S100 run directory required'}
$results=Join-Path $public 'results'
$private=Join-Path 'C:\ProgramData\vaultsyncd-lab-private' (Split-Path $public -Leaf)
$logonPath='SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
function Write-Json($path,$value){[IO.File]::WriteAllText($path,($value|ConvertTo-Json -Depth 15),(New-Object Text.UTF8Encoding($false)))}
if($Mode-eq 'Prepare'){
  if((Get-Volume -DriveLetter C).FileSystem-ne 'NTFS'){throw 'native installation validation requires NTFS'}
  $hashes=Get-Content (Join-Path $public 'hashes.json') -Raw|ConvertFrom-Json
  foreach($item in $hashes.PSObject.Properties){if((Get-FileHash (Join-Path $public $item.Name) -Algorithm SHA256).Hash.ToLowerInvariant()-ne $item.Value){throw 'setup/script transport hash mismatch'}}
  $existing=Get-Content 'C:\ProgramData\vaultsyncd-lab-private\s099-1793986849-3965593\test-user.json' -Raw|ConvertFrom-Json
  $username=$existing.username;$password=$existing.password
  $user=Get-LocalUser -Name $username
  if($user.SID.Value-ne $existing.sid -or $username-ne 'vs_s099_4fcc1e'){throw 'retained S099 user identity mismatch'}
  New-Item -ItemType Directory -Path $private -Force|Out-Null
  & icacls $private /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' |Out-Null
  if($LASTEXITCODE-ne 0){throw 'private backup ACL failed'}
  . (Join-Path $public 's099-logon-secrets.ps1')
  [S099LogonSecret]::Backup((Join-Path $private 'prior-lsa.bin'))
  if(Test-Path (Join-Path $private 'prior-logon.json')){throw 'original registry backup already exists'}
  $key=[Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($logonPath,$true)
  $saved=@{}
  foreach($name in @('AutoAdminLogon','DefaultUserName','DefaultDomainName','DefaultPassword','AutoLogonCount','ForceAutoLogon')){
    if($key.GetValueNames()-contains $name){$saved[$name]=@{present=$true;kind=[int]$key.GetValueKind($name);value=$key.GetValue($name)}}else{$saved[$name]=@{present=$false}}
  }
  Write-Json (Join-Path $private 'prior-logon.json') $saved
  New-Item -ItemType Directory $results|Out-Null
  & icacls $public /grant ($username+':(OI)(CI)RX')|Out-Null
  if($LASTEXITCODE-ne 0){throw 'test user read ACL failed'}
  & icacls $results /grant ($username+':(OI)(CI)M')|Out-Null
  if($LASTEXITCODE-ne 0){throw 'test result ACL failed'}
  $task='vaultsyncd-s100-'+[Guid]::NewGuid().ToString('N')
  $action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Mode Worker"
  $principal=New-ScheduledTaskPrincipal -UserId ($env:COMPUTERNAME+'\'+$username) -LogonType Interactive -RunLevel Limited
  $trigger=New-ScheduledTaskTrigger -AtLogOn -User ($env:COMPUTERNAME+'\'+$username)
  Register-ScheduledTask -TaskName $task -Action $action -Principal $principal -Trigger $trigger|Out-Null
  Write-Json (Join-Path $public 'test-user.json') @{username=$username;sid=$user.SID.Value;task=$task}
  Write-Json (Join-Path $public 'resources-before.json') @{boot=(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToString('o');processes=@(Get-Process|Select-Object Id,ProcessName,SessionId)}
  [IO.File]::WriteAllText((Join-Path $results 'phase.txt'),'install')
  [S099LogonSecret]::SetTestPassword($password)
  $key.SetValue('DefaultUserName',$username);$key.SetValue('DefaultDomainName',$env:COMPUTERNAME);$key.SetValue('AutoAdminLogon','1');$key.SetValue('ForceAutoLogon','1')
  if($key.GetValueNames()-contains 'AutoLogonCount'){$key.DeleteValue('AutoLogonCount')}
  $key.Close()
  Write-Output 'PREPARED isolated standard user and private logon backup (secrets not exported)'
  exit
}
if($Mode-eq 'Restore'){
  if(Test-Path (Join-Path $private 'prior-lsa.bin')){
    . (Join-Path $public 's099-logon-secrets.ps1')
    [S099LogonSecret]::Restore((Join-Path $private 'prior-lsa.bin'))
  }
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
  if(Test-Path (Join-Path $private 'prior-lsa.bin')){Remove-Item (Join-Path $private 'prior-lsa.bin')}
  Write-Output 'RESTORED prior automatic logon; retained user, Root/state/cache and recovery evidence preserved'
  exit
}


# The retained S099 standard user's real desktop session; no password reset or offline DB edits.
try{
  $identity=[Security.Principal.WindowsIdentity]::GetCurrent()
  $user=Get-Content (Join-Path $public 'test-user.json') -Raw|ConvertFrom-Json
  if($identity.User.Value-ne $user.sid){throw 'wrong retained user'}
  if((New-Object Security.Principal.WindowsPrincipal($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)){throw 'standard user required'}
  $fixture=Get-Content (Join-Path $public 'fixture.json') -Raw|ConvertFrom-Json
  $program=(Get-ItemProperty 'HKCU:\Software\vaultsyncd').InstallDir
  $agentProfile=Join-Path $env:LOCALAPPDATA 'vaultsyncd\agent'
  $cli=Join-Path $program 'vaultsyncd-cli.exe'
  $local=Join-Path $env:USERPROFILE ('Documents\S100 升级 目录-'+(Split-Path $public -Leaf))
  $cache=Join-Path $env:LOCALAPPDATA ('vaultsyncd\cache-'+(Split-Path $public -Leaf))
  $binding=[Guid]::NewGuid().ToString('N')
  function Invoke-Cli([string[]]$arguments){$text=& $cli @arguments;if($LASTEXITCODE-ne 0){throw ('CLI failed '+($arguments -join ' '))};($text|ConvertFrom-Json).result}
  function Status {Invoke-Cli @('--json','status')}
  function Files {(Invoke-Cli @('--json','files','--binding-id',$binding)).management.files}
  function Poll([string]$label,[scriptblock]$test,[int]$seconds=180){$end=(Get-Date).AddSeconds($seconds);do{try{$value=& $test;if($value){return $value}}catch{$last=$_|Out-String};Start-Sleep -Milliseconds 300}while((Get-Date)-lt $end);throw ('timeout '+$label+' '+$last)}
  function Wait-Agent {Poll 'installed agent' {try{$s=Status;if($s){return $s}}catch{};$null}}
  function Agent-Id {$items=@(Get-CimInstance Win32_Process|Where-Object{$_.Name-eq 'vaultsyncd-agent.exe' -and $_.ExecutablePath-eq (Join-Path $program 'vaultsyncd-agent.exe') -and $_.CommandLine.Contains('--config')});if($items.Count-ne 1){throw 'one exact installed daemon required'};$items[0].ProcessId}
  function Hash-Files {
    $map=@{}
    if($script:beforeHashes -and $script:beforeHashes.Count-gt 0){$paths=@($script:beforeHashes.Keys)}else{$paths=@((Join-Path $local 'downloaded.txt'),(Join-Path $local 'dirty.txt'))+@($oldContent.PSObject.Properties|ForEach-Object{$_.Name})}
    foreach($path in $paths){$map[$path]=(Get-FileHash $path -Algorithm SHA256).Hash.ToLowerInvariant()};$map
  }
  function Check-Content {$actual=Hash-Files;foreach($path in $script:beforeHashes.Keys){if($actual[$path]-ne $script:beforeHashes[$path]){throw ('preserved bytes changed '+$path)}};$actual}
  function Run-Setup([int]$expected){$p=Start-Process (Join-Path $public $fixture.installer) -ArgumentList ('/S /D='+$program) -PassThru;$null=$p.Handle;if(!$p.WaitForExit(240000)){throw 'setup timeout'};$p.WaitForExit();$p.Refresh();if($p.ExitCode-ne $expected){throw ('setup code='+$p.ExitCode+' expected='+$expected)};$p.ExitCode}
  function Copied-Uninstaller([string]$name){$source=Join-Path $program 'uninstall.exe';$copy=Join-Path $results $name;Copy-Item $source $copy;if((Get-FileHash $copy).Hash-ne (Get-FileHash $source).Hash){throw 'uninstaller copy changed'};$copy}
  function Run-Uninstall([string]$flags,[int]$expected){
    # Official NSIS _?= disables asynchronous self-copy; this exact hash copy runs outside INSTDIR.
    $copy=Copied-Uninstaller ('uninstall-'+[Guid]::NewGuid().ToString('N')+'.exe')
    $p=Start-Process $copy -ArgumentList ($flags+' _?='+$program) -PassThru;$null=$p.Handle
    if(!$p.WaitForExit(240000)){throw 'uninstaller timeout'};$p.WaitForExit();$p.Refresh();if($p.ExitCode-ne $expected){throw ('uninstall code='+$p.ExitCode+' expected='+$expected)};$p.ExitCode
  }
  function Check-Installed([string]$commit){$m=Get-Content (Join-Path $program 'installed-files.json') -Raw -Encoding UTF8|ConvertFrom-Json;if($m.source.commit-ne $commit){throw 'installed commit mismatch'};foreach($f in $m.files){if((Get-FileHash (Join-Path $program $f.path)).Hash.ToLowerInvariant()-ne $f.sha256){throw 'installed executable hash mismatch'}};$m}
  function Exports {
    $out=@();foreach($manifestPath in Get-ChildItem (Join-Path $agentProfile 'maintenance') -Filter manifest.json -Recurse){
      $manifest=Get-Content $manifestPath.FullName -Raw -Encoding UTF8|ConvertFrom-Json
      foreach($f in $manifest.files){$path=Join-Path $manifestPath.Directory.FullName $f.name;if((Get-FileHash $path).Hash.ToLowerInvariant()-ne $f.sha256 -or (Get-Item $path).Length-ne $f.size){throw 'recovery package not readable/hash mismatch'}}
      $out+=@{directory=$manifestPath.Directory.FullName;manifest=$manifest}
    };$out
  }
  Add-Type -TypeDefinition @'
using System;using System.Runtime.InteropServices;using System.Text;
public static class S100Native {
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)]static extern IntPtr CreateFileW(string p,uint a,uint s,IntPtr sec,uint d,uint f,IntPtr t);
 [DllImport("kernel32.dll")]static extern bool CloseHandle(IntPtr h);
 [DllImport("kernel32.dll",SetLastError=true)]static extern bool GetFileInformationByHandleEx(IntPtr h,int c,IntPtr b,uint n);
 [DllImport("cldapi.dll")]static extern int CfGetPlaceholderInfo(IntPtr h,int c,IntPtr b,uint n,out uint r);
 [DllImport("cldapi.dll",CharSet=CharSet.Unicode)]static extern int CfGetSyncRootInfoByPath(string p,int c,IntPtr b,uint n,out uint r);
 public static long[] Observe(string p){IntPtr h=CreateFileW(p,0x80,7,IntPtr.Zero,3,0x02200000,IntPtr.Zero);if(h==new IntPtr(-1))throw new System.ComponentModel.Win32Exception();IntPtr b=Marshal.AllocHGlobal(8192);try{if(!GetFileInformationByHandleEx(h,1,b,64))throw new System.ComponentModel.Win32Exception();long size=Marshal.ReadInt64(b,8);if(!GetFileInformationByHandleEx(h,9,b,64))throw new System.ComponentModel.Win32Exception();long attr=(uint)Marshal.ReadInt32(b,0),tag=(uint)Marshal.ReadInt32(b,4);uint used;int hr=CfGetPlaceholderInfo(h,1,b,8192,out used);if(hr==unchecked((int)0x80070178))return new long[]{0,0,size,tag,attr};if(hr<0)throw new Exception("CfGetPlaceholderInfo "+hr.ToString("x"));return new long[]{1,Marshal.ReadInt64(b,0),size,tag,attr};}finally{Marshal.FreeHGlobal(b);CloseHandle(h);}}
 public static int Root(string p){IntPtr b=Marshal.AllocHGlobal(8192);try{uint used;return CfGetSyncRootInfoByPath(p,0,b,8192,out used);}finally{Marshal.FreeHGlobal(b);}}
 delegate bool Window(IntPtr h,IntPtr p);
 [DllImport("user32.dll")]static extern bool EnumWindows(Window c,IntPtr p);
 [DllImport("user32.dll")]static extern bool EnumChildWindows(IntPtr h,Window c,IntPtr p);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)]static extern int GetWindowTextW(IntPtr h,StringBuilder t,int n);
 [DllImport("user32.dll")]static extern IntPtr GetDlgItem(IntPtr h,int id);
 [DllImport("user32.dll")]static extern int GetWindowLongW(IntPtr h,int i);
 [DllImport("user32.dll")]static extern IntPtr SendMessageW(IntPtr h,uint m,IntPtr w,IntPtr l);
 static string Text(IntPtr h){var t=new StringBuilder(4096);GetWindowTextW(h,t,t.Capacity);return t.ToString();}
 public static long Prompt(){long found=0;EnumWindows(delegate(IntPtr h,IntPtr p){EnumChildWindows(h,delegate(IntPtr c,IntPtr q){if(Text(c).Contains("Remove provider registration too?"))found=h.ToInt64();return true;},IntPtr.Zero);return found==0;},IntPtr.Zero);return found;}
 public static bool DefaultNo(long h){return (GetWindowLongW(GetDlgItem(new IntPtr(h),7),-16)&15)==1;}
 public static string PromptText(long h){string result="";EnumChildWindows(new IntPtr(h),delegate(IntPtr c,IntPtr p){string s=Text(c);if(s.Contains("Remove provider registration too?"))result=s;return true;},IntPtr.Zero);return result;}
 public static void No(long h){SendMessageW(GetDlgItem(new IntPtr(h),7),0xf5,IntPtr.Zero,IntPtr.Zero);}
}
'@
  $initial=Wait-Agent
  $oldManifest=Check-Installed 'cf1d174c3bd47b4babc2db053514619b2e733b07'
  if($initial.agent_version-ne '0.1.0'){throw 'actual S099 development client required'}
  $device=(Get-Content (Join-Path $agentProfile 'agent.json') -Raw -Encoding UTF8|ConvertFrom-Json).device_id
  if(@($initial.management.bindings).Count-ne 1 -or $initial.management.bindings[0].local_path-ne (Join-Path $env:USERPROFILE 'Documents\同步 样本')){throw 'retained S099 fixture scope changed'}
  $oldContent=@{};foreach($b in $initial.management.bindings){foreach($leaf in @('server-seed.txt','after-gui-close.txt','after-reboot.txt')){$p=Join-Path $b.local_path $leaf;$oldContent[$p]=(Get-FileHash $p).Hash.ToLowerInvariant()}}
  # Normalize as a JSON-like property bag for the fixed helper; no user content is executed.
  $oldContent=($oldContent|ConvertTo-Json|ConvertFrom-Json)
  Write-Json (Join-Path $results 'ready.json') @{ok=$true;username=$env:USERNAME;sid=$identity.User.Value;source=$oldManifest.source;program=$program;profile=$agentProfile;device_id=$device;initial_status=$initial;old_hashes=$oldContent}
  $end=(Get-Date).AddMinutes(35)
  while((Get-Date)-lt $end){
    $request=Join-Path $results 'request.json';if(!(Test-Path $request)){Start-Sleep -Milliseconds 150;continue}
    $action=Get-Content $request -Raw -Encoding UTF8|ConvertFrom-Json;Remove-Item $request
    $answer=@{id=$action.id;action=$action.action;ok=$true}
    switch($action.action){
      'enroll' {
        $sid=$initial.management.connection.server_id
        $null=Invoke-Cli @('--json','set-connection','--server-id',$sid,'--server-url',$fixture.url,'--ca-file',(Join-Path $public 'ca.pem'))
        $login=$fixture.password|& $cli --json login --username $fixture.login;if($LASTEXITCODE-ne 0){throw 'real fixture login failed'}
        New-Item -ItemType Directory $local,$cache|Out-Null
        $null=Invoke-Cli @('--json','create-binding','--binding-id',$binding,'--root-id',$fixture.root,'--local-path',$local,'--cache-path',$cache,'--mode','full')
        $null=Poll 'initial actual full download' {$b=@((Status).bindings|Where-Object binding_id -eq $binding);$b.Count-eq 1 -and $b[0].synchronized}
        $files=Files;$cloud=$files|Where-Object path -eq 'cloud-only.bin';if(!$cloud.entry_id){throw 'cloud entry identity absent'}
        $null=Invoke-Cli @('--json','mode','--binding-id',$binding,'--mode','on_demand')
        $null=Invoke-Cli @('--json','release','--binding-id',$binding,'--entry-id',$cloud.entry_id,'--operation-id',([Guid]::NewGuid().ToString('N')))
        $observation=[S100Native]::Observe((Join-Path $local 'cloud-only.bin'));if($observation[0]-ne 1 -or $observation[1]-ne 0 -or $observation[2]-ne 1048576){throw 'actual cloud-only placeholder required'}
        $answer.binding_id=$binding;$answer.local=$local;$answer.cache=$cache;$answer.cloud_metadata=$observation;$answer.files=Files;$answer.status=Status
      }
      'dirty' {
        [IO.File]::WriteAllText((Join-Path $local 'dirty.txt'),'S100 unconfirmed dirty bytes',(New-Object Text.UTF8Encoding($false)))
        $s=Poll 'real unconfirmed dirty state' {$s=Status;$b=$s.bindings|Where-Object binding_id -eq $binding;if($b.dirty-ne '0'){return $s}}
        $dirty=Files|Where-Object path -eq 'dirty.txt';if(!$dirty.dirty){throw 'dirty file projection missing'}
        $script:beforeHashes=Hash-Files;$answer.hashes=$script:beforeHashes;$answer.status=$s;$answer.dirty_file=$dirty;$answer.cloud_metadata=[S100Native]::Observe((Join-Path $local 'cloud-only.bin'))
      }
      'upgrade-fail' {
        $locked=[IO.File]::Open($cli,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::Read)
        try{$answer.exit_code=Run-Setup 27}finally{$locked.Dispose()}
        $null=Wait-Agent;$answer.source=(Check-Installed 'cf1d174c3bd47b4babc2db053514619b2e733b07').source;$answer.hashes=Check-Content;$answer.status=Status
        if($answer.status.agent_version-ne '0.1.0'){throw 'failed upgrade did not restore old actual client'}
        if(Test-Path (Join-Path $agentProfile 'maintenance.json')){throw 'failed file replacement did not recover barrier'}
        $answer.rollback_reports=@(Get-ChildItem (Join-Path $agentProfile 'maintenance') -Filter rollback-complete.json -Recurse|ForEach-Object{Get-Content $_.FullName -Raw|ConvertFrom-Json})
        if(!$answer.rollback_reports.Count){throw 'actual offline rollback report absent'}
      }
      'upgrade' {
        $prior=Agent-Id;$answer.exit_code=Run-Setup 0;$s=Wait-Agent;$answer.source=(Check-Installed $fixture.source_commit).source
        if($s.agent_version-ne '0.1.1' -or $s.ipc_protocol.minor-ne 5 -or (Agent-Id)-eq $prior){throw 'new installed agent not running'}
        if((Get-Content (Join-Path $agentProfile 'agent.json') -Raw -Encoding UTF8|ConvertFrom-Json).device_id-ne $device){throw 'upgrade changed DeviceId'}
        $answer.hashes=Check-Content;$answer.cloud_metadata=[S100Native]::Observe((Join-Path $local 'cloud-only.bin'));$answer.status=$s;$answer.agent_pid=Agent-Id;$answer.exports=Exports
        $dirtyHash=$script:beforeHashes[(Join-Path $local 'dirty.txt')];if(!@($answer.exports|ForEach-Object{$_.manifest.files}|Where-Object sha256 -eq $dirtyHash).Count){throw 'latest dirty bytes absent from recovery package'}
      }
      'uninstall-prompt' {
        $copy=Copied-Uninstaller 'uninstall-visible.exe';$script:uninstaller=Start-Process $copy -ArgumentList ('_?='+$program) -PassThru;$null=$script:uninstaller.Handle
        $script:prompt=Poll 'actual native uninstall prompt' {[S100Native]::Prompt()}
        $answer.text=[S100Native]::PromptText($script:prompt);$answer.default_no=[S100Native]::DefaultNo($script:prompt)
        if(!$answer.default_no -or !$answer.text.Contains('cannot provide complete offline files')){throw 'native default/uninstall explanation mismatch'}
      }
      'confirm-program-only' {
        [S100Native]::No($script:prompt)
        $null=Poll 'program-only actual uninstall' {!(Test-Path $cli) -and !(Get-ItemProperty 'HKCU:\Software\vaultsyncd' -ErrorAction SilentlyContinue)}
        $script:uninstaller.Refresh();if(!$script:uninstaller.HasExited){$null=$script:uninstaller.CloseMainWindow();$null=$script:uninstaller.WaitForExit(15000)}
        $answer.hashes=Check-Content;$cloudPath=Join-Path $local 'cloud-only.bin';$observation=[S100Native]::Observe($cloudPath)
        if($observation[0]-ne 1 -or $observation[1]-ne 0 -or [S100Native]::Root($local)-lt 0){throw 'program-only removal destroyed registration/placeholder'}
        $answer.cloud_metadata=$observation;$answer.root_registered=$true
        try{$null=[IO.File]::ReadAllBytes($cloudPath);throw 'cloud-only data unexpectedly readable without provider'}catch{if($_.Exception.Message.Contains('unexpectedly')){throw};$answer.unavailable_hresult=$_.Exception.HResult;$answer.unavailable_type=$_.Exception.GetType().FullName}
        if(!(Test-Path (Join-Path $agentProfile 'state\client.sqlite')) -or !(Test-Path $cache)){throw 'default uninstall removed state/cache'}
        $answer.state_and_cache_retained=$true
      }
      'reinstall' {
        $answer.exit_code=Run-Setup 0;$s=Wait-Agent;$null=Check-Installed $fixture.source_commit
        if(!@($s.management.bindings|Where-Object binding_id -eq $binding).Count -or !$s.management.connection.authenticated){throw 'reinstall lost binding/credentials'}
        $answer.hashes=Check-Content;$answer.cloud_metadata=[S100Native]::Observe((Join-Path $local 'cloud-only.bin'));$answer.status=$s
      }
      'provider-refusal' {
        $answer.exit_code=Run-Uninstall '/S /PROVIDER' 27;$null=Wait-Agent;$null=Check-Installed $fixture.source_commit
        $answer.hashes=Check-Content;$answer.cloud_metadata=[S100Native]::Observe((Join-Path $local 'cloud-only.bin'));$answer.root_hresult=[S100Native]::Root($local)
        if($answer.cloud_metadata[0]-ne 1 -or $answer.cloud_metadata[1]-ne 0 -or $answer.root_hresult-lt 0){throw 'provider refusal did not preserve placeholder/registration'}
      }
      'materialize' {
        # Full-mode tasks intentionally wait for unresolved dirty uploads. Recover a known
        # complete fixed-version cache instead; this is real file I/O, never a residency flag.
        $expected='11030261d987f0966338a7afb2fb76b1503b1683d72ffc4ffacd111bc298722f'
        $source=Get-ChildItem $cache -Filter 'download-*' -File|Where-Object{$_.Length-eq 1048576 -and (Get-FileHash $_.FullName).Hash.ToLowerInvariant()-eq $expected}|Select-Object -First 1
        if(!$source){throw 'verified complete fixed-version cache unavailable'}
        $target=Join-Path $local 'cloud-only.bin';$answer.before_metadata=[S100Native]::Observe($target)
        if($answer.before_metadata[0]-ne 1 -or $answer.before_metadata[1]-ne 0){throw 'clean cloud-only fixture required before manual recovery'}
        $null=Invoke-Cli @('--json','pause','--binding-id',$binding)
        Copy-Item -LiteralPath $source.FullName -Destination $target -Force
        $answer.after_metadata=[S100Native]::Observe($target);$answer.cloud_hash=(Get-FileHash $target).Hash.ToLowerInvariant()
        if($answer.cloud_hash-ne $expected -or $answer.after_metadata[2]-ne 1048576 -or ($answer.after_metadata[0]-eq 1 -and $answer.after_metadata[1]-lt 1048576)){throw 'actual complete cached bytes not recovered'}
        $script:beforeHashes[$target]=$answer.cloud_hash;$answer.hashes=Check-Content
        $null=Invoke-Cli @('--json','resume','--binding-id',$binding)
        $answer.status=Status;$answer.original_dirty=Files|Where-Object path -eq 'dirty.txt'
        if(!$answer.original_dirty.dirty){throw 'manual recovery cleared original unconfirmed dirty'}
        $answer.cache=@{path=$source.FullName;bytes=$source.Length;sha256=$expected}
        $answer.materialization_kind='manual verified complete cache recovery; no network success claimed'
      }
      'provider-uninstall' {
        $answer.exit_code=Run-Uninstall '/S /PROVIDER' 0;$answer.hashes=Check-Content
        $answer.cloud_metadata=[S100Native]::Observe((Join-Path $local 'cloud-only.bin'));$answer.root_hresult=[S100Native]::Root($local)
        if($answer.cloud_metadata[0]-ne 0 -or $answer.root_hresult-ge 0 -or (Test-Path $cli)){throw 'provider removal was not actual ordinary files/unregistered/program removal'}
        $answer.state_retained=Test-Path (Join-Path $agentProfile 'state\client.sqlite');$answer.cache_retained=Test-Path $cache
        $answer.provider_reports=@(Get-ChildItem (Join-Path $agentProfile 'maintenance') -Filter provider-removal.json -Recurse|ForEach-Object{Get-Content $_.FullName -Raw -Encoding UTF8|ConvertFrom-Json})
        $answer.exports=Exports
        $exportCopy=Join-Path $results 'recovery-exports';New-Item -ItemType Directory $exportCopy|Out-Null
        foreach($export in $answer.exports){$name=[Guid]::NewGuid().ToString('N');Copy-Item $export.directory (Join-Path $exportCopy $name) -Recurse}
      }
      'finish' {$answer.processes=@(Get-CimInstance Win32_Process|Where-Object{$_.Name-eq 'vaultsyncd-agent.exe'}|Select ProcessId,ExecutablePath,CommandLine)}
      default {throw 'unknown fixed test action'}
    }
    Write-Json (Join-Path $results ($action.action+'.json')) $answer
    Write-Json (Join-Path $results 'response.json') $answer
    if($action.action-eq 'finish'){break}
  }
}catch{Write-Json (Join-Path $results 'worker-error.json') @{error=($_|Out-String);action=$action.action};exit 1}
