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
const CLASS: *const u16 = crate::identity::SETTINGS_CLASS;
const PANE: *const u16 = w!("WinSizeP.Settings.Pane");
const BASE: i32 = 2000;
const STRIDE: i32 = 16;
const SCROLL: u32 = WM_APP + 80;
const DROP_FONT: u32 = WM_APP + 81;
const RESIZE: u32 = WM_APP + 82;
const HOVER: u32 = WM_APP + 83;
const EDIT_LAYOUT: u32 = WM_APP + 84;
const APP_EXIT: u32 = WM_APP + 85;
/// Private WM_COMMAND notification: Enter was pressed in an edit field.
const EDIT_COMMIT: u32 = 0x7f00;
const ACTIVATE_PREVIEW: u32 = WM_APP + 86;
const SEARCH_KEY: u32 = WM_APP + 87;
const SEARCH_FOCUS: u32 = WM_APP + 88;
const SEARCH_EDIT: i32 = 700;
const SEARCH_RESULTS: i32 = 701;
const SEARCH_CLEAR: i32 = 702;
const SEARCH_FRAME: i32 = 703;
const SEARCH_TIMER: usize = 3;
#[cfg(test)]
const PREVIEW_STATE: u32 = WM_APP + 99;
#[cfg(test)]
const TEST_THEME: u32 = WM_APP + 98;
const TAB_BASE: i32 = 100;
const AXIS_H: i32 = 500;
const AXIS_V: i32 = 501;
const AXIS_BOTH: i32 = 502;
const AXIS_CUSTOM: i32 = 503;
const SS_LEFT: u32 = 0;
const SS_CENTER: u32 = 1;
const SS_OWNERDRAW: u32 = 13;
const SS_NOTIFY: u32 = 0x0100;
// Keep the gap between save notices independent of user configuration.
const SAVE_STATUS_DELAY_MS: u32 = 100;
const SAVE_STATUS_TIMER: usize = 2;
const ERROR_STATUS_DURATION: Duration = Duration::from_secs(10);
thread_local! { static EDITOR: RefCell<Option<Editor>> = const { RefCell::new(None) }; }
thread_local! { static UPDATING: Cell<bool> = const { Cell::new(false) }; }
thread_local! {
    static DISPLAY_UPDATE: Cell<bool> = const { Cell::new(false) };
    static DPI_RECT: Cell<Option<RECT>> = const { Cell::new(None) };
}
thread_local! { static MEASUREMENTS: RefCell<HashMap<(usize,i32,String),i32>> = RefCell::new(HashMap::new()); }
pub fn close_for_restart() -> Result<bool, String> {
    let window = unsafe { FindWindowW(CLASS, null()) };
    if window.is_null() {
        return Ok(false);
    }
    if !close_for_exit()? {
        return Err("Settings did not respond / 设置窗口没有响应".into());
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { IsWindow(window) } != 0 {
        if Instant::now() >= deadline {
            return Err("Settings did not close before restart / 设置窗口未能在重启前关闭".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(true)
}
pub fn open() -> Result<(), String> {
    unsafe {
        let existing = FindWindowW(CLASS, null());
        if !existing.is_null() {
            ShowWindowAsync(existing, SW_RESTORE);
            SetForegroundWindow(existing);
            PostMessageW(existing, SEARCH_FOCUS, 0, 0);
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
/// `Ok(false)` means the window did not answer at all (hung, or protected by a higher
/// integrity level): the caller decides whether it may go on without it.
pub fn close_for_exit() -> Result<bool, String> {
    unsafe {
        let hwnd = FindWindowW(CLASS, null());
        if hwnd.is_null() {
            return Ok(true);
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
                return Ok(IsWindow(hwnd) == 0);
            }
            if result == 1 {
                return Ok(true);
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
    search: Search,
    cast: crate::cast::Visuals,
    cast_next: Option<Instant>,
    window: Window,
    panes: [HWND; 2],
    tabs: Vec<HWND>,
    status: HWND,
    status_until: Cell<Option<Instant>>,
    layout_size: Option<(i32, i32)>,
    pending_status: RefCell<Option<(Instant, String, bool)>>,
    save_error: Cell<bool>,
    last_error: RefCell<Option<String>>,
    axis_controls: [HWND; 4],
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
#[derive(Default)]
struct Search {
    frame: HWND,
    edit: HWND,
    list: HWND,
    clear: HWND,
    index: crate::settings_search::Index,
    matches: Vec<usize>,
    selected: usize,
    query: String,
    stale: bool,
    visible: bool,
    dismissed: bool,
    composing: bool,
    highlight: Option<crate::settings_search::Target>,
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
    unsafe {
        if msg == CB_SETITEMHEIGHT && SendMessageW(hwnd, CB_GETITEMHEIGHT, wp, 0) == lp {
            return 0;
        }
        if msg == CB_SETDROPPEDWIDTH && SendMessageW(hwnd, CB_GETDROPPEDWIDTH, 0, 0) == wp as isize
        {
            return wp as isize;
        }
        SendMessageW(hwnd, msg, wp, lp)
    }
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
            // A combo reports its collapsed height, whereas SetWindowPos receives the
            // dropdown height. Remember that request to avoid resizing it on every tick.
            let combo = !GetPropW(hwnd, w!("SettingsCombo")).is_null();
            let same_height = if combo {
                GetPropW(hwnd, w!("RequestedHeight")) as usize == height.max(1) as usize
            } else {
                old.bottom - old.top == height.max(1)
            };
            if (old.left, old.top, old.right - old.left) == (x, y, width.max(1)) && same_height {
                return;
            }
            if combo {
                SetPropW(
                    hwnd,
                    w!("RequestedHeight"),
                    height.max(1) as usize as HANDLE,
                );
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
            // Resizing can change line wrapping and the edit formatting rectangle.
            // Repaint the whole child instead of retaining pixels from its old width.
            RedrawWindow(
                hwnd,
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_ERASE | RDW_FRAME | RDW_ALLCHILDREN,
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
    fn apply_config(&mut self, cfg: Config) -> Result<(), String> {
        let font_changed = self.cfg.font != cfg.font
            || self.cfg.font_size != cfg.font_size
            || self.cfg.font_bold != cfg.font_bold
            || self.cfg.font_italic != cfg.font_italic;
        if self.cfg != cfg {
            self.search.stale = true;
            if unsafe { GetFocus() } == self.search.edit && !self.search.query.is_empty() {
                unsafe {
                    SetTimer(self.window.0, SEARCH_TIMER, 150, None);
                }
            }
        }
        if self.cfg.fps != cfg.fps {
            self.clock.reset(cfg.fps);
        }
        self.cfg = cfg;
        if font_changed {
            self.refresh_display()?;
        }
        Ok(())
    }
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
            for hwnd in &self.axis_controls {
                unsafe {
                    ShowWindow(*hwnd, if tab == 5 { SW_SHOW } else { SW_HIDE });
                }
            }
            self.update_axis_controls();
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
    fn init_search(&mut self) -> Result<(), String> {
        self.search.frame = self.create(
            self.window.0,
            "STATIC",
            "",
            SS_OWNERDRAW | SS_NOTIFY | WS_CLIPSIBLINGS,
            SEARCH_FRAME,
        )?;
        self.search.edit = self.create(
            self.window.0,
            "EDIT",
            "",
            WS_TABSTOP | ES_AUTOHSCROLL as u32,
            SEARCH_EDIT,
        )?;
        self.search.list = self.create(
            self.window.0,
            "LISTBOX",
            "",
            WS_BORDER
                | WS_VSCROLL
                | WS_TABSTOP
                | LBS_NOTIFY as u32
                | LBS_OWNERDRAWFIXED as u32
                | LBS_HASSTRINGS as u32
                | LBS_NOINTEGRALHEIGHT as u32,
            SEARCH_RESULTS,
        )?;
        self.search.clear = self.create(
            self.window.0,
            "BUTTON",
            "×",
            BS_PUSHBUTTON as u32 | WS_TABSTOP,
            SEARCH_CLEAR,
        )?;
        send(self.search.edit, EM_SETLIMITTEXT, 160, 0);
        self.search.stale = true;
        unsafe {
            ShowWindow(self.search.list, SW_HIDE);
        }
        Ok(())
    }
    fn layout_search(&self, left: i32, right: i32, height: i32) -> i32 {
        if self.search.edit.is_null() {
            return 0;
        }
        let padding = self.s(16);
        let h = self.font.height() + self.s(10);
        let x = left + padding;
        let width = (right - left - padding * 2).max(1);
        let clear = self.s(30).min(width / 3);
        let input_width = (width - clear - self.s(4)).max(1);
        place(self.search.frame, x, padding, input_width, h);
        place(
            self.search.edit,
            x + self.s(8),
            padding + (h - self.font.height()) / 2,
            (input_width - self.s(16)).max(1),
            self.font.height(),
        );
        place(
            self.search.clear,
            x + width - clear,
            padding,
            clear.max(1),
            h,
        );
        send(
            self.search.edit,
            EM_SETCUEBANNER,
            1,
            wide(self.cfg.tr("Search settings…", "搜索设置…")).as_ptr() as isize,
        );
        unsafe {
            EnableWindow(
                self.search.clear,
                i32::from(!caption(self.search.edit).is_empty()),
            );
        }
        if self.search.visible {
            let row = self.font.height() + self.s(12);
            let rows = self.search.matches.len().clamp(1, 6) as i32;
            let list_height = (rows * row + self.s(4)).min((height / 3).max(row));
            send(self.search.list, LB_SETITEMHEIGHT, 0, row as isize);
            place(
                self.search.list,
                x,
                padding + h + self.s(4),
                width,
                list_height,
            );
            unsafe {
                ShowWindow(self.search.list, SW_SHOWNOACTIVATE);
            }
            if !self.search.matches.is_empty() {
                send(self.search.list, LB_SETCURSEL, self.search.selected, 0);
                let top = send(self.search.list, LB_GETTOPINDEX, 0, 0);
                if top > self.search.selected as isize
                    || self.search.selected as isize >= top + rows as isize
                {
                    send(self.search.list, LB_SETTOPINDEX, self.search.selected, 0);
                }
            }
            padding * 2 + h + self.s(4) + list_height
        } else {
            unsafe {
                ShowWindow(self.search.list, SW_HIDE);
            }
            padding * 2 + h
        }
    }
    fn refresh_search_index(&mut self) -> Result<(), String> {
        if self.search.stale {
            let draft = self.collect().unwrap_or_else(|_| self.doc.clone());
            self.search.index = crate::settings_search::Index::build(&draft)
                .or_else(|_| crate::settings_search::Index::build(&self.doc))?;
            self.search.stale = false;
        }
        Ok(())
    }
    fn fill_search_results(&self) {
        send(self.search.list, WM_SETREDRAW, 0, 0);
        send(self.search.list, LB_RESETCONTENT, 0, 0);
        if self.search.matches.is_empty() {
            send(
                self.search.list,
                LB_ADDSTRING,
                0,
                wide(self.cfg.tr("No matching settings", "未找到匹配的设置项")).as_ptr() as isize,
            );
        } else {
            for &at in &self.search.matches {
                send(
                    self.search.list,
                    LB_ADDSTRING,
                    0,
                    wide(&self.search.index.entries[at].label).as_ptr() as isize,
                );
            }
            send(self.search.list, LB_SETCURSEL, self.search.selected, 0);
        }
        send(self.search.list, WM_SETREDRAW, 1, 0);
        unsafe {
            InvalidateRect(self.search.list, null(), 1);
        }
    }
    fn run_search(&mut self) -> Result<(), String> {
        if self.search.composing {
            return Ok(());
        }
        let query = caption(self.search.edit);
        if query.trim().is_empty() {
            return self.close_search(true);
        }
        let previous = if query == self.search.query {
            self.search
                .matches
                .get(self.search.selected)
                .map(|&at| self.search.index.entries[at].target.clone())
        } else {
            None
        };
        self.search.query = query;
        self.refresh_search_index()?;
        self.search.matches = self.search.index.find(&self.search.query);
        self.search.selected = previous
            .and_then(|target| {
                self.search
                    .matches
                    .iter()
                    .position(|&at| self.search.index.entries[at].target == target)
            })
            .unwrap_or(0);
        self.search.visible = true;
        self.search.dismissed = false;
        self.fill_search_results();
        self.layout()?;
        if self.search.matches.is_empty() {
            self.search.highlight = None;
            self.update_search_highlight();
            unsafe {
                RedrawWindow(
                    self.panes[1],
                    null(),
                    null_mut(),
                    RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN,
                );
            }
            return Ok(());
        }
        self.navigate_search(false)
    }
    fn search_row(&self, target: &crate::settings_search::Target) -> Option<usize> {
        let crate::settings_search::Target::Field {
            tab,
            axis,
            section,
            key,
            occurrence,
        } = target
        else {
            return None;
        };
        if *tab != self.tab || (*tab == 5 && *axis != self.axis) {
            return None;
        }
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.field.section == *section && row.field.key == *key)
            .nth(*occurrence)
            .map(|(at, _)| at)
    }
    fn search_control(&self, target: &crate::settings_search::Target) -> HWND {
        match target {
            crate::settings_search::Target::Axis(at) => {
                let control = self.axis_controls[*at];
                if unsafe { IsWindowEnabled(control) } == 0 {
                    self.axis_controls[3]
                } else {
                    control
                }
            }
            _ => self
                .search_row(target)
                .map(|at| {
                    let row = &self.rows[at];
                    if !row.c(0).is_null() {
                        row.c(0)
                    } else {
                        row.c(1)
                    }
                })
                .unwrap_or(null_mut()),
        }
    }
    fn navigate_search(&mut self, focus: bool) -> Result<(), String> {
        let Some(&at) = self.search.matches.get(self.search.selected) else {
            return Ok(());
        };
        let target = self.search.index.entries[at].target.clone();
        self.save()?;
        if self.search.stale {
            self.refresh_search_index()?;
            self.search.matches = self.search.index.find(&self.search.query);
            let Some(selected) = self
                .search
                .matches
                .iter()
                .position(|&at| self.search.index.entries[at].target == target)
            else {
                self.search.highlight = None;
                self.search.selected = 0;
                self.fill_search_results();
                return self.layout();
            };
            self.search.selected = selected;
            self.fill_search_results();
        }
        let (tab, mut axis) = target.page();
        if tab == 5 && self.cfg.crosshair_unified {
            axis = 0;
        }
        self.switch_page(tab, axis)?;
        self.search.highlight = Some(target.clone());
        if !focus {
            self.overlay.hide_tip();
            self.preview.hide();
            self.cast.clear();
            self.cast_next = None;
            input::annotation_preview(false)?;
            input::track_preview(false)?;
        }
        if focus {
            self.search.visible = false;
            self.search.dismissed = true;
            self.layout()?;
            self.render()?;
        }
        let control = self.search_control(&target);
        if control.is_null() {
            return Ok(());
        }
        let anchor = self
            .search_row(&target)
            .map(|at| {
                let row = &self.rows[at];
                if !row.c(1).is_null() {
                    row.c(1)
                } else {
                    control
                }
            })
            .unwrap_or(control);
        let mut rect = RECT::default();
        unsafe {
            GetWindowRect(anchor, &mut rect);
            MapWindowPoints(
                null_mut(),
                self.panes[1],
                (&mut rect as *mut RECT).cast(),
                2,
            );
        }
        self.scroll_by(1, u16::MAX, rect.top - self.s(16))?;
        self.update_search_highlight();
        let mut focus_control = control;
        if focus
            && unsafe { IsWindowEnabled(control) } == 0
            && let crate::settings_search::Target::Field { section, key, .. } = &target
        {
            let enabler = match (section.as_str(), key.as_str()) {
                ("topmost_marker", _) => Some(("topmost_marker", "enabled")),
                ("shortcuts", "move_button") => Some(("shortcuts", "move_enabled")),
                ("shortcuts", "resize_button") => Some(("shortcuts", "resize_enabled")),
                ("shortcuts", "center_button") => Some(("shortcuts", "center_enabled")),
                _ => None,
            };
            focus_control = enabler
                .and_then(|(section, key)| {
                    self.rows
                        .iter()
                        .find(|row| row.field.section == section && row.field.key == key)
                })
                .map(|row| row.c(0))
                .unwrap_or(anchor);
        }
        unsafe {
            RedrawWindow(
                self.panes[1],
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN,
            );
            SetFocus(if focus {
                focus_control
            } else {
                self.search.edit
            });
        }
        Ok(())
    }
    fn update_search_highlight(&self) {
        let pane = self.panes[1];
        unsafe {
            RemovePropW(pane, w!("SearchMatch"));
        }
        let Some(target) = &self.search.highlight else {
            return;
        };
        if target.page().0 != self.tab {
            return;
        }
        let controls: Vec<_> = if let Some(at) = self.search_row(target) {
            self.rows[at]
                .controls
                .iter()
                .enumerate()
                .filter_map(|(slot, &hwnd)| (slot != 11 && !hwnd.is_null()).then_some(hwnd))
                .collect()
        } else if matches!(target, crate::settings_search::Target::Axis(_)) {
            vec![self.search_control(target)]
        } else {
            return;
        };
        let mut bounds = None;
        for hwnd in controls {
            if unsafe { IsWindowVisible(hwnd) } == 0 {
                continue;
            }
            let mut rect = RECT::default();
            unsafe {
                GetWindowRect(hwnd, &mut rect);
                MapWindowPoints(null_mut(), pane, (&mut rect as *mut RECT).cast(), 2);
            }
            bounds = Some(if let Some(old) = bounds {
                let mut joined = RECT::default();
                unsafe {
                    UnionRect(&mut joined, &old, &rect);
                }
                joined
            } else {
                rect
            });
        }
        if let Some(rect) = bounds {
            let pad = self.s(6);
            unsafe {
                for (name, value) in [
                    (w!("SearchLeft"), rect.left - pad),
                    (w!("SearchTop"), rect.top - pad),
                    (w!("SearchRight"), rect.right + pad),
                    (w!("SearchBottom"), rect.bottom + pad),
                ] {
                    SetPropW(pane, name, value as usize as HANDLE);
                }
                SetPropW(pane, w!("SearchMatch"), 1usize as HANDLE);
            }
        }
    }
    fn close_search(&mut self, clear: bool) -> Result<(), String> {
        unsafe {
            KillTimer(self.window.0, SEARCH_TIMER);
        }
        let was_visible = self.search.visible;
        self.search.visible = false;
        self.search.dismissed = true;
        if clear {
            self.search.query.clear();
            self.search.matches.clear();
            self.search.highlight = None;
            set_text(self.search.edit, "");
        }
        self.layout()?;
        if was_visible {
            self.render()?;
        }
        Ok(())
    }
    fn search_key(&mut self, key: u32) -> Result<(), String> {
        if self.search.composing {
            return Ok(());
        }
        if matches!(key, 13 | 38 | 40)
            && self.search.query != caption(self.search.edit)
            && !self.search.composing
        {
            unsafe {
                KillTimer(self.window.0, SEARCH_TIMER);
            }
            self.run_search()?;
        }
        match key {
            27 => {
                self.close_search(!self.search.visible)?;
                unsafe {
                    SetFocus(self.search.edit);
                }
            }
            13 => self.navigate_search(true)?,
            38 | 40 if !self.search.matches.is_empty() => {
                self.search.selected = if key == 38 {
                    self.search.selected.saturating_sub(1)
                } else {
                    (self.search.selected + 1).min(self.search.matches.len() - 1)
                };
                self.search.visible = true;
                self.search.dismissed = false;
                send(self.search.list, LB_SETCURSEL, self.search.selected, 0);
                self.layout()?;
                self.navigate_search(false)?;
            }
            _ => {}
        }
        Ok(())
    }
    fn update_axis_controls(&self) {
        for (i, control) in self.axis_controls.iter().enumerate() {
            send(
                *control,
                BM_SETCHECK,
                usize::from(match i {
                    0 => self.axis == 0,
                    1 => self.axis == 1,
                    2 => self.cfg.crosshair_unified,
                    _ => !self.cfg.crosshair_unified,
                }),
                0,
            );
            unsafe {
                EnableWindow(*control, i32::from(i >= 2 || !self.cfg.crosshair_unified));
            }
        }
    }
    fn status(&self, text: &str) {
        if self.save_error.get() {
            return;
        }
        self.show_status(text, false);
    }
    fn show_status(&self, text: &str, error: bool) {
        self.pending_status.borrow_mut().take();
        unsafe {
            KillTimer(self.window.0, SAVE_STATUS_TIMER);
        }
        self.save_error.set(error);
        self.last_error.replace(error.then(|| text.to_owned()));
        set_text(self.status, text);
        self.status_until.set(Some(
            Instant::now()
                + if error {
                    ERROR_STATUS_DURATION
                } else {
                    Duration::from_secs(3)
                },
        ));
        self.layout_status();
    }
    fn error(&self, text: &str) {
        // Automatic save retries must not keep the same error visible forever.
        if self.save_error.get() && self.last_error.borrow().as_deref() == Some(text) {
            return;
        }
        self.show_status(text, true);
    }
    fn saved_status(&self, text: &str, error: bool) {
        let interrupted = self.pending_status.borrow().is_some()
            || (!caption(self.status).is_empty()
                && self
                    .status_until
                    .get()
                    .is_some_and(|until| Instant::now() < until));
        if !interrupted {
            self.show_status(text, error);
            return;
        }
        // Repeated saves replace the pending notice and restart its quiet interval.
        self.pending_status.replace(Some((
            Instant::now() + Duration::from_millis(SAVE_STATUS_DELAY_MS.into()),
            text.to_owned(),
            error,
        )));
        self.save_error.set(error);
        self.last_error.replace(error.then(|| text.to_owned()));
        self.status_until.set(None);
        set_text(self.status, "");
        self.layout_status();
        unsafe {
            SetTimer(self.window.0, SAVE_STATUS_TIMER, SAVE_STATUS_DELAY_MS, None);
        }
    }
    fn show_pending_status(&self, now: Instant) {
        // Ignore a timer message queued for an earlier save. A newer save must get
        // its full delay even when the old timer message was already in the queue.
        let ready = self
            .pending_status
            .borrow_mut()
            .take_if(|(when, _, _)| now >= *when);
        if let Some((_, text, error)) = ready {
            self.show_status(&text, error);
        }
    }
    fn update_status(&self, now: Instant) {
        self.show_pending_status(now);
        if self
            .status_until
            .get()
            .is_some_and(|deadline| now >= deadline)
        {
            self.status_until.set(None);
            set_text(self.status, "");
            self.layout_status();
        }
    }
    fn layout_status(&self) {
        let mut client = RECT::default();
        let mut previous = RECT::default();
        unsafe {
            GetClientRect(self.window.0, &mut client);
            GetWindowRect(self.status, &mut previous);
            MapWindowPoints(
                null_mut(),
                self.window.0,
                (&mut previous as *mut RECT).cast(),
                2,
            );
        }
        let text = caption(self.status);
        if text.is_empty() {
            unsafe {
                if GetWindowLongW(self.status, GWL_STYLE) as u32 & WS_VISIBLE != 0 {
                    ShowWindow(self.status, SW_HIDE);
                    RedrawWindow(
                        self.window.0,
                        &previous,
                        null_mut(),
                        RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN,
                    );
                }
            }
            return;
        }
        let width = (text_width(&text, &self.font) + self.s(12))
            .min(client.right - self.s(32))
            .max(1);
        let height = measured(&text, &self.font, (width - self.s(12)).max(1)).min(
            if self.save_error.get() {
                (client.bottom / 3).max(self.font.height() * 2)
            } else {
                self.font.height() * 2
            },
        ) + self.s(8);
        unsafe {
            // Move and reshape without copying pixels painted under the previous clip.
            // Repaint both footprints afterwards, including the panes visible in the corners.
            SetWindowPos(
                self.status,
                HWND_TOP,
                0,
                client.bottom - height,
                width,
                height,
                SWP_NOACTIVATE | SWP_NOREDRAW | SWP_NOCOPYBITS | SWP_SHOWWINDOW,
            );
            // SetWindowRgn clips owner drawing and hit testing; Windows owns a successful region.
            let diameter = self.s(10).min(width).min(height).max(1);
            let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, diameter, diameter);
            if !region.is_null() && SetWindowRgn(self.status, region, 0) == 0 {
                DeleteObject(region);
            }
            let current = RECT {
                left: 0,
                top: client.bottom - height,
                right: width,
                bottom: client.bottom,
            };
            let mut damaged = RECT::default();
            UnionRect(&mut damaged, &previous, &current);
            RedrawWindow(
                self.window.0,
                &damaged,
                null_mut(),
                RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN,
            );
        }
    }
    fn create(
        &self,
        parent: HWND,
        class: &str,
        label: &str,
        style: u32,
        id: i32,
    ) -> Result<HWND, String> {
        let is_edit = class == "EDIT";
        let is_combo = class == "COMBOBOX";
        let class = wide(class);
        let style = if is_edit && id != SEARCH_EDIT {
            style | ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32
        } else if is_combo {
            (style & !3) | CBS_DROPDOWN as u32 | CBS_OWNERDRAWFIXED as u32 | CBS_HASSTRINGS as u32
        } else {
            style
        };
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
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
        let control = Window(hwnd);
        crate::settings_theme::apply(hwnd);
        send(hwnd, WM_SETFONT, self.font.0 as usize, 0);
        unsafe {
            desktop::check(SetWindowSubclass(
                hwnd,
                Some(control_proc),
                1,
                self.window.0 as usize,
            ))?;
            if is_combo {
                crate::settings_combo::attach(hwnd, self.cfg.zh);
                desktop::check(SetPropW(hwnd, w!("SettingsCombo"), 1usize as HANDLE))?;
                let mut info = COMBOBOXINFO {
                    cbSize: size_of::<COMBOBOXINFO>() as u32,
                    ..Default::default()
                };
                if GetComboBoxInfo(hwnd, &mut info) != 0 {
                    desktop::check(SetPropW(info.hwndList, w!("OwnerCombo"), hwnd))?;
                    desktop::check(SetWindowSubclass(
                        info.hwndList,
                        Some(control_proc),
                        1,
                        self.window.0 as usize,
                    ))?;
                }
                send(hwnd, CB_SETMINVISIBLE, 8, 0);
            }
        }
        std::mem::forget(control); // The parent HWND now owns this successfully initialized child.
        Ok(hwnd)
    }
    fn control(
        &self,
        row: &mut Row,
        index: usize,
        slot: usize,
        class: &str,
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
        let focused = unsafe { GetFocus() };
        let focus = self.rows.iter().find_map(|row| {
            row.controls
                .iter()
                .position(|&handle| !handle.is_null() && handle == focused)
                .map(|slot| {
                    (
                        row.field.section.clone(),
                        row.field.key.clone(),
                        slot,
                        send(focused, EM_GETSEL, 0, 0),
                    )
                })
        });
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
            &format!(
                "{} {}",
                crate::identity::NAME,
                self.cfg.tr("Settings", "设置")
            ),
        );
        for (i, hwnd) in self.axis_controls.iter().enumerate() {
            set_text(
                *hwnd,
                if i == 0 {
                    self.cfg.tr("Horizontal line", "横线")
                } else if i == 1 {
                    self.cfg.tr("Vertical line", "竖线")
                } else {
                    if i == 2 {
                        self.cfg.tr("Unified", "统一")
                    } else {
                        self.cfg.tr("Custom", "自定义")
                    }
                },
            );
            unsafe {
                ShowWindow(*hwnd, if self.tab == 5 { SW_SHOW } else { SW_HIDE });
            }
        }
        self.update_axis_controls();
        for (index, field) in schema::fields(self.tab, self.axis, &self.cfg, &self.doc)
            .into_iter()
            .enumerate()
        {
            let mut row = Row {
                field,
                controls: [null_mut(); 16],
                options: Vec::new(),
            };
            if self
                .rows
                .last()
                .is_some_and(|previous| schema::group(&previous.field) != schema::group(&row.field))
            {
                self.control(&mut row, index, 11, "STATIC", "", SS_OWNERDRAW)?;
                unsafe {
                    SetPropW(row.c(11), w!("Separator"), 1usize as HANDLE);
                }
            }
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
            if !matches!(
                kind,
                Kind::Bool | Kind::Preview | Kind::Add | Kind::OpenConfig
            ) {
                self.control(&mut row, index, 1, "STATIC", &label, SS_LEFT)?;
            }
            self.control(&mut row, index, 2, "STATIC", &hint, SS_LEFT)?;
            if !matches!(
                kind,
                Kind::Note | Kind::Preview | Kind::Add | Kind::OpenConfig
            ) {
                self.control(&mut row, index, 12, "STATIC", "", SS_LEFT)?;
                send(row.c(12), WM_SETFONT, self.hint_font.0 as usize, 0);
            }
            send(row.c(2), WM_SETFONT, self.hint_font.0 as usize, 0);
            match kind {
                Kind::Bool | Kind::Preview => {
                    self.control(
                        &mut row,
                        index,
                        0,
                        "BUTTON",
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
                    "EDIT",
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
                    // create() makes every combo box owner-drawn.
                    let style = CBS_DROPDOWNLIST as u32 | WS_TABSTOP | WS_VSCROLL;
                    self.control(&mut row, index, 0, "COMBOBOX", "", style)?;
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
                    let key = if row.field.section == "keys" {
                        config::shortcut_name(&row.field.key)
                    } else {
                        row.field.key.clone()
                    };
                    self.control(&mut row, index, 3, "EDIT", &key, WS_BORDER | WS_TABSTOP)?;
                    let related_label = if row.field.section == "presets" {
                        self.cfg.tr("Find shortcuts", "查看快捷键")
                    } else {
                        self.cfg.tr("View region", "查看分区")
                    };
                    self.control(
                        &mut row,
                        index,
                        6,
                        "BUTTON",
                        related_label,
                        BS_PUSHBUTTON as u32 | WS_TABSTOP,
                    )?;
                    if row.field.section == "keys" {
                        unsafe {
                            SetPropW(row.c(3), w!("ShortcutCapture"), 1 as HANDLE);
                            SetPropW(
                                row.c(3),
                                w!("ShortcutZh"),
                                usize::from(self.cfg.zh) as HANDLE,
                            );
                        }
                        let hint = shortcut_caption(&key, false, self.cfg.zh);
                        self.control(&mut row, index, 7, "STATIC", &hint, SS_LEFT)?;
                    }
                    self.control(
                        &mut row,
                        index,
                        4,
                        "BUTTON",
                        self.cfg.tr("Delete", "删除"),
                        BS_PUSHBUTTON as u32 | WS_TABSTOP,
                    )?;
                    if row.field.section == "presets" {
                        self.control(
                            &mut row,
                            index,
                            0,
                            "EDIT",
                            &text,
                            WS_BORDER
                                | WS_TABSTOP
                                | ES_MULTILINE as u32
                                | ES_AUTOVSCROLL as u32
                                | ES_AUTOHSCROLL as u32,
                        )?;
                        self.control(
                            &mut row,
                            index,
                            5,
                            "BUTTON",
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
                        self.control(
                            &mut row,
                            index,
                            5,
                            "BUTTON",
                            self.cfg.tr("Preview this region", "预览此分区"),
                            BS_PUSHBUTTON as u32 | WS_TABSTOP,
                        )?;
                        row.options = schema::actions(&self.cfg, false);
                        self.control(
                            &mut row,
                            index,
                            0,
                            "COMBOBOX",
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
                        "BUTTON",
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
                            "EDIT",
                            &amount.to_string(),
                            WS_BORDER | WS_TABSTOP | ES_NUMBER as u32,
                        )?;
                        self.control(
                            &mut row,
                            index,
                            channel + 7,
                            "STATIC",
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
                        "LISTBOX",
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
                        "COMBOBOX",
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
                            "BUTTON",
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
                Kind::Add | Kind::OpenConfig => self.control(
                    &mut row,
                    index,
                    0,
                    "BUTTON",
                    &label,
                    BS_PUSHBUTTON as u32 | WS_TABSTOP,
                )?,
                Kind::Note => {}
            }
            self.rows.push(row);
        }
        self.update_enabled_controls();
        self.layout()?;
        for row in &self.rows {
            for hwnd in row.controls {
                if !hwnd.is_null() {
                    send(hwnd, EDIT_LAYOUT, 0, 0);
                }
            }
        }
        self.render()?;
        if let Some((section, key, slot, selection)) = focus
            && let Some(row) = self
                .rows
                .iter()
                .find(|row| row.field.section == section && row.field.key == key)
            && !row.c(slot).is_null()
        {
            unsafe {
                SetFocus(row.c(slot));
            }
            send(
                row.c(slot),
                EM_SETSEL,
                (selection as usize) & 0xffff,
                (selection >> 16) & 0xffff,
            );
        }
        #[cfg(test)]
        unsafe {
            SetPropW(self.window.0, w!("ReadyTab"), (self.tab + 1) as HANDLE);
        }
        Ok(())
    }
    fn resize(&mut self) -> Result<(), String> {
        let mut client = RECT::default();
        unsafe {
            GetClientRect(self.window.0, &mut client);
        }
        if self.layout_size == Some((client.right, client.bottom)) {
            unsafe {
                RemovePropW(self.window.0, w!("ResizeQueued"));
            }
            return Ok(());
        }
        self.layout()
    }
    fn layout(&mut self) -> Result<(), String> {
        #[cfg(test)]
        unsafe {
            let count = GetPropW(self.window.0, w!("LayoutCount")) as usize;
            SetPropW(self.window.0, w!("LayoutCount"), (count + 1) as HANDLE);
        }
        unsafe {
            RemovePropW(self.window.0, w!("ResizeQueued"));
        }
        let mut client = RECT::default();
        unsafe {
            GetClientRect(self.window.0, &mut client);
        }
        self.layout_size = Some((client.right, client.bottom));
        let height = client.bottom.max(1);
        let left = self.sidebar_width(client.right);
        place(self.panes[0], 0, 0, left, height);
        let header = self.layout_search(left, client.right, height);
        place(
            self.panes[1],
            left,
            header,
            client.right - left,
            (height - header).max(1),
        );
        self.layout_status();
        let padding = self.s(16);
        let gap = self.s(4);
        let mut rect = RECT::default();
        unsafe {
            GetClientRect(self.panes[0], &mut rect);
        }
        let width = (rect.right - padding * 2).max(1);
        place(
            unsafe { GetDlgItem(self.panes[0], 99) },
            rect.right - 1,
            0,
            1,
            height,
        );
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
        let number_width = (text_width("0000", &self.font) + self.s(16)).min(width);
        let button_width = self.s(68).min(width);
        let name_width = combo_width;
        let expression_width = (combo_width * 2).min(width);
        let mut y = padding;
        for row in &self.rows {
            if !row.c(11).is_null() {
                y += self.s(2);
                place(row.c(11), padding, y - self.scroll[1], width, 1);
                y += self.s(14);
            }
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
                    let row_gap = self.s(10).min((width - 3).max(0) / 4);
                    let delete_width = button_width.min((width / 4).max(1));
                    let fields_width = (width - delete_width - 2 * row_gap).max(2);
                    let key_width = name_width.min((fields_width / 3).max(1));
                    let value_width = (fields_width - key_width).max(1);
                    let top = y - self.scroll[1];
                    place(row.c(3), padding, top, key_width, h);
                    place(
                        row.c(0),
                        padding + key_width + row_gap,
                        top,
                        value_width,
                        if preset { h } else { h * 10 },
                    );
                    place(
                        row.c(4),
                        padding + width - delete_width,
                        top,
                        delete_width,
                        h,
                    );
                    if !preset {
                        send(
                            row.c(0),
                            CB_SETITEMHEIGHT,
                            usize::MAX,
                            (h - self.s(4)) as isize,
                        );
                        send(row.c(0), CB_SETDROPPEDWIDTH, value_width as usize, 0);
                    }
                    y += h + gap;
                    let related_width = (text_width(&caption(row.c(6)), &self.font) + self.s(24))
                        .min((width - row_gap).max(2) / 2);
                    let preview_width = (text_width(&caption(row.c(5)), &self.font) + self.s(30))
                        .min((width - related_width - row_gap).max(1));
                    place(row.c(5), padding, y - self.scroll[1], preview_width, h);
                    place(
                        row.c(6),
                        padding + width - related_width,
                        y - self.scroll[1],
                        related_width,
                        h,
                    );
                    y += h;
                    if !row.c(7).is_null() {
                        y += gap;
                        let hint_height =
                            measured(&caption(row.c(7)), &self.hint_font, width).max(h);
                        place(row.c(7), padding, y - self.scroll[1], width, hint_height);
                        y += hint_height;
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
                    let add_x = if font_width + gap + button_width <= width {
                        padding + font_width + gap
                    } else {
                        y += h + gap;
                        padding
                    };
                    place(row.c(4), add_x, y - self.scroll[1], button_width, h);
                    y += h + gap;
                    let columns = ((width + gap) / (button_width + gap)).clamp(1, 3);
                    for slot in 5..=7 {
                        let index = slot as i32 - 5;
                        place(
                            row.c(slot),
                            padding + (index % columns) * (button_width + gap),
                            y + (index / columns) * (h + gap) - self.scroll[1],
                            button_width,
                            h,
                        );
                    }
                    y += ((3 + columns - 1) / columns) * (h + gap) - gap;
                }
                Kind::Note => {}
                Kind::Number => {
                    place(
                        row.c(0),
                        padding,
                        y - self.scroll[1],
                        if row.field.key.ends_with("_ms") {
                            (number_width + self.s(20)).min(width)
                        } else {
                            number_width
                        },
                        h,
                    );
                    y += h;
                }
                Kind::Add | Kind::OpenConfig => {
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
            if !row.c(12).is_null() {
                let state = caption(row.c(12));
                if state.is_empty() {
                    unsafe {
                        ShowWindow(row.c(12), SW_HIDE);
                    }
                } else {
                    let state_height =
                        measured(&state, &self.hint_font, width).max(self.hint_font.height());
                    y += self.s(2);
                    place(row.c(12), padding, y - self.scroll[1], width, state_height);
                    y += state_height;
                    unsafe {
                        ShowWindow(row.c(12), SW_SHOW);
                    }
                }
            }
            y += self.s(12);
            if self.tab == 5 && row.field.section == "crosshair" {
                let axis_height = self
                    .axis_controls
                    .iter()
                    .map(|control| {
                        measured(
                            &caption(*control),
                            &self.font,
                            (width / 2 - self.s(24)).max(1),
                        ) + self.s(10)
                    })
                    .max()
                    .unwrap_or(h)
                    .max(h);
                for (i, control) in self.axis_controls.iter().enumerate() {
                    place(
                        *control,
                        padding + (i % 2) as i32 * (width / 2),
                        y + if i < 2 { axis_height } else { 0 } - self.scroll[1],
                        width / 2,
                        axis_height,
                    );
                }
                y += axis_height * 2 + self.s(10);
            }
        }
        self.extent[1] = y + padding;
        let mut adjusted = false;
        for pane in 0..2 {
            let viewport = if pane == 0 {
                height
            } else {
                (height - header).max(1)
            };
            let old = self.scroll[pane];
            self.scroll[pane] = old.clamp(0, (self.extent[pane] - viewport).max(0));
            adjusted |= old != self.scroll[pane];
            let info = SCROLLINFO {
                cbSize: size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                nMin: 0,
                nMax: self.extent[pane] - 1,
                nPage: viewport as u32,
                nPos: self.scroll[pane],
                ..Default::default()
            };
            if pane == 1 {
                unsafe {
                    // Keep the scrollbar gutter stable when content starts/stops overflowing.
                    let info = SCROLLINFO {
                        fMask: info.fMask | SIF_DISABLENOSCROLL,
                        ..info
                    };
                    let mut old = SCROLLINFO {
                        cbSize: info.cbSize,
                        fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                        ..Default::default()
                    };
                    GetScrollInfo(self.panes[pane], SB_VERT, &mut old);
                    if (old.nMax, old.nPage, old.nPos) != (info.nMax, info.nPage, info.nPos) {
                        SetScrollInfo(self.panes[pane], SB_VERT, &info, 1);
                    }
                }
            }
        }
        unsafe {
            GetClientRect(self.panes[1], &mut rect);
        }
        if adjusted || rect.right != initial_right_width {
            self.layout()?;
        }
        self.update_search_highlight();
        // Complete this resize before Windows draws the next drag frame. This also
        // erases newly exposed text and the background behind rounded status corners.
        unsafe {
            RedrawWindow(
                self.window.0,
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
            );
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
            if pane == 1 {
                SetScrollPos(self.panes[pane], SB_VERT, self.scroll[pane], 1);
            }
        }
        self.update_search_highlight();
        Ok(())
    }
    fn selected_value(row: &Row, slot: usize) -> String {
        let i = crate::settings_combo::selected_index(row.c(slot));
        row.options
            .get(i.unwrap_or(usize::MAX))
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
                Kind::Choice(_) if self.doc[section][key].is_integer() => value(
                    Self::selected_value(row, 0)
                        .parse::<i64>()
                        .map_err(|error| error.to_string())?,
                ),
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
                        config::assign(
                            &mut draft[section]["opacity"],
                            value(i64::from(channels[3])),
                        );
                    }
                    value(format!(
                        "#{:02X}{:02X}{:02X}",
                        channels[0], channels[1], channels[2]
                    ))
                }
                Kind::Mapping => {
                    let name = caption(row.c(3)).trim().to_owned();
                    let name = if section == "keys" {
                        config::shortcut_name(&name)
                    } else {
                        name
                    };
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
                        crate::layout::normalize(&caption(row.c(0)))
                            .map_err(|error| format!("presets.{name}: {error}"))?
                    } else {
                        Self::selected_value(row, 0)
                    };
                    if name != *key {
                        if draft[section]
                            .as_table()
                            .unwrap()
                            .iter()
                            .any(|(existing, _)| {
                                existing != key
                                    && config::command_name(existing) == config::command_name(&name)
                            })
                        {
                            return Err(self.cfg.tr("Name already exists", "名称已存在").into());
                        }
                        rename_entry(draft[section].as_table_mut().unwrap(), key, &name);
                        if section == "presets" {
                            for bindings in ["keys", "commands"] {
                                for (_, item) in draft[bindings].as_table_mut().unwrap().iter_mut()
                                {
                                    if item.as_str().is_some_and(|v| {
                                        config::command_name(v) == config::command_name(key)
                                    }) {
                                        config::assign(item, value(&name));
                                    }
                                }
                            }
                        }
                    }
                    config::assign(&mut draft[section][&name], value(text));
                    continue;
                }
                _ => continue,
            };
            config::assign(&mut draft[section][key], replacement);
        }
        Ok(draft)
    }
    fn save(&mut self) -> Result<(), String> {
        self.pending = None;
        if self.dirty.is_empty() {
            return Ok(());
        }
        let draft = match self
            .collect()
            .and_then(|draft| Config::parse(&draft).map(|_| draft))
        {
            Ok(draft) => draft,
            Err(error) => {
                self.mark_edit_error(&error);
                self.layout()?;
                return Err(error);
            }
        };
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
        let saved = match config::save_settings(&self.path, &self.original, &draft) {
            Ok(saved) => saved,
            Err(error) => {
                for &index in &self.dirty {
                    if let Some(row) = self.rows.get(index) {
                        set_text(
                            row.c(12),
                            self.cfg.tr(
                                "Not saved: file write failed; see details below",
                                "未保存：文件写入失败，详见底部提示",
                            ),
                        );
                    }
                }
                // The edits themselves are valid, so the file is busy, read-only or was
                // changed meanwhile. Nothing else would try again: retry until it works
                // instead of leaving the edits unsaved once the message fades.
                if !error.contains("Concurrent configuration edit preserved") {
                    self.pending = Some(Instant::now() + Duration::from_secs(2));
                }
                return Err(error);
            }
        };
        self.doc = saved.parse::<DocumentMut>().map_err(|e| e.to_string())?;
        self.original = saved;
        self.search.stale = true;
        let cfg = Config::parse(&self.doc)?;
        // The edit already updated the live preview. Saving only restarts it
        // when merging concurrent file changes alters the effective settings.
        let preview_changed = cfg != self.cfg;
        self.apply_config(cfg)?;
        for row in &mut self.rows {
            if matches!(row.field.kind, Kind::Mapping) {
                let name = caption(row.c(3)).trim().to_owned();
                let name = if row.field.section == "keys" {
                    config::shortcut_name(&name)
                } else {
                    name
                };
                if self.doc[&row.field.section].get(&name).is_some() && name != row.field.key {
                    row.field.key = name.clone();
                    row.field.label = name.clone();
                    set_text(row.c(1), &name);
                }
            }
        }
        for &index in &self.dirty {
            if let Some(row) = self.rows.get(index) {
                set_text(row.c(12), "");
            }
        }
        self.dirty.clear();
        self.save_error.set(false);
        self.saved_status(self.cfg.tr("Saved", "已保存"), false);
        self.invalidate_pages();
        if rebuild || self.rows_changed() {
            self.rebuild()?;
        } else {
            self.refresh_mapping_options();
            self.update_values();
            if preview_changed && self.tab != 0 && self.tab != 4 {
                self.render()?;
            }
        }
        Ok(())
    }
    fn mark_edit_error(&self, error: &str) {
        let matches =
            |row: &Row| error.starts_with(&format!("{}.{}", row.field.section, row.field.key));
        let found = self
            .dirty
            .iter()
            .filter_map(|i| self.rows.get(*i))
            .any(matches);
        for &index in &self.dirty {
            if let Some(row) = self.rows.get(index) {
                let state = if !found || matches(row) {
                    format!(
                        "{}: {error}",
                        self.cfg
                            .tr("Invalid input · Not saved", "输入有误 · 未保存")
                    )
                } else {
                    self.cfg
                        .tr(
                            "Not saved · Correct the marked field first",
                            "未保存 · 请先修正标出的字段",
                        )
                        .into()
                };
                set_text(row.c(12), &state);
            }
        }
    }
    fn update_enabled_controls(&self) {
        let field = |section: &str, key: &str| {
            self.rows
                .iter()
                .find(|row| row.field.section == section && row.field.key == key)
        };
        let saved =
            |section: &str, key: &str| self.doc.get(section).and_then(|table| table.get(key));
        // Prefer the visible control so dependants follow an edit before it is saved;
        // switches on another page are read from the document.
        let checked = |section: &str, key: &str| {
            field(section, key).map_or_else(
                || saved(section, key).and_then(Item::as_bool).unwrap_or(true),
                |row| send(row.c(0), BM_GETCHECK, 0, 0) == 1,
            )
        };
        let gestures = checked("shortcuts", "mouse_enabled");
        for (section, key, active) in [
            ("shortcuts", "linked_resize", gestures),
            ("shortcuts", "move_enabled", gestures),
            ("shortcuts", "resize_enabled", gestures),
            ("shortcuts", "center_enabled", gestures),
            ("selection", "add_button", gestures),
            ("selection", "apply_button", gestures),
            (
                "topmost_marker",
                "size",
                checked("topmost_marker", "enabled"),
            ),
            (
                "topmost_marker",
                "position",
                checked("topmost_marker", "enabled"),
            ),
            (
                "shortcuts",
                "move_button",
                gestures && checked("shortcuts", "move_enabled"),
            ),
            (
                "shortcuts",
                "resize_button",
                gestures && checked("shortcuts", "resize_enabled"),
            ),
            (
                "shortcuts",
                "center_button",
                gestures && checked("shortcuts", "center_enabled"),
            ),
        ] {
            if let Some(row) = field(section, key) {
                // SAFETY: these child HWNDs belong to the editor and stay alive for this call.
                unsafe {
                    if (IsWindowEnabled(row.c(0)) != 0) != active {
                        if !active && matches!(row.field.kind, Kind::Choice(_)) {
                            send(row.c(0), CB_SHOWDROPDOWN, 0, 0);
                        }
                        EnableWindow(row.c(0), i32::from(active));
                        InvalidateRect(row.c(0), null(), 1);
                    }
                }
            }
        }
        for row in &self.rows {
            if matches!(row.field.kind, Kind::Mapping) && row.field.section != "presets" {
                let region = self
                    .cfg
                    .presets
                    .contains_key(&config::command_name(&Self::selected_value(row, 0)));
                unsafe {
                    EnableWindow(row.c(5), i32::from(region));
                    EnableWindow(row.c(6), i32::from(region));
                }
            }
        }
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
                    let key = if row.field.section == "keys" {
                        config::shortcut_name(&row.field.key)
                    } else {
                        row.field.key.clone()
                    };
                    set_text(row.c(3), &key);
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
                    let selected = item.and_then(|item| {
                        item.as_str()
                            .map(str::to_owned)
                            .or_else(|| item.as_integer().map(|number| number.to_string()))
                    });
                    if let Some(v) = selected
                        && let Some(i) = row
                            .options
                            .iter()
                            .position(|o| o.0.eq_ignore_ascii_case(&v))
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
        self.update_enabled_controls();
    }
    fn rows_changed(&self) -> bool {
        let expected = schema::fields(self.tab, self.axis, &self.cfg, &self.doc);
        expected.len() != self.rows.len()
            || expected.iter().zip(&self.rows).any(|(a, b)| {
                a.section != b.field.section
                    || a.key != b.field.key
                    || a.label != b.field.label
                    || a.hint != b.field.hint
            })
    }

    fn refresh_mapping_options(&mut self) {
        let height = self.font.height() + self.s(14);
        for row in &mut self.rows {
            if !matches!(row.field.kind, Kind::Mapping) || row.field.section == "presets" {
                continue;
            }
            let options = schema::actions(&self.cfg, false);
            if row.options != options {
                let selected = self
                    .doc
                    .get(&row.field.section)
                    .and_then(|table| table.get(&row.field.key))
                    .and_then(Item::as_str)
                    .unwrap_or("");
                row.options = options;
                populate(row.c(0), &row.options, selected, height);
            }
        }
    }
    fn render(&mut self) -> Result<(), String> {
        self.cast.clear();
        self.cast_next = None;
        input::annotation_preview(false)?;
        unsafe {
            SetTimer(self.window.0, 1, 100, None);
        }
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
        if self.tab == 1 {
            self.overlay.tip(
                self.cfg.tr(
                    "Left click to select · Right click to apply · Esc to exit",
                    "左键累计选择 · 右键立即应用 · Esc 退出",
                ),
                &self.cfg,
            )?;
        } else if self.live {
            match self.tab {
                2 | 4 => {
                    if let Some(name) = &self.selected {
                        self.preview.select(self.window.0, name, &self.cfg)?;
                    }
                }
                3 => self.preview.start(self.window.0, &self.cfg)?,
                6 => self.preview.start(self.window.0, &self.cfg)?,
                7 | 8 => {
                    self.cast
                        .preview(self.tab == 7, desktop::cursor()?, &self.cfg)?;
                    self.cast_next = Some(
                        Instant::now()
                            + crate::cast::Visuals::preview_interval(self.tab == 7, &self.cfg),
                    );
                    unsafe {
                        SetTimer(self.window.0, 1, 16, None);
                    }
                }
                _ => {}
            }
        }
        if !self.preview.warnings().is_empty() {
            self.status(&self.preview.warnings().join("\n"));
        }
        input::set_guides(guides != 0)?;
        input::annotation_preview(self.cast_next.is_some())?;
        input::track_preview(self.preview.visible())
    }
    fn commit_document(&mut self, draft: DocumentMut) -> Result<(), String> {
        Config::parse(&draft)?;
        self.original = config::save_settings(&self.path, &self.original, &draft)?;
        self.search.stale = true;
        self.doc = self
            .original
            .parse::<DocumentMut>()
            .map_err(|e| e.to_string())?;
        self.apply_config(Config::parse(&self.doc)?)?;
        self.dirty.clear();
        self.pending = None;
        self.save_error.set(false);
        self.saved_status(self.cfg.tr("Saved", "已保存"), false);
        if !self.rows_changed() {
            self.invalidate_pages();
            self.refresh_mapping_options();
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
            "keys" => (2..=24)
                .filter(|n| !config::reserved_key(0x6f + n))
                .map(|n| format!("F{n}"))
                .collect(),
            "presets" => (1..=999).map(|n| format!("Region{n}")).collect(),
            _ => (1..=999).map(|n| format!("Command{n}")).collect(),
        };
        let name = names
            .into_iter()
            .find(|n| {
                !draft[section].as_table().unwrap().iter().any(|(k, _)| {
                    if section == "keys" {
                        config::vk(k) == config::vk(n)
                    } else {
                        config::command_name(k) == config::command_name(n)
                    }
                }) && (section != "commands"
                    || !self.cfg.presets.contains_key(&config::command_name(n)))
                    && (section != "presets"
                        || !self.cfg.commands.contains_key(&config::command_name(n)))
            })
            .ok_or("No free name")?;
        let text = if section == "presets" {
            "10sw|10sh|80sw|80sh".to_owned()
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
        // The row is going away: its own unsaved, possibly invalid, edits must neither
        // block the deletion nor rename the entry that is about to be removed.
        self.dirty.remove(&index);
        self.save()?;
        let mut draft = self.doc.clone();
        draft[&section].as_table_mut().unwrap().remove(&key);
        if section == "presets" {
            for section in ["keys", "commands"] {
                for (_, item) in draft[section].as_table_mut().unwrap().iter_mut() {
                    if item
                        .as_str()
                        .is_some_and(|n| config::command_name(n) == config::command_name(&key))
                    {
                        config::assign(item, value(""));
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
        if crate::settings_combo::notification(sender, event) {
            return Ok(());
        }
        if id == SEARCH_FRAME && event == STN_CLICKED {
            unsafe {
                SetFocus(self.search.edit);
            }
            return Ok(());
        }
        if id == SEARCH_EDIT && sender == self.search.edit {
            if event == EN_CHANGE && !self.search.composing {
                self.search.dismissed = false;
                unsafe {
                    SetTimer(self.window.0, SEARCH_TIMER, 150, None);
                }
            } else if event == EN_KILLFOCUS {
                unsafe {
                    KillTimer(self.window.0, SEARCH_TIMER);
                }
            } else if event == EN_SETFOCUS
                && !self.search.composing
                && (caption(self.search.edit) != self.search.query
                    || (self.search.stale
                        && !self.search.dismissed
                        && !self.search.query.is_empty()))
            {
                self.search.dismissed = false;
                unsafe {
                    SetTimer(self.window.0, SEARCH_TIMER, 150, None);
                }
            } else if event == EN_SETFOCUS
                && !self.search.dismissed
                && !self.search.query.is_empty()
                && !self.search.visible
            {
                self.search.visible = true;
                self.layout()?;
            }
            return Ok(());
        }
        if id == SEARCH_RESULTS && sender == self.search.list {
            if matches!(event, LBN_SELCHANGE | LBN_DBLCLK) {
                let selected = send(sender, LB_GETCURSEL, 0, 0);
                if selected >= 0 && (selected as usize) < self.search.matches.len() {
                    self.search.selected = selected as usize;
                    self.navigate_search(event == LBN_DBLCLK)?;
                }
            }
            return Ok(());
        }
        if id == SEARCH_CLEAR && event == BN_CLICKED {
            self.close_search(true)?;
            unsafe {
                SetFocus(self.search.edit);
            }
            return Ok(());
        }
        if matches!(
            event,
            BN_CLICKED | crate::settings_combo::COMMIT | EN_CHANGE | EDIT_COMMIT
        ) {
            // A new user action can report the same problem again; background
            // retries without another edit keep the original ten-second deadline.
            self.last_error.borrow_mut().take();
        }
        if event == CBN_DROPDOWN && id >= BASE {
            self.active_combo = Some(sender);
        }
        if event == CBN_CLOSEUP {
            self.active_combo = None;
            self.tooltip.hide();
        }
        if (TAB_BASE..TAB_BASE + TABS.len() as i32).contains(&id) && event == BN_CLICKED {
            self.save()?;
            if !self.search.edit.is_null() {
                self.close_search(false)?;
            }
            self.search.highlight = None;
            return self.switch_page((id - TAB_BASE) as usize, self.axis);
        }
        if matches!(id, AXIS_H | AXIS_V | AXIS_BOTH | AXIS_CUSTOM) && event == BN_CLICKED {
            self.save()?;
            if matches!(id, AXIS_BOTH | AXIS_CUSTOM) {
                let mut draft = self.doc.clone();
                draft["crosshair"]["mode"] =
                    value(if id == AXIS_BOTH { "unified" } else { "custom" });
                self.commit_document(draft)?;
                self.invalidate_pages();
                if id == AXIS_BOTH {
                    self.axis = 0;
                }
                return self.rebuild();
            }
            if self.cfg.crosshair_unified {
                self.update_axis_controls();
                return Ok(());
            }
            self.update_axis_controls();
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
        if matches!(event, EN_KILLFOCUS | EDIT_COMMIT) {
            if self.dirty.contains(&index) {
                self.pending = Some(Instant::now());
            }
            return Ok(());
        }
        match kind {
            Kind::OpenConfig if event == BN_CLICKED => {
                self.save()?;
                return crate::config_access::open(&self.path);
            }
            Kind::OpenConfig => return Ok(()),
            Kind::Preview if event == BN_CLICKED => {
                self.live = send(row.c(0), BM_GETCHECK, 0, 0) == 1;
                return self.render();
            }
            Kind::Add if event == BN_CLICKED => {
                let section = row.field.section.clone();
                return self.add(&section);
            }
            Kind::Mapping if slot == 4 && event == BN_CLICKED => return self.delete(index),
            Kind::Mapping if slot == 6 && event == BN_CLICKED => {
                let source = row.field.section.clone();
                let name = if source == "presets" {
                    row.field.key.clone()
                } else {
                    Self::selected_value(row, 0)
                };
                self.save()?;
                let tab = if source == "presets" { 4 } else { 2 };
                self.switch_page(tab, 0)?;
                let normalized = config::command_name(&name);
                let target = self
                    .rows
                    .iter()
                    .find(|r| {
                        if source == "presets" {
                            r.field.section == "keys"
                                && self.doc["keys"]
                                    .get(&r.field.key)
                                    .and_then(Item::as_str)
                                    .is_some_and(|v| config::command_name(v) == normalized)
                        } else {
                            r.field.section == "presets"
                                && config::command_name(&r.field.key) == normalized
                        }
                    })
                    .map(|r| r.c(3));
                if let Some(control) = target {
                    let mut rect = RECT::default();
                    unsafe {
                        GetWindowRect(control, &mut rect);
                        MapWindowPoints(
                            null_mut(),
                            self.panes[1],
                            (&mut rect as *mut RECT).cast(),
                            2,
                        );
                    }
                    self.scroll_by(1, u16::MAX, rect.top - self.s(16))?;
                    unsafe {
                        SetFocus(control);
                    }
                } else {
                    self.status(self.cfg.tr(
                        "No linked item; add a shortcut or select a region action.",
                        "尚无关联项；请添加快捷键或选择分区动作。",
                    ));
                }
                return Ok(());
            }
            Kind::Mapping if slot == 5 && event == BN_CLICKED => {
                let name = if row.field.section == "presets" {
                    row.field.key.clone()
                } else {
                    Self::selected_value(row, 0)
                };
                if !self.cfg.presets.contains_key(&config::command_name(&name)) {
                    return Ok(());
                }
                self.selected = Some(name);
                self.live = true;
                for row in &self.rows {
                    if !row.c(5).is_null() && row.field.section == "presets" {
                        send(
                            row.c(5),
                            BM_SETCHECK,
                            usize::from(self.selected.as_ref() == Some(&row.field.key)),
                            0,
                        );
                    }
                    if matches!(row.field.kind, Kind::Preview) {
                        send(row.c(0), BM_SETCHECK, 1, 0);
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
            Kind::Choice(_) | Kind::Font => event == crate::settings_combo::COMMIT,
            Kind::Mapping if slot == 0 && row.field.section != "presets" => {
                event == crate::settings_combo::COMMIT
            }
            _ => event == EN_CHANGE,
        };
        if changed {
            self.dirty.insert(index);
            set_text(
                self.rows[index].c(12),
                self.cfg.tr(
                    "Not saved · Enter or leave text field to save",
                    "未保存 · 文本按 Enter 或离开后保存",
                ),
            );
            // Typed text is saved when its field loses focus or Enter is pressed, so a
            // half-typed number or name never reaches the file. Switches and lists are
            // complete as soon as they change.
            if event != EN_CHANGE {
                self.pending = Some(Instant::now() + Duration::from_millis(450));
            }
            self.update_enabled_controls();
            match self.collect().and_then(|draft| Config::parse(&draft)) {
                Ok(cfg) => {
                    self.apply_config(cfg)?;
                    if self.tab != 0 && self.tab != 4 {
                        self.render()?;
                    }
                    if matches!(kind, Kind::Color(_)) {
                        unsafe {
                            InvalidateRect(self.rows[index].c(0), null(), 1);
                        }
                    }
                }
                Err(error) => self.mark_edit_error(&error),
            }
            self.layout()?;
        }
        Ok(())
    }
    fn refresh_display(&mut self) -> Result<(), String> {
        DISPLAY_UPDATE.with(|flag| flag.set(false));
        self.dpi = unsafe { GetDpiForWindow(self.window.0) }.max(96) as i32;
        // Keep pending (even temporarily invalid) edits in their controls.
        // Old fonts remain alive until every control has its replacement.
        let font = Font::new(
            &self.cfg.font,
            self.cfg.font_size,
            self.dpi,
            self.cfg.font_bold,
            self.cfg.font_italic,
        )?;
        let hint = Font::new(
            &self.cfg.font,
            (self.cfg.font_size * 3 / 4).max(6),
            self.dpi,
            false,
            self.cfg.font_italic,
        )?;
        let _old_font = std::mem::replace(&mut self.font, font);
        let _old_hint = std::mem::replace(&mut self.hint_font, hint);
        crate::settings_paint::reset_fonts();
        MEASUREMENTS.with_borrow_mut(HashMap::clear);
        for handle in self
            .tabs
            .iter()
            .chain(self.axis_controls.iter())
            .chain(std::iter::once(&self.status))
            .chain([&self.search.edit, &self.search.list, &self.search.clear])
            .chain(std::iter::once(&self.search.frame))
        {
            send(*handle, WM_SETFONT, self.font.0 as usize, 1);
        }
        for row in self
            .rows
            .iter()
            .chain(self.pages.values().flat_map(|(rows, _)| rows))
        {
            for (slot, handle) in row.controls.iter().enumerate() {
                if !handle.is_null() {
                    send(
                        *handle,
                        WM_SETFONT,
                        if matches!(slot, 2 | 12) {
                            self.hint_font.0
                        } else {
                            self.font.0
                        } as usize,
                        1,
                    );
                    let mut class = [0u16; 32];
                    let len =
                        unsafe { GetClassNameW(*handle, class.as_mut_ptr(), class.len() as i32) };
                    if String::from_utf16_lossy(&class[..len.max(0) as usize])
                        .eq_ignore_ascii_case("COMBOBOX")
                    {
                        send(
                            *handle,
                            CB_SETITEMHEIGHT,
                            0,
                            (self.font.height() + self.s(14)) as isize,
                        );
                    }
                }
            }
        }
        if let Some(rect) = DPI_RECT.with(|rect| rect.take()) {
            place(
                self.window.0,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
            );
        }
        self.layout()?;
        self.render()
    }
    fn tick(&mut self) -> Result<(), String> {
        if DISPLAY_UPDATE.with(Cell::get) {
            self.refresh_display()?;
        }
        if !unsafe { RemovePropW(self.window.0, w!("PreviewActivationPending")) }.is_null() {
            self.render()?;
        }
        let now = Instant::now();
        self.cast.tick(&self.cfg)?;
        if self.cast_next.is_some_and(|when| now >= when) {
            self.cast
                .preview(self.tab == 7, desktop::cursor()?, &self.cfg)?;
            self.cast_next =
                Some(now + crate::cast::Visuals::preview_interval(self.tab == 7, &self.cfg));
        }
        // Native combo popups capture the pointer and do not reliably deliver list mouse-move
        // messages to subclasses. Hit-test the open list on the existing UI timer as well.
        if let Some(combo) = self.active_combo {
            self.hover(combo, -1);
        }
        self.tooltip.tick();
        self.update_status(now);
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
                self.search.stale = true;
                self.doc = loaded.document;
                self.apply_config(loaded.config)?;
                self.save_error.set(false);
                if let Some(error) = loaded.write_error {
                    self.error(&error);
                }
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
                if self.rows_changed() {
                    self.rebuild()?;
                } else {
                    self.invalidate_pages();
                    self.refresh_mapping_options();
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
        let original = crate::settings_combo::original_index(control, selected);
        let Some((_, label)) = row.options.get(original.unwrap_or(usize::MAX)) else {
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
        let focus = unsafe { GetFocus() };
        if !self.search.edit.is_null()
            && (focus == self.search.edit || focus == self.search.list)
            && (self.search.visible || !caption(self.search.edit).is_empty())
        {
            return self.search_key(27);
        }
        input::annotation_preview(false)?;
        self.cast.clear();
        self.cast_next = None;
        self.live = false;
        for row in self
            .rows
            .iter()
            .chain(self.pages.values().flat_map(|(rows, _)| rows))
        {
            if matches!(row.field.kind, Kind::Preview) {
                send(row.c(0), BM_SETCHECK, 0, 0);
            }
        }
        self.overlay.guides(0, &self.cfg)?;
        input::set_guides(false)?;
        unsafe {
            SetTimer(self.window.0, 1, 100, None);
        }
        self.tooltip.hide();
        self.overlay.hide_tip();
        self.preview.hide();
        input::track_preview(false)?;
        Ok(())
    }
}
fn shortcut_caption(name: &str, captured: bool, zh: bool) -> String {
    let value = config::vk(name).map_or_else(
        || name.to_owned(),
        |vk| format!("{} · VK:{vk:02X}", config::shortcut_name(name)),
    );
    format!(
        "{value} · {}",
        match (captured, zh) {
            (true, true) => "已录入；再次点击可重录",
            (false, true) => "点击后按一个键录入",
            (true, false) => "Captured; click to recapture",
            (false, false) => "Click, then press one key",
        }
    )
}
fn populate(hwnd: HWND, options: &[(String, String)], selected: &str, height: i32) {
    crate::settings_combo::populate(hwnd, options, selected);
    send(hwnd, CB_SETITEMHEIGHT, usize::MAX, height as isize);
    send(hwnd, CB_SETITEMHEIGHT, 0, height as isize);
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
fn minimum_window_size(dpi: u32, work: (i32, i32)) -> POINT {
    let scale =
        |size: u32| (u64::from(size) * u64::from(dpi.max(96)) / 96).min(i32::MAX as u64) as i32;
    POINT {
        x: scale(780).min(work.0.max(1)),
        y: scale(540).min(work.1.max(1)),
    }
}
fn with_editor(action: impl FnOnce(&mut Editor) -> Result<(), String>) {
    EDITOR.with(|slot| {
        if let Ok(mut guard) = slot.try_borrow_mut()
            && let Some(editor) = guard.as_mut()
        {
            UPDATING.with(|flag| flag.set(true));
            if let Err(error) = action(editor) {
                editor.error(&error);
            }
            UPDATING.with(|flag| flag.set(false));
        }
    });
}
fn settings_command(id: i32, event: u32, sender: HWND, confirm: impl FnOnce(HWND, &str) -> bool) {
    let mut question = None;
    with_editor(|editor| {
        if event == BN_CLICKED
            && id >= BASE
            && (id - BASE) % STRIDE == 0
            && let Some(row) = editor.rows.get(((id - BASE) / STRIDE) as usize)
            && row.c(0) == sender
            && row.field.section == "general"
            && row.field.key == "elevate"
            && editor.doc["general"]["elevate"].as_bool() == Some(false)
            && send(sender, BM_GETCHECK, 0, 0) == 1
        {
            send(sender, BM_SETCHECK, 0, 0);
            question = Some((editor.window.0, editor.cfg.tr(
                "Run WinSizeP as administrator? If ordinary users can modify the program folder, they can replace the program or its dependencies and run code as administrator, including at sign-in when autostart is enabled. Enable administrator mode and accept this risk?",
                "是否以管理员身份运行 WinSizeP？若普通用户可修改程序目录，程序或依赖文件被替换后将以管理员权限执行；启用自启动时，登录后也会自动提权运行。是否启用管理员模式并接受此风险？",
            ).to_owned()));
        }
        Ok(())
    });
    if let Some((owner, text)) = question {
        // Keep the editor unborrowed and the switch off while the modal dialog runs.
        if !confirm(owner, &text) {
            return;
        }
        send(sender, BM_SETCHECK, 1, 0);
    }
    with_editor(|editor| editor.command(id, event, sender));
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
fn forward_search_wheel(root: HWND, wp: WPARAM, lp: LPARAM) -> bool {
    // SAFETY: the UI thread owns the list; Windows supplies signed screen coordinates.
    unsafe {
        let list = GetDlgItem(root, SEARCH_RESULTS);
        let point = POINT {
            x: lp as u16 as i16 as i32,
            y: (lp >> 16) as u16 as i16 as i32,
        };
        let mut rect = RECT::default();
        if !list.is_null()
            && IsWindowVisible(list) != 0
            && GetWindowRect(list, &mut rect) != 0
            && PtInRect(&rect, point) != 0
        {
            SendMessageW(list, WM_MOUSEWHEEL, wp, lp);
            return true;
        }
    }
    false
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
        #[cfg(test)]
        if matches!(message, WM_SIZE | WM_PAINT) {
            let key = if message == WM_SIZE {
                w!("SizeCount")
            } else {
                w!("PaintCount")
            };
            SetPropW(hwnd, key, (GetPropW(hwnd, key) as usize + 1) as HANDLE);
        }
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
            RemovePropW(hwnd, w!("ShortcutCapture"));
            RemovePropW(hwnd, w!("ShortcutZh"));
            RemovePropW(hwnd, w!("OwnerCombo"));
            RemoveWindowSubclass(hwnd, Some(control_proc), 1);
            return DefSubclassProc(hwnd, message, wp, lp);
        }
        let mut class = [0u16; 32];
        let length = GetClassNameW(hwnd, class.as_mut_ptr(), 32);
        let class = String::from_utf16_lossy(&class[..length as usize]);
        let id = GetDlgCtrlID(hwnd);
        if !GetPropW(hwnd, w!("ShortcutCapture")).is_null() {
            let hint = GetDlgItem(GetParent(hwnd), id + 4);
            let zh = !GetPropW(hwnd, w!("ShortcutZh")).is_null();
            if matches!(message, WM_SETFOCUS | WM_LBUTTONDOWN) {
                SetPropW(hwnd, w!("ShortcutCapture"), 1 as HANDLE);
                set_text(
                    hint,
                    if zh {
                        "正在录入：请按一个键"
                    } else {
                        "Recording: press one key"
                    },
                );
            }
            if message == WM_KILLFOCUS {
                set_text(hint, &shortcut_caption(&caption(hwnd), false, zh));
            }
            if message == WM_KEYDOWN || message == WM_SYSKEYDOWN {
                if GetPropW(hwnd, w!("ShortcutCapture")) as usize == 2
                    && !matches!(wp, 9 | 13 | 27)
                    && GetKeyState(0x11) >= 0
                {
                    return 0;
                }
                let key =
                    config::physical_key(wp as u32, (lp as u32 >> 16) & 0xff, lp & (1 << 24) != 0);
                if !config::reserved_key(key) && GetKeyState(0x11) >= 0 {
                    let name = config::shortcut_name(&format!("VK:{key:02X}"));
                    set_text(hwnd, &name);
                    SetPropW(hwnd, w!("ShortcutCapture"), 2 as HANDLE);
                    set_text(hint, &shortcut_caption(&name, true, zh));
                    SendMessageW(hwnd, EM_SETSEL, 0, -1);
                    return 0;
                }
                if config::reserved_key(key) && !matches!(wp,9|13|27|0x10..=0x12|0xa0..=0xa5) {
                    set_text(
                        hint,
                        if zh {
                            "该按键由系统保留，请换一个"
                        } else {
                            "Reserved key; choose another"
                        },
                    );
                    return 0;
                }
            }
            if matches!(message, WM_CHAR | WM_SYSCHAR)
                && !matches!(wp, 9 | 13 | 27)
                && GetKeyState(0x11) >= 0
            {
                return 0;
            }
            if message == WM_GETDLGCODE && lp != 0 {
                let routed = &*(lp as *const MSG);
                let key = config::physical_key(
                    routed.wParam as u32,
                    (routed.lParam as u32 >> 16) & 0xff,
                    routed.lParam & (1 << 24) != 0,
                );
                if !config::reserved_key(key) {
                    return DefSubclassProc(hwnd, message, wp, lp) | DLGC_WANTMESSAGE as isize;
                }
            }
        }
        if id == SEARCH_EDIT && matches!(message, WM_SETFOCUS | WM_KILLFOCUS) {
            InvalidateRect(GetDlgItem(root as HWND, SEARCH_FRAME), null(), 0);
        }
        if matches!(id, SEARCH_EDIT | SEARCH_RESULTS) {
            if message == WM_GETDLGCODE {
                let mut flags =
                    DefSubclassProc(hwnd, message, wp, lp) & !(DLGC_WANTALLKEYS as isize);
                flags |= DLGC_WANTARROWS as isize;
                if lp != 0 && matches!((*(lp as *const MSG)).wParam, 13 | 27 | 38 | 40) {
                    flags |= DLGC_WANTMESSAGE as isize;
                }
                return flags;
            }
            if message == WM_KEYDOWN && matches!(wp, 13 | 27 | 38 | 40) {
                let composing = id == SEARCH_EDIT
                    && EDITOR.with(|slot| {
                        slot.try_borrow()
                            .ok()
                            .and_then(|e| e.as_ref().map(|e| e.search.composing))
                            .unwrap_or(false)
                    });
                if composing {
                    return DefSubclassProc(hwnd, message, wp, lp);
                }
                PostMessageW(root as HWND, SEARCH_KEY, wp, 0);
                return 0;
            }
            if message == WM_CHAR && matches!(wp, 13 | 27) {
                return 0;
            }
            if id == SEARCH_EDIT && message == WM_IME_STARTCOMPOSITION {
                with_editor(|e| {
                    e.search.composing = true;
                    KillTimer(e.window.0, SEARCH_TIMER);
                    Ok(())
                });
            }
            if id == SEARCH_EDIT && message == WM_IME_ENDCOMPOSITION {
                let result = DefSubclassProc(hwnd, message, wp, lp);
                with_editor(|e| {
                    e.search.composing = false;
                    e.search.dismissed = false;
                    SetTimer(e.window.0, SEARCH_TIMER, 150, None);
                    Ok(())
                });
                return result;
            }
        }
        if class.eq_ignore_ascii_case("button")
            && message == WM_GETDLGCODE
            && lp != 0
            && (*(lp as *const MSG)).wParam == 13
        {
            return DefSubclassProc(hwnd, message, wp, lp) | DLGC_WANTMESSAGE as isize;
        }
        if class.eq_ignore_ascii_case("button") && message == WM_KEYDOWN && wp == 13 {
            PostMessageW(hwnd, BM_CLICK, 0, 0);
            return 0;
        }
        if class.eq_ignore_ascii_case("edit")
            && id != SEARCH_EDIT
            && matches!(message, WM_SIZE | EDIT_LAYOUT)
        {
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
            send(hwnd, EM_SETRECTNP, 0, (&rect as *const RECT) as isize);
            RedrawWindow(
                hwnd,
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_ERASE | RDW_FRAME,
            );
            return result;
        }
        if class.eq_ignore_ascii_case("edit") && message == WM_GETDLGCODE {
            // Every edit is multiline (for vertical centering) and would therefore claim
            // all keys: Tab could not leave the field and Esc would close its parent
            // pane. Leave navigation to the dialog manager and keep only Enter, which
            // commits the field.
            let mut code = DefSubclassProc(hwnd, message, wp, lp) & !(DLGC_WANTALLKEYS as isize);
            if lp != 0 {
                // SAFETY: a nonzero parameter is the message being routed.
                let routed = &*(lp as *const MSG);
                if routed.wParam == 13 && matches!(routed.message, WM_KEYDOWN | WM_CHAR) {
                    code |= DLGC_WANTMESSAGE as isize;
                }
            }
            return code;
        }
        if class.eq_ignore_ascii_case("edit") && message == WM_CHAR && wp == 13 {
            PostMessageW(
                GetParent(hwnd),
                WM_COMMAND,
                (GetDlgCtrlID(hwnd) as usize & 0xffff) | ((EDIT_COMMIT as usize) << 16),
                hwnd as isize,
            );
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
            if id == SEARCH_RESULTS {
                return DefSubclassProc(hwnd, message, wp, lp);
            }
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
            PostMessageW(root as HWND, WM_MOUSEWHEEL, wp, lp);
            return 0;
        }
        if id != SEARCH_RESULTS
            && class.eq_ignore_ascii_case("listbox")
            && GetPropW(hwnd, w!("OwnerCombo")).is_null()
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
                    // Commit item identity before CLOSEUP restores the full list.
                    // Combo notifications are synchronous; the editor runs later.
                    if crate::settings_combo::notification(lp as HWND, (wp >> 16) as u32) {
                        return 0;
                    }
                    PostMessageW(root, WM_COMMAND, wp, lp);
                }
                0
            }
            WM_DRAWITEM | WM_MEASUREITEM => SendMessageW(root, message, wp, lp),
            // Only the main window closes; a pane must outlive its controls.
            WM_CLOSE => 0,
            WM_VSCROLL => {
                PostMessageW(root, WM_VSCROLL, wp, hwnd as isize);
                0
            }
            WM_MOUSEWHEEL => {
                PostMessageW(root, WM_MOUSEWHEEL, wp, lp);
                0
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORBTN => {
                let id = GetDlgCtrlID(lp as HWND);
                let hint = id >= BASE && (id - BASE) % STRIDE == 2;
                crate::settings_theme::color(
                    wp as HDC,
                    hint,
                    matches!(message, WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX),
                )
            }
            WM_ERASEBKGND => {
                let mut rect = RECT::default();
                GetClientRect(hwnd, &mut rect);
                FillRect(wp as HDC, &rect, crate::settings_theme::surface());
                if !GetPropW(hwnd, w!("SearchMatch")).is_null() {
                    let coord = |name| GetPropW(hwnd, name) as usize as i32;
                    let region = CreateRoundRectRgn(
                        coord(w!("SearchLeft")),
                        coord(w!("SearchTop")),
                        coord(w!("SearchRight")),
                        coord(w!("SearchBottom")),
                        8,
                        8,
                    );
                    if !region.is_null() {
                        SetDCBrushColor(wp as HDC, GetSysColor(COLOR_HIGHLIGHT));
                        let width = (2 * GetDpiForWindow(hwnd).max(96) / 96) as i32;
                        FrameRgn(wp as HDC, region, GetStockObject(DC_BRUSH), width, width);
                        DeleteObject(region);
                    }
                }
                1
            }
            _ => DefWindowProcW(hwnd, message, wp, lp),
        }
    }
}
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match message {
        WM_MOUSEWHEEL => {
            if !forward_search_wheel(hwnd, wp, lp) {
                with_editor(|e| {
                    let mut point = POINT {
                        x: lp as u16 as i16 as i32,
                        y: (lp >> 16) as u16 as i16 as i32,
                    };
                    let mut client = RECT::default();
                    let mut sidebar = RECT::default();
                    // Hit-test the pointer, since the focused control may be in the search bar.
                    unsafe {
                        GetWindowRect(e.panes[0], &mut sidebar);
                        let pane = usize::from(PtInRect(&sidebar, point) == 0);
                        ScreenToClient(hwnd, &mut point);
                        GetClientRect(hwnd, &mut client);
                        if PtInRect(&client, point) != 0 {
                            let delta = (wp >> 16) as u16 as i16 as i32;
                            e.scroll_by(pane, u16::MAX, -delta * 120 / WHEEL_DELTA as i32)?;
                        }
                    }
                    Ok(())
                });
            }
            0
        }
        #[cfg(test)]
        TEST_THEME => {
            crate::settings_theme::test_dark(match wp {
                1 => Some(false),
                2 => Some(true),
                _ => None,
            });
            unsafe { window_proc(hwnd, WM_THEMECHANGED, 0, 0) }
        }
        #[cfg(test)]
        PREVIEW_STATE => EDITOR.with(|slot| {
            slot.try_borrow()
                .ok()
                .and_then(|guard| {
                    guard
                        .as_ref()
                        .map(|editor| if editor.preview.visible() { 1 } else { 0 })
                })
                .unwrap_or(-1)
        }),
        WM_SIZE => {
            if wp == SIZE_MINIMIZED as usize {
                return 0;
            }
            unsafe {
                if GetPropW(hwnd, w!("ResizeQueued")).is_null() {
                    SetPropW(hwnd, w!("ResizeQueued"), 1usize as HANDLE);
                }
            }
            with_editor(|e| e.resize());
            // Nested resize callbacks cannot borrow Editor; finish them after the
            // current operation. Ordinary live resizing is laid out synchronously.
            if !unsafe { GetPropW(hwnd, w!("ResizeQueued")) }.is_null() {
                unsafe {
                    PostMessageW(hwnd, RESIZE, 0, 0);
                }
            }
            0
        }
        RESIZE => {
            if !unsafe { GetPropW(hwnd, w!("ResizeQueued")) }.is_null() {
                with_editor(|e| e.resize());
            }
            0
        }
        WM_EXITSIZEMOVE => {
            with_editor(|e| e.resize());
            0
        }
        WM_WINDOWPOSCHANGING => unsafe {
            let result = DefWindowProcW(hwnd, message, wp, lp);
            if lp != 0 {
                let position = &mut *(lp as *mut WINDOWPOS);
                let mut current = RECT::default();
                // MoveWindow and some drag paths pass the unchanged size without
                // SWP_NOSIZE. Retain pixels unless the dimensions actually change.
                if position.flags & SWP_NOSIZE == 0
                    && GetWindowRect(hwnd, &mut current) != 0
                    && (position.cx, position.cy)
                        != (current.right - current.left, current.bottom - current.top)
                {
                    position.flags |= SWP_NOCOPYBITS;
                }
            }
            result
        },
        WM_GETMINMAXINFO => {
            unsafe {
                let info = &mut *(lp as *mut MINMAXINFO);
                let dpi = GetDpiForWindow(hwnd).max(96);
                let mut monitor = MONITORINFO {
                    cbSize: size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetMonitorInfoW(
                    MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
                    &mut monitor,
                ) != 0
                {
                    info.ptMinTrackSize = minimum_window_size(
                        dpi,
                        (
                            monitor.rcWork.right - monitor.rcWork.left,
                            monitor.rcWork.bottom - monitor.rcWork.top,
                        ),
                    );
                }
            }
            0
        }
        WM_DPICHANGED | WM_SETTINGCHANGE | WM_THEMECHANGED | WM_SYSCOLORCHANGE => {
            crate::settings_theme::refresh();
            crate::settings_theme::apply(hwnd);
            unsafe {
                let mut child = GetWindow(hwnd, GW_CHILD);
                while !child.is_null() {
                    crate::settings_theme::apply(child);
                    let mut control = GetWindow(child, GW_CHILD);
                    while !control.is_null() {
                        crate::settings_theme::apply(control);
                        control = GetWindow(control, GW_HWNDNEXT);
                    }
                    child = GetWindow(child, GW_HWNDNEXT);
                }
                RedrawWindow(
                    hwnd,
                    null(),
                    null_mut(),
                    RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_FRAME,
                );
            }
            input::display_changed();
            // Modal dialogs can reenter while Editor is borrowed. Copy the suggested
            // rectangle now: lParam is only valid during this callback.
            DISPLAY_UPDATE.with(|flag| flag.set(true));
            if message == WM_DPICHANGED && lp != 0 {
                DPI_RECT.with(|rect| rect.set(Some(unsafe { *(lp as *const RECT) })));
            }
            with_editor(|e| e.refresh_display());
            0
        }
        WM_COMMAND => {
            if (wp & 0xffff) == 600 && (wp >> 16) == STN_CLICKED as usize {
                unsafe {
                    MessageBoxW(
                        hwnd,
                        wide(caption(GetDlgItem(hwnd, 600))).as_ptr(),
                        w!("WinSizeP"),
                        MB_OK | MB_ICONINFORMATION,
                    );
                }
                return 0;
            }
            settings_command(
                (wp & 0xffff) as i32,
                (wp >> 16) as u32,
                lp as HWND,
                |owner, text| {
                    // SAFETY: the owner and UTF-16 text remain valid during MessageBoxW.
                    unsafe {
                        MessageBoxW(
                            owner,
                            wide(text).as_ptr(),
                            w!("WinSizeP"),
                            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
                        ) == IDYES
                    }
                },
            );
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
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORBTN => unsafe {
            crate::settings_theme::color(
                wp as HDC,
                false,
                GetDlgCtrlID(lp as HWND) == 600
                    || matches!(message, WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX),
            )
        },
        WM_ERASEBKGND => unsafe {
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect);
            FillRect(wp as HDC, &rect, crate::settings_theme::surface());
            1
        },
        WM_TIMER => {
            with_editor(|e| {
                if wp == SEARCH_TIMER {
                    unsafe {
                        KillTimer(hwnd, SEARCH_TIMER);
                    }
                    // KillTimer cannot remove a timer message already in the queue.
                    if !e.search.dismissed && unsafe { GetFocus() } == e.search.edit {
                        e.run_search()
                    } else {
                        Ok(())
                    }
                } else if wp == SAVE_STATUS_TIMER {
                    e.show_pending_status(Instant::now());
                    Ok(())
                } else {
                    e.tick()
                }
            });
            0
        }
        SEARCH_KEY => {
            with_editor(|e| e.search_key(wp as u32));
            0
        }
        SEARCH_FOCUS => {
            with_editor(|e| {
                unsafe {
                    SetFocus(e.search.edit);
                }
                Ok(())
            });
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
            let mut question = None;
            with_editor(|e| {
                match e.save() {
                    Ok(()) => unsafe { PostQuitMessage(0) },
                    Err(error) => {
                        question = Some(format!(
                            "{error}\n\n{}",
                            e.cfg.tr(
                                "Close and discard the unsaved changes?",
                                "是否放弃未保存的修改并关闭？"
                            )
                        ))
                    }
                }
                Ok(())
            });
            // The editor is no longer borrowed here: the message box pumps messages.
            if let Some(question) = question
                && unsafe {
                    MessageBoxW(
                        hwnd,
                        wide(&question).as_ptr(),
                        w!("WinSizeP"),
                        MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
                    )
                } == IDYES
            {
                unsafe {
                    PostQuitMessage(0);
                }
            }
            0
        }
        WM_ENDSESSION => {
            if wp != 0 {
                with_editor(|e| e.save());
            }
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
            if handled && !accepted {
                // The caller refuses to exit; show where the unsaved field is.
                unsafe {
                    ShowWindowAsync(hwnd, SW_RESTORE);
                    SetForegroundWindow(hwnd);
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
        let mutex = CreateMutexW(null(), 0, crate::identity::mutex(true).as_ptr());
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
                PostMessageW(hwnd, SEARCH_FOCUS, 0, 0);
            }
            return Ok(());
        }
        let loaded = config::load_snapshot(path, None)?;
        let write_error = loaded.write_error;
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
                hbrBackground: null_mut(),
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
        let window = Window(hwnd);
        crate::settings_theme::refresh();
        crate::settings_theme::apply(hwnd);
        desktop::check(SetPropW(hwnd, w!("PreviewSuspended"), 1usize as HANDLE))?;
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
                WS_CHILD
                    | WS_VISIBLE
                    | WS_CLIPCHILDREN
                    | WS_CLIPSIBLINGS
                    | if index == 1 { WS_VSCROLL } else { 0 },
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
            crate::settings_theme::apply(*pane);
        }
        let ui_font = Font::new(
            &cfg.font,
            cfg.font_size,
            dpi,
            cfg.font_bold,
            cfg.font_italic,
        )?;
        let ui_hint = Font::new(
            &cfg.font,
            (cfg.font_size * 3 / 4).max(6),
            dpi,
            false,
            cfg.font_italic,
        )?;
        let mut editor = Editor {
            search: Search::default(),
            cast: crate::cast::Visuals::new(),
            cast_next: None,
            window,
            panes,
            tabs: Vec::new(),
            status: null_mut(),
            status_until: Cell::new(None),
            layout_size: None,
            pending_status: RefCell::new(None),
            save_error: Cell::new(false),
            last_error: RefCell::new(None),
            axis_controls: [null_mut(); 4],
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
            font: ui_font,
            hint_font: ui_hint,
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
                "BUTTON",
                editor.cfg.tr(en, zh),
                BS_OWNERDRAW as u32 | WS_TABSTOP,
                TAB_BASE + index as i32,
            )?);
        }
        let divider = editor.create(panes[0], "STATIC", "", SS_OWNERDRAW, 99)?;
        SetPropW(divider, w!("Separator"), 1usize as HANDLE);
        editor.status = editor.create(hwnd, "STATIC", "", SS_OWNERDRAW | SS_NOTIFY, 600)?;
        editor.init_search()?;
        editor.axis_controls[0] = editor.create(
            panes[1],
            "BUTTON",
            "",
            BS_AUTORADIOBUTTON as u32 | BS_MULTILINE as u32 | WS_TABSTOP | WS_GROUP,
            AXIS_H,
        )?;
        editor.axis_controls[1] = editor.create(
            panes[1],
            "BUTTON",
            "",
            BS_AUTORADIOBUTTON as u32 | BS_MULTILINE as u32 | WS_TABSTOP,
            AXIS_V,
        )?;
        editor.axis_controls[2] = editor.create(
            panes[1],
            "BUTTON",
            "",
            BS_AUTORADIOBUTTON as u32 | BS_MULTILINE as u32 | WS_TABSTOP | WS_GROUP,
            AXIS_BOTH,
        )?;
        editor.axis_controls[3] = editor.create(
            panes[1],
            "BUTTON",
            "",
            BS_AUTORADIOBUTTON as u32 | BS_MULTILINE as u32 | WS_TABSTOP,
            AXIS_CUSTOM,
        )?;
        if !cfg!(test) && !crate::resident::running() {
            crate::launch(editor.cfg.elevate)?;
        }
        let _hooks = input::Hooks::escape_only()?;
        let timer = editor.clock.handle();
        EDITOR.with_borrow_mut(|slot| *slot = Some(editor));
        with_editor(|e| e.rebuild());
        if let Some(error) = write_error {
            with_editor(|e| {
                e.error(&error);
                Ok(())
            });
        }
        SetTimer(hwnd, 1, 100, None);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        with_editor(|editor| {
            SetFocus(editor.search.edit);
            Ok(())
        });
        let result = message_loop(hwnd, timer);
        KillTimer(hwnd, 1);
        KillTimer(hwnd, SEARCH_TIMER);
        EDITOR.with_borrow_mut(|slot| {
            slot.take();
        });
        result
    }
}
fn message_loop(hwnd: HWND, timer: HANDLE) -> Result<(), String> {
    unsafe {
        loop {
            for _ in 0..64 {
                let Some(msg) = input::next_event() else {
                    break;
                };
                match msg.message {
                    input::RECOVERED => with_editor(|e| {
                        e.cast.clear();
                        e.preview.hide();
                        e.overlay.hide_tip();
                        Ok(())
                    }),
                    input::TIP_SCROLL => with_editor(|e| {
                        e.overlay.scroll_tip(msg.wParam as i32);
                        Ok(())
                    }),
                    input::DISPLAY_CHANGED => with_editor(|e| {
                        e.cfg.mousecast.color = crate::colors::accent();
                        e.cfg.mousecast.dark = crate::overlay::dark();
                        e.overlay.follow(desktop::cursor()?, &e.cfg)?;
                        e.preview.tick(&e.cfg)?;
                        e.cast.tick(&e.cfg)
                    }),
                    input::MOVE => with_editor(|e| e.moved(input::moved())),
                    input::CANCEL => with_editor(|e| e.escape()),
                    _ => {}
                }
            }
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
    include!("settings_search_tests.rs");
    #[test]
    #[ignore = "uses a temporary settings window to verify search focus, keyboard input and navigation"]
    fn search_navigation_preserves_focus_values_and_fixed_header() {
        let _dpi = crate::test_support::Dpi::new();
        let _hooks = input::Hooks::escape_only().unwrap();
        let editor = test_editor(0, "settings-search-ui-test");
        let original = editor.original.clone();
        let hwnd = editor.window.0;
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, window_proc as *const () as isize);
        }
        EDITOR.with_borrow_mut(|slot| *slot = Some(editor));
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                EDITOR.with_borrow_mut(|slot| {
                    slot.take();
                });
            }
        }
        let _reset = Reset;
        with_editor(|e| {
            e.init_search()?;
            e.layout()
        });
        let edit = EDITOR.with_borrow(|slot| slot.as_ref().unwrap().search.edit);
        let list = EDITOR.with_borrow(|slot| slot.as_ref().unwrap().search.list);
        unsafe {
            ShowWindow(hwnd, SW_SHOW);
            SetForegroundWindow(hwnd);
            PostMessageW(hwnd, SEARCH_FOCUS, 0, 0);
        }
        crate::test_support::pump();
        assert_eq!(unsafe { GetFocus() }, edit);
        let frame = EDITOR.with_borrow(|slot| slot.as_ref().unwrap().search.frame);
        let mut outer = RECT::default();
        let mut inner = RECT::default();
        unsafe {
            GetWindowRect(frame, &mut outer);
            GetWindowRect(edit, &mut inner);
        }
        assert!(((outer.top + outer.bottom) - (inner.top + inner.bottom)).abs() <= 1);
        fn wait(query: &str) {
            let until = Instant::now() + Duration::from_secs(3);
            loop {
                crate::test_support::pump();
                let ready = EDITOR.with_borrow(|slot| {
                    slot.as_ref()
                        .is_some_and(|e| e.search.query == query && e.search.visible)
                });
                if ready {
                    return;
                }
                assert!(Instant::now() < until, "Search timer did not run: {query}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        set_text(edit, "q");
        std::thread::sleep(Duration::from_millis(40));
        crate::test_support::pump();
        EDITOR.with_borrow(|slot| assert!(slot.as_ref().unwrap().search.query.is_empty()));
        wait("q");
        EDITOR.with_borrow(|slot| {
            let e = slot.as_ref().unwrap();
            assert_eq!(e.tab, 4);
            assert!(matches!(e.search.highlight, Some(crate::settings_search::Target::Field { ref section, ref key, .. }) if section == "keys" && key == "Q"));
            assert!(!unsafe { GetPropW(e.panes[1], w!("SearchMatch")) }.is_null());
        });
        assert_eq!(unsafe { GetFocus() }, edit);
        unsafe {
            SendMessageW(edit, WM_KEYDOWN, 40, 0);
        }
        crate::test_support::pump();
        EDITOR.with_borrow(|slot| assert_eq!(slot.as_ref().unwrap().search.selected, 1));
        unsafe {
            SetFocus(list);
            SendMessageW(list, WM_KEYDOWN, 38, 0);
        }
        crate::test_support::pump();
        EDITOR.with_borrow(|slot| assert_eq!(slot.as_ref().unwrap().search.selected, 0));
        unsafe {
            SendMessageW(edit, WM_KEYDOWN, 13, 0);
        }
        crate::test_support::pump();
        EDITOR.with_borrow(|slot| {
            let e = slot.as_ref().unwrap();
            assert!(!e.search.visible);
            assert_eq!(
                unsafe { GetFocus() },
                e.search_control(e.search.highlight.as_ref().unwrap())
            );
            assert_eq!(e.original, original);
            assert!(e.dirty.is_empty());
        });
        let mut before = RECT::default();
        unsafe {
            GetWindowRect(edit, &mut before);
        }
        with_editor(|e| e.scroll_by(1, u16::MAX, 500));
        let mut after = RECT::default();
        unsafe {
            GetWindowRect(edit, &mut after);
        }
        assert_eq!(
            (before.left, before.top, before.right, before.bottom),
            (after.left, after.top, after.right, after.bottom)
        );
        unsafe {
            SetFocus(edit);
            SendMessageW(edit, WM_KEYDOWN, 40, 0);
        }
        crate::test_support::pump();
        unsafe {
            SetFocus(list);
            SendMessageW(list, WM_KEYDOWN, 27, 0);
        }
        crate::test_support::pump();
        assert_eq!(unsafe { GetFocus() }, edit);
        EDITOR.with_borrow(|slot| assert!(!slot.as_ref().unwrap().search.visible));
        assert_eq!(caption(edit), "q");
        unsafe {
            SendMessageW(edit, WM_KEYDOWN, 27, 0);
        }
        crate::test_support::pump();
        assert!(caption(edit).is_empty());
        unsafe {
            SendMessageW(edit, WM_IME_STARTCOMPOSITION, 0, 0);
        }
        set_text(edit, "准星");
        std::thread::sleep(Duration::from_millis(200));
        crate::test_support::pump();
        EDITOR.with_borrow(|slot| assert!(slot.as_ref().unwrap().search.query.is_empty()));
        unsafe {
            SendMessageW(edit, WM_IME_ENDCOMPOSITION, 0, 0);
        }
        wait("准星");
        EDITOR.with_borrow(|slot| assert_eq!(slot.as_ref().unwrap().tab, 5));
        assert!(send(list, LB_GETCOUNT, 0, 0) > 6);
        let selection = send(list, LB_GETCURSEL, 0, 0);
        let scroll = EDITOR.with_borrow(|slot| slot.as_ref().unwrap().scroll);
        let top = send(list, LB_GETTOPINDEX, 0, 0);
        send(list, WM_MOUSEWHEEL, (-120i16 as u16 as usize) << 16, 0);
        crate::test_support::pump();
        assert!(send(list, LB_GETTOPINDEX, 0, 0) > top);
        assert_eq!(send(list, LB_GETCURSEL, 0, 0), selection);
        assert_eq!(unsafe { GetFocus() }, edit);
        EDITOR.with_borrow(|slot| {
            let e = slot.as_ref().unwrap();
            assert_eq!(e.tab, 5);
            assert_eq!(e.scroll, scroll);
            assert_eq!(e.original, original);
            assert!(e.dirty.is_empty());
        });
        send(list, WM_MOUSEWHEEL, (120usize) << 16, 0);
        assert_eq!(send(list, LB_GETTOPINDEX, 0, 0), top);
        let mut list_rect = RECT::default();
        unsafe {
            GetWindowRect(list, &mut list_rect);
        }
        let point =
            (((list_rect.top + 10) as u16 as usize) << 16) | (list_rect.left + 10) as u16 as usize;
        send(
            edit,
            WM_MOUSEWHEEL,
            (-120i16 as u16 as usize) << 16,
            point as isize,
        );
        crate::test_support::pump();
        assert!(send(list, LB_GETTOPINDEX, 0, 0) > top);
        assert_eq!(send(list, LB_GETCURSEL, 0, 0), selection);
        assert_eq!(unsafe { GetFocus() }, edit);
        EDITOR.with_borrow(|slot| assert_eq!(slot.as_ref().unwrap().scroll, scroll));
        send(hwnd, WM_MOUSEWHEEL, (120usize) << 16, point as isize);
        assert_eq!(send(list, LB_GETTOPINDEX, 0, 0), top);
        send(list, WM_LBUTTONDOWN, 1, (10 << 16) | 10);
        assert!(unsafe { GetPropW(list, w!("DragIndex")) }.is_null());
        send(list, WM_LBUTTONUP, 0, (10 << 16) | 10);
        crate::test_support::pump();
        set_text(edit, "不存在的设置关键词");
        wait("不存在的设置关键词");
        EDITOR.with_borrow(|slot| {
            let e = slot.as_ref().unwrap();
            assert!(e.search.matches.is_empty());
            assert_eq!(e.original, original);
        });
        send(
            edit,
            WM_MOUSEWHEEL,
            (-120i16 as u16 as usize) << 16,
            point as isize,
        );
        crate::test_support::pump();
        assert_eq!(send(list, LB_GETTOPINDEX, 0, 0), 0);
        with_editor(|e| {
            e.close_search(true)?;
            e.switch_page(0, 0)?;
            let at = e
                .rows
                .iter()
                .position(|row| row.field.section == "topmost_marker" && row.field.key == "enabled")
                .unwrap();
            let control = e.rows[at].c(0);
            send(control, BM_SETCHECK, 0, 0);
            e.command(BASE + at as i32 * STRIDE, BN_CLICKED, control)?;
            e.save()
        });
        set_text(edit, "20");
        wait("20");
        unsafe {
            SendMessageW(edit, WM_KEYDOWN, 13, 0);
        }
        crate::test_support::pump();
        EDITOR.with_borrow(|slot| {
            let e = slot.as_ref().unwrap();
            assert!(!e.cfg.topmost_marker.enabled);
            let enabler = e
                .rows
                .iter()
                .find(|row| row.field.section == "topmost_marker" && row.field.key == "enabled")
                .unwrap()
                .c(0);
            assert_eq!(unsafe { GetFocus() }, enabler);
        });
    }

    #[test]
    #[ignore = "uses hidden settings controls and an isolated configuration file"]
    fn topmost_marker_choices_save_as_numbers_and_follow_the_enable_switch() {
        let mut editor = test_editor(0, "settings-topmost-marker-test");
        let row = |editor: &Editor, key: &str| {
            editor
                .rows
                .iter()
                .position(|row| row.field.section == "topmost_marker" && row.field.key == key)
                .unwrap()
        };
        let size = row(&editor, "size");
        let position = row(&editor, "position");
        let enabled = row(&editor, "enabled");
        assert_eq!(
            editor.rows[size]
                .options
                .iter()
                .map(|option| option.0.as_str())
                .collect::<Vec<_>>(),
            ["16", "20", "24", "32"]
        );
        assert_eq!(
            editor.rows[position]
                .options
                .iter()
                .map(|option| option.0.as_str())
                .collect::<Vec<_>>(),
            ["top-left", "bottom-left", "bottom-right"]
        );
        assert_eq!(Editor::selected_value(&editor.rows[size], 0), "20");
        assert_eq!(
            Editor::selected_value(&editor.rows[position], 0),
            "top-left"
        );
        for (index, choice) in [(size, 3), (position, 2)] {
            let control = editor.rows[index].c(0);
            send(control, CB_SETCURSEL, choice, 0);
            editor
                .command(
                    BASE + index as i32 * STRIDE,
                    crate::settings_combo::COMMIT,
                    control,
                )
                .unwrap();
        }
        editor.save().unwrap();
        assert_eq!(editor.doc["topmost_marker"]["size"].as_integer(), Some(32));
        assert_eq!(editor.cfg.topmost_marker.size, 32);
        assert_eq!(editor.cfg.topmost_marker.position, "bottom-right");
        for active in [false, true] {
            let control = editor.rows[enabled].c(0);
            send(control, BM_SETCHECK, usize::from(active), 0);
            editor
                .command(BASE + enabled as i32 * STRIDE, BN_CLICKED, control)
                .unwrap();
            assert_eq!(
                unsafe { IsWindowEnabled(editor.rows[size].c(0)) != 0 },
                active
            );
            assert_eq!(
                unsafe { IsWindowEnabled(editor.rows[position].c(0)) != 0 },
                active
            );
            editor.save().unwrap();
            assert_eq!(editor.cfg.topmost_marker.enabled, active);
            assert_eq!(editor.cfg.topmost_marker.size, 32);
        }
        editor.doc["topmost_marker"]["size"] = value(16);
        editor.update_values();
        assert_eq!(Editor::selected_value(&editor.rows[size], 0), "16");
    }

    #[test]
    #[ignore = "uses hidden settings controls and an isolated configuration file"]
    fn administrator_confirmation_only_applies_to_user_enablement() {
        let editor = test_editor(0, "settings-elevation-confirm-test");
        let index = editor
            .rows
            .iter()
            .position(|row| row.field.section == "general" && row.field.key == "elevate")
            .unwrap();
        let control = editor.rows[index].c(0);
        let id = BASE + index as i32 * STRIDE;
        EDITOR.with_borrow_mut(|slot| *slot = Some(editor));
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                EDITOR.with_borrow_mut(|slot| {
                    slot.take();
                });
            }
        }
        let _reset = Reset;
        send(control, BM_SETCHECK, 1, 0);
        settings_command(id, BN_CLICKED, control, |_, text| {
            assert!(text.contains("管理员") || text.contains("administrator"));
            assert_eq!(send(control, BM_GETCHECK, 0, 0), 0);
            EDITOR.with_borrow_mut(|slot| {
                let editor = slot.as_mut().unwrap();
                editor.save().unwrap();
                assert!(!editor.cfg.elevate);
            });
            false
        });
        EDITOR.with_borrow(|slot| {
            let editor = slot.as_ref().unwrap();
            assert!(!editor.cfg.elevate);
            assert!(editor.dirty.is_empty());
            assert_eq!(editor.doc["general"]["elevate"].as_bool(), Some(false));
        });
        assert_eq!(send(control, BM_GETCHECK, 0, 0), 0);
        send(control, BM_SETCHECK, 1, 0);
        settings_command(id, BN_CLICKED, control, |_, _| true);
        EDITOR.with_borrow_mut(|slot| {
            let editor = slot.as_mut().unwrap();
            assert!(editor.cfg.elevate);
            editor.save().unwrap();
            assert_eq!(editor.doc["general"]["elevate"].as_bool(), Some(true));
        });
        settings_command(id, BN_CLICKED, control, |_, _| panic!("Already enabled"));
        send(control, BM_SETCHECK, 0, 0);
        settings_command(id, BN_CLICKED, control, |_, _| panic!("Disabling"));
        EDITOR.with_borrow_mut(|slot| slot.as_mut().unwrap().save().unwrap());
        settings_command(id, EN_CHANGE, control, |_, _| panic!("Not a user toggle"));
    }

    fn test_editor(tab: usize, name: &str) -> Editor {
        let cfg = crate::test_support::cfg();
        let original = config::help().to_owned();
        let doc = original.parse::<DocumentMut>().unwrap();
        let folder = std::env::current_dir()
            .unwrap()
            .join(format!("target/{name}"));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("config.toml");
        std::fs::write(&path, &original).unwrap();
        // SAFETY: the owned hidden parent keeps these isolated test controls alive.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                w!("STATIC"),
                w!("Preview timing test"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                900,
                700,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            )
        };
        assert!(!hwnd.is_null());
        let mut editor = Editor {
            search: Search::default(),
            cast: crate::cast::Visuals::new(),
            cast_next: None,
            window: Window(hwnd),
            panes: [hwnd; 2],
            tabs: Vec::new(),
            status: null_mut(),
            status_until: Cell::new(None),
            layout_size: None,
            pending_status: RefCell::new(None),
            save_error: Cell::new(false),
            last_error: RefCell::new(None),
            axis_controls: [null_mut(); 4],
            path,
            original,
            doc,
            clock: FrameClock::new(cfg.fps).unwrap(),
            cfg,
            tab,
            axis: 0,
            rows: Vec::new(),
            pages: HashMap::new(),
            scroll: [0; 2],
            extent: [0; 2],
            dpi: 96,
            font: Font::new("SystemFont", 12, 96, false, false).unwrap(),
            hint_font: Font::new("SystemFont", 9, 96, false, false).unwrap(),
            families: Vec::new(),
            live: true,
            selected: None,
            pending: None,
            dirty: HashSet::new(),
            disk_check: Instant::now(),
            overlay: Overlay::new().unwrap(),
            preview: Preview::new().unwrap(),
            tooltip: HoverTip::new(hwnd).unwrap(),
            active_combo: None,
        };
        editor.status = editor.create(hwnd, "STATIC", "", 0, 600).unwrap();
        editor.panes = [
            editor.create(hwnd, "STATIC", "", 0, 10).unwrap(),
            editor.create(hwnd, "STATIC", "", 0, 11).unwrap(),
        ];
        editor.rebuild().unwrap();
        editor
    }
    #[test]
    #[ignore = "shows a keyboard sample and writes only target/settings-preview-test/config.toml"]
    fn saving_preserves_preview_unless_merged_settings_change() {
        let _hooks = input::Hooks::escape_only().unwrap();
        let mut editor = test_editor(8, "settings-preview-test");
        let index = editor
            .rows
            .iter()
            .position(|row| row.field.section == "keycast" && row.field.key == "duration_ms")
            .unwrap();
        let edit = editor.rows[index].c(0);
        let id = BASE + index as i32 * STRIDE;
        set_text(edit, "1600");
        editor.command(id, EN_CHANGE, edit).unwrap();
        let first = editor.cast_next.unwrap();
        editor.save().unwrap();
        assert_eq!(editor.cfg.keycast.duration, 1600);
        assert_eq!(editor.cast_next, Some(first));
        assert!(editor.pending.is_none());

        set_text(edit, "1700");
        editor.command(id, EN_CHANGE, edit).unwrap();
        let before_merge = editor.cast_next.unwrap();
        let mut disk = editor.doc.clone();
        disk["keycast"]["duration_ms"] = value(1800);
        std::fs::write(&editor.path, disk.to_string()).unwrap();
        editor.save().unwrap();
        assert_eq!(editor.cfg.keycast.duration, 1800);
        assert!(editor.cast_next.unwrap() > before_merge);
        editor.cast.clear();
        unsafe {
            KillTimer(editor.window.0, 1);
        }
    }
    #[test]
    fn minimum_window_fits_small_high_dpi_work_areas() {
        for (dpi, work) in [
            (144, (1366, 728)),
            (192, (1366, 728)),
            (288, (800, 560)),
            (96, (1920, 1040)),
        ] {
            let size = super::minimum_window_size(dpi, work);
            assert!(size.x > 0 && size.x <= work.0);
            assert!(size.y > 0 && size.y <= work.1);
        }
        let ordinary = super::minimum_window_size(96, (1920, 1040));
        assert_eq!((ordinary.x, ordinary.y), (780, 540));
    }
    #[test]
    #[ignore = "uses temporary settings controls and isolated configuration files"]
    fn merged_documents_refresh_rows_options_and_preserve_focus() {
        let _hooks = input::Hooks::escape_only().unwrap();
        let mut editor = test_editor(2, "settings-merge-test");
        let index = editor
            .rows
            .iter()
            .position(|row| row.field.section == "presets" && row.field.key == "5/9")
            .unwrap();
        let focused = editor.rows[index].c(0);
        set_text(focused, "20sw|20sh|60sw|60sh");
        editor.dirty.insert(index);
        unsafe {
            ShowWindow(editor.window.0, SW_SHOW);
            SetFocus(focused);
        }
        send(focused, EM_SETSEL, 2, 5);
        let mut disk = editor.doc.clone();
        disk["presets"].as_table_mut().unwrap().remove("1/9");
        std::fs::write(&editor.path, disk.to_string()).unwrap();
        editor.save().unwrap();
        assert!(
            !editor
                .rows
                .iter()
                .any(|row| row.field.section == "presets" && row.field.key == "1/9")
        );
        let center = editor
            .rows
            .iter()
            .find(|row| row.field.section == "presets" && row.field.key == "5/9")
            .unwrap();
        assert_eq!(unsafe { GetFocus() }, center.c(0));
        assert_eq!(send(center.c(0), EM_GETSEL, 0, 0), 2 | (5 << 16));
        assert_eq!(caption(center.c(0)), "20sw|20sh|60sw|60sh");

        editor.tab = 4;
        editor.rebuild().unwrap();
        let index = editor
            .rows
            .iter()
            .position(|row| row.field.section == "keys")
            .unwrap();
        let combo = editor.rows[index].c(0);
        let before = send(combo, CB_GETCOUNT, 0, 0);
        editor.dirty.insert(index);
        let mut disk = editor.doc.clone();
        disk["presets"]["ExternalRegion"] = value("0|0|25sw|25sh");
        std::fs::write(&editor.path, disk.to_string()).unwrap();
        editor.save().unwrap();
        assert_eq!(editor.rows[index].c(0), combo);
        assert_eq!(send(combo, CB_GETCOUNT, 0, 0), before + 1);
        assert!(
            editor.rows[index]
                .options
                .iter()
                .any(|option| option.0 == "ExternalRegion")
        );
        editor.dirty.insert(index);
        editor.save().unwrap();
        assert_eq!(send(combo, CB_GETCOUNT, 0, 0), before + 1);
    }

    #[test]
    #[ignore = "uses temporary settings controls to exercise modal callbacks and status clipping"]
    fn modal_display_updates_and_save_errors_remain_visible() {
        let _hooks = input::Hooks::escape_only().unwrap();
        let mut editor = test_editor(0, "settings-modal-test");
        let hwnd = editor.window.0;
        editor.saved_status("Initial save", false);
        assert_eq!(caption(editor.status), "Initial save");
        assert!(editor.pending_status.borrow().is_none());
        editor.update_status(editor.status_until.get().unwrap());
        editor.saved_status("After normal expiry", false);
        assert_eq!(caption(editor.status), "After normal expiry");
        assert!(editor.pending_status.borrow().is_none());
        editor.error("Save failed: full detailed error");
        editor.status("Saved");
        assert_eq!(caption(editor.status), "Save failed: full detailed error");
        let error_until = editor.status_until.get().unwrap();
        assert!(error_until > Instant::now() + Duration::from_secs(9));
        editor.error("Save failed: full detailed error");
        assert_eq!(editor.status_until.get(), Some(error_until));
        editor.update_status(error_until);
        assert!(caption(editor.status).is_empty());
        editor.error("Save failed: full detailed error");
        assert!(
            caption(editor.status).is_empty(),
            "automatic retries restarted the same error"
        );
        editor.error("A different error");
        editor.saved_status("First save", false);
        assert!(caption(editor.status).is_empty());
        assert_eq!(
            unsafe { GetWindowLongW(editor.status, GWL_STYLE) } as u32 & WS_VISIBLE,
            0
        );
        let first = editor.pending_status.borrow().as_ref().unwrap().0;
        editor.saved_status("Latest save", false);
        let latest = editor.pending_status.borrow().as_ref().unwrap().0;
        assert!(latest > first);
        editor.show_pending_status(first);
        assert!(
            caption(editor.status).is_empty(),
            "stale timer showed the newer notice early"
        );
        editor.show_pending_status(latest - Duration::from_nanos(1));
        assert!(caption(editor.status).is_empty());
        assert!(
            editor.status_until.get().is_none(),
            "hidden time must not consume the display lifetime"
        );
        editor.show_pending_status(latest);
        assert_eq!(caption(editor.status), "Latest save");
        assert!(editor.pending_status.borrow().is_none());
        assert!(editor.status_until.get().unwrap() > Instant::now() + Duration::from_secs(2));
        editor.saved_status("Stale success", false);
        let stale = editor.pending_status.borrow().as_ref().unwrap().0;
        editor.error("Save failed: full detailed error");
        editor.show_pending_status(stale);
        assert_eq!(caption(editor.status), "Save failed: full detailed error");
        assert!(editor.status_until.get().unwrap() > Instant::now() + Duration::from_secs(9));
        let region = unsafe { CreateRectRgn(0, 0, 0, 0) };
        assert_ne!(unsafe { GetWindowRgn(editor.status, region) }, ERROR);
        assert_eq!(unsafe { PtInRegion(region, 0, 0) }, 0);
        unsafe {
            DeleteObject(region);
        }
        let index = editor
            .rows
            .iter()
            .position(|row| matches!(row.field.kind, Kind::Number))
            .unwrap();
        let control = editor.rows[index].c(0);
        set_text(control, "");
        editor.dirty.insert(index);
        EDITOR.with_borrow_mut(|slot| *slot = Some(editor));
        EDITOR.with_borrow_mut(|slot| {
            let editor = slot.as_mut().unwrap();
            let mut suggested = RECT {
                left: 120,
                top: 100,
                right: 1020,
                bottom: 800,
            };
            unsafe {
                window_proc(
                    hwnd,
                    WM_DPICHANGED,
                    144,
                    (&mut suggested as *mut RECT) as isize,
                );
            }
            assert!(DISPLAY_UPDATE.with(Cell::get));
            assert_eq!(DPI_RECT.with(Cell::get).unwrap().left, 120);
            assert_eq!(caption(control), "");
            editor.tick().unwrap();
            assert!(!DISPLAY_UPDATE.with(Cell::get));
            assert!(DPI_RECT.with(Cell::get).is_none());
            let mut actual = RECT::default();
            unsafe {
                GetWindowRect(hwnd, &mut actual);
            }
            assert_eq!((actual.left, actual.top), (120, 100));
            assert_eq!(caption(control), "");
            assert!(editor.dirty.contains(&index));
        });
        EDITOR.with_borrow_mut(|slot| {
            slot.take();
        });
    }
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
        let mut doc = "[keys]\nQ='1/9'\nG='Guide'\nH='Help'\n"
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

    fn painted_pixels(hwnd: HWND, rect: RECT) -> Vec<u32> {
        // Read only this test window's client surface, including overlapping children.
        unsafe {
            let dc = GetDCEx(hwnd, null_mut(), DCX_CACHE);
            assert!(!dc.is_null());
            let memory = CreateCompatibleDC(dc);
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top;
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
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
            assert_ne!(
                BitBlt(
                    memory, 0, 0, width, height, dc, rect.left, rect.top, SRCCOPY
                ),
                0
            );
            GdiFlush();
            let pixels = std::slice::from_raw_parts(bits as *const u32, (width * height) as usize)
                .iter()
                .map(|pixel| pixel & 0xffffff)
                .collect();
            SelectObject(memory, old);
            DeleteObject(bitmap);
            DeleteDC(memory);
            ReleaseDC(hwnd, dc);
            pixels
        }
    }

    #[test]
    #[ignore = "checks real region and shortcut control geometry at narrow and wide widths"]
    fn mapping_inputs_and_actions_share_rows_with_right_aligned_delete() {
        let _hooks = input::Hooks::escape_only().unwrap();
        for tab in [2, 4] {
            let mut editor = test_editor(tab, &format!("mapping-rows-{tab}"));
            for (dpi, width) in [(96, 640), (96, 980), (144, 1100), (192, 1280)] {
                editor.dpi = dpi;
                unsafe {
                    SetWindowPos(
                        editor.window.0,
                        null_mut(),
                        100,
                        100,
                        width,
                        760,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                editor.layout().unwrap();
                let geometry = |hwnd| {
                    let mut r = RECT::default();
                    unsafe {
                        GetWindowRect(hwnd, &mut r);
                        MapWindowPoints(
                            null_mut(),
                            editor.panes[1],
                            (&mut r as *mut RECT).cast(),
                            2,
                        );
                    }
                    r
                };
                let mut pane = RECT::default();
                unsafe {
                    GetClientRect(editor.panes[1], &mut pane);
                }
                for row in editor
                    .rows
                    .iter()
                    .filter(|r| matches!(r.field.kind, Kind::Mapping))
                {
                    let name = geometry(row.c(3));
                    let value = geometry(row.c(0));
                    let delete = geometry(row.c(4));
                    assert_eq!(name.top, value.top);
                    assert_eq!(value.top, delete.top);
                    assert!(
                        name.right < value.left && value.right < delete.left,
                        "inputs overlap at dpi={dpi}, width={width}"
                    );
                    assert_eq!(delete.right, pane.right - editor.s(16));
                    let preview = geometry(row.c(5));
                    let related = geometry(row.c(6));
                    assert_eq!(preview.top, related.top);
                    assert!(preview.right < related.left);
                    assert_eq!(related.right, delete.right);
                    assert!(caption(row.c(12)).is_empty());
                }
            }
        }
    }

    #[test]
    #[ignore = "saves an isolated region expression and checks two-place rounding in both editor and config"]
    fn region_expression_saves_at_two_decimal_places() {
        let mut editor = test_editor(2, "region-precision-test");
        let index = editor
            .rows
            .iter()
            .position(|r| r.field.section == "presets" && r.field.key == "1/9")
            .unwrap();
        let edit = editor.rows[index].c(0);
        set_text(edit, "1.234|2.987|50.12345sw|40.999sh");
        editor
            .command(BASE + index as i32 * STRIDE, EN_CHANGE, edit)
            .unwrap();
        editor.save().unwrap();
        assert_eq!(caption(edit), "1.23|2.99|50.12sw|41sh");
        let saved = config::read_settings(&editor.path)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            saved["presets"]["1/9"].as_str(),
            Some("1.23|2.99|50.12sw|41sh")
        );
    }

    #[test]
    #[ignore = "updates the shared UI font and verifies controls resize without moving focus"]
    fn shared_ui_font_updates_settings_and_survives_page_switches() {
        let _hooks = input::Hooks::escape_only().unwrap();
        let mut editor = test_editor(0, "shared-ui-font-test");
        let index = editor
            .rows
            .iter()
            .position(|r| r.field.section == "ui" && r.field.key == "font_size")
            .unwrap();
        let edit = editor.rows[index].c(0);
        unsafe {
            SetFocus(edit);
        }
        set_text(edit, "18");
        editor
            .command(BASE + index as i32 * STRIDE, EN_CHANGE, edit)
            .unwrap();
        assert_eq!(editor.cfg.font_size, 18);
        assert_eq!(unsafe { GetFocus() }, edit);
        assert_eq!(
            send(edit, WM_GETFONT, 0, 0) as usize,
            editor.font.0 as usize
        );
        editor.save().unwrap();
        editor.switch_page(2, 0).unwrap();
        assert!(!editor.rows.iter().any(|r| r.field.key.starts_with("font")));
        let field = editor
            .rows
            .iter()
            .find(|r| r.field.section == "presets")
            .unwrap()
            .c(0);
        assert_eq!(
            send(field, WM_GETFONT, 0, 0) as usize,
            editor.font.0 as usize
        );
        let loaded = config::load(&editor.path, None).unwrap();
        assert_eq!(loaded.font_size, 18);
        for tab in [1, 2] {
            assert!(
                !schema::fields(tab, 0, &loaded, &editor.doc)
                    .iter()
                    .any(|f| f.section == "ui" || f.key.starts_with("font"))
            );
        }
    }

    #[test]
    #[ignore = "edits an isolated setting and verifies inline draft, error and saved feedback"]
    fn inline_edit_status_tracks_validity_and_persistence() {
        let mut editor = test_editor(5, "inline-status-test");
        editor.cfg.zh = true;
        let index = editor
            .rows
            .iter()
            .position(|r| r.field.section == "crosshair" && r.field.key == "fps")
            .unwrap();
        let edit = editor.rows[index].c(0);
        let state = editor.rows[index].c(12);
        let id = BASE + index as i32 * STRIDE;
        set_text(edit, "invalid");
        editor.command(id, EN_CHANGE, edit).unwrap();
        assert!(caption(state).contains("未保存"));
        assert!(caption(state).contains("输入有误"));
        assert!(editor.save().is_err());
        set_text(edit, "60");
        editor.command(id, EN_CHANGE, edit).unwrap();
        assert!(caption(state).contains("Not saved") || caption(state).contains("未保存"));
        editor.save().unwrap();
        assert!(caption(state).is_empty());
        assert_eq!(config::load(&editor.path, None).unwrap().fps, 60.0);
    }

    #[test]
    #[ignore = "navigates directly between a region and its shortcut controls"]
    fn related_region_shortcut_buttons_navigate_both_directions() {
        let _hooks = input::Hooks::escape_only().unwrap();
        let mut editor = test_editor(2, "related-region-shortcuts");
        let index = editor
            .rows
            .iter()
            .position(|r| r.field.section == "presets" && r.field.key == "1/4")
            .unwrap();
        let preview = editor.rows[index].c(5);
        editor
            .command(BASE + index as i32 * STRIDE + 5, BN_CLICKED, preview)
            .unwrap();
        assert!(editor.preview.visible());
        editor.escape().unwrap();
        editor.render().unwrap();
        assert!(!editor.preview.visible());
        let button = editor.rows[index].c(6);
        editor
            .command(BASE + index as i32 * STRIDE + 6, BN_CLICKED, button)
            .unwrap();
        assert_eq!(editor.tab, 4);
        let index = editor
            .rows
            .iter()
            .position(|r| r.field.section == "keys" && r.field.key == "1")
            .unwrap();
        let preview = editor.rows[index].c(5);
        editor
            .command(BASE + index as i32 * STRIDE + 5, BN_CLICKED, preview)
            .unwrap();
        assert!(editor.preview.visible());
        assert_eq!(editor.selected.as_deref(), Some("1/4"));
        editor.escape().unwrap();
        editor.render().unwrap();
        assert!(!editor.preview.visible());
        let button = editor.rows[index].c(6);
        editor
            .command(BASE + index as i32 * STRIDE + 6, BN_CLICKED, button)
            .unwrap();
        assert_eq!(editor.tab, 2);
    }

    #[test]
    #[ignore = "checks all dependent controls when mouse actions are disabled"]
    fn mouse_switch_disables_region_click_controls() {
        let _hooks = input::Hooks::escape_only().unwrap();
        let mut editor = test_editor(6, "mouse-master-switch");
        editor.doc["shortcuts"]["mouse_enabled"] = value(false);
        editor.cfg = Config::parse(&editor.doc).unwrap();
        editor.rebuild().unwrap();
        for row in &editor.rows {
            if row.field.section == "selection" {
                assert_eq!(unsafe { IsWindowEnabled(row.c(0)) }, 0);
            }
        }
    }

    #[test]
    #[ignore = "shows settings region outlines beyond the former dwell delay and closes them with one Escape"]
    fn region_preview_keeps_all_outlines_and_escape_closes_once() {
        let _hooks = input::Hooks::escape_only().unwrap();
        let mut editor = test_editor(6, "settings-preview-no-dwell");
        editor.live = true;
        editor.render().unwrap();
        assert!(editor.preview.visible());
        std::thread::sleep(Duration::from_millis(2100));
        editor.preview.tick(&editor.cfg).unwrap();
        assert!(editor.preview.visible());
        editor.escape().unwrap();
        assert!(!editor.preview.visible());
        assert!(!editor.live);
        editor.render().unwrap();
        assert!(!editor.preview.visible());
        for row in &editor.rows {
            if matches!(row.field.kind, Kind::Preview) {
                assert_eq!(send(row.c(0), BM_GETCHECK, 0, 0), 0);
            }
        }
    }

    #[test]
    #[ignore = "uses actual settings combos to verify filtering does not edit bindings until selection"]
    fn searchable_action_combo_saves_original_filtered_region() {
        let mut editor = test_editor(4, "settings-combo-filter");
        let index = editor
            .rows
            .iter()
            .position(|r| r.field.section == "keys" && r.field.key == "Q")
            .unwrap();
        let combo = editor.rows[index].c(0);
        let id = BASE + index as i32 * STRIDE;
        let options = editor.rows[index].options.clone();
        let selected = options.iter().position(|(v, _)| v == "4/4").unwrap();
        set_text(combo, "4/4");
        editor.command(id, CBN_EDITCHANGE, combo).unwrap();
        assert_eq!(send(combo, CB_GETCOUNT, 0, 0), 1);
        assert_eq!(Editor::selected_value(&editor.rows[index], 0), "1/9");
        send(combo, CB_SETCURSEL, 0, 0);
        crate::settings_combo::notification(combo, CBN_SELENDOK);
        editor
            .command(id, crate::settings_combo::COMMIT, combo)
            .unwrap();
        assert_eq!(crate::settings_combo::selected_index(combo), Some(selected));
        assert_eq!(editor.collect().unwrap()["keys"]["Q"].as_str(), Some("4/4"));
    }

    #[test]
    #[ignore = "uses temporary shortcut controls to distinguish top-row and Num Lock-independent keypad input"]
    fn shortcut_capture_distinguishes_keypad_digits_and_navigation() {
        let editor = test_editor(4, "settings-shortcut-capture");
        let row = editor
            .rows
            .iter()
            .find(|row| row.field.section == "keys" && row.field.key == "Q")
            .unwrap();
        let edit = row.c(3);
        for (vk, scan, extended, expected) in [
            (0x31, 0x02, false, "1"),
            (0x61, 0x4f, false, "Numpad1"),
            (0x23, 0x4f, false, "Numpad1"),
            (0x70, 0x3b, false, "F1"),
        ] {
            send(edit, WM_SETFOCUS, 0, 0);
            send(
                edit,
                WM_KEYDOWN,
                vk,
                1 | (scan << 16) | (isize::from(extended) << 24),
            );
            assert_eq!(caption(edit), expected);
            assert_eq!(
                config::vk(&caption(edit)),
                Some(if expected == "Numpad1" {
                    0x61
                } else {
                    vk as u32
                })
            );
        }
        assert_eq!(config::physical_key(0x23, 0x4f, true), 0x23);
        assert_ne!(config::vk("1"), config::vk("Numpad1"));
        send(edit, WM_SETFOCUS, 0, 0);
        send(edit, WM_KEYDOWN, 0x46, 1);
        send(edit, WM_KEYDOWN, 0x31, 1);
        send(edit, WM_KEYDOWN, 0x32, 1);
        assert_eq!(caption(edit), "F");
        assert!(caption(row.c(7)).contains("VK:46"));
    }

    #[test]
    #[ignore = "opens temporary edits and compares repeated Delete/Backspace paints with full repaint"]
    fn repeated_delete_and_backspace_leave_no_stale_text_pixels() {
        let _dpi = crate::test_support::Dpi::new();
        let editor = test_editor(0, "settings-delete-paint-test");
        let root = editor.window.0;
        unsafe {
            SetWindowLongPtrW(root, GWLP_WNDPROC, window_proc as *const () as isize);
            for pane in editor.panes {
                ShowWindow(pane, SW_HIDE);
            }
            ShowWindow(root, SW_SHOW);
        }
        for style in [
            ES_AUTOHSCROLL,
            ES_MULTILINE | ES_AUTOVSCROLL,
            ES_NUMBER | ES_AUTOHSCROLL,
        ] {
            let edit = editor
                .create(root, "EDIT", "", WS_BORDER | WS_TABSTOP | style as u32, 900)
                .unwrap();
            place(edit, 20, 20, 320, 100);
            for dark in [false, true] {
                crate::settings_theme::test_dark(Some(dark));
                for backspace in [false, true] {
                    set_text(edit, "12345678901234567890123456789012345678901234567890");
                    unsafe {
                        SetFocus(edit);
                        SendMessageW(
                            edit,
                            EM_SETSEL,
                            if backspace { 50 } else { 0 },
                            if backspace { 50 } else { 0 },
                        );
                        HideCaret(edit);
                        RedrawWindow(
                            edit,
                            null(),
                            null_mut(),
                            RDW_INVALIDATE | RDW_ERASE | RDW_UPDATENOW,
                        );
                    }
                    for count in 0..50 {
                        unsafe {
                            if backspace {
                                SendMessageW(edit, WM_CHAR, 8, 1);
                            } else {
                                SendMessageW(edit, WM_KEYDOWN, 0x2e, 1);
                                SendMessageW(edit, WM_KEYUP, 0x2e, 1);
                            }
                            RedrawWindow(edit, null(), null_mut(), RDW_UPDATENOW);
                        }
                        assert_eq!(caption(edit).len(), 49 - count);
                        let mut rect = RECT::default();
                        unsafe {
                            GetClientRect(edit, &mut rect);
                        }
                        let incremental = painted_pixels(edit, rect);
                        unsafe {
                            RedrawWindow(
                                edit,
                                null(),
                                null_mut(),
                                RDW_INVALIDATE | RDW_ERASE | RDW_UPDATENOW,
                            );
                        }
                        let full = painted_pixels(edit, rect);
                        assert_eq!(
                            incremental
                                .iter()
                                .zip(&full)
                                .filter(|(a, b)| a != b)
                                .count(),
                            0,
                            "delete={count}, backspace={backspace}, dark={dark}, style={style}"
                        );
                    }
                }
            }
            unsafe {
                DestroyWindow(edit);
            }
        }
        crate::settings_theme::test_dark(None);
    }

    #[test]
    #[ignore = "resizes a multiline edit across wrapping widths and compares with a full repaint"]
    fn long_edit_text_repaints_after_shrinking_and_growing() {
        let _dpi = crate::test_support::Dpi::new();
        let _hooks = input::Hooks::escape_only().unwrap();
        let editor = test_editor(0, "settings-edit-paint-test");
        let root = editor.window.0;
        unsafe {
            SetWindowLongPtrW(root, GWLP_WNDPROC, window_proc as *const () as isize);
            for pane in editor.panes {
                ShowWindow(pane, SW_HIDE);
            }
            SetWindowPos(
                root,
                null_mut(),
                0,
                0,
                1000,
                600,
                SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOZORDER,
            );
            ShowWindow(root, SW_SHOW);
        }
        let text = "A long configuration value with several words to wrap. 窗口缩小时旧文字必须完全擦除，不能留下前一次绘制的字符。";
        let edit = editor
            .create(root, "EDIT", text, WS_BORDER | WS_TABSTOP, 900)
            .unwrap();
        for dark in [false, true] {
            crate::settings_theme::test_dark(Some(dark));
            unsafe {
                RedrawWindow(
                    root,
                    null(),
                    null_mut(),
                    RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
                );
            }
            for width in [850, 300, 180, 650, 180, 850] {
                place(edit, 20, 20, width, 220);
                crate::test_support::pump();
                unsafe {
                    RedrawWindow(root, null(), null_mut(), RDW_UPDATENOW | RDW_ALLCHILDREN);
                }
                let rect = RECT {
                    left: 0,
                    top: 0,
                    right: 900,
                    bottom: 260,
                };
                let incremental = painted_pixels(root, rect);
                unsafe {
                    RedrawWindow(
                        root,
                        null(),
                        null_mut(),
                        RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
                    );
                }
                let full = painted_pixels(root, rect);
                let changed = incremental
                    .iter()
                    .zip(&full)
                    .filter(|(a, b)| a != b)
                    .count();
                assert_eq!(
                    changed, 0,
                    "stale multiline edit pixels at width={width}, dark={dark}"
                );
                assert_eq!(caption(edit), text);
            }
            place(edit, 20, 20, 1000, 220);
            for width in [1250, 800, 1150, 780, 1250] {
                unsafe {
                    SetWindowPos(
                        root,
                        null_mut(),
                        0,
                        0,
                        width,
                        600,
                        SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOZORDER,
                    );
                }
                crate::test_support::pump();
                unsafe {
                    RedrawWindow(root, null(), null_mut(), RDW_UPDATENOW | RDW_ALLCHILDREN);
                }
                let mut rect = RECT::default();
                unsafe {
                    GetClientRect(root, &mut rect);
                }
                rect.bottom = 260;
                let incremental = painted_pixels(root, rect);
                unsafe {
                    RedrawWindow(
                        root,
                        null(),
                        null_mut(),
                        RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
                    );
                }
                let full = painted_pixels(root, rect);
                assert_eq!(
                    incremental, full,
                    "text clipped by parent width={width}, dark={dark}"
                );
            }
        }
        crate::settings_theme::test_dark(None);
    }

    #[test]
    #[ignore = "moves a temporary settings window and checks layout and child paint counts"]
    fn moving_settings_does_not_relayout_or_repaint_controls() {
        let _dpi = crate::test_support::Dpi::new();
        let _hooks = input::Hooks::escape_only().unwrap();
        let editor = test_editor(0, "settings-move-paint-test");
        let root = editor.window.0;
        let controls: Vec<_> = editor
            .rows
            .iter()
            .flat_map(|row| row.controls)
            .chain(std::iter::once(editor.status))
            .filter(|hwnd| !hwnd.is_null())
            .collect();
        unsafe {
            SetWindowLongPtrW(root, GWLP_WNDPROC, window_proc as *const () as isize);
            SetWindowLongW(
                root,
                GWL_STYLE,
                (GetWindowLongW(root, GWL_STYLE) as u32 | WS_CLIPCHILDREN) as i32,
            );
            for pane in editor.panes {
                SetWindowLongPtrW(pane, GWLP_WNDPROC, pane_proc as *const () as isize);
                SetWindowLongW(
                    pane,
                    GWL_STYLE,
                    (WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS) as i32,
                );
            }
            ShowWindow(root, SW_SHOW);
        }
        editor.status("Saved");
        EDITOR.with_borrow_mut(|slot| *slot = Some(editor));
        struct Cleanup;
        impl Drop for Cleanup {
            fn drop(&mut self) {
                EDITOR.with_borrow_mut(|slot| {
                    slot.take();
                });
            }
        }
        let _cleanup = Cleanup;
        with_editor(|editor| editor.layout());
        crate::test_support::pump();
        unsafe {
            RedrawWindow(
                root,
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
            );
        }
        let layouts = unsafe { GetPropW(root, w!("LayoutCount")) } as usize;
        let counts = || {
            controls
                .iter()
                .map(|&hwnd| unsafe {
                    (
                        GetPropW(hwnd, w!("SizeCount")) as usize,
                        GetPropW(hwnd, w!("PaintCount")) as usize,
                    )
                })
                .collect::<Vec<_>>()
        };
        let before = counts();
        let mut rect = RECT::default();
        unsafe {
            GetWindowRect(root, &mut rect);
        }
        let mut position = WINDOWPOS {
            hwnd: root,
            x: rect.left + 5,
            y: rect.top + 5,
            cx: rect.right - rect.left,
            cy: rect.bottom - rect.top,
            flags: SWP_NOACTIVATE | SWP_NOZORDER,
            ..Default::default()
        };
        unsafe {
            window_proc(
                root,
                WM_WINDOWPOSCHANGING,
                0,
                (&mut position as *mut WINDOWPOS) as isize,
            );
        }
        assert_eq!(
            position.flags & SWP_NOCOPYBITS,
            0,
            "same-size moves must retain client pixels"
        );
        for flags in [SWP_NOSIZE, 0] {
            for step in 1..=4 {
                unsafe {
                    SetWindowPos(
                        root,
                        null_mut(),
                        rect.left + step * 5,
                        rect.top + step * 3,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        flags | SWP_NOACTIVATE | SWP_NOZORDER,
                    );
                    // Duplicate size notifications and the end of a move must also
                    // preserve the existing layout when the client dimensions match.
                    SendMessageW(root, WM_SIZE, SIZE_RESTORED as usize, 0);
                    SendMessageW(root, WM_EXITSIZEMOVE, 0, 0);
                }
                crate::test_support::pump();
            }
        }
        assert_eq!(
            unsafe { GetPropW(root, w!("LayoutCount")) } as usize,
            layouts
        );
        assert_eq!(
            counts(),
            before,
            "moving the window redrew or resized a child"
        );
        unsafe {
            SetWindowPos(
                root,
                null_mut(),
                0,
                0,
                position.cx + 80,
                position.cy + 40,
                SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOZORDER,
            );
        }
        crate::test_support::pump();
        assert!(
            unsafe { GetPropW(root, w!("LayoutCount")) } as usize > layouts,
            "an actual resize must still update the layout"
        );
    }

    #[test]
    #[ignore = "opens a temporary settings window and compares resize paints with a full repaint"]
    fn status_corners_repaint_cleanly_when_resized_or_hidden() {
        let _dpi = crate::test_support::Dpi::new();
        let _hooks = input::Hooks::escape_only().unwrap();
        let mut editor = test_editor(0, "settings-status-paint-test");
        let root = editor.window.0;
        unsafe {
            SetWindowLongPtrW(root, GWLP_WNDPROC, window_proc as *const () as isize);
            SetWindowLongW(
                root,
                GWL_STYLE,
                (GetWindowLongW(root, GWL_STYLE) as u32 | WS_CLIPCHILDREN) as i32,
            );
            SetWindowLongW(
                editor.status,
                GWL_STYLE,
                (WS_CHILD | WS_VISIBLE | SS_OWNERDRAW | SS_NOTIFY) as i32,
            );
            for pane in editor.panes {
                SetWindowLongPtrW(pane, GWLP_WNDPROC, pane_proc as *const () as isize);
                SetWindowLongW(
                    pane,
                    GWL_STYLE,
                    (WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS) as i32,
                );
            }
            ShowWindow(root, SW_SHOW);
        }
        for dark in [false, true] {
            crate::settings_theme::test_dark(Some(dark));
            unsafe {
                RedrawWindow(
                    root,
                    null(),
                    null_mut(),
                    RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
                );
            }
            editor.error("Configuration could not be saved. Click here for the full error. 配置无法保存，点击查看完整错误信息。");
            for (width, height) in [(900, 700), (600, 420), (1050, 800), (680, 490), (900, 700)] {
                unsafe {
                    SetWindowPos(
                        root,
                        null_mut(),
                        0,
                        0,
                        width,
                        height,
                        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                editor.layout().unwrap();
                crate::test_support::pump();
                unsafe {
                    RedrawWindow(root, null(), null_mut(), RDW_UPDATENOW | RDW_ALLCHILDREN);
                }
                let mut rect = RECT::default();
                unsafe {
                    GetClientRect(root, &mut rect);
                }
                rect.top = (rect.bottom - 180).max(0);
                let incremental = painted_pixels(root, rect);
                unsafe {
                    RedrawWindow(
                        root,
                        null(),
                        null_mut(),
                        RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
                    );
                }
                let full = painted_pixels(root, rect);
                let changed = incremental
                    .iter()
                    .zip(&full)
                    .filter(|(a, b)| a != b)
                    .count();
                let samples: Vec<_> = incremental
                    .iter()
                    .zip(&full)
                    .enumerate()
                    .filter(|(_, (a, b))| a != b)
                    .take(16)
                    .map(|(i, (a, b))| {
                        (
                            i as i32 % rect.right,
                            rect.top + i as i32 / rect.right,
                            format!("{a:06x}->{b:06x}"),
                        )
                    })
                    .collect();
                assert_eq!(
                    changed, 0,
                    "stale pixels after {width}x{height}, dark={dark}: {samples:?}"
                );
            }
            set_text(editor.status, "");
            editor.layout_status();
            crate::test_support::pump();
            unsafe {
                RedrawWindow(root, null(), null_mut(), RDW_UPDATENOW | RDW_ALLCHILDREN);
            }
            let mut rect = RECT::default();
            unsafe {
                GetClientRect(root, &mut rect);
            }
            rect.top = rect.bottom - 180;
            let hidden = painted_pixels(root, rect);
            unsafe {
                RedrawWindow(
                    root,
                    null(),
                    null_mut(),
                    RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
                );
            }
            assert_eq!(hidden, painted_pixels(root, rect));
        }
        crate::settings_theme::test_dark(None);
    }

    #[test]
    #[ignore = "opens a temporary settings window and exercises live resize"]
    fn resize_keeps_panes_at_bottom_and_sidebar_centered() {
        let folder = std::env::current_dir().unwrap().join("target/resize-test");
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("config.toml");
        std::fs::write(&path, config::help()).unwrap();
        assert!(unsafe { FindWindowW(CLASS, null()) }.is_null());
        let screenshots = folder.clone();
        let controller = std::thread::spawn(move || {
            let _dpi = crate::test_support::Dpi::new();
            let deadline = Instant::now() + Duration::from_secs(10);
            let root = loop {
                let root = unsafe { FindWindowW(CLASS, null()) };
                if !root.is_null() {
                    break root;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(20));
            };
            let result = std::panic::catch_unwind(|| unsafe {
                for height in [780, 1000, 320, 480, 900, 320] {
                    let combo = GetDlgItem(GetDlgItem(root, 2), BASE + 2 * STRIDE);
                    let before_size = GetPropW(combo, w!("SizeCount")) as usize;
                    SetWindowPos(
                        root,
                        null_mut(),
                        0,
                        0,
                        900,
                        height,
                        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                    std::thread::sleep(Duration::from_millis(100));
                    let mut client = RECT::default();
                    GetClientRect(root, &mut client);
                    for id in [1, 2] {
                        let pane = GetDlgItem(root, id);
                        let mut rect = RECT::default();
                        GetWindowRect(pane, &mut rect);
                        MapWindowPoints(null_mut(), root, (&mut rect as *mut RECT).cast(), 2);
                        assert_eq!(rect.bottom, client.bottom, "pane {id}, height {height}");
                    }
                    let sidebar = GetDlgItem(root, 1);
                    assert_eq!(GetWindowLongW(sidebar, GWL_STYLE) as u32 & WS_VSCROLL, 0);
                    let mut pane = RECT::default();
                    GetClientRect(sidebar, &mut pane);
                    let mut tab = RECT::default();
                    GetWindowRect(GetDlgItem(sidebar, TAB_BASE), &mut tab);
                    MapWindowPoints(null_mut(), sidebar, (&mut tab as *mut RECT).cast(), 2);
                    assert!((tab.left - (pane.right - tab.right)).abs() <= 1);
                    let right = GetDlgItem(root, 2);
                    let combo = GetDlgItem(right, BASE + 2 * STRIDE);
                    if !combo.is_null() {
                        assert_eq!(GetPropW(combo, w!("SettingsCombo")) as usize, 1);
                        if height == 1000 {
                            assert_eq!(
                                GetPropW(combo, w!("SizeCount")) as usize,
                                before_size,
                                "height-only resize must not resize the language dropdown"
                            );
                        }
                    }
                }
            });
            unsafe {
                SetWindowPos(
                    root,
                    null_mut(),
                    0,
                    0,
                    900,
                    780,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
                let language = GetDlgItem(GetDlgItem(root, 2), BASE + 2 * STRIDE);
                send(language, CB_SETCURSEL, 1, 0);
                send(
                    root,
                    WM_COMMAND,
                    (BASE + 2 * STRIDE) as usize | ((crate::settings_combo::COMMIT as usize) << 16),
                    language as isize,
                );
                std::thread::sleep(Duration::from_millis(800));
                for (mode, name) in [(1, "light"), (2, "dark")] {
                    send(root, TEST_THEME, mode, 0);
                    std::thread::sleep(Duration::from_millis(100));
                    let pane = GetDlgItem(root, 2);
                    let dc = GetDC(pane);
                    let background = GetPixel(dc, 4, 4);
                    ReleaseDC(pane, dc);
                    assert_eq!(background, if mode == 1 { 0xffffff } else { 0x202020 });
                    let status = GetDlgItem(root, 600);
                    assert!(!caption(status).is_empty());
                    assert_eq!(GetWindow(root, GW_CHILD), status);
                    let mut status_rect = RECT::default();
                    let mut root_rect = RECT::default();
                    GetClientRect(root, &mut root_rect);
                    GetWindowRect(status, &mut status_rect);
                    MapWindowPoints(null_mut(), root, (&mut status_rect as *mut RECT).cast(), 2);
                    assert_eq!(
                        (status_rect.left, status_rect.bottom),
                        (0, root_rect.bottom)
                    );
                    snapshot(root, &screenshots.join(format!("{name}.bmp")));
                }
                send(root, TEST_THEME, 0, 0);
                PostMessageW(root, WM_CLOSE, 0, 0);
            }
            result.unwrap();
        });
        let _dpi = crate::test_support::Dpi::new();
        run(&path).unwrap();
        controller.join().unwrap();
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
        doc["ui"]["font_fallbacks"] = value(fallbacks);
        let overflow_name = "这是一个用于验证下拉框溢出提示的超长分区名称".repeat(4);
        doc["presets"][&overflow_name] = value("0|0|50sw|50sh");
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
                let search = unsafe { GetDlgItem(hwnd, SEARCH_EDIT) };
                let list = unsafe { GetDlgItem(hwnd, SEARCH_RESULTS) };
                let mut focus = GUITHREADINFO {
                    cbSize: size_of::<GUITHREADINFO>() as u32,
                    ..Default::default()
                };
                unsafe {
                    GetGUIThreadInfo(GetWindowThreadProcessId(hwnd, null_mut()), &mut focus);
                }
                if focus.hwndFocus != search {
                    return Err("Search did not receive initial focus".into());
                }
                set_text(search, "准星");
                let until = Instant::now() + Duration::from_secs(5);
                while unsafe { IsWindowVisible(list) } == 0
                    || unsafe { GetPropW(hwnd, w!("ReadyTab")) } as usize != 6
                {
                    if Instant::now() > until {
                        return Err("Search did not navigate to crosshair".into());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                snapshot(hwnd, &output.join("search.bmp"));
                unsafe {
                    SendMessageW(list, WM_KEYDOWN, 27, 0);
                }
                std::thread::sleep(Duration::from_millis(100));
                if unsafe { IsWindowVisible(list) } != 0 {
                    return Err("Escape did not dismiss search results".into());
                }
                set_text(search, "");
                std::thread::sleep(Duration::from_millis(200));
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
                for tab in 0..TABS.len() as i32 {
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
                    let fields =
                        schema::fields(tab as usize, 0, &Config::parse(&doc).unwrap(), &doc);
                    let mut child = unsafe { GetWindow(pane, GW_CHILD) };
                    while !child.is_null() {
                        let id = unsafe { GetDlgCtrlID(child) };
                        let region_action = id >= BASE
                            && matches!((id - BASE) % STRIDE, 5 | 6)
                            && fields
                                .get(((id - BASE) / STRIDE) as usize)
                                .is_some_and(|f| {
                                    matches!(f.kind, Kind::Mapping) && f.section != "presets"
                                });
                        if unsafe { IsWindowVisible(child) } != 0
                            && unsafe { IsWindowEnabled(child) } == 0
                            && !matches!(id, AXIS_H | AXIS_V)
                            && !region_action
                        {
                            return Err(format!(
                                "Tab {tab} contains a permanently disabled control"
                            ));
                        }
                        child = unsafe { GetWindow(child, GW_HWNDNEXT) };
                    }
                    for (index, field) in fields.iter().enumerate() {
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
                    if tab == 0 {
                        let pane = unsafe { GetDlgItem(hwnd, 2) };
                        unsafe {
                            PostMessageW(hwnd, SCROLL, 900, pane as isize);
                        }
                        std::thread::sleep(Duration::from_millis(200));
                        snapshot(hwnd, &output.join("fonts.bmp"));
                    }
                }
                for tab in 0..TABS.len() as i32 {
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
                // Mouse controls follow switches immediately and retain saved selections.
                send(hwnd, WM_COMMAND, (TAB_BASE + 6) as usize, 0);
                wait_tab(hwnd, 6)?;
                let fields = schema::fields(6, 0, &Config::parse(&doc).unwrap(), &doc);
                let control_id = |key: &str| {
                    BASE + fields
                        .iter()
                        .position(|field| field.section == "shortcuts" && field.key == key)
                        .unwrap() as i32
                        * STRIDE
                };
                for (switch, button) in [
                    ("move_enabled", "move_button"),
                    ("resize_enabled", "resize_button"),
                    ("center_enabled", "center_button"),
                ] {
                    let pane = unsafe { GetDlgItem(hwnd, 2) };
                    let checkbox = unsafe { GetDlgItem(pane, control_id(switch)) };
                    let combo = unsafe { GetDlgItem(pane, control_id(button)) };
                    let selection = send(combo, CB_GETCURSEL, 0, 0);
                    send(checkbox, BM_CLICK, 0, 0);
                    let deadline = Instant::now() + Duration::from_millis(300);
                    while unsafe { IsWindowEnabled(combo) } != 0 && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    if unsafe { IsWindowEnabled(combo) } != 0 {
                        return Err(format!("{button} did not follow its disabled switch"));
                    }
                    std::thread::sleep(Duration::from_millis(800));
                    send(hwnd, WM_COMMAND, TAB_BASE as usize, 0);
                    wait_tab(hwnd, 0)?;
                    send(hwnd, WM_COMMAND, (TAB_BASE + 6) as usize, 0);
                    wait_tab(hwnd, 6)?;
                    let checkbox = unsafe { GetDlgItem(pane, control_id(switch)) };
                    let combo = unsafe { GetDlgItem(pane, control_id(button)) };
                    if unsafe { IsWindowEnabled(combo) } != 0
                        || send(combo, CB_GETCURSEL, 0, 0) != selection
                    {
                        return Err(format!(
                            "{button} lost its disabled state or selection after switching pages"
                        ));
                    }
                    send(checkbox, BM_CLICK, 0, 0);
                    let deadline = Instant::now() + Duration::from_millis(300);
                    while unsafe { IsWindowEnabled(combo) } == 0 && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    if unsafe { IsWindowEnabled(combo) } == 0
                        || send(combo, CB_GETCURSEL, 0, 0) != selection
                    {
                        return Err(format!(
                            "{button} did not restore its saved selection when enabled"
                        ));
                    }
                }
                let pane = unsafe { GetDlgItem(hwnd, 2) };
                let mouse = unsafe { GetDlgItem(pane, control_id("mouse_enabled")) };
                send(mouse, BM_CLICK, 0, 0);
                std::thread::sleep(Duration::from_millis(100));
                for key in ["move_button", "resize_button", "center_button"] {
                    if unsafe { IsWindowEnabled(GetDlgItem(pane, control_id(key))) } != 0 {
                        return Err(format!(
                            "{key} remained enabled while mouse actions were off"
                        ));
                    }
                }
                send(mouse, BM_CLICK, 0, 0);
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
                for tab in [1, 2, 3, 5, 6, 7, 8] {
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
                    if tab == 6 && send(hwnd, PREVIEW_STATE, 0, 0) != 1 {
                        return Err("Selection preview not visible".into());
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
                let typed = config::read_settings(&output.join("config.toml"))?
                    .parse::<DocumentMut>()
                    .map_err(|e| e.to_string())?;
                if typed["crosshair"]["fps"].as_integer() == Some(1000) {
                    return Err("Typed text was saved before the field was committed".into());
                }
                // Enter commits the field; so does moving the focus away.
                send(fps, WM_CHAR, 13, 0);
                std::thread::sleep(Duration::from_millis(500));
                let doc = config::read_settings(&output.join("config.toml"))?
                    .parse::<DocumentMut>()
                    .map_err(|e| e.to_string())?;
                if doc["crosshair"]["fps"].as_integer() != Some(1000) {
                    return Err(format!(
                        "FPS was not clamped and saved: {}",
                        caption(unsafe { GetDlgItem(hwnd, 600) })
                    ));
                }
                // Changing the same setting in the file before the edit is committed wins.
                send(fps, EM_SETSEL, 0, -1);
                send(fps, EM_REPLACESEL, 1, w!("50") as isize);
                let mut external = doc.clone();
                external["crosshair"]["fps"] = value(75);
                std::fs::write(output.join("config.toml"), external.to_string())
                    .map_err(|e| e.to_string())?;
                send(fps, WM_CHAR, 13, 0);
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
                    wide(&overflow_name).as_ptr() as isize,
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
                    PostMessageW(hwnd, WM_COMMAND, TAB_BASE as usize, 0);
                }
                wait_tab(hwnd, 0)?;
                let font_row = schema::fields(0, 0, &Config::parse(&merged).unwrap(), &merged)
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
                send(name, WM_CHAR, 13, 0);
                std::thread::sleep(Duration::from_millis(800));
                if unsafe { GetDlgItem(pane, BASE + row as i32 * STRIDE) } != combo {
                    return Err("Renaming a mapping recreated its control".into());
                }
                // Shared style changes affect both axes, while FPS remains one shared field.
                unsafe {
                    PostMessageW(hwnd, WM_COMMAND, (TAB_BASE + 5) as usize, 0);
                }
                wait_tab(hwnd, 5)?;
                send(hwnd, WM_COMMAND, AXIS_BOTH as usize, 0);
                wait_tab(hwnd, 5)?;
                let pane = unsafe { GetDlgItem(hwnd, 2) };
                let width_row = schema::fields(5, 0, &Config::parse(&merged)?, &merged)
                    .iter()
                    .position(|field| field.section == "horizontal" && field.key == "width")
                    .ok_or("Horizontal line width missing")?;
                let width = unsafe { GetDlgItem(pane, BASE + width_row as i32 * STRIDE) };
                send(width, EM_SETSEL, 0, -1);
                send(width, EM_REPLACESEL, 1, w!("3") as isize);
                send(width, WM_CHAR, 13, 0);
                std::thread::sleep(Duration::from_millis(800));
                let saved = config::read_settings(&output.join("config.toml"))?
                    .parse::<DocumentMut>()
                    .map_err(|e| e.to_string())?;
                let effective = Config::parse(&saved)?;
                if effective.lines[0].width != 3 || effective.lines[1].width != 3 {
                    return Err(format!(
                        "Unified axis style did not update both lines: horizontal={}, vertical={}, input={}, status={}",
                        effective.lines[0].width,
                        effective.lines[1].width,
                        caption(width),
                        caption(unsafe { GetDlgItem(hwnd, 600) })
                    ));
                }
                send(hwnd, WM_COMMAND, AXIS_CUSTOM as usize, 0);
                let pane = unsafe { GetDlgItem(hwnd, 2) };
                if unsafe { IsWindowEnabled(GetDlgItem(pane, AXIS_H)) } == 0
                    || unsafe { IsWindowEnabled(GetDlgItem(pane, AXIS_V)) } == 0
                {
                    return Err("Custom mode did not enable both axis choices".into());
                }
                send(unsafe { GetDlgItem(pane, AXIS_H) }, BM_CLICK, 0, 0);
                std::thread::sleep(Duration::from_millis(100));
                if send(unsafe { GetDlgItem(pane, AXIS_CUSTOM) }, BM_GETCHECK, 0, 0) != 1 {
                    return Err("Selecting an axis cleared the custom mode".into());
                }
                send(hwnd, WM_COMMAND, AXIS_V as usize, 0);
                let mut custom_doc = saved.clone();
                custom_doc["crosshair"]["mode"] = value("custom");
                let width_row = schema::fields(5, 1, &Config::parse(&custom_doc)?, &custom_doc)
                    .iter()
                    .position(|field| field.section == "vertical" && field.key == "width")
                    .ok_or("Vertical line width missing")?;
                let vertical_width = unsafe { GetDlgItem(pane, BASE + width_row as i32 * STRIDE) };
                send(vertical_width, EM_SETSEL, 0, -1);
                send(vertical_width, EM_REPLACESEL, 1, w!("7") as isize);
                send(vertical_width, WM_CHAR, 13, 0);
                std::thread::sleep(Duration::from_millis(800));
                let custom = config::read_settings(&output.join("config.toml"))?
                    .parse::<DocumentMut>()
                    .map_err(|e| e.to_string())?;
                if custom["crosshair"]["mode"].as_str() != Some("custom")
                    || Config::parse(&custom)?.lines[1].width != 7
                {
                    return Err("Custom mode or vertical style was not saved".into());
                }
                send(hwnd, WM_COMMAND, AXIS_BOTH as usize, 0);
                let unified = config::read_settings(&output.join("config.toml"))?
                    .parse::<DocumentMut>()
                    .map_err(|e| e.to_string())?;
                if unified["vertical"]["width"].as_integer() != Some(7)
                    || Config::parse(&unified)?.lines[1].width != 3
                {
                    return Err("Unified mode lost the saved custom vertical style".into());
                }
                // The window manager may offer a work area smaller than our usual
                // minimum at high scaling. Controls must wrap within a narrow pane.
                for tab in [1, 2, 4, 5] {
                    send(hwnd, WM_COMMAND, (TAB_BASE + tab) as usize, 0);
                    wait_tab(hwnd, tab as usize)?;
                    unsafe {
                        SetWindowPos(
                            hwnd,
                            null_mut(),
                            0,
                            0,
                            520,
                            600,
                            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE,
                        );
                        send(hwnd, RESIZE, 0, 0);
                        let pane = GetDlgItem(hwnd, 2);
                        let mut area = RECT::default();
                        GetClientRect(pane, &mut area);
                        let mut child = GetWindow(pane, GW_CHILD);
                        while !child.is_null() {
                            if GetWindowLongW(child, GWL_STYLE) as u32 & WS_VISIBLE != 0 {
                                let mut rect = RECT::default();
                                GetWindowRect(child, &mut rect);
                                MapWindowPoints(
                                    null_mut(),
                                    pane,
                                    (&mut rect as *mut RECT).cast(),
                                    2,
                                );
                                if rect.left < 0 || rect.right > area.right {
                                    return Err(format!(
                                        "Tab {tab} control {} exceeds pane width {}: {}..{}",
                                        GetDlgCtrlID(child),
                                        area.right,
                                        rect.left,
                                        rect.right
                                    ));
                                }
                            }
                            child = GetWindow(child, GW_HWNDNEXT);
                        }
                    }
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
