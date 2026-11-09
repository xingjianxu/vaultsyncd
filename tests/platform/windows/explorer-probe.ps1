param(
  [Parameter(Mandatory=$true)][string]$Vault,
  [Parameter(Mandatory=$true)][string]$Evidence,
  [Parameter(Mandatory=$true)][string]$Phase,
  [Parameter(Mandatory=$true)][int]$ExpectedState
)
$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
if($Vault -notmatch '^C:\\vaultsyncd-lab\\s129-driver-[0-9]+\\run-explorer-[0-9]+\\vault$'){throw 'isolated S129 vault required'}
if($Phase -notmatch '^[a-z-]+$'){throw 'unsafe evidence phase'}
if((Get-Process -Id $PID).SessionId -eq 0){throw 'Explorer evidence requires interactive desktop'}
$shell=New-Object -ComObject Shell.Application
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class ExplorerEvidence {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr window,int command);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("kernel32.dll")] public static extern uint SetThreadExecutionState(uint flags);
}
'@
[void][ExplorerEvidence]::SetThreadExecutionState(0x80000003L)
$window=$null
try {
  # Open only this synthetic vault, and close only the window acquired below.
  $prior=@($shell.Windows() | ForEach-Object { $_.HWND })
  Start-Process explorer.exe -ArgumentList ('/n,"'+$Vault+'"')
  $end=(Get-Date).AddSeconds(20)
  do {
    Start-Sleep -Milliseconds 200
    $window=$shell.Windows() | Where-Object { $_.HWND -notin $prior -and $_.Document.Folder.Self.Path -eq $Vault } | Select-Object -First 1
  } while(!$window -and (Get-Date) -lt $end)
  if(!$window){throw 'could not find this run Explorer window'}
  $window.Visible=$true
  $window.Left=20;$window.Top=20;$window.Width=1100;$window.Height=700
  $window.Document.CurrentViewMode=4 # Details; preserve global Explorer settings.
  $folder=$shell.NameSpace($Vault)
  $end=(Get-Date).AddSeconds(20)
  do {
    $item=$folder.ParseName('file.txt')
    $state=$item.ExtendedProperty('System.StorageProviderState')
    if($null -ne $state -and [int]$state -eq $ExpectedState){break}
    Start-Sleep -Milliseconds 200
  } while((Get-Date) -lt $end)
  $values=[ordered]@{
    phase=$Phase;expected=$ExpectedState;state=$state
    placeholder=$item.ExtendedProperty('System.FilePlaceholderStatus')
    provider=$item.ExtendedProperty('System.StorageProviderId')
    session=(Get-Process -Id $PID).SessionId
  }
  $values | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $Evidence ($Phase+'.json')) -Encoding UTF8
  # Capture the actual desktop after Shell has painted the state column/icons.
  Add-Type -AssemblyName System.Drawing,System.Windows.Forms
  [void][ExplorerEvidence]::ShowWindow([IntPtr]$window.HWND,9)
  [void][ExplorerEvidence]::SetForegroundWindow([IntPtr]$window.HWND)
  Start-Sleep -Milliseconds 700
  if([ExplorerEvidence]::GetForegroundWindow() -ne [IntPtr]$window.HWND){throw 'test Explorer window is obscured; refuse screenshot evidence'}
  $screen=[Windows.Forms.Screen]::PrimaryScreen.Bounds
  $bitmap=New-Object Drawing.Bitmap($screen.Width,$screen.Height)
  $graphics=[Drawing.Graphics]::FromImage($bitmap)
  try {
    $graphics.CopyFromScreen($screen.Location,[Drawing.Point]::Empty,$screen.Size)
    $bitmap.Save((Join-Path $Evidence ($Phase+'.png')),[Drawing.Imaging.ImageFormat]::Png)
  } finally {$graphics.Dispose();$bitmap.Dispose()}
  if($null -eq $state -or [int]$state -ne $ExpectedState){throw "Shell state $state does not equal $ExpectedState"}
  if(([int]$values.placeholder -band 8) -eq 0){throw 'Shell does not recognize a cloud placeholder'}
  Write-Output "check=pass explorer phase=$Phase storage_provider_state=$state placeholder=$($values.placeholder) session=$($values.session)"
} finally {
  if($window){$window.Quit()}
  [void][ExplorerEvidence]::SetThreadExecutionState(0x80000000L)
}
