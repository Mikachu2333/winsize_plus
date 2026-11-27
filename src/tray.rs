use crate::{
    config::Config,
    desktop::{self, err, wide},
};
use std::ptr::{null, null_mut};
use windows_sys::{
    Win32::{
        Foundation::*,
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::w,
};

pub const APP_ICON: *const u16 = std::ptr::without_provenance(1);
const CALLBACK: u32 = WM_APP + 41;
// NIN_KEYSELECT is an SDK macro not exposed by windows-sys.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
pub const MENU: u32 = WM_APP + 42;
struct MenuTick<'a> {
    tick: &'a mut dyn FnMut(),
    busy: bool,
}
unsafe extern "system" fn menu_tick(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    // SAFETY: the stack context is owned by menu until its synchronous
    // TrackPopupMenu loop exits. Reentrant timer messages never alias its closure.
    if message == WM_TIMER && wp == id {
        let state = data as *mut MenuTick<'_>;
        unsafe {
            if !(*state).busy {
                (*state).busy = true;
                ((*state).tick)();
                (*state).busy = false;
            }
        }
        return 0;
    }
    unsafe { DefSubclassProc(hwnd, message, wp, lp) }
}
struct MenuPump(HWND, usize);
impl Drop for MenuPump {
    fn drop(&mut self) {
        unsafe {
            KillTimer(self.0, self.1);
            RemoveWindowSubclass(self.0, Some(menu_tick), self.1);
        }
    }
}

fn icon() -> Result<HICON, String> {
    let handle = unsafe {
        LoadImageW(
            GetModuleHandleW(null()),
            APP_ICON,
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR,
        )
    };
    if handle.is_null() {
        Err(err())
    } else {
        Ok(handle)
    }
}

fn notify_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    }
}

fn add(hwnd: HWND, icon: HICON) -> Result<(), String> {
    let mut data = notify_data(hwnd);
    data.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP | NIF_MESSAGE;
    data.uCallbackMessage = CALLBACK;
    data.hIcon = icon;
    let mut tip = [0u16; 128];
    for (unit, value) in tip
        .iter_mut()
        .zip(concat!("WinSizeP ", env!("CARGO_PKG_VERSION")).encode_utf16())
    {
        *unit = value;
    }
    data.szTip = tip;
    unsafe {
        desktop::check(Shell_NotifyIconW(NIM_ADD, &data))?;
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &data);
    }
    Ok(())
}

unsafe extern "system" fn taskbar_event(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    taskbar_created: usize,
    icon: usize,
) -> LRESULT {
    if message as usize == taskbar_created {
        let _ = add(hwnd, icon as HICON);
    }
    if message == CALLBACK {
        let event = lp as u32 & 0xffff;
        if matches!(event, WM_CONTEXTMENU | WM_RBUTTONUP | NIN_KEYSELECT) {
            let anchor = if event == WM_RBUTTONUP {
                -1
            } else {
                wp as isize
            };
            unsafe { PostThreadMessageW(GetCurrentThreadId(), MENU, 0, anchor) };
        }
        return 0;
    }
    unsafe { DefSubclassProc(hwnd, message, wp, lp) }
}

pub struct Tray {
    hwnd: HWND,
    icon: HICON,
    taskbar_created: usize,
}

