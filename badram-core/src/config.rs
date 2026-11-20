//! Parser for `badram.cfg` (see DESIGN.md §3).
//!
//! Line-oriented, `#` starts a comment (anywhere on the line). Keys:
//!
//! ```text
//! range <start> <end>          # hex, 0x prefix required, end exclusive; repeatable
//! memory_type unusable|reserved
//! on_error prompt|boot|halt
//! timeout <seconds>            # decimal
//! chainload <\path\to\file.efi>
//! ```
//!
//! Anything unexpected is an error: unknown keys, repeated single-value keys,
//! malformed numbers, and a config with no `range` at all.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::range::{Range, RangeError};

/// UEFI memory type used for the allocation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MemoryType {
    /// `EfiUnusableMemory` (8).
    #[default]
    Unusable,
    /// `EfiReservedMemoryType` (0).
    Reserved,
}

impl MemoryType {
    /// Numeric `EFI_MEMORY_TYPE` value.
    pub const fn efi_type(self) -> u32 {
        match self {
            MemoryType::Unusable => crate::memmap::mem_type::UNUSABLE,
            MemoryType::Reserved => crate::memmap::mem_type::RESERVED,
        }
    }

    /// Name as written in the config file.
    pub const fn name(self) -> &'static str {
        match self {
            MemoryType::Unusable => "unusable",
            MemoryType::Reserved => "reserved",
        }
    }
}

/// What to do when something goes wrong (DESIGN.md §2.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnError {
    /// Show the error and wait for B (boot) / R (reboot) / S (shutdown).
    #[default]
    Prompt,
    /// Show a warning and boot Windows anyway.
    Boot,
    /// Stop; do not boot.
    Halt,
}

impl OnError {
    pub const fn name(self) -> &'static str {
        match self {
            OnError::Prompt => "prompt",
            OnError::Boot => "boot",
            OnError::Halt => "halt",
        }
    }
}

