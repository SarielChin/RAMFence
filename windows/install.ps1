<#
.SYNOPSIS
    Install ramfence.efi as the first firmware boot entry.

.DESCRIPTION
    Run as Administrator. Steps:
      1. Checks: UEFI boot, config syntax, Secure Boot, BitLocker, Fast Startup.
      2. Copies scripts and files to "$env:ProgramFiles\RAMFence" and exports a
         BCD backup there.
      3. Copies ramfence.efi + ramfence.cfg to \EFI\ramfence\ on the ESP.
         bootmgfw.efi is never modified.
      4. Creates a firmware boot entry "RAMFence" (copy of {bootmgr}) pointing
         at \EFI\ramfence\ramfence.efi and puts it first in the boot order.
      5. Registers a logon task running check.ps1 (section 5.3).

    Running it again updates the files and keeps the existing entry.

.PARAMETER Config
    Path to ramfence.cfg.

.PARAMETER Efi
    Path to ramfence.efi. Defaults to the release build in this repository.

.PARAMETER SuspendBitLocker
    Required when BitLocker protects the system drive: suspends it for two
    reboots. Back up your recovery key first.

.PARAMETER AllowSecureBoot
    Install even though Secure Boot is on. Only use this after enrolling the
    hash of this exact ramfence.efi in the firmware's db;
    otherwise the firmware silently skips it.

.EXAMPLE
    .\install.ps1 -Config .\ramfence.cfg -WhatIf

.EXAMPLE
    .\install.ps1 -Config .\ramfence.cfg
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    [Parameter(Mandatory)][string]$Config,
    [string]$Efi = '',
    [switch]$SuspendBitLocker,
    [switch]$AllowSecureBoot
)

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\common.ps1"

# --- checks ------------------------------------------------------------------

if (-not $WhatIfPreference) { Assert-Admin }
Assert-UefiBoot

# Resolved here: $PSScriptRoot is empty in param defaults on Windows PowerShell 5.1.
if (-not $Efi) {
    $Efi = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\x86_64-unknown-uefi\release\ramfence.efi'
}
$Config = (Resolve-Path $Config).Path
$Efi = (Resolve-Path $Efi).Path
foreach ($f in "$PSScriptRoot\common.ps1", "$PSScriptRoot\check.ps1", "$PSScriptRoot\uninstall.ps1") {
    if (-not (Test-Path $f)) { throw "Missing $f" }
}

$cfgErrors = Test-RamFenceConfig $Config
if ($cfgErrors.Count) { throw "Config errors in ${Config}:`n  $($cfgErrors -join "`n  ")" }
Write-Host "Config OK: $Config"

$secureBoot = $null
try { $secureBoot = Confirm-SecureBootUEFI } catch { }
if ($secureBoot -eq $true -and -not $AllowSecureBoot) {
    throw @'
Secure Boot is ON. The firmware will refuse the unsigned ramfence.efi and fall
through to Windows Boot Manager, leaving bad memory unprotected.
Either turn Secure Boot off, or enroll this ramfence.efi's hash in db and rerun
with -AllowSecureBoot.
'@
} elseif ($null -eq $secureBoot) {
    Write-Warning 'Could not read the Secure Boot state.'
}

$bitlocker = $null
try { $bitlocker = Get-BitLockerVolume -MountPoint $env:SystemDrive -ErrorAction Stop } catch { }
if ($bitlocker -and $bitlocker.ProtectionStatus -eq 'On') {
    if (-not $SuspendBitLocker) {
        throw @"
BitLocker protects $env:SystemDrive. Changing the boot path can trigger
recovery mode. Back up the recovery key first:
    manage-bde -protectors -get $env:SystemDrive
then rerun with -SuspendBitLocker.
"@
    }
} elseif ($null -eq $bitlocker) {
    Write-Warning 'Could not read the BitLocker state (not installed, or not elevated).'
}

$hiberboot = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Power' -Name HiberbootEnabled -ErrorAction SilentlyContinue).HiberbootEnabled
if ($hiberboot -eq 1) {
    Write-Warning 'Fast Startup is on. Windows may restore a session that was started without ramfence.efi; consider "powercfg /h off" (also turns off hibernation).'
}

# --- install -----------------------------------------------------------------

if ($bitlocker -and $bitlocker.ProtectionStatus -eq 'On' -and
    $PSCmdlet.ShouldProcess($env:SystemDrive, 'Suspend BitLocker for 2 reboots')) {
    Suspend-BitLocker -MountPoint $env:SystemDrive -RebootCount 2 | Out-Null
}

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
if ($PSCmdlet.ShouldProcess($RamFenceInstallDir, 'Copy scripts and export BCD backup')) {
    New-Item -ItemType Directory -Force $RamFenceInstallDir | Out-Null
    Copy-Item "$PSScriptRoot\common.ps1", "$PSScriptRoot\check.ps1", "$PSScriptRoot\uninstall.ps1" $RamFenceInstallDir -Force
    Copy-Item $Efi (Join-Path $RamFenceInstallDir 'ramfence.efi') -Force
    Copy-Item $Config (Join-Path $RamFenceInstallDir 'ramfence.cfg') -Force
    $backup = Join-Path $RamFenceInstallDir "BCD-backup-$stamp"
    Invoke-Bcdedit /export $backup | Out-Null
    Write-Host "BCD backup: $backup  (restore: bcdedit /import `"$backup`")"
}

