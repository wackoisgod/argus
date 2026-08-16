//! Parallel, cached per-process enrichment: user name and executable
//! description. Both require opening a handle per process (plus LSA lookups
//! and version-resource parsing), which is far too slow to do serially on the
//! sampler tick. Resolution fans out across a small below-normal-priority
//! rayon pool; results are cached for the process lifetime and the sampler
//! only ever does a non-blocking cache lookup.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    GetTokenInformation, LookupAccountSidW, TokenUser, SID_NAME_USE, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::RemoteDesktop::{
    WTSEnumerateProcessesW, WTSFreeMemory, WTS_PROCESS_INFOW,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, QueryDosDeviceW, VerQueryValueW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    SetThreadPriority, PROCESS_QUERY_LIMITED_INFORMATION, THREAD_PRIORITY_BELOW_NORMAL,
};

#[derive(Debug)]
pub struct Enriched {
    pub user: Arc<str>,
    pub description: Arc<str>,
}

type Key = (u32, i64); // (pid, create_time) — stable across pid reuse

/// Pid→user map from WTS enumeration. Works unelevated for every process
/// (including services), unlike opening each process's token.
#[derive(Default)]
struct WtsUsers {
    refreshed: Option<Instant>,
    users: HashMap<u32, Arc<str>>,
}

pub struct Enricher {
    /// `None` marks an in-flight resolution so it is scheduled exactly once.
    cache: Arc<Mutex<HashMap<Key, Option<Arc<Enriched>>>>>,
    wts: Arc<Mutex<WtsUsers>>,
    pool: rayon::ThreadPool,
}

impl Enricher {
    pub fn new() -> Self {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .thread_name(|i| format!("enrich-{i}"))
            .start_handler(|_| unsafe {
                // Enrichment is cosmetic; never compete with foreground work.
                SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
            })
            .build()
            .expect("build enrich pool");
        Enricher {
            cache: Arc::new(Mutex::new(HashMap::new())),
            wts: Arc::new(Mutex::new(WtsUsers::default())),
            pool,
        }
    }

    /// Non-blocking: returns cached info when resolved; on first sight of a
    /// process, schedules resolution on the pool and returns `None`.
    pub fn get_or_schedule(&self, key: Key) -> Option<Arc<Enriched>> {
        let mut cache = self.cache.lock().unwrap();
        match cache.get(&key) {
            Some(v) => v.clone(),
            None => {
                cache.insert(key, None);
                drop(cache);
                let cache = Arc::clone(&self.cache);
                let wts = Arc::clone(&self.wts);
                self.pool.spawn(move || {
                    let info = Arc::new(resolve(key.0, &wts));
                    cache.lock().unwrap().insert(key, Some(info));
                });
                None
            }
        }
    }

    /// Drop cache entries for processes that no longer exist.
    pub fn retain(&self, live: impl Fn(&Key) -> bool) {
        self.cache.lock().unwrap().retain(|k, _| live(k));
    }
}

impl Default for Enricher {
    fn default() -> Self {
        Self::new()
    }
}

fn resolve(pid: u32, wts: &Mutex<WtsUsers>) -> Enriched {
    let mut user: Arc<str> = Arc::from("");
    let mut description: Arc<str> = Arc::from("");
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if !handle.is_null() {
            let mut path = [0u16; 1024];
            let mut len = path.len() as u32;
            if QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut len) != 0 && len > 0
            {
                if let Some(d) = file_description(&path[..len as usize]) {
                    description = d;
                }
            }
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(handle, TOKEN_QUERY, &mut token) != 0 {
                if let Some(u) = token_user(token) {
                    user = u;
                }
                CloseHandle(token);
            }
            CloseHandle(handle);
        }
    }
    // Protected/service processes refuse OpenProcess unelevated, but the
    // kernel will still tell us their image path without a handle; version
    // resources are world-readable, so the description works for everyone.
    if description.is_empty() {
        if let Some(dos) = crate::nt::image_nt_path(pid).and_then(|nt| nt_path_to_dos(&nt)) {
            if let Some(d) = unsafe { file_description(&dos) } {
                description = d;
            }
        }
    }
    // Service/system processes refuse token access unelevated; the WTS
    // enumeration still knows their user (SYSTEM, LOCAL SERVICE, ...).
    if user.is_empty() {
        if let Some(u) = wts_user(pid, wts) {
            user = u;
        }
    }
    Enriched { user, description }
}

