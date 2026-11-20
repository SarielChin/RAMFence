//! Half-open physical address ranges `[start, end)`.

use alloc::vec::Vec;
use core::fmt;

pub const PAGE_SIZE: u64 = 4096;
const PAGE_MASK: u64 = PAGE_SIZE - 1;

/// A non-empty half-open physical address range `[start, end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Range {
    start: u64,
    end: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeError {
    /// `start >= end`.
    Empty { start: u64, end: u64 },
    /// Rounding the end up to a page boundary would overflow `u64`.
    AlignOverflow { end: u64 },
}

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RangeError::Empty { start, end } => {
                write!(f, "empty range: start {start:#x} >= end {end:#x}")
            }
            RangeError::AlignOverflow { end } => {
                write!(f, "range end {end:#x} cannot be page-aligned (overflow)")
            }
        }
    }
}

impl Range {
    /// Creates `[start, end)`. Fails if the range would be empty.
    pub const fn new(start: u64, end: u64) -> Result<Self, RangeError> {
        if start < end {
            Ok(Range { start, end })
        } else {
            Err(RangeError::Empty { start, end })
        }
    }

    /// Creates the range covering `pages` pages starting at `start`.
    /// Returns `None` if `pages == 0` or the end would overflow.
    pub fn from_pages(start: u64, pages: u64) -> Option<Self> {
        let len = pages.checked_mul(PAGE_SIZE)?;
        let end = start.checked_add(len)?;
        Range::new(start, end).ok()
    }

    pub const fn start(&self) -> u64 {
        self.start
    }

    /// Exclusive end.
    pub const fn end(&self) -> u64 {
        self.end
    }

    pub const fn len(&self) -> u64 {
        self.end - self.start
    }

    /// Ranges are never empty; provided for clippy's `len_without_is_empty`.
    pub const fn is_empty(&self) -> bool {
        false
    }

    pub const fn is_page_aligned(&self) -> bool {
        self.start & PAGE_MASK == 0 && self.end & PAGE_MASK == 0
    }

    /// Number of pages touched by this range (partial pages count as whole).
    pub const fn pages(&self) -> u64 {
        let first = self.start / PAGE_SIZE;
        let last = (self.end - 1) / PAGE_SIZE;
        last - first + 1
    }

    /// Expands the range outward to page boundaries: start rounds down, end
    /// rounds up. We would rather block a little too much than too little.
    pub const fn align_outward(&self) -> Result<Self, RangeError> {
        let start = self.start & !PAGE_MASK;
        let end = match self.end.checked_add(PAGE_MASK) {
            Some(e) => e & !PAGE_MASK,
            None => return Err(RangeError::AlignOverflow { end: self.end }),
        };
        Ok(Range { start, end })
    }

    pub fn intersect(&self, other: &Range) -> Option<Range> {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        Range::new(start, end).ok()
    }

    pub const fn overlaps(&self, other: &Range) -> bool {
        self.start < other.end && other.start < self.end
    }

    pub const fn contains(&self, addr: u64) -> bool {
        self.start <= addr && addr < self.end
    }

    /// Splits the range into consecutive pieces of at most `max_pages` pages
    /// each. The range must be page-aligned. `max_pages == 0` is treated as 1.
    pub fn chunks(&self, max_pages: u64) -> Chunks {
        debug_assert!(self.is_page_aligned());
        Chunks {
            next: self.start,
            end: self.end,
            step: max_pages.max(1).saturating_mul(PAGE_SIZE),
        }
    }
}

impl fmt::Display for Range {
    /// Formats as `0xSTART-0xEND` (end exclusive).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}-{:#x}", self.start, self.end)
    }
}

/// Iterator returned by [`Range::chunks`].
#[derive(Clone, Debug)]
pub struct Chunks {
    next: u64,
    end: u64,
    step: u64,
}

impl Iterator for Chunks {
    type Item = Range;

    fn next(&mut self) -> Option<Range> {
        if self.next >= self.end {
            return None;
        }
        let start = self.next;
        let end = start.saturating_add(self.step).min(self.end);
        self.next = end;
        Some(Range { start, end })
    }
}

