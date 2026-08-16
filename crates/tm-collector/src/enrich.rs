//! Parallel, cached per-process enrichment: user name and executable
//! description. Both require opening a handle per process (plus LSA lookups
//! and version-resource parsing), which is far too slow to do serially on the
//! sampler tick. Resolution fans out across a small below-normal-priority
//! rayon pool; results are cached for the process lifetime and the sampler
//! only ever does a non-blocking cache lookup.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    GetTokenInformation, LookupAccountSidW, TokenUser, SID_NAME_USE, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
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

pub struct Enricher {
    /// `None` marks an in-flight resolution so it is scheduled exactly once.
    cache: Arc<Mutex<HashMap<Key, Option<Arc<Enriched>>>>>,
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
                self.pool.spawn(move || {
                    let info = Arc::new(resolve(key.0));
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

fn resolve(pid: u32) -> Enriched {
    let mut user: Arc<str> = Arc::from("");
    let mut description: Arc<str> = Arc::from("");
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return Enriched { user, description };
        }
        let mut path = [0u16; 1024];
        let mut len = path.len() as u32;
        if QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut len) != 0 && len > 0 {
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
    Enriched { user, description }
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
