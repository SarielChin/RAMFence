//! Runs a memory test scheduled by RAMFence Manager (`test.req` next to
//! ramfence.efi), once, before any range is reserved.
//!
//! Free memory in the requested ranges is allocated in 64 MiB chunks,
//! tested with ramfence-core's engine through volatile accesses (UEFI
//! identity-maps physical memory), then freed again so the normal
//! reservation sees exactly what it would have seen without a test.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ptr::NonNull;

use ramfence_core::memmap::{self, type_name};
use ramfence_core::memtest::{
    self, REQUEST_FILE, RESULT_FILE, TestHost, TestMemory, TestResult, Untested,
};
use ramfence_core::config::AUTO_MARGIN;
use ramfence_core::{PAGE_SIZE, Range};
use uefi::boot::{self, AllocateType};
use uefi::fs::FileSystem;
use uefi::mem::memory_map::MemoryType;
use uefi::{CString16, print, println};

use crate::console::{self, Log};
use crate::machine;
use crate::reserve::{self, CHUNK_PAGES};

struct Phys {
    base: u64,
    words: usize,
}

impl TestMemory for Phys {
    fn base(&self) -> u64 {
        self.base
    }

    fn words(&self) -> usize {
        self.words
    }

    #[inline(always)]
    fn write(&mut self, i: usize, v: u64) {
        // SAFETY: i < words and the chunk is allocated to us; physical
        // memory is identity-mapped during boot services.
        unsafe { core::ptr::write_volatile((self.base as *mut u64).add(i), v) }
    }

    #[inline(always)]
    fn read(&self, i: usize) -> u64 {
        // SAFETY: as above.
        unsafe { core::ptr::read_volatile((self.base as *const u64).add(i)) }
    }
}

struct Host {
    step: &'static str,
    percent: u64,
}

impl TestHost for Host {
    fn flush_caches(&mut self) {
        // SAFETY: privileged but harmless instruction; we run in ring 0.
        unsafe { core::arch::asm!("wbinvd", options(nostack, preserves_flags)) };
    }

    fn progress(&mut self, step: &'static str, done: u64, total: u64) -> bool {
        let percent = done * 100 / total.max(1);
        if percent != self.percent || step != self.step {
            self.percent = percent;
            self.step = step;
            print!("\r  {percent:>3}%  {step:<20} (Esc = abort)   ");
        }
        !console::escape_pressed()
    }
}

fn path16(dir: &str, name: &str) -> Option<CString16> {
    CString16::try_from(format!("{dir}{name}").as_str()).ok()
}

fn open_fs() -> Result<FileSystem, String> {
    boot::get_image_file_system(boot::image_handle())
        .map(FileSystem::new)
        .map_err(|e| format!("cannot open ramfence.efi's volume: {:?}", e.status()))
}

/// Runs the test if `test.req` exists. Returns true if a test ran (so the
/// caller can leave the summary on screen longer). Problems are warnings:
/// they never affect the reservation itself.
pub fn run_if_requested(dir: &str, log: &mut Log) -> bool {
    let (Some(req_path), Some(res_path)) = (path16(dir, REQUEST_FILE), path16(dir, RESULT_FILE)) else {
        return false;
    };
    let Ok(mut fs) = open_fs() else { return false };
    if !fs.try_exists(&*req_path).unwrap_or(false) {
        return false;
    }
    let text = fs.read(&*req_path).ok().and_then(|b| String::from_utf8(b).ok());
    // Delete first: a hang or crash during the test must not repeat on
    // every boot.
    if fs.remove_file(&*req_path).is_err() {
        log.warn("memory test: cannot delete test.req; skipping the test so it cannot repeat");
        return false;
    }
    let req = match text.as_deref().map(memtest::parse_request) {
        Some(Ok(r)) => r,
        Some(Err(e)) => {
            log.warn(format_args!("memory test: {e}"));
            return false;
        }
        None => {
            log.warn("memory test: test.req is unreadable");
            return false;
        }
    };
    drop(fs);

    log.info("");
    log.info(format_args!(
        "Memory test requested: {} range(s), {} pass(es)",
        req.ranges.len(),
        req.passes
    ));
    let result = run(&req.ranges, req.passes, log);
    summarize(&result, log);
    save_result(dir, &res_path, &result, log);
    true
}

fn save_result(dir: &str, path: &CString16, result: &TestResult, log: &mut Log) {
    match open_fs().and_then(|mut fs| {
        fs.write(&**path, result.to_text().as_bytes())
            .map_err(|e| format!("cannot write {RESULT_FILE}: {e:?}"))
    }) {
        Ok(()) => log.info(format_args!("Result saved to {dir}{RESULT_FILE}")),
        Err(e) => log.warn(format_args!("memory test: {e}")),
    }
}

/// What `mode auto` decided during this boot.
#[derive(Default)]
pub struct AutoOutcome {
    /// Error clusters plus [`AUTO_MARGIN`]: the ranges to reserve.
    pub ranges: Vec<Range>,
    /// A scan ran during this boot.
    pub scanned: bool,
    /// That scan was stopped before it finished: only the errors found so
    /// far are in `ranges`.
    pub stopped: bool,
}

