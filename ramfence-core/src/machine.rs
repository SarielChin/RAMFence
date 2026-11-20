//! Identifies the PC and its memory configuration, so that a saved memory
//! test result is only reused where it is still valid.
//!
//! The id is a 64-bit FNV-1a hash over SMBIOS data and the RAM extent from
//! the UEFI memory map:
//! - system: UUID, manufacturer, product name, serial number (type 1)
//! - firmware: BIOS version and release date (type 0)
//! - every memory device: size, slot, manufacturer, serial and part number
//!   (type 17)
//!
//! It changes on another PC, after a firmware update (which can change how
//! physical addresses map to the memory chips), and when a memory module is
//! added, removed or swapped.

use crate::range::Range;

const ID_VERSION: &[u8] = b"ramfence-machine-1";

/// Locates the SMBIOS structure table from an SMBIOS entry point structure
/// (`_SM3_`, 64-bit, at least 0x18 bytes; or `_SM_`, 32-bit, at least 0x1F
/// bytes). Returns the table's physical address and its (maximum) length.
pub fn table_location(entry: &[u8]) -> Option<(u64, usize)> {
    if entry.starts_with(b"_SM3_") {
        let len = u32::from_le_bytes(entry.get(0x0C..0x10)?.try_into().ok()?) as usize;
        let addr = u64::from_le_bytes(entry.get(0x10..0x18)?.try_into().ok()?);
        Some((addr, len))
    } else if entry.starts_with(b"_SM_") {
        let len = u16::from_le_bytes(entry.get(0x16..0x18)?.try_into().ok()?) as usize;
        let addr = u32::from_le_bytes(entry.get(0x18..0x1C)?.try_into().ok()?) as u64;
        Some((addr, len))
    } else {
        None
    }
}

/// Length of the entry point structure that starts with `anchor` (its first
/// 5 bytes), or `None` if it is not an SMBIOS entry point.
pub fn entry_point_len(anchor: &[u8]) -> Option<usize> {
    if anchor.starts_with(b"_SM3_") {
        Some(0x18)
    } else if anchor.starts_with(b"_SM_") {
        Some(0x1F)
    } else {
        None
    }
}

/// One SMBIOS structure: the formatted area (header included, so offsets
/// match the specification) and the string set that follows it.
struct Structure<'a> {
    ty: u8,
    data: &'a [u8],
    strings: &'a [u8],
}

impl<'a> Structure<'a> {
    /// The string whose 1-based index is stored at `offset`; empty if absent.
    fn string_at(&self, offset: usize) -> &'a [u8] {
        match self.data.get(offset) {
            Some(&n) if n > 0 => self.strings.split(|&b| b == 0).nth(n as usize - 1).unwrap_or(&[]),
            _ => &[],
        }
    }

    fn bytes_at(&self, start: usize, len: usize) -> &'a [u8] {
        self.data.get(start..start + len).unwrap_or(&[])
    }
}

/// Iterates over the structures of an SMBIOS table. Stops at the end-of-table
/// structure (type 127) or at the first malformed structure.
struct Structures<'a> {
    table: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for Structures<'a> {
    type Item = Structure<'a>;

    fn next(&mut self) -> Option<Structure<'a>> {
        let t = self.table;
        let start = self.pos;
        let ty = *t.get(start)?;
        let len = *t.get(start + 1)? as usize;
        if len < 4 || start + len > t.len() {
            self.pos = t.len();
            return None;
        }
        // The string set ends with two NULs (just "\0\0" if it is empty).
        let strings_start = start + len;
        let mut end = strings_start;
        loop {
            if end + 1 >= t.len() {
                self.pos = t.len();
                return None;
            }
            if t[end] == 0 && t[end + 1] == 0 {
                break;
            }
            end += 1;
        }
        self.pos = if ty == 127 { t.len() } else { end + 2 };
        Some(Structure { ty, data: &t[start..strings_start], strings: &t[strings_start..end] })
    }
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.0 ^= u64::from(x);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    /// Length-prefixed, so adjacent fields cannot run into each other.
    fn field(&mut self, b: &[u8]) {
        self.bytes(&(b.len() as u32).to_le_bytes());
        self.bytes(b);
    }
}