pub const DEFAULT_TIMEOUT_SECS: u32 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Ranges exactly as written (not yet aligned or merged; see
    /// [`crate::range::normalize`]).
    pub ranges: Vec<Range>,
    pub memory_type: MemoryType,
    pub on_error: OnError,
    pub timeout_secs: u32,
    /// Override for the chainload target path, e.g. `\EFI\Microsoft\Boot\bootmgfw.efi`.
    pub chainload: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line number; 0 for errors about the file as a whole.
    pub line: usize,
    pub kind: ParseErrorKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseErrorKind {
    UnknownKey(String),
    /// Wrong number of arguments for `key`.
    WrongArgCount { key: &'static str, expected: usize, got: usize },
    /// A single-value key appeared more than once.
    DuplicateKey(&'static str),
    /// Address not of the form `0x<hex>` or out of range.
    InvalidAddress(String),
    InvalidTimeout(String),
    InvalidValue { key: &'static str, value: String },
    /// `chainload` path must start with `\`.
    InvalidPath(String),
    Range(RangeError),
    /// The file contains no `range` line.
    NoRanges,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line > 0 {
            write!(f, "badram.cfg line {}: ", self.line)?;
        } else {
            write!(f, "badram.cfg: ")?;
        }
        match &self.kind {
            ParseErrorKind::UnknownKey(k) => write!(f, "unknown key '{k}'"),
            ParseErrorKind::WrongArgCount { key, expected, got } => {
                write!(f, "'{key}' takes {expected} value(s), got {got}")
            }
            ParseErrorKind::DuplicateKey(k) => write!(f, "'{k}' given more than once"),
            ParseErrorKind::InvalidAddress(s) => {
                write!(f, "invalid address '{s}' (expected hex like 0x1000)")
            }
            ParseErrorKind::InvalidTimeout(s) => write!(f, "invalid timeout '{s}'"),
            ParseErrorKind::InvalidValue { key, value } => {
                write!(f, "invalid value '{value}' for '{key}'")
            }
            ParseErrorKind::InvalidPath(s) => {
                write!(f, "chainload path '{s}' must start with '\\'")
            }
            ParseErrorKind::Range(e) => write!(f, "{e}"),
            ParseErrorKind::NoRanges => write!(f, "no 'range' line found"),
        }
    }
}

impl core::error::Error for ParseError {}

/// Parses the contents of `badram.cfg`. Accepts LF or CRLF line endings and a
/// leading UTF-8 BOM (Notepad adds one).
pub fn parse(text: &str) -> Result<Config, ParseError> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);

    let mut ranges = Vec::new();
    let mut memory_type = None;
    let mut on_error = None;
    let mut timeout_secs = None;
    let mut chainload = None;

    for (idx, raw) in text.lines().enumerate() {
        let line = idx + 1;
        let err = |kind| ParseError { line, kind };

        let content = match raw.find('#') {
            Some(i) => &raw[..i],
            None => raw,
        }
        .trim();
        if content.is_empty() {
            continue;
        }

        let (key, rest) = match content.find(char::is_whitespace) {
            Some(i) => (&content[..i], content[i..].trim_start()),
            None => (content, ""),
        };
        let args: Vec<&str> = rest.split_whitespace().collect();

        match key {
            "range" => {
                expect_args("range", &args, 2).map_err(err)?;
                let start = parse_address(args[0]).map_err(err)?;
                let end = parse_address(args[1]).map_err(err)?;
                let r = Range::new(start, end).map_err(|e| err(ParseErrorKind::Range(e)))?;
                // Reject now so the user sees the line number.
                r.align_outward().map_err(|e| err(ParseErrorKind::Range(e)))?;
                ranges.push(r);
            }
            "memory_type" => {
                expect_args("memory_type", &args, 1).map_err(err)?;
                let v = match_value("memory_type", args[0], &[
                    ("unusable", MemoryType::Unusable),
                    ("reserved", MemoryType::Reserved),
                ])
                .map_err(err)?;
                set_once("memory_type", &mut memory_type, v).map_err(err)?;
            }
            "on_error" => {
                expect_args("on_error", &args, 1).map_err(err)?;
                let v = match_value("on_error", args[0], &[
                    ("prompt", OnError::Prompt),
                    ("boot", OnError::Boot),
                    ("halt", OnError::Halt),
                ])
                .map_err(err)?;
                set_once("on_error", &mut on_error, v).map_err(err)?;
            }
            "timeout" => {
                expect_args("timeout", &args, 1).map_err(err)?;
                let v: u32 = args[0]
                    .parse()
                    .map_err(|_| err(ParseErrorKind::InvalidTimeout(args[0].into())))?;
                set_once("timeout", &mut timeout_secs, v).map_err(err)?;
            }
            "chainload" => {
                // The path is the rest of the line, so it may contain spaces.
                if rest.is_empty() {
                    return Err(err(ParseErrorKind::WrongArgCount {
                        key: "chainload",
                        expected: 1,
                        got: 0,
                    }));
                }
                if !rest.starts_with('\\') {
                    return Err(err(ParseErrorKind::InvalidPath(rest.into())));
                }
                set_once("chainload", &mut chainload, String::from(rest)).map_err(err)?;
            }
            other => return Err(err(ParseErrorKind::UnknownKey(other.into()))),
        }
    }

    if ranges.is_empty() {
        return Err(ParseError {
            line: 0,
            kind: ParseErrorKind::NoRanges,
        });
    }

    Ok(Config {
        ranges,
        memory_type: memory_type.unwrap_or_default(),
        on_error: on_error.unwrap_or_default(),
        timeout_secs: timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
        chainload,
    })
}

fn expect_args(key: &'static str, args: &[&str], expected: usize) -> Result<(), ParseErrorKind> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(ParseErrorKind::WrongArgCount {
            key,
            expected,
            got: args.len(),
        })
    }
}

fn set_once<T>(key: &'static str, slot: &mut Option<T>, value: T) -> Result<(), ParseErrorKind> {
    if slot.is_some() {
        return Err(ParseErrorKind::DuplicateKey(key));
    }
    *slot = Some(value);
    Ok(())
}

fn match_value<T: Copy>(
    key: &'static str,
    value: &str,
    options: &[(&str, T)],
) -> Result<T, ParseErrorKind> {
    options
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(value))
        .map(|&(_, v)| v)
        .ok_or_else(|| ParseErrorKind::InvalidValue {
            key,
            value: value.into(),
        })
}

impl Config {
    /// Serializes back to `badram.cfg` syntax. `parse(&c.to_text()) == Ok(c)`.
    pub fn to_text(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        // Writing to a String cannot fail.
        for r in &self.ranges {
            let _ = writeln!(s, "range {:#x} {:#x}", r.start(), r.end());
        }
        let _ = writeln!(s, "memory_type {}", self.memory_type.name());
        let _ = writeln!(s, "on_error {}", self.on_error.name());
        let _ = writeln!(s, "timeout {}", self.timeout_secs);
        if let Some(path) = &self.chainload {
            let _ = writeln!(s, "chainload {path}");
        }
        s
    }
}

