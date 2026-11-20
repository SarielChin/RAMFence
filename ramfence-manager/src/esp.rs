//! Temporarily mount the EFI System Partition with `mountvol X: /S`.

use std::path::{Path, PathBuf};

use crate::win::{run, run_ok};

fn free_drive_letter() -> Option<char> {
    ('G'..='Z').rev().find(|c| !Path::new(&format!("{c}:\\")).exists())
}

/// Mounts the ESP, runs `f` with its root (e.g. `Z:\`), then unmounts.
pub fn with_esp<T>(f: impl FnOnce(&Path) -> Result<T, String>) -> Result<T, String> {
    let letter = free_drive_letter().ok_or("no free drive letter to mount the EFI System Partition")?;
    let drive = format!("{letter}:");
    run_ok("mountvol.exe", &[&drive, "/S"])?;
    let result = f(&PathBuf::from(format!("{drive}\\")));
    let _ = run("mountvol.exe", &[&drive, "/D"]);
    result
}