/// `mode auto` (rescue USB): use the saved result next to ramfence.efi if it
/// was made on this PC with this memory, else scan all memory and save the
/// result.
pub fn auto(dir: &str, log: &mut Log) -> AutoOutcome {
    let Some(res_path) = path16(dir, RESULT_FILE) else {
        log.error("auto mode: invalid result path");
        return AutoOutcome::default();
    };
    let here = machine::id();
    let saved = open_fs()
        .ok()
        .and_then(|mut fs| fs.read(&*res_path).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|t| memtest::parse_result(&t));
    // A saved result is only valid for the PC and memory it was made on, and
    // only if the scan finished and the file is complete.
    let saved = match saved {
        Some(r) if r.machine != Some(here) => {
            log.info(
                "Auto mode: the saved scan result does not match this PC and its memory \
                 (another PC, changed memory or firmware, or an older version); scanning again.",
            );
            None
        }
        Some(r) if !r.completed || !r.ended => {
            log.info("Auto mode: the saved scan result is incomplete; scanning again.");
            None
        }
        r => r,
    };

    let (result, scanned) = match saved {
        Some(r) => {
            log.info(format_args!(
                "Auto mode: using the saved scan result ({} error(s); delete {RESULT_FILE} to scan again)",
                r.errors.count
            ));
            (r, false)
        }
        None => {
            let extent = reserve::read_map().ok().and_then(|m| memmap::ram_extent(&m));
            let Some(extent) = extent else {
                log.error("auto mode: cannot determine the memory size to scan");
                return AutoOutcome::default();
            };
            log.info("");
            log.info(format_args!(
                "Auto mode: scanning all memory ({} MiB). This takes a few minutes.",
                extent.len() >> 20
            ));
            let r = run(&[extent], 1, log);
            summarize(&r, log);
            if r.completed {
                save_result(dir, &res_path, &r, log);
            } else {
                // Not saved: the next start scans again. Errors found so
                // far are still reserved now.
                log.warn("auto mode: scan was stopped; it runs again at the next start");
            }
            (r, true)
        }
    };
    let ranges = result
        .errors
        .suggested_ranges(AUTO_MARGIN, 1 << 20, memtest::SUGGEST_GAP);
    let stopped = scanned && !result.completed;
    if ranges.is_empty() {
        if stopped {
            log.warn("Auto mode: no bad memory found before the scan was stopped; nothing reserved.");
        } else {
            log.info("Auto mode: no bad memory found; nothing to reserve.");
        }
    }
    AutoOutcome { ranges, scanned, stopped }
}

fn run(ranges: &[Range], passes: u32, log: &mut Log) -> TestResult {
    let mut result = TestResult {
        completed: false,
        passes,
        machine: Some(machine::id()),
        requested: ranges.to_vec(),
        tested: Vec::new(),
        untested: Vec::new(),
        errors: Default::default(),
        ended: false,
    };

    let map = match reserve::read_map() {
        Ok(m) => m,
        Err(e) => {
            log.warn(format_args!("memory test: GetMemoryMap failed: {:?}", e.status()));
            return result;
        }
    };
    let plan = memmap::plan(ranges, &map);
    for s in plan.skipped.iter().chain(&plan.conflicts) {
        result.untested.push(Untested {
            range: s.range,
            reason: format!("{} (not free)", type_name(s.ty)),
        });
    }
    for u in &plan.unmapped {
        result.untested.push(Untested { range: *u, reason: "not in memory map".into() });
    }

    // Allocate free memory in chunks as temporary LoaderData.
    let mut chunks: Vec<(NonNull<u8>, usize, Range)> = Vec::new();
    for piece in &plan.allocate {
        for c in piece.chunks(CHUNK_PAGES) {
            let pages = c.pages() as usize;
            match boot::allocate_pages(AllocateType::Address(c.start()), MemoryType::LOADER_DATA, pages) {
                Ok(p) => chunks.push((p, pages, c)),
                Err(e) => result.untested.push(Untested {
                    range: c,
                    reason: format!("AllocatePages failed: {:?}", e.status()),
                }),
            }
        }
    }
    result.untested.sort_by_key(|u| u.range.start());

    let mut regions: Vec<Phys> = chunks
        .iter()
        .map(|(_, pages, r)| Phys { base: r.start(), words: pages * (PAGE_SIZE as usize / 8) })
        .collect();
    for (_, _, r) in &chunks {
        match result.tested.last_mut() {
            Some(last) if last.end() == r.start() => *last = Range::new(last.start(), r.end()).unwrap(),
            _ => result.tested.push(*r),
        }
    }

    log.info(format_args!(
        "Testing {} MiB ({} MiB untested)",
        result.tested_bytes() >> 20,
        result.untested.iter().map(|u| u.range.len()).sum::<u64>() >> 20
    ));
    let mut host = Host { step: "", percent: u64::MAX };
    let (errors, completed) = memtest::run_tests(&mut regions, passes, &mut host);
    println!();
    result.errors = errors;
    result.completed = completed;

    for (p, pages, r) in chunks {
        // SAFETY: allocated above with the same page count; no references
        // into it remain.
        if unsafe { boot::free_pages(p, pages) }.is_err() {
            log.warn(format_args!("memory test: could not free {r}"));
        }
    }
    result
}

fn summarize(r: &TestResult, log: &mut Log) {
    let e = &r.errors;
    let state = if r.completed { "completed" } else { "ABORTED" };
    if e.count == 0 {
        log.good(format_args!(
            "Memory test {state}: no errors in {} MiB.",
            r.tested_bytes() >> 20
        ));
    } else {
        log.warn(format_args!(
            "Memory test {state}: {} error(s) in {} MiB, {:#x} - {:#x}, mask {:#018x}",
            e.count,
            r.tested_bytes() >> 20,
            e.lowest.unwrap_or(0),
            e.highest.unwrap_or(0),
            e.mask
        ));
    }
}
