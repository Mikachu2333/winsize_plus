use crate::{
    config::{self, Config},
    desktop::{self, err, wide},
    fonts::{self, Font},
    framerate::FrameClock,
    input,
    overlay::Overlay,
    preview::Preview,
    settings_schema::{self as schema, Field, Kind, TABS},
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
const TAB_BASE: i32 = 100;
const AXIS_H: i32 = 500;
const AXIS_V: i32 = 501;
const SS_LEFT: u32 = 0;
const SS_CENTER: u32 = 1;
thread_local! { static EDITOR: RefCell<Option<Editor>> = const { RefCell::new(None) }; }
thread_local! { static UPDATING: Cell<bool> = const { Cell::new(false) }; }
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
    axis_controls: [HWND; 2],
    path: PathBuf,
    original: String,
    doc: DocumentMut,
    cfg: Config,
    tab: usize,
    axis: usize,
    rows: Vec<Row>,
    scroll: [i32; 2],
    extent: [i32; 2],
    dpi: i32,
    font: Font,
    hint_font: Font,
    families: Vec<String>,
    family_fonts: HashMap<usize, Font>,
    live: bool,
    selected: Option<String>,
    pending: Option<Instant>,
    dirty: HashSet<usize>,
    disk_check: Instant,
    overlay: Overlay,
    preview: Preview,
    clock: FrameClock,
    tooltip: HoverTip,
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
            SetWindowPos(
                hwnd,
                null_mut(),
                x,
                y,
                width.max(1),
                height.max(1),
                SWP_NOACTIVATE | SWP_NOZORDER,
            );
        }
    }
}
fn measured(text: &str, font: &Font, width: i32) -> i32 {
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
        rect.bottom.max(font.height())
    }
}
impl Editor {
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
            InvalidateRect(self.window.0, null(), 1);
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
        let style = if is_edit {
            style | ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32
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
        self.tooltip.hide();
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
                } else {
                    self.cfg.tr("Vertical line", "竖线")
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
                Kind::Color => {
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
                        if channel == 3 && alpha.is_none() {
                            unsafe {
                                EnableWindow(row.c(6), 0);
                            }
                        }
                    }
                }
                Kind::Fallbacks => {
                    self.control(
                        &mut row,
                        index,
                        0,
                        w!("LISTBOX"),
                        "",
                        LBS_NOTIFY as u32 | WS_BORDER | WS_VSCROLL | WS_TABSTOP,
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
        self.render()
    }
    fn layout(&mut self) -> Result<(), String> {
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
        let number_width = text_width("0000", &self.font) + self.s(16);
        let button_width = self.s(68);
        let name_width = combo_width;
        let expression_width = (combo_width * 2).min(width);
        let mut y = padding;
        if self.tab == 5 {
            for (i, control) in self.axis_controls.iter().enumerate() {
                place(
                    *control,
                    padding + i as i32 * combo_width,
                    y - self.scroll[1],
                    combo_width,
                    h,
                );
            }
            y += h + self.s(10);
        }
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
                    place(row.c(0), padding, y - self.scroll[1], combo_width, h * 10);
                    send(
                        row.c(0),
                        CB_SETITEMHEIGHT,
                        usize::MAX,
                        (h - self.s(4)) as isize,
                    );
                    send(row.c(0), CB_SETDROPPEDWIDTH, combo_width as usize, 0);
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
                Kind::Color => {
                    let color_width = (combo_width + self.s(80)).min(width);
                    place(row.c(0), padding, y - self.scroll[1], color_width, h);
                    y += h + gap;
                    let unit = number_width + self.s(28);
                    let columns = (width / unit).clamp(1, 4) as usize;
                    for channel in 0..4 {
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
                    y += 4usize.div_ceil(columns) as i32 * (h + gap) - gap;
                }
                Kind::Fallbacks => {
                    let list_width = (combo_width * 2).min(width);
                    place(row.c(0), padding, y - self.scroll[1], list_width, h * 3);
                    y += h * 3 + gap;
                    place(row.c(3), padding, y - self.scroll[1], combo_width, h * 10);
                    send(
                        row.c(3),
                        CB_SETITEMHEIGHT,
                        usize::MAX,
                        (h - self.s(4)) as isize,
                    );
                    send(row.c(3), CB_SETDROPPEDWIDTH, combo_width as usize, 0);
                    place(
                        row.c(4),
                        padding + combo_width + gap,
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
                InvalidateRect(self.panes[pane], null(), 1);
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
        self.layout()
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
                Kind::Color => {
                    let channels: Vec<u8> = (3..=6)
                        .map(|slot| {
                            caption(row.c(slot)).parse::<u8>().map_err(|_| {
                                self.cfg
                                    .tr("RGBA must be 0–255", "RGBA 必须为 0–255")
                                    .to_owned()
                            })
                        })
                        .collect::<Result<_, _>>()?;
                    if draft[section].get("opacity").is_some() {
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
                        draft[section].as_table_mut().unwrap().remove(key);
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
            self.rows.get(*i).is_some_and(|r| {
                (matches!(r.field.kind, Kind::Mapping) && caption(r.c(3)).trim() != r.field.key)
                    || (r.field.section == "general" && r.field.key == "language")
            })
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
        self.dirty.clear();
        self.clock.reset(self.cfg.fps);
        self.status(self.cfg.tr("Saved", "已保存"));
        if rebuild {
            self.rebuild()?;
        } else {
            self.update_values();
            self.render()?;
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
                Kind::Color => {
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
        let guides = if self.tab == 5 && self.live {
            if self.axis == 0 { 1 } else { 2 }
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
        self.rebuild()
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
            unsafe {
                let mut info = COMBOBOXINFO {
                    cbSize: size_of::<COMBOBOXINFO>() as u32,
                    ..Default::default()
                };
                if GetComboBoxInfo(sender, &mut info) != 0 {
                    SetPropW(info.hwndList, w!("OwnerCombo"), sender);
                    SetWindowSubclass(info.hwndList, Some(control_proc), 1, self.window.0 as usize);
                }
            }
        }
        if event == CBN_CLOSEUP {
            self.tooltip.hide();
        }
        if (TAB_BASE..TAB_BASE + 7).contains(&id) && event == BN_CLICKED {
            self.save()?;
            self.tab = (id - TAB_BASE) as usize;
            self.scroll[1] = 0;
            self.selected = None;
            return self.rebuild();
        }
        if matches!(id, AXIS_H | AXIS_V) && event == BN_CLICKED {
            self.save()?;
            self.axis = usize::from(id == AXIS_V);
            return self.rebuild();
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
            Kind::Color if slot == 0 && event == BN_CLICKED => return self.color_dialog(index),
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
                self.render()?;
                if matches!(kind, Kind::Color) {
                    unsafe {
                        InvalidateRect(self.rows[index].c(0), null(), 1);
                    }
                }
            }
        }
        Ok(())
    }
    fn tick(&mut self) -> Result<(), String> {
        let now = Instant::now();
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
        if self.preview.deadline().is_some_and(|when| now >= when) {
            self.preview.tick(&self.cfg)?;
        }
        if self.dirty.is_empty() && now.duration_since(self.disk_check) >= Duration::from_secs(1) {
            self.disk_check = now;
            let disk = config::read_settings(&self.path)?;
            if disk != self.original {
                config::load(&self.path, None)?;
                self.original = config::read_settings(&self.path)?;
                self.doc = self
                    .original
                    .parse::<DocumentMut>()
                    .map_err(|e| e.to_string())?;
                self.cfg = Config::parse(&self.doc)?;
                self.status(
                    self.cfg
                        .tr("Manual file changes applied", "已应用配置文件中的修改"),
                );
                self.rebuild()?;
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
        let selected = if item < 0 {
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
        let font = if matches!(row.field.kind, Kind::Font | Kind::Fallbacks) {
            self.family_fonts
                .get(&(selected as usize))
                .unwrap_or(&self.font)
        } else {
            &self.font
        };
        let overflow = text_width(label, font) > rect.right - self.s(30);
        if let Ok(point) = desktop::cursor() {
            self.tooltip.hover(overflow.then(|| label.clone()), point);
        }
    }
    fn escape(&mut self) -> Result<(), String> {
        self.overlay.hide_tip();
        if !self.preview.reset(&self.cfg)? {
            self.preview.hide();
            input::track_preview(false)?;
        }
        Ok(())
    }
    fn draw(&mut self, draw: &DRAWITEMSTRUCT) {
        unsafe {
            let id = draw.CtlID as i32;
            let dc = draw.hDC;
            let mut rect = draw.rcItem;
            if (TAB_BASE..TAB_BASE + 7).contains(&id) {
                let selected = id == TAB_BASE + self.tab as i32;
                SetDCBrushColor(dc, GetSysColor(COLOR_WINDOW));
                FillRect(dc, &rect, GetStockObject(DC_BRUSH));
                SetDCBrushColor(
                    dc,
                    GetSysColor(if selected {
                        COLOR_HIGHLIGHT
                    } else {
                        COLOR_BTNFACE
                    }),
                );
                let old_brush = SelectObject(dc, GetStockObject(DC_BRUSH));
                let old_pen = SelectObject(dc, GetStockObject(NULL_PEN));
                RoundRect(
                    dc,
                    rect.left,
                    rect.top,
                    rect.right,
                    rect.bottom,
                    self.s(12),
                    self.s(12),
                );
                SelectObject(dc, old_brush);
                SelectObject(dc, old_pen);
                let old = SelectObject(dc, self.font.0);
                SetBkMode(dc, TRANSPARENT as i32);
                SetTextColor(
                    dc,
                    GetSysColor(if selected {
                        COLOR_HIGHLIGHTTEXT
                    } else {
                        COLOR_BTNTEXT
                    }),
                );
                let text = wide(self.cfg.tr(
                    TABS[(id - TAB_BASE) as usize].0,
                    TABS[(id - TAB_BASE) as usize].1,
                ));
                let height = measured(
                    &String::from_utf16_lossy(&text[..text.len() - 1]),
                    &self.font,
                    rect.right - rect.left - self.s(32),
                );
                rect.left += self.s(16);
                rect.right -= self.s(16);
                rect.top += ((rect.bottom - rect.top - height) / 2).max(0);
                DrawTextW(
                    dc,
                    text.as_ptr(),
                    (text.len() - 1) as i32,
                    &mut rect,
                    DT_LEFT | DT_WORDBREAK | DT_NOPREFIX,
                );
                SelectObject(dc, old);
                if draw.itemState & ODS_FOCUS != 0 {
                    DrawFocusRect(dc, &draw.rcItem);
                }
                return;
            }
            if id < BASE {
                return;
            }
            let index = ((id - BASE) / STRIDE) as usize;
            let Some(row) = self.rows.get(index) else {
                return;
            };
            if matches!(row.field.kind, Kind::Color) {
                let channels: Vec<_> = (3..=6)
                    .map(|slot| caption(row.c(slot)).parse::<u32>().unwrap_or(255).min(255))
                    .collect();
                let tile = self.s(10).max(1);
                for y in (rect.top..rect.bottom).step_by(tile as usize) {
                    for x in (rect.left..rect.right).step_by(tile as usize) {
                        let base = if (x / tile + y / tile) % 2 == 0 {
                            230
                        } else {
                            180
                        };
                        let mix = |c: u32| (c * channels[3] + base * (255 - channels[3])) / 255;
                        SetDCBrushColor(
                            dc,
                            mix(channels[0]) | (mix(channels[1]) << 8) | (mix(channels[2]) << 16),
                        );
                        FillRect(
                            dc,
                            &RECT {
                                left: x,
                                top: y,
                                right: (x + tile).min(rect.right),
                                bottom: (y + tile).min(rect.bottom),
                            },
                            GetStockObject(DC_BRUSH),
                        );
                    }
                }
                let text = wide(format!(
                    "#{:02X}{:02X}{:02X}   A: {}   {}",
                    channels[0],
                    channels[1],
                    channels[2],
                    channels[3],
                    self.cfg.tr("Choose…", "选择…")
                ));
                let old = SelectObject(dc, self.font.0);
                SetBkMode(dc, TRANSPARENT as i32);
                SetTextColor(
                    dc,
                    if channels[3] > 180 && channels[..3].iter().sum::<u32>() < 300 {
                        0xffffff
                    } else {
                        0
                    },
                );
                DrawTextW(
                    dc,
                    text.as_ptr(),
                    (text.len() - 1) as i32,
                    &mut rect,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                );
                SelectObject(dc, old);
                return;
            }
            if !matches!(row.field.kind, Kind::Font | Kind::Fallbacks) || draw.itemID == u32::MAX {
                return;
            }
            let item = draw.itemID as usize;
            let Some((family, label)) = row.options.get(item) else {
                return;
            };
            if !self.family_fonts.contains_key(&item)
                && let Ok(font) = Font::new(family, 11, self.dpi, false, false)
            {
                self.family_fonts.insert(item, font);
            }
            let font = self.family_fonts.get(&item).unwrap_or(&self.font);
            let selected = draw.itemState & ODS_SELECTED != 0;
            SetDCBrushColor(
                dc,
                GetSysColor(if selected {
                    COLOR_HIGHLIGHT
                } else {
                    COLOR_WINDOW
                }),
            );
            FillRect(dc, &rect, GetStockObject(DC_BRUSH));
            let old = SelectObject(dc, font.0);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(
                dc,
                GetSysColor(if selected {
                    COLOR_HIGHLIGHTTEXT
                } else {
                    COLOR_WINDOWTEXT
                }),
            );
            rect.left += self.s(8);
            let text = wide(label);
            DrawTextW(
                dc,
                text.as_ptr(),
                (text.len() - 1) as i32,
                &mut rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            SelectObject(dc, old);
        }
    }
}
fn populate(hwnd: HWND, options: &[(String, String)], selected: &str, height: i32) {
    for (i, (value, label)) in options.iter().enumerate() {
        send(hwnd, CB_ADDSTRING, 0, wide(label).as_ptr() as isize);
        if value.eq_ignore_ascii_case(selected) {
            send(hwnd, CB_SETCURSEL, i, 0);
        }
    }
    send(hwnd, CB_SETITEMHEIGHT, usize::MAX, height as isize);
    send(hwnd, CB_SETITEMHEIGHT, 0, height as isize);
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
unsafe extern "system" fn control_proc(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    root: usize,
) -> LRESULT {
    unsafe {
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
            rect.bottom -= padding;
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
        if message == WM_MOUSEWHEEL
            && !(class == "ComboBox" && send(hwnd, CB_GETDROPPEDSTATE, 0, 0) != 0)
        {
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
                        0x707070
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
                PostMessageW(hwnd, RESIZE, 0, 0);
            }
            0
        }
        RESIZE => {
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
                e.family_fonts.clear();
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
            GetStockObject(NULL_BRUSH) as isize
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
            with_editor(|e| {
                e.draw(unsafe { &*(lp as *const DRAWITEMSTRUCT) });
                Ok(())
            });
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
        config::load(path, None)?;
        let original = config::read_settings(path)?;
        let doc = original.parse::<DocumentMut>().map_err(|e| e.to_string())?;
        let cfg = Config::parse(&doc)?;
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
            axis_controls: [null_mut(); 2],
            path: path.into(),
            original,
            doc,
            clock: FrameClock::new(cfg.fps)?,
            cfg,
            tab: 0,
            axis: 0,
            rows: Vec::new(),
            scroll: [0; 2],
            extent: [0; 2],
            dpi,
            font: Font::new("SystemFont", 0, dpi, false, false)?,
            hint_font: Font::new("SystemFont", 9, dpi, false, false)?,
            families: fonts::families(),
            family_fonts: HashMap::new(),
            live: false,
            selected: None,
            pending: None,
            dirty: HashSet::new(),
            disk_check: Instant::now(),
            overlay: Overlay::new()?,
            preview: Preview::new()?,
            tooltip: HoverTip::new(hwnd)?,
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
        let folder = std::env::current_dir().unwrap().join("target/ui-smoke");
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("config.toml");
        let mut doc = config::help().parse::<DocumentMut>().unwrap();
        doc["general"]["language"] = value("zh-CN");
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
                    unsafe {
                        PostMessageW(hwnd, WM_COMMAND, (TAB_BASE + tab) as usize, 0);
                    }
                    std::thread::sleep(Duration::from_millis(350));
                    let status = caption(unsafe { GetDlgItem(hwnd, 600) });
                    if status.contains("error") || status.contains("错误") {
                        return Err(status);
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
                unsafe {
                    let mut client = RECT::default(); GetClientRect(fps, &mut client);
                    let mut format = RECT::default(); send(fps, EM_GETRECT, 0, (&mut format as *mut RECT) as isize);
                    let dc = GetDC(fps); let old = SelectObject(dc, send(fps, WM_GETFONT, 0, 0) as HFONT);
                    let mut metrics = TEXTMETRICW::default(); GetTextMetricsW(dc, &mut metrics); SelectObject(dc, old); ReleaseDC(fps, dc);
                    let top = format.top; let bottom = client.bottom - top - metrics.tmHeight;
                    if (top - bottom).abs() > 2 { return Err(format!("Edit text not vertically centered: top={top}, bottom={bottom}")); }
                    if client.right > 100 { return Err("Numeric edit is unnecessarily wide".into()); }
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
                if caption(unsafe { GetDlgItem(hwnd, 600) }) != "已保存" { return Err("Saved status is missing".into()); }
                std::thread::sleep(Duration::from_millis(3300));
                if !caption(unsafe { GetDlgItem(hwnd, 600) }).is_empty() { return Err("Saved status did not expire".into()); }
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
