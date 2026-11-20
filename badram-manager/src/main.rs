//! BadRAM Manager: install, configure, check and uninstall badram.efi.
//!
//! Usage:
//!   badram-manager              GUI (asks for elevation)
//!   badram-manager --no-elevate GUI without the UAC prompt (read-only checks)
//!   badram-manager --check      headless logon check (scheduled task)
//!   badram-manager --status     print the status report to the console

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod bcd;
mod esp;
mod ops;
mod paths;
mod physmem;
mod report;
mod state;
mod sysstatus;
mod task;
mod win;

fn print_status() {
    let s = sysstatus::collect();
    let (level, headline) = report::headline(&s);
    println!("[{}] {headline}", level.symbol());
    for r in report::rows(&s) {
        println!("[{}] {:<20} {}", r.level.symbol(), r.label, r.text);
    }
    if let Ok(ranges) = &s.windows_ranges {
        println!("Windows physical memory ranges:");
        for r in ranges {
            println!("    {r}");
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    if has("--check") {
        std::process::exit(ops::run_check());
    }
    if has("--status") {
        win::attach_parent_console();
        print_status();
        return;
    }
    if !has("--no-elevate") && !win::is_elevated() && win::relaunch_elevated("") {
        return;
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("BadRAM Manager")
            .with_inner_size([860.0, 720.0])
            .with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native(
        "BadRAM Manager",
        options,
        Box::new(|cc| Ok(Box::new(app::ManagerApp::new(cc)))),
    ) {
        win::message_box("BadRAM Manager", &format!("Cannot start the window: {e}"), win::Icon::Error);
    }
}
