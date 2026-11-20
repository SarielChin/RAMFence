# CLAUDE.md

RAMFence: a UEFI pre-boot program (Rust) that marks configured physical memory
ranges as unusable before Windows Boot Manager starts, then chainloads Windows.
RAMFence Manager is the Windows GUI that installs, configures and checks it.
See `README.md` for how it works and how it is used.

## Commands

- Install the target: `rustup target add x86_64-unknown-uefi`
- Build the EFI app: `cargo build -p ramfence-efi --target x86_64-unknown-uefi --release`
- Test the pure logic: `cargo test -p ramfence-core`
- GUI manager: `cargo build -p ramfence-manager --release` (the Windows GNU
  toolchain needs MinGW-w64 `dlltool` and `as` on PATH; tests:
  `cargo test -p ramfence-manager`)
  - `ramfence-manager.exe --status` prints the status report; `--check` is the
    windowless check run by the logon task
  - Single-file release: `tools\build-release.ps1` →
    `dist\RAMFence-Manager-<version>.exe` (build.rs embeds ramfence.efi; the
    script builds it, remaps local paths and strips symbols)
- QEMU tests: `tools\qemu-test.ps1 -Case <case>` (needs QEMU with its bundled
  OVMF/edk2 firmware; the `conflict` and `halt` cases also need
  `-ConflictRange` set to a BS_Data region from the `ok` case's memmap). Check
  the result in `target\qemu\<case>\serial.txt`.

## Design notes

Non-obvious decisions; keep them unless there is a tested reason to change.

- **Reserving memory:** each range is aligned outward to 4 KiB pages
  (reserve more rather than less) and overlapping ranges are merged. Only the
  `EfiConventionalMemory` parts are allocated (`AllocateAddress`, in 64 MiB
  chunks with a page-by-page fallback). Parts that are already Reserved, MMIO,
  ACPI NVS, Runtime or Unusable are skipped, since the OS never gets them.
  Every other type inside a range (`BootServicesCode/Data`,
  `LoaderCode/Data`, ACPI Reclaim, unknown types) is an **error**: it is in
  use now and the OS may reclaim it later, so it cannot be protected
  (`memmap::classify` is deliberately conservative). The memory map is read
  again afterwards to verify.
- **Chainloading:** look for `\EFI\Microsoft\Boot\bootmgfw.efi` on ramfence.efi's
  own volume first, then on every FAT volume (so a USB stick can start the
  Windows on the internal disk). Load it with a **full device path** (volume
  handle + file path), otherwise bootmgr cannot find its BCD.
- **Never replace or modify `bootmgfw.efi`.** Windows updates restore it and
  the protection would disappear silently. RAMFence is its own firmware boot
  entry (`bcdedit /copy {bootmgr}`, path changed, put first in
  `{fwbootmgr} displayorder`). Updates sometimes put Windows Boot Manager first
  again; the logon check repairs that.
- **`RamFenceStatus` variable:** volatile (`BOOTSERVICE_ACCESS |
  RUNTIME_ACCESS`, no `NON_VOLATILE`), so it exists only if *this* boot went
  through ramfence.efi. Its text format (`version=1;ok=…;ranges=…;…`, see
  `ramfence-core/src/status.rs`) is a contract between ramfence.efi, the
  manager and `windows\check.ps1`: change it compatibly or bump `version`.
- **Two Windows-side implementations:** RAMFence Manager (Rust) and the
  PowerShell scripts in `windows\` install, check and uninstall the same way.
  Keep these in sync: ESP folder, install folder, boot entry name, task name,
  `state.json` format, and the vendor GUID (in `ramfence-efi/src/main.rs`,
  `ramfence-manager/src/paths.rs`, `windows/common.ps1` and
  `tools/qemu-test.ps1`).
- **Fast Startup / hibernation should be off:** resuming a hibernation image
  that was created during a boot without RAMFence could bring back data that
  sits in bad memory.
- **Not tested yet:** the effect of Hyper-V / VBS / Memory Integrity.

## Rules

- `ramfence-core` stays `no_std` and does not depend on the `uefi` crate. All
  testable logic goes there.
- Memory may only be allocated as `EfiUnusableMemory` or
  `EfiReservedMemoryType`, never a type that is reclaimed after
  ExitBootServices.
- No error may let the program silently continue: always handle it according to
  `on_error` and show it on screen.
- Files committed to git (code comments, README, commit messages, etc.) must not
  contain machine-specific information: computer model, hardware configuration,
  actual bad memory addresses, host names, local paths. Those belong only in
  `CLAUDE.local.md`. The same applies to published release binaries.
