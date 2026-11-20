//! egui front end. Long operations (status collection, install, ...) run on
//! a worker thread and stream log lines back; the UI only renders.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

use ramfence_core::config::parse_address;
use ramfence_core::{Config, MemoryType, Mode, OnError, PAGE_SIZE, Range, range};
use eframe::egui::{self, Color32, RichText};

use crate::ops::{self, InstallOptions, Log};
use crate::paths;
use crate::report::{self, Level};
use crate::sysstatus::{self, SystemStatus, Verdict};

const DEFAULT_CHAINLOAD: &str = r"\EFI\Microsoft\Boot\bootmgfw.efi";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Setup,
    Status,
    Config,
    MemTest,
    UsbTool,
    Install,
}

/// Default margin around configured ranges for a scheduled test.
const DEFAULT_MARGIN_MIB: u64 = 256;
const MIB: u64 = 1024 * 1024;

enum JobMsg {
    Line(String),
    Done(Result<(), String>),
}

struct Job {
    rx: Receiver<JobMsg>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Confirm {
    Install,
    Uninstall,
    ApplyConfig,
    RemoveLimit,
    Restart,
    RestartFirmware,
    ReplaceRanges,
    CreateUsb,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Header,
    Normal,
    Ok,
    Error,
}

#[derive(Clone, Default)]
struct RangeRow {
    start: String,
    end: String,
}

struct Editor {
    rows: Vec<RangeRow>,
    memory_type: MemoryType,
    on_error: OnError,
    timeout: u32,
    chainload: String,
    source: String,
}

impl Editor {
    fn new() -> Self {
        Editor {
            rows: vec![RangeRow::default()],
            memory_type: MemoryType::Unusable,
            on_error: OnError::Prompt,
            timeout: 3,
            chainload: String::new(),
            source: "new".into(),
        }
    }

    fn load(&mut self, c: &Config, source: String) {
        self.rows = c
            .ranges
            .iter()
            .map(|r| RangeRow {
                start: format!("{:#x}", r.start()),
                end: format!("{:#x}", r.end()),
            })
            .collect();
        if self.rows.is_empty() {
            self.rows.push(RangeRow::default());
        }
        self.memory_type = c.memory_type;
        self.on_error = c.on_error;
        self.timeout = c.timeout_secs;
        self.chainload = c.chainload.clone().unwrap_or_default();
        self.source = source;
    }

    fn row_range(row: &RangeRow) -> Result<Range, String> {
        let s = parse_address(row.start.trim()).map_err(|_| "start: expected hex like 0x1000".to_string())?;
        let e = parse_address(row.end.trim()).map_err(|_| "end: expected hex like 0x2000".to_string())?;
        Range::new(s, e).map_err(|_| "start must be below end".to_string())
    }

    fn build(&self) -> Result<Config, String> {
        let mut ranges = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            ranges.push(Self::row_range(row).map_err(|e| format!("range {}: {e}", i + 1))?);
        }
        let chainload = self.chainload.trim();
        let cfg = Config {
            mode: Mode::Protect,
            ranges,
            memory_type: self.memory_type,
            on_error: self.on_error,
            timeout_secs: self.timeout,
            chainload: (!chainload.is_empty()).then(|| chainload.to_string()),
        };
        // Round-trip through the same parser ramfence.efi uses.
        ops::validate_config(&cfg.to_text()).map(|(c, _)| c)
    }
}

pub struct ManagerApp {
    tab: Tab,
    status: Option<SystemStatus>,
    status_rx: Option<Receiver<SystemStatus>>,
    editor: Editor,
    /// ramfence.efi to install; None until the user picks a file (only when
    /// this build has no embedded copy and none was found).
    efi: Option<ops::EfiSource>,
    suspend_bitlocker: bool,
    allow_secure_boot: bool,
    job: Option<Job>,
    log: Vec<(LineKind, String)>,
    confirm: Option<Confirm>,
    mt: MemTestState,
    setup: SetupState,
    usb: UsbState,
    /// The first status picked the starting tab (Setup or Status).
    tab_chosen: bool,
}

struct SetupState {
    recovery_keys: Option<Result<Vec<String>, String>>,
    key_saved: bool,
    passes: u32,
    /// Ranges waiting for the "update ranges" confirmation.
    pending_ranges: Vec<Range>,
}

struct UsbState {
    disks: Option<Result<Vec<ops::UsbDisk>, String>>,
    rx: Option<Receiver<Result<Vec<ops::UsbDisk>, String>>>,
    /// Selected disk (number, size) — the size guards against renumbering.
    selected: Option<(u32, u64)>,
    erase_ok: bool,
}

struct MemTestState {
    rows: Vec<RangeRow>,
    passes: u32,
    /// Margin added around the error extent for the suggested range.
    suggest_margin_mib: u64,
    files: Option<Result<ops::TestFiles, String>>,
    rx: Option<Receiver<Result<ops::TestFiles, String>>>,
    loaded_once: bool,
}

impl MemTestState {
    /// Scan ranges: each configured bad range widened by the default margin.
    fn rows_around(cfg: Option<&Config>) -> Vec<RangeRow> {
        let ranges = cfg.and_then(|c| range::normalize(&c.ranges).ok()).unwrap_or_default();
        let mut rows: Vec<RangeRow> = ranges
            .iter()
            .map(|r| {
                let m = DEFAULT_MARGIN_MIB * MIB;
                RangeRow {
                    start: format!("{:#x}", r.start().saturating_sub(m) & !(MIB - 1)),
                    end: format!("{:#x}", r.end().saturating_add(m).div_ceil(MIB) * MIB),
                }
            })
            .collect();
        if rows.is_empty() {
            rows.push(RangeRow::default());
        }
        rows
    }

    fn request(&self) -> Result<String, String> {
        let mut text = String::new();
        for (i, row) in self.rows.iter().enumerate() {
            let r = Editor::row_range(row).map_err(|e| format!("range {}: {e}", i + 1))?;
            text.push_str(&format!("scan {:#x} {:#x}\n", r.start(), r.end()));
        }
        text.push_str(&format!("passes {}\n", self.passes));
        ramfence_core::memtest::parse_request(&text).map_err(|e| e.to_string())?;
        Ok(text)
    }
}

fn level_color(l: Level) -> Color32 {
    match l {
        Level::Good => Color32::from_rgb(0x2e, 0xa0, 0x4f),
        Level::Info => Color32::GRAY,
        Level::Warn => Color32::from_rgb(0xd0, 0x8a, 0x00),
        Level::Bad => Color32::from_rgb(0xd6, 0x3a, 0x3a),
    }
}

fn level_icon(l: Level) -> &'static str {
    match l {
        Level::Good => "✔",
        Level::Info => "•",
        Level::Warn => "⚠",
        Level::Bad => "✖",
    }
}

