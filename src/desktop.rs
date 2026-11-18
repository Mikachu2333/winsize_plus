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
fn desktop() -> Rect {
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
                .rsplit('\\')
                .next()
                .unwrap_or_default()
                .into()
        }
    }
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

fn bounds(hwnd: HWND) -> Result<(Rect, Rect), String> {
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
        if IsIconic(hwnd) != 0 || IsZoomed(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
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
            SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER,
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
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOOWNERZORDER,
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
