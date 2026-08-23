<#
.SYNOPSIS
  Capture a thegn debug session from inside the terminal you are actually using.

.DESCRIPTION
  Windows capability detection depends on things only the real terminal
  provides - the environment it exports, and whether stdout is a console that
  takes VT processing. None of that is observable from a captured/redirected
  shell, so "it looks wrong in WezTerm" can only be diagnosed from inside
  WezTerm.

  Writes everything to a single directory:
    env.txt      - the terminal-relevant environment, verbatim
    doctor.txt   - `thegn doctor`, i.e. what thegn resolved from that
    thegn.log    - THEGN_LOG=debug from the compositor run
    console.txt  - whether stdout is a console, its mode bits and code page

  Then launches the compositor. Quit it normally (`q`) and the log is waiting.

.PARAMETER OutDir
  Where to write the capture. Defaults to a timestamped dir under %TEMP%.

.PARAMETER NoRun
  Capture the environment and doctor output, then stop. Use when the
  compositor is the thing crashing and you only want the "before" picture.

.EXAMPLE
  # From inside WezTerm:
  powershell -NoProfile -ExecutionPolicy Bypass -File dev\wezterm-debug.ps1
#>
[CmdletBinding()]
param(
    [string]$OutDir,
    [switch]$NoRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $repo 'target\release\thegn.exe'
if (-not (Test-Path $bin)) {
    $bin = Join-Path $repo 'target\debug\thegn.exe'
}
if (-not (Test-Path $bin)) {
    throw "no thegn.exe in target\release or target\debug - run: cargo build --release -p thegn-host"
}

if (-not $OutDir) {
    $OutDir = Join-Path $env:TEMP ('thegn-debug-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

Write-Host "thegn debug capture -> $OutDir" -ForegroundColor Cyan
Write-Host "binary: $bin"
Write-Host ""

# --- the terminal's own environment -------------------------------------------
$envKeys = @(
    'TERM', 'COLORTERM', 'TERM_PROGRAM', 'TERM_PROGRAM_VERSION', 'WT_SESSION',
    'WT_PROFILE_ID', 'WEZTERM_EXECUTABLE', 'WEZTERM_PANE', 'WEZTERM_UNIX_SOCKET',
    'VTE_VERSION', 'NO_COLOR', 'LANG', 'LC_ALL', 'LC_CTYPE', 'SHELL', 'COMSPEC',
    'THEGN_CHANNEL', 'THEGN_DIR', 'XDG_CONFIG_HOME', 'XDG_STATE_HOME'
)
$envLines = foreach ($k in $envKeys) {
    $v = [Environment]::GetEnvironmentVariable($k)
    "{0,-22} {1}" -f $k, $(if ($null -eq $v) { '(unset)' } else { "'$v'" })
}
$envLines | Set-Content -Path (Join-Path $OutDir 'env.txt') -Encoding utf8
$envLines | Write-Host

# NO_COLOR is inherited, and inheriting it is easy to do by accident: any
# terminal launched from a shell that sets it (Claude Code does, for every
# process it spawns) starts with it, and thegn then correctly renders
# monochrome. That reads exactly like a broken theme. Say so, loudly, and drop
# it for the capture — a debug harness that silently reproduces the wrong
# environment is worse than none.
if ($env:NO_COLOR) {
    Write-Host ""
    Write-Host "NOTE: NO_COLOR=$($env:NO_COLOR) was inherited by this terminal." -ForegroundColor Yellow
    Write-Host "      thegn honours it and renders monochrome - that is the setting, not a bug." -ForegroundColor Yellow
    Write-Host "      Unsetting it for this capture so the rest of the report is representative." -ForegroundColor Yellow
    Remove-Item env:NO_COLOR
}

# --- is stdout a console, and what can it do? ---------------------------------
# The same question `platform::console_caps` asks. Answering it here separately
# proves whether thegn's view matches the terminal's reality.
$consoleProbe = @'
using System;
using System.Runtime.InteropServices;
public static class ConProbe {
    [DllImport("kernel32.dll", SetLastError=true)] public static extern IntPtr GetStdHandle(int n);
    [DllImport("kernel32.dll", SetLastError=true)] public static extern bool GetConsoleMode(IntPtr h, out uint m);
    [DllImport("kernel32.dll", SetLastError=true)] public static extern uint GetConsoleOutputCP();
    public static string Report() {
        IntPtr h = GetStdHandle(-11);
        uint mode;
        if (!GetConsoleMode(h, out mode)) return "stdout is NOT a console (redirected?)";
        bool vt = (mode & 0x0004) != 0;
        return string.Format("console mode = 0x{0:X}  VT_PROCESSING={1}  output_cp={2}", mode, vt, GetConsoleOutputCP());
    }
}
'@
try {
    Add-Type -TypeDefinition $consoleProbe -Language CSharp -ErrorAction Stop
    $con = [ConProbe]::Report()
} catch {
    $con = "console probe failed: $($_.Exception.Message)"
}
$con | Set-Content -Path (Join-Path $OutDir 'console.txt') -Encoding utf8
Write-Host ""
Write-Host $con -ForegroundColor Yellow

# --- what thegn makes of it ---------------------------------------------------
# NB: `doctor` must NOT be redirected, or stdout stops being a console and the
# capability answer changes. Tee it instead.
Write-Host ""
Write-Host "running: thegn doctor" -ForegroundColor Cyan
& $bin doctor | Tee-Object -FilePath (Join-Path $OutDir 'doctor.txt')

if ($NoRun) {
    Write-Host ""
    Write-Host "capture (no compositor run): $OutDir" -ForegroundColor Green
    exit 0
}

# --- the compositor, with debug logging ---------------------------------------
# THEGN_LOG writes to $XDG_STATE_HOME/thegn/logs/thegn.log; point it somewhere
# per-capture so the file only holds this session.
$env:THEGN_LOG = 'debug'
$env:THEGN_PERF = '1'
Write-Host ""
Write-Host "launching the compositor with THEGN_LOG=debug." -ForegroundColor Cyan
Write-Host "quit it normally (q) - the log is collected afterwards." -ForegroundColor Cyan
Write-Host ""
& $bin
$rc = $LASTEXITCODE

$log = Join-Path $env:LOCALAPPDATA 'thegn\logs\thegn.log'
if (Test-Path $log) {
    Copy-Item $log (Join-Path $OutDir 'thegn.log') -Force
}
Write-Host ""
Write-Host "compositor exited with $rc" -ForegroundColor Cyan
Write-Host "capture: $OutDir" -ForegroundColor Green
Get-ChildItem $OutDir | Select-Object Name, Length | Format-Table | Out-Host
