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
pub const MENU: u32 = WM_APP + 42;

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

fn add(hwnd: HWND, icon: HICON) {
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
        if Shell_NotifyIconW(NIM_ADD, &data) != 0 {
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &data);
        }
    }
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
        add(hwnd, icon as HICON);
    }
    if message == CALLBACK {
        let event = lp as u32 & 0xffff;
        if matches!(event, WM_CONTEXTMENU | WM_RBUTTONUP | NIN_SELECT) || event == NIN_SELECT + 1 {
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
            add(hwnd, icon);
            Ok(Self {
                hwnd,
                icon,
                taskbar_created: message as usize,
            })
        }
    }

    pub fn menu(&self, cfg: &Config, anchor: isize) -> Result<Option<&'static str>, String> {
        let entries = [
            ("settings", cfg.tr("Settings…", "设置…")),
            ("help", cfg.tr("Help", "帮助")),
            ("reload", cfg.tr("Reload configuration", "重载配置")),
            ("version", cfg.tr("Version", "版本")),
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
                if AppendMenuW(menu, MF_STRING, index + 1, wide(label).as_ptr()) == 0 {
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