impl ManagerApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_fallback(&cc.egui_ctx);
        let mut editor = Editor::new();
        let installed = sysstatus::load_installed_config().and_then(Result::ok);
        if let Some(c) = &installed {
            editor.load(c, format!("installed ({})", paths::installed_cfg().display()));
        }
        let mt = MemTestState {
            rows: MemTestState::rows_around(installed.as_ref()),
            passes: 1,
            suggest_margin_mib: DEFAULT_MARGIN_MIB,
            files: None,
            rx: None,
            loaded_once: false,
        };
        let mut app = ManagerApp {
            tab: Tab::Status,
            status: None,
            status_rx: None,
            editor,
            efi: ops::EfiSource::default_source(),
            suspend_bitlocker: false,
            allow_secure_boot: false,
            job: None,
            log: Vec::new(),
            confirm: None,
            mt,
            setup: SetupState { recovery_keys: None, key_saved: false, passes: 1, pending_ranges: Vec::new() },
            usb: UsbState { disks: None, rx: None, selected: None, erase_ok: false },
            tab_chosen: false,
        };
        app.refresh(&cc.egui_ctx);
        app
    }

    fn busy(&self) -> bool {
        self.job.is_some()
    }

    fn refresh(&mut self, ctx: &egui::Context) {
        if self.status_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(sysstatus::collect());
            ctx.request_repaint();
        });
        self.status_rx = Some(rx);
    }

    fn start_job<F>(&mut self, ctx: &egui::Context, title: &str, f: F)
    where
        F: FnOnce(Log) -> Result<(), String> + Send + 'static,
    {
        if self.busy() {
            return;
        }
        self.log.push((LineKind::Header, format!("== {title} ==")));
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let line_tx = tx.clone();
            let line_ctx = ctx.clone();
            let mut log = move |s: String| {
                let _ = line_tx.send(JobMsg::Line(s));
                line_ctx.request_repaint();
            };
            let result = f(&mut log);
            let _ = tx.send(JobMsg::Done(result));
            ctx.request_repaint();
        });
        self.job = Some(Job { rx });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.status_rx
            && let Ok(s) = rx.try_recv()
        {
            // Start on the Setup tab until protection is configured.
            if !self.tab_chosen {
                self.tab_chosen = true;
                let configured = sysstatus::load_installed_config()
                    .and_then(Result::ok)
                    .is_some_and(|c| c.mode == Mode::Protect);
                if !configured {
                    self.tab = Tab::Setup;
                }
            }
            self.status = Some(s);
            self.status_rx = None;
        }
        let mut finished = false;
        if let Some(job) = &self.job {
            while let Ok(msg) = job.rx.try_recv() {
                match msg {
                    JobMsg::Line(l) => self.log.push((LineKind::Normal, l)),
                    JobMsg::Done(Ok(())) => {
                        self.log.push((LineKind::Ok, "Done.".into()));
                        finished = true;
                    }
                    JobMsg::Done(Err(e)) => {
                        self.log.push((LineKind::Error, format!("Failed: {e}")));
                        finished = true;
                    }
                }
            }
        }
        if let Some(rx) = &self.usb.rx
            && let Ok(d) = rx.try_recv()
        {
            self.usb.disks = Some(d);
            self.usb.rx = None;
        }
        if let Some(rx) = &self.mt.rx
            && let Ok(f) = rx.try_recv()
        {
            self.mt.files = Some(f);
            self.mt.rx = None;
        }
        if finished {
            self.job = None;
            self.refresh(ctx);
            if self.tab == Tab::MemTest {
                self.load_test_files(ctx);
            }
        }
    }

    fn load_test_files(&mut self, ctx: &egui::Context) {
        if self.mt.rx.is_some() {
            return;
        }
        self.mt.loaded_once = true;
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(ops::read_test_files());
            ctx.request_repaint();
        });
        self.mt.rx = Some(rx);
    }

    fn installed(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.installed())
    }

    fn elevated(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.elevated)
    }

    // ---------------------------------------------------------------- Status

    fn status_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let loading = self.status_rx.is_some();
            if ui.add_enabled(!loading, egui::Button::new("⟳ Refresh")).clicked() {
                self.refresh(ui.ctx());
            }
            if loading {
                ui.spinner();
                ui.label("Collecting status…");
            }
        });
        ui.add_space(6.0);

        let Some(s) = self.status.clone() else {
            return;
        };
        let (level, text) = report::headline(&s);
        egui::Frame::new()
            .fill(level_color(level).gamma_multiply(0.18))
            .stroke(egui::Stroke::new(1.0, level_color(level)))
            .inner_margin(egui::Margin::same(10))
            .corner_radius(4.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(format!("{}  {text}", level_icon(level)))
                        .size(16.0)
                        .color(level_color(level))
                        .strong(),
                );
            });
        ui.add_space(8.0);

        egui::Grid::new("status_grid")
            .num_columns(3)
            .striped(true)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                for r in report::rows(&s) {
                    ui.label(RichText::new(level_icon(r.level)).color(level_color(r.level)));
                    ui.strong(r.label);
                    ui.add(egui::Label::new(&r.text).wrap());
                    ui.end_row();
                }
            });
        ui.add_space(8.0);

        ui.horizontal_wrapped(|ui| {
            let enabled = !self.busy() && s.elevated;
            if s.ramfence_first() == Some(false)
                && ui.add_enabled(enabled, egui::Button::new("Put RAMFence first in boot order")).clicked()
            {
                self.start_job(ui.ctx(), "Fix boot order", ops::fix_boot_order);
            }
            if matches!(s.truncatememory, Ok(Some(_))) {
                let ok = s.verdict() == Verdict::Protected;
                let resp = ui.add_enabled(enabled && ok, egui::Button::new("Remove memory limit…"));
                let resp = if ok {
                    resp.on_hover_text("Remove truncatememory so Windows can use all good memory.")
                } else {
                    resp.on_disabled_hover_text("Only possible when this boot is verified as protected.")
                };
                if resp.clicked() {
                    self.confirm = Some(Confirm::RemoveLimit);
                }
            }
            if ui.button("Open install folder").clicked() {
                let _ = std::process::Command::new("explorer.exe").arg(paths::install_dir()).spawn();
            }
            if paths::log_file().is_file() && ui.button("View check.log").clicked() {
                let _ = std::process::Command::new("notepad.exe").arg(paths::log_file()).spawn();
            }
        });

        if let Ok(ranges) = &s.windows_ranges {
            ui.add_space(6.0);
            egui::CollapsingHeader::new(format!("Physical memory used by Windows ({} ranges)", ranges.len()))
                .show(ui, |ui| {
                    let protected = s.protected_ranges();
                    for r in ranges {
                        ui.monospace(format!("{:>14} - {:<14} {:>10}", format!("{:#x}", r.start()),
                            format!("{:#x}", r.end()), report::size(r.len())));
                    }
                    if !protected.is_empty() {
                        ui.add_space(4.0);
                        ui.label("Protected ranges:");
                        for r in protected {
                            ui.monospace(format!("{:>14} - {:<14} {:>10}", format!("{:#x}", r.start()),
                                format!("{:#x}", r.end()), report::size(r.len())));
                        }
                    }
                });
        }
    }

    // ---------------------------------------------------------- Configuration

    fn config_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(format!("Source: {}", self.editor.source));
        });
        ui.horizontal(|ui| {
            if ui.button("Load installed").clicked() {
                match sysstatus::load_installed_config() {
                    Some(Ok(c)) => self.editor.load(&c, format!("installed ({})", paths::installed_cfg().display())),
                    Some(Err(e)) => self.log.push((LineKind::Error, format!("Installed config is invalid: {e}"))),
                    None => self.log.push((LineKind::Error, "No installed config found.".into())),
                }
            }
            if ui.button("Open file…").clicked()
                && let Some(p) = rfd::FileDialog::new().add_filter("ramfence.cfg", &["cfg", "txt"]).pick_file()
            {
                match std::fs::read_to_string(&p).map_err(|e| e.to_string()).and_then(|t| {
                    ramfence_core::config::parse(&t).map_err(|e| e.to_string())
                }) {
                    Ok(c) => self.editor.load(&c, p.display().to_string()),
                    Err(e) => self.log.push((LineKind::Error, format!("{}: {e}", p.display()))),
                }
            }
            let built = self.editor.build();
            if ui.add_enabled(built.is_ok(), egui::Button::new("Save as…")).clicked()
                && let Ok(c) = &built
                && let Some(p) = rfd::FileDialog::new().set_file_name("ramfence.cfg").save_file()
            {
                match std::fs::write(&p, ops::config_file_text(c)) {
                    Ok(()) => self.log.push((LineKind::Ok, format!("Saved {}", p.display()))),
                    Err(e) => self.log.push((LineKind::Error, format!("Cannot save {}: {e}", p.display()))),
                }
            }
        });
        ui.separator();

        ui.strong("Bad memory ranges");
        ui.label(RichText::new("Physical addresses in hex; start inclusive, end exclusive. Rounded outward to 4 KiB pages.").weak());
        // Fixed-size cells: inside a Grid, egui sizes TextEdits to the
        // column, which collapses them to a few characters.
        const FIELD: [f32; 2] = [180.0, 22.0];
        let mut remove = None;
        ui.horizontal(|ui| {
            ui.add_sized(FIELD, egui::Label::new("Start"));
            ui.add_sized(FIELD, egui::Label::new("End"));
        });
        for (i, row) in self.editor.rows.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add_sized(FIELD, egui::TextEdit::singleline(&mut row.start)
                    .font(egui::TextStyle::Monospace).hint_text("0x100000000"));
                ui.add_sized(FIELD, egui::TextEdit::singleline(&mut row.end)
                    .font(egui::TextStyle::Monospace).hint_text("0x110000000"));
                match Editor::row_range(row) {
                    Ok(r) => ui.label(report::size(r.pages() * PAGE_SIZE)),
                    Err(e) => ui.colored_label(level_color(Level::Bad), e),
                };
                if ui.small_button("✖").on_hover_text("Remove this range").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.editor.rows.remove(i);
            if self.editor.rows.is_empty() {
                self.editor.rows.push(RangeRow::default());
            }
        }
        if ui.button("+ Add range").clicked() {
            self.editor.rows.push(RangeRow::default());
        }
        ui.add_space(8.0);

        egui::Grid::new("options").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.strong("Memory type");
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.editor.memory_type, MemoryType::Unusable, "unusable")
                    .on_hover_text("EfiUnusableMemory (8): memory with detected errors. Recommended.");
                ui.radio_value(&mut self.editor.memory_type, MemoryType::Reserved, "reserved")
                    .on_hover_text("EfiReservedMemoryType (0).");
            });
            ui.end_row();

            ui.strong("On error");
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.editor.on_error, OnError::Prompt, "prompt")
                    .on_hover_text("Show the error and wait for B (boot) / R (reboot) / S (shut down). Recommended.");
                ui.radio_value(&mut self.editor.on_error, OnError::Boot, "boot")
                    .on_hover_text("Warn, then boot Windows anyway (unattended machines).");
                ui.radio_value(&mut self.editor.on_error, OnError::Halt, "halt")
                    .on_hover_text("Never boot after an error.");
            });
            ui.end_row();

            ui.strong("Summary screen");
            ui.add(egui::DragValue::new(&mut self.editor.timeout).range(0..=600).suffix(" s"));
            ui.end_row();

            ui.strong("Chainload");
            ui.add(egui::TextEdit::singleline(&mut self.editor.chainload).desired_width(360.0).hint_text(DEFAULT_CHAINLOAD));
            ui.end_row();
        });
        ui.add_space(8.0);

        let built = self.editor.build();
        match &built {
            Ok(c) => {
                let ranges = range::normalize(&c.ranges).unwrap_or_default();
                let pages = range::total_pages(&ranges);
                ui.colored_label(
                    level_color(Level::Good),
                    format!("✔ Valid: {} range(s) after alignment/merging, {} pages ({})",
                        ranges.len(), pages, report::size(pages * PAGE_SIZE)),
                );
            }
            Err(e) => {
                ui.colored_label(level_color(Level::Bad), format!("✖ {e}"));
            }
        }
        if let Ok(c) = &built {
            egui::CollapsingHeader::new("Preview ramfence.cfg").show(ui, |ui| {
                ui.monospace(c.to_text());
            });
        }
        ui.add_space(6.0);
        let can_apply = built.is_ok() && self.installed() && self.elevated() && !self.busy();
        let resp = ui.add_enabled(can_apply, egui::Button::new("Apply to installed RAMFence…"));
        let resp = if self.installed() {
            resp.on_hover_text("Writes ramfence.cfg to the ESP. Takes effect on the next boot.")
        } else {
            resp.on_disabled_hover_text("RAMFence is not installed. Use the Install tab.")
        };
        if resp.clicked() {
            self.confirm = Some(Confirm::ApplyConfig);
        }
    }

    // --------------------------------------------------------------- Install

    fn install_tab(&mut self, ui: &mut egui::Ui) {
        let Some(s) = self.status.clone() else {
            ui.spinner();
            return;
        };
        ui.strong("Pre-flight checks");
        let built = self.editor.build();
        let efi_ok = self.efi.as_ref().is_some_and(|e| e.load().is_ok());
        let sb_on = s.secure_boot == Some(true);
        let bl_on = s.bitlocker.as_deref() == Some("On");

        let mut checks: Vec<(Level, &str, String)> = vec![
            if s.elevated { (Level::Good, "Administrator", "yes".into()) } else { (Level::Bad, "Administrator", "no: restart RAMFence Manager as administrator".into()) },
            match s.uefi { Some(true) => (Level::Good, "Boot mode", "UEFI".into()), Some(false) => (Level::Bad, "Boot mode", "Legacy BIOS (not supported)".into()), None => (Level::Info, "Boot mode", "unknown".into()) },
            match &built { Ok(_) => (Level::Good, "Configuration", "valid (see Configuration tab)".into()), Err(e) => (Level::Bad, "Configuration", e.clone()) },
            if efi_ok { (Level::Good, "ramfence.efi", "found".into()) } else { (Level::Bad, "ramfence.efi", "not found; choose the file below".into()) },
        ];
        checks.push(if sb_on {
            (if self.allow_secure_boot { Level::Warn } else { Level::Bad }, "Secure Boot", "On: turn it off in the BIOS, or enroll ramfence.efi's hash in db".into())
        } else {
            (Level::Good, "Secure Boot", "Off".into())
        });
        checks.push(if bl_on {
            (if self.suspend_bitlocker { Level::Warn } else { Level::Bad }, "BitLocker", "On: back up the recovery key, then allow suspending it".into())
        } else {
            (Level::Good, "BitLocker", s.bitlocker.clone().unwrap_or_else(|| "unknown".into()))
        });
        if s.fast_startup == Some(true) {
            checks.push((Level::Warn, "Fast Startup", "On: consider \"powercfg /h off\"".into()));
        }
        egui::Grid::new("preflight").num_columns(3).spacing([12.0, 6.0]).show(ui, |ui| {
            for (l, label, text) in &checks {
                ui.label(RichText::new(level_icon(*l)).color(level_color(*l)));
                ui.strong(*label);
                ui.label(text);
                ui.end_row();
            }
        });
        ui.add_space(8.0);

        self.efi_picker(ui);
        if bl_on {
            ui.checkbox(&mut self.suspend_bitlocker, "Suspend BitLocker for 2 reboots (I have backed up the recovery key)");
        }
        if sb_on {
            ui.checkbox(&mut self.allow_secure_boot, "Allow Secure Boot (I enrolled this ramfence.efi's hash in db)");
        }
        ui.add_space(8.0);

        let blocked = !s.elevated || s.uefi == Some(false) || built.is_err() || !efi_ok
            || (sb_on && !self.allow_secure_boot) || (bl_on && !self.suspend_bitlocker);
        let label = if s.installed() { "Update installation…" } else { "Install…" };
        if ui.add_enabled(!blocked && !self.busy(), egui::Button::new(RichText::new(label).strong())).clicked() {
            self.confirm = Some(Confirm::Install);
        }

        if s.installed() {
            ui.add_space(16.0);
            ui.separator();
            ui.strong("Uninstall");
            ui.label("Removes the boot entry, the files on the ESP and the logon task. Windows can use the bad memory again after the next boot.");
            if ui.add_enabled(s.elevated && !self.busy(), egui::Button::new("Uninstall…")).clicked() {
                self.confirm = Some(Confirm::Uninstall);
            }
        }
    }

    // --------------------------------------------------------------- Dialogs

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(which) = self.confirm else { return };
        let (title, body): (&str, String) = match which {
            Confirm::Install => {
                let summary = self.editor.build().map(|c| {
                    let r = range::normalize(&c.ranges).unwrap_or_default();
                    format!("{} range(s), {} pages", r.len(), range::total_pages(&r))
                }).unwrap_or_default();
                ("Install RAMFence?", format!(
                    "This changes the firmware boot order so ramfence.efi starts before Windows.\n\n\
                     Configuration: {summary}\nramfence.efi: {}\n\nA BCD backup is saved first.", self.efi_label()))
            }
            Confirm::Uninstall => ("Uninstall RAMFence?", "Windows Boot Manager is put first again and RAMFence is removed.\n\n\
                 After the next boot Windows CAN use the bad memory again. Restore a memory limit first if needed.".into()),
            Confirm::ApplyConfig => ("Apply configuration?", "ramfence.cfg on the ESP is replaced. The new ranges take effect on the next boot.".into()),
            Confirm::RemoveLimit => ("Remove memory limit?", "truncatememory is removed from the running Windows entry, so Windows uses all memory \
                 ramfence.efi does not reserve.\n\nOnly do this while RAMFence protection is verified.".into()),
            Confirm::Restart => ("Restart now?", "Windows restarts immediately. Save your work first.\n\n\
                 If a memory scan is scheduled, it runs before Windows starts; press Esc on that screen to stop it.".into()),
            Confirm::RestartFirmware => ("Restart into firmware settings?", "Windows restarts immediately into the PC's \
                 firmware settings (BIOS). Save your work first.\n\nTurn Secure Boot off there, then save and exit \
                 (usually F10). Windows starts again afterwards.".into()),
            Confirm::ReplaceRanges => {
                let fmt = |v: &[Range]| v.iter().map(|r| format!("  {r}  ({})", report::size(r.len()))).collect::<Vec<_>>().join("\n");
                let current = sysstatus::load_installed_config().and_then(Result::ok)
                    .map(|c| range::normalize(&c.ranges).unwrap_or_default()).unwrap_or_default();
                ("Update the protected ranges?", format!(
                    "Currently protected:\n{}\n\nNew ranges from the scan:\n{}\n\n\
                     Shrinking the protected area is only safe if the scan is reliable: weak memory can fail \
                     only sometimes (for example when the PC is warm). Best scan with 3 or more passes first. \
                     Errors outside the new ranges would reach Windows.\n\nTakes effect at the next restart.",
                    fmt(&current), fmt(&self.setup.pending_ranges)))
            }
            Confirm::CreateUsb => {
                let disk = self.selected_usb_disk();
                ("Erase this USB drive?", match disk {
                    Some(d) => format!(
                        "EVERYTHING on this drive will be erased:\n\n  Disk {}: {}  ({})\n\n\
                         It becomes a RAMFence rescue USB stick.", d.number, d.friendly_name, report::size(d.size)),
                    None => "The selected drive is no longer available. Refresh the list.".into(),
                })
            }
        };
        let mut decision = None;
        egui::Modal::new(egui::Id::unique("ramfence-confirm")).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.heading(title);
            ui.add_space(6.0);
            ui.label(body);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Confirm").strong()).clicked() {
                    decision = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    decision = Some(false);
                }
            });
        });
        match decision {
            Some(true) => {
                self.confirm = None;
                self.run_confirmed(ctx, which);
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }

    fn run_confirmed(&mut self, ctx: &egui::Context, which: Confirm) {
        match which {
            Confirm::Install => {
                let Ok(c) = self.editor.build() else { return };
                let Some(efi) = self.efi.clone() else { return };
                let opts = InstallOptions {
                    config_text: c.to_text(),
                    efi,
                    suspend_bitlocker: self.suspend_bitlocker,
                    allow_secure_boot: self.allow_secure_boot,
                };
                self.start_job(ctx, "Install", move |log| ops::install(&opts, log));
            }
            Confirm::Uninstall => self.start_job(ctx, "Uninstall", ops::uninstall),
            Confirm::ApplyConfig => {
                let Ok(c) = self.editor.build() else { return };
                let text = c.to_text();
                self.start_job(ctx, "Apply configuration", move |log| ops::apply_config(&text, log));
            }
            Confirm::RemoveLimit => self.start_job(ctx, "Remove memory limit", ops::remove_memory_limit),
            Confirm::Restart => self.start_job(ctx, "Restart", ops::restart_now),
            Confirm::RestartFirmware => self.start_job(ctx, "Restart into firmware settings", ops::restart_to_firmware),
            Confirm::ReplaceRanges => {
                let ranges = std::mem::take(&mut self.setup.pending_ranges);
                if !ranges.is_empty() {
                    self.start_job(ctx, "Update protected ranges", move |log| ops::protect_ranges(&ranges, log));
                }
            }
            Confirm::CreateUsb => {
                let (Some(disk), Some(efi)) = (self.selected_usb_disk(), self.efi.clone()) else { return };
                self.usb.erase_ok = false;
                self.usb.disks = None; // drive letters/partitions change
                self.start_job(ctx, "Create rescue USB", move |log| ops::create_usb(&disk, &efi, log));
            }
        }
    }

    // ----------------------------------------------------------- Memory test

    fn memtest_tab(&mut self, ui: &mut egui::Ui) {
        if !self.mt.loaded_once && self.elevated() {
            self.load_test_files(ui.ctx());
        }
        ui.label(
            "ramfence.efi can test memory once, before Windows starts, including the reserved bad \
             ranges (Windows itself can never reach them). Schedule a test, restart, and the \
             results appear below. It is quick and covers a few passes; MemTest86 remains the \
             thorough reference.",
        );
        ui.add_space(8.0);

        ui.strong("Scan ranges");
        const FIELD: [f32; 2] = [180.0, 22.0];
        let mut remove = None;
        for (i, row) in self.mt.rows.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add_sized(FIELD, egui::TextEdit::singleline(&mut row.start).font(egui::TextStyle::Monospace).hint_text("start"));
                ui.add_sized(FIELD, egui::TextEdit::singleline(&mut row.end).font(egui::TextStyle::Monospace).hint_text("end"));
                match Editor::row_range(row) {
                    Ok(r) => ui.label(report::size(r.len())),
                    Err(e) => ui.colored_label(level_color(Level::Bad), e),
                };
                if ui.small_button("✖").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.mt.rows.remove(i);
            if self.mt.rows.is_empty() {
                self.mt.rows.push(RangeRow::default());
            }
        }
        ui.horizontal(|ui| {
            if ui.button("+ Add range").clicked() {
                self.mt.rows.push(RangeRow::default());
            }
            if ui.button(format!("Reset to configured ranges ± {DEFAULT_MARGIN_MIB} MiB")).clicked() {
                let cfg = self.editor.build().ok();
                self.mt.rows = MemTestState::rows_around(cfg.as_ref());
            }
            if ui.button("All memory").on_hover_text("One range covering all RAM; parts that are not RAM are skipped.").clicked()
                && let Some(s) = &self.status
            {
                let r = ops::full_scan_range(s.windows_ranges.as_deref().unwrap_or(&[]), &s.protected_ranges(), s.installed_mem);
                self.mt.rows = vec![RangeRow { start: format!("{:#x}", r.start()), end: format!("{:#x}", r.end()) }];
            }
        });
        ui.horizontal(|ui| {
            ui.label("Passes:");
            ui.add(egui::DragValue::new(&mut self.mt.passes).range(1..=ramfence_core::memtest::MAX_PASSES));
            ui.label(RichText::new("each pass writes and verifies every word 12 times").weak());
        });
        ui.add_space(6.0);

        let request = self.mt.request();
        if let Err(e) = &request {
            ui.colored_label(level_color(Level::Bad), format!("✖ {e}"));
        }
        let pending = matches!(&self.mt.files, Some(Ok(f)) if f.pending);
        let unsupported = matches!(&self.mt.files, Some(Ok(f)) if !f.efi_supports_test);
        if unsupported {
            ui.colored_label(
                level_color(Level::Warn),
                "⚠ The installed ramfence.efi is too old to run memory tests. Update the installation \
                 (Install tab → Update installation) first.",
            );
        }
        let can_act = self.installed() && self.elevated() && !self.busy() && !unsupported;
        ui.horizontal(|ui| {
            let resp = ui.add_enabled(can_act && request.is_ok(), egui::Button::new(RichText::new("Schedule test for next boot").strong()));
            let resp = if self.installed() { resp } else { resp.on_disabled_hover_text("RAMFence is not installed.") };
            if resp.clicked()
                && let Ok(text) = request.clone()
            {
                self.start_job(ui.ctx(), "Schedule memory test", move |log| ops::schedule_test(&text, log));
            }
            if pending && ui.add_enabled(can_act, egui::Button::new("Cancel scheduled test")).clicked() {
                self.start_job(ui.ctx(), "Cancel memory test", ops::cancel_test);
            }
            if ui.add_enabled(!self.busy(), egui::Button::new("Restart now…")).clicked() {
                self.confirm = Some(Confirm::Restart);
            }
        });
        if pending {
            ui.colored_label(level_color(Level::Warn), "⚠ A memory test is scheduled for the next boot.");
        }
        ui.add_space(10.0);
        ui.separator();

        ui.horizontal(|ui| {
            ui.strong("Last result");
            if ui.add_enabled(self.mt.rx.is_none() && self.elevated(), egui::Button::new("⟳ Reload")).clicked() {
                self.load_test_files(ui.ctx());
            }
            if self.mt.rx.is_some() {
                ui.spinner();
            }
        });
        let files = self.mt.files.clone();
        match files {
            None if !self.elevated() => {
                ui.label("Run as administrator to read results from the EFI System Partition.");
            }
            None => {}
            Some(Err(e)) => {
                ui.colored_label(level_color(Level::Bad), e);
            }
            Some(Ok(f)) => match f.result {
                None => {
                    ui.label("No result yet.");
                }
                Some(Err(e)) => {
                    ui.colored_label(level_color(Level::Bad), e);
                }
                Some(Ok(r)) => self.show_result(ui, &r),
            },
        }
    }

    fn show_result(&mut self, ui: &mut egui::Ui, r: &ramfence_core::memtest::TestResult) {
        let e = &r.errors;
        // "not in memory map" = not RAM at all (MMIO holes etc.): not a gap
        // in the test.
        let untested: u64 = r.untested.iter().filter(|u| !is_not_ram(u)).map(|u| u.range.len()).sum();
        let (level, headline) = match (r.completed, e.count) {
            (false, _) => (Level::Warn, format!("Aborted after finding {} error(s).", e.count)),
            (true, 0) => (Level::Good, "No errors found.".to_string()),
            (true, n) => (Level::Bad, format!("{n} error(s) found.")),
        };
        ui.label(RichText::new(format!("{}  {headline}", level_icon(level))).size(15.0).strong().color(level_color(level)));
        egui::Grid::new("mt_result").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            ui.strong("Tested");
            ui.label(format!("{} in {} region(s), {} pass(es)", report::size(r.tested_bytes()), r.tested.len(), r.passes));
            ui.end_row();
            ui.strong("Untested");
            ui.label(if untested == 0 { "none".to_string() } else { format!("{} (in use by the firmware)", report::size(untested)) });
            ui.end_row();
            if let (Some(lo), Some(hi)) = (e.lowest, e.highest) {
                ui.strong("Error extent");
                ui.monospace(format!("{lo:#x} – {hi:#x}"));
                ui.end_row();
                ui.strong("Error mask");
                ui.monospace(format!("{:#018x}", e.mask));
                ui.end_row();
            }
        });
        ui.add_space(6.0);
        let protected = self.editor.build().ok()
            .and_then(|c| range::normalize(&c.ranges).ok())
            .unwrap_or_default();
        histogram(ui, r, &protected);
        ui.horizontal(|ui| {
            for (color, text) in [
                (level_color(Level::Bad), "errors (per 64 MiB)"),
                (Color32::from_rgb(0x3a, 0x7b, 0xd5), "configured bad range"),
                (level_color(Level::Good), "tested"),
                (Color32::GRAY, "untested"),
            ] {
                ui.colored_label(color, "■");
                ui.label(RichText::new(text).small());
            }
        });

        if e.count > 0 {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("Suggested bad range(s), margin");
                ui.add(egui::DragValue::new(&mut self.mt.suggest_margin_mib).range(0..=4096).suffix(" MiB"));
            });
            let suggested = self.suggested(e);
            for s in &suggested {
                ui.monospace(format!("range {:#x} {:#x}   ({})", s.start(), s.end(), report::size(s.len())));
            }
            if !suggested.is_empty() {
                if ui.button("Use in Configuration").on_hover_text("Replaces the ranges in the Configuration tab; review and Apply there.").clicked() {
                    self.editor.rows = suggested.iter().map(|s| RangeRow { start: format!("{:#x}", s.start()), end: format!("{:#x}", s.end()) }).collect();
                    self.editor.source = "suggested by memory test (not applied yet)".into();
                    self.tab = Tab::Config;
                }
                let covered = suggested.iter().all(|s| protected.iter().any(|p| p.start() <= s.start() && s.end() <= p.end()));
                if !covered && !protected.is_empty() {
                    ui.colored_label(level_color(Level::Warn), "⚠ The configured bad ranges do not cover all errors plus margin.");
                }
            }
            egui::CollapsingHeader::new(format!("First {} error(s)", e.samples.len().min(200))).show(ui, |ui| {
                ui.monospace(format!("{:<16} {:<20} {:<20}", "address", "expected", "actual"));
                for x in e.samples.iter().take(200) {
                    ui.monospace(format!("{:<16} {:#018x}   {:#018x}", format!("{:#x}", x.addr), x.expected, x.actual));
                }
            });
        }
        let skipped: Vec<_> = r.untested.iter().filter(|u| !is_not_ram(u)).collect();
        if !skipped.is_empty() {
            egui::CollapsingHeader::new(format!("Untested regions ({})", skipped.len())).show(ui, |ui| {
                for u in skipped {
                    ui.monospace(format!("{}  {}", u.range, u.reason));
                }
            });
        }
    }

    fn suggested(&self, e: &ramfence_core::memtest::ErrorLog) -> Vec<Range> {
        e.suggested_ranges(self.mt.suggest_margin_mib * MIB, MIB, ramfence_core::memtest::SUGGEST_GAP)
    }

    fn efi_label(&self) -> String {
        match &self.efi {
            Some(ops::EfiSource::Embedded) => format!("built-in (bundled with RAMFence Manager {})", env!("CARGO_PKG_VERSION")),
            Some(ops::EfiSource::File(p)) => p.display().to_string(),
            None => "not selected".into(),
        }
    }

    fn efi_picker(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("ramfence.efi:");
            if paths::EMBEDDED_EFI.is_some() {
                ui.radio_value(&mut self.efi, Some(ops::EfiSource::Embedded), "built-in");
            }
            if let Some(ops::EfiSource::File(p)) = &self.efi {
                ui.label(p.display().to_string());
            }
            if ui.button("Choose file…").clicked()
                && let Some(p) = rfd::FileDialog::new().add_filter("EFI application", &["efi"]).pick_file()
            {
                self.efi = Some(ops::EfiSource::File(p));
            }
        });
    }

    // -------------------------------------------------------------- USB tool

    fn selected_usb_disk(&self) -> Option<ops::UsbDisk> {
        let (n, size) = self.usb.selected?;
        let disks = self.usb.disks.as_ref()?.as_ref().ok()?;
        disks.iter().find(|d| d.number == n && d.size == size && d.erasable()).cloned()
    }

    fn refresh_usb(&mut self, ctx: &egui::Context) {
        if self.usb.rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(ops::list_usb_disks());
            ctx.request_repaint();
        });
        self.usb.rx = Some(rx);
    }

    fn usb_tab(&mut self, ui: &mut egui::Ui) {
        if self.usb.disks.is_none() && self.usb.rx.is_none() && !self.busy() {
            self.refresh_usb(ui.ctx());
        }
        ui.heading("Rescue USB stick");
        ui.label(
            "For a PC that crashes before or while Windows starts because of bad memory. The stick \
             starts RAMFence first: on its first start it tests all memory and saves the result on the \
             stick, then it keeps the damaged areas away from Windows and starts Windows. In Windows, \
             run RAMFence-Manager.exe from the stick to install RAMFence on that PC.",
        );
        ui.add_space(6.0);
        ui.label(RichText::new(
            "The PC that uses the stick needs Secure Boot turned off, and you open its boot menu \
             (often F12, F11, F8, Esc or F10 at the maker's logo) to start from the stick.").weak());
        ui.add_space(10.0);

        ui.horizontal(|ui| {
            ui.strong("USB drives");
            if ui.add_enabled(self.usb.rx.is_none() && !self.busy(), egui::Button::new("⟳ Refresh")).clicked() {
                self.refresh_usb(ui.ctx());
            }
            if self.usb.rx.is_some() {
                ui.spinner();
            }
        });
        match &self.usb.disks {
            None => {}
            Some(Err(e)) => {
                ui.colored_label(level_color(Level::Bad), e);
            }
            Some(Ok(disks)) if disks.is_empty() => {
                ui.label("No USB drive found. Plug one in and press Refresh.");
            }
            Some(Ok(disks)) => {
                for d in disks.clone() {
                    let text = format!("Disk {}: {}  ({})", d.number, d.friendly_name, report::size(d.size));
                    if d.erasable() {
                        ui.radio_value(&mut self.usb.selected, Some((d.number, d.size)), text);
                    } else {
                        ui.add_enabled(false, egui::RadioButton::new(false, format!("{text}  (Windows starts from this disk)")));
                    }
                }
            }
        }
        ui.add_space(8.0);

        let disk = self.selected_usb_disk();
        if disk.is_some() {
            ui.checkbox(&mut self.usb.erase_ok, "I understand that everything on this USB drive will be erased");
        }
        if self.efi.is_none() {
            self.efi_picker(ui);
        }
        let can = disk.is_some() && self.usb.erase_ok && self.elevated() && self.efi.is_some() && !self.busy();
        if ui.add_enabled(can, egui::Button::new(RichText::new("Create rescue USB…").strong())).clicked() {
            self.confirm = Some(Confirm::CreateUsb);
        }
        ui.add_space(10.0);
        ui.separator();
        ui.label(RichText::new(
            "The stick gets: EFI\\BOOT\\BOOTX64.EFI (ramfence.efi in auto mode), EFI\\BOOT\\ramfence.cfg, \
             RAMFence-Manager.exe and README.txt. Only a 2 GiB FAT32 partition is created; delete \
             EFI\\BOOT\\test-result.txt on the stick to test again.").weak());
    }

    // ----------------------------------------------------------------- Setup

    fn setup_tab(&mut self, ui: &mut egui::Ui) {
        let Some(s) = self.status.clone() else {
            ui.spinner();
            return;
        };
        if !self.mt.loaded_once && s.elevated && s.installed() {
            self.load_test_files(ui.ctx());
        }
        let busy = self.busy();
        let verdict = s.verdict();
        let installed_cfg = sysstatus::load_installed_config().and_then(Result::ok);
        let protect_configured = installed_cfg.as_ref().is_some_and(|c| c.mode == Mode::Protect);
        let files = self.mt.files.as_ref().and_then(|f| f.as_ref().ok()).cloned();
        let pending = files.as_ref().is_some_and(|f| f.pending);
        let result = files.as_ref().and_then(|f| f.result.clone()).and_then(Result::ok);

        ui.heading("Set up RAMFence");
        ui.label(
            "RAMFence keeps Windows away from damaged memory. Follow the steps in order; each \
             step checks itself and turns green when it is done.",
        );
        ui.add_space(8.0);

        // ---- Step 1: prepare
        let bl_on = s.bitlocker.as_deref() == Some("On");
        let sb_on = s.secure_boot == Some(true);
        let prepared = s.elevated && s.uefi == Some(true) && !sb_on && !bl_on && s.fast_startup != Some(true);
        step_header(ui, 1, "Prepare this PC", prepared);
        ui.indent("step1", |ui| {
            if !s.elevated {
                ui.colored_label(level_color(Level::Bad), "RAMFence Manager needs administrator rights.");
                if ui.button("Restart as administrator").clicked() && crate::win::relaunch_elevated("") {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                return;
            }
            if s.uefi == Some(false) {
                ui.colored_label(level_color(Level::Bad),
                    "This PC starts Windows in the old \"legacy BIOS\" mode. RAMFence only works with UEFI, \
                     so it cannot be used on this PC.");
                return;
            }
            check_line(ui, Level::Good, "Windows starts in UEFI mode.");

            if bl_on {
                check_line(ui, Level::Warn, "BitLocker drive encryption is on.");
                ui.label(
                    "Changing how the PC starts (the next items) makes BitLocker ask for its 48-digit \
                     recovery key at startup. Save the key first, then pause BitLocker for two restarts.",
                );
                if ui.button("Show my recovery key").clicked() {
                    self.setup.recovery_keys = Some(ops::bitlocker_recovery_keys());
                }
                match &self.setup.recovery_keys {
                    Some(Ok(keys)) => {
                        for k in keys.clone() {
                            ui.horizontal(|ui| {
                                ui.monospace(RichText::new(&k).size(15.0).strong());
                                if ui.small_button("Copy").clicked() {
                                    ui.ctx().copy_text(k.clone());
                                }
                            });
                        }
                        ui.label(RichText::new("Write it on paper or save it on another device (not only on this PC).").weak());
                    }
                    Some(Err(e)) => {
                        ui.colored_label(level_color(Level::Bad), e);
                    }
                    None => {}
                }
                ui.checkbox(&mut self.setup.key_saved, "I have saved my recovery key somewhere else");
                if ui.add_enabled(self.setup.key_saved && !busy, egui::Button::new("Pause BitLocker for the next 2 restarts")).clicked() {
                    self.start_job(ui.ctx(), "Suspend BitLocker", ops::suspend_bitlocker);
                }
            } else {
                check_line(ui, Level::Good, match s.bitlocker.as_deref() {
                    Some(_) => "BitLocker is off or paused.",
                    None => "BitLocker is not in use (or its state is unknown).",
                });
            }

            if sb_on {
                check_line(ui, Level::Bad, "Secure Boot is on and must be turned off.");
                ui.label(
                    "ramfence.efi is not signed by Microsoft, so the PC refuses to start it while Secure Boot \
                     is on. Turning it off is done in the PC's firmware settings (often called BIOS):",
                );
                ui.label("  1. Click the button below. The PC restarts into its firmware settings.");
                ui.label("  2. Find \"Secure Boot\" (usually under Security, Boot or Authentication).");
                ui.label("  3. Set it to Disabled (some PCs: set \"OS Type\" to \"Other OS\").");
                ui.label("  4. Save and exit (usually F10). Windows starts again; open RAMFence Manager.");
                let resp = ui.add_enabled(!bl_on && !busy, egui::Button::new("Restart into firmware settings…"));
                if bl_on {
                    resp.on_disabled_hover_text("Save the BitLocker recovery key and pause BitLocker first.");
                } else if resp.clicked() {
                    self.confirm = Some(Confirm::RestartFirmware);
                }
            } else if s.secure_boot == Some(false) {
                check_line(ui, Level::Good, "Secure Boot is off.");
            }

            if s.fast_startup == Some(true) {
                check_line(ui, Level::Warn, "Fast Startup is on (Windows can then skip RAMFence when starting).");
                if ui.add_enabled(!busy, egui::Button::new("Turn off Fast Startup")).clicked() {
                    self.start_job(ui.ctx(), "Turn off Fast Startup", ops::disable_hibernation);
                }
            } else {
                check_line(ui, Level::Good, "Fast Startup is off.");
            }
        });
        ui.add_space(10.0);

        // ---- Step 2: find the bad memory
        // Started from a rescue USB stick (RAMFence not installed on this PC):
        // the stick already found and reserved the bad memory.
        let usb_ranges: Option<Vec<Range>> = match &s.ramfence {
            Some(i) if !s.installed() && i.ok && !i.scan_only => Some(i.ranges.clone()),
            _ => None,
        };
        // The stick's scan was stopped: its ranges are incomplete.
        let usb_stopped = usb_ranges.is_some() && s.ramfence.as_ref().is_some_and(|i| i.scan_stopped);
        let found = result.as_ref().is_some_and(|r| r.completed)
            || protect_configured
            || (usb_ranges.is_some() && !usb_stopped);
        step_header(ui, 2, "Find the bad memory", found);
        ui.indent("step2", |ui| {
            if usb_stopped {
                check_line(ui, Level::Warn,
                    "This start used a RAMFence rescue USB stick, but its memory scan was stopped before it \
                     finished. Only the errors found so far are reserved. Start from the stick again and let \
                     the scan finish.");
                return;
            }
            if let Some(r) = &usb_ranges {
                let total: u64 = r.iter().map(Range::len).sum();
                check_line(ui, Level::Good, &if r.is_empty() {
                    "This start used a RAMFence rescue USB stick, which found no bad memory.".to_string()
                } else {
                    format!("This start used a RAMFence rescue USB stick, which found and reserved {} in {} range(s).",
                        report::size(total), r.len())
                });
                return;
            }
            if !prepared && !found {
                ui.label(RichText::new("Finish step 1 first.").weak());
                return;
            }
            if pending {
                check_line(ui, Level::Warn, "A memory scan is scheduled for the next restart.");
                ui.label(
                    "Restart now. Before Windows starts, RAMFence tests the memory (this takes a few minutes; \
                     press Esc to stop it), then Windows starts by itself. Open RAMFence Manager again afterwards.",
                );
                if ui.add_enabled(!busy, egui::Button::new(RichText::new("Restart now…").strong())).clicked() {
                    self.confirm = Some(Confirm::Restart);
                }
                return;
            }
            if let Some(r) = &result {
                let e = &r.errors;
                if e.count == 0 {
                    check_line(ui, Level::Good, &format!(
                        "The scan found no errors in {}.", report::size(r.tested_bytes())));
                    ui.label(RichText::new(
                        "If you still suspect bad memory, scan again with more passes, or use MemTest86.").weak());
                } else {
                    check_line(ui, Level::Bad, &format!(
                        "The scan found {} error(s) between {:#x} and {:#x}.",
                        e.count, e.lowest.unwrap_or(0), e.highest.unwrap_or(0)));
                }
                if !r.completed {
                    check_line(ui, Level::Warn, "The scan was stopped before it finished.");
                }
            } else if protect_configured {
                check_line(ui, Level::Good, "Bad ranges are already configured.");
            } else {
                ui.label(
                    "RAMFence can test all memory before Windows starts. This does not change anything \
                     yet: it only finds out where the damaged memory is.",
                );
            }
            ui.horizontal(|ui| {
                ui.label("Passes:");
                ui.add(egui::DragValue::new(&mut self.setup.passes).range(1..=ramfence_core::memtest::MAX_PASSES));
                ui.label(RichText::new("1 is quick; 3+ finds weak memory more reliably").weak());
            });
            let label = if result.is_some() { "Scan again at next restart" } else { "Scan all memory at next restart" };
            let can = prepared && self.efi.is_some() && !busy;
            if ui.add_enabled(can, egui::Button::new(RichText::new(label).strong())).clicked() {
                let scan = ops::full_scan_range(
                    s.windows_ranges.as_deref().unwrap_or(&[]),
                    &s.protected_ranges(),
                    s.installed_mem,
                );
                let efi = self.efi.clone().unwrap_or(ops::EfiSource::Embedded);
                let passes = self.setup.passes;
                self.start_job(ui.ctx(), "Schedule memory scan", move |log| {
                    ops::start_first_scan(efi, scan, passes, false, log)
                });
            }
            if self.efi.is_none() {
                self.efi_picker(ui);
            }
            ui.add_space(4.0);
            ui.label(RichText::new(
                "Already know the bad addresses (for example from MemTest86)? Enter them in the \
                 Configuration tab and install from the Install tab instead.").weak());
        });
        ui.add_space(10.0);

        // ---- Step 3: protect
        let protected_now = verdict == Verdict::Protected;
        step_header(ui, 3, "Protect the bad memory", protect_configured);
        ui.indent("step3", |ui| {
            if usb_stopped {
                ui.label(RichText::new("Finish step 2 first: the scan must complete before RAMFence is installed with its result.").weak());
                return;
            }
            if let Some(r) = usb_ranges.as_ref().filter(|r| !r.is_empty()) {
                ui.label("Install RAMFence on this PC with the ranges the USB stick found, so the stick is no longer needed:");
                for x in r {
                    ui.monospace(format!("  {x}  ({})", report::size(x.len())));
                }
                let can = prepared && self.efi.is_some() && !busy;
                let resp = ui.add_enabled(can, egui::Button::new(RichText::new("Install RAMFence with these ranges").strong()));
                if !prepared {
                    resp.on_disabled_hover_text("Finish step 1 first.");
                } else if resp.clicked() {
                    let ranges = r.clone();
                    let efi = self.efi.clone().unwrap_or(ops::EfiSource::Embedded);
                    self.start_job(ui.ctx(), "Install RAMFence", move |log| ops::install_ranges(&ranges, efi, log));
                }
                return;
            }
            if let Some(cfg) = installed_cfg.as_ref().filter(|c| c.mode == Mode::Protect) {
                let ranges = range::normalize(&cfg.ranges).unwrap_or_default();
                check_line(ui, Level::Good, &format!("Protecting {} range(s):", ranges.len()));
                for r in &ranges {
                    ui.monospace(format!("  {r}  ({})", report::size(r.len())));
                }
            }
            let Some(r) = result.as_ref().filter(|r| r.errors.count > 0) else {
                if !protect_configured {
                    ui.label(RichText::new("Finish step 2 first. Nothing needs protecting if no errors are found.").weak());
                }
                return;
            };
            ui.horizontal(|ui| {
                ui.label("Safety margin around the errors:");
                ui.add(egui::DragValue::new(&mut self.mt.suggest_margin_mib).range(0..=4096).suffix(" MiB"));
            });
            let suggested = self.suggested(&r.errors);
            let total: u64 = suggested.iter().map(Range::len).sum();
            let current = if protect_configured {
                installed_cfg.as_ref().map(|c| range::normalize(&c.ranges).unwrap_or_default()).unwrap_or_default()
            } else {
                Vec::new()
            };
            let covered = !current.is_empty()
                && suggested.iter().all(|x| current.iter().any(|p| p.start() <= x.start() && x.end() <= p.end()));
            ui.label(if covered {
                format!("The errors found, plus the margin, span {} in {} range(s):", report::size(total), suggested.len())
            } else {
                format!("RAMFence will reserve {} in {} range(s):", report::size(total), suggested.len())
            });
            for x in &suggested {
                ui.monospace(format!("  {x}  ({})", report::size(x.len())));
            }
            if covered {
                // No prominent nudge towards shrinking a working setup: the
                // button is secondary and asks for confirmation.
                check_line(ui, Level::Good, "The protected ranges already cover every error found, plus the margin.");
                if current != suggested
                    && ui.add_enabled(s.installed() && !busy, egui::Button::new("Update to these ranges…"))
                        .on_hover_text("Replace the protected ranges with the scan result. Frees memory, but only safe if the scan is reliable.")
                        .clicked()
                {
                    self.setup.pending_ranges = suggested;
                    self.confirm = Some(Confirm::ReplaceRanges);
                }
                return;
            }
            // With ranges already protected, add the new ones: a single scan may
            // miss errors that the existing ranges cover, so never drop them here.
            // (Shrinking is the confirmed "Update to these ranges" path above.)
            let (label, target) = if protect_configured {
                check_line(ui, Level::Warn, "Some errors are outside the protected ranges.");
                let mut both = current.clone();
                both.extend_from_slice(&suggested);
                ("Add these ranges", range::normalize(&both).unwrap_or(both))
            } else {
                ("Protect these ranges", suggested.clone())
            };
            let resp = ui.add_enabled(
                s.installed() && !busy && !suggested.is_empty(),
                egui::Button::new(RichText::new(label).strong()),
            );
            let resp = if protect_configured {
                resp.on_hover_text("Keeps the ranges already protected and adds these.")
            } else {
                resp
            };
            if resp.clicked() {
                self.start_job(ui.ctx(), "Protect bad memory", move |log| ops::protect_ranges(&target, log));
            }
        });
        ui.add_space(10.0);

        // ---- Step 4: verify
        step_header(ui, 4, "Check that it works", protected_now);
        ui.indent("step4", |ui| {
            if protected_now {
                let pages = s.ramfence.as_ref().map_or(0, |i| i.pages);
                check_line(ui, Level::Good, &format!(
                    "Done. {} of damaged memory was reserved before Windows started, and Windows does not use it.",
                    report::size(pages * PAGE_SIZE)));
                if matches!(s.truncatememory, Ok(Some(_))) {
                    check_line(ui, Level::Warn, "An old memory limit is still set, so Windows ignores some good memory.");
                    if ui.add_enabled(!busy, egui::Button::new("Remove the old memory limit…")).clicked() {
                        self.confirm = Some(Confirm::RemoveLimit);
                    }
                }
                ui.add_space(4.0);
                ui.label(RichText::new(
                    "Later: if a message says this start was not protected, restart as it asks. After a BIOS \
                     update or a memory change, scan again.").weak());
            } else if protect_configured {
                ui.label("Restart to switch the protection on, then open RAMFence Manager again to check.");
                if ui.add_enabled(!busy, egui::Button::new("Restart now…")).clicked() {
                    self.confirm = Some(Confirm::Restart);
                }
            } else {
                ui.label(RichText::new("Finish step 3 first.").weak());
            }
        });
    }

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Log");
            if self.busy() {
                ui.spinner();
            }
            if ui.small_button("Clear").clicked() {
                self.log.clear();
            }
        });
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            for (kind, line) in &self.log {
                let text = RichText::new(line).monospace();
                ui.label(match kind {
                    LineKind::Header => text.strong(),
                    LineKind::Normal => text,
                    LineKind::Ok => text.color(level_color(Level::Good)),
                    LineKind::Error => text.color(level_color(Level::Bad)),
                });
            }
        });
    }
}

