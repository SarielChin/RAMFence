//! Reserve the configured ranges via `AllocatePages(AllocateAddress, ...)`
//! and verify the result (DESIGN.md §2 steps 2–4).

use alloc::vec::Vec;

use badram_core::memmap::{self, MemDesc, type_name};
use badram_core::{MemoryType as CfgMemoryType, Range};
use uefi::boot::{self, AllocateType};
use uefi::mem::memory_map::{MemoryMap, MemoryType};
use uefi::proto::loaded_image::LoadedImage;

use crate::console::Log;

/// When a piece cannot be allocated in one call, retry in chunks of this many
/// pages (64 MiB), then page by page within a failing chunk.
const CHUNK_PAGES: u64 = 16 * 1024;

fn read_map() -> uefi::Result<Vec<MemDesc>> {
    let map = boot::memory_map(MemoryType::LOADER_DATA)?;
    Ok(map
        .entries()
        .map(|d| MemDesc {
            ty: d.ty.0,
            phys_start: d.phys_start,
            page_count: d.page_count,
        })
        .collect())
}

fn try_alloc(r: Range, mt: MemoryType) -> uefi::Result<()> {
    let pages = usize::try_from(r.pages()).map_err(|_| uefi::Status::BAD_BUFFER_SIZE)?;
    // Note: uefi-rs retries an allocation that lands at address 0, which
    // makes AllocateAddress(0) fail. It then shows up as a reported error
    // (never silently), and page 0 is never usable RAM for Windows anyway.
    boot::allocate_pages(AllocateType::Address(r.start()), mt, pages).map(|_| ())
}

fn push_merged(list: &mut Vec<Range>, r: Range) {
    if let Some(last) = list.last_mut()
        && last.end() == r.start()
    {
        *last = Range::new(last.start(), r.end()).unwrap();
        return;
    }
    list.push(r);
}

/// Allocates one free piece; returns pages reserved and pieces that failed.
fn allocate_piece(piece: Range, mt: MemoryType, log: &mut Log) -> (u64, Vec<Range>) {
    if try_alloc(piece, mt).is_ok() {
        return (piece.pages(), Vec::new());
    }
    log.info(format_args!(
        "  {piece}: single allocation failed, retrying in chunks"
    ));

    let mut reserved = 0;
    let mut failed = Vec::new();
    for chunk in piece.chunks(CHUNK_PAGES) {
        if try_alloc(chunk, mt).is_ok() {
            reserved += chunk.pages();
            continue;
        }
        for page in chunk.chunks(1) {
            match try_alloc(page, mt) {
                Ok(()) => reserved += 1,
                Err(_) => push_merged(&mut failed, page),
            }
        }
    }
    (reserved, failed)
}

fn own_image_range() -> Option<Range> {
    let li = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle()).ok()?;
    let (base, size) = li.info();
    let base = base as u64;
    Range::new(base, base.checked_add(size)?).ok()
}

/// Reserves `ranges` (normalized) and verifies. Returns pages reserved.
/// Every problem is reported through `log.error`.
pub fn run(ranges: &[Range], cfg_type: CfgMemoryType, log: &mut Log) -> u64 {
    let mt = MemoryType(cfg_type.efi_type());

    let map = match read_map() {
        Ok(m) => m,
        Err(e) => {
            log.error(format_args!("GetMemoryMap failed: {:?}", e.status()));
            return 0;
        }
    };
    let plan = memmap::plan(ranges, &map);

    if let Some(img) = own_image_range()
        && let Some(r) = memmap::find_overlap(ranges, &img)
    {
        log.warn(format_args!(
            "badram.efi itself is loaded at {img}, inside configured range {r}"
        ));
    }

    for s in &plan.skipped {
        log.info(format_args!(
            "  {}: already {} (OS will not use it), skipped",
            s.range,
            type_name(s.ty)
        ));
    }
    for u in &plan.unmapped {
        log.info(format_args!("  {u}: not in memory map (not RAM), skipped"));
    }
    for c in &plan.conflicts {
        log.error(format_args!(
            "{} is {} and cannot be reserved; Windows may use it later",
            c.range,
            type_name(c.ty)
        ));
    }

    let mut reserved = 0;
    let mut failed = Vec::new();
    for &piece in &plan.allocate {
        let (ok, bad) = allocate_piece(piece, mt, log);
        reserved += ok;
        for f in bad {
            log.error(format_args!("{f}: AllocatePages failed"));
            failed.push(f);
        }
    }

    // Verify on a fresh map. Pieces already reported above are not repeated.
    let reported = |r: &Range| {
        plan.conflicts.iter().map(|c| &c.range).chain(failed.iter())
            .any(|k| k.start() <= r.start() && r.end() <= k.end())
    };
    match read_map() {
        Ok(after) => {
            for s in memmap::verify(ranges, &after) {
                if !reported(&s.range) {
                    log.error(format_args!(
                        "verify: {} is {} after reservation",
                        s.range,
                        type_name(s.ty)
                    ));
                }
            }
        }
        Err(e) => log.error(format_args!(
            "GetMemoryMap (verify) failed: {:?}",
            e.status()
        )),
    }

    reserved
}
