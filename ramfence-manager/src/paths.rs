//! Names and locations shared with windows/*.ps1, so an install made by
//! either tool can be managed by the other.

use std::path::PathBuf;

/// Must match STATUS_VENDOR in ramfence-efi/src/main.rs.
pub const STATUS_VENDOR_GUID: &str = "{0B0C86EE-7039-477B-9A53-E39345EF90D5}";
pub const STATUS_VARIABLE: &str = "RamFenceStatus";

pub const ENTRY_DESCRIPTION: &str = "RAMFence";
/// Directory on the ESP, relative to its root.
pub const ESP_DIR: &str = r"EFI\ramfence";
/// Path written into the firmware boot entry.
pub const ESP_EFI_PATH: &str = r"\EFI\ramfence\ramfence.efi";

pub const TASK_NAME: &str = "RAMFence boot check";
pub const EXE_NAME: &str = "ramfence-manager.exe";
pub const EFI_NAME: &str = "ramfence.efi";
pub const CFG_NAME: &str = "ramfence.cfg";

/// `%ProgramFiles%\RAMFence`: writable only by administrators, which matters
/// because the logon check runs elevated from here.
pub fn install_dir() -> PathBuf {
    let base = std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into());
    PathBuf::from(base).join("RAMFence")
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

/// ramfence.efi bundled into this executable at build time (see build.rs).
#[cfg(embedded_efi)]
pub const EMBEDDED_EFI: Option<&[u8]> = Some(include_bytes!(concat!(env!("OUT_DIR"), "/ramfence.efi")));
#[cfg(not(embedded_efi))]
pub const EMBEDDED_EFI: Option<&[u8]> = None;

/// Where to look for ramfence.efi to install: next to this exe, the install
/// directory, then the repository's release build (when run from target\).
pub fn efi_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        v.push(dir.join(EFI_NAME));
        // target\{debug,release}\ramfence-manager.exe -> target\x86_64-unknown-uefi\release
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
