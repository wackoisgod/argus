//! Which pids own an Alt-Tab-worthy top-level window — the Task Manager
//! "Apps" criterion. Mirrors the shell's rules: visible, unowned, not a
//! tool window (unless it opts back in with WS_EX_APPWINDOW), and not
//! DWM-cloaked (which is how UWP hosting windows like ApplicationFrameHost
//! and TextInputHost hide from Alt-Tab while staying "visible").

use rustc_hash::FxHashSet;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowLongPtrW, GetWindowThreadProcessId, IsWindowVisible,
    GWL_EXSTYLE, GW_OWNER, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

pub fn pids_with_visible_windows() -> FxHashSet<u32> {
    let mut pids = FxHashSet::default();
    unsafe extern "system" fn cb(hwnd: HWND, lparam: isize) -> i32 {
        if IsWindowVisible(hwnd) == 0 || !GetWindow(hwnd, GW_OWNER).is_null() {
            return 1;
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW != 0 && ex & WS_EX_APPWINDOW == 0 {
            return 1;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        );
        if cloaked != 0 {
            return 1;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != 0 {
            (*(lparam as *mut FxHashSet<u32>)).insert(pid);
        }
        1
    }
    unsafe {
        let _ = EnumWindows(Some(cb), &mut pids as *mut FxHashSet<u32> as isize);
    }
    pids
}
