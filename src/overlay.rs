use crate::{
    config::Config,
    desktop::{self, Display, check, err},
    fonts::{Fonts, TextLayout},
    layout::Rect,
    tray::APP_ICON,
};
use std::{
    cell::Cell,
    ptr::{null, null_mut},
};
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
                if !GetPropW(hwnd, w!("PixelAlpha")).is_null() {
                    EndPaint(hwnd, &paint);
                    return 0;
                }
                let mut area = RECT::default();
                GetClientRect(hwnd, &mut area);
                SetDCBrushColor(dc, GetWindowLongPtrW(hwnd, GWLP_USERDATA) as u32);
                FillRect(dc, &area, GetStockObject(DC_BRUSH));
                let axis = GetPropW(hwnd, w!("Axis")) as usize;
                if axis != 0 {
                    let start = GetPropW(hwnd, w!("GapStart")) as usize as i32;
                    let end = GetPropW(hwnd, w!("GapEnd")) as usize as i32;
                    let gap = if axis == 1 {
                        RECT {
                            left: start,
                            right: end,
                            ..area
                        }
                    } else {
                        RECT {
                            top: start,
                            bottom: end,
                            ..area
                        }
                    };
                    SetDCBrushColor(dc, GetPropW(hwnd, w!("ColorKey")) as usize as u32);
                    FillRect(dc, &gap, GetStockObject(DC_BRUSH));
                }
                let layout = GetPropW(hwnd, w!("TextLayout")) as *const TextLayout;
                if !layout.is_null() {
                    // SAFETY: the owning overlay removes this property before dropping the boxed layout.
                    let layout = &*layout;
                    let border = GetPropW(hwnd, w!("BorderWidth")) as usize as i32;
                    let radius = GetPropW(hwnd, w!("CornerDiameter")) as usize as i32;
                    let outline = CreateRoundRectRgn(0, 0, area.right, area.bottom, radius, radius);
                    if !outline.is_null() {
                        SetDCBrushColor(dc, GetPropW(hwnd, w!("BorderColor")) as usize as u32);
                        FrameRgn(dc, outline, GetStockObject(DC_BRUSH), border, border);
                        DeleteObject(outline);
                    }
                    let left = GetPropW(hwnd, w!("PaddingX")) as usize as i32;
                    let top = GetPropW(hwnd, w!("PaddingTop")) as usize as i32;
                    let color = GetPropW(hwnd, w!("TextColor")) as usize as u32;
                    layout.draw(dc, left, top, color, false);
                }
                EndPaint(hwnd, &paint);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
