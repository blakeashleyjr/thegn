# WS06-P4-00 - PSEC / MXC spike: the scriptable PRECONDITION part only.
# Run ON each Windows 11 build under test (26100.9278+ AND 26200.9278+), once per build:
#   pwsh -File scripts\spikes\ws06-p4-00.ps1 -OutDir C:\spike-out
# What it does (read-only; no process is started under PSEC):
#   - records Windows build + UBR and compares with the supported-build table,
#   - loads processmodel.dll from System32 and records which of the four PSEC
#     exports exist (LoadLibraryEx + GetProcAddress by NAME; no call signatures needed),
#   - records the Rust toolchain and, if given, the MXC source revision.
# What it does NOT do, and why: X1.2-X1.5, X2, X3, X4, X5 call undocumented exports or
# the pinned mxc-sdk. Their signatures and the FlatBuffer policy come from MXC's own
# source (secenv.rs, ProcessSecurityEnvironment.fbs, base_container_runner.rs), which
# the spike author reads at a pinned revision. No code is written here from memory: an
# unreviewed guess at an undocumented ABI would produce misleading evidence. Write that
# spike code under tools\spikes\psec\ on an UNMERGED branch as the procedure page says.
param(
    [Parameter(Mandatory = $true)][string]$OutDir,
    [string]$MxcRevision = ''
)
. "$PSScriptRoot\lib.ps1"
$item = 'WS06-P4-00'
Sp-Init $item $OutDir
try {
    $reg = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
    $build = [int]$reg.CurrentBuild
    $ubr = [int]$reg.UBR
    $min = @{ 26100 = 9278; 26200 = 9278 }
    Sp-Kv 'build' $build
    Sp-Kv 'ubr' $ubr
    Sp-Kv 'display_version' $reg.DisplayVersion
    Sp-Kv 'edition' $reg.EditionID
    Sp-Kv 'mxc_revision' $(if ($MxcRevision) { $MxcRevision } else { 'NOT PROVIDED (record the pinned MXC / Codex source revision)' })
    Sp-Kv 'rust_toolchain' $(if (Get-Command rustc -ErrorAction SilentlyContinue) { (rustc -V) } else { 'rustc not installed' })

    $supported = $false
    if ($min.ContainsKey($build)) { $supported = ($ubr -ge $min[$build]) }
    Sp-Kv 'supported_build' $(if ($supported) { 'YES' } else { "NO (need 26100.9278+ or 26200.9278+; other builds refuse, never downgrade)" })

    Add-Type -Namespace TgSpike -Name Native -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError=true, CharSet=System.Runtime.InteropServices.CharSet.Unicode)]
public static extern System.IntPtr LoadLibraryExW(string lpFileName, System.IntPtr hFile, uint dwFlags);
[System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError=true, CharSet=System.Runtime.InteropServices.CharSet.Ansi)]
public static extern System.IntPtr GetProcAddress(System.IntPtr hModule, string lpProcName);
[System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError=true)]
public static extern bool FreeLibrary(System.IntPtr hModule);
'@
    $LOAD_LIBRARY_SEARCH_SYSTEM32 = 0x00000800
    $h = [TgSpike.Native]::LoadLibraryExW('processmodel.dll', [IntPtr]::Zero, $LOAD_LIBRARY_SEARCH_SYSTEM32)
    $present = @{}
    if ($h -eq [IntPtr]::Zero) {
        Sp-Kv 'processmodel_dll' ("NOT LOADABLE (Win32 error {0})" -f [Runtime.InteropServices.Marshal]::GetLastWin32Error())
    } else {
        Sp-OnExit { [void][TgSpike.Native]::FreeLibrary($h) }
        Sp-Kv 'processmodel_dll' 'loaded from System32'
        foreach ($n in 'CreateProcessSecurityEnvironment', 'QueryProcessSecurityEnvironmentSupport', 'IsProcessSecurityEnvironmentVersionSupported', 'CloseProcessSecurityEnvironment') {
            $p = [TgSpike.Native]::GetProcAddress($h, $n)
            $present[$n] = ($p -ne [IntPtr]::Zero)
            Sp-Kv "export_$n" $(if ($present[$n]) { 'present' } else { 'ABSENT' })
        }
    }
    Sp-Note ''
    Sp-Note 'NEXT (manual / spike code, per the procedure page): X1.2 support queries, X1.3 empty deny-all create/close HRESULTs, X1.4 whoami under PSEC with a deny canary, X1.5 the F1-F4/N1-N4/P1-P3 subset, X2 peer identity, X3 userinfo, X4 mxc-sdk, X5 weight. Repeat on BOTH builds.'
    $allExports = ($present.Count -eq 4) -and (@($present.Values | Where-Object { -not $_ }).Count -eq 0)
    Sp-Verdict 'precondition only: supported build at the stated UBR and all four processmodel.dll exports present; the helper-strategy / peer-string / userinfo / ConPTY decisions come from X1-X5 on BOTH builds' ("build $build.$ubr supported=$supported; all four exports present=$allExports (decision not computable by this script)")
} finally {
    Sp-RunCleanups
}
