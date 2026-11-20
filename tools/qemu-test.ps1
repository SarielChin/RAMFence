<#
.SYNOPSIS
    Run ramfence.efi under QEMU + OVMF and capture the
    serial console.

.DESCRIPTION
    Builds a FAT test folder per case under target\qemu\<case>\esp with
    ramfence.efi + ramfence.cfg in \EFI\ramfence\ and a startup.nsh for the
    built-in UEFI Shell. The shell runs ramfence.efi; afterwards it prints
    `memmap` and the RamFenceStatus variable, then powers off.

    Cases that hit the on_error prompt get their key presses sent through
    QMP once the expected text appears on the serial console.

    The cleaned serial log is written to target\qemu\<case>\serial.txt.

.EXAMPLE
    .\tools\qemu-test.ps1 -Case ok

.EXAMPLE
    .\tools\qemu-test.ps1 -Case conflict -ConflictRange '0x7F000000 0x7F100000'
#>
param(
    [Parameter(Mandatory)]
    [ValidateSet('ok', 'reserved', 'missing-config', 'parse-error', 'conflict', 'halt', 'memtest', 'scan-only', 'auto-scan', 'auto-saved', 'auto-mismatch', 'auto-stopped')]
    [string]$Case,

    # "0xSTART 0xEND" for the conflict/halt cases: pick a BS_Data region
    # from the memmap printed by the 'ok' case.
    [string]$ConflictRange = '',

    [int]$TimeoutSec = 240,

    [switch]$NoBuild,

    [string]$QemuDir = 'C:\Program Files\qemu'
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
$VendorGuid = '0B0C86EE-7039-477B-9A53-E39345EF90D5'
$QmpPort = 4444
$Missing = '\EFI\none\missing.efi'

# --- test cases ------------------------------------------------------------

# Test ranges inside the guest's 2 GiB of low RAM.
$TestRanges = @'
range 0x40000000 0x48000000
range 0x50000123 0x50000fff   # unaligned: expands to one page
'@

# A saved scan result with two errors, for the auto-mode cases.
$SavedResult = "ramfence-test 1`nresult complete`npasses 1`nrequested 0x100000 0x80000000`ntested 0x100000 0x80000000`nerrors 2`nmask 0x0000ff0000000000`nlowest 0x30000100`nhighest 0x30800000`nbucket 0x30000000 2`nerror 0x30000100 0x30000100 0x0000ff0030000100`nerror 0x30800000 0x30800000 0x0000ff0030800000`nend`n"

$Cases = @{
    'ok'             = @{
        # Stage 1 chainloads a second copy of ramfence.efi. That exercises
        # LoadImage/StartImage via a full device path and the second copy
        # finding its own ramfence.cfg. Stage 2's ranges are already reserved.
        Cfg    = "$TestRanges`non_error boot`ntimeout 1`nchainload \EFI\stage2\ramfence.efi`n"
        Stage2 = "$TestRanges`non_error boot`ntimeout 0`nchainload $Missing`n"
        Keys   = @()
    }
    'reserved'       = @{
        Cfg  = "$TestRanges`nmemory_type reserved`non_error boot`ntimeout 0`nchainload $Missing`n"
        Keys = @()
    }
    'missing-config' = @{
        Cfg  = $null
        # Default on_error is prompt. B tries the default bootmgfw path (not
        # present), which leads to the R/S prompt; S powers off.
        Keys = @(@{ Wait = '[B] boot anyway'; Key = 'b' }, @{ Wait = '[R] reboot   [S] shut down'; Key = 's' })
    }
    'parse-error'    = @{
        Cfg  = "range 0x40000000`n"
        # R resets; QEMU runs with -no-reboot, so it exits.
        Keys = @(@{ Wait = '[B] boot anyway'; Key = 'r' })
    }
    'conflict'       = @{
        Cfg  = "range $ConflictRange`non_error boot`ntimeout 0`nchainload $Missing`n"
        Keys = @()
    }
    'memtest'        = @{
        # Scheduled pre-boot memory test: the configured range plus 64 MiB
        # margins, and a low region that includes firmware BootServicesData
        # (must be reported as untested). The reservation must be unaffected.
        Cfg     = "$TestRanges`non_error boot`ntimeout 0`nchainload $Missing`n"
        TestReq = "scan 0x3C000000 0x4C000000`nscan 0x800000 0x1800000`npasses 1`n"
        Keys    = @()
    }
    'scan-only'      = @{
        # First-time setup: nothing reserved, scheduled scan runs, status
        # says mode=scan-only.
        Cfg     = "mode scan-only`non_error boot`ntimeout 0`nchainload $Missing`n"
        TestReq = "scan 0x30000000 0x50000000`npasses 1`n"
        Keys    = @()
    }
    'auto-scan'      = @{
        # Rescue USB, first start: no saved result, so all RAM is scanned
        # and the result saved. QEMU memory has no errors: nothing reserved.
        Cfg  = "mode auto`non_error boot`ntimeout 0`nchainload $Missing`n"
        Mem  = '768M'
        Keys = @()
    }
    'auto-saved'     = @{
        # Rescue USB, later start: the saved result (with two errors) is
        # used without scanning; error cluster + 256 MiB margin reserved:
        # expect Unusable 0x20000000-0x408FFFFF. The result must carry this
        # guest's machine id, taken from the 'memtest' case (same 2G guest):
        # run that case first.
        Cfg         = "mode auto`non_error boot`ntimeout 0`nchainload $Missing`n"
        Result      = $SavedResult
        MachineFrom = 'memtest'
        Keys        = @()
    }
    'auto-mismatch'  = @{
        # Rescue USB moved to another PC: the saved result's machine id does
        # not match, so it is ignored and all RAM is scanned again (nothing
        # reserved, a new result with this guest's id is saved).
        Cfg     = "mode auto`non_error boot`ntimeout 0`nchainload $Missing`n"
        Result  = $SavedResult.Replace("passes 1`n", "passes 1`nmachine 0x0000000000000001`n")
        Mem     = '768M'
        Keys    = @()
    }
    'auto-stopped'   = @{
        # Esc during the auto-mode scan: nothing saved, status says
        # scan=stopped so Windows can warn.
        Cfg  = "mode auto`non_error boot`ntimeout 0`nchainload $Missing`n"
        Mem  = '768M'
        Keys = @(@{ Wait = 'Esc = abort'; Key = 'esc'; Delay = 2 })
    }
    'halt'           = @{
        Cfg  = "range $ConflictRange`non_error halt`ntimeout 0`nchainload $Missing`n"
        # B must be ignored under halt; S powers off.
        Keys = @(@{ Wait = 'on_error=halt'; Key = 'b' }, @{ Wait = '[S] shut down'; Key = 's'; Delay = 3 })
    }
}

if ($Case -in 'conflict', 'halt' -and -not $ConflictRange) {
    throw "-ConflictRange is required for case '$Case'"
}
$Spec = $Cases[$Case]

# --- build -----------------------------------------------------------------

$Efi = Join-Path $Root 'target\x86_64-unknown-uefi\release\ramfence.efi'
if (-not $NoBuild) {
    Push-Location $Root
    # cargo reports progress on stderr; Windows PowerShell would turn that
    # into a terminating error under 'Stop'.
    $ErrorActionPreference = 'Continue'
    try {
        cargo build -p ramfence-efi --target x86_64-unknown-uefi --release 2>&1 | ForEach-Object { "$_" }
        if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    } finally {
        $ErrorActionPreference = 'Stop'
        Pop-Location
    }
}

# --- test folder -----------------------------------------------------------

$CaseDir = Join-Path $Root "target\qemu\$Case"
if (Test-Path $CaseDir) { Remove-Item -Recurse -Force $CaseDir }
$Esp = Join-Path $CaseDir 'esp'
New-Item -ItemType Directory -Force (Join-Path $Esp 'EFI\ramfence') | Out-Null

function Write-Ascii([string]$Path, [string]$Text) {
    [IO.File]::WriteAllText($Path, $Text.Replace("`r`n", "`n"), [Text.Encoding]::ASCII)
}

Copy-Item $Efi (Join-Path $Esp 'EFI\ramfence\ramfence.efi')
if ($null -ne $Spec.Cfg) { Write-Ascii (Join-Path $Esp 'EFI\ramfence\ramfence.cfg') $Spec.Cfg }
if ($Spec.ContainsKey('TestReq')) { Write-Ascii (Join-Path $Esp 'EFI\ramfence\test.req') $Spec.TestReq }
if ($Spec.ContainsKey('Result')) {
    $result = $Spec.Result
    if ($Spec.ContainsKey('MachineFrom')) {
        # From the serial log, where the guest prints the saved result: QEMU
        # does not reliably write the guest's files back to the host folder.
        $src = Join-Path $Root "target\qemu\$($Spec.MachineFrom)\serial.txt"
        $line = if (Test-Path $src) {
            Get-Content $src | ForEach-Object { $_.Trim() } |
                Where-Object { $_ -match '^machine 0x[0-9a-f]{16}$' } | Select-Object -First 1
        }
        if (-not $line) { throw "No machine id in ${src}: run -Case $($Spec.MachineFrom) first." }
        $result = $result.Replace("passes 1`n", "passes 1`n$line`n")
    }
    Write-Ascii (Join-Path $Esp 'EFI\ramfence\test-result.txt') $result
}
$Mem = if ($Spec.ContainsKey('Mem')) { $Spec.Mem } else { '2G' }
if ($Spec.Stage2) {
    New-Item -ItemType Directory -Force (Join-Path $Esp 'EFI\stage2') | Out-Null
    Copy-Item $Efi (Join-Path $Esp 'EFI\stage2\ramfence.efi')
    Write-Ascii (Join-Path $Esp 'EFI\stage2\ramfence.cfg') $Spec.Stage2
}

Write-Ascii (Join-Path $Esp 'startup.nsh') @"
fs0:
\EFI\ramfence\ramfence.efi
echo "=== memmap ==="
memmap
echo "=== RamFenceStatus ==="
dmpstore RamFenceStatus -guid $VendorGuid
echo "=== files ==="
ls \EFI\ramfence
type \EFI\ramfence\test-result.txt
echo "=== end ==="
reset -s
"@

Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') (Join-Path $CaseDir 'vars.fd')

# --- run -------------------------------------------------------------------

$SerialLog = Join-Path $CaseDir 'serial.log'
$QemuArgs = @(
    '-machine', 'q35', '-accel', 'tcg', '-m', $Mem, '-smp', '1',
    '-drive', "if=pflash,format=raw,readonly=on,file=`"$(Join-Path $QemuDir 'share\edk2-x86_64-code.fd')`"",
    '-drive', 'if=pflash,format=raw,file=vars.fd',
    # vvfat: relative path, because a drive letter's ':' confuses the parser.
    '-drive', 'file=fat:rw:esp,format=raw,media=disk',
    '-nic', 'none', '-display', 'none', '-no-reboot',
    '-serial', 'file:serial.log',
    '-qmp', "tcp:127.0.0.1:${QmpPort},server=on,wait=off"
)

function Get-CleanLog {
    if (-not (Test-Path $SerialLog)) { return '' }
    $fs = [IO.File]::Open($SerialLog, 'Open', 'Read', 'ReadWrite')
    try { $raw = [IO.StreamReader]::new($fs).ReadToEnd() } finally { $fs.Close() }
    # Strip ANSI escape sequences and carriage returns. ([char]27: Windows
    # PowerShell 5.1 has no `e escape.)
    $esc = [regex]::Escape([string][char]27)
    ($raw -replace "$esc\[[0-9;?=]*[A-Za-z]", '' -replace "$esc[()][0-9A-Za-z]", '') -replace "`r", ''
}

