//! Text console: colored log lines, countdown, and the `on_error` prompt.
//!
//! UEFI consoles generally cannot render CJK, so all on-screen text is English.

use alloc::string::String;
use core::fmt::Display;
use core::time::Duration;

use badram_core::OnError;
use uefi::proto::console::text::{Color, Key};
use uefi::{boot, print, println, system};

/// Minimum time an `on_error boot` warning stays on screen.
const MIN_WARNING_SECS: u32 = 10;

fn set_color(fg: Color) {
    system::with_stdout(|out| {
        let _ = out.set_color(fg, Color::Black);
    });
}

fn colored(fg: Color, msg: impl Display) {
    set_color(fg);
    println!("{msg}");
    set_color(Color::LightGray);
}

pub fn clear() {
    system::with_stdout(|out| {
        let _ = out.clear();
    });
}

pub fn title(msg: impl Display) {
    colored(Color::White, msg);
}

/// Collects error state while printing every message as it happens.
#[derive(Default)]
pub struct Log {
    errors: usize,
    warnings: usize,
    first_error: Option<String>,
}

impl Log {
    pub fn info(&mut self, msg: impl Display) {
        colored(Color::LightGray, msg);
    }

    pub fn good(&mut self, msg: impl Display) {
        colored(Color::LightGreen, msg);
    }

    pub fn warn(&mut self, msg: impl Display) {
        self.warnings += 1;
        colored(Color::Yellow, format_args!("WARNING: {msg}"));
    }

    pub fn error(&mut self, msg: impl Display) {
        self.errors += 1;
        let text = alloc::format!("{msg}");
        colored(Color::LightRed, format_args!("ERROR: {text}"));
        self.first_error.get_or_insert(text);
    }

    pub fn is_ok(&self) -> bool {
        self.errors == 0
    }

    pub fn errors(&self) -> usize {
        self.errors
    }

    pub fn warnings(&self) -> usize {
        self.warnings
    }

    pub fn first_error(&self) -> Option<&str> {
        self.first_error.as_deref()
    }
}

fn read_key() -> Option<Key> {
    system::with_stdin(|input| input.read_key().ok().flatten())
}

fn read_char() -> Option<char> {
    match read_key()? {
        Key::Printable(c) => Some(char::from(c).to_ascii_lowercase()),
        Key::Special(_) => Some('\0'),
    }
}

/// Non-blocking: true if Esc was pressed since the last check.
pub fn escape_pressed() -> bool {
    use uefi::proto::console::text::ScanCode;
    matches!(read_key(), Some(Key::Special(ScanCode::ESCAPE)))
}

/// Discards keys pressed before a prompt was shown.
fn drain_keys() {
    while read_key().is_some() {}
}

fn wait_char() -> char {
    loop {
        if let Some(c) = read_char() {
            return c;
        }
        boot::stall(Duration::from_millis(50));
    }
}

/// Counts down `secs` seconds. Any key skips the wait; `P` pauses until the
/// next key press.
pub fn countdown(secs: u32) {
    if secs == 0 {
        return;
    }
    drain_keys();
    for remaining in (1..=secs).rev() {
        print!("\rContinuing in {remaining:>3} s  (any key = now, P = pause) ");
        for _ in 0..10 {
            boot::stall(Duration::from_millis(100));
            if let Some(c) = read_char() {
                println!();
                if c == 'p' {
                    println!("Paused. Press any key to continue.");
                    wait_char();
                }
                return;
            }
        }
    }
    println!();
}

/// What to do after errors were reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Boot,
    Reboot,
    Shutdown,
}

fn ask(boot_allowed: bool) -> Decision {
    drain_keys();
    loop {
        match wait_char() {
            'b' if boot_allowed => return Decision::Boot,
            'r' => return Decision::Reboot,
            's' => return Decision::Shutdown,
            _ => {}
        }
    }
}

/// Applies `on_error` (DESIGN.md §2.3) after at least one error.
pub fn handle_error(mode: OnError, timeout: u32, log: &Log) -> Decision {
    println!();
    colored(
        Color::LightRed,
        format_args!(
            "{} error(s): bad memory may NOT be fully protected.",
            log.errors()
        ),
    );
    match mode {
        OnError::Boot => {
            colored(Color::Yellow, "on_error=boot: booting anyway.");
            countdown(timeout.max(MIN_WARNING_SECS));
            Decision::Boot
        }
        OnError::Prompt => {
            colored(
                Color::White,
                "[B] boot anyway   [R] reboot   [S] shut down",
            );
            ask(true)
        }
        OnError::Halt => {
            colored(
                Color::White,
                "on_error=halt: not booting.   [R] reboot   [S] shut down",
            );
            ask(false)
        }
    }
}

/// Used when chainloading itself failed: booting is no longer possible.
pub fn after_boot_failure(mode: OnError) -> Option<Decision> {
    match mode {
        // Unattended: return to firmware so it can try the next boot option.
        OnError::Boot => {
            colored(
                Color::Yellow,
                "on_error=boot: returning to firmware boot manager.",
            );
            countdown(MIN_WARNING_SECS);
            None
        }
        OnError::Prompt | OnError::Halt => {
            colored(Color::White, "[R] reboot   [S] shut down");
            Some(ask(false))
        }
    }
}
