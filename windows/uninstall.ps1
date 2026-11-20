<#
.SYNOPSIS
    Remove badram.efi (DESIGN.md section 5.4).

.DESCRIPTION
    Run as Administrator. Puts Windows Boot Manager first, deletes the BadRAM
    firmware entry, removes \EFI\badram\ from the ESP, unregisters the logon
    task and deletes the installed files. BCD backups and check.log in
    "$env:ProgramFiles\BadRAM" are kept.

    WARNING: after this, Windows can use the bad memory again. Restore a
    workaround (e.g. a maximum-memory limit) before rebooting if needed.
#>
[CmdletBinding(SupportsShouldProcess)]
param()

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\common.ps1"

if (-not $WhatIfPreference) { Assert-Admin }

$state = Get-BadRamState
if ($state -and (Test-BcdEntry $state.EntryId)) {
    $id = $state.EntryId
    if ($PSCmdlet.ShouldProcess('{fwbootmgr}', 'Put Windows Boot Manager first')) {
        Invoke-Bcdedit /set '{fwbootmgr}' displayorder '{bootmgr}' /addfirst | Out-Null
    }
    if ($PSCmdlet.ShouldProcess($id, 'Delete BadRAM firmware boot entry')) {
        Invoke-Bcdedit /delete $id | Out-Null
    }
} else {
    Write-Warning "No BadRAM boot entry recorded in $BadRamStateFile (or it no longer exists)."
}

if ($PSCmdlet.ShouldProcess("ESP $BadRamEspDir", 'Delete')) {
    Invoke-WithEsp {
        param($root)
        $dir = Join-Path $root $BadRamEspDir.TrimStart('\')
        if (Test-Path $dir) { Remove-Item -Recurse -Force $dir }
    }
}

if ((Get-ScheduledTask -TaskName $BadRamTaskName -ErrorAction SilentlyContinue) -and
    $PSCmdlet.ShouldProcess($BadRamTaskName, 'Unregister scheduled task')) {
    Unregister-ScheduledTask -TaskName $BadRamTaskName -Confirm:$false
}

if ((Test-Path $BadRamInstallDir) -and
    $PSCmdlet.ShouldProcess($BadRamInstallDir, 'Delete installed files (keep BCD backups and check.log)')) {
    foreach ($name in 'badram.efi', 'badram.cfg', 'state.json', 'check.ps1', 'uninstall.ps1', 'common.ps1') {
        $p = Join-Path $BadRamInstallDir $name
        if (Test-Path $p) { Remove-Item -Force $p }
    }
}

if ($WhatIfPreference) {
    Write-Host 'Dry run (-WhatIf): nothing was changed.' -ForegroundColor Yellow
} else {
    Write-Host 'BadRAM removed. Windows can use the bad memory again after the next boot.' -ForegroundColor Yellow
}
