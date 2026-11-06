param(
  [Parameter(Mandatory=$true)][string]$Agent,
  [Parameter(Mandatory=$true)][string]$Cli,
  [Parameter(Mandatory=$true)][string]$Base,
  [Parameter(Mandatory=$true)][string]$CaSha256,
  [Parameter(Mandatory=$true)][string]$AgentSha256,
  [Parameter(Mandatory=$true)][string]$CliSha256
)
$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
if($Base -notmatch '^C:\\vaultsyncd-lab\\S097-[a-zA-Z0-9-]+$'){throw 'unsafe trial path'}
$serverId='9e9d06c5d0d371a43c2cb4043de02cd8'
$target="vaultsyncd/session/$serverId"
$ca=Join-Path $Base 'development-ca.pem'
$caCertificate=New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($ca)
$actualCa=[BitConverter]::ToString([System.Security.Cryptography.SHA256]::Create().ComputeHash($caCertificate.RawData)).Replace('-','')
if($actualCa -ne $CaSha256.Replace(':','')){throw 'development CA fingerprint differs from explicit expected fingerprint'}
if((Get-FileHash $Agent -Algorithm SHA256).Hash -ne $AgentSha256){throw 'agent is not the receipt-verified product'}
if((Get-FileHash $Cli -Algorithm SHA256).Hash -ne $CliSha256){throw 'CLI is not the receipt-verified product'}
Add-Type -TypeDefinition @'
using System;using System.Runtime.InteropServices;
public static class VSS097Credential {
 [DllImport("advapi32.dll",CharSet=CharSet.Unicode,SetLastError=true)]static extern bool CredReadW(string target,int type,int flags,out IntPtr value);
 [DllImport("advapi32.dll")]static extern void CredFree(IntPtr value);
 public static bool Exists(string target){IntPtr value;bool ok=CredReadW(target,1,0,out value);if(ok)CredFree(value);else if(Marshal.GetLastWin32Error()!=1168)throw new Exception("credential query failed");return ok;}
}
'@
if([VSS097Credential]::Exists($target)){throw 'existing trial server credential must be retained, refusing overwrite'}
$loginJson=[Console]::In.ReadToEnd()
$login=$loginJson|ConvertFrom-Json
if($login.login -ne 'owner' -or [string]::IsNullOrWhiteSpace($login.password)){throw 'missing anonymous-stdin login input'}
$state=Join-Path $Base 'state'
if(Test-Path $state){throw 'trial state already exists'}
New-Item -ItemType Directory $state|Out-Null
$config=Join-Path $Base 'agent.json'
$settings=@{server=@{id=$serverId;url='https://10.0.2.2:18443';development_http=$false};device_id=[Guid]::NewGuid().ToString('N');state_dir=$state;poll_millis=1000;bindings=@()}
$agentProcess=$null;$edge=$null;$productLogin=$false
$result=@{passed=$false;agent=$Agent;agent_sha256=$AgentSha256;cli_sha256=$CliSha256;ca_sha256=$actualCa;checks=@()}
function Invoke-ProductLogin {
 $settings|ConvertTo-Json -Depth 8|Set-Content -Encoding UTF8 $config
 # UTF8 in Windows PowerShell includes a BOM; the product expects strict JSON bytes.
 [IO.File]::WriteAllText($config,($settings|ConvertTo-Json -Depth 8),(New-Object Text.UTF8Encoding($false)))
 $p=New-Object Diagnostics.Process
 $p.StartInfo.FileName=$Agent;$p.StartInfo.Arguments="--config `"$config`" --login-stdin"
 $p.StartInfo.UseShellExecute=$false;$p.StartInfo.RedirectStandardInput=$true;$p.StartInfo.RedirectStandardOutput=$true;$p.StartInfo.RedirectStandardError=$true;$p.StartInfo.CreateNoWindow=$true
 [void]$p.Start();$p.StandardInput.Write($loginJson);$p.StandardInput.Close()
 if(-not $p.WaitForExit(45000)){$p.Kill();throw 'native TLS login timed out'}
 $out=$p.StandardOutput.ReadToEnd();$err=$p.StandardError.ReadToEnd()
 if($out.Contains($login.password)-or $err.Contains($login.password)){throw 'native product exposed password'}
 return @{code=$p.ExitCode;stderr=$err}
}
try {
 $untrusted=Invoke-ProductLogin
 if($untrusted.code -eq 0 -or $untrusted.stderr -notmatch 'certificate'){throw 'untrusted native product TLS did not fail as certificate error'}
 if([VSS097Credential]::Exists($target)){throw 'failed TLS request persisted a credential'}
 $result.checks+= 'native-product-default-roots-reject-private-CA-without-persisting-session'
 $settings.server.ca_file=$ca
 $settings.server.url='https://10.0.2.2:18444'
 $mismatch=Invoke-ProductLogin
 if($mismatch.code -eq 0 -or $mismatch.stderr -notmatch 'certificate'){throw 'explicit native CA disabled hostname validation'}
 if([VSS097Credential]::Exists($target)){throw 'wrong-host TLS persisted a credential'}
 $result.checks+='native-product-explicit-CA-still-rejects-wrong-host-certificate'
 $settings.server.url='https://10.0.2.2:18443'
 $trusted=Invoke-ProductLogin
 if($trusted.code -ne 0 -or $trusted.stderr -notmatch 'login-stored'){throw 'native product explicit ca_file login failed'}
 $productLogin=$true
 if(-not [VSS097Credential]::Exists($target)){throw 'native successful login did not use Credential Manager'}
 $result.checks+='receipt-verified-product-explicit-ca_file-HTTPS-login-and-Credential-Manager'
 # Start only this disposable profile; parent grants the global per-user agent window.
 $agentProcess=New-Object Diagnostics.Process
 $agentProcess.StartInfo.FileName=$Agent;$agentProcess.StartInfo.Arguments="--config `"$config`""
 $agentProcess.StartInfo.UseShellExecute=$false;$agentProcess.StartInfo.RedirectStandardOutput=$true;$agentProcess.StartInfo.RedirectStandardError=$true;$agentProcess.StartInfo.CreateNoWindow=$true
 [void]$agentProcess.Start()
 $ready=$false
 for($n=0;$n -lt 100;$n++){
  if($agentProcess.HasExited){throw 'trial agent exited before IPC became ready'}
  $status=& $Cli --json status 2>$null
  if($LASTEXITCODE -eq 0){$ready=$true;break};Start-Sleep -Milliseconds 200
 }
 if(-not $ready){throw 'trial same-user IPC not ready'}
 $logout=& $Cli --json logout
 if($LASTEXITCODE -ne 0 -or [VSS097Credential]::Exists($target)){throw 'native HTTPS logout did not revoke and remove credential'}
 $productLogin=$false
 $result.checks+='native-CLI-same-user-IPC-HTTPS-logout-removes-exact-test-credential'
 if(-not $agentProcess.HasExited){$agentProcess.Kill();$agentProcess.WaitForExit()}
 $agentProcess=$null
 # Explicitly trust the already fingerprint-checked PUBLIC CA for this Windows user only.
 $store=New-Object System.Security.Cryptography.X509Certificates.X509Store('Root','CurrentUser')
 $store.Open([System.Security.Cryptography.X509Certificates.OpenFlags]::ReadWrite)
 try{$store.Add($caCertificate)}finally{$store.Close()}
 $result.checks+='fingerprint-checked-public-development-CA-added-to-CurrentUser-Root'
 $edgePath=Join-Path ${env:ProgramFiles(x86)} 'Microsoft\Edge\Application\msedge.exe'
 if(-not (Test-Path $edgePath)){$edgePath=Join-Path $env:ProgramFiles 'Microsoft\Edge\Application\msedge.exe'}
 if(-not (Test-Path $edgePath)){throw 'real Windows Edge missing'}
 $profile=Join-Path $Base 'edge-runtime'
 $edge=Start-Process -FilePath $edgePath -ArgumentList @('--headless=new','--no-first-run','--no-default-browser-check','--no-proxy-server','--remote-debugging-address=127.0.0.1','--remote-debugging-port=9227',"--user-data-dir=$profile") -PassThru
 $result.edge_pid=$edge.Id;$result.edge_path=$edgePath;$result.passed=$true
 $result|ConvertTo-Json -Depth 8|Set-Content -Encoding UTF8 (Join-Path $Base 'native-result.json')
 # Keep the SSH job alive until the host completes actual Edge login/upload/SSE.
 for($n=0;$n -lt 900;$n++){if(Test-Path (Join-Path $Base 'stop-edge')){break};Start-Sleep -Milliseconds 500}
} finally {
 $loginJson=$null;$login=$null
 if($agentProcess -and -not $agentProcess.HasExited){$agentProcess.Kill();$agentProcess.WaitForExit()}
 if($edge -and -not $edge.HasExited){$edge.Kill();$edge.WaitForExit()}
 if(Test-Path (Join-Path $Base 'edge-runtime')){Remove-Item -Recurse -Force (Join-Path $Base 'edge-runtime')}
 # Never delete a credential that existed before this isolated test.
 if($productLogin){cmdkey.exe /delete:$target|Out-Null}
}
