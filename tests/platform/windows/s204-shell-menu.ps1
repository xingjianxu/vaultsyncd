# S204 production Explorer menu and per-file WebView acceptance. No build tools in the VM.
param([switch]$Worker)
$ErrorActionPreference='Stop'
[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false)
if($Worker) {
  $env:TEMP=Join-Path $PSScriptRoot 'tmp'; $env:TMP=$env:TEMP
  New-Item -ItemType Directory -Path $env:TEMP | Out-Null
  $env:LOCALAPPDATA=Join-Path $PSScriptRoot 'local'
  $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--force-renderer-accessibility'
  Add-Type -AssemblyName UIAutomationClient,UIAutomationTypes,System.Drawing,System.Windows.Forms
  Add-Type -TypeDefinition @'
using System;using System.Text;using System.Runtime.InteropServices;
public static class S204Window {
 delegate bool Each(IntPtr w,IntPtr p);
 [DllImport("user32.dll")]static extern bool EnumWindows(Each c,IntPtr p);
 [DllImport("user32.dll")]static extern uint GetWindowThreadProcessId(IntPtr h,out uint p);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)]static extern int GetClassNameW(IntPtr h,StringBuilder s,int n);
 [DllImport("user32.dll")]public static extern bool IsWindowVisible(IntPtr h);
 [DllImport("user32.dll")]public static extern bool PostMessageW(IntPtr h,uint m,IntPtr w,IntPtr l);
 [DllImport("user32.dll")]public static extern bool SetForegroundWindow(IntPtr h);
 [DllImport("user32.dll")]public static extern bool ShowWindow(IntPtr h,int n);
 [DllImport("user32.dll")]public static extern IntPtr GetForegroundWindow();
 [DllImport("user32.dll")]static extern bool SetCursorPos(int x,int y);
 [DllImport("user32.dll")]static extern void mouse_event(uint f,uint x,uint y,uint d,UIntPtr e);
 [DllImport("kernel32.dll")]public static extern uint SetThreadExecutionState(uint f);
 public static IntPtr Find(uint process,string kind){IntPtr found=IntPtr.Zero;EnumWindows((h,p)=>{uint pid;GetWindowThreadProcessId(h,out pid);var s=new StringBuilder(256);GetClassNameW(h,s,256);if((process==0||pid==process)&&s.ToString()==kind&&IsWindowVisible(h)){found=h;return false;}return true;},IntPtr.Zero);return found;}
 public static void Hover(int x,int y){SetCursorPos(x,y);}
 public static void Click(int x,int y){SetCursorPos(x,y);mouse_event(2,0,0,0,UIntPtr.Zero);mouse_event(4,0,0,0,UIntPtr.Zero);}
}
'@
  $checks=[Collections.Generic.List[string]]::new(); $exitCode='1'
  $program=Join-Path $PSScriptRoot 'program'
  $desktopFile=Join-Path $program 'vaultsyncd-desktop.exe'; $agentFile=Join-Path $program 'vaultsyncd-agent.exe'; $cli=Join-Path $program 'vaultsyncd-cli.exe'
  $session=(Get-Process -Id $PID).SessionId; $main=$null; $fileProcess=$null; $window=[IntPtr]::Zero; $explorer=$null; $lock=$null
  $registry='HKCU\Software\Classes\*\shell\vaultsyncd'; $psRegistry='Registry::HKEY_CURRENT_USER\Software\Classes\*\shell\vaultsyncd'
  $priorMenu=$false; $menuBackup=Join-Path $PSScriptRoot 'prior-menu.reg';$ownsSession=$false;$restoreMenu=$false
  function Check($ok,[string]$name) {
    if(!$ok){throw $name}; $checks.Add($name)
    $checks.ToArray() | ConvertTo-Json | Set-Content (Join-Path $PSScriptRoot 'checks.json') -Encoding UTF8
  }
  function Wait-Until([scriptblock]$condition,[int]$seconds=30) {
    $end=(Get-Date).AddSeconds($seconds)
    do{if(Test-Path (Join-Path $PSScriptRoot 'cancel')){throw 'S204 worker cancellation requested; preserve evidence'};if(& $condition){return $true};Start-Sleep -Milliseconds 150}while((Get-Date)-lt $end)
    return $false
  }
  function Invoke-Cli([string[]]$arguments) {
    $raw=& $cli --json @arguments 2>&1
    if($LASTEXITCODE -ne 0){throw ('CLI '+$arguments[0]+' failed: '+($raw -join ' '))}
    return (($raw -join "`n" | ConvertFrom-Json).result)
  }
  function File-Row {
    $result=Invoke-Cli @('files','--binding-id',$binding)
    return @($result.management.files | Where-Object {$_.path -eq $name}) | Select-Object -First 1
  }
  function Window-Text {
    $root=[Windows.Automation.AutomationElement]::FromHandle($script:window)
    return (@($root.FindAll([Windows.Automation.TreeScope]::Descendants,[Windows.Automation.Condition]::TrueCondition) | ForEach-Object {$_.Current.Name}) -join ' ')
  }
  function Find-Control([string]$name) {
    if($script:window -eq [IntPtr]::Zero){return $null}
    $root=[Windows.Automation.AutomationElement]::FromHandle($script:window)
    $condition=New-Object Windows.Automation.PropertyCondition([Windows.Automation.AutomationElement]::NameProperty,$name)
    foreach($item in $root.FindAll([Windows.Automation.TreeScope]::Descendants,$condition)) {
      if($item.Current.ControlType -eq [Windows.Automation.ControlType]::Button -and !$item.Current.IsOffscreen){return $item}
    }
    return $null
  }
  function Invoke-Control($control) {
    if($control.Current.ControlType -eq [Windows.Automation.ControlType]::MenuItem){
      $rect=$control.Current.BoundingRectangle
      if($rect.IsEmpty){throw 'menu item has no clickable rectangle'}
      [S204Window]::Click([int]($rect.X+$rect.Width/2),[int]($rect.Y+$rect.Height/2));return
    }
    $pattern=$null
    if($control.TryGetCurrentPattern([Windows.Automation.InvokePattern]::Pattern,[ref]$pattern)){$pattern.Invoke();return}
    $rect=$control.Current.BoundingRectangle
    if($rect.IsEmpty){throw 'control has no clickable rectangle'}
    [S204Window]::Click([int]($rect.X+$rect.Width/2),[int]($rect.Y+$rect.Height/2))
  }
  function Click-Control([string]$name) {
    Check (Wait-Until {$c=Find-Control $name;$c -and $c.Current.IsEnabled}) ($name+' enabled in production WebView')
    Invoke-Control (Find-Control $name)
  }
  function Find-Menu([string]$name) {
    $condition=New-Object Windows.Automation.PropertyCondition([Windows.Automation.AutomationElement]::ControlTypeProperty,[Windows.Automation.ControlType]::MenuItem)
    foreach($item in [Windows.Automation.AutomationElement]::RootElement.FindAll([Windows.Automation.TreeScope]::Descendants,$condition)) {
      if(!$item.Current.IsOffscreen -and $item.Current.Name.Replace('&','').Trim() -eq $name){return $item}
    }
    return $null
  }
  function Screenshot([string]$phase) {
    $screen=[Windows.Forms.Screen]::PrimaryScreen.Bounds
    $bitmap=New-Object Drawing.Bitmap($screen.Width,$screen.Height);$graphics=[Drawing.Graphics]::FromImage($bitmap)
    try{$graphics.CopyFromScreen($screen.Location,[Drawing.Point]::Empty,$screen.Size);$bitmap.Save((Join-Path $PSScriptRoot ($phase+'.png')),[Drawing.Imaging.ImageFormat]::Png)}finally{$graphics.Dispose();$bitmap.Dispose()}
  }
  function Close-Explorer {
    [Windows.Forms.SendKeys]::SendWait('{ESC}{ESC}')
    if($script:explorer){$script:explorer.Quit();$script:explorer=$null}
  }
  function Open-Menu([string]$path,[bool]$expected,[string]$phase) {
    Close-Explorer
    $directory=Split-Path $path; $leaf=Split-Path $path -Leaf
    $shell=New-Object -ComObject Shell.Application
    $prior=@($shell.Windows() | ForEach-Object {$_.HWND})
    Start-Process explorer.exe -ArgumentList ('/n,"'+$directory+'"')
    Check (Wait-Until {$script:explorer=$shell.Windows() | Where-Object {$_.HWND -notin $prior -and $_.Document.Folder.Self.Path -eq $directory} | Select-Object -First 1; $null -ne $script:explorer}) ($phase+' opens its own real Explorer window')
    $explorer.Visible=$true;$explorer.Left=10;$explorer.Top=10;$explorer.Width=1100;$explorer.Height=720
    $item=$explorer.Document.Folder.ParseName($leaf)
    Check ($null -ne $item) ($phase+' Shell resolves exact selected file')
    $explorer.Document.SelectItem($item,29)
    [void][S204Window]::ShowWindow([IntPtr]$explorer.HWND,9)
    [void][S204Window]::SetForegroundWindow([IntPtr]$explorer.HWND)
    Check (Wait-Until {[S204Window]::GetForegroundWindow() -eq [IntPtr]$explorer.HWND}) ($phase+' Explorer is in foreground')
    Start-Sleep -Milliseconds 500
    [Windows.Forms.SendKeys]::SendWait('+{F10}')
    # Some Windows 11 builds route the keyboard context key through the new menu.
    # Explicitly follow its observed More options control to the classic cascade.
    if(!(Wait-Until {[S204Window]::Find(0,'#32768') -ne [IntPtr]::Zero} 3)) {
      $more=Find-Menu 'Show more options';if(!$more){$more=Find-Menu '显示更多选项'}
      if($more){Invoke-Control $more}
    }
    Check (Wait-Until {[S204Window]::Find(0,'#32768') -ne [IntPtr]::Zero}) ($phase+' opens native classic context menu')
    if($expected) {
      Check (Wait-Until {$null -ne (Find-Menu 'vaultsyncd')}) ($phase+' contains one vaultsyncd parent')
      # Physical hover lets Explorer send WM_INITMENUPOPUP. UIA ExpandCollapse can
      # expose an uninitialized empty flyout even for a correctly registered cascade.
      $parent=Find-Menu 'vaultsyncd';$rect=$parent.Current.BoundingRectangle
      [S204Window]::Hover([int]($rect.X+$rect.Width/2),[int]($rect.Y+$rect.Height/2))
      if(!(Wait-Until {$null -ne (Find-Menu '查看文件历史')} 3)){Invoke-Control $parent}
      foreach($label in @('查看文件历史','下载文件','释放空间')){Check (Wait-Until {$null -ne (Find-Menu $label)}) ($phase+' has child '+$label)}
    }else {
      Start-Sleep -Milliseconds 400
      Check ($null -eq (Find-Menu 'vaultsyncd')) ($phase+' hides vaultsyncd outside binding')
    }
    Screenshot $phase
  }
  function Open-File([string]$action,[string]$path) {
    $script:fileProcess=Start-Process $desktopFile -ArgumentList ('--shell-action '+$action+' -- "'+$path+'"') -PassThru -RedirectStandardError (Join-Path $PSScriptRoot ($action+'-'+[Guid]::NewGuid().ToString('N')+'.log'))
    $null=$fileProcess.Handle
    Check (Wait-Until {$fileProcess.Refresh();$script:window=[S204Window]::Find($fileProcess.Id,'Tauri Window');!$fileProcess.HasExited -and $script:window -ne [IntPtr]::Zero} 90) ($action+' opens a separate production file window')
  }
  function Close-File {
    if($fileProcess -and !$fileProcess.HasExited){[void][S204Window]::PostMessageW($window,0x10,[IntPtr]::Zero,[IntPtr]::Zero);Check ($fileProcess.WaitForExit(15000)) 'file window closes without stopping agent';$fileProcess.WaitForExit();Check ($fileProcess.ExitCode -eq 0) 'file window exits normally'}
    $script:fileProcess=$null;$script:window=[IntPtr]::Zero
    $null=Invoke-Cli @('status')
  }
  try {
    Check ($session -ne 0) 'real interactive Windows desktop'
    [void][S204Window]::SetThreadExecutionState(0x80000003L)
    $existing=@(Get-Process vaultsyncd-agent,vaultsyncd-desktop -ErrorAction SilentlyContinue | Where-Object {$_.SessionId -eq $session})
    Check ($existing.Count -eq 0) 'no preexisting client in interactive session'
    $priorMenu=Test-Path -LiteralPath $psRegistry
    if($priorMenu){Check ((Get-ItemProperty -LiteralPath $psRegistry).Owner -eq 'org.vaultsyncd.desktop.explorer-menu') 'existing registry key belongs to vaultsyncd'; & reg.exe export $registry $menuBackup /y | Out-Null;Check ($LASTEXITCODE -eq 0) 'prior menu registration backed up'}
    $restoreMenu=$true
    $settings=Join-Path $env:LOCALAPPDATA 'vaultsyncd/desktop-settings.json'
    New-Item -ItemType Directory -Path (Split-Path $settings) | Out-Null
    [IO.File]::WriteAllText($settings,'{"close_action":"exit","stop_agent_on_exit":false}')
    $fixture=Get-Content (Join-Path $PSScriptRoot 'fixture.json') -Raw | ConvertFrom-Json
    $ownsSession=$true
    $main=Start-Process $desktopFile -PassThru -RedirectStandardError (Join-Path $PSScriptRoot 'main.log');$null=$main.Handle
    Check (Wait-Until {[S204Window]::Find($main.Id,'Tauri Window') -ne [IntPtr]::Zero} 90) 'production main window opens'
    $null=Invoke-Cli @('set-connection','--server-id',$fixture.serverId,'--server-url',$fixture.url,'--ca-file',(Join-Path $PSScriptRoot 'ca.pem'))
    $loginRaw=$fixture.password | & $cli --json login --username $fixture.login 2>&1
    Check ($LASTEXITCODE -eq 0) 'real isolated HTTPS login succeeds'
    $binding=[Guid]::NewGuid().ToString('N');$local=Join-Path $PSScriptRoot '绑定 & %PATH% 文件';$cache=Join-Path $PSScriptRoot 'binding-cache'
    $outside=Join-Path $PSScriptRoot 'outside';$neighbor=$local+'-neighbor'
    New-Item -ItemType Directory -Path $local,$cache,$outside,$neighbor | Out-Null
    $name='文档 & %PATH% 空格.txt';$file=Join-Path $local $name
    foreach($p in @($outside,$neighbor)){[IO.File]::WriteAllText((Join-Path $p $name),'S204 unrelated file')}
    $null=Invoke-Cli @('create-binding','--binding-id',$binding,'--vault-id',$fixture.vault,'--local-path',$local,'--cache-path',$cache,'--mode','full')
    Check (Wait-Until {$s=Invoke-Cli @('status');@($s.bindings | Where-Object {$_.binding_id -eq $binding -and $_.synchronized}).Count -eq 1} 120) 'real CFAPI full binding reaches committed baseline'
    Check (Wait-Until {(Test-Path -LiteralPath $psRegistry) -and (Get-ItemProperty -LiteralPath $psRegistry).AppliesTo.Contains($local)}) 'main poll registers binding-scoped menu'
    $menu=Get-ItemProperty -LiteralPath $psRegistry
    Check ($menu.ExtendedSubCommandsKey -eq '*\shell\vaultsyncd' -and $null -eq $menu.SubCommands) 'cascade uses registered nested subverbs without a CommandStore list'
    $ownedKey=[Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Software\Classes\*\shell\vaultsyncd',$true)
    $ownedKey.SetValue('SubCommands','',[Microsoft.Win32.RegistryValueKind]::String)
    $ownedKey.SetValue('','stale',[Microsoft.Win32.RegistryValueKind]::String)
    $ownedKey.Close()
    Check (Wait-Until {$m=Get-ItemProperty -LiteralPath $psRegistry;$k=Get-Item -LiteralPath $psRegistry;$default=$k.GetValue('',$null);$k.Close();$null -eq $m.SubCommands -and $null -eq $default -and $m.ExtendedSubCommandsKey -eq '*\shell\vaultsyncd'}) 'ordinary polling repairs obsolete empty-menu registry values'
    & reg.exe query $registry /s | Out-File (Join-Path $PSScriptRoot 'registered-menu.txt') -Encoding UTF8
    Check ([IO.File]::ReadAllText($file) -eq 'S204 committed server bytes') 'special filename bytes originate from real server'
    Open-Menu (Join-Path $outside $name) $false 'outside-menu'; Close-Explorer
    Open-Menu (Join-Path $neighbor $name) $false 'same-prefix-menu'; Close-Explorer
    Open-File 'history' (Join-Path $neighbor $name)
    Check (Wait-Until {(Window-Text).Contains('此文件不在活动的同步目录中')}) 'direct invocation rejects same-prefix path outside active binding'
    Close-File
    Open-Menu $file $true 'bound-menu'
    $before=@(Get-Process vaultsyncd-desktop -ErrorAction SilentlyContinue | ForEach-Object {$_.Id})
    Invoke-Control (Find-Menu '查看文件历史')
    Check (Wait-Until {$script:fileProcess=Get-Process vaultsyncd-desktop -ErrorAction SilentlyContinue | Where-Object {$_.SessionId -eq $session -and $_.Id -notin $before} | Select-Object -First 1;$null -ne $script:fileProcess} 90) 'actual Explorer history verb launches production desktop'
    $null=$fileProcess.Handle
    Check (Wait-Until {$script:window=[S204Window]::Find($fileProcess.Id,'Tauri Window');$script:window -ne [IntPtr]::Zero} 90) 'actual menu invocation displays file window'
    $entry=(File-Row).entry_id
    $history=Invoke-Cli @('history','--binding-id',$binding,'--entry-id',$entry)
    $version=$history.management.items[0].version_id
    Check ($null -ne $version) 'authenticated agent provides committed file history'
    Check (Wait-Until {(Window-Text).Contains($name) -and (Window-Text).Contains($version)} 60) 'file window shows exact selected file and real committed version'
    Screenshot 'history-window';Close-Explorer;Close-File
    [void][S204Window]::PostMessageW([S204Window]::Find($main.Id,'Tauri Window'),0x10,[IntPtr]::Zero,[IntPtr]::Zero)
    Check ($main.WaitForExit(15000)) 'old program location main window exits normally'
    $movedProgram=Join-Path $PSScriptRoot 'program moved & %PATH%'
    Copy-Item -LiteralPath $program -Destination $movedProgram -Recurse
    $desktopFile=Join-Path $movedProgram 'vaultsyncd-desktop.exe'
    $main=Start-Process $desktopFile -PassThru -RedirectStandardError (Join-Path $PSScriptRoot 'moved-main.log');$null=$main.Handle
    Check (Wait-Until {[S204Window]::Find($main.Id,'Tauri Window') -ne [IntPtr]::Zero} 90) 'portable client starts from a different program directory'
    Check (Wait-Until {(Get-ItemProperty -LiteralPath $psRegistry).Icon.Contains($desktopFile)}) 'menu command registration follows current executable location'
    Open-Menu $file $true 'moved-program-menu'
    $before=@(Get-Process vaultsyncd-desktop -ErrorAction SilentlyContinue | ForEach-Object {$_.Id})
    Invoke-Control (Find-Menu '查看文件历史')
    Check (Wait-Until {$script:fileProcess=Get-Process vaultsyncd-desktop -ErrorAction SilentlyContinue | Where-Object {$_.SessionId -eq $session -and $_.Id -notin $before} | Select-Object -First 1;$null -ne $script:fileProcess} 90) 'actual menu invokes the moved program path literally'
    $null=$fileProcess.Handle
    Check (Wait-Until {$script:window=[S204Window]::Find($fileProcess.Id,'Tauri Window');$script:window -ne [IntPtr]::Zero} 90) 'moved program history window opens'
    Check (Wait-Until {(Window-Text).Contains($name) -and (Window-Text).Contains($version)} 60) 'special program and file paths retain literal percent and ampersand characters'
    Close-Explorer;Close-File
    Open-File 'release' $file
    Check (Wait-Until {$null -ne (Find-Control '释放空间')}) 'full-mode file release view loads'
    Check (!(Find-Control '释放空间').Current.IsEnabled) 'full binding disables release'
    Close-File
    $null=Invoke-Cli @('mode','--binding-id',$binding,'--mode','on_demand')
    Check (Wait-Until {$row=File-Row;$row.release_allowed -and !$row.dirty -and !$row.pending} 90) 'on-demand confirmed file becomes releasable'
    Open-File 'release' $file;Click-Control '释放空间'
    Check (Wait-Until {(Window-Text).Contains('离线将不可读取')}) 'release requires explicit impact confirmation'
    Screenshot 'release-confirmation';Click-Control '确认释放'
    Check (Wait-Until {$row=File-Row;$row.residency -eq 'metadata_only'} 60) 'confirmed release creates a cloud-only placeholder through real CFAPI'
    Close-File
    Open-File 'download' $file;Click-Control '下载到本地'
    Check (Wait-Until {$row=File-Row;$row.residency -eq 'materialized' -and !$row.pending} 90) 'download repopulates real CFAPI file'
    Check ([IO.File]::ReadAllText($file) -eq 'S204 committed server bytes') 'downloaded bytes match committed server content'
    Close-File
    $lock=[IO.File]::Open($file,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::None)
    try{
      Open-File 'release' $file
      Check (Wait-Until {$c=Find-Control '释放空间';($c -and !$c.Current.IsEnabled) -or (Window-Text)-match '无法|失败|不可用|正在使用|未确认'} 60) 'busy file cannot be released'
      Screenshot 'busy-protection';Close-File
    }finally{$lock.Dispose();$lock=$null}
    $null=Invoke-Cli @('pause','--binding-id',$binding)
    [IO.File]::WriteAllText($file,'S204 unconfirmed local content')
    $dirtyHash=(Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash
    Open-File 'release' $file
    Check (Wait-Until {$c=Find-Control '释放空间';($c -and !$c.Current.IsEnabled) -or (Window-Text)-match '未确认|修改|无法|失败'} 60) 'unconfirmed local content cannot be released'
    Close-File
    Check ((Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash -eq $dirtyHash) 'dirty bytes survive refused release'
    $null=Invoke-Cli @('resume','--binding-id',$binding)
    Check (Wait-Until {$row=File-Row;$row.synchronized -and !$row.dirty -and !$row.pending} 90) 'edited content receives real server confirmation'
    Open-File 'release' $file;Click-Control '释放空间'
    $renamed=Join-Path $local 'moved-after-confirmation.txt'
    Move-Item -LiteralPath $file -Destination $renamed
    Click-Control '确认释放'
    Check (Wait-Until {(Window-Text)-match '变化|失败|无法|核对原操作'} 60) 'path move after confirmation is rejected during operation recheck'
    Close-File
    Check (Wait-Until {$s=Invoke-Cli @('status');@($s.bindings | Where-Object {$_.binding_id -eq $binding -and $_.synchronized}).Count -eq 1} 90) 'moved file is reconciled before detachment'
    Check (Wait-Until {
      $movedRows=(Invoke-Cli @('files','--binding-id',$binding)).management.files
      $movedRows | ConvertTo-Json -Depth 6 | Set-Content (Join-Path $PSScriptRoot 'moved-before-read-result.json') -Encoding UTF8
      @($movedRows | Where-Object {$_.path -eq 'moved-after-confirmation.txt' -and $_.residency -eq 'materialized' -and !$_.pending -and !$_.dirty}).Count -eq 1
    } 90) 'stale release leaves moved file resident before any content read'
    Check ((Get-FileHash -LiteralPath $renamed -Algorithm SHA256).Hash -eq $dirtyHash) 'moved file bytes survive stale release action'
    $null=Invoke-Cli @('detach','--binding-id',$binding)
    Check (Wait-Until {!(Test-Path -LiteralPath $psRegistry)} 60) 'last binding detachment removes menu registration'
    Open-File 'history' $renamed
    Check (Wait-Until {(Window-Text).Contains('此文件不在活动的同步目录中')} 60) 'detached path cannot be used by a stale menu invocation'
    Close-File
    [void][S204Window]::PostMessageW([S204Window]::Find($main.Id,'Tauri Window'),0x10,[IntPtr]::Zero,[IntPtr]::Zero)
    Check ($main.WaitForExit(15000)) 'main exits with keep-agent preference'
    $main=$null
    $null=Invoke-Cli @('status')
    $unregister=Start-Process $desktopFile -ArgumentList '--unregister-shell-menu' -PassThru;$null=$unregister.Handle;$unregister.WaitForExit()
    Check ($unregister.ExitCode -eq 0) 'portable menu unregister succeeds without an active binding'
    @{ok=$true;checks=$checks.ToArray();count=$checks.Count;session=$session;native='Windows 11 x64';scope='real Explorer classic cascade, actual history verb, production WebView, HTTPS/CFAPI release and download; full/busy/dirty/move/detach protection'} | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $PSScriptRoot 'result.json') -Encoding UTF8
    $exitCode='0'
  }catch{
    $_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'error.txt')
    try{if($window -ne [IntPtr]::Zero){Window-Text | Set-Content (Join-Path $PSScriptRoot 'ui-tree.txt') -Encoding UTF8};Screenshot 'failure'}catch{}
  }finally{
    if($lock){$lock.Dispose()}
    try{Close-Explorer}catch{}
    try{if($fileProcess){Close-File}}catch{$_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'file-cleanup-error.txt')}
    if($main -and !$main.HasExited){[void][S204Window]::PostMessageW([S204Window]::Find($main.Id,'Tauri Window'),0x10,[IntPtr]::Zero,[IntPtr]::Zero);[void]$main.WaitForExit(15000)}
    try{if($ownsSession){
      $out=Join-Path $PSScriptRoot 'shutdown.log';$err=$out+'.err'
      $stop=Start-Process $agentFile -ArgumentList @('--user-session','shutdown') -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
      $null=$stop.Handle
      if(!$stop.WaitForExit(90000)){throw 'isolated agent shutdown timed out; preserve process and state'}
      $stop.WaitForExit()
      if($stop.ExitCode -ne 0){throw 'isolated agent shutdown failed; preserve process and state'}
    }}catch{$_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'agent-cleanup-error.txt');$exitCode='1'}
    try{if($restoreMenu){
      $remove=Start-Process $desktopFile -ArgumentList '--unregister-shell-menu' -PassThru;$null=$remove.Handle;$remove.WaitForExit()
      if($remove.ExitCode -ne 0){throw 'menu unregister failed'}
      if($priorMenu){& reg.exe import $menuBackup | Out-Null;if($LASTEXITCODE -ne 0){throw 'prior menu restoration failed'}}
    }}catch{$_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'menu-cleanup-error.txt');$exitCode='1'}
    Remove-Item (Join-Path $PSScriptRoot 'fixture.json') -ErrorAction SilentlyContinue
    [void][S204Window]::SetThreadExecutionState(0x80000000L)
  }
  $exitCode | Set-Content (Join-Path $PSScriptRoot 'exit.txt')
  exit
}
$hashes=Get-Content (Join-Path $PSScriptRoot 'hashes.json') -Raw | ConvertFrom-Json
foreach($item in $hashes.PSObject.Properties){if((Get-FileHash (Join-Path (Join-Path $PSScriptRoot 'program') $item.Name) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $item.Value){throw 'test transport hash mismatch'}}
$task='vaultsyncd-s204-'+[Guid]::NewGuid().ToString('N')
$action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker"
$principal=New-ScheduledTaskPrincipal -UserId 'Quickemu' -LogonType Interactive -RunLevel Limited
try{
  Register-ScheduledTask -TaskName $task -Action $action -Principal $principal | Out-Null
  Start-ScheduledTask -TaskName $task
  $end=(Get-Date).AddSeconds(900)
  while(!(Test-Path (Join-Path $PSScriptRoot 'exit.txt'))){if((Get-Date)-ge $end){throw 'interactive S204 timeout; preserve run'};Start-Sleep -Milliseconds 200}
  if((Get-Content (Join-Path $PSScriptRoot 'exit.txt')) -ne '0'){Get-Content (Join-Path $PSScriptRoot 'error.txt') -ErrorAction SilentlyContinue;exit 1}
  Get-Content (Join-Path $PSScriptRoot 'result.json')
}finally{Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue}
