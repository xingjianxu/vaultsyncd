param([string]$ExpectedHash,[string]$FixtureHash)
$ErrorActionPreference='Stop'
$exe=Join-Path $PSScriptRoot 's069-native-tests.exe'
$fixture=Join-Path $PSScriptRoot 'guest-fixture.json'
if ((Get-FileHash $exe -Algorithm SHA256).Hash.ToLowerInvariant() -ne $ExpectedHash) { throw 'capacity exe transport mismatch' }
if ((Get-FileHash $fixture -Algorithm SHA256).Hash.ToLowerInvariant() -ne $FixtureHash) { throw 'capacity fixture transport mismatch' }
$disk=Join-Path $PSScriptRoot 's105-isolated-capacity.vhd'
if (Test-Path $disk) {throw 'refuse existing disk'}
$letter=@('Z','Y','X','W','V','U','T') | Where-Object { !(Test-Path ($_.ToString()+':\')) } | Select-Object -First 1
if (!$letter) {throw 'no unused drive letter'}
$attached=$false
$result=1
try {
  @("create vdisk file=`"$disk`" maximum=128 type=expandable","select vdisk file=`"$disk`"",'attach vdisk','create partition primary','format fs=ntfs quick label=VS_S105',"assign letter=$letter") | Set-Content (Join-Path $PSScriptRoot 'create-capacity.txt') -Encoding ASCII
  & diskpart.exe /s (Join-Path $PSScriptRoot 'create-capacity.txt') | Set-Content (Join-Path $PSScriptRoot 'diskpart-create.log')
  $attached=(Get-DiskImage -ImagePath $disk).Attached
  if (!$attached) {throw 'owned VHD was not attached'}
  $volume=Get-Volume -DriveLetter $letter
  if ($volume.FileSystemType -ne 'NTFS' -or $volume.FileSystemLabel -ne 'VS_S105' -or $volume.Size -gt 128MB) {throw 'unexpected capacity test volume'}
  # A newly attached VHD may not yet have the Cloud Files minifilter instance.
  & fltmc.exe attach CldFlt ($letter+':') | Set-Content (Join-Path $PSScriptRoot 'cloudfilter-attach.log')
  $env:VAULTSYNCD_RELEASE_FULL_VOLUME=$letter+':\'
  $env:VAULTSYNCD_RELEASE_FIXTURE=$fixture
  & $exe native_ntfs_exhaustion_keeps_downloaded_bytes_and_reports_no_space --exact --ignored --nocapture --test-threads=1
  $result=$LASTEXITCODE
} finally {
  Remove-Item Env:\VAULTSYNCD_RELEASE_FULL_VOLUME -ErrorAction SilentlyContinue
  Remove-Item Env:\VAULTSYNCD_RELEASE_FIXTURE -ErrorAction SilentlyContinue
  if (Test-Path $disk) {
    # Only this create-new image can be detached or deleted; never select another disk number.
    if ((Get-DiskImage -ImagePath $disk).Attached) {
      @("select vdisk file=`"$disk`"",'detach vdisk') | Set-Content (Join-Path $PSScriptRoot 'detach-capacity.txt') -Encoding ASCII
      & diskpart.exe /s (Join-Path $PSScriptRoot 'detach-capacity.txt') | Set-Content (Join-Path $PSScriptRoot 'diskpart-detach.log')
    }
    if ((Get-DiskImage -ImagePath $disk).Attached) {throw 'owned capacity VHD still attached; preserve evidence'}
    Remove-Item -LiteralPath $disk -Force
  }
  Remove-Item -LiteralPath $fixture -Force -ErrorAction SilentlyContinue
}
Write-Output 'check=pass s105_owned_ntfs_vhd_detached_and_deleted'
exit $result
