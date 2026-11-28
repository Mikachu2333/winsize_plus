use crate::{config::Config, desktop, input, layout::Layout};
use std::ptr::{null, null_mut};
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
    if text.contains(['|', '｜', '、']) {
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

pub struct Prompt {
    pub window: HWND,
    edit: HWND,
    status: HWND,
}
impl Prompt {
    pub fn new(cfg: &Config) -> Result<Self, String> {
        // SAFETY: class strings are static; child windows belong to this UI thread.
        unsafe {
            let instance = GetModuleHandleW(null());
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: (COLOR_WINDOW + 1) as HBRUSH,
                lpszClassName: w!("WinSizeP.Command"),
                ..Default::default()
            };
            RegisterClassW(&class);
            let screen = desktop::screen(desktop::cursor()?, 0)?;
            let width = (620 * screen.dpi / 96).min(screen.work.width);
            let height = 145 * screen.dpi / 96;
            let window = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                class.lpszClassName,
                desktop::wide(cfg.tr("Region or command", "分区或命令")).as_ptr(),
                WS_CAPTION | WS_SYSMENU,
                screen.work.x + (screen.work.width - width) / 2,
                screen.work.y + screen.work.height / 3,
                width,
                height,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            if window.is_null() {
                return Err(desktop::err());
            }
            let mut prompt = Self {
                window,
                edit: null_mut(),
                status: null_mut(),
            };
            let scale = |v| v * screen.dpi / 96;
            prompt.edit = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("EDIT"),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
                scale(12),
                scale(12),
                width - scale(40),
                scale(30),
                window,
                1 as HMENU,
                instance,
                null(),
            );
            prompt.status = CreateWindowExW(
                0,
                w!("STATIC"),
                desktop::wide(cfg.tr("Enter to apply · Esc to cancel", "Enter 确认 · Esc 取消"))
                    .as_ptr(),
                WS_CHILD | WS_VISIBLE,
                scale(12),
                scale(52),
                width - scale(40),
                scale(48),
                window,
                2 as HMENU,
                instance,
                null(),
            );
            if prompt.edit.is_null() || prompt.status.is_null() {
                return Err(desktop::err());
            }
            desktop::check(SetWindowSubclass(prompt.edit, Some(edit_proc), 1, 0))?;
            for child in [prompt.edit, prompt.status] {
                SendMessageW(
                    child,
                    WM_SETFONT,
                    GetStockObject(DEFAULT_GUI_FONT) as usize,
                    1,
                );
            }
            SendMessageW(prompt.edit, EM_SETLIMITTEXT, 4096, 0);
            ShowWindow(window, SW_SHOW);
            SetForegroundWindow(window);
            SetFocus(prompt.edit);
            Ok(prompt)
        }
    }
    pub fn text(&self) -> String {
        // SAFETY: the owned edit remains alive and the buffer includes a terminator.
        unsafe {
            let mut text = vec![0; GetWindowTextLengthW(self.edit).max(0) as usize + 1];
            let count = GetWindowTextW(self.edit, text.as_mut_ptr(), text.len() as i32);
            String::from_utf16_lossy(&text[..count.max(0) as usize])
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
            DestroyWindow(self.window);
        }
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: callback arguments are supplied by Windows on the owning thread.
    unsafe {
        if message == WM_CLOSE {
            input::prompt_event(false);
            return 0;
        }
        if message == WM_SETFOCUS {
            SetFocus(GetDlgItem(hwnd, 1));
            return 0;
        }
        DefWindowProcW(hwnd, message, wp, lp)
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
                input::prompt_event(wp == 13);
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
        for expression in ["0|0|100sw|50sh", "0｜0、100sw|50sh"] {
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
        let prompt = Prompt::new(&cfg).unwrap();
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
