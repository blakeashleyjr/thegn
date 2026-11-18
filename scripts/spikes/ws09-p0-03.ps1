# WS09-P0-03 - Windows-host transport facts (the Windows half; the paid-account rows are
# manual, see the procedure page). Run ON WINDOWS 11 after the WS06-P0-05 baseline gate:
#   $env:SPIKE_SSH_TARGET = 'user@linux-host'    # key login, no prompts
#   pwsh -File scripts\spikes\ws09-p0-03.ps1 -OutDir C:\spike-out [-Distro Ubuntu]
# Measures, with sha256 on both sides: ssh.exe stdio binary safety (50 MB), wsl.exe -e
# stdio binary safety (50 MB), the Win32-OpenSSH ProxyCommand interpreter and % escaping,
# ControlMaster on Win32-OpenSSH, and ControlMaster via WSL OpenSSH incl. the agent gap.
# The WSL idle-timeout vs held `sleep infinity` row is C6 of ws06-p2-00.ps1 (same device
# session, shared row): copy that result into VERIFICATION.md instead of re-measuring.
# The PowerShell/cmd quoting goldens of WS09-P1-02 do not exist yet: recorded
# "blocked: goldens not yet written". Redirections use cmd.exe so bytes are never decoded.
param(
    [Parameter(Mandatory = $true)][string]$OutDir,
    [string]$Distro = 'Ubuntu'
)
. "$PSScriptRoot\lib.ps1"
$item = 'WS09-P0-03'
Sp-Init $item $OutDir
$d = Join-Path $OutDir $item
try {
    if (-not $env:SPIKE_SSH_TARGET) { Sp-Die 'SPIKE_SSH_TARGET is not set (a Linux host reachable with key login)' }
    Sp-Need @('ssh.exe', 'wsl.exe', 'cmd.exe')
    $target = $env:SPIKE_SSH_TARGET
    Sp-Kv 'ssh_version' ((& ssh.exe -V 2>&1) -join ' ')
    Sp-Kv 'wsl_version_first_line' ((& wsl.exe --version 2>$null | Select-Object -First 1) -join '')
    Sp-Kv 'target' '<SPIKE_SSH_TARGET>'

    $inFile = Join-Path $d 'in.bin'
    $bytes = New-Object byte[] (50MB)
    [System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    [System.IO.File]::WriteAllBytes($inFile, $bytes)
    Sp-OnExit { Remove-Item (Join-Path $d '*.bin') -Force -ErrorAction SilentlyContinue }
    $sent = (Get-FileHash $inFile -Algorithm SHA256).Hash
    Sp-Kv 'sent_sha256' $sent

    function RoundTrip([string]$Name, [string]$Cmd) {
        $o = Join-Path $d "$Name.bin"
        $rc = Sp-Cap $Name 600 'cmd.exe' ("/c `"$Cmd < `"$inFile`" > `"$o`"`"")
        $got = if (Test-Path $o) { (Get-FileHash $o -Algorithm SHA256).Hash } else { 'none' }
        Remove-Item $o -Force -ErrorAction SilentlyContinue
        $ok = ($rc -eq 0 -and $got -eq $sent)
        Sp-Kv "$Name" ("rc=$rc round_trip_match=$ok")
        return $ok
    }
    $sshSafe = RoundTrip 'ssh-exe-stdio-50MB' "ssh.exe -o BatchMode=yes $target cat"
    $wslSafe = RoundTrip 'wsl-exe-e-stdio-50MB' "wsl.exe -d $Distro -e cat"

    # ProxyCommand interpreter and % escaping: the command writes what it saw to stderr
    $null = Sp-Cap 'proxycommand-cmd' 30 'ssh.exe' "-o BatchMode=yes -o ConnectTimeout=5 -o `"ProxyCommand=cmd /c echo HOST=%h PORT=%p USER=%r 1>&2 & exit 1`" tgspike-proxy-host"
    $null = Sp-Cap 'proxycommand-powershell' 30 'ssh.exe' "-o BatchMode=yes -o ConnectTimeout=5 -o `"ProxyCommand=powershell -NoProfile -Command [Console]::Error.WriteLine('HOST=%h PORT=%p'); exit 1`" tgspike-proxy-host"
    $null = Sp-Cap 'proxycommand-percent-escape' 30 'ssh.exe' "-o BatchMode=yes -o ConnectTimeout=5 -o `"ProxyCommand=cmd /c echo LITERAL=%%h 1>&2 & exit 1`" tgspike-proxy-host"
    Sp-Note 'read proxycommand-*.err: which interpreter ran (cmd vs powershell), whether %h/%p/%r expanded, and whether %% yields a literal %'

    # ControlMaster on Win32-OpenSSH (expected unsupported) and via WSL OpenSSH
    $cp = Join-Path $env:TEMP 'tgspike-cm-%C'
    $null = Sp-Cap 'controlmaster-win32' 60 'ssh.exe' "-o BatchMode=yes -o ControlMaster=auto -o `"ControlPath=$cp`" -o ControlPersist=10 $target true"
    $rcWslCm = Sp-Cap 'controlmaster-wsl-first' 90 'wsl.exe' "-d $Distro -e ssh -o BatchMode=yes -o ControlMaster=auto -o ControlPath=/tmp/tgspike-%C -o ControlPersist=15 $target true"
    $rcWslChk = Sp-Cap 'controlmaster-wsl-check' 30 'wsl.exe' "-d $Distro -e ssh -o BatchMode=yes -o ControlPath=/tmp/tgspike-%C -O check $target"
    Sp-OnExit { & wsl.exe -d $Distro -e ssh -o "ControlPath=/tmp/tgspike-%C" -O exit $target 2>$null }
    $null = Sp-Cap 'agent-socket-in-wsl' 30 'wsl.exe' "-d $Distro -e sh -c `"echo SSH_AUTH_SOCK=`${SSH_AUTH_SOCK:-unset}`""
    Sp-Note 'agent-socket gap: if SSH_AUTH_SOCK is unset in WSL, keys held by the Windows ssh-agent are not visible to WSL ssh.'

    Sp-Note ''
    Sp-Note 'ROW blocked: PowerShell/cmd quoting goldens of WS09-P1-02 (goldens not yet written)'
    Sp-Note 'ROW shared with WS06-P2-00 C6: WSL instanceIdleTimeout vs held sleep infinity'
    Sp-Note 'ACCOUNT rows (AWS SSM / EC2 Instance Connect, GCP IAP, Azure Bastion, Teleport, Twingate): manual; "blocked: no account" with owner and W5 date if no account'
    $v = "ssh.exe stdio binary-safe=$sshSafe ; wsl.exe -e stdio binary-safe=$wslSafe ; win32 ControlMaster rc in controlmaster-win32 (expected unsupported) ; WSL ControlMaster check rc=$rcWslChk"
    $consequence = @()
    if (-not $sshSafe) { $consequence += 'multiplex = "thegn" on Windows needs ssh_program via WSL or the b64 framing mux feature (WS09-P4-04a adds b64)' }
    if (-not $wslSafe) { $consequence += 'WS06 generated connector sets exec_binary_safe = false: no sync/mux over wsl.exe -e; use a \\wsl.localhost copy for install' }
    if ($consequence.Count -eq 0) { $consequence += 'both stdios binary-safe: no b64 framing needed, exec_binary_safe stays true' }
    Sp-Verdict 'ssh.exe not binary-safe => ssh_program via WSL or b64 framing; wsl.exe -e not binary-safe => exec_binary_safe = false' ($v + ' => ' + ($consequence -join '; '))
} finally {
    Sp-RunCleanups
}
