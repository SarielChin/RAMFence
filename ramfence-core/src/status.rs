//! Contents of the volatile `RamFenceStatus` UEFI variable.
//!
//! Format (ASCII, `;`-separated `key=value`):
//!
//! ```text
//! version=1;ok=1;ranges=0x...-0x...,0x...-0x...;pages=N;type=unusable[;mode=scan-only][;error=...]
//! ```

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::config::{MemoryType, Mode};
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
    mode: Mode,
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
    if mode != Mode::Protect {
        let _ = write!(s, ";mode={}", mode.name());
    }
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

/// Parsed contents of `RamFenceStatus`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusInfo {
    pub version: u32,
    pub ok: bool,
    pub ranges: Vec<Range>,
    pub pages: u64,
    /// `None` if the type field is missing or unknown.
    pub memory_type: Option<MemoryType>,
    /// ramfence.efi ran in scan-only mode and reserved nothing on purpose.
    pub scan_only: bool,
    pub error: Option<String>,
}

/// Parses a status string written by [`format_status`]. Unknown keys are
/// ignored; a missing or malformed `version`/`ok` is an error. A trailing
/// NUL (as read back from firmware) is tolerated.
pub fn parse_status(text: &str) -> Option<StatusInfo> {
    let text = text.trim_end_matches('\0');
    let mut version = None;
    let mut ok = None;
    let mut ranges = Vec::new();
    let mut pages = 0;
    let mut memory_type = None;
    let mut scan_only = false;
    let mut error = None;

    for field in text.split(';') {
        let (key, value) = field.split_once('=')?;
        match key {
            "version" => version = Some(value.parse().ok()?),
            "ok" => {
                ok = Some(match value {
                    "1" => true,
                    "0" => false,
                    _ => return None,
                })
            }
            "ranges" => {
                for part in value.split(',').filter(|p| !p.is_empty()) {
                    let (a, b) = part.split_once('-')?;
                    let a = crate::config::parse_address(a).ok()?;
                    let b = crate::config::parse_address(b).ok()?;
                    ranges.push(Range::new(a, b).ok()?);
                }
            }
            "pages" => pages = value.parse().ok()?,
            "type" => {
                memory_type = match value {
                    "unusable" => Some(MemoryType::Unusable),
                    "reserved" => Some(MemoryType::Reserved),
                    _ => None,
                }
            }
            "mode" => scan_only = value == Mode::ScanOnly.name(),
            "error" => error = Some(String::from(value)),
            _ => {}
        }
    }

    Some(StatusInfo {
        version: version?,
        ok: ok?,
        ranges,
        pages,
        memory_type,
        scan_only,
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trips() {
        let ranges = [r(0x1000, 0x2000), r(0x5000, 0x6000)];
        let s = format_status(false, &ranges, 2, MemoryType::Reserved, Mode::Protect, Some("x; y"));
        assert_eq!(
            parse_status(&s),
            Some(StatusInfo {
                version: 1,
                ok: false,
                ranges: ranges.to_vec(),
                pages: 2,
                memory_type: Some(MemoryType::Reserved),
                scan_only: false,
                error: Some("x, y".into()),
            })
        );
        let ok = format_status(true, &[r(0x1000, 0x2000)], 1, MemoryType::Unusable, Mode::Protect, None);
        let mut with_nul = ok.clone();
        with_nul.push('\0');
        let parsed = parse_status(&with_nul).unwrap();
        assert!(parsed.ok);
        assert_eq!(parsed.error, None);
        assert_eq!(parse_status(&ok), Some(parsed));
    }

    #[test]
    fn scan_only_mode_round_trips() {
        let s = format_status(true, &[], 0, MemoryType::Unusable, Mode::ScanOnly, None);
        assert_eq!(s, "version=1;ok=1;ranges=;pages=0;type=unusable;mode=scan-only");
        let info = parse_status(&s).unwrap();
        assert!(info.scan_only && info.ok && info.ranges.is_empty());
        assert!(!parse_status("version=1;ok=1").unwrap().scan_only);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!(parse_status(""), None);
        assert_eq!(parse_status("hello"), None);
        assert_eq!(parse_status("version=1"), None); // no ok
        assert_eq!(parse_status("version=1;ok=2"), None);
        assert_eq!(parse_status("version=1;ok=1;ranges=0x2000-0x1000"), None);
        assert!(parse_status("version=1;ok=1;future=thing").is_some());
    }

    fn r(start: u64, end: u64) -> Range {
        Range::new(start, end).unwrap()
    }

    #[test]
    fn ok_single_range() {
        assert_eq!(
            format_status(true, &[r(0x1_0000_0000, 0x1_1000_0000)], 65536, MemoryType::Unusable, Mode::Protect, None),
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
                Mode::Protect,
                Some("BootServicesData at 0x5000; cannot allocate\n")
            ),
            "version=1;ok=0;ranges=0x1000-0x2000,0x5000-0x6000;pages=1;type=reserved;\
             error=BootServicesData at 0x5000, cannot allocate?"
        );
    }

    #[test]
    fn no_ranges() {
        assert_eq!(
            format_status(false, &[], 0, MemoryType::Unusable, Mode::Protect, Some("config missing")),
            "version=1;ok=0;ranges=;pages=0;type=unusable;error=config missing"
        );
    }
}
