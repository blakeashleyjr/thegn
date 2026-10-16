# First-logon provisioning for the opt-in Windows test VM (scripts/windows-vm.sh).
#
# `windows-vm.sh setup` copies this file and the VM's ssh public key onto the
# unattended-install ISO and adds a FirstLogonCommand that runs it, so a fresh
# guest ends up as a test-remote target with no clicks:
#   1. OpenSSH Server, started automatically, key-only for the admin user;
#   2. Visual Studio Build Tools (MSVC + Windows SDK: the C deps — bundled
#      sqlite, libgit2 — need cl.exe and link.exe);
#   3. Git for Windows (test-remote commits a snapshot so build.rs sees a HEAD);
#   4. rustup with the stable x86_64-pc-windows-msvc toolchain.
# Then it writes C:\thegn-provision.done, which `windows-vm.sh up` waits for.
#
# winget is preferred, but on a first logon of a fresh image App Installer is
# often not registered yet, so every install has a direct-download fallback
# from the vendor's own URL. Re-running is safe: each step checks first.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue' # Invoke-WebRequest is ~10x slower with the bar
$log = 'C:\thegn-provision.log'
Start-Transcript -Path $log -Append | Out-Null
$here = Split-Path -Parent $MyInvocation.MyCommand.Path

function Have-Winget { [bool](Get-Command winget -ErrorAction SilentlyContinue) }

function Winget-Install([string]$id, [string]$extra = '') {
  $wargs = @('install', '--id', $id, '-e', '--silent', '--accept-package-agreements', '--accept-source-agreements', '--disable-interactivity')
  if ($extra) { $wargs += @('--override', $extra) }
  & winget @wargs
  return ($LASTEXITCODE -eq 0)
}

function Download([string]$url, [string]$name) {
  $dest = Join-Path $env:TEMP $name
  Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $dest
  return $dest
}

# --- 1. OpenSSH Server ----------------------------------------------------------
if (-not (Get-Service sshd -ErrorAction SilentlyContinue)) {
  Add-WindowsCapability -Online -Name 'OpenSSH.Server~~~~0.0.1.0' | Out-Null
}
# The admin user's keys are read from administrators_authorized_keys, not from
# ~\.ssh, and sshd ignores the file unless only SYSTEM and Administrators can
# write it.
$keySrc = Join-Path $here 'thegn-winvm.pub'
$keyDst = 'C:\ProgramData\ssh\administrators_authorized_keys'
New-Item -ItemType Directory -Force -Path 'C:\ProgramData\ssh' | Out-Null
Copy-Item -Force $keySrc $keyDst
icacls.exe $keyDst /inheritance:r /grant 'Administrators:F' /grant 'SYSTEM:F' | Out-Null
Set-Service -Name sshd -StartupType Automatic
Start-Service sshd # first start generates host keys and the default sshd_config
# Key-only. quickemu forwards the guest's port 22 on every host interface, and
# the unattended install's admin password is the well-known `quickemu`, so a
# password-accepting sshd would be an admin login open to the LAN.
$cfg = 'C:\ProgramData\ssh\sshd_config'
$lines = Get-Content $cfg | Where-Object { $_ -notmatch '^\s*#?\s*(PasswordAuthentication|KbdInteractiveAuthentication)\b' }
Set-Content -Path $cfg -Value (@('PasswordAuthentication no', 'KbdInteractiveAuthentication no') + $lines)
Restart-Service sshd
if (-not (Get-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -ErrorAction SilentlyContinue)) {
  New-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -DisplayName 'OpenSSH Server (sshd)' -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 | Out-Null
}

# --- 2. Visual Studio Build Tools (MSVC) ---------------------------------------
$vsArgs = '--quiet --wait --norestart --nocache --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended'
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$haveVc = (Test-Path $vswhere) -and (& $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath)
if (-not $haveVc) {
  if (-not ((Have-Winget) -and (Winget-Install 'Microsoft.VisualStudio.2022.BuildTools' $vsArgs))) {
    $exe = Download 'https://aka.ms/vs/17/release/vs_BuildTools.exe' 'vs_BuildTools.exe'
    $p = Start-Process -FilePath $exe -ArgumentList $vsArgs -Wait -PassThru
    # 3010 = success, reboot required; the guest is rebooted by `up` anyway.
    if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) { throw "vs_BuildTools exited $($p.ExitCode)" }
  }
}

# --- 3. Git for Windows ---------------------------------------------------------
if (-not (Test-Path 'C:\Program Files\Git\cmd\git.exe')) {
  if (-not ((Have-Winget) -and (Winget-Install 'Git.Git'))) {
    $rel = Invoke-RestMethod -UseBasicParsing -Uri 'https://api.github.com/repos/git-for-windows/git/releases/latest'
    $asset = $rel.assets | Where-Object { $_.name -match '^Git-.*-64-bit\.exe$' } | Select-Object -First 1
    $exe = Download $asset.browser_download_url 'git-setup.exe'
    $p = Start-Process -FilePath $exe -ArgumentList '/VERYSILENT', '/NORESTART', '/SUPPRESSMSGBOXES' -Wait -PassThru
    if ($p.ExitCode -ne 0) { throw "git setup exited $($p.ExitCode)" }
  }
}
# Deep cargo target paths exceed MAX_PATH without this.
& 'C:\Program Files\Git\cmd\git.exe' config --system core.longpaths true
New-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem' -Name LongPathsEnabled -Value 1 -PropertyType DWORD -Force | Out-Null

# --- 4. rustup + stable msvc ----------------------------------------------------
$cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
if (-not (Test-Path $cargo)) {
  $exe = Download 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe' 'rustup-init.exe'
  & $exe -y --profile minimal --default-toolchain stable --default-host x86_64-pc-windows-msvc
  if ($LASTEXITCODE -ne 0) { throw "rustup-init exited $LASTEXITCODE" }
}
& $cargo --version

Set-Content -Path 'C:\thegn-provision.done' -Value (Get-Date -Format o)
Stop-Transcript | Out-Null