/// Aligns every range outward to page boundaries, sorts them, and merges
/// ranges that overlap or touch. The result is sorted and disjoint.
pub fn normalize(ranges: &[Range]) -> Result<Vec<Range>, RangeError> {
    let mut aligned = ranges
        .iter()
        .map(Range::align_outward)
        .collect::<Result<Vec<_>, _>>()?;
    aligned.sort_unstable();

    let mut merged: Vec<Range> = Vec::with_capacity(aligned.len());
    for r in aligned {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    Ok(merged)
}

/// Total pages covered by a set of disjoint ranges.
pub fn total_pages(ranges: &[Range]) -> u64 {
    ranges.iter().map(Range::pages).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(start: u64, end: u64) -> Range {
        Range::new(start, end).unwrap()
    }

    #[test]
    fn new_rejects_empty() {
        assert_eq!(
            Range::new(5, 5),
            Err(RangeError::Empty { start: 5, end: 5 })
        );
        assert!(Range::new(6, 5).is_err());
        assert!(Range::new(5, 6).is_ok());
    }

    #[test]
    fn from_pages() {
        assert_eq!(Range::from_pages(0x1000, 2), Some(r(0x1000, 0x3000)));
        assert_eq!(Range::from_pages(0x1000, 0), None);
        assert_eq!(Range::from_pages(u64::MAX - 0xFFF, 1), None);
        assert_eq!(Range::from_pages(0, u64::MAX), None);
    }

    #[test]
    fn align_outward_rounds_both_ways() {
        assert_eq!(r(0x1234, 0x5678).align_outward(), Ok(r(0x1000, 0x6000)));
        assert_eq!(r(0x1000, 0x2000).align_outward(), Ok(r(0x1000, 0x2000)));
        assert_eq!(r(0x1FFF, 0x2001).align_outward(), Ok(r(0x1000, 0x3000)));
    }

    #[test]
    fn align_outward_overflow() {
        assert_eq!(
            r(0, u64::MAX).align_outward(),
            Err(RangeError::AlignOverflow { end: u64::MAX })
        );
        // Already-aligned end near the top is fine.
        let top = !PAGE_MASK;
        assert_eq!(r(0, top).align_outward(), Ok(r(0, top)));
    }

    #[test]
    fn pages_counts_partial_pages() {
        assert_eq!(r(0x1000, 0x2000).pages(), 1);
        assert_eq!(r(0x1000, 0x2001).pages(), 2);
        assert_eq!(r(0x1FFF, 0x2001).pages(), 2);
        assert_eq!(r(0, 1).pages(), 1);
        // 1 GiB
        assert_eq!(r(0x1_0000_0000, 0x1_4000_0000).pages(), 262_144);
    }

    #[test]
    fn intersect_and_overlaps() {
        let a = r(0x1000, 0x5000);
        assert_eq!(a.intersect(&r(0x3000, 0x8000)), Some(r(0x3000, 0x5000)));
        assert_eq!(a.intersect(&r(0x2000, 0x3000)), Some(r(0x2000, 0x3000)));
        assert_eq!(a.intersect(&r(0x5000, 0x6000)), None); // touching only
        assert!(!a.overlaps(&r(0x5000, 0x6000)));
        assert!(!a.overlaps(&r(0, 0x1000)));
        assert!(a.overlaps(&r(0x4FFF, 0x6000)));
    }

    #[test]
    fn normalize_merges_overlapping_and_adjacent() {
        let input = [
            r(0x9000, 0xA000),
            r(0x1000, 0x3000),
            r(0x2800, 0x4000),  // overlaps previous
            r(0x4000, 0x5000),  // adjacent
            r(0x6001, 0x6002),  // aligns to 0x6000-0x7000
            r(0x7000, 0x7001),  // aligns to 0x7000-0x8000, adjacent to previous
        ];
        assert_eq!(
            normalize(&input).unwrap(),
            vec![r(0x1000, 0x5000), r(0x6000, 0x8000), r(0x9000, 0xA000)]
        );
    }

    #[test]
    fn normalize_contained_range() {
        let input = [r(0x1000, 0x9000), r(0x2000, 0x3000)];
        assert_eq!(normalize(&input).unwrap(), vec![r(0x1000, 0x9000)]);
    }

    #[test]
    fn normalize_propagates_overflow() {
        assert!(normalize(&[r(0, u64::MAX)]).is_err());
    }

    #[test]
    fn normalize_empty_input() {
        assert!(normalize(&[]).unwrap().is_empty());
    }

    #[test]
    fn chunks_split_evenly_and_with_remainder() {
        let big = r(0x10000, 0x10000 + 5 * PAGE_SIZE);
        let parts: Vec<_> = big.chunks(2).collect();
        assert_eq!(
            parts,
            vec![
                r(0x10000, 0x12000),
                r(0x12000, 0x14000),
                r(0x14000, 0x15000),
            ]
        );
        assert_eq!(big.chunks(5).count(), 1);
        assert_eq!(big.chunks(100).count(), 1);
        assert_eq!(big.chunks(0).count(), 5);
    }

    #[test]
    fn chunks_near_top_of_address_space() {
        let top = !PAGE_MASK;
        let hi = r(top - 2 * PAGE_SIZE, top);
        assert_eq!(hi.chunks(u64::MAX).collect::<Vec<_>>(), vec![hi]);
    }

    #[test]
    fn display_format() {
        assert_eq!(r(0x1000, 0x2000).to_string(), "0x1000-0x2000");
    }

    #[test]
    fn total_pages_sums() {
        assert_eq!(total_pages(&[r(0, 0x2000), r(0x5000, 0x6000)]), 3);
    }
}
