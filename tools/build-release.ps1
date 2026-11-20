<#
.SYNOPSIS
    Build a single-file release: dist\RAMFence-Manager-<version>.exe with
    ramfence.efi embedded.

.DESCRIPTION
    1. Builds ramfence.efi (x86_64-unknown-uefi, release).
    2. Runs the unit tests.
    3. Builds ramfence-manager.exe (release); its build.rs embeds the
       ramfence.efi from step 1.
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
    Invoke-Cargo test -p ramfence-core -p ramfence-manager

    # Release binaries are published: strip the builder's local paths (user
    # name, checkout location) from panic locations and debug info.
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    $savedFlags = $env:RUSTFLAGS
    $savedEfi = $env:RAMFENCE_EFI
    $savedStrip = $env:CARGO_PROFILE_RELEASE_STRIP
    $env:RUSTFLAGS = "--remap-path-prefix=$cargoHome=/cargo --remap-path-prefix=$Root=/ramfence"
    # The GNU toolchain also leaves the build directory in import symbol names
    # (raw-dylib via dlltool); dropping the symbol table removes those.
    $env:CARGO_PROFILE_RELEASE_STRIP = "symbols"
    # Separate target dir: different RUSTFLAGS would otherwise force full
    # rebuilds of the normal dev builds, and a running copy of
    # target\release\ramfence-manager.exe would lock the output file.
    $efiPath = Join-Path $Root 'target\pkg\x86_64-unknown-uefi\release\ramfence.efi'
    $env:RAMFENCE_EFI = $efiPath
    try {
        Invoke-Cargo build -p ramfence-efi --target x86_64-unknown-uefi --release --target-dir target\pkg
        Invoke-Cargo build -p ramfence-manager --release --target-dir target\pkg
    } finally {
        $env:RUSTFLAGS = $savedFlags
        $env:RAMFENCE_EFI = $savedEfi
        $env:CARGO_PROFILE_RELEASE_STRIP = $savedStrip
    }

    $version = (Select-String -Path ramfence-manager\Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
    $dist = Join-Path $Root 'dist'
    New-Item -ItemType Directory -Force $dist | Out-Null
    $out = Join-Path $dist "RAMFence-Manager-$version.exe"
    Copy-Item target\pkg\release\ramfence-manager.exe $out -Force

    $efi = [IO.File]::ReadAllBytes($efiPath)
    $exe = [IO.File]::ReadAllBytes($out)
    $exeText = [Text.Encoding]::ASCII.GetString($exe)
    # Sanity checks: the exe must contain the embedded ramfence.efi marker,
    # and no local paths may be left in either binary.
    if (-not $exeText.Contains('RAMFENCE-FEATURES: memtest1')) {
        throw 'ramfence.efi was not embedded into the manager'
    }
    foreach ($p in @($cargoHome, $Root, $env:USERPROFILE, ($Root -replace "[^A-Za-z0-9]", "_"))) {
        if ($exeText.IndexOf($p, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
            throw "local path '$p' found in the release exe"
        }
    }
    Write-Host ''
    Write-Host "Release: $out" -ForegroundColor Green
    Write-Host ("  size    {0:N0} bytes (ramfence.efi {1:N0} bytes embedded)" -f $exe.Length, $efi.Length)
    Write-Host "  sha256  $((Get-FileHash $out -Algorithm SHA256).Hash)"
} finally {
    Pop-Location
}
