use crate::{
    config::Config,
    desktop::{self, Display, check, err},
    fonts::{Font, Fonts, TextLayout},
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
        UI::{
            Controls::{SetScrollInfo, ShowScrollBar},
            WindowsAndMessaging::*,
        },
    },
    core::w,
};

const CLASS: *const u16 = w!("WinSizeP.Overlay");

/// A viewport-sized bitmap reused throughout scrolling, never as tall as the full text.
struct PaintBuffer {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
}
impl PaintBuffer {
    fn new(hwnd: HWND, width: i32, height: i32) -> Result<Self, String> {
        // SAFETY: the window is live; the screen DC is released and bitmap selection is restored
        // on every path. Only this UI thread owns and paints into the memory DC.
        unsafe {
            let screen = GetDC(hwnd);
            if screen.is_null() {
                return Err(err());
            }
            let dc = CreateCompatibleDC(screen);
            let bitmap = CreateCompatibleBitmap(screen, width.max(1), height.max(1));
            ReleaseDC(hwnd, screen);
            if dc.is_null() || bitmap.is_null() {
                let error = err();
                if !dc.is_null() {
                    DeleteDC(dc);
                }
                if !bitmap.is_null() {
                    DeleteObject(bitmap);
                }
                return Err(error);
            }
            let previous = SelectObject(dc, bitmap);
            if previous.is_null() || previous as isize == -1 {
                let error = err();
                DeleteDC(dc);
                DeleteObject(bitmap);
                return Err(error);
            }
            Ok(Self {
                dc,
                bitmap,
                previous,
            })
        }
    }
}
impl Drop for PaintBuffer {
    fn drop(&mut self) {
        // SAFETY: restore the original selection before destroying the owned bitmap and DC.
        unsafe {
            SelectObject(self.dc, self.previous);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

struct TipContent {
    layout: TextLayout,
    buffer: PaintBuffer,
    scroll_font: Option<Font>,
}

fn paint_tip_outline(hwnd: HWND, dc: HDC) {
    // SAFETY: the borderless popup's client and window origins coincide. The full window
    // rectangle includes the native scrollbar; each caller clips to its own paint surface.
    unsafe {
        let mut window = RECT::default();
        GetWindowRect(hwnd, &mut window);
        let border = GetPropW(hwnd, w!("BorderWidth")) as usize as i32;
        let diameter = GetPropW(hwnd, w!("CornerDiameter")) as usize as i32;
        let outline = CreateRoundRectRgn(
            0,
            0,
            window.right - window.left,
            window.bottom - window.top,
            diameter,
            diameter,
        );
        if !outline.is_null() {
            SetDCBrushColor(dc, GetPropW(hwnd, w!("BorderColor")) as usize as u32);
            FrameRgn(dc, outline, GetStockObject(DC_BRUSH), border, border);
            DeleteObject(outline);
        }
    }
}

fn paint_tip_scrollbar_outline(hwnd: HWND) {
    // SAFETY: this is a live overlay; the common window DC is released after clipping out
    // the client area. Native scrollbar state is untouched.
    unsafe {
        if GetPropW(hwnd, w!("TipContent")).is_null() || GetPropW(hwnd, w!("ScrollLimit")).is_null()
        {
            return;
        }
        let dc = GetWindowDC(hwnd);
        if !dc.is_null() {
            let mut client = RECT::default();
            GetClientRect(hwnd, &mut client);
            ExcludeClipRect(dc, 0, 0, client.right, client.bottom);
            paint_tip_outline(hwnd, dc);
            ReleaseDC(hwnd, dc);
        }
    }
}

unsafe extern "system" fn paint(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_DISPLAYCHANGE | WM_SETTINGCHANGE | WM_DPICHANGED => {
                crate::input::display_changed();
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_NCHITTEST if GetPropW(hwnd, w!("ScrollLimit")).is_null() => HTTRANSPARENT as isize,
            WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
            WM_VSCROLL => {
                let mut info = SCROLLINFO {
                    cbSize: size_of::<SCROLLINFO>() as u32,
                    fMask: SIF_ALL,
                    ..Default::default()
                };
                if GetScrollInfo(hwnd, SB_VERT, &mut info) != 0 {
                    let position = match (wp & 0xffff) as i32 {
                        SB_LINEUP => info.nPos.saturating_sub(40),
                        SB_LINEDOWN => info.nPos.saturating_add(40),
                        SB_PAGEUP => info.nPos.saturating_sub(info.nPage as i32),
                        SB_PAGEDOWN => info.nPos.saturating_add(info.nPage as i32),
                        SB_TOP => 0,
                        SB_BOTTOM => i32::MAX,
                        SB_THUMBPOSITION | SB_THUMBTRACK => info.nTrackPos,
                        _ => info.nPos,
                    };
                    set_tip_offset(hwnd, position);
                }
                0
            }
            WM_MOUSEWHEEL => {
                let current = GetPropW(hwnd, w!("TextOffset")) as usize as i32;
                let delta = (wp >> 16) as i16 as i32;
                set_tip_offset(hwnd, current.saturating_sub(delta));
                0
            }
            WM_ERASEBKGND => 1,
            WM_NCPAINT | WM_NCMOUSEMOVE | WM_NCMOUSELEAVE | WM_NCLBUTTONDOWN | WM_NCLBUTTONUP => {
                let result = DefWindowProcW(hwnd, msg, wp, lp);
                paint_tip_scrollbar_outline(hwnd);
                result
            }
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let target = BeginPaint(hwnd, &mut paint);
                if !GetPropW(hwnd, w!("PixelAlpha")).is_null() {
                    EndPaint(hwnd, &paint);
                    return 0;
                }
                let mut area = RECT::default();
                GetClientRect(hwnd, &mut area);
                // SAFETY: Overlay owns the boxed content and removes the property before dropping
                // it. Painting and replacement both run on the window's UI thread.
                let content = (GetPropW(hwnd, w!("TipContent")) as *const TipContent).as_ref();
                let dc = content.map_or(target, |content| content.buffer.dc);
                let buffer_state = content.map(|_| {
                    let state = SaveDC(dc);
                    IntersectClipRect(
                        dc,
                        paint.rcPaint.left,
                        paint.rcPaint.top,
                        paint.rcPaint.right,
                        paint.rcPaint.bottom,
                    );
                    state
                });
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
                if let Some(content) = content {
                    let border = GetPropW(hwnd, w!("BorderWidth")) as usize as i32;
                    paint_tip_outline(hwnd, dc);
                    let left = GetPropW(hwnd, w!("PaddingX")) as usize as i32;
                    let top = GetPropW(hwnd, w!("PaddingTop")) as usize as i32;
                    let color = GetPropW(hwnd, w!("TextColor")) as usize as u32;
                    let saved = SaveDC(dc);
                    let bottom = GetPropW(hwnd, w!("PaddingBottom")) as usize as i32;
                    IntersectClipRect(dc, left, top, area.right - left, area.bottom - bottom);
                    let offset = GetPropW(hwnd, w!("TextOffset")) as usize as i32;
                    content.layout.draw(dc, left, top - offset, color, false);
                    RestoreDC(dc, saved);
                    if let Some(font) = &content.scroll_font {
                        let old = SelectObject(dc, font.0);
                        let mut footer = RECT {
                            left,
                            top: area.bottom - bottom,
                            right: area.right - left,
                            bottom: area.bottom - border,
                        };
                        SetBkMode(dc, TRANSPARENT as i32);
                        SetTextColor(dc, color);
                        DrawTextW(
                            dc,
                            w!("PgUp/PgDn  ↑↓  Home/End  ·  Esc"),
                            -1,
                            &mut footer,
                            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
                        );
                        SelectObject(dc, old);
                    }
                }
                if let Some(state) = buffer_state {
                    RestoreDC(dc, state);
                    let rect = paint.rcPaint;
                    BitBlt(
                        target,
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        dc,
                        rect.left,
                        rect.top,
                        SRCCOPY,
                    );
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
    pub(crate) fn round(&self, width: i32, height: i32, radius: i32) -> Result<(), String> {
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
    content: Option<Box<TipContent>>,
    scroll_limit: i32,
    tip_text: String,
    tip_display: Option<Display>,
    tip_epoch: u64,
    segments: Vec<Panel>,
    mode: u8,
    last_monitor: HMONITOR,
    display: Option<Display>,
    display_epoch: u64,
    _renderer: crate::label_text::Session,
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
                content: None,
                scroll_limit: 0,
                tip_text: String::new(),
                tip_display: None,
                tip_epoch: desktop::display_epoch(),
                segments: Vec::new(),
                mode: 0,
                last_monitor: null_mut(),
                display: None,
                display_epoch: desktop::display_epoch(),
                _renderer: crate::label_text::Session::new(),
            })
        }
    }
    pub fn hide_tip(&self) {
        crate::input::tip_visible(false);
        self.tip.hide();
        self.shadow.hide()
    }
    #[cfg(test)]
    pub fn tip_visible(&self) -> bool {
        self.tip.1.get().is_some()
    }
    #[cfg(test)]
    pub fn tip_text(&self) -> &str {
        &self.tip_text
    }
    pub fn tip(&mut self, text: &str, cfg: &Config) -> Result<(), String> {
        let screen = desktop::screen(desktop::cursor()?, 0)?;
        self.tip_on(text, cfg, screen)
    }
    fn tip_on(&mut self, text: &str, cfg: &Config, screen: Display) -> Result<(), String> {
        self.tip_text = text.to_owned();
        let dpi = screen.dpi;
        let border = scale(1, dpi).max(1);
        let horizontal = scale(10, dpi) + border;
        let top = scale(4, dpi) + border;
        let mut bottom = scale(6, dpi) + border;
        let radius = scale(12, dpi);
        let make_layout = |width| {
            let fonts = Fonts::opaque(
                &cfg.font,
                &cfg.font_fallbacks,
                cfg.font_size,
                dpi,
                cfg.font_bold,
                cfg.font_italic,
            )?;
            TextLayout::new(text, fonts, width)
        };
        let max_height = tip_height_limit(screen.work.height);
        let available_width = (screen.work.width - 2 * horizontal).max(1);
        let mut text = make_layout(available_width)?;
        let scrollable = text.height + top + bottom > max_height;
        let scrollbar = if scrollable {
            // SAFETY: querying a metric does not retain pointers or resources.
            unsafe {
                windows_sys::Win32::UI::HiDpi::GetSystemMetricsForDpi(SM_CXVSCROLL, dpi as u32)
            }
            .max(1)
        } else {
            0
        };
        if scrollable && text.width + scrollbar > available_width {
            text = make_layout((available_width - scrollbar).max(1))?;
        }
        let width = (text.width + 2 * horizontal + scrollbar).min(screen.work.width);
        let height = (text.height + top + bottom).min(max_height);
        let scroll_font = if scrollable {
            let font = Font::new("SystemFont", 0, dpi, false, false)?;
            bottom += font.height() + scale(6, dpi);
            Some(font)
        } else {
            None
        };
        self.scroll_limit = (text.height - (height - top - bottom).max(1)).max(0);
        let index = if cfg.theme == "dark" || (cfg.theme == "system" && dark()) {
            2
        } else {
            0
        };
        let content = Box::new(TipContent {
            layout: text,
            buffer: PaintBuffer::new(self.tip.0, width, height)?,
            scroll_font,
        });
        unsafe {
            let style = GetWindowLongW(self.tip.0, GWL_EXSTYLE) as u32;
            SetWindowLongW(
                self.tip.0,
                GWL_EXSTYLE,
                if scrollable {
                    style & !WS_EX_TRANSPARENT
                } else {
                    style | WS_EX_TRANSPARENT
                } as i32,
            );
            check(SetPropW(
                self.tip.0,
                w!("TipContent"),
                (&*content as *const TipContent) as HANDLE,
            ))?;
            for (property, value) in [
                (w!("PaddingX"), horizontal as usize),
                (w!("PaddingTop"), top as usize),
                (w!("PaddingBottom"), bottom as usize),
                (w!("BorderWidth"), border as usize),
                (w!("CornerDiameter"), radius as usize),
                (w!("TextOffset"), 0),
                (w!("ScrollLimit"), self.scroll_limit as usize),
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
            let info = SCROLLINFO {
                cbSize: size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                nMax: content.layout.height.saturating_sub(1),
                nPage: (height - top - bottom).max(1) as u32,
                ..Default::default()
            };
            SetScrollInfo(self.tip.0, SB_VERT, &info, 1);
            ShowScrollBar(self.tip.0, SB_VERT, i32::from(scrollable));
        }
        self.content = Some(content);
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
        // A new buffer has no preserved pixels when Windows resizes an existing popup.
        // Invalidate after resizing so even newly exposed or retained edges are rendered.
        unsafe {
            InvalidateRect(self.tip.0, null(), 0);
        }
        crate::input::tip_visible(true);
        crate::input::tip_scrollable(self.scroll_limit > 0, self.tip.0 as usize);
        self.tip_display = Some(screen);
        self.tip_epoch = desktop::display_epoch();
        Ok(())
    }
    fn refresh_tip(&mut self, cfg: &Config) -> Result<(), String> {
        if self.tip.1.get().is_none() {
            return Ok(());
        }
        if self.tip_epoch == desktop::display_epoch()
            && self.tip_display.as_ref().is_some_and(|d| {
                d.dpi
                    == unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(self.tip.0) } as i32
            })
        {
            return Ok(());
        }
        let screen =
            desktop::display(unsafe { MonitorFromWindow(self.tip.0, MONITOR_DEFAULTTONEAREST) })?;
        if self.tip_display.as_ref().is_some_and(|old| {
            old.dpi == screen.dpi && old.work == screen.work && old.number == screen.number
        }) {
            self.tip_epoch = desktop::display_epoch();
            return Ok(());
        }
        let offset = unsafe { GetPropW(self.tip.0, w!("TextOffset")) } as usize as i32;
        let old_limit = self.scroll_limit.max(1);
        self.tip_on(&self.tip_text.clone(), cfg, screen)?;
        let offset =
            (i64::from(offset) * i64::from(self.scroll_limit) / i64::from(old_limit)) as i32;
        set_tip_offset(self.tip.0, offset);
        Ok(())
    }
    pub fn scroll_tip(&mut self, delta: i32) {
        // SAFETY: the panel owns a live window; mouse and keyboard scrolling share its offset.
        unsafe {
            let current = GetPropW(self.tip.0, w!("TextOffset")) as usize as i32;
            let delta = if matches!(delta, -400 | 400) {
                let mut info = SCROLLINFO {
                    cbSize: size_of::<SCROLLINFO>() as u32,
                    fMask: SIF_PAGE,
                    ..Default::default()
                };
                GetScrollInfo(self.tip.0, SB_VERT, &mut info);
                delta.signum() * (info.nPage as i32).max(1)
            } else {
                delta
            };
            set_tip_offset(self.tip.0, scroll_offset(current, delta, self.scroll_limit));
        }
    }
    pub fn tip_scrollable(&self) -> bool {
        self.tip.1.get().is_some() && self.scroll_limit > 0
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
        self.refresh_tip(cfg)?;
        if self.mode == 0 {
            return Ok(());
        }
        let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
        if self.last_monitor != monitor || self.display_epoch != desktop::display_epoch() {
            self.display = Some(desktop::display(monitor)?);
            self.last_monitor = monitor;
            self.display_epoch = desktop::display_epoch();
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
            // Keep two surfaces at monitor length; only their thickness is clipped
            // at screen edges. Most frames change position and the transparent gap.
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
fn scroll_offset(current: i32, delta: i32, limit: i32) -> i32 {
    current.saturating_add(delta).clamp(0, limit)
}
fn tip_height_limit(work_height: i32) -> i32 {
    (work_height / 3).max(1)
}
fn set_tip_offset(hwnd: HWND, position: i32) {
    // SAFETY: callers pass a live overlay window, and all state is stored on its UI thread.
    unsafe {
        let limit = GetPropW(hwnd, w!("ScrollLimit")) as usize as i32;
        let position = position.clamp(0, limit);
        if position == GetPropW(hwnd, w!("TextOffset")) as usize as i32 {
            return;
        }
        SetPropW(hwnd, w!("TextOffset"), position as usize as HANDLE);
        let info = SCROLLINFO {
            cbSize: size_of::<SCROLLINFO>() as u32,
            fMask: SIF_POS,
            nPos: position,
            ..Default::default()
        };
        SetScrollInfo(hwnd, SB_VERT, &info, 1);
        // SetScrollInfo can paint the native bar directly without sending WM_NCPAINT.
        paint_tip_scrollbar_outline(hwnd);
        let mut content = RECT::default();
        GetClientRect(hwnd, &mut content);
        let horizontal = GetPropW(hwnd, w!("PaddingX")) as usize as i32;
        content.left += horizontal;
        content.right -= horizontal;
        content.top += GetPropW(hwnd, w!("PaddingTop")) as usize as i32;
        content.bottom -= GetPropW(hwnd, w!("PaddingBottom")) as usize as i32;
        InvalidateRect(hwnd, &content, 0);
    }
}
impl Drop for Overlay {
    fn drop(&mut self) {
        if self.tip.1.get().is_some() {
            crate::input::tip_visible(false);
        }
        unsafe {
            RemovePropW(self.tip.0, w!("TipContent"));
        }
    }
}

pub struct Label {
    panel: Panel,
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
        let image = crate::label_text::render(caption, region.width, dpi, color, cfg)?;
        let size = Rect {
            x: region.x + (region.width - image.width) / 2,
            y: region.y + (region.height - image.height) / 2,
            width: image.width,
            height: image.height,
        };
        self.image(&image, size.x, size.y, 255)
    }
    pub(crate) fn image(
        &self,
        image: &crate::label_text::Image,
        x: i32,
        y: i32,
        alpha: u8,
    ) -> Result<(), String> {
        let size = Rect {
            x,
            y,
            width: image.width,
            height: image.height,
        };
        // SAFETY: Direct2D has finished drawing into the owned premultiplied bitmap.
        // UpdateLayeredWindow copies it synchronously before Image releases its DC.
        unsafe {
            SetPropW(self.panel.0, w!("PixelAlpha"), 1usize as HANDLE);
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
                SourceConstantAlpha: alpha,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            check(UpdateLayeredWindow(
                self.panel.0,
                null_mut(),
                &position,
                &dimensions,
                image.dc,
                &origin,
                0,
                &blend,
                ULW_ALPHA,
            ))?;
        }
        self.panel.show(size);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "opens a temporary tip and measures synchronous scrolling paints"]
    fn tip_scrolling_paint_diagnostics() {
        let cfg = Config::parse(&crate::config::help().parse().unwrap()).unwrap();
        let mut overlay = Overlay::new().unwrap();
        for (name, text) in [
            ("help", cfg.help_text()),
            ("4500 lines", "Long tip to scroll\n".repeat(4500)),
        ] {
            overlay.tip(&text, &cfg).unwrap();
            // SAFETY: all handles are owned by the test and used on their creating UI thread.
            unsafe {
                UpdateWindow(overlay.tip.0);
                let start = Instant::now();
                for frame in 0..120 {
                    set_tip_offset(
                        overlay.tip.0,
                        (i64::from(overlay.scroll_limit) * (frame % 60) / 59) as i32,
                    );
                    UpdateWindow(overlay.tip.0);
                }
                GdiFlush();
                println!("{name}: 120 scrolling paints in {:?}", start.elapsed());
                overlay.hide_tip();
            }
        }
    }
    #[test]
    #[ignore = "opens temporary tips and compares buffered repaint pixels and GDI resource lifetime"]
    fn buffered_tip_partial_paints_match_full_paints_without_leaking_gdi_objects() {
        let cfg = Config::parse(&crate::config::help().parse().unwrap()).unwrap();
        // SAFETY: test windows, DCs, and their content are owned and used on this UI thread.
        unsafe {
            // Warm up the GDI font cache before counting resources owned by the overlay.
            let mut warmup = Overlay::new().unwrap();
            warmup.tip(&cfg.help_text(), &cfg).unwrap();
            UpdateWindow(warmup.tip.0);
            warmup.hide_tip();
            drop(warmup);
            let before = GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS);
            let mut overlay = Overlay::new().unwrap();
            for theme in ["light", "dark"] {
                let mut themed = Config::parse(&crate::config::help().parse().unwrap()).unwrap();
                themed.theme = theme.into();
                overlay.tip(&themed.help_text(), &themed).unwrap();
                UpdateWindow(overlay.tip.0);
                let dc = overlay.content.as_ref().unwrap().buffer.dc;
                let mut client = RECT::default();
                GetClientRect(overlay.tip.0, &mut client);
                let snapshot = || {
                    (0..client.bottom)
                        .flat_map(|y| (0..client.right).map(move |x| GetPixel(dc, x, y)))
                        .collect::<Vec<_>>()
                };
                for position in [17, overlay.scroll_limit / 2, overlay.scroll_limit, 0] {
                    set_tip_offset(overlay.tip.0, position);
                    UpdateWindow(overlay.tip.0);
                    let partial = snapshot();
                    InvalidateRect(overlay.tip.0, null(), 0);
                    UpdateWindow(overlay.tip.0);
                    assert_eq!(partial, snapshot(), "{theme}, offset {position}");
                }
            }
            overlay.hide_tip();
            drop(overlay);
            assert_eq!(GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS), before);
        }
    }
    #[test]
    #[ignore = "opens temporary tips to verify a continuous outer border across the native scrollbar"]
    fn scrollbar_shares_the_tip_outer_corners() {
        // SAFETY: this test owns each overlay and queries only its live window and memory DC.
        unsafe {
            for theme in ["light", "dark"] {
                let mut cfg = Config::parse(&crate::config::help().parse().unwrap()).unwrap();
                cfg.theme = theme.into();
                let mut overlay = Overlay::new().unwrap();
                overlay.tip(&"A line of text\n".repeat(100), &cfg).unwrap();
                UpdateWindow(overlay.tip.0);
                let mut client = RECT::default();
                GetClientRect(overlay.tip.0, &mut client);
                let buffer = overlay.content.as_ref().unwrap().buffer.dc;
                let background = GetWindowLongPtrW(overlay.tip.0, GWLP_USERDATA) as u32;
                let border_color = GetPropW(overlay.tip.0, w!("BorderColor")) as usize as u32;
                let border = GetPropW(overlay.tip.0, w!("BorderWidth")) as usize as i32;
                let diameter = GetPropW(overlay.tip.0, w!("CornerDiameter")) as usize as i32;
                // No inner rounded edge or vertical stroke at the text/scrollbar junction.
                for y in border..diameter {
                    assert_eq!(GetPixel(buffer, client.right - 1, y), background);
                    assert_eq!(
                        GetPixel(buffer, client.right - 1, client.bottom - 2 - y),
                        background
                    );
                }
                assert_eq!(
                    GetPixel(buffer, client.right - 1, client.bottom / 2),
                    background
                );
                assert_eq!(GetPixel(buffer, client.right - 1, 0), border_color);
                let window = overlay.tip.1.get().unwrap();
                assert!(window.width > client.right);
                let region = CreateRectRgn(0, 0, 0, 0);
                assert_ne!(GetWindowRgn(overlay.tip.0, region), 0);
                assert_eq!(PtInRegion(region, window.width - 1, 0), 0);
                assert_ne!(PtInRegion(region, client.right - 1, border), 0);
                DeleteObject(region);
                // The non-client border resumes on the scrollbar's outer side.
                for offset in [0, overlay.scroll_limit / 2, overlay.scroll_limit] {
                    set_tip_offset(overlay.tip.0, offset);
                    UpdateWindow(overlay.tip.0);
                    let dc = GetWindowDC(overlay.tip.0);
                    let outer = GetPixel(dc, window.width - 2, window.height / 2);
                    ReleaseDC(overlay.tip.0, dc);
                    assert_eq!(outer, border_color, "{theme}, scroll {offset}");
                }
                overlay.tip("Short tip", &cfg).unwrap();
                UpdateWindow(overlay.shadow.0);
                UpdateWindow(overlay.tip.0);
                GetClientRect(overlay.tip.0, &mut client);
                assert_eq!(overlay.tip.1.get().unwrap().width, client.right);
                assert_eq!(
                    GetPixel(
                        overlay.content.as_ref().unwrap().buffer.dc,
                        client.right - 2,
                        client.bottom / 2
                    ),
                    border_color
                );
                overlay.hide_tip();
            }
        }
    }
    #[test]
    fn tips_never_exceed_one_third_of_the_work_area() {
        for height in [480, 720, 1040, 1080, 1440, 2160] {
            assert!(super::tip_height_limit(height) * 3 <= height);
        }
    }
    #[test]
    #[ignore = "opens a temporary tip to verify native scrollbar interaction"]
    fn tip_scrollbar_tracks_mouse_keyboard_and_shorter_content() {
        let cfg = Config::parse(&crate::config::help().parse().unwrap()).unwrap();
        let screen = desktop::screen(desktop::cursor().unwrap(), 0).unwrap();
        let mut overlay = Overlay::new().unwrap();
        // SAFETY: this test owns the windows and sends synchronous messages on their UI thread.
        unsafe {
            let foreground = GetForegroundWindow();
            overlay.tip(&"Long tip\n".repeat(4500), &cfg).unwrap();
            let rect = overlay.tip.1.get().unwrap();
            assert!(rect.height <= screen.work.height / 3);
            assert!(overlay.tip_scrollable());
            assert!(overlay.scroll_limit > 65535);
            assert_ne!(
                GetWindowLongW(overlay.tip.0, GWL_STYLE) as u32 & WS_VSCROLL,
                0
            );
            assert_eq!(GetForegroundWindow(), foreground);
            assert_eq!(
                SendMessageW(overlay.tip.0, WM_MOUSEACTIVATE, 0, 0),
                MA_NOACTIVATE as isize
            );
            let mut client = RECT::default();
            GetClientRect(overlay.tip.0, &mut client);
            let padding = GetPropW(overlay.tip.0, w!("PaddingX")) as usize as i32;
            assert!(overlay.content.as_ref().unwrap().layout.width <= client.right - padding * 2);
            let buffer = overlay.content.as_ref().unwrap().buffer.dc;
            UpdateWindow(overlay.tip.0);
            for command in [SB_TOP, SB_LINEUP, SB_PAGEUP, SB_ENDSCROLL] {
                SendMessageW(overlay.tip.0, WM_VSCROLL, command as usize, 0);
                assert_eq!(GetUpdateRect(overlay.tip.0, null_mut(), 0), 0);
            }
            overlay.scroll_tip(-400);
            SendMessageW(overlay.tip.0, WM_MOUSEWHEEL, 120usize << 16, 0);
            assert_eq!(GetUpdateRect(overlay.tip.0, null_mut(), 0), 0);
            SendMessageW(overlay.tip.0, WM_VSCROLL, SB_BOTTOM as usize, 0);
            let offset = |hwnd| GetPropW(hwnd, w!("TextOffset")) as usize as i32;
            assert_eq!(offset(overlay.tip.0), overlay.scroll_limit);
            let mut dirty = RECT::default();
            assert_ne!(GetUpdateRect(overlay.tip.0, &mut dirty, 0), 0);
            assert_eq!(dirty.left, padding);
            assert_eq!(dirty.right, client.right - padding);
            assert!(dirty.top > 0 && dirty.bottom < client.bottom);
            UpdateWindow(overlay.tip.0);
            for command in [SB_BOTTOM, SB_LINEDOWN, SB_PAGEDOWN, SB_ENDSCROLL] {
                SendMessageW(overlay.tip.0, WM_VSCROLL, command as usize, 0);
                assert_eq!(GetUpdateRect(overlay.tip.0, null_mut(), 0), 0);
            }
            // The visible viewport must contain text after a large jump, not just its background.
            let background = GetWindowLongPtrW(overlay.tip.0, GWLP_USERDATA) as u32;
            assert!(
                (dirty.top..dirty.bottom).any(
                    |y| (dirty.left..dirty.right).any(|x| GetPixel(buffer, x, y) != background)
                )
            );
            overlay.scroll_tip(-40);
            assert_eq!(offset(overlay.tip.0), overlay.scroll_limit - 40);
            let mut info = SCROLLINFO {
                cbSize: size_of::<SCROLLINFO>() as u32,
                fMask: SIF_ALL,
                ..Default::default()
            };
            assert_ne!(GetScrollInfo(overlay.tip.0, SB_VERT, &mut info), 0);
            assert_eq!(info.nPos, offset(overlay.tip.0));
            SendMessageW(overlay.tip.0, WM_VSCROLL, SB_TOP as usize, 0);
            overlay.scroll_tip(400);
            assert_eq!(offset(overlay.tip.0), info.nPage as i32);
            overlay.scroll_tip(-400);
            assert_eq!(offset(overlay.tip.0), 0);
            SendMessageW(
                overlay.tip.0,
                WM_MOUSEWHEEL,
                (u32::from((-120i16) as u16) << 16) as usize,
                0,
            );
            assert_eq!(offset(overlay.tip.0), 120);
            assert_eq!(overlay.content.as_ref().unwrap().buffer.dc, buffer);
            overlay.tip("Short tip", &cfg).unwrap();
            assert!(!overlay.tip_scrollable());
            assert_eq!(offset(overlay.tip.0), 0);
            assert_eq!(
                GetWindowLongW(overlay.tip.0, GWL_STYLE) as u32 & WS_VSCROLL,
                0
            );
            overlay.hide_tip();
        }
    }
    #[test]
    fn long_tips_can_reach_both_ends_without_overflow() {
        assert_eq!(super::scroll_offset(0, i32::MAX, 11804), 11804);
        assert_eq!(super::scroll_offset(11804, i32::MIN, 11804), 0);
        assert_eq!(super::scroll_offset(400, -40, 11804), 360);
        assert_eq!(super::scroll_offset(0, 400, 0), 0);
    }
    use super::*;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::System::Threading::*;
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

#[cfg(test)]
pub(crate) fn label_rect(label: &Label) -> Rect {
    desktop::bounds(label.panel.0).unwrap().0
}

#[cfg(test)]
mod multi_monitor_regressions {
    use super::*;
    use crate::test_support::{Dpi, displays, point};

    #[test]
    #[ignore = "requires an interactive desktop; some cases need mixed DPI monitors"]
    fn crosshair_tracks_both_real_dpis_and_monitor_edges() {
        let _dpi = Dpi::new();
        let cfg = crate::test_support::cfg();
        let mut overlay = Overlay::new().unwrap();
        overlay.guides(3, &cfg).unwrap();
        for (_, d) in displays() {
            for p in [
                point(&d),
                POINT {
                    x: d.full.x,
                    y: d.full.y,
                },
                POINT {
                    x: d.full.x + d.full.width - 1,
                    y: d.full.y + d.full.height - 1,
                },
            ] {
                overlay.follow(p, &cfg).unwrap();
                for panel in &overlay.segments {
                    let actual = desktop::bounds(panel.0).unwrap().0;
                    assert!(actual.x >= d.full.x && actual.y >= d.full.y, "{actual:?}");
                    assert!(
                        actual.x + actual.width <= d.full.x + d.full.width,
                        "{actual:?}"
                    );
                    assert!(
                        actual.y + actual.height <= d.full.y + d.full.height,
                        "{actual:?}"
                    );
                }
            }
        }
        overlay.guides(0, &cfg).unwrap();
    }

    #[test]
    #[ignore = "requires an interactive desktop; some cases need mixed DPI monitors"]
    fn visible_tip_relayouts_after_dpi_change() {
        let _dpi = Dpi::new();
        let Some(all) = crate::test_support::require_displays(true) else {
            return;
        };
        let cfg = crate::test_support::cfg();
        let mut overlay = Overlay::new().unwrap();
        overlay.tip("Monitor change audit", &cfg).unwrap();
        let old_dpi =
            unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(overlay.tip.0) } as i32;
        let destination = &all.iter().find(|(_, d)| d.dpi != old_dpi).unwrap().1;
        let before = overlay.content.as_ref().unwrap().layout.height;
        let rect = overlay.tip.1.get().unwrap();
        overlay.tip.show(Rect {
            x: destination.work.x + 100,
            y: destination.work.y + 100,
            ..rect
        });
        crate::test_support::pump();
        // The resident's DISPLAY_CHANGED branch calls follow and preview.tick only.
        overlay.follow(point(destination), &cfg).unwrap();
        let actual = overlay.content.as_ref().unwrap().layout.height;
        let expected = TextLayout::new(
            "Monitor change audit",
            Fonts::opaque(
                &cfg.font,
                &cfg.font_fallbacks,
                cfg.font_size,
                destination.dpi,
                cfg.font_bold,
                cfg.font_italic,
            )
            .unwrap(),
            destination.work.width,
        )
        .unwrap()
        .height;
        let new_dpi =
            unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(overlay.tip.0) } as i32;
        overlay.hide_tip();
        println!(
            "tip dpi={old_dpi}->{new_dpi} old_height={before} actual_height={actual} expected_height={expected}"
        );
        assert_eq!(new_dpi, destination.dpi);
        assert_eq!(
            actual, expected,
            "Visible tip retains its old text layout across a real WM_DPICHANGED"
        );
    }
}
