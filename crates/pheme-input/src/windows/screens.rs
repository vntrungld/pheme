//! Monitor enumeration and DPI awareness.

use std::sync::Once;

use pheme_proto::ScreenInfo;
use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

/// Makes the process per-monitor DPI aware so all coordinates are physical pixels.
/// Safe to call repeatedly; only the first call has an effect.
pub fn ensure_dpi_aware() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        // Fails if the manifest already set awareness; that is fine.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    });
}

unsafe extern "system" fn monitor_cb(
    hmon: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let out = &mut *(data.0 as *mut Vec<ScreenInfo>);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(hmon, &mut info).as_bool() {
        let r = info.rcMonitor;
        out.push(ScreenInfo {
            x: r.left,
            y: r.top,
            w: (r.right - r.left) as u32,
            h: (r.bottom - r.top) as u32,
            primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
    }
    BOOL(1)
}

pub fn enum_screens() -> Vec<ScreenInfo> {
    ensure_dpi_aware();
    let mut out: Vec<ScreenInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(monitor_cb),
            LPARAM(&mut out as *mut _ as isize),
        );
    }
    if out.is_empty() {
        out.push(ScreenInfo {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
            primary: true,
        });
    }
    out
}
