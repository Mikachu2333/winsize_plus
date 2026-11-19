use crate::{
    config::{self, Config},
    desktop::{self, err, wide},
    framerate::FrameClock,
    input,
    overlay::Overlay,
    preview::Preview,
};
use std::{
    cell::RefCell,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    ptr::{null, null_mut},
};
use toml_edit::{DocumentMut, Value};
use windows_sys::Win32::{
    Foundation::*,
    System::{LibraryLoader::GetModuleHandleW, Threading::*},
    UI::{Input::KeyboardAndMouse::EnableWindow, WindowsAndMessaging::*},
};

const SECTION: i32 = 101;
const FIELD: i32 = 102;
const VALUE: i32 = 103;
const LIVE: i32 = 104;
const STATUS: i32 = 105;
const SECTIONS: [(&str, &str); 9] = [
    ("general", "常规"),
    ("tip", "提示框"),
    ("crosshair", "十字线"),
    ("horizontal", "水平线"),
    ("vertical", "垂直线"),
    ("presets", "布局预设"),
    ("grid", "右键九宫格"),
    ("keys", "快捷键"),
    ("commands", "指令"),
];

pub fn open() -> Result<(), String> {
    Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
        .arg("--settings")
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

struct Dialog(HWND);
impl Drop for Dialog {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.0);
        }
    }
}
impl Dialog {
    fn send(&self, id: i32, msg: u32, wp: usize, lp: isize) -> isize {
        unsafe { SendDlgItemMessageW(self.0, id, msg, wp, lp) }
    }
    fn text(&self, id: i32, text: &str) {
        unsafe {
            SetDlgItemTextW(self.0, id, wide(text).as_ptr());
        }
    }
    fn value(&self) -> String {
        let mut text =
            vec![0; unsafe { GetWindowTextLengthW(GetDlgItem(self.0, VALUE)) } as usize + 1];
        let len = unsafe { GetDlgItemTextW(self.0, VALUE, text.as_mut_ptr(), text.len() as i32) };
        String::from_utf16_lossy(&text[..len as usize])
    }
}

