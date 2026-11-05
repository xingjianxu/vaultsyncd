# vaultsyncd Windows 实验室客体首登脚本。
# 由 patched autounattend.xml 的 oobeSystem FirstLogonCommands 以管理员身份调用一次：
#   powershell.exe -NoProfile -ExecutionPolicy Bypass -File <介质>:\provision.ps1
# 安装期间 quickemu 挂载 unattended.iso（本脚本/公钥/SPICE msi）与 virtio-win.iso（驱动/agent），
# 盘符由 Windows 分配，且安装完成后两张盘都不再挂载；脚本按内容识别盘符，缺介质时跳过对应步骤。
# 输出落在 C:\vaultsyncd-lab\provision.log（Start-Transcript），安装结束后由宿主侧 SSH 复核。
#
# 约束：本脚本不改系统语言，不关 Defender，不关防火墙整体；只新增 22/5985 入站规则。

$ErrorActionPreference = 'Continue'
$LabRoot = 'C:\vaultsyncd-lab'
New-Item -ItemType Directory -Force -Path $LabRoot | Out-Null
Start-Transcript -Path (Join-Path $LabRoot 'provision.log') -Append -Force | Out-Null

function Step {
    param([string]$Text)
    Write-Host ('[{0}] {1}' -f (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ'), $Text)
}

function Try-Step {
    param([string]$Text, [scriptblock]$Action)
    Step ('BEGIN ' + $Text)
    try { & $Action } catch { Step ('FAILED ' + $Text + ': ' + $_.Exception.Message) }
    Step ('END ' + $Text)
}

# 盘符不稳定，且安装完成后 quickemu 不再挂载安装介质（"disk image appears to be used" 时
# 不引导安装介质，也就不会挂 unattended.iso / virtio-win.iso）。因此按内容识别 CD-ROM，
# 找不到就跳过依赖介质的步骤，让脚本在首登与安装后重跑两种情况下都能收尾。
function Find-CdromWithFile {
    param([string]$RelativePath)
    foreach ($disk in (Get-CimInstance -ClassName Win32_LogicalDisk -Filter 'DriveType=5' | ForEach-Object { $_.DeviceID })) {
        if (Test-Path (Join-Path ($disk + '\') $RelativePath)) { return $disk }
    }
    return $null
}

Step 'provision start'

$UnattendRoot = Find-CdromWithFile 'ssh_authorized_key.pub'
$VirtioRoot = Find-CdromWithFile 'guest-agent\qemu-ga-x86_64.msi'
$UnattendLabel = if ($UnattendRoot) { $UnattendRoot } else { '<absent>' }
$VirtioLabel = if ($VirtioRoot) { $VirtioRoot } else { '<absent>' }
Step ('media: unattended=' + $UnattendLabel + ' virtio=' + $VirtioLabel)

# ---- 1. 记录安装后的系统事实（时区/编码/卷/版本按实测记录，不做猜测性修改）----
Try-Step 'record baseline facts' {
    $os = Get-CimInstance -ClassName Win32_OperatingSystem
    Step ('OS: ' + $os.Caption + ' | Version ' + $os.Version + ' | Build ' + $os.BuildNumber + ' | Arch ' + $os.OSArchitecture)
    Step ('ComputerName: ' + $env:COMPUTERNAME)
    Step ('TimeZone(before): ' + (Get-TimeZone).Id + ' / ' + (Get-TimeZone).DisplayName)
    Step ('Culture: ' + (Get-Culture).Name + ' | UI culture: ' + (Get-UICulture).Name + ' | WinSystemLocale: ' + (Get-WinSystemLocale).Name)
    Step ('Active codepage: ' + [Console]::OutputEncoding.CodePage)
}

# 时区与开发主机一致（Asia/Shanghai），便于宿主与客体日志按时间对齐。
Try-Step 'set time zone to China Standard Time' {
    Set-TimeZone -Id 'China Standard Time'
    Step ('TimeZone(after): ' + (Get-TimeZone).Id)
}

# ---- 2. 长路径 ----
Try-Step 'enable LongPathsEnabled' {
    New-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem' -Name 'LongPathsEnabled' -PropertyType DWord -Value 1 -Force | Out-Null
    Step ('LongPathsEnabled: ' + (Get-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem' -Name 'LongPathsEnabled').LongPathsEnabled)
}

# ---- 2b. 实验室收敛：关闭 Delivery Optimization 的 P2P 上传与局域网发现 ----
# 默认策略会让客体与内网/外网其他 Windows 机器建立 7680 连接；实验室只需要 HTTP 下载。
Try-Step 'disable Delivery Optimization peer-to-peer' {
    $do = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\DeliveryOptimization'
    if (-not (Test-Path $do)) { New-Item -Path $do -Force | Out-Null }
    New-ItemProperty -Path $do -Name 'DODownloadMode' -PropertyType DWord -Value 0 -Force | Out-Null
    Step ('DODownloadMode: ' + (Get-ItemProperty -Path $do -Name 'DODownloadMode').DODownloadMode)
}

# ---- 3. 电源：关闭休眠/待机/快速启动，避免无人值守时客体挂起 ----
Try-Step 'disable hibernation, sleep and fast startup' {
    powercfg.exe /hibernate off | Out-Null
    powercfg.exe /setacvalueindex SCHEME_CURRENT SUB_SLEEP STANDBYIDLE 0 | Out-Null
    powercfg.exe /setacvalueindex SCHEME_CURRENT SUB_SLEEP HIBERNATEIDLE 0 | Out-Null
    powercfg.exe /setactive SCHEME_CURRENT | Out-Null
    New-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Power' -Name 'HiberbootEnabled' -PropertyType DWord -Value 0 -Force | Out-Null
    $scheme = (powercfg.exe /getactivescheme) -join ' '
    Step ('Active power scheme: ' + $scheme)
}

# ---- 4. OpenSSH Server：能力（可选功能）优先，失败则回退官方 Win32-OpenSSH MSI ----
Try-Step 'install OpenSSH Server' {
    $cap = Get-WindowsCapability -Online -Name 'OpenSSH.Server*' | Select-Object -First 1
    Step ('OpenSSH.Server capability: ' + $cap.Name + ' = ' + $cap.State)
    if ($cap.State -ne 'Installed') {
        Add-WindowsCapability -Online -Name $cap.Name | Out-Null
    }
}

$sshdExe = @(
    "$env:SystemRoot\System32\OpenSSH\sshd.exe",
    "$env:ProgramFiles\OpenSSH\sshd.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1

Try-Step 'fallback Win32-OpenSSH MSI when capability payload missing' {
    if ($sshdExe) {
        Step ('sshd binary already present: ' + $sshdExe)
        return
    }
    $rel = Invoke-RestMethod -Uri 'https://api.github.com/repos/PowerShell/Win32-OpenSSH/releases/latest' -UseBasicParsing
    $asset = $rel.assets | Where-Object { $_.name -like 'OpenSSH-Win64-*.msi' } | Select-Object -First 1
    $msi = Join-Path $env:TEMP $asset.name
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $msi -UseBasicParsing
    Start-Process -FilePath 'msiexec.exe' -ArgumentList "/i `"$msi`" /qn /norestart ADDLOCAL=Server" -Wait
    $script:sshdExe = @(
        "$env:SystemRoot\System32\OpenSSH\sshd.exe",
        "$env:ProgramFiles\OpenSSH\sshd.exe"
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
    Step ('sshd binary after fallback: ' + $script:sshdExe)
}

Try-Step 'enable and start sshd service' {
    if (Get-Service -Name sshd -ErrorAction SilentlyContinue) {
        Set-Service -Name sshd -StartupType Automatic
    } else {
        New-Service -Name sshd -BinaryPathName ('"' + $sshdExe + '"') -StartupType Automatic | Out-Null
    }
    Start-Service -Name sshd
    Step ('sshd service: ' + (Get-Service -Name sshd).Status + ' / ' + (Get-Service -Name sshd).StartType)
}

# 只保留公钥认证：sshd 默认配置同时开启 password 与 keyboard-interactive，实验室只允许密钥登录。
# 注意指令必须插在 `Match Group administrators` 之前，否则会被算进该 Match 块。
Try-Step 'restrict sshd to publickey authentication' {
    $cfg = "$env:ProgramData\ssh\sshd_config"
    if (-not (Test-Path $cfg)) { throw "$cfg 不存在，sshd 未正确安装" }
    $lines = @(Get-Content -Path $cfg)
    $wanted = @(
        @{ Name = 'PubkeyAuthentication'; Value = 'yes' },
        @{ Name = 'PasswordAuthentication'; Value = 'no' },
        @{ Name = 'KbdInteractiveAuthentication'; Value = 'no' }
    )
    foreach ($item in $wanted) {
        $pattern = '^\s*#?\s*' + $item.Name + '\s+.*$'
        $idx = -1
        for ($i = 0; $i -lt $lines.Count; $i++) { if ($lines[$i] -match $pattern) { $idx = $i; break } }
        if ($idx -ge 0) {
            $lines[$idx] = $item.Name + ' ' + $item.Value
        } else {
            $matchIdx = -1
            for ($i = 0; $i -lt $lines.Count; $i++) { if ($lines[$i] -match '^Match ') { $matchIdx = $i; break } }
            $entry = $item.Name + ' ' + $item.Value
            if ($matchIdx -ge 0) {
                $lines = @($lines[0..($matchIdx - 1)]) + @($entry) + @($lines[$matchIdx..($lines.Count - 1)])
            } else {
                $lines += $entry
            }
        }
    }
    Set-Content -Path $cfg -Value $lines -Encoding ascii
    Restart-Service -Name sshd
    Step ('sshd auth settings: ' + ((Select-String -Path $cfg -Pattern '^(PubkeyAuthentication|PasswordAuthentication|KbdInteractiveAuthentication)' | ForEach-Object { $_.Line }) -join '; '))
}

# 管理员账号的 authorized_keys 必须是 ProgramData\ssh\administrators_authorized_keys，
# 并且只允许 Administrators 与 SYSTEM 访问（否则 sshd 拒绝该文件）。
Try-Step 'install administrators_authorized_keys' {
    if (-not $UnattendRoot) { Step 'SKIP: unattended media not attached (expected after installation)'; return }
    $keysFile = Join-Path ($UnattendRoot + '\') 'ssh_authorized_key.pub'
    $target = Join-Path $env:ProgramData 'ssh\administrators_authorized_keys'
    New-Item -ItemType Directory -Force -Path (Split-Path $target) | Out-Null
    Set-Content -Path $target -Value ((Get-Content -Path $keysFile -Raw).Trim()) -Encoding ascii
    icacls.exe $target /inheritance:r /grant '*S-1-5-32-544:F' /grant '*S-1-5-18:F' | Out-Null
    Step ('authorized_keys installed: ' + $target + ' (' + (Get-Content $target | Measure-Object -Line).Lines + ' line(s))')
}

# ---- 5. 入站规则：22（SSH）。防火墙整体保持启用 ----
# 安装 OpenSSH.Server 能力时自带的规则只覆盖 Private profile，而 guest 的唯一网卡
# 被 Windows 判为 Public；不改成 Any 时宿主到 2222 的 SYN 得不到响应（实测）。
Try-Step 'firewall inbound rule for tcp/22 (any profile)' {
    if (Get-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -ErrorAction SilentlyContinue) {
        Set-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -Profile Any -Enabled True
    } else {
        New-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -DisplayName 'OpenSSH Server (sshd)' -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 -Profile Any | Out-Null
    }
    $rule = Get-NetFirewallRule -Name 'OpenSSH-Server-In-TCP'
    $profile = (Get-NetConnectionProfile | Select-Object -First 1).NetworkCategory
    Step ('Firewall rule OpenSSH-Server-In-TCP: profile=' + $rule.Profile + ' enabled=' + $rule.Enabled + ' nic-category=' + $profile)
}

# ---- 6. 备选通道：WinRM（HTTP 5985，仅实验网络内使用）----
Try-Step 'enable WinRM (HTTP listener 5985)' {
    Enable-PSRemoting -SkipNetworkProfileCheck -Force | Out-Null
    Set-Service -Name WinRM -StartupType Automatic
    Start-Service -Name WinRM
    Step ('WinRM service: ' + (Get-Service -Name WinRM).Status)
    Step ('WinRM listening on 5985: ' + [bool](Get-NetTCPConnection -State Listen -LocalPort 5985 -ErrorAction SilentlyContinue))
}

# ---- 7. VirtIO 附加组件（失败只记录，不阻塞 SSH 通道）----
Try-Step 'install QEMU guest agent from virtio-win ISO' {
    if (-not $VirtioRoot) { Step 'SKIP: virtio-win media not attached (expected after installation)'; return }
    $gaMsi = Join-Path ($VirtioRoot + '\') 'guest-agent\qemu-ga-x86_64.msi'
    Start-Process -FilePath 'msiexec.exe' -ArgumentList ('/i "' + $gaMsi + '" /qn /norestart') -Wait
    Step ('QEMU guest agent service: ' + (Get-Service -Name 'QEMU-GA' -ErrorAction SilentlyContinue).Status)
}

Try-Step 'install virtio gpu driver (viogpudo)' {
    if (-not $VirtioRoot) { Step 'SKIP: virtio-win media not attached (expected after installation)'; return }
    $gpuInf = Join-Path ($VirtioRoot + '\') 'viogpudo\w10\amd64\viogpudo.inf'
    if (-not (Test-Path $gpuInf)) { Step ('SKIP: ' + $gpuInf + ' not present'); return }
    pnputil.exe /add-driver $gpuInf /install | Out-Null
}

foreach ($msiName in @('spice-vdagent-x64-0.10.0.msi', 'spice-webdavd-x64-latest.msi')) {
    Try-Step ('install ' + $msiName) {
        if (-not $UnattendRoot) { Step 'SKIP: unattended media not attached (expected after installation)'; return }
        $path = Join-Path ($UnattendRoot + '\') $msiName
        if (Test-Path $path) {
            Start-Process -FilePath 'msiexec.exe' -ArgumentList ('/i "' + $path + '" /qn /norestart') -Wait
        } else {
            Step ('SKIP: ' + $path + ' not present')
        }
    }
}

# ---- 8. 收尾：把验收要用的原始输出写进日志 ----
Try-Step 'record disk and volume facts' {
    Get-Volume -DriveLetter C | Format-List FileSystemType, DriveType, Size, SizeRemaining | Out-String | Write-Host
    fsutil.exe fsinfo volumeinfo C: | Out-String | Write-Host
}

Step 'provision end'
Stop-Transcript | Out-Null
