//! Static system information for the Information tab: OS identity,
//! processor, disks, and physical memory modules (SMBIOS Type 17). Gathered
//! once on first view and cached — none of it changes while running, except
//! uptime, which the UI derives from the boot time we report.

use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_ANY};
use windows_sys::Win32::System::SystemInformation::{
    GetSystemFirmwareTable, GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX,
};

/// Key/value section content, ready for display.
pub type Section = Vec<(&'static str, String)>;

#[derive(Clone, Debug, Default)]
pub struct SystemInformation {
    pub os: Section,
    pub cpu: Section,
    pub system: Section,
    /// One section per volume.
    pub disks: Vec<Section>,
    pub memory: Section,
    /// One section per populated SMBIOS memory module.
    pub modules: Vec<Section>,
}

fn reg_str(key: &str, value: &str) -> Option<String> {
    let key_w: Vec<u16> = key.encode_utf16().chain([0]).collect();
    let val_w: Vec<u16> = value.encode_utf16().chain([0]).collect();
    let mut buf = [0u16; 512];
    let mut size = (buf.len() * 2) as u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key_w.as_ptr(),
            val_w.as_ptr(),
            RRF_RT_ANY,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            &mut size,
        ) == 0
    };
    if !ok {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
    Some(String::from_utf16_lossy(&buf[..end]).trim().to_string())
}

fn reg_dword(key: &str, value: &str) -> Option<u32> {
    let key_w: Vec<u16> = key.encode_utf16().chain([0]).collect();
    let val_w: Vec<u16> = value.encode_utf16().chain([0]).collect();
    let mut data = 0u32;
    let mut size = 4u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key_w.as_ptr(),
            val_w.as_ptr(),
            RRF_RT_ANY,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut core::ffi::c_void,
            &mut size,
        ) == 0
    };
    if ok {
        Some(data)
    } else {
        None
    }
}

/// Unix seconds → "YYYY-MM-DD HH:MM:SS" local time.
fn unix_to_local(secs: u64) -> String {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let ft_val = secs * 10_000_000 + 116_444_736_000_000_000;
    let ft = FILETIME {
        dwLowDateTime: ft_val as u32,
        dwHighDateTime: (ft_val >> 32) as u32,
    };
    unsafe {
        let mut st = std::mem::zeroed::<SYSTEMTIME>();
        if FileTimeToSystemTime(&ft, &mut st) == 0 {
            return String::new();
        }
        let mut lt = std::mem::zeroed::<SYSTEMTIME>();
        if SystemTimeToTzSpecificLocalTime(std::ptr::null(), &st, &mut lt) == 0 {
            return String::new();
        }
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            lt.wYear, lt.wMonth, lt.wDay, lt.wHour, lt.wMinute, lt.wSecond
        )
    }
}

fn fmt_bytes(bytes: u64) -> String {
    crate::fmt_bytes(bytes)
}

