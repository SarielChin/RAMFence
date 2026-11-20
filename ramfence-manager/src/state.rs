//! `state.json` in the install directory (same format as install.ps1).

use serde::{Deserialize, Serialize};

use crate::paths::state_file;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct State {
    pub entry_id: String,
    pub installed_at: String,
}

/// Parses state.json; tolerates the UTF-8 BOM Windows PowerShell writes.
pub fn parse(text: &str) -> Option<State> {
    let text = text.trim_start_matches('\u{FEFF}');
    let mut s: State = serde_json::from_str(text).ok()?;
    s.entry_id = s.entry_id.to_ascii_lowercase();
    Some(s)
}

pub fn load() -> Option<State> {
    parse(&std::fs::read_to_string(state_file()).ok()?)
}

pub fn save(state: &State) -> Result<(), String> {
    let text = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    std::fs::write(state_file(), text).map_err(|e| format!("cannot write state.json: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_powershell_output() {
        let ps = "\u{FEFF}{\r\n    \"EntryId\":  \"{12345678-9ABC-DEF0-1234-56789ABCDEF0}\",\r\n    \"InstalledAt\":  \"2026-01-02T03:04:05\"\r\n}\r\n";
        assert_eq!(
            parse(ps),
            Some(State {
                entry_id: "{12345678-9abc-def0-1234-56789abcdef0}".into(),
                installed_at: "2026-01-02T03:04:05".into(),
            })
        );
    }

    #[test]
    fn round_trip_and_garbage() {
        let s = State { entry_id: "{x}".into(), installed_at: "t".into() };
        assert_eq!(parse(&serde_json::to_string(&s).unwrap()), Some(s));
        assert_eq!(parse("not json"), None);
    }
}
