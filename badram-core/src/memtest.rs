//! Pre-boot memory test: request/result file formats and the test engine.
//!
//! The GUI writes a request (`test.req`) to the ESP; badram.efi runs the
//! test once before Windows starts and writes `test-result.txt`. The engine
//! is generic over [`TestMemory`] so it can be unit-tested on the host with
//! simulated faults, and run by badram.efi on raw physical memory.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};

use crate::config::parse_address;
use crate::range::{PAGE_SIZE, Range, RangeError, normalize};

pub const REQUEST_FILE: &str = "test.req";
pub const RESULT_FILE: &str = "test-result.txt";
pub const MAX_PASSES: u32 = 10;
/// Error histogram granularity.
pub const BUCKET_SIZE: u64 = 64 * 1024 * 1024;
/// Individual errors kept in the result (the count is always exact).
pub const MAX_SAMPLES: usize = 1000;

// ------------------------------------------------------------------ request

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestRequest {
    /// Sorted, disjoint, page-aligned.
    pub ranges: Vec<Range>,
    pub passes: u32,
}

impl TestRequest {
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        for r in &self.ranges {
            let _ = writeln!(s, "scan {:#x} {:#x}", r.start(), r.end());
        }
        let _ = writeln!(s, "passes {}", self.passes);
        s
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line > 0 {
            write!(f, "{REQUEST_FILE} line {}: {}", self.line, self.message)
        } else {
            write!(f, "{REQUEST_FILE}: {}", self.message)
        }
    }
}

/// `scan <start> <end>` (repeatable, hex, end exclusive) and `passes <n>`.
pub fn parse_request(text: &str) -> Result<TestRequest, RequestError> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut ranges = Vec::new();
    let mut passes = None;
    for (idx, raw) in text.lines().enumerate() {
        let line = idx + 1;
        let err = |m: &str| RequestError { line, message: m.into() };
        let content = raw.split('#').next().unwrap_or("").trim();
        if content.is_empty() {
            continue;
        }
        let parts: Vec<&str> = content.split_whitespace().collect();
        match parts[0] {
            "scan" => {
                if parts.len() != 3 {
                    return Err(err("expected 'scan 0xSTART 0xEND'"));
                }
                let s = parse_address(parts[1]).map_err(|_| err("invalid start address"))?;
                let e = parse_address(parts[2]).map_err(|_| err("invalid end address"))?;
                ranges.push(Range::new(s, e).map_err(|_| err("start must be below end"))?);
            }
            "passes" => {
                if parts.len() != 2 || passes.is_some() {
                    return Err(err("expected one 'passes <n>'"));
                }
                let n: u32 = parts[1].parse().map_err(|_| err("invalid number"))?;
                if !(1..=MAX_PASSES).contains(&n) {
                    return Err(err("passes must be 1..=10"));
                }
                passes = Some(n);
            }
            _ => return Err(err("unknown key")),
        }
    }
    if ranges.is_empty() {
        return Err(RequestError { line: 0, message: "no 'scan' line".into() });
    }
    let ranges = normalize(&ranges).map_err(|e: RangeError| RequestError {
        line: 0,
        message: alloc::format!("{e}"),
    })?;
    Ok(TestRequest { ranges, passes: passes.unwrap_or(1) })
}