/// Translate "\Device\HarddiskVolumeN\..." to "C:\..." using the dos-device
/// mappings; version APIs don't accept NT paths.
fn nt_path_to_dos(nt: &[u16]) -> Option<Vec<u16>> {
    use std::sync::OnceLock;
    static DEVICES: OnceLock<Vec<(Vec<u16>, [u16; 2])>> = OnceLock::new();
    let devices = DEVICES.get_or_init(|| {
        let mut out = Vec::new();
        let mut target = [0u16; 512];
        for letter in b'A'..=b'Z' {
            let drive = [letter as u16, b':' as u16, 0];
            let n = unsafe { QueryDosDeviceW(drive.as_ptr(), target.as_mut_ptr(), 512) };
            if n > 0 {
                let end = target.iter().position(|&c| c == 0).unwrap_or(0);
                if end > 0 {
                    out.push((target[..end].to_vec(), [letter as u16, b':' as u16]));
                }
            }
        }
        out
    });
    fn fold(c: u16) -> u16 {
        if (b'A' as u16..=b'Z' as u16).contains(&c) {
            c + 32
        } else {
            c
        }
    }
    for (device, drive) in devices {
        if nt.len() > device.len()
            && nt[..device.len()]
                .iter()
                .zip(device.iter())
                .all(|(a, b)| fold(*a) == fold(*b))
            && nt[device.len()] == b'\\' as u16
        {
            let mut dos = drive.to_vec();
            dos.extend_from_slice(&nt[device.len()..]);
            return Some(dos);
        }
    }
    None
}

fn wts_user(pid: u32, wts: &Mutex<WtsUsers>) -> Option<Arc<str>> {
    let mut wts = wts.lock().unwrap();
    let stale = wts
        .refreshed
        .map(|t| t.elapsed().as_secs() >= 5)
        .unwrap_or(true);
    if stale || !wts.users.contains_key(&pid) {
        if stale {
            wts.refreshed = Some(Instant::now());
            unsafe { wts_refresh(&mut wts.users) };
        }
    }
    wts.users.get(&pid).cloned()
}

unsafe fn wts_refresh(map: &mut HashMap<u32, Arc<str>>) {
    let mut info: *mut WTS_PROCESS_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    // WTS_CURRENT_SERVER_HANDLE = null
    if WTSEnumerateProcessesW(std::ptr::null_mut(), 0, 1, &mut info, &mut count) == 0 {
        return;
    }
    map.clear();
    for i in 0..count as usize {
        let entry = &*info.add(i);
        if entry.pUserSid.is_null() {
            continue;
        }
        if let Some(name) = sid_to_name(entry.pUserSid) {
            map.insert(entry.ProcessId, name);
        }
    }
    WTSFreeMemory(info.cast());
}

/// Read FileDescription from the executable's version resource.
unsafe fn file_description(path: &[u16]) -> Option<Arc<str>> {
    let mut pathz = path.to_vec();
    pathz.push(0);
    let mut ignored = 0u32;
    let size = GetFileVersionInfoSizeW(pathz.as_ptr(), &mut ignored);
    if size == 0 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    if GetFileVersionInfoW(pathz.as_ptr(), 0, size, data.as_mut_ptr().cast()) == 0 {
        return None;
    }

    let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut vlen = 0u32;
    // Prefer the resource's own first translation; fall back to en-US.
    let (mut lang, mut cp) = (0x0409u16, 0x04B0u16);
    let tq: Vec<u16> = "\\VarFileInfo\\Translation".encode_utf16().chain([0]).collect();
    if VerQueryValueW(data.as_ptr().cast(), tq.as_ptr(), &mut ptr, &mut vlen) != 0
        && !ptr.is_null()
        && vlen >= 4
    {
        let words = ptr as *const u16;
        lang = *words;
        cp = *words.add(1);
    }
    let q = format!("\\StringFileInfo\\{lang:04X}{cp:04X}\\FileDescription");
    let qw: Vec<u16> = q.encode_utf16().chain([0]).collect();
    if VerQueryValueW(data.as_ptr().cast(), qw.as_ptr(), &mut ptr, &mut vlen) == 0
        || ptr.is_null()
        || vlen == 0
    {
        return None;
    }
    let slice = std::slice::from_raw_parts(ptr as *const u16, vlen as usize);
    let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    let s = String::from_utf16_lossy(&slice[..end]);
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(Arc::from(s))
    }
}

/// Resolve the token's user SID to an account name.
unsafe fn token_user(token: HANDLE) -> Option<Arc<str>> {
    let mut needed = 0u32;
    GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
    if needed == 0 {
        return None;
    }
    let mut buf = vec![0u8; needed as usize];
    if GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), needed, &mut needed) == 0 {
        return None;
    }
    let sid = (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid;
    sid_to_name(sid)
}

unsafe fn sid_to_name(sid: *mut core::ffi::c_void) -> Option<Arc<str>> {
    let mut name = [0u16; 256];
    let mut name_len = name.len() as u32;
    let mut domain = [0u16; 256];
    let mut domain_len = domain.len() as u32;
    let mut sid_use: SID_NAME_USE = 0;
    if LookupAccountSidW(
        std::ptr::null(),
        sid,
        name.as_mut_ptr(),
        &mut name_len,
        domain.as_mut_ptr(),
        &mut domain_len,
        &mut sid_use,
    ) == 0
    {
        return None;
    }
    Some(Arc::from(String::from_utf16_lossy(
        &name[..name_len as usize],
    )))
}