pub fn query_system_information() -> SystemInformation {
    const CV: &str = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion";
    const CPU0: &str = "HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0";

    let mut info = SystemInformation::default();

    // Operating system.
    let build: u32 = reg_str(CV, "CurrentBuildNumber")
        .and_then(|b| b.parse().ok())
        .unwrap_or(0);
    let ubr = reg_dword(CV, "UBR").unwrap_or(0);
    let mut product = reg_str(CV, "ProductName").unwrap_or_default();
    // The registry still says "Windows 10" on Windows 11 (build >= 22000).
    if build >= 22000 {
        product = product.replace("Windows 10", "Windows 11");
    }
    info.os.push(("OS Name", product));
    info.os.push((
        "Version",
        reg_str(CV, "DisplayVersion").unwrap_or_default(),
    ));
    info.os.push(("Build", format!("{build}.{ubr}")));
    info.os.push((
        "Architecture",
        std::env::var("PROCESSOR_ARCHITECTURE")
            .map(|a| if a == "AMD64" { "x64 (64-bit)".into() } else { a })
            .unwrap_or_default(),
    ));
    if let Some(ts) = reg_dword(CV, "InstallDate") {
        info.os.push(("Install Date", unix_to_local(ts as u64)));
    }
    info.os
        .push(("Product ID", reg_str(CV, "ProductId").unwrap_or_default()));

    // Processor.
    info.cpu.push((
        "Name",
        reg_str(CPU0, "ProcessorNameString").unwrap_or_default(),
    ));
    info.cpu.push((
        "Vendor",
        reg_str(CPU0, "VendorIdentifier").unwrap_or_default(),
    ));
    info.cpu
        .push(("Identifier", reg_str(CPU0, "Identifier").unwrap_or_default()));
    info.cpu.push((
        "Logical Processors",
        std::thread::available_parallelism()
            .map(|n| n.get().to_string())
            .unwrap_or_default(),
    ));
    info.cpu.push((
        "Speed",
        reg_dword(CPU0, "~MHz")
            .map(|m| format!("{m} MHz"))
            .unwrap_or_default(),
    ));

    // System.
    info.system.push((
        "Computer Name",
        std::env::var("COMPUTERNAME").unwrap_or_default(),
    ));
    info.system
        .push(("User Name", std::env::var("USERNAME").unwrap_or_default()));
    info.system.push((
        "User Domain",
        std::env::var("USERDOMAIN").unwrap_or_default(),
    ));
    let uptime_ms = unsafe { GetTickCount64() };
    let boot_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(uptime_ms / 1000))
        .unwrap_or(0);
    info.system.push(("Boot Time", unix_to_local(boot_unix)));

    // Disks.
    info.disks = query_volumes();

    // Memory totals.
    let mut mem = unsafe { std::mem::zeroed::<MEMORYSTATUSEX>() };
    mem.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    unsafe { GlobalMemoryStatusEx(&mut mem) };
    info.memory
        .push(("Total Physical", fmt_bytes(mem.ullTotalPhys)));
    info.memory
        .push(("Available Physical", fmt_bytes(mem.ullAvailPhys)));
    info.memory
        .push(("Memory Usage", format!("{}%", mem.dwMemoryLoad)));
    info.memory.push((
        "Commit Limit",
        fmt_bytes(mem.ullTotalPageFile),
    ));

    // Physical modules from SMBIOS.
    info.modules = query_memory_modules();

    info
}

fn query_volumes() -> Vec<Section> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOVABLE: u32 = 2;
    let mut out = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as u16;
        let root = [letter, b':' as u16, b'\\' as u16, 0];
        let ty = unsafe { GetDriveTypeW(root.as_ptr()) };
        if ty != DRIVE_FIXED && ty != DRIVE_REMOVABLE {
            continue;
        }
        let mut name = [0u16; 128];
        let mut fs = [0u16; 64];
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                name.as_mut_ptr(),
                name.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            ) != 0
        };
        let (mut free, mut total) = (0u64, 0u64);
        unsafe {
            GetDiskFreeSpaceExW(
                root.as_ptr(),
                std::ptr::null_mut(),
                &mut total,
                &mut free,
            )
        };
        let wide_str = |buf: &[u16]| {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
            String::from_utf16_lossy(&buf[..end])
        };
        let mut section: Section = Vec::new();
        section.push((
            "Path",
            format!(
                "{}: ({})",
                (b'A' + i as u8) as char,
                if ty == DRIVE_FIXED { "Fixed" } else { "Removable" }
            ),
        ));
        if ok {
            let label = wide_str(&name);
            let fs = wide_str(&fs);
            section.push((
                "Volume",
                if label.is_empty() {
                    fs.clone()
                } else {
                    format!("{label} - {fs}")
                },
            ));
        }
        if total > 0 {
            section.push((
                "Space",
                format!("{} free of {}", fmt_bytes(free), fmt_bytes(total)),
            ));
        }
        out.push(section);
    }
    out
}

