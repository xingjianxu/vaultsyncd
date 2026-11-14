param(
  [Parameter(Mandatory=$true)][string]$ExpectedHash,
  [Parameter(Mandatory=$true)][string]$IconHash,
  [switch]$Worker
)
$ErrorActionPreference='Stop'
$OutputEncoding=[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false)
$ProgressPreference='SilentlyContinue'
$exe=Join-Path $PSScriptRoot 'shell-branding-tests.exe'
if((Get-FileHash -Algorithm SHA256 $exe).Hash.ToLowerInvariant() -ne $ExpectedHash){throw 'fixture transport hash mismatch'}
if(!$Worker){
  $task='vaultsyncd-s222-'+[Guid]::NewGuid().ToString('N')
  $action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -STA -ExecutionPolicy Bypass -File `"$PSCommandPath`" -ExpectedHash $ExpectedHash -IconHash $IconHash -Worker"
  $principal=New-ScheduledTaskPrincipal -UserId 'Quickemu' -LogonType Interactive -RunLevel Limited
  try {
    Register-ScheduledTask -TaskName $task -Action $action -Principal $principal | Out-Null
    Start-ScheduledTask -TaskName $task
    $end=(Get-Date).AddSeconds(120)
    while(!(Test-Path (Join-Path $PSScriptRoot 'result.exit'))){
      if((Get-Date) -gt $end){throw 'interactive fixture timeout; preserve run for recovery'}
      Start-Sleep -Milliseconds 200
    }
    Get-Content (Join-Path $PSScriptRoot 'result.log') -Encoding UTF8
    exit ([int](Get-Content (Join-Path $PSScriptRoot 'result.exit')))
  } finally {Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue}
}
Start-Transcript -Path (Join-Path $PSScriptRoot 'result.log') -Force | Out-Null
$result=1
$window=$null
try {
  if((Get-Process -Id $PID).SessionId -eq 0){throw 'interactive Explorer required'}
  $root='C:\vaultsyncd-lab\s222-'+[Guid]::NewGuid().ToString('N')
  $env:VAULTSYNCD_S222_ROOT=$root
  $bindings=@(1..3 | ForEach-Object {[Guid]::NewGuid().ToString('N')})
  $env:VAULTSYNCD_S222_BINDINGS=$bindings -join ','
  $env:VAULTSYNCD_S222_ACTION='register'
  & $exe --exact native_shell_branding_fixture --nocapture --test-threads=1
  if($LASTEXITCODE -ne 0){throw 'native register fixture failed'}
  $null=[Windows.Storage.Provider.StorageProviderSyncRootManager,Windows.Storage.Provider,ContentType=WindowsRuntime]
  $sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value
  $ids=@($bindings | ForEach-Object {"vaultsyncd!$sid!$_"})
  $names=@('资料, 项目 @alpha','工作文档','资料, 项目 @alpha')
  function Roots { @([Windows.Storage.Provider.StorageProviderSyncRootManager]::GetCurrentSyncRoots() | Where-Object {$_.Id -in $ids}) }
  function Check-Roots {
    $roots=Roots
    if($roots.Count -ne 3){throw "expected 3 roots, got $($roots.Count)"}
    foreach($i in 0..2){
      $info=$roots | Where-Object {$_.Id -eq $ids[$i]}
      if($info.DisplayNameResource -cne $names[$i]){throw "wrong display name: $($info.DisplayNameResource)"}
      $icon=[string]$info.IconResource
      # IconResource accepts a standalone ICO or an indexed/quoted resource path.
      $iconPath=($icon -replace ',0$','').Trim('"')
      if(!(Test-Path -LiteralPath $iconPath)){throw "missing durable icon: $icon"}
      if($iconPath.StartsWith($PSScriptRoot,[StringComparison]::OrdinalIgnoreCase) -or $iconPath.StartsWith($root,[StringComparison]::OrdinalIgnoreCase)){throw 'icon depends on portable/sync directory'}
      if((Get-FileHash -LiteralPath $iconPath -Algorithm SHA256).Hash.ToLowerInvariant() -ne $IconHash){throw 'icon differs from product ICO'}
      Add-Type -AssemblyName System.Drawing
      $decoded=New-Object Drawing.Icon($iconPath,32,32)
      $pixels=$decoded.ToBitmap()
      try {$pixels.Save((Join-Path $PSScriptRoot "icon-$i.png"),[Drawing.Imaging.ImageFormat]::Png)} finally {$pixels.Dispose();$decoded.Dispose()}
    }
    $roots | Select-Object Id,DisplayNameResource,IconResource | ConvertTo-Json | Set-Content -Encoding UTF8 (Join-Path $PSScriptRoot 'roots.json')
  }
  Check-Roots
  $iconBefore=(Roots | Select-Object -First 1).IconResource
  # Downgrade only display metadata; keep IDs, paths, policies and contexts intact.
  foreach($info in (Roots)){
    $info.DisplayNameResource='vaultsyncd'
    $info.IconResource='%SystemRoot%\system32\imageres.dll,-1040'
    [Windows.Storage.Provider.StorageProviderSyncRootManager]::Register($info)
  }
  $shell=New-Object -ComObject Shell.Application
  $prior=@($shell.Windows() | ForEach-Object {$_.HWND})
  $path=Join-Path $root $names[0]
  Start-Process explorer.exe -ArgumentList ('/n,"'+$path+'"')
  $end=(Get-Date).AddSeconds(20)
  do {
    Start-Sleep -Milliseconds 200
    $window=$shell.Windows() | Where-Object {$_.HWND -notin $prior -and $_.Document.Folder.Self.Path -eq $path} | Select-Object -First 1
  } while(!$window -and (Get-Date) -lt $end)
  if(!$window){throw 'fixture Explorer window missing'}
  $window.Left=10;$window.Top=10;$window.Width=1200;$window.Height=760
  Add-Type -AssemblyName Accessibility,System.Windows.Forms
  Add-Type -ReferencedAssemblies ([Accessibility.IAccessible].Assembly.Location) -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class S222Window {
 [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
 [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
 [DllImport("user32.dll")] static extern bool EnumChildWindows(IntPtr hwnd, EnumWindow callback, IntPtr data);
 delegate bool EnumWindow(IntPtr hwnd, IntPtr data);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)] static extern int GetClassName(IntPtr hwnd,System.Text.StringBuilder text,int count);
 [DllImport("user32.dll")] static extern IntPtr SendMessage(IntPtr hwnd,uint message,IntPtr wparam,IntPtr lparam);
 [DllImport("oleacc.dll")] static extern int AccessibleObjectFromWindow(IntPtr hwnd,uint id,ref Guid iid,[MarshalAs(UnmanagedType.Interface)] out Accessibility.IAccessible accessible);
 [DllImport("oleacc.dll")] static extern int AccessibleChildren(Accessibility.IAccessible accessible,int start,int count,[Out,MarshalAs(UnmanagedType.LPArray,SizeParamIndex=2)] object[] children,out int obtained);
 public static string[] NavigationNames(string reveal) {
   var iid=new Guid("618736e0-3c3d-11cf-810c-00aa00389b71");
   Accessibility.IAccessible accessible;
   Marshal.ThrowExceptionForHR(AccessibleObjectFromWindow(TreeHandle,0xfffffffc,ref iid,out accessible));
   var children=new object[accessible.accChildCount]; int obtained;
   Marshal.ThrowExceptionForHR(AccessibleChildren(accessible,0,children.Length,children,out obtained));
   var names=new System.Collections.Generic.List<string>();
   for(int i=0;i<obtained;i++) {
     var child=children[i];
     string name=child is int ? accessible.get_accName(child) : ((Accessibility.IAccessible)child).get_accName(0);
     names.Add(name);
     if(name==reveal && child is int) {
       var item=SendMessage(TreeHandle,0x112a,new IntPtr((int)child),IntPtr.Zero);
       if(item!=IntPtr.Zero) SendMessage(TreeHandle,0x1114,IntPtr.Zero,item);
     }
   }
   return names.ToArray();
 }
 public static IntPtr TreeHandle;
 public static string ScrollNavigationToTop(IntPtr hwnd) {
   var classes=new System.Text.StringBuilder();
   EnumChildWindows(hwnd,delegate(IntPtr child,IntPtr data) {
     var name=new System.Text.StringBuilder(256);GetClassName(child,name,256);
     classes.AppendLine(name.ToString());
     if(name.ToString()=="SysTreeView32") {
       TreeHandle=child;
       SendMessage(child,0x115,new IntPtr(6),IntPtr.Zero);
     }
     return true;
   },IntPtr.Zero);
   return classes.ToString();
 }
}
'@
  function Capture([string]$name){
    [void][S222Window]::SetForegroundWindow([IntPtr]$window.HWND)
    Start-Sleep -Milliseconds 500
    if([S222Window]::GetForegroundWindow() -ne [IntPtr]$window.HWND){throw 'fixture window obscured'}
    $screen=[Windows.Forms.Screen]::PrimaryScreen.Bounds
    $bitmap=New-Object Drawing.Bitmap($screen.Width,$screen.Height)
    $graphics=[Drawing.Graphics]::FromImage($bitmap)
    try {
      $graphics.CopyFromScreen($screen.Location,[Drawing.Point]::Empty,$screen.Size)
      $bitmap.Save((Join-Path $PSScriptRoot ($name+'.png')),[Drawing.Imaging.ImageFormat]::Png)
    } finally {$graphics.Dispose();$bitmap.Dispose()}
  }
  [S222Window]::ScrollNavigationToTop([IntPtr]$window.HWND) | Set-Content (Join-Path $PSScriptRoot 'window-classes.txt')
  Capture 'old-registration'
  # Invoke from a second portable program location and a new process.
  $moved=Join-Path $PSScriptRoot 'moved-program'
  New-Item -ItemType Directory -Path $moved | Out-Null
  Copy-Item -LiteralPath $exe -Destination (Join-Path $moved 'shell-branding-tests.exe')
  & (Join-Path $moved 'shell-branding-tests.exe') --exact native_shell_branding_fixture --nocapture --test-threads=1
  if($LASTEXITCODE -ne 0){throw 'old-registration refresh failed'}
  Check-Roots
  if((Roots | Select-Object -First 1).IconResource -cne $iconBefore){throw 'portable move changed icon resource'}
  [void][S222Window]::ScrollNavigationToTop([IntPtr]$window.HWND)
  $end=(Get-Date).AddSeconds(15)
  do {
    # The classic namespace tree exposes MSAA children but no UIA descendants
    # on this Windows 11 build. Read the actual tree, not title/address labels.
    $treeNames=@([S222Window]::NavigationNames($names[0]))
    if(($treeNames -ccontains $names[0]) -and ($treeNames -ccontains $names[1])){break}
    Start-Sleep -Milliseconds 300
  } while((Get-Date) -lt $end)
  ConvertTo-Json -InputObject @($treeNames) | Set-Content -Encoding UTF8 (Join-Path $PSScriptRoot 'navigation-names.json')
  Capture 'updated-registration'
  [void][S222Window]::NavigationNames($names[1])
  Capture 'updated-second-registration'
  if(!($treeNames -ccontains $names[0]) -or !($treeNames -ccontains $names[1])){throw 'open Explorer navigation did not refresh expected folder names'}
  & $exe --exact native_shell_branding_fixture --nocapture --test-threads=1
  if($LASTEXITCODE -ne 0){throw 'repeat registration failed'}
  Check-Roots
  Write-Output 'check=pass three roots Unicode comma at-sign duplicate basename old-registration update portable move persistent logo repeat registration and open Explorer navigation'
  $result=0
} catch {Write-Output ($_ | Out-String)} finally {
  if($window){$window.Quit()}
  if($root -and (Test-Path -LiteralPath $root)){
    $env:VAULTSYNCD_S222_ACTION='cleanup'
    & $exe --exact native_shell_branding_fixture --nocapture --test-threads=1
    if($LASTEXITCODE -eq 0){Remove-Item -LiteralPath $root -Recurse -Force} else {$result=1;Write-Output "cleanup failed; preserve $root"}
  }
  Stop-Transcript | Out-Null
  $result | Set-Content (Join-Path $PSScriptRoot 'result.exit')
}
exit $result