impl Tray {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let message = RegisterWindowMessageW(w!("TaskbarCreated"));
            if message == 0 {
                return Err(err());
            }
            let icon = icon()?;
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("WinSizeP.Tray"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            if hwnd.is_null() {
                let problem = err();
                DestroyIcon(icon);
                return Err(problem);
            }
            if SetWindowSubclass(hwnd, Some(taskbar_event), message as usize, icon as usize) == 0 {
                let problem = err();
                DestroyWindow(hwnd);
                DestroyIcon(icon);
                return Err(problem);
            }
            ChangeWindowMessageFilterEx(hwnd, message, MSGFLT_ALLOW, std::ptr::null_mut());
            ChangeWindowMessageFilterEx(hwnd, CALLBACK, MSGFLT_ALLOW, null_mut());
            let tray = Self {
                hwnd,
                icon,
                taskbar_created: message as usize,
            };
            add(hwnd, icon)?;
            Ok(tray)
        }
    }

    pub fn menu(
        &self,
        cfg: &Config,
        paused: bool,
        anchor: isize,
        mut tick: impl FnMut(),
    ) -> Result<Option<&'static str>, String> {
        let mut context = MenuTick {
            tick: &mut tick,
            busy: false,
        };
        let timer = 0x5743usize;
        unsafe {
            desktop::check(SetWindowSubclass(
                self.hwnd,
                Some(menu_tick),
                timer,
                (&mut context as *mut MenuTick<'_>) as usize,
            ))?;
            if SetTimer(self.hwnd, timer, 16, None) == 0 {
                RemoveWindowSubclass(self.hwnd, Some(menu_tick), timer);
                return Err(err());
            }
        }
        let _pump = MenuPump(self.hwnd, timer);
        let entries = [
            (
                "pause",
                if paused {
                    cfg.tr("Enable", "启用")
                } else {
                    cfg.tr("Pause", "暂停")
                },
            ),
            ("", ""),
            (
                "mousecast",
                cfg.tr("Toggle mouse annotations", "切换鼠标标注"),
            ),
            (
                "keycast",
                cfg.tr("Toggle keyboard annotations", "切换键盘标注"),
            ),
            ("", ""),
            ("help", cfg.tr("Help", "帮助")),
            ("version", cfg.tr("Version", "版本")),
            ("", ""),
            ("settings", cfg.tr("Settings…", "设置…")),
            ("reload", cfg.tr("Reload configuration", "重载配置")),
            ("", ""),
            ("quit", cfg.tr("Exit", "退出")),
        ];
        let mut point = POINT {
            x: anchor as i16 as i32,
            y: (anchor >> 16) as i16 as i32,
        };
        unsafe {
            if point.x == -1 && point.y == -1 {
                let mut rect = RECT::default();
                let id = NOTIFYICONIDENTIFIER {
                    cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
                    hWnd: self.hwnd,
                    uID: 1,
                    ..Default::default()
                };
                point = if Shell_NotifyIconGetRect(&id, &mut rect) >= 0 {
                    POINT {
                        x: rect.left,
                        y: rect.top,
                    }
                } else {
                    desktop::cursor()?
                };
            }
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return Err(err());
            }
            for (index, (_, label)) in entries.iter().enumerate() {
                if AppendMenuW(
                    menu,
                    if label.is_empty() {
                        MF_SEPARATOR
                    } else {
                        MF_STRING
                    },
                    index + 1,
                    wide(label).as_ptr(),
                ) == 0
                {
                    let problem = err();
                    DestroyMenu(menu);
                    return Err(problem);
                }
            }
            SetForegroundWindow(self.hwnd);
            let id = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                self.hwnd,
                null(),
            );
            DestroyMenu(menu);
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
            Shell_NotifyIconW(NIM_SETFOCUS, &notify_data(self.hwnd));
            Ok(id
                .checked_sub(1)
                .and_then(|index| entries.get(index as usize))
                .map(|entry| entry.0))
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            RemoveWindowSubclass(self.hwnd, Some(taskbar_event), self.taskbar_created);
            Shell_NotifyIconW(NIM_DELETE, &notify_data(self.hwnd));
            DestroyIcon(self.icon);
            DestroyWindow(self.hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keyboard_and_context_activation_open_menu_but_left_click_does_not() {
        unsafe {
            let mut message = MSG::default();
            PeekMessageW(&mut message, null_mut(), MENU, MENU, PM_REMOVE);
            for event in [
                NIN_SELECT,
                WM_LBUTTONUP,
                NIN_KEYSELECT,
                WM_CONTEXTMENU,
                WM_RBUTTONUP,
            ] {
                let position = (40 << 16) | 30;
                taskbar_event(null_mut(), CALLBACK, position, event as isize, 0, 0);
                let received = PeekMessageW(&mut message, null_mut(), MENU, MENU, PM_REMOVE) != 0;
                assert_eq!(
                    received,
                    matches!(event, NIN_KEYSELECT | WM_CONTEXTMENU | WM_RBUTTONUP)
                );
                if received {
                    assert_eq!(
                        message.lParam,
                        if event == WM_RBUTTONUP {
                            -1
                        } else {
                            position as isize
                        }
                    );
                }
            }
        }
    }
    #[test]
    #[ignore = "opens a temporary tray menu and verifies animation while the menu remains open"]
    fn mouse_release_fades_while_tray_menu_is_open() {
        let _dpi = crate::test_support::Dpi::new();
        let _overlay = crate::overlay::Overlay::new().unwrap();
        let cfg = crate::test_support::cfg();
        let tray = Tray::new().unwrap();
        let mut cast = crate::cast::Visuals::new();
        let point = desktop::cursor().unwrap();
        cast.mouse(2, 0, point, &cfg).unwrap();
        let start = std::time::Instant::now();
        let mut released = false;
        let mut faded = false;
        let result = tray
            .menu(&cfg, false, -1, || {
                if !released {
                    cast.mouse(2, crate::mouse_cast::RELEASE, point, &cfg)
                        .unwrap();
                    released = true;
                }
                cast.tick(&cfg).unwrap();
                faded |= !cast.mouse_active();
                if faded || start.elapsed() > std::time::Duration::from_secs(3) {
                    unsafe {
                        EndMenu();
                    }
                }
            })
            .unwrap();
        assert!(result.is_none());
        assert!(
            released && faded,
            "The modal menu froze the released mouse indicator"
        );
        assert!(start.elapsed() < std::time::Duration::from_millis(1000));
    }
}
