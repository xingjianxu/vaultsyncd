param([switch]$Worker)
$ErrorActionPreference='Stop'
if($Worker){
  $agent=$null;$desktop=$null;$counter=0
  $pref=Join-Path $env:LOCALAPPDATA 'vaultsyncd/desktop-notifications.json'
  $shortcut=Join-Path $env:APPDATA 'Microsoft/Windows/Start Menu/Programs/vaultsyncd.lnk'
  $hadPref=Test-Path $pref;$hadShortcut=Test-Path $shortcut
  if($hadPref){Copy-Item $pref (Join-Path $PSScriptRoot 'prior-prefs')}
  if($hadShortcut){Copy-Item $shortcut (Join-Path $PSScriptRoot 'prior-shortcut.lnk')}
  try{
    $state=Join-Path $PSScriptRoot 'state';New-Item -ItemType Directory $state|Out-Null
    & (Join-Path $PSScriptRoot 's083_notice_fixture.exe') $state seed
    if($LASTEXITCODE -ne 0){throw 'offline core fixture failed'}
    $config=@{server=@{id=[Guid]::NewGuid().ToString('N');url='https://example.invalid'};device_id=[Guid]::NewGuid().ToString('N');state_dir=$state;poll_millis=500;bindings=@()}
    [IO.File]::WriteAllText((Join-Path $PSScriptRoot 'agent.json'),($config|ConvertTo-Json -Depth 5),(New-Object Text.UTF8Encoding($false)))
    function Start-Agent { Start-Process (Join-Path $PSScriptRoot 'vaultsyncd-agent.exe') -ArgumentList @('--config',(Join-Path $PSScriptRoot 'agent.json')) -PassThru -RedirectStandardError (Join-Path $PSScriptRoot ('agent-'+$script:counter+'.log')) }
    $agent=Start-Agent;Start-Sleep -Seconds 2;if($agent.HasExited){throw 'agent startup'}
    $request=Join-Path $PSScriptRoot 'request.txt';$ack=Join-Path $PSScriptRoot 'ack.txt'
    $reports=@()
    foreach($phase in @('notifications','restart_check')){
      $fixture=@{phase=$phase;requestPath=$request;ackPath=$ack}
      $env:VAULTSYNCD_S083_FIXTURE=Join-Path $PSScriptRoot 'fixture.json'
      $env:VAULTSYNCD_S080_REPORT=Join-Path $PSScriptRoot ($phase+'-report.json')
      [IO.File]::WriteAllText($env:VAULTSYNCD_S083_FIXTURE,($fixture|ConvertTo-Json),(New-Object Text.UTF8Encoding($false)))
      $desktop=Start-Process (Join-Path $PSScriptRoot 'vaultsyncd-desktop.exe') -PassThru -RedirectStandardError (Join-Path $PSScriptRoot ($phase+'-desktop.log'))
      $end=(Get-Date).AddSeconds(300)
      while(!$desktop.HasExited){
        if((Get-Date)-ge $end){throw 'real notification WebView timeout'}
        if(Test-Path $request){
          $action=[IO.File]::ReadAllText($request);Remove-Item $request
          if(!$agent.HasExited){Stop-Process -Id $agent.Id -Force;$agent.WaitForExit()}
          if(!$agent.HasExited){throw 'refuse live DB fixture write'}
          & (Join-Path $PSScriptRoot 's083_notice_fixture.exe') $state $action
          if($LASTEXITCODE -ne 0){throw 'offline core phase failed'}
          $script:counter++;$agent=Start-Agent;Start-Sleep -Seconds 1
          if($agent.HasExited){throw 'agent restart failed'}
          [IO.File]::WriteAllText($ack,$action,(New-Object Text.UTF8Encoding($false)))
        }
        Start-Sleep -Milliseconds 100;$desktop.Refresh()
      }
      $report=Get-Content $env:VAULTSYNCD_S080_REPORT -Raw -Encoding UTF8|ConvertFrom-Json
      if(!$report.ok){throw ('native notification failed '+($report|ConvertTo-Json -Compress -Depth 8))}
      $reports+=$report
    }
    $reports|ConvertTo-Json -Depth 8|Set-Content (Join-Path $PSScriptRoot 'result.json') -Encoding UTF8
    '0'|Set-Content (Join-Path $PSScriptRoot 'exit.txt')
  }catch{$_|Out-String|Set-Content (Join-Path $PSScriptRoot 'error.txt');'1'|Set-Content (Join-Path $PSScriptRoot 'exit.txt')}
  finally{
    if($desktop -and !$desktop.HasExited){Stop-Process -Id $desktop.Id -Force}
    if($agent -and !$agent.HasExited){Stop-Process -Id $agent.Id -Force;$agent.WaitForExit()}
    if($hadPref){Copy-Item (Join-Path $PSScriptRoot 'prior-prefs') $pref -Force}elseif(Test-Path $pref){Remove-Item $pref}
    if($hadShortcut){Copy-Item (Join-Path $PSScriptRoot 'prior-shortcut.lnk') $shortcut -Force}elseif(Test-Path $shortcut){Remove-Item $shortcut}
  }
  exit
}
$hashes=Get-Content (Join-Path $PSScriptRoot 'hashes.json') -Raw|ConvertFrom-Json
foreach($item in $hashes.PSObject.Properties){if((Get-FileHash (Join-Path $PSScriptRoot $item.Name) -Algorithm SHA256).Hash.ToLowerInvariant()-ne $item.Value){throw 'transport hash mismatch'}}
$task='vaultsyncd-s083-'+[Guid]::NewGuid().ToString('N')
$action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker"
$principal=New-ScheduledTaskPrincipal -UserId 'Quickemu' -LogonType Interactive -RunLevel Limited
try{
  Register-ScheduledTask -TaskName $task -Action $action -Principal $principal|Out-Null;Start-ScheduledTask -TaskName $task
  $end=(Get-Date).AddSeconds(650)
  while(!(Test-Path (Join-Path $PSScriptRoot 'exit.txt'))){if((Get-Date)-ge $end){throw 'interactive worker timeout'};Start-Sleep -Milliseconds 200}
  if((Get-Content (Join-Path $PSScriptRoot 'exit.txt'))-ne '0'){Get-Content (Join-Path $PSScriptRoot 'error.txt');Get-Content (Join-Path $PSScriptRoot 'notifications-desktop.log') -ErrorAction SilentlyContinue;exit 1}
  Get-Content (Join-Path $PSScriptRoot 'result.json') -Encoding UTF8
}finally{Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue}
