use crate::{
    config::Config,
    desktop::{self, Display, check, err, wide},
    layout::Rect,
};
use std::ptr::{null, null_mut};
use windows_sys::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{LibraryLoader::GetModuleHandleW, Registry::*},
        UI::WindowsAndMessaging::*,
    },
    core::w,
};

const CLASS: *const u16 = w!("WinSizeP.Overlay");
unsafe extern "system" fn paint(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_NCHITTEST => HTTRANSPARENT as isize,
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut paint);
                let mut area = RECT::default();
                GetClientRect(hwnd, &mut area);
                SetDCBrushColor(dc, GetWindowLongPtrW(hwnd, GWLP_USERDATA) as u32);
                FillRect(dc, &area, GetStockObject(DC_BRUSH));
                let font = GetPropW(hwnd, w!("Font"));
                if !font.is_null() {
                    let mut text = vec![0; GetWindowTextLengthW(hwnd) as usize + 1];
                    let len = GetWindowTextW(hwnd, text.as_mut_ptr(), text.len() as i32);
                    let old = SelectObject(dc, font);
                    SetBkMode(dc, TRANSPARENT as i32);
                    SetTextColor(dc, GetPropW(hwnd, w!("TextColor")) as usize as u32);
                    let padding = GetPropW(hwnd, w!("Padding")) as usize as i32;
                    area.left += padding * 2;
                    area.right -= padding * 2;
                    area.top += padding;
                    DrawTextW(
                        dc,
                        text.as_ptr(),
                        len,
                        &mut area,
                        DT_LEFT | DT_NOPREFIX | DT_WORDBREAK,
                    );
                    SelectObject(dc, old);
                }
                EndPaint(hwnd, &paint);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
