//! Embeds ramfence.efi into ramfence-manager.exe when it has been built, so the
//! manager is a single file to download. Build the UEFI target first:
//!   cargo build -p ramfence-efi --target x86_64-unknown-uefi --release
//! or point RAMFENCE_EFI at a ramfence.efi. Without one, the manager still
//! builds and asks for the file at install time.

use std::path::PathBuf;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(embedded_efi)");
    println!("cargo::rerun-if-env-changed=RAMFENCE_EFI");

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let src = std::env::var_os("RAMFENCE_EFI")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../target/x86_64-unknown-uefi/release/ramfence.efi"));
    println!("cargo::rerun-if-changed={}", src.display());

    if src.is_file() {
        let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("ramfence.efi");
        std::fs::copy(&src, &out).expect("copy ramfence.efi into OUT_DIR");
        println!("cargo::rustc-cfg=embedded_efi");
    } else {
        println!(
            "cargo::warning=ramfence.efi not found at {}; building without an embedded copy",
            src.display()
        );
    }
}