// ------------------------------------------------------------------- errors

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorSample {
    pub addr: u64,
    pub expected: u64,
    pub actual: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ErrorLog {
    pub count: u64,
    /// OR of `expected ^ actual` over all errors (MemTest86 "error mask").
    pub mask: u64,
    pub lowest: Option<u64>,
    pub highest: Option<u64>,
    /// Bucket start address -> error count.
    pub buckets: BTreeMap<u64, u64>,
    pub samples: Vec<ErrorSample>,
}

impl ErrorLog {
    pub fn record(&mut self, addr: u64, expected: u64, actual: u64) {
        self.count += 1;
        self.mask |= expected ^ actual;
        self.lowest = Some(self.lowest.map_or(addr, |l| l.min(addr)));
        self.highest = Some(self.highest.map_or(addr, |h| h.max(addr)));
        *self.buckets.entry(addr & !(BUCKET_SIZE - 1)).or_default() += 1;
        if self.samples.len() < MAX_SAMPLES {
            self.samples.push(ErrorSample { addr, expected, actual });
        }
    }

    /// Range covering every error, widened by `margin` on both sides and
    /// rounded outward to `align` (a power of two, at least a page).
    pub fn suggested_range(&self, margin: u64, align: u64) -> Option<Range> {
        let align = align.max(PAGE_SIZE);
        let lo = self.lowest?.saturating_sub(margin) & !(align - 1);
        let hi = self.highest?.checked_add(8 + margin)?.checked_add(align - 1)? & !(align - 1);
        Range::new(lo, hi).ok()
    }
}

// ------------------------------------------------------------------- result

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Untested {
    pub range: Range,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestResult {
    pub completed: bool,
    pub passes: u32,
    pub requested: Vec<Range>,
    pub tested: Vec<Range>,
    pub untested: Vec<Untested>,
    pub errors: ErrorLog,
}

impl TestResult {
    pub fn tested_bytes(&self) -> u64 {
        self.tested.iter().map(Range::len).sum()
    }

    pub fn to_text(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "badram-test 1");
        let _ = writeln!(s, "result {}", if self.completed { "complete" } else { "aborted" });
        let _ = writeln!(s, "passes {}", self.passes);
        for r in &self.requested {
            let _ = writeln!(s, "requested {:#x} {:#x}", r.start(), r.end());
        }
        for r in &self.tested {
            let _ = writeln!(s, "tested {:#x} {:#x}", r.start(), r.end());
        }
        for u in &self.untested {
            // Reason last: it may contain spaces.
            let _ = writeln!(s, "untested {:#x} {:#x} {}", u.range.start(), u.range.end(), u.reason);
        }
        let e = &self.errors;
        let _ = writeln!(s, "errors {}", e.count);
        let _ = writeln!(s, "mask {:#018x}", e.mask);
        if let (Some(l), Some(h)) = (e.lowest, e.highest) {
            let _ = writeln!(s, "lowest {l:#x}");
            let _ = writeln!(s, "highest {h:#x}");
        }
        for (b, n) in &e.buckets {
            let _ = writeln!(s, "bucket {b:#x} {n}");
        }
        for x in &e.samples {
            let _ = writeln!(s, "error {:#x} {:#018x} {:#018x}", x.addr, x.expected, x.actual);
        }
        s
    }
}

/// Parses [`TestResult::to_text`] output. `None` if it is not a result file.
pub fn parse_result(text: &str) -> Option<TestResult> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next()? != "badram-test 1" {
        return None;
    }
    let mut r = TestResult {
        completed: false,
        passes: 0,
        requested: Vec::new(),
        tested: Vec::new(),
        untested: Vec::new(),
        errors: ErrorLog::default(),
    };
    let addr = |s: &str| parse_address(s).ok();
    let range = |a: &str, b: &str| Range::new(addr(a)?, addr(b)?).ok();
    for line in lines {
        let p: Vec<&str> = line.splitn(4, ' ').collect();
        match (p[0], p.len()) {
            ("result", 2) => r.completed = p[1] == "complete",
            ("passes", 2) => r.passes = p[1].parse().ok()?,
            ("requested", 3) => r.requested.push(range(p[1], p[2])?),
            ("tested", 3) => r.tested.push(range(p[1], p[2])?),
            ("untested", 4) => r.untested.push(Untested { range: range(p[1], p[2])?, reason: p[3].into() }),
            ("errors", 2) => r.errors.count = p[1].parse().ok()?,
            ("mask", 2) => r.errors.mask = addr(p[1])?,
            ("lowest", 2) => r.errors.lowest = Some(addr(p[1])?),
            ("highest", 2) => r.errors.highest = Some(addr(p[1])?),
            ("bucket", 3) => {
                r.errors.buckets.insert(addr(p[1])?, p[2].parse().ok()?);
            }
            ("error", 4) => r.errors.samples.push(ErrorSample {
                addr: addr(p[1])?,
                expected: addr(p[2])?,
                actual: addr(p[3])?,
            }),
            _ => {} // forward compatible: ignore unknown lines
        }
    }
    Some(r)
}

// ------------------------------------------------------------------- engine

