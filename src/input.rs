use crate::{
    config::{Config, Identity},
    desktop::{self, err},
};
use std::{
    cell::{Cell, RefCell},
    ptr::{null, null_mut},
    sync::mpsc,
    thread::JoinHandle,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::*,
    System::{LibraryLoader::GetModuleHandleW, Threading::*},
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
pub const DISPLAY_CHANGED: u32 = WM_APP + 12;
pub fn display_changed() {
    desktop::invalidate_displays();
    post(DISPLAY_CHANGED, 0, 0);
}
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
    last_tap: Option<(u32, u32)>,
    pressed: u8,
    tap: Option<(u32, u32)>,
    second: bool,
    bound: [u64; 4],
    command: [u64; 4],
    eaten: [u64; 4],
    guides: bool,
    preview_enabled: bool,
    preview_tracking: bool,
    mouse: HHOOK,
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
thread_local! {
    static EVENTS: RefCell<Option<(mpsc::Sender<QueuedEvent>, usize)>> = const { RefCell::new(None) };
    static INBOX: RefCell<Option<mpsc::Receiver<QueuedEvent>>> = const { RefCell::new(None) };
    static CURRENT: Cell<Option<(u32, POINT)>> = const { Cell::new(None) };
    static DRIVER: RefCell<Option<Driver>> = const { RefCell::new(None) };
}
const WAKE: u32 = WM_APP + 90;
struct QueuedEvent {
    message: u32,
    wp: usize,
    lp: isize,
    point: Option<POINT>,
}
/// Consume owned event data only in the outer application loop. A native modal
/// loop may dispatch the HWND wakeup, but cannot discard the channel's contents.
pub fn next_event() -> Option<MSG> {
    let event = INBOX.with_borrow(|inbox| inbox.as_ref()?.try_recv().ok())?;
    // Leave a wakeup for any backlog after an outer loop's bounded drain, even
    // when a modal loop already consumed all original wakeup messages.
    DRIVER.with_borrow(|driver| {
        if let Some(driver) = driver {
            unsafe {
                PostMessageW(driver.window, WAKE, 0, 0);
            }
        }
    });
    CURRENT.with(|current| current.set(event.point.map(|point| (event.message, point))));
    Some(MSG {
        message: event.message,
        wParam: event.wp,
        lParam: event.lp,
        ..Default::default()
    })
}
type Job = Box<dyn FnOnce() + Send>;
enum Control {
    Run(Job),
    Stop,
}
struct Driver {
    commands: mpsc::Sender<Control>,
    wake: HANDLE,
    window: HWND,
}
fn on_hook<R: Send + 'static>(action: impl FnOnce() -> R + Send + 'static) -> Result<R, String> {
    DRIVER.with_borrow(|driver| {
        let Some(driver) = driver else {
            return Ok(action());
        };
        let (send, receive) = mpsc::sync_channel(1);
        driver
            .commands
            .send(Control::Run(Box::new(move || {
                let _ = send.send(action());
            })))
            .map_err(|_| "Input thread stopped".to_owned())?;
        // SAFETY: Hooks owns the event until the worker has joined; no window messages
        // or UI-owned references cross this channel.
        desktop::check(unsafe { SetEvent(driver.wake) })?;
        receive
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| "Input thread did not respond".into())
    })
}
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
    post_event(message, wp, lp, None);
}
fn post_event(message: u32, wp: usize, lp: isize, point: Option<POINT>) -> bool {
    EVENTS.with_borrow(|events| {
        let Some((send, hwnd)) = events else {
            return false;
        };
        if send
            .send(QueuedEvent {
                message,
                wp,
                lp,
                point,
            })
            .is_err()
        {
            return false;
        }
        // Only integers cross the window-message boundary. On queue saturation the
        // next outer-loop drain still reads the owned event; MOVE can retry waking.
        unsafe { PostMessageW(*hwnd as HWND, WAKE, 0, 0) != 0 }
    })
}
impl Input {
    fn update_mouse(&mut self) -> Result<(), String> {
        unsafe {
            let needed = self.guides
                || self.preview_tracking
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
            if let Some(drag) = &mut self.drag {
                drag.canceled = true;
            }
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
                if self.pressed & (self.gesture_mask | self.alt_mask) == 0
                    && side & (self.gesture_mask | self.alt_mask) != 0
                    && self.preview_enabled
                    && self.target_allowed
                {
                    post(ALT_HOLD, self.gesture_target as usize, 0);
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
                && side & (self.gesture_mask | self.alt_mask) != 0
                && self.pressed & (self.gesture_mask | self.alt_mask) == 0
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
                    post(ARMED, self.gesture_target as usize, 0)
                } else {
                    self.last_tap = Some((key, when))
                }
            }
            if !consumed && self.pressed & self.gesture_mask == 0 && self.drag.is_none() {
                self.target_allowed = false;
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
            // Ordinary Alt shortcuts (for example Alt+Tab) abandon the pending preview.
            if self.pressed != 0 {
                post(CANCEL, 0, 0);
            }
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
                // Every WinSizeP UI thread has its own hook. Let the other hook observe Esc
                // before consuming it, so resident tips and settings previews close together.
                if key == VK_ESCAPE as u32 {
                    CallNextHookEx(null_mut(), code, wp, lp);
                }
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
                            if let Some(mut drag) = input.drag
                                && drag.draggable && !drag.canceled && !drag.active && ((data.pt.x as i64 - drag.start.x as i64).abs() >= GetSystemMetrics(SM_CXDRAG) as i64 || (data.pt.y as i64 - drag.start.y as i64).abs() >= GetSystemMetrics(SM_CYDRAG) as i64) {
                                    drag.active = true;
                                    post_event(DRAG_START, input.gesture_target as usize, isize::from(drag.resize), Some(drag.start));
                                    input.drag = Some(drag);
                                }
                            if !input.queued {
                                input.queued = post_event(MOVE, 0, 0, None);
                            }
                            false
                        }
                        message if mouse_button(message, data.mouseData).is_some() => {
                            let (button, down) = mouse_button(message, data.mouseData).unwrap();
                            if !down && input.drag.is_some_and(|drag| drag.button == button) {
                                let drag = input.drag.take().unwrap();
                                input.point = data.pt;
                                if drag.canceled { /* Consume the matching release without a click. */ }
                                else if drag.active { post_event(DRAG_END, 0, 0, Some(data.pt)); }
                                else if button == 2 {
                                    post_event(CLICK, input.gesture_target as usize, 0, Some(data.pt));
                                }
                                let _ = input.update_mouse();
                                return true;
                            }
                            if !down { return false; }
                            if input.preview_tracking && input.pressed==0 {post(CANCEL,0,0);input.mode=Mode::Idle;}
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
pub fn drag_origin() -> POINT {
    CURRENT
        .with(Cell::get)
        .filter(|(kind, _)| *kind == DRAG_START)
        .map(|(_, point)| point)
        .unwrap_or_default()
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
    let _ = on_hook(move || INPUT.with_borrow_mut(|input| input.tip_visible = visible));
}
pub fn click_point() -> POINT {
    CURRENT
        .with(Cell::get)
        .filter(|(kind, _)| *kind == CLICK)
        .map(|(_, point)| point)
        .unwrap_or_default()
}
pub fn moved() -> POINT {
    if let Some((DRAG_END, point)) = CURRENT.with(Cell::get) {
        return point;
    }
    on_hook(|| {
        INPUT.with_borrow_mut(|input| {
            input.queued = false;
            input.point
        })
    })
    .unwrap_or_else(|_| desktop::cursor().unwrap_or_default())
}
pub fn cancel() {
    let _ = on_hook(|| {
        INPUT.with_borrow_mut(|input| {
            input.mode = Mode::Idle;
            if let Some(drag) = &mut input.drag {
                drag.canceled = true;
            }
        })
    });
}
pub fn alt_held() -> bool {
    on_hook(|| {
        INPUT.with_borrow(|input| input.pressed & (input.alt_mask | input.gesture_mask) != 0)
    })
    .unwrap_or(false)
}
pub fn command_mode() {
    let _ = on_hook(|| INPUT.with_borrow_mut(|input| input.mode = Mode::Command));
}
pub fn set_guides(on: bool) -> Result<(), String> {
    on_hook(move || {
        INPUT.with_borrow_mut(|input| {
            input.guides = on;
            input.update_mouse()
        })
    })?
}
pub fn track_preview(on: bool) -> Result<(), String> {
    on_hook(move || {
        INPUT.with_borrow_mut(|input| {
            input.preview_tracking = on;
            input.update_mouse()
        })
    })?
}
pub fn configure(cfg: &Config) -> Result<(), String> {
    let (alt, gesture, preview, exclusions, move_button, resize_button, double_ms) = (
        mask(&cfg.alt),
        mask(&cfg.gesture),
        cfg.preview,
        cfg.exclusions.clone(),
        cfg.move_button,
        cfg.resize_button,
        cfg.double_ms,
    );
    let keys = cfg.keys.clone();
    on_hook(move || {
        INPUT.with_borrow_mut(|input| {
            input.mode = Mode::Idle;
            input.tap = None;
            input.last_tap = None;
            input.alt_mask = alt;
            input.gesture_mask = gesture;
            input.preview_enabled = preview;
            input.exclusions = exclusions;
            input.move_button = move_button;
            input.resize_button = resize_button;
            input.double_ms = double_ms;
            input.command = [0; 4];
            input.bound = [0; 4];
            for (key, action) in &keys {
                let (index, flag) = bit(*key);
                input.bound[index] |= flag;
                if action == "command" {
                    input.command[index] |= flag;
                }
            }
            input.update_mouse()
        })
    })?
}
pub struct Hooks {
    worker: Option<JoinHandle<()>>,
    // Raw handles also prevent moving this UI-thread guard to another thread.
    wake: HANDLE,
    window: HWND,
}
impl Hooks {
    pub fn escape_only() -> Result<Self, String> {
        if DRIVER.with_borrow(Option::is_some) {
            return Err("Input hooks already running on this UI thread".into());
        }
        // A message-only window gives notifications a destination native modal
        // loops can dispatch, without executing application code reentrantly.
        let window = unsafe {
            CreateWindowExW(
                0,
                windows_sys::core::w!("STATIC"),
                null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            )
        };
        if window.is_null() {
            return Err(err());
        }
        let wake = unsafe { CreateEventW(null(), 0, 0, null()) };
        if wake.is_null() {
            unsafe {
                DestroyWindow(window);
            }
            return Err(err());
        }
        let (send_events, receive_events) = mpsc::channel();
        let destination = window as usize;
        EVENTS.with_borrow_mut(|events| *events = Some((send_events.clone(), destination)));
        let event = wake as usize;
        let (commands, receive) = mpsc::channel();
        let (ready, started) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("winsizep-input".into())
            .spawn(move || {
                EVENTS.with_borrow_mut(|events| *events = Some((send_events, destination)));
                // SAFETY: both hooks are installed, pumped and removed on this worker.
                unsafe {
                    let hook = SetWindowsHookExW(
                        WH_KEYBOARD_LL,
                        Some(keyboard),
                        GetModuleHandleW(null()),
                        0,
                    );
                    if hook.is_null() {
                        let _ = ready.send(Err(err()));
                        return;
                    }
                    let _ = ready.send(Ok(()));
                    'pump: loop {
                        match MsgWaitForMultipleObjectsEx(
                            1,
                            &(event as HANDLE),
                            INFINITE,
                            QS_ALLINPUT,
                            MWMO_INPUTAVAILABLE,
                        ) {
                            WAIT_OBJECT_0 => {
                                while let Ok(command) = receive.try_recv() {
                                    match command {
                                        Control::Run(action) => action(),
                                        Control::Stop => break 'pump,
                                    }
                                }
                            }
                            value if value == WAIT_OBJECT_0 + 1 => {
                                let mut msg = MSG::default();
                                for _ in 0..64 {
                                    if PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) == 0 {
                                        break;
                                    }
                                    if msg.message == WM_QUIT {
                                        break 'pump;
                                    }
                                    DispatchMessageW(&msg);
                                }
                            }
                            _ => break,
                        }
                    }
                    UnhookWindowsHookEx(hook);
                    INPUT.with_borrow_mut(|input| {
                        if !input.mouse.is_null() {
                            UnhookWindowsHookEx(input.mouse);
                        }
                        *input = Input::default();
                    });
                }
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                unsafe {
                    CloseHandle(wake);
                    DestroyWindow(window);
                }
                return Err(error.to_string());
            }
        };
        DRIVER.with_borrow_mut(|driver| {
            *driver = Some(Driver {
                commands,
                wake,
                window,
            })
        });
        INBOX.with_borrow_mut(|inbox| *inbox = Some(receive_events));
        let hooks = Self {
            worker: Some(worker),
            wake,
            window,
        };
        started
            .recv()
            .map_err(|_| "Input thread failed to start".to_owned())??;
        Ok(hooks)
    }
    pub fn start(cfg: &Config) -> Result<Self, String> {
        let hooks = Self::escape_only()?;
        configure(cfg)?;
        Ok(hooks)
    }
}
impl Drop for Hooks {
    fn drop(&mut self) {
        DRIVER.with_borrow_mut(|driver| {
            if let Some(driver) = driver.take() {
                let _ = driver.commands.send(Control::Stop);
                unsafe {
                    SetEvent(driver.wake);
                }
            }
        });
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        unsafe {
            CloseHandle(self.wake);
            DestroyWindow(self.window);
        }
        INBOX.with_borrow_mut(|inbox| {
            inbox.take();
        });
        CURRENT.with(|current| current.set(None));
        EVENTS.with_borrow_mut(|events| {
            events.take();
        });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn modal_dispatch_preserves_input_events_and_their_coordinates() {
        let hooks = super::Hooks::escape_only().unwrap();
        super::on_hook(|| {
            assert!(super::post_event(
                super::CLICK,
                17,
                0,
                Some(super::POINT {
                    x: -80000,
                    y: 90000
                })
            ));
            assert!(super::post_event(
                super::CLICK,
                18,
                0,
                Some(super::POINT { x: 3, y: 4 })
            ));
        })
        .unwrap();
        // Simulate a native loop: it dispatches only HWND messages and knows
        // nothing about the application's input protocol.
        unsafe {
            let mut msg = super::MSG::default();
            while super::PeekMessageW(&mut msg, hooks.window, 0, 0, super::PM_REMOVE) != 0 {
                super::DispatchMessageW(&msg);
            }
        }
        assert_eq!(super::next_event().unwrap().wParam, 17);
        let first = super::click_point();
        assert_eq!((first.x, first.y), (-80000, 90000));
        assert_eq!(super::next_event().unwrap().wParam, 18);
        let second = super::click_point();
        assert_eq!((second.x, second.y), (3, 4));
        assert!(super::next_event().is_none());
    }
    #[test]
    fn hook_worker_keeps_pumping_while_the_ui_is_busy() {
        let _hooks = super::Hooks::escape_only().unwrap();
        let worker_id = super::on_hook(|| unsafe {
            windows_sys::Win32::System::Threading::GetCurrentThreadId()
        })
        .unwrap();
        assert_ne!(worker_id, unsafe {
            windows_sys::Win32::System::Threading::GetCurrentThreadId()
        });
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(super::on_hook(|| 42).unwrap(), 42);
    }
}
