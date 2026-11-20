# Shared helpers for install.ps1 / uninstall.ps1 / check.ps1.
# Dot-source this file: . "$PSScriptRoot\common.ps1"

Set-StrictMode -Version 3.0

# Must match STATUS_VENDOR in badram-efi/src/main.rs.
$script:BadRamVendorGuid = '{5B7C3F1E-9A42-4D8E-B6A1-2F0C8E4D7A93}'
$script:BadRamVariable = 'BadRamStatus'
$script:BadRamEntryName = 'BadRAM'
$script:BadRamEspDir = '\EFI\badram'
$script:BadRamEfiPath = '\EFI\badram\badram.efi'
# Program Files is writable only by administrators, which matters because
# check.ps1 runs elevated from a scheduled task.
$script:BadRamInstallDir = Join-Path $env:ProgramFiles 'BadRAM'
$script:BadRamStateFile = Join-Path $script:BadRamInstallDir 'state.json'
$script:BadRamLogFile = Join-Path $script:BadRamInstallDir 'check.log'
$script:BadRamTaskName = 'BadRAM boot check'

function Assert-Admin {
    $p = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    if (-not $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'This script must be run as Administrator.'
    }
}

function Assert-UefiBoot {
    # Windows sets %firmware_type% to UEFI, Legacy or Unknown.
    if ($env:firmware_type -ne 'UEFI') {
        throw "Windows was not booted in UEFI mode (firmware_type=$env:firmware_type); badram.efi cannot be used."
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

function Set-BadRamFirst([string]$Id) {
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

# --- BadRamStatus variable -------------------------------------------------------

$script:FirmwareApiLoaded = $false
function Initialize-FirmwareApi {
    if ($script:FirmwareApiLoaded) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class BadRamFirmware {
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
function Get-BadRamStatus {
    Initialize-FirmwareApi
    $e = [BadRamFirmware]::EnableSystemEnvironmentPrivilege()
    if ($e -ne 0) { throw "Cannot enable SeSystemEnvironmentPrivilege (Win32 error $e). Run as Administrator." }
    $err = 0
    $data = [BadRamFirmware]::Read($script:BadRamVariable, $script:BadRamVendorGuid, [ref]$err)
    if ($null -eq $data) {
        if ($err -eq 203) { return @{ Present = $false; Raw = ''; Fields = @{} } }  # ERROR_ENVVAR_NOT_FOUND
        throw "Reading firmware variable $($script:BadRamVariable) failed (Win32 error $err)."
    }
    $raw = [Text.Encoding]::ASCII.GetString($data)
    @{ Present = $true; Raw = $raw; Fields = (ConvertFrom-BadRamStatus $raw) }
}

# "version=1;ok=1;ranges=...;pages=N;type=unusable[;error=...]" -> hashtable
function ConvertFrom-BadRamStatus([string]$Text) {
    $h = @{}
    foreach ($part in $Text.TrimEnd([char]0).Split(';')) {
        $i = $part.IndexOf('=')
        if ($i -gt 0) { $h[$part.Substring(0, $i)] = $part.Substring($i + 1) }
    }
    $h
}

# --- state -----------------------------------------------------------------------

function Get-BadRamState {
    if (-not (Test-Path $script:BadRamStateFile)) { return $null }
    Get-Content -Raw $script:BadRamStateFile | ConvertFrom-Json
}

function Save-BadRamState([string]$Id) {
    @{ EntryId = $Id; InstalledAt = (Get-Date).ToString('s') } |
        ConvertTo-Json | Set-Content -Encoding UTF8 $script:BadRamStateFile
}

# --- config validation -----------------------------------------------------------

# Light check mirroring badram-core's parser, so obvious mistakes are caught
# before rebooting. The real parser in badram.efi remains authoritative.
function Test-BadRamConfig([string]$Path) {
    $errors = @()
    $ranges = 0
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
            'memory_type' { if ($parts.Count -ne 2 -or $parts[1] -notin 'unusable', 'reserved') { $errors += "line ${n}: memory_type must be unusable or reserved" } }
            'on_error' { if ($parts.Count -ne 2 -or $parts[1] -notin 'prompt', 'boot', 'halt') { $errors += "line ${n}: on_error must be prompt, boot or halt" } }
            'timeout' { if ($parts.Count -ne 2 -or $parts[1] -notmatch '^\d+$') { $errors += "line ${n}: timeout must be a number of seconds" } }
            'chainload' { if (-not $line.Substring(9).Trim().StartsWith('\')) { $errors += "line ${n}: chainload path must start with '\'" } }
            default { $errors += "line ${n}: unknown key '$($parts[0])'" }
        }
    }
    if ($ranges -eq 0) { $errors += "no 'range' line" }
    , $errors
}
