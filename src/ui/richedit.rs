use wxdragon::prelude::*;

/// Stops a rich edit control beeping when the cursor can't move (e.g. Down on the last line).
/// Turns off `TXTBIT_ALLOWBEEP` through the control's `ITextServices`, obtained by querying
/// the `IRichEditOle` from `EM_GETOLEINTERFACE`. Does nothing outside Windows.
#[cfg(windows)]
pub fn disable_beep(ctrl: &TextCtrl) {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::Controls::RichEdit::{EM_GETOLEINTERFACE, ITextServices, TXTBIT_ALLOWBEEP};
    use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
    use windows::core::{GUID, IUnknown, Interface};

    // The Windows metadata has no IID for ITextServices; this is the one from textserv.h.
    const IID_ITEXTSERVICES: GUID = GUID::from_u128(0x8d33f740_cf58_11ce_a89d_00aa006cadc5);

    let hwnd = HWND(ctrl.get_handle());
    if hwnd.is_invalid() {
        return;
    }
    unsafe {
        let mut ole: *mut std::ffi::c_void = std::ptr::null_mut();
        let ok = SendMessageW(hwnd, EM_GETOLEINTERFACE, Some(WPARAM(0)), Some(LPARAM(&mut ole as *mut _ as isize)));
        if ok.0 == 0 || ole.is_null() {
            return;
        }
        // EM_GETOLEINTERFACE hands back an AddRef'd pointer; from_raw takes ownership.
        let ole = IUnknown::from_raw(ole);
        let mut services: *mut std::ffi::c_void = std::ptr::null_mut();
        if ole.query(&IID_ITEXTSERVICES, &mut services).is_err() || services.is_null() {
            return;
        }
        let services = ITextServices::from_raw(services);
        let _ = services.OnTxPropertyBitsChange(TXTBIT_ALLOWBEEP, 0);
    }
}

#[cfg(not(windows))]
pub fn disable_beep(_ctrl: &TextCtrl) {}
