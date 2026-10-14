/// Whether one of irctest's windows (main frame or a dialog) is the foreground window.
/// Always true outside Windows.
#[cfg(windows)]
pub fn app_is_foreground() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return false;
        }
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pid == std::process::id()
    }
}

#[cfg(not(windows))]
pub fn app_is_foreground() -> bool {
    true
}