pub(crate) struct Panel(pub HWND, Cell<Option<Rect>>);
impl Panel {
    pub(crate) fn new() -> Result<Self, String> {
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
                Ok(Self(hwnd, Cell::new(None)))
            }
        }
    }
    pub(crate) fn style(&self, color: u32, alpha: u8) {
        unsafe {
            SetWindowLongPtrW(self.0, GWLP_USERDATA, color as isize);
            SetLayeredWindowAttributes(self.0, 0, alpha, LWA_ALPHA);
            InvalidateRect(self.0, null(), 0);
        }
    }
    pub(crate) fn show(&self, rect: Rect) {
        if self.1.get() == Some(rect) {
            return;
        }
        unsafe {
            if rect.width > 0 && rect.height > 0 {
                let z_order = if self.1.get().is_some() {
                    SWP_NOZORDER
                } else {
                    0
                };
                SetWindowPos(
                    self.0,
                    HWND_TOPMOST,
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    SWP_NOACTIVATE
                        | SWP_SHOWWINDOW
                        | SWP_NOOWNERZORDER
                        | SWP_NOSENDCHANGING
                        | z_order,
                );
                self.1.set(Some(rect));
            } else {
                self.hide();
            }
        }
    }
    pub(crate) fn hide(&self) {
        if self.1.take().is_none() {
            return;
        }
        unsafe {
            ShowWindow(self.0, SW_HIDE);
        }
    }
    fn gap(&self, start: i32, end: i32, axis: usize) {
        unsafe {
            let old_start = GetPropW(self.0, w!("GapStart")) as usize as i32;
            let old_end = GetPropW(self.0, w!("GapEnd")) as usize as i32;
            if (start, end) == (old_start, old_end) {
                return;
            }
            SetPropW(self.0, w!("GapStart"), start as usize as HANDLE);
            SetPropW(self.0, w!("GapEnd"), end as usize as HANDLE);
            let mut area = RECT::default();
            GetClientRect(self.0, &mut area);
            if axis == 0 {
                area.left = start.min(old_start);
                area.right = end.max(old_end);
            } else {
                area.top = start.min(old_start);
                area.bottom = end.max(old_end);
            }
            InvalidateRect(self.0, &area, 0);
        }
    }
    fn round(&self, width: i32, height: i32, radius: i32) -> Result<(), String> {
        unsafe {
            let region = CreateRoundRectRgn(0, 0, width, height, radius, radius);
            if region.is_null() {
                return Err(err());
            }
            if SetWindowRgn(self.0, region, 1) == 0 {
                let problem = err();
                DeleteObject(region);
                return Err(problem);
            }
            Ok(())
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
    shadow: Panel,
    text: Option<Box<TextLayout>>,
    segments: Vec<Panel>,
    mode: u8,
    last_monitor: HMONITOR,
    display: Option<Display>,
}
fn scale(value: i32, dpi: i32) -> i32 {
    (value * dpi + 48) / 96
}
pub fn dark() -> bool {
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
            let instance = GetModuleHandleW(null());
            let cls = WNDCLASSW {
                lpfnWndProc: Some(paint),
                hInstance: instance,
                hIcon: LoadIconW(instance, APP_ICON),
                lpszClassName: CLASS,
                ..Default::default()
            };
            if RegisterClassW(&cls) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                return Err(err());
            }
            let tip = Panel::new()?;
            Ok(Self {
                shadow: Panel::new()?,
                tip,
                text: None,
                segments: Vec::new(),
                mode: 0,
                last_monitor: null_mut(),
                display: None,
            })
        }
    }
    pub fn hide_tip(&self) {
        crate::input::tip_visible(false);
        self.tip.hide();
        self.shadow.hide()
    }
    pub fn tip(&mut self, text: &str, cfg: &Config) -> Result<(), String> {
        let screen = desktop::screen(desktop::cursor()?, 0)?;
        let dpi = screen.dpi;
        let border = scale(1, dpi).max(1);
        let horizontal = scale(10, dpi) + border;
        let top = scale(4, dpi) + border;
        let bottom = scale(6, dpi) + border;
        let radius = scale(12, dpi);
        let fonts = Fonts::opaque(
            &cfg.font,
            &cfg.font_fallbacks,
            cfg.font_size,
            dpi,
            cfg.font_bold,
            cfg.font_italic,
        )?;
        let text = Box::new(TextLayout::new(
            text,
            fonts,
            (screen.work.width - 2 * horizontal).max(1),
        )?);
        let width = (text.width + 2 * horizontal).min(screen.work.width);
        let height = (text.height + top + bottom).min(screen.work.height);
        let index = if cfg.theme == "dark" || (cfg.theme == "system" && dark()) {
            2
        } else {
            0
        };
        unsafe {
            check(SetPropW(
                self.tip.0,
                w!("TextLayout"),
                (&*text as *const TextLayout) as HANDLE,
            ))?;
            for (property, value) in [
                (w!("PaddingX"), horizontal as usize),
                (w!("PaddingTop"), top as usize),
                (w!("BorderWidth"), border as usize),
                (w!("CornerDiameter"), radius as usize),
                (
                    w!("BorderColor"),
                    crate::colors::tip_border(cfg.colors[index]) as usize,
                ),
            ] {
                SetPropW(self.tip.0, property, value as HANDLE);
            }
            SetPropW(
                self.tip.0,
                w!("TextColor"),
                cfg.colors[index + 1] as usize as HANDLE,
            );
        }
        self.text = Some(text);
        self.tip.style(cfg.colors[index], 255);
        let rect = Rect {
            x: screen.work.x + (screen.work.width - width) / 2,
            y: screen.work.y + screen.work.height - height - scale(6, dpi),
            width,
            height,
        };
        self.tip.round(width, height, radius)?;
        self.shadow.round(width, height, radius)?;
        self.shadow.style(
            crate::colors::BLACK,
            if index == 2 {
                crate::colors::SHADOW_DARK_ALPHA
            } else {
                crate::colors::SHADOW_LIGHT_ALPHA
            },
        );
        self.shadow.show(Rect {
            y: rect.y + scale(3, dpi),
            ..rect
        });
        self.tip.show(rect);
        crate::input::tip_visible(true);
        Ok(())
    }
    pub fn mode(&self) -> u8 {
        self.mode
    }
    pub fn guides(&mut self, mode: u8, cfg: &Config) -> Result<(), String> {
        if mode != 0 {
            while self.segments.len() < 2 {
                self.segments.push(Panel::new()?);
            }
        }
        for (index, panel) in self.segments.iter().enumerate() {
            let line = cfg.lines[index];
            panel.style(line.color, line.opacity);
            unsafe {
                let key = line.color ^ 0x00ffffff;
                SetPropW(panel.0, w!("ColorKey"), key as usize as HANDLE);
                SetPropW(panel.0, w!("Axis"), (index + 1) as HANDLE);
                SetLayeredWindowAttributes(panel.0, key, line.opacity, LWA_ALPHA | LWA_COLORKEY);
            }
            if mode & (1 << index) == 0 {
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
            // Keep each layered surface at a constant size. Only its position and the small
            // transparent pointer gap change; resizing four surfaces every frame is costly.
            let panel = &self.segments[axis];
            panel.show(if axis == 0 {
                Rect {
                    x: start,
                    y: cross,
                    width: end - start,
                    height: thickness,
                }
            } else {
                Rect {
                    x: cross,
                    y: start,
                    width: thickness,
                    height: end - start,
                }
            });
            panel.gap(
                (center - gap - start).clamp(0, end - start),
                (center + gap - start).clamp(0, end - start),
                axis,
            );
        }
        Ok(())
    }
}
impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            RemovePropW(self.tip.0, w!("TextLayout"));
        }
    }
}

