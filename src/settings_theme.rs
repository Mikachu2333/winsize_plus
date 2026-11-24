//! Settings colors are cached on the UI thread and refreshed by Windows theme events.
use std::cell::RefCell;
use windows_sys::Win32::{
    Foundation::HWND,
    Graphics::{Dwm::*, Gdi::*},
    UI::Controls::SetWindowTheme,
};
use windows_sys::core::w;

struct Palette {
    dark: bool,
    surface: HBRUSH,
    control: HBRUSH,
    edge: HBRUSH,
}
impl Palette {
    fn new(dark: bool) -> Self {
        // SAFETY: brushes have no external owners; Drop releases them on this thread.
        unsafe {
            Self {
                dark,
                surface: CreateSolidBrush(if dark { 0x202020 } else { 0xffffff }),
                control: CreateSolidBrush(if dark { 0x303030 } else { 0xf5f5f5 }),
                edge: CreateSolidBrush(if dark { 0x606060 } else { 0xaaaaaa }),
            }
        }
    }
}
impl Drop for Palette {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.surface);
            DeleteObject(self.control);
            DeleteObject(self.edge);
        }
    }
}
thread_local! { static PALETTE: RefCell<Palette> = RefCell::new(Palette::new(crate::overlay::dark())); }
pub fn refresh() {
    PALETTE.with_borrow_mut(|p| {
        let dark = crate::overlay::dark();
        if dark != p.dark {
            *p = Palette::new(dark);
        }
    });
}
pub fn dark() -> bool {
    PALETTE.with_borrow(|p| p.dark)
}
pub fn text() -> u32 {
    if dark() { 0xffffff } else { 0x242424 }
}
pub fn hint() -> u32 {
    if dark() {
        crate::colors::TEXT_SECONDARY_DARK
    } else {
        crate::colors::TEXT_SECONDARY_LIGHT
    }
}
pub fn background() -> u32 {
    if dark() { 0x202020 } else { 0xffffff }
}
pub fn surface() -> HBRUSH {
    PALETTE.with_borrow(|p| p.surface)
}
pub fn control() -> HBRUSH {
    PALETTE.with_borrow(|p| p.control)
}
pub fn edge() -> HBRUSH {
    PALETTE.with_borrow(|p| p.edge)
}
pub fn apply(hwnd: HWND) {
    // SAFETY: Windows validates HWND, strings and the stack-owned attribute value.
    unsafe {
        SetWindowTheme(
            hwnd,
            if dark() {
                w!("DarkMode_Explorer")
            } else {
                w!("Explorer")
            },
            std::ptr::null(),
        );
        let dark = i32::from(dark());
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            (&dark as *const i32).cast(),
            size_of::<i32>() as u32,
        );
    }
}
pub fn color(dc: HDC, hint_text: bool, field: bool) -> isize {
    unsafe {
        SetTextColor(dc, if hint_text { hint() } else { text() });
        SetBkMode(dc, TRANSPARENT as i32);
        SetBkColor(
            dc,
            if field && dark() {
                0x303030
            } else {
                background()
            },
        );
    }
    (if field { control() } else { surface() }) as isize
}