/// A block of memory under test, addressed in 64-bit words.
pub trait TestMemory {
    /// Physical address of word 0.
    fn base(&self) -> u64;
    fn words(&self) -> usize;
    fn write(&mut self, i: usize, v: u64);
    fn read(&self, i: usize) -> u64;
}

/// Environment hooks for the engine.
pub trait TestHost {
    /// Write back and invalidate CPU caches so reads hit DRAM.
    fn flush_caches(&mut self);
    /// Called periodically. Return `false` to abort.
    fn progress(&mut self, step: &'static str, done: u64, total: u64) -> bool;
}

/// Words between progress callbacks (8 MiB).
const PROGRESS_WORDS: usize = 1 << 20;

struct Run<'a, H: TestHost> {
    host: &'a mut H,
    log: ErrorLog,
    done: u64,
    total: u64,
    step: &'static str,
    aborted: bool,
}

impl<H: TestHost> Run<'_, H> {
    /// Sweeps all regions (ascending or descending), calling `f(mem, i,
    /// address)` per word. Returns false if aborted.
    fn sweep<M: TestMemory>(
        &mut self,
        regions: &mut [M],
        descending: bool,
        mut f: impl FnMut(&mut M, usize, u64, &mut ErrorLog),
    ) -> bool {
        let order: Vec<usize> = if descending {
            (0..regions.len()).rev().collect()
        } else {
            (0..regions.len()).collect()
        };
        for ri in order {
            let m = &mut regions[ri];
            let n = m.words();
            let base = m.base();
            let mut i = 0;
            while i < n {
                let chunk_end = (i + PROGRESS_WORDS).min(n);
                if descending {
                    for k in (n - chunk_end..n - i).rev() {
                        f(m, k, base + (k as u64) * 8, &mut self.log);
                    }
                } else {
                    for k in i..chunk_end {
                        f(m, k, base + (k as u64) * 8, &mut self.log);
                    }
                }
                self.done += (chunk_end - i) as u64;
                i = chunk_end;
                if !self.host.progress(self.step, self.done, self.total) {
                    self.aborted = true;
                    return false;
                }
            }
        }
        true
    }

    fn flush(&mut self) {
        self.host.flush_caches();
    }
}

fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

/// Number of word sweeps one pass performs (for progress totals).
const SWEEPS_PER_PASS: u64 = 2 /* address */ + 4 * 2 /* inversions x2 patterns */ + 2 /* random */;