pub struct Label {
    panel: Panel,
}
fn premultiply_coverage(coverage: u32, color: u32) -> u32 {
    let alpha = (coverage & 255)
        .max((coverage >> 8) & 255)
        .max((coverage >> 16) & 255);
    let red = (color & 255) * alpha / 255;
    let green = ((color >> 8) & 255) * alpha / 255;
    let blue = ((color >> 16) & 255) * alpha / 255;
    (alpha << 24) | (red << 16) | (green << 8) | blue
}
impl Label {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            panel: Panel::new()?,
        })
    }
    pub fn hide(&self) {
        self.panel.hide();
    }
    pub fn show(
        &mut self,
        caption: &str,
        region: Rect,
        color: u32,
        dpi: i32,
        cfg: &Config,
    ) -> Result<(), String> {
        let fonts = Fonts::new(
            &cfg.region_font,
            &cfg.region_fallbacks,
            cfg.region_font_size,
            dpi,
            cfg.region_bold,
            cfg.region_italic,
        )?;
        let text = Box::new(TextLayout::new(caption, fonts, region.width.max(1))?);
        let size = Rect {
            x: region.x + (region.width - text.width) / 2,
            y: region.y + (region.height - text.height) / 2,
            width: text.width,
            height: text.height,
        };
        // Render neutral glyph coverage, then premultiply the desired color. Color-keyed
        // anti-aliasing mixes text edges with the key color and leaves opaque halos.
        unsafe {
            SetPropW(self.panel.0, w!("PixelAlpha"), 1usize as HANDLE);
            let dc = GetDC(self.panel.0);
            let memory = CreateCompatibleDC(dc);
            if memory.is_null() {
                ReleaseDC(self.panel.0, dc);
                return Err(err());
            }
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: size.width,
                    biHeight: -size.height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = null_mut();
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bitmap.is_null() {
                DeleteDC(memory);
                ReleaseDC(self.panel.0, dc);
                return Err(err());
            }
            let old = SelectObject(memory, bitmap);
            let pixels = std::slice::from_raw_parts_mut(
                bits as *mut u32,
                (size.width * size.height) as usize,
            );
            pixels.fill(0);
            text.draw(memory, 0, 0, 0xffffff, true);
            GdiFlush();
            for pixel in pixels.iter_mut() {
                *pixel = premultiply_coverage(*pixel, color);
            }
            let position = POINT {
                x: size.x,
                y: size.y,
            };
            let dimensions = SIZE {
                cx: size.width,
                cy: size.height,
            };
            let origin = POINT::default();
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let ok = UpdateLayeredWindow(
                self.panel.0,
                dc,
                &position,
                &dimensions,
                memory,
                &origin,
                0,
                &blend,
                ULW_ALPHA,
            );
            let error = err();
            SelectObject(memory, old);
            DeleteObject(bitmap);
            DeleteDC(memory);
            ReleaseDC(self.panel.0, dc);
            if ok == 0 {
                return Err(error);
            }
        }
        self.panel.show(size);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::System::Threading::*;
    #[test]
    fn transparent_text_edges_have_coverage_not_a_color_key() {
        let blue = crate::colors::colorref(0x0f6cbd);
        assert_eq!(premultiply_coverage(0, blue), 0);
        assert_eq!(premultiply_coverage(0xffffff, blue), 0xff0f6cbd);
        for coverage in 1..255u32 {
            let pixel = premultiply_coverage(coverage * 0x010101, blue);
            assert_eq!(pixel >> 24, coverage);
            for shift in [0, 8, 16] {
                assert!((pixel >> shift) & 255 <= coverage);
            }
        }
    }
    fn cpu_seconds() -> f64 {
        unsafe {
            let (mut creation, mut exit, mut kernel, mut user) = (
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
            );
            assert_ne!(
                GetProcessTimes(
                    GetCurrentProcess(),
                    &mut creation,
                    &mut exit,
                    &mut kernel,
                    &mut user
                ),
                0
            );
            let ticks = |time: FILETIME| {
                (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
            };
            (ticks(kernel) + ticks(user)) as f64 / 10_000_000.0
        }
    }
    #[test]
    #[ignore = "draws crosshair windows and measures real GDI CPU usage; run in release mode"]
    fn crosshair_cpu_smoke() {
        let doc = crate::config::help()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        let cfg = Config::parse(&doc).unwrap();
        let mut overlay = Overlay::new().unwrap();
        let screen = desktop::screen(desktop::cursor().unwrap(), 0).unwrap();
        overlay.guides(3, &cfg).unwrap();
        for rate in [0.0, 30.0, 60.0] {
            overlay
                .guides(if rate == 0.0 { 0 } else { 3 }, &cfg)
                .unwrap();
            let mut clock =
                crate::framerate::FrameClock::new(if rate == 0.0 { 30.0 } else { rate }).unwrap();
            let start_cpu = cpu_seconds();
            let start = Instant::now();
            let mut updates = 0;
            let mut samples = 0;
            while start.elapsed() < Duration::from_secs(3) {
                samples += 1;
                let point = POINT {
                    x: screen.full.x + screen.full.width / 2 + samples % 100,
                    y: screen.full.y + screen.full.height / 2 + samples % 60,
                };
                if let Some(point) = clock.moved(point).unwrap() {
                    overlay.follow(point, &cfg).unwrap();
                    updates += 1;
                }
                unsafe {
                    if WaitForSingleObject(clock.handle(), 0) == WAIT_OBJECT_0
                        && let Some(point) = clock.tick()
                    {
                        overlay.follow(point, &cfg).unwrap();
                        updates += 1;
                    }
                    let mut msg = MSG::default();
                    while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let cpu = (cpu_seconds() - start_cpu) / start.elapsed().as_secs_f64() * 100.0;
            println!(
                "crosshair fps={rate} inputs={samples} frames={updates} cpu_one_core={cpu:.3}%"
            );
            assert!(updates <= (rate.max(30.0) * start.elapsed().as_secs_f64()).ceil() as i32 + 3);
        }
        overlay.guides(0, &cfg).unwrap();
    }
}
