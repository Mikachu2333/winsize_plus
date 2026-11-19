use crate::{
    config::{Config, Identity},
    desktop::{self, err},
};
use std::{
    cell::RefCell,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

pub const ARMED: u32 = WM_APP + 1;
pub const ACTION: u32 = WM_APP + 2;
pub const TYPE: u32 = WM_APP + 3;
pub const CLICK: u32 = WM_APP + 4;
pub const MOVE: u32 = WM_APP + 5;
pub const CANCEL: u32 = WM_APP + 6;
pub const ALT_HOLD: u32 = WM_APP + 7;
pub const ALT_RELEASE: u32 = WM_APP + 8;
pub const DRAG_START: u32 = WM_APP + 9;
pub const DRAG_END: u32 = WM_APP + 10;
pub const GRID_SHOW: u32 = WM_APP + 11;
const LEFT: u32 = VK_LMENU as u32;
const RIGHT: u32 = VK_RMENU as u32;

#[derive(Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Idle,
    Wait,
    Command,
}
#[derive(Default)]
struct Input {
    mode: Mode,
    alt_mask: u8,
    gesture_mask: u8,
    gesture_target: HWND,
    double_ms: u32,
    wait_ms: u32,
    last_tap: Option<(u32, u32)>,
    pressed: u8,
    tap: Option<(u32, u32)>,
    second: bool,
    armed_at: u32,
    bound: [u64; 4],
    command: [u64; 4],
    eaten: [u64; 4],
    guides: bool,
    preview_enabled: bool,
    preview_tracking: bool,
    mouse: HHOOK,
    click: bool,
    point: POINT,
    queued: bool,
    tip_visible: bool,
    target_allowed: bool,
    exclusions: Vec<Identity>,
    move_button: Option<u32>,
    resize_button: Option<u32>,
    drag: Option<PointerAction>,
}
#[derive(Clone, Copy)]
struct PointerAction {
    start: POINT,
    button: u32,
    resize: bool,
    draggable: bool,
    active: bool,
    canceled: bool,
}
thread_local! {static INPUT:RefCell<Input>=RefCell::new(Input::default());}
fn mask(text: &str) -> u8 {
    match text {
        "left" => 1,
        "right" => 2,
        "both" => 3,
        _ => 0,
    }
}
fn bit(key: u32) -> (usize, u64) {
    ((key / 64) as usize, 1u64 << (key % 64))
}
fn post(message: u32, wp: usize, lp: isize) {
    unsafe {
        PostThreadMessageW(GetCurrentThreadId(), message, wp, lp);
    }
}
impl Input {
    fn update_mouse(&mut self) -> Result<(), String> {
        unsafe {
            let needed = self.guides
                || self.preview_tracking
                || self.click
                || self.drag.is_some()
                || (self.target_allowed && self.pressed & self.gesture_mask != 0);
            if needed && self.mouse.is_null() {
                self.mouse =
                    SetWindowsHookExW(WH_MOUSE_LL, Some(mouse), GetModuleHandleW(null()), 0);
                if self.mouse.is_null() {
                    return Err(err());
                }
            }
            if !needed && !self.mouse.is_null() {
                UnhookWindowsHookEx(self.mouse);
                self.mouse = null_mut();
            }
            Ok(())
        }
    }
    fn keyboard(&mut self, key: u32, down: bool, time: u32) -> bool {
        if key >= 256 {
            return false;
        }
        let (index, flag) = bit(key);
        if self.eaten[index] & flag != 0 {
            if !down {
                self.eaten[index] &= !flag;
            }
            return true;
        }
        if down
            && key == VK_ESCAPE as u32
            && (self.tip_visible
                || self.preview_tracking
                || self.mode != Mode::Idle
                || self.drag.is_some())
        {
            self.mode = Mode::Idle;
            self.tap = None;
            self.last_tap = None;
            post(CANCEL, 0, 0);
            self.eaten[index] |= flag;
            return true;
        }
        if matches!(key, LEFT | RIGHT) {
            let side = if key == LEFT { 1 } else { 2 };
            if down {
                if self.pressed & side != 0 {
                    return self.second;
                }
                if self.pressed == 0 {
                    self.gesture_target = unsafe { GetForegroundWindow() };
                    self.target_allowed = desktop::allowed(self.gesture_target, &self.exclusions);
                }
                if self.pressed & self.gesture_mask == 0 && side & self.gesture_mask != 0 {
                    if self.preview_enabled && self.target_allowed {
                        post(ALT_HOLD, self.gesture_target as usize, 0);
                    }
                }
                self.pressed |= side;
                let clean = self.pressed == side
                    && unsafe {
                        [VK_CONTROL, VK_SHIFT, VK_LWIN, VK_RWIN]
                            .iter()
                            .all(|code| GetAsyncKeyState(*code as i32) >= 0)
                    };
                self.tap = (clean && self.target_allowed && self.alt_mask & side != 0)
                    .then_some((key, time));
                self.second = self.tap.is_some()
                    && self.last_tap.is_some_and(|(last, when)| {
                        last == key && time.wrapping_sub(when) <= self.double_ms
                    });
                self.last_tap = None;
                let _ = self.update_mouse();
                return self.second;
            }
            self.pressed &= !side;
            if self.preview_enabled
                && side & self.gesture_mask != 0
                && self.pressed & self.gesture_mask == 0
            {
                post(ALT_RELEASE, 0, 0);
            }
            let _ = self.update_mouse();
            let consumed = self.second;
            self.second = false;
            if let Some((pressed, when)) = self.tap.take()
                && pressed == key
                && time.wrapping_sub(when) <= self.double_ms
            {
                if consumed {
                    self.mode = Mode::Wait;
                    self.armed_at = time;
                    post(ARMED, unsafe { GetForegroundWindow() } as usize, 0)
                } else {
                    self.last_tap = Some((key, time))
                }
            }
            return consumed;
        }
        if !down {
            return false;
        }
        self.tap = None;
        self.last_tap = None;
        if matches!(key, 0x10 | 0xa0 | 0xa1) {
            return false;
        }
        if self.mode == Mode::Idle
            && self.target_allowed
            && self.pressed & self.alt_mask != 0
            && self.bound[index] & flag != 0
        {
            post(ARMED, self.gesture_target as usize, 0);
            post(ACTION, key as usize, 0);
            self.eaten[index] |= flag;
            if self.command[index] & flag != 0 {
                self.mode = Mode::Command;
            }
            return true;
        }
        if self.mode == Mode::Idle {
            return false;
        }
        let eat = match self.mode {
            Mode::Wait => {
                let bound = self.bound[index] & flag != 0;
                self.mode = if self.command[index] & flag != 0 {
                    Mode::Command
                } else {
                    Mode::Idle
                };
                if bound {
                    post(ACTION, key as usize, 0)
                } else {
                    post(CANCEL, 0, 0)
                }
                bound || key == VK_ESCAPE as u32
            }
            Mode::Command => {
                let shifted = unsafe { GetAsyncKeyState(VK_SHIFT as i32) < 0 };
                let code = match key {
                    0x41..=0x5a => key + 32,
                    0x30..=0x39 | 8 | 13 | 27 | 32 => key,
                    0xbd => {
                        if shifted {
                            95
                        } else {
                            45
                        }
                    }
                    _ => 0,
                };
                if code != 0 {
                    post(TYPE, code as usize, 0)
                }
                if matches!(code, 13 | 27) {
                    self.mode = Mode::Idle
                }
                true
            }
            Mode::Idle => false,
        };
        if eat {
            self.eaten[index] |= flag;
        }
        eat
    }
}
unsafe extern "system" fn keyboard(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION as i32 {
            let data = &*(lp as *const KBDLLHOOKSTRUCT);
            let down = matches!(wp as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let key = if data.vkCode == VK_PROCESSKEY.into() {
                match data.scanCode {
                    0x29 => 0xc0,
                    0x0c => 0xbd,
                    0x0d => 0xbb,
                    0x2b => 0xdc,
                    0x27 => 0xba,
                    0x33 => 0xbc,
                    0x34 => 0xbe,
                    0x35 => 0xbf,
                    _ => MapVirtualKeyW(data.scanCode, MAPVK_VSC_TO_VK_EX),
                }
            } else if data.vkCode == VK_PACKET.into() {
                char::from_u32(data.scanCode)
                    .and_then(|ch| crate::config::vk(&ch.to_string()))
                    .unwrap_or(data.vkCode)
            } else {
                data.vkCode
            };
            if INPUT.with(|cell| {
                cell.try_borrow_mut()
                    .is_ok_and(|mut input| input.keyboard(key, down, data.time))
            }) {
                return 1;
            }
        }
        CallNextHookEx(null_mut(), code, wp, lp)
    }
}
unsafe extern "system" fn mouse(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION as i32 {
            let data = &*(lp as *const MSLLHOOKSTRUCT);
            let eaten = INPUT.with(|cell| {
                cell.try_borrow_mut()
                    .is_ok_and(|mut input| match wp as u32 {
                        WM_MOUSEMOVE if input.guides || input.preview_tracking || input.drag.is_some() => {
                            input.point = data.pt;
                            if let Some(mut drag) = input.drag {
                                if drag.draggable && !drag.canceled && !drag.active && ((data.pt.x as i64 - drag.start.x as i64).abs() >= GetSystemMetrics(SM_CXDRAG) as i64 || (data.pt.y as i64 - drag.start.y as i64).abs() >= GetSystemMetrics(SM_CYDRAG) as i64) {
                                    drag.active = true;
                                    DRAG_ORIGIN.with(|origin| origin.set(drag.start));
                                    post(DRAG_START, input.gesture_target as usize, isize::from(drag.resize));
                                    input.drag = Some(drag);
                                }
                            }
                            if !input.queued {
                                input.queued = true;
                                post(MOVE, 0, 0)
                            }
                            false
                        }
                        message if mouse_button(message, data.mouseData).is_some() => {
                            let (button, down) = mouse_button(message, data.mouseData).unwrap();
                            if !down && input.drag.is_some_and(|drag| drag.button == button) {
                                let drag = input.drag.take().unwrap();
                                input.point = data.pt;
                                if drag.canceled { /* Consume the matching release without a click. */ }
                                else if drag.active { post(DRAG_END, 0, 0); }
                                else if button == 2 {
                                    CLICK_POS.with(|point| point.set((data.pt, input.gesture_target)));
                                    post(CLICK, input.gesture_target as usize, pack(data.pt));
                                }
                                let _ = input.update_mouse();
                                return true;
                            }
                            if !down { return false; }
                            input.tap = None;
                            input.last_tap = None;
                            if input.target_allowed && input.pressed & input.gesture_mask != 0 && input.drag.is_none() {
                                let resize = input.resize_button == Some(button);
                                let draggable = resize || input.move_button == Some(button);
                                if draggable || button == 2 {
                                    if button == 2 { post(GRID_SHOW, input.gesture_target as usize, 0); }
                                    input.drag = Some(PointerAction { start: data.pt, button, resize, draggable, active: false, canceled: false });
                                    return true;
                                }
                            }
                            false
                        }
                        _ => false,
                    })
            });
            if eaten {
                return 1;
            }
        }
        CallNextHookEx(null_mut(), code, wp, lp)
    }
}
thread_local! {static CLICK_POS:std::cell::Cell<(POINT,HWND)>=Default::default();}
thread_local! {static DRAG_ORIGIN:std::cell::Cell<POINT>=Default::default();}
pub fn drag_origin() -> POINT {
    DRAG_ORIGIN.with(|origin| origin.get())
}
fn pack(point: POINT) -> isize {
    ((point.x as u32 as u64) | ((point.y as u32 as u64) << 32)) as isize
}
fn mouse_button(message: u32, data: u32) -> Option<(u32, bool)> {
    Some(match message {
        WM_LBUTTONDOWN => (1, true),
        WM_LBUTTONUP => (1, false),
        WM_RBUTTONDOWN => (2, true),
        WM_RBUTTONUP => (2, false),
        WM_MBUTTONDOWN => (4, true),
        WM_MBUTTONUP => (4, false),
        WM_XBUTTONDOWN | WM_XBUTTONUP => (
            if data >> 16 == XBUTTON1 as u32 { 5 } else { 6 },
            message == WM_XBUTTONDOWN,
        ),
        _ => return None,
    })
}
pub fn tip_visible(visible: bool) {
    INPUT.with_borrow_mut(|input| input.tip_visible = visible);
}
pub fn click_point(packed: isize) -> POINT {
    if usize::BITS >= 64 {
        POINT {
            x: packed as i32,
            y: ((packed as u64) >> 32) as i32,
        }
    } else {
        CLICK_POS.with(|point| point.get().0)
    }
}
pub fn moved() -> POINT {
    INPUT.with_borrow_mut(|input| {
        input.queued = false;
        input.point
    })
}
pub fn cancel() {
    INPUT.with_borrow_mut(|input| {
        input.mode = Mode::Idle;
        if let Some(drag) = &mut input.drag {
            drag.canceled = true;
        }
    })
}
pub fn command_mode() {
    INPUT.with_borrow_mut(|input| input.mode = Mode::Command);
}
pub fn set_guides(on: bool) -> Result<(), String> {
    INPUT.with_borrow_mut(|input| {
        input.guides = on;
        input.update_mouse()
    })
}
pub fn track_preview(on: bool) -> Result<(), String> {
    INPUT.with_borrow_mut(|input| {
        input.preview_tracking = on;
        input.update_mouse()
    })
}
pub fn configure(cfg: &Config) -> Result<(), String> {
    INPUT.with_borrow_mut(|input| {
        input.mode = Mode::Idle;
        input.tap = None;
        input.last_tap = None;
        input.alt_mask = mask(&cfg.alt);
        input.gesture_mask = mask(&cfg.gesture);
        input.preview_enabled = cfg.preview;
        input.exclusions = cfg.exclusions.clone();
        input.move_button = cfg.move_button;
        input.resize_button = cfg.resize_button;
        input.double_ms = cfg.double_ms;
        input.wait_ms = cfg.wait_ms;
        input.command = [0; 4];
        input.bound = [0; 4];
        for (key, action) in &cfg.keys {
            let (index, flag) = bit(*key);
            input.bound[index] |= flag;
            if action == "command" {
                input.command[index] |= flag;
            }
        }
        input.update_mouse()
    })
}
pub struct Hooks(HHOOK);
impl Hooks {
    pub fn escape_only() -> Result<Self, String> {
        let hook = unsafe {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), GetModuleHandleW(null()), 0)
        };
        if hook.is_null() {
            Err(err())
        } else {
            Ok(Self(hook))
        }
    }
    pub fn start(cfg: &Config) -> Result<Self, String> {
        configure(cfg)?;
        let hook = unsafe {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), GetModuleHandleW(null()), 0)
        };
        if hook.is_null() {
            Err(err())
        } else {
            Ok(Self(hook))
        }
    }
}
impl Drop for Hooks {
    fn drop(&mut self) {
        unsafe {
            UnhookWindowsHookEx(self.0);
            INPUT.with_borrow_mut(|input| {
                if !input.mouse.is_null() {
                    UnhookWindowsHookEx(input.mouse);
                }
                *input = Input::default();
            });
        }
    }
}
