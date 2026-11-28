use crate::fonts::Font;
use crate::{config::Config, desktop, input, layout::Layout};
use std::{
    cell::RefCell,
    ptr::{null, null_mut},
    sync::atomic::{AtomicUsize, Ordering},
};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{Controls::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
    core::w,
};

pub enum Command {
    Action(String),
    Layout(Layout),
}

pub fn parse(text: &str, cfg: &Config) -> Result<Command, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(cfg
            .tr(
                "Enter a region, expression or command",
                "请输入分区名称、表达式或命令",
            )
            .into());
    }
    let name = crate::config::command_name(text);
    if cfg.presets.contains_key(&name) {
        return Ok(Command::Action(name));
    }
    if text.contains(['|', '｜', '、', '\\']) {
        return text.parse().map(Command::Layout).map_err(|err| {
            format!(
                "{}: {err}",
                cfg.tr("Invalid region expression", "分区表达式无效")
            )
        });
    }
    let action = cfg.action(text);
    if crate::config::ACTIONS.contains(&action.as_str()) || cfg.presets.contains_key(&action) {
        Ok(Command::Action(action))
    } else {
        Err(format!(
            "{}: {text}",
            cfg.tr("Unknown region or command", "未知分区或命令")
        ))
    }
}

struct Appearance {
    family: String,
    points: i32,
    bold: bool,
    italic: bool,
    font: Font,
    theme: String,
    colors: [u32; 4],
    background: u32,
    foreground: u32,
    brush: HBRUSH,
}
impl Appearance {
    fn update_colors(&mut self) {
        let dark = self.theme == "dark" || (self.theme == "system" && crate::overlay::dark());
        self.background = self.colors[if dark { 2 } else { 0 }];
        self.foreground = self.colors[if dark { 3 } else { 1 }];
        unsafe {
            if !self.brush.is_null() {
                DeleteObject(self.brush);
            }
            self.brush = CreateSolidBrush(self.background);
        }
    }
}
impl Drop for Appearance {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.brush);
        }
    }
}
pub struct Prompt {
    pub window: HWND,
    edit: HWND,
    status: HWND,
    appearance: Box<RefCell<Appearance>>,
    original_style: i32,
    original_ex_style: i32,
    pub token: usize,
}
impl Prompt {
    pub fn attach(window: HWND, cfg: &Config, target: HWND) -> Result<Self, String> {
        let screen = desktop::screen(desktop::cursor()?, 0)?;
        let font = Font::new(
            &cfg.font,
            cfg.font_size,
            screen.dpi,
            cfg.font_bold,
            cfg.font_italic,
        )?;
        let mut style = Appearance {
            family: cfg.font.clone(),
            points: cfg.font_size,
            bold: cfg.font_bold,
            italic: cfg.font_italic,
            font,
            theme: cfg.theme.clone(),
            colors: cfg.colors,
            background: 0,
            foreground: 0,
            brush: null_mut(),
        };
        style.update_colors();
        let appearance = Box::new(RefCell::new(style));
        // SAFETY: the overlay owns HWND. This attachment owns only its children
        // and removes its subclass/properties before freeing the boxed appearance.
        unsafe {
            let instance = GetModuleHandleW(null());
            let original_style = GetWindowLongW(window, GWL_STYLE);
            let original_ex_style = GetWindowLongW(window, GWL_EXSTYLE);
            static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(1);
            let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
            let mut prompt = Self {
                window,
                edit: null_mut(),
                status: null_mut(),
                appearance,
                original_style,
                original_ex_style,
                token,
            };
            desktop::check(SetPropW(
                window,
                w!("PromptAppearance"),
                (&*prompt.appearance as *const RefCell<Appearance>) as HANDLE,
            ))?;
            desktop::check(SetPropW(window, w!("CommandToken"), token as HANDLE))?;
            desktop::check(SetWindowSubclass(window, Some(window_proc), 3, 0))?;
            SetWindowLongW(
                window,
                GWL_STYLE,
                (original_style as u32 | WS_CLIPCHILDREN) as i32,
            );
            SetWindowLongW(
                window,
                GWL_EXSTYLE,
                (original_ex_style as u32 & !(WS_EX_NOACTIVATE | WS_EX_TRANSPARENT)) as i32,
            );
            let display = desktop::bounds(target)
                .ok()
                .and_then(|(_, rect)| {
                    desktop::screen(
                        POINT {
                            x: rect.x + rect.width / 2,
                            y: rect.y + rect.height / 2,
                        },
                        0,
                    )
                    .ok()
                })
                .map_or(screen.number, |d| d.number);
            let target_text = format!(
                "{}: {}  ·  {} {display}",
                cfg.tr("Target", "目标"),
                desktop::window_text(target, "{title} — {process}"),
                cfg.tr("Screen", "屏幕")
            );
            let target_label = CreateWindowExW(
                0,
                w!("STATIC"),
                desktop::wide(target_text).as_ptr(),
                WS_CHILD | WS_VISIBLE | 0x4000, // SS_ENDELLIPSIS from WinUser.h.
                0,
                0,
                1,
                1,
                window,
                3 as HMENU,
                instance,
                null(),
            );
            prompt.edit = CreateWindowExW(
                0,
                w!("EDIT"),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
                0,
                0,
                1,
                1,
                window,
                1 as HMENU,
                instance,
                null(),
            );
            prompt.status = CreateWindowExW(
                0,
                w!("EDIT"),
                desktop::wide(cfg.tr(
                    "Enter to apply · Esc to cancel · Switching windows cancels",
                    "Enter 确认 · Esc 取消 · 切换窗口时取消",
                ))
                .as_ptr(),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_VSCROLL
                    | ES_MULTILINE as u32
                    | ES_AUTOVSCROLL as u32
                    | ES_READONLY as u32,
                0,
                0,
                1,
                1,
                window,
                2 as HMENU,
                instance,
                null(),
            );
            if target_label.is_null() || prompt.edit.is_null() || prompt.status.is_null() {
                return Err(desktop::err());
            }
            desktop::check(SetWindowSubclass(prompt.edit, Some(edit_proc), 1, 0))?;
            desktop::check(SetWindowSubclass(prompt.status, Some(edit_proc), 1, 0))?;
            for child in [target_label, prompt.edit, prompt.status] {
                SendMessageW(
                    child,
                    WM_SETFONT,
                    prompt.appearance.borrow().font.0 as usize,
                    1,
                );
            }
            SendMessageW(prompt.edit, EM_SETLIMITTEXT, 4096, 0);
            layout(window, &prompt.appearance.borrow());
            ShowWindow(window, SW_SHOW);
            SetForegroundWindow(window);
            SetFocus(prompt.edit);
            Ok(prompt)
        }
    }
    pub fn text(&self) -> String {
        unsafe {
            let mut text = vec![0; GetWindowTextLengthW(self.edit).max(0) as usize + 1];
            let n = GetWindowTextW(self.edit, text.as_mut_ptr(), text.len() as i32);
            String::from_utf16_lossy(&text[..n.max(0) as usize])
        }
    }
    pub fn error(&self, text: &str) {
        unsafe {
            SetWindowTextW(self.status, desktop::wide(text).as_ptr());
            SetFocus(self.edit);
        }
    }
}
impl Drop for Prompt {
    fn drop(&mut self) {
        unsafe {
            RemoveWindowSubclass(self.window, Some(window_proc), 3);
            RemovePropW(self.window, w!("PromptAppearance"));
            RemovePropW(self.window, w!("CommandToken"));
            let shadow = RemovePropW(self.window, w!("CommandShadow"));
            if !shadow.is_null() {
                ShowWindow(shadow, SW_HIDE);
            }
            for child in [self.edit, self.status, GetDlgItem(self.window, 3)] {
                if !child.is_null() {
                    DestroyWindow(child);
                }
            }
            SetWindowLongW(self.window, GWL_STYLE, self.original_style);
            SetWindowLongW(self.window, GWL_EXSTYLE, self.original_ex_style);
            ShowWindow(self.window, SW_HIDE);
        }
    }
}
fn layout(hwnd: HWND, style: &Appearance) {
    unsafe {
        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let padding = 12 * dpi / 96;
        let mut rect = RECT::default();
        GetClientRect(hwnd, &mut rect);
        let line = style.font.height().max(18 * dpi / 96);
        let width = (rect.right - 2 * padding).max(1);
        MoveWindow(GetDlgItem(hwnd, 3), padding, padding, width, line + 4, 1);
        MoveWindow(
            GetDlgItem(hwnd, 1),
            padding,
            padding + line + 12,
            width,
            line + 12,
            1,
        );
        let top = padding + line * 2 + 36;
        MoveWindow(
            GetDlgItem(hwnd, 2),
            padding,
            top,
            width,
            (rect.bottom - top - padding).max(line),
            1,
        );
    }
}
fn sync_frame(hwnd: HWND) {
    // SAFETY: both panels belong to Overlay on this UI thread. Successful
    // SetWindowRgn calls take ownership of the region, failures release it here.
    unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect) == 0 {
            return;
        }
        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let shadow = GetPropW(hwnd, w!("CommandShadow"));
        for panel in [hwnd, shadow] {
            if panel.is_null() {
                continue;
            }
            let region = CreateRoundRectRgn(
                0,
                0,
                rect.right - rect.left + 1,
                rect.bottom - rect.top + 1,
                12 * dpi / 96,
                12 * dpi / 96,
            );
            if !region.is_null() && SetWindowRgn(panel, region, 1) == 0 {
                DeleteObject(region);
            }
        }
        if !shadow.is_null() {
            SetWindowPos(
                shadow,
                hwnd,
                rect.left,
                rect.top + 3 * dpi / 96,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
        }
    }
}
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    unsafe {
        if message == WM_NCHITTEST {
            return HTCLIENT as isize;
        }
        if message == WM_MOUSEACTIVATE {
            return MA_ACTIVATE as isize;
        }
        if message == WM_WINDOWPOSCHANGED {
            let result = DefSubclassProc(hwnd, message, wp, lp);
            if (*(lp as *const WINDOWPOS)).flags & (SWP_NOMOVE | SWP_NOSIZE)
                != (SWP_NOMOVE | SWP_NOSIZE)
            {
                sync_frame(hwnd);
            }
            return result;
        }
        if message == WM_MOUSEWHEEL {
            return SendMessageW(GetDlgItem(hwnd, 2), message, wp, lp);
        }
        if message == WM_CLOSE {
            input::prompt_event(hwnd, false);
            return 0;
        }
        if message == WM_ACTIVATE && wp & 0xffff == WA_INACTIVE as usize {
            input::prompt_deactivated(hwnd);
        }
        if message == WM_SETFOCUS {
            SetFocus(GetDlgItem(hwnd, 1));
            return 0;
        }
        let state = GetPropW(hwnd, w!("PromptAppearance")) as *const RefCell<Appearance>;
        if let Some(state) = state.as_ref() {
            match message {
                WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
                    if let Ok(s) = state.try_borrow() {
                        SetTextColor(wp as HDC, s.foreground);
                        SetBkColor(wp as HDC, s.background);
                        SetBkMode(wp as HDC, OPAQUE as i32);
                        return s.brush as isize;
                    }
                }
                WM_PAINT => {
                    let mut paint = PAINTSTRUCT::default();
                    let dc = BeginPaint(hwnd, &mut paint);
                    if let Ok(s) = state.try_borrow() {
                        let mut rect = RECT::default();
                        GetClientRect(hwnd, &mut rect);
                        FillRect(dc, &rect, s.brush);
                        let border = CreateSolidBrush(crate::colors::tip_border(s.background));
                        FrameRect(dc, &rect, border);
                        DeleteObject(border);
                    }
                    EndPaint(hwnd, &paint);
                    return 0;
                }
                WM_ERASEBKGND => {
                    if let Ok(s) = state.try_borrow() {
                        let mut r = RECT::default();
                        GetClientRect(hwnd, &mut r);
                        FillRect(wp as HDC, &r, s.brush);
                        return 1;
                    }
                }
                WM_SIZE => {
                    if let Ok(s) = state.try_borrow() {
                        layout(hwnd, &s);
                    }
                    return 0;
                }
                WM_DPICHANGED => {
                    let dpi = (wp & 0xffff) as i32;
                    let old = if let Ok(mut s) = state.try_borrow_mut() {
                        Font::new(&s.family, s.points, dpi, s.bold, s.italic)
                            .ok()
                            .map(|font| std::mem::replace(&mut s.font, font))
                    } else {
                        None
                    };
                    if let Ok(s) = state.try_borrow() {
                        for id in [1, 2, 3] {
                            SendMessageW(GetDlgItem(hwnd, id), WM_SETFONT, s.font.0 as usize, 1);
                        }
                    }
                    drop(old);
                    let r = &*(lp as *const RECT);
                    SetWindowPos(
                        hwnd,
                        null_mut(),
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOACTIVATE | SWP_NOZORDER,
                    );
                    return 0;
                }
                WM_THEMECHANGED | WM_SETTINGCHANGE => {
                    if let Ok(mut s) = state.try_borrow_mut() {
                        s.update_colors();
                    }
                    RedrawWindow(
                        hwnd,
                        null(),
                        null_mut(),
                        RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN,
                    );
                }
                _ => {}
            }
        }
        DefSubclassProc(hwnd, message, wp, lp)
    }
}
unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    // SAFETY: subclass lifetime is bounded by the native edit; properties carry integers only.
    unsafe {
        match message {
            WM_IME_STARTCOMPOSITION => {
                SetPropW(hwnd, w!("Composing"), 1 as HANDLE);
            }
            WM_IME_ENDCOMPOSITION => {
                RemovePropW(hwnd, w!("Composing"));
            }
            WM_GETDLGCODE => return DLGC_WANTALLKEYS as isize,
            WM_KEYDOWN | WM_SYSKEYDOWN
                if matches!(wp, 13 | 27) && GetPropW(hwnd, w!("Composing")).is_null() =>
            {
                input::prompt_event(GetParent(hwnd), wp == 13);
                return 0;
            }
            WM_CHAR if matches!(wp, 13 | 27) => return 0,
            WM_SYSCHAR => return DefSubclassProc(hwnd, WM_CHAR, wp, lp),
            WM_NCDESTROY => {
                RemoveWindowSubclass(hwnd, Some(edit_proc), 1);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, message, wp, lp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "opens a themed command prompt and checks target text, font, long errors and DPI changes"]
    fn prompt_displays_target_and_uses_theme_font_and_scrollable_errors() {
        let _hooks = input::Hooks::escape_only().unwrap();
        let mut cfg = crate::test_support::cfg();
        cfg.theme = "dark".into();
        cfg.font_size = 18;
        let mut overlay = crate::overlay::Overlay::new().unwrap();
        let window = overlay.command_surface(&cfg).unwrap();
        let prompt = Prompt::attach(window, &cfg, unsafe { GetForegroundWindow() }).unwrap();
        assert_eq!(prompt.window, window);
        assert_eq!(
            unsafe { GetWindowLongW(window, GWL_STYLE) } as u32 & (WS_CAPTION | WS_THICKFRAME),
            0
        );
        assert_eq!(unsafe { GetParent(prompt.edit) }, window);
        assert_eq!(unsafe { GetFocus() }, prompt.edit);
        let label = unsafe { GetDlgItem(prompt.window, 3) };
        let mut text = [0u16; 1024];
        let n = unsafe { GetWindowTextW(label, text.as_mut_ptr(), 1024) };
        let text = String::from_utf16_lossy(&text[..n as usize]);
        assert!(text.contains("屏幕") || text.contains("Screen"));
        assert_eq!(prompt.appearance.borrow().background, cfg.colors[2]);
        assert_eq!(
            unsafe { SendMessageW(prompt.edit, WM_GETFONT, 0, 0) } as usize,
            prompt.appearance.borrow().font.0 as usize
        );
        let error = "A detailed error line\n".repeat(100);
        prompt.error(&error);
        assert_eq!(
            unsafe { GetWindowTextLengthW(prompt.status) } as usize,
            error.encode_utf16().count()
        );
        assert!(unsafe { GetWindowLongW(prompt.status, GWL_STYLE) } as u32 & WS_VSCROLL != 0);
        let before = prompt.appearance.borrow().font.height();
        let rect = RECT {
            left: 100,
            top: 100,
            right: 900,
            bottom: 500,
        };
        unsafe {
            SendMessageW(
                prompt.window,
                WM_DPICHANGED,
                192 | (192 << 16),
                (&rect as *const RECT) as isize,
            );
        }
        assert!(prompt.appearance.borrow().font.height() >= before);
        unsafe {
            SendMessageW(prompt.status, WM_KEYDOWN, 27, 1);
        }
        let cancel = std::iter::from_fn(input::next_event)
            .find(|e| e.message == input::COMMAND_CANCEL)
            .unwrap();
        assert_eq!(cancel.wParam, prompt.window as usize);
        drop(prompt);
        overlay.tip("Ready", &cfg).unwrap();
        assert_ne!(unsafe { IsWindow(window) }, 0);
        assert!(unsafe { GetDlgItem(window, 1) }.is_null());
        assert!(unsafe { GetPropW(window, w!("PromptAppearance")) }.is_null());
        assert_ne!(
            unsafe { GetWindowLongW(window, GWL_EXSTYLE) } as u32 & WS_EX_NOACTIVATE,
            0
        );
        assert_eq!(overlay.tip_text(), "Ready");
    }
    #[test]
    fn names_precede_expressions_and_punctuation_is_preserved() {
        let mut doc = crate::config::help()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        for name in ["1/4", "a\\b-c", "a|b", "区域"] {
            doc["presets"][name] = toml_edit::value("0|0|50sw|50sh");
        }
        let cfg = Config::parse(&doc).unwrap();
        for name in ["1/4", "a\\b-c", "a|b", "区域"] {
            assert!(matches!(parse(name,&cfg).unwrap(), Command::Action(a) if a == name));
        }
        for expression in [
            "0|0|100sw|50sh",
            "0｜0、100sw|50sh",
            r"20sw\20sh\60sw\60sh",
            r"0\0、100sw｜50sh",
        ] {
            assert!(matches!(
                parse(expression, &cfg).unwrap(),
                Command::Layout(_)
            ));
        }
        assert!(parse("0|broken|100sw|50sh", &cfg).is_err());
        assert!(parse("does not exist", &cfg).is_err());
        assert!(matches!(parse("pin",&cfg).unwrap(), Command::Action(a) if a == "top"));
    }
    #[test]
    #[ignore = "opens a temporary native command edit to verify Unicode, editing and IME confirmation"]
    fn native_editor_accepts_unicode_punctuation_and_defers_enter_during_composition() {
        let _hooks = input::Hooks::escape_only().unwrap();
        let cfg = crate::test_support::cfg();
        let mut overlay = crate::overlay::Overlay::new().unwrap();
        let window = overlay.command_surface(&cfg).unwrap();
        let prompt = Prompt::attach(window, &cfg, unsafe { GetForegroundWindow() }).unwrap();
        assert_eq!(prompt.window, window);
        let text = "区域1/4\\-｜、|";
        unsafe {
            for ch in text.encode_utf16() {
                SendMessageW(prompt.edit, WM_CHAR, ch as usize, 1);
            }
            assert_eq!(prompt.text(), text);
            SendMessageW(prompt.edit, EM_SETSEL, 0, 2);
            SendMessageW(
                prompt.edit,
                EM_REPLACESEL,
                1,
                desktop::wide("窗口").as_ptr() as isize,
            );
            assert_eq!(prompt.text(), "窗口1/4\\-｜、|");
            SendMessageW(prompt.edit, WM_IME_STARTCOMPOSITION, 0, 0);
            SendMessageW(prompt.edit, WM_KEYDOWN, 13, 0);
            assert!(input::next_event().is_none());
            SendMessageW(prompt.edit, WM_IME_ENDCOMPOSITION, 0, 0);
            SendMessageW(prompt.edit, WM_KEYDOWN, 13, 0);
        }
        assert_eq!(input::next_event().unwrap().message, input::COMMAND_SUBMIT);
    }
}