function Send-QmpKey([string]$Key) {
    $client = [Net.Sockets.TcpClient]::new('127.0.0.1', $QmpPort)
    try {
        $stream = $client.GetStream()
        $reader = [IO.StreamReader]::new($stream)
        $writer = [IO.StreamWriter]::new($stream)
        $writer.AutoFlush = $true
        $null = $reader.ReadLine()   # greeting
        $writer.WriteLine('{"execute":"qmp_capabilities"}')
        $null = $reader.ReadLine()
        $writer.WriteLine('{"execute":"send-key","arguments":{"keys":[{"type":"qcode","data":"' + $Key + '"}]}}')
        $null = $reader.ReadLine()
    } finally { $client.Close() }
}

Write-Host "Running case '$Case' (timeout $TimeoutSec s)..."
$proc = Start-Process -FilePath (Join-Path $QemuDir 'qemu-system-x86_64.exe') -ArgumentList $QemuArgs `
    -WorkingDirectory $CaseDir -PassThru -WindowStyle Hidden

$deadline = (Get-Date).AddSeconds($TimeoutSec)
$keyIndex = 0
$searchFrom = 0
$timedOut = $false
while (-not $proc.HasExited) {
    if ((Get-Date) -gt $deadline) { $timedOut = $true; break }
    Start-Sleep -Milliseconds 500
    if ($keyIndex -lt $Spec.Keys.Count) {
        $k = $Spec.Keys[$keyIndex]
        $log = Get-CleanLog
        $pos = $log.IndexOf($k.Wait, $searchFrom)
        if ($pos -ge 0) {
            if ($k.Delay) { Start-Sleep -Seconds $k.Delay } else { Start-Sleep -Milliseconds 500 }
            Write-Host "  sending key '$($k.Key)'"
            Send-QmpKey $k.Key
            $searchFrom = $pos + $k.Wait.Length
            $keyIndex++
        }
    }
}
if ($timedOut) {
    Stop-Process -Id $proc.Id -Force
    Write-Warning "Timed out after $TimeoutSec s; QEMU killed."
}

$clean = Get-CleanLog
$out = Join-Path $CaseDir 'serial.txt'
[IO.File]::WriteAllText($out, $clean)
Write-Host "Keys sent: $keyIndex of $($Spec.Keys.Count). Log: $out"
