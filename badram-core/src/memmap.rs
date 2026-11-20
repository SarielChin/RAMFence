//! Intersect configured ranges with the UEFI memory map (DESIGN.md §2, §2.2).
//!
//! The UEFI side converts its memory descriptors into [`MemDesc`] and calls
//! [`plan`] to learn which pieces to `AllocatePages(AllocateAddress, ...)`,
//! then calls [`verify`] on a fresh memory map afterwards.

use alloc::vec::Vec;

use crate::range::{PAGE_SIZE, Range};

/// `EFI_MEMORY_TYPE` values (UEFI spec 2.10, §7.2).
pub mod mem_type {
    pub const RESERVED: u32 = 0;
    pub const LOADER_CODE: u32 = 1;
    pub const LOADER_DATA: u32 = 2;
    pub const BOOT_SERVICES_CODE: u32 = 3;
    pub const BOOT_SERVICES_DATA: u32 = 4;
    pub const RUNTIME_SERVICES_CODE: u32 = 5;
    pub const RUNTIME_SERVICES_DATA: u32 = 6;
    pub const CONVENTIONAL: u32 = 7;
    pub const UNUSABLE: u32 = 8;
    pub const ACPI_RECLAIM: u32 = 9;
    pub const ACPI_NVS: u32 = 10;
    pub const MMIO: u32 = 11;
    pub const MMIO_PORT_SPACE: u32 = 12;
    pub const PAL_CODE: u32 = 13;
    pub const PERSISTENT: u32 = 14;
    pub const UNACCEPTED: u32 = 15;
}

/// Human-readable name of a memory type, for logs and the summary screen.
pub const fn type_name(ty: u32) -> &'static str {
    use mem_type::*;
    match ty {
        RESERVED => "Reserved",
        LOADER_CODE => "LoaderCode",
        LOADER_DATA => "LoaderData",
        BOOT_SERVICES_CODE => "BootServicesCode",
        BOOT_SERVICES_DATA => "BootServicesData",
        RUNTIME_SERVICES_CODE => "RuntimeServicesCode",
        RUNTIME_SERVICES_DATA => "RuntimeServicesData",
        CONVENTIONAL => "Conventional",
        UNUSABLE => "Unusable",
        ACPI_RECLAIM => "ACPIReclaim",
        ACPI_NVS => "ACPINVS",
        MMIO => "MMIO",
        MMIO_PORT_SPACE => "MMIOPortSpace",
        PAL_CODE => "PalCode",
        PERSISTENT => "Persistent",
        UNACCEPTED => "Unaccepted",
        0x7000_0000..=0x7FFF_FFFF => "OEM",
        0x8000_0000..=0xFFFF_FFFF => "OSV",
        _ => "Unknown",
    }
}

/// How a memory type relates to what we are trying to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// `EfiConventionalMemory`: free now, we can and must allocate it.
    Free,
    /// The OS will never use it as general RAM; nothing to do.
    Unavailable,
    /// Not free now, but the OS may reclaim it later (boot-services/loader
    /// memory after `ExitBootServices`, ACPI reclaim after reading tables,
    /// persistent/unaccepted memory, or a type we don't recognise). If this
    /// overlaps a bad range, the bad memory can end up in use: an error.
    OsReclaimable,
}

/// Conservative classification: anything not known to be permanently
/// unavailable to the OS is treated as reclaimable.
pub const fn classify(ty: u32) -> Class {
    use mem_type::*;
    match ty {
        CONVENTIONAL => Class::Free,
        RESERVED | RUNTIME_SERVICES_CODE | RUNTIME_SERVICES_DATA | UNUSABLE | ACPI_NVS | MMIO
        | MMIO_PORT_SPACE | PAL_CODE => Class::Unavailable,
        _ => Class::OsReclaimable,
    }
}

/// The parts of a UEFI memory descriptor we need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemDesc {
    pub ty: u32,
    pub phys_start: u64,
    pub page_count: u64,
}

impl MemDesc {
    /// Address range of this descriptor. `None` for zero-page descriptors.
    /// A descriptor that would run past the end of the address space is
    /// clamped to the last page boundary.
    pub fn range(&self) -> Option<Range> {
        if self.page_count == 0 {
            return None;
        }
        let end = self
            .page_count
            .checked_mul(PAGE_SIZE)
            .and_then(|len| self.phys_start.checked_add(len))
            .unwrap_or(!(PAGE_SIZE - 1));
        Range::new(self.phys_start, end).ok()
    }
}

/// A piece of a configured range covered by one memory descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub range: Range,
    pub ty: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Free pieces to allocate, page-aligned, sorted by address.
    pub allocate: Vec<Range>,
    /// Pieces the OS already won't use (log only).
    pub skipped: Vec<Segment>,
    /// Pieces the OS may reclaim that we cannot allocate: errors.
    pub conflicts: Vec<Segment>,
    /// Pieces not described by any descriptor (not RAM; log only).
    pub unmapped: Vec<Range>,
}

