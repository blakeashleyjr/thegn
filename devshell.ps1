<#
.SYNOPSIS
  The Windows analogue of `nix develop` - an ephemeral dev environment.

.DESCRIPTION
  Pairs with dev/windows.dsc.yaml. That manifest declares what a package
  manager installs (git, rustup, MSVC Build Tools, Windows Terminal); this
  script assembles the *environment* those pieces need, which is the half a
  package manager cannot express:

    * PATH   - ~/.cargo/bin, plus Git for Windows' `usr\bin` for the POSIX
               userland (sh, cat, printf, sha256sum, stty, ...) that the test
               fixtures spawn. Without it, ~40 tests fail with
               "program not found".
    * env    - the same knobs flake.nix exports: a CARGO_BUILD_JOBS cap that
               leaves headroom, and sccache wiring when sccache is present.
    * tools  - the cargo-installed dev tools (cargo-nextest, cargo-llvm-cov).

  SESSION SCOPE ONLY. Every change lands in this process's environment; nothing
  is ever written to the User or Machine environment. That is deliberate, not
  laziness: Git's `usr\bin` contains `find.exe` and `sort.exe`, which shadow the
  Windows built-ins that other tooling depends on. It is safe for a dev shell
  and actively harmful machine-wide.

  Note the Rust toolchain itself is NOT configured here - rust-toolchain.toml
  declares the channel and components, and rustup applies it on first use.

.PARAMETER Command
  Run one command inside the environment and exit, like `nix develop --command`.
  Without it you get an interactive nested shell; `exit` returns you to where
  you started.

.PARAMETER Check
  Report what resolved and exit non-zero if anything required is missing.
  Changes nothing. Use it in CI or to debug a broken box.

.PARAMETER NoToolInstall
  Skip installing the cargo dev tools (they are a one-time compile each).

.EXAMPLE
  .\devshell.ps1
.EXAMPLE
  .\devshell.ps1 -Command "cargo test --workspace"
.EXAMPLE
  .\devshell.ps1 -Check
