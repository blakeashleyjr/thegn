<#
.SYNOPSIS
  Standalone (non-Nix) install of the native compositor host on Windows.

.DESCRIPTION
  The Windows counterpart to install.sh. Per-user, no administrator rights
  needed - everything lands under %LOCALAPPDATA% and the user PATH.

  Installs:
    thegn.exe  - the native host (compositor + CLI verbs)
    tg.cmd     - short alias, forwards every argument to thegn.exe
    tg-tui.cmd - compat alias for `tg` (matches the unix install)
  Plus a Start Menu shortcut that opens thegn inside Windows Terminal.

  There is deliberately no `--standalone` alias as on unix: that one opens a
  dedicated alacritty window, whereas on Windows the Start Menu shortcut
  already launches a dedicated Windows Terminal window.

.PARAMETER BinDir
  Install directory. Defaults to %LOCALAPPDATA%\Programs\thegn.

.PARAMETER DryRun
  Print the install plan without building or changing anything.

.PARAMETER NoBuild
  Skip `cargo build` and install the release binary that is already in
  target\release (useful for a CI artifact you dropped in yourself).

.EXAMPLE
  .\install.ps1
.EXAMPLE
  .\install.ps1 -DryRun
.EXAMPLE
  .\install.ps1 -BinDir C:\tools\thegn -NoBuild
#>
[CmdletBinding()]
param(
    [string]$BinDir,
    [switch]$DryRun,
    [switch]$NoBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $BinDir) { $BinDir = Join-Path $env:LOCALAPPDATA 'Programs\thegn' }

$exeName    = 'thegn.exe'
$releaseBin = Join-Path $here "target\release\$exeName"
$targetExe  = Join-Path $BinDir $exeName
$tgCmd      = Join-Path $BinDir 'tg.cmd'
$tgTuiCmd   = Join-Path $BinDir 'tg-tui.cmd'
$startMenu  = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
$shortcut   = Join-Path $startMenu 'thegn.lnk'

function Write-Step($msg) { Write-Host $msg }
function Write-Warn($msg) { Write-Host "warning: $msg" -ForegroundColor Yellow }

# --- preflight ---------------------------------------------------------------
# thegn refuses to start under legacy conhost.exe, so an install without
# Windows Terminal produces a binary the user cannot actually launch
# interactively. Warn loudly rather than fail - the CLI verbs still work.
$wt = Get-Command wt.exe -ErrorAction SilentlyContinue
if (-not $wt) {
    Write-Warn 'Windows Terminal (wt.exe) not found. thegn refuses to start under legacy'
    Write-Warn 'conhost.exe - install it from https://aka.ms/terminal before running the TUI.'
    Write-Warn 'CLI verbs (thegn pr / diff / list / doctor) work regardless.'
}
if (-not (Get-Command git.exe -ErrorAction SilentlyContinue)) {
    Write-Warn 'git not found on PATH - git reads will fail. Install Git for Windows.'
}

if ($DryRun) {
    Write-Step 'install plan (dry run):'
    Write-Step "  build           -> cargo build --release -p thegn-host"
    Write-Step "  $targetExe   <- $releaseBin"
    Write-Step "  $tgCmd        -> thegn.exe (current terminal, forwards args)"
    Write-Step "  $tgTuiCmd    -> thegn.exe (compat alias)"
    Write-Step "  PATH (user)     += $BinDir"
    Write-Step "  $shortcut -> Start Menu entry (opens in Windows Terminal)"
    exit 0
}

# --- build -------------------------------------------------------------------
if (-not $NoBuild) {
    if (-not (Get-Command cargo.exe -ErrorAction SilentlyContinue)) {
        throw "cargo not found on PATH. Install rustup (https://rustup.rs) plus the VS Build Tools C++ workload, or re-run with -NoBuild."
    }
    Write-Step 'building release binary (this takes a while on a cold cache)...'
    Push-Location $here
    try {
        & cargo build --release -p thegn-host
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
    } finally {
        Pop-Location
    }
}

if (-not (Test-Path $releaseBin)) {
    throw "$releaseBin is missing - nothing to install. Run without -NoBuild, or drop a release thegn.exe there."
}

# --- install -----------------------------------------------------------------
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

# Copy rather than symlink so the install survives the source tree it came from
# (and needs no Developer Mode, which symlink creation would). Re-run this
# script to pick up a rebuild.
try {
    Copy-Item -Path $releaseBin -Destination $targetExe -Force
} catch {
    throw "could not write $targetExe - is a thegn instance running? Close it and retry. ($_)"
}
Write-Step "installed $targetExe"

# `%~dp0` keeps the shims pointing at their sibling exe wherever BinDir moves.
# `@` suppresses echo; `%*` forwards every argument verbatim.
Set-Content -Path $tgCmd -Encoding ascii -Value @(
    '@echo off',
    'rem thegn short alias - forwards all arguments to the native host.',
    '"%~dp0thegn.exe" %*'
)
Set-Content -Path $tgTuiCmd -Encoding ascii -Value @(
    '@echo off',
    'rem compat alias for `tg` (matches the unix install layout).',
    '"%~dp0thegn.exe" %*'
)
Write-Step "installed $tgCmd and $tgTuiCmd"

# --- PATH --------------------------------------------------------------------
# User scope: no admin, and it survives reboots. Idempotent - re-running the
# installer must not append a duplicate entry.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $userPath) { $userPath = '' }
$already = $userPath.Split(';') | Where-Object { $_.TrimEnd('\') -ieq $BinDir.TrimEnd('\') }
if ($already) {
    Write-Step "PATH already contains $BinDir"
} else {
    $newPath = if ($userPath.TrimEnd(';')) { $userPath.TrimEnd(';') + ';' + $BinDir } else { $BinDir }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Step "added $BinDir to the user PATH (open a new terminal to pick it up)"
}

# --- Start Menu shortcut -----------------------------------------------------
# Launch through Windows Terminal: a bare .lnk to thegn.exe would open in
# conhost, which thegn refuses. `wt.exe` is resolved at click time via PATH so
# the shortcut keeps working across Windows Terminal updates (its install path
# carries a version).
if ($wt) {
    try {
        New-Item -ItemType Directory -Force -Path $startMenu | Out-Null
        $shell = New-Object -ComObject WScript.Shell
        $lnk = $shell.CreateShortcut($shortcut)
        $lnk.TargetPath       = $wt.Source
        $lnk.Arguments        = "new-tab --title thegn `"$targetExe`""
        $lnk.WorkingDirectory = $env:USERPROFILE
        $lnk.IconLocation     = $targetExe
        $lnk.Description      = 'thegn - terminal-native git worktree IDE'
        $lnk.Save()
        Write-Step "wrote Start Menu entry: $shortcut"
    } catch {
        Write-Warn "could not create the Start Menu shortcut: $_"
    }
} else {
    Write-Warn 'skipped the Start Menu shortcut (needs Windows Terminal).'
}

# --- summary -----------------------------------------------------------------
Write-Host ''
Write-Step 'done:'
Write-Step "  $targetExe    <- copy of $releaseBin (re-run install.ps1 after a rebuild)"
Write-Step "  $tgCmd         -> short alias"
Write-Step "  $tgTuiCmd     -> compat alias"
if ($wt) { Write-Step "  $shortcut  -> Start Menu entry" }
Write-Host ''
Write-Step 'State lives in %APPDATA%\thegn (config) and %LOCALAPPDATA%\thegn (DB, logs).'
Write-Step 'Verify with:  thegn doctor'
