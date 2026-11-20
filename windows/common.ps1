# Shared helpers for install.ps1 / uninstall.ps1 / check.ps1.
# Dot-source this file: . "$PSScriptRoot\common.ps1"

Set-StrictMode -Version 3.0

# Must match STATUS_VENDOR in ramfence-efi/src/main.rs.
$script:RamFenceVendorGuid = '{0B0C86EE-7039-477B-9A53-E39345EF90D5}'
$script:RamFenceVariable = 'RamFenceStatus'
$script:RamFenceEntryName = 'RAMFence'
$script:RamFenceEspDir = '\EFI\ramfence'
$script:RamFenceEfiPath = '\EFI\ramfence\ramfence.efi'
# Program Files is writable only by administrators, which matters because
# check.ps1 runs elevated from a scheduled task.
$script:RamFenceInstallDir = Join-Path $env:ProgramFiles 'RAMFence'
$script:RamFenceStateFile = Join-Path $script:RamFenceInstallDir 'state.json'
$script:RamFenceLogFile = Join-Path $script:RamFenceInstallDir 'check.log'
$script:RamFenceCfgFile = Join-Path $script:RamFenceInstallDir 'ramfence.cfg'
$script:RamFenceTaskName = 'RAMFence boot check'

function Assert-Admin {
    $p = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    if (-not $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'This script must be run as Administrator.'
    }
}

function Assert-UefiBoot {
    # Windows sets %firmware_type% to UEFI, Legacy or Unknown.
    if ($env:firmware_type -ne 'UEFI') {
        throw "Windows was not booted in UEFI mode (firmware_type=$env:firmware_type); ramfence.efi cannot be used."
    }
}

# --- bcdedit -----------------------------------------------------------------

function Invoke-Bcdedit {
    param([Parameter(ValueFromRemainingArguments)][string[]]$Arguments)
    $out = & bcdedit.exe @Arguments 2>&1 | ForEach-Object { "$_" }
    if ($LASTEXITCODE -ne 0) {
        throw "bcdedit $($Arguments -join ' ') failed (exit $LASTEXITCODE): $($out -join ' ')"
    }
    $out
}

$script:GuidPattern = '\{[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\}'

# Firmware boot order as GUIDs. Element names such as "displayorder" are not
# localized by bcdedit; headings are, so only element names are matched.
function Get-FirmwareBootOrder {
    $lines = Invoke-Bcdedit /enum '{fwbootmgr}' /v
    ConvertFrom-DisplayOrder $lines
}

function ConvertFrom-DisplayOrder([string[]]$Lines) {
    $order = @()
    $inOrder = $false
    foreach ($line in $Lines) {
        if ($line -match '^displayorder\s+(.*)$') {
            $inOrder = $true
            $rest = $Matches[1]
        } elseif ($inOrder -and $line -match '^\s+(\{.*)$') {
            $rest = $Matches[1]
        } else {
            $inOrder = $false
            continue
        }
        if ($rest -match $script:GuidPattern) { $order += $Matches[0].ToLowerInvariant() }
    }
    , $order
}

function Test-BcdEntry([string]$Id) {
    & bcdedit.exe /enum $Id 2>&1 | Out-Null
    $LASTEXITCODE -eq 0
}

function Set-RamFenceFirst([string]$Id) {
    Invoke-Bcdedit /set '{fwbootmgr}' displayorder $Id /addfirst | Out-Null
}

# --- ESP -----------------------------------------------------------------------

function Get-FreeDriveLetter {
    $used = (Get-PSDrive -PSProvider FileSystem).Name
    foreach ($c in [char[]]('ZYXWVUTSRQPONMLKJIHG')) {
        if ($used -notcontains [string]$c) { return "${c}:" }
    }
    throw 'No free drive letter to mount the EFI System Partition.'
}

# Mounts the ESP, runs $Action with the drive root (e.g. 'Z:\'), unmounts.
function Invoke-WithEsp([scriptblock]$Action) {
    $letter = Get-FreeDriveLetter
    $out = & mountvol.exe $letter /S 2>&1
    if ($LASTEXITCODE -ne 0) { throw "mountvol $letter /S failed: $out" }
    try {
        & $Action "$letter\"
    } finally {
        & mountvol.exe $letter /D 2>&1 | Out-Null
    }
}

# --- RamFenceStatus variable -------------------------------------------------------

