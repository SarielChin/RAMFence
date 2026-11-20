//! Install / uninstall / apply config / repairs / headless `--check`.
//! Mirrors windows/*.ps1 step for step. Every step reports through `log`;
//! the first failure aborts and is returned.

use std::fs;
use std::path::{Path, PathBuf};

use ramfence_core::{Config, Range, config, memtest, range};

use crate::paths::{self, CFG_NAME, EFI_NAME, ENTRY_DESCRIPTION, ESP_DIR, ESP_EFI_PATH, EXE_NAME};
use crate::state::{self, State};
use crate::sysstatus::{self, Verdict};
use crate::win::{self, Icon};
use crate::{bcd, esp, task};

pub type Log<'a> = &'a mut dyn FnMut(String);

/// Parses and normalizes config text exactly like ramfence.efi does.
pub fn validate_config(text: &str) -> Result<(Config, Vec<Range>), String> {
    let cfg = config::parse(text).map_err(|e| e.to_string())?;
    let ranges = range::normalize(&cfg.ranges).map_err(|e| e.to_string())?;
    Ok((cfg, ranges))
}

/// Text written to disk: a header comment plus the canonical config.
pub fn config_file_text(cfg: &Config) -> String {
    format!(
        "# ramfence.cfg, written by RAMFence Manager {}\n{}",
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
/// already-installed ramfence.efi).
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

/// Where the ramfence.efi to install comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EfiSource {
    /// The copy bundled into this executable.
    Embedded,
    File(PathBuf),
}

impl EfiSource {
    /// The default: the bundled copy, else a ramfence.efi found on disk.
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
                .ok_or("this build has no embedded ramfence.efi")?
                .to_vec(),
            EfiSource::File(p) => fs::read(p).map_err(|e| format!("cannot read {}: {e}", p.display()))?,
        };
        if bytes.len() < 1024 || !bytes.starts_with(b"MZ") {
            return Err("the selected ramfence.efi does not look like an EFI executable".into());
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
    /// The user confirmed the recovery key is at hand although the BitLocker
    /// status cannot be read.
    pub bitlocker_unknown_ok: bool,
}

pub fn install(o: &InstallOptions, log: Log) -> Result<(), String> {
    require_admin()?;
    if win::booted_uefi() != Some(true) {
        return Err("Windows was not booted in UEFI mode; ramfence.efi cannot be used".into());
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
        return Err("Secure Boot is on: the firmware would refuse the unsigned ramfence.efi and \
                    boot Windows unprotected. Turn Secure Boot off, or enroll this ramfence.efi's \
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
    } else if status.bitlocker.is_none() {
        // Unknown is not "off": the drive may still be encrypted.
        if !o.bitlocker_unknown_ok {
            return Err("The BitLocker status of the system drive cannot be read. If the drive is \
                        encrypted (BitLocker or device encryption), Windows asks for the recovery key \
                        after this change. Make sure you have the key (or that the drive is not \
                        encrypted), then confirm it and try again."
                .into());
        }
        log("Warning: BitLocker status unknown; confirmed that the recovery key is at hand.".into());
    }
    if status.fast_startup == Some(true) {
        log("Warning: Fast Startup is on; Windows may restore a session that was started \
             without ramfence.efi (consider \"powercfg /h off\", which also turns off hibernation)."
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
            log("Warning: bootmgfw.efi not found on this ESP; ramfence.efi will search other volumes.".into());
        }
        Ok(())
    })?;
    log(format!("Copied ramfence.efi and ramfence.cfg to ESP \\{ESP_DIR}"));

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
    log("RAMFence is first in the firmware boot order".into());

    task::register(&dir.join(EXE_NAME))?;
    log(format!("Registered logon task \"{}\"", paths::TASK_NAME));
    log("Installed. Reboot, then check the Status tab before removing any memory limit.".into());
    Ok(())
}

pub fn uninstall(log: Log) -> Result<(), String> {
    require_admin()?;
    // Windows Boot Manager first in every case, before ramfence.efi is
    // deleted from the ESP: a first entry pointing to a missing file would
    // leave the next start to the firmware's fallback behaviour.
    bcd::put_windows_boot_manager_first()?;
    log("Windows Boot Manager is first in the firmware boot order".into());
    let mut ids: Vec<String> = state::load()
        .map(|s| s.entry_id)
        .into_iter()
        .filter(|id| bcd::entry_exists(id))
        .collect();
    // Entries the state file does not know about (state.json lost, or an
    // older install): find them by the file they start.
    match bcd::firmware_entries_with_path(ESP_EFI_PATH) {
        Ok(found) => {
            for id in found {
                if !ids.iter().any(|x| x.eq_ignore_ascii_case(&id)) {
                    ids.push(id);
                }
            }
        }
        Err(e) => log(format!("Warning: cannot list firmware boot entries: {e}")),
    }
    if ids.is_empty() {
        log("No RAMFence boot entry found (already removed?)".into());
    }
    for id in ids {
        bcd::delete_entry(&id)?;
        log(format!("Deleted boot entry {id}"));
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
        return Err("RAMFence is not installed".into());
    }
    let (cfg, ranges) = validate_config(text)?;
    let cfg_text = config_file_text(&cfg);
    // ESP first: that copy is what takes effect. If it fails, the installed
    // copy (which the app shows) still matches it.
    esp::with_esp(|root| write(&root.join(ESP_DIR).join(CFG_NAME), &cfg_text))?;
    write(&paths::installed_cfg(), &cfg_text).map_err(|e| {
        format!("{e}. The new configuration is on the ESP and takes effect, but the copy in the install folder is old.")
    })?;
    log(format!(
        "Config applied ({} pages). It takes effect on the next boot.",
        range::total_pages(&ranges)
    ));
    Ok(())
}

pub fn fix_boot_order(log: Log) -> Result<(), String> {
    require_admin()?;
    let s = state::load().ok_or("RAMFence is not installed")?;
    if !bcd::entry_exists(&s.entry_id) {
        return Err(format!("boot entry {} no longer exists; install again", s.entry_id));
    }
    bcd::set_first(&s.entry_id)?;
    log("RAMFence is first in the firmware boot order again".into());
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
    /// The installed ramfence.efi can run memory tests (0.2.0 or later).
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

/// ramfence.efi 0.2.0+ embeds this marker (see FEATURES in ramfence-efi).
const MEMTEST_MARKER: &[u8] = b"RAMFENCE-FEATURES: memtest1";

fn efi_supports_test(efi: &Path) -> bool {
    fs::read(efi).is_ok_and(|b| contains(&b, MEMTEST_MARKER))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Writes `test.req` (and removes an old result) so ramfence.efi runs the
/// test once on the next boot.
pub fn schedule_test(request_text: &str, log: Log) -> Result<(), String> {
    require_admin()?;
    if state::load().is_none() {
        return Err("RAMFence is not installed".into());
    }
    let req = memtest::parse_request(request_text).map_err(|e| e.to_string())?;
    esp::with_esp(|root| {
        let dir = root.join(ESP_DIR);
        if !efi_supports_test(&dir.join(EFI_NAME)) {
            return Err("the installed ramfence.efi is too old to run memory tests; \
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
        mode: ramfence_core::Mode::ScanOnly,
        ranges: Vec::new(),
        memory_type: ramfence_core::MemoryType::Unusable,
        on_error: ramfence_core::OnError::Prompt,
        timeout_secs: config::DEFAULT_TIMEOUT_SECS,
        chainload: None,
    }
    .to_text()
}

/// One scan range covering all RAM: from 1 MiB to past the highest address
/// any source knows about. Parts that are not RAM are skipped (and listed
/// as "not in memory map") by ramfence.efi.
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
/// RAMFence is not installed yet, then schedules a scan of `scan`.
pub fn start_first_scan(
    efi: EfiSource,
    scan: Range,
    passes: u32,
    allow_secure_boot: bool,
    bitlocker_unknown_ok: bool,
    log: Log,
) -> Result<(), String> {
    if state::load().is_none() {
        install(
            &InstallOptions {
                config_text: scan_only_config_text(),
                efi,
                suspend_bitlocker: false,
                allow_secure_boot,
                bitlocker_unknown_ok,
            },
            log,
        )?;
    }
    let req = memtest::TestRequest { ranges: vec![scan], passes };
    schedule_test(&req.to_text(), log)
}

// ------------------------------------------------------------- rescue USB

/// A USB disk as reported by `Get-Disk`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UsbDisk {
    pub number: u32,
    pub friendly_name: String,
    pub size: u64,
    pub is_boot: bool,
    pub is_system: bool,
    /// Device-specific id (`Get-Disk` UniqueId); may be missing.
    #[serde(default)]
    pub unique_id: Option<String>,
    #[serde(default)]
    pub serial_number: Option<String>,
}

impl UsbDisk {
    /// Disks Windows itself starts from must never be erased.
    pub fn erasable(&self) -> bool {
        !self.is_boot && !self.is_system
    }

    /// Identifies this physical drive, so that a different drive that got
    /// the same disk number (two sticks of the same model swapped, say) is
    /// not mistaken for it.
    pub fn identity(&self) -> String {
        let t = |s: &Option<String>| s.as_deref().unwrap_or("").trim().to_string();
        format!("{}|{}|{}|{}", self.size, self.friendly_name.trim(), t(&self.unique_id), t(&self.serial_number))
    }
}

/// Standard base64, used to pass arbitrary strings (disk ids) into a
/// PowerShell script without quoting issues.
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn parse_usb_disks(json: &str) -> Result<Vec<UsbDisk>, String> {
    let json = json.trim();
    if json.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(json).map_err(|e| format!("cannot read the disk list: {e}"))
}

pub fn list_usb_disks() -> Result<Vec<UsbDisk>, String> {
    let out = win::run_ok(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "ConvertTo-Json -Compress -InputObject @(Get-Disk | Where-Object { $_.BusType -eq 'USB' } | \
             Select-Object Number, FriendlyName, @{n='Size';e={[uint64]$_.Size}}, IsBoot, IsSystem, UniqueId, SerialNumber)",
        ],
    )?;
    parse_usb_disks(&out)
}

const USB_CONFIG: &str = "\
# RAMFence rescue USB: scan all memory on the first start, save the result on
# this stick, then reserve the damaged areas on every start.
mode auto
on_error prompt
timeout 10
";

const USB_README: &str = "\
RAMFence rescue USB
=================

This stick protects a PC with damaged memory (RAM), even when Windows
cannot start normally.

1. Secure Boot must be off in the PC's firmware settings (BIOS). If the PC
   uses BitLocker or device encryption, have the recovery key ready
   (aka.ms/myrecoverykey): starting from this stick can make Windows ask
   for it.
2. Plug in the stick, switch the PC on and open the boot menu (often F12,
   F11, F8, Esc or F10 while the maker's logo shows). Choose the USB stick
   (the entry marked UEFI).
3. First start: RAMFence tests all memory (a few minutes; Esc stops it),
   saves the result on this stick, reserves the damaged areas, then starts
   Windows. Later starts on the same PC skip the test (another PC, or
   changed memory or firmware, is tested again).
4. In Windows, run RAMFence-Manager.exe from this stick (as administrator)
   and follow the Setup tab to install RAMFence on the PC. After that the
   stick is no longer needed.

To test again, delete EFI\\BOOT\\test-result.txt from the stick.
";

/// Erases `disk` and makes it a rescue USB: ramfence.efi as the removable
/// boot loader (\EFI\BOOT\BOOTX64.EFI) in `mode auto`, plus this manager
/// and a README.
pub fn create_usb(disk: &UsbDisk, efi: &EfiSource, log: Log) -> Result<(), String> {
    require_admin()?;
    if !disk.erasable() {
        return Err("refusing to erase a disk Windows starts from".into());
    }
    let efi_bytes = efi.load()?;
    let n = disk.number;
    let size = disk.size;
    let b64 = |s: Option<&str>| base64(s.unwrap_or("").trim().as_bytes());
    let (name, uid, serial) = (
        b64(Some(&disk.friendly_name)),
        b64(disk.unique_id.as_deref()),
        b64(disk.serial_number.as_deref()),
    );
    // Re-checks the disk right before erasing: numbers can change when
    // drives are plugged in or out, so the size, name, unique id and serial
    // number must all still match the drive that was selected.
    let script = format!(
        "$ErrorActionPreference = 'Stop'
function Dec([string]$s) {{ [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($s)) }}
$d = Get-Disk -Number {n}
if ($d.BusType -ne 'USB' -or [uint64]$d.Size -ne {size} -or $d.IsBoot -or $d.IsSystem -or
    \"$($d.FriendlyName)\".Trim() -cne (Dec '{name}') -or
    \"$($d.UniqueId)\".Trim() -cne (Dec '{uid}') -or
    \"$($d.SerialNumber)\".Trim() -cne (Dec '{serial}')) {{
    throw 'Disk {n} is no longer the selected USB drive. Refresh the list and try again.'
}}
if ($d.PartitionStyle -ne 'RAW') {{ Clear-Disk -Number {n} -RemoveData -RemoveOEM -Confirm:$false }}
if ((Get-Disk -Number {n}).PartitionStyle -eq 'RAW') {{ Initialize-Disk -Number {n} -PartitionStyle GPT }}
$d = Get-Disk -Number {n}
$size = [Math]::Min([uint64]2GB, [uint64]$d.LargestFreeExtent)
if ($d.PartitionStyle -eq 'GPT') {{
    $p = New-Partition -DiskNumber {n} -Size $size -GptType '{{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}}' -AssignDriveLetter
}} else {{
    $p = New-Partition -DiskNumber {n} -Size $size -MbrType FAT32 -AssignDriveLetter
}}
$v = Format-Volume -Partition $p -FileSystem FAT32 -NewFileSystemLabel 'RAMFENCE' -Confirm:$false
if (-not $v.DriveLetter) {{ throw 'The new partition got no drive letter.' }}
Write-Output (\"LETTER=\" + $v.DriveLetter)"
    );
    log(format!("Erasing disk {n} ({}) and creating a FAT32 partition…", disk.friendly_name));
    let out = win::run_ok("powershell.exe", &["-NoProfile", "-NonInteractive", "-Command", &script])?;
    let letter = out
        .lines()
        .find_map(|l| l.trim().strip_prefix("LETTER="))
        .and_then(|s| s.chars().next())
        .ok_or_else(|| format!("could not find the new drive letter: {}", out.trim()))?;
    let root = PathBuf::from(format!("{letter}:\\"));
    log(format!("Formatted as {letter}:"));

    let boot = root.join(r"EFI\BOOT");
    fs::create_dir_all(&boot).map_err(|e| format!("cannot create {}: {e}", boot.display()))?;
    write_bytes(&boot.join("BOOTX64.EFI"), &efi_bytes)?;
    write(&boot.join(CFG_NAME), USB_CONFIG)?;
    if let Ok(me) = std::env::current_exe() {
        copy(&me, &root.join("RAMFence-Manager.exe"))?;
    }
    write(&root.join("README.txt"), &USB_README.replace('\n', "\r\n"))?;
    log(format!(
        "Rescue USB ready on {letter}: (ramfence.efi in auto mode, RAMFence-Manager.exe, README.txt). \
         Eject it safely before unplugging."
    ));
    Ok(())
}

/// Installs RAMFence in protect mode with `ranges` and default settings (used
/// after starting from a rescue USB stick).
pub fn install_ranges(ranges: &[Range], efi: EfiSource, bitlocker_unknown_ok: bool, log: Log) -> Result<(), String> {
    let cfg = Config {
        mode: ramfence_core::Mode::Protect,
        ranges: ranges.to_vec(),
        memory_type: ramfence_core::MemoryType::Unusable,
        on_error: ramfence_core::OnError::Prompt,
        timeout_secs: config::DEFAULT_TIMEOUT_SECS,
        chainload: None,
    };
    install(
        &InstallOptions {
            config_text: cfg.to_text(),
            efi,
            suspend_bitlocker: false,
            allow_secure_boot: false,
            bitlocker_unknown_ok,
        },
        log,
    )
}

/// Writes a protect-mode config with `ranges` to the installation (keeping
/// the other settings of the installed config).
pub fn protect_ranges(ranges: &[Range], log: Log) -> Result<(), String> {
    let mut cfg = match sysstatus::load_installed_config() {
        Some(Ok(c)) => c,
        _ => config::parse(&scan_only_config_text()).map_err(|e| e.to_string())?,
    };
    cfg.mode = ramfence_core::Mode::Protect;
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
/// 0 OK, 1 check failed, 2 boot not protected, 3 ramfence.efi/memory problem.
pub fn run_check() -> i32 {
    const TITLE: &str = "RAMFence";
    if !win::is_elevated() {
        append_check_log("Check failed: not running as administrator");
        win::message_box(TITLE, "RAMFence check must run as administrator.", Icon::Error);
        return 1;
    }
    let Some(s) = state::load() else {
        append_check_log("Check failed: RAMFence is not installed");
        return 1;
    };

    // Boot order first, so it is repaired even if we alert below.
    let mut order_note = String::new();
    if !bcd::entry_exists(&s.entry_id) {
        order_note = format!("The RAMFence boot entry {} no longer exists. Install again.", s.entry_id);
    } else {
        match bcd::firmware_boot_order() {
            Ok(order) if order.first() == Some(&s.entry_id) => {}
            Ok(order) => match bcd::set_first(&s.entry_id) {
                Ok(()) => append_check_log(&format!(
                    "Boot order restored: RAMFence is first again (was: {})",
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
            let info = st.ramfence.as_ref();
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
            append_check_log("NOT PROTECTED: RamFenceStatus variable missing");
            win::message_box(
                TITLE,
                &with_note(
                    "This boot did NOT go through ramfence.efi, so bad memory may be in use right now.\n\n\
                     Possible causes: the boot order was changed (restored now unless noted below), \
                     or Secure Boot rejected ramfence.efi.\n\n\
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
            win::message_box(TITLE, &with_note(format!("RAMFence check failed:\n\n{u}")), Icon::Warning);
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
        assert_eq!(cfg.on_error, ramfence_core::OnError::Boot);
        assert_eq!(ranges, vec![Range::new(0x1000, 0x6000).unwrap()]);
        assert!(validate_config("range 0x2000 0x1000").is_err());
        assert!(validate_config("").is_err());
    }

    #[test]
    fn usb_disk_list_parsing() {
        let json = r#"[{"Number":2,"FriendlyName":"Some USB Stick","Size":30752000000,"IsBoot":false,"IsSystem":false,
                        "UniqueId":"USBSTOR\\DISK&VEN_X&PROD_Y\\AA11&0","SerialNumber":"AA11  "},
                       {"Number":3,"FriendlyName":"Boot USB","Size":64000000000,"IsBoot":true,"IsSystem":false,
                        "UniqueId":null,"SerialNumber":null},
                       {"Number":4,"FriendlyName":"Old","Size":1,"IsBoot":false,"IsSystem":false}]"#;
        let disks = parse_usb_disks(json).unwrap();
        assert_eq!(disks.len(), 3);
        assert_eq!(disks[0].number, 2);
        assert!(disks[0].erasable());
        assert!(!disks[1].erasable());
        assert_eq!(disks[0].serial_number.as_deref(), Some("AA11  "));
        assert_eq!(disks[2].unique_id, None);
        // Same model, same size, different stick: different identity.
        let mut other = disks[0].clone();
        other.serial_number = Some("BB22".into());
        assert_ne!(disks[0].identity(), other.identity());
        assert!(parse_usb_disks("").unwrap().is_empty());
        assert!(parse_usb_disks("[]").unwrap().is_empty());
        assert!(parse_usb_disks("not json").is_err());
    }

    #[test]
    fn base64_encoding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64("USB'\u{2019}\"".as_bytes()), "VVNCJ+KAmSI=");
    }

    #[test]
    fn usb_config_is_valid_auto_mode() {
        let (cfg, ranges) = validate_config(USB_CONFIG).unwrap();
        assert_eq!(cfg.mode, ramfence_core::Mode::Auto);
        assert!(ranges.is_empty());
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
        assert_eq!(cfg.mode, ramfence_core::Mode::ScanOnly);
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
            .join("../target/x86_64-unknown-uefi/release/ramfence.efi");
        if let Ok(bytes) = fs::read(&efi) {
            assert!(contains(&bytes, MEMTEST_MARKER), "{} lacks the marker", efi.display());
        }
        assert!(!contains(b"RAMFENCE-FEATURES: memtest", MEMTEST_MARKER));
    }

    #[test]
    fn copy_onto_itself_is_a_no_op() {
        let dir = std::env::temp_dir().join(format!("ramfence-copy-test-{}", std::process::id()));
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
        assert!(text.starts_with("# ramfence.cfg, written by RAMFence Manager "));
        assert_eq!(config::parse(&text).unwrap(), cfg);
    }
}