struct Panel(HWND);
impl Panel {
    fn new() -> Result<Self, String> {
        unsafe {
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT,
                CLASS,
                null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            if hwnd.is_null() {
                Err(err())
            } else {
                Ok(Self(hwnd))
            }
        }
    }
    fn style(&self, color: u32, alpha: u8) {
        unsafe {
            SetWindowLongPtrW(self.0, GWLP_USERDATA, color as isize);
            SetLayeredWindowAttributes(self.0, 0, alpha, LWA_ALPHA);
            InvalidateRect(self.0, null(), 0);
        }
    }
    fn show(&self, rect: Rect) {
        unsafe {
            if rect.width > 0 && rect.height > 0 {
                SetWindowPos(
                    self.0,
                    HWND_TOPMOST,
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOOWNERZORDER,
                );
            } else {
                self.hide();
            }
        }
    }
    fn hide(&self) {
        unsafe {
            ShowWindow(self.0, SW_HIDE);
        }
    }
}
impl Drop for Panel {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.0);
        }
    }
}
pub struct Overlay {
    tip: Panel,
    font: HFONT,
    segments: Vec<Panel>,
    mode: u8,
    last_monitor: HMONITOR,
    display: Option<Display>,
}
fn scale(value: i32, dpi: i32) -> i32 {
    (value * dpi + 48) / 96
}
fn dark() -> bool {
    unsafe {
        let mut light = 1u32;
        let mut len = 4;
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut light as *mut u32).cast(),
            &mut len,
        );
        light == 0
    }
}
impl Overlay {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let cls = WNDCLASSW {
                lpfnWndProc: Some(paint),
                hInstance: GetModuleHandleW(null()),
                lpszClassName: CLASS,
                ..Default::default()
            };
            if RegisterClassW(&cls) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                return Err(err());
            }
            Ok(Self {
                tip: Panel::new()?,
                font: null_mut(),
                segments: Vec::new(),
                mode: 0,
                last_monitor: null_mut(),
                display: None,
            })
        }
    }
    pub fn hide_tip(&self) {
        self.tip.hide()
    }
    pub fn tip(&mut self, text: &str, cfg: &Config) -> Result<(), String> {
        unsafe {
            let screen = desktop::screen(desktop::cursor()?, 0)?;
            let dpi = screen.dpi;
            let text = wide(text);
            let font = CreateFontW(
                -scale(cfg.font_size, dpi),
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET.into(),
                0,
                0,
                CLEARTYPE_QUALITY.into(),
                0,
                wide(&cfg.font).as_ptr(),
            );
            if font.is_null() {
                return Err(err());
            }
            let hwnd = self.tip.0;
            let dc = GetDC(hwnd);
            if dc.is_null() {
                DeleteObject(font);
                return Err(err());
            }
            let old = SelectObject(dc, font);
            let padding = scale(8, dpi);
            let mut size = RECT {
                right: (screen.work.width - 4 * padding).max(1),
                ..Default::default()
            };
            let measured = DrawTextW(
                dc,
                text.as_ptr(),
                (text.len() - 1) as i32,
                &mut size,
                DT_CALCRECT | DT_NOPREFIX | DT_WORDBREAK,
            );
            SelectObject(dc, old);
            ReleaseDC(hwnd, dc);
            if measured == 0 || SetPropW(hwnd, w!("Font"), font) == 0 {
                DeleteObject(font);
                return Err(err());
            }
            if !self.font.is_null() {
                DeleteObject(self.font);
            }
            self.font = font;
            SetPropW(hwnd, w!("Padding"), padding as usize as HANDLE);
            let index = if cfg.theme == "dark" || (cfg.theme == "system" && dark()) {
                2
            } else {
                0
            };
            SetPropW(
                hwnd,
                w!("TextColor"),
                cfg.colors[index + 1] as usize as HANDLE,
            );
            check(SetWindowTextW(hwnd, text.as_ptr()))?;
            self.tip.style(cfg.colors[index], 255);
            let width = (size.right + 4 * padding).min(screen.work.width);
            let height = (size.bottom + 2 * padding).min(screen.work.height);
            self.tip.show(Rect {
                x: screen.work.x + (screen.work.width - width) / 2,
                y: screen.work.y + screen.work.height - height - scale(6, dpi),
                width,
                height,
            });
            Ok(())
        }
    }
    pub fn mode(&self) -> u8 {
        self.mode
    }
    pub fn guides(&mut self, mode: u8, cfg: &Config) -> Result<(), String> {
        if mode != 0 {
            while self.segments.len() < 4 {
                self.segments.push(Panel::new()?);
            }
        }
        for (index, panel) in self.segments.iter().enumerate() {
            let line = cfg.lines[index / 2];
            panel.style(line.color, line.opacity);
            if mode & (1 << (index / 2)) == 0 {
                panel.hide()
            }
        }
        self.mode = mode;
        self.last_monitor = null_mut();
        if mode != 0 {
            self.follow(desktop::cursor()?, cfg)?
        }
        Ok(())
    }
    pub fn follow(&mut self, point: POINT, cfg: &Config) -> Result<(), String> {
        if self.mode == 0 {
            return Ok(());
        }
        let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
        if self.last_monitor != monitor {
            self.display = Some(desktop::display(monitor)?);
            self.last_monitor = monitor;
        }
        let screen = self.display.as_ref().ok_or("display")?;
        let area = screen.full;
        for (axis, line) in cfg.lines.iter().enumerate() {
            if self.mode & (1 << axis) == 0 {
                continue;
            }
            let (start, end, center, across, cross_min, cross_max) = if axis == 0 {
                (
                    area.x,
                    area.x + area.width,
                    point.x,
                    point.y,
                    area.y,
                    area.y + area.height,
                )
            } else {
                (
                    area.y,
                    area.y + area.height,
                    point.y,
                    point.x,
                    area.x,
                    area.x + area.width,
                )
            };
            let gap = scale(line.gap, screen.dpi);
            let width = scale(line.width, screen.dpi).max(1);
            let cross = (across - width / 2).max(cross_min);
            let thickness = (across - width / 2 + width).min(cross_max) - cross;
            for (half, (begin, finish)) in [
                (start, (center - gap).min(end)),
                ((center + gap).max(start), end),
            ]
            .into_iter()
            .enumerate()
            {
                let length = finish - begin;
                self.segments[axis * 2 + half].show(if axis == 0 {
                    Rect {
                        x: begin,
                        y: cross,
                        width: length,
                        height: thickness,
                    }
                } else {
                    Rect {
                        x: cross,
                        y: begin,
                        width: thickness,
                        height: length,
                    }
                });
            }
        }
        Ok(())
    }
}
impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            RemovePropW(self.tip.0, w!("Font"));
            if !self.font.is_null() {
                DeleteObject(self.font);
            }
        }
    }
}
