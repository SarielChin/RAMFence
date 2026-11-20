//! Install / uninstall / apply config / repairs / headless `--check`.
//! Mirrors windows/*.ps1 step for step. Every step reports through `log`;
//! the first failure aborts and is returned.

use std::fs;
use std::path::{Path, PathBuf};

use badram_core::{Config, Range, config, memtest, range};

use crate::paths::{self, CFG_NAME, EFI_NAME, ENTRY_DESCRIPTION, ESP_DIR, ESP_EFI_PATH, EXE_NAME};
use crate::state::{self, State};
use crate::sysstatus::{self, Verdict};
use crate::win::{self, Icon};
use crate::{bcd, esp, task};

pub type Log<'a> = &'a mut dyn FnMut(String);

/// Parses and normalizes config text exactly like badram.efi does.
pub fn validate_config(text: &str) -> Result<(Config, Vec<Range>), String> {
    let cfg = config::parse(text).map_err(|e| e.to_string())?;
    let ranges = range::normalize(&cfg.ranges).map_err(|e| e.to_string())?;
    Ok((cfg, ranges))
}

/// Text written to disk: a header comment plus the canonical config.
pub fn config_file_text(cfg: &Config) -> String {
    format!(
        "# badram.cfg, written by BadRAM Manager {}\n{}",
        win::local_timestamp(),
        cfg.to_text()
    )
}

