//! Logon task that runs `ramfence-manager.exe --check` elevated.

use std::path::Path;

use crate::paths::TASK_NAME;
use crate::win::{run, run_ok};

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn task_xml(user: &str, exe: &str) -> String {
    let (user, exe) = (xml_escape(user), xml_escape(exe));
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Checks that this boot went through ramfence.efi and keeps it first in the boot order.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT5M</ExecutionTimeLimit>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>--check</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// `DOMAIN\user` of the current (elevated) process.
pub fn current_user() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    match std::env::var("USERDOMAIN") {
        Ok(d) if !d.is_empty() => format!("{d}\\{user}"),
        _ => user,
    }
}

pub fn register(exe: &Path) -> Result<(), String> {
    let xml = task_xml(&current_user(), &exe.to_string_lossy());
    // schtasks expects UTF-16LE with BOM when the declaration says UTF-16.
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    // In the install folder, not %TEMP%: only administrators can write
    // there, so no other program can swap the file before schtasks reads it.
    let tmp = crate::paths::install_dir().join(format!("task-{}.xml", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &bytes))
        .map_err(|e| format!("cannot write task XML: {e}"))?;
    let tmp_s = tmp.to_string_lossy().into_owned();
    let result = run_ok("schtasks.exe", &["/Create", "/TN", TASK_NAME, "/XML", &tmp_s, "/F"]);
    let _ = std::fs::remove_file(&tmp);
    result.map(drop)
}

pub fn exists() -> bool {
    run("schtasks.exe", &["/Query", "/TN", TASK_NAME]).is_ok_and(|o| o.code == 0)
}

pub fn unregister() -> Result<(), String> {
    if !exists() {
        return Ok(());
    }
    run_ok("schtasks.exe", &["/Delete", "/TN", TASK_NAME, "/F"]).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escapes_values() {
        let x = task_xml("PC\\a&b", r"C:\Program Files\RAMFence\ramfence-manager.exe");
        assert!(x.contains("<UserId>PC\\a&amp;b</UserId>"));
        assert!(x.contains(r"<Command>C:\Program Files\RAMFence\ramfence-manager.exe</Command>"));
        assert!(x.contains("<Arguments>--check</Arguments>"));
    }
}