if ($PSCmdlet.ShouldProcess("ESP $RamFenceEspDir", 'Copy ramfence.efi and ramfence.cfg')) {
    Invoke-WithEsp {
        param($root)
        $dir = Join-Path $root $RamFenceEspDir.TrimStart('\')
        New-Item -ItemType Directory -Force $dir | Out-Null
        Copy-Item $Efi (Join-Path $dir 'ramfence.efi') -Force
        Copy-Item $Config (Join-Path $dir 'ramfence.cfg') -Force
        if (-not (Test-Path (Join-Path $root 'EFI\Microsoft\Boot\bootmgfw.efi'))) {
            Write-Warning 'bootmgfw.efi not found on this ESP; ramfence.efi will search other volumes.'
        }
    }
}

$state = Get-RamFenceState
$id = $null
if ($state -and (Test-BcdEntry $state.EntryId)) {
    $id = $state.EntryId
    Write-Host "Using existing boot entry $id"
} elseif ($PSCmdlet.ShouldProcess('{fwbootmgr}', "Create firmware boot entry '$RamFenceEntryName'")) {
    $out = Invoke-Bcdedit /copy '{bootmgr}' /d $RamFenceEntryName
    $m = [regex]::Match(($out -join ' '), $GuidPattern)
    if (-not $m.Success) { throw "Could not find the new entry id in bcdedit output: $out" }
    $id = $m.Value.ToLowerInvariant()
    try {
        Invoke-Bcdedit /set $id path $RamFenceEfiPath | Out-Null
        Save-RamFenceState $id
    } catch {
        & bcdedit.exe /delete $id 2>&1 | Out-Null
        throw
    }
    Write-Host "Created boot entry $id"
}

if ($id -and $PSCmdlet.ShouldProcess($id, 'Put RAMFence first in the firmware boot order')) {
    Set-RamFenceFirst $id
    $order = Get-FirmwareBootOrder
    if ($order[0] -ne $id) { throw "Boot order did not change; first entry is $($order[0])." }
}

if ($PSCmdlet.ShouldProcess($RamFenceTaskName, 'Register logon task for check.ps1')) {
    $user = [Security.Principal.WindowsIdentity]::GetCurrent().Name
    $action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument (
        "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$(Join-Path $RamFenceInstallDir 'check.ps1')`"")
    $trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
    $principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Highest
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Minutes 5)
    Register-ScheduledTask -TaskName $RamFenceTaskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
}

Write-Host ''
if ($WhatIfPreference) {
    Write-Host 'Dry run (-WhatIf): nothing was changed.' -ForegroundColor Yellow
    return
}
Write-Host 'Installed. Next steps:' -ForegroundColor Green
Write-Host '  1. Reboot. The ramfence.efi summary should appear before Windows starts.'
Write-Host "  2. After logging in, run: & '$RamFenceInstallDir\check.ps1'"
Write-Host '  3. Only after check.ps1 reports OK, remove any maximum-memory /'
Write-Host '     truncatememory limit you used as a workaround.'