/// Runs `passes` passes of: own-address, moving inversions (two patterns),
/// and a pseudo-random pattern. Returns the errors and whether it finished.
pub fn run_tests<M: TestMemory, H: TestHost>(regions: &mut [M], passes: u32, host: &mut H) -> (ErrorLog, bool) {
    let words: u64 = regions.iter().map(|m| m.words() as u64).sum();
    let mut run = Run {
        host,
        log: ErrorLog::default(),
        done: 0,
        total: words * SWEEPS_PER_PASS * u64::from(passes),
        step: "",
        aborted: false,
    };

    'passes: for pass in 0..passes {
        // 1. Own address: catches address-line faults and aliasing.
        run.step = "address: write";
        if !run.sweep(regions, false, |m, i, a, _| m.write(i, a)) {
            break;
        }
        run.flush();
        run.step = "address: verify";
        if !run.sweep(regions, false, |m, i, a, log| {
            let v = m.read(i);
            if v != a {
                log.record(a, a, v);
            }
        }) {
            break;
        }

        // 2. Moving inversions with two patterns.
        for p in [0u64, 0x5555_5555_5555_5555] {
            run.step = "inversions: fill";
            if !run.sweep(regions, false, |m, i, _, _| m.write(i, p)) {
                break 'passes;
            }
            run.flush();
            run.step = "inversions: up";
            if !run.sweep(regions, false, |m, i, a, log| {
                let v = m.read(i);
                if v != p {
                    log.record(a, p, v);
                }
                m.write(i, !p);
            }) {
                break 'passes;
            }
            run.flush();
            run.step = "inversions: down";
            if !run.sweep(regions, true, |m, i, a, log| {
                let v = m.read(i);
                if v != !p {
                    log.record(a, !p, v);
                }
                m.write(i, p);
            }) {
                break 'passes;
            }
            run.flush();
            run.step = "inversions: verify";
            if !run.sweep(regions, false, |m, i, a, log| {
                let v = m.read(i);
                if v != p {
                    log.record(a, p, v);
                }
            }) {
                break 'passes;
            }
        }

        // 3. Pseudo-random data derived from the address and pass number.
        let seed = 0x9E37_79B9_7F4A_7C15u64.wrapping_mul(u64::from(pass) + 1);
        let value = move |a: u64| xorshift(a ^ seed) | 1;
        run.step = "random: write";
        if !run.sweep(regions, false, |m, i, a, _| m.write(i, value(a))) {
            break;
        }
        run.flush();
        run.step = "random: verify";
        if !run.sweep(regions, false, |m, i, a, log| {
            let want = value(a);
            let v = m.read(i);
            if v != want {
                log.record(a, want, v);
            }
        }) {
            break;
        }
    }
    let finished = !run.aborted;
    (run.log, finished)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(a: u64, b: u64) -> Range {
        Range::new(a, b).unwrap()
    }

    /// Host memory with optional faults: a stuck-at bit, or an address
    /// alias (writes to word `alias.0` land in word `alias.1`).
    struct FakeMem {
        base: u64,
        data: Vec<u64>,
        stuck: Option<(usize, u64, bool)>, // (word, bit mask, stuck value)
        alias: Option<(usize, usize)>,
    }

    impl FakeMem {
        fn new(base: u64, words: usize) -> Self {
            FakeMem { base, data: vec![0; words], stuck: None, alias: None }
        }
    }

    impl TestMemory for FakeMem {
        fn base(&self) -> u64 {
            self.base
        }
        fn words(&self) -> usize {
            self.data.len()
        }
        fn write(&mut self, i: usize, v: u64) {
            let i = match self.alias {
                Some((from, to)) if i == from => to,
                _ => i,
            };
            self.data[i] = v;
        }
        fn read(&self, i: usize) -> u64 {
            let v = self.data[i];
            match self.stuck {
                Some((w, mask, true)) if w == i => v | mask,
                Some((w, mask, false)) if w == i => v & !mask,
                _ => v,
            }
        }
    }

    struct Host {
        flushes: usize,
        calls: usize,
        abort_after: Option<usize>,
        last_total: u64,
        last_done: u64,
    }

    impl Host {
        fn new() -> Self {
            Host { flushes: 0, calls: 0, abort_after: None, last_total: 0, last_done: 0 }
        }
    }

    impl TestHost for Host {
        fn flush_caches(&mut self) {
            self.flushes += 1;
        }
        fn progress(&mut self, _step: &'static str, done: u64, total: u64) -> bool {
            self.calls += 1;
            self.last_done = done;
            self.last_total = total;
            self.abort_after.is_none_or(|n| self.calls < n)
        }
    }

    #[test]
    fn clean_memory_has_no_errors() {
        let mut mem = [FakeMem::new(0x10_0000, 4096), FakeMem::new(0x20_0000, 1000)];
        let mut host = Host::new();
        let (log, done) = run_tests(&mut mem, 2, &mut host);
        assert!(done);
        assert_eq!(log, ErrorLog::default());
        assert!(host.flushes > 0);
        assert_eq!(host.last_done, host.last_total);
    }

    #[test]
    fn stuck_bit_is_found_with_address_and_mask() {
        let mut m = FakeMem::new(0x1_0000_0000, 4096);
        m.stuck = Some((100, 1 << 40, true));
        let mut host = Host::new();
        let (log, done) = run_tests(core::slice::from_mut(&mut m), 1, &mut host);
        assert!(done);
        let addr = 0x1_0000_0000 + 100 * 8;
        assert!(log.count > 0);
        assert_eq!(log.mask, 1 << 40);
        assert_eq!((log.lowest, log.highest), (Some(addr), Some(addr)));
        assert_eq!(log.buckets.get(&0x1_0000_0000), Some(&log.count));
        assert!(log.samples.iter().all(|s| s.addr == addr));
    }

    #[test]
    fn stuck_low_bit_is_found() {
        let mut m = FakeMem::new(0, 512);
        m.stuck = Some((7, 1, false));
        let (log, _) = run_tests(core::slice::from_mut(&mut m), 1, &mut Host::new());
        assert!(log.count > 0);
        assert_eq!(log.mask, 1);
    }

    #[test]
    fn address_alias_is_found() {
        let mut m = FakeMem::new(0, 512);
        m.alias = Some((10, 20));
        let (log, _) = run_tests(core::slice::from_mut(&mut m), 1, &mut Host::new());
        assert!(log.count > 0);
        assert!(log.samples.iter().any(|s| s.addr == 10 * 8 || s.addr == 20 * 8));
    }

    #[test]
    fn abort_stops_early() {
        let mut m = FakeMem::new(0, PROGRESS_WORDS * 3);
        let mut host = Host::new();
        host.abort_after = Some(2);
        let (_, done) = run_tests(core::slice::from_mut(&mut m), 1, &mut host);
        assert!(!done);
        assert_eq!(host.calls, 2);
    }

    #[test]
    fn samples_are_capped_but_count_is_exact() {
        let mut log = ErrorLog::default();
        for i in 0..(MAX_SAMPLES as u64 + 50) {
            log.record(0x1000 + i * 8, 0, 1);
        }
        assert_eq!(log.count, MAX_SAMPLES as u64 + 50);
        assert_eq!(log.samples.len(), MAX_SAMPLES);
    }

    #[test]
    fn suggested_range() {
        let mut log = ErrorLog::default();
        assert_eq!(log.suggested_range(0, PAGE_SIZE), None);
        log.record(0x1_2345_6788, 0, 1);
        log.record(0x1_3000_0010, 0, 1);
        let mib = 1 << 20;
        assert_eq!(
            log.suggested_range(16 * mib, mib),
            Some(r(0x1_2245_0000 & !(mib - 1), 0x1_3110_0000))
        );
        // Margin larger than the address saturates at 0.
        assert_eq!(log.suggested_range(u64::MAX / 2, mib).unwrap().start(), 0);
    }

    #[test]
    fn request_parse_and_round_trip() {
        let req = parse_request("# test\nscan 0x1000 0x2fff\nscan 0x2000 0x5000\npasses 3\n").unwrap();
        assert_eq!(req, TestRequest { ranges: vec![r(0x1000, 0x5000)], passes: 3 });
        assert_eq!(parse_request(&req.to_text()), Ok(req));
        assert_eq!(parse_request("scan 0x1000 0x2000").unwrap().passes, 1);
    }

    #[test]
    fn request_errors() {
        assert_eq!(parse_request("").unwrap_err().line, 0);
        assert_eq!(parse_request("passes 1").unwrap_err().line, 0);
        assert_eq!(parse_request("scan 0x2000 0x1000").unwrap_err().line, 1);
        assert_eq!(parse_request("scan 0x1000 0x2000\npasses 0").unwrap_err().line, 2);
        assert_eq!(parse_request("scan 0x1000 0x2000\npasses 11").unwrap_err().line, 2);
        assert_eq!(parse_request("scan 0x1000 0x2000\nfoo").unwrap_err().line, 2);
        assert!(
            parse_request("scan 0x1000 0x2000\nfoo")
                .unwrap_err()
                .to_string()
                .starts_with("test.req line 2:")
        );
    }

    #[test]
    fn result_round_trip() {
        let mut errors = ErrorLog::default();
        errors.record(0x1_0000_0100, 0x5555, 0x5554);
        errors.record(0x1_0800_0000, u64::MAX, 0);
        let res = TestResult {
            completed: true,
            passes: 2,
            requested: vec![r(0x1_0000_0000, 0x1_1000_0000)],
            tested: vec![r(0x1_0000_0000, 0x1_0400_0000), r(0x1_0500_0000, 0x1_1000_0000)],
            untested: vec![Untested { range: r(0x1_0400_0000, 0x1_0500_0000), reason: "BootServicesData in use".into() }],
            errors,
        };
        let text = res.to_text();
        assert!(text.starts_with("badram-test 1\nresult complete\n"));
        assert_eq!(parse_result(&text), Some(res.clone()));
        assert_eq!(res.tested_bytes(), 0x0F00_0000);
        assert_eq!(parse_result("something else"), None);

        let aborted = TestResult { completed: false, errors: ErrorLog::default(), ..res };
        assert_eq!(parse_result(&aborted.to_text()), Some(aborted));
    }
}