fn require_admin() -> Result<(), String> {
    if win::is_elevated() {
        Ok(())
    } else {
        Err("administrator rights are required".into())
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

/// Copies unless `from` and `to` are the same file (e.g. reinstalling the
/// already-installed badram.efi).
fn copy(from: &Path, to: &Path) -> Result<(), String> {
    if same_file(from, to) {
        return Ok(());
    }
    fs::copy(from, to)
        .map(drop)
        .map_err(|e| format!("cannot copy {} to {}: {e}", from.display(), to.display()))
}

fn write(to: &Path, text: &str) -> Result<(), String> {
    fs::write(to, text).map_err(|e| format!("cannot write {}: {e}", to.display()))
}

fn write_bytes(to: &Path, bytes: &[u8]) -> Result<(), String> {
    fs::write(to, bytes).map_err(|e| format!("cannot write {}: {e}", to.display()))
}

/// Where the badram.efi to install comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EfiSource {
    /// The copy bundled into this executable.
    Embedded,
    File(PathBuf),
}

impl EfiSource {
    /// The default: the bundled copy, else a badram.efi found on disk.
    pub fn default_source() -> Option<EfiSource> {
        if paths::EMBEDDED_EFI.is_some() {
            Some(EfiSource::Embedded)
        } else {
            paths::find_efi().map(EfiSource::File)
        }
    }

    pub fn load(&self) -> Result<Vec<u8>, String> {
        let bytes = match self {
            EfiSource::Embedded => paths::EMBEDDED_EFI
                .ok_or("this build has no embedded badram.efi")?
                .to_vec(),
            EfiSource::File(p) => fs::read(p).map_err(|e| format!("cannot read {}: {e}", p.display()))?,
        };
        if bytes.len() < 1024 || !bytes.starts_with(b"MZ") {
            return Err("the selected badram.efi does not look like an EFI executable".into());
        }
        Ok(bytes)
    }
}

#[derive(Clone, Debug)]
pub struct InstallOptions {
    pub config_text: String,
    pub efi: EfiSource,
    pub suspend_bitlocker: bool,
    pub allow_secure_boot: bool,
}

pub fn install(o: &InstallOptions, log: Log) -> Result<(), String> {
    require_admin()?;
    if win::booted_uefi() != Some(true) {
        return Err("Windows was not booted in UEFI mode; badram.efi cannot be used".into());
    }
    let (cfg, ranges) = validate_config(&o.config_text)?;
    log(format!(
        "Config OK: mode {}, {} range(s), {} pages",
        cfg.mode.name(),
        ranges.len(),
        range::total_pages(&ranges)
    ));
    let efi = o.efi.load()?;

    let status = sysstatus::collect();
    if status.secure_boot == Some(true) && !o.allow_secure_boot {
        return Err("Secure Boot is on: the firmware would refuse the unsigned badram.efi and \
                    boot Windows unprotected. Turn Secure Boot off, or enroll this badram.efi's \
                    hash in db and tick \"Allow Secure Boot\"."
            .into());
    }
    if status.bitlocker.as_deref() == Some("On") {
        if !o.suspend_bitlocker {
            return Err("BitLocker protects the system drive. Back up the recovery key \
                        (manage-bde -protectors -get C:), then tick \"Suspend BitLocker\"."
                .into());
        }
        suspend_bitlocker(log)?;
    }
    if status.fast_startup == Some(true) {
        log("Warning: Fast Startup is on; a resume from hibernation skips badram.efi \
             (consider \"powercfg /h off\")."
            .into());
    }

    let dir = paths::install_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let cfg_text = config_file_text(&cfg);
    if let Ok(me) = std::env::current_exe() {
        copy(&me, &dir.join(EXE_NAME))?;
    }
    write_bytes(&dir.join(EFI_NAME), &efi)?;
    write(&dir.join(CFG_NAME), &cfg_text)?;
    let backup = dir.join(format!("BCD-backup-{}", win::local_timestamp().replace(':', "")));
    bcd::export(&backup.to_string_lossy())?;
    log(format!("Files copied to {}; BCD backup: {}", dir.display(), backup.display()));

    esp::with_esp(|root| {
        let esp_dir = root.join(ESP_DIR);
        fs::create_dir_all(&esp_dir).map_err(|e| format!("cannot create {}: {e}", esp_dir.display()))?;
        write_bytes(&esp_dir.join(EFI_NAME), &efi)?;
        write(&esp_dir.join(CFG_NAME), &cfg_text)?;
        if !root.join(r"EFI\Microsoft\Boot\bootmgfw.efi").is_file() {
            log("Warning: bootmgfw.efi not found on this ESP; badram.efi will search other volumes.".into());
        }
        Ok(())
    })?;
    log(format!("Copied badram.efi and badram.cfg to ESP \\{ESP_DIR}"));

    let id = match state::load() {
        Some(s) if bcd::entry_exists(&s.entry_id) => {
            log(format!("Using existing boot entry {}", s.entry_id));
            s.entry_id
        }
        _ => {
            let id = bcd::create_firmware_entry(ENTRY_DESCRIPTION, ESP_EFI_PATH)?;
            state::save(&State {
                entry_id: id.clone(),
                installed_at: win::local_timestamp(),
            })?;
            log(format!("Created firmware boot entry {id}"));
            id
        }
    };

    bcd::set_first(&id)?;
    let order = bcd::firmware_boot_order()?;
    if order.first() != Some(&id) {
        return Err(format!("boot order did not change; first entry is {:?}", order.first()));
    }
    log("BadRAM is first in the firmware boot order".into());

    task::register(&dir.join(EXE_NAME))?;
    log(format!("Registered logon task \"{}\"", paths::TASK_NAME));
    log("Installed. Reboot, then check the Status tab before removing any memory limit.".into());
    Ok(())
}

pub fn uninstall(log: Log) -> Result<(), String> {
    require_admin()?;
    match state::load() {
        Some(s) if bcd::entry_exists(&s.entry_id) => {
            bcd::put_windows_boot_manager_first()?;
            bcd::delete_entry(&s.entry_id)?;
            log(format!("Windows Boot Manager first again; deleted entry {}", s.entry_id));
        }
        _ => log("No recorded BadRAM boot entry (already removed?)".into()),
    }
    esp::with_esp(|root| {
        let d = root.join(ESP_DIR);
        if d.exists() {
            fs::remove_dir_all(&d).map_err(|e| format!("cannot delete {}: {e}", d.display()))?;
        }
        Ok(())
    })?;
    log(format!("Removed \\{ESP_DIR} from the ESP"));
    task::unregister()?;
    log("Removed logon task".into());

    let dir = paths::install_dir();
    let me = std::env::current_exe().ok().and_then(|p| fs::canonicalize(p).ok());
    for name in [EFI_NAME, CFG_NAME, "state.json", "check.ps1", "uninstall.ps1", "common.ps1", EXE_NAME] {
        let p = dir.join(name);
        if !p.exists() {
            continue;
        }
        if fs::canonicalize(&p).ok() == me {
            log(format!("Leaving {} (currently running); delete it manually.", p.display()));
            continue;
        }
        fs::remove_file(&p).map_err(|e| format!("cannot delete {}: {e}", p.display()))?;
    }
    log(format!("Deleted installed files (BCD backups and check.log kept in {})", dir.display()));
    log("Uninstalled. After the next boot Windows can use the bad memory again.".into());
    Ok(())
}

/// Writes a new config to the install directory and the ESP. Takes effect
/// on the next boot.
pub fn apply_config(text: &str, log: Log) -> Result<(), String> {
    require_admin()?;
    if state::load().is_none() {
        return Err("BadRAM is not installed".into());
    }
    let (cfg, ranges) = validate_config(text)?;
    let cfg_text = config_file_text(&cfg);
    write(&paths::installed_cfg(), &cfg_text)?;
    esp::with_esp(|root| write(&root.join(ESP_DIR).join(CFG_NAME), &cfg_text))?;
    log(format!(
        "Config applied ({} pages). It takes effect on the next boot.",
        range::total_pages(&ranges)
    ));
    Ok(())
}

pub fn fix_boot_order(log: Log) -> Result<(), String> {
    require_admin()?;
    let s = state::load().ok_or("BadRAM is not installed")?;
    if !bcd::entry_exists(&s.entry_id) {
        return Err(format!("boot entry {} no longer exists; install again", s.entry_id));
    }
    bcd::set_first(&s.entry_id)?;
    log("BadRAM is first in the firmware boot order again".into());
    Ok(())
}

/// Removes `truncatememory` from the running entry, but only when this boot
/// is verifiably protected.
pub fn remove_memory_limit(log: Log) -> Result<(), String> {
    require_admin()?;
    let st = sysstatus::collect();
    if st.verdict() != Verdict::Protected {
        return Err("this boot is not verified as protected; keep the memory limit".into());
    }
    bcd::remove_current_truncatememory()?;
    log("Removed truncatememory from {current}. All memory is usable after the next boot.".into());
    Ok(())
}

/// What is on the ESP regarding the pre-boot memory test.
#[derive(Clone, Debug)]
pub struct TestFiles {
    /// The installed badram.efi can run memory tests (0.2.0 or later).
    pub efi_supports_test: bool,
    /// `test.req` exists: a test runs on the next boot.
    pub pending: bool,
    /// Last `test-result.txt`, parsed.
    pub result: Option<Result<memtest::TestResult, String>>,
}

pub fn read_test_files() -> Result<TestFiles, String> {
    require_admin()?;
    esp::with_esp(|root| {
        let dir = root.join(ESP_DIR);
        let result = match fs::read_to_string(dir.join(memtest::RESULT_FILE)) {
            Ok(t) => Some(memtest::parse_result(&t).ok_or_else(|| "test-result.txt is not a valid result".to_string())),
            Err(_) => None,
        };
        Ok(TestFiles {
            efi_supports_test: efi_supports_test(&dir.join(EFI_NAME)),
            pending: dir.join(memtest::REQUEST_FILE).is_file(),
            result,
        })
    })
}

/// badram.efi 0.2.0+ embeds this marker (see FEATURES in badram-efi).
const MEMTEST_MARKER: &[u8] = b"BADRAM-FEATURES: memtest1";

fn efi_supports_test(efi: &Path) -> bool {
    fs::read(efi).is_ok_and(|b| contains(&b, MEMTEST_MARKER))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Writes `test.req` (and removes an old result) so badram.efi runs the
/// test once on the next boot.
pub fn schedule_test(request_text: &str, log: Log) -> Result<(), String> {
    require_admin()?;
    if state::load().is_none() {
        return Err("BadRAM is not installed".into());
    }
    let req = memtest::parse_request(request_text).map_err(|e| e.to_string())?;
    esp::with_esp(|root| {
        let dir = root.join(ESP_DIR);
        if !efi_supports_test(&dir.join(EFI_NAME)) {
            return Err("the installed badram.efi is too old to run memory tests; \
                        update the installation first (Install tab)"
                .into());
        }
        let old = dir.join(memtest::RESULT_FILE);
        if old.exists() {
            fs::remove_file(&old).map_err(|e| format!("cannot delete old result: {e}"))?;
        }
        write(&dir.join(memtest::REQUEST_FILE), &req.to_text())
    })?;
    let bytes: u64 = req.ranges.iter().map(Range::len).sum();
    log(format!(
        "Memory test scheduled: {} MiB, {} pass(es). It runs once on the next boot (Esc aborts it).",
        bytes >> 20,
        req.passes
    ));
    Ok(())
}

pub fn cancel_test(log: Log) -> Result<(), String> {
    require_admin()?;
    esp::with_esp(|root| {
        let p = root.join(ESP_DIR).join(memtest::REQUEST_FILE);
        if p.exists() {
            fs::remove_file(&p).map_err(|e| format!("cannot delete {}: {e}", p.display()))?;
        }
        Ok(())
    })?;
    log("Scheduled memory test cancelled".into());
    Ok(())
}

pub fn restart_now(log: Log) -> Result<(), String> {
    log("Restarting…".into());
    win::run_ok("shutdown.exe", &["/r", "/t", "0"]).map(drop)
}

/// Restarts straight into the firmware (BIOS/UEFI) setup screen.
pub fn restart_to_firmware(log: Log) -> Result<(), String> {
    log("Restarting into the firmware settings…".into());
    win::run_ok("shutdown.exe", &["/r", "/fw", "/t", "0"]).map(drop)
}

pub fn suspend_bitlocker(log: Log) -> Result<(), String> {
    require_admin()?;
    win::run_ok(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Suspend-BitLocker -MountPoint $env:SystemDrive -RebootCount 2 | Out-Null",
        ],
    )?;
    log("BitLocker suspended for the next 2 restarts (it resumes by itself).".into());
    Ok(())
}

pub fn disable_hibernation(log: Log) -> Result<(), String> {
    require_admin()?;
    win::run_ok("powercfg.exe", &["/h", "off"])?;
    log("Hibernation and Fast Startup turned off.".into());
    Ok(())
}

/// BitLocker recovery passwords (48 digits in 8 groups) of the system drive.
pub fn bitlocker_recovery_keys() -> Result<Vec<String>, String> {
    require_admin()?;
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let out = win::run_ok("manage-bde.exe", &["-protectors", "-get", &drive, "-Type", "RecoveryPassword"])?;
    let keys = find_recovery_passwords(&out);
    if keys.is_empty() {
        Err("no BitLocker recovery password found on the system drive".into())
    } else {
        Ok(keys)
    }
}

/// Finds `dddddd-dddddd-…` (8 groups of 6 digits) in localized tool output.
pub fn find_recovery_passwords(text: &str) -> Vec<String> {
    let mut keys = Vec::new();
    for token in text.split_whitespace() {
        let groups: Vec<&str> = token.split('-').collect();
        if groups.len() == 8 && groups.iter().all(|g| g.len() == 6 && g.bytes().all(|b| b.is_ascii_digit())) {
            keys.push(token.to_string());
        }
    }
    keys.dedup();
    keys
}

/// Config text for the first-time scan: reserve nothing.
pub fn scan_only_config_text() -> String {
    Config {
        mode: badram_core::Mode::ScanOnly,
        ranges: Vec::new(),
        memory_type: badram_core::MemoryType::Unusable,
        on_error: badram_core::OnError::Prompt,
        timeout_secs: config::DEFAULT_TIMEOUT_SECS,
        chainload: None,
    }
    .to_text()
}

/// One scan range covering all RAM: from 1 MiB to past the highest address
/// any source knows about. Parts that are not RAM are skipped (and listed
/// as "not in memory map") by badram.efi.
pub fn full_scan_range(windows: &[Range], protected: &[Range], installed_bytes: Option<u64>) -> Range {
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    let known_max = windows.iter().chain(protected).map(Range::end).max().unwrap_or(0);
    // With a memory limit (truncatememory), Windows does not report RAM
    // above the limit; installed RAM plus a 4 GiB MMIO hole bounds it.
    let from_installed = installed_bytes.map_or(0, |b| b.saturating_add(4 * GIB));
    let end = known_max.max(from_installed).max(GIB).div_ceil(GIB) * GIB;
    Range::new(MIB, end).expect("end >= 1 GiB > 1 MiB")
}

/// First-time setup, step "find bad memory": installs in scan-only mode if
/// BadRAM is not installed yet, then schedules a scan of `scan`.
pub fn start_first_scan(efi: EfiSource, scan: Range, passes: u32, allow_secure_boot: bool, log: Log) -> Result<(), String> {
    if state::load().is_none() {
        install(
            &InstallOptions {
                config_text: scan_only_config_text(),
                efi,
                suspend_bitlocker: false,
                allow_secure_boot,
            },
            log,
        )?;
    }
    let req = memtest::TestRequest { ranges: vec![scan], passes };
    schedule_test(&req.to_text(), log)
}

/// Writes a protect-mode config with `ranges` to the installation (keeping
/// the other settings of the installed config).
pub fn protect_ranges(ranges: &[Range], log: Log) -> Result<(), String> {
    let mut cfg = match sysstatus::load_installed_config() {
        Some(Ok(c)) => c,
        _ => config::parse(&scan_only_config_text()).map_err(|e| e.to_string())?,
    };
    cfg.mode = badram_core::Mode::Protect;
    cfg.ranges = ranges.to_vec();
    apply_config(&cfg.to_text(), log)
}

fn append_check_log(line: &str) {
    use std::io::Write;
    let line = format!("{}  {line}\r\n", win::local_timestamp());
    if paths::install_dir().is_dir()
        && let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(paths::log_file())
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `--check`: run at logon by the scheduled task. Returns the exit code:
/// 0 OK, 1 check failed, 2 boot not protected, 3 badram.efi/memory problem.
pub fn run_check() -> i32 {
    const TITLE: &str = "BadRAM";
    if !win::is_elevated() {
        append_check_log("Check failed: not running as administrator");
        win::message_box(TITLE, "BadRAM check must run as administrator.", Icon::Error);
        return 1;
    }
    let Some(s) = state::load() else {
        append_check_log("Check failed: BadRAM is not installed");
        return 1;
    };

    // Boot order first, so it is repaired even if we alert below.
    let mut order_note = String::new();
    if !bcd::entry_exists(&s.entry_id) {
        order_note = format!("The BadRAM boot entry {} no longer exists. Install again.", s.entry_id);
    } else {
        match bcd::firmware_boot_order() {
            Ok(order) if order.first() == Some(&s.entry_id) => {}
            Ok(order) => match bcd::set_first(&s.entry_id) {
                Ok(()) => append_check_log(&format!(
                    "Boot order restored: BadRAM is first again (was: {})",
                    order.first().map_or("(none)", |x| x.as_str())
                )),
                Err(e) => order_note = format!("Could not restore the boot order: {e}"),
            },
            Err(e) => order_note = format!("Could not read the boot order: {e}"),
        }
    }
    if !order_note.is_empty() {
        append_check_log(&order_note);
    }
    let with_note = |msg: String| {
        if order_note.is_empty() { msg } else { format!("{msg}\n\n{order_note}") }
    };

    let st = sysstatus::collect();
    match st.verdict() {
        Verdict::Protected => {
            let info = st.badram.as_ref();
            append_check_log(&format!(
                "OK: ranges={} pages={}",
                info.map(|i| i.ranges.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(","))
                    .unwrap_or_default(),
                info.map_or(0, |i| i.pages)
            ));
            if !order_note.is_empty() {
                win::message_box(TITLE, &order_note, Icon::Warning);
            }
            0
        }
        Verdict::ScanOnly => {
            append_check_log("Scan-only mode (setup not finished): nothing is reserved");
            0
        }
        Verdict::NotProtected | Verdict::NotInstalled => {
            append_check_log("NOT PROTECTED: BadRamStatus variable missing");
            win::message_box(
                TITLE,
                &with_note(
                    "This boot did NOT go through badram.efi, so bad memory may be in use right now.\n\n\
                     Possible causes: the boot order was changed (restored now unless noted below), \
                     Secure Boot rejected badram.efi, or Windows resumed from hibernation.\n\n\
                     Save your work and RESTART NOW."
                        .into(),
                ),
                Icon::Error,
            );
            2
        }
        Verdict::Problem(p) => {
            append_check_log(&format!("PROBLEM: {p}"));
            win::message_box(
                TITLE,
                &with_note(format!("Bad memory may not be fully protected.\n\n{p}")),
                Icon::Error,
            );
            3
        }
        Verdict::Unknown(u) => {
            append_check_log(&format!("Check failed: {u}"));
            win::message_box(TITLE, &with_note(format!("BadRAM check failed:\n\n{u}")), Icon::Warning);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_matches_efi_parser() {
        let (cfg, ranges) = validate_config("range 0x1234 0x5678\non_error boot").unwrap();
        assert_eq!(cfg.on_error, badram_core::OnError::Boot);
        assert_eq!(ranges, vec![Range::new(0x1000, 0x6000).unwrap()]);
        assert!(validate_config("range 0x2000 0x1000").is_err());
        assert!(validate_config("").is_err());
    }

    #[test]
    fn recovery_password_extraction() {
        let out = "BitLocker Drive Encryption: Configuration Tool\r\n\
                   Numerical Password:\r\n      ID: {01234567-89AB-CDEF-0123-456789ABCDEF}\r\n      \
                   Password:\r\n        111111-222222-333333-444444-555555-666666-777777-888888\r\n";
        assert_eq!(find_recovery_passwords(out), vec!["111111-222222-333333-444444-555555-666666-777777-888888"]);
        assert!(find_recovery_passwords("123456-123456").is_empty());
        assert!(find_recovery_passwords("12345a-222222-333333-444444-555555-666666-777777-888888").is_empty());
    }

    #[test]
    fn scan_only_config_is_valid() {
        let (cfg, ranges) = validate_config(&scan_only_config_text()).unwrap();
        assert_eq!(cfg.mode, badram_core::Mode::ScanOnly);
        assert!(ranges.is_empty());
    }

    #[test]
    fn full_scan_range_bounds() {
        let gib = 1u64 << 30;
        let r = |a, b| Range::new(a, b).unwrap();
        // Windows map tops out at 25.5 GiB: round up to 26 GiB.
        let win = [r(0x1000, 0x9F000), r(4 * gib, 25 * gib + gib / 2)];
        assert_eq!(full_scan_range(&win, &[], None), r(1 << 20, 26 * gib));
        // Truncated Windows map: installed RAM + 4 GiB wins.
        assert_eq!(full_scan_range(&[r(0x1000, 8 * gib)], &[], Some(24 * gib)), r(1 << 20, 28 * gib));
        // Nothing known: at least 1 GiB.
        assert_eq!(full_scan_range(&[], &[], None), r(1 << 20, gib));
    }

    #[test]
    fn built_efi_has_memtest_marker() {
        // Skipped when the UEFI target has not been built.
        let efi = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/x86_64-unknown-uefi/release/badram.efi");
        if let Ok(bytes) = fs::read(&efi) {
            assert!(contains(&bytes, MEMTEST_MARKER), "{} lacks the marker", efi.display());
        }
        assert!(!contains(b"BADRAM-FEATURES: memtest", MEMTEST_MARKER));
    }

    #[test]
    fn copy_onto_itself_is_a_no_op() {
        let dir = std::env::temp_dir().join(format!("badram-copy-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.bin");
        fs::write(&f, b"content").unwrap();
        copy(&f, &f).unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"content");
        let g = dir.join("y.bin");
        copy(&f, &g).unwrap();
        assert_eq!(fs::read(&g).unwrap(), b"content");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn written_config_reparses() {
        let (cfg, _) = validate_config("range 0x1000 0x2000\ntimeout 9").unwrap();
        let text = config_file_text(&cfg);
        assert!(text.starts_with("# badram.cfg, written by BadRAM Manager "));
        assert_eq!(config::parse(&text).unwrap(), cfg);
    }
}
