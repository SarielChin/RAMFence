//! bcdedit wrappers. Element names (`displayorder`, `path`, ...) are not
//! localized by bcdedit, so only those are matched; headings are ignored.

use crate::win::{run, run_ok};

const BCDEDIT: &str = "bcdedit.exe";

/// Every `{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}` in `s`, lowercased.
pub fn find_guids(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 38 <= b.len() {
        if b[i] == b'{' && b[i + 37] == b'}' {
            let inner = &s[i + 1..i + 37];
            let shape_ok = inner.char_indices().all(|(j, c)| match j {
                8 | 13 | 18 | 23 => c == '-',
                _ => c.is_ascii_hexdigit(),
            });
            if shape_ok {
                out.push(format!("{{{}}}", inner.to_ascii_lowercase()));
                i += 38;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Values of a (possibly multi-line) element, e.g. `displayorder`.
fn element_lines<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let mut values = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let starts_with_name = line
            .strip_prefix(name)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace));
        if starts_with_name {
            inside = true;
            values.push(line[name.len()..].trim());
        } else if inside && line.starts_with(char::is_whitespace) && !line.trim().is_empty() {
            values.push(line.trim());
        } else {
            inside = false;
        }
    }
    values
}

/// Firmware boot order (GUIDs) from `bcdedit /enum {fwbootmgr} /v`.
pub fn parse_display_order(text: &str) -> Vec<String> {
    element_lines(text, "displayorder")
        .into_iter()
        .flat_map(find_guids)
        .collect()
}

/// First value of a single-line element, e.g. `truncatememory`.
pub fn parse_element(text: &str, name: &str) -> Option<String> {
    element_lines(text, name).first().map(|s| s.to_string())
}

pub fn firmware_boot_order() -> Result<Vec<String>, String> {
    Ok(parse_display_order(&run_ok(BCDEDIT, &["/enum", "{fwbootmgr}", "/v"])?))
}

pub fn entry_exists(id: &str) -> bool {
    run(BCDEDIT, &["/enum", id]).is_ok_and(|o| o.code == 0)
}

pub fn set_first(id: &str) -> Result<(), String> {
    run_ok(BCDEDIT, &["/set", "{fwbootmgr}", "displayorder", id, "/addfirst"]).map(drop)
}

/// Copies `{bootmgr}` into a new firmware application entry pointing at
/// `efi_path` on the ESP. Returns the new entry's GUID.
pub fn create_firmware_entry(description: &str, efi_path: &str) -> Result<String, String> {
    let out = run_ok(BCDEDIT, &["/copy", "{bootmgr}", "/d", description])?;
    let id = find_guids(&out)
        .into_iter()
        .next()
        .ok_or_else(|| format!("no entry id in bcdedit output: {}", out.trim()))?;
    if let Err(e) = run_ok(BCDEDIT, &["/set", &id, "path", efi_path]) {
        let _ = run(BCDEDIT, &["/delete", &id]);
        return Err(e);
    }
    Ok(id)
}

pub fn delete_entry(id: &str) -> Result<(), String> {
    run_ok(BCDEDIT, &["/delete", id]).map(drop)
}

pub fn put_windows_boot_manager_first() -> Result<(), String> {
    run_ok(BCDEDIT, &["/set", "{fwbootmgr}", "displayorder", "{bootmgr}", "/addfirst"]).map(drop)
}

pub fn export(path: &str) -> Result<(), String> {
    run_ok(BCDEDIT, &["/export", path]).map(drop)
}

/// `truncatememory` on the running Windows entry (msconfig "Maximum memory").
pub fn current_truncatememory() -> Result<Option<String>, String> {
    Ok(parse_element(&run_ok(BCDEDIT, &["/enum", "{current}"])?, "truncatememory"))
}

pub fn remove_current_truncatememory() -> Result<(), String> {
    run_ok(BCDEDIT, &["/deletevalue", "{current}", "truncatememory"]).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Built from lines: a `\` string continuation would strip the
    // indentation that marks continuation lines.
    const FW_LINES: [&str; 7] = [
        "Firmware Boot Manager",
        "--------------------",
        "identifier              {a5a30fa2-3d06-4e9f-b5f4-a01df9d1fcba}",
        "displayorder            {11111111-2222-3333-4444-555555555555}",
        "                        {9DEA862C-5CDD-4E70-ACC1-F32B344D4795}",
        "                        {aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}",
        "timeout                 0",
    ];

    #[test]
    fn display_order() {
        let fw = FW_LINES.join("\r\n");
        assert_eq!(
            parse_display_order(&fw),
            vec![
                "{11111111-2222-3333-4444-555555555555}",
                "{9dea862c-5cdd-4e70-acc1-f32b344d4795}",
                "{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}",
            ]
        );
        assert!(parse_display_order("identifier {a5a30fa2-3d06-4e9f-b5f4-a01df9d1fcba}\n").is_empty());
    }

    #[test]
    fn display_order_not_confused_by_similar_names() {
        let t = "displayorderx            {11111111-2222-3333-4444-555555555555}\n";
        assert!(parse_display_order(t).is_empty());
    }

    #[test]
    fn guids() {
        assert_eq!(
            find_guids("copied to {0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9}."),
            vec!["{0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9}"]
        );
        assert!(find_guids("{not-a-guid}").is_empty());
        assert!(find_guids("{bootmgr}").is_empty());
    }

    #[test]
    fn element() {
        let t = "Windows Boot Loader\n-------------------\nidentifier              {current}\n\
                 truncatememory          0x100000000\nnx                      OptIn\n";
        assert_eq!(parse_element(t, "truncatememory").as_deref(), Some("0x100000000"));
        assert_eq!(parse_element(t, "badmemorylist"), None);
    }
}
