# WS06-P0-05 - Windows baseline gate: scripted part of the on-machine checklist.
# Run ON WINDOWS 11 from a WINDOWS TERMINAL tab (not conhost), in a checkout of the
# commit under test, with the Rust toolchain installed.
#   pwsh -File scripts\spikes\ws06-p0-05.ps1 -OutDir C:\spike-out [-CiRunUrl <url>] [-Manual '2.2=pass,2.5=pass,...']
# It builds (cargo; heavy, once) and automates: environment record, CI run status
# (via `gh`, if present), check 2.1 (waker_spike ticks + idle CPU), 2.7 (conhost
# refusal) and 2.8 (daemon two-instance race, isolated state dir). Checks 2.2, 2.3,
# 2.4, 2.5, 2.6, 2.9 need a human at the terminal: do them, then re-run with -Manual so
# the gate verdict can be computed. It never edits the repo and never dispatches CI.
# The opt-in CI run is dispatched by a person: gh workflow run ci.yml --ref <branch> -f extras=true
param(
    [Parameter(Mandatory = $true)][string]$OutDir,
    [string]$RepoDir = (Get-Location).Path,
    [string]$CiRunUrl = $env:THEGN_CI_RUN_URL,
    [string]$Manual = ''
)
. "$PSScriptRoot\lib.ps1"
$item = 'WS06-P0-05'
Sp-Init $item $OutDir
try {
    if (-not $env:WT_SESSION) { Sp-Die 'not running in Windows Terminal (WT_SESSION unset): the checks need a real ConPTY host' }
    Sp-Need @('cargo', 'git')
    Set-Location $RepoDir
    Sp-Kv 'thegn_commit' ((git rev-parse HEAD) 2>&1)
    Sp-Kv 'cargo' ((cargo --version) 2>&1)
    Sp-Kv 'ci_run_url' $(if ($CiRunUrl) { $CiRunUrl } else { 'NOT PROVIDED' })

    # --- CI: green windows + windows-artifact-smoke?
    $ciGreen = 'NOT CONFIRMED'
    if ($CiRunUrl -and (Get-Command gh -ErrorAction SilentlyContinue)) {
        $json = (gh run view $CiRunUrl --json status, conclusion, jobs) 2>&1 | Out-String
        Sp-Note $json
        try {
            $r = $json | ConvertFrom-Json
            $w = $r.jobs | Where-Object { $_.name -eq 'windows' }
            $s = $r.jobs | Where-Object { $_.name -eq 'windows-artifact-smoke' }
            if ($w -and $w.conclusion -eq 'success' -and $s -and $s.conclusion -eq 'success') { $ciGreen = 'GREEN' } else { $ciGreen = 'NOT GREEN (see jobs above)' }
        } catch { $ciGreen = 'could not parse gh output' }
    }
    Sp-Kv 'ci_windows_and_artifact_smoke' $ciGreen

    # --- isolated state so the daemon check cannot touch a real session
    $state = Join-Path $env:TEMP ("tgspike-state-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Force $state | Out-Null
    $oldState = $env:XDG_STATE_HOME
    $env:XDG_STATE_HOME = $state
    Sp-OnExit { $env:XDG_STATE_HOME = $oldState; Remove-Item -Recurse -Force $state -ErrorAction SilentlyContinue }

    # --- build once
    $rc = Sp-Cap 'build-spike' 3000 'cargo' 'build -p thegn-host --example waker_spike --bin thegn'
    if ($rc -ne 0) { Sp-Die 'cargo build failed: see build-spike.err (the gate is open: a build failure is a port bug)' }
    $target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $RepoDir 'target' }
    $thegn = Join-Path $target 'debug\thegn.exe'
    $spike = Join-Path $target 'debug\examples\waker_spike.exe'

    $res = @{}

    # --- 2.1 waker_spike: ~1 tick/second, ~0% CPU between ticks
    $sp = Start-Process -FilePath $spike -NoNewWindow -PassThru `
        -RedirectStandardOutput (Join-Path $OutDir "$item\c2.1.out") -RedirectStandardError (Join-Path $OutDir "$item\c2.1.err")
    Sp-OnExit { if (-not $sp.HasExited) { $sp.Kill() } }
    Start-Sleep -Seconds 3
    $c0 = $sp.TotalProcessorTime.TotalSeconds
    Start-Sleep -Seconds 8
    $c1 = $sp.TotalProcessorTime.TotalSeconds
    $sp.Kill(); $sp.WaitForExit()
    $ticks = @(Get-Content (Join-Path $OutDir "$item\c2.1.out") | Where-Object { $_ -match 'tick' }).Count
    $cpu = [Math]::Round((($c1 - $c0) / 8.0) * 100, 2)
    Sp-Kv 'c2.1_ticks_in_11s' $ticks
    Sp-Kv 'c2.1_cpu_percent_over_8s' $cpu
    Sp-Note 'c2.1 key echo is instant: MANUAL (type into the spike and watch)'
    $res['2.1'] = $(if ($ticks -ge 9 -and $ticks -le 13 -and $cpu -lt 1.0) { 'pass' } else { 'fail' })

    # --- 2.7 conhost refusal
    $out27 = Join-Path $OutDir "$item\c2.7.txt"
    $rc = Sp-Cap 'c2.7' 30 'conhost.exe' ("cmd /c `"`"$thegn`" > `"$out27`" 2>&1`"")
    Start-Sleep -Seconds 2
    $t27 = if (Test-Path $out27) { Get-Content $out27 -Raw } else { '' }
    Get-Process thegn -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    $res['2.7'] = $(if ($t27 -match 'Windows Terminal') { 'pass' } else { 'fail' })
    Sp-Kv 'c2.7_output_head' ($t27.Substring(0, [Math]::Min(300, $t27.Length)))

    # --- 2.8 daemon two-instance race (state dir isolated above)
    $d1 = Start-Process -FilePath $thegn -ArgumentList 'daemon' -NoNewWindow -PassThru `
        -RedirectStandardOutput (Join-Path $OutDir "$item\c2.8-first.out") -RedirectStandardError (Join-Path $OutDir "$item\c2.8-first.err")
    Sp-OnExit { if (-not $d1.HasExited) { $d1.Kill() } }
    Start-Sleep -Seconds 4
    $rc2 = Sp-Cap 'c2.8-second' 30 $thegn 'daemon'
    $t28 = Get-Content (Join-Path $OutDir "$item\c2.8-second.out"), (Join-Path $OutDir "$item\c2.8-second.err") -Raw -ErrorAction SilentlyContinue
    $res['2.8'] = $(if ($rc2 -eq 0 -and ("$t28" -match 'already running')) { 'pass' } else { 'fail' })
    Sp-Note 'c2.8 second half (a daemon-backed pane opens over the pipe): MANUAL'

    # --- manual results folded in
    foreach ($kv in ($Manual -split ',')) {
        if ($kv -match '^\s*(2\.\d)\s*=\s*(pass|fail|blocked)\s*$') { $res[$Matches[1]] = $Matches[2] }
    }
    Sp-Note ''
    Sp-Note '## checklist (2.1-2.9 of add-windows-compositor-validation)'
    foreach ($k in '2.1', '2.2', '2.3', '2.4', '2.5', '2.6', '2.7', '2.8', '2.9') {
        $v = if ($res.ContainsKey($k)) { $res[$k] } else { 'MANUAL: not yet recorded (pass -Manual)' }
        Sp-Note "check $k : $v"
    }
    $required = '2.1', '2.2', '2.5', '2.8'
    $missing = @($required | Where-Object { -not $res.ContainsKey($_) -or $res[$_] -ne 'pass' })
    if ($ciGreen -eq 'GREEN' -and $missing.Count -eq 0) {
        $v = 'gate PASSED (green opt-in windows run + checks 2.1, 2.2, 2.5, 2.8 pass). Others may be known issues, each with a filed issue.'
    } else {
        $v = "gate OPEN: ci=$ciGreen; required checks not passing: $($missing -join ', '). File the failures; fix up to ~400 lines here, larger work as separate issues."
    }
    Sp-Verdict 'passed iff green opt-in windows run AND checks 2.1, 2.2, 2.5, 2.8 pass; otherwise open' $v
} finally {
    Sp-RunCleanups
}
