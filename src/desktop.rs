use crate::{
    cli::{Request, Selector, Switch},
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

thread_local! { static DISPLAY_EPOCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
pub fn display_epoch() -> u64 {
    DISPLAY_EPOCH.with(std::cell::Cell::get)
}
pub fn invalidate_displays() {
    DISPLAY_EPOCH.with(|epoch| epoch.set(epoch.get().wrapping_add(1)));
}

#[derive(Clone, Debug)]
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
            .map_err(|_| format!("Unknown display device / 未知屏幕设备: {device}"))?;
        // A hidden window on the monitor has the same PerMonitorV2 context as
        // the actual UI. GetDpiForMonitor must not be used in that context.
        let probe = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            windows_sys::core::w!("STATIC"),
            std::ptr::null(),
            WS_POPUP | WS_DISABLED,
            info.monitorInfo.rcMonitor.left,
            info.monitorInfo.rcMonitor.top,
            1,
            1,
            null_mut(),
            null_mut(),
            null_mut(),
            std::ptr::null(),
        );
        if probe.is_null() {
            return Err(err());
        }
        let dpi = GetDpiForWindow(probe);
        DestroyWindow(probe);
        if dpi == 0 {
            return Err("Cannot determine monitor DPI / 无法读取屏幕 DPI".into());
        }
        Ok(Display {
            work: rect(info.monitorInfo.rcWork),
            full: rect(info.monitorInfo.rcMonitor),
            number,
            dpi: dpi as i32,
        })
    }
}
unsafe extern "system" fn enum_monitor(
    monitor: HMONITOR,
    _: HDC,
    _: *mut RECT,
    data: LPARAM,
) -> i32 {
    // SAFETY: EnumDisplayMonitors calls this synchronously with the live vector below.
    unsafe { &mut *(data as *mut Vec<HMONITOR>) }.push(monitor);
    1
}
pub fn displays() -> Result<Vec<Display>, String> {
    let mut monitors = Vec::<HMONITOR>::new();
    unsafe {
        check(EnumDisplayMonitors(
            null_mut(),
            std::ptr::null(),
            Some(enum_monitor),
            from_mut(&mut monitors) as isize,
        ))?;
    }
    monitors.into_iter().map(display).collect()
}
pub fn screen(point: POINT, wanted: u32) -> Result<Display, String> {
    if wanted == 0 {
        display(unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) })
    } else {
        displays()?
            .into_iter()
            .find(|screen| screen.number == wanted)
            .ok_or_else(|| format!("Monitor {wanted} is unavailable / 屏幕 {wanted} 不可用"))
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
        let mut buf = vec![0u16; 512];
        let result = loop {
            let mut len = buf.len() as u32;
            if QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len) != 0 {
                break String::from_utf16_lossy(&buf[..len as usize]);
            }
            if GetLastError() != ERROR_INSUFFICIENT_BUFFER || buf.len() >= 32768 {
                break String::new();
            }
            buf.resize((buf.len() * 2).min(32768), 0);
        };
        CloseHandle(process);
        result
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
            || crate::identity::settings_class(&class(GetAncestor(hwnd, GA_ROOTOWNER)))
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
                | "WinSizeP.TopmostMarker"
        )
    }
}
pub fn allowed(hwnd: HWND, exclusions: &[Identity]) -> bool {
    eligible(hwnd)
        && crate::security::can_control_window(hwnd)
        && (exclusions.is_empty() || !excluded_or_unknown(&identity(hwnd), exclusions))
}
fn excluded_or_unknown(id: &Identity, exclusions: &[Identity]) -> bool {
    exclusions
        .iter()
        .any(|rule| rule.matches(id) || (!rule.name.is_empty() && id.name.is_empty()))
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
    original: Option<WINDOWPLACEMENT>,
    linked: Option<crate::neighbors::Drag>,
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
        let mut minimized = IsIconic(hwnd) != 0;
        check(ShowWindowAsync(hwnd, SW_RESTORE))?;
        let mut deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        while IsZoomed(hwnd) != 0 || IsIconic(hwnd) != 0 {
            if minimized && IsIconic(hwnd) == 0 {
                // A window minimized while maximized is restored to maximized first.
                minimized = false;
                check(ShowWindowAsync(hwnd, SW_RESTORE))?;
                deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
            }
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
        if cfg.mouse_enabled
            && cfg.linked_resize
            && let Some(linked) = crate::neighbors::Drag::start(origin, cursor()?, cfg)?
        {
            return Ok(Self {
                original: None,
                linked: Some(linked),
                hwnd,
                origin,
                frame: Rect::default(),
                resize,
                minimum: (1, 1),
            });
        }
        if !allowed(hwnd, &cfg.exclusions) {
            return Err(cfg.tr("Window unavailable", "窗口不可操作").into());
        }
        unsafe {
            if IsHungAppWindow(hwnd) != 0 {
                return Err(cfg.tr("Window unresponsive", "窗口无响应").into());
            }
            let mut original = WINDOWPLACEMENT {
                length: size_of::<WINDOWPLACEMENT>() as u32,
                ..Default::default()
            };
            check(GetWindowPlacement(hwnd, &mut original))?;
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
                original: Some(original),
                linked: None,
                hwnd,
                origin,
                frame,
                resize,
                minimum,
            })
        }
    }
    pub fn follow(&self, point: POINT) -> Result<(), String> {
        if let Some(linked) = &self.linked {
            return linked.follow(point);
        }
        let wanted = drag_rect(self.frame, self.origin, point, self.resize, self.minimum);
        let (outer, frame) = bounds(self.hwnd)?;
        if wanted != frame {
            set_frame(self.hwnd, wanted, outer, frame)?;
        }
        Ok(())
    }
    pub fn cancel(&self) -> Result<(), String> {
        if let Some(linked) = &self.linked {
            linked.cancel()
        } else if let Some(original) = &self.original {
            unsafe { check(SetWindowPlacement(self.hwnd, original)) }
        } else {
            Ok(())
        }
    }
}
#[cfg(test)]
pub(crate) fn set_frames(wanted: &[(HWND, Rect)]) -> Result<(), String> {
    // Validate every participant before posting any move. Coordinates are converted
    // from visible frames using each window's own invisible resize margins.
    let positions = wanted
        .iter()
        .map(|&(hwnd, rect)| {
            if unsafe { IsWindow(hwnd) } == 0 {
                return Err("Linked window closed / 联动窗口已关闭".into());
            }
            let (outer, frame) = bounds(hwnd)?;
            Ok((hwnd, frame_position(rect, outer, frame)?))
        })
        .collect::<Result<Vec<_>, String>>()?;
    for (hwnd, rect) in positions {
        unsafe {
            check(SetWindowPos(
                hwnd,
                null_mut(),
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER,
            ))?;
        }
    }
    Ok(())
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
        let area = preset
            .map(|layout| screen(point, layout.monitor))
            .transpose()?;
        let resolve = |frame: Rect| -> Result<Rect, String> {
            let base = match (preset, &area) {
                (Some(layout), Some(area)) => layout.resolve(area.work, frame, desktop())?,
                _ => frame,
            };
            let (width, height) = req.size.unwrap_or((base.width, base.height));
            Ok(Rect {
                x: req.x.unwrap_or(base.x),
                y: req.y.unwrap_or(base.y),
                width,
                height,
            })
        };
        // Reject missing monitors and invalid rectangles before restoring a maximized target.
        let (outer, frame) = bounds(hwnd)?;
        frame_position(resolve(frame)?, outer, frame)?;
        restore(hwnd)?;
        let (outer, frame) = bounds(hwnd)?;
        let wanted = resolve(frame)?;
        set_frame(hwnd, wanted, outer, frame)?;
        // A posted move is not a completed move. Both the target's DPI handler and
        // DWM's visible frame can settle later; require a stable observed result.
        use std::time::{Duration, Instant};
        let until = Instant::now() + Duration::from_millis(1200);
        let mut observed = (outer, frame, GetDpiForWindow(hwnd));
        let mut stable = Instant::now();
        let mut last_request = Instant::now();
        let mut corrections = 0;
        let mut refused = None;
        let mut answered: Option<Instant> = None;
        while Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
            let (outer, frame) = bounds(hwnd)?;
            let current = (outer, frame, GetDpiForWindow(hwnd));
            if current != observed {
                observed = current;
                stable = Instant::now();
                answered = None;
            }
            if stable.elapsed() < Duration::from_millis(60) {
                continue;
            }
            if frame == wanted {
                return Ok(());
            }
            // A window that enforces its own size (fixed dialogs, minimum track size,
            // character-cell consoles) never matches. Once a repeated request leaves it
            // where it was and its thread keeps answering, that rectangle is final. A
            // busy thread does not answer, so a pending move is never mistaken for this.
            let mut reply = 0;
            if refused == Some(frame)
                && SendMessageTimeoutW(hwnd, WM_NULL, 0, 0, SMTO_ABORTIFHUNG, 50, &mut reply) != 0
            {
                match answered {
                    Some(first) if first.elapsed() >= Duration::from_millis(40) => return Ok(()),
                    Some(_) => {}
                    None => answered = Some(Instant::now()),
                }
            }
            if corrections < 4 && last_request.elapsed() >= Duration::from_millis(180) {
                refused = Some(frame);
                set_frame(hwnd, wanted, outer, frame)?;
                last_request = Instant::now();
                stable = Instant::now();
                corrections += 1;
            }
        }
        Err("Window did not reach the requested rectangle (timed out or constrained) / 窗口未达到请求的位置尺寸（超时或受窗口限制）".into())
    }
}

