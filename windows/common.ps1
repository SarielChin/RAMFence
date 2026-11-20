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
# check.ps1 runs elevated from a scheduled task. The known folder, not
# $env:ProgramFiles, which a user-level environment variable could redirect.
$script:RamFenceInstallDir = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'RAMFence'
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

# Identifiers of firmware entries (bcdedit /enum firmware) whose path is $EfiPath.
function Get-FirmwareEntriesWithPath([string]$EfiPath) {
    $ids = @()
    $id = $null
    foreach ($line in (Invoke-Bcdedit /enum firmware)) {
        if (-not $line.Trim()) { $id = $null; continue }
        if ($line -match '^identifier\s+(\S+)') { $id = $Matches[1] }
        if ($line -match '^path\s+(.+)$' -and $Matches[1].Trim() -eq $EfiPath -and $id -match $script:GuidPattern) {
            $ids += $Matches[0].ToLowerInvariant()
        }
    }
    , $ids
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
        byte[] buf = new byte[65536];
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

# Mirrors ramfence-core's parser (ramfence-core/src/config.rs) rule for rule,
# so a file that passes here is also accepted by ramfence.efi at boot: keys
# and values are case-sensitive, single-value keys may appear once, the file
# must be UTF-8 (optionally with BOM). ramfence.efi remains authoritative.
function Test-RamFenceConfig([string]$Path) {
    $errors = @()
    try {
        $bytes = [IO.File]::ReadAllBytes((Resolve-Path $Path))
        # Strict decoder: throws on bytes that are not valid UTF-8 (e.g. a
        # UTF-16 file saved by Notepad), which ramfence.efi would reject.
        $text = (New-Object Text.UTF8Encoding($false, $true)).GetString($bytes)
    } catch {
        return , @("cannot read the file as UTF-8 text: $($_.Exception.Message)")
    }
    # Ordinal check: a culture-aware StartsWith ignores U+FEFF and always matches.
    if ($text.Length -and [int]$text[0] -eq 0xFEFF) { $text = $text.Substring(1) }

    $ranges = 0
    $mode = 'protect'
    $seen = @{}
    $n = 0
    # Rust's str::lines(): split at \n, drop one trailing \r.
    foreach ($raw in ($text -split "`n")) {
        $n++
        $raw = $raw -replace "`r$", ''
        $i = $raw.IndexOf('#')
        $line = $(if ($i -ge 0) { $raw.Substring(0, $i) } else { $raw }).Trim()
        if (-not $line) { continue }
        $parts = @($line -split '\s+')
        $key = $parts[0]
        $argc = $parts.Count - 1
        if ($key -cin 'mode', 'memory_type', 'on_error', 'timeout', 'chainload') {
            if ($seen.ContainsKey($key)) { $errors += "line ${n}: '$key' appears more than once"; continue }
            $seen[$key] = $true
        }
        switch -CaseSensitive ($key) {
            'range' {
                if ($argc -ne 2) { $errors += "line ${n}: 'range' takes 2 value(s), got $argc"; break }
                $s = ConvertFrom-ConfigAddress $parts[1]
                $e = ConvertFrom-ConfigAddress $parts[2]
                if ($null -eq $s) { $errors += "line ${n}: invalid address '$($parts[1])'" }
                elseif ($null -eq $e) { $errors += "line ${n}: invalid address '$($parts[2])'" }
                elseif ($s -ge $e) { $errors += "line ${n}: start must be below end" }
                elseif ($e -gt [UInt64]::MaxValue - 4095) { $errors += "line ${n}: range end cannot be page-aligned" }
                else { $ranges++ }
            }
            'mode' {
                if ($argc -ne 1 -or $parts[1] -cnotin 'protect', 'scan-only', 'auto') { $errors += "line ${n}: mode must be protect, scan-only or auto" }
                else { $mode = $parts[1] }
            }
            'memory_type' { if ($argc -ne 1 -or $parts[1] -cnotin 'unusable', 'reserved') { $errors += "line ${n}: memory_type must be unusable or reserved" } }
            'on_error' { if ($argc -ne 1 -or $parts[1] -cnotin 'prompt', 'boot', 'halt') { $errors += "line ${n}: on_error must be prompt, boot or halt" } }
            'timeout' {
                $t = [UInt32]0
                if ($argc -ne 1 -or $parts[1] -notmatch '^\+?[0-9]+$' -or
                    -not [UInt32]::TryParse($parts[1].TrimStart('+'), [Globalization.NumberStyles]::None, [Globalization.CultureInfo]::InvariantCulture, [ref]$t)) {
                    $errors += "line ${n}: timeout must be a number of seconds (0 to 4294967295)"
                }
            }
            'chainload' {
                $rest = $line.Substring($key.Length).Trim()
                if (-not $rest) { $errors += "line ${n}: 'chainload' takes 1 value(s), got 0" }
                elseif (-not $rest.StartsWith('\')) { $errors += "line ${n}: chainload path must start with '\'" }
            }
            default { $errors += "line ${n}: unknown key '$key'" }
        }
    }
    if ($mode -ceq 'protect' -and $ranges -eq 0) { $errors += "no 'range' line" }
    if ($mode -cne 'protect' -and $ranges -gt 0) { $errors += "'range' lines are not allowed with 'mode $mode'" }
    , $errors
}

# 0x-prefixed hex as ramfence.efi parses it ('_' allowed between digits);
# $null if invalid or larger than 64 bits.
function ConvertFrom-ConfigAddress([string]$Text) {
    if ($Text -cnotmatch '^0[xX]([0-9a-fA-F][0-9a-fA-F_]*)$') { return $null }
    $hex = $Matches[1]
    if ($hex.EndsWith('_')) { return $null }
    $digits = ($hex -replace '_', '').TrimStart('0')
    if (-not $digits) { $digits = '0' }
    if ($digits.Length -gt 16) { return $null }
    $v = [UInt64]0
    if (-not [UInt64]::TryParse($digits, [Globalization.NumberStyles]::AllowHexSpecifier, [Globalization.CultureInfo]::InvariantCulture, [ref]$v)) { return $null }
    $v
}
