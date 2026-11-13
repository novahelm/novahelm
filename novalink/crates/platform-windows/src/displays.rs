//! Monitor enumeration in physical pixels (the process is per-monitor-DPI aware v2).

use crate::logic::scale_from_dpi;
use novalink_proto::DisplayInfo;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::core::BOOL;

const MONITORINFOF_PRIMARY: u32 = 1;

unsafe extern "system" fn collect(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    // SAFETY: `data` is the `&mut Vec` passed by `list`, alive for the whole enumeration.
    let out = unsafe { &mut *(data.0 as *mut Vec<(bool, DisplayInfo)>) };
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: mi is correctly sized; h comes from the enumeration.
    if unsafe { GetMonitorInfoW(h, &mut mi) }.as_bool() {
        let (mut dx, mut dy) = (96u32, 96u32);
        // SAFETY: out-pointers are valid. On failure we keep 96 (scale 1.0).
        let _ = unsafe { GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
        let r = mi.rcMonitor;
        out.push((
            mi.dwFlags & MONITORINFOF_PRIMARY != 0,
            DisplayInfo {
                id: h.0 as usize as u32,
                x: r.left as f64,
                y: r.top as f64,
                width: (r.right - r.left) as f64,
                height: (r.bottom - r.top) as f64,
                scale: scale_from_dpi(dx),
            },
        ));
    }
    BOOL(1)
}

/// All attached monitors, primary first, then left-to-right / top-to-bottom.
pub fn displays() -> Vec<DisplayInfo> {
    let mut v: Vec<(bool, DisplayInfo)> = Vec::new();
    // SAFETY: the callback only dereferences `v`, which outlives the call.
    let _ = unsafe {
        EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut v as *mut _ as isize))
    };
    v.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(a.1.x.total_cmp(&b.1.x))
            .then(a.1.y.total_cmp(&b.1.y))
    });
    v.into_iter().map(|(_, d)| d).collect()
}