/// Parses `0x`-prefixed hex; `_` separators are allowed (`0x3_5000_0000`).
pub fn parse_address(s: &str) -> Result<u64, ParseErrorKind> {
    let invalid = || ParseErrorKind::InvalidAddress(s.into());
    let hex = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .ok_or_else(invalid)?;
    if hex.is_empty() || hex.starts_with('_') || hex.ends_with('_') {
        return Err(invalid());
    }
    let mut value: u64 = 0;
    for c in hex.chars() {
        if c == '_' {
            continue;
        }
        let d = c.to_digit(16).ok_or_else(invalid)?;
        value = value
            .checked_mul(16)
            .and_then(|v| v.checked_add(u64::from(d)))
            .ok_or_else(invalid)?;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(start: u64, end: u64) -> Range {
        Range::new(start, end).unwrap()
    }

    fn kind(text: &str) -> (usize, ParseErrorKind) {
        let e = parse(text).unwrap_err();
        (e.line, e.kind)
    }

    #[test]
    fn full_example() {
        let text = "\
# comment line
range 0x100000000 0x110000000
range 0x200000000 0x200001000   # trailing comment

memory_type reserved
on_error halt
timeout 10
chainload \\EFI\\Some Dir\\shell.efi
";
        let c = parse(text).unwrap();
        assert_eq!(
            c,
            Config {
                ranges: vec![r(0x1_0000_0000, 0x1_1000_0000), r(0x2_0000_0000, 0x2_0000_1000)],
                memory_type: MemoryType::Reserved,
                on_error: OnError::Halt,
                timeout_secs: 10,
                chainload: Some("\\EFI\\Some Dir\\shell.efi".into()),
            }
        );
    }

    #[test]
    fn defaults() {
        let c = parse("range 0x1000 0x2000").unwrap();
        assert_eq!(c.memory_type, MemoryType::Unusable);
        assert_eq!(c.on_error, OnError::Prompt);
        assert_eq!(c.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(c.chainload, None);
    }

    #[test]
    fn crlf_bom_tabs_and_case() {
        let text = "\u{FEFF}range\t0X1000\t0xABCdef\r\nmemory_type  UNUSABLE\r\non_error Boot\r\n";
        let c = parse(text).unwrap();
        assert_eq!(c.ranges, vec![r(0x1000, 0xAB_CDEF)]);
        assert_eq!(c.memory_type, MemoryType::Unusable);
        assert_eq!(c.on_error, OnError::Boot);
    }

    #[test]
    fn underscores_in_addresses() {
        let c = parse("range 0x1_0000_0000 0x1_1000_0000").unwrap();
        assert_eq!(c.ranges, vec![r(0x1_0000_0000, 0x1_1000_0000)]);
        for bad in ["0x_1000", "0x1000_", "0x"] {
            let text = alloc::format!("range {bad} 0x2000");
            assert!(matches!(kind(&text), (1, ParseErrorKind::InvalidAddress(_))), "{bad}");
        }
    }

    #[test]
    fn unaligned_range_is_kept_as_written() {
        // Alignment happens in range::normalize, not in the parser.
        let c = parse("range 0x1234 0x5678").unwrap();
        assert_eq!(c.ranges, vec![r(0x1234, 0x5678)]);
    }

    #[test]
    fn no_ranges() {
        assert_eq!(kind(""), (0, ParseErrorKind::NoRanges));
        assert_eq!(kind("# only comments\n\ntimeout 1\n"), (0, ParseErrorKind::NoRanges));
    }

    #[test]
    fn unknown_key_reports_line() {
        assert_eq!(
            kind("range 0x1000 0x2000\n\nrnage 0x1 0x2\n"),
            (3, ParseErrorKind::UnknownKey("rnage".into()))
        );
    }

    #[test]
    fn wrong_arg_counts() {
        assert_eq!(
            kind("range 0x1000"),
            (1, ParseErrorKind::WrongArgCount { key: "range", expected: 2, got: 1 })
        );
        assert_eq!(
            kind("range 0x1000 0x2000 0x3000"),
            (1, ParseErrorKind::WrongArgCount { key: "range", expected: 2, got: 3 })
        );
        assert_eq!(
            kind("range 0x1000 0x2000\ntimeout"),
            (2, ParseErrorKind::WrongArgCount { key: "timeout", expected: 1, got: 0 })
        );
        assert_eq!(
            kind("range 0x1000 0x2000\nchainload"),
            (2, ParseErrorKind::WrongArgCount { key: "chainload", expected: 1, got: 0 })
        );
    }

    #[test]
    fn decimal_address_rejected() {
        assert_eq!(
            kind("range 4096 0x2000"),
            (1, ParseErrorKind::InvalidAddress("4096".into()))
        );
    }

    #[test]
    fn address_overflow_rejected() {
        assert!(matches!(
            kind("range 0x1 0x10000000000000000"),
            (1, ParseErrorKind::InvalidAddress(_))
        ));
    }

    #[test]
    fn empty_and_reversed_ranges_rejected() {
        assert_eq!(
            kind("range 0x2000 0x2000"),
            (1, ParseErrorKind::Range(RangeError::Empty { start: 0x2000, end: 0x2000 }))
        );
        assert!(matches!(kind("range 0x3000 0x2000"), (1, ParseErrorKind::Range(_))));
    }

    #[test]
    fn unalignable_range_rejected() {
        assert!(matches!(
            kind("range 0x0 0xFFFFFFFFFFFFFFFF"),
            (1, ParseErrorKind::Range(RangeError::AlignOverflow { .. }))
        ));
    }

    #[test]
    fn duplicate_single_value_keys() {
        assert_eq!(
            kind("range 0x1000 0x2000\non_error boot\non_error halt"),
            (3, ParseErrorKind::DuplicateKey("on_error"))
        );
        assert_eq!(
            kind("range 0x1000 0x2000\nmemory_type unusable\nmemory_type unusable"),
            (3, ParseErrorKind::DuplicateKey("memory_type"))
        );
    }

    #[test]
    fn invalid_enum_values() {
        assert_eq!(
            kind("range 0x1000 0x2000\nmemory_type loaderdata"),
            (2, ParseErrorKind::InvalidValue { key: "memory_type", value: "loaderdata".into() })
        );
        assert_eq!(
            kind("range 0x1000 0x2000\non_error ignore"),
            (2, ParseErrorKind::InvalidValue { key: "on_error", value: "ignore".into() })
        );
    }

    #[test]
    fn invalid_timeout() {
        for bad in ["-1", "3s", "0x3", "99999999999"] {
            let text = alloc::format!("range 0x1000 0x2000\ntimeout {bad}");
            assert_eq!(kind(&text), (2, ParseErrorKind::InvalidTimeout(bad.into())), "{bad}");
        }
        assert_eq!(parse("range 0x1000 0x2000\ntimeout 0").unwrap().timeout_secs, 0);
    }

    #[test]
    fn chainload_must_be_absolute() {
        assert_eq!(
            kind("range 0x1000 0x2000\nchainload EFI\\x.efi"),
            (2, ParseErrorKind::InvalidPath("EFI\\x.efi".into()))
        );
    }

    #[test]
    fn error_display() {
        let e = parse("range 0x1000 0x2000\nfoo").unwrap_err();
        assert_eq!(e.to_string(), "badram.cfg line 2: unknown key 'foo'");
        let e = parse("").unwrap_err();
        assert_eq!(e.to_string(), "badram.cfg: no 'range' line found");
    }

    #[test]
    fn to_text_round_trips() {
        let original = Config {
            ranges: vec![r(0x1234, 0x5678), r(0x1_0000_0000, 0x1_1000_0000)],
            memory_type: MemoryType::Reserved,
            on_error: OnError::Halt,
            timeout_secs: 7,
            chainload: Some("\\EFI\\Some Dir\\x.efi".into()),
        };
        let text = original.to_text();
        assert_eq!(
            text,
            "range 0x1234 0x5678\nrange 0x100000000 0x110000000\nmemory_type reserved\n\
             on_error halt\ntimeout 7\nchainload \\EFI\\Some Dir\\x.efi\n"
        );
        assert_eq!(parse(&text), Ok(original));

        let minimal = parse("range 0x1000 0x2000").unwrap();
        assert_eq!(parse(&minimal.to_text()), Ok(minimal));
    }

    #[test]
    fn memory_type_values() {
        assert_eq!(MemoryType::Unusable.efi_type(), 8);
        assert_eq!(MemoryType::Reserved.efi_type(), 0);
    }
}
