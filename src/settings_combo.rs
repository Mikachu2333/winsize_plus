//! Searchable native combos keep item identity separate from the filtered row index.
use crate::desktop::wide;
use std::{cell::RefCell, collections::HashMap};
use windows_sys::{
    Win32::{
        Foundation::*,
        UI::{Controls::*, Shell::*, WindowsAndMessaging::*},
    },
    core::w,
};

#[derive(Clone)]
struct Search {
    options: Vec<(String, String)>,
    committed: Option<usize>,
    query: String,
    busy: bool,
    composing: bool,
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
fn matching(options: &[(String, String)], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    options
        .iter()
        .enumerate()
        .filter_map(|(i, (value, label))| {
            let candidate = format!("{value} {label}").to_lowercase();
            query
                .split_whitespace()
                .all(|word| candidate.contains(word))
                .then_some(i)
        })
        .collect()
}
pub fn attach(combo: HWND) {
    unsafe {
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
                w!("Search / 搜索") as isize,
            );
        }
    }
}
pub fn populate(combo: HWND, options: &[(String, String)], selected: &str) {
    SEARCHES.with_borrow_mut(|all| {
        all.insert(
            combo as usize,
            Search {
                options: options.to_vec(),
                committed: options
                    .iter()
                    .position(|(v, _)| v.eq_ignore_ascii_case(selected)),
                query: String::new(),
                busy: false,
                composing: false,
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
    send(combo, WM_SETREDRAW, 1, 0);
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
        s.query.clear();
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
    let state = SEARCHES.with_borrow(|all| {
        all.get(&(combo as usize))
            .map(|s| (s.query.is_empty(), s.committed))
    });
    state.and_then(|(empty, committed)| {
        if empty {
            original_index(combo, send(combo, CB_GETCURSEL, 0, 0)).or(committed)
        } else {
            committed
        }
    })
}
fn commit(combo: HWND) {
    let index = original_index(combo, send(combo, CB_GETCURSEL, 0, 0));
    if let Some(index) = index {
        SEARCHES.with_borrow_mut(|all| {
            if let Some(s) = all.get_mut(&(combo as usize)) {
                s.committed = Some(index);
                s.query.clear();
            }
        });
    }
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
        Some(s.clone())
    });
    let Some(search) = search else {
        return;
    };
    let indices = matching(&search.options, &query);
    rebuild(combo, &search, &indices);
    send(combo, CB_SHOWDROPDOWN, usize::from(!indices.is_empty()), 0);
    unsafe {
        SetWindowTextW(combo, wide(query).as_ptr());
    }
    send(combo, CB_SETEDITSEL, 0, selection);
    finish_rebuild(combo);
}
/// Returns true for search-only changes which must not become configuration edits.
pub fn notification(combo: HWND, event: u32) -> bool {
    let state = SEARCHES.with_borrow(|all| {
        all.get(&(combo as usize))
            .map(|s| (s.busy, s.query.is_empty()))
    });
    let Some((busy, empty)) = state else {
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
            commit(combo);
            false
        }
        CBN_SELENDCANCEL | CBN_KILLFOCUS => {
            if !empty {
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
        if msg == CB_SETCURSEL {
            let result = DefSubclassProc(hwnd, msg, wp, lp);
            let original = original_index(hwnd, result);
            SEARCHES.with_borrow_mut(|all| {
                if let Some(search) = all.get_mut(&(hwnd as usize))
                    && !search.busy
                    && search.query.is_empty()
                {
                    search.committed = original;
                }
            });
            return result;
        }
        if msg == WM_NCDESTROY {
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
                commit(combo);
                PostMessageW(
                    GetParent(combo),
                    WM_COMMAND,
                    (GetDlgCtrlID(combo) as usize & 0xffff) | ((CBN_SELCHANGE as usize) << 16),
                    combo as isize,
                );
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
    fn searches_values_labels_and_multiple_unicode_terms() {
        let options = vec![
            ("1/4".into(), "左上区域".into()),
            ("custom".into(), "工作区 二".into()),
        ];
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
            attach(combo);
            let mut options = (0..400)
                .map(|n| (format!("region{n}"), format!("区域 {n}")))
                .collect::<Vec<_>>();
            options.push(("special".into(), "特殊分区".into()));
            populate(combo, &options, "region10");
            SetWindowTextW(combo, wide("特殊").as_ptr());
            filter(combo);
            assert_eq!(send(combo, CB_GETCOUNT, 0, 0), 1);
            assert_eq!(selected_index(combo), Some(10));
            send(combo, CB_SETCURSEL, 0, 0);
            assert!(!notification(combo, CBN_SELCHANGE));
            assert_eq!(selected_index(combo), Some(400));
            notification(combo, CBN_CLOSEUP);
            assert_eq!(text(combo), "特殊分区");
            SetWindowTextW(combo, wide("not found").as_ptr());
            filter(combo);
            assert_eq!(send(combo, CB_GETCOUNT, 0, 0), 0);
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