impl eframe::App for ManagerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("RAMFence Manager");
                if !self.elevated() && self.status.is_some() {
                    ui.colored_label(level_color(Level::Warn), "⚠ not running as administrator");
                    if ui.small_button("Restart as administrator").clicked() && crate::win::relaunch_elevated("") {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Setup, "Setup");
                ui.selectable_value(&mut self.tab, Tab::Status, "Status");
                ui.selectable_value(&mut self.tab, Tab::Config, "Configuration");
                ui.selectable_value(&mut self.tab, Tab::MemTest, "Memory Test");
                ui.selectable_value(&mut self.tab, Tab::UsbTool, "USB Tool");
                ui.selectable_value(&mut self.tab, Tab::Install, "Install / Uninstall");
            });
            ui.add_space(2.0);
        });

        if !self.log.is_empty() || self.busy() {
            egui::Panel::bottom("log").resizable(true).default_size(160.0).show(ui, |ui| {
                self.log_panel(ui);
            });
        }

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
                Tab::Status => self.status_tab(ui),
                Tab::Setup => self.setup_tab(ui),
                Tab::Config => self.config_tab(ui),
                Tab::MemTest => self.memtest_tab(ui),
                Tab::UsbTool => self.usb_tab(ui),
                Tab::Install => self.install_tab(ui),
            });
        });

        self.confirm_dialog(&ctx);
    }
}

