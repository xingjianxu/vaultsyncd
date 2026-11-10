param([switch]$Worker)
$ErrorActionPreference='Stop'
[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false)
if($PSScriptRoot-notmatch '^C:\\vaultsyncd-lab\\s180-[0-9]+$'){throw 'isolated S180 run required'}
if(!$Worker){
  $hashes=Get-Content (Join-Path $PSScriptRoot 'hashes.json') -Raw|ConvertFrom-Json
  foreach($f in $hashes.PSObject.Properties){if((Get-FileHash (Join-Path $PSScriptRoot $f.Name)).Hash.ToLowerInvariant()-ne $f.Value){throw 'transport hash mismatch'}}
  $task='vaultsyncd-s180-'+[Guid]::NewGuid().ToString('N')
  $action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker"
  $principal=New-ScheduledTaskPrincipal -UserId 'Quickemu' -LogonType Interactive -RunLevel Limited
  try{
    Register-ScheduledTask -TaskName $task -Action $action -Principal $principal|Out-Null
    Start-ScheduledTask -TaskName $task
    $end=(Get-Date).AddSeconds(90)
    while(!(Test-Path (Join-Path $PSScriptRoot 'exit.txt'))){if((Get-Date)-gt $end){throw 'native progress timeout'};Start-Sleep -Milliseconds 200}
    if((Get-Content (Join-Path $PSScriptRoot 'exit.txt'))-ne '0'){Get-Content (Join-Path $PSScriptRoot 'error.txt');exit 1}
    Get-Content (Join-Path $PSScriptRoot 'result.json') -Raw
  }finally{Unregister-ScheduledTask -TaskName $task -Confirm:$false}
  exit
}
try{
Add-Type -TypeDefinition @'
using System;using System.Text;using System.Runtime.InteropServices;
public static class S180Native {
 delegate bool Each(IntPtr h,IntPtr p);
 [DllImport("user32.dll")]static extern bool EnumWindows(Each c,IntPtr p);
 [DllImport("user32.dll")]static extern bool EnumChildWindows(IntPtr h,Each c,IntPtr p);
 [DllImport("user32.dll")]static extern uint GetWindowThreadProcessId(IntPtr h,out uint p);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)]static extern int GetWindowTextW(IntPtr h,StringBuilder s,int n);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)]static extern int GetClassNameW(IntPtr h,StringBuilder s,int n);
 [DllImport("user32.dll")]static extern bool IsWindowVisible(IntPtr h);
 [DllImport("user32.dll")]static extern int GetWindowLongW(IntPtr h,int i);
 [DllImport("user32.dll")]static extern IntPtr SendMessageW(IntPtr h,uint m,IntPtr w,IntPtr l);
 public class State {public long window;public string text="";public bool bar=false,marquee=false;public int position,maximum;}
 public static State Read(uint process){State found=null;EnumWindows((h,p)=>{uint pid;GetWindowThreadProcessId(h,out pid);var title=new StringBuilder(512);GetWindowTextW(h,title,512);if(pid!=process||!IsWindowVisible(h))return true;found=new State();found.window=h.ToInt64();found.text=title.ToString()+"\n";EnumChildWindows(h,(child,unused)=>{var text=new StringBuilder(1024);GetWindowTextW(child,text,1024);found.text+=text.ToString()+"\n";var kind=new StringBuilder(128);GetClassNameW(child,kind,128);if(kind.ToString()=="msctls_progress32"&&IsWindowVisible(child)){found.bar=true;found.marquee=(GetWindowLongW(child,-16)&8)!=0;found.position=(int)SendMessageW(child,0x408,IntPtr.Zero,IntPtr.Zero);found.maximum=(int)SendMessageW(child,0x407,IntPtr.Zero,IntPtr.Zero);}return true;},IntPtr.Zero);return !found.bar;},IntPtr.Zero);return found;}
}
'@
  Add-Type -AssemblyName UIAutomationClient,UIAutomationTypes
  $p=Start-Process -FilePath (Join-Path $PSScriptRoot 'platform-tests.exe') -ArgumentList @('--exact','shell::progress::tests::native_update_progress','--ignored','--nocapture') -PassThru -RedirectStandardOutput (Join-Path $PSScriptRoot 'test.out') -RedirectStandardError (Join-Path $PSScriptRoot 'test.err')
  $null=$p.Handle
  $checks=@();$observed=@()
  foreach($phase in @('正在检查更新…','正在下载更新… 25%','正在下载更新… 100%','正在校验并解压更新…','正在排空同步、备份并安装更新…','更新启动失败，正在恢复旧程序与状态…')){
    $expected=if($phase.Contains('25%')){25}else{100}
    $end=(Get-Date).AddSeconds(12);$state=$null
    do{$state=[S180Native]::Read($p.Id);if($state){$element=[System.Windows.Automation.AutomationElement]::FromHandle([IntPtr]$state.window);if($element){$controls=$element.FindAll([System.Windows.Automation.TreeScope]::Descendants,[System.Windows.Automation.Condition]::TrueCondition);$state.text+=($controls|ForEach-Object {$_.Current.Name})-join "`n"};$observed+=$state};if($state-and $state.text.Contains($phase)-and $state.bar){if(!$phase.Contains("下载")-or ($state.maximum-gt 0-and [Math]::Abs($state.position*100.0/$state.maximum-$expected)-le 1)){break}};Start-Sleep -Milliseconds 50}while((Get-Date)-lt $end-and !$p.HasExited)
    if(!$state-or !$state.bar-or !$state.text.Contains($phase)){throw ('native progress phase missing: '+$phase)}
    if($phase.Contains('下载')){
      if($state.marquee){throw 'download progress must be measurable'}
      if($state.maximum-le 0-or [Math]::Abs($state.position*100.0/$state.maximum-$expected)-gt 1){throw 'native download position differs from actual bytes'}
      if(!$state.text.Contains('0.1.7')-or !$state.text.Contains('字节')){throw 'download version/bytes missing'}
    }elseif(!$state.marquee){throw 'unknown total must use marquee'}
    $checks+=@{phase=$phase;state=$state}
  }
  if(!$p.WaitForExit(15000)){throw 'native progress test timeout'}
  $p.WaitForExit();$p.Refresh()
  if($p.ExitCode-ne 0){throw ('native progress test failed: '+$p.ExitCode)}
  if([S180Native]::Read($p.Id)){throw 'progress window retained after completion'}
  @{ok=$true;checks=$checks;windows=[Environment]::OSVersion.VersionString}|ConvertTo-Json -Depth 8|Set-Content (Join-Path $PSScriptRoot 'result.json') -Encoding UTF8
  '0'|Set-Content (Join-Path $PSScriptRoot 'exit.txt')
}catch{
  $observed|ConvertTo-Json -Depth 6|Set-Content (Join-Path $PSScriptRoot 'observed.json') -Encoding UTF8
  if($p){$null=$p.WaitForExit(30000)}
  $_|Out-String|Set-Content (Join-Path $PSScriptRoot 'error.txt')
  '1'|Set-Content (Join-Path $PSScriptRoot 'exit.txt')
}
