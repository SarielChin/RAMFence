//! Thin safe wrappers over the Win32 calls the manager needs.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, LUID, SYSTEMTIME};
use windows_sys::Win32::Globalization::{CP_OEMCP, MultiByteToWideChar};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
    SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_ELEVATION, TOKEN_PRIVILEGES, TOKEN_QUERY,
    TokenElevation,
};
use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows_sys::Win32::System::Registry::{HKEY, HKEY_LOCAL_MACHINE, RRF_RT_ANY, RegGetValueW};
use windows_sys::Win32::System::SystemInformation::{
    FIRMWARE_TYPE, GetFirmwareType, GetLocalTime, GetPhysicallyInstalledSystemMemory,
    GlobalMemoryStatusEx, MEMORYSTATUSEX,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::System::WindowsProgramming::GetFirmwareEnvironmentVariableExW;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MessageBoxW,
    SW_SHOWNORMAL,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Makes DLLs that are loaded at run time come only from System32, never
/// from the directory this exe was started from (for example Downloads).
pub fn restrict_dll_search() {
    use windows_sys::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories};
    // SAFETY: plain flag argument.
    unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) };
}

/// DLL files next to this exe. Windows loads some of the exe's imports from
/// its own directory before any code of ours runs, so a planted DLL there
/// (say, in Downloads) would run with administrator rights once elevated.
pub fn dlls_next_to_exe() -> Vec<String> {
    let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Into::into)) else {
        return Vec::new();
    };
    let dir: std::path::PathBuf = dir;
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_ascii_lowercase().ends_with(".dll"))
        .collect();
    names.sort();
    names
}

/// The real "Program Files" folder (known folder, not the `ProgramFiles`
/// environment variable, which a user-level setting could redirect).
pub fn program_files() -> Option<std::path::PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath};
    let mut p: windows_sys::core::PWSTR = ptr::null_mut();
    // SAFETY: valid GUID pointer and out pointer; the returned buffer is
    // freed with CoTaskMemFree in every case, as documented.
    unsafe {
        let hr = SHGetKnownFolderPath(&FOLDERID_ProgramFiles, 0, ptr::null_mut(), &mut p);
        let out = if hr == 0 && !p.is_null() {
            let len = (0..).take_while(|&i| *p.add(i) != 0).count();
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
            Some(std::path::PathBuf::from(s))
        } else {
            None
        };
        CoTaskMemFree(p as *const _);
        out
    }
}
const ERROR_ENVVAR_NOT_FOUND: u32 = 203;

/// NUL-terminated UTF-16.
pub fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// Decodes console output (OEM code page, e.g. cp950 on zh-TW Windows).
pub fn decode_oem(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let Ok(len) = i32::try_from(bytes.len()) else {
        return String::from_utf8_lossy(bytes).into_owned();
    };
    // SAFETY: valid input slice; first call only measures.
    let n = unsafe { MultiByteToWideChar(CP_OEMCP, 0, bytes.as_ptr(), len, ptr::null_mut(), 0) };
    if n <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut buf = vec![0u16; n as usize];
    // SAFETY: buf has exactly n elements.
    let n = unsafe { MultiByteToWideChar(CP_OEMCP, 0, bytes.as_ptr(), len, buf.as_mut_ptr(), n) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub struct CmdOutput {
    pub code: i32,
    /// stdout followed by stderr, decoded.
    pub text: String,
}

/// Runs a console program without flashing a window.
pub fn run(program: &str, args: &[&str]) -> Result<CmdOutput, String> {
    let out = Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    let mut text = decode_oem(&out.stdout);
    text.push_str(&decode_oem(&out.stderr));
    Ok(CmdOutput {
        code: out.status.code().unwrap_or(-1),
        text,
    })
}

/// Like [`run`] but fails on a non-zero exit code.
pub fn run_ok(program: &str, args: &[&str]) -> Result<String, String> {
    let out = run(program, args)?;
    if out.code == 0 {
        Ok(out.text)
    } else {
        Err(format!(
            "{program} {} failed (exit {}): {}",
            args.join(" "),
            out.code,
            out.text.trim()
        ))
    }
}

struct Token(HANDLE);

impl Token {
    fn open(access: u32) -> Option<Token> {
        let mut h: HANDLE = ptr::null_mut();
        // SAFETY: pseudo-handle for the current process; out-pointer is valid.
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut h) };
        (ok != 0).then_some(Token(h))
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        // SAFETY: handle came from OpenProcessToken.
        unsafe { CloseHandle(self.0) };
    }
}

