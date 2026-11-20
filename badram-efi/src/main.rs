//! badram.efi — reserve bad physical memory before Windows boots, then
//! chainload bootmgfw.efi. See DESIGN.md §2 for the flow.

#![no_main]
#![no_std]

extern crate alloc;

mod chainload;
mod console;
mod memtest;
mod reserve;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use badram_core::config::{self, DEFAULT_TIMEOUT_SECS};
use badram_core::range::{self, PAGE_SIZE};
use badram_core::{Config, MemoryType as CfgMemoryType, Mode, OnError, Range, status};
use uefi::boot;
use uefi::fs::FileSystem;
use uefi::prelude::*;
use uefi::proto::device_path::DevicePathNodeEnum;
use uefi::proto::loaded_image::LoadedImage;
use uefi::runtime::{self, ResetType, VariableAttributes, VariableVendor};
use uefi::{CString16, cstr16, guid};

use console::{Decision, Log};

const CONFIG_NAME: &str = "badram.cfg";
const DEFAULT_CHAINLOAD: &str = "\\EFI\\Microsoft\\Boot\\bootmgfw.efi";

/// Feature marker that BadRAM Manager looks for in the installed binary to
/// tell whether this badram.efi supports memory tests.
static FEATURES: [u8; 26] = *b"BADRAM-FEATURES: memtest1\0";

/// Vendor GUID of the `BadRamStatus` variable (DESIGN.md §2.5).
pub const STATUS_VENDOR: VariableVendor =
    VariableVendor(guid!("5b7c3f1e-9a42-4d8e-b6a1-2f0c8e4d7a93"));

#[entry]
fn main() -> Status {
    if uefi::helpers::init().is_err() {
        return Status::ABORTED;
    }
    // Keep the feature marker in the binary.
    core::hint::black_box(&FEATURES);
    // Firmware arms a 5-minute watchdog before starting a boot option; turn
    // it off so the error prompt can wait. bootmgr arms its own.
    let _ = boot::set_watchdog_timer(0, 0x10000, None);

    console::clear();
    console::title(concat!("badram.efi ", env!("CARGO_PKG_VERSION")));
    let mut log = Log::default();

    let config = match load_config() {
        Ok(c) => Some(c),
        Err(e) => {
            log.error(e);
            None
        }
    };
    let on_error = config.as_ref().map_or(OnError::default(), |c| c.on_error);
    let timeout = config.as_ref().map_or(DEFAULT_TIMEOUT_SECS, |c| c.timeout_secs);
    let mem_type = config.as_ref().map_or(CfgMemoryType::default(), |c| c.memory_type);
    let target = config
        .as_ref()
        .and_then(|c| c.chainload.clone())
        .unwrap_or_else(|| DEFAULT_CHAINLOAD.to_string());

    // A scheduled memory test runs before anything is reserved, so it can
    // also cover the configured bad ranges themselves.
    let tested = image_dir().is_ok_and(|dir| memtest::run_if_requested(&dir, &mut log));

    let mode = config.as_ref().map_or(Mode::Protect, |c| c.mode);
    let mut ranges: Vec<Range> = Vec::new();
    let mut outcome = reserve::Outcome::default();
    if mode == Mode::ScanOnly {
        // Explicitly configured (the bad ranges are not known yet), so not
        // an error; but say clearly that nothing is protected.
        log.warn("scan-only mode: no memory is reserved during this boot");
    } else if let Some(cfg) = &config {
        match range::normalize(&cfg.ranges) {
            Ok(r) => ranges = r,
            Err(e) => log.error(format_args!("{CONFIG_NAME}: {e}")),
        }
        for r in &ranges {
            log.info(format_args!(
                "Range {r} ({}) as {}",
                Size(r.len()),
                mem_type.name()
            ));
        }
        outcome = reserve::run(&ranges, mem_type, &mut log);
    }

    // Written before any prompt so Windows can tell what happened even if
    // the user chooses to boot anyway.
    let status_text = status::format_status(
        log.is_ok(),
        &ranges,
        outcome.protected,
        mem_type,
        mode,
        log.first_error(),
    );
    if let Err(e) = set_status_variable(&status_text) {
        log.error(format_args!("cannot set BadRamStatus variable: {:?}", e.status()));
    }

    let total = range::total_pages(&ranges);
    log.info("");
    let summary = format!(
        "{} of {} pages protected ({}); {} newly reserved by this run.",
        outcome.protected,
        total,
        Size(outcome.protected * PAGE_SIZE),
        outcome.reserved_now
    );
    if log.is_ok() {
        log.good(format_args!("OK: {summary} {} warning(s).", log.warnings()));
    } else {
        log.info(summary);
    }

    // Leave a memory test summary on screen long enough to read.
    let timeout = if tested { timeout.max(30) } else { timeout };
    let decision = if log.is_ok() {
        console::countdown(timeout);
        Decision::Boot
    } else {
        console::handle_error(on_error, timeout, &log)
    };

    match decision {
        Decision::Boot => {}
        Decision::Reboot => runtime::reset(ResetType::COLD, Status::SUCCESS, None),
        Decision::Shutdown => runtime::reset(ResetType::SHUTDOWN, Status::SUCCESS, None),
    }

    // Only returns on failure.
    if let Err(e) = chainload::boot(&target, &mut log) {
        log.error(e);
    }
    match console::after_boot_failure(on_error) {
        Some(Decision::Shutdown) => runtime::reset(ResetType::SHUTDOWN, Status::SUCCESS, None),
        Some(_) => runtime::reset(ResetType::COLD, Status::SUCCESS, None),
        None => Status::LOAD_ERROR,
    }
}

