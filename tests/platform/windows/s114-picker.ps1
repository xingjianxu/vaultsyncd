param([Parameter(Mandatory=$true)][string]$Path,[switch]$SelectOnly)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName System.Windows.Forms
Add-Type @'
using System;
using System.Runtime.InteropServices;
public class PickerWindow {
 [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
 [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr window, uint message, IntPtr wparam, IntPtr lparam);
 [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
 [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, IntPtr process);
 [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint attach, uint attachTo, bool join);
 [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr window);
 [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
 [DllImport("user32.dll")] public static extern void SwitchToThisWindow(IntPtr window, bool altTab);
 [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr window, System.Text.StringBuilder text, int length);
 [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassNameW(IntPtr window, System.Text.StringBuilder text, int length);
 public static string Describe(IntPtr window) {
  var title = new System.Text.StringBuilder(512); GetWindowTextW(window, title, 512);
  var cls = new System.Text.StringBuilder(256); GetClassNameW(window, cls, 256);
  uint pid; GetWindowThreadProcessId(window, out pid);
  string name = "?";
  try { name = System.Diagnostics.Process.GetProcessById((int)pid).ProcessName; } catch {}
  return window + "|" + pid + "|" + name + "|" + cls + "|" + title;
 }
}
'@
$root = [Windows.Automation.AutomationElement]::RootElement
function Trace([string]$message) { Add-Content (Join-Path $PSScriptRoot 'picker-trace.log') $message }
Trace 'started'
function Wait-Dialog {
  $end = (Get-Date).AddSeconds(150)
  while ((Get-Date) -lt $end) {
    $condition = New-Object Windows.Automation.PropertyCondition([Windows.Automation.AutomationElement]::NameProperty, '选择本地同步目录')
    $dialog = $root.FindFirst([Windows.Automation.TreeScope]::Descendants, $condition)
    if ($dialog) { Trace ("found handle="+$dialog.Current.NativeWindowHandle); return $dialog }
    Start-Sleep -Milliseconds 150
  }
  throw 'native directory picker did not open'
}
# Windows blocks SetForegroundWindow from a background process; join the foreground input
# queue before retrying so the address-bar keys really land in this dialog.
function Set-DialogForeground([IntPtr]$target) {
  Trace ('foreground_before='+[PickerWindow]::Describe([PickerWindow]::GetForegroundWindow()))
  if ([PickerWindow]::GetForegroundWindow() -eq $target) { return $true }
  [void][PickerWindow]::SetForegroundWindow($target)
  if ([PickerWindow]::GetForegroundWindow() -eq $target) { Trace 'foreground=set'; return $true }
  $foregroundThread = [PickerWindow]::GetWindowThreadProcessId([PickerWindow]::GetForegroundWindow(), [IntPtr]::Zero)
  $currentThread = [PickerWindow]::GetCurrentThreadId()
  [void][PickerWindow]::AttachThreadInput($currentThread, $foregroundThread, $true)
  try {
    [void][PickerWindow]::BringWindowToTop($target)
    [void][PickerWindow]::SetForegroundWindow($target)
  } finally {
    [void][PickerWindow]::AttachThreadInput($currentThread, $foregroundThread, $false)
  }
  if ([PickerWindow]::GetForegroundWindow() -eq $target) { Trace 'foreground=attached'; return $true }
  [PickerWindow]::SwitchToThisWindow($target, $true)
  Start-Sleep -Milliseconds 300
  if ([PickerWindow]::GetForegroundWindow() -eq $target) { Trace 'foreground=switch'; return $true }
  Trace ('foreground_after='+[PickerWindow]::Describe([PickerWindow]::GetForegroundWindow()))
  return $false
}
function Click-DialogButton($dialog, [string]$id) {
  # This guest exposes standard Win32 controls as UIA Pane without InvokePattern.
  # Resolve the actual button handle by its observed dialog control ID, then click it.
  $condition = New-Object Windows.Automation.PropertyCondition([Windows.Automation.AutomationElement]::AutomationIdProperty, $id)
  $button = $dialog.FindFirst([Windows.Automation.TreeScope]::Descendants, $condition)
  if (!$button -or $button.Current.ClassName -ne 'Button') { throw ('native dialog button missing: '+$id) }
  [PickerWindow]::SendMessage([IntPtr]$button.Current.NativeWindowHandle, 0x00F5, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
}
# Real Common Item Dialog, no frontend or native API substitution. Select a Unicode path.
$dialog = Wait-Dialog
if (!(Set-DialogForeground([IntPtr]$dialog.Current.NativeWindowHandle))) { throw 'native directory dialog did not reach the foreground' }
Start-Sleep -Milliseconds 200
Trace 'setting address'
[Windows.Forms.SendKeys]::SendWait('^l')
Start-Sleep -Milliseconds 300
[Windows.Forms.SendKeys]::SendWait($Path)
[Windows.Forms.SendKeys]::SendWait('{ENTER}')
Start-Sleep -Milliseconds 700
Trace 'choosing directory'
Click-DialogButton $dialog '1'
Start-Sleep -Seconds 1
if($SelectOnly){
  @{selected=$Path;cancelled=$false} | ConvertTo-Json | Set-Content (Join-Path $PSScriptRoot 'picker-result.json') -Encoding UTF8
  exit 0
}
# Open a second dialog and click Cancel; the form must retain the first path.
$dialog = Wait-Dialog
[PickerWindow]::SetForegroundWindow([IntPtr]$dialog.Current.NativeWindowHandle) | Out-Null
Start-Sleep -Milliseconds 200
Trace 'cancelling'
Click-DialogButton $dialog '2'
@{selected=$Path;cancelled=$true} | ConvertTo-Json | Set-Content (Join-Path $PSScriptRoot 'picker-result.json') -Encoding UTF8
exit 0
