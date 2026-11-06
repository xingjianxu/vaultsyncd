param(
 [Parameter(Mandatory=$true)][string]$Agent,
 [Parameter(Mandatory=$true)][string]$Cli,
 [Parameter(Mandatory=$true)][string]$Base,
 [Parameter(Mandatory=$true)][string]$CaSha256,
 [Parameter(Mandatory=$true)][string]$AgentSha256,
 [Parameter(Mandatory=$true)][string]$CliSha256,
 [string]$InputPipe,
 [switch]$Worker
)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
if($Base -notmatch '^C:\\vaultsyncd-lab\\S097-[a-zA-Z0-9-]+$'){throw 'unsafe trial path'}
$exitFile=Join-Path $Base 'interactive.exit'
if($Worker){
 try{
  & (Join-Path $Base 'windows-native.ps1') -Agent $Agent -Cli $Cli -Base $Base -CaSha256 $CaSha256 -AgentSha256 $AgentSha256 -CliSha256 $CliSha256 -InputPipe $InputPipe
  '0'|Set-Content $exitFile
 }catch{
  # No input, credential payload, process logs or exception call stacks are serialized.
  [pscustomobject]@{passed=$false;type=$_.Exception.GetType().Name;message=$_.Exception.Message.Split([char]10)[0]}|ConvertTo-Json|Set-Content (Join-Path $Base 'interactive-error.json')
  '1'|Set-Content $exitFile
 }
 exit
}
if(Test-Path $exitFile){throw 'refusing to reuse completed native run'}
$login=[Console]::In.ReadToEnd()
$sid=[Security.Principal.WindowsIdentity]::GetCurrent().User
$security=New-Object IO.Pipes.PipeSecurity
$security.SetAccessRuleProtection($true,$false)
$security.AddAccessRule((New-Object IO.Pipes.PipeAccessRule($sid,[IO.Pipes.PipeAccessRights]::FullControl,[Security.AccessControl.AccessControlType]::Allow)))
$system=New-Object Security.Principal.SecurityIdentifier('S-1-5-18')
$security.AddAccessRule((New-Object IO.Pipes.PipeAccessRule($system,[IO.Pipes.PipeAccessRights]::FullControl,[Security.AccessControl.AccessControlType]::Allow)))
$pipeName='vaultsyncd-s097-'+[Guid]::NewGuid().ToString('N')
$pipe=New-Object IO.Pipes.NamedPipeServerStream($pipeName,[IO.Pipes.PipeDirection]::Out,1,[IO.Pipes.PipeTransmissionMode]::Byte,[IO.Pipes.PipeOptions]::Asynchronous,4096,4096,$security)
$task='vaultsyncd-s097-'+[Guid]::NewGuid().ToString('N')
$arguments="-NoProfile -NonInteractive -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker -Agent `"$Agent`" -Cli `"$Cli`" -Base `"$Base`" -CaSha256 $CaSha256 -AgentSha256 $AgentSha256 -CliSha256 $CliSha256 -InputPipe $pipeName"
$action=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument $arguments
$principal=New-ScheduledTaskPrincipal -UserId 'Quickemu' -LogonType Interactive -RunLevel Limited
try{
 Register-ScheduledTask -TaskName $task -Action $action -Principal $principal|Out-Null
 Start-ScheduledTask -TaskName $task
 $waiting=$pipe.WaitForConnectionAsync()
 if(-not $waiting.Wait(30000)){throw 'interactive secret pipe did not connect'}
 $writer=New-Object IO.StreamWriter($pipe,(New-Object Text.UTF8Encoding($false)))
 $writer.Write($login);$writer.Flush();$writer.Dispose();$pipe.Dispose();$login=$null
 $end=(Get-Date).AddSeconds(900)
 while(-not(Test-Path $exitFile)){
  if((Get-Date)-ge $end){throw 'S097 interactive worker deadline exceeded'}
  Start-Sleep -Milliseconds 300
 }
 $code=[int](Get-Content $exitFile)
 if($code -ne 0 -and(Test-Path (Join-Path $Base 'interactive-error.json'))){Get-Content (Join-Path $Base 'interactive-error.json')}
 exit $code
}finally{
 $login=$null;$pipe.Dispose()
 if(-not(Test-Path $exitFile)){Stop-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue}
 Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
}
