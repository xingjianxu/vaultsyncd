param([switch]$Worker)
$ErrorActionPreference='Stop'
[Console]::OutputEncoding=New-Object Text.UTF8Encoding($false)
if($Worker){
  $driver=$null
  try{
    $env:VAULTSYNCD_S084_FIXTURE=Join-Path $PSScriptRoot 'fixture.json'
    $env:VAULTSYNCD_AGENT_EXE=Join-Path $PSScriptRoot 'vaultsyncd-agent.exe'
    $driver=Start-Process (Join-Path $PSScriptRoot 's084-native-tests.exe') -ArgumentList @('--test-threads=1','--nocapture') -PassThru -RedirectStandardOutput (Join-Path $PSScriptRoot 'native-tests.log') -RedirectStandardError (Join-Path $PSScriptRoot 'native-tests.err')
    $null=$driver.Handle
    if(!$driver.WaitForExit(300000)){throw 'native recovery driver timeout'}
    $driver.WaitForExit();$driver.Refresh()
    if($driver.ExitCode-ne 0){throw ('native recovery driver failed; exit='+$driver.ExitCode)}
    '0'|Set-Content (Join-Path $PSScriptRoot 'exit.txt')
  }catch{$_|Out-String|Set-Content (Join-Path $PSScriptRoot 'error.txt');'1'|Set-Content (Join-Path $PSScriptRoot 'exit.txt')}
  finally{
    if($driver -and !$driver.HasExited){Stop-Process -Id $driver.Id -Force;$driver.WaitForExit()}
    # Kill only recorded test-owned agent PIDs, after verifying their exact executable and config.
    foreach($file in @(Get-ChildItem $PSScriptRoot -Filter 'agent-pids.txt' -Recurse -ErrorAction SilentlyContinue)){
      foreach($ownedId in @(Get-Content $file.FullName)){
        $process=Get-CimInstance Win32_Process -Filter ('ProcessId='+[int]$ownedId)
        if($process -and $process.ExecutablePath-eq $env:VAULTSYNCD_AGENT_EXE -and $process.CommandLine.Contains($file.DirectoryName)){Stop-Process -Id ([int]$ownedId) -Force}
      }
    }
  }
  exit
}
if((Get-Volume -DriveLetter C).FileSystem-ne 'NTFS'){throw 'real NTFS required'}
$hashes=Get-Content (Join-Path $PSScriptRoot 'hashes.json') -Raw|ConvertFrom-Json
foreach($item in $hashes.PSObject.Properties){if((Get-FileHash (Join-Path $PSScriptRoot $item.Name) -Algorithm SHA256).Hash.ToLowerInvariant()-ne $item.Value){throw 'transport hash mismatch'}}
$task='vaultsyncd-s084-'+[Guid]::NewGuid().ToString('N')
$action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker"
$principal=New-ScheduledTaskPrincipal -UserId 'Quickemu' -LogonType Interactive -RunLevel Limited
try{
  Register-ScheduledTask -TaskName $task -Action $action -Principal $principal|Out-Null;Start-ScheduledTask -TaskName $task
  $end=(Get-Date).AddSeconds(350)
  while(!(Test-Path (Join-Path $PSScriptRoot 'exit.txt'))){if((Get-Date)-ge $end){throw 'interactive recovery worker timeout'};Start-Sleep -Milliseconds 200}
  Get-Content (Join-Path $PSScriptRoot 'native-tests.log') -Encoding UTF8
  Get-Content (Join-Path $PSScriptRoot 'native-tests.err') -Encoding UTF8
  if((Get-Content (Join-Path $PSScriptRoot 'exit.txt'))-ne '0'){Get-Content (Join-Path $PSScriptRoot 'error.txt');exit 1}
}finally{Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue}