$script:FirmwareApiLoaded = $false
function Initialize-FirmwareApi {
    if ($script:FirmwareApiLoaded) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class RamFenceFirmware {
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern uint GetFirmwareEnvironmentVariableExW(
        string name, string guid, byte[] buffer, uint size, out uint attributes);

    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);

    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool LookupPrivilegeValueW(string system, string name, out long luid);

    [StructLayout(LayoutKind.Sequential, Pack = 4)]
    struct TokenPrivileges { public uint Count; public long Luid; public uint Attributes; }

    [DllImport("advapi32.dll", SetLastError = true)]
    static extern bool AdjustTokenPrivileges(IntPtr token, bool disableAll,
        ref TokenPrivileges state, uint len, IntPtr prev, IntPtr retLen);

    [DllImport("kernel32.dll")]
    static extern IntPtr GetCurrentProcess();

    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr h);

    // Returns 0 on success, otherwise a Win32 error code.
    public static int EnableSystemEnvironmentPrivilege() {
        IntPtr token;
        if (!OpenProcessToken(GetCurrentProcess(), 0x0020 | 0x0008, out token))
            return Marshal.GetLastWin32Error();
        try {
            TokenPrivileges tp = new TokenPrivileges();
            tp.Count = 1;
            tp.Attributes = 2; // SE_PRIVILEGE_ENABLED
            if (!LookupPrivilegeValueW(null, "SeSystemEnvironmentPrivilege", out tp.Luid))
                return Marshal.GetLastWin32Error();
            AdjustTokenPrivileges(token, false, ref tp, 0, IntPtr.Zero, IntPtr.Zero);
            return Marshal.GetLastWin32Error(); // 1300 = not all assigned
        } finally { CloseHandle(token); }
    }

    // data == null with error code on failure.
    public static byte[] Read(string name, string guid, out int error) {
        byte[] buf = new byte[4096];
        uint attrs;
        uint n = GetFirmwareEnvironmentVariableExW(name, guid, buf, (uint)buf.Length, out attrs);
        if (n == 0) { error = Marshal.GetLastWin32Error(); return null; }
        error = 0;
        byte[] data = new byte[n];
        Array.Copy(buf, data, n);
        return data;
    }
}
'@
    $script:FirmwareApiLoaded = $true
}

# Returns @{ Present = $bool; Raw = 'text'; Fields = @{...} }.
# Throws on errors other than "variable not found".
function Get-RamFenceStatus {
    Initialize-FirmwareApi
    $e = [RamFenceFirmware]::EnableSystemEnvironmentPrivilege()
    if ($e -ne 0) { throw "Cannot enable SeSystemEnvironmentPrivilege (Win32 error $e). Run as Administrator." }
    $err = 0
    $data = [RamFenceFirmware]::Read($script:RamFenceVariable, $script:RamFenceVendorGuid, [ref]$err)
    if ($null -eq $data) {
        if ($err -eq 203) { return @{ Present = $false; Raw = ''; Fields = @{} } }  # ERROR_ENVVAR_NOT_FOUND
        throw "Reading firmware variable $($script:RamFenceVariable) failed (Win32 error $err)."
    }
    $raw = [Text.Encoding]::ASCII.GetString($data)
    @{ Present = $true; Raw = $raw; Fields = (ConvertFrom-RamFenceStatus $raw) }
}

# "version=1;ok=1;ranges=...;pages=N;type=unusable[;error=...]" -> hashtable
function ConvertFrom-RamFenceStatus([string]$Text) {
    $h = @{}
    foreach ($part in $Text.TrimEnd([char]0).Split(';')) {
        $i = $part.IndexOf('=')
        if ($i -gt 0) { $h[$part.Substring(0, $i)] = $part.Substring($i + 1) }
    }
    $h
}

# --- ranges ----------------------------------------------------------------------
# A range is a two-element array @([UInt64]start, [UInt64]end), end exclusive.

function ConvertFrom-HexAddress([string]$Text) {
    [UInt64]::Parse(($Text.Substring(2) -replace '_', ''), 'AllowHexSpecifier')
}

# Ranges of a protect-mode ramfence.cfg, aligned outward to 4 KiB pages like
# ramfence.efi does. Empty for scan-only/auto mode or a missing file.
function Get-RamFenceConfigRanges([string]$Path) {
    $list = @()
    if (-not (Test-Path $Path)) { return , $list }
    $mode = 'protect'
    $page = [UInt64]4096
    foreach ($raw in (Get-Content $Path)) {
        $parts = @((($raw -replace '#.*$', '').Trim()) -split '\s+')
        if ($parts[0] -ceq 'mode' -and $parts.Count -eq 2) { $mode = $parts[1] }
        if ($parts[0] -ceq 'range' -and $parts.Count -eq 3) {
            $s = ConvertFrom-HexAddress $parts[1]
            $e = ConvertFrom-HexAddress $parts[2]
            $s = [UInt64]($s - ($s % $page))
            if ($e % $page) { $e = [UInt64]($e + ($page - ($e % $page))) }
            $list += , @($s, $e)
        }
    }
    if ($mode -ne 'protect') { return , @() }
    , $list
}

