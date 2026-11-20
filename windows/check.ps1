<#
.SYNOPSIS
    Check that this boot went through ramfence.efi.

.DESCRIPTION
    Run as Administrator; install.ps1 registers it as a logon task.
      - Reads the volatile RamFenceStatus firmware variable.
        Missing  -> this boot was NOT protected (boot order changed, or the
                    firmware rejected ramfence.efi): alert, restart needed.
        ok=0     -> ramfence.efi reported an error: alert with the message.
        ok=1     -> protected, unless a configured range was not reserved
                    (booted from another copy, e.g. a rescue USB stick) or
                    an auto-mode scan was stopped.
      - Makes sure the RAMFence entry is still first in the firmware boot
        order and restores it if Windows Update moved it (unless -NoFix).

    Exit codes: 0 OK, 1 check failed, 2 boot not protected, 3 ramfence.efi error.

.PARAMETER NoFix
    Report only; do not change the boot order.

.PARAMETER NoPopup
    Do not show message boxes (console/log only).
#>
[CmdletBinding()]
param(
    [switch]$NoFix,
    [switch]$NoPopup
)

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\common.ps1"

function Write-CheckLog([string]$Message) {
    $line = '{0}  {1}' -f (Get-Date -Format 's'), $Message
    Write-Host $line
    if (Test-Path $RamFenceInstallDir) {
        try { Add-Content -Path $RamFenceLogFile -Value $line -Encoding UTF8 } catch { }
    }
}

function Show-Alert([string]$Message, [string]$Icon = 'Warning') {
    if ($NoPopup) { return }
    Add-Type -AssemblyName System.Windows.Forms
    [void][System.Windows.Forms.MessageBox]::Show($Message, 'RAMFence', 'OK', $Icon)
}

$exitCode = 0
try {
    Assert-Admin
    $state = Get-RamFenceState
    if (-not $state) { throw "RAMFence is not installed ($RamFenceStateFile missing)." }
    $id = $state.EntryId.ToLowerInvariant()

    # Boot order first, so the fix is in place even if we alert below.
    $orderNote = ''
    if (-not (Test-BcdEntry $id)) {
        $orderNote = "The RAMFence boot entry $id no longer exists. Run install.ps1 again."
    } else {
        $order = Get-FirmwareBootOrder
        if ($order.Count -eq 0 -or $order[0] -ne $id) {
            $first = if ($order.Count) { $order[0] } else { '(none)' }
            if ($NoFix) {
                $orderNote = "RAMFence is not first in the boot order (first: $first)."
            } else {
                Set-RamFenceFirst $id
                Write-CheckLog "Boot order restored: RAMFence is first again (was: $first)."
            }
        }
    }
    if ($orderNote) { Write-CheckLog $orderNote }

    $status = Get-RamFenceStatus
    if (-not $status.Present) {
        $exitCode = 2
        $msg = "This boot did NOT go through ramfence.efi, so bad memory may be in use right now.`n`n" +
            "Possible causes: the boot order was changed (restored now unless noted below), " +
            "Secure Boot rejected ramfence.efi, or Windows resumed from hibernation.`n`n" +
            "Save your work and RESTART NOW (a full restart, not shut down + power on with Fast Startup)."
        if ($orderNote) { $msg += "`n`n$orderNote" }
        Write-CheckLog 'NOT PROTECTED: RamFenceStatus variable missing.'
        Show-Alert $msg 'Error'
    } elseif ($status.Fields['ok'] -ne '1') {
        $exitCode = 3
        $err = $status.Fields['error']
        Write-CheckLog "ramfence.efi reported an error: $err  [$($status.Raw)]"
        Show-Alert ("ramfence.efi reported an error during boot; bad memory may not be fully protected.`n`n$err" +
            $(if ($orderNote) { "`n`n$orderNote" } else { '' })) 'Error'
    } else {
        # ok=1 only says that what this boot was asked to reserve is reserved.
        # Compare with the installed configuration: a boot from another copy
        # of ramfence.efi (e.g. a rescue USB stick) may have reserved less.
        $missing = @()
        if ($status.Fields.ContainsKey('mode') -and $status.Fields['mode'] -eq 'scan-only') {
            Write-CheckLog 'Scan-only mode (setup not finished): nothing is reserved'
        } else {
            $have = @(if ($status.Fields.ContainsKey('ranges')) { ConvertFrom-StatusRanges $status.Fields['ranges'] })
            $missing = @(Get-UncoveredRanges (Get-RamFenceConfigRanges $RamFenceCfgFile) $have)
        }
        if ($missing.Count) {
            $exitCode = 3
            $list = $missing -join ', '
            Write-CheckLog "PROBLEM: configured range(s) not reserved by this boot: $list  [$($status.Raw)]"
            Show-Alert ("This boot did not reserve the configured range(s) $list, so bad memory may be in use right now.`n`n" +
                "It probably started from another copy of ramfence.efi (for example a rescue USB stick), " +
                "or the configuration on the EFI System Partition differs from the installed one." +
                $(if ($orderNote) { "`n`n$orderNote" } else { '' })) 'Error'
        } elseif ($status.Fields.ContainsKey('scan') -and $status.Fields['scan'] -eq 'stopped') {
            $exitCode = 3
            Write-CheckLog "PROBLEM: the auto-mode memory scan was stopped; only errors found so far are reserved  [$($status.Raw)]"
            Show-Alert ("The memory scan was stopped before it finished, so only the errors found so far are reserved." +
                $(if ($orderNote) { "`n`n$orderNote" } else { '' })) 'Error'
        } else {
            Write-CheckLog "OK: ranges=$($status.Fields['ranges']) pages=$($status.Fields['pages']) type=$($status.Fields['type'])"
            if ($orderNote) { Show-Alert $orderNote }
        }
    }
} catch {
    $exitCode = 1
    Write-CheckLog "Check failed: $($_.Exception.Message)"
    Show-Alert "RAMFence check failed:`n`n$($_.Exception.Message)"
}
exit $exitCode
