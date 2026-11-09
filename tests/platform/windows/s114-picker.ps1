param([Parameter(Mandatory=$true)][string]$Path)
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
[PickerWindow]::SetForegroundWindow([IntPtr]$dialog.Current.NativeWindowHandle) | Out-Null
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
# Open a second dialog and click Cancel; the form must retain the first path.
$dialog = Wait-Dialog
[PickerWindow]::SetForegroundWindow([IntPtr]$dialog.Current.NativeWindowHandle) | Out-Null
Start-Sleep -Milliseconds 200
Trace 'cancelling'
Click-DialogButton $dialog '2'
@{selected=$Path;cancelled=$true} | ConvertTo-Json | Set-Content (Join-Path $PSScriptRoot 'picker-result.json') -Encoding UTF8
exit 0
