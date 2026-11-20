//! Physical memory ranges Windows actually uses, from
//! `HKLM\HARDWARE\RESOURCEMAP\System Resources\Physical Memory\.Translated`
//! (a REG_RESOURCE_LIST written by the kernel at boot).

use badram_core::Range;

use crate::win::reg_hklm_bytes;

const KEY: &str = r"HARDWARE\RESOURCEMAP\System Resources\Physical Memory";

const CM_RESOURCE_TYPE_MEMORY: u8 = 3;
const CM_RESOURCE_TYPE_MEMORY_LARGE: u8 = 7;
const MEMORY_LARGE_40: u16 = 0x0200;
const MEMORY_LARGE_48: u16 = 0x0400;
const MEMORY_LARGE_64: u16 = 0x0800;

/// CM_PARTIAL_RESOURCE_DESCRIPTOR on 64-bit Windows: 4-byte header + 16-byte
/// union (the interrupt variant carries an 8-byte KAFFINITY).
const PARTIAL_SIZE: usize = 20;
/// CM_FULL_RESOURCE_DESCRIPTOR header: InterfaceType, BusNumber,
/// Version/Revision, Count.
const FULL_HEADER_SIZE: usize = 16;

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

/// Decodes the memory descriptors of a CM_RESOURCE_LIST, sorted by address.
pub fn decode_resource_list(b: &[u8]) -> Option<Vec<Range>> {
    let mut ranges = Vec::new();
    let full_count = u32_at(b, 0)?;
    let mut o = 4;
    for _ in 0..full_count {
        let count = u32_at(b, o + 12)?;
        o += FULL_HEADER_SIZE;
        for _ in 0..count {
            b.get(o..o + PARTIAL_SIZE)?; // whole record must be present
            let ty = *b.get(o)?;
            let flags = u16_at(b, o + 2)?;
            let start = u64_at(b, o + 4)?;
            let raw_len = u64::from(u32_at(b, o + 12)?);
            let len = match ty {
                CM_RESOURCE_TYPE_MEMORY => Some(raw_len),
                CM_RESOURCE_TYPE_MEMORY_LARGE if flags & MEMORY_LARGE_40 != 0 => Some(raw_len << 8),
                CM_RESOURCE_TYPE_MEMORY_LARGE if flags & MEMORY_LARGE_48 != 0 => Some(raw_len << 16),
                CM_RESOURCE_TYPE_MEMORY_LARGE if flags & MEMORY_LARGE_64 != 0 => Some(raw_len << 32),
                _ => None,
            };
            if let Some(len) = len
                && let Some(end) = start.checked_add(len)
                && let Ok(r) = Range::new(start, end)
            {
                ranges.push(r);
            }
            o += PARTIAL_SIZE;
        }
    }
    ranges.sort();
    Some(ranges)
}

pub fn windows_ranges() -> Result<Vec<Range>, String> {
    let bytes = reg_hklm_bytes(KEY, ".Translated")?;
    decode_resource_list(&bytes).ok_or_else(|| "cannot decode physical memory resource list".into())
}

/// Parts of `protected` that Windows uses anyway (should be empty).
pub fn overlaps(windows: &[Range], protected: &[Range]) -> Vec<Range> {
    let mut out = Vec::new();
    for w in windows {
        for p in protected {
            if let Some(i) = w.intersect(p) {
                out.push(i);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(a: u64, b: u64) -> Range {
        Range::new(a, b).unwrap()
    }

    fn partial(ty: u8, flags: u16, start: u64, len: u32) -> Vec<u8> {
        let mut v = vec![ty, 1];
        v.extend(flags.to_le_bytes());
        v.extend(start.to_le_bytes());
        v.extend(len.to_le_bytes());
        v.extend([0u8; 4]);
        assert_eq!(v.len(), PARTIAL_SIZE);
        v
    }

    fn list(parts: &[Vec<u8>]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(1u32.to_le_bytes()); // one full descriptor
        v.extend([0u8; 12]); // interface, bus, version, revision
        v.extend((parts.len() as u32).to_le_bytes());
        for p in parts {
            v.extend(p);
        }
        v
    }

    #[test]
    fn decodes_small_and_large_descriptors() {
        let b = list(&[
            partial(3, 0, 0x1000, 0x9E000),
            partial(7, MEMORY_LARGE_40, 0x1_0000_0000, 0x0100_0000), // << 8 = 4 GiB
            partial(7, MEMORY_LARGE_48, 0x3_0000_0000, 0x0001_0000), // << 16 = 4 GiB
            partial(1, 0, 0xFFFF, 0x10),                             // port: ignored
        ]);
        assert_eq!(
            decode_resource_list(&b).unwrap(),
            vec![
                r(0x1000, 0x9F000),
                r(0x1_0000_0000, 0x2_0000_0000),
                r(0x3_0000_0000, 0x4_0000_0000),
            ]
        );
    }

    #[test]
    fn truncated_input_is_rejected() {
        let b = list(&[partial(3, 0, 0x1000, 0x1000)]);
        assert!(decode_resource_list(&b[..b.len() - 1]).is_none());
        assert!(decode_resource_list(&[]).is_none());
    }

    #[test]
    fn overlap_detection() {
        let win = [r(0x1000, 0x10_0000), r(0x20_0000, 0x40_0000)];
        assert!(overlaps(&win, &[r(0x10_0000, 0x20_0000)]).is_empty());
        assert_eq!(overlaps(&win, &[r(0x3F_0000, 0x50_0000)]), vec![r(0x3F_0000, 0x40_0000)]);
    }
}