struct Editor {
    dialog: Dialog,
    path: PathBuf,
    original: String,
    doc: DocumentMut,
    cfg: Config,
    section: &'static str,
    keys: Vec<String>,
    key: String,
    overlay: Overlay,
    preview: Preview,
    clock: FrameClock,
}
impl Editor {
    fn init(&mut self) -> Result<(), String> {
        let title = self.cfg.tr("WinSizeP Settings", "WinSizeP 设置");
        unsafe {
            SetWindowTextW(self.dialog.0, wide(title).as_ptr());
        }
        for (id, en, zh) in [
            (110, "Section", "类别"),
            (111, "Setting", "设置项"),
            (112, "Value", "值"),
            (LIVE, "Live desktop preview", "实时桌面预览"),
            (IDOK, "Save", "保存"),
            (IDCANCEL, "Cancel", "取消"),
        ] {
            self.dialog.text(id, self.cfg.tr(en, zh));
        }
        for (name, zh) in SECTIONS {
            self.dialog.send(
                SECTION,
                CB_ADDSTRING,
                0,
                wide(self.cfg.tr(name, zh)).as_ptr() as isize,
            );
        }
        self.dialog.send(SECTION, CB_SETCURSEL, 0, 0);
        self.dialog.send(LIVE, BM_SETCHECK, 1, 0);
        self.select_section()
    }
    fn select_section(&mut self) -> Result<(), String> {
        let index = self.dialog.send(SECTION, CB_GETCURSEL, 0, 0) as usize;
        self.section = SECTIONS.get(index).ok_or("section")?.0;
        self.keys = self.doc[self.section]
            .as_table()
            .ok_or(self.section)?
            .iter()
            .filter(|(_, item)| item.is_value())
            .map(|(name, _)| name.to_string())
            .collect();
        self.dialog.send(FIELD, LB_RESETCONTENT, 0, 0);
        for name in &self.keys {
            self.dialog
                .send(FIELD, LB_ADDSTRING, 0, wide(name).as_ptr() as isize);
        }
        self.dialog.send(FIELD, LB_SETCURSEL, 0, 0);
        self.select_field()
    }
    fn select_field(&mut self) -> Result<(), String> {
        let index = self.dialog.send(FIELD, LB_GETCURSEL, 0, 0) as usize;
        self.key = self.keys.get(index).ok_or("setting")?.clone();
        let value = self.doc[self.section][&self.key]
            .as_value()
            .ok_or(&self.key)?;
        let text = value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.clone().decorated("", "").to_string());
        let choices: &[&str] = match (self.section, self.key.as_str()) {
            ("general", "language") => &["auto", "zh-CN", "en"],
            ("general", "alt") => &["left", "right", "both"],
            ("general", "gesture") => &["left", "right", "both", "off"],
            ("tip", "show") => &["monitor", "time", "off"],
            ("tip", "theme") => &["system", "light", "dark"],
            _ if value.is_bool() => &["true", "false"],
            _ => &[],
        };
        self.dialog.send(VALUE, CB_RESETCONTENT, 0, 0);
        let mut options = choices.to_vec();
        if matches!(self.section, "grid" | "keys" | "commands") {
            options.extend(self.cfg.presets.keys().map(String::as_str));
            if self.section != "grid" {
                options.extend(config::ACTIONS);
            }
            options.sort_unstable();
        }
        for choice in options {
            self.dialog
                .send(VALUE, CB_ADDSTRING, 0, wide(choice).as_ptr() as isize);
        }
        self.dialog.text(VALUE, &text);
        self.valid(true);
        self.render()
    }
    fn valid(&self, valid: bool) {
        unsafe {
            EnableWindow(GetDlgItem(self.dialog.0, IDOK), i32::from(valid));
        }
    }
    fn edit(&mut self) -> Result<(), String> {
        self.valid(false);
        let text = self.dialog.value();
        let mut draft = self.doc.clone();
        let value = draft[self.section][&self.key]
            .as_value_mut()
            .ok_or(&self.key)?;
        let mut new = if value.is_str() {
            Value::from(text)
        } else if self.section == "crosshair" && self.key == "fps" {
            Value::from(text.parse::<f64>().map_err(|_| "fps (1..1000)")?)
        } else {
            text.parse::<Value>().map_err(|error| error.to_string())?
        };
        *new.decor_mut() = value.decor().clone();
        *value = new;
        self.cfg = Config::parse(&draft)?;
        self.doc = draft;
        self.valid(true);
        self.render()
    }
    fn render(&mut self) -> Result<(), String> {
        let live = self.dialog.send(LIVE, BM_GETCHECK, 0, 0) == 1;
        self.preview.hide();
        self.overlay.hide_tip();
        let axis = if live {
            match self.section {
                "horizontal" => 1,
                "vertical" => 2,
                "crosshair" => 3,
                _ => 0,
            }
        } else {
            0
        };
        self.clock.reset(self.cfg.fps);
        self.overlay.guides(axis, &self.cfg)?;
        if live && self.section == "tip" {
            if self.cfg.show != "off" {
                let sample = if self.cfg.show == "time" {
                    let mut time = SYSTEMTIME::default();
                    unsafe {
                        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut time);
                    }
                    format!("{:02}:{:02}:{:02}", time.wHour, time.wMinute, time.wSecond)
                } else {
                    format!(
                        "{} {}",
                        self.cfg.tr("Monitor", "屏幕"),
                        desktop::screen(desktop::cursor()?, 0)?.number
                    )
                };
                self.overlay.tip(&sample, &self.cfg)?;
            }
        } else if live && self.cfg.preview && axis == 0 {
            self.preview.start(self.dialog.0, &self.cfg)?;
        }
        input::set_guides(axis != 0 || self.preview.visible())?;
        let (en, zh) = match self.section {
            "presets" => (
                "x | y | width | height [| monitor]. Use sw/sh for percentages of the work area.",
                "x | y | 宽 | 高 [| 屏号]。sw、sh 表示工作区宽高的百分比。",
            ),
            "crosshair" => (
                "Target FPS: 1–1000, including fractional values such as 59.94.",
                "目标刷新率为 1–1000 FPS，可填 59.94 等小数。",
            ),
            "horizontal" | "vertical" => (
                "Width and gap are in logical pixels. Color uses #RRGGBB and opacity ranges from 0 to 255.",
                "宽度和缺口以逻辑像素计。颜色格式为 #RRGGBB，透明度为 0–255。",
            ),
            "keys" | "commands" | "grid" => (
                "Select a binding or clear the value to disable it.",
                "选择要绑定的动作或预设，留空可停用该项。",
            ),
            _ => ("Save to apply your changes.", "保存后应用更改。"),
        };
        self.dialog.text(STATUS, self.cfg.tr(en, zh));
        Ok(())
    }
    fn moved(&mut self, point: POINT) -> Result<(), String> {
        if self.preview.visible() {
            self.preview.follow(point, &self.cfg)?;
        }
        if self.overlay.mode() != 0
            && let Some(point) = self.clock.moved(point)?
        {
            self.overlay.follow(point, &self.cfg)?;
        }
        Ok(())
    }
    fn command(&mut self, id: i32, event: u32) -> Result<(), String> {
        match (id, event) {
            (SECTION, CBN_SELCHANGE) => self.select_section(),
            (FIELD, LBN_SELCHANGE) => self.select_field(),
            (VALUE, CBN_EDITCHANGE) => self.edit(),
            (VALUE, CBN_SELCHANGE) => {
                let index = self.dialog.send(VALUE, CB_GETCURSEL, 0, 0) as usize;
                let len = self.dialog.send(VALUE, CB_GETLBTEXTLEN, index, 0);
                if len < 0 {
                    return Ok(());
                }
                let mut text = vec![0u16; len as usize + 1];
                self.dialog
                    .send(VALUE, CB_GETLBTEXT, index, text.as_mut_ptr() as isize);
                self.dialog
                    .text(VALUE, &String::from_utf16_lossy(&text[..len as usize]));
                self.edit()
            }
            (LIVE, _) => self.render(),
            (IDOK, _) => {
                self.edit()?;
                self.original = config::save_settings(&self.path, &self.original, &self.doc)?;
                self.doc = self
                    .original
                    .parse::<DocumentMut>()
                    .map_err(|error| error.to_string())?;
                self.cfg = Config::parse(&self.doc)?;
                if !crate::resident::running() {
                    crate::launch(self.cfg.elevate)?;
                }
                unsafe {
                    PostQuitMessage(0);
                }
                Ok(())
            }
            (IDCANCEL, _) => {
                unsafe {
                    PostQuitMessage(0);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

thread_local! { static EDITOR: RefCell<Option<Editor>> = const { RefCell::new(None) }; }
fn with_editor(action: impl FnOnce(&mut Editor) -> Result<(), String>) {
    EDITOR.with(|slot| {
        if let Ok(mut guard) = slot.try_borrow_mut()
            && let Some(editor) = guard.as_mut()
            && let Err(problem) = action(editor)
        {
            editor.dialog.text(STATUS, &problem);
        }
    });
}
unsafe extern "system" fn dialog_proc(_: HWND, message: u32, wp: WPARAM, _: LPARAM) -> isize {
    match message {
        WM_INITDIALOG => 1,
        WM_COMMAND => {
            with_editor(|editor| editor.command((wp & 0xffff) as i32, (wp >> 16) as u32));
            1
        }
        WM_CLOSE => {
            unsafe {
                PostQuitMessage(0);
            }
            1
        }
        _ => 0,
    }
}

pub fn run(path: &Path) -> Result<(), String> {
    config::load(path, None)?;
    let original = config::read_settings(path)?;
    let doc = original
        .parse::<DocumentMut>()
        .map_err(|error| error.to_string())?;
    let cfg = Config::parse(&doc)?;
    let hwnd = unsafe {
        CreateDialogParamW(
            GetModuleHandleW(null()),
            std::ptr::without_provenance(200),
            null_mut(),
            Some(dialog_proc),
            0,
        )
    };
    if hwnd.is_null() {
        return Err(err());
    }
    let mut editor = Editor {
        dialog: Dialog(hwnd),
        path: path.into(),
        original,
        doc,
        clock: FrameClock::new(cfg.fps)?,
        cfg,
        section: "general",
        keys: Vec::new(),
        key: String::new(),
        overlay: Overlay::new()?,
        preview: Preview::new()?,
    };
    let timer = editor.clock.handle();
    editor.init()?;
    EDITOR.with_borrow_mut(|slot| *slot = Some(editor));
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
    }
    let result = message_loop(hwnd, timer);
    input::set_guides(false)?;
    EDITOR.with_borrow_mut(|slot| {
        slot.take();
    });
    result
}
fn message_loop(hwnd: HWND, timer: HANDLE) -> Result<(), String> {
    unsafe {
        loop {
            match MsgWaitForMultipleObjectsEx(1, &timer, INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
            {
                WAIT_OBJECT_0 => with_editor(|editor| {
                    if let Some(point) = editor.clock.tick() {
                        editor.overlay.follow(point, &editor.cfg)?;
                    }
                    Ok(())
                }),
                value if value == WAIT_OBJECT_0 + 1 => {
                    let mut msg = MSG::default();
                    for _ in 0..64 {
                        if PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) == 0 {
                            break;
                        }
                        if msg.message == WM_QUIT {
                            return Ok(());
                        }
                        if msg.hwnd.is_null() && msg.message == input::MOVE {
                            with_editor(|editor| editor.moved(input::moved()));
                        } else if IsDialogMessageW(hwnd, &msg) == 0 {
                            TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }
                }
                _ => return Err(err()),
            }
        }
    }
}