/// SMBIOS Type 17 (Memory Device) parse via GetSystemFirmwareTable('RSMB').
fn query_memory_modules() -> Vec<Section> {
    let mut out = Vec::new();
    const RSMB: u32 = u32::from_be_bytes(*b"RSMB");
    unsafe {
        let size = GetSystemFirmwareTable(RSMB, 0, std::ptr::null_mut(), 0);
        if size == 0 {
            return out;
        }
        let mut buf = vec![0u8; size as usize];
        if GetSystemFirmwareTable(RSMB, 0, buf.as_mut_ptr(), size) == 0 {
            return out;
        }
        // RawSMBIOSData header: method u8, major u8, minor u8, dmi u8, len u32.
        if buf.len() < 8 {
            return out;
        }
        let table = &buf[8..];
        let mut off = 0usize;
        let mut module_no = 0u32;
        while off + 4 <= table.len() {
            let ty = table[off];
            let len = table[off + 1] as usize;
            if len < 4 || off + len > table.len() {
                break;
            }
            // String area: after the formatted section, double-NUL terminated.
            let strings_start = off + len;
            let mut strings: Vec<&[u8]> = Vec::new();
            let mut s = strings_start;
            loop {
                if s >= table.len() {
                    break;
                }
                if table[s] == 0 {
                    s += if s + 1 < table.len() && table[s + 1] == 0 { 2 } else { 1 };
                    if strings.is_empty() {
                        // Empty string set is a single double-NUL.
                        break;
                    }
                    if s > strings_start
                        && s >= 2
                        && table[s - 1] == 0
                        && table[s - 2] == 0
                    {
                        break;
                    }
                    continue;
                }
                let start = s;
                while s < table.len() && table[s] != 0 {
                    s += 1;
                }
                strings.push(&table[start..s]);
                s += 1;
                if s < table.len() && table[s] == 0 {
                    s += 1;
                    break;
                }
            }
            let next = s.max(strings_start + 2);
            let get_str = |idx: u8| -> String {
                if idx == 0 || idx as usize > strings.len() {
                    String::new()
                } else {
                    String::from_utf8_lossy(strings[idx as usize - 1])
                        .trim()
                        .to_string()
                }
            };

            if ty == 127 {
                break; // end-of-table
            }
            if ty == 17 && len >= 0x1B {
                let field_u16 =
                    |o: usize| u16::from_le_bytes([table[off + o], table[off + o + 1]]);
                let size_field = field_u16(0x0C);
                if size_field != 0 {
                    module_no += 1;
                    let mut section: Section = Vec::new();
                    section.push(("Module", format!("Module {module_no}")));
                    let locator = get_str(table[off + 0x10]);
                    if !locator.is_empty() {
                        section.push(("Slot", locator));
                    }
                    let bytes: u64 = if size_field == 0x7FFF && len >= 0x20 {
                        let ext = u32::from_le_bytes([
                            table[off + 0x1C],
                            table[off + 0x1D],
                            table[off + 0x1E],
                            table[off + 0x1F],
                        ]);
                        (ext as u64 & 0x7FFF_FFFF) * 1024 * 1024
                    } else if size_field & 0x8000 != 0 {
                        (size_field as u64 & 0x7FFF) * 1024
                    } else {
                        size_field as u64 * 1024 * 1024
                    };
                    section.push(("Capacity", fmt_bytes(bytes)));
                    if len >= 0x17 {
                        let speed = field_u16(0x15);
                        if speed != 0 {
                            section.push(("Speed", format!("{speed} MT/s")));
                        }
                    }
                    let mem_type = match table[off + 0x12] {
                        0x1A => "DDR4",
                        0x1E => "LPDDR4",
                        0x22 => "DDR5",
                        0x23 => "LPDDR5",
                        0x18 => "DDR3",
                        _ => "",
                    };
                    if !mem_type.is_empty() {
                        section.push(("Type", mem_type.to_string()));
                    }
                    if len >= 0x1B {
                        let manufacturer = get_str(table[off + 0x17]);
                        if !manufacturer.is_empty() {
                            section.push(("Manufacturer", manufacturer));
                        }
                        let part = get_str(table[off + 0x1A]);
                        if !part.is_empty() {
                            section.push(("Part Number", part));
                        }
                    }
                    out.push(section);
                }
            }
            off = next;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_information_populates() {
        let info = query_system_information();
        assert!(!info.os.is_empty());
        assert!(info.os.iter().any(|(k, v)| *k == "Build" && !v.is_empty()));
        assert!(!info.disks.is_empty());
        assert!(!info.modules.is_empty(), "expected SMBIOS memory modules");
    }
}
