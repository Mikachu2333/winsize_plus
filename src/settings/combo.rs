//! Searchable native combos keep item identity separate from the filtered row index.
use crate::desktop::wide;
use std::{cell::RefCell, collections::HashMap, rc::Rc};
use windows_sys::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        UI::{Controls::*, Input::KeyboardAndMouse::GetFocus, Shell::*, WindowsAndMessaging::*},
    },
    core::w,
};
pub const COMMIT: u32 = 0x7f01;

#[derive(Clone)]
struct Search {
    options: Rc<Vec<(String, String)>>,
    terms: Rc<Vec<String>>,
    visible: Option<Rc<Vec<usize>>>,
    restored: bool,
    committed: Option<usize>,
    query: String,
    busy: bool,
    composing: bool,
    editing: bool,
}
thread_local! {static SEARCHES: RefCell<HashMap<usize,Search>> = RefCell::new(HashMap::new());}
fn text(hwnd: HWND) -> String {
    unsafe {
        let mut s = vec![0; GetWindowTextLengthW(hwnd).max(0) as usize + 1];
        let n = GetWindowTextW(hwnd, s.as_mut_ptr(), s.len() as i32);
        String::from_utf16_lossy(&s[..n.max(0) as usize])
    }
}
fn send(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    unsafe { SendMessageW(hwnd, msg, wp, lp) }
}
fn center_edit(combo: HWND) {
    // The searchable combo owns a single-line EDIT; its formatting rectangle
    // cannot supply vertical padding. Center the child at its actual font height.
    unsafe {
        let mut info = COMBOBOXINFO {
            cbSize: size_of::<COMBOBOXINFO>() as u32,
            ..Default::default()
        };
        if GetComboBoxInfo(combo, &mut info) == 0 || info.hwndItem.is_null() {
            return;
        }
        let dc = GetDC(info.hwndItem);
        if dc.is_null() {
            return;
        }
        let font = send(info.hwndItem, WM_GETFONT, 0, 0) as HFONT;
        let old = SelectObject(dc, font);
        let mut metrics = TEXTMETRICW::default();
        let measured = GetTextMetricsW(dc, &mut metrics);
        SelectObject(dc, old);
        ReleaseDC(info.hwndItem, dc);
        if measured == 0 {
            return;
        }
        let height = metrics
            .tmHeight
            .max(1)
            .min((info.rcItem.bottom - info.rcItem.top).max(1));
        let y = info.rcItem.top + (info.rcItem.bottom - info.rcItem.top - height) / 2;
        let mut current = RECT::default();
        GetWindowRect(info.hwndItem, &mut current);
        MapWindowPoints(
            std::ptr::null_mut(),
            combo,
            (&mut current as *mut RECT).cast(),
            2,
        );
        if current.top != y || current.bottom - current.top != height {
            SetWindowPos(
                info.hwndItem,
                std::ptr::null_mut(),
                info.rcItem.left,
                y,
                (info.rcItem.right - info.rcItem.left).max(1),
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}
fn paint_edit_padding(combo: HWND) {
    // SAFETY: all handles are borrowed from this combo for synchronous painting;
    // the DC is released and its clipping state restored before returning.
    unsafe {
        let mut info = COMBOBOXINFO {
            cbSize: size_of::<COMBOBOXINFO>() as u32,
            ..Default::default()
        };
        if GetComboBoxInfo(combo, &mut info) == 0 || info.hwndItem.is_null() {
            return;
        }
        let mut edit = RECT::default();
        GetWindowRect(info.hwndItem, &mut edit);
        MapWindowPoints(
            std::ptr::null_mut(),
            combo,
            (&mut edit as *mut RECT).cast(),
            2,
        );
        let dc = GetDC(combo);
        if dc.is_null() {
            return;
        }
        let saved = SaveDC(dc);
        ExcludeClipRect(dc, edit.left, edit.top, edit.right, edit.bottom);
        FillRect(dc, &info.rcItem, crate::settings::theme::control());
        RestoreDC(dc, saved);
        ReleaseDC(combo, dc);
    }
}
fn matching(options: &[String], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    let words: Vec<_> = query.split_whitespace().collect();
    options
        .iter()
        .enumerate()
        .filter_map(|(i, candidate)| {
            words
                .iter()
                .all(|word| candidate.contains(word))
                .then_some(i)
        })
        .collect()
}
pub fn attach(combo: HWND, zh: bool) {
    unsafe {
        SetPropW(combo, w!("ComboZh"), usize::from(zh) as HANDLE);
        SetWindowSubclass(combo, Some(combo_proc), 2, 0);
        let mut info = COMBOBOXINFO {
            cbSize: size_of::<COMBOBOXINFO>() as u32,
            ..Default::default()
        };
        if GetComboBoxInfo(combo, &mut info) != 0 && !info.hwndItem.is_null() {
            SetWindowSubclass(info.hwndItem, Some(edit_proc), 2, combo as usize);
            SendMessageW(
                info.hwndItem,
                EM_SETCUEBANNER,
                0,
                if zh {
                    w!("输入搜索词")
                } else {
                    w!("Type to search")
                } as isize,
            );
        }
    }
}
pub fn populate(combo: HWND, options: &[(String, String)], selected: &str) {
    SEARCHES.with_borrow_mut(|all| {
        all.insert(
            combo as usize,
            Search {
                options: Rc::new(options.to_vec()),
                terms: Rc::new(
                    options
                        .iter()
                        .map(|(value, label)| format!("{value} {label}").to_lowercase())
                        .collect(),
                ),
                visible: None,
                restored: false,
                committed: options
                    .iter()
                    .position(|(v, _)| v.eq_ignore_ascii_case(selected)),
                query: String::new(),
                busy: false,
                composing: false,
                editing: false,
            },
        );
    });
    restore(combo);
}
fn rebuild(combo: HWND, search: &Search, indices: &[usize]) {
    SEARCHES.with_borrow_mut(|all| {
        if let Some(s) = all.get_mut(&(combo as usize)) {
            s.busy = true;
        }
    });
    if search
        .visible
        .as_deref()
        .is_some_and(|shown| shown.as_slice() == indices)
    {
        return;
    }
    send(combo, WM_SETREDRAW, 0, 0);
    send(combo, CB_RESETCONTENT, 0, 0);
    for &original in indices {
        let index = send(
            combo,
            CB_ADDSTRING,
            0,
            wide(&search.options[original].1).as_ptr() as isize,
        );
        if index >= 0 {
            send(combo, CB_SETITEMDATA, index as usize, original as isize);
        }
    }
    if indices.is_empty() {
        let zh = unsafe { !GetPropW(combo, w!("ComboZh")).is_null() };
        let row = send(
            combo,
            CB_ADDSTRING,
            0,
            wide(if zh { "无匹配结果" } else { "No matches" }).as_ptr() as isize,
        );
        send(combo, CB_SETITEMDATA, row as usize, -1);
    }
    send(combo, WM_SETREDRAW, 1, 0);
    SEARCHES.with_borrow_mut(|all| {
        if let Some(s) = all.get_mut(&(combo as usize)) {
            s.visible = Some(Rc::new(indices.to_vec()));
        }
    });
}
fn finish_rebuild(combo: HWND) {
    SEARCHES.with_borrow_mut(|all| {
        if let Some(s) = all.get_mut(&(combo as usize)) {
            s.busy = false;
        }
    });
}
fn restore(combo: HWND) {
    let search = SEARCHES.with_borrow_mut(|all| {
        let s = all.get_mut(&(combo as usize))?;
        if s.restored && !s.editing {
            return None;
        }
        s.query.clear();
        s.editing = false;
        s.restored = true;
        Some(s.clone())
    });
    let Some(search) = search else {
        return;
    };
    rebuild(
        combo,
        &search,
        &(0..search.options.len()).collect::<Vec<_>>(),
    );
    send(
        combo,
        CB_SETCURSEL,
        search.committed.unwrap_or(usize::MAX),
        0,
    );
    finish_rebuild(combo);
}
pub fn original_index(combo: HWND, index: isize) -> Option<usize> {
    if index < 0 {
        return None;
    }
    let data = send(combo, CB_GETITEMDATA, index as usize, 0);
    (data >= 0).then_some(data as usize)
}
pub fn selected_index(combo: HWND) -> Option<usize> {
    SEARCHES.with_borrow(|all| all.get(&(combo as usize)).and_then(|s| s.committed))
}
fn commit(combo: HWND) -> bool {
    let index = original_index(combo, send(combo, CB_GETCURSEL, 0, 0));
    if let Some(index) = index {
        SEARCHES.with_borrow_mut(|all| {
            if let Some(s) = all.get_mut(&(combo as usize)) {
                s.committed = Some(index);
                s.query.clear();
            }
        });
        return true;
    }
    false
}
fn confirm(combo: HWND) -> bool {
    if commit(combo) {
        unsafe {
            PostMessageW(
                GetParent(combo),
                WM_COMMAND,
                (GetDlgCtrlID(combo) as usize & 0xffff) | ((COMMIT as usize) << 16),
                combo as isize,
            );
        }
        return true;
    }
    false
}
pub fn filter(combo: HWND) {
    let query = text(combo);
    let selection = send(combo, CB_GETEDITSEL, 0, 0);
    let search = SEARCHES.with_borrow_mut(|all| {
        let s = all.get_mut(&(combo as usize))?;
        if s.busy || s.composing {
            return None;
        }
        s.query = query.clone();
        s.editing = true;
        s.restored = false;
        Some(s.clone())
    });
    let Some(search) = search else {
        return;
    };
    let indices = matching(&search.terms, &query);
    rebuild(combo, &search, &indices);
    send(combo, CB_SHOWDROPDOWN, 1, 0);
    unsafe {
        SetWindowTextW(combo, wide(query).as_ptr());
    }
    send(combo, CB_SETEDITSEL, 0, selection);
    finish_rebuild(combo);
}
/// Returns true for search-only changes which must not become configuration edits.
pub fn notification(combo: HWND, event: u32) -> bool {
    let state = SEARCHES.with_borrow(|all| all.get(&(combo as usize)).map(|s| (s.busy, s.editing)));
    let Some((busy, editing)) = state else {
        return false;
    };
    if busy {
        return true;
    }
    match event {
        CBN_EDITCHANGE => {
            filter(combo);
            true
        }
        CBN_SELCHANGE => {
            SEARCHES.with_borrow_mut(|all| {
                if let Some(s) = all.get_mut(&(combo as usize)) {
                    s.editing = true;
                }
            });
            true
        }
        CBN_DROPDOWN => {
            SEARCHES.with_borrow_mut(|all| {
                if let Some(s) = all.get_mut(&(combo as usize)) {
                    s.editing = true;
                }
            });
            false
        }
        CBN_SELENDOK => {
            let _ = confirm(combo);
            true
        }
        CBN_SELENDCANCEL | CBN_KILLFOCUS => {
            if editing {
                restore(combo);
            }
            true
        }
        CBN_CLOSEUP => {
            restore(combo);
            false
        }
        _ => false,
    }
}
unsafe extern "system" fn combo_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    unsafe {
        if msg == WM_PAINT {
            let result = DefSubclassProc(hwnd, msg, wp, lp);
            // Native painting assumes the edit fills rcItem. Centering it leaves
            // padding that the combo does not erase when moved or uncovered.
            paint_edit_padding(hwnd);
            return result;
        }
        if matches!(
            msg,
            WM_WINDOWPOSCHANGED | WM_SETFONT | CB_SETITEMHEIGHT | WM_THEMECHANGED
        ) {
            let result = DefSubclassProc(hwnd, msg, wp, lp);
            center_edit(hwnd);
            return result;
        }
        if msg == CB_SETCURSEL {
            let result = DefSubclassProc(hwnd, msg, wp, lp);
            let original = original_index(hwnd, result);
            SEARCHES.with_borrow_mut(|all| {
                if let Some(search) = all.get_mut(&(hwnd as usize))
                    && !search.busy
                    && !search.editing
                {
                    search.committed = original;
                }
            });
            return result;
        }
        if msg == WM_NCDESTROY {
            RemovePropW(hwnd, w!("ComboZh"));
            SEARCHES.with_borrow_mut(|s| {
                s.remove(&(hwnd as usize));
            });
            RemoveWindowSubclass(hwnd, Some(combo_proc), 2);
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}
unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    data: usize,
) -> LRESULT {
    let combo = data as HWND;
    unsafe {
        // The combo creates its EDIT with ES_NOHIDESEL, which cannot be changed
        // after creation. Clear inactive selections before painting, including
        // ones restored internally by CB_SETCURSEL or a page refresh.
        if msg == WM_PAINT && GetFocus() != hwnd {
            let selection = send(hwnd, EM_GETSEL, 0, 0) as u32;
            if selection as u16 != (selection >> 16) as u16 {
                send(hwnd, EM_SETSEL, 0, 0);
            }
        }
        if msg == WM_KILLFOCUS {
            let result = DefSubclassProc(hwnd, msg, wp, lp);
            send(hwnd, EM_SETSEL, 0, 0);
            return result;
        }
        if msg == WM_IME_STARTCOMPOSITION {
            SEARCHES.with_borrow_mut(|s| {
                if let Some(s) = s.get_mut(&data) {
                    s.composing = true;
                }
            });
        }
        if msg == WM_IME_ENDCOMPOSITION {
            let result = DefSubclassProc(hwnd, msg, wp, lp);
            SEARCHES.with_borrow_mut(|s| {
                if let Some(s) = s.get_mut(&data) {
                    s.composing = false;
                }
            });
            filter(combo);
            return result;
        }
        if msg == WM_GETDLGCODE && lp != 0 && matches!((*(lp as *const MSG)).wParam, 13 | 27) {
            return DLGC_WANTMESSAGE as isize;
        }
        if msg == WM_KEYDOWN
            && matches!(wp, 13 | 27)
            && !SEARCHES.with_borrow(|s| s.get(&data).is_some_and(|s| s.composing))
        {
            if wp == 13 {
                if send(combo, CB_GETCURSEL, 0, 0) < 0 && send(combo, CB_GETCOUNT, 0, 0) > 0 {
                    send(combo, CB_SETCURSEL, 0, 0);
                }
                if !confirm(combo) {
                    return 0;
                }
            }
            send(combo, CB_SHOWDROPDOWN, 0, 0);
            restore(combo);
            return 0;
        }
        if msg == WM_CHAR && matches!(wp, 13 | 27) {
            return 0;
        }
        if msg == WM_NCDESTROY {
            RemoveWindowSubclass(hwnd, Some(edit_proc), 2);
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "uses native searchable combos to check edit alignment after font changes and filtering"]
    fn searchable_edit_stays_vertically_centered() {
        unsafe {
            let root = CreateWindowExW(
                0,
                w!("STATIC"),
                w!("Combo alignment"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                600,
                400,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            let combo = CreateWindowExW(
                0,
                w!("COMBOBOX"),
                w!(""),
                WS_CHILD
                    | WS_VISIBLE
                    | CBS_DROPDOWN as u32
                    | CBS_OWNERDRAWFIXED as u32
                    | CBS_HASSTRINGS as u32,
                10,
                10,
                300,
                240,
                root,
                1 as HMENU,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            attach(combo, true);
            for dpi in [96, 144, 192] {
                let font = crate::fonts::Font::new("SystemFont", 12, dpi, false, false).unwrap();
                send(combo, WM_SETFONT, font.0 as usize, 1);
                send(
                    combo,
                    CB_SETITEMHEIGHT,
                    usize::MAX,
                    (font.height() + 16 * dpi / 96) as isize,
                );
                populate(
                    combo,
                    &[
                        ("first".into(), "First value".into()),
                        ("second".into(), "Second value".into()),
                    ],
                    "first",
                );
                for search in [false, true] {
                    if search {
                        SetWindowTextW(combo, w!("second"));
                        filter(combo);
                    }
                    let mut info = COMBOBOXINFO {
                        cbSize: size_of::<COMBOBOXINFO>() as u32,
                        ..Default::default()
                    };
                    GetComboBoxInfo(combo, &mut info);
                    let mut edit = RECT::default();
                    GetWindowRect(info.hwndItem, &mut edit);
                    MapWindowPoints(
                        std::ptr::null_mut(),
                        combo,
                        (&mut edit as *mut RECT).cast(),
                        2,
                    );
                    let top = edit.top - info.rcItem.top;
                    let bottom = info.rcItem.bottom - edit.bottom;
                    assert!(
                        (top - bottom).abs() <= 1,
                        "dpi={dpi}, search={search}, top={top}, bottom={bottom}"
                    );
                    assert!(
                        edit.bottom - edit.top >= font.height() - 2,
                        "text clipped at dpi={dpi}"
                    );
                    send(combo, CB_SHOWDROPDOWN, 0, 0);
                    restore(combo);
                }
                send(
                    combo,
                    WM_SETFONT,
                    GetStockObject(DEFAULT_GUI_FONT) as usize,
                    0,
                );
            }
            DestroyWindow(root);
        }
    }
    #[test]
    #[ignore = "uses native dropdown navigation to verify Escape restores and Enter confirms"]
    fn browsing_with_arrows_does_not_commit_until_confirmed() {
        unsafe extern "system" fn parent(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
            if msg == WM_COMMAND {
                notification(lp as HWND, (wp >> 16) as u32);
                return 0;
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        unsafe {
            let root = CreateWindowExW(
                0,
                w!("STATIC"),
                w!("Dropdown transaction test"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                600,
                400,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            SetWindowLongPtrW(root, GWLP_WNDPROC, parent as *const () as isize);
            let combo = CreateWindowExW(
                0,
                w!("COMBOBOX"),
                w!(""),
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWN as u32 | WS_VSCROLL,
                10,
                10,
                300,
                240,
                root,
                1 as HMENU,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            attach(combo, true);
            populate(
                combo,
                &[
                    ("old".into(), "Old selection".into()),
                    ("new".into(), "New selection".into()),
                ],
                "old",
            );
            let mut info = COMBOBOXINFO {
                cbSize: size_of::<COMBOBOXINFO>() as u32,
                ..Default::default()
            };
            GetComboBoxInfo(combo, &mut info);
            for confirm in [false, true] {
                SendMessageW(combo, CB_SHOWDROPDOWN, 1, 0);
                SendMessageW(combo, WM_KEYDOWN, 0x28, 1);
                assert_eq!(selected_index(combo), Some(0));
                SendMessageW(info.hwndItem, WM_KEYDOWN, if confirm { 13 } else { 27 }, 1);
                assert_eq!(selected_index(combo), Some(usize::from(confirm)));
            }
            DestroyWindow(root);
        }
    }
    #[test]
    fn searches_values_labels_and_multiple_unicode_terms() {
        let options = vec!["1/4 左上区域".into(), "custom 工作区 二".into()];
        assert_eq!(matching(&options, "1/4"), [0]);
        assert_eq!(matching(&options, "CUSTOM 二"), [1]);
        assert!(matching(&options, "missing").is_empty());
    }
    #[test]
    #[ignore = "opens a temporary searchable dropdown and verifies stable item identity, no-match and cancel"]
    fn filtered_native_combo_keeps_original_value_and_restores_on_cancel() {
        unsafe {
            let root = CreateWindowExW(
                0,
                w!("STATIC"),
                w!("Combo search test"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                600,
                400,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            let combo = CreateWindowExW(
                0,
                w!("COMBOBOX"),
                w!(""),
                WS_CHILD | WS_VISIBLE | CBS_DROPDOWN as u32 | WS_VSCROLL,
                10,
                10,
                300,
                240,
                root,
                1 as HMENU,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            attach(combo, true);
            let mut options = (0..400)
                .map(|n| (format!("region{n}"), format!("区域 {n}")))
                .collect::<Vec<_>>();
            options.push(("special".into(), "特殊分区".into()));
            populate(combo, &options, "region10");
            let full = SEARCHES.with_borrow(|all| all[&(combo as usize)].visible.clone().unwrap());
            notification(combo, CBN_CLOSEUP);
            assert!(
                SEARCHES.with_borrow(|all| Rc::ptr_eq(
                    &full,
                    all[&(combo as usize)].visible.as_ref().unwrap()
                )),
                "closing an unchanged list must not rebuild it"
            );
            SetWindowTextW(combo, wide("特殊").as_ptr());
            filter(combo);
            let filtered =
                SEARCHES.with_borrow(|all| all[&(combo as usize)].visible.clone().unwrap());
            filter(combo);
            assert!(
                SEARCHES.with_borrow(|all| Rc::ptr_eq(
                    &filtered,
                    all[&(combo as usize)].visible.as_ref().unwrap()
                )),
                "unchanged matches must reuse their list"
            );
            assert_eq!(send(combo, CB_GETCOUNT, 0, 0), 1);
            assert_eq!(selected_index(combo), Some(10));
            send(combo, CB_SETCURSEL, 0, 0);
            assert!(notification(combo, CBN_SELCHANGE));
            assert_eq!(selected_index(combo), Some(10));
            assert!(notification(combo, CBN_SELENDOK));
            assert_eq!(selected_index(combo), Some(400));
            notification(combo, CBN_CLOSEUP);
            assert_eq!(text(combo), "特殊分区");
            SetWindowTextW(combo, wide("not found").as_ptr());
            filter(combo);
            assert_eq!(send(combo, CB_GETCOUNT, 0, 0), 1);
            assert_eq!(original_index(combo, 0), None);
            assert_eq!(selected_index(combo), Some(400));
            notification(combo, CBN_SELENDCANCEL);
            assert_eq!(text(combo), "特殊分区");
            assert_eq!(send(combo, CB_GETCOUNT, 0, 0), 401);
            // A configuration refresh can change the selection without a user notification.
            send(combo, CB_SETCURSEL, 20, 0);
            SetWindowTextW(combo, wide("missing again").as_ptr());
            filter(combo);
            notification(combo, CBN_SELENDCANCEL);
            assert_eq!(selected_index(combo), Some(20));
            assert_eq!(text(combo), "区域 20");
            DestroyWindow(root);
            assert!(!SEARCHES.with_borrow(|all| all.contains_key(&(combo as usize))));
        }
    }
}
