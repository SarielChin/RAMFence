//! Collects everything the Status tab and `--check` need. Every probe is
//! independent; failures are recorded instead of aborting.

use ramfence_core::status::{StatusInfo, parse_status};
use ramfence_core::{Config, Mode, Range, config, range};

use crate::paths::{STATUS_VARIABLE, STATUS_VENDOR_GUID, installed_cfg};
use crate::state::{self, State};
use crate::{bcd, physmem, task, win};

#[derive(Clone, Debug)]
pub struct SystemStatus {
    pub elevated: bool,
    pub uefi: Option<bool>,
    pub secure_boot: Option<bool>,
    /// "On" / "Off" (BitLocker ProtectionStatus) or None if unknown.
    pub bitlocker: Option<String>,
    pub fast_startup: Option<bool>,
    pub installed_mem: Option<u64>,
    pub visible_mem: Option<u64>,
    pub state: Option<State>,
    pub entry_exists: Option<bool>,
    pub boot_order: Result<Vec<String>, String>,
    /// Raw RamFenceStatus text; Ok(None) = variable missing (boot not protected).
    pub ramfence_raw: Result<Option<String>, String>,
    pub ramfence: Option<StatusInfo>,
    pub windows_ranges: Result<Vec<Range>, String>,
    pub installed_config: Option<Result<Config, String>>,
    pub truncatememory: Result<Option<String>, String>,
    pub task_exists: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// ramfence.efi ran with ok=1 and Windows uses none of the ranges.
    Protected,
    /// ramfence.efi ran but reported an error, or Windows uses bad memory.
    Problem(String),
    /// This boot did not go through ramfence.efi.
    NotProtected,
    /// ramfence.efi ran in scan-only mode (first-time setup): nothing is
    /// reserved yet, on purpose.
    ScanOnly,
    NotInstalled,
    Unknown(String),
}

impl SystemStatus {
    pub fn installed(&self) -> bool {
        self.state.is_some()
    }