fn step_header(ui: &mut egui::Ui, n: u32, title: &str, done: bool) {
    let (icon, color) = if done {
        ("✔", level_color(Level::Good))
    } else {
        ("●", ui.visuals().strong_text_color())
    };
    ui.label(RichText::new(format!("{icon}  Step {n}: {title}")).size(17.0).strong().color(color));
}

fn check_line(ui: &mut egui::Ui, level: Level, text: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(level_icon(level)).color(level_color(level)));
        ui.label(text);
    });
}

fn is_not_ram(u: &ramfence_core::memtest::Untested) -> bool {
    u.reason == "not in memory map"
}

/// Error histogram over the requested span, with tested/untested strips and
/// the configured bad ranges shaded.
fn histogram(ui: &mut egui::Ui, r: &ramfence_core::memtest::TestResult, protected: &[Range]) {
    let (Some(lo), Some(hi)) = (
        r.requested.iter().map(Range::start).min(),
        r.requested.iter().map(Range::end).max(),
    ) else {
        return;
    };
    let span = (hi - lo).max(1) as f32;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 120.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    let x = |a: u64| rect.left() + (a.clamp(lo, hi) - lo) as f32 / span * rect.width();
    let band = |a: u64, b: u64, y0: f32, y1: f32| {
        egui::Rect::from_x_y_ranges(x(a)..=x(b).max(x(a) + 1.0), y0..=y1)
    };
    let bottom = rect.bottom();
    for pr in protected {
        p.rect_filled(band(pr.start(), pr.end(), rect.top(), bottom - 8.0), 0.0, Color32::from_rgba_unmultiplied(0x3a, 0x7b, 0xd5, 50));
    }
    for t in &r.tested {
        p.rect_filled(band(t.start(), t.end(), bottom - 6.0, bottom), 0.0, level_color(Level::Good));
    }
    for u in r.untested.iter().filter(|u| !is_not_ram(u)) {
        p.rect_filled(band(u.range.start(), u.range.end(), bottom - 6.0, bottom), 0.0, Color32::GRAY);
    }
    let max = r.errors.buckets.values().copied().max().unwrap_or(0).max(1) as f32;
    let usable = rect.height() - 12.0;
    for (&b, &n) in &r.errors.buckets {
        let h = (n as f32).sqrt() / max.sqrt() * usable;
        let end = b + ramfence_core::memtest::BUCKET_SIZE;
        p.rect_filled(band(b, end, bottom - 8.0 - h, bottom - 8.0), 0.0, level_color(Level::Bad));
    }
    ui.horizontal(|ui| {
        ui.monospace(format!("{lo:#x}"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.monospace(format!("{hi:#x}"));
        });
    });
}

/// Adds Windows fonts as fallbacks: Segoe UI Symbol for the status icons
/// (✔ ✖ ⚠ ⟳, missing from egui's default fonts) and Microsoft JhengHei so
/// localized bcdedit/system messages render. The UI itself is English.
fn install_cjk_fallback(ctx: &egui::Context) {
    let Some(windir) = std::env::var_os("WINDIR") else { return };
    let fonts_dir = PathBuf::from(windir).join("Fonts");
    let groups: [(&str, &[&str]); 2] = [
        ("symbols", &["seguisym.ttf"]),
        ("cjk", &["msjh.ttc", "msyh.ttc", "simsun.ttc"]),
    ];
    let mut fonts = egui::FontDefinitions::default();
    for (name, files) in groups {
        let Some(bytes) = files.iter().find_map(|f| std::fs::read(fonts_dir.join(f)).ok()) else {
            continue;
        };
        fonts.font_data.insert(name.into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push(name.into());
        }
    }
    ctx.set_fonts(fonts);
}
