use crate::{config::Config, desktop::err};
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
                || self.pressed & self.gesture_mask != 0;
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
        if matches!(key, LEFT | RIGHT) {
            let side = if key == LEFT { 1 } else { 2 };
            if down {
                if self.pressed & side != 0 {
                    return self.second;
                }
                if self.pressed & self.gesture_mask == 0 && side & self.gesture_mask != 0 {
                    self.gesture_target = unsafe { GetForegroundWindow() };
                    if self.preview_enabled {
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
                self.tap = (clean && self.alt_mask & side != 0).then_some((key, time));
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
        if self.mode == Mode::Idle {
            return false;
        }
        let eat = match self.mode {
            Mode::Wait if time.wrapping_sub(self.armed_at) > self.wait_ms => {
                self.mode = Mode::Idle;
                false
            }
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
            let key = if (data.vkCode == VK_PACKET.into()
                && matches!(data.scanCode, 0x60 | 0x7e | 0xff5e))
                || (data.vkCode == VK_PROCESSKEY.into() && data.scanCode == 0x29)
            {
                VK_OEM_3.into()
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
                        WM_MOUSEMOVE if input.guides || input.preview_tracking => {
                            input.point = data.pt;
                            if !input.queued {
                                input.queued = true;
                                post(MOVE, 0, 0)
                            }
                            false
                        }
                        WM_RBUTTONDOWN if input.pressed & input.gesture_mask != 0 => {
                            input.tap = None;
                            input.last_tap = None;
                            input.click = true;
                            let hwnd = input.gesture_target;
                            let coords =
                                (data.pt.x as u32 as u64) | ((data.pt.y as u32 as u64) << 32);
                            CLICK_POS.with(|point| point.set((data.pt, hwnd)));
                            post(CLICK, hwnd as usize, coords as isize);
                            true
                        }
                        WM_RBUTTONUP if input.click => {
                            input.click = false;
                            let _ = input.update_mouse();
                            true
                        }
                        WM_LBUTTONDOWN | WM_MBUTTONDOWN | WM_RBUTTONDOWN => {
                            input.tap = None;
                            input.last_tap = None;
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
pub fn is_waiting() -> bool {
    INPUT.with_borrow(|input| input.mode == Mode::Wait)
}
pub fn cancel() {
    INPUT.with_borrow_mut(|input| input.mode = Mode::Idle)
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