#>
[CmdletBinding()]
param(
    [string]$Command,
    [switch]$Check,
    [switch]$NoToolInstall
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$here = Split-Path -Parent $MyInvocation.MyCommand.Path

# Cargo-installed dev tools. Set a version string to pin one (the closest this
# gets to a lockfile); $null means "latest, with --locked". `just test` uses
# nextest; `just coverage` uses llvm-cov.
$CargoTools = @(
    @{ Bin = 'cargo-nextest';  Crate = 'cargo-nextest';  Version = $null },
    @{ Bin = 'cargo-llvm-cov'; Crate = 'cargo-llvm-cov'; Version = $null }
)

$problems = New-Object System.Collections.Generic.List[string]
function Note($msg) { Write-Host "  $msg" }
function Bad($msg) { $problems.Add($msg) | Out-Null; Write-Host "  $msg" -ForegroundColor Yellow }

Write-Host "thegn dev shell (Windows)" -ForegroundColor Cyan
Write-Host ""

# --- PATH: cargo -------------------------------------------------------------
# rustup installs per-user and updates the User PATH, but a shell opened before
# that (or a service/agent context) will not have picked it up.
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if (Test-Path $cargoBin) {
    if (-not (($env:PATH -split ';') | Where-Object { $_.TrimEnd('\') -ieq $cargoBin.TrimEnd('\') })) {
        $env:PATH = "$cargoBin;$env:PATH"
    }
} else {
    Bad "rustup not found ($cargoBin). Run: winget configure dev/windows.dsc.yaml"
}

# --- PATH: POSIX userland from Git for Windows -------------------------------
# thegn already hard-requires git, so this is an existing dependency being
# declared rather than a new one.
$posixDir = $null
$git = Get-Command git.exe -ErrorAction SilentlyContinue
if ($git) {
    $gitRoot = Split-Path (Split-Path $git.Source -Parent) -Parent
    foreach ($cand in @((Join-Path $gitRoot 'usr\bin'), (Join-Path $gitRoot 'bin'))) {
        if (Test-Path (Join-Path $cand 'sh.exe')) { $posixDir = $cand; break }
    }
    if ($posixDir) {
        if (-not (($env:PATH -split ';') | Where-Object { $_.TrimEnd('\') -ieq $posixDir.TrimEnd('\') })) {
            $env:PATH = "$posixDir;$env:PATH"
        }
    } else {
        Bad "git found but no bundled sh.exe under $gitRoot - the POSIX tests will fail."
    }
} else {
    Bad "git not found. Run: winget configure dev/windows.dsc.yaml"
}

# --- MSVC --------------------------------------------------------------------
# Only a presence check: rustc and the cc crate locate the toolchain themselves
# via vswhere, so there is no vcvars to source here.
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vsPath = $null
if (Test-Path $vswhere) {
    $vsPath = & $vswhere -products * -latest -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
}
if (-not $vsPath) {
    Bad "Visual Studio Build Tools (Desktop C++) not found - the C deps cannot link."
}

# --- Build knobs (mirrors flake.nix's shellHook) ------------------------------
# Leave the box usable while a full-workspace build runs; the workspace is
# ~350k LOC and saturating every core makes the machine unpleasant.
if (-not $env:CARGO_BUILD_JOBS) {
    $cores = [Environment]::ProcessorCount
    $env:CARGO_BUILD_JOBS = if ($cores -gt 2) { $cores - 2 } else { 1 }
}
# sccache is optional; wire it only when present. CARGO_INCREMENTAL=0 because
# incremental and sccache are mutually exclusive.
if (Get-Command sccache.exe -ErrorAction SilentlyContinue) {
    $env:RUSTC_WRAPPER = 'sccache'
    $env:CARGO_INCREMENTAL = '0'
}

# --- Report -------------------------------------------------------------------
Write-Host "environment:"
if ($git) { Note ("git             " + (& git --version)) }
$rustc = Get-Command rustc.exe -ErrorAction SilentlyContinue
if ($rustc) { Note ("rustc           " + (& rustc --version)) }
if ($posixDir) {
    $sh = Join-Path $posixDir 'sh.exe'
    Note "posix userland  $posixDir"
    Note ("                " + ((& $sh -c 'echo sh ok') 2>&1))
}
if ($vsPath) { Note "msvc            $vsPath" }
Note "CARGO_BUILD_JOBS $env:CARGO_BUILD_JOBS"
if ($env:RUSTC_WRAPPER) { Note "RUSTC_WRAPPER   $env:RUSTC_WRAPPER" }

if ($problems.Count -gt 0) {
    Write-Host ""
    Write-Host "$($problems.Count) problem(s) above." -ForegroundColor Yellow
    Write-Host "Declared prerequisites: winget configure dev/windows.dsc.yaml" -ForegroundColor Yellow
    if ($Check) { exit 1 }
}
if ($Check) { Write-Host ""; Write-Host "ok" -ForegroundColor Green; exit 0 }

# --- Cargo dev tools ----------------------------------------------------------
if (-not $NoToolInstall) {
    foreach ($t in $CargoTools) {
        if (Get-Command "$($t.Bin).exe" -ErrorAction SilentlyContinue) { continue }
        $ver = if ($t.Version) { @('--version', $t.Version) } else { @() }
        Write-Host "installing $($t.Crate) (one-time compile)..."
        & cargo install $t.Crate --locked @ver
        if ($LASTEXITCODE -ne 0) { Write-Host "  install failed; continuing" -ForegroundColor Yellow }
    }
}

# --- Enter --------------------------------------------------------------------
# The justfile does NOT apply here: its recipes are bash, and nix/devenv are
# unavailable. Bare cargo is the loop (see CONTRIBUTING "Windows (native)").
Write-Host ""
if ($Command) {
    & powershell -NoProfile -Command $Command
    exit $LASTEXITCODE
}
Write-Host "entering dev shell - 'exit' to leave. Try: cargo test --workspace" -ForegroundColor Cyan
& powershell -NoProfile -NoExit -Command "function prompt { 'thegn-dev ' + (Get-Location).Path + '> ' }"
exit $LASTEXITCODE