    /// Ranges ramfence.efi reported, else the installed config's ranges.
    pub fn protected_ranges(&self) -> Vec<Range> {
        if let Some(info) = &self.ramfence {
            return info.ranges.clone();
        }
        match &self.installed_config {
            Some(Ok(c)) => range::normalize(&c.ranges).unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// Ranges the installed protect-mode configuration asks for (empty for
    /// other modes or if it cannot be read).
    pub fn configured_ranges(&self) -> Vec<Range> {
        match &self.installed_config {
            Some(Ok(c)) if c.mode == Mode::Protect => range::normalize(&c.ranges).unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// Parts of the reported and the configured ranges that Windows uses
    /// (should be empty).
    pub fn overlaps(&self) -> Option<Vec<Range>> {
        let win = self.windows_ranges.as_ref().ok()?;
        let mut all = self.protected_ranges();
        all.extend(self.configured_ranges());
        let all = range::normalize(&all).unwrap_or(all);
        Some(physmem::overlaps(win, &all))
    }

    /// Configured ranges this boot did not reserve (should be empty). Only
    /// meaningful when ramfence.efi ran and reported success.
    pub fn missing_ranges(&self) -> Vec<Range> {
        match &self.ramfence {
            Some(info) if info.ok && !info.scan_only => range::subtract(&self.configured_ranges(), &info.ranges),
            _ => Vec::new(),
        }
    }

    pub fn ramfence_first(&self) -> Option<bool> {
        let id = &self.state.as_ref()?.entry_id;
        let order = self.boot_order.as_ref().ok()?;
        Some(order.first() == Some(id))
    }

    pub fn verdict(&self) -> Verdict {
        if !self.elevated {
            return Verdict::Unknown("run as administrator to read the boot status".into());
        }
        let missing = self.missing_ranges();
        if !missing.is_empty() {
            let list: Vec<String> = missing.iter().map(|r| r.to_string()).collect();
            return Verdict::Problem(format!(
                "this boot did not reserve the configured range(s) {}. It probably started from another \
                 copy of ramfence.efi (for example a rescue USB stick), or the configuration on the EFI \
                 System Partition differs from the installed one.",
                list.join(", ")
            ));
        }
        if let Some(o) = self.overlaps()
            && !o.is_empty()
        {
            let list: Vec<String> = o.iter().map(|r| r.to_string()).collect();
            return Verdict::Problem(format!("Windows is using protected memory: {}", list.join(", ")));
        }
        match (&self.ramfence_raw, &self.ramfence) {
            (Err(e), _) => Verdict::Unknown(e.clone()),
            (Ok(None), _) if !self.installed() => Verdict::NotInstalled,
            (Ok(None), _) => Verdict::NotProtected,
            (Ok(Some(raw)), None) => Verdict::Problem(format!("unreadable RamFenceStatus: {raw}")),
            (Ok(Some(_)), Some(info)) if info.ok && info.scan_only => Verdict::ScanOnly,
            (Ok(Some(_)), Some(info)) if info.ok && info.scan_stopped => Verdict::Problem(
                "the memory scan of the rescue USB stick was stopped before it finished, so only the \
                 errors found so far are reserved. Start from the stick again and let the scan finish."
                    .into(),
            ),
            (Ok(Some(_)), Some(info)) if !info.ok => Verdict::Problem(format!(
                "ramfence.efi reported an error: {}",
                info.error.as_deref().unwrap_or("(no details)")
            )),
            (Ok(Some(_)), Some(_)) => Verdict::Protected,
        }
    }
}

fn bitlocker_status() -> Option<String> {
    let out = win::run(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-BitLockerVolume -MountPoint $env:SystemDrive -ErrorAction Stop).ProtectionStatus",
        ],
    )
    .ok()?;
    let s = out.text.trim();
    (out.code == 0 && (s == "On" || s == "Off")).then(|| s.to_string())
}

fn fast_startup() -> Option<bool> {
    let hiberboot = win::reg_hklm_dword(
        r"SYSTEM\CurrentControlSet\Control\Session Manager\Power",
        "HiberbootEnabled",
    )?;
    let hibernate = win::reg_hklm_dword(r"SYSTEM\CurrentControlSet\Control\Power", "HibernateEnabled")
        .unwrap_or(1);
    Some(hiberboot == 1 && hibernate == 1)
}

pub fn read_ramfence_status() -> Result<Option<String>, String> {
    Ok(win::read_firmware_variable(STATUS_VARIABLE, STATUS_VENDOR_GUID)?
        .map(|b| String::from_utf8_lossy(&b).trim_end_matches('\0').to_string()))
}

pub fn load_installed_config() -> Option<Result<Config, String>> {
    let text = std::fs::read_to_string(installed_cfg()).ok()?;
    Some(config::parse(&text).map_err(|e| e.to_string()))
}

pub fn collect() -> SystemStatus {
    let elevated = win::is_elevated();
    let (installed_mem, visible_mem) = win::memory_totals();
    let state = state::load();
    let ramfence_raw = if elevated {
        read_ramfence_status()
    } else {
        Err("administrator rights needed".into())
    };
    let ramfence = ramfence_raw.as_ref().ok().and_then(|o| o.as_deref()).and_then(parse_status);
    fn need_admin<T>() -> Result<T, String> {
        Err("administrator rights needed".into())
    }
    SystemStatus {
        elevated,
        uefi: win::booted_uefi(),
        secure_boot: win::reg_hklm_dword(
            r"SYSTEM\CurrentControlSet\Control\SecureBoot\State",
            "UEFISecureBootEnabled",
        )
        .map(|v| v == 1),
        bitlocker: if elevated { bitlocker_status() } else { None },
        fast_startup: fast_startup(),
        installed_mem,
        visible_mem,
        entry_exists: match (&state, elevated) {
            (Some(s), true) => Some(bcd::entry_exists(&s.entry_id)),
            _ => None,
        },
        state,
        boot_order: if elevated { bcd::firmware_boot_order() } else { need_admin() },
        ramfence_raw,
        ramfence,
        windows_ranges: physmem::windows_ranges(),
        installed_config: load_installed_config(),
        truncatememory: if elevated { bcd::current_truncatememory() } else { need_admin() },
        task_exists: task::exists(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> SystemStatus {
        SystemStatus {
            elevated: true,
            uefi: Some(true),
            secure_boot: Some(false),
            bitlocker: Some("Off".into()),
            fast_startup: Some(false),
            installed_mem: None,
            visible_mem: None,
            state: Some(State { entry_id: "{e}".into(), installed_at: String::new() }),
            entry_exists: Some(true),
            boot_order: Ok(vec!["{e}".into(), "{w}".into()]),
            ramfence_raw: Ok(Some("version=1;ok=1;ranges=0x10000-0x20000;pages=16;type=unusable".into())),
            ramfence: parse_status("version=1;ok=1;ranges=0x10000-0x20000;pages=16;type=unusable"),
            windows_ranges: Ok(vec![Range::new(0x1000, 0x10000).unwrap(), Range::new(0x20000, 0x90000).unwrap()]),
            installed_config: None,
            truncatememory: Ok(None),
            task_exists: true,
        }
    }

    #[test]
    fn verdicts() {
        let s = base();
        assert_eq!(s.verdict(), Verdict::Protected);
        assert_eq!(s.ramfence_first(), Some(true));

        let mut s = base();
        s.ramfence_raw = Ok(None);
        s.ramfence = None;
        assert_eq!(s.verdict(), Verdict::NotProtected);
        s.state = None;
        assert_eq!(s.verdict(), Verdict::NotInstalled);

        let mut s = base();
        s.windows_ranges = Ok(vec![Range::new(0x1000, 0x18000).unwrap()]);
        assert!(matches!(s.verdict(), Verdict::Problem(_)));

        let mut s = base();
        s.ramfence = parse_status("version=1;ok=0;ranges=;pages=0;type=unusable;error=boom");
        assert_eq!(s.verdict(), Verdict::Problem("ramfence.efi reported an error: boom".into()));

        let mut s = base();
        s.boot_order = Ok(vec!["{w}".into(), "{e}".into()]);
        assert_eq!(s.ramfence_first(), Some(false));

        let mut s = base();
        s.elevated = false;
        assert!(matches!(s.verdict(), Verdict::Unknown(_)));

        let mut s = base();
        s.ramfence = parse_status("version=1;ok=1;ranges=;pages=0;type=unusable;mode=scan-only");
        assert_eq!(s.verdict(), Verdict::ScanOnly);
    }

    fn cfg(text: &str) -> Option<Result<Config, String>> {
        Some(config::parse(text).map_err(|e| e.to_string()))
    }

    #[test]
    fn verdict_compares_with_installed_config() {
        // The installed config matches what this boot reserved.
        let mut s = base();
        s.installed_config = cfg("range 0x10000 0x20000\n");
        assert_eq!(s.verdict(), Verdict::Protected);

        // Booted from another copy (e.g. a rescue USB stick that found
        // nothing): ok=1 but the configured range was not reserved.
        let mut s = base();
        s.installed_config = cfg("range 0x10000 0x20000\n");
        s.ramfence = parse_status("version=1;ok=1;ranges=;pages=0;type=unusable;mode=auto");
        s.windows_ranges = Ok(vec![]);
        assert_eq!(s.missing_ranges(), vec![Range::new(0x10000, 0x20000).unwrap()]);
        assert!(matches!(s.verdict(), Verdict::Problem(p) if p.contains("0x10000-0x20000")));

        // Windows uses a configured range that this boot did not report.
        let mut s = base();
        s.installed_config = cfg("range 0x10000 0x20000\nrange 0x30000 0x40000\n");
        s.windows_ranges = Ok(vec![Range::new(0x30000, 0x40000).unwrap()]);
        assert!(matches!(s.verdict(), Verdict::Problem(_)));

        // Non-protect configs have no ranges to compare.
        let mut s = base();
        s.installed_config = cfg("mode scan-only\n");
        assert!(s.configured_ranges().is_empty());
        assert_eq!(s.verdict(), Verdict::Protected);
    }

    #[test]
    fn stopped_scan_is_a_problem() {
        let mut s = base();
        s.state = None;
        s.ramfence = parse_status("version=1;ok=1;ranges=0x10000-0x20000;pages=16;type=unusable;mode=auto;scan=stopped");
        assert!(matches!(s.verdict(), Verdict::Problem(p) if p.contains("stopped")));
    }
}