pub(crate) fn frame_position(wanted: Rect, outer: Rect, frame: Rect) -> Result<Rect, String> {
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
    Ok(Rect {
        x,
        y,
        width,
        height,
    })
}
fn centered_rect(frame: Rect, source_dpi: i32, destination: &Display) -> Rect {
    let scale = |value: i32, limit: i32| {
        ((i64::from(value) * i64::from(destination.dpi.max(1)) + i64::from(source_dpi.max(1)) / 2)
            / i64::from(source_dpi.max(1)))
        .clamp(1, i64::from(limit.max(1))) as i32
    };
    let width = scale(frame.width, destination.work.width);
    let height = scale(frame.height, destination.work.height);
    Rect {
        x: destination.work.x + (destination.work.width - width) / 2,
        y: destination.work.y + (destination.work.height - height) / 2,
        width,
        height,
    }
}
fn center_window(hwnd: HWND, cfg: &Config, point: POINT) -> Result<(), String> {
    let destination = screen(point, 0)?;
    let source = display(unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) })?;
    restore(hwnd)?;
    let wanted = centered_rect(bounds(hwnd)?.1, source.dpi, &destination);
    position(
        hwnd,
        &Request {
            x: Some(wanted.x),
            y: Some(wanted.y),
            size: Some((wanted.width, wanted.height)),
            ..Default::default()
        },
        cfg,
        point,
    )
}
fn fullscreen_window(hwnd: HWND, cfg: &Config, point: POINT) -> Result<(), String> {
    use std::time::{Duration, Instant};
    // SAFETY: only a payload-free system command is sent to the validated target.
    unsafe {
        if GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_MAXIMIZEBOX != 0 {
            if IsZoomed(hwnd) != 0 {
                return Ok(());
            }
            let mut result = 0;
            if SendMessageTimeoutW(
                hwnd,
                WM_SYSCOMMAND,
                SC_MAXIMIZE as usize,
                0,
                SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
                250,
                &mut result,
            ) != 0
            {
                let deadline = Instant::now() + Duration::from_millis(250);
                while Instant::now() < deadline {
                    if IsZoomed(hwnd) != 0 {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
    let work = screen(point, 0)?.work;
    position(
        hwnd,
        &Request {
            x: Some(work.x),
            y: Some(work.y),
            size: Some((work.width, work.height)),
            ..Default::default()
        },
        cfg,
        point,
    )
}
fn set_frame(hwnd: HWND, wanted: Rect, outer: Rect, frame: Rect) -> Result<(), String> {
    let Rect {
        x,
        y,
        width,
        height,
    } = frame_position(wanted, outer, frame)?;
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
pub fn topmost(hwnd: HWND) -> Result<bool, String> {
    // SAFETY: querying a foreign HWND does not dereference it. A zero style is
    // valid, so distinguish it from a failed query using the thread's last error.
    unsafe {
        SetLastError(ERROR_SUCCESS);
        let style = GetWindowLongW(hwnd, GWL_EXSTYLE);
        if style == 0 && GetLastError() != ERROR_SUCCESS {
            return Err(err());
        }
        Ok(style as u32 & WS_EX_TOPMOST != 0)
    }
}
fn set_topmost(hwnd: HWND, mode: Switch) -> Result<(), String> {
    let current = topmost(hwnd)?;
    let wanted = mode.value(current);
    if wanted == current {
        return Ok(());
    }
    // SAFETY: SetWindowPos validates the handle. Keep the request asynchronous
    // so an unresponsive window procedure cannot block this thread indefinitely.
    unsafe {
        check(SetWindowPos(
            hwnd,
            if wanted { HWND_TOPMOST } else { HWND_NOTOPMOST },
            0,
            0,
            0,
            0,
            SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOOWNERZORDER,
        ))?;
    }
    // Posting the request only confirms it was queued. Wait for the actual
    // style before a caller reports success or decides the next toggle.
    let until = std::time::Instant::now() + std::time::Duration::from_millis(1200);
    loop {
        if topmost(hwnd)? == wanted {
            return Ok(());
        }
        if std::time::Instant::now() >= until {
            return Err("Always-on-top change timed out / 置顶状态切换超时".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
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
    if (req.geometry() || req.top.is_some()) && !crate::security::can_control_window(hwnd) {
        return Err(cfg
            .tr(
                "Window permissions are higher or unavailable",
                "目标窗口权限较高或无法查询权限",
            )
            .into());
    }
    let id = (req.ignore.is_some() || !cfg.exclusions.is_empty()).then(|| identity(hwnd));
    let excluded = id
        .as_ref()
        .is_some_and(|id| excluded_or_unknown(id, &cfg.exclusions));
    if excluded
        && req.ignore.is_none_or(|mode| mode.value(true))
        && (req.geometry() || req.top.is_some())
    {
        return Err(cfg.tr("Excluded window", "窗口已排除").into());
    }
    if req.fullscreen {
        fullscreen_window(hwnd, cfg, point)?;
    } else if req.center {
        center_window(hwnd, cfg, point)?;
    } else if req.geometry() {
        position(hwnd, req, cfg, point)?
    }
    if let Some(mode) = req.top {
        set_topmost(hwnd, mode)?;
        crate::topmost_marker::record(hwnd, topmost(hwnd)?)?;
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
    fn centering_scales_dpi_and_clamps_only_oversized_edges() {
        use super::*;
        let destination = Display {
            number: 2,
            dpi: 144,
            full: Rect {
                x: -1920,
                y: -1080,
                width: 1920,
                height: 1080,
            },
            work: Rect {
                x: -1920,
                y: -1080,
                width: 1920,
                height: 1040,
            },
        };
        let frame = Rect {
            x: 100,
            y: 100,
            width: 800,
            height: 400,
        };
        assert_eq!(
            centered_rect(frame, 96, &destination),
            Rect {
                x: -1560,
                y: -860,
                width: 1200,
                height: 600
            }
        );
        assert_eq!(
            centered_rect(
                Rect {
                    width: 1600,
                    ..frame
                },
                96,
                &destination
            ),
            Rect {
                x: -1920,
                y: -860,
                width: 1920,
                height: 600
            }
        );
        assert_eq!(
            centered_rect(
                Rect {
                    width: 3000,
                    height: 2000,
                    ..frame
                },
                96,
                &destination
            ),
            destination.work
        );
        assert_eq!(
            centered_rect(
                Rect {
                    width: 1200,
                    height: 600,
                    ..frame
                },
                144,
                &Display {
                    dpi: 96,
                    ..destination
                }
            ),
            Rect {
                x: -1360,
                y: -760,
                width: 800,
                height: 400
            }
        );
    }
    struct TopmostWindow {
        hwnd: super::HWND,
        stop: std::sync::mpsc::Sender<()>,
        owner: Option<std::thread::JoinHandle<()>>,
    }
    impl TopmostWindow {
        fn new(top: bool, delay: std::time::Duration) -> Self {
            use super::*;
            let (ready, window) = std::sync::mpsc::sync_channel(1);
            let (stop, stopped) = std::sync::mpsc::channel();
            let owner = std::thread::spawn(move || {
                // SAFETY: the test thread owns this hidden window, pumps its
                // messages, and destroys it before exiting. No user window is used.
                unsafe {
                    let hwnd = CreateWindowExW(
                        if top { WS_EX_TOPMOST } else { 0 },
                        windows_sys::core::w!("STATIC"),
                        std::ptr::null(),
                        WS_POPUP,
                        0,
                        0,
                        100,
                        100,
                        null_mut(),
                        null_mut(),
                        null_mut(),
                        std::ptr::null(),
                    );
                    assert!(!hwnd.is_null());
                    ready.send(hwnd as usize).unwrap();
                    // Model a busy target that cannot process the posted change yet.
                    if matches!(
                        stopped.recv_timeout(delay),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    ) {
                        loop {
                            let mut message = MSG::default();
                            while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                                DispatchMessageW(&message);
                            }
                            if !matches!(
                                stopped.recv_timeout(std::time::Duration::from_millis(1)),
                                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                            ) {
                                break;
                            }
                        }
                    }
                    DestroyWindow(hwnd);
                }
            });
            Self {
                hwnd: window
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap() as _,
                stop,
                owner: Some(owner),
            }
        }
    }
    impl Drop for TopmostWindow {
        fn drop(&mut self) {
            let _ = self.stop.send(());
            self.owner.take().unwrap().join().unwrap();
        }
    }
    #[test]
    fn topmost_toggle_waits_for_the_target_and_reads_external_changes() {
        use std::time::Duration;
        for initial in [false, true] {
            let window = TopmostWindow::new(initial, Duration::from_millis(150));
            assert_eq!(topmost(window.hwnd).unwrap(), initial);
            set_topmost(window.hwnd, Switch::Toggle).unwrap();
            assert_eq!(topmost(window.hwnd).unwrap(), !initial);
            set_topmost(window.hwnd, Switch::Toggle).unwrap();
            assert_eq!(topmost(window.hwnd).unwrap(), initial);
            for (mode, wanted) in [(Switch::On, true), (Switch::Off, false)] {
                set_topmost(window.hwnd, mode).unwrap();
                set_topmost(window.hwnd, mode).unwrap();
                assert_eq!(topmost(window.hwnd).unwrap(), wanted);
            }
            // SAFETY: the fixture keeps this window and its message pump alive.
            // Change it outside our helper, as another tool or the application would.
            unsafe {
                check(SetWindowPos(
                    window.hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
                ))
                .unwrap();
            }
            assert!(topmost(window.hwnd).unwrap());
            set_topmost(window.hwnd, Switch::Toggle).unwrap();
            assert!(!topmost(window.hwnd).unwrap());
        }
    }
    #[test]
    #[ignore = "requires access to the interactive desktop and its work area"]
    fn fullscreen_uses_maximize_or_work_area_fallback() {
        use super::*;
        let _dpi = crate::test_support::Dpi::new();
        let cfg = crate::test_support::cfg();
        let point = cursor().unwrap();
        let target = TopmostWindow::new(false, std::time::Duration::ZERO);
        fullscreen_window(target.hwnd, &cfg, point).unwrap();
        assert_eq!(
            bounds(target.hwnd).unwrap().1,
            screen(point, 0).unwrap().work
        );
        unsafe {
            SetWindowLongW(
                target.hwnd,
                GWL_STYLE,
                (WS_OVERLAPPEDWINDOW | WS_VISIBLE) as i32,
            );
        }
        fullscreen_window(target.hwnd, &cfg, point).unwrap();
        assert_ne!(unsafe { IsZoomed(target.hwnd) }, 0);
    }
    #[test]
    fn topmost_does_not_report_success_when_the_target_stops_processing() {
        let window = TopmostWindow::new(true, std::time::Duration::from_secs(5));
        let error = set_topmost(window.hwnd, Switch::Toggle).unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(topmost(window.hwnd).unwrap());
    }
    #[test]
    fn topmost_rejects_invalid_handles_but_accepts_a_zero_extended_style() {
        assert!(topmost(null_mut()).is_err());
        let window = TopmostWindow::new(false, std::time::Duration::ZERO);
        // SAFETY: the fixture owns the window until this assertion finishes.
        assert_eq!(unsafe { GetWindowLongW(window.hwnd, GWL_EXSTYLE) }, 0);
        assert!(!topmost(window.hwnd).unwrap());
    }
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
        let window = unsafe { FindWindowW(crate::identity::SETTINGS_CLASS, std::ptr::null()) };
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

#[cfg(test)]
mod multi_monitor_regressions {
    use super::*;
    use crate::test_support::{Dpi, displays, point};
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        time::{Duration, Instant},
    };

    unsafe extern "system" fn proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if message == WM_DPICHANGED {
            let delay = unsafe { GetPropW(hwnd, windows_sys::core::w!("AuditDelay")) } as usize;
            if delay != 0 {
                std::thread::sleep(Duration::from_millis(delay as u64));
            }
            let r = unsafe { &*(lp as *const RECT) };
            unsafe {
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            return 0;
        }
        if message == WM_APP + 77 {
            let events = unsafe {
                &*(GetPropW(hwnd, windows_sys::core::w!("AuditEvents")) as *const AtomicUsize)
            };
            events.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(300));
            return 0;
        }
        if message == WM_CLOSE {
            unsafe {
                DestroyWindow(hwnd);
                PostQuitMessage(0);
            }
            return 0;
        }
        unsafe { DefWindowProcW(hwnd, message, wp, lp) }
    }

    fn target(d: &Display, delay: usize) -> (usize, std::thread::JoinHandle<()>, Arc<AtomicUsize>) {
        let (tx, rx) = mpsc::sync_channel(1);
        let x = d.work.x + 40;
        let y = d.work.y + 40;
        let events = Arc::new(AtomicUsize::new(0));
        let copy = events.clone();
        let worker = std::thread::spawn(move || {
            let _dpi = Dpi::new();
            unsafe {
                let name = windows_sys::core::w!("WinSizeP.EdgeAudit.Target");
                let module =
                    windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
                RegisterClassW(&WNDCLASSW {
                    lpszClassName: name,
                    lpfnWndProc: Some(proc),
                    hInstance: module,
                    ..Default::default()
                });
                let window = CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    name,
                    windows_sys::core::w!("WinSizeP audit temporary target"),
                    WS_OVERLAPPEDWINDOW,
                    x,
                    y,
                    800,
                    600,
                    null_mut(),
                    null_mut(),
                    module,
                    std::ptr::null(),
                );
                assert!(!window.is_null());
                SetPropW(window, windows_sys::core::w!("AuditDelay"), delay as HANDLE);
                SetPropW(
                    window,
                    windows_sys::core::w!("AuditEvents"),
                    Arc::as_ptr(&copy) as HANDLE,
                );
                ShowWindow(window, SW_SHOWNOACTIVATE);
                tx.send(window as usize).unwrap();
                let mut message = MSG::default();
                while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        });
        (
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            worker,
            events,
        )
    }

    fn probe(delay: usize, busy: bool) -> Option<(Result<(), String>, Rect, Rect, u128)> {
        let _dpi = Dpi::new();
        let displays = crate::test_support::require_displays(true)?;
        let low = &displays.first().unwrap().1;
        let high = &displays.last().unwrap().1;
        let (handle, worker, events) = target(low, delay);
        let hwnd = handle as HWND;
        if busy {
            unsafe {
                PostMessageW(hwnd, WM_APP + 77, 0, 0);
            }
            let deadline = Instant::now() + Duration::from_secs(1);
            while events.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(events.load(Ordering::SeqCst), 1);
        }
        let wanted = Rect {
            x: high.work.x + 100,
            y: high.work.y + 100,
            width: 1000,
            height: 700,
        };
        let request = Request {
            x: Some(wanted.x),
            y: Some(wanted.y),
            size: Some((wanted.width, wanted.height)),
            ..Default::default()
        };
        let start = Instant::now();
        let result = position(hwnd, &request, &crate::test_support::cfg(), point(high));
        let elapsed = start.elapsed().as_millis();
        let at_return = bounds(hwnd).unwrap().1;
        std::thread::sleep(Duration::from_millis(600));
        let eventual = bounds(hwnd).unwrap().1;
        println!(
            "delay={delay} busy={busy} result={result:?} elapsed_ms={elapsed} requested={wanted:?} at_return={at_return:?} eventual={eventual:?} dpi={}",
            unsafe { GetDpiForWindow(hwnd) }
        );
        unsafe {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
        worker.join().unwrap();
        Some((result, wanted, eventual, elapsed))
    }

    #[test]
    #[ignore = "requires two monitors with different DPI and temporary target windows"]
    fn center_scales_on_real_mixed_dpi_monitors() {
        let _dpi = Dpi::new();
        let Some(displays) = crate::test_support::require_displays(true) else {
            return;
        };
        let low = &displays.first().unwrap().1;
        let high = &displays.last().unwrap().1;
        for (source, destination) in [(low, high), (high, low)] {
            let (handle, worker, _) = target(source, 0);
            let hwnd = handle as HWND;
            let wanted = centered_rect(bounds(hwnd).unwrap().1, source.dpi, destination);
            let result = center_window(hwnd, &crate::test_support::cfg(), point(destination));
            let actual = bounds(hwnd).unwrap().1;
            unsafe {
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }
            worker.join().unwrap();
            println!(
                "center dpi {} -> {}, expected={wanted:?}, actual={actual:?}, result={result:?}",
                source.dpi, destination.dpi
            );
            result.unwrap();
            assert_eq!(actual, wanted);
        }
    }

    #[test]
    #[ignore = "measures same-monitor positioning latency on a responsive and deliberately busy temporary window"]
    fn measure_positioning_wait_on_responsive_and_busy_windows() {
        let _dpi = Dpi::new();
        let display = displays().remove(0).1;
        for busy in [false, true] {
            let (handle, worker, events) = target(&display, 0);
            let hwnd = handle as HWND;
            if busy {
                unsafe {
                    PostMessageW(hwnd, WM_APP + 77, 0, 0);
                }
                let deadline = Instant::now() + Duration::from_secs(1);
                while events.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            let started = Instant::now();
            let req = Request {
                x: Some(display.work.x + 70),
                y: Some(display.work.y + 70),
                size: Some((600, 400)),
                ..Default::default()
            };
            let result = position(hwnd, &req, &crate::test_support::cfg(), point(&display));
            let elapsed = started.elapsed();
            let actual = bounds(hwnd).unwrap().1;
            unsafe {
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }
            worker.join().unwrap();
            result.unwrap();
            assert_eq!(
                actual,
                Rect {
                    x: display.work.x + 70,
                    y: display.work.y + 70,
                    width: 600,
                    height: 400
                }
            );
            println!("position busy={busy} elapsed_ms={}", elapsed.as_millis());
        }
    }

    #[test]
    #[ignore = "requires an interactive desktop; some cases need mixed DPI monitors"]
    fn position_corrects_immediate_cross_dpi_target() {
        let Some((result, wanted, actual, _)) = probe(0, false) else {
            return;
        };
        result.unwrap();
        assert_eq!(actual, wanted);
    }

    #[test]
    #[ignore = "requires an interactive desktop; some cases need mixed DPI monitors"]
    fn position_verifies_busy_cross_dpi_target_before_success() {
        let Some((result, wanted, actual, _)) = probe(0, true) else {
            return;
        };
        assert!(
            result.is_err() || actual == wanted,
            "The operation reports success but the target eventually settles at a different frame"
        );
    }

    #[test]
    #[ignore = "requires an interactive desktop; some cases need mixed DPI monitors"]
    fn process_identity_handles_long_executable_paths() {
        let _dpi = Dpi::new();
        let d = displays();
        let (handle, worker, _) = target(&d[0].1, 0);
        let path = std::env::current_exe().unwrap();
        let expected = path.file_name().unwrap().to_string_lossy();
        let id = identity(handle as HWND);
        let rule = crate::config::Identity {
            name: expected.into_owned(),
            ..Default::default()
        };
        unsafe {
            PostMessageW(handle as HWND, WM_CLOSE, 0, 0);
        }
        worker.join().unwrap();
        println!(
            "executable_path_length={} expected_name={} actual_name={:?} exclusion_matches={}",
            path.as_os_str().encode_wide().count(),
            rule.name,
            id.name,
            rule.matches(&id)
        );
        assert!(
            rule.matches(&id),
            "A long executable path must not bypass a process-name exclusion"
        );
    }

    #[test]
    #[ignore = "requires an interactive desktop; some cases need mixed DPI monitors"]
    fn invalid_preset_must_not_restore_a_maximized_window() {
        let _dpi = Dpi::new();
        let d = displays();
        let (handle, worker, _) = target(&d[0].1, 0);
        let hwnd = handle as HWND;
        unsafe {
            ShowWindowAsync(hwnd, SW_MAXIMIZE);
        }
        let end = Instant::now() + Duration::from_secs(2);
        while unsafe { IsZoomed(hwnd) } == 0 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_ne!(unsafe { IsZoomed(hwnd) }, 0);
        let mut cfg = crate::test_support::cfg();
        cfg.presets
            .insert("bad".into(), "0|0|999999999999sw|100sh".parse().unwrap());
        let request = Request {
            preset: Some("bad".into()),
            ..Default::default()
        };
        let result = position(hwnd, &request, &cfg, point(&d[0].1));
        let still_maximized = unsafe { IsZoomed(hwnd) } != 0;
        unsafe {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
        worker.join().unwrap();
        println!("invalid preset result={result:?} still_maximized={still_maximized}");
        assert!(
            result.is_err() && still_maximized,
            "Reject invalid geometry before altering the target state"
        );
    }
}
