//! Windows services enumeration — unelevated. EnumServicesStatusExW gives
//! name/display/state/pid in one call; startup type, account, and image path
//! come from one QueryServiceConfigW per service. The whole sweep is a few
//! milliseconds for ~300 services, so the UI refreshes it only while the
//! Services tab is visible.

use std::sync::Arc;

use windows_sys::Win32::System::Services::{
    CloseServiceHandle, EnumServicesStatusExW, OpenSCManagerW, OpenServiceW,
    QueryServiceConfigW, ENUM_SERVICE_STATUS_PROCESSW, QUERY_SERVICE_CONFIGW,
    SC_ENUM_PROCESS_INFO, SC_MANAGER_CONNECT, SC_MANAGER_ENUMERATE_SERVICE,
    SERVICE_QUERY_CONFIG, SERVICE_RUNNING, SERVICE_STATE_ALL, SERVICE_WIN32,
};

#[derive(Clone, Debug, Default)]
pub struct ServiceInfo {
    pub name: Arc<str>,
    pub display: Arc<str>,
    pub running: bool,
    pub pid: u32,
    /// 2 = Automatic, 3 = Manual, 4 = Disabled (SERVICE_*_START); 2 with
    /// delayed flag shows as Automatic in the UI like Task Manager.
    pub startup: u32,
    pub user: Arc<str>,
    pub path: Arc<str>,
}

impl ServiceInfo {
    pub fn startup_label(&self) -> &'static str {
        match self.startup {
            0 | 1 | 2 => "Automatic",
            3 => "Manual",
            4 => "Disabled",
            _ => "",
        }
    }
}

fn wide_to_arc(ptr: *const u16) -> Arc<str> {
    if ptr.is_null() {
        return Arc::from("");
    }
    unsafe {
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        Arc::from(String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len)))
    }
}

/// Enumerate all Win32 services. Returns an empty vec on failure (e.g. an
/// environment where the SCM is unreachable) rather than erroring.
pub fn query_services() -> Vec<ServiceInfo> {
    let mut out = Vec::new();
    unsafe {
        let scm = OpenSCManagerW(
            std::ptr::null(),
            std::ptr::null(),
            SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE,
        );
        if scm.is_null() {
            return out;
        }

        // Two-call pattern: size probe, then fill.
        let mut needed = 0u32;
        let mut count = 0u32;
        EnumServicesStatusExW(
            scm,
            SC_ENUM_PROCESS_INFO,
            SERVICE_WIN32,
            SERVICE_STATE_ALL,
            std::ptr::null_mut(),
            0,
            &mut needed,
            &mut count,
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if needed == 0 {
            CloseServiceHandle(scm);
            return out;
        }
        let mut buf = vec![0u8; needed as usize];
        if EnumServicesStatusExW(
            scm,
            SC_ENUM_PROCESS_INFO,
            SERVICE_WIN32,
            SERVICE_STATE_ALL,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut needed,
            &mut count,
            std::ptr::null_mut(),
            std::ptr::null(),
        ) == 0
        {
            CloseServiceHandle(scm);
            return out;
        }

        let entries =
            std::slice::from_raw_parts(buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW, count as usize);
        out.reserve(entries.len());
        // Startup type / account / path change rarely; one OpenService +
        // QueryServiceConfig per service dominates the sweep, so cache the
        // config per service name for the process lifetime. Status and pid
        // come from the (single-call) enumeration and stay fresh.
        static CONFIG_CACHE: std::sync::Mutex<
            Option<rustc_hash::FxHashMap<Arc<str>, (u32, Arc<str>, Arc<str>)>>,
        > = std::sync::Mutex::new(None);
        let mut cache_guard = CONFIG_CACHE.lock().unwrap();
        let cache = cache_guard.get_or_insert_with(Default::default);
        let mut cfg_buf = vec![0u8; 8192];
        for e in entries {
            let mut info = ServiceInfo {
                name: wide_to_arc(e.lpServiceName),
                display: wide_to_arc(e.lpDisplayName),
                running: e.ServiceStatusProcess.dwCurrentState == SERVICE_RUNNING,
                pid: e.ServiceStatusProcess.dwProcessId,
                ..Default::default()
            };
            if let Some((startup, user, path)) = cache.get(&info.name) {
                info.startup = *startup;
                info.user = user.clone();
                info.path = path.clone();
            } else {
                let svc = OpenServiceW(scm, e.lpServiceName, SERVICE_QUERY_CONFIG);
                if !svc.is_null() {
                    let mut cfg_needed = 0u32;
                    if QueryServiceConfigW(
                        svc,
                        cfg_buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW,
                        cfg_buf.len() as u32,
                        &mut cfg_needed,
                    ) != 0
                    {
                        let cfg = &*(cfg_buf.as_ptr() as *const QUERY_SERVICE_CONFIGW);
                        info.startup = cfg.dwStartType;
                        info.user = wide_to_arc(cfg.lpServiceStartName);
                        info.path = wide_to_arc(cfg.lpBinaryPathName);
                        cache.insert(
                            info.name.clone(),
                            (info.startup, info.user.clone(), info.path.clone()),
                        );
                    }
                    CloseServiceHandle(svc);
                }
            }
            out.push(info);
        }
        CloseServiceHandle(scm);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_enumerate() {
        let services = query_services();
        assert!(services.len() > 20, "expected a real service list");
        assert!(services.iter().any(|s| s.running && s.pid != 0));
        assert!(services.iter().any(|s| !s.path.is_empty()));
    }
}
