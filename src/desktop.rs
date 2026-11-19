use crate::{
    cli::{Request, Selector},
    config::{self, Config, Identity},
    layout::Rect,
};
use std::{
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    path::Path,
    ptr::{from_mut, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::*},
    System::Threading::*,
    UI::{HiDpi::*, WindowsAndMessaging::*},
};

pub fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain(Some(0)).collect()
}
pub fn err() -> String {
    std::io::Error::last_os_error().to_string()
}
pub fn check(status: i32) -> Result<(), String> {
    if status == 0 { Err(err()) } else { Ok(()) }
}
fn rect(raw: RECT) -> Rect {
    Rect {
        x: raw.left,
        y: raw.top,
        width: raw.right.saturating_sub(raw.left),
        height: raw.bottom.saturating_sub(raw.top),
    }
}
pub fn cursor() -> Result<POINT, String> {
    let mut point = POINT::default();
    unsafe { check(GetCursorPos(&mut point))? };
    Ok(point)
}

pub struct Display {
    pub work: Rect,
    pub full: Rect,
    pub number: u32,
    pub dpi: i32,
}
pub fn display(monitor: HMONITOR) -> Result<Display, String> {
    unsafe {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        check(GetMonitorInfoW(monitor, &mut info.monitorInfo))?;
        let device = String::from_utf16_lossy(
            &info.szDevice[..info
                .szDevice
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(info.szDevice.len())],
        );
        let number = device
            .trim_start_matches(r"\\.\DISPLAY")
            .parse()
            .unwrap_or(1);
        let (mut dpi, mut second) = (96, 96);
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut second) < 0 {
            dpi = 96;
        }
        Ok(Display {
            work: rect(info.monitorInfo.rcWork),
            full: rect(info.monitorInfo.rcMonitor),
            number,
            dpi: dpi as i32,
        })
    }
}
struct SearchDisplay {
    wanted: u32,
    found: HMONITOR,
}
unsafe extern "system" fn enum_monitor(
    monitor: HMONITOR,
    _: HDC,
    _: *mut RECT,
    data: LPARAM,
) -> i32 {
    let search = unsafe { &mut *(data as *mut SearchDisplay) };
    if display(monitor).is_ok_and(|screen| screen.number == search.wanted) {
        search.found = monitor;
        0
    } else {
        1
    }
}
pub fn screen(point: POINT, wanted: u32) -> Result<Display, String> {
    unsafe {
        let mut search = SearchDisplay {
            wanted,
            found: null_mut(),
        };
        if wanted != 0 {
            EnumDisplayMonitors(
                null_mut(),
                std::ptr::null(),
                Some(enum_monitor),
                from_mut(&mut search) as isize,
            );
        }
        display(if search.found.is_null() {
            MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST)
        } else {
            search.found
        })
    }
}
pub fn desktop() -> Rect {
    unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

fn pid(hwnd: HWND) -> u32 {
    let mut id = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut id) };
    id
}
fn class(hwnd: HWND) -> String {
    let mut buf = [0; 256];
    let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..len as usize])
}
fn title(hwnd: HWND) -> String {
    unsafe {
        let mut buf = vec![0; GetWindowTextLengthW(hwnd).clamp(0, 32767) as usize + 1];
        let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..len as usize])
    }
}
fn name(hwnd: HWND) -> String {
    process_path(hwnd)
        .rsplit('\\')
        .next()
        .unwrap_or_default()
        .into()
}
fn process_path(hwnd: HWND) -> String {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid(hwnd));
        if process.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(process);
        if ok == 0 {
            String::new()
        } else {
            String::from_utf16_lossy(&buf[..len as usize])
        }
    }
}
pub fn window_text(hwnd: HWND, format: &str) -> String {
    let id = identity(hwnd);
    let display_name = if format.contains("{display_name}") {
        process_description(hwnd).unwrap_or_else(|| id.name.clone())
    } else {
        String::new()
    };
    // Parse placeholders once so a window title containing braces remains literal.
    let values = [
        ("hwnd", format!("0x{:X}", hwnd as usize)),
        ("title", id.title),
        ("pid", pid(hwnd).to_string()),
        ("process", id.name),
        ("display_name", display_name),
        ("class", id.class),
    ];
    let mut result = String::new();
    let mut rest = format;
    while let Some(start) = rest.find('{') {
        result.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            result.push_str(&rest[start..]);
            return result;
        };
        let token = &rest[start + 1..start + end];
        if let Some((_, value)) = values.iter().find(|(key, _)| *key == token) {
            result.push_str(value);
        } else {
            result.push_str(&rest[start..=start + end]);
        }
        rest = &rest[start + end + 1..];
    }
    result.push_str(rest);
    result
}
fn process_description(hwnd: HWND) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::*;
    let path = wide(process_path(hwnd));
    unsafe {
        let mut ignored = 0;
        let size = GetFileVersionInfoSizeW(path.as_ptr(), &mut ignored);
        if size == 0 || size > 16 * 1024 * 1024 {
            return None;
        }
        // Typed storage guarantees UTF-16 alignment independently of the allocator.
        let mut data = vec![0u16; (size as usize).div_ceil(2)];
        if GetFileVersionInfoW(path.as_ptr(), 0, size, data.as_mut_ptr().cast()) == 0 {
            return None;
        }
        let mut pointer = null_mut();
        let mut length = 0;
        if VerQueryValueW(
            data.as_ptr().cast(),
            wide("\\VarFileInfo\\Translation").as_ptr(),
            &mut pointer,
            &mut length,
        ) == 0
            || length < 4
        {
            return None;
        }
        let pair = version_units(&data, size as usize, pointer.cast(), 2)?;
        let query = wide(format!(
            "\\StringFileInfo\\{:04x}{:04x}\\FileDescription",
            pair[0], pair[1]
        ));
        if VerQueryValueW(
            data.as_ptr().cast(),
            query.as_ptr(),
            &mut pointer,
            &mut length,
        ) == 0
            || pointer.is_null()
            || length == 0
        {
            return None;
        }
        Some(
            String::from_utf16_lossy(version_units(
                &data,
                size as usize,
                pointer.cast(),
                length as usize,
            )?)
            .trim_end_matches('\0')
            .to_owned(),
        )
    }
}
fn version_units(
    storage: &[u16],
    bytes: usize,
    pointer: *const u16,
    length: usize,
) -> Option<&[u16]> {
    let start = pointer.addr().checked_sub(storage.as_ptr().addr())?;
    let count = length.checked_mul(2)?;
    if start % 2 != 0 || start.checked_add(count)? > bytes {
        return None;
    }
    // Return a slice through the original allocation rather than dereferencing
    // a foreign pointer. Null/out-of-buffer/misaligned results are rejected.
    storage.get(start / 2..start / 2 + length)
}
pub fn time_text(format: &str) -> String {
    let mut time = SYSTEMTIME::default();
    unsafe {
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut time);
    }
    format
        .replace("YYYY", &format!("{:04}", time.wYear))
        .replace("MM", &format!("{:02}", time.wMonth))
        .replace("DD", &format!("{:02}", time.wDay))
        .replace("hh", &format!("{:02}", time.wHour))
        .replace("mm", &format!("{:02}", time.wMinute))
        .replace("ss", &format!("{:02}", time.wSecond))
}
pub fn identity(hwnd: HWND) -> Identity {
    Identity {
        name: name(hwnd),
        class: class(hwnd),
        title: title(hwnd),
    }
}
fn eligible(hwnd: HWND) -> bool {
    unsafe {
        if hwnd.is_null()
            || IsWindowVisible(hwnd) == 0
            || GetAncestor(hwnd, GA_ROOT) != hwnd
            || hwnd == GetDesktopWindow()
            || hwnd == GetShellWindow()
            || pid(hwnd) == std::process::id()
            // Settings run in a separate process; owned native dialogs share its root owner.
            || class(GetAncestor(hwnd, GA_ROOTOWNER)) == "WinSizeP.Settings"
        {
            return false;
        }
        !matches!(
            class(hwnd).as_str(),
            "Progman"
                | "WorkerW"
                | "Shell_TrayWnd"
                | "Shell_SecondaryTrayWnd"
                | "NotifyIconOverflowWindow"
                | "WinSizeP.Overlay"
        )
    }
}
pub fn allowed(hwnd: HWND, exclusions: &[Identity]) -> bool {
    eligible(hwnd)
        && (exclusions.is_empty() || !exclusions.iter().any(|rule| rule.matches(&identity(hwnd))))
}
fn matches(hwnd: HWND, selector: &Selector) -> bool {
    eligible(hwnd)
        && selector.pid.is_none_or(|id| id == pid(hwnd))
        && selector
            .class
            .as_ref()
            .is_none_or(|text| *text == class(hwnd))
        && selector
            .title
            .as_ref()
            .is_none_or(|text| *text == title(hwnd))
        && selector
            .name
            .as_ref()
            .is_none_or(|text| text.eq_ignore_ascii_case(&name(hwnd)))
}
struct SearchWindow<'a> {
    selector: &'a Selector,
    found: HWND,
}
unsafe extern "system" fn enum_window(hwnd: HWND, data: LPARAM) -> i32 {
    let search = unsafe { &mut *(data as *mut SearchWindow<'_>) };
    if matches(hwnd, search.selector) {
        search.found = hwnd;
        0
    } else {
        1
    }
}
pub fn target(selector: &Selector) -> Result<HWND, String> {
    unsafe {
        let direct = selector.hwnd.or_else(|| {
            (selector.pid.is_none()
                && selector.name.is_none()
                && selector.class.is_none()
                && selector.title.is_none())
            .then_some(0)
        });
        let found = if let Some(handle) = direct {
            let hwnd = if handle == 0 {
                GetForegroundWindow()
            } else {
                handle as HWND
            };
            if matches(hwnd, selector) {
                hwnd
            } else {
                null_mut()
            }
        } else {
            let mut search = SearchWindow {
                selector,
                found: null_mut(),
            };
            EnumWindows(Some(enum_window), from_mut(&mut search) as isize);
            search.found
        };
        if found.is_null() {
            Err("No matching window / 没有匹配的窗口".into())
        } else {
            Ok(found)
        }
    }
}

pub fn bounds(hwnd: HWND) -> Result<(Rect, Rect), String> {
    unsafe {
        let mut outer = RECT::default();
        check(GetWindowRect(hwnd, &mut outer))?;
        let mut frame = outer;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            from_mut(&mut frame).cast(),
            size_of::<RECT>() as u32,
        ) < 0
            || frame.right <= frame.left
            || frame.bottom <= frame.top
        {
            frame = outer
        }
        Ok((rect(outer), rect(frame)))
    }
}

pub struct Drag {
    hwnd: HWND,
    origin: POINT,
    frame: Rect,
    resize: bool,
    minimum: (i32, i32),
}
fn restore(hwnd: HWND) -> Result<(), String> {
    // SAFETY: HWND is validated by the caller. Posting a restore never waits for
    // the target's window procedure; polling only queries window-manager state.
    unsafe {
        if IsZoomed(hwnd) == 0 && IsIconic(hwnd) == 0 {
            return Ok(());
        }
        check(ShowWindowAsync(hwnd, SW_RESTORE))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        while IsZoomed(hwnd) != 0 || IsIconic(hwnd) != 0 {
            if std::time::Instant::now() >= deadline {
                return Err("Window restore timed out / 窗口还原超时".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    Ok(())
}
impl Drag {
    pub fn start(hwnd: HWND, origin: POINT, resize: bool, cfg: &Config) -> Result<Self, String> {
        if !allowed(hwnd, &cfg.exclusions) {
            return Err(cfg.tr("Window unavailable", "窗口不可操作").into());
        }
        unsafe {
            if IsHungAppWindow(hwnd) != 0 {
                return Err(cfg.tr("Window unresponsive", "窗口无响应").into());
            }
            restore(hwnd)?;
            let (outer, frame) = bounds(hwnd)?;
            let mut limits = MINMAXINFO::default();
            let mut result = 0;
            SendMessageTimeoutW(
                hwnd,
                WM_GETMINMAXINFO,
                0,
                from_mut(&mut limits) as isize,
                SMTO_ABORTIFHUNG,
                100,
                &mut result,
            );
            let minimum = (
                (limits.ptMinTrackSize.x - (outer.width - frame.width)).max(1),
                (limits.ptMinTrackSize.y - (outer.height - frame.height)).max(1),
            );
            Ok(Self {
                hwnd,
                origin,
                frame,
                resize,
                minimum,
            })
        }
    }
    pub fn follow(&self, point: POINT) -> Result<(), String> {
        let wanted = drag_rect(self.frame, self.origin, point, self.resize, self.minimum);
        let (outer, frame) = bounds(self.hwnd)?;
        if wanted != frame {
            set_frame(self.hwnd, wanted, outer, frame)?;
        }
        Ok(())
    }
}
fn drag_rect(frame: Rect, origin: POINT, point: POINT, resize: bool, minimum: (i32, i32)) -> Rect {
    let dx = point.x.saturating_sub(origin.x);
    let dy = point.y.saturating_sub(origin.y);
    if resize {
        Rect {
            width: frame.width.saturating_add(dx).max(minimum.0),
            height: frame.height.saturating_add(dy).max(minimum.1),
            ..frame
        }
    } else {
        Rect {
            x: frame.x.saturating_add(dx),
            y: frame.y.saturating_add(dy),
            ..frame
        }
    }
}
fn position(hwnd: HWND, req: &Request, cfg: &Config, point: POINT) -> Result<(), String> {
    unsafe {
        let preset = req
            .preset
            .as_ref()
            .map(|key| {
                cfg.preset(key).ok_or_else(|| {
                    format!(
                        "Unknown preset or key without a layout / 未知预设或按键未绑定布局: {key}"
                    )
                })
            })
            .transpose()?;
        if IsHungAppWindow(hwnd) != 0 {
            return Err("Window unresponsive / 窗口无响应".into());
        }
        restore(hwnd)?;
        let (outer, frame) = bounds(hwnd)?;
        let base = if let Some(layout) = preset {
            layout.resolve(screen(point, layout.monitor)?.work, frame, desktop())?
        } else {
            frame
        };
        let (width, height) = req.size.unwrap_or((base.width, base.height));
        let wanted = Rect {
            x: req.x.unwrap_or(base.x),
            y: req.y.unwrap_or(base.y),
            width,
            height,
        };
        let dpi = GetDpiForWindow(hwnd);
        set_frame(hwnd, wanted, outer, frame)?;
        // Cross-thread positioning is asynchronous. Give the target a bounded
        // opportunity to update its DPI before correcting its nonclient frame.
        let until = std::time::Instant::now() + std::time::Duration::from_millis(100);
        while std::time::Instant::now() < until && GetDpiForWindow(hwnd) == dpi {
            if bounds(hwnd).is_ok_and(|(_, current)| current != frame || current == wanted) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        if dpi != 0 && GetDpiForWindow(hwnd) != dpi {
            let (outer, frame) = bounds(hwnd)?;
            if frame != wanted {
                set_frame(hwnd, wanted, outer, frame)?;
            }
        }
        Ok(())
    }
}

fn set_frame(hwnd: HWND, wanted: Rect, outer: Rect, frame: Rect) -> Result<(), String> {
    let overflow = "Invalid rectangle / 矩形越界";
    let offset = |value: i32, raw: i32, visible: i32| {
        i32::try_from(i64::from(value) + i64::from(raw) - i64::from(visible)).map_err(|_| overflow)
    };
    if wanted.width <= 0 || wanted.height <= 0 {
        return Err(overflow.into());
    }
    let x = offset(wanted.x, outer.x, frame.x)?;
    let y = offset(wanted.y, outer.y, frame.y)?;
    let width = offset(wanted.width, outer.width, frame.width)?;
    let height = offset(wanted.height, outer.height, frame.height)?;
    if width <= 0
        || height <= 0
        || x.checked_add(width).is_none()
        || y.checked_add(height).is_none()
    {
        return Err(overflow.into());
    }
    unsafe {
        check(SetWindowPos(
            hwnd,
            null_mut(),
            x,
            y,
            width,
            height,
            SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER,
        ))
    }
}
pub fn apply(
    hwnd: HWND,
    req: &Request,
    cfg: &Config,
    path: &Path,
    point: POINT,
) -> Result<(), String> {
    if !eligible(hwnd) {
        return Err(cfg.tr("No window", "无可操作的窗口").into());
    }
    let id = (req.ignore.is_some() || !cfg.exclusions.is_empty()).then(|| identity(hwnd));
    let excluded = id
        .as_ref()
        .is_some_and(|id| cfg.exclusions.iter().any(|rule| rule.matches(id)));
    if excluded
        && req.ignore.is_none_or(|mode| mode.value(true))
        && (req.geometry() || req.top.is_some())
    {
        return Err(cfg.tr("Excluded window", "窗口已排除").into());
    }
    if req.geometry() {
        position(hwnd, req, cfg, point)?
    }
    if let Some(mode) = req.top {
        unsafe {
            let was = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST != 0;
            check(SetWindowPos(
                hwnd,
                if mode.value(was) {
                    HWND_TOPMOST
                } else {
                    HWND_NOTOPMOST
                },
                0,
                0,
                0,
                0,
                SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOOWNERZORDER,
            ))?
        }
    }
    if let (Some(mode), Some(id)) = (req.ignore, id) {
        if id.name.is_empty() {
            return Err(cfg.tr("Cannot read process name", "无法读取进程名").into());
        }
        config::load(path, Some((&id, mode)))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_resources_reject_unaligned_and_out_of_bounds_results() {
        let data = [10u16, 20, 30, 40];
        assert_eq!(
            super::version_units(&data, 8, data.as_ptr().wrapping_add(1), 2),
            Some(&data[1..3])
        );
        assert!(super::version_units(&data, 8, data.as_ptr().wrapping_add(3), 2).is_none());
        assert!(super::version_units(&data, 7, data.as_ptr().wrapping_add(3), 1).is_none());
        assert!(
            super::version_units(
                &data,
                8,
                data.as_ptr().cast::<u8>().wrapping_add(1).cast(),
                1
            )
            .is_none()
        );
        assert!(super::version_units(&data, 8, std::ptr::null(), 1).is_none());
    }
    #[test]
    fn positioning_does_not_wait_for_another_window_thread() {
        use std::{
            ptr::{null, null_mut},
            time::{Duration, Instant},
        };
        use windows_sys::{Win32::UI::WindowsAndMessaging::*, core::w};
        let (send, receive) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || unsafe {
            let hwnd = CreateWindowExW(
                0,
                w!("STATIC"),
                null(),
                WS_POPUP,
                0,
                0,
                100,
                100,
                null_mut(),
                null_mut(),
                null_mut(),
                null(),
            );
            assert!(!hwnd.is_null());
            send.send(hwnd as usize).unwrap();
            // Deliberately stop this test window's pump, never another application's.
            std::thread::sleep(Duration::from_millis(1000));
            let mut message = MSG::default();
            while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                DispatchMessageW(&message);
            }
            DestroyWindow(hwnd);
        });
        let hwnd = receive.recv_timeout(Duration::from_secs(5)).unwrap() as _;
        let original = super::Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let started = Instant::now();
        let result = super::set_frame(hwnd, super::Rect { x: 10, ..original }, original, original);
        let elapsed = started.elapsed();
        owner.join().unwrap();
        result.unwrap();
        assert!(
            elapsed < Duration::from_millis(750),
            "window thread blocked caller for {elapsed:?}"
        );
    }
    use super::*;
    #[test]
    #[ignore = "read-only exclusion check against an already-open settings window"]
    fn open_settings_are_excluded() {
        // SAFETY: only query handles and window metadata; the existing UI is left untouched.
        let window =
            unsafe { FindWindowW(windows_sys::core::w!("WinSizeP.Settings"), std::ptr::null()) };
        assert!(
            !window.is_null(),
            "Open settings before this read-only check"
        );
        assert_ne!(
            pid(window),
            std::process::id(),
            "Needs a separate settings process"
        );
        for hwnd in [window, unsafe { GetLastActivePopup(window) }] {
            assert!(
                !allowed(hwnd, &[]),
                "Settings or its owned dialog was allowed"
            );
            assert!(
                target(&Selector {
                    hwnd: Some(hwnd as usize),
                    ..Default::default()
                })
                .is_err()
            );
        }
        assert!(
            target(&Selector {
                pid: Some(pid(window)),
                ..Default::default()
            })
            .is_err()
        );
    }
    #[test]
    fn resizing_preserves_top_left_and_respects_minimum() {
        let frame = Rect {
            x: -800,
            y: 20,
            width: 640,
            height: 480,
        };
        let origin = POINT { x: 20, y: 20 };
        let resized = drag_rect(frame, origin, POINT { x: -900, y: 80 }, true, (120, 90));
        assert_eq!(
            resized,
            Rect {
                width: 120,
                height: 540,
                ..frame
            }
        );
        let moved = drag_rect(frame, origin, POINT { x: 100, y: -20 }, false, (1, 1));
        assert_eq!(
            moved,
            Rect {
                x: -720,
                y: -20,
                ..frame
            }
        );
    }
}
