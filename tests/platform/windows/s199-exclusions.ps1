param([switch]$Worker, [switch]$PartialOnly)
$ErrorActionPreference = 'Stop'
$OutputEncoding = New-Object Text.UTF8Encoding($false)
[Console]::OutputEncoding = $OutputEncoding

function Assert([bool]$condition, [string]$message) { if (!$condition) { throw $message } }
function Write-Text([string]$path, [string]$text) {
  [IO.File]::WriteAllText($path, $text, (New-Object Text.UTF8Encoding($false)))
}
function Hash([string]$path) { (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() }
function Wait-Until([string]$description, [scriptblock]$test, [int]$seconds = 90) {
  $end = (Get-Date).AddSeconds($seconds)
  do {
    $answer = & $test
    if ($answer) { return $answer }
    Start-Sleep -Milliseconds 250
  } while ((Get-Date) -lt $end)
  throw "Timeout: $description"
}

if ($Worker) {
  $agent = $null; $fixture = $null; $root = $null; $ownedBindings = @(); $workerExit = 1
  $cli = Join-Path $PSScriptRoot 'vaultsyncd-cli.exe'
  $agentExe = Join-Path $PSScriptRoot 'vaultsyncd-agent.exe'
  function Mark([string]$phase) { Add-Content -LiteralPath (Join-Path $PSScriptRoot 'progress.log') -Value $phase }
  function Invoke-Cli([string[]]$arguments) {
    $deadline=(Get-Date).AddSeconds(30)
    do {
      $text = & $cli --json @arguments
      $exit = $LASTEXITCODE
      $raw = $text -join "`n"
      Add-Content -LiteralPath (Join-Path $PSScriptRoot 'cli.log') -Value "$($arguments -join ' ') exit=$exit $raw"
      $answer = $raw | ConvertFrom-Json
      if ($exit -eq 0 -or $answer.error.code -ne 'agent_unavailable' -or $arguments[0] -notin @('files','bindings','status','problems','rules','connections','connection') -or !$agent -or $agent.HasExited -or (Get-Date) -ge $deadline) { break }
      Start-Sleep -Milliseconds 250
    } while ($true)
    Assert ($exit -eq 0 -and $answer.ok) "CLI $($arguments[0]) exit $exit ($($answer.error.message))"
    return $answer.result
  }
  function Start-Agent([string]$phase) {
    $process = Start-Process -FilePath $agentExe -ArgumentList @('--config', ('"'+$configPath+'"')) -PassThru `
      -RedirectStandardError (Join-Path $PSScriptRoot "agent-$phase.log")
    $null = $process.Handle
    return $process
  }
  function Wait-Agent($process) {
    Wait-Until 'run-owned IPC' {
      if ($process.HasExited) { throw "run-owned agent exited $($process.ExitCode) before readiness" }
      $text = & $cli --json status
      if ($LASTEXITCODE -eq 0) { return (($text -join "`n") | ConvertFrom-Json).result }
    }
  }
  function Stop-Agent($process) {
    Assert (!$process.HasExited) 'agent exited before orderly stop'
    $null = & $agentExe --user-session stop
    Assert ($LASTEXITCODE -eq 0) 'orderly agent stop command failed'
    Assert ($process.WaitForExit(20000)) 'agent failed to exit after orderly stop'
    Assert ($process.ExitCode -eq 0) "agent orderly stop exit $($process.ExitCode)"
  }
  function Confirmed-File([string]$path) {
    Wait-Until "confirmed $path" {
      $answer = Invoke-Cli @('files','--binding-id',$binding)
      $file = @($answer.management.files | Where-Object { $_.path -eq $path })
      if ($file.Count -eq 1 -and $file[0].entry_id -and $file[0].synchronized) { return $file[0] }
    }
  }
  function Set-Rules([string]$rules) {
    Write-Text $rulesPath $rules
    $null = Invoke-Cli @('set-rules','--binding-id',$binding,'--file',$rulesPath,'--version',([Guid]::NewGuid().ToString('N')))
    $saved = Invoke-Cli @('rules','--binding-id',$binding)
    Assert ($saved.management.rules -ceq $rules) 'rules API did not preserve additional text exactly'
  }
  # This test's own observation channel pins the run's server certificate by SHA-256 before
  # any request: Windows cannot install a custom CA silently (the user root store needs an
  # interactive trust dialog, certutil returns ERROR_NOT_SUPPORTED and the in-box curl uses
  # Schannel, which ignores --cacert). The product channel (agent to server) keeps full CA
  # verification through `set-connection --ca-file`; only this pinned observation channel
  # skips the *store-based* chain that Windows refuses to build.
  # Observation channel: a run-private SSH reverse tunnel forwards the loopback backend port,
  # because Windows cannot trust this run's CA silently (user root store needs an interactive
  # dialog, certutil reports ERROR_NOT_SUPPORTED, in-box curl uses Schannel and ignores
  # --cacert). The product channel (agent to server) still verifies HTTPS with `--ca-file`;
  # only these test observations travel over the authenticated SSH tunnel in cleartext.
  function Remote([string]$method, [string]$path, $body = $null) {
    $params = @{Method=$method;Uri=($fixture.apiUrl+$path);Headers=@{Authorization=('Bearer '+$fixture.token)};TimeoutSec=20}
    if ($null -ne $body) { $params.ContentType='application/json'; $params.Body=($body | ConvertTo-Json -Depth 12 -Compress) }
    Invoke-RestMethod @params
  }
  function Remote-Rows {
    $snapshot = Remote 'POST' "/v1/vaults/$($fixture.vault)/snapshots" @{}
    $page = Remote 'GET' "/v1/vaults/$($fixture.vault)/snapshots/$($snapshot.snapshot_id)/entries"
    Assert (!$page.next_cursor) 'test snapshot unexpectedly paginated'
    return @($page.entries)
  }
  function Assert-Remote([string]$name, [string]$content) {
    $rows = @(Remote-Rows | Where-Object { $_.name -ceq $name })
    Assert ($rows.Count -eq 1 -and $rows[0].hash -eq (Text-Hash $content)) "server content missing or changed: $name"
  }
  function Text-Hash([string]$text) {
    $sha = [Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($text)))).Replace('-','').ToLowerInvariant() }
    finally { $sha.Dispose() }
  }
  function Put-Remote([string]$name, [string]$content) {
    $snapshot = Remote 'POST' "/v1/vaults/$($fixture.vault)/snapshots" @{}
    $page = Remote 'GET' "/v1/vaults/$($fixture.vault)/snapshots/$($snapshot.snapshot_id)/entries"
    $rows = @($page.entries | Where-Object { $_.name -ceq $name })
    Assert ($rows.Count -eq 1) "remote mutation target missing: $name"
    $bytes = [Text.Encoding]::UTF8.GetBytes($content); $hash = Text-Hash $content
    $upload = Remote 'POST' "/v1/vaults/$($fixture.vault)/uploads" @{hash=$hash;total_size=([string]$bytes.Length);chunk_size=([string][Math]::Max(4096,$bytes.Length))}
    $null = Invoke-RestMethod -Method PUT -Uri "$($fixture.apiUrl)/v1/vaults/$($fixture.vault)/uploads/$($upload.upload_id)/parts/0" `
      -Headers @{Authorization=('Bearer '+$fixture.token);'x-part-sha256'=$hash} -ContentType 'application/octet-stream' -Body $bytes -TimeoutSec 20
    $null = Remote 'POST' "/v1/vaults/$($fixture.vault)/uploads/$($upload.upload_id)/finalize" @{}
    $operation = Remote 'POST' "/v1/vaults/$($fixture.vault)/operations" @{
      op_id=[Guid]::NewGuid().ToString('N');device_id=[Guid]::NewGuid().ToString('N');server_epoch=$snapshot.server_epoch;
      kind='put_content';entry_id=$rows[0].entry_id;base_revision=$rows[0].revision;upload_id=$upload.upload_id;
      local_generation='1';conflict_policy='preserve_both'}
    Assert ($operation.state -eq 'committed') 'remote mutation was not committed'
    Assert-Remote $name $content
  }
  function Barrier([string]$phase) {
    $name = "barrier-$phase.txt"
    Write-Text (Join-Path $root $name) "S196-$phase"
    $null = Confirmed-File $name
    # Observe several actual poll cycles after an ordinary upload commits. Negative
    # assertions are repeated below, rather than treating a queued command as success.
    Start-Sleep -Seconds 3
    Assert-Remote $name "S196-$phase"
  }
  function Assert-Defaults {
    Assert ((Hash (Join-Path $root '~$local-default.xlsx')) -eq (Text-Hash 'S196-local-lock')) 'default exclusion lost local lock bytes'
    Assert (!(Test-Path -LiteralPath (Join-Path $root '~$remote-default.xlsx'))) 'default lock file downloaded'
    $rows = @(Remote-Rows)
    Assert (@($rows | Where-Object { $_.name -ceq '~$local-default.xlsx' }).Count -eq 0) 'default local lock uploaded'
    Assert-Remote '~$remote-default.xlsx' 'S196-remote-lock'
  }

  function Prepare-Maintenance([string]$destination) {
    New-Item -ItemType Directory -Path $destination | Out-Null
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $session = (Get-Process -Id $PID).SessionId
    $pipe = New-Object IO.Pipes.NamedPipeClientStream('.', "vaultsyncd-agent-$sid-$session", [IO.Pipes.PipeDirection]::InOut)
    try {
      $pipe.Connect(5000)
      $request = @{request_id=[Guid]::NewGuid().ToString('N');ipc_version=@{major=3;minor=1};command=@{name='prepare_maintenance';parameters=@{destination=$destination}}} | ConvertTo-Json -Depth 8 -Compress
      $bytes = [Text.Encoding]::UTF8.GetBytes($request); $header = [BitConverter]::GetBytes([uint32]$bytes.Length); [Array]::Reverse($header)
      $pipe.Write($header,0,4); $pipe.Write($bytes,0,$bytes.Length); $pipe.Flush()
      $read = 0; while ($read -lt 4) { $n=$pipe.Read($header,$read,4-$read); Assert ($n -gt 0) 'maintenance response header closed'; $read += $n }
      [Array]::Reverse($header); $length=[BitConverter]::ToUInt32($header,0); Assert ($length -le 1048576) 'maintenance response too large'
      $reply = New-Object byte[] $length; $read=0
      while ($read -lt $length) { $n=$pipe.Read($reply,$read,$length-$read); Assert ($n -gt 0) 'maintenance response closed'; $read += $n }
      $answer=[Text.Encoding]::UTF8.GetString($reply) | ConvertFrom-Json
      Write-Text (Join-Path $PSScriptRoot ('maintenance-'+(Split-Path $destination -Leaf)+'.json')) ($answer | ConvertTo-Json -Depth 12)
      Assert ($answer.kind -eq 'ok') "maintenance failed: $($answer.error.message)"
      Assert ($agent.WaitForExit(20000) -and $agent.ExitCode -eq 0) 'maintenance did not stop cleanly'
    } finally { $pipe.Dispose() }
  }
  function Export-State([string]$destination) {
    $null=Invoke-Cli @('pause','--binding-id',$binding)
    $null=Invoke-Cli @('export-recovery','--binding-id',$binding,'--destination',$destination)
    $manifest=Get-Content (Join-Path $destination 'manifest.json') -Raw | ConvertFrom-Json
    foreach($file in $manifest.files) { Assert ((Hash (Join-Path $destination $file.name)) -eq $file.sha256) "export hash: $($file.name)" }
    return (Get-Content (Join-Path $destination 'state.json') -Raw | ConvertFrom-Json)
  }
  function Partial-Info([string]$path) { return [S199Ranges]::Info($path) }
  Add-Type -TypeDefinition @'
using System; using System.IO; using System.Runtime.InteropServices; using Microsoft.Win32.SafeHandles;
public static class S199Ranges {
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern SafeFileHandle CreateFileW(string path,uint access,uint share,IntPtr security,uint creation,uint flags,IntPtr template);
 [DllImport("cldapi.dll")] static extern int CfGetPlaceholderInfo(SafeFileHandle h,int cls,byte[] buffer,uint size,out uint used);
 public static long[] Info(string path) { using(var h=CreateFileW(path,0x80,7,IntPtr.Zero,3,0x00200000,IntPtr.Zero)) {
   if(h.IsInvalid) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
   var info=new byte[8192];uint used;int hr=CfGetPlaceholderInfo(h,1,info,(uint)info.Length,out used);if(hr<0) Marshal.ThrowExceptionForHR(hr);
   return new long[]{BitConverter.ToInt64(info,0),BitConverter.ToInt64(info,16),BitConverter.ToUInt32(info,36)};
 } }
 [DllImport("cldapi.dll")] static extern int CfHydratePlaceholder(SafeFileHandle h,long offset,long length,uint flags,IntPtr parameters);
 public static void ReadRange(string path) { using(var h=CreateFileW(path,0x80,7,IntPtr.Zero,3,0x00200000,IntPtr.Zero)) {
   if(h.IsInvalid) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
   int hr=CfHydratePlaceholder(h,4000000,1048576,0,IntPtr.Zero);if(hr<0) Marshal.ThrowExceptionForHR(hr);
 } }
 public static byte[] ReadRetained(string path) { using(var h=CreateFileW(path,0x81,7,IntPtr.Zero,3,0x00300000,IntPtr.Zero)) {
   if(h.IsInvalid) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
   using(var file=new FileStream(h,FileAccess.Read)) {file.Position=4000000;var bytes=new byte[65536];if(file.Read(bytes,0,bytes.Length)!=bytes.Length) throw new IOException("retained range short read");return bytes;}
 } }
 [DllImport("cldapi.dll",CharSet=CharSet.Unicode)] static extern int CfOpenFileWithOplock(string path,uint flags,out IntPtr handle);
 [DllImport("cldapi.dll")] static extern IntPtr CfGetWin32HandleFromProtectedHandle(IntPtr handle);
 [DllImport("cldapi.dll")] static extern void CfCloseHandle(IntPtr handle);
 [DllImport("cldapi.dll")] static extern int CfSetInSyncState(IntPtr handle,int state,uint flags,IntPtr usn);
 public static void MarkUnconfirmed(string path) {
   // A metadata-only WRITE_DAC handle avoids implicit data recall and satisfies
   // CfSetInSyncState's native access check in either direction.
   using(var file=CreateFileW(path,0x00040080,7,IntPtr.Zero,3,0x00300000,IntPtr.Zero)) {
     if(file.IsInvalid)throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
     int hr=CfSetInSyncState(file.DangerousGetHandle(),0,0,IntPtr.Zero);if(hr<0)Marshal.ThrowExceptionForHR(hr);
   }
 }

}
'@

  Mark 'worker-begin'
  try {
    Assert (@(Get-Process vaultsyncd-agent,vaultsyncd-desktop -ErrorAction SilentlyContinue).Count -eq 0) 'preserve existing product processes'
    Assert ((Get-Volume -DriveLetter C).FileSystem -eq 'NTFS') 'guest-local NTFS is required'
    Assert ([Environment]::UserInteractive) 'interactive Windows session is required'
    Mark 'preconditions-checked'
    $fixturePath = Join-Path $PSScriptRoot 'fixture.json'
    $fixture = Get-Content -LiteralPath $fixturePath -Raw -Encoding UTF8 | ConvertFrom-Json
    Remove-Item -LiteralPath $fixturePath -Force
    $null = Remote 'GET' '/v1/capabilities'
    Mark 'observation-channel-ready'
    $env:LOCALAPPDATA = Join-Path $PSScriptRoot 'local'
    $env:APPDATA = Join-Path $PSScriptRoot 'roaming'
    $env:TEMP = Join-Path $PSScriptRoot 'temp'; $env:TMP = $env:TEMP
    $profile = Join-Path $env:LOCALAPPDATA 'vaultsyncd\agent'
    $statePath = Join-Path $profile 'state'; $configPath = Join-Path $profile 'agent.json'
    $root = Join-Path $PSScriptRoot 'data\root'; $cache = Join-Path $PSScriptRoot 'cache'
    $rulesPath = Join-Path $PSScriptRoot 'rules.txt'
    New-Item -ItemType Directory -Path @($statePath,$env:APPDATA,$env:TEMP,$root,$cache) | Out-Null
    $config = @{server=@{id=[Guid]::NewGuid().ToString('N');url='https://localhost'};
      device_id=[Guid]::NewGuid().ToString('N');state_dir=$statePath;poll_millis=250;bindings=@()}
    Write-Text $configPath ($config | ConvertTo-Json -Depth 5)
    Write-Text (Join-Path $root 'local.xlsx') 'S196-local-workbook'
    Write-Text (Join-Path $root '~$local-default.xlsx') 'S196-local-lock'
    Write-Text (Join-Path $root '~$include-local.xlsx') 'S196-included-local-lock'
    Mark 'start-login'
    $agent = Start-Agent 'initial'; $status = Wait-Agent $agent
    Assert (@($status.management.bindings).Count -eq 0) 'fresh profile contained bindings'
    $null = Invoke-Cli @('set-connection','--server-id',$fixture.serverId,'--server-url',$fixture.url,'--ca-file',(Join-Path $PSScriptRoot 'ca.pem'))
    $line = $fixture.password | & $cli --json login --server-id $fixture.serverId --username $fixture.login
    Assert ($LASTEXITCODE -eq 0 -and ($line | ConvertFrom-Json).result.management.connection.authenticated) 'isolated HTTPS login failed'
    $fixture.password = $null
    $binding = [Guid]::NewGuid().ToString('N'); $ownedBindings += $binding
    $null = Invoke-Cli @('create-binding','--binding-id',$binding,'--server-id',$fixture.serverId,
      '--vault-id',$fixture.vault,'--local-path',$root,'--cache-path',$cache,'--mode',$(if($PartialOnly){'on_demand'}else{'full'}))

    if (!$PartialOnly) {
    Mark 'default-only'
    $seed = Confirmed-File 'server.xlsx'; $null = Confirmed-File 'local.xlsx'
    Assert ($seed.residency -eq 'materialized') 'ordinary server workbook not fully downloaded'
    Assert ([IO.File]::ReadAllText((Join-Path $root 'server.xlsx')) -ceq 'S196-server-workbook') 'ordinary download bytes differ'
    Assert ([bool]((Get-Item -LiteralPath (Join-Path $root 'server.xlsx')).Attributes -band [IO.FileAttributes]::ReparsePoint)) 'ordinary download was not native CFAPI'
    $saved = Invoke-Cli @('rules','--binding-id',$binding)
    Assert ($saved.management.rules -ceq '') 'new binding additional rules not empty'
    Barrier 'default'; Assert-Defaults
    Assert (!(Test-Path -LiteralPath (Join-Path $root '~$reconcile.xlsx'))) 'default reconcile seed downloaded before negation'
    Assert (@(Remote-Rows | Where-Object { $_.name -ceq '~$include-local.xlsx' }).Count -eq 0) 'negation target uploaded before negation'

    Mark 'additional'
    Set-Rules "*.extra.xlsx`n"
    Write-Text (Join-Path $root 'local.extra.xlsx') 'S196-extra-workbook'
    Barrier 'additional'; Assert-Defaults
    Assert (@(Remote-Rows | Where-Object { $_.name -ceq 'local.extra.xlsx' }).Count -eq 0) 'additional rule did not stop upload'
    Assert ((Hash (Join-Path $root 'local.extra.xlsx')) -eq (Text-Hash 'S196-extra-workbook')) 'additional exclusion deleted local bytes'

    Mark 'cleared-additional'
    Set-Rules ''
    $null = Confirmed-File 'local.extra.xlsx'
    Barrier 'cleared'; Assert-Defaults
    Assert-Remote 'local.extra.xlsx' 'S196-extra-workbook'

    # The helper adopted this remote-only seed before binding creation. No remote
    # mutation occurs before this first !: rule change alone must discover it.
    Mark 'negation-initial-remote-only-without-new-event'
    $includeRules = "!~`$include-local.xlsx`n!~`$reconcile.xlsx`n"
    Set-Rules $includeRules
    $null = Confirmed-File '~$include-local.xlsx'; $null = Confirmed-File '~$reconcile.xlsx'
    Assert ([IO.File]::ReadAllText((Join-Path $root '~$reconcile.xlsx')) -ceq 'S196-remote-original') 'negation did not download default-excluded file'
    Barrier 'included'; Assert-Defaults
    Assert-Remote '~$include-local.xlsx' 'S196-included-local-lock'

    Mark 'exclude-retains-both-sides'
    Set-Rules ''
    Barrier 'retained'; Assert-Defaults
    Assert-Remote '~$reconcile.xlsx' 'S196-remote-original'
    Assert-Remote '~$include-local.xlsx' 'S196-included-local-lock'
    Assert ((Hash (Join-Path $root '~$include-local.xlsx')) -eq (Text-Hash 'S196-included-local-lock')) 're-exclusion deleted downloaded local content'

    Mark 'excluded-local-delete-must-not-delete-server'
    # Deliberately delete only this run's fully confirmed fixture while default
    # exclusion is active. Missing local bytes are not an authorized remote Delete.
    $deletePath = Join-Path $root '~$include-local.xlsx'
    Assert ((Hash $deletePath) -eq (Text-Hash 'S196-included-local-lock')) 'refuse to delete unexpected fixture bytes'
    Remove-Item -LiteralPath $deletePath -Force
    Barrier 'excluded-local-deleted'; Assert-Defaults
    Assert (!(Test-Path -LiteralPath $deletePath)) 'excluded deleted fixture unexpectedly restored before !'
    Assert-Remote '~$include-local.xlsx' 'S196-included-local-lock'
    Put-Remote '~$reconcile.xlsx' 'S196-remote-newer-while-excluded'
    Barrier 'remote-edited'; Assert-Defaults
    Assert ([IO.File]::ReadAllText((Join-Path $root '~$reconcile.xlsx')) -ceq 'S196-remote-original') 'excluded remote change overwrote retained local bytes'

    Mark 'restart-default-only'
    $absentState = Export-State (Join-Path $PSScriptRoot 'export-missing')
    Assert (@($absentState.current_observations | Where-Object { $_[1].state -eq 'absent' }).Count -eq 1) 'missing excluded file was not explicitly catalogued'
    Prepare-Maintenance (Join-Path $PSScriptRoot 'prepare-missing')
    $agent = Start-Agent 'restarted'; $null = Wait-Agent $agent
    $null=Invoke-Cli @('resume','--binding-id',$binding)
    $saved = Invoke-Cli @('rules','--binding-id',$binding)
    Assert ($saved.management.rules -ceq '') 'restart restored removed additional rules'
    Barrier 'restart'; Assert-Defaults
    Assert ([IO.File]::ReadAllText((Join-Path $root '~$reconcile.xlsx')) -ceq 'S196-remote-original') 'restart bypassed exclusion'
    Assert-Remote '~$reconcile.xlsx' 'S196-remote-newer-while-excluded'
    Assert (!(Test-Path -LiteralPath $deletePath)) 'restart ignored exclusion of a locally deleted tracked file'
    Assert-Remote '~$include-local.xlsx' 'S196-included-local-lock'

    Mark 'negation-reconciles-stale-local-and-missing-local'
    Set-Rules $includeRules
    $null = Wait-Until 're-included file reconciles to newer server bytes' {
      if ([IO.File]::ReadAllText((Join-Path $root '~$reconcile.xlsx')) -ceq 'S196-remote-newer-while-excluded') { return $true }
    }
    $null = Confirmed-File '~$reconcile.xlsx'
    # A tracked file the user deleted while it was excluded is not silently re-downloaded:
    # it keeps the reconciliation fence and the server bytes stay untouched. Only a never
    # tracked remote entry (the case above) is restored automatically.
    Assert (!(Test-Path -LiteralPath $deletePath)) 're-inclusion silently restored a user-deleted tracked file'
    Assert-Remote '~$include-local.xlsx' 'S196-included-local-lock'
    $fenced = Invoke-Cli @('files','--binding-id',$binding)
    $fencedRow = @($fenced.management.files | Where-Object { $_.path -eq '~$include-local.xlsx' })
    Assert ($fencedRow.Count -eq 1 -and !$fencedRow[0].synchronized) 'deleted tracked file reported as synchronized'
    Assert ($fencedRow[0].release_reason -like '*下载*') 'missing recovery action not explained'
    $reuse=[Guid]::NewGuid().ToString('N')
    $null=Invoke-Cli @('download','--binding-id',$binding,'--entry-id',$seed.entry_id,'--operation-id',$reuse)
    $bad=& $cli --json download --binding-id $binding --entry-id $fencedRow[0].entry_id --operation-id $reuse
    Assert ($LASTEXITCODE -ne 0 -and !(Test-Path $deletePath)) 'reused operation restored a file before rejection'
    Write-Text (Join-Path $PSScriptRoot 'operation-reuse.json') ($bad -join "`n")
    $restoreOp=[Guid]::NewGuid().ToString('N')
    $null=Invoke-Cli @('download','--binding-id',$binding,'--entry-id',$fencedRow[0].entry_id,'--operation-id',$restoreOp)
    Assert ([IO.File]::ReadAllText($deletePath) -ceq 'S196-included-local-lock') 'explicit restore bytes differ'
    $null=Confirmed-File '~$include-local.xlsx'
    Set-Rules ''
    Remove-Item -LiteralPath $deletePath -Force
    Set-Rules $includeRules
    $bad=& $cli --json download --binding-id $binding --entry-id $fencedRow[0].entry_id --operation-id $restoreOp
    Assert ($LASTEXITCODE -ne 0 -and !(Test-Path $deletePath)) 'old completed operation silently restored a second deletion'
    $null=Invoke-Cli @('download','--binding-id',$binding,'--entry-id',$fencedRow[0].entry_id)
    Barrier 'reconciled'; Assert-Defaults
    Assert-Remote '~$reconcile.xlsx' 'S196-remote-newer-while-excluded'
    $null = Wait-Until 'full-mode completion with default-excluded remote files' {
      $row = @((Invoke-Cli @('status')).bindings | Where-Object { $_.binding_id -eq $binding })
      if ($row.Count -eq 1 -and $row[0].synchronized) { return $true }
    }
    Mark 'excluded-both-sides-modified'
    Set-Rules ''
    Barrier 'before-both-edits'
    $localEdit = 'S196-unique-local-edit-while-excluded'
    $remoteEdit = 'S196-concurrent-remote-edit-while-excluded'
    Write-Text (Join-Path $root '~$reconcile.xlsx') $localEdit
    Put-Remote '~$reconcile.xlsx' $remoteEdit
    Barrier 'both-edited'; Assert-Defaults
    Assert ((Hash (Join-Path $root '~$reconcile.xlsx')) -eq (Text-Hash $localEdit)) 'excluded remote edit lost unconfirmed local bytes'
    Assert-Remote '~$reconcile.xlsx' $remoteEdit

    Mark 'negation-preserves-both-edits'
    Set-Rules $includeRules
    $localEditHash = Text-Hash $localEdit; $remoteEditHash = Text-Hash $remoteEdit
    $preservedRows = Wait-Until 'both exclusion-period edits survive re-inclusion' {
      $rows = @(Remote-Rows)
      if (@($rows | Where-Object { $_.hash -eq $localEditHash }).Count -gt 0 -and
          @($rows | Where-Object { $_.hash -eq $remoteEditHash }).Count -gt 0) { return $rows }
    }
    $null = Wait-Until 'unique local bytes remain available in the local sync root' {
      foreach ($file in Get-ChildItem -LiteralPath $root -File -Recurse -Force) {
        if ((Hash $file.FullName) -eq $localEditHash) { return $true }
      }
    }
    Write-Text (Join-Path $PSScriptRoot 'both-edits-preserved.json') ($preservedRows | ConvertTo-Json -Depth 8)
    Barrier 'both-preserved'; Assert-Defaults
    }
    if ($PartialOnly) { $null=Confirmed-File 'local.xlsx'; $null=Confirmed-File 'server.xlsx' }
    Mark 'partial-native-maintenance-and-reinclude'
    $null=Invoke-Cli @('mode','--binding-id',$binding,'--mode','on_demand')
    Set-Rules '!~$partial-large.bin'
    $large=Confirmed-File '~$partial-large.bin'; $largePath=Join-Path $root '~$partial-large.bin'
    [S199Ranges]::ReadRange($largePath)
    $info=Partial-Info $largePath
    Write-Text (Join-Path $PSScriptRoot 'partial-observation.json') ($info | ConvertTo-Json)
    Assert ($info[0] -gt 0 -and $info[0] -lt 16777216 -and $info[1] -eq 0) 'not a real clean partial placeholder'
    Set-Rules ''
    $state=Export-State (Join-Path $PSScriptRoot 'export-partial')
    Assert (@($state.current_observations | Where-Object { $_[1].state -eq 'remote_partial' -and $_[1].on_disk -gt 0 }).Count -eq 1) 'partial export omitted real residency'
    Prepare-Maintenance (Join-Path $PSScriptRoot 'prepare-partial')
    $agent=Start-Agent 'partial-restarted';$null=Wait-Agent $agent
    $null=Invoke-Cli @('resume','--binding-id',$binding)
    $info2=Partial-Info $largePath;Assert ($info2[0] -eq $info[0]) 'maintenance changed partial residency'
    Set-Rules '!~$partial-large.bin'
    $null=Wait-Until 'same-version partial participation resumes' {
      $file=@((Invoke-Cli @('files','--binding-id',$binding)).management.files | Where-Object { $_.entry_id -eq $large.entry_id })
      if($file.Count -eq 1 -and $file[0].residency -eq 'partial' -and $file[0].release_reason -eq '本地不是完整已下载内容') { return $true }
    }
    $null=Invoke-Cli @('download','--binding-id',$binding,'--entry-id',$large.entry_id)
    Assert ((Hash $largePath) -eq (Text-Hash ('p' * 16777216))) 'same-version partial download hash'
    $large=Confirmed-File '~$partial-large.bin'
    $null=Invoke-Cli @('release','--binding-id',$binding,'--entry-id',$large.entry_id)
    [S199Ranges]::ReadRange($largePath);Set-Rules ''
    Put-Remote '~$partial-large.bin' 'S199-remote-new-version'
    Barrier 'partial-remote-edited'
    Set-Rules '!~$partial-large.bin'
    $null=Invoke-Cli @('download','--binding-id',$binding,'--entry-id',$large.entry_id)
    $null=Wait-Until 'changed remote version reconciles after original partial completes' { if((Get-Item $largePath).Length -eq 23 -and [IO.File]::ReadAllText($largePath) -ceq 'S199-remote-new-version') { return $true } }
    $null=Confirmed-File '~$partial-large.bin'
    # A real partial file whose native in-sync evidence is withdrawn is unconfirmed,
    # even if no write ranges have yet been reported. Keep an actual unique excluded
    # file alongside it, to prove refusal never removes unconfirmed local bytes.
    Put-Remote '~$partial-large.bin' ('p' * 16777216)
    $null=Wait-Until 'large remote restored' { if((Get-Item $largePath).Length -eq 16777216) { return $true } }
    $large=Confirmed-File '~$partial-large.bin';$null=Invoke-Cli @('release','--binding-id',$binding,'--entry-id',$large.entry_id)
    [S199Ranges]::ReadRange($largePath);Set-Rules ''
    $unconfirmed=Join-Path $root '~$unconfirmed-local.xlsx'
    Write-Text $unconfirmed 'S199-unique-unconfirmed-local-bytes'
    $uniqueHash=Hash $unconfirmed
    [S199Ranges]::MarkUnconfirmed($largePath)
    $unconfirmedInfo=Partial-Info $largePath
    Assert ($unconfirmedInfo[0] -gt 0 -and $unconfirmedInfo[0] -lt 16777216 -and $unconfirmedInfo[2] -eq 0) 'not a real native unconfirmed partial'
    Write-Text (Join-Path $PSScriptRoot 'unconfirmed-partial-observation.json') ($unconfirmedInfo|ConvertTo-Json)
    $null=Invoke-Cli @('pause','--binding-id',$binding)
    $bad=& $cli --json export-recovery --binding-id $binding --destination (Join-Path $PSScriptRoot 'export-unconfirmed-partial')
    Assert ($LASTEXITCODE -ne 0 -and !(Test-Path (Join-Path $PSScriptRoot 'export-unconfirmed-partial/manifest.json'))) 'unconfirmed partial exported as success'
    Write-Text (Join-Path $PSScriptRoot 'unconfirmed-partial-refusal.json') ($bad -join "`n")
    $null=Invoke-Cli @('resume','--binding-id',$binding);Set-Rules '!~$partial-large.bin'
    $bad=& $cli --json download --binding-id $binding --entry-id $large.entry_id
    Assert ($LASTEXITCODE -ne 0) 'unconfirmed partial was overwritten by explicit recovery'
    Assert ((Hash $unconfirmed) -eq $uniqueHash -and (Partial-Info $largePath)[0] -eq $unconfirmedInfo[0]) 'refusal changed unique local bytes or partial residency'
    # Never manufacture an in-sync acknowledgement. Explicitly read the original
    # immutable version through CFAPI, preserving existing ranges, then prove all bytes.
    $beforeRestore=Partial-Info $largePath
    Assert ($beforeRestore[1] -eq 0 -and $beforeRestore[0] -eq $unconfirmedInfo[0]) 'fixture may not replace changed bytes'
    $read=[IO.File]::ReadAllBytes($largePath)
    Assert ($read.Length -eq 16777216 -and (Hash $largePath) -eq (Text-Hash ('p'*16777216))) 'explicit original version read differs'
    $null=Invoke-Cli @('download','--binding-id',$binding,'--entry-id',$large.entry_id)
    Assert ((Hash $largePath) -eq (Text-Hash ('p'*16777216))) 'cleanup fixed version differs'
    Assert ((Hash $unconfirmed) -eq $uniqueHash) 'cleanup lost unique local bytes'
    Set-Rules ''
    $unconfirmedPartialRefused=$true
    # Teardown needs complete fixtures before stopping their provider; no cloud-only
    # files may be hashed or unregistered offline as if they were complete.
    $remaining=(Invoke-Cli @('files','--binding-id',$binding)).management.files
    foreach($file in $remaining) {
      if($file.entry_id -and !$file.dirty -and $file.residency -in @('metadata_only','partial')) {
        $null=Invoke-Cli @('download','--binding-id',$binding,'--entry-id',$file.entry_id)
      }
    }
    $null = Invoke-Cli @('logout','--server-id',$fixture.serverId)
    Stop-Agent $agent; $agent = $null
    $regression = !$PartialOnly
    @{ok=$true;binding=$binding;partialOnly=[bool]$PartialOnly;defaults=$regression;additional=$regression;cleared=$regression;negation=$regression;
      restart=$true;reconciliation=$true;retained=$true;cfapi=$true;fullMode=$regression;
      initialRemoteOnly=$regression;deletedTrackedStaysFenced=$regression;bothEditsPreserved=$regression;
      maintenanceMissing=$regression;explicitRestore=$regression;partialSameVersion=$true;partialChangedVersion=$true;
      partialMaintenance=$true;unconfirmedPartialRefused=$unconfirmedPartialRefused;operationReuseRefused=$regression;
      windowsVersion=[Environment]::OSVersion.VersionString} | ConvertTo-Json -Depth 8 |
      Set-Content -LiteralPath (Join-Path $PSScriptRoot 'result.json') -Encoding UTF8
    $workerExit = 0
  } catch {
    $_ | Out-String | Set-Content -LiteralPath (Join-Path $PSScriptRoot 'error.txt')
    if ($agent -and !$agent.HasExited) {
      foreach ($command in @('status','bindings','problems')) {
        try { $diagnostic = & $cli --json $command; Write-Text (Join-Path $PSScriptRoot "failure-$command.json") ($diagnostic -join "`n") }
        catch { $_ | Out-String | Add-Content -LiteralPath (Join-Path $PSScriptRoot 'diagnostics.log') }
      }
      if ($ownedBindings.Count -gt 0) {
        try { $diagnostic = & $cli --json files --binding-id $ownedBindings[-1]; Write-Text (Join-Path $PSScriptRoot 'failure-files.json') ($diagnostic -join "`n") }
        catch { $_ | Out-String | Add-Content -LiteralPath (Join-Path $PSScriptRoot 'diagnostics.log') }
      }
    }
  } finally {
    $cleanupErrors = Join-Path $PSScriptRoot 'cleanup-errors.log'
    if ($agent -and !$agent.HasExited) {
      try { Stop-Agent $agent }
      catch {
        $_ | Out-String | Add-Content -LiteralPath $cleanupErrors
        $workerExit = 1
        if (!$agent.HasExited) { Stop-Process -Id $agent.Id }
      }
    }
    try {
      if ($root -and (Test-Path -LiteralPath $root)) {
        # Same conservative native teardown as S195: stop our writer, refuse partial
        # placeholders, preserve every file, and unregister only this run's identities.
        Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.ComponentModel;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class S199Cleanup {
  [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
  static extern SafeFileHandle CreateFileW(string p,uint a,uint s,IntPtr x,uint d,uint f,IntPtr t);
  [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetFileSizeEx(SafeFileHandle h,out long size);
  [DllImport("cldapi.dll")] static extern int CfGetPlaceholderInfo(SafeFileHandle h,int c,byte[] b,uint n,out uint used);
  [DllImport("cldapi.dll")] static extern int CfRevertPlaceholder(SafeFileHandle h,uint flags,IntPtr ov);
  [DllImport("cldapi.dll", CharSet=CharSet.Unicode)] public static extern int CfUnregisterSyncRoot(string p);
  public static void Revert(string path,bool directory) {
    using (var handle=CreateFileW(path,0xC0000000,0,IntPtr.Zero,3,0x02200000,IntPtr.Zero)) {
      if(handle.IsInvalid) throw new Win32Exception(Marshal.GetLastWin32Error());
      byte[] info=new byte[8192]; uint used;
      int hr=CfGetPlaceholderInfo(handle,1,info,(uint)info.Length,out used);
      if(hr==unchecked((int)0x80070178)) {
        if((File.GetAttributes(path)&FileAttributes.ReparsePoint)!=0) throw new IOException("unknown reparse point");
        return;
      }
      if(hr<0) Marshal.ThrowExceptionForHR(hr);
      if(!directory) {
        long size;
        if(!GetFileSizeEx(handle,out size)) throw new Win32Exception(Marshal.GetLastWin32Error());
        if(used<64 || BitConverter.ToInt64(info,0)<size) throw new IOException("partial placeholder retained: "+path);
      }
      hr=CfRevertPlaceholder(handle,0,IntPtr.Zero);
      if(hr<0) Marshal.ThrowExceptionForHR(hr);
    }
  }
}
'@
        $canonical = [IO.Path]::GetFullPath($root)
        Assert ($canonical -eq (Join-Path $PSScriptRoot 'data\root') -and (Split-Path $PSScriptRoot -Leaf) -cmatch '^S199-[0-9]+-[0-9]+$') 'refuse cleanup outside exact run root'
        $preserved = @{}
        foreach ($file in Get-ChildItem -LiteralPath $root -File -Recurse -Force) { $preserved[$file.FullName] = Hash $file.FullName }
        $entries = @(Get-ChildItem -LiteralPath $root -Recurse -Force | Sort-Object { $_.FullName.Length } -Descending)
        foreach ($item in $entries) { [S199Cleanup]::Revert($item.FullName, $item.PSIsContainer) }
        foreach ($path in $preserved.Keys) { Assert ((Hash $path) -eq $preserved[$path]) "cleanup changed bytes: $path" }
        $hr = [S199Cleanup]::CfUnregisterSyncRoot($root)
        Assert ($hr -ge 0 -or $hr -eq [int]-2147024506) ('unregister HRESULT '+$hr)
        $null = [Windows.Storage.Provider.StorageProviderSyncRootManager,Windows.Storage.Provider,ContentType=WindowsRuntime]
        $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
        foreach ($owned in $ownedBindings) {
          $registration = "vaultsyncd!$sid!$owned"
          $matches = @([Windows.Storage.Provider.StorageProviderSyncRootManager]::GetCurrentSyncRoots() | Where-Object { $_.Id -eq $registration })
          if ($matches.Count -gt 0) { [Windows.Storage.Provider.StorageProviderSyncRootManager]::Unregister($registration) }
        }
        Write-Text (Join-Path $PSScriptRoot 'cleanup.json') (@{preserved=$preserved;bindings=$ownedBindings} | ConvertTo-Json -Depth 8)
      }
    } catch {
      $_ | Out-String | Add-Content -LiteralPath $cleanupErrors
      $workerExit = 1
    }
    if ($fixture -and $fixture.serverId -cmatch '^[0-9a-f]{32}$') { $null = & cmdkey.exe "/delete:vaultsyncd/session/$($fixture.serverId)" }
    "$workerExit" | Set-Content -LiteralPath (Join-Path $PSScriptRoot 'exit.txt')
  }
  exit $workerExit
}

$hashes = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'hashes.json') -Raw | ConvertFrom-Json
foreach ($item in $hashes.PSObject.Properties) {
  Assert ((Hash (Join-Path $PSScriptRoot $item.Name)) -eq $item.Value) "transport hash mismatch: $($item.Name)"
}
$task = 'vaultsyncd-s199-'+[Guid]::NewGuid().ToString('N')
$action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Worker $(if($PartialOnly){'-PartialOnly'})"
$principal = New-ScheduledTaskPrincipal -UserId ([Security.Principal.WindowsIdentity]::GetCurrent().Name) -LogonType Interactive -RunLevel Limited
try {
  Register-ScheduledTask -TaskName $task -Action $action -Principal $principal | Out-Null
  Start-ScheduledTask -TaskName $task
  $null = Wait-Until 'interactive S199 acceptance' { Test-Path (Join-Path $PSScriptRoot 'exit.txt') } 900
  Get-Content -LiteralPath (Join-Path $PSScriptRoot 'progress.log') -ErrorAction SilentlyContinue
  $workerExit = [int](Get-Content -LiteralPath (Join-Path $PSScriptRoot 'exit.txt'))
  if ($workerExit -ne 0) {
    Get-Content -LiteralPath (Join-Path $PSScriptRoot 'error.txt') -ErrorAction SilentlyContinue
    Get-Content -LiteralPath (Join-Path $PSScriptRoot 'cleanup-errors.log') -ErrorAction SilentlyContinue
    exit $workerExit
  }
  $result = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'result.json') -Raw -Encoding UTF8 | ConvertFrom-Json
  Assert ($result.ok) 'worker did not produce a passing receipt'
  $result | ConvertTo-Json -Depth 8
} finally {
  Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
}
