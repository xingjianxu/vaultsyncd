# 在 Windows 客体上核对交叉编译产物的 SHA-256 并原生执行（S009）。
# 由 run-on-guest.sh 经 scp + ssh 调用；只读客体状态，不安装任何工具链。
# 输出只用 ASCII：客体控制台代码页实测为 437，非 ASCII 会转写出乱码。
# 退出码：0 表示全部二进制返回 0；1 表示任一哈希/执行失败。

param(
    [Parameter(Mandatory = $true)][string]$Directory,
    # 逗号分隔的二进制名：`-File` 调用方式不会把多个位置参数绑成数组，因此在脚本内拆分。
    [Parameter(Mandatory = $true)][string]$Executables
)

$ErrorActionPreference = 'Stop'
$failed = 0
$executableList = $Executables.Split(',')

$os = Get-CimInstance -ClassName Win32_OperatingSystem
Write-Output "guest_os: $($os.Caption)"
Write-Output "guest_version: $($os.Version)"
Write-Output "guest_build: $($os.BuildNumber)"
Write-Output "guest_architecture: $env:PROCESSOR_ARCHITECTURE"
Write-Output "guest_powershell: $($PSVersionTable.PSVersion)"

foreach ($exe in $executableList) {
    $path = Join-Path -Path $Directory -ChildPath $exe
    $hash = (Get-FileHash -Algorithm SHA256 -Path $path).Hash
    Write-Output "guest_sha256 $exe $hash"
}

foreach ($exe in $executableList) {
    $path = Join-Path -Path $Directory -ChildPath $exe
    Write-Output "=== run $exe"
    & $path
    $code = $LASTEXITCODE
    Write-Output "guest_exit_code $exe $code"
    if ($code -ne 0) {
        $failed = 1
    }
}

if ($failed -ne 0) {
    Write-Output 'guest_result: FAILED'
}
else {
    Write-Output 'guest_result: OK'
}
exit $failed
