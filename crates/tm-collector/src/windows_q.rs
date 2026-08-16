//! Which pids own a visible, titled, unowned top-level window — the
//! Task Manager "Apps" criterion.

use rustc_hash::FxHashSet;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible,
    GW_OWNER,
};

pub fn pids_with_visible_windows() -> FxHashSet<u32> {
    let mut pids = FxHashSet::default();
    unsafe extern "system" fn cb(hwnd: HWND, lparam: isize) -> i32 {
        if IsWindowVisible(hwnd) != 0
            && GetWindow(hwnd, GW_OWNER).is_null()
            && GetWindowTextLengthW(hwnd) > 0
        {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid != 0 {
                (*(lparam as *mut FxHashSet<u32>)).insert(pid);
            }
        }
        1
    }
    unsafe {
        let _ = EnumWindows(Some(cb), &mut pids as *mut FxHashSet<u32> as isize);
    }
    pids
}
