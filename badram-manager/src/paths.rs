//! Names and locations shared with windows/*.ps1, so an install made by
//! either tool can be managed by the other.

use std::path::PathBuf;

/// Must match STATUS_VENDOR in badram-efi/src/main.rs.
pub const STATUS_VENDOR_GUID: &str = "{5B7C3F1E-9A42-4D8E-B6A1-2F0C8E4D7A93}";
pub const STATUS_VARIABLE: &str = "BadRamStatus";

pub const ENTRY_DESCRIPTION: &str = "BadRAM";
/// Directory on the ESP, relative to its root.
pub const ESP_DIR: &str = r"EFI\badram";
/// Path written into the firmware boot entry.
pub const ESP_EFI_PATH: &str = r"\EFI\badram\badram.efi";

pub const TASK_NAME: &str = "BadRAM boot check";
pub const EXE_NAME: &str = "badram-manager.exe";
pub const EFI_NAME: &str = "badram.efi";
pub const CFG_NAME: &str = "badram.cfg";

/// `%ProgramFiles%\BadRAM`: writable only by administrators, which matters
/// because the logon check runs elevated from here.
pub fn install_dir() -> PathBuf {
    let base = std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into());
    PathBuf::from(base).join("BadRAM")
}

pub fn state_file() -> PathBuf {
    install_dir().join("state.json")
}

pub fn log_file() -> PathBuf {
    install_dir().join("check.log")
}

pub fn installed_cfg() -> PathBuf {
    install_dir().join(CFG_NAME)
}

/// badram.efi bundled into this executable at build time (see build.rs).
#[cfg(embedded_efi)]
pub const EMBEDDED_EFI: Option<&[u8]> = Some(include_bytes!(concat!(env!("OUT_DIR"), "/badram.efi")));
#[cfg(not(embedded_efi))]
pub const EMBEDDED_EFI: Option<&[u8]> = None;

/// Where to look for badram.efi to install: next to this exe, the install
/// directory, then the repository's release build (when run from target\).
pub fn efi_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        v.push(dir.join(EFI_NAME));
        // target\{debug,release}\badram-manager.exe -> target\x86_64-unknown-uefi\release
        if let Some(target) = dir.parent() {
            v.push(target.join(r"x86_64-unknown-uefi\release").join(EFI_NAME));
        }
    }
    v.push(install_dir().join(EFI_NAME));
    v
}

pub fn find_efi() -> Option<PathBuf> {
    efi_candidates().into_iter().find(|p| p.is_file())
}
