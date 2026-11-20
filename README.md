# RAMFence

Keep Windows away from damaged RAM, without replacing the hardware.

RAMFence is a small UEFI program that runs **before Windows starts**. It marks
the physical memory ranges you specify as unusable in the firmware memory map,
then starts the normal Windows Boot Manager. Windows never sees those ranges,
so it never stores anything in the damaged memory. It is the Windows equivalent
of Linux's `memmap=`/`badram` options.

It comes with **RAMFence Manager**, a Windows app that installs, configures and
checks it, can find the bad memory for you, and can create a rescue USB stick.

> **Status:** works on the machine it was developed for. Changing how a PC
> boots always carries some risk: read [Safety](#safety) before installing.

---

## Why

Laptops often have memory soldered to the mainboard. When part of it fails,
Windows crashes randomly, updates fail, and the usual fix is a new mainboard.
The options Windows offers are limited:

| Option | Problem |
|---|---|
| `bcdedit /set {badmemory} badmemorylist` | The list is stored in the BCD registry hive and is capped at roughly 130,000 pages (about 512 MiB). Larger defects do not fit. |
| `truncatememory` / msconfig "Maximum memory" | Throws away **all** memory above an address, including good memory. It is set per boot entry, so the setup environment of feature updates (SafeOS) ignores it, crashes on the bad memory and rolls the update back. |
| Kernel drivers that grab bad pages | Need driver signing, load after the kernel may already use the pages, and do not run in WinRE or SafeOS. |

RAMFence works one level lower, in the firmware, so **every** Windows boot path
(normal Windows, WinRE, the SafeOS phase of feature updates) gets a memory map
without the bad ranges, and the size of the bad area does not matter.

## How it works

```
Mainboard chip        EFI System Partition on the disk                     Windows partition (C:)
┌──────────────┐      ┌──────────────┐      ┌───────────────────────┐      ┌──────────────────┐
│   Firmware   │ ───> │ ramfence.efi │ ───> │  Windows Boot Manager │ ───> │  Windows kernel  │
│  (UEFI/BIOS) │      │              │      │    (bootmgfw.efi)     │      │                  │
└──────────────┘      └──────────────┘      └───────────────────────┘      └──────────────────┘
```

- The **firmware** (UEFI, often still called the BIOS) is stored on a chip on
  the mainboard and runs first. It initializes the memory and builds the
  memory map: the list of which physical memory is usable.
- **`ramfence.efi`** and **Windows Boot Manager** are both programs stored as
  files on the EFI System Partition. The firmware starts whichever comes first
  in its boot order, and RAMFence is placed first.
- RAMFence runs after the firmware and before Windows Boot Manager. It asks the
  firmware to mark the bad ranges as unusable, so Windows Boot Manager and
  Windows receive a memory map that already excludes them.

What `ramfence.efi` does on every boot:

```
Firmware ──> ramfence.efi ──────────────────────────> Windows Boot Manager ──> Windows
             1. read ramfence.cfg                       (bootmgfw.efi, unmodified)
             2. AllocatePages(EfiUnusableMemory) on each bad range
             3. re-read the memory map and verify
             4. set the RamFenceStatus UEFI variable
             5. show a short summary, then chainload Windows
```

- `ramfence.efi` is installed as its own firmware boot entry, placed first in the
  boot order. Windows' own boot files are never modified.
- Memory is reserved as `EfiUnusableMemory` ("memory with detected errors"),
  which Windows does not use.
- Every problem (a range that cannot be reserved, a missing config, ...) is
  shown on screen and handled according to `on_error`: ask, boot anyway, or
  stop. Nothing fails silently.
- At each logon, a scheduled check reads `RamFenceStatus`. If Windows started
  without going through RAMFence (for example because an update moved Windows
  Boot Manager back to the front of the boot order), it restores the boot
  order and tells you to restart.

## Features

- **Pre-boot memory test.** RAMFence can test memory before Windows starts,
  including the ranges it reserves (Windows itself can never reach them).
  Results show where the errors are and suggest ranges to reserve.
- **Setup wizard.** Step-by-step: prepare the PC (BitLocker, Secure Boot,
  Fast Startup), scan the memory, protect the bad ranges, verify.
- **Rescue USB.** For a PC that cannot start Windows any more: the stick scans
  the memory on its first start, reserves the damaged areas and starts
  Windows. From there, RAMFence can be installed on the PC.
- **Verification.** The Status page compares the protected ranges with the
  physical memory Windows actually uses.
- **Single file.** `RAMFence-Manager.exe` contains `ramfence.efi`.

## Requirements

- A 64-bit x86 PC that starts Windows in **UEFI** mode (not legacy BIOS/CSM).
- Windows 10 or 11, 64-bit, and administrator rights.
- **Secure Boot turned off.** `ramfence.efi` is not signed by Microsoft, so the
  firmware refuses to start it while Secure Boot is on. (Advanced users can
  keep Secure Boot on: see [Using Secure Boot](#using-secure-boot-advanced).)
- If **BitLocker** (or Windows "device encryption") is on: your recovery key.
  Changing the boot path makes BitLocker ask for it unless BitLocker is paused
  first. The wizard shows the key and can pause BitLocker for you.

## Quick start

1. Download or [build](#building-from-source) `RAMFence-Manager-<version>.exe`
   and run it as administrator.
2. Follow the **Setup** tab:
   1. **Prepare this PC.** Save the BitLocker recovery key and pause
      BitLocker if it is on, turn Secure Boot off (the app can restart into
      the firmware settings), turn Fast Startup off.
   2. **Find the bad memory.** "Scan all memory at next restart", then restart.
      Before Windows starts, RAMFence tests all memory (a few minutes; Esc stops
      it) and Windows then starts normally.
   3. **Protect the bad memory.** Review the suggested ranges (error areas
      plus a safety margin) and apply them.
   4. **Check that it works.** After one more restart, the Status tab should
      say *Protected*. If you used a memory limit (`truncatememory`/msconfig)
      as a workaround, remove it now.

Already know the bad addresses (for example from
[MemTest86](https://www.memtest86.com/))? Enter them in the **Configuration**
tab and install from the **Install / Uninstall** tab instead.

### Rescue USB stick

If the PC crashes before or while Windows starts:

1. On any working Windows PC, open RAMFence Manager → **USB Tool**, choose a USB
   stick (it will be erased) and create the rescue stick.
2. On the affected PC, turn Secure Boot off, then start from the stick (boot
   menu, often F12, F11, F8, Esc or F10 at the maker's logo).
3. The first start scans all memory and saves the result on the stick; it then
   reserves the damaged areas and starts Windows. Later starts on the same PC
   reuse the result; on another PC, or after a memory or firmware change, the
   stick scans again.
4. In Windows, run `RAMFence-Manager.exe` from the stick. The Setup tab offers to
   install RAMFence with the ranges the stick found.

## Configuration (`ramfence.cfg`)

RAMFence Manager writes this file for you. Format: one setting per line, `#`
starts a comment, addresses are hexadecimal physical addresses, the end of a
range is exclusive, and ranges are rounded outward to 4 KiB pages.

```ini
range 0x100000000 0x140000000   # repeatable
memory_type unusable            # unusable (default) | reserved
on_error prompt                 # prompt (default) | boot | halt
timeout 3                       # seconds the summary stays on screen
# mode protect                  # protect (default) | scan-only | auto
# chainload \EFI\Microsoft\Boot\bootmgfw.efi
```

| Mode | Meaning |
|---|---|
| `protect` | Reserve the configured ranges (normal use). |
| `scan-only` | Reserve nothing; only run a scheduled memory test. Used while setting up. |
| `auto` | Rescue USB: scan all memory unless a complete result saved on this PC with this memory exists, then reserve the error ranges from the result plus a 256 MiB margin. |

## The memory test

- Three algorithms per pass: each word holds its own address (finds address
  faults), moving inversions with two patterns, and pseudo-random data.
  Caches are written back and invalidated between writing and checking.
- Memory the firmware itself uses at that moment cannot be tested and is
  listed as untested.
- It is a quick check, not a replacement for a thorough MemTest86 run. Weak
  memory may only fail sometimes (for example when warm): run several passes
  before shrinking a protected range.

## Using Secure Boot (advanced)

> [!WARNING]
> **Changing Secure Boot keys can leave the PC unable to start.** A wrong
> choice in the firmware's key settings can lock out Windows itself, and
> BitLocker may ask for the recovery key. Firmware menus differ between
> makers and models, so this section only describes the general method.
> **If you are not familiar with firmware (BIOS) settings, do not try this.**
> Leaving Secure Boot off is the supported setup. If you need Secure Boot,
> ask someone experienced with PCs, or an AI agent, to help you with the
> steps for your specific model.

With Secure Boot on, the firmware only starts programs that are signed by a
trusted certificate or whose hash is listed in the **`db`** ("Authorized
Signatures") database. `ramfence.efi` is not signed by Microsoft, but you can
add its hash to `db`. Windows Boot Manager, which RAMFence starts next, is
signed by Microsoft and keeps working.

### Before you start

- Install RAMFence with Secure Boot **off** and check that the Status tab says
  *Protected*. That way you know RAMFence itself works.
- If BitLocker is on: save the recovery key somewhere outside this PC, and
  pause BitLocker (the Setup tab can do both). Changing Secure Boot settings
  or keys changes what BitLocker measures at startup.
- Copy `%ProgramFiles%\RAMFence\ramfence.efi` to a FAT32 USB stick. This is
  the exact file that is installed; the hash must be of this file.
- Find out how to undo the change on your PC (see *If something goes wrong*
  below) before you change anything.

### General steps

1. Restart into the firmware settings (the Setup tab can do this, or press the
   setup key at the maker's logo, often F2 or Del).
2. Find the Secure Boot settings, usually under *Security*, *Boot* or
   *Authentication*. On some PCs you must first change the Secure Boot mode
   from *Standard* to *Custom* to edit keys.
3. Open the key management and select **`db`** (*Authorized Signatures*).
4. Choose **Append** / **Add**, never *Set*, *Replace*, *Delete* or *Clear*:
   those can remove Microsoft's certificates, and then Windows will not start.
   Do not change `PK`, `KEK` or `dbx`.
5. Select `ramfence.efi` on the USB stick. If the firmware asks for the type,
   choose the EFI image (PE/COFF) option: the firmware then computes the
   correct hash itself. The hash is an Authenticode image hash, **not** the
   SHA-256 of the whole file. If your firmware only accepts certificates or
   signature lists, it cannot add the file directly; stop here unless you know
   how to create a signature list.
6. Turn Secure Boot on, save and restart.
7. Check: the RAMFence summary appears during startup, the Status tab says
   *Protected*, and *System Information* (`msinfo32`) shows *Secure Boot
   State: On*. Resume BitLocker if you paused it.

### Updating RAMFence later

Every new version of `ramfence.efi` has a different hash. If the firmware does
not know the new hash, it refuses to start RAMFence and usually starts Windows
Boot Manager instead: **Windows then runs without protection**. To update:
turn Secure Boot off, update RAMFence, add the new file's hash to `db` as
above, then turn Secure Boot on again. Changing `ramfence.cfg` (ranges and
settings) does not change the hash.

A firmware (BIOS) update or *Restore factory keys* resets `db` and removes the
hash, with the same result. The logon check notices a start without RAMFence
and warns you, but only after that unprotected start.

Experts can instead create their own signing key, add its certificate to `db`
and sign `ramfence.efi` with it. Updates then only need to be signed again,
not enrolled again. Keep that private key safe: anything signed with it is
trusted by the PC.

### If something goes wrong

- **The PC does not start Windows, or shows a security violation:** enter the
  firmware settings again and turn Secure Boot off.
- **Windows itself is refused (keys damaged):** use the firmware's *Restore
  factory keys* / *Install default Secure Boot keys* option. This restores
  Microsoft's keys and removes your entry for `ramfence.efi`.
- **BitLocker asks for the recovery key:** enter the key you saved.

## Uninstalling

**Install / Uninstall** tab → *Uninstall*. Windows Boot Manager is put first
again and the RAMFence boot entry, its files on the EFI System Partition and the
logon check are removed. After the next restart Windows can use the bad memory
again, so restore a workaround first if you still need one.

If the PC does not start properly, select **Windows Boot Manager** in the
firmware's boot menu: it starts Windows without RAMFence.

## Safety

- RAMFence never modifies `bootmgfw.efi` or other Windows boot files. Before
  changing anything, the installer exports a BCD backup to
  `%ProgramFiles%\RAMFence`.
- Reserving a range too small leaves damaged memory in use. Keep a margin
  around the errors and test before shrinking ranges.
- Firmware updates or memory changes can move physical addresses. Scan again
  afterwards.
- The software is provided as is, without warranty. You use it at your own
  risk.

## Building from source

Requirements: [Rust](https://rustup.rs/) with the UEFI target, and on the
Windows GNU toolchain MinGW-w64 binutils (`dlltool`, `as`) on `PATH`.

```powershell
rustup target add x86_64-unknown-uefi

# ramfence.efi
cargo build -p ramfence-efi --target x86_64-unknown-uefi --release

# tests (pure logic: config, ranges, memory map planning, memory test engine)
cargo test -p ramfence-core -p ramfence-manager

# single-file release: dist\RAMFence-Manager-<version>.exe (ramfence.efi embedded)
.\tools\build-release.ps1
```

`tools\qemu-test.ps1` runs `ramfence.efi` under QEMU with OVMF firmware and
checks the resulting memory map, status variable and memory test results.

## Project layout

| Path | Contents |
|---|---|
| `ramfence-core/` | `no_std` logic shared by everything: config parser, range handling, memory map planning and verification, status format, memory test engine and file formats. |
| `ramfence-efi/` | The UEFI application (`ramfence.efi`), built on the [`uefi`](https://crates.io/crates/uefi) crate. |
| `ramfence-manager/` | RAMFence Manager, the Windows app ([egui](https://github.com/emilk/egui)). |
| `windows/` | PowerShell scripts for installing, checking and uninstalling without the app. |
| `tools/` | Release build and QEMU test scripts. |

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.
