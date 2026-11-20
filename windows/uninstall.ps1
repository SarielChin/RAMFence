<#
.SYNOPSIS
    Remove ramfence.efi.

.DESCRIPTION
    Run as Administrator. Puts Windows Boot Manager first, deletes the RAMFence
    firmware entry, removes \EFI\ramfence\ from the ESP, unregisters the logon
    task and deletes the installed files. BCD backups and check.log in
    "$env:ProgramFiles\RAMFence" are kept.

    WARNING: after this, Windows can use the bad memory again. Restore a
    workaround (e.g. a maximum-memory limit) before rebooting if needed.
#>
[CmdletBinding(SupportsShouldProcess)]
param()

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\common.ps1"

if (-not $WhatIfPreference) { Assert-Admin }

$state = Get-RamFenceState
if ($state -and (Test-BcdEntry $state.EntryId)) {
    $id = $state.EntryId
    if ($PSCmdlet.ShouldProcess('{fwbootmgr}', 'Put Windows Boot Manager first')) {
        Invoke-Bcdedit /set '{fwbootmgr}' displayorder '{bootmgr}' /addfirst | Out-Null
    }
    if ($PSCmdlet.ShouldProcess($id, 'Delete RAMFence firmware boot entry')) {
        Invoke-Bcdedit /delete $id | Out-Null
    }
} else {
    Write-Warning "No RAMFence boot entry recorded in $RamFenceStateFile (or it no longer exists)."
}

if ($PSCmdlet.ShouldProcess("ESP $RamFenceEspDir", 'Delete')) {
    Invoke-WithEsp {
        param($root)
        $dir = Join-Path $root $RamFenceEspDir.TrimStart('\')
        if (Test-Path $dir) { Remove-Item -Recurse -Force $dir }
    }
}

if ((Get-ScheduledTask -TaskName $RamFenceTaskName -ErrorAction SilentlyContinue) -and
    $PSCmdlet.ShouldProcess($RamFenceTaskName, 'Unregister scheduled task')) {
    Unregister-ScheduledTask -TaskName $RamFenceTaskName -Confirm:$false
}

if ((Test-Path $RamFenceInstallDir) -and
    $PSCmdlet.ShouldProcess($RamFenceInstallDir, 'Delete installed files (keep BCD backups and check.log)')) {
    foreach ($name in 'ramfence.efi', 'ramfence.cfg', 'state.json', 'check.ps1', 'uninstall.ps1', 'common.ps1') {
        $p = Join-Path $RamFenceInstallDir $name
        if (Test-Path $p) { Remove-Item -Force $p }
    }
}

if ($WhatIfPreference) {
    Write-Host 'Dry run (-WhatIf): nothing was changed.' -ForegroundColor Yellow
} else {
    Write-Host 'RAMFence removed. Windows can use the bad memory again after the next boot.' -ForegroundColor Yellow
}
