# WS06-P2-00 - capture real WSL behaviour (checks C1-C11, the scriptable parts).
# Run ON WINDOWS 11 with WSL2 and a distro, from a WINDOWS TERMINAL tab:
#   pwsh -File scripts\spikes\ws06-p2-00.ps1 -OutDir C:\spike-out [-Distro Ubuntu] [-ModifyWslConf] [-RunExport]
# Raw bytes are captured through Start-Process file redirection (never a PowerShell
# pipeline that decodes to a string). MUTATING opt-ins, off by default:
#   -ModifyWslConf  C8: writes [interop] enabled=false / appendWindowsPath=false into the
#                   distro's /etc/wsl.conf (backup restored in finally, distro terminated)
#   -RunExport      C10: wsl --export/--import a copy named tgspike-<pid> (unregistered in
#                   finally). Needs free disk of ~2x the distro size.
# NOT scriptable here (do by hand, see the procedure page): C3 resize + Ctrl+C relay,
# C4 vim/claude/codex rendering, C7 fd 3 survival (needs a CreateProcess with an
# inherited handle), C9 on a policy-managed machine. Fixtures to commit are produced
# in OutDir\WS06-P2-00: C1-*.bin (raw) and lxss-dump.redacted.reg (review it first).
param(
    [Parameter(Mandatory = $true)][string]$OutDir,
    [string]$Distro = 'Ubuntu',
    [switch]$ModifyWslConf,
    [switch]$RunExport
)
. "$PSScriptRoot\lib.ps1"
$item = 'WS06-P2-00'
Sp-Init $item $OutDir
$d = Join-Path $OutDir $item
try {
    if (-not $env:WT_SESSION) { Sp-Die 'not running in Windows Terminal (WT_SESSION unset): C3-C4 need a real ConPTY host' }
    Sp-Need @('wsl.exe', 'reg.exe', 'cmd.exe')
    $pidTag = $PID

    function Wsl([string]$Name, [int]$Secs, [string]$A) { return (Sp-Cap $Name $Secs 'wsl.exe' $A) }
    function Hex16([string]$Path) {
        $b = [System.IO.File]::ReadAllBytes($Path)
        $n = [Math]::Min(16, $b.Length)
        return ((0..($n - 1) | ForEach-Object { '{0:x2}' -f $b[$_] }) -join ' ')
    }
    function Enc([string]$Path) {
        $b = [System.IO.File]::ReadAllBytes($Path)
        if ($b.Length -ge 2 -and $b[0] -eq 0xFF -and $b[1] -eq 0xFE) { return 'UTF-16LE with BOM' }
        if ($b.Length -ge 2 -and $b[1] -eq 0) { return 'UTF-16LE without BOM' }
        if ($b.Length -ge 3 -and $b[0] -eq 0xEF -and $b[1] -eq 0xBB -and $b[2] -eq 0xBF) { return 'UTF-8 with BOM' }
        return 'UTF-8 (or ASCII)'
    }

    [void](Wsl 'wsl-version' 30 '--version')
    Sp-Note 'wsl --version is captured raw in wsl-version.out (UTF-16LE unless WSL_UTF8=1); copy the version lines into the record.'
    [void](Wsl 'distro-uname' 60 "-d $Distro -e uname -r")
    Sp-Kv 'distro' $Distro

    # C1: raw bytes of wsl -l -v / --status, default and WSL_UTF8=1
    foreach ($utf8 in @($false, $true)) {
        $tag = if ($utf8) { 'utf8env' } else { 'default' }
        if ($utf8) { $env:WSL_UTF8 = '1' } else { Remove-Item Env:WSL_UTF8 -ErrorAction SilentlyContinue }
        foreach ($cmd in @(@('list', 'wsl -l -v'), @('status', 'wsl --status'))) {
            $f = Join-Path $d ("C1-{0}-{1}.bin" -f $cmd[0], $tag)
            [void](Sp-Cap ("C1-{0}-{1}" -f $cmd[0], $tag) 30 'cmd.exe' ("/c `"{0} > `"{1}`" 2>&1`"" -f $cmd[1], $f))
            Sp-Kv ("C1_{0}_{1}_encoding" -f $cmd[0], $tag) (Enc $f)
            Sp-Kv ("C1_{0}_{1}_first16" -f $cmd[0], $tag) (Hex16 $f)
        }
    }
    Remove-Item Env:WSL_UTF8 -ErrorAction SilentlyContinue
    Sp-Note 'C1 NOT CAPTURED here: localized headers (second language pack), WSL-absent text, WSL1 distro. Capture by hand if possible, else record "not captured".'

    # C2: registry dump (redacted copy for committing)
    $reg = Join-Path $d 'lxss-dump.reg'
    [void](Sp-Cap 'C2-export' 30 'reg.exe' ("export HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss `"$reg`" /y"))
    if (Test-Path $reg) {
        $txt = [System.Text.Encoding]::Unicode.GetString([System.IO.File]::ReadAllBytes($reg))
        $red = (Sp-Redact $txt)
        [System.IO.File]::WriteAllText((Join-Path $d 'lxss-dump.redacted.reg'), $red, [System.Text.Encoding]::Unicode)
        Remove-Item $reg  # never leave the unredacted dump in the evidence directory
        Sp-Note 'C2: lxss-dump.redacted.reg written (user name/profile path replaced). REVIEW for GUIDs and BasePath before committing; note redactions in the record.'
    }

    # C3 (non-interactive part): --cd / -u / -e and the pty size the distro sees
    [void](Wsl 'C3-cd-u-e' 60 "-d $Distro -u root --cd /tmp -e sh -c `"pwd; id -u; stty size`"")
    Sp-Note 'C3 resize (SIGWINCH reaches the Linux pty) and Ctrl+C (SIGINT exactly once, wsl.exe survives): MANUAL, see the procedure page'

    # C5: kernel, Landlock ABI, bwrap, AppArmor
    [void](Wsl 'C5-landlock-abi' 60 "-d $Distro -e python3 -c `"import ctypes;print(ctypes.CDLL(None).syscall(444,None,0,1))`"")
    [void](Wsl 'C5-bwrap' 60 "-d $Distro -e sh -c `"bwrap --unshare-user true; echo bwrap_rc=`$?`"")
    [void](Wsl 'C5-apparmor' 60 "-d $Distro -e sh -c `"ls /sys/module/apparmor 2>&1; aa-status 2>&1 | head -n3`"")

    # C6: does a held wsl.exe keep the distro Running past the idle timeout?
    $env:WSL_UTF8 = '1'
    function DistroState {
        $out = & wsl.exe -l -v 2>$null
        $line = $out | Where-Object { $_ -match "\b$([regex]::Escape($Distro))\b" } | Select-Object -First 1
        if ($line -match 'Running') { return 'Running' } elseif ($line -match 'Stopped') { return 'Stopped' } else { return 'unknown' }
    }
    function PollStates([int]$Total, [int]$Step) {
        $s = @()
        for ($t = 0; $t -le $Total; $t += $Step) { $s += ("t={0}s:{1}" -f $t, (DistroState)); Start-Sleep -Seconds $Step }
        return ($s -join ' ')
    }
    & wsl.exe --terminate $Distro 2>$null
    $hold = Start-Process -FilePath 'wsl.exe' -ArgumentList "-d $Distro -e /bin/sleep infinity" -PassThru -WindowStyle Hidden
    Sp-OnExit { if (-not $hold.HasExited) { $hold.Kill() }; & wsl.exe --terminate $Distro 2>$null }
    Start-Sleep -Seconds 3
    $heldStates = PollStates 90 15
    $hold.Kill(); $hold.WaitForExit()
    & wsl.exe --terminate $Distro 2>$null
    & wsl.exe -d $Distro -e true 2>$null
    $ctlStates = PollStates 90 15
    Remove-Item Env:WSL_UTF8 -ErrorAction SilentlyContinue
    Sp-Kv 'C6_held' $heldStates
    Sp-Kv 'C6_control_no_hold' $ctlStates
    $holdWorks = ($heldStates -notmatch 'Stopped') -and ($ctlStates -match 'Stopped')

    # C8: WSLENV name passing with interop disabled
    $c8 = 'NOT RUN (pass -ModifyWslConf)'
    if ($ModifyWslConf) {
        [void](Wsl 'C8-conf-backup' 30 "-d $Distro -u root -e sh -c `"cp -a /etc/wsl.conf /etc/wsl.conf.tgspike.bak 2>/dev/null || echo none > /etc/wsl.conf.tgspike.none`"")
        Sp-OnExit {
            & wsl.exe -d $Distro -u root -e sh -c 'if [ -f /etc/wsl.conf.tgspike.bak ]; then mv /etc/wsl.conf.tgspike.bak /etc/wsl.conf; else rm -f /etc/wsl.conf /etc/wsl.conf.tgspike.none; fi' 2>$null
            & wsl.exe --terminate $Distro 2>$null
        }
        [void](Wsl 'C8-conf-write' 30 "-d $Distro -u root -e sh -c `"printf '[interop]\nenabled=false\nappendWindowsPath=false\n' > /etc/wsl.conf`"")
        & wsl.exe --terminate $Distro 2>$null
        Start-Sleep -Seconds 8
        $env:TG_SPIKE_VAL = 'tgspike-not-a-secret-123'
        $env:WSLENV = 'TG_SPIKE_VAL'
        [void](Wsl 'C8-printenv' 60 "-d $Distro -e printenv TG_SPIKE_VAL")
        Remove-Item Env:TG_SPIKE_VAL, Env:WSLENV -ErrorAction SilentlyContinue
        $got = (Get-Content (Join-Path $d 'C8-printenv.out') -Raw -ErrorAction SilentlyContinue)
        $c8 = if ("$got" -match 'tgspike-not-a-secret-123') { 'WSLENV name passing WORKS with interop disabled' } else { 'WSLENV value did NOT arrive' }
    }
    Sp-Kv 'C8' $c8

    # C9: policy-disabled detection (policy-managed VM only)
    Sp-Kv 'C9_HKLM_Policies_WSL_present' (Test-Path 'HKLM:\SOFTWARE\Policies\WSL')
    Sp-Note 'C9: if no policy-managed VM is available record "deferred to device check W11".'

    # C10: export streaming and import from stdin
    $c10 = 'NOT RUN (pass -RunExport)'
    if ($RunExport) {
        $tar = Join-Path $d 'export.tar'
        $dest = Join-Path $env:TEMP "tgspike-import-$pidTag"
        $name = "tgspike-$pidTag"
        Sp-OnExit { & wsl.exe --unregister $name 2>$null; Remove-Item -Recurse -Force $dest, $tar -ErrorAction SilentlyContinue }
        $sw = [Diagnostics.Stopwatch]::StartNew()
        [void](Sp-Cap 'C10-export-stdout' 1800 'cmd.exe' ("/c `"wsl --export $Distro - > `"$tar`"`""))
        $tExport = $sw.Elapsed.TotalSeconds
        $sizeA = (Get-Item $tar).Length
        New-Item -ItemType Directory -Force $dest | Out-Null
        $sw.Restart()
        $rcI = Sp-Cap 'C10-import-stdin' 1800 'cmd.exe' ("/c `"wsl --import $name `"$dest`" - --version 2 < `"$tar`"`"")
        $tImport = $sw.Elapsed.TotalSeconds
        $rcU = Wsl 'C10-imported-uname' 60 "-d $name -e uname -r"
        $c10 = "export_bytes=$sizeA export_s=$([Math]::Round($tExport,1)) import_rc=$rcI import_s=$([Math]::Round($tImport,1)) imported_uname_rc=$rcU sha256=$((Get-FileHash $tar).Hash)"
    }
    Sp-Kv 'C10' $c10

    # C11: command-line length through wsl.exe -e
    $largest = 0
    foreach ($n in 4096, 8192, 16384, 24576, 32000) {
        $arg = ('A' * $n)
        $rc = Wsl ("C11-$n") 60 "-d $Distro -e sh -c `"printf %s \`"`$1\`" | wc -c`" _ $arg"
        $outTxt = (Get-Content (Join-Path $d "C11-$n.out") -Raw -ErrorAction SilentlyContinue)
        if ($rc -eq 0 -and "$outTxt" -match "\b$n\b") { $largest = $n }
    }
    Sp-Kv 'C11_largest_working_arg_bytes' $largest

    # decisions that can be computed
    Sp-Note ''
    Sp-Note '## decisions (computed where possible)'
    Sp-Note ("C6: " + $(if ($holdWorks) { 'held sleep infinity KEEPS the distro Running and the control stops: connector emits keepalive.hold' } else { 'hold did not keep the distro Running (or control did not stop): omit keepalive.hold; doctor recommends .wslconfig instanceIdleTimeout=-1' }))
    Sp-Note "C8: $c8"
    Sp-Note "C10: $c10"
    Sp-Note ("C11: intent size bound = min(16 KiB, " + $largest + " bytes)")
    Sp-Note 'C1/C2: fixtures + parser/discovery drivers (encoding lines above). C7 (fd 3), C3, C4: MANUAL.'
    Sp-Verdict 'C6 hold keeps Running => keepalive.hold else omit; C8 WSLENV works => env via WSLENV else refuse secret passthrough; C10 stdin import works => streaming else temp tarball; C11 sets the intent bound; C3/C4 failure blocks P2-01/P2-02/P2-05a' ("C6 hold=$holdWorks; C8=$c8; C11 largest=$largest; C3/C4/C7 manual")
} finally {
    Sp-RunCleanups
}
