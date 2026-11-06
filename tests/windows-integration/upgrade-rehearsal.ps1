param([Parameter(Mandatory=$true)][string]$InputHash)
$ErrorActionPreference='Stop'
$inputFile=Join-Path $PSScriptRoot 'native-input.json'
if ((Get-FileHash $inputFile -Algorithm SHA256).Hash.ToLowerInvariant() -ne $InputHash) {throw 'S091 input manifest transport mismatch'}
$manifest=Get-Content -Raw $inputFile | ConvertFrom-Json
if ([Environment]::OSVersion.Platform -ne 'Win32NT') {throw 'requires actual Windows'}
if ((Get-Volume -DriveLetter C).FileSystem -ne 'NTFS') {throw 'requires native NTFS'}
$env:VAULTSYNCD_S091_RUN_ROOT=Join-Path $PSScriptRoot 'cases'
New-Item -ItemType Directory -Path $env:VAULTSYNCD_S091_RUN_ROOT | Out-Null
$results=@()
try {
    foreach ($case in $manifest.cases) {
        $exe=Join-Path $PSScriptRoot $case.file
        if ((Get-FileHash $exe -Algorithm SHA256).Hash.ToLowerInvariant() -ne $case.sha256) {throw ('S091 executable transport mismatch: '+$case.file)}
        $ErrorActionPreference='Continue'
        $lines=& $exe $case.filter --exact --test-threads=1 --nocapture 2>&1
        $status=$LASTEXITCODE
        $ErrorActionPreference='Stop'
        $text=$lines -join "`n"
        Write-Output $text
        [IO.File]::WriteAllText((Join-Path $PSScriptRoot ($case.file+'.log')),$text,(New-Object Text.UTF8Encoding($false)))
        if ($status -ne 0 -or $text -notmatch '1 passed; 0 failed' -or $text -notmatch $case.marker) {throw ('S091 native test did not actually pass: '+$case.file)}
        $results+=@{file=$case.file;sha256=$case.sha256;filter=$case.filter;passed=1;failed=0}
    }
    if ((Get-ChildItem -LiteralPath $env:VAULTSYNCD_S091_RUN_ROOT -Force | Measure-Object).Count -ne 0) {throw 'S091 test fixture cleanup incomplete'}
    $report=@{run_id=$manifest.run_id;source=$manifest.source;windows=[Environment]::OSVersion.VersionString;filesystem='NTFS';formal_previous_release=$false;agent_processes=0;cfapi_registrations=0;cases=$results}
    [IO.File]::WriteAllText((Join-Path $PSScriptRoot 'native-result.json'),($report|ConvertTo-Json -Depth 8),(New-Object Text.UTF8Encoding($false)))
} finally {
    Remove-Item Env:VAULTSYNCD_S091_RUN_ROOT -ErrorAction SilentlyContinue
}
