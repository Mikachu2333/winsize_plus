use crate::{config, desktop};
use std::ptr::null_mut;
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    UI::{HiDpi::*, WindowsAndMessaging::*},
};

pub struct Dpi(DPI_AWARENESS_CONTEXT);
impl Dpi {
    pub fn new() -> Self {
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}
impl Drop for Dpi {
    fn drop(&mut self) {
        unsafe {
            SetThreadDpiAwarenessContext(self.0);
        }
    }
}
pub fn displays() -> Vec<(usize, desktop::Display)> {
    unsafe extern "system" fn visit(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> i32 {
        unsafe {
            (&mut *(data as *mut Vec<(usize, desktop::Display)>))
                .push((m as usize, desktop::display(m).unwrap()));
        }
        1
    }
    let mut result = Vec::<(usize, desktop::Display)>::new();
    assert_ne!(
        unsafe {
            EnumDisplayMonitors(
                null_mut(),
                std::ptr::null(),
                Some(visit),
                (&mut result as *mut _) as isize,
            )
        },
        0
    );
    result.sort_by_key(|(_, display)| display.dpi);
    // Exercise the single-monitor skip path without changing the contributor's desktop.
    if std::env::var_os("WINSIZEP_TEST_SINGLE_MONITOR").is_some() {
        result.truncate(1);
    }
    result
}
pub fn require_displays(mixed_dpi: bool) -> Option<Vec<(usize, desktop::Display)>> {
    let result = displays();
    if result.len() < 2 || (mixed_dpi && result.first()?.1.dpi == result.last()?.1.dpi) {
        eprintln!(
            "SKIPPED: this case needs two {}monitors",
            if mixed_dpi { "different-DPI " } else { "" }
        );
        None
    } else {
        Some(result)
    }
}
pub fn cfg() -> config::Config {
    config::Config::parse(&config::help().parse().unwrap()).unwrap()
}
pub fn point(display: &desktop::Display) -> POINT {
    POINT {
        x: display.work.x + display.work.width / 2,
        y: display.work.y + display.work.height / 2,
    }
}
pub fn pump() {
    unsafe {
        let mut message = MSG::default();
        for _ in 0..200 {
            if PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) == 0 {
                break;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[test]
fn real_monitor_lookup_and_work_areas() {
    let _dpi = Dpi::new();
    for (_, d) in displays() {
        let actual = desktop::screen(point(&d), d.number).unwrap();
        println!(
            "display={} dpi={} full={:?} work={:?}",
            d.number, d.dpi, d.full, d.work
        );
        assert_eq!(actual.number, d.number);
        assert_eq!(actual.work, d.work);
        assert_eq!(actual.dpi, d.dpi);
    }
}

#[test]
fn missing_explicit_monitor_must_not_select_another_screen() {
    let _dpi = Dpi::new();
    let displays = displays();
    let missing = (1..1000)
        .find(|n| displays.iter().all(|(_, d)| d.number != *n))
        .unwrap();
    let result = desktop::screen(point(&displays[0].1), missing);
    if let Ok(ref d) = result {
        println!(
            "requested={missing}, returned={} work={:?}",
            d.number, d.work
        );
    }
    assert!(
        result.is_err(),
        "An unavailable explicit monitor silently resolves to the pointer monitor"
    );
}
