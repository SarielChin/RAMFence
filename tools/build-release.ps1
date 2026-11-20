<#
.SYNOPSIS
    Build a single-file release: dist\BadRAM-Manager-<version>.exe with
    badram.efi embedded.

.DESCRIPTION
    1. Builds badram.efi (x86_64-unknown-uefi, release).
    2. Runs the unit tests.
    3. Builds badram-manager.exe (release); its build.rs embeds the
       badram.efi from step 1.
    4. Copies the result to dist\ and prints its SHA-256.

    Needs rustup's x86_64-unknown-uefi target and, for the GNU host
    toolchain, MinGW-w64 binutils (dlltool, as) on PATH.
#>
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot

# Pick up PATH changes made by installers since this shell started.
$env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' +
    [Environment]::GetEnvironmentVariable('Path', 'User') + ';' + $env:Path

function Invoke-Cargo {
    # Plain $args: an advanced function would take "-p" as -PipelineVariable.
    $CargoArgs = [string[]]$args
    Write-Host "> cargo $($CargoArgs -join ' ')" -ForegroundColor Cyan
    # cargo reports progress on stderr; don't let Windows PowerShell treat
    # that as an error.
    $ErrorActionPreference = 'Continue'
    & cargo @CargoArgs 2>&1 | ForEach-Object { "$_" }
    $code = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($code -ne 0) { throw "cargo $($CargoArgs -join ' ') failed (exit $code)" }
}

Push-Location $Root
try {
    Invoke-Cargo build -p badram-efi --target x86_64-unknown-uefi --release
    Invoke-Cargo test -p badram-core -p badram-manager
    # Separate target dir: a running copy of target\release\badram-manager.exe
    # (e.g. open for testing) would otherwise lock the output file.
    Invoke-Cargo build -p badram-manager --release --target-dir target\pkg

    $version = (Select-String -Path badram-manager\Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
    $dist = Join-Path $Root 'dist'
    New-Item -ItemType Directory -Force $dist | Out-Null
    $out = Join-Path $dist "BadRAM-Manager-$version.exe"
    Copy-Item target\pkg\release\badram-manager.exe $out -Force

    $efi = [IO.File]::ReadAllBytes((Join-Path $Root 'target\x86_64-unknown-uefi\release\badram.efi'))
    $exe = [IO.File]::ReadAllBytes($out)
    # Sanity check: the exe must contain the embedded badram.efi marker.
    if (-not [Text.Encoding]::ASCII.GetString($exe).Contains('BADRAM-FEATURES: memtest1')) {
        throw 'badram.efi was not embedded into the manager'
    }
    Write-Host ''
    Write-Host "Release: $out" -ForegroundColor Green
    Write-Host ("  size    {0:N0} bytes (badram.efi {1:N0} bytes embedded)" -f $exe.Length, $efi.Length)
    Write-Host "  sha256  $((Get-FileHash $out -Algorithm SHA256).Hash)"
} finally {
    Pop-Location
}
