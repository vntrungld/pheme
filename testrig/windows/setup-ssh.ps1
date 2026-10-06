# Runs once, as SYSTEM, in Setup's specialize pass, from the seed ISO.
# Everything the rig needs to reach this machine, plus the marker the runner
# polls for.
$ErrorActionPreference = 'Stop'
Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0
Set-Service sshd -StartupType Automatic
# The first start writes sshd_config from its default, which is then edited.
Start-Service sshd
New-NetFirewallRule -Name sshd -DisplayName 'OpenSSH Server' `
    -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22

# The runner speaks PowerShell to this machine; OpenSSH's default is cmd.exe.
New-Item -Path HKLM:\SOFTWARE\OpenSSH -Force | Out-Null
New-ItemProperty -Path HKLM:\SOFTWARE\OpenSSH -Name DefaultShell -Force `
    -Value "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" | Out-Null

# The rig's key lives in a machine-wide file rather than in rig's profile,
# because the profile does not exist yet: rig is created in the oobeSystem
# pass, after this one. sshd refuses a key file anyone but SYSTEM and
# Administrators can write, so inheritance is cut.
$keys = "$env:ProgramData\ssh\rig_authorized_keys"
Copy-Item (Join-Path $PSScriptRoot 'id_rig.pub') $keys
icacls $keys /inheritance:r /grant 'SYSTEM:F' /grant '*S-1-5-32-544:F' | Out-Null
$conf = "$env:ProgramData\ssh\sshd_config"
(Get-Content $conf) -replace '^#?AuthorizedKeysFile.*',
    'AuthorizedKeysFile .ssh/authorized_keys __PROGRAMDATA__/ssh/rig_authorized_keys' |
    Set-Content -Encoding ascii $conf
Restart-Service sshd

# Written last, so its presence means every line above succeeded.
New-Item -ItemType File -Force -Path "$env:PUBLIC\pheme-rig-ready" | Out-Null
