# S185 native acceptance. Isolated LOCALAPPDATA; never touches the interactive user's profile.
param([switch]$Worker)
$ErrorActionPreference = 'Stop'
if ($Worker) {
  Add-Type -AssemblyName UIAutomationClient
  Add-Type -TypeDefinition @'
using System;using System.Text;using System.Runtime.InteropServices;
public static class S185Tray {
 delegate bool Each(IntPtr w,IntPtr p);
 [DllImport("user32.dll")]static extern bool EnumWindows(Each c,IntPtr p);
 [DllImport("user32.dll")]static extern uint GetWindowThreadProcessId(IntPtr h,out uint p);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)]static extern int GetClassNameW(IntPtr h,StringBuilder s,int n);
 [DllImport("user32.dll")]static extern bool PostMessageW(IntPtr h,uint m,IntPtr w,IntPtr l);
 [DllImport("user32.dll")]static extern IntPtr SendMessageW(IntPtr h,uint m,IntPtr w,IntPtr l);
 [DllImport("user32.dll")]static extern bool SetCursorPos(int x,int y);
 [DllImport("user32.dll")]static extern bool GetMenuItemRect(IntPtr w,IntPtr m,uint i,out Rect r);
 [DllImport("user32.dll")]static extern void mouse_event(uint f,uint x,uint y,uint d,UIntPtr e);
 [StructLayout(LayoutKind.Sequential)]struct Rect{public int left,top,right,bottom;}
 public static IntPtr Find(uint process,string kind){IntPtr found=IntPtr.Zero;EnumWindows((h,p)=>{uint pid;GetWindowThreadProcessId(h,out pid);var s=new StringBuilder(256);GetClassNameW(h,s,256);if((process==0||pid==process)&&s.ToString()==kind){found=h;return false;}return true;},IntPtr.Zero);return found;}
 public static void Popup(IntPtr h){SetCursorPos(650,500);PostMessageW(h,6002,IntPtr.Zero,(IntPtr)0x205);}
 public static void Select(IntPtr popup,uint position){var menu=SendMessageW(popup,0x1e1,IntPtr.Zero,IntPtr.Zero);Rect r;if(!GetMenuItemRect(IntPtr.Zero,menu,position,out r))throw new Exception("menu item missing");SetCursorPos((r.left+r.right)/2,(r.top+r.bottom)/2);mouse_event(2,0,0,0,UIntPtr.Zero);mouse_event(4,0,0,0,UIntPtr.Zero);}
}
'@
  $checks = [Collections.Generic.List[string]]::new()
  $program = Join-Path $PSScriptRoot 'program'
  $desktop = $null
  $session = (Get-Process -Id $PID).SessionId
  function Check($ok, [string]$name) {
    if (!$ok) { throw $name }
    $checks.Add($name)
    @($checks.ToArray()) | ConvertTo-Json | Set-Content (Join-Path $PSScriptRoot 'checks.json') -Encoding UTF8
  }
  function Agents {
    return ,@(Get-CimInstance Win32_Process -Filter "Name='vaultsyncd-agent.exe'" | Where-Object {$_.SessionId -eq $session -and $_.CommandLine -match '--config'})
  }
  function Launch([string]$name, [bool]$failure = $false) {
    $p = Start-Process (Join-Path $script:program 'vaultsyncd-desktop.exe') -PassThru -RedirectStandardError (Join-Path $PSScriptRoot ($name+'.log'))
    $null = $p.Handle
    $end = (Get-Date).AddSeconds(90)
    do {
      Start-Sleep -Milliseconds 100
      $p.Refresh()
      if ($p.HasExited) { throw "desktop exited before window: $name" }
    } while (($p.MainWindowHandle -eq 0 -or !$p.MainWindowTitle) -and (Get-Date) -lt $end)
    Check ($p.MainWindowHandle -ne 0 -and $p.MainWindowTitle) ($name+' visible native window')
    if ($failure) {
      $element = [Windows.Automation.AutomationElement]::FromHandle($p.MainWindowHandle)
      $items = $element.FindAll([Windows.Automation.TreeScope]::Descendants, [Windows.Automation.Condition]::TrueCondition)
      $text = @($items | ForEach-Object {$_.Current.Name}) -join ' '
      Check ($text -match 'agent') ($name+' real startup error visible')
      $text | Set-Content (Join-Path $PSScriptRoot ($name+'-dialog.txt')) -Encoding UTF8
      $null = $p.CloseMainWindow()
      Check ($p.WaitForExit(15000)) ($name+' failed desktop exits')
      Check ($p.ExitCode -eq 1) ($name+' returns failure')
    }
    return $p
  }
  function TrayCommand([uint32]$position) {
    $end = (Get-Date).AddSeconds(45)
    do {
      $tray = [S185Tray]::Find($script:desktop.Id, 'tray_icon_app')
      if ($tray -ne [IntPtr]::Zero) { break }
      Start-Sleep -Milliseconds 100
    } while ((Get-Date) -lt $end)
    Check ($tray -ne [IntPtr]::Zero) 'real desktop tray exists'
    [S185Tray]::Popup($tray)
    Start-Sleep -Milliseconds 300
    $popup = [S185Tray]::Find(0, '#32768')
    Check ($popup -ne [IntPtr]::Zero) 'real tray menu opens'
    [S185Tray]::Select($popup, $position)
  }
  function CloseDesktop([switch]$QuitTray) {
    if ($QuitTray) {
      TrayCommand 2
      Check ($script:desktop.WaitForExit(30000)) 'Start cmd desktop exits via real tray Quit'
      $script:desktop = $null
      return
    }
    $null = $script:desktop.Handle
    $end = (Get-Date).AddSeconds(30)
    do {
      $script:desktop.Refresh()
      if ($script:desktop.HasExited) { break }
      # A just-created WebView can still be entering its message loop. Retry the normal
      # window-close request; never force-kill a GUI or agent to manufacture acceptance.
      $condition = New-Object Windows.Automation.PropertyCondition([Windows.Automation.AutomationElement]::ProcessIdProperty, $script:desktop.Id)
      $windows = [Windows.Automation.AutomationElement]::RootElement.FindAll([Windows.Automation.TreeScope]::Children, $condition)
      foreach ($window in $windows) {
        $pattern = $null
        if ($window.TryGetCurrentPattern([Windows.Automation.WindowPattern]::Pattern, [ref]$pattern)) { $pattern.Close() }
      }
      if ($script:desktop.WaitForExit(2000)) { break }
    } while ((Get-Date) -lt $end)
    Check ($script:desktop.HasExited) 'desktop closes normally without stopping agent'
    $script:desktop = $null
  }
  function Action([string]$action) {
    $out = Join-Path $PSScriptRoot ($action+'-'+[Guid]::NewGuid().ToString('N')+'.log')
    $err = $out+'.err'
    $p = Start-Process (Join-Path $script:program 'vaultsyncd-agent.exe') -ArgumentList @('--user-session',$action) -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
    $null = $p.Handle
    Check ($p.WaitForExit(90000)) ($action+' command terminates')
    $p.WaitForExit()
    Check ($p.ExitCode -eq 0) ($action+' command succeeds: '+[IO.File]::ReadAllText($err))
    return [IO.File]::ReadAllText($out)
  }
  try {
    Check ($session -ne 0) 'real interactive Windows session'
    $existing = @(Get-Process vaultsyncd-agent,vaultsyncd-desktop -ErrorAction SilentlyContinue | Where-Object {$_.SessionId -eq $session})
    Check ($existing.Count -eq 0) 'no existing client in interactive session'
    Check (!(Test-Path (Join-Path $program 'vaultsyncd-zip.exe'))) 'removed ZIP executable is absent'
    $env:LOCALAPPDATA = Join-Path $PSScriptRoot 'local'
    New-Item -ItemType Directory -Path $env:LOCALAPPDATA | Out-Null
    # S200 owns default tray/stop acceptance. Keep this startup/provenance suite's
    # explicit GUI-only lifetime so its existing writer-reuse checks remain valid.
    $desktopProfile = Join-Path $env:LOCALAPPDATA 'vaultsyncd'
    New-Item -ItemType Directory -Path $desktopProfile | Out-Null
    [IO.File]::WriteAllText((Join-Path $desktopProfile 'desktop-settings.json'), '{"close_action":"exit","stop_agent_on_exit":false}')
    # Neither a manifest nor an owner is required for the very first startup.
    foreach ($file in @('zip-files.json','installed-files.json')) {
      if (Test-Path (Join-Path $program $file)) { Remove-Item (Join-Path $program $file) }
    }
    $desktop = Launch 'no-manifests-first-start'
    $null = Action 'health'
    Check ((Agents).Count -eq 1) 'one background writer after direct desktop startup'
    $firstPid = (Agents)[0].ProcessId
    $profile = Join-Path $env:LOCALAPPDATA 'vaultsyncd/agent'
    $config = Join-Path $profile 'agent.json'
    $initialConfig = [IO.File]::ReadAllText($config)
    $owner = Join-Path $env:LOCALAPPDATA 'vaultsyncd/zip-owner.json'
    Check (!(Test-Path $owner)) 'fresh startup creates no ZIP owner'
    CloseDesktop
    Check ((Agents)[0].ProcessId -eq $firstPid) 'closing GUI preserves background agent'
    # Legacy owner is intentionally invalid, not just a different path/source.
    [IO.File]::WriteAllText($owner, 'legacy-invalid-owner-do-not-modify')
    [IO.File]::WriteAllText((Join-Path $program 'zip-files.json'), 'invalid-zip-manifest')
    [IO.File]::WriteAllText((Join-Path $program 'installed-files.json'), 'invalid-installed-manifest')
    $copy = Join-Path $PSScriptRoot ([string][char]0x4E2D+[string][char]0x6587+' second folder')
    Copy-Item -LiteralPath $program -Destination $copy -Recurse
    $program = $copy
    $desktop = Launch 'cross-directory-live-reuse'
    Check ((Agents).Count -eq 1 -and (Agents)[0].ProcessId -eq $firstPid) 'another directory reuses original healthy agent'
    $null = Action 'health'
    # Explicit update must stop before network/download or GUI exit when its target differs
    # from the reused writer. Ordinary startup must remain unrestricted.
    TrayCommand 1
    $end = (Get-Date).AddSeconds(30)
    $message = ''
    do {
      Start-Sleep -Milliseconds 100
      $dialog = [S185Tray]::Find($desktop.Id, '#32770')
      if ($dialog -ne [IntPtr]::Zero) {
        $element = [Windows.Automation.AutomationElement]::FromHandle($dialog)
        $items = $element.FindAll([Windows.Automation.TreeScope]::Descendants, [Windows.Automation.Condition]::TrueCondition)
        $message = @($items | ForEach-Object {$_.Current.Name}) -join ' '
      }
    } while ($message -notmatch 'Stop vaultsyncd.cmd' -and (Get-Date) -lt $end)
    Check ($message -match 'Stop vaultsyncd.cmd') 'cross-directory tray update explains safe Stop before switching targets'
    $message | Set-Content (Join-Path $PSScriptRoot 'cross-directory-update-dialog.txt') -Encoding UTF8
    $buttons = @($items | Where-Object {$_.Current.ControlType -eq [Windows.Automation.ControlType]::Button})
    $invoke = $buttons[0].GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern)
    $invoke.Invoke()
    Check (!(Test-Path (Join-Path $env:LOCALAPPDATA 'vaultsyncd/updates'))) 'cross-directory update does not create download staging'
    Check ((Agents).Count -eq 1 -and (Agents)[0].ProcessId -eq $firstPid -and !$desktop.HasExited) 'cross-directory update preserves live writer and GUI'
    CloseDesktop
    $null = Action 'stop'
    Check ((Agents).Count -eq 0) 'safe stop from another directory drains original agent'
    $null = Action 'stop'
    $desktop = Launch 'cross-directory-cold-start'
    $null = Action 'health'
    Check ((Agents).Count -eq 1 -and (Agents)[0].ExecutablePath -eq (Join-Path $program 'vaultsyncd-agent.exe')) 'cold start uses the selected program folder'
    Check ([IO.File]::ReadAllText($config) -eq $initialConfig) 'cross-directory cold start preserves device and configuration'
    CloseDesktop
    $null = Action 'stop'
    $moved = Join-Path $PSScriptRoot 'renamed moved folder'
    Move-Item -LiteralPath $program -Destination $moved
    $program = $moved
    $desktop = Launch 'moved-folder'
    $null = Action 'health'
    Check ([IO.File]::ReadAllText($config) -eq $initialConfig) 'moving folder preserves state and identity'
    Check ([IO.File]::ReadAllText($owner) -eq 'legacy-invalid-owner-do-not-modify') 'legacy owner is neither read as JSON nor rewritten'
    $duplicate = Start-Process (Join-Path $program 'vaultsyncd-desktop.exe') -PassThru
    $null = $duplicate.Handle
    Check ($duplicate.WaitForExit(15000) -and $duplicate.ExitCode -eq 0) 'duplicate desktop exits successfully'
    Check ((Agents).Count -eq 1) 'duplicate launch keeps single writer'
    CloseDesktop
    $null = Action 'stop'
    # Real transaction protection remains, distinct from program/owner verification.
    $barrier = Join-Path $profile 'maintenance.json'
    [IO.File]::WriteAllText($barrier, '{"test":"S185-owned-barrier"}')
    $null = Launch 'maintenance-barrier' $true
    Check ((Agents).Count -eq 0 -and (Test-Path $barrier)) 'pending maintenance remains intact and blocks writes'
    Remove-Item $barrier
    [IO.File]::WriteAllText($config, 'invalid-existing-config')
    $null = Launch 'invalid-config' $true
    Check ([IO.File]::ReadAllText($config) -eq 'invalid-existing-config') 'invalid user config is preserved, not replaced'
    [IO.File]::WriteAllText($config, $initialConfig)
    # Test the shipped Start wrapper, not only direct desktop.
    $entry = Join-Path $program 'Start vaultsyncd.cmd'
    $p = Start-Process $env:ComSpec -ArgumentList ('/d /s /c ""'+$entry+'""') -PassThru
    $null = $p.Handle
    Check ($p.WaitForExit(90000) -and $p.ExitCode -eq 0) 'shipped Start cmd works with spaces and no ZIP executable'
    $end = (Get-Date).AddSeconds(45)
    do {
      Start-Sleep -Milliseconds 200
      $windows = @(Get-Process vaultsyncd-desktop -ErrorAction SilentlyContinue | Where-Object {$_.SessionId -eq $session -and $_.MainWindowHandle -ne 0})
    } while ($windows.Count -ne 1 -and (Get-Date) -lt $end)
    Check ($windows.Count -eq 1) 'Start cmd opens real desktop'
    $desktop = $windows[0]
    CloseDesktop -QuitTray
    $null = Action 'stop'
    Check ([IO.File]::ReadAllText($owner) -eq 'legacy-invalid-owner-do-not-modify') 'all paths leave legacy owner untouched'
    Check ([IO.File]::ReadAllText($config) -eq $initialConfig) 'all paths preserve configuration'
    @{ok=$true;checks=$checks.ToArray();count=$checks.Count;session=$session;native='Windows 11 x64';scope='startup, no live CFAPI synchronization or release claim'} | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $PSScriptRoot 'result.json') -Encoding UTF8
    '0' | Set-Content (Join-Path $PSScriptRoot 'exit.txt')
  } catch {
    $_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'error.txt')
    '1' | Set-Content (Join-Path $PSScriptRoot 'exit.txt')
  } finally {
    # Only normal tray Quit and authenticated safe stop; failed runs stay for diagnosis.
    try { if ($desktop -and !$desktop.HasExited) { CloseDesktop -QuitTray } } catch { $_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'gui-cleanup-error.txt') }
    try { if ((Agents).Count -gt 0) { $null = Action 'stop' } } catch { $_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'cleanup-error.txt') }
  }
  exit
}
$hashes = Get-Content (Join-Path $PSScriptRoot 'hashes.json') -Raw | ConvertFrom-Json
foreach ($item in $hashes.PSObject.Properties) {
  if ((Get-FileHash (Join-Path (Join-Path $PSScriptRoot 'program') $item.Name) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $item.Value) { throw 'test transport hash mismatch' }
}
$task = 'vaultsyncd-s185-'+[Guid]::NewGuid().ToString('N')
$action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker"
$principal = New-ScheduledTaskPrincipal -UserId 'Quickemu' -LogonType Interactive -RunLevel Limited
try {
  Register-ScheduledTask -TaskName $task -Action $action -Principal $principal | Out-Null
  Start-ScheduledTask -TaskName $task
  $end = (Get-Date).AddSeconds(600)
  while (!(Test-Path (Join-Path $PSScriptRoot 'exit.txt'))) {
    if ((Get-Date) -ge $end) { throw 'interactive native startup worker timeout; preserve the run' }
    Start-Sleep -Milliseconds 200
  }
  if ((Get-Content (Join-Path $PSScriptRoot 'exit.txt')) -ne '0') { Get-Content (Join-Path $PSScriptRoot 'error.txt'); exit 1 }
  Get-Content (Join-Path $PSScriptRoot 'result.json')
} finally { Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue }
