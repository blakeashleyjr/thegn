# scripts/spikes/lib.ps1 - shared helpers for the Windows wave-0 spike scripts.
# Dot-source it (. "$PSScriptRoot\lib.ps1"); do not run it. Windows PowerShell 5.1
# and PowerShell 7 compatible. Same contract as lib.sh: the script takes -OutDir, writes
# one evidence file <OutDir>\<ITEM>.evidence.txt (raw captures under <OutDir>\<ITEM>\ only
# where a procedure asks for fixtures), exits non-zero on a failed precondition, cleans
# up in a finally block (Sp-RunCleanups) and prints the decision-rule verdict when it can
# be computed. Secrets come from named environment variables only.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:SpItem = ''
$script:SpOut = ''
$script:SpEvid = ''
$script:SpCleanups = New-Object System.Collections.ArrayList

function Sp-Die([string]$Message) {
    [Console]::Error.WriteLine("error: $Message")
    exit 2
}

function Sp-Init([string]$Item, [string]$OutDir) {
    # Windows PowerShell 5.1 has no $IsWindows (and is always Windows): test Edition first.
    if ($PSVersionTable.PSEdition -eq 'Core' -and -not $IsWindows) { Sp-Die 'Windows only' }
    $script:SpItem = $Item
    $script:SpOut = $OutDir
    New-Item -ItemType Directory -Force -Path (Join-Path $OutDir $Item) | Out-Null
    $script:SpEvid = Join-Path $OutDir "$Item.evidence.txt"
    Set-Content -Path $script:SpEvid -Value "# $Item evidence" -Encoding UTF8
    Sp-Kv 'date_utc' ((Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ'))
    Sp-Kv 'host' ((Get-CimInstance Win32_OperatingSystem).Caption)
    $reg = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
    Sp-Kv 'windows_build_ubr' ("{0}.{1} ({2})" -f $reg.CurrentBuild, $reg.UBR, $reg.DisplayVersion)
    Sp-Kv 'powershell' $PSVersionTable.PSVersion.ToString()
}

function Sp-Redact([string]$Text) {
    if ($null -eq $Text) { return '' }
    $t = $Text.Replace($env:USERPROFILE, '<USERPROFILE>')
    return $t.Replace($env:USERNAME, '<USER>')
}

function Sp-Note([string]$Text) { Add-Content -Path $script:SpEvid -Value $Text -Encoding UTF8 }
function Sp-Kv([string]$Key, $Value) { Sp-Note ("{0}: {1}" -f $Key, (Sp-Redact ([string]$Value))) }

function Sp-OnExit([scriptblock]$Block) { [void]$script:SpCleanups.Add($Block) }

function Sp-RunCleanups {
    for ($i = $script:SpCleanups.Count - 1; $i -ge 0; $i--) {
        try { & $script:SpCleanups[$i] } catch { }  # best-effort: cleanup never masks the exit status
    }
}

function Sp-Need([string[]]$Tools) {
    foreach ($t in $Tools) {
        if (-not (Get-Command $t -ErrorAction SilentlyContinue)) { Sp-Die "missing required tool: $t" }
    }
}

# Sp-Cap NAME SECONDS FILE ARGSTRING : run FILE, capturing RAW stdout/stderr bytes to
# <OutDir>\<ITEM>\NAME.out / NAME.err (no PowerShell re-encoding), with a timeout.
# Records argv, rc and a redacted head in the evidence file; returns the exit code
# (124 on timeout) and leaves the raw files for callers to parse.
function Sp-Cap([string]$Name, [int]$Secs, [string]$File, [string]$ArgString = '') {
    $base = Join-Path (Join-Path $script:SpOut $script:SpItem) $Name
    $p = Start-Process -FilePath $File -ArgumentList $ArgString -NoNewWindow -PassThru `
        -RedirectStandardOutput "$base.out" -RedirectStandardError "$base.err"
    $null = $p.Handle  # keeps ExitCode readable after WaitForExit
    if (-not $p.WaitForExit($Secs * 1000)) {
        try { $p.Kill() } catch { }
        $p.WaitForExit()
        $rc = 124
    } else { $rc = $p.ExitCode }
    Sp-Note ''
    Sp-Note "## $Name"
    Sp-Note "argv: $File $ArgString"
    Sp-Note "rc: $rc"
    foreach ($ext in 'out', 'err') {
        Sp-Note "--- std$ext (head, decoded as UTF-8 for display only; raw bytes are in $Name.$ext)"
        $bytes = [System.IO.File]::ReadAllBytes("$base.$ext")
        $n = [Math]::Min($bytes.Length, 2000)
        Sp-Note (Sp-Redact ([System.Text.Encoding]::UTF8.GetString($bytes, 0, $n)))
    }
    return $rc
}

function Sp-Verdict([string]$Rule, [string]$Result) {
    Sp-Note ''
    Sp-Note '## VERDICT'
    Sp-Note "rule: $Rule"
    Sp-Note "result: $Result"
    Write-Output "VERDICT ($script:SpItem): $Result"
}
