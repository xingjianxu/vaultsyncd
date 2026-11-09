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
  Write-Output 'RESTORED prior automatic logon; retained user, Vault/state/cache and recovery evidence preserved'
  exit
}


try{
  $identity=[Security.Principal.WindowsIdentity]::GetCurrent()
  $user=Get-Content (Join-Path $public 'test-user.json') -Raw|ConvertFrom-Json
  if($identity.User.Value-ne $user.sid){throw 'wrong retained user'}
  if((New-Object Security.Principal.WindowsPrincipal($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)){throw 'standard user required'}
  $fixture=Get-Content (Join-Path $public 'fixture.json') -Raw|ConvertFrom-Json
  $prior=Get-Content (Join-Path $public 'prior.json') -Raw -Encoding UTF8|ConvertFrom-Json
  $program=(Get-ItemProperty 'HKCU:\Software\vaultsyncd').InstallDir
  $agentProfile=Join-Path $env:LOCALAPPDATA 'vaultsyncd\agent'
  $cli=Join-Path $program 'vaultsyncd-cli.exe'
  $local=$prior.enroll.local;$cache=$prior.enroll.cache;$binding=$prior.enroll.binding_id
  $oldContent=$prior.ready.old_hashes
  $script:beforeHashes=@{};foreach($item in $prior.dirty.hashes.PSObject.Properties){$script:beforeHashes[$item.Name]=$item.Value}
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
 public static int Vault(string p){IntPtr b=Marshal.AllocHGlobal(8192);try{uint used;return CfGetSyncRootInfoByPath(p,0,b,8192,out used);}finally{Marshal.FreeHGlobal(b);}}
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

  $initial=Wait-Agent;$null=Check-Installed $fixture.source_commit
  if($initial.agent_version-ne '0.1.1'){throw 'actual tested 0.1.1 package required'}
  $dirty=Files|Where-Object path -eq 'dirty.txt';if(!$dirty.dirty){throw 'original unconfirmed dirty lost'}
  $answer=@{ok=$true;status=$initial;hashes=(Check-Content);cloud_metadata=([S100Native]::Observe((Join-Path $local 'cloud-only.bin')));vault_hresult=([S100Native]::Vault($local));materialization_source='verified existing complete cache; no network success claimed'}
  Write-Json (Join-Path $results 'ready.json') $answer
  $end=(Get-Date).AddMinutes(20)
  while((Get-Date)-lt $end){
    $request=Join-Path $results 'request.json';if(!(Test-Path $request)){Start-Sleep -Milliseconds 150;continue}
    $action=Get-Content $request -Raw -Encoding UTF8|ConvertFrom-Json;Remove-Item $request
    $answer=@{id=$action.id;action=$action.action;ok=$true}
    switch($action.action){
      'materialize-cache' {
        $source=$fixture.cache_file;$target=Join-Path $local 'cloud-only.bin'
        if((Get-FileHash $source).Hash.ToLowerInvariant()-ne $fixture.cache_sha256 -or (Get-Item $source).Length-ne $fixture.cache_bytes){throw 'cached fixed-version bytes are not verified complete'}
        $answer.before_metadata=[S100Native]::Observe($target);$answer.before_dirty=Files|Where-Object path -eq 'dirty.txt'
        $null=Invoke-Cli @('--json','pause','--binding-id',$binding)
        if($answer.before_metadata[0]-eq 1 -and $answer.before_metadata[1]-eq 0){
          Copy-Item -LiteralPath $source -Destination $target -Force
          $answer.restore_performed_this_run=$true
        }else{
          if((Get-FileHash $target).Hash.ToLowerInvariant()-ne $fixture.cache_sha256){throw 'existing materialized file differs; do not overwrite unconfirmed contents'}
          $answer.restore_performed_this_run=$false
          $answer.restore_completed_by_prior_recorded_manual_attempt=$true
        }
        $answer.after_metadata=[S100Native]::Observe($target)
        $hash=(Get-FileHash $target).Hash.ToLowerInvariant();if($hash-ne $fixture.cache_sha256){throw 'manual cache restore changed bytes'}
        if($answer.after_metadata[2]-ne $fixture.cache_bytes -or ($answer.after_metadata[0]-eq 1 -and $answer.after_metadata[1]-lt $fixture.cache_bytes)){throw 'actual physical completeness absent'}
        $script:beforeHashes[$target]=$hash
        $answer.cache=@{path=$source;bytes=$fixture.cache_bytes;sha256=$fixture.cache_sha256};$answer.hashes=Check-Content
        $null=Invoke-Cli @('--json','resume','--binding-id',$binding)
        $s=Poll 'original dirty remains unconfirmed' {$s=Status;$b=$s.bindings|Where-Object binding_id -eq $binding;if($b.dirty-ne '0'){return $s}} 60
        $answer.status=$s;$answer.cloud=Files|Where-Object path -eq 'cloud-only.bin';$answer.original_dirty=Files|Where-Object path -eq 'dirty.txt'
        if(!$answer.original_dirty.dirty){throw 'manual cache restore cleared original dirty'}
        $meta=@();foreach($path in $script:beforeHashes.Keys){$meta+=@{path=$path;metadata=[S100Native]::Observe($path)}}
        $answer.pre_unregister_metadata=$meta
        if(!@($meta|Where-Object{$_.metadata[0]-eq 1 -and $_.metadata[1]-ge $_.metadata[2]}).Count){throw 'actual fully hydrated placeholders required to exercise revert'}
      }
      'provider-uninstall' {
        $answer.exit_code=Run-Uninstall '/S /PROVIDER' 0;$answer.hashes=Check-Content
        $answer.cloud_metadata=[S100Native]::Observe((Join-Path $local 'cloud-only.bin'));$answer.vault_hresult=[S100Native]::Vault($local)
        if($answer.cloud_metadata[0]-ne 0 -or $answer.vault_hresult-ge 0 -or (Test-Path $cli)){throw 'provider removal was not actual ordinary files/unregistered/program removal'}
        $answer.state_retained=Test-Path (Join-Path $agentProfile 'state\client.sqlite');$answer.cache_retained=Test-Path $cache
        $answer.provider_reports=@(Get-ChildItem (Join-Path $agentProfile 'maintenance') -Filter provider-removal.json -Recurse|ForEach-Object{Get-Content $_.FullName -Raw -Encoding UTF8|ConvertFrom-Json})
        $answer.exports=Exports
        $exportCopy=Join-Path $results 'recovery-exports';New-Item -ItemType Directory $exportCopy|Out-Null
        foreach($export in $answer.exports){Copy-Item $export.directory (Join-Path $exportCopy ([Guid]::NewGuid().ToString('N'))) -Recurse}
      }
      'finish' {$answer.processes=@(Get-CimInstance Win32_Process|Where-Object{$_.Name-eq 'vaultsyncd-agent.exe'}|Select ProcessId,ExecutablePath,CommandLine)}
      default{throw 'unknown continuation action'}
    }
    Write-Json (Join-Path $results ($action.action+'.json')) $answer
    Write-Json (Join-Path $results 'response.json') $answer
    if($action.action-eq 'finish'){break}
  }
}catch{Write-Json (Join-Path $results 'worker-error.json') @{error=($_|Out-String);action=$action.action};exit 1}