/// Computes the machine id from the SMBIOS structure table (if available)
/// and the RAM extent of the memory map (if known).
pub fn machine_id(smbios_table: Option<&[u8]>, ram: Option<Range>) -> u64 {
    let mut h = Fnv::new();
    h.field(ID_VERSION);
    match ram {
        Some(r) => {
            h.field(&r.start().to_le_bytes());
            h.field(&r.end().to_le_bytes());
        }
        None => h.field(b"no-ram-extent"),
    }
    let structures = Structures { table: smbios_table.unwrap_or(&[]), pos: 0 };
    for s in structures {
        match s.ty {
            0 => {
                h.field(&[0]);
                h.field(s.string_at(0x05)); // BIOS version
                h.field(s.string_at(0x08)); // BIOS release date
            }
            1 => {
                h.field(&[1]);
                let uuid = s.bytes_at(0x08, 16);
                // All-zero / all-FF means "not set": not an identifier.
                if uuid.iter().all(|&b| b == 0) || uuid.iter().all(|&b| b == 0xFF) {
                    h.field(&[]);
                } else {
                    h.field(uuid);
                }
                h.field(s.string_at(0x04)); // manufacturer
                h.field(s.string_at(0x05)); // product name
                h.field(s.string_at(0x07)); // serial number
            }
            17 => {
                h.field(&[17]);
                h.field(s.bytes_at(0x0C, 2)); // size
                h.field(s.bytes_at(0x1C, 4)); // extended size
                h.field(s.string_at(0x10)); // device locator (slot)
                h.field(s.string_at(0x17)); // manufacturer
                h.field(s.string_at(0x18)); // serial number
                h.field(s.string_at(0x1A)); // part number
            }
            _ => {}
        }
    }
    h.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Appends a structure with `len`-byte formatted area (fields set by
    /// `set`) and the given strings.
    fn push(t: &mut Vec<u8>, ty: u8, len: usize, set: &[(usize, &[u8])], strings: &[&str]) {
        let mut data = vec![0u8; len];
        data[0] = ty;
        data[1] = len as u8;
        for (off, bytes) in set {
            data[*off..*off + bytes.len()].copy_from_slice(bytes);
        }
        t.extend_from_slice(&data);
        if strings.is_empty() {
            t.extend_from_slice(&[0, 0]);
        } else {
            for s in strings {
                t.extend_from_slice(s.as_bytes());
                t.push(0);
            }
            t.push(0);
        }
    }

    fn table(bios: &str, uuid: [u8; 16], part: &str) -> Vec<u8> {
        let mut t = Vec::new();
        push(&mut t, 0, 0x18, &[(0x04, &[1]), (0x05, &[2]), (0x08, &[3])], &["Vendor", bios, "01/02/2026"]);
        push(&mut t, 1, 0x1B, &[(0x04, &[1]), (0x05, &[2]), (0x07, &[3]), (0x08, &uuid)], &["Maker", "Model", "SN1"]);
        push(&mut t, 17, 0x28, &[(0x0C, &[0, 0x40]), (0x10, &[1]), (0x18, &[2]), (0x1A, &[3])], &["DIMM A", "1234", part]);
        push(&mut t, 2, 0x08, &[], &[]); // ignored type, no strings
        push(&mut t, 127, 4, &[], &[]);
        t
    }

    const UUID: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

    fn ram() -> Option<Range> {
        Range::new(0x10_0000, 0x4_0000_0000).ok()
    }

    #[test]
    fn same_input_same_id() {
        let a = machine_id(Some(&table("1.0", UUID, "PART-A")), ram());
        let b = machine_id(Some(&table("1.0", UUID, "PART-A")), ram());
        assert_eq!(a, b);
    }

    #[test]
    fn id_changes_with_machine_firmware_memory_and_ram() {
        let base = machine_id(Some(&table("1.0", UUID, "PART-A")), ram());
        let mut other_uuid = UUID;
        other_uuid[15] = 99;
        assert_ne!(base, machine_id(Some(&table("1.0", other_uuid, "PART-A")), ram()));
        assert_ne!(base, machine_id(Some(&table("1.1", UUID, "PART-A")), ram()));
        assert_ne!(base, machine_id(Some(&table("1.0", UUID, "PART-B")), ram()));
        assert_ne!(base, machine_id(Some(&table("1.0", UUID, "PART-A")), Range::new(0x10_0000, 0x2_0000_0000).ok()));
        assert_ne!(base, machine_id(None, ram()));
    }

    #[test]
    fn data_after_end_of_table_is_ignored() {
        let t = table("1.0", UUID, "PART-A");
        let mut longer = t.clone();
        push(&mut longer, 1, 0x1B, &[(0x08, &[7; 16])], &["Other"]);
        assert_eq!(machine_id(Some(&t), ram()), machine_id(Some(&longer), ram()));
    }

    #[test]
    fn malformed_tables_do_not_panic() {
        for t in [&[][..], &[1][..], &[1, 2, 0, 0][..], &[1, 0x40, 0, 0, 0, 0][..], &[0, 4, 0, 0, b'x'][..]] {
            let _ = machine_id(Some(t), None);
        }
        // A string index past the string set reads as empty.
        let mut t = Vec::new();
        push(&mut t, 0, 0x18, &[(0x05, &[9])], &["only"]);
        let _ = machine_id(Some(&t), None);
    }

    #[test]
    fn entry_points() {
        let mut e3 = vec![0u8; 0x18];
        e3[..5].copy_from_slice(b"_SM3_");
        e3[0x0C..0x10].copy_from_slice(&0x1234u32.to_le_bytes());
        e3[0x10..0x18].copy_from_slice(&0x7F00_0000u64.to_le_bytes());
        assert_eq!(entry_point_len(&e3[..5]), Some(0x18));
        assert_eq!(table_location(&e3), Some((0x7F00_0000, 0x1234)));

        let mut e2 = vec![0u8; 0x1F];
        e2[..4].copy_from_slice(b"_SM_");
        e2[0x16..0x18].copy_from_slice(&0x0800u16.to_le_bytes());
        e2[0x18..0x1C].copy_from_slice(&0x000F_0000u32.to_le_bytes());
        assert_eq!(entry_point_len(&e2[..5]), Some(0x1F));
        assert_eq!(table_location(&e2), Some((0x000F_0000, 0x0800)));

        assert_eq!(entry_point_len(b"_XX_x"), None);
        assert_eq!(table_location(&e3[..0x10]), None);
    }
}
