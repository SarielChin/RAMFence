//! Human-readable rows for a [`SystemStatus`], shared by the Status tab and
//! the `--status` text output.

use ramfence_core::PAGE_SIZE;

use crate::sysstatus::{SystemStatus, Verdict};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Good,
    Info,
    Warn,
    Bad,
}

impl Level {
    pub fn symbol(self) -> &'static str {
        match self {
            Level::Good => "OK",
            Level::Info => "--",
            Level::Warn => "!!",
            Level::Bad => "XX",
        }
    }
}

pub struct Row {
    pub label: &'static str,
    pub level: Level,
    pub text: String,
}

fn row(label: &'static str, level: Level, text: impl Into<String>) -> Row {
    Row { label, level, text: text.into() }
}

pub fn size(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{} KiB", bytes / 1024)
    }
}

/// One-line headline and its level.
pub fn headline(s: &SystemStatus) -> (Level, String) {
    match s.verdict() {
        Verdict::Protected => {
            let pages = s.ramfence.as_ref().map_or(0, |i| i.pages);
            (
                Level::Good,
                format!(
                    "Protected: {} pages ({}) reserved before Windows started; Windows does not use them.",
                    pages,
                    size(pages * PAGE_SIZE)
                ),
            )
        }
        Verdict::NotProtected => (
            Level::Bad,
            "NOT protected: this boot did not go through ramfence.efi. Restart (and check the boot order)."
                .into(),
        ),
        Verdict::Problem(p) => (Level::Bad, p),
        Verdict::ScanOnly => (
            Level::Warn,
            "Scan-only mode: RAMFence can test memory but protects nothing yet. Finish the Setup tab.".into(),
        ),
        Verdict::NotInstalled => (Level::Info, "RAMFence is not installed.".into()),
        Verdict::Unknown(u) => (Level::Warn, format!("Status unknown: {u}")),
    }
}

fn yes_no(b: Option<bool>, yes: (Level, &str), no: (Level, &str)) -> (Level, String) {
    match b {
        Some(true) => (yes.0, yes.1.into()),
        Some(false) => (no.0, no.1.into()),
        None => (Level::Info, "unknown".into()),
    }
}

pub fn rows(s: &SystemStatus) -> Vec<Row> {
    let mut v = Vec::new();
    v.push(row(
        "Administrator",
        if s.elevated { Level::Good } else { Level::Warn },
        if s.elevated { "yes" } else { "no (most checks need administrator rights)" },
    ));
    let (l, t) = yes_no(s.uefi, (Level::Good, "UEFI"), (Level::Bad, "Legacy BIOS (not supported)"));
    v.push(row("Boot mode", l, t));
    let (l, t) = yes_no(
        s.secure_boot,
        (Level::Warn, "On (unsigned ramfence.efi is refused unless its hash is enrolled)"),
        (Level::Good, "Off"),
    );
    v.push(row("Secure Boot", l, t));
    v.push(match s.bitlocker.as_deref() {
        Some("On") => row("BitLocker", Level::Warn, "On (have the recovery key before changing boot settings)"),
        Some(x) => row("BitLocker", Level::Good, x.to_string()),
        None => row("BitLocker", Level::Info, "unknown"),
    });
    let (l, t) = yes_no(
        s.fast_startup,
        (Level::Warn, "On (a resume from hibernation skips ramfence.efi)"),
        (Level::Good, "Off"),
    );
    v.push(row("Fast Startup", l, t));

    match &s.state {
        Some(st) => {
            v.push(row("Installed", Level::Good, format!("yes, since {}", st.installed_at)));
            let (l, t) = yes_no(
                s.entry_exists,
                (Level::Good, &format!("{} exists", st.entry_id)),
                (Level::Bad, &format!("{} is missing; install again", st.entry_id)),
            );
            v.push(row("Boot entry", l, t));
            v.push(match (&s.boot_order, s.ramfence_first()) {
                (_, Some(true)) => row("Boot order", Level::Good, "RAMFence is first"),
                (Ok(o), Some(false)) => row(
                    "Boot order",
                    Level::Bad,
                    format!("RAMFence is not first (first: {})", o.first().map_or("none", |x| x)),
                ),
                (Err(e), _) => row("Boot order", Level::Info, e.clone()),
                _ => row("Boot order", Level::Info, "unknown"),
            });
            v.push(row(
                "Logon check",
                if s.task_exists { Level::Good } else { Level::Warn },
                if s.task_exists { "scheduled task present" } else { "scheduled task missing" },
            ));
        }
        None => v.push(row("Installed", Level::Info, "no")),
    }

    v.push(match &s.ramfence_raw {
        Ok(Some(raw)) => row(
            "RamFenceStatus",
            match &s.ramfence {
                Some(i) if i.ok && i.scan_only => Level::Warn,
                Some(i) if i.ok => Level::Good,
                _ => Level::Bad,
            },
            raw.clone(),
        ),
        Ok(None) => row(
            "RamFenceStatus",
            if s.installed() { Level::Bad } else { Level::Info },
            "missing (this boot did not go through ramfence.efi)",
        ),
        Err(e) => row("RamFenceStatus", Level::Info, e.clone()),
    });

    if let (Some(inst), Some(vis)) = (s.installed_mem, s.visible_mem) {
        v.push(row(
            "Memory",
            Level::Info,
            format!("{} usable by Windows of {} installed", size(vis), size(inst)),
        ));
    }

    let protected = s.protected_ranges();
    v.push(match (s.overlaps(), &s.windows_ranges) {
        (_, Err(e)) => row("Windows memory map", Level::Info, e.clone()),
        (_, Ok(_)) if protected.is_empty() => row("Windows memory map", Level::Info, "no ranges to compare"),
        (Some(o), Ok(_)) if o.is_empty() => row(
            "Windows memory map",
            Level::Good,
            "Windows uses none of the protected ranges",
        ),
        (Some(o), Ok(_)) => row(
            "Windows memory map",
            Level::Bad,
            format!(
                "Windows uses protected memory: {}",
                o.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(", ")
            ),
        ),
        (None, Ok(_)) => row("Windows memory map", Level::Info, "unknown"),
    });

    v.push(match &s.truncatememory {
        Ok(None) => row("Memory limit", Level::Good, "none (truncatememory not set)"),
        Ok(Some(x)) => row(
            "Memory limit",
            Level::Warn,
            format!("truncatememory {x}: memory above this address is unused"),
        ),
        Err(e) => row("Memory limit", Level::Info, e.clone()),
    });
    v
}
