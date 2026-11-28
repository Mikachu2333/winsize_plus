struct SearchEditorReset;
impl Drop for SearchEditorReset {
    fn drop(&mut self) {
        EDITOR.with_borrow_mut(|slot| {
            slot.take();
        });
    }
}
fn search_editor(tab: usize, name: &str) -> (SearchEditorReset, HWND, HWND) {
    let editor = test_editor(tab, name);
    let root = editor.window.0;
    unsafe {
        SetWindowLongPtrW(root, GWLP_WNDPROC, window_proc as *const () as isize);
        SetWindowLongW(
            root,
            GWL_STYLE,
            (GetWindowLongW(root, GWL_STYLE) as u32 | WS_CLIPCHILDREN) as i32,
        );
    }
    EDITOR.with_borrow_mut(|slot| *slot = Some(editor));
    with_editor(|e| {
        e.live = false;
        e.init_search()?;
        e.layout()
    });
    unsafe {
        ShowWindow(root, SW_SHOW);
        SetForegroundWindow(root);
    }
    let edit = EDITOR.with_borrow(|slot| slot.as_ref().unwrap().search.edit);
    unsafe {
        SetFocus(edit);
    }
    crate::test_support::pump();
    (SearchEditorReset, root, edit)
}

#[test]
#[ignore = "opens temporary settings controls and uses an isolated configuration"]
fn search_wheel_with_focus_scrolls_content() {
    let _dpi = crate::test_support::Dpi::new();
    let _hooks = input::Hooks::escape_only().unwrap();
    let (_reset, root, edit) = search_editor(4, "settings-search-wheel-test");
    let (before, pane, extent) = EDITOR.with_borrow(|slot| {
        let e = slot.as_ref().unwrap();
        (e.scroll, e.panes[1], e.extent[1])
    });
    let mut r = RECT::default();
    unsafe {
        GetWindowRect(pane, &mut r);
    }
    assert!(extent > r.bottom - r.top);
    let x = (r.left + r.right) / 2;
    let y = (r.top + r.bottom) / 2;
    let lp = ((y as u16 as usize) << 16 | x as u16 as usize) as isize;
    send(edit, WM_MOUSEWHEEL, (-120i16 as u16 as usize) << 16, lp);
    crate::test_support::pump();
    let after = EDITOR.with_borrow(|slot| slot.as_ref().unwrap().scroll);
    println!("wheel before={before:?} after={after:?} content_extent={extent}");
    assert!(
        after[1] > before[1],
        "Wheel over right content was not delivered to the right pane"
    );
    assert_eq!(after[0], before[0]);
    send(root, WM_MOUSEWHEEL, 120usize << 16, lp);
    assert_eq!(
        EDITOR.with_borrow(|slot| slot.as_ref().unwrap().scroll),
        before
    );
    assert_eq!(unsafe { GetFocus() }, edit);
}

#[test]
#[ignore = "opens temporary settings controls and uses an isolated configuration"]
fn search_timer_preserves_new_focus() {
    let _dpi = crate::test_support::Dpi::new();
    let _hooks = input::Hooks::escape_only().unwrap();
    let (_reset, root, edit) = search_editor(0, "settings-search-focus-test");
    set_text(edit, "language");
    let other = EDITOR.with_borrow(|slot| {
        slot.as_ref()
            .unwrap()
            .rows
            .iter()
            .find(|row| row.field.key == "language")
            .unwrap()
            .c(0)
    });
    assert!(!other.is_null());
    unsafe {
        SetFocus(other);
    }
    assert_eq!(unsafe { GetFocus() }, other);
    // Also cover WM_TIMER already queued before KillTimer.
    send(root, WM_TIMER, SEARCH_TIMER, 0);
    assert_eq!(
        unsafe { GetFocus() },
        other,
        "Delayed search reclaimed focus after user left search"
    );
    EDITOR.with_borrow(|slot| assert!(slot.as_ref().unwrap().search.query.is_empty()));
    unsafe {
        SetFocus(edit);
    }
    send(root, WM_TIMER, SEARCH_TIMER, 0);
    EDITOR.with_borrow(|slot| {
        let e = slot.as_ref().unwrap();
        assert_eq!(e.search.query, "language");
        assert!(e.search.visible);
    });
}

#[test]
#[ignore = "opens temporary settings controls and uses an isolated configuration"]
fn search_empty_results_wheel_returns() {
    let _dpi = crate::test_support::Dpi::new();
    let _hooks = input::Hooks::escape_only().unwrap();
    let (_reset, _root, edit) = search_editor(0, "settings-search-empty-test");
    set_text(edit, "noresultzzzzzzzz");
    with_editor(|e| e.run_search());
    let list = EDITOR.with_borrow(|slot| slot.as_ref().unwrap().search.list);
    let mut r = RECT::default();
    unsafe {
        GetWindowRect(list, &mut r);
    }
    let x = (r.left + r.right) / 2;
    let y = (r.top + r.bottom) / 2;
    let lp = ((y as u16 as usize) << 16 | x as u16 as usize) as isize;
    send(edit, WM_MOUSEWHEEL, (-120i16 as u16 as usize) << 16, lp);
    crate::test_support::pump();
    assert_eq!(send(list, LB_GETTOPINDEX, 0, 0), 0);
    assert_eq!(unsafe { GetFocus() }, edit);
    EDITOR.with_borrow(|slot| assert!(slot.as_ref().unwrap().search.matches.is_empty()));
}

#[test]
#[ignore = "opens temporary settings controls and uses an isolated configuration"]
fn search_accepting_result_restores_enabled_preview() {
    let _dpi = crate::test_support::Dpi::new();
    let _hooks = input::Hooks::escape_only().unwrap();
    let (_reset, _root, edit) = search_editor(3, "settings-search-preview-test");
    with_editor(|e| {
        e.live = true;
        e.update_values();
        e.render()
    });
    EDITOR.with_borrow(|slot| assert!(slot.as_ref().unwrap().preview.visible()));
    set_text(edit, "fill_opacity");
    for finish in [13, 27] {
        with_editor(|e| e.run_search());
        EDITOR.with_borrow(|slot| assert!(!slot.as_ref().unwrap().preview.visible()));
        with_editor(|e| e.search_key(finish));
        EDITOR.with_borrow(|slot| assert!(slot.as_ref().unwrap().preview.visible()));
    }
    EDITOR.with_borrow(|slot| {
        let e = slot.as_ref().unwrap();
        let checkbox = e
            .rows
            .iter()
            .find(|row| matches!(row.field.kind, Kind::Preview))
            .unwrap()
            .c(0);
        assert!(!e.search.visible);
        assert!(e.live);
        assert_eq!(send(checkbox, BM_GETCHECK, 0, 0), 1);
        assert!(
            e.preview.visible(),
            "Search accepted, preview checkbox is enabled, but preview stays hidden"
        );
    });
}
