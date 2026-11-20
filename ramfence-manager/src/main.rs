//! RAMFence Manager: install, configure, check and uninstall ramfence.efi.
//!
//! Usage:
//!   ramfence-manager              GUI (asks for elevation)
//!   ramfence-manager --no-elevate GUI without the UAC prompt (read-only checks)
//!   ramfence-manager --check      headless logon check (scheduled task)
//!   ramfence-manager --status     print the status report to the console

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
    win::restrict_dll_search();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    // Running with administrator rights next to foreign DLLs is unsafe (see
    // win::dlls_next_to_exe). The logon check runs from the install folder,
    // which only administrators can write to.
    if !has("--check") && !has("--status") {
        let dlls = win::dlls_next_to_exe();
        if !dlls.is_empty() {
            win::message_box(
                "RAMFence Manager",
                &format!(
                    "The folder this program was started from also contains DLL files ({}). Windows could \
                     load them into RAMFence Manager, which runs with administrator rights.\n\n\
                     Move RAMFence-Manager.exe into a folder of its own and start it from there.",
                    dlls.join(", ")
                ),
                win::Icon::Error,
            );
            return;
        }
    }

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
            .with_title("RAMFence Manager")
            .with_inner_size([860.0, 720.0])
            .with_min_inner_size([640.0, 480.0]),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native(
        "RAMFence Manager",
        options,
        Box::new(|cc| Ok(Box::new(app::ManagerApp::new(cc)))),
    ) {
        win::message_box("RAMFence Manager", &format!("Cannot start the window: {e}"), win::Icon::Error);
    }
}
