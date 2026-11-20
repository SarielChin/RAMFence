//! Contents of the volatile `BadRamStatus` UEFI variable (DESIGN.md §2.5).
//!
//! Format (ASCII, `;`-separated `key=value`):
//!
//! ```text
//! version=1;ok=1;ranges=0x...-0x...,0x...-0x...;pages=N;type=unusable[;error=...]
//! ```

use alloc::string::String;
use core::fmt::Write;

use crate::config::MemoryType;
use crate::range::Range;

pub const STATUS_VERSION: u32 = 1;

/// Builds the status string. `error` is a short human-readable summary when
/// `ok` is false; `;` and non-printable/non-ASCII characters in it are
/// replaced so the string stays parseable.
pub fn format_status(
    ok: bool,
    ranges: &[Range],
    pages: u64,
    memory_type: MemoryType,
    error: Option<&str>,
) -> String {
    let mut s = String::new();
    // Writing to a String cannot fail.
    let _ = write!(s, "version={STATUS_VERSION};ok={};ranges=", u8::from(ok));
    for (i, r) in ranges.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{r}");
    }
    let _ = write!(s, ";pages={pages};type={}", memory_type.name());
    if let Some(e) = error {
        s.push_str(";error=");
        s.extend(e.chars().map(|c| match c {
            ';' => ',',
            c if c.is_ascii_graphic() || c == ' ' => c,
            _ => '?',
        }));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(start: u64, end: u64) -> Range {
        Range::new(start, end).unwrap()
    }

    #[test]
    fn ok_single_range() {
        assert_eq!(
            format_status(true, &[r(0x1_0000_0000, 0x1_1000_0000)], 65536, MemoryType::Unusable, None),
            "version=1;ok=1;ranges=0x100000000-0x110000000;pages=65536;type=unusable"
        );
    }

    #[test]
    fn multiple_ranges_and_error() {
        assert_eq!(
            format_status(
                false,
                &[r(0x1000, 0x2000), r(0x5000, 0x6000)],
                1,
                MemoryType::Reserved,
                Some("BootServicesData at 0x5000; cannot allocate\n")
            ),
            "version=1;ok=0;ranges=0x1000-0x2000,0x5000-0x6000;pages=1;type=reserved;\
             error=BootServicesData at 0x5000, cannot allocate?"
        );
    }

    #[test]
    fn no_ranges() {
        assert_eq!(
            format_status(false, &[], 0, MemoryType::Unusable, Some("config missing")),
            "version=1;ok=0;ranges=;pages=0;type=unusable;error=config missing"
        );
    }
}