pub fn is_elevated() -> bool {
    let Some(token) = Token::open(TOKEN_QUERY) else {
        return false;
    };
    let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    // SAFETY: buffer is a TOKEN_ELEVATION of the stated size.
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            (&raw mut elev).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    ok != 0 && elev.TokenIsElevated != 0
}

/// Starts this executable again via the UAC "runas" verb. Returns false if
/// the user declined or the launch failed.
pub fn relaunch_elevated(args: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe = wide(&exe.to_string_lossy());
    let verb = wide("runas");
    let params = wide(args);
    // SAFETY: all strings are NUL-terminated and outlive the call.
    let h = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr(),
            exe.as_ptr(),
            params.as_ptr(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    h as isize > 32
}

#[derive(Clone, Copy)]
pub enum Icon {
    Warning,
    Error,
}

pub fn message_box(title: &str, text: &str, icon: Icon) {
    let style = MB_OK
        | MB_SETFOREGROUND
        | match icon {
            Icon::Warning => MB_ICONWARNING,
            Icon::Error => MB_ICONERROR,
        };
    let (t, c) = (wide(text), wide(title));
    // SAFETY: NUL-terminated strings; no owner window.
    unsafe { MessageBoxW(ptr::null_mut(), t.as_ptr(), c.as_ptr(), style) };
}

/// Enables a privilege the token holds but has disabled (e.g.
/// SeSystemEnvironmentPrivilege for an elevated administrator).
fn enable_privilege(name: &str) -> Result<(), u32> {
    let token = Token::open(TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY)
        // SAFETY: trivially safe.
        .ok_or_else(|| unsafe { GetLastError() })?;
    let mut luid = LUID {
        LowPart: 0,
        HighPart: 0,
    };
    let name = wide(name);
    // SAFETY: valid strings and out-pointer.
    if unsafe { LookupPrivilegeValueW(ptr::null(), name.as_ptr(), &mut luid) } == 0 {
        // SAFETY: trivially safe.
        return Err(unsafe { GetLastError() });
    }
    let tp = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: tp is a valid TOKEN_PRIVILEGES; no previous-state buffer.
    unsafe {
        AdjustTokenPrivileges(token.0, 0, &tp, 0, ptr::null_mut(), ptr::null_mut());
    }
    // AdjustTokenPrivileges "succeeds" even when the privilege is not held;
    // GetLastError distinguishes (1300 = ERROR_NOT_ALL_ASSIGNED).
    // SAFETY: trivially safe.
    match unsafe { GetLastError() } {
        0 => Ok(()),
        e => Err(e),
    }
}

/// Reads a UEFI variable. `Ok(None)` if it does not exist.
pub fn read_firmware_variable(name: &str, guid: &str) -> Result<Option<Vec<u8>>, String> {
    enable_privilege("SeSystemEnvironmentPrivilege").map_err(|e| {
        format!("cannot enable SeSystemEnvironmentPrivilege (Win32 error {e}); run as administrator")
    })?;
    let (n, g) = (wide(name), wide(guid));
    let mut buf = vec![0u8; 64 * 1024];
    let mut attrs = 0u32;
    // SAFETY: buffer and strings valid for the call.
    let len = unsafe {
        GetFirmwareEnvironmentVariableExW(
            n.as_ptr(),
            g.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            &mut attrs,
        )
    };
    if len == 0 {
        // SAFETY: trivially safe.
        let err = unsafe { GetLastError() };
        if err == ERROR_ENVVAR_NOT_FOUND {
            return Ok(None);
        }
        return Err(format!("reading firmware variable {name} failed (Win32 error {err})"));
    }
    buf.truncate(len as usize);
    Ok(Some(buf))
}

fn reg_get(root: HKEY, subkey: &str, value: &str) -> Result<Vec<u8>, u32> {
    let (k, v) = (wide(subkey), wide(value));
    let mut size = 0u32;
    // SAFETY: size query with null data pointer.
    let err = unsafe {
        RegGetValueW(root, k.as_ptr(), v.as_ptr(), RRF_RT_ANY, ptr::null_mut(), ptr::null_mut(), &mut size)
    };
    if err != 0 {
        return Err(err);
    }
    let mut buf = vec![0u8; size as usize];
    // SAFETY: buf holds `size` bytes.
    let err = unsafe {
        RegGetValueW(root, k.as_ptr(), v.as_ptr(), RRF_RT_ANY, ptr::null_mut(), buf.as_mut_ptr().cast(), &mut size)
    };
    if err != 0 {
        return Err(err);
    }
    buf.truncate(size as usize);
    Ok(buf)
}

/// Raw bytes of a value under HKEY_LOCAL_MACHINE.
pub fn reg_hklm_bytes(subkey: &str, value: &str) -> Result<Vec<u8>, String> {
    reg_get(HKEY_LOCAL_MACHINE, subkey, value)
        .map_err(|e| format!("registry HKLM\\{subkey}\\{value}: Win32 error {e}"))
}

pub fn reg_hklm_dword(subkey: &str, value: &str) -> Option<u32> {
    let b = reg_get(HKEY_LOCAL_MACHINE, subkey, value).ok()?;
    Some(u32::from_le_bytes(b.get(..4)?.try_into().ok()?))
}

/// `Some(true)` when Windows booted via UEFI.
pub fn booted_uefi() -> Option<bool> {
    let mut t: FIRMWARE_TYPE = 0;
    // SAFETY: valid out-pointer.
    if unsafe { GetFirmwareType(&mut t) } == 0 {
        return None;
    }
    Some(t == windows_sys::Win32::System::SystemInformation::FirmwareTypeUefi)
}

/// (installed RAM, RAM visible to Windows) in bytes.
pub fn memory_totals() -> (Option<u64>, Option<u64>) {
    let mut kb = 0u64;
    // SAFETY: valid out-pointer.
    let installed = (unsafe { GetPhysicallyInstalledSystemMemory(&mut kb) } != 0).then(|| kb * 1024);
    // SAFETY: zero is a valid bit pattern for this plain-data struct.
    let mut ms: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    ms.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: dwLength set as required.
    let visible = (unsafe { GlobalMemoryStatusEx(&mut ms) } != 0).then_some(ms.ullTotalPhys);
    (installed, visible)
}

/// Local time as `YYYY-MM-DDTHH:MM:SS`.
pub fn local_timestamp() -> String {
    // SAFETY: zero is a valid bit pattern; GetLocalTime fills it.
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    // SAFETY: valid out-pointer.
    unsafe { GetLocalTime(&mut t) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// Lets a GUI-subsystem process print to the console it was started from.
pub fn attach_parent_console() {
    // SAFETY: trivially safe; failure (no parent console) is fine.
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_files_is_the_known_folder() {
        let p = program_files().expect("known folder");
        assert!(p.is_absolute() && p.is_dir(), "{}", p.display());
        // Not taken from the environment variable.
        assert!(std::env::var_os("ProgramFiles").is_none_or(|v| std::path::Path::new(&v) == p));
    }

    #[test]
    fn wide_is_nul_terminated() {
        assert_eq!(wide("ab"), vec![b'a' as u16, b'b' as u16, 0]);
    }

    #[test]
    fn decode_ascii() {
        assert_eq!(decode_oem(b"identifier {abc}\r\n"), "identifier {abc}\r\n");
        assert_eq!(decode_oem(b""), "");
    }

    #[test]
    fn timestamp_shape() {
        let t = local_timestamp();
        assert_eq!(t.len(), 19);
        assert_eq!(&t[10..11], "T");
    }
}