# The "ranges" field of RamFenceStatus ("0xA-0xB,0xC-0xD") as ranges.
function ConvertFrom-StatusRanges([string]$Text) {
    $list = @()
    foreach ($part in ($Text -split ',')) {
        if (-not $part) { continue }
        $ab = $part -split '-'
        $list += , @((ConvertFrom-HexAddress $ab[0]), (ConvertFrom-HexAddress $ab[1]))
    }
    , $list
}

# Parts of the ranges in $Needed that no range in $Have covers, as
# "0xA-0xB" strings.
function Get-UncoveredRanges($Needed, $Have) {
    $out = @()
    foreach ($n in $Needed) {
        $cur = [UInt64]$n[0]
        $end = [UInt64]$n[1]
        while ($cur -lt $end) {
            $cover = @($Have | Where-Object { $_[0] -le $cur -and $cur -lt $_[1] })
            if ($cover.Count) {
                foreach ($c in $cover) { if ([UInt64]$c[1] -gt $cur) { $cur = [UInt64]$c[1] } }
                continue
            }
            $next = @($Have | Where-Object { $_[0] -gt $cur -and $_[0] -lt $end } | ForEach-Object { $_[0] } |
                Sort-Object | Select-Object -First 1)
            $gapEnd = if ($next.Count) { [UInt64]$next[0] } else { $end }
            $out += ('0x{0:x}-0x{1:x}' -f $cur, $gapEnd)
            $cur = $gapEnd
        }
    }
    , $out
}

# --- state -----------------------------------------------------------------------

function Get-RamFenceState {
    if (-not (Test-Path $script:RamFenceStateFile)) { return $null }
    Get-Content -Raw $script:RamFenceStateFile | ConvertFrom-Json
}

function Save-RamFenceState([string]$Id) {
    @{ EntryId = $Id; InstalledAt = (Get-Date).ToString('s') } |
        ConvertTo-Json | Set-Content -Encoding UTF8 $script:RamFenceStateFile
}

# --- config validation -----------------------------------------------------------

# Light check mirroring ramfence-core's parser, so obvious mistakes are caught
# before rebooting. The real parser in ramfence.efi remains authoritative.
function Test-RamFenceConfig([string]$Path) {
    $errors = @()
    $ranges = 0
    $mode = 'protect'
    $n = 0
    foreach ($raw in (Get-Content $Path)) {
        $n++
        $line = ($raw -replace '#.*$', '').Trim()
        if (-not $line) { continue }
        $parts = $line -split '\s+'
        switch ($parts[0]) {
            'range' {
                if ($parts.Count -ne 3 -or $parts[1] -notmatch '^0[xX][0-9a-fA-F_]+$' -or $parts[2] -notmatch '^0[xX][0-9a-fA-F_]+$') {
                    $errors += "line ${n}: expected 'range 0xSTART 0xEND'"
                } else {
                    $s = [UInt64]::Parse(($parts[1].Substring(2) -replace '_', ''), 'AllowHexSpecifier')
                    $e = [UInt64]::Parse(($parts[2].Substring(2) -replace '_', ''), 'AllowHexSpecifier')
                    if ($s -ge $e) { $errors += "line ${n}: start must be below end" } else { $ranges++ }
                }
            }
            'mode' {
                if ($parts.Count -ne 2 -or $parts[1] -notin 'protect', 'scan-only', 'auto') { $errors += "line ${n}: mode must be protect, scan-only or auto" }
                else { $mode = $parts[1] }
            }
            'memory_type' { if ($parts.Count -ne 2 -or $parts[1] -notin 'unusable', 'reserved') { $errors += "line ${n}: memory_type must be unusable or reserved" } }
            'on_error' { if ($parts.Count -ne 2 -or $parts[1] -notin 'prompt', 'boot', 'halt') { $errors += "line ${n}: on_error must be prompt, boot or halt" } }
            'timeout' { if ($parts.Count -ne 2 -or $parts[1] -notmatch '^\d+$') { $errors += "line ${n}: timeout must be a number of seconds" } }
            'chainload' { if (-not $line.Substring(9).Trim().StartsWith('\')) { $errors += "line ${n}: chainload path must start with '\'" } }
            default { $errors += "line ${n}: unknown key '$($parts[0])'" }
        }
    }
    if ($mode -eq 'protect' -and $ranges -eq 0) { $errors += "no 'range' line" }
    if ($mode -ne 'protect' -and $ranges -gt 0) { $errors += "'range' lines are not allowed with 'mode $mode'" }
    , $errors
}
