//! Startup apps: registry Run keys, startup folders, and packaged
//! (MSIX/AppX) StartupTask entries, with enabled/disabled state from
//! Explorer's StartupApproved registry data. Queried once on demand — the
//! set changes rarely — and re-queried on tab activation.

use std::sync::Arc;

use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW, HKEY,
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, REG_SZ,
};

#[derive(Clone, Debug, Default)]
pub struct StartupApp {
    pub name: Arc<str>,
    pub publisher: Arc<str>,
    pub enabled: bool,
    /// "Registry", "Folder", or "Packaged app".
    pub kind: &'static str,
    /// Where it lives: "HKCU Run", "Startup Folder (User)", package name...
    pub location: Arc<str>,
    pub command: Arc<str>,
}

struct Key(HKEY);

impl Key {
    fn open(root: HKEY, path: &str, flags: u32) -> Option<Key> {
        let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
        let mut key: HKEY = std::ptr::null_mut();
        let ok = unsafe {
            RegOpenKeyExW(root, wide.as_ptr(), 0, KEY_READ | flags, &mut key) == 0
        };
        if ok {
            Some(Key(key))
        } else {
            None
        }
    }

    /// All (name, data) string values in this key.
    fn string_values(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut index = 0u32;
        loop {
            let mut name = [0u16; 512];
            let mut name_len = name.len() as u32;
            let mut data = [0u8; 4096];
            let mut data_len = data.len() as u32;
            let mut ty = 0u32;
            let rc = unsafe {
                RegEnumValueW(
                    self.0,
                    index,
                    name.as_mut_ptr(),
                    &mut name_len,
                    std::ptr::null_mut(),
                    &mut ty,
                    data.as_mut_ptr(),
                    &mut data_len,
                )
            };
            if rc != 0 {
                break;
            }
            index += 1;
            if ty == REG_SZ || ty == 2u32 /* REG_EXPAND_SZ */ {
                let name = String::from_utf16_lossy(&name[..name_len as usize]);
                let chars = data_len as usize / 2;
                let wide =
                    unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u16, chars) };
                let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
                out.push((name, String::from_utf16_lossy(&wide[..end])));
            }
        }
        out
    }

    /// Binary value (StartupApproved format).
    fn binary_value(&self, name: &str) -> Option<Vec<u8>> {
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let mut data = [0u8; 64];
        let mut len = data.len() as u32;
        let ok = unsafe {
            RegQueryValueExW(
                self.0,
                wide.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                data.as_mut_ptr(),
                &mut len,
            ) == 0
        };
        if ok {
            Some(data[..len as usize].to_vec())
        } else {
            None
        }
    }

    fn dword_value(&self, name: &str) -> Option<u32> {
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let mut data = 0u32;
        let mut len = 4u32;
        let ok = unsafe {
            RegQueryValueExW(
                self.0,
                wide.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut data as *mut u32 as *mut u8,
                &mut len,
            ) == 0
        };
        if ok {
            Some(data)
        } else {
            None
        }
    }

    fn subkeys(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut index = 0u32;
        loop {
            let mut name = [0u16; 512];
            let mut len = name.len() as u32;
            let rc = unsafe {
                RegEnumKeyExW(
                    self.0,
                    index,
                    name.as_mut_ptr(),
                    &mut len,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if rc != 0 {
                break;
            }
            index += 1;
            out.push(String::from_utf16_lossy(&name[..len as usize]));
        }
        out
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

/// StartupApproved binary format: first byte even = enabled, odd/0x03 =
/// disabled. Missing entry = enabled (never toggled).
fn approved_state(approved: &Option<Key>, name: &str) -> bool {
    match approved {
        Some(key) => match key.binary_value(name) {
            Some(data) if !data.is_empty() => data[0] & 0x1 == 0 && data[0] != 0x0,
            _ => true,
        },
        None => true,
    }
}

/// Publisher (CompanyName) from the exe a command line points at.
fn publisher_of(command: &str) -> Arc<str> {
    // Extract the exe path: quoted prefix, or up to ".exe".
    let cmd = command.trim();
    let path = if let Some(rest) = cmd.strip_prefix('"') {
        rest.split('"').next().unwrap_or("")
    } else if let Some(ix) = cmd.to_ascii_lowercase().find(".exe") {
        &cmd[..ix + 4]
    } else {
        cmd
    };
    if path.is_empty() {
        return Arc::from("");
    }
    let expanded = expand_env(path);
    let wide: Vec<u16> = expanded.encode_utf16().collect();
    let (_, company) = unsafe { crate::enrich::version_strings(&wide) };
    company.unwrap_or_else(|| Arc::from(""))
}

fn expand_env(path: &str) -> String {
    if !path.contains('%') {
        return path.to_string();
    }
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
    let mut buf = [0u16; 1024];
    let n = unsafe { ExpandEnvironmentStringsW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if n > 1 {
        String::from_utf16_lossy(&buf[..n as usize - 1])
    } else {
        path.to_string()
    }
}

pub fn query_startup_apps() -> Vec<StartupApp> {
    let mut out = Vec::new();
    const APPROVED: &str =
        "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved";

    // Registry Run keys, with their matching StartupApproved subkeys.
    let sources: [(HKEY, &str, u32, &str, &str); 3] = [
        (HKEY_CURRENT_USER, "Software\\Microsoft\\Windows\\CurrentVersion\\Run", 0, "Run", "HKCU Run"),
        (HKEY_LOCAL_MACHINE, "Software\\Microsoft\\Windows\\CurrentVersion\\Run", 0, "Run", "HKLM Run"),
        (HKEY_LOCAL_MACHINE, "Software\\Microsoft\\Windows\\CurrentVersion\\Run", KEY_WOW64_32KEY, "Run32", "HKLM Run (32-bit)"),
    ];
    for (root, path, flags, approved_sub, label) in sources {
        let Some(key) = Key::open(root, path, flags) else {
            continue;
        };
        let approved = Key::open(root, &format!("{APPROVED}\\{approved_sub}"), flags);
        for (name, command) in key.string_values() {
            out.push(StartupApp {
                publisher: publisher_of(&command),
                enabled: approved_state(&approved, &name),
                kind: "Registry",
                location: Arc::from(label),
                command: Arc::from(command),
                name: Arc::from(name),
            });
        }
    }

    // Startup folders (per-user and common): .lnk / executables.
    let folders = [
        (std::env::var("APPDATA").ok().map(|p| {
            p + "\\Microsoft\\Windows\\Start Menu\\Programs\\Startup"
        }), "Startup Folder (User)", HKEY_CURRENT_USER),
        (std::env::var("ProgramData").ok().map(|p| {
            p + "\\Microsoft\\Windows\\Start Menu\\Programs\\Startup"
        }), "Startup Folder (Common)", HKEY_LOCAL_MACHINE),
    ];
    for (dir, label, approved_root) in folders {
        let Some(dir) = dir else { continue };
        let approved = Key::open(approved_root, &format!("{APPROVED}\\StartupFolder"), 0);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name().to_string_lossy().to_string();
            if file_name.eq_ignore_ascii_case("desktop.ini") {
                continue;
            }
            let target = resolve_link(&entry.path());
            let display = file_name
                .rsplit_once('.')
                .map(|(stem, _)| stem.to_string())
                .unwrap_or_else(|| file_name.clone());
            out.push(StartupApp {
                publisher: publisher_of(&target),
                enabled: approved_state(&approved, &file_name),
                kind: "Folder",
                location: Arc::from(label),
                command: Arc::from(target),
                name: Arc::from(display),
            });
        }
    }

    // Packaged apps: AppModel StartupTask state per package family.
    if let Some(key) = Key::open(
        HKEY_CURRENT_USER,
        "Software\\Classes\\Local Settings\\Software\\Microsoft\\Windows\\CurrentVersion\\AppModel\\SystemAppData",
        0,
    ) {
        for family in key.subkeys() {
            let Some(task_root) = Key::open(
                HKEY_CURRENT_USER,
                &format!("Software\\Classes\\Local Settings\\Software\\Microsoft\\Windows\\CurrentVersion\\AppModel\\SystemAppData\\{family}"),
                0,
            ) else {
                continue;
            };
            for task in task_root.subkeys() {
                let Some(task_key) = Key::open(
                    HKEY_CURRENT_USER,
                    &format!("Software\\Classes\\Local Settings\\Software\\Microsoft\\Windows\\CurrentVersion\\AppModel\\SystemAppData\\{family}\\{task}"),
                    0,
                ) else {
                    continue;
                };
                let Some(state) = task_key.dword_value("State") else {
                    continue;
                };
                // Friendly-ish name: family name up to the publisher-hash
                // suffix, last dotted segment.
                let base = family.split('_').next().unwrap_or(&family);
                let display = base.rsplit('.').next().unwrap_or(base);
                out.push(StartupApp {
                    name: Arc::from(display),
                    publisher: Arc::from(base.split('.').next().unwrap_or("")),
                    // State: 0/1 disabled, 2/3 enabled (2 = enabled by user).
                    enabled: state >= 2,
                    kind: "Packaged app",
                    location: Arc::from(family.as_str()),
                    command: Arc::from(""),
                });
            }
        }
    }

    out.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
    out
}

/// Resolve a .lnk's target via IShellLinkW; non-links return their own path.
fn resolve_link(path: &std::path::Path) -> String {
    let is_lnk = path
        .extension()
        .map(|e| e.eq_ignore_ascii_case("lnk"))
        .unwrap_or(false);
    if !is_lnk {
        return path.to_string_lossy().to_string();
    }
    // Shell COM link resolution needs apartment init and a good chunk of
    // code; the target path inside a .lnk is also discoverable by scanning
    // for the embedded path. Keep it simple and robust: read the file and
    // pull the first ASCII run that looks like a drive path ending in .exe.
    if let Ok(bytes) = std::fs::read(path) {
        let hay = String::from_utf8_lossy(&bytes);
        for start in hay.match_indices(":\\").map(|(i, _)| i) {
            if start == 0 {
                continue;
            }
            let begin = start - 1;
            let rest = &hay[begin..];
            if let Some(end) = rest.to_ascii_lowercase().find(".exe") {
                let candidate = &rest[..end + 4];
                if candidate.len() < 260 && candidate.chars().all(|c| c != '\0') {
                    return candidate.to_string();
                }
            }
        }
    }
    path.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_apps_enumerate() {
        let apps = query_startup_apps();
        // Any real Windows box has at least one Run entry or packaged task.
        assert!(!apps.is_empty());
    }
}
