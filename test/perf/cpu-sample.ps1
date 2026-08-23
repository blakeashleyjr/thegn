<#
.SYNOPSIS
  Steady-state / idle CPU harness for thegn on Windows.

.DESCRIPTION
  The Windows counterpart to test/perf/cpu-sample.sh, which is Linux-only by
  construction (it reads /proc/PID/task/*/stat for the per-thread breakdown and
  exits 0 everywhere else). That left the ~0%-idle invariant with no gate at all
  on the platform where subprocess and connection cost hurt most - which is how
  audit finding W2 (23% of a core at idle, release) went unnoticed.

  Same shape as the shell harness so the numbers are comparable:
    - the same fixture: N worktrees off one repo with a bare origin, the first
      <dirty> of them carrying an untracked file (mirrors test/perf/lib/fixture.sh
      and crates/thegn-svc/benches/support/fixture.rs - keep all three in sync)
    - fully isolated state, settle, then sample over a fixed window
    - emits the same JSON keys (cores_total, scenario, build, worktrees,
      window_ms, git_sha, host_tag)

  It adds one metric the shell harness does not have: **git_spawns**, the number
  of child processes thegn creates during the window. On Windows a bare
  CreateProcess costs ~40ms (vs ~10-15ms on a clean box, more with a security
  agent scanning every spawn), so spawn count is the leading indicator of idle
  cost here - and it is a portable, quantized signal, unlike wall-clock CPU.

  Isolation note: the harness sets LOCALAPPDATA + APPDATA + THEGN_DIR, which are
  what util::xdg_state_home()/xdg_config_home() read by default on Windows.
  XDG_STATE_HOME/XDG_CONFIG_HOME also work now (an explicitly set one wins on
  every platform) - either way the run must never touch the daily-driver DB.

.PARAMETER Scenario
  Label recorded in the output. Default "idle".

.PARAMETER Ceiling
  Cores. Exit 2 when an idle release run exceeds it. Default 0.12, matching
  cpu-sample.sh's fixed guard.

.EXAMPLE
  pwsh test/perf/cpu-sample.ps1
.EXAMPLE
  pwsh test/perf/cpu-sample.ps1 -Worktrees 14 -Dirty 4 -Json
#>
[CmdletBinding()]
param(
    [string]$Scenario = 'idle',
    [string]$Bin = 'target\release\thegn.exe',
    [int]$Worktrees = 14,
    [int]$Dirty = 4,
    [int]$SettleMs = 2500,
    [int]$WindowMs = 8000,
    [double]$Ceiling = 0.12,
    [switch]$Json,
    [switch]$KeepTmp
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$binPath = if ([IO.Path]::IsPathRooted($Bin)) { $Bin } else { Join-Path $repoRoot $Bin }

# --- guard: never measure a debug or stale binary (mirrors just _perf-guard) ---
if (-not (Test-Path $binPath)) {
    Write-Error "perf: $binPath not found - run: cargo build --release -p thegn-host"
}
$binInfo = Get-Item $binPath
if ($binPath -notmatch '[\\/]release[\\/]') {
    Write-Error "perf: refusing to measure a non-release binary: $binPath"
}
$newer = Get-ChildItem (Join-Path $repoRoot 'crates') -Recurse -Filter *.rs -ErrorAction SilentlyContinue |
    Where-Object { $_.LastWriteTime -gt $binInfo.LastWriteTime } | Select-Object -First 1
if ($newer) {
    Write-Error "perf: $binPath is STALE (newer source: $($newer.FullName)) - rebuild first"
}
Write-Host "perf: binary=$binPath mtime=$($binInfo.LastWriteTime) profile=release"

# --- isolated environment -----------------------------------------------------
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("tg-perf-" + [Guid]::NewGuid().ToString('N').Substring(0, 12))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$env:LOCALAPPDATA = Join-Path $tmp 'state'
$env:APPDATA = Join-Path $tmp 'config'
$env:THEGN_DIR = Join-Path $tmp 'thegnhome'
$env:HOME = $tmp
$env:USERPROFILE = $tmp
$env:GIT_CONFIG_GLOBAL = Join-Path $tmp 'gitconfig'
$env:GIT_CONFIG_SYSTEM = 'NUL'
$env:THEGN_NO_MIGRATE = '1'
$env:THEGN_NO_KEYRING = '1'
New-Item -ItemType Directory -Force -Path $env:LOCALAPPDATA, $env:APPDATA, $env:THEGN_DIR | Out-Null
Set-Content -Path $env:GIT_CONFIG_GLOBAL -Value "[user]`n`tname = perf`n`temail = perf@example.com`n" -Encoding ascii

function Git-In([string]$dir, [string[]]$gitArgs) {
    & git -C $dir @gitArgs 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Error "git $($gitArgs -join ' ') failed in $dir" }
}

# --- fixture (mirrors test/perf/lib/fixture.sh) --------------------------------
Write-Host "perf: building fixture ($Worktrees worktrees, $Dirty dirty)..."
$root = Join-Path $tmp 'repo'
$origin = Join-Path $tmp 'origin.git'
New-Item -ItemType Directory -Force -Path $root | Out-Null
Git-In $root @('init', '-q', '-b', 'main', '.')
1..20 | ForEach-Object { Set-Content -Path (Join-Path $root "file_$_.txt") -Value "line $_" -Encoding ascii }
New-Item -ItemType Directory -Force -Path (Join-Path $root 'src') | Out-Null
1..20 | ForEach-Object { Set-Content -Path (Join-Path $root "src\mod_$_.rs") -Value "fn f$_() {}" -Encoding ascii }
Git-In $root @('add', '-A')
Git-In $root @('-c', 'commit.gpgsign=false', 'commit', '-q', '-m', 'seed')
& git clone -q --bare $root $origin 2>&1 | Out-Null
Git-In $root @('remote', 'add', 'origin', $origin)
Git-In $root @('fetch', '-q', 'origin')
Git-In $root @('branch', '-q', '--set-upstream-to=origin/main', 'main')
$wtDir = Join-Path $tmp 'worktrees'
New-Item -ItemType Directory -Force -Path $wtDir | Out-Null
for ($i = 1; $i -le $Worktrees; $i++) {
    $p = Join-Path $wtDir "wt-$i"
    Git-In $root @('worktree', 'add', '-q', '-b', "wt-$i", $p, 'main')
    & git -C $p branch -q --set-upstream-to=origin/main "wt-$i" 2>&1 | Out-Null
}
for ($i = 1; $i -le [Math]::Min($Dirty, $Worktrees); $i++) {
    Set-Content -Path (Join-Path $wtDir "wt-$i\UNCOMMITTED.txt") -Value 'scratch' -Encoding ascii
}

# --- launch -------------------------------------------------------------------
# A HIDDEN console, not a Windows Terminal tab. thegn used to refuse anything
# that did not identify itself as WT, so this harness had to open a real
# desktop window to measure anything - which made it unusable while the machine
# was in use, and impossible headless. thegn now accepts any console that takes
# ENABLE_VIRTUAL_TERMINAL_PROCESSING (see platform::console_caps), so a hidden
# one works and nothing appears on screen.
#
# THEGN_BENCH_RUN_MS runs the full loop (ticker, hydration, tokio pool) for a
# bounded window and then exits via the existing shutdown flag with a single
# waker pulse - it does not introduce a poll timeout, so the ~0%-idle event
# model is measured intact.
$runMs = $SettleMs + $WindowMs + 4000
$launch = Join-Path $tmp 'launch.ps1'
@"
`$env:LOCALAPPDATA='$($env:LOCALAPPDATA)'; `$env:APPDATA='$($env:APPDATA)'
`$env:THEGN_DIR='$($env:THEGN_DIR)'; `$env:HOME='$tmp'; `$env:USERPROFILE='$tmp'
`$env:GIT_CONFIG_GLOBAL='$($env:GIT_CONFIG_GLOBAL)'; `$env:GIT_CONFIG_SYSTEM='NUL'
`$env:THEGN_NO_MIGRATE='1'; `$env:THEGN_NO_KEYRING='1'
`$env:THEGN_BENCH_RUN_MS='$runMs'; `$env:THEGN_PERF='1'; `$env:THEGN_LOG='info'
Set-Location '$root'
# stderr to a file, stdout LEFT ON THE CONSOLE. Redirecting stdout (``*>``)
# means the frame is written to a file rather than a terminal, so the run does
# not measure rendering at all - and thegn now correctly refuses to start when
# stdout is not a console and nothing in the environment vouches for one.
& '$binPath' 2> '$tmp\thegn.out'
"@ | Set-Content -Path $launch -Encoding utf8

Start-Process powershell.exe -WindowStyle Hidden `
    -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $launch) | Out-Null

$deadline = (Get-Date).AddSeconds(30)
$ui = $null
while ((Get-Date) -lt $deadline -and -not $ui) {
    Start-Sleep -Milliseconds 300
    $ui = Get-CimInstance Win32_Process -Filter "Name='thegn.exe'" |
        Where-Object { $_.CommandLine -notlike '*daemon*' } | Select-Object -First 1
}
if (-not $ui) { Write-Error "perf: thegn never started; see $tmp\thegn.out" }
$proc = Get-Process -Id $ui.ProcessId

# --- settle, then sample ------------------------------------------------------
Start-Sleep -Milliseconds $SettleMs
$proc.Refresh(); $c0 = $proc.TotalProcessorTime; $w0 = Get-Date
$children = @{}
$sampleEnd = (Get-Date).AddMilliseconds($WindowMs)
while ((Get-Date) -lt $sampleEnd) {
    Get-CimInstance Win32_Process -Filter "ParentProcessId=$($ui.ProcessId)" |
        ForEach-Object { $children["$($_.ProcessId)|$($_.Name)"] = $_.CommandLine }
    Start-Sleep -Milliseconds 150
}
$proc.Refresh(); $c1 = $proc.TotalProcessorTime; $w1 = Get-Date

$wallMs = ($w1 - $w0).TotalMilliseconds
$coresTotal = [Math]::Round((($c1 - $c0).TotalMilliseconds / $wallMs), 4)
$spawnsByName = $children.Keys | ForEach-Object { ($_ -split '\|')[1] } | Group-Object |
    Sort-Object Count -Descending
$gitSpawns = ($spawnsByName | Where-Object { $_.Name -eq 'git.exe' } | Select-Object -First 1).Count
if (-not $gitSpawns) { $gitSpawns = 0 }

Get-Process -Name thegn -ErrorAction SilentlyContinue | ForEach-Object { try { $_.Kill() } catch {} }

# Best-effort: a source archive (not a clone) has no .git, and that must not
# fail a measurement run.
$gitSha = 'unknown'
try {
    $sha = & git -C $repoRoot rev-parse --short HEAD 2>&1
    if ($LASTEXITCODE -eq 0 -and $sha) { $gitSha = "$sha".Trim() }
} catch { }
$cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name -replace '\s+', ' '
$hostTag = 'x86_64-' + [Math]::Abs($cpu.GetHashCode())

$result = [ordered]@{
    scenario    = $Scenario
    build       = 'release'
    worktrees   = $Worktrees
    window_ms   = [int]$wallMs
    cores_total = $coresTotal
    git_spawns  = $gitSpawns
    git_sha     = $gitSha
    host_tag    = $hostTag
}
if (-not $KeepTmp) { Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue }

if ($Json) {
    $result | ConvertTo-Json -Compress
} else {
    "cores_total=$coresTotal  git_spawns=$gitSpawns/$([int]$wallMs)ms  (host=$hostTag sha=$gitSha)"
    '  child processes spawned during the window:'
    $spawnsByName | ForEach-Object { "    {0,-14} {1}" -f $_.Name, $_.Count }
    # Which git reads are still on the hot path. `-C <dir>` is stripped so the
    # subcommand groups regardless of which worktree it targeted.
    $subs = $children.Values | Where-Object { $_ -and $_ -match '(?i)\bgit(\.exe)?\b' } | ForEach-Object {
        $t = ($_ -replace '(?i)^.*?git(\.exe)?"?\s*', '') -replace '^-C\s+("[^"]*"|\S+)\s*', ''
        $t = $t -replace '^(-c\s+\S+\s+)+', ''
        ($t -split '\s+' | Where-Object { $_ -and $_ -notmatch '^-' } | Select-Object -First 2) -join ' '
    }
    if ($subs) {
        '  git subcommands:'
        $subs | Group-Object | Sort-Object Count -Descending |
            ForEach-Object { "    {0,-34} {1}" -f $_.Name, $_.Count }
    }
}

if ($Scenario -eq 'idle' -and $coresTotal -gt $Ceiling) {
    Write-Host "FAIL: idle cores_total=$coresTotal exceeds ceiling=$Ceiling cores" -ForegroundColor Red
    exit 2
}
exit 0