/// Byte count shown as MiB, or KiB when under 1 MiB.
struct Size(u64);

impl core::fmt::Display for Size {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        const MIB: u64 = 1024 * 1024;
        if self.0 >= MIB {
            write!(f, "{} MiB", self.0 / MIB)
        } else {
            write!(f, "{} KiB", self.0 / 1024)
        }
    }
}

fn set_status_variable(text: &str) -> uefi::Result {
    runtime::set_variable(
        cstr16!("BadRamStatus"),
        &STATUS_VENDOR,
        VariableAttributes::BOOTSERVICE_ACCESS | VariableAttributes::RUNTIME_ACCESS,
        text.as_bytes(),
    )
}

/// Directory of this image on its volume, with trailing `\`, e.g. `\EFI\badram\`.
fn image_dir() -> Result<String, String> {
    let li = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle())
        .map_err(|e| format!("cannot open LoadedImage: {:?}", e.status()))?;
    let fp = li
        .file_path()
        .ok_or_else(|| "badram.efi has no file path".to_string())?;

    let mut path = String::new();
    for node in fp.node_iter() {
        if let Ok(DevicePathNodeEnum::MediaFilePath(f)) = node.as_enum() {
            let units: Vec<u16> = f.path_name().to_vec().into_iter().take_while(|&u| u != 0).collect();
            let part = String::from_utf16_lossy(&units).replace('/', "\\");
            if !path.ends_with('\\') && !part.starts_with('\\') {
                path.push('\\');
            }
            path.push_str(&part);
        }
    }
    if path.is_empty() {
        return Err("cannot determine badram.efi file path".to_string());
    }
    if !path.starts_with('\\') {
        path.insert(0, '\\');
    }
    let cut = path.rfind('\\').map_or(1, |i| i + 1);
    path.truncate(cut);
    Ok(path)
}

fn load_config() -> Result<Config, String> {
    let path = format!("{}{CONFIG_NAME}", image_dir()?);
    let path16 = CString16::try_from(path.as_str())
        .map_err(|_| format!("invalid config path '{path}'"))?;
    let sfs = boot::get_image_file_system(boot::image_handle())
        .map_err(|e| format!("cannot open badram.efi's volume: {:?}", e.status()))?;
    let bytes = FileSystem::new(sfs)
        .read(&*path16)
        .map_err(|e| match e {
            uefi::fs::Error::Io(io) => format!("cannot read {path}: {:?}", io.uefi_error.status()),
            other => format!("cannot read {path}: {other}"),
        })?;
    let text = core::str::from_utf8(&bytes).map_err(|_| format!("{path} is not valid UTF-8"))?;
    config::parse(text).map_err(|e| e.to_string())
}