impl Plan {
    /// `true` if every part of every range is either allocatable or already
    /// unavailable to the OS.
    pub fn is_ok(&self) -> bool {
        self.conflicts.is_empty()
    }

    pub fn allocate_pages(&self) -> u64 {
        crate::range::total_pages(&self.allocate)
    }
}

/// Every piece of `ranges` covered by a descriptor in `map`, sorted by
/// address. `ranges` should be normalized (sorted, disjoint, page-aligned).
pub fn segments(ranges: &[Range], map: &[MemDesc]) -> Vec<Segment> {
    let mut out = Vec::new();
    for r in ranges {
        for d in map {
            if let Some(dr) = d.range()
                && let Some(piece) = r.intersect(&dr)
            {
                out.push(Segment { range: piece, ty: d.ty });
            }
        }
    }
    out.sort_unstable_by_key(|s| (s.range.start(), s.range.end()));
    out
}

/// Decides what to allocate for normalized `ranges` given the current
/// memory map.
pub fn plan(ranges: &[Range], map: &[MemDesc]) -> Plan {
    let mut p = Plan::default();

    for r in ranges {
        let mut cursor = r.start();
        for seg in segments(core::slice::from_ref(r), map) {
            if seg.range.start() > cursor {
                p.unmapped.push(Range::new(cursor, seg.range.start()).unwrap());
            }
            cursor = cursor.max(seg.range.end());

            match classify(seg.ty) {
                // The spec guarantees 4 KiB-aligned descriptors; if firmware
                // breaks that we can't allocate the piece exactly, so refuse.
                Class::Free if seg.range.is_page_aligned() => p.allocate.push(seg.range),
                Class::Free | Class::OsReclaimable => p.conflicts.push(seg),
                Class::Unavailable => p.skipped.push(seg),
            }
        }
        if cursor < r.end() {
            p.unmapped.push(Range::new(cursor, r.end()).unwrap());
        }
    }
    p
}

/// Checks a memory map taken after allocation. Returns every piece of
/// `ranges` that the OS could still use; empty means success.
pub fn verify(ranges: &[Range], map: &[MemDesc]) -> Vec<Segment> {
    segments(ranges, map)
        .into_iter()
        .filter(|s| classify(s.ty) != Class::Unavailable)
        .collect()
}

/// First range in `ranges` that overlaps `region` (e.g. our own loaded image).
pub fn find_overlap(ranges: &[Range], region: &Range) -> Option<Range> {
    ranges.iter().copied().find(|r| r.overlaps(region))
}

#[cfg(test)]
mod tests {
    use super::mem_type::*;
    use super::*;

    const P: u64 = PAGE_SIZE;

    fn r(start: u64, end: u64) -> Range {
        Range::new(start, end).unwrap()
    }

    fn d(ty: u32, start: u64, pages: u64) -> MemDesc {
        MemDesc { ty, phys_start: start, page_count: pages }
    }

    fn seg(ty: u32, start: u64, end: u64) -> Segment {
        Segment { range: r(start, end), ty }
    }

    #[test]
    fn classification() {
        assert_eq!(classify(CONVENTIONAL), Class::Free);
        for ty in [RESERVED, RUNTIME_SERVICES_CODE, RUNTIME_SERVICES_DATA, UNUSABLE, ACPI_NVS, MMIO, MMIO_PORT_SPACE, PAL_CODE] {
            assert_eq!(classify(ty), Class::Unavailable, "{}", type_name(ty));
        }
        for ty in [LOADER_CODE, LOADER_DATA, BOOT_SERVICES_CODE, BOOT_SERVICES_DATA, ACPI_RECLAIM, PERSISTENT, UNACCEPTED, 16, 0x7000_0001, 0x8000_0000] {
            assert_eq!(classify(ty), Class::OsReclaimable, "{ty:#x}");
        }
    }

    #[test]
    fn type_names() {
        assert_eq!(type_name(BOOT_SERVICES_DATA), "BootServicesData");
        assert_eq!(type_name(0x7000_0005), "OEM");
        assert_eq!(type_name(0x8000_0000), "OSV");
        assert_eq!(type_name(99), "Unknown");
    }

    #[test]
    fn desc_range() {
        assert_eq!(d(7, 0x1000, 2).range(), Some(r(0x1000, 0x3000)));
        assert_eq!(d(7, 0x1000, 0).range(), None);
        let top = !(P - 1);
        assert_eq!(d(7, top - P, u64::MAX).range(), Some(r(top - P, top)));
    }

    #[test]
    fn range_inside_single_conventional_descriptor() {
        let map = [d(CONVENTIONAL, 0, 0x100)];
        let p = plan(&[r(0x10 * P, 0x20 * P)], &map);
        assert_eq!(p.allocate, vec![r(0x10 * P, 0x20 * P)]);
        assert!(p.is_ok());
        assert!(p.skipped.is_empty() && p.unmapped.is_empty());
        assert_eq!(p.allocate_pages(), 0x10);
    }

