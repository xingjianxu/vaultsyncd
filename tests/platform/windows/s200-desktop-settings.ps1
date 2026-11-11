# S200 production WebView/Win32 acceptance; isolated LOCALAPPDATA and authenticated safe stop.
param([switch]$Worker)
$ErrorActionPreference = 'Stop'
if ($Worker) {
  $env:TEMP = Join-Path $PSScriptRoot 'tmp'
  $env:TMP = $env:TEMP
  New-Item -ItemType Directory -Path $env:TEMP | Out-Null
  Add-Type -AssemblyName UIAutomationClient
  Add-Type -AssemblyName UIAutomationTypes
  Add-Type -TypeDefinition @'
using System;using System.Text;using System.Runtime.InteropServices;
public static class S200Window {
 delegate bool Each(IntPtr w,IntPtr p);
 [DllImport("user32.dll")]static extern bool EnumWindows(Each c,IntPtr p);
 [DllImport("user32.dll")]static extern uint GetWindowThreadProcessId(IntPtr h,out uint p);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)]static extern int GetClassNameW(IntPtr h,StringBuilder s,int n);
 [DllImport("user32.dll")]public static extern bool IsWindowVisible(IntPtr h);
 [DllImport("user32.dll")]public static extern bool PostMessageW(IntPtr h,uint m,IntPtr w,IntPtr l);
 [DllImport("user32.dll")]public static extern IntPtr GetDlgItem(IntPtr h,int id);
 [DllImport("user32.dll")]public static extern IntPtr SendMessageW(IntPtr h,uint m,IntPtr w,IntPtr l);
 [DllImport("user32.dll")]static extern bool SetCursorPos(int x,int y);
 [DllImport("user32.dll")]static extern bool GetMenuItemRect(IntPtr w,IntPtr m,uint i,out Rect r);
 [DllImport("user32.dll")]static extern void mouse_event(uint f,uint x,uint y,uint d,UIntPtr e);
 [StructLayout(LayoutKind.Sequential)]struct Rect{public int left,top,right,bottom;}
 public static IntPtr Find(uint process,string kind){IntPtr found=IntPtr.Zero;EnumWindows((h,p)=>{uint pid;GetWindowThreadProcessId(h,out pid);var s=new StringBuilder(256);GetClassNameW(h,s,256);if((process==0||pid==process)&&s.ToString()==kind){found=h;return false;}return true;},IntPtr.Zero);return found;}
 public static void Popup(IntPtr h){SetCursorPos(650,500);PostMessageW(h,6002,IntPtr.Zero,(IntPtr)0x205);}
 public static void Click(int x,int y){SetCursorPos(x,y);mouse_event(2,0,0,0,UIntPtr.Zero);mouse_event(4,0,0,0,UIntPtr.Zero);}
 public static void Select(IntPtr popup,uint position){var menu=SendMessageW(popup,0x1e1,IntPtr.Zero,IntPtr.Zero);Rect r;if(!GetMenuItemRect(IntPtr.Zero,menu,position,out r))throw new Exception("menu item missing");Click((r.left+r.right)/2,(r.top+r.bottom)/2);}
}
'@
  $checks = [Collections.Generic.List[string]]::new()
  $resultCode = '1'
  $program = Join-Path $PSScriptRoot 'program'
  $desktop = $null
  $main = [IntPtr]::Zero
  $session = (Get-Process -Id $PID).SessionId
  $agentFile = Join-Path $program 'vaultsyncd-agent.exe'
  $agentMoved = $agentFile+'.held'
  $env:LOCALAPPDATA = Join-Path $PSScriptRoot 'local'
  $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = '--force-renderer-accessibility'
  $settings = Join-Path $env:LOCALAPPDATA 'vaultsyncd/desktop-settings.json'
  function Check($ok,[string]$name) {
    if (!$ok) { throw $name }
    $checks.Add($name)
    @($checks.ToArray()) | ConvertTo-Json | Set-Content (Join-Path $PSScriptRoot 'checks.json') -Encoding UTF8
  }
  function Wait-Until([scriptblock]$condition,[int]$seconds=30) {
    $end=(Get-Date).AddSeconds($seconds)
    do { if (& $condition) { return $true }; Start-Sleep -Milliseconds 100 } while ((Get-Date) -lt $end)
    return $false
  }
  function Agents {
    return ,@(Get-CimInstance Win32_Process -Filter "Name='vaultsyncd-agent.exe'" | Where-Object {$_.SessionId -eq $session -and $_.CommandLine -match '--config'})
  }
  function Read-Dialog([IntPtr]$dialog) {
    $element=[Windows.Automation.AutomationElement]::FromHandle($dialog)
    $items=$element.FindAll([Windows.Automation.TreeScope]::Descendants,[Windows.Automation.Condition]::TrueCondition)
    return (@($items | ForEach-Object {$_.Current.Name}) -join ' ')
  }
  function Dismiss-Dialog([IntPtr]$dialog) {
    $element=[Windows.Automation.AutomationElement]::FromHandle($dialog)
    $items=$element.FindAll([Windows.Automation.TreeScope]::Descendants,[Windows.Automation.Condition]::TrueCondition)
    foreach($item in $items) {
      if($item.Current.ControlType -eq [Windows.Automation.ControlType]::Button) {
        $invoke=$null
        if($item.TryGetCurrentPattern([Windows.Automation.InvokePattern]::Pattern,[ref]$invoke)) { $invoke.Invoke(); return }
      }
    }
    # This lab can expose standard MessageBox buttons as UIA Pane. Target only this
    # process-owned dialog's observed OK child (ID 1 or 2); no global keyboard input.
    foreach($id in @(1,2)) {
      $ok=[S200Window]::GetDlgItem($dialog,$id)
      if($ok -ne [IntPtr]::Zero) { [void][S200Window]::SendMessageW($ok,0x00F5,[IntPtr]::Zero,[IntPtr]::Zero); return }
    }
    throw 'native configuration dialog has no dismiss control'
  }
  function Launch([string]$name,[bool]$badSettings=$false) {
    $script:desktop=Start-Process (Join-Path $program 'vaultsyncd-desktop.exe') -PassThru -RedirectStandardError (Join-Path $PSScriptRoot ($name+'.log'))
    $null=$desktop.Handle
    if($badSettings) {
      Check (Wait-Until { [S200Window]::Find($desktop.Id,'#32770') -ne [IntPtr]::Zero } 60) 'corrupt preferences produce a native error'
      $dialog=[S200Window]::Find($desktop.Id,'#32770')
      $text=Read-Dialog $dialog
      $text | Set-Content (Join-Path $PSScriptRoot 'corrupt-settings-dialog.txt') -Encoding UTF8
      Check ($text -match '配置') 'corrupt preferences error identifies configuration'
      Dismiss-Dialog $dialog
    }
    Check (Wait-Until { $desktop.Refresh(); $h=[S200Window]::Find($desktop.Id,'Tauri Window'); !$desktop.HasExited -and $h -ne [IntPtr]::Zero -and [S200Window]::IsWindowVisible($h) } 90) ($name+' opens production main window')
    $script:main=[S200Window]::Find($desktop.Id,'Tauri Window')
    Check (Wait-Until { [S200Window]::Find($desktop.Id,'tray_icon_app') -ne [IntPtr]::Zero }) ($name+' owns a real tray icon')
  }
  function Tray([uint32]$position) {
    $tray=[S200Window]::Find($desktop.Id,'tray_icon_app')
    Check ($tray -ne [IntPtr]::Zero) 'tray stays available'
    [S200Window]::Popup($tray)
    Check (Wait-Until { [S200Window]::Find(0,'#32768') -ne [IntPtr]::Zero }) 'native tray menu opens'
    [S200Window]::Select([S200Window]::Find(0,'#32768'),$position)
  }
  function Close-Main { [void][S200Window]::PostMessageW($main,0x0010,[IntPtr]::Zero,[IntPtr]::Zero) }
  function Expect-Hidden([string]$name) {
    Check (Wait-Until { ![S200Window]::IsWindowVisible($main) }) ($name+' hides the main window')
    $desktop.Refresh()
    Check (!$desktop.HasExited) ($name+' keeps GUI process alive')
  }
  function Expect-Exited([string]$name) {
    Check ($desktop.WaitForExit(90000)) ($name+' exits GUI normally')
    $desktop.WaitForExit()
    Check ($desktop.ExitCode -eq 0) ($name+' returns success')
    $script:desktop=$null
  }
  function Find-Control([string]$name) {
    $root=[Windows.Automation.AutomationElement]::FromHandle($main)
    $condition=New-Object Windows.Automation.PropertyCondition([Windows.Automation.AutomationElement]::NameProperty,$name)
    $found=$root.FindAll([Windows.Automation.TreeScope]::Descendants,$condition)
    foreach($item in $found) { if($item.Current.ControlType -ne [Windows.Automation.ControlType]::Text -and !$item.Current.IsOffscreen) { return $item } }
    return $null
  }
  function Click-Control([string]$name) {
    Check (Wait-Until { $null -ne (Find-Control $name) }) ('production WebView exposes '+$name)
    Check (Wait-Until { $item=Find-Control $name; $item -and $item.Current.IsEnabled }) ($name+' is enabled before interaction')
    $control=Find-Control $name
    $pattern=$null
    if($control.TryGetCurrentPattern([Windows.Automation.TogglePattern]::Pattern,[ref]$pattern)) { $pattern.Toggle(); return }
    if($control.TryGetCurrentPattern([Windows.Automation.SelectionItemPattern]::Pattern,[ref]$pattern)) { $pattern.Select(); return }
    $invoke=$null
    if($control.TryGetCurrentPattern([Windows.Automation.InvokePattern]::Pattern,[ref]$invoke)) { $invoke.Invoke(); return }
    $rect=$control.Current.BoundingRectangle
    Check (!$rect.IsEmpty -and $rect.Width -gt 0 -and $rect.Height -gt 0) ($name+' is clickable')
    [S200Window]::Click([int]($rect.X+$rect.Width/2),[int]($rect.Y+$rect.Height/2))
  }
  function Open-Settings { Click-Control '程序配置'; Check (Wait-Until { $null -ne (Find-Control '保存程序配置') }) 'dedicated program settings page loads' }
  function Save-Settings([string]$action,[bool]$stop) {
    Click-Control '保存程序配置'
    Check (Wait-Until {
      if(!(Test-Path $settings)) { return $false }
      try { $s=Get-Content $settings -Raw | ConvertFrom-Json; return $s.close_action -eq $action -and $s.stop_agent_on_exit -eq $stop } catch { return $false }
    }) ('native settings persist '+$action+'/'+$stop)
  }
  function Action([string]$action) {
    $out=Join-Path $PSScriptRoot ($action+'-'+[Guid]::NewGuid().ToString('N')+'.log')
    $err=$out+'.err'
    $p=Start-Process $agentFile -ArgumentList @('--user-session',$action) -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
    $null=$p.Handle
    Check ($p.WaitForExit(90000)) ($action+' helper terminates')
    $p.WaitForExit()
    Check ($p.ExitCode -eq 0) ($action+' helper succeeds: '+[IO.File]::ReadAllText($err))
    return [IO.File]::ReadAllText($out)
  }
  function Write-Settings([string]$action,[bool]$stop) {
    [IO.File]::WriteAllText($settings,(@{close_action=$action;stop_agent_on_exit=$stop} | ConvertTo-Json -Compress))
  }
  try {
    Check ($session -ne 0) 'real interactive Windows session'
    $existing=@(Get-Process vaultsyncd-agent,vaultsyncd-desktop -ErrorAction SilentlyContinue | Where-Object {$_.SessionId -eq $session})
    Check ($existing.Count -eq 0) 'no preexisting client in the interactive session'
    New-Item -ItemType Directory -Path $env:LOCALAPPDATA | Out-Null
    Launch 'defaults'
    $null=Action 'health'
    Check ((Agents).Count -eq 1) 'default startup owns one independent agent'
    $firstPid=(Agents)[0].ProcessId
    Close-Main
    Expect-Hidden 'default close'
    Check ((Agents)[0].ProcessId -eq $firstPid) 'default close preserves original writer'
    $null=Action 'health'
    Tray 0
    Check (Wait-Until { [S200Window]::IsWindowVisible($main) }) 'tray Open restores the original main window'
    Open-Settings
    $radio=Find-Control '最小化到托盘'
    $selected=$radio.GetCurrentPattern([Windows.Automation.SelectionItemPattern]::Pattern)
    Check $selected.Current.IsSelected 'new profile defaults to tray'
    $toggle=Find-Control '退出程序时自动关闭 agent'
    Check (!$toggle.Current.IsEnabled) 'agent exit switch is disabled for tray mode'
    Click-Control '完全退出程序'
    $toggle=Find-Control '退出程序时自动关闭 agent'
    $toggleState=$toggle.GetCurrentPattern([Windows.Automation.TogglePattern]::Pattern)
    Check ($toggleState.Current.ToggleState -eq [Windows.Automation.ToggleState]::On) 'complete exit defaults to stopping agent'
    Click-Control '退出程序时自动关闭 agent'
    Save-Settings 'exit' $false
    Close-Main
    Expect-Exited 'exit with agent kept'
    Check ((Agents).Count -eq 1 -and (Agents)[0].ProcessId -eq $firstPid) 'configured GUI-only exit preserves original agent'
    Launch 'persisted-exit'
    Open-Settings
    $radio=Find-Control '完全退出程序'
    $selected=$radio.GetCurrentPattern([Windows.Automation.SelectionItemPattern]::Pattern)
    Check $selected.Current.IsSelected 'complete-exit preference survives restart'
    $toggleState=(Find-Control '退出程序时自动关闭 agent').GetCurrentPattern([Windows.Automation.TogglePattern]::Pattern)
    Check ($toggleState.Current.ToggleState -eq [Windows.Automation.ToggleState]::Off) 'keep-agent preference survives restart'
    Close-Main
    Expect-Exited 'persisted complete exit'
    Check ((Agents)[0].ProcessId -eq $firstPid) 'persisted keep-agent behavior remains effective'
    Launch 'safe-close'
    Open-Settings
    Click-Control '退出程序时自动关闭 agent'
    Save-Settings 'exit' $true
    $profile=Join-Path $env:LOCALAPPDATA 'vaultsyncd/agent'
    $exportsBefore=@(Get-ChildItem $profile -Directory -Filter 'session-stop-*').Count
    $stopTimer=[Diagnostics.Stopwatch]::StartNew()
    Close-Main
    Close-Main
    Close-Main
    Expect-Exited 'complete exit with safe agent stop'
    Check ($stopTimer.Elapsed.TotalSeconds -lt 15) 'idle orderly exit completes within 15 seconds'
    Check (Wait-Until { (Agents).Count -eq 0 }) 'complete exit stops the authenticated original writer'
    $profile=Join-Path $env:LOCALAPPDATA 'vaultsyncd/agent'
    Check (@(Get-ChildItem $profile -Directory -Filter 'session-stop-*').Count -eq $exportsBefore) 'ordinary stop retains recovery state in place without a full export'
    Write-Settings 'tray' $true
    Launch 'tray-safe-exit'
    Close-Main
    Expect-Hidden 'configured tray close'
    Tray 2
    Expect-Exited 'tray Quit with agent stop'
    Check (Wait-Until { (Agents).Count -eq 0 }) 'tray Quit follows stop-agent preference'
    Write-Settings 'exit' $false
    Launch 'tray-keep-agent'
    $keptPid=(Agents)[0].ProcessId
    Tray 2
    Expect-Exited 'tray Quit keeping agent'
    Check ((Agents).Count -eq 1 -and (Agents)[0].ProcessId -eq $keptPid) 'tray Quit also supports keeping agent'
    [IO.File]::WriteAllText($settings,'S200 corrupt configuration must survive')
    Launch 'corrupt-settings' $true
    Close-Main
    Expect-Hidden 'corrupt-settings fallback'
    Check ([IO.File]::ReadAllText($settings) -eq 'S200 corrupt configuration must survive') 'corrupt preferences are never silently overwritten'
    Check ((Agents)[0].ProcessId -eq $keptPid) 'corrupt settings never trigger implicit stop'
    Tray 2
    Check (Wait-Until { [S200Window]::Find($desktop.Id,'#32770') -ne [IntPtr]::Zero }) 'corrupt settings reject explicit Quit visibly'
    $dialog=[S200Window]::Find($desktop.Id,'#32770')
    $text=Read-Dialog $dialog
    $text | Set-Content (Join-Path $PSScriptRoot 'corrupt-settings-quit-dialog.txt') -Encoding UTF8
    Check ($text -match '配置') 'Quit refusal explains configuration repair'
    Check ((Agents)[0].ProcessId -eq $keptPid) 'refused Quit preserves original writer'
    Dismiss-Dialog $dialog
    Open-Settings
    Click-Control '使用默认配置'
    Save-Settings 'tray' $true
    Tray 2
    Expect-Exited 'explicit tray Quit after configuration repair'
    Check (Wait-Until { (Agents).Count -eq 0 }) 'repaired configuration allows authenticated safe stop'
    Write-Settings 'exit' $true
    Launch 'stop-failure'
    $failurePid=(Agents)[0].ProcessId
    Move-Item -LiteralPath $agentFile -Destination $agentMoved
    Close-Main
    Check (Wait-Until { [S200Window]::Find($desktop.Id,'#32770') -ne [IntPtr]::Zero } 45) 'safe-stop launch failure is visible as native error'
    $dialog=[S200Window]::Find($desktop.Id,'#32770')
    $message=Read-Dialog $dialog
    $message | Set-Content (Join-Path $PSScriptRoot 'stop-failure-dialog.txt') -Encoding UTF8
    Check ($message -match 'agent') 'stop failure identifies the agent error'
    $desktop.Refresh()
    Check (!$desktop.HasExited -and [S200Window]::IsWindowVisible($main)) 'failed stop keeps GUI visible and alive'
    Check ((Agents).Count -eq 1 -and (Agents)[0].ProcessId -eq $failurePid) 'failed stop preserves the running agent'
    Dismiss-Dialog $dialog
    Move-Item -LiteralPath $agentMoved -Destination $agentFile
    Close-Main
    Expect-Exited 'retry after restoring stop helper'
    Check (Wait-Until { (Agents).Count -eq 0 }) 'retry safely stops original agent'
    if(Test-Path (Join-Path $PSScriptRoot 'fixture.json')) {
      $fixture=Get-Content (Join-Path $PSScriptRoot 'fixture.json') -Raw | ConvertFrom-Json
      $cli=Join-Path $program 'vaultsyncd-cli.exe'
      function Invoke-S203Cli([string[]]$arguments) {
        $raw=& $cli --json @arguments 2>&1
        if($LASTEXITCODE -ne 0) { throw ('S203 CLI '+$arguments[0]+' failed with exit '+$LASTEXITCODE) }
        return (($raw -join "`n" | ConvertFrom-Json).result)
      }
      Launch 'binding-shutdown'
      $null=Invoke-S203Cli @('set-connection','--server-id',$fixture.serverId,'--server-url',$fixture.url,'--ca-file',(Join-Path $PSScriptRoot 'ca.pem'))
      $loginRaw=$fixture.password | & $cli --json login --username $fixture.login 2>&1
      Check ($LASTEXITCODE -eq 0) 'real isolated server login succeeds'
      $binding=[Guid]::NewGuid().ToString('N')
      $local=Join-Path $PSScriptRoot 'bound-content'
      $cache=Join-Path $PSScriptRoot 'binding-cache'
      New-Item -ItemType Directory -Path $local,$cache | Out-Null
      $null=Invoke-S203Cli @('create-binding','--binding-id',$binding,'--vault-id',$fixture.vault,'--local-path',$local,'--cache-path',$cache,'--mode','full')
      Check (Wait-Until { $s=Invoke-S203Cli @('status'); @($s.bindings | Where-Object { $_.binding_id -eq $binding -and $_.synchronized }).Count -eq 1 } 120) 'real CFAPI full binding reaches committed baseline'
      $null=Invoke-S203Cli @('pause','--binding-id',$binding)
      $unconfirmed=Join-Path $local 'server-seed.txt'
      [IO.File]::WriteAllText($unconfirmed,'S203 unconfirmed bytes must survive ordinary exit')
      $beforeHash=(Get-FileHash $unconfirmed -Algorithm SHA256).Hash
      $locked=[IO.File]::Open($unconfirmed,[IO.FileMode]::Open,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)
      $beforeExports=@(Get-ChildItem $profile -Directory -Filter 'session-stop-*').Count
      $timer=[Diagnostics.Stopwatch]::StartNew()
      try {
        Close-Main
        Expect-Exited 'ordinary exit with a locked unconfirmed bound file'
        $timer.Stop()
        Check ($timer.Elapsed.TotalSeconds -lt 15) 'locked bound file does not trigger recovery scan or copy'
        Check ((Agents).Count -eq 0) 'bound writer actually exits after native callback drain'
      } finally { $locked.Dispose() }
      Check ((Get-FileHash $unconfirmed -Algorithm SHA256).Hash -eq $beforeHash) 'unconfirmed bytes survive exact native shutdown'
      Check ((Test-Path $cache) -and (Test-Path (Join-Path $profile 'state/client.sqlite'))) 'cache and durable state remain in place'
      Check (@(Get-ChildItem $profile -Directory -Filter 'session-stop-*').Count -eq $beforeExports) 'bound ordinary exit makes no maintenance export'
      Launch 'binding-restart'
      $s=Invoke-S203Cli @('status')
      Check (@($s.management.bindings | Where-Object { $_.binding_id -eq $binding -and $_.paused }).Count -eq 1) 'restart retains binding identity and explicit pause'
      Check ((Get-FileHash $unconfirmed -Algorithm SHA256).Hash -eq $beforeHash) 'restart preserves unconfirmed bytes'
      Close-Main
      Expect-Exited 'restarted binding closes safely'
      @{exit_seconds=$timer.Elapsed.TotalSeconds;unconfirmed_sha256=$beforeHash;binding=$binding;maintenance_export_created=$false} | ConvertTo-Json | Set-Content (Join-Path $PSScriptRoot 's203-binding.json') -Encoding UTF8
    }
    @{ok=$true;checks=$checks.ToArray();count=$checks.Count;session=$session;native='Windows 11 x64';scope='production WebView settings, window close, real tray and authenticated session stop; optional S203 real CFAPI binding and locked unconfirmed file'} | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $PSScriptRoot 'result.json') -Encoding UTF8
    $resultCode = '0'
  } catch {
    $_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'error.txt')
    try {
      if($main -ne [IntPtr]::Zero) {
        $root=[Windows.Automation.AutomationElement]::FromHandle($main)
        $nodes=$root.FindAll([Windows.Automation.TreeScope]::Descendants,[Windows.Automation.Condition]::TrueCondition)
        @($nodes | ForEach-Object { $_.Current.ControlType.ProgrammaticName+' | '+$_.Current.Name }) | Set-Content (Join-Path $PSScriptRoot 'ui-tree.txt') -Encoding UTF8
      }
    } catch {}
    $resultCode = '1'
  } finally {
    Remove-Item (Join-Path $PSScriptRoot 'fixture.json') -ErrorAction SilentlyContinue
    if(Test-Path $agentMoved) { Move-Item -LiteralPath $agentMoved -Destination $agentFile }
    # Only act on this isolated run. Failure evidence and user data remain on disk.
    try { if($desktop -and !$desktop.HasExited) { $dialog=[S200Window]::Find($desktop.Id,'#32770'); if($dialog -ne [IntPtr]::Zero) { Dismiss-Dialog $dialog }; Tray 2; Expect-Exited 'cleanup tray Quit' } } catch { $_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'gui-cleanup-error.txt') }
    try { if((Agents).Count -gt 0) { $null=Action 'stop' } } catch { $_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'agent-cleanup-error.txt') }
  }
  $resultCode | Set-Content (Join-Path $PSScriptRoot 'exit.txt')
  exit
}
$hashes=Get-Content (Join-Path $PSScriptRoot 'hashes.json') -Raw | ConvertFrom-Json
foreach($item in $hashes.PSObject.Properties) {
  if((Get-FileHash (Join-Path (Join-Path $PSScriptRoot 'program') $item.Name) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $item.Value) { throw 'test transport hash mismatch' }
}
$task='vaultsyncd-s200-'+[Guid]::NewGuid().ToString('N')
$action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker"
$principal=New-ScheduledTaskPrincipal -UserId 'Quickemu' -LogonType Interactive -RunLevel Limited
try {
  Register-ScheduledTask -TaskName $task -Action $action -Principal $principal | Out-Null
  Start-ScheduledTask -TaskName $task
  $end=(Get-Date).AddSeconds(900)
  while(!(Test-Path (Join-Path $PSScriptRoot 'exit.txt'))) {
    if((Get-Date) -ge $end) { throw 'interactive S200 worker timeout; preserve the run' }
    Start-Sleep -Milliseconds 200
  }
  if((Get-Content (Join-Path $PSScriptRoot 'exit.txt')) -ne '0') { Get-Content (Join-Path $PSScriptRoot 'error.txt'); exit 1 }
  Get-Content (Join-Path $PSScriptRoot 'result.json')
} finally { Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue }
