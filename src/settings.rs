use crate::{
    config::{self, Config},
    desktop::{self, err, wide},
    fonts::{self, Font},
    framerate::FrameClock,
    input,
    overlay::Overlay,
    preview::Preview,
    settings_schema::{self as schema, ColorChannels, Field, Kind, TABS},
    settings_tooltip::HoverTip,
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use toml_edit::{DocumentMut, Item, Value, value};
use windows_sys::{
    Win32::{
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*},
        System::{LibraryLoader::GetModuleHandleW, Threading::*},
        UI::{
            Controls::{Dialogs::*, *},
            HiDpi::*,
            Input::KeyboardAndMouse::*,
            Shell::*,
            WindowsAndMessaging::*,
        },
    },
    core::w,
};
const CLASS: *const u16 = w!("WinSizeP.Settings");
const PANE: *const u16 = w!("WinSizeP.Settings.Pane");
const BASE: i32 = 2000;
const STRIDE: i32 = 16;
const SCROLL: u32 = WM_APP + 80;
const DROP_FONT: u32 = WM_APP + 81;
const RESIZE: u32 = WM_APP + 82;
const HOVER: u32 = WM_APP + 83;
const EDIT_LAYOUT: u32 = WM_APP + 84;
const APP_EXIT: u32 = WM_APP + 85;
const ACTIVATE_PREVIEW: u32 = WM_APP + 86;
const TAB_BASE: i32 = 100;
const AXIS_H: i32 = 500;
const AXIS_V: i32 = 501;
const AXIS_BOTH: i32 = 502;
const SS_LEFT: u32 = 0;
const SS_CENTER: u32 = 1;
thread_local! { static EDITOR: RefCell<Option<Editor>> = const { RefCell::new(None) }; }
thread_local! { static UPDATING: Cell<bool> = const { Cell::new(false) }; }
thread_local! { static MEASUREMENTS: RefCell<HashMap<(usize,i32,String),i32>> = RefCell::new(HashMap::new()); }
pub fn open() -> Result<(), String> {
    unsafe {
        let existing = FindWindowW(CLASS, null());
        if !existing.is_null() {
            ShowWindow(existing, SW_RESTORE);
            SetForegroundWindow(existing);
            return Ok(());
        }
    }
    Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
        .arg("--settings")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Ask the settings process to flush its pending edits and leave its message loop.
/// A failed save leaves both processes running so the error can be corrected.
pub fn close_for_exit() -> Result<(), String> {
    unsafe {
        let hwnd = FindWindowW(CLASS, null());
        if hwnd.is_null() {
            return Ok(());
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut result = 0;
            let timeout = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .clamp(1, 5000) as u32;
            let sent = SendMessageTimeoutW(
                hwnd,
                APP_EXIT,
                0,
                0,
                SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
                timeout,
                &mut result,
            );
            if sent == 0 {
                if IsWindow(hwnd) == 0 {
                    return Ok(());
                }
                return Err(format!(
                    "Cannot close settings / 无法关闭设置窗口: {}",
                    err()
                ));
            }
            if result == 1 {
                return Ok(());
            }
            if result == 2 && Instant::now() < deadline {
                // Let a canceled native color dialog return before flushing the editor.
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            return Err("Settings could not be saved. Correct the indicated field or close its dialog before exiting. / 设置未能保存，请先修正提示的字段或关闭设置中的对话框。".into());
        }
    }
}
struct Window(HWND);
impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.0);
        }
    }
}
struct MutexHandle(HANDLE);
impl Drop for MutexHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct RedrawGuard(HWND, bool);
impl RedrawGuard {
    fn new(hwnd: HWND) -> Self {
        unsafe {
            let first = GetPropW(hwnd, w!("LayoutBusy")).is_null();
            if first {
                SetPropW(hwnd, w!("LayoutBusy"), 1usize as HANDLE);
                SendMessageW(hwnd, WM_SETREDRAW, 0, 0);
            }
            Self(hwnd, first)
        }
    }
}
impl Drop for RedrawGuard {
    fn drop(&mut self) {
        if self.1 {
            unsafe {
                RemovePropW(self.0, w!("LayoutBusy"));
                SendMessageW(self.0, WM_SETREDRAW, 1, 0);
                RedrawWindow(
                    self.0,
                    null(),
                    null_mut(),
                    RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_FRAME,
                );
            }
        }
    }
}
fn destroy_rows(rows: Vec<Row>) {
    for row in rows {
        for hwnd in row.controls {
            if !hwnd.is_null() {
                unsafe {
                    DestroyWindow(hwnd);
                }
            }
        }
    }
}
struct Row {
    field: Field,
    controls: [HWND; 16],
    options: Vec<(String, String)>,
}
impl Row {
    fn c(&self, slot: usize) -> HWND {
        self.controls[slot]
    }
}
struct Editor {
    window: Window,
    panes: [HWND; 2],
    tabs: Vec<HWND>,
    status: HWND,
    status_until: Cell<Option<Instant>>,
    axis_controls: [HWND; 3],
    path: PathBuf,
    original: String,
    doc: DocumentMut,
    cfg: Config,
    tab: usize,
    axis: usize,
    rows: Vec<Row>,
    pages: HashMap<(usize, usize), (Vec<Row>, i32)>,
    scroll: [i32; 2],
    extent: [i32; 2],
    dpi: i32,
    font: Font,
    hint_font: Font,
    families: Vec<String>,
    live: bool,
    selected: Option<String>,
    pending: Option<Instant>,
    dirty: HashSet<usize>,
    disk_check: Instant,
    overlay: Overlay,
    preview: Preview,
    clock: FrameClock,
    tooltip: HoverTip,
    active_combo: Option<HWND>,
}
fn text_width(text: &str, font: &Font) -> i32 {
    unsafe {
        let dc = GetDC(null_mut());
        let old = SelectObject(dc, font.0);
        let text = wide(text);
        let mut size = SIZE::default();
        GetTextExtentPoint32W(dc, text.as_ptr(), (text.len() - 1) as i32, &mut size);
        SelectObject(dc, old);
        ReleaseDC(null_mut(), dc);
        size.cx
    }
}
fn send(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    unsafe { SendMessageW(hwnd, msg, wp, lp) }
}
fn caption(hwnd: HWND) -> String {
    unsafe {
        let mut buf = vec![0; GetWindowTextLengthW(hwnd).max(0) as usize + 1];
        let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..len as usize])
    }
}
fn set_text(hwnd: HWND, text: &str) {
    if caption(hwnd) != text {
        unsafe {
            SetWindowTextW(hwnd, wide(text).as_ptr());
        }
    }
}
fn place(hwnd: HWND, x: i32, y: i32, width: i32, height: i32) {
    if !hwnd.is_null() {
        unsafe {
            let mut old = RECT::default();
            GetWindowRect(hwnd, &mut old);
            MapWindowPoints(
                null_mut(),
                GetParent(hwnd),
                (&mut old as *mut RECT).cast(),
                2,
            );
            if (
                old.left,
                old.top,
                old.right - old.left,
                old.bottom - old.top,
            ) == (x, y, width.max(1), height.max(1))
            {
                return;
            }
            SetWindowPos(
                hwnd,
                null_mut(),
                x,
                y,
                width.max(1),
                height.max(1),
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOCOPYBITS,
            );
        }
    }
}
fn measured(text: &str, font: &Font, width: i32) -> i32 {
    let key = (font.0 as usize, width, text.to_owned());
    if let Some(height) = MEASUREMENTS.with_borrow(|cache| cache.get(&key).copied()) {
        return height;
    }
    unsafe {
        let dc = GetDC(null_mut());
        let old = SelectObject(dc, font.0);
        let mut rect = RECT {
            right: width.max(1),
            ..Default::default()
        };
        let text = wide(text);
        DrawTextW(
            dc,
            text.as_ptr(),
            (text.len() - 1) as i32,
            &mut rect,
            DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX,
        );
        SelectObject(dc, old);
        ReleaseDC(null_mut(), dc);
        let height = rect.bottom.max(font.height());
        MEASUREMENTS.with_borrow_mut(|cache| {
            if cache.len() > 8192 {
                cache.clear();
            }
            cache.insert(key, height);
        });
        height
    }
}
impl Editor {
    fn invalidate_pages(&mut self) {
        for (_, (rows, _)) in self.pages.drain() {
            destroy_rows(rows);
        }
    }
    fn switch_page(&mut self, tab: usize, axis: usize) -> Result<(), String> {
        if self.tab == tab && self.axis == axis {
            return Ok(());
        }
        let _redraw = RedrawGuard::new(self.panes[1]);
        let old_key = (self.tab, if self.tab == 5 { self.axis } else { 0 });
        let rows = std::mem::take(&mut self.rows);
        for row in &rows {
            for hwnd in row.controls {
                if !hwnd.is_null() {
                    unsafe {
                        ShowWindow(hwnd, SW_HIDE);
                        SetWindowLongPtrW(hwnd, GWLP_ID, -GetDlgCtrlID(hwnd) as isize);
                    }
                }
            }
        }
        self.pages.insert(old_key, (rows, self.scroll[1]));
        self.tab = tab;
        self.axis = axis;
        self.selected = None;
        self.preview.hide();
        self.overlay.hide_tip();
        self.tooltip.hide();
        self.active_combo = None;
        let key = (tab, if tab == 5 { axis } else { 0 });
        if let Some((rows, scroll)) = self.pages.remove(&key) {
            self.rows = rows;
            self.scroll[1] = scroll;
            for row in &self.rows {
                for hwnd in row.controls {
                    if !hwnd.is_null() {
                        unsafe {
                            SetWindowLongPtrW(hwnd, GWLP_ID, -GetDlgCtrlID(hwnd) as isize);
                            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                        }
                    }
                }
            }
            for (index, hwnd) in self.tabs.iter().enumerate() {
                unsafe {
                    SetPropW(
                        *hwnd,
                        w!("SelectedTab"),
                        usize::from(index == tab) as HANDLE,
                    );
                    InvalidateRect(*hwnd, null(), 0);
                }
            }
            for (index, hwnd) in self.axis_controls.iter().enumerate() {
                unsafe {
                    ShowWindow(*hwnd, if tab == 5 { SW_SHOW } else { SW_HIDE });
                }
                send(*hwnd, BM_SETCHECK, usize::from(axis == index), 0);
            }
            self.update_values();
            self.layout()?;
            self.render()?;
        } else {
            self.scroll[1] = 0;
            let cache = std::mem::take(&mut self.pages);
            let result = self.rebuild();
            self.pages = cache;
            result?;
        }
        #[cfg(test)]
        unsafe {
            SetPropW(self.window.0, w!("ReadyTab"), (self.tab + 1) as HANDLE);
        }
        Ok(())
    }
    fn s(&self, n: i32) -> i32 {
        (n * self.dpi + 48) / 96
    }
    fn status(&self, text: &str) {
        set_text(self.status, text);
        self.status_until
            .set(Some(Instant::now() + Duration::from_secs(3)));
        self.layout_status();
    }
    fn layout_status(&self) {
        let mut client = RECT::default();
        unsafe {
            GetClientRect(self.window.0, &mut client);
        }
        let text = caption(self.status);
        if text.is_empty() {
            unsafe {
                ShowWindow(self.status, SW_HIDE);
            }
            return;
        }
        let width = (text_width(&text, &self.font) + self.s(4))
            .min(client.right - self.s(32))
            .max(1);
        let height = measured(&text, &self.font, width).min(self.font.height() * 2);
        place(
            self.status,
            self.s(16),
            client.bottom - height - self.s(8),
            width,
            height,
        );
        unsafe {
            ShowWindow(self.status, SW_SHOWNOACTIVATE);
            InvalidateRect(self.status, null(), 1);
        }
    }
    fn create(
        &self,
        parent: HWND,
        class: *const u16,
        label: &str,
        style: u32,
        id: i32,
    ) -> Result<HWND, String> {
        let is_edit = unsafe { std::slice::from_raw_parts(class, 5) == [69u16, 68, 73, 84, 0] };
        let is_combo = unsafe { std::slice::from_raw_parts(class, 5) == [67u16, 79, 77, 66, 79] };
        let style = if is_edit {
            style | ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32
        } else if is_combo {
            style | CBS_OWNERDRAWFIXED as u32 | CBS_HASSTRINGS as u32
        } else {
            style
        };
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class,
                wide(label).as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                0,
                0,
                1,
                1,
                parent,
                id as usize as HMENU,
                GetModuleHandleW(null()),
                null(),
            )
        };
        if hwnd.is_null() {
            return Err(err());
        }
        send(hwnd, WM_SETFONT, self.font.0 as usize, 0);
        unsafe {
            SetWindowSubclass(hwnd, Some(control_proc), 1, self.window.0 as usize);
            if is_combo {
                let mut info = COMBOBOXINFO {
                    cbSize: size_of::<COMBOBOXINFO>() as u32,
                    ..Default::default()
                };
                if GetComboBoxInfo(hwnd, &mut info) != 0 {
                    SetPropW(info.hwndList, w!("OwnerCombo"), hwnd);
                    SetWindowSubclass(info.hwndList, Some(control_proc), 1, self.window.0 as usize);
                }
                send(hwnd, CB_SETMINVISIBLE, 8, 0);
            }
        }
        Ok(hwnd)
    }
    fn control(
        &self,
        row: &mut Row,
        index: usize,
        slot: usize,
        class: *const u16,
        text: &str,
        style: u32,
    ) -> Result<(), String> {
        row.controls[slot] = self.create(
            self.panes[1],
            class,
            text,
            style,
            BASE + index as i32 * STRIDE + slot as i32,
        )?;
        Ok(())
    }
    fn rebuild(&mut self) -> Result<(), String> {
        let _redraw = RedrawGuard::new(self.panes[1]);
        for (_, (rows, _)) in self.pages.drain() {
            destroy_rows(rows);
        }
        #[cfg(test)]
        unsafe {
            RemovePropW(self.window.0, w!("ReadyTab"));
        }
        self.tooltip.hide();
        self.active_combo = None;
        self.preview.hide();
        self.overlay.hide_tip();
        self.overlay.guides(0, &self.cfg)?;
        for row in self.rows.drain(..) {
            for hwnd in row.controls {
                if !hwnd.is_null() {
                    unsafe {
                        DestroyWindow(hwnd);
                    }
                }
            }
        }
        for (i, hwnd) in self.tabs.iter().enumerate() {
            set_text(*hwnd, self.cfg.tr(TABS[i].0, TABS[i].1));
            unsafe {
                SetPropW(
                    *hwnd,
                    w!("SelectedTab"),
                    usize::from(i == self.tab) as HANDLE,
                );
                InvalidateRect(*hwnd, null(), 1);
            }
        }
        set_text(
            self.window.0,
            self.cfg.tr("WinSizeP Settings", "WinSizeP 设置"),
        );
        for (i, hwnd) in self.axis_controls.iter().enumerate() {
            set_text(
                *hwnd,
                if i == 0 {
                    self.cfg.tr("Horizontal line", "横线")
                } else if i == 1 {
                    self.cfg.tr("Vertical line", "竖线")
                } else {
                    self.cfg.tr("Both axes", "统一设置")
                },
            );
            unsafe {
                ShowWindow(*hwnd, if self.tab == 5 { SW_SHOW } else { SW_HIDE });
            }
            send(*hwnd, BM_SETCHECK, usize::from(self.axis == i), 0);
        }
        for (index, field) in schema::fields(self.tab, self.axis, &self.cfg, &self.doc)
            .into_iter()
            .enumerate()
        {
            let mut row = Row {
                field,
                controls: [null_mut(); 16],
                options: Vec::new(),
            };
            let item = self
                .doc
                .get(&row.field.section)
                .and_then(|s| s.get(&row.field.key));
            let text = item
                .and_then(Item::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    item.and_then(Item::as_integer)
                        .map(|v| v.to_string())
                        .unwrap_or_default()
                });
            let kind = row.field.kind.clone();
            let label = row.field.label.clone();
            let hint = row.field.hint.clone();
            if !matches!(kind, Kind::Bool | Kind::Preview | Kind::Add) {
                self.control(&mut row, index, 1, w!("STATIC"), &label, SS_LEFT)?;
            }
            self.control(&mut row, index, 2, w!("STATIC"), &hint, SS_LEFT)?;
            send(row.c(2), WM_SETFONT, self.hint_font.0 as usize, 0);
            match kind {
                Kind::Bool | Kind::Preview => {
                    self.control(
                        &mut row,
                        index,
                        0,
                        w!("BUTTON"),
                        &label,
                        BS_AUTOCHECKBOX as u32 | BS_MULTILINE as u32 | WS_TABSTOP,
                    )?;
                    let checked = if matches!(kind, Kind::Preview) {
                        self.live
                    } else {
                        item.and_then(Item::as_bool).unwrap_or(false)
                    };
                    send(row.c(0), BM_SETCHECK, usize::from(checked), 0);
                }
                Kind::Number | Kind::Text => self.control(
                    &mut row,
                    index,
                    0,
                    w!("EDIT"),
                    &text,
                    WS_BORDER
                        | WS_TABSTOP
                        | if matches!(kind, Kind::Number) {
                            ES_NUMBER as u32
                        } else {
                            ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32
                        },
                )?,
                Kind::Font | Kind::Choice(_) => {
                    row.options = if let Kind::Choice(options) = kind {
                        options
                    } else {
                        self.families
                            .iter()
                            .map(|n| {
                                (
                                    n.clone(),
                                    schema::option(&self.cfg, schema::ChoiceContext::Font, n),
                                )
                            })
                            .collect()
                    };
                    let style = CBS_DROPDOWNLIST as u32
                        | WS_TABSTOP
                        | WS_VSCROLL
                        | if matches!(row.field.kind, Kind::Font) {
                            CBS_OWNERDRAWFIXED as u32 | CBS_HASSTRINGS as u32
                        } else {
                            0
                        };
                    self.control(&mut row, index, 0, w!("COMBOBOX"), "", style)?;
                    if matches!(row.field.kind, Kind::Font) {
                        unsafe {
                            SetPropW(row.c(0), w!("FontPicker"), 1usize as HANDLE);
                        }
                    }
                    populate(
                        row.c(0),
                        &row.options,
                        &text,
                        self.font.height() + self.s(14),
                    );
                }
                Kind::Mapping => {
                    let key = row.field.key.clone();
                    self.control(&mut row, index, 3, w!("EDIT"), &key, WS_BORDER | WS_TABSTOP)?;
                    self.control(
                        &mut row,
                        index,
                        4,
                        w!("BUTTON"),
                        self.cfg.tr("Delete", "删除"),
                        BS_PUSHBUTTON as u32 | WS_TABSTOP,
                    )?;
                    if row.field.section == "presets" {
                        self.control(
                            &mut row,
                            index,
                            0,
                            w!("EDIT"),
                            &text,
                            WS_BORDER | WS_TABSTOP | ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32,
                        )?;
                        self.control(
                            &mut row,
                            index,
                            5,
                            w!("BUTTON"),
                            self.cfg.tr("Preview this region", "预览此分区"),
                            BS_AUTORADIOBUTTON as u32 | BS_MULTILINE as u32 | WS_TABSTOP,
                        )?;
                        send(
                            row.c(5),
                            BM_SETCHECK,
                            usize::from(self.selected.as_ref() == Some(&row.field.key)),
                            0,
                        );
                    } else {
                        row.options = schema::actions(&self.cfg, row.field.section == "grid");
                        self.control(
                            &mut row,
                            index,
                            0,
                            w!("COMBOBOX"),
                            "",
                            CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL,
                        )?;
                        populate(
                            row.c(0),
                            &row.options,
                            &text,
                            self.font.height() + self.s(14),
                        );
                    }
                }
                Kind::Color(channels) => {
                    self.control(
                        &mut row,
                        index,
                        0,
                        w!("BUTTON"),
                        &text,
                        BS_OWNERDRAW as u32 | WS_TABSTOP,
                    )?;
                    let rgb = u32::from_str_radix(text.trim_start_matches('#'), 16).unwrap_or(0);
                    let alpha = self
                        .doc
                        .get(&row.field.section)
                        .and_then(|s| s.get("opacity"))
                        .and_then(Item::as_integer);
                    for (channel, amount) in [
                        (rgb >> 16) & 255,
                        (rgb >> 8) & 255,
                        rgb & 255,
                        alpha.unwrap_or(255) as u32,
                    ]
                    .into_iter()
                    .take(channels.count())
                    .enumerate()
                    {
                        self.control(
                            &mut row,
                            index,
                            channel + 3,
                            w!("EDIT"),
                            &amount.to_string(),
                            WS_BORDER | WS_TABSTOP | ES_NUMBER as u32,
                        )?;
                        self.control(
                            &mut row,
                            index,
                            channel + 7,
                            w!("STATIC"),
                            ["R", "G", "B", "A"][channel],
                            SS_CENTER,
                        )?;
                    }
                }
                Kind::Fallbacks => {
                    self.control(
                        &mut row,
                        index,
                        0,
                        w!("LISTBOX"),
                        "",
                        LBS_NOTIFY as u32
                            | LBS_OWNERDRAWFIXED as u32
                            | LBS_HASSTRINGS as u32
                            | WS_BORDER
                            | WS_VSCROLL
                            | WS_TABSTOP,
                    )?;
                    if let Some(fonts) = item.and_then(Item::as_array) {
                        for family in fonts.iter().filter_map(Value::as_str) {
                            send(row.c(0), LB_ADDSTRING, 0, wide(family).as_ptr() as isize);
                        }
                    }
                    row.options = self
                        .families
                        .iter()
                        .map(|n| {
                            (
                                n.clone(),
                                schema::option(&self.cfg, schema::ChoiceContext::Font, n),
                            )
                        })
                        .collect();
                    self.control(
                        &mut row,
                        index,
                        3,
                        w!("COMBOBOX"),
                        "",
                        CBS_DROPDOWNLIST as u32
                            | CBS_OWNERDRAWFIXED as u32
                            | CBS_HASSTRINGS as u32
                            | WS_VSCROLL
                            | WS_TABSTOP,
                    )?;
                    populate(
                        row.c(3),
                        &row.options,
                        "SystemFont",
                        self.font.height() + self.s(14),
                    );
                    unsafe {
                        SetPropW(row.c(3), w!("FontPicker"), 1usize as HANDLE);
                    }
                    for (slot, en, zh) in [
                        (4, "Add", "添加"),
                        (5, "Remove", "删除"),
                        (6, "Up", "上移"),
                        (7, "Down", "下移"),
                    ] {
                        self.control(
                            &mut row,
                            index,
                            slot,
                            w!("BUTTON"),
                            self.cfg.tr(en, zh),
                            BS_PUSHBUTTON as u32 | WS_TABSTOP,
                        )?;
                    }
                    send(
                        row.c(0),
                        LB_SETITEMHEIGHT,
                        0,
                        (self.font.height() + self.s(8)) as isize,
                    );
                }
                Kind::Add => self.control(
                    &mut row,
                    index,
                    0,
                    w!("BUTTON"),
                    &label,
                    BS_PUSHBUTTON as u32 | WS_TABSTOP,
                )?,
                Kind::Note => {}
            }
            self.rows.push(row);
        }
        self.layout()?;
        for row in &self.rows {
            for hwnd in row.controls {
                if !hwnd.is_null() {
                    send(hwnd, EDIT_LAYOUT, 0, 0);
                }
            }
        }
        self.render()?;
        #[cfg(test)]
        unsafe {
            SetPropW(self.window.0, w!("ReadyTab"), (self.tab + 1) as HANDLE);
        }
        Ok(())
    }
    fn layout(&mut self) -> Result<(), String> {
        let _redraw = RedrawGuard::new(self.panes[1]);
        let mut client = RECT::default();
        unsafe {
            GetClientRect(self.window.0, &mut client);
        }
        let footer = self.font.height() * 2 + self.s(16);
        let height = (client.bottom - footer).max(1);
        let left = self.sidebar_width(client.right);
        place(self.panes[0], 0, 0, left, height);
        place(self.panes[1], left, 0, client.right - left, height);
        self.layout_status();
        let padding = self.s(16);
        let gap = self.s(4);
        let mut rect = RECT::default();
        unsafe {
            GetClientRect(self.panes[0], &mut rect);
        }
        let width = (rect.right - padding * 2).max(1);
        let mut y = padding;
        for hwnd in &self.tabs {
            let h =
                measured(&caption(*hwnd), &self.font, (width - padding * 2).max(1)) + self.s(20);
            place(*hwnd, padding, y - self.scroll[0], width, h);
            y += h + self.s(6);
        }
        self.extent[0] = y + padding;
        unsafe {
            GetClientRect(self.panes[1], &mut rect);
        }
        let initial_right_width = rect.right;
        let width = (rect.right - padding * 2).max(1);
        let h = self.font.height() + self.s(10);
        let combo_width = (text_width("中文中文中文中文", &self.font) + self.s(30)).min(width);
        let font_width =
            (text_width("中文中文中文中文中文中文中文中文", &self.font) + self.s(30)).min(width);
        let number_width = text_width("0000", &self.font) + self.s(16);
        let button_width = self.s(68);
        let name_width = combo_width;
        let expression_width = (combo_width * 2).min(width);
        let mut y = padding;
        for row in &self.rows {
            let hint = if row.field.hint.is_empty() {
                0
            } else {
                measured(&row.field.hint, &self.hint_font, width)
            };
            if !row.c(1).is_null() {
                let label = measured(&row.field.label, &self.font, width);
                place(row.c(1), padding, y - self.scroll[1], width, label);
                y += label + gap;
            }
            match row.field.kind {
                Kind::Bool | Kind::Preview => {
                    let height =
                        measured(&row.field.label, &self.font, width - self.s(28)) + self.s(6);
                    place(row.c(0), padding, y - self.scroll[1], width, height);
                    y += height;
                }
                Kind::Choice(_) | Kind::Font => {
                    let control_width = if matches!(row.field.kind, Kind::Font) {
                        font_width
                    } else {
                        combo_width
                    };
                    place(row.c(0), padding, y - self.scroll[1], control_width, h * 10);
                    send(
                        row.c(0),
                        CB_SETITEMHEIGHT,
                        usize::MAX,
                        (h - self.s(4)) as isize,
                    );
                    send(row.c(0), CB_SETDROPPEDWIDTH, control_width as usize, 0);
                    y += h;
                }
                Kind::Mapping => {
                    let preset = row.field.section == "presets";
                    let input_width = if row.field.section == "grid" {
                        number_width
                    } else {
                        name_width
                    };
                    place(row.c(3), padding, y - self.scroll[1], input_width, h);
                    if preset {
                        place(
                            row.c(4),
                            padding + input_width + gap,
                            y - self.scroll[1],
                            button_width,
                            h,
                        );
                        y += h + gap;
                        place(row.c(0), padding, y - self.scroll[1], expression_width, h);
                        y += h + gap;
                        place(row.c(5), padding, y - self.scroll[1], width, h);
                        y += h;
                    } else {
                        let fits = input_width + combo_width + button_width + gap * 2 <= width;
                        let x = if fits {
                            padding + input_width + gap
                        } else {
                            padding
                        };
                        if !fits {
                            y += h + gap;
                        }
                        place(row.c(0), x, y - self.scroll[1], combo_width, h * 10);
                        send(
                            row.c(0),
                            CB_SETITEMHEIGHT,
                            usize::MAX,
                            (h - self.s(4)) as isize,
                        );
                        send(row.c(0), CB_SETDROPPEDWIDTH, combo_width as usize, 0);
                        place(
                            row.c(4),
                            x + combo_width + gap,
                            y - self.scroll[1],
                            button_width,
                            h,
                        );
                        y += h;
                    }
                }
                Kind::Color(channels) => {
                    let color_width = (combo_width + self.s(80)).min(width);
                    place(row.c(0), padding, y - self.scroll[1], color_width, h);
                    y += h + gap;
                    let unit = number_width + self.s(28);
                    let columns = (width / unit).clamp(1, channels.count() as i32) as usize;
                    for channel in 0..channels.count() {
                        let x = padding + (channel % columns) as i32 * unit;
                        let top = y + (channel / columns) as i32 * (h + gap) - self.scroll[1];
                        place(
                            row.c(channel + 7),
                            x,
                            top + (h - self.font.height()) / 2,
                            self.s(18),
                            self.font.height(),
                        );
                        place(row.c(channel + 3), x + self.s(20), top, number_width, h);
                    }
                    y += channels.count().div_ceil(columns) as i32 * (h + gap) - gap;
                }
                Kind::Fallbacks => {
                    let list_width = (combo_width * 2).min(width);
                    place(row.c(0), padding, y - self.scroll[1], list_width, h * 3);
                    y += h * 3 + gap;
                    place(row.c(3), padding, y - self.scroll[1], font_width, h * 10);
                    send(
                        row.c(3),
                        CB_SETITEMHEIGHT,
                        usize::MAX,
                        (h - self.s(4)) as isize,
                    );
                    send(row.c(3), CB_SETDROPPEDWIDTH, font_width as usize, 0);
                    place(
                        row.c(4),
                        padding + font_width + gap,
                        y - self.scroll[1],
                        button_width,
                        h,
                    );
                    y += h + gap;
                    for slot in 5..=7 {
                        place(
                            row.c(slot),
                            padding + (slot as i32 - 5) * (button_width + gap),
                            y - self.scroll[1],
                            button_width,
                            h,
                        );
                    }
                    y += h;
                }
                Kind::Note => {}
                Kind::Number => {
                    place(
                        row.c(0),
                        padding,
                        y - self.scroll[1],
                        if row.field.key == "duration_ms" {
                            number_width + self.s(20)
                        } else {
                            number_width
                        },
                        h,
                    );
                    y += h;
                }
                Kind::Add => {
                    place(row.c(0), padding, y - self.scroll[1], combo_width, h);
                    y += h;
                }
                Kind::Text => {
                    let text = caption(row.c(0));
                    let text_height =
                        measured(&text, &self.font, (expression_width - self.s(12)).max(1))
                            + self.s(10);
                    place(
                        row.c(0),
                        padding,
                        y - self.scroll[1],
                        expression_width,
                        text_height.max(h),
                    );
                    y += text_height.max(h);
                }
            }
            if hint > 0 {
                y += self.s(2);
                place(row.c(2), padding, y - self.scroll[1], width, hint);
                y += hint;
            }
            y += self.s(12);
            if self.tab == 5 && row.field.section == "crosshair" {
                for (i, control) in self.axis_controls.iter().enumerate() {
                    place(
                        *control,
                        padding + i as i32 * (width / 3),
                        y - self.scroll[1],
                        width / 3,
                        h,
                    );
                }
                y += h + self.s(10);
            }
        }
        self.extent[1] = y + padding;
        let mut adjusted = false;
        for pane in 0..2 {
            let old = self.scroll[pane];
            self.scroll[pane] = old.clamp(0, (self.extent[pane] - height).max(0));
            adjusted |= old != self.scroll[pane];
            let info = SCROLLINFO {
                cbSize: size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                nMin: 0,
                nMax: self.extent[pane] - 1,
                nPage: height as u32,
                nPos: self.scroll[pane],
                ..Default::default()
            };
            unsafe {
                SetScrollInfo(self.panes[pane], SB_VERT, &info, 1);
            }
        }
        unsafe {
            GetClientRect(self.panes[1], &mut rect);
        }
        if adjusted || rect.right != initial_right_width {
            self.layout()?;
        }
        Ok(())
    }
    fn sidebar_width(&self, total: i32) -> i32 {
        let mut widest = 0;
        unsafe {
            let dc = GetDC(self.window.0);
            let old = SelectObject(dc, self.font.0);
            for (en, zh) in TABS {
                let text = wide(self.cfg.tr(en, zh));
                let mut size = SIZE::default();
                GetTextExtentPoint32W(dc, text.as_ptr(), (text.len() - 1) as i32, &mut size);
                widest = widest.max(size.cx);
            }
            SelectObject(dc, old);
            ReleaseDC(self.window.0, dc);
        }
        sidebar_size(total, self.dpi, widest)
    }
    fn scroll_by(&mut self, pane: usize, command: u16, delta: i32) -> Result<(), String> {
        let before = self.scroll[pane];
        let mut rect = RECT::default();
        unsafe {
            GetClientRect(self.panes[pane], &mut rect);
        }
        let amount = match command as i32 {
            SB_LINEUP => -self.s(40),
            SB_LINEDOWN => self.s(40),
            SB_PAGEUP => -rect.bottom,
            SB_PAGEDOWN => rect.bottom,
            _ => delta,
        };
        if command as i32 == SB_THUMBTRACK || command as i32 == SB_THUMBPOSITION {
            let mut info = SCROLLINFO {
                cbSize: size_of::<SCROLLINFO>() as u32,
                fMask: SIF_TRACKPOS,
                ..Default::default()
            };
            unsafe {
                GetScrollInfo(self.panes[pane], SB_VERT, &mut info);
            }
            self.scroll[pane] = info.nTrackPos;
        } else {
            self.scroll[pane] = self.scroll[pane].saturating_add(amount);
        }
        self.scroll[pane] = self.scroll[pane].clamp(0, (self.extent[pane] - rect.bottom).max(0));
        if before == self.scroll[pane] {
            return Ok(());
        }
        // Move existing child controls only; measurements and row layout stay unchanged.
        let _redraw = RedrawGuard::new(self.panes[pane]);
        unsafe {
            let mut child = GetWindow(self.panes[pane], GW_CHILD);
            let mut batch = BeginDeferWindowPos(64);
            while !child.is_null() {
                if GetWindowLongW(child, GWL_STYLE) as u32 & WS_VISIBLE == 0 {
                    child = GetWindow(child, GW_HWNDNEXT);
                    continue;
                }
                let mut position = RECT::default();
                GetWindowRect(child, &mut position);
                MapWindowPoints(
                    null_mut(),
                    self.panes[pane],
                    (&mut position as *mut RECT).cast(),
                    2,
                );
                batch = DeferWindowPos(
                    batch,
                    child,
                    null_mut(),
                    position.left,
                    position.top + before - self.scroll[pane],
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOSIZE | SWP_NOCOPYBITS,
                );
                child = GetWindow(child, GW_HWNDNEXT);
            }
            if !batch.is_null() {
                EndDeferWindowPos(batch);
            }
            SetScrollPos(self.panes[pane], SB_VERT, self.scroll[pane], 1);
        }
        Ok(())
    }
    fn selected_value(row: &Row, slot: usize) -> String {
        let i = send(row.c(slot), CB_GETCURSEL, 0, 0);
        row.options
            .get(i as usize)
            .map(|v| v.0.clone())
            .unwrap_or_default()
    }
    fn collect(&self) -> Result<DocumentMut, String> {
        let mut draft = self.doc.clone();
        for &index in &self.dirty {
            let Some(row) = self.rows.get(index) else {
                continue;
            };
            let section = &row.field.section;
            let key = &row.field.key;
            let replacement = match &row.field.kind {
                Kind::Bool => value(send(row.c(0), BM_GETCHECK, 0, 0) == 1),
                Kind::Number => {
                    let mut n = caption(row.c(0)).trim().parse::<i64>().map_err(|_| {
                        format!(
                            "{section}.{key}: {}",
                            self.cfg.tr("enter an integer", "请输入整数")
                        )
                    })?;
                    if section == "crosshair" && key == "fps" {
                        n = n.clamp(24, 1000);
                    }
                    value(n)
                }
                Kind::Text => value(caption(row.c(0))),
                Kind::Font | Kind::Choice(_) => value(Self::selected_value(row, 0)),
                Kind::Color(format) => {
                    let channels: Vec<u8> = (3..3 + format.count())
                        .map(|slot| {
                            caption(row.c(slot)).parse::<u8>().map_err(|_| {
                                format!(
                                    "{} {}",
                                    format.label(),
                                    self.cfg.tr("must be 0–255", "必须为 0–255")
                                )
                            })
                        })
                        .collect::<Result<_, _>>()?;
                    if *format == ColorChannels::Rgba {
                        draft[section]["opacity"] = value(i64::from(channels[3]));
                    }
                    value(format!(
                        "#{:02X}{:02X}{:02X}",
                        channels[0], channels[1], channels[2]
                    ))
                }
                Kind::Mapping => {
                    let name = caption(row.c(3)).trim().to_owned();
                    if name.is_empty()
                        || name.eq_ignore_ascii_case("esc")
                        || name.eq_ignore_ascii_case("escape")
                    {
                        return Err(self
                            .cfg
                            .tr(
                                "A nonempty name other than Esc is required",
                                "名称不可为空，也不可为 Esc",
                            )
                            .into());
                    }
                    let text = if section == "presets" {
                        caption(row.c(0))
                    } else {
                        Self::selected_value(row, 0)
                    };
                    if name != *key {
                        if draft[section]
                            .as_table()
                            .unwrap()
                            .iter()
                            .any(|(existing, _)| {
                                existing != key && existing.eq_ignore_ascii_case(&name)
                            })
                        {
                            return Err(self.cfg.tr("Name already exists", "名称已存在").into());
                        }
                        rename_entry(draft[section].as_table_mut().unwrap(), key, &name);
                        if section == "presets" {
                            for bindings in ["keys", "commands", "grid"] {
                                for (_, item) in draft[bindings].as_table_mut().unwrap().iter_mut()
                                {
                                    if item.as_str().is_some_and(|v| v.eq_ignore_ascii_case(key)) {
                                        *item = value(&name);
                                    }
                                }
                            }
                        }
                    }
                    draft[section][&name] = value(text);
                    continue;
                }
                _ => continue,
            };
            draft[section][key] = replacement;
            if self.tab == 5 && self.axis == 2 && section == "horizontal" {
                draft["vertical"][key] = draft[section][key].clone();
                if key == "color" {
                    draft["vertical"]["opacity"] = draft[section]["opacity"].clone();
                }
            }
        }
        Config::parse(&draft)?;
        Ok(draft)
    }
    fn save(&mut self) -> Result<(), String> {
        self.pending = None;
        if self.dirty.is_empty() {
            return Ok(());
        }
        let draft = self.collect()?;
        let rebuild = self.dirty.iter().any(|i| {
            self.rows
                .get(*i)
                .is_some_and(|r| r.field.section == "general" && r.field.key == "language")
        });
        for &index in &self.dirty {
            if let Some(row) = self.rows.get(index)
                && self.selected.as_ref() == Some(&row.field.key)
                && row.field.section == "presets"
            {
                self.selected = Some(caption(row.c(3)).trim().to_owned());
            }
        }
        let saved = config::save_settings(&self.path, &self.original, &draft)?;
        self.doc = saved.parse::<DocumentMut>().map_err(|e| e.to_string())?;
        self.original = saved;
        self.cfg = Config::parse(&self.doc)?;
        for row in &mut self.rows {
            if matches!(row.field.kind, Kind::Mapping) {
                let name = caption(row.c(3)).trim().to_owned();
                if self.doc[&row.field.section].get(&name).is_some() && name != row.field.key {
                    row.field.key = name.clone();
                    row.field.label = name.clone();
                    set_text(row.c(1), &name);
                }
            }
        }
        self.dirty.clear();
        self.clock.reset(self.cfg.fps);
        self.status(self.cfg.tr("Saved", "已保存"));
        self.invalidate_pages();
        if rebuild {
            self.rebuild()?;
        } else {
            self.update_values();
            if self.tab != 0 && self.tab != 4 {
                self.render()?;
            }
        }
        Ok(())
    }
    fn update_values(&self) {
        for row in &self.rows {
            let item = self
                .doc
                .get(&row.field.section)
                .and_then(|s| s.get(&row.field.key));
            match row.field.kind {
                Kind::Preview => {
                    send(row.c(0), BM_SETCHECK, usize::from(self.live), 0);
                }
                Kind::Bool => {
                    send(
                        row.c(0),
                        BM_SETCHECK,
                        usize::from(item.and_then(Item::as_bool).unwrap_or(false)),
                        0,
                    );
                }
                Kind::Number => {
                    if let Some(n) = item.and_then(Item::as_integer) {
                        set_text(row.c(0), &n.to_string());
                    }
                }
                Kind::Text => {
                    if let Some(text) = item.and_then(Item::as_str) {
                        set_text(row.c(0), text);
                    }
                }
                Kind::Mapping => {
                    set_text(row.c(3), &row.field.key);
                    if row.field.section == "presets" {
                        send(
                            row.c(5),
                            BM_SETCHECK,
                            usize::from(self.selected.as_ref() == Some(&row.field.key)),
                            0,
                        );
                    }
                    if let Some(text) = item.and_then(Item::as_str) {
                        if row.field.section == "presets" {
                            set_text(row.c(0), text);
                        } else if let Some(index) = row
                            .options
                            .iter()
                            .position(|option| option.0.eq_ignore_ascii_case(text))
                        {
                            send(row.c(0), CB_SETCURSEL, index, 0);
                        }
                    }
                }
                Kind::Choice(_) | Kind::Font => {
                    if let Some(v) = item.and_then(Item::as_str)
                        && let Some(i) =
                            row.options.iter().position(|o| o.0.eq_ignore_ascii_case(v))
                    {
                        send(row.c(0), CB_SETCURSEL, i, 0);
                    }
                }
                Kind::Fallbacks => {
                    let selected = send(row.c(0), LB_GETCURSEL, 0, 0);
                    send(row.c(0), LB_RESETCONTENT, 0, 0);
                    if let Some(array) = item.and_then(Item::as_array) {
                        for name in array.iter().filter_map(Value::as_str) {
                            send(row.c(0), LB_ADDSTRING, 0, wide(name).as_ptr() as isize);
                        }
                    }
                    if selected >= 0 {
                        send(row.c(0), LB_SETCURSEL, selected as usize, 0);
                    }
                }
                Kind::Color(channels) => {
                    if let Some(text) = item.and_then(Item::as_str) {
                        set_text(row.c(0), text);
                        let rgb =
                            u32::from_str_radix(text.trim_start_matches('#'), 16).unwrap_or(0);
                        let a = self.doc[&row.field.section]
                            .get("opacity")
                            .and_then(Item::as_integer)
                            .unwrap_or(255);
                        for (i, n) in [(rgb >> 16) & 255, (rgb >> 8) & 255, rgb & 255, a as u32]
                            .into_iter()
                            .take(channels.count())
                            .enumerate()
                        {
                            set_text(row.c(i + 3), &n.to_string());
                        }
                        unsafe {
                            InvalidateRect(row.c(0), null(), 1);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    fn render(&mut self) -> Result<(), String> {
        self.preview.hide();
        self.overlay.hide_tip();
        if !unsafe { GetPropW(self.window.0, w!("PreviewSuspended")) }.is_null() {
            self.overlay.guides(0, &self.cfg)?;
            input::set_guides(false)?;
            return input::track_preview(false);
        }
        let guides = if self.tab == 5 && self.live {
            match self.axis {
                0 => 1,
                1 => 2,
                _ => 3,
            }
        } else {
            0
        };
        self.overlay.guides(guides, &self.cfg)?;
        if self.tab == 1 && self.cfg.show != "off" {
            let text = match self.cfg.show.as_str() {
                "time" => desktop::time_text(&self.cfg.time_format),
                "monitor" => format!(
                    "{} {}",
                    self.cfg.tr("Monitor", "屏幕"),
                    desktop::screen(desktop::cursor()?, 0)?.number
                ),
                _ => desktop::window_text(self.window.0, &self.cfg.window_format),
            };
            self.overlay.tip(&text, &self.cfg)?;
        } else if self.live {
            match self.tab {
                2 => {
                    if let Some(name) = &self.selected {
                        self.preview.select(self.window.0, name, &self.cfg)?;
                    }
                }
                3 => self.preview.start(self.window.0, &self.cfg)?,
                6 => self.preview.grid(self.window.0, &self.cfg)?,
                _ => {}
            }
        }
        input::set_guides(guides != 0)?;
        input::track_preview(self.preview.visible())
    }
    fn commit_document(&mut self, draft: DocumentMut) -> Result<(), String> {
        Config::parse(&draft)?;
        self.original = config::save_settings(&self.path, &self.original, &draft)?;
        self.doc = self
            .original
            .parse::<DocumentMut>()
            .map_err(|e| e.to_string())?;
        self.cfg = Config::parse(&self.doc)?;
        self.dirty.clear();
        self.pending = None;
        self.status(self.cfg.tr("Saved", "已保存"));
        let expected = schema::fields(self.tab, self.axis, &self.cfg, &self.doc);
        if expected.len() == self.rows.len()
            && expected
                .iter()
                .zip(&self.rows)
                .all(|(a, b)| a.section == b.field.section && a.key == b.field.key)
        {
            self.invalidate_pages();
            self.update_values();
            self.render()
        } else {
            self.rebuild()
        }
    }
    fn add(&mut self, section: &str) -> Result<(), String> {
        self.save()?;
        let mut draft = self.doc.clone();
        let names: Vec<String> = match section {
            "keys" => (2..=24).map(|n| format!("F{n}")).collect(),
            "grid" => (1..=999)
                .filter(|n| n % 10 != 0)
                .map(|n| n.to_string())
                .collect(),
            "presets" => (1..=999).map(|n| format!("Region{n}")).collect(),
            _ => (1..=999).map(|n| format!("Command{n}")).collect(),
        };
        let name = names
            .into_iter()
            .find(|n| {
                !draft[section]
                    .as_table()
                    .unwrap()
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case(n))
            })
            .ok_or("No free name")?;
        let text = if section == "presets" {
            "10sw|10sh|80sw|80sh".to_owned()
        } else if section == "grid" {
            self.cfg
                .preset_names
                .values()
                .min()
                .cloned()
                .unwrap_or_default()
        } else {
            "Help".into()
        };
        draft[section][&name] = value(text);
        if section == "presets" {
            self.selected = Some(name);
        }
        self.commit_document(draft)
    }
    fn delete(&mut self, index: usize) -> Result<(), String> {
        // Save may rebuild the rows, so capture the identity first.
        let row = self.rows.get(index).ok_or("Setting")?;
        let section = row.field.section.clone();
        let key = row.field.key.clone();
        self.save()?;
        let mut draft = self.doc.clone();
        draft[&section].as_table_mut().unwrap().remove(&key);
        if section == "presets" {
            for section in ["keys", "commands", "grid"] {
                for (_, item) in draft[section].as_table_mut().unwrap().iter_mut() {
                    if item.as_str().is_some_and(|n| n.eq_ignore_ascii_case(&key)) {
                        *item = value("");
                    }
                }
            }
            if self.selected.as_ref() == Some(&key) {
                self.selected = None;
            }
        }
        self.commit_document(draft)
    }
    fn font_list(&mut self, index: usize, action: usize) -> Result<(), String> {
        self.save()?;
        let row = &self.rows[index];
        let section = row.field.section.clone();
        let key = row.field.key.clone();
        let selected = send(row.c(0), LB_GETCURSEL, 0, 0);
        let mut draft = self.doc.clone();
        let array = draft[&section][&key]
            .as_array_mut()
            .ok_or("font_fallbacks")?;
        if action == 4 {
            if array.len() >= 3 {
                return Err(self
                    .cfg
                    .tr("At most three fallback fonts", "最多添加三个回退字体")
                    .into());
            }
            let name = Self::selected_value(row, 3);
            if !array.iter().any(|font| font.as_str() == Some(&name)) {
                array.push(name);
            }
        } else if selected >= 0 && (selected as usize) < array.len() {
            let selected = selected as usize;
            if action == 5 {
                array.remove(selected);
            } else if action == 6 && selected > 0 {
                let font = array.remove(selected);
                array.insert(selected - 1, font);
            } else if action == 7 && selected + 1 < array.len() {
                let font = array.remove(selected);
                array.insert(selected + 1, font);
            }
        }
        self.commit_document(draft)
    }
    fn reorder_font(&mut self, index: usize, from: usize, to: usize) -> Result<(), String> {
        self.save()?;
        let row = self.rows.get(index).ok_or("Font list")?;
        let mut draft = self.doc.clone();
        let array = draft[&row.field.section][&row.field.key]
            .as_array_mut()
            .ok_or("Font list")?;
        if from < array.len() && to < array.len() && from != to {
            let font = array.remove(from);
            array.insert(to, font);
            self.commit_document(draft)?;
        }
        Ok(())
    }
    fn color_dialog(&mut self, index: usize) -> Result<(), String> {
        let row = &self.rows[index];
        let rgb = u32::from_str_radix(caption(row.c(0)).trim_start_matches('#'), 16).unwrap_or(0);
        let mut colors = [0u32; 16];
        let mut dialog = CHOOSECOLORW {
            lStructSize: size_of::<CHOOSECOLORW>() as u32,
            hwndOwner: self.window.0,
            rgbResult: (rgb >> 16) | (rgb & 0xff00) | ((rgb & 255) << 16),
            lpCustColors: colors.as_mut_ptr(),
            Flags: CC_FULLOPEN | CC_RGBINIT,
            ..Default::default()
        };
        if unsafe { ChooseColorW(&mut dialog) } != 0 {
            let result = dialog.rgbResult;
            for (channel, n) in [result & 255, (result >> 8) & 255, (result >> 16) & 255]
                .into_iter()
                .enumerate()
            {
                set_text(row.c(channel + 3), &n.to_string());
            }
            self.dirty.insert(index);
            self.save()?;
        }
        Ok(())
    }
    fn command(&mut self, id: i32, event: u32, sender: HWND) -> Result<(), String> {
        if event == CBN_DROPDOWN && id >= BASE {
            self.active_combo = Some(sender);
        }
        if event == CBN_CLOSEUP {
            self.active_combo = None;
            self.tooltip.hide();
        }
        if (TAB_BASE..TAB_BASE + 7).contains(&id) && event == BN_CLICKED {
            self.save()?;
            return self.switch_page((id - TAB_BASE) as usize, self.axis);
        }
        if matches!(id, AXIS_H | AXIS_V | AXIS_BOTH) && event == BN_CLICKED {
            self.save()?;
            if id == AXIS_BOTH && self.axis != 2 {
                let mut draft = self.doc.clone();
                draft["vertical"] = draft["horizontal"].clone();
                self.original = config::save_settings(&self.path, &self.original, &draft)?;
                self.doc = self
                    .original
                    .parse::<DocumentMut>()
                    .map_err(|error| error.to_string())?;
                self.cfg = Config::parse(&self.doc)?;
                self.invalidate_pages();
                self.status(self.cfg.tr("Saved", "已保存"));
            }
            return self.switch_page(self.tab, (id - AXIS_H) as usize);
        }
        if id < BASE {
            return Ok(());
        }
        let index = ((id - BASE) / STRIDE) as usize;
        let slot = ((id - BASE) % STRIDE) as usize;
        let Some(row) = self.rows.get(index) else {
            return Ok(());
        };
        if row.c(slot) != sender {
            return Ok(());
        }
        let kind = row.field.kind.clone();
        match kind {
            Kind::Preview if event == BN_CLICKED => {
                self.live = send(row.c(0), BM_GETCHECK, 0, 0) == 1;
                return self.render();
            }
            Kind::Add if event == BN_CLICKED => {
                let section = row.field.section.clone();
                return self.add(&section);
            }
            Kind::Mapping if slot == 4 && event == BN_CLICKED => return self.delete(index),
            Kind::Mapping if slot == 5 && event == BN_CLICKED => {
                self.selected = Some(row.field.key.clone());
                for row in &self.rows {
                    if !row.c(5).is_null() && row.field.section == "presets" {
                        send(
                            row.c(5),
                            BM_SETCHECK,
                            usize::from(self.selected.as_ref() == Some(&row.field.key)),
                            0,
                        );
                    }
                }
                return self.render();
            }
            Kind::Fallbacks if (4..=7).contains(&slot) && event == BN_CLICKED => {
                return self.font_list(index, slot);
            }
            Kind::Fallbacks => return Ok(()),
            Kind::Color(_) if slot == 0 && event == BN_CLICKED => return self.color_dialog(index),
            Kind::Note => return Ok(()),
            _ => {}
        }
        let changed = match kind {
            Kind::Bool => event == BN_CLICKED,
            Kind::Choice(_) | Kind::Font => event == CBN_SELCHANGE,
            Kind::Mapping if slot == 0 && row.field.section != "presets" => event == CBN_SELCHANGE,
            _ => event == EN_CHANGE,
        };
        if changed {
            self.dirty.insert(index);
            self.pending = Some(Instant::now() + Duration::from_millis(450));
            if let Ok(draft) = self.collect()
                && let Ok(cfg) = Config::parse(&draft)
            {
                self.cfg = cfg;
                if self.tab != 0 && self.tab != 4 {
                    self.render()?;
                }
                if matches!(kind, Kind::Color(_)) {
                    unsafe {
                        InvalidateRect(self.rows[index].c(0), null(), 1);
                    }
                }
            }
        }
        Ok(())
    }
    fn tick(&mut self) -> Result<(), String> {
        if !unsafe { RemovePropW(self.window.0, w!("PreviewActivationPending")) }.is_null() {
            self.render()?;
        }
        let now = Instant::now();
        // Native combo popups capture the pointer and do not reliably deliver list mouse-move
        // messages to subclasses. Hit-test the open list on the existing UI timer as well.
        if let Some(combo) = self.active_combo {
            self.hover(combo, -1);
        }
        self.tooltip.tick();
        if self
            .status_until
            .get()
            .is_some_and(|deadline| now >= deadline)
        {
            self.status_until.set(None);
            set_text(self.status, "");
            self.layout_status();
        }
        if self.pending.is_some_and(|when| now >= when) {
            self.save()?;
        }
        self.preview.tick(&self.cfg)?;
        if self.dirty.is_empty() && now.duration_since(self.disk_check) >= Duration::from_secs(1) {
            self.disk_check = now;
            let disk = config::read_settings(&self.path)?;
            if disk != self.original {
                let loaded = config::load_snapshot(&self.path, None)?;
                self.original = loaded.text;
                self.doc = loaded.document;
                self.cfg = loaded.config;
                if self.cfg.warnings.is_empty() {
                    self.status(
                        self.cfg
                            .tr("Manual file changes applied", "已应用配置文件中的修改"),
                    );
                } else {
                    self.status(&format!(
                        "{}: {}",
                        self.cfg.tr(
                            "Invalid fields restored to defaults",
                            "以下字段有误，已恢复默认值"
                        ),
                        self.cfg.warnings.join(", ")
                    ));
                }
                let expected = schema::fields(self.tab, self.axis, &self.cfg, &self.doc);
                if expected.len() != self.rows.len()
                    || expected.iter().zip(&self.rows).any(|(a, b)| {
                        a.section != b.field.section
                            || a.key != b.field.key
                            || a.label != b.field.label
                    })
                {
                    self.rebuild()?;
                } else {
                    self.invalidate_pages();
                    self.update_values();
                    self.render()?;
                }
            }
        }
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
    fn hover(&mut self, source: HWND, item: isize) {
        let combo = unsafe { GetPropW(source, w!("OwnerCombo")) };
        let control = if combo.is_null() { source } else { combo };
        let id = unsafe { GetDlgCtrlID(control) };
        let Some(index) = id
            .checked_sub(BASE)
            .filter(|n| *n >= 0)
            .map(|n| (n / STRIDE) as usize)
        else {
            self.tooltip.hide();
            return;
        };
        let Some(row) = self.rows.get(index) else {
            self.tooltip.hide();
            return;
        };
        let selected = if item < 0 && send(control, CB_GETDROPPEDSTATE, 0, 0) != 0 {
            let mut info = COMBOBOXINFO {
                cbSize: size_of::<COMBOBOXINFO>() as u32,
                ..Default::default()
            };
            let Ok(mut point) = desktop::cursor() else {
                self.tooltip.hide();
                return;
            };
            unsafe {
                GetComboBoxInfo(control, &mut info);
                ScreenToClient(info.hwndList, &mut point);
            }
            let hit = send(
                info.hwndList,
                LB_ITEMFROMPOINT,
                0,
                ((point.x as u16 as u32) | ((point.y as u16 as u32) << 16)) as isize,
            );
            if hit >> 16 != 0 {
                self.tooltip.hide();
                return;
            }
            hit & 0xffff
        } else if item < 0 {
            send(control, CB_GETCURSEL, 0, 0)
        } else {
            item
        };
        let Some((_, label)) = row.options.get(selected as usize) else {
            self.tooltip.hide();
            return;
        };
        let mut rect = RECT::default();
        unsafe {
            GetClientRect(control, &mut rect);
        }
        let font = &self.font;
        let overflow = text_width(label, font) > rect.right - self.s(30);
        if let Ok(point) = desktop::cursor() {
            self.tooltip.hover(overflow.then(|| label.clone()), point);
        }
    }
    fn escape(&mut self) -> Result<(), String> {
        self.tooltip.hide();
        self.overlay.hide_tip();
        if !self.preview.reset(&self.cfg)? {
            self.preview.hide();
            input::track_preview(false)?;
        }
        Ok(())
    }
}
fn populate(hwnd: HWND, options: &[(String, String)], selected: &str, height: i32) {
    send(hwnd, WM_SETREDRAW, 0, 0);
    for (i, (value, label)) in options.iter().enumerate() {
        send(hwnd, CB_ADDSTRING, 0, wide(label).as_ptr() as isize);
        if value.eq_ignore_ascii_case(selected) {
            send(hwnd, CB_SETCURSEL, i, 0);
        }
    }
    send(hwnd, CB_SETITEMHEIGHT, usize::MAX, height as isize);
    send(hwnd, CB_SETITEMHEIGHT, 0, height as isize);
    send(hwnd, WM_SETREDRAW, 1, 0);
}
fn rename_entry(table: &mut toml_edit::Table, old: &str, new: &str) {
    let order: HashMap<_, _> = table
        .iter()
        .enumerate()
        .map(|(index, (key, _))| {
            (
                if key == old {
                    new.to_owned()
                } else {
                    key.to_owned()
                },
                index,
            )
        })
        .collect();
    if let Some((_, item)) = table.remove_entry(old) {
        table.insert(new, item);
    }
    table.sort_values_by(|a, _, b, _| order.get(a.get()).cmp(&order.get(b.get())));
}
fn sidebar_size(total: i32, dpi: i32, text_width: i32) -> i32 {
    let scale = |value| (value * dpi + 48) / 96;
    (text_width + scale(56))
        .clamp(scale(160), scale(240))
        .min(total * 28 / 100)
}
fn with_editor(action: impl FnOnce(&mut Editor) -> Result<(), String>) {
    EDITOR.with(|slot| {
        if let Ok(mut guard) = slot.try_borrow_mut()
            && let Some(editor) = guard.as_mut()
        {
            UPDATING.with(|flag| flag.set(true));
            if let Err(error) = action(editor) {
                editor.status(&error);
            }
            UPDATING.with(|flag| flag.set(false));
        }
    });
}
fn size_dropdown(root: HWND, combo: HWND, position: &mut WINDOWPOS) {
    // SAFETY: all handles belong to this UI thread; WINDOWPOS is live for the callback.
    unsafe {
        let mut window = RECT::default();
        let mut control = RECT::default();
        let mut monitor = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetClientRect(root, &mut window) == 0
            || GetWindowRect(combo, &mut control) == 0
            || GetMonitorInfoW(
                MonitorFromWindow(combo, MONITOR_DEFAULTTONEAREST),
                &mut monitor,
            ) == 0
        {
            return;
        }
        let row = send(combo, CB_GETITEMHEIGHT, 0, 0).max(1) as i32;
        let border = 2 * GetSystemMetricsForDpi(SM_CYBORDER, GetDpiForWindow(root).max(96));
        let below = (monitor.rcWork.bottom - control.bottom).max(0);
        let above = (control.top - monitor.rcWork.top).max(0);
        let limit = window.bottom.min(below.max(above));
        let count = send(combo, CB_GETCOUNT, 0, 0).clamp(1, 8) as i32;
        let rows = ((limit - border) / row).max(1).min(count);
        position.cy = (rows * row + border).min(limit).max(1);
        position.y = if below >= position.cy || below >= above {
            control.bottom
        } else {
            control.top - position.cy
        };
        position.y = position.y.clamp(
            monitor.rcWork.top,
            (monitor.rcWork.bottom - position.cy).max(monitor.rcWork.top),
        );
        position.flags &= !(SWP_NOSIZE | SWP_NOMOVE);
    }
}
unsafe extern "system" fn control_proc(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    root: usize,
) -> LRESULT {
    unsafe {
        let combo = GetPropW(hwnd, w!("OwnerCombo"));
        if message == WM_WINDOWPOSCHANGING
            && !combo.is_null()
            && (*(lp as *const WINDOWPOS)).flags & SWP_NOSIZE == 0
        {
            let result = DefSubclassProc(hwnd, message, wp, lp);
            // SAFETY: Windows supplies a writable WINDOWPOS for WM_WINDOWPOSCHANGING.
            size_dropdown(root as HWND, combo, &mut *(lp as *mut WINDOWPOS));
            return result;
        }
        if message == WM_NCDESTROY {
            RemovePropW(hwnd, w!("OwnerCombo"));
            RemoveWindowSubclass(hwnd, Some(control_proc), 1);
            return DefSubclassProc(hwnd, message, wp, lp);
        }
        let mut class = [0u16; 32];
        let length = GetClassNameW(hwnd, class.as_mut_ptr(), 32);
        let class = String::from_utf16_lossy(&class[..length as usize]);
        if class.eq_ignore_ascii_case("edit") && matches!(message, WM_SIZE | EDIT_LAYOUT) {
            let result = DefSubclassProc(hwnd, message, wp, lp);
            let dc = GetDC(hwnd);
            let old = SelectObject(dc, send(hwnd, WM_GETFONT, 0, 0) as HFONT);
            let mut metrics = TEXTMETRICW::default();
            GetTextMetricsW(dc, &mut metrics);
            SelectObject(dc, old);
            ReleaseDC(hwnd, dc);
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect);
            let padding = (4 * GetDpiForWindow(hwnd).max(96) / 96) as i32;
            rect.left += padding;
            rect.right -= padding;
            rect.top = if send(hwnd, EM_GETLINECOUNT, 0, 0) <= 1 {
                ((rect.bottom - metrics.tmHeight) / 2).max(padding)
            } else {
                padding
            };
            // The format box must contain a whole line even when top padding is nonzero.
            // GDI otherwise resets the edit rectangle to its default top-aligned position.
            rect.bottom = (rect.top + metrics.tmHeight).max(rect.bottom - padding);
            send(hwnd, EM_SETRECT, 0, (&rect as *const RECT) as isize);
            return result;
        }
        if class.eq_ignore_ascii_case("edit") && message == WM_CHAR && wp == 13 {
            return 0;
        }
        if class.eq_ignore_ascii_case("edit")
            && matches!(
                message,
                WM_SETTEXT | WM_SETFONT | WM_CHAR | EM_REPLACESEL | WM_PASTE
            )
        {
            let result = DefSubclassProc(hwnd, message, wp, lp);
            PostMessageW(hwnd, EDIT_LAYOUT, 0, 0);
            return result;
        }
        if message == WM_MOUSEMOVE {
            let combo = GetPropW(hwnd, w!("OwnerCombo"));
            if class.eq_ignore_ascii_case("combobox") || !combo.is_null() {
                let item = if combo.is_null() {
                    -1
                } else {
                    let item = send(hwnd, LB_ITEMFROMPOINT, 0, lp);
                    if item >> 16 != 0 { -2 } else { item & 0xffff }
                };
                PostMessageW(root as HWND, HOVER, hwnd as usize, item);
                let mut track = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut track);
            }
        }
        if message == WM_MOUSELEAVE {
            PostMessageW(root as HWND, HOVER, 0, 0);
        }
        if message == WM_MOUSEWHEEL {
            let owner = GetPropW(hwnd, w!("OwnerCombo"));
            let combo = if class.eq_ignore_ascii_case("combobox") {
                hwnd
            } else {
                owner
            };
            if !combo.is_null() && send(combo, CB_GETDROPPEDSTATE, 0, 0) != 0 {
                let mut info = COMBOBOXINFO {
                    cbSize: size_of::<COMBOBOXINFO>() as u32,
                    ..Default::default()
                };
                GetComboBoxInfo(combo, &mut info);
                let wheel = (wp >> 16) as u16 as i16 as i32;
                let top = send(info.hwndList, LB_GETTOPINDEX, 0, 0) as i32;
                let count = send(info.hwndList, LB_GETCOUNT, 0, 0) as i32;
                send(
                    info.hwndList,
                    LB_SETTOPINDEX,
                    (top - wheel / WHEEL_DELTA as i32 * 3).clamp(0, (count - 1).max(0)) as usize,
                    0,
                );
                return 0;
            }
            PostMessageW(
                root as HWND,
                SCROLL,
                ((wp >> 16) as u16 as i16 as i32 * -120 / WHEEL_DELTA as i32) as usize,
                GetParent(hwnd) as isize,
            );
            return 0;
        }
        if (class == "ListBox" || class == "LISTBOX") && GetPropW(hwnd, w!("OwnerCombo")).is_null()
        {
            if message == WM_LBUTTONDOWN {
                let item = send(hwnd, LB_ITEMFROMPOINT, 0, lp);
                if item >> 16 == 0 {
                    SetPropW(hwnd, w!("DragIndex"), (item + 1) as HANDLE);
                    SetCapture(hwnd);
                }
            } else if message == WM_LBUTTONUP {
                let from = RemovePropW(hwnd, w!("DragIndex")) as usize;
                ReleaseCapture();
                let to = send(hwnd, LB_ITEMFROMPOINT, 0, lp);
                if from > 0 && to >> 16 == 0 {
                    let row = (GetDlgCtrlID(hwnd) - BASE) / STRIDE;
                    PostMessageW(
                        root as HWND,
                        DROP_FONT,
                        row as usize,
                        ((from - 1) | ((to as usize) << 16)) as isize,
                    );
                }
            }
        }
        DefSubclassProc(hwnd, message, wp, lp)
    }
}
unsafe extern "system" fn pane_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let root = GetParent(hwnd);
        match message {
            WM_COMMAND => {
                if !UPDATING.with(Cell::get) {
                    PostMessageW(root, WM_COMMAND, wp, lp);
                }
                0
            }
            WM_DRAWITEM | WM_MEASUREITEM => SendMessageW(root, message, wp, lp),
            WM_VSCROLL => {
                PostMessageW(root, WM_VSCROLL, wp, hwnd as isize);
                0
            }
            WM_MOUSEWHEEL => {
                PostMessageW(
                    root,
                    SCROLL,
                    ((wp >> 16) as u16 as i16 as i32 * -120 / WHEEL_DELTA as i32) as usize,
                    hwnd as isize,
                );
                0
            }
            WM_CTLCOLORSTATIC => {
                let id = GetDlgCtrlID(lp as HWND);
                let hint = id >= BASE && (id - BASE) % STRIDE == 2;
                SetTextColor(
                    wp as HDC,
                    if hint {
                        crate::colors::secondary_text()
                    } else {
                        GetSysColor(COLOR_WINDOWTEXT)
                    },
                );
                SetBkMode(wp as HDC, TRANSPARENT as i32);
                GetSysColorBrush(COLOR_WINDOW) as isize
            }
            WM_ERASEBKGND => {
                let mut rect = RECT::default();
                GetClientRect(hwnd, &mut rect);
                FillRect(wp as HDC, &rect, GetSysColorBrush(COLOR_WINDOW));
                1
            }
            _ => DefWindowProcW(hwnd, message, wp, lp),
        }
    }
}
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match message {
        WM_SIZE => {
            unsafe {
                if GetPropW(hwnd, w!("ResizeQueued")).is_null() {
                    SetPropW(hwnd, w!("ResizeQueued"), 1usize as HANDLE);
                    PostMessageW(hwnd, RESIZE, 0, 0);
                }
            }
            0
        }
        RESIZE => {
            unsafe {
                RemovePropW(hwnd, w!("ResizeQueued"));
            }
            with_editor(|e| e.layout());
            0
        }
        WM_GETMINMAXINFO => {
            unsafe {
                let info = &mut *(lp as *mut MINMAXINFO);
                let dpi = GetDpiForWindow(hwnd).max(96);
                info.ptMinTrackSize = POINT {
                    x: (780 * dpi / 96) as i32,
                    y: (540 * dpi / 96) as i32,
                };
            }
            0
        }
        WM_DPICHANGED | WM_SETTINGCHANGE => {
            with_editor(|e| {
                e.save()?;
                e.dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as i32;
                e.font = Font::new("SystemFont", 0, e.dpi, false, false)?;
                e.hint_font = Font::new("SystemFont", 9, e.dpi, false, false)?;
                crate::settings_paint::reset_fonts();
                MEASUREMENTS.with_borrow_mut(HashMap::clear);
                for handle in e
                    .tabs
                    .iter()
                    .chain(e.axis_controls.iter())
                    .chain(std::iter::once(&e.status))
                {
                    send(*handle, WM_SETFONT, e.font.0 as usize, 1);
                }
                if message == WM_DPICHANGED {
                    let rect = unsafe { &*(lp as *const RECT) };
                    place(
                        hwnd,
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                    );
                }
                e.rebuild()
            });
            0
        }
        WM_COMMAND => {
            with_editor(|e| e.command((wp & 0xffff) as i32, (wp >> 16) as u32, lp as HWND));
            0
        }
        HOVER => {
            with_editor(|editor| {
                if wp == 0 || lp == -2 {
                    editor.tooltip.hide();
                } else {
                    editor.hover(wp as HWND, lp);
                }
                Ok(())
            });
            0
        }
        WM_CTLCOLORSTATIC => unsafe {
            SetBkMode(wp as HDC, TRANSPARENT as i32);
            SetTextColor(wp as HDC, GetSysColor(COLOR_WINDOWTEXT));
            // The label has no visible background, but its previous glyphs must be cleared.
            GetSysColorBrush(COLOR_WINDOW) as isize
        },
        WM_TIMER => {
            with_editor(|e| e.tick());
            0
        }
        WM_VSCROLL | SCROLL => {
            with_editor(|e| {
                let pane = usize::from(lp as HWND == e.panes[1]);
                e.scroll_by(
                    pane,
                    if message == SCROLL {
                        u16::MAX
                    } else {
                        (wp & 0xffff) as u16
                    },
                    wp as i32,
                )
            });
            0
        }
        DROP_FONT => {
            with_editor(|e| e.reorder_font(wp, lp as usize & 0xffff, (lp as usize >> 16) & 0xffff));
            0
        }
        WM_DRAWITEM => {
            crate::settings_paint::draw(hwnd, unsafe { &*(lp as *const DRAWITEMSTRUCT) });
            1
        }
        WM_MEASUREITEM => {
            unsafe {
                (*(lp as *mut MEASUREITEMSTRUCT)).itemHeight =
                    36 * GetDpiForWindow(hwnd).max(96) / 96;
            }
            1
        }
        WM_CLOSE => {
            with_editor(|e| {
                e.save()?;
                unsafe {
                    PostQuitMessage(0);
                }
                Ok(())
            });
            0
        }
        APP_EXIT => {
            if !UPDATING.with(Cell::get) {
                // Synchronous interprocess messages can overtake posted EN_CHANGE notices.
                // Drain those first so an immediate tray exit still saves the final keystroke.
                unsafe {
                    let mut command = MSG::default();
                    while PeekMessageW(&mut command, hwnd, WM_COMMAND, WM_COMMAND, PM_REMOVE) != 0 {
                        DispatchMessageW(&command);
                    }
                }
            }
            let mut accepted = false;
            let mut handled = false;
            with_editor(|editor| {
                handled = true;
                editor.save()?;
                editor.preview.hide();
                editor.overlay.hide_tip();
                editor.tooltip.hide();
                editor.overlay.guides(0, &editor.cfg)?;
                input::track_preview(false)?;
                input::set_guides(false)?;
                accepted = true;
                unsafe {
                    PostQuitMessage(0);
                }
                Ok(())
            });
            if !handled {
                // The native color chooser pumps messages while the editor is borrowed.
                // Cancel only that owned modal popup, then let the caller retry normally.
                unsafe {
                    let popup = GetLastActivePopup(hwnd);
                    if popup != hwnd
                        && GetWindow(popup, GW_OWNER) == hwnd
                        && IsWindowVisible(popup) != 0
                    {
                        PostMessageW(popup, WM_COMMAND, IDCANCEL as usize, 0);
                        return 2;
                    }
                }
            }
            isize::from(accepted)
        }
        WM_ACTIVATE => {
            // Activation can arrive inside a native modal dialog while Editor is borrowed.
            // Record the latest state immediately, then render outside that callback.
            unsafe {
                if (wp & 0xffff) as u32 == WA_INACTIVE {
                    SetPropW(hwnd, w!("PreviewSuspended"), 1usize as HANDLE);
                } else {
                    RemovePropW(hwnd, w!("PreviewSuspended"));
                }
                SetPropW(hwnd, w!("PreviewActivationPending"), 1usize as HANDLE);
                PostMessageW(hwnd, ACTIVATE_PREVIEW, 0, 0);
            }
            unsafe { DefWindowProcW(hwnd, message, wp, lp) }
        }
        ACTIVATE_PREVIEW => {
            with_editor(|editor| {
                unsafe { RemovePropW(hwnd, w!("PreviewActivationPending")) };
                editor.render()
            });
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wp, lp) },
    }
}
pub fn run(path: &Path) -> Result<(), String> {
    unsafe {
        let mutex = CreateMutexW(
            null(),
            0,
            w!("Local\\D5C9A59069D907909B153C76CEBDC023.Settings"),
        );
        if mutex.is_null() {
            return Err(err());
        }
        let status = GetLastError();
        let _mutex = MutexHandle(mutex);
        if status == ERROR_ALREADY_EXISTS {
            let hwnd = FindWindowW(CLASS, null());
            if !hwnd.is_null() {
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
            }
            return Ok(());
        }
        let loaded = config::load_snapshot(path, None)?;
        let original = loaded.text;
        let doc = loaded.document;
        let cfg = loaded.config;
        let screen = desktop::screen(desktop::cursor()?, 0)?;
        let dpi = screen.dpi;
        for (name, proc) in [
            (
                CLASS,
                Some(
                    window_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
                ),
            ),
            (
                PANE,
                Some(pane_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT),
            ),
        ] {
            let class = WNDCLASSW {
                lpfnWndProc: proc,
                hInstance: GetModuleHandleW(null()),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hIcon: LoadIconW(GetModuleHandleW(null()), crate::tray::APP_ICON),
                hbrBackground: GetSysColorBrush(COLOR_WINDOW),
                lpszClassName: name,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                return Err(err());
            }
        }
        let width = (1040 * dpi / 96).min(screen.work.width);
        let height = (780 * dpi / 96).min(screen.work.height);
        let hwnd = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            CLASS,
            w!("WinSizeP"),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            screen.work.x + (screen.work.width - width) / 2,
            screen.work.y + (screen.work.height - height) / 2,
            width,
            height,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        );
        if hwnd.is_null() {
            return Err(err());
        }
        SetPropW(hwnd, w!("PreviewSuspended"), 1usize as HANDLE);
        // Settings may retain an elevated token after the resident is restarted normally.
        // Only this payload-free lifecycle message is allowed across that boundary.
        desktop::check(ChangeWindowMessageFilterEx(
            hwnd,
            APP_EXIT,
            MSGFLT_ALLOW,
            null_mut(),
        ))?;
        let corners = DWMWCP_ROUND;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&corners as *const DWM_WINDOW_CORNER_PREFERENCE).cast(),
            size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
        let mut panes = [null_mut(); 2];
        for (index, pane) in panes.iter_mut().enumerate() {
            *pane = CreateWindowExW(
                WS_EX_CONTROLPARENT,
                PANE,
                null(),
                WS_CHILD | WS_VISIBLE | WS_VSCROLL | WS_CLIPCHILDREN,
                0,
                0,
                1,
                1,
                hwnd,
                (index + 1) as HMENU,
                GetModuleHandleW(null()),
                null(),
            );
            if pane.is_null() {
                return Err(err());
            }
        }
        let mut editor = Editor {
            window: Window(hwnd),
            panes,
            tabs: Vec::new(),
            status: null_mut(),
            status_until: Cell::new(None),
            axis_controls: [null_mut(); 3],
            path: path.into(),
            original,
            doc,
            clock: FrameClock::new(cfg.fps)?,
            cfg,
            tab: 0,
            axis: 0,
            rows: Vec::new(),
            pages: HashMap::new(),
            scroll: [0; 2],
            extent: [0; 2],
            dpi,
            font: Font::new("SystemFont", 0, dpi, false, false)?,
            hint_font: Font::new("SystemFont", 9, dpi, false, false)?,
            families: fonts::families(),
            live: false,
            selected: None,
            pending: None,
            dirty: HashSet::new(),
            disk_check: Instant::now(),
            overlay: Overlay::new()?,
            preview: Preview::new()?,
            tooltip: HoverTip::new(hwnd)?,
            active_combo: None,
        };
        for (index, (en, zh)) in TABS.iter().enumerate() {
            editor.tabs.push(editor.create(
                panes[0],
                w!("BUTTON"),
                editor.cfg.tr(en, zh),
                BS_OWNERDRAW as u32 | WS_TABSTOP,
                TAB_BASE + index as i32,
            )?);
        }
        editor.status = editor.create(hwnd, w!("STATIC"), "", SS_LEFT, 600)?;
        editor.axis_controls[0] = editor.create(
            panes[1],
            w!("BUTTON"),
            "",
            BS_AUTORADIOBUTTON as u32 | BS_MULTILINE as u32 | WS_TABSTOP,
            AXIS_H,
        )?;
        editor.axis_controls[1] = editor.create(
            panes[1],
            w!("BUTTON"),
            "",
            BS_AUTORADIOBUTTON as u32 | BS_MULTILINE as u32 | WS_TABSTOP,
            AXIS_V,
        )?;
        editor.axis_controls[2] = editor.create(
            panes[1],
            w!("BUTTON"),
            "",
            BS_AUTORADIOBUTTON as u32 | BS_MULTILINE as u32 | WS_TABSTOP,
            AXIS_BOTH,
        )?;
        if !cfg!(test) && !crate::resident::running() {
            crate::launch(editor.cfg.elevate)?;
        }
        let _hooks = input::Hooks::escape_only()?;
        let timer = editor.clock.handle();
        EDITOR.with_borrow_mut(|slot| *slot = Some(editor));
        with_editor(|e| e.rebuild());
        SetTimer(hwnd, 1, 100, None);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        let result = message_loop(hwnd, timer);
        KillTimer(hwnd, 1);
        EDITOR.with_borrow_mut(|slot| {
            slot.take();
        });
        result
    }
}
fn message_loop(hwnd: HWND, timer: HANDLE) -> Result<(), String> {
    unsafe {
        loop {
            match MsgWaitForMultipleObjectsEx(1, &timer, INFINITE, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
            {
                WAIT_OBJECT_0 => with_editor(|e| {
                    if let Some(point) = e.clock.tick() {
                        e.overlay.follow(point, &e.cfg)?;
                    }
                    Ok(())
                }),
                n if n == WAIT_OBJECT_0 + 1 => {
                    let mut msg = MSG::default();
                    for _ in 0..64 {
                        if PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) == 0 {
                            break;
                        }
                        if msg.message == WM_QUIT {
                            return Ok(());
                        }
                        if msg.hwnd.is_null() && msg.message == input::MOVE {
                            with_editor(|e| e.moved(input::moved()));
                        } else if msg.message == input::CANCEL {
                            with_editor(|e| e.escape());
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "helper process for the tray shutdown integration test"]
    fn exit_settings_host() {
        if let Some(path) = std::env::var_os("WINSIZEP_SETTINGS_TEST_CONFIG") {
            run(Path::new(&path)).unwrap();
        }
    }
    #[test]
    fn renamed_mapping_keeps_its_position() {
        let mut doc = "[keys]\nQ='LeftTop'\nG='Guide'\nH='Help'\n"
            .parse::<DocumentMut>()
            .unwrap();
        rename_entry(doc["keys"].as_table_mut().unwrap(), "G", "F12");
        assert_eq!(
            doc["keys"]
                .as_table()
                .unwrap()
                .iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>(),
            ["Q", "F12", "H"]
        );
    }
    #[test]
    fn sidebar_fits_labels_without_consuming_one_third() {
        assert_eq!(sidebar_size(1000, 96, 70), 160);
        assert_eq!(sidebar_size(1000, 96, 150), 206);
        assert_eq!(sidebar_size(1000, 96, 900), 240);
        assert_eq!(sidebar_size(2000, 192, 140), 320);
        assert!(sidebar_size(780, 96, 900) < 780 / 3);
    }

    fn snapshot(hwnd: HWND, file: &Path) {
        // Capture only the test window into an owned DIB, never the surrounding desktop.
        unsafe {
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect);
            let dc = GetDC(hwnd);
            let memory = CreateCompatibleDC(dc);
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: rect.right,
                    biHeight: -rect.bottom,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = null_mut();
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            assert!(!bitmap.is_null());
            let old = SelectObject(memory, bitmap);
            SendMessageW(
                hwnd,
                WM_PRINT,
                memory as usize,
                (PRF_CLIENT | PRF_CHILDREN | PRF_ERASEBKGND) as isize,
            );
            let count = (rect.right * rect.bottom * 4) as usize;
            let mut bytes = Vec::with_capacity(54 + count);
            bytes.extend_from_slice(b"BM");
            bytes.extend_from_slice(&((54 + count) as u32).to_le_bytes());
            bytes.extend_from_slice(&[0; 4]);
            bytes.extend_from_slice(&54u32.to_le_bytes());
            bytes.extend_from_slice(&40u32.to_le_bytes());
            bytes.extend_from_slice(&rect.right.to_le_bytes());
            bytes.extend_from_slice(&(-rect.bottom).to_le_bytes());
            bytes.extend_from_slice(&1u16.to_le_bytes());
            bytes.extend_from_slice(&32u16.to_le_bytes());
            bytes.extend_from_slice(&[0; 24]);
            bytes.extend_from_slice(std::slice::from_raw_parts(bits as *const u8, count));
            std::fs::write(file, bytes).unwrap();
            SelectObject(memory, old);
            DeleteObject(bitmap);
            DeleteDC(memory);
            ReleaseDC(hwnd, dc);
        }
    }

    #[test]
    #[ignore = "opens a real settings window; run explicitly on an interactive Windows desktop"]
    fn interactive_settings_smoke() {
        fn check_dropdown(root: HWND, combo: HWND) -> Result<(), String> {
            unsafe {
                let mut info = COMBOBOXINFO {
                    cbSize: size_of::<COMBOBOXINFO>() as u32,
                    ..Default::default()
                };
                GetComboBoxInfo(combo, &mut info);
                let mut window = RECT::default();
                let mut popup = RECT::default();
                GetWindowRect(root, &mut window);
                GetWindowRect(info.hwndList, &mut popup);
                let height = popup.bottom - popup.top;
                let row = send(combo, CB_GETITEMHEIGHT, 0, 0) as i32;
                if height > window.bottom - window.top || height > row * 8 + 8 || height <= row {
                    return Err(format!("Invalid dropdown height: {height}, row={row}"));
                }
                let count = send(combo, CB_GETCOUNT, 0, 0) as i32;
                if count * row > height
                    && GetWindowLongW(info.hwndList, GWL_STYLE) as u32 & WS_VSCROLL == 0
                {
                    return Err("Overflow dropdown has no scrollbar".into());
                }
                Ok(())
            }
        }
        fn wait_tab(hwnd: HWND, tab: usize) -> Result<(), String> {
            let until = Instant::now() + Duration::from_secs(5);
            while unsafe { GetPropW(hwnd, w!("ReadyTab")) } as usize != tab + 1 {
                if Instant::now() > until {
                    return Err(format!("Tab {tab} did not finish rendering"));
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            Ok(())
        }
        let folder = std::env::current_dir().unwrap().join("target/ui-smoke");
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("config.toml");
        let mut doc = config::help().parse::<DocumentMut>().unwrap();
        doc["general"]["language"] = value("zh-CN");
        let mut fallbacks = toml_edit::Array::new();
        fallbacks.push("Noto Color Emoji");
        fallbacks.push("Microsoft YaHei UI");
        doc["tip"]["font_fallbacks"] = value(fallbacks);
        doc["presets"]["这是一个超过八个汉字的测试分区名称"] = value("0|0|50sw|50sh");
        std::fs::write(&path, doc.to_string()).unwrap();
        assert!(
            unsafe { FindWindowW(CLASS, null()) }.is_null(),
            "Close existing settings before the smoke test"
        );
        let output = folder.clone();
        let controller = std::thread::spawn(move || -> Result<(), String> {
            let until = Instant::now() + Duration::from_secs(15);
            let hwnd = loop {
                let hwnd = unsafe { FindWindowW(CLASS, null()) };
                if !hwnd.is_null() {
                    break hwnd;
                }
                if Instant::now() > until {
                    return Err("Settings did not open".into());
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            let checks = (|| -> Result<(), String> {
                std::thread::sleep(Duration::from_millis(600));
                unsafe {
                    let mut window = RECT::default();
                    GetWindowRect(hwnd, &mut window);
                    SetWindowPos(
                        hwnd,
                        null_mut(),
                        window.left,
                        window.top,
                        980,
                        720,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                for tab in 0..7 {
                    let start = Instant::now();
                    unsafe {
                        PostMessageW(hwnd, WM_COMMAND, (TAB_BASE + tab) as usize, 0);
                    }
                    wait_tab(hwnd, tab as usize)?;
                    println!("tab {tab} first_load_ms={}", start.elapsed().as_millis());
                    std::thread::sleep(Duration::from_millis(100));
                    let status = caption(unsafe { GetDlgItem(hwnd, 600) });
                    if status.contains("error") || status.contains("错误") {
                        return Err(status);
                    }
                    let pane = unsafe { GetDlgItem(hwnd, 2) };
                    let mut child = unsafe { GetWindow(pane, GW_CHILD) };
                    while !child.is_null() {
                        if unsafe { IsWindowVisible(child) } != 0
                            && unsafe { IsWindowEnabled(child) } == 0
                        {
                            return Err(format!(
                                "Tab {tab} contains a permanently disabled control"
                            ));
                        }
                        child = unsafe { GetWindow(child, GW_HWNDNEXT) };
                    }
                    for (index, field) in
                        schema::fields(tab as usize, 0, &Config::parse(&doc).unwrap(), &doc)
                            .iter()
                            .enumerate()
                    {
                        if let Kind::Color(format) = field.kind {
                            let alpha =
                                unsafe { GetDlgItem(pane, BASE + index as i32 * STRIDE + 6) };
                            let alpha_label =
                                unsafe { GetDlgItem(pane, BASE + index as i32 * STRIDE + 10) };
                            if (format == ColorChannels::Rgb)
                                && (!alpha.is_null() || !alpha_label.is_null())
                            {
                                return Err("Opaque color exposes an alpha control".into());
                            }
                            if (format == ColorChannels::Rgba)
                                && (alpha.is_null() || alpha_label.is_null())
                            {
                                return Err("Editable alpha channel is missing".into());
                            }
                        }
                    }
                    snapshot(hwnd, &output.join(format!("tab-{tab}.bmp")));
                    if tab == 1 {
                        let pane = unsafe { GetDlgItem(hwnd, 2) };
                        unsafe {
                            PostMessageW(hwnd, SCROLL, 900, pane as isize);
                        }
                        std::thread::sleep(Duration::from_millis(200));
                        snapshot(hwnd, &output.join("fonts.bmp"));
                    }
                }
                for tab in [0, 1, 2, 3, 4, 5, 6] {
                    let start = Instant::now();
                    unsafe {
                        PostMessageW(hwnd, WM_COMMAND, (TAB_BASE + tab) as usize, 0);
                    }
                    wait_tab(hwnd, tab as usize)?;
                    println!("tab {tab} cached_ms={}", start.elapsed().as_millis());
                    if start.elapsed() > Duration::from_secs(1) {
                        return Err(format!("Cached tab {tab} took over one second"));
                    }
                }
                // Every preview pauses in the background and resumes with its checkbox intact.
                let overlay_count = || unsafe {
                    let mut count = 0;
                    let mut overlay = null_mut();
                    loop {
                        overlay =
                            FindWindowExW(null_mut(), overlay, w!("WinSizeP.Overlay"), null());
                        if overlay.is_null() {
                            break;
                        }
                        let mut process = 0;
                        GetWindowThreadProcessId(overlay, &mut process);
                        if process == GetCurrentProcessId() && IsWindowVisible(overlay) != 0 {
                            count += 1;
                        }
                    }
                    count
                };
                for tab in [1, 2, 3, 5, 6] {
                    send(hwnd, WM_COMMAND, (TAB_BASE + tab) as usize, 0);
                    wait_tab(hwnd, tab as usize)?;
                    let pane = unsafe { GetDlgItem(hwnd, 2) };
                    let fields =
                        schema::fields(tab as usize, 0, &Config::parse(&doc).unwrap(), &doc);
                    let preview = fields
                        .iter()
                        .position(|f| matches!(f.kind, Kind::Preview))
                        .map(|row| unsafe { GetDlgItem(pane, BASE + row as i32 * STRIDE) });
                    if let Some(control) = preview
                        && send(control, BM_GETCHECK, 0, 0) == 0
                    {
                        send(control, BM_CLICK, 0, 0);
                    }
                    if tab == 2 {
                        let row = fields.iter().position(|f| f.section == "presets").unwrap();
                        send(
                            unsafe { GetDlgItem(pane, BASE + row as i32 * STRIDE + 5) },
                            BM_CLICK,
                            0,
                            0,
                        );
                    }
                    send(hwnd, WM_ACTIVATE, WA_ACTIVE as usize, 0);
                    send(hwnd, ACTIVATE_PREVIEW, 0, 0);
                    if overlay_count() == 0 {
                        return Err(format!("Preview missing on tab {tab}"));
                    }
                    send(hwnd, WM_ACTIVATE, WA_INACTIVE as usize, 0);
                    send(hwnd, ACTIVATE_PREVIEW, 0, 0);
                    // A background rerender must also leave the preview hidden.
                    send(hwnd, ACTIVATE_PREVIEW, 0, 0);
                    if overlay_count() != 0 {
                        return Err(format!("Background preview visible on tab {tab}"));
                    }
                    if preview.is_some_and(|control| send(control, BM_GETCHECK, 0, 0) != 1) {
                        return Err(format!("Preview selection lost on tab {tab}"));
                    }
                    send(hwnd, WM_ACTIVATE, WA_ACTIVE as usize, 0);
                    send(hwnd, ACTIVATE_PREVIEW, 0, 0);
                    if overlay_count() == 0 {
                        return Err(format!("Preview did not resume on tab {tab}"));
                    }
                    if tab == 6 {
                        let original = desktop::cursor()?;
                        let screen = desktop::screen(original, 0)?.full;
                        for cell in [1, 9, 1] {
                            let point = POINT {
                                x: screen.x + screen.width * if cell == 1 { 1 } else { 5 } / 6,
                                y: screen.y + screen.height * if cell == 1 { 1 } else { 5 } / 6,
                            };
                            let started = Instant::now();
                            unsafe {
                                SetCursorPos(point.x, point.y);
                            }
                            // Observe the old focus being cleared before waiting for the new one.
                            // A fixed sleep can still sample the previous region on a busy UI thread.
                            while overlay_count() == 2 && started.elapsed() < Duration::from_secs(1)
                            {
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            if overlay_count() == 2 {
                                return Err(
                                    "Moving out of the focused region did not restore the grid"
                                        .into(),
                                );
                            }
                            while overlay_count() != 2 && started.elapsed() < Duration::from_secs(5)
                            {
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            let elapsed = started.elapsed();
                            println!("grid cell={cell} dwell_ms={}", elapsed.as_millis());
                            if elapsed < Duration::from_millis(1900)
                                || elapsed > Duration::from_millis(2300)
                            {
                                return Err(format!("Grid dwell took {elapsed:?} for cell {cell}"));
                            }
                        }
                        unsafe {
                            SetCursorPos(original.x, original.y);
                        }
                    }
                }
                // Change FPS through the native edit notification, then confirm persisted clamping.
                unsafe {
                    PostMessageW(hwnd, WM_COMMAND, (TAB_BASE + 5) as usize, 0);
                }
                std::thread::sleep(Duration::from_millis(250));
                let pane = unsafe { GetDlgItem(hwnd, 2) };
                let fps = unsafe { GetDlgItem(pane, BASE + STRIDE) };
                if fps.is_null() {
                    return Err("FPS control missing".into());
                }
                let resize_started = Instant::now();
                unsafe {
                    let mut rect = RECT::default();
                    GetWindowRect(hwnd, &mut rect);
                    for step in 0..16 {
                        SetWindowPos(
                            hwnd,
                            null_mut(),
                            rect.left,
                            rect.top,
                            980 + step * 6,
                            720 + step * 2,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                    SendMessageW(hwnd, RESIZE, 0, 0);
                }
                println!(
                    "resize_16_steps_ms={}",
                    resize_started.elapsed().as_millis()
                );
                if resize_started.elapsed() > Duration::from_secs(2) {
                    return Err("Resize sequence took over two seconds".into());
                }
                if unsafe { GetDlgItem(pane, BASE + STRIDE) } != fps {
                    return Err("Resize recreated controls".into());
                }
                unsafe {
                    let mut client = RECT::default();
                    GetClientRect(fps, &mut client);
                    let mut format = RECT::default();
                    send(fps, EM_GETRECT, 0, (&mut format as *mut RECT) as isize);
                    let dc = GetDC(fps);
                    let old = SelectObject(dc, send(fps, WM_GETFONT, 0, 0) as HFONT);
                    let mut metrics = TEXTMETRICW::default();
                    GetTextMetricsW(dc, &mut metrics);
                    SelectObject(dc, old);
                    ReleaseDC(fps, dc);
                    let top = format.top;
                    let bottom = client.bottom - top - metrics.tmHeight;
                    if (top - bottom).abs() > 2 {
                        return Err(format!(
                            "Edit text not vertically centered: top={top}, bottom={bottom}"
                        ));
                    }
                    if client.right > 100 {
                        return Err("Numeric edit is unnecessarily wide".into());
                    }
                }
                send(fps, EM_SETSEL, 0, -1);
                send(fps, EM_REPLACESEL, 1, w!("2000") as isize);
                std::thread::sleep(Duration::from_millis(800));
                let doc = config::read_settings(&output.join("config.toml"))?
                    .parse::<DocumentMut>()
                    .map_err(|e| e.to_string())?;
                if doc["crosshair"]["fps"].as_integer() != Some(1000) {
                    return Err(format!(
                        "FPS was not clamped and saved: {}",
                        caption(unsafe { GetDlgItem(hwnd, 600) })
                    ));
                }
                // Changing the same setting in the file while the edit debounce is pending wins.
                send(fps, EM_SETSEL, 0, -1);
                send(fps, EM_REPLACESEL, 1, w!("50") as isize);
                let mut external = doc.clone();
                external["crosshair"]["fps"] = value(75);
                std::fs::write(output.join("config.toml"), external.to_string())
                    .map_err(|e| e.to_string())?;
                std::thread::sleep(Duration::from_millis(800));
                let merged = config::read_settings(&output.join("config.toml"))?
                    .parse::<DocumentMut>()
                    .map_err(|e| e.to_string())?;
                if merged["crosshair"]["fps"].as_integer() != Some(75) {
                    return Err("External edit lost to GUI debounce".into());
                }
                if caption(unsafe { GetDlgItem(hwnd, 600) }) != "已保存" {
                    return Err("Saved status is missing".into());
                }
                std::thread::sleep(Duration::from_millis(3300));
                if !caption(unsafe { GetDlgItem(hwnd, 600) }).is_empty() {
                    return Err("Saved status did not expire".into());
                }
                if unsafe { IsWindowVisible(GetDlgItem(hwnd, 600)) } != 0 {
                    return Err("Empty status control is still visible".into());
                }
                // Long dropdown entries expose their full label without widening the control.
                unsafe {
                    PostMessageW(hwnd, WM_COMMAND, (TAB_BASE + 4) as usize, 0);
                }
                wait_tab(hwnd, 4)?;
                let pane = unsafe { GetDlgItem(hwnd, 2) };
                let row = schema::fields(4, 0, &Config::parse(&merged).unwrap(), &merged)
                    .iter()
                    .position(|field| field.section == "keys" && field.key == "L")
                    .ok_or("Guide mapping missing")?;
                let combo = unsafe { GetDlgItem(pane, BASE + row as i32 * STRIDE) };
                let mut combo_class = [0u16; 64];
                let class_len = unsafe { GetClassNameW(combo, combo_class.as_mut_ptr(), 64) };
                if !String::from_utf16_lossy(&combo_class[..class_len as usize])
                    .eq_ignore_ascii_case("combobox")
                {
                    snapshot(hwnd, &output.join("tooltip-failure.bmp"));
                    return Err(format!(
                        "Tooltip target is not a combo: row={row}, hwnd={combo:?}, class={}, status={}",
                        String::from_utf16_lossy(&combo_class[..class_len as usize]),
                        caption(unsafe { GetDlgItem(hwnd, 600) })
                    ));
                }
                // Scroll to the row before opening its popup.
                let mut position = RECT::default();
                unsafe {
                    GetWindowRect(combo, &mut position);
                    MapWindowPoints(null_mut(), pane, (&mut position as *mut RECT).cast(), 2);
                    PostMessageW(hwnd, SCROLL, (position.top - 120) as usize, pane as isize);
                }
                std::thread::sleep(Duration::from_millis(150));
                send(combo, CB_SHOWDROPDOWN, 1, 0);
                check_dropdown(hwnd, combo)?;
                let mut info = COMBOBOXINFO {
                    cbSize: size_of::<COMBOBOXINFO>() as u32,
                    ..Default::default()
                };
                unsafe {
                    GetComboBoxInfo(combo, &mut info);
                }
                let index = send(
                    combo,
                    CB_FINDSTRINGEXACT,
                    usize::MAX,
                    wide("这是一个超过八个汉字的测试分区名称").as_ptr() as isize,
                );
                if index < 0 {
                    return Err("Long action label missing".into());
                }
                send(info.hwndList, LB_SETTOPINDEX, index as usize, 0);
                std::thread::sleep(Duration::from_millis(100));
                let original_cursor = desktop::cursor()?;
                let mut item_rect = RECT::default();
                send(
                    info.hwndList,
                    LB_GETITEMRECT,
                    index as usize,
                    (&mut item_rect as *mut RECT) as isize,
                );
                let mut hover_point = POINT {
                    x: item_rect.left + 10,
                    y: (item_rect.top + item_rect.bottom) / 2,
                };
                unsafe {
                    ClientToScreen(info.hwndList, &mut hover_point);
                    SetCursorPos(hover_point.x, hover_point.y);
                }
                std::thread::sleep(Duration::from_millis(650));
                let tooltip = unsafe { GetWindow(hwnd, GW_HWNDFIRST) };
                let mut current = tooltip;
                let mut visible = false;
                while !current.is_null() {
                    let mut class = [0u16; 64];
                    let len = unsafe { GetClassNameW(current, class.as_mut_ptr(), 64) };
                    if String::from_utf16_lossy(&class[..len as usize]) == "tooltips_class32"
                        && unsafe { GetWindow(current, GW_OWNER) } == hwnd
                        && unsafe { IsWindowVisible(current) } != 0
                    {
                        visible = true;
                        break;
                    }
                    current = unsafe { GetWindow(current, GW_HWNDNEXT) };
                }
                send(combo, CB_SHOWDROPDOWN, 0, 0);
                unsafe {
                    SetCursorPos(original_cursor.x, original_cursor.y);
                }
                if !visible {
                    snapshot(hwnd, &output.join("tooltip-failure.bmp"));
                    return Err(format!(
                        "Overflow tooltip was not displayed (list={:?}, item={index}, rect={},{},{},{}, owner={:?})",
                        info.hwndList,
                        item_rect.left,
                        item_rect.top,
                        item_rect.right,
                        item_rect.bottom,
                        unsafe { GetPropW(info.hwndList, w!("OwnerCombo")) }
                    ));
                }
                unsafe {
                    PostMessageW(hwnd, WM_COMMAND, (TAB_BASE + 1) as usize, 0);
                }
                wait_tab(hwnd, 1)?;
                let font_row = schema::fields(1, 0, &Config::parse(&merged).unwrap(), &merged)
                    .iter()
                    .position(|field| field.key == "font")
                    .ok_or("Font field missing")?;
                let font_combo = unsafe { GetDlgItem(pane, BASE + font_row as i32 * STRIDE) };
                let mut font_pos = RECT::default();
                unsafe {
                    GetWindowRect(font_combo, &mut font_pos);
                    MapWindowPoints(null_mut(), pane, (&mut font_pos as *mut RECT).cast(), 2);
                    PostMessageW(hwnd, SCROLL, (font_pos.top - 120) as usize, pane as isize);
                }
                std::thread::sleep(Duration::from_millis(150));
                send(font_combo, CB_SHOWDROPDOWN, 1, 0);
                std::thread::sleep(Duration::from_millis(100));
                check_dropdown(hwnd, font_combo)?;
                let mut font_info = COMBOBOXINFO {
                    cbSize: size_of::<COMBOBOXINFO>() as u32,
                    ..Default::default()
                };
                unsafe {
                    GetComboBoxInfo(font_combo, &mut font_info);
                }
                let top = send(font_info.hwndList, LB_GETTOPINDEX, 0, 0);
                send(
                    font_combo,
                    WM_MOUSEWHEEL,
                    (-120i16 as u16 as usize) << 16,
                    0,
                );
                if send(font_info.hwndList, LB_GETTOPINDEX, 0, 0) <= top {
                    return Err("Open font combo did not scroll".into());
                }
                send(font_combo, CB_SHOWDROPDOWN, 0, 0);
                unsafe {
                    PostMessageW(hwnd, WM_COMMAND, (TAB_BASE + 4) as usize, 0);
                }
                wait_tab(hwnd, 4)?;
                // Repeated wheel input at the boundary must not relayout/recreate controls.
                unsafe {
                    PostMessageW(hwnd, SCROLL, 100000, pane as isize);
                }
                std::thread::sleep(Duration::from_millis(150));
                let mut before = RECT::default();
                unsafe {
                    GetWindowRect(combo, &mut before);
                }
                for _ in 0..12 {
                    unsafe {
                        PostMessageW(hwnd, SCROLL, 120, pane as isize);
                    }
                }
                std::thread::sleep(Duration::from_millis(150));
                let mut after = RECT::default();
                unsafe {
                    GetWindowRect(combo, &mut after);
                }
                if before.left != after.left || before.top != after.top {
                    return Err("Scrolling beyond the end moved controls".into());
                }
                // Rename a mapping without replacing its editor or changing row order.
                let name = unsafe { GetDlgItem(pane, BASE + row as i32 * STRIDE + 3) };
                send(name, EM_SETSEL, 0, -1);
                send(name, EM_REPLACESEL, 1, w!("F12") as isize);
                std::thread::sleep(Duration::from_millis(800));
                if unsafe { GetDlgItem(pane, BASE + row as i32 * STRIDE) } != combo {
                    return Err("Renaming a mapping recreated its control".into());
                }
                // Shared style changes affect both axes, while FPS remains one shared field.
                unsafe {
                    PostMessageW(hwnd, WM_COMMAND, (TAB_BASE + 5) as usize, 0);
                }
                wait_tab(hwnd, 5)?;
                unsafe {
                    PostMessageW(hwnd, WM_COMMAND, AXIS_BOTH as usize, 0);
                }
                std::thread::sleep(Duration::from_millis(200));
                let pane = unsafe { GetDlgItem(hwnd, 2) };
                let width = unsafe { GetDlgItem(pane, BASE + 2 * STRIDE) };
                send(width, EM_SETSEL, 0, -1);
                send(width, EM_REPLACESEL, 1, w!("3") as isize);
                std::thread::sleep(Duration::from_millis(800));
                let saved = config::read_settings(&output.join("config.toml"))?
                    .parse::<DocumentMut>()
                    .map_err(|e| e.to_string())?;
                if saved["horizontal"]["width"].as_integer() != Some(3)
                    || saved["vertical"]["width"].as_integer() != Some(3)
                {
                    return Err("Unified axis style did not update both lines".into());
                }
                Ok(())
            })();
            unsafe {
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }
            checks
        });
        let result = run(&path);
        result.unwrap();
        controller.join().unwrap().unwrap();
    }
}