    #[test]
    fn range_spanning_several_descriptors() {
        // [0,10) conv | [10,12) MMIO | [12,14) gap | [14,20) conv | [20,30) BSData
        let map = [
            d(CONVENTIONAL, 0, 10),
            d(MMIO, 10 * P, 2),
            d(CONVENTIONAL, 14 * P, 6),
            d(BOOT_SERVICES_DATA, 20 * P, 10),
        ];
        let p = plan(&[r(5 * P, 25 * P)], &map);
        assert_eq!(p.allocate, vec![r(5 * P, 10 * P), r(14 * P, 20 * P)]);
        assert_eq!(p.skipped, vec![seg(MMIO, 10 * P, 12 * P)]);
        assert_eq!(p.unmapped, vec![r(12 * P, 14 * P)]);
        assert_eq!(p.conflicts, vec![seg(BOOT_SERVICES_DATA, 20 * P, 25 * P)]);
        assert!(!p.is_ok());
    }

    #[test]
    fn descriptor_order_in_map_does_not_matter() {
        let map = [d(CONVENTIONAL, 20 * P, 10), d(CONVENTIONAL, 0, 10), d(RESERVED, 10 * P, 10)];
        let p = plan(&[r(0, 30 * P)], &map);
        assert_eq!(p.allocate, vec![r(0, 10 * P), r(20 * P, 30 * P)]);
        assert_eq!(p.skipped, vec![seg(RESERVED, 10 * P, 20 * P)]);
    }

    #[test]
    fn range_entirely_unmapped() {
        let p = plan(&[r(100 * P, 110 * P)], &[d(CONVENTIONAL, 0, 10)]);
        assert_eq!(p.unmapped, vec![r(100 * P, 110 * P)]);
        assert!(p.allocate.is_empty());
        assert!(p.is_ok());
    }

    #[test]
    fn trailing_and_leading_gaps() {
        let p = plan(&[r(0, 30 * P)], &[d(CONVENTIONAL, 10 * P, 10)]);
        assert_eq!(p.unmapped, vec![r(0, 10 * P), r(20 * P, 30 * P)]);
        assert_eq!(p.allocate, vec![r(10 * P, 20 * P)]);
    }

    #[test]
    fn multiple_ranges() {
        let map = [d(CONVENTIONAL, 0, 100)];
        let p = plan(&[r(0, P), r(50 * P, 60 * P)], &map);
        assert_eq!(p.allocate, vec![r(0, P), r(50 * P, 60 * P)]);
    }

    #[test]
    fn already_unusable_is_skipped() {
        let p = plan(&[r(0, 10 * P)], &[d(UNUSABLE, 0, 10)]);
        assert!(p.allocate.is_empty());
        assert_eq!(p.skipped, vec![seg(UNUSABLE, 0, 10 * P)]);
        assert!(p.is_ok());
    }

    #[test]
    fn loader_and_acpi_reclaim_are_conflicts() {
        let map = [d(LOADER_CODE, 0, 1), d(ACPI_RECLAIM, P, 1)];
        let p = plan(&[r(0, 2 * P)], &map);
        assert_eq!(p.conflicts, vec![seg(LOADER_CODE, 0, P), seg(ACPI_RECLAIM, P, 2 * P)]);
    }

    #[test]
    fn unaligned_conventional_descriptor_is_conflict() {
        let map = [d(CONVENTIONAL, 0x800, 2)];
        let p = plan(&[r(0, 4 * P)], &map);
        assert!(p.allocate.is_empty());
        assert_eq!(p.conflicts, vec![seg(CONVENTIONAL, 0x800, 0x2800)]);
    }

    #[test]
    fn overlapping_descriptors_do_not_produce_bogus_gaps() {
        // Buggy firmware map: second descriptor overlaps the first.
        let map = [d(RESERVED, 0, 10), d(RESERVED, 5 * P, 3)];
        let p = plan(&[r(0, 10 * P)], &map);
        assert!(p.unmapped.is_empty());
    }

    #[test]
    fn verify_success_after_allocation() {
        let map = [d(CONVENTIONAL, 0, 10), d(UNUSABLE, 10 * P, 10), d(MMIO, 20 * P, 5), d(CONVENTIONAL, 25 * P, 10)];
        assert!(verify(&[r(10 * P, 25 * P)], &map).is_empty());
    }

    #[test]
    fn verify_reports_leftovers() {
        let map = [
            d(UNUSABLE, 0, 10),
            d(CONVENTIONAL, 10 * P, 2),
            d(BOOT_SERVICES_CODE, 12 * P, 2),
            d(RESERVED, 14 * P, 6),
        ];
        assert_eq!(
            verify(&[r(0, 20 * P)], &map),
            vec![seg(CONVENTIONAL, 10 * P, 12 * P), seg(BOOT_SERVICES_CODE, 12 * P, 14 * P)]
        );
    }

    #[test]
    fn overlap_detection() {
        let ranges = [r(0x10 * P, 0x20 * P), r(0x40 * P, 0x50 * P)];
        assert_eq!(find_overlap(&ranges, &r(0x4F * P, 0x60 * P)), Some(ranges[1]));
        assert_eq!(find_overlap(&ranges, &r(0x20 * P, 0x40 * P)), None);
    }
}
