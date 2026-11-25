use crate::{
    config::{Config, Identity},
    desktop::{self, err},
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::*,
    System::{LibraryLoader::GetModuleHandleW, SystemInformation::GetTickCount, Threading::*},
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
pub const TIP_SCROLL: u32 = WM_APP + 13;
pub const CAST_KEY: u32 = WM_APP + 14;
pub const CAST_MOUSE: u32 = WM_APP + 15;
pub const RECOVERED: u32 = WM_APP + 16;
pub const CENTER: u32 = WM_APP + 17;
thread_local! { static DISPLAY_PENDING: Cell<bool> = const { Cell::new(false) }; }
pub fn display_changed() {
    desktop::invalidate_displays();
    // Every overlay window receives the same broadcast. One event is enough: its
    // handler reads the current topology whenever it runs.
    if !DISPLAY_PENDING.with(|pending| pending.replace(true))
        && !post_event(DISPLAY_CHANGED, 0, 0, None)
    {
        DISPLAY_PENDING.with(|pending| pending.set(false));
    }
}
const LEFT: u32 = VK_LMENU as u32;
const RIGHT: u32 = VK_RMENU as u32;
// VK_E8 is unassigned: unlike Ctrl it cannot change a real modifier's state.
const MENU_MASK_KEY: u16 = 0xe8;
const MENU_MASK_TAG: usize = 0x5753_504d;

#[derive(Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Idle,
    Wait,
    Command,
}
#[derive(Default)]
struct Input {
    paused: bool,
    mode: Mode,
    alt_mask: u8,
    gesture_mask: u8,
    gesture_target: HWND,
    double_ms: u32,
    hold_ms: u32,
    hold_started: Option<u32>,
    hold_active: bool,
    mask_alt_menu: bool,
    last_tap: Option<(u32, u32)>,
    pressed: u8,
    tap: Option<(u32, u32)>,
    second: bool,
    bound: [u64; 4],
    command: [u64; 4],
    eaten: [u64; 4],
    guides: bool,
    preview_tracking: bool,
    annotation_preview: bool,
    last_event: u32,
    mouse: HHOOK,
    point: POINT,
    tip_visible: bool,
    tip_scrollable: bool,
    tip_window: usize,
    target_allowed: bool,
    exclusions: Vec<Identity>,
    move_button: Option<u32>,
    resize_button: Option<u32>,
    center_button: Option<u32>,
    centered_button: Option<u32>,
    cast_mouse: bool,
    cast_buttons: u32,
    cast_wheel_time: Option<u32>,
    cast_keys: bool,
    cast_toggles: [u64; 4],
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
thread_local! {static KEYBOARD_HOOK: Cell<HHOOK> = const {Cell::new(null_mut())};}
thread_local! {
    static EVENTS: RefCell<Option<EventSender>> = const { RefCell::new(None) };
    static INBOX: RefCell<Option<mpsc::Receiver<QueuedEvent>>> = const { RefCell::new(None) };
    static CURRENT: Cell<Option<(u32, POINT)>> = const { Cell::new(None) };
    static DRIVER: RefCell<Option<Driver>> = const { RefCell::new(None) };
    static DEFERRED: RefCell<VecDeque<QueuedEvent>> = const {RefCell::new(VecDeque::new())};
}
const WAKE: u32 = WM_APP + 90;
const SCROLL_READY: u32 = WM_APP + 91;
#[derive(Clone, Copy)]
struct QueuedEvent {
    message: u32,
    wp: usize,
    lp: isize,
    point: Option<POINT>,
}
#[derive(Default)]
struct PendingScroll {
    latest: [Option<QueuedEvent>; 2],
    queued: bool,
}
struct EventSender {
    send: mpsc::Sender<QueuedEvent>,
    window: usize,
    scroll: Arc<Mutex<PendingScroll>>,
    stopped: Arc<AtomicBool>,
    movement: Arc<Movement>,
    alt_held: Arc<AtomicBool>,
}
#[derive(Default)]
struct Movement {
    point: AtomicU64,
    queued: AtomicBool,
}
/// Consume owned event data only in the outer application loop. A native modal
/// loop may dispatch the HWND wakeup, but cannot discard the channel's contents.
fn receive_event() -> Option<QueuedEvent> {
    let mut event = INBOX.with_borrow(|inbox| inbox.as_ref()?.try_recv().ok())?;
    if event.message == SCROLL_READY {
        event = EVENTS.with_borrow(|events| {
            let events = events.as_ref()?;
            let (event, more) = {
                let mut pending = events.scroll.lock().ok()?;
                let event = pending.latest[0]
                    .take()
                    .or_else(|| pending.latest[1].take())?;
                let more = pending.latest.iter().any(Option::is_some);
                pending.queued = more;
                (event, more)
            };
            if more {
                let _ = events.send.send(QueuedEvent {
                    message: SCROLL_READY,
                    wp: 0,
                    lp: 0,
                    point: None,
                });
            }
            Some(event)
        })?;
    }
    // Leave a wakeup for any backlog after an outer loop's bounded drain, even
    // when a modal loop already consumed all original wakeup messages.
    DRIVER.with_borrow(|driver| {
        if let Some(driver) = driver {
            unsafe {
                PostMessageW(driver.window, WAKE, 0, 0);
            }
        }
    });
    Some(event)
}
fn event_message(event: QueuedEvent) -> MSG {
    if event.message == DISPLAY_CHANGED {
        DISPLAY_PENDING.with(|pending| pending.set(false));
    }
    CURRENT.with(|current| current.set(event.point.map(|point| (event.message, point))));
    MSG {
        message: event.message,
        wParam: event.wp,
        lParam: event.lp,
        ..Default::default()
    }
}
pub fn next_event() -> Option<MSG> {
    let (deferred, more) =
        DEFERRED.with_borrow_mut(|events| (events.pop_front(), !events.is_empty()));
    if more {
        DRIVER.with_borrow(|driver| {
            if let Some(driver) = driver {
                unsafe {
                    PostMessageW(driver.window, WAKE, 0, 0);
                }
            }
        });
    }
    deferred.or_else(receive_event).map(event_message)
}
/// Native menus pump HWND messages but cannot run ordinary application actions.
/// Drain only annotation data and preserve every other event for the outer loop.
pub fn next_annotation_event() -> Option<MSG> {
    for _ in 0..64 {
        let event = receive_event()?;
        if matches!(event.message, CAST_KEY | CAST_MOUSE) {
            return Some(event_message(event));
        }
        DEFERRED.with_borrow_mut(|deferred| deferred.push_back(event));
    }
    None
}
type Job = Box<dyn FnOnce() + Send>;
enum Control {
    Run(Job),
    Stop,
}
struct Driver {
    commands: mpsc::Sender<Control>,
    wake: Arc<WakeEvent>,
    window: HWND,
    movement: Arc<Movement>,
}
struct WakeEvent(usize);
impl Drop for WakeEvent {
    fn drop(&mut self) {
        // SAFETY: the last Arc owns the event; the hook worker retains its Arc until it exits.
        unsafe {
            CloseHandle(self.0 as HANDLE);
        }
    }
}
fn on_hook<R: Send + 'static>(action: impl FnOnce() -> R + Send + 'static) -> Result<R, String> {
    DRIVER.with_borrow(|driver| {
        let Some(driver) = driver else {
            return Ok(action());
        };
        let (send, receive) = mpsc::sync_channel(1);
        let canceled = Arc::new(AtomicBool::new(false));
        let pending = canceled.clone();
        driver
            .commands
            .send(Control::Run(Box::new(move || {
                if !pending.load(Ordering::Acquire) {
                    let _ = send.send(action());
                }
            })))
            .map_err(|_| "Input thread stopped".to_owned())?;
        // SAFETY: Hooks owns the event until the worker has joined; no window messages
        // or UI-owned references cross this channel.
        desktop::check(unsafe { SetEvent(driver.wake.0 as HANDLE) })?;
        receive.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            canceled.store(true, Ordering::Release);
            "Input thread did not respond".into()
        })
    })
}
fn queue_hook(action: impl FnOnce() + Send + 'static) -> Result<(), String> {
    DRIVER.with_borrow(|driver| {
        let Some(driver) = driver else {
            action();
            return Ok(());
        };
        driver
            .commands
            .send(Control::Run(Box::new(action)))
            .map_err(|_| "Input thread stopped".to_owned())?;
        desktop::check(unsafe { SetEvent(driver.wake.0 as HANDLE) })
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
        let Some(events) = events else {
            return false;
        };
        if events.stopped.load(Ordering::Acquire) {
            return false;
        }
        let mut event = QueuedEvent {
            message,
            wp,
            lp,
            point,
        };
        if message == CAST_MOUSE && (8..=11).contains(&wp) {
            let Ok(mut pending) = events.scroll.lock() else {
                return false;
            };
            pending.latest[usize::from(wp >= 10)] = Some(event);
            if pending.queued {
                return true;
            }
            pending.queued = true;
            event = QueuedEvent {
                message: SCROLL_READY,
                wp: 0,
                lp: 0,
                point: None,
            };
        }
        if events.send.send(event).is_err() {
            return false;
        }
        // Only integers cross the window-message boundary. If the wakeup cannot be
        // posted, the owned event stays in the channel for the next outer-loop drain.
        unsafe { PostMessageW(events.window as HWND, WAKE, 0, 0) != 0 }
    })
}
impl Input {
    fn cancel_hold(&mut self) {
        self.hold_started = None;
        self.hold_active = false;
    }
    fn advance_hold(&mut self, time: u32) {
        if self
            .hold_started
            .is_some_and(|when| time.wrapping_sub(when) >= self.hold_ms)
            && self.pressed & (self.alt_mask | self.gesture_mask) != 0
        {
            self.hold_started = None;
            self.hold_active = true;
            // A long hold cannot also be the first tap of a double-tap sequence.
            if self
                .tap
                .is_some_and(|(_, when)| time.wrapping_sub(when) > self.double_ms)
            {
                self.tap = None;
                self.last_tap = None;
            }
            if self.target_allowed {
                self.mask_alt_menu = true;
                post(ALT_HOLD, self.gesture_target as usize, 0);
            }
            let _ = self.update_mouse();
        }
    }
    fn hold_wait(&self, time: u32) -> u32 {
        self.hold_started.map_or(100, |when| {
            self.hold_ms
                .saturating_sub(time.wrapping_sub(when))
                .clamp(1, 100)
        })
    }
    fn select_target(&mut self, hwnd: HWND) {
        self.gesture_target = hwnd;
        self.target_allowed = desktop::allowed(hwnd, &self.exclusions);
    }
    fn reset_gesture(&mut self) {
        self.cancel_hold();
        self.mode = Mode::Idle;
        self.pressed = 0;
        self.mask_alt_menu = false;
        self.tap = None;
        self.last_tap = None;
        self.second = false;
        self.eaten = [0; 4];
        self.target_allowed = false;
        self.gesture_target = null_mut();
        self.drag = None;
        self.centered_button = None;
        self.cast_buttons = 0;
        self.cast_wheel_time = None;
    }
    fn reconcile_keys(&mut self, down: impl Fn(u32) -> bool) -> bool {
        let physical = u8::from(down(LEFT)) | (u8::from(down(RIGHT)) << 1);
        // A consumed second Alt-down is intentionally absent from async state.
        // Reconcile only ordinary Alt holds; intercepted sequences end on their hook up-event.
        let missing_alt = if self.second {
            0
        } else {
            self.pressed & !physical
        };
        if missing_alt != 0 {
            self.pressed &= physical;
            if self.pressed & (self.alt_mask | self.gesture_mask) == 0 {
                self.cancel_hold();
            }
            self.tap = None;
            self.last_tap = None;
            self.second = false;
            if self.pressed == 0 {
                self.mask_alt_menu = false;
                self.target_allowed = false;
                self.mode = Mode::Idle;
                self.drag = None;
            }
        }
        // Consumed key-downs do not update GetAsyncKeyState. Their release masks
        // must survive polling or held shortcut keys would repeat their actions.
        // Releases can be lost at an integrity/desktop boundary. Only reconcile
        // outside callbacks after an idle interval, when async state is current.
        for (button, key) in [
            (1, VK_LBUTTON),
            (2, VK_RBUTTON),
            (4, VK_MBUTTON),
            (5, VK_XBUTTON1),
            (6, VK_XBUTTON2),
        ] {
            if self.cast_buttons & (1 << button) != 0
                && !self.drag.is_some_and(|drag| drag.button == button)
                && !down(u32::from(key))
            {
                self.cast_buttons &= !(1 << button);
                post_event(
                    CAST_MOUSE,
                    button as usize,
                    crate::mouse_cast::RELEASE as isize,
                    Some(self.point),
                );
            }
        }
        missing_alt != 0
    }
    fn update_mouse(&mut self) -> Result<(), String> {
        // Alt transitions, configuration and recovery all refresh this subscription.
        // Publish their state so UI expiry never waits for the hook worker.
        EVENTS.with_borrow(|events| {
            if let Some(events) = events {
                events.alt_held.store(
                    self.pressed & (self.alt_mask | self.gesture_mask) != 0,
                    Ordering::Release,
                );
            }
        });
        unsafe {
            let needed = !self.paused
                && (self.guides
                    || self.cast_mouse
                    || self.preview_tracking
                    || self.drag.is_some()
                    || self.centered_button.is_some()
                    || (self.tip_scrollable && self.tip_window != 0)
                    || (self.target_allowed && self.pressed & self.gesture_mask != 0));
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
        let eaten = self.keyboard_inner(key, down, time);
        if eaten && down && !matches!(key, LEFT | RIGHT) && self.pressed != 0 {
            self.mask_alt_menu = true;
        }
        eaten
    }
    fn masks_release(&self, key: u32, down: bool) -> bool {
        let side = match key {
            LEFT => 1,
            RIGHT => 2,
            _ => 0,
        };
        !down && !self.second && self.mask_alt_menu && self.pressed & side != 0
    }
    fn keyboard_inner(&mut self, key: u32, down: bool, time: u32) -> bool {
        if self.paused {
            return false;
        }
        if key >= 256 {
            return false;
        }
        self.last_event = time;
        self.advance_hold(time);
        let (index, flag) = bit(key);
        if self.eaten[index] & flag != 0 {
            if !down {
                self.eaten[index] &= !flag;
            }
            return true;
        }
        if down
            && self.hold_active
            && self.pressed & self.alt_mask != 0
            && self.cast_toggles[index] & flag != 0
        {
            post(ACTION, key as usize, 0);
            self.eaten[index] |= flag;
            self.tap = None;
            self.last_tap = None;
            return true;
        }
        if down && self.tip_scrollable && matches!(key, 0x21..=0x24 | 0x26 | 0x28) {
            let delta = match key {
                0x21 => -400,
                0x22 => 400,
                0x23 => i32::MAX,
                0x24 => i32::MIN,
                0x26 => -40,
                _ => 40,
            };
            post(TIP_SCROLL, delta as usize, 0);
            self.eaten[index] |= flag;
            return true;
        }
        if down
            && key == VK_ESCAPE as u32
            && (self.tip_visible
                || self.annotation_preview
                || self.preview_tracking
                || self.mode != Mode::Idle
                || self.drag.is_some())
        {
            self.mode = Mode::Idle;
            self.cancel_hold();
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
                    self.select_target(unsafe { GetForegroundWindow() });
                }
                let start_hold = self.pressed & (self.gesture_mask | self.alt_mask) == 0
                    && side & (self.gesture_mask | self.alt_mask) != 0;
                self.pressed |= side;
                // SAFETY: querying modifier state uses only valid virtual-key codes.
                let unmodified = unsafe {
                    [VK_CONTROL, VK_SHIFT, VK_LWIN, VK_RWIN]
                        .iter()
                        .all(|code| GetAsyncKeyState(*code as i32) >= 0)
                };
                let clean = self.pressed == side && unmodified;
                self.tap = (clean && self.target_allowed && self.alt_mask & side != 0)
                    .then_some((key, time));
                self.second = self.tap.is_some()
                    && self.last_tap.is_some_and(|(last, when)| {
                        last == key && time.wrapping_sub(when) <= self.double_ms
                    });
                self.last_tap = None;
                if self.second {
                    self.hold_started = None;
                    self.hold_active = true;
                } else if start_hold && unmodified {
                    self.hold_started = Some(time);
                    self.advance_hold(time);
                }
                let _ = self.update_mouse();
                return self.second;
            }
            self.pressed &= !side;
            if self.pressed == 0 {
                self.mask_alt_menu = false;
            }
            if self.pressed & (self.gesture_mask | self.alt_mask) == 0 {
                if self.hold_active {
                    post(ALT_RELEASE, 0, 0);
                }
                self.cancel_hold();
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
        if !self.hold_active {
            self.hold_started = None;
        }
        if matches!(key, 0x10 | 0xa0 | 0xa1) {
            return false;
        }
        if self.mode == Mode::Idle
            && self.hold_active
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
                self.cancel_hold();
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
fn mask_alt_menu() {
    let keys = [0, KEYEVENTF_KEYUP].map(|flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: MENU_MASK_KEY,
                dwFlags: flags,
                dwExtraInfo: MENU_MASK_TAG,
                ..Default::default()
            },
        },
    });
    // SAFETY: the array owns two initialized keyboard records for this synchronous call.
    // Send before forwarding Alt-up so the foreground app sees an intervening key.
    // If injection is denied, still forward the physical Alt-up to avoid a stuck Alt.
    unsafe {
        if SendInput(keys.len() as u32, keys.as_ptr(), size_of::<INPUT>() as i32) == 1 {
            SendInput(1, &keys[1], size_of::<INPUT>() as i32);
        }
    }
}
unsafe extern "system" fn keyboard(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if hook_stopped() || INPUT.with(|cell| cell.try_borrow().is_ok_and(|input| input.paused)) {
            return CallNextHookEx(null_mut(), code, wp, lp);
        }
        if code == HC_ACTION as i32 {
            let data = &*(lp as *const KBDLLHOOKSTRUCT);
            // All WinSizeP hooks must pass their own mask through without casting it,
            // treating it as an application shortcut, or recursively modifying input.
            if data.dwExtraInfo == MENU_MASK_TAG && data.flags & LLKHF_INJECTED != 0 {
                return CallNextHookEx(null_mut(), code, wp, lp);
            }
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
            // Only physical Alt events may change preview and gesture state. Forward
            // injected presses and releases without consuming them or casting them.
            if matches!(key, LEFT | RIGHT)
                && data.flags & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED) != 0
            {
                return CallNextHookEx(null_mut(), code, wp, lp);
            }
            let casting = INPUT.with(|cell| {
                cell.try_borrow().is_ok_and(|input| {
                    input.cast_keys
                        && (key >= 256 || {
                            let (index, flag) = bit(key);
                            !input.hold_active
                                || input.pressed & input.alt_mask == 0
                                || input.cast_toggles[index] & flag == 0
                        })
                })
            });
            if casting && down {
                let detail = cast_modifiers()
                    | ((data.scanCode & 255) << 8)
                    | (u32::from(data.flags & LLKHF_EXTENDED != 0) << 16);
                let mut point = POINT::default();
                if GetCursorPos(&mut point) != 0 {
                    post_event(CAST_KEY, key as usize, detail as isize, Some(point));
                }
            }
            let (eaten, mask) = INPUT.with(|cell| {
                let Ok(mut input) = cell.try_borrow_mut() else {
                    return (false, false);
                };
                input.advance_hold(data.time);
                let mask = input.masks_release(key, down);
                (input.keyboard(key, down, data.time), mask)
            });
            // Drop the RefCell borrow before SendInput can reenter any hook.
            if mask && !eaten {
                mask_alt_menu();
            }
            if eaten {
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
        if hook_stopped() || INPUT.with(|cell| cell.try_borrow().is_ok_and(|input| input.paused)) {
            return CallNextHookEx(null_mut(), code, wp, lp);
        }
        if code == HC_ACTION as i32 {
            let data = &*(lp as *const MSLLHOOKSTRUCT);
            INPUT.with(|cell| {
                if let Ok(mut input) = cell.try_borrow_mut() {
                    input.last_event = data.time;
                    input.point = data.pt;
                    input.advance_hold(data.time);
                    if !input.hold_active
                        && (mouse_button(wp as u32, data.mouseData).is_some_and(|(_, down)| down)
                            || matches!(wp as u32, WM_MOUSEWHEEL | WM_MOUSEHWHEEL))
                    {
                        input.hold_started = None;
                        input.tap = None;
                        input.last_tap = None;
                    }
                }
            });
            if INPUT.with(|cell| cell.try_borrow().is_ok_and(|input| input.cast_mouse))
                && let Some((action, flags)) = cast_mouse_action(wp as u32, data.mouseData)
            {
                INPUT.with(|cell| {
                    if let Ok(mut input) = cell.try_borrow_mut() {
                        if action < 8 {
                            if flags & crate::mouse_cast::RELEASE == 0 {
                                input.cast_buttons |= 1 << action;
                            } else {
                                input.cast_buttons &= !(1 << action);
                            }
                        } else {
                            input.cast_wheel_time = Some(data.time);
                        }
                    }
                });
                post_event(CAST_MOUSE, action as usize, flags as isize, Some(data.pt));
            }
            // Clicking the tip's scrollbar must not dismiss an Alt preview or begin a drag.
            let tip_window = INPUT.with(|cell| {
                let Ok(input) = cell.try_borrow() else {
                    return 0;
                };
                if input.tip_scrollable
                    && input.drag.is_none()
                    && input.tip_window != 0
                    && WindowFromPoint(data.pt) as usize == input.tip_window
                {
                    input.tip_window
                } else {
                    0
                }
            });
            if tip_window != 0 {
                if wp as u32 == WM_MOUSEWHEEL
                    && PostMessageW(
                        tip_window as HWND,
                        WM_MOUSEWHEEL,
                        data.mouseData as usize,
                        0,
                    ) != 0
                {
                    return 1;
                }
                return CallNextHookEx(null_mut(), code, wp, lp);
            }
            let eaten = INPUT.with(|cell| {
                cell.try_borrow_mut()
                    .is_ok_and(|mut input| match wp as u32 {
                        WM_MOUSEMOVE if input.guides || input.preview_tracking || input.drag.is_some()
                            || (input.cast_mouse && (input.cast_buttons!=0 || input.cast_wheel_time.is_some_and(|when|data.time.wrapping_sub(when)<600))) => {
                            input.point = data.pt;
                            if let Some(mut drag) = input.drag
                                && drag.draggable && !drag.canceled && !drag.active && ((data.pt.x as i64 - drag.start.x as i64).abs() >= GetSystemMetrics(SM_CXDRAG) as i64 || (data.pt.y as i64 - drag.start.y as i64).abs() >= GetSystemMetrics(SM_CYDRAG) as i64) {
                                    drag.active = true;
                                    post_event(DRAG_START, input.gesture_target as usize, isize::from(drag.resize), Some(drag.start));
                                    input.drag = Some(drag);
                                }
                            post_move(data.pt);
                            false
                        }
                        message if mouse_button(message, data.mouseData).is_some() => {
                            let (button, down) = mouse_button(message, data.mouseData).unwrap();
                            if !down && input.centered_button == Some(button) { input.centered_button = None; let _ = input.update_mouse(); return true; }
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
                            if input.hold_active && input.target_allowed && input.pressed & input.gesture_mask != 0 && input.drag.is_none() {
                                if input.center_button == Some(button) {
                                    input.centered_button = Some(button);
                                    post_event(CENTER, input.gesture_target as usize, 0, Some(data.pt));
                                    return true;
                                }
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
fn hook_stopped() -> bool {
    EVENTS.with(|cell| {
        cell.try_borrow().map_or(true, |events| {
            events
                .as_ref()
                .is_some_and(|events| events.stopped.load(Ordering::Acquire))
        })
    })
}
pub fn drag_origin() -> POINT {
    CURRENT
        .with(Cell::get)
        .filter(|(kind, _)| *kind == DRAG_START)
        .map(|(_, point)| point)
        .unwrap_or_default()
}
fn cast_modifiers() -> u32 {
    let mut flags = 0;
    for (key, flag) in [
        (VK_CONTROL, crate::cast::CTRL),
        (VK_MENU, crate::cast::ALT),
        (VK_SHIFT, crate::cast::SHIFT),
        (VK_LWIN, crate::cast::WIN),
        (VK_RWIN, crate::cast::WIN),
    ] {
        if unsafe { GetAsyncKeyState(key as i32) } < 0 {
            flags |= flag;
        }
    }
    flags
}
pub fn cast_point() -> POINT {
    CURRENT
        .with(Cell::get)
        .filter(|(kind, _)| matches!(*kind, CAST_KEY | CAST_MOUSE))
        .map(|(_, point)| point)
        .unwrap_or_default()
}
pub fn set_casting(mouse: bool, keys: bool) -> Result<(), String> {
    on_hook(move || {
        INPUT.with_borrow_mut(|input| {
            let previous = (input.cast_mouse, input.cast_keys);
            input.cast_mouse = mouse;
            if !mouse {
                input.cast_buttons = 0;
                input.cast_wheel_time = None;
            }
            input.cast_keys = keys;
            if let Err(error) = input.update_mouse() {
                (input.cast_mouse, input.cast_keys) = previous;
                return Err(error);
            }
            Ok(())
        })
    })?
}
pub fn annotation_preview(active: bool) -> Result<(), String> {
    on_hook(move || INPUT.with_borrow_mut(|input| input.annotation_preview = active))
}
fn cast_mouse_action(message: u32, data: u32) -> Option<(u32, u32)> {
    let delta = (data >> 16) as i16;
    // Use the OS-delivered wheel delta, after device/driver inversion. Reading a
    // physical wheel direction or applying FlipFlopWheel again would invert it twice.
    match message {
        WM_MOUSEWHEEL if delta != 0 => Some((if delta > 0 { 8 } else { 9 }, 0)),
        WM_MOUSEHWHEEL if delta != 0 => Some((if delta > 0 { 11 } else { 10 }, 0)),
        _ => mouse_button(message, data)
            .map(|(button, down)| (button, if down { 0 } else { crate::mouse_cast::RELEASE })),
    }
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
    let _ = queue_hook(move || {
        INPUT.with_borrow_mut(|input| {
            input.tip_visible = visible;
            if !visible {
                input.tip_scrollable = false;
                input.tip_window = 0;
            }
            let _ = input.update_mouse();
        })
    });
}
pub fn tip_scrollable(scrollable: bool, window: usize) {
    let _ = queue_hook(move || {
        INPUT.with_borrow_mut(|input| {
            input.tip_scrollable = scrollable;
            input.tip_window = window;
            let _ = input.update_mouse();
        })
    });
}
pub fn click_point() -> POINT {
    CURRENT
        .with(Cell::get)
        .filter(|(kind, _)| matches!(*kind, CLICK | CENTER))
        .map(|(_, point)| point)
        .unwrap_or_default()
}
pub fn moved() -> POINT {
    if let Some((DRAG_END, point)) = CURRENT.with(Cell::get) {
        return point;
    }
    if let Some(point) = DRIVER.with_borrow(|driver| {
        driver.as_ref().map(|driver| {
            driver.movement.queued.store(false, Ordering::Release);
            let value = driver.movement.point.load(Ordering::Acquire);
            POINT {
                x: value as u32 as i32,
                y: (value >> 32) as u32 as i32,
            }
        })
    }) {
        return point;
    }
    INPUT.with_borrow(|input| input.point)
}
fn post_move(point: POINT) {
    let needed = EVENTS.with_borrow(|events| {
        let Some(events) = events else {
            return false;
        };
        events.movement.point.store(
            u64::from(point.x as u32) | (u64::from(point.y as u32) << 32),
            Ordering::Release,
        );
        !events.movement.queued.swap(true, Ordering::AcqRel)
    });
    if needed {
        post(MOVE, 0, 0);
    }
}
pub fn cancel() {
    let _ = queue_hook(|| {
        INPUT.with_borrow_mut(|input| {
            input.mode = Mode::Idle;
            if let Some(drag) = &mut input.drag {
                drag.canceled = true;
            }
        })
    });
}
pub fn alt_held() -> bool {
    EVENTS.with_borrow(|events| {
        events.as_ref().map_or_else(
            || {
                INPUT
                    .with_borrow(|input| input.pressed & (input.alt_mask | input.gesture_mask) != 0)
            },
            |events| {
                !events.stopped.load(Ordering::Acquire) && events.alt_held.load(Ordering::Acquire)
            },
        )
    })
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
    if !on {
        // FIFO ordering preserves a subsequent start without blocking UI cleanup.
        return queue_hook(|| {
            INPUT.with_borrow_mut(|input| {
                input.preview_tracking = false;
                let _ = input.update_mouse();
            })
        });
    }
    on_hook(move || {
        INPUT.with_borrow_mut(|input| {
            input.preview_tracking = on;
            input.update_mouse()
        })
    })?
}
pub fn configure(cfg: &Config) -> Result<(), String> {
    let center_button = cfg.center_button;
    let (alt, gesture, hold_ms, exclusions, move_button, resize_button, double_ms) = (
        mask(&cfg.alt),
        mask(&cfg.gesture),
        cfg.alt_hold_ms,
        cfg.exclusions.clone(),
        cfg.move_button,
        cfg.resize_button,
        cfg.double_ms,
    );
    let keys = cfg.keys.clone();
    on_hook(move || {
        INPUT.with_borrow_mut(|input| {
            input.mode = Mode::Idle;
            input.cancel_hold();
            input.tap = None;
            input.last_tap = None;
            input.alt_mask = alt;
            input.gesture_mask = gesture;
            input.hold_ms = hold_ms;
            input.exclusions = exclusions;
            input.move_button = move_button;
            input.resize_button = resize_button;
            input.center_button = center_button;
            input.double_ms = double_ms;
            input.command = [0; 4];
            input.bound = [0; 4];
            input.cast_toggles = [0; 4];
            for (key, action) in &keys {
                let (index, flag) = bit(*key);
                input.bound[index] |= flag;
                if action == "command" {
                    input.command[index] |= flag;
                }
                if matches!(action.as_str(), "mousecast" | "keycast") {
                    input.cast_toggles[index] |= flag;
                }
            }
            input.update_mouse()
        })
    })?
}
pub fn set_paused(paused: bool) -> Result<(), String> {
    on_hook(move || {
        INPUT.with_borrow_mut(|input| {
            if input.paused == paused {
                return Ok(());
            }
            // SAFETY: hooks are installed and removed on their owning worker thread.
            unsafe {
                if paused {
                    let hook = KEYBOARD_HOOK.with(|slot| slot.replace(null_mut()));
                    if !hook.is_null() {
                        UnhookWindowsHookEx(hook);
                    }
                } else {
                    let hook = SetWindowsHookExW(
                        WH_KEYBOARD_LL,
                        Some(keyboard),
                        GetModuleHandleW(null()),
                        0,
                    );
                    if hook.is_null() {
                        return Err(err());
                    }
                    KEYBOARD_HOOK.with(|slot| slot.set(hook));
                }
            }
            input.paused = paused;
            input.reset_gesture();
            if let Err(error) = input.update_mouse() {
                // A failed resume leaves capture paused as the tray still reports.
                input.paused = true;
                unsafe {
                    let hook = KEYBOARD_HOOK.with(|slot| slot.replace(null_mut()));
                    if !hook.is_null() {
                        UnhookWindowsHookEx(hook);
                    }
                }
                return Err(error);
            }
            Ok(())
        })
    })??;
    DEFERRED.with_borrow_mut(VecDeque::clear);
    INBOX.with_borrow(|inbox| {
        if let Some(inbox) = inbox {
            for _ in inbox.try_iter() {}
        }
    });
    // A discarded MOVE or SCROLL_READY leaves its "already queued" latch set, and
    // nothing else clears it: pointer tracking and wheel marks would stay dead after
    // resuming. Reset after draining so a concurrent hook event cannot be lost.
    DISPLAY_PENDING.with(|pending| pending.set(false));
    EVENTS.with_borrow(|events| {
        if let Some(events) = events {
            events.movement.queued.store(false, Ordering::Release);
            if let Ok(mut scroll) = events.scroll.lock() {
                *scroll = PendingScroll::default();
            }
        }
    });
    Ok(())
}
pub struct Hooks {
    worker: Option<JoinHandle<()>>,
    // Raw handles also prevent moving this UI-thread guard to another thread.
    wake: Arc<WakeEvent>,
    window: HWND,
    stopped: Arc<AtomicBool>,
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
            // Read the cause before cleanup replaces it.
            let error = err();
            unsafe {
                DestroyWindow(window);
            }
            return Err(error);
        }
        let wake = Arc::new(WakeEvent(wake as usize));
        let stopped = Arc::new(AtomicBool::new(false));
        let (send_events, receive_events) = mpsc::channel();
        let destination = window as usize;
        let scroll = Arc::new(Mutex::new(PendingScroll::default()));
        let movement = Arc::new(Movement::default());
        let alt_held = Arc::new(AtomicBool::new(false));
        EVENTS.with_borrow_mut(|events| {
            *events = Some(EventSender {
                send: send_events.clone(),
                window: destination,
                scroll: scroll.clone(),
                stopped: stopped.clone(),
                movement: movement.clone(),
                alt_held: alt_held.clone(),
            })
        });
        let event = wake.clone();
        let worker_stop = stopped.clone();
        let worker_movement = movement.clone();
        let (commands, receive) = mpsc::channel();
        let (ready, started) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("winsizep-input".into())
            .spawn(move || {
                EVENTS.with_borrow_mut(|events| {
                    *events = Some(EventSender {
                        send: send_events,
                        window: destination,
                        scroll,
                        stopped: worker_stop.clone(),
                        movement: worker_movement.clone(),
                        alt_held,
                    })
                });
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
                    KEYBOARD_HOOK.with(|slot| slot.set(hook));
                    let _ = ready.send(Ok(()));
                    let mut heartbeat = std::time::Instant::now();
                    'pump: loop {
                        if worker_stop.load(Ordering::Acquire) {
                            break;
                        }
                        if heartbeat.elapsed() > Duration::from_millis(750)
                            && !INPUT.with_borrow(|input| input.paused)
                        {
                            // Windows may silently remove low-level hooks after a long stall.
                            // Re-arm on this owning thread and abandon incomplete gestures.
                            let mut recovered = false;
                            INPUT.with_borrow_mut(|input| {
                                KEYBOARD_HOOK.with(|slot| {
                                    UnhookWindowsHookEx(slot.get());
                                    let hook = SetWindowsHookExW(
                                        WH_KEYBOARD_LL,
                                        Some(keyboard),
                                        GetModuleHandleW(null()),
                                        0,
                                    );
                                    slot.set(hook);
                                    recovered = !hook.is_null();
                                });
                                if !input.mouse.is_null() {
                                    UnhookWindowsHookEx(input.mouse);
                                    input.mouse = null_mut();
                                }
                                input.reset_gesture();
                                recovered &= input.update_mouse().is_ok();
                            });
                            worker_movement.queued.store(false, Ordering::Release);
                            post(RECOVERED, usize::from(recovered), 0);
                        }
                        heartbeat = std::time::Instant::now();
                        match MsgWaitForMultipleObjectsEx(
                            1,
                            &(event.0 as HANDLE),
                            INPUT.with_borrow(|input| input.hold_wait(GetTickCount())),
                            QS_ALLINPUT,
                            MWMO_INPUTAVAILABLE,
                        ) {
                            WAIT_OBJECT_0 => {
                                while let Ok(command) = receive.try_recv() {
                                    if worker_stop.load(Ordering::Acquire) {
                                        break 'pump;
                                    }
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
                            WAIT_TIMEOUT => {
                                INPUT.with_borrow_mut(|input| {
                                    if GetTickCount().wrapping_sub(input.last_event) >= 150
                                        && input
                                            .reconcile_keys(|key| GetAsyncKeyState(key as i32) < 0)
                                    {
                                        post(CANCEL, 0, 0);
                                        let _ = input.update_mouse();
                                    }
                                });
                            }
                            _ => break,
                        }
                        // Also advance during continuous mouse movement or key repeats;
                        // those messages can keep the wait from timing out indefinitely.
                        INPUT.with_borrow_mut(|input| input.advance_hold(GetTickCount()));
                    }
                    INPUT.with_borrow_mut(|input| {
                        let hook = KEYBOARD_HOOK.with(|slot| slot.replace(null_mut()));
                        if !hook.is_null() {
                            UnhookWindowsHookEx(hook);
                        }
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
                    DestroyWindow(window);
                }
                return Err(error.to_string());
            }
        };
        DRIVER.with_borrow_mut(|driver| {
            *driver = Some(Driver {
                commands,
                wake: wake.clone(),
                window,
                movement,
            })
        });
        INBOX.with_borrow_mut(|inbox| *inbox = Some(receive_events));
        DISPLAY_PENDING.with(|pending| pending.set(false));
        let hooks = Self {
            worker: Some(worker),
            wake,
            window,
            stopped,
        };
        started
            .recv_timeout(Duration::from_secs(2))
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
        self.stopped.store(true, Ordering::Release);
        DRIVER.with_borrow_mut(|driver| {
            if let Some(driver) = driver.take() {
                let _ = driver.commands.send(Control::Stop);
            }
        });
        unsafe {
            SetEvent(self.wake.0 as HANDLE);
        }
        if let Some(worker) = self.worker.take() {
            let until = std::time::Instant::now() + Duration::from_millis(500);
            while !worker.is_finished() && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
            // A stuck foreign hook must not deadlock our UI shutdown. The stopped worker
            // passes future input through, owns its wake event, and cleans up on return.
        }
        unsafe {
            DestroyWindow(self.window);
        }
        INBOX.with_borrow_mut(|inbox| {
            inbox.take();
        });
        CURRENT.with(|current| current.set(None));
        DEFERRED.with_borrow_mut(VecDeque::clear);
        EVENTS.with_borrow_mut(|events| {
            events.take();
        });
    }
}

#[cfg(test)]
pub(super) mod tests {
    #[test]
    fn configured_hold_delay_keeps_the_saved_value_and_handles_boundaries() {
        use super::*;
        let _hooks = Hooks::escape_only().unwrap();
        let mut cfg = crate::test_support::cfg();
        assert_eq!(cfg.alt_hold_ms, 1500);
        for (saved, expected) in [
            (0, 0),
            (750, 750),
            (1000, 1000),
            (1001, 1001),
            (1500, 1500),
            (60000, 60000),
        ] {
            cfg.alt_hold_ms = saved;
            configure(&cfg).unwrap();
            assert_eq!(
                on_hook(|| INPUT.with_borrow(|input| input.hold_ms)).unwrap(),
                expected
            );
            assert_eq!(cfg.alt_hold_ms, saved);
        }
    }

    #[test]
    fn center_click_runs_once_and_consumes_release_after_alt_is_released() {
        use super::*;
        let _hooks = Hooks::escape_only().unwrap();
        on_hook(|| {
            INPUT.with_borrow_mut(|input| {
                input.hold_active = true;
                input.pressed = 1;
                input.target_allowed = true;
                input.gesture_mask = 1;
                input.alt_mask = 1;
                input.center_button = Some(4);
            });
            let data = MSLLHOOKSTRUCT {
                pt: POINT { x: -900, y: 250 },
                time: 1000,
                ..Default::default()
            };
            assert_eq!(
                unsafe {
                    mouse(
                        HC_ACTION as i32,
                        WM_MBUTTONDOWN as usize,
                        (&data as *const MSLLHOOKSTRUCT) as isize,
                    )
                },
                1
            );
            INPUT.with_borrow_mut(|input| {
                input.keyboard(LEFT, false, 1001);
            });
            assert_eq!(
                unsafe {
                    mouse(
                        HC_ACTION as i32,
                        WM_MBUTTONUP as usize,
                        (&data as *const MSLLHOOKSTRUCT) as isize,
                    )
                },
                1
            );
            INPUT.with_borrow(|input| {
                assert!(input.centered_button.is_none() && input.drag.is_none())
            });
        })
        .unwrap();
        assert_eq!(next_event().unwrap().message, CENTER);
        assert_eq!((click_point().x, click_point().y), (-900, 250));
        assert_eq!(next_event().unwrap().message, ALT_RELEASE);
        assert!(next_event().is_none());
    }

    #[test]
    fn pause_removes_hooks_and_pending_gestures_then_restores_capture() {
        use super::*;
        let cfg = crate::test_support::cfg();
        let _hooks = Hooks::start(&cfg).unwrap();
        set_casting(true, true).unwrap();
        on_hook(|| {
            INPUT.with_borrow_mut(|input| {
                input.mode = Mode::Wait;
                input.hold_started = Some(1);
                input.pressed = 1;
            })
        })
        .unwrap();
        set_paused(true).unwrap();
        assert!(
            on_hook(|| INPUT.with_borrow_mut(|input| input.paused
                && input.mouse.is_null()
                && KEYBOARD_HOOK.with(Cell::get).is_null()
                && input.mode == Mode::Idle
                && input.hold_started.is_none()
                && !input.keyboard(0x41, true, 10)))
            .unwrap()
        );
        assert!(next_event().is_none());
        set_paused(false).unwrap();
        assert!(
            on_hook(|| INPUT.with_borrow(|input| !input.paused
                && !input.mouse.is_null()
                && !KEYBOARD_HOOK.with(Cell::get).is_null()))
            .unwrap()
        );
    }
    fn pending_hold(when: u32) -> super::Input {
        let mut input = super::Input {
            alt_mask: 1,
            hold_ms: 2000,
            hold_started: Some(when),
            pressed: 1,
            target_allowed: true,
            double_ms: 500,
            tap: Some((super::LEFT, when)),
            ..Default::default()
        };
        let (index, flag) = super::bit(0x51);
        input.bound[index] = flag;
        input
    }
    #[test]
    fn menu_mask_is_reserved_for_captured_alt_gestures() {
        use super::*;
        let mut input = pending_hold(100);
        assert!(
            !input.masks_release(LEFT, false),
            "short Alt tap must keep its menu"
        );
        input.keyboard(LEFT, false, 300);
        assert!(!input.mask_alt_menu);

        let mut input = pending_hold(100);
        input.advance_hold(2100);
        assert!(input.masks_release(LEFT, false));
        assert!(!input.masks_release(LEFT, true));
        assert!(!input.masks_release(RIGHT, false));
        input.preview_tracking = true;
        assert!(input.keyboard(VK_ESCAPE as u32, true, 2101));
        assert!(
            input.masks_release(LEFT, false),
            "Esc must not turn a canceled preview into a menu"
        );
        input.keyboard(LEFT, false, 2102);
        assert!(!input.mask_alt_menu);

        for key in [VK_TAB, VK_CONTROL, VK_SHIFT, 0x58] {
            let mut input = pending_hold(100);
            input.keyboard(key as u32, true, 300);
            input.advance_hold(2200);
            assert!(!input.masks_release(LEFT, false), "native shortcut {key}");
        }
        let mut excluded = pending_hold(100);
        excluded.target_allowed = false;
        excluded.advance_hold(2100);
        assert!(!excluded.masks_release(LEFT, false));
        let mut second = pending_hold(100);
        second.second = true;
        second.advance_hold(2100);
        assert!(
            !second.masks_release(LEFT, false),
            "the second Alt press was never forwarded"
        );
        let mut right = pending_hold(100);
        right.pressed = 2;
        right.alt_mask = 2;
        right.advance_hold(2100);
        assert!(right.masks_release(RIGHT, false));
        right.reconcile_keys(|_| false);
        assert!(!right.mask_alt_menu);
    }

    #[test]
    fn menu_mask_events_do_not_trigger_bindings_or_annotations() {
        use super::*;
        let _hooks = Hooks::escape_only().unwrap();
        on_hook(|| {
            INPUT.with_borrow_mut(|input| {
                input.cast_keys = true;
                input.mode = Mode::Command;
                input.pressed = 1;
                input.hold_active = true;
                let (index, flag) = bit(MENU_MASK_KEY as u32);
                input.bound[index] = flag;
            });
            for message in [WM_KEYDOWN, WM_KEYUP] {
                let event = KBDLLHOOKSTRUCT {
                    vkCode: MENU_MASK_KEY as u32,
                    flags: LLKHF_INJECTED,
                    dwExtraInfo: MENU_MASK_TAG,
                    ..Default::default()
                };
                assert_eq!(
                    unsafe {
                        keyboard(
                            HC_ACTION as i32,
                            message as usize,
                            (&event as *const _) as isize,
                        )
                    },
                    0
                );
            }
            INPUT.with_borrow(|input| assert!(input.mode == Mode::Command && input.hold_active));
        })
        .unwrap();
        assert!(next_event().is_none());
    }

    #[test]
    #[ignore = "focuses a temporary menu window and sends Alt input to verify native menu activation"]
    fn captured_alt_does_not_activate_the_foreground_menu() {
        use super::*;
        use windows_sys::core::w;
        const PHYSICAL_TEST_TAG: usize = 0x5753_5054;
        unsafe extern "system" fn replay_physical(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
            if code == HC_ACTION as i32 {
                // Exercise the production hook at the real input boundary with test
                // events interpreted as physical. Other injected events retain their flags.
                let mut event = unsafe { *(lp as *const KBDLLHOOKSTRUCT) };
                if event.dwExtraInfo == PHYSICAL_TEST_TAG {
                    event.flags &= !(LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED);
                    return unsafe { keyboard(code, wp, (&event as *const _) as isize) };
                }
            }
            unsafe { keyboard(code, wp, lp) }
        }
        unsafe extern "system" fn probe(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
            unsafe {
                if msg == WM_SYSCOMMAND && wp as u32 & 0xfff0 == SC_KEYMENU {
                    let count = GetPropW(hwnd, w!("MenuCount")) as usize;
                    SetPropW(hwnd, w!("MenuCount"), (count + 1) as HANDLE);
                    return 0;
                }
                if matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) && wp == MENU_MASK_KEY as usize {
                    let count = GetPropW(hwnd, w!("MaskCount")) as usize;
                    SetPropW(hwnd, w!("MaskCount"), (count + 1) as HANDLE);
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        fn send_key(key: u16, down: bool) {
            let event = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: key,
                        dwFlags: if down { 0 } else { KEYEVENTF_KEYUP },
                        dwExtraInfo: PHYSICAL_TEST_TAG,
                        ..Default::default()
                    },
                },
            };
            assert_eq!(
                unsafe { SendInput(1, &event, size_of::<INPUT>() as i32) },
                1
            );
            settle();
        }
        fn settle() {
            for _ in 0..5 {
                crate::test_support::pump();
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        struct Window(HWND, HWND);
        impl Drop for Window {
            fn drop(&mut self) {
                unsafe {
                    let release = INPUT {
                        r#type: INPUT_KEYBOARD,
                        Anonymous: INPUT_0 {
                            ki: KEYBDINPUT {
                                wVk: VK_LMENU,
                                dwFlags: KEYEVENTF_KEYUP,
                                dwExtraInfo: MENU_MASK_TAG,
                                ..Default::default()
                            },
                        },
                    };
                    SendInput(1, &release, size_of::<INPUT>() as i32);
                    DestroyWindow(self.0);
                    SetForegroundWindow(self.1);
                }
            }
        }
        let _dpi = crate::test_support::Dpi::new();
        let mut cfg = crate::test_support::cfg();
        cfg.alt_hold_ms = 120;
        let hooks = Hooks::start(&cfg).unwrap();
        on_hook(|| unsafe {
            let replacement = SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(replay_physical),
                GetModuleHandleW(null()),
                0,
            );
            assert!(!replacement.is_null());
            UnhookWindowsHookEx(KEYBOARD_HOOK.with(|slot| slot.replace(replacement)));
        })
        .unwrap();
        let root = unsafe {
            let class = WNDCLASSW {
                lpfnWndProc: Some(probe),
                hInstance: GetModuleHandleW(null()),
                lpszClassName: w!("WinSizeP.MenuMask.Test"),
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let menu = CreateMenu();
            let popup = CreatePopupMenu();
            assert_ne!(AppendMenuW(popup, MF_STRING, 1, w!("Test item")), 0);
            assert_ne!(AppendMenuW(menu, MF_POPUP, popup as usize, w!("&File")), 0);
            let previous = GetForegroundWindow();
            let hwnd = CreateWindowExW(
                0,
                class.lpszClassName,
                w!("WinSizeP Alt menu regression"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                480,
                300,
                null_mut(),
                menu,
                class.hInstance,
                null(),
            );
            assert!(!hwnd.is_null());
            let foreground_thread = GetWindowThreadProcessId(previous, null_mut());
            let attached = AttachThreadInput(GetCurrentThreadId(), foreground_thread, 1) != 0;
            SetForegroundWindow(hwnd);
            SetFocus(hwnd);
            if attached {
                AttachThreadInput(GetCurrentThreadId(), foreground_thread, 0);
            }
            Window(hwnd, previous)
        };
        settle();
        assert_eq!(unsafe { GetForegroundWindow() }, root.0);
        for (scenario, hold, action, menus, masks) in [
            ("short tap", false, None, 1, 0),
            ("native Alt+F", false, Some(0x46), 1, 0),
            ("hold only", true, None, 0, 1),
            ("captured shortcut", true, Some(0x51), 0, 1),
            ("canceled preview", true, Some(VK_ESCAPE), 0, 1),
        ] {
            on_hook(|| INPUT.with_borrow_mut(Input::reset_gesture)).unwrap();
            configure(&cfg).unwrap();
            unsafe {
                RemovePropW(root.0, w!("MenuCount"));
                RemovePropW(root.0, w!("MaskCount"));
            }
            send_key(VK_LMENU, true);
            // The application excludes its own windows in production; this fixture
            // is deliberately an in-process target so it can observe menu messages.
            on_hook(|| INPUT.with_borrow_mut(|input| input.target_allowed = true)).unwrap();
            if hold {
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while !on_hook(|| INPUT.with_borrow(|input| input.hold_active)).unwrap() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "hold activation: {scenario}, state={:?}",
                        on_hook(|| INPUT.with_borrow(|input| (
                            input.pressed,
                            input.hold_started,
                            input.hold_active,
                            input.hold_ms,
                            unsafe { GetAsyncKeyState(VK_LMENU as i32) },
                            input.last_event,
                            unsafe { GetTickCount() }
                        )))
                        .unwrap()
                    );
                    settle();
                }
            }
            if let Some(key) = action {
                if key == VK_ESCAPE {
                    on_hook(|| INPUT.with_borrow_mut(|input| input.preview_tracking = true))
                        .unwrap();
                }
                send_key(key, true);
                send_key(key, false);
            }
            send_key(VK_LMENU, false);
            let count = unsafe { GetPropW(root.0, w!("MenuCount")) } as usize;
            // This probe handles SC_KEYMENU without entering a modal menu loop;
            // a mnemonic may therefore report both keydown and WM_SYSCHAR.
            assert_eq!(count > 0, menus > 0, "{scenario}: menu activations={count}");
            assert_eq!(
                unsafe { GetPropW(root.0, w!("MaskCount")) } as usize,
                masks,
                "{scenario}"
            );
            assert!(
                unsafe { GetAsyncKeyState(VK_MENU as i32) } >= 0,
                "Alt remained down: {scenario}"
            );
        }
        drop(hooks);
    }
    #[test]
    fn alt_hold_activates_at_the_deadline_once_and_consumes_matching_release() {
        use super::*;
        let _hooks = Hooks::escape_only().unwrap();
        let mut input = pending_hold(100);
        input.advance_hold(2099);
        assert!(!input.hold_active);
        assert!(next_event().is_none());
        input.advance_hold(2100);
        assert!(input.hold_active);
        assert_eq!(next_event().unwrap().message, ALT_HOLD);
        input.advance_hold(2300);
        assert!(next_event().is_none());
        assert!(input.tap.is_none());
        assert!(input.keyboard(0x51, true, 2301));
        assert_eq!(next_event().unwrap().message, ARMED);
        assert_eq!(next_event().unwrap().message, ACTION);
        assert!(!input.keyboard(LEFT, false, 2302));
        assert_eq!(next_event().unwrap().message, ALT_RELEASE);
        assert!(input.keyboard(0x51, false, 2303));
        assert!(!input.hold_active);
        assert!(next_event().is_none());
    }
    #[test]
    fn early_application_shortcuts_and_repeats_cancel_the_whole_hold() {
        use super::*;
        let _hooks = Hooks::escape_only().unwrap();
        for key in [
            0x51,
            0x4d,
            u32::from(VK_TAB),
            u32::from(VK_CONTROL),
            u32::from(VK_SHIFT),
        ] {
            let mut input = pending_hold(100);
            let (index, flag) = bit(0x4d);
            input.cast_toggles[index] = flag;
            assert!(!input.keyboard(key, true, 500), "{key}");
            assert!(input.hold_started.is_none());
            input.advance_hold(2500);
            assert!(!input.hold_active);
            assert!(!input.keyboard(key, true, 2501), "repeat {key}");
            assert!(!input.keyboard(key, false, 2502), "release {key}");
            assert!(!input.keyboard(0x51, true, 2503));
            assert!(!input.keyboard(LEFT, false, 2504));
            assert!(input.eaten.iter().all(|mask| *mask == 0));
            while let Some(event) = next_event() {
                assert_eq!(event.message, CANCEL);
            }
        }
    }
    #[test]
    fn release_or_escape_cannot_leave_a_pending_hold_to_reactivate() {
        use super::*;
        let _hooks = Hooks::escape_only().unwrap();
        let mut input = pending_hold(100);
        assert!(!input.keyboard(LEFT, false, 300));
        input.advance_hold(2500);
        assert!(!input.hold_active);
        assert!(next_event().is_none());
        let mut input = pending_hold(100);
        input.advance_hold(2100);
        assert_eq!(next_event().unwrap().message, ALT_HOLD);
        input.preview_tracking = true;
        assert!(input.keyboard(u32::from(VK_ESCAPE), true, 2101));
        assert_eq!(next_event().unwrap().message, CANCEL);
        input.advance_hold(5000);
        assert!(!input.hold_active);
        assert!(!input.keyboard(0x51, true, 5001));
    }
    #[test]
    fn zero_delay_and_tick_wraparound_activate_correctly() {
        use super::*;
        let _hooks = Hooks::escape_only().unwrap();
        let mut input = pending_hold(100);
        input.hold_ms = 0;
        assert!(input.keyboard(0x51, true, 100));
        assert_eq!(next_event().unwrap().message, ALT_HOLD);
        assert_eq!(next_event().unwrap().message, ARMED);
        assert_eq!(next_event().unwrap().message, ACTION);
        let mut input = pending_hold(u32::MAX - 999);
        input.advance_hold(999);
        assert!(!input.hold_active);
        input.advance_hold(1000);
        assert!(input.hold_active);
        assert_eq!(next_event().unwrap().message, ALT_HOLD);
    }
    #[test]
    fn double_tap_still_arms_without_waiting_for_the_hold_delay() {
        use super::*;
        let _hooks = Hooks::escape_only().unwrap();
        let mut input = pending_hold(100);
        input.second = true;
        input.hold_active = true;
        assert!(input.keyboard(LEFT, false, 130));
        assert_eq!(next_event().unwrap().message, ALT_RELEASE);
        assert_eq!(next_event().unwrap().message, ARMED);
        assert!(input.keyboard(0x51, true, 200));
        assert_eq!(next_event().unwrap().message, ACTION);
    }
    #[test]
    fn mouse_buttons_pass_through_before_activation_and_enable_after_the_delay() {
        use super::*;
        let _hooks = Hooks::escape_only().unwrap();
        on_hook(|| {
            let button = |message, time| {
                let data = MSLLHOOKSTRUCT {
                    time,
                    ..Default::default()
                };
                // SAFETY: the event lives through the synchronous callback.
                unsafe {
                    mouse(
                        HC_ACTION as i32,
                        message as usize,
                        (&data as *const _) as isize,
                    )
                }
            };
            INPUT.with_borrow_mut(|input| {
                *input = pending_hold(100);
                input.gesture_mask = 1;
            });
            assert_eq!(button(WM_RBUTTONDOWN, 500), 0);
            assert_eq!(button(WM_RBUTTONUP, 501), 0);
            assert_eq!(button(WM_RBUTTONDOWN, 2500), 0);
            INPUT.with_borrow(|input| assert!(input.drag.is_none() && !input.hold_active));
            INPUT.with_borrow_mut(|input| input.hold_started = Some(3000));
            assert_eq!(button(WM_RBUTTONDOWN, 5000), 1);
            assert_eq!(button(WM_RBUTTONUP, 5001), 1);
        })
        .unwrap();
        assert_eq!(next_event().unwrap().message, ALT_HOLD);
        assert_eq!(next_event().unwrap().message, GRID_SHOW);
        assert_eq!(next_event().unwrap().message, CLICK);
        assert!(next_event().is_none());
    }
    pub fn pause_worker() -> super::mpsc::Sender<()> {
        use super::*;
        let (resume, wait) = mpsc::channel();
        let (started, ready) = mpsc::channel();
        queue_hook(move || {
            started.send(()).unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(10));
        })
        .unwrap();
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        resume
    }
    #[test]
    fn preview_cleanup_and_alt_query_do_not_wait_for_a_paused_worker() {
        use super::*;
        let _hooks = Hooks::start(&crate::test_support::cfg()).unwrap();
        on_hook(|| {
            INPUT.with_borrow_mut(|input| {
                input.pressed = 1;
                input.second = true;
                input.update_mouse().unwrap();
            });
        })
        .unwrap();
        track_preview(true).unwrap();
        let resume = pause_worker();
        let started = std::time::Instant::now();
        assert!(alt_held());
        cancel();
        track_preview(false).unwrap();
        assert!(started.elapsed() < Duration::from_millis(250));
        drop(resume);
        on_hook(|| {
            INPUT.with_borrow(|input| {
                assert!(input.mode == Mode::Idle);
                assert!(!input.preview_tracking);
            });
        })
        .unwrap();
        // A queued stop must not overtake a later start.
        track_preview(true).unwrap();
        on_hook(|| INPUT.with_borrow(|input| assert!(input.preview_tracking))).unwrap();
    }
    fn alt_event(key: u32, down: bool, flags: u32) -> isize {
        use super::*;
        let event = KBDLLHOOKSTRUCT {
            vkCode: key,
            flags: flags | if down { 0 } else { LLKHF_UP },
            // SAFETY: GetTickCount takes no pointers or handles.
            time: unsafe { GetTickCount() },
            ..Default::default()
        };
        // SAFETY: event stays valid for the synchronous hook callback.
        unsafe {
            keyboard(
                HC_ACTION as i32,
                if down { WM_SYSKEYDOWN } else { WM_SYSKEYUP } as usize,
                (&event as *const _) as isize,
            )
        }
    }
    /// Replay physical Alt events on the hook thread for interactive UI tests.
    /// Those tests also use SendInput to maintain the async key state for polling.
    pub fn physical_alt_sender() -> impl Fn(u16, bool) + Send {
        use super::*;
        let (commands, wake) = DRIVER.with_borrow(|driver| {
            let driver = driver.as_ref().unwrap();
            (driver.commands.clone(), driver.wake.clone())
        });
        move |key, down| {
            assert!(matches!(u32::from(key), LEFT | RIGHT));
            let (sent, received) = mpsc::sync_channel(1);
            commands
                .send(Control::Run(Box::new(move || {
                    alt_event(u32::from(key), down, 0);
                    sent.send(()).unwrap();
                })))
                .unwrap();
            // SAFETY: this closure retains the Arc owning the worker's wake event.
            assert_ne!(unsafe { SetEvent(wake.0 as HANDLE) }, 0);
            received.recv_timeout(Duration::from_secs(2)).unwrap();
        }
    }
    #[test]
    fn injected_alt_does_not_start_shortcuts_or_preview() {
        use super::*;
        let _hooks = Hooks::escape_only().unwrap();
        configure(&crate::test_support::cfg()).unwrap();
        on_hook(|| {
            INPUT.with_borrow_mut(|input| input.cast_keys = true);
            for key in [LEFT, RIGHT] {
                for flags in [
                    LLKHF_INJECTED,
                    LLKHF_LOWER_IL_INJECTED,
                    LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED,
                ] {
                    for down in [true, false, true, false] {
                        assert_eq!(alt_event(key, down, flags), 0);
                        INPUT.with_borrow(|input| {
                            assert_eq!(input.pressed, 0);
                            assert!(input.mode == Mode::Idle);
                            assert!(input.tap.is_none() && input.last_tap.is_none());
                            assert!(!input.second && input.mouse.is_null());
                        });
                    }
                }
            }
        })
        .unwrap();
        assert!(next_event().is_none());
    }
    #[test]
    fn injected_alt_preserves_physical_hold_and_does_not_enable_mouse_gestures() {
        use super::*;
        let _hooks = Hooks::escape_only().unwrap();
        on_hook(|| {
            assert_eq!(alt_event(RIGHT, true, LLKHF_EXTENDED), 0);
            INPUT.with_borrow_mut(|input| {
                assert_eq!(input.pressed, 2);
                input.alt_mask = 1;
                input.gesture_mask = 1;
                input.hold_ms = 0;
                input.target_allowed = true;
            });
            assert_eq!(alt_event(RIGHT, false, LLKHF_INJECTED), 0);
            assert_eq!(alt_event(LEFT, true, LLKHF_INJECTED), 0);
            INPUT.with_borrow(|input| assert_eq!(input.pressed, 2));
            let click = || {
                let event = MSLLHOOKSTRUCT::default();
                // SAFETY: event stays valid for the synchronous hook callback.
                unsafe {
                    mouse(
                        HC_ACTION as i32,
                        WM_RBUTTONDOWN as usize,
                        (&event as *const _) as isize,
                    )
                }
            };
            assert_eq!(click(), 0);
            INPUT.with_borrow(|input| assert!(input.drag.is_none()));
            // The same physical Alt event still opens the preview and enables gestures.
            assert_eq!(alt_event(LEFT, true, LLKHF_ALTDOWN), 0);
            assert_eq!(click(), 1);
            INPUT.with_borrow(|input| {
                assert_eq!(input.pressed, 3);
                assert!(input.drag.is_some());
            });
            alt_event(LEFT, false, LLKHF_ALTDOWN);
            alt_event(RIGHT, false, LLKHF_EXTENDED);
        })
        .unwrap();
        assert_eq!(next_event().unwrap().message, ALT_HOLD);
        assert_eq!(next_event().unwrap().message, GRID_SHOW);
        assert_eq!(next_event().unwrap().message, ALT_RELEASE);
        assert!(next_event().is_none());
    }
    #[test]
    fn missing_mouse_release_after_permission_boundary_is_reconciled() {
        use super::*;
        let _hooks = Hooks::escape_only().unwrap();
        on_hook(|| {
            INPUT.with_borrow_mut(|input| {
                input.cast_buttons = 1 << 2;
                input.point = POINT { x: 45, y: 67 };
                input.reconcile_keys(|_| false);
                assert_eq!(input.cast_buttons, 0);
            })
        })
        .unwrap();
        let event = next_event().unwrap();
        assert_eq!(
            (event.message, event.wParam, event.lParam),
            (CAST_MOUSE, 2, crate::mouse_cast::RELEASE as isize)
        );
        assert_eq!((cast_point().x, cast_point().y), (45, 67));
    }
    #[test]
    fn timeout_cancels_pending_command_and_worker_recovers() {
        use super::*;
        let _hooks = Hooks::escape_only().unwrap();
        let (send, ready) = mpsc::channel();
        DRIVER.with_borrow(|driver| {
            let driver = driver.as_ref().unwrap();
            driver
                .commands
                .send(Control::Run(Box::new(move || {
                    send.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(2200));
                })))
                .unwrap();
            unsafe {
                SetEvent(driver.wake.0 as HANDLE);
            }
        });
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        let called = Arc::new(AtomicBool::new(false));
        let queued = called.clone();
        let error = on_hook(move || queued.store(true, Ordering::Release)).unwrap_err();
        assert_eq!(error, "Input thread did not respond");
        let end = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(msg) = next_event()
                && msg.message == RECOVERED
            {
                assert_eq!(msg.wParam, 1);
                break;
            }
            assert!(std::time::Instant::now() < end);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !called.load(Ordering::Acquire),
            "Timed-out state changes executed later"
        );
        assert_eq!(on_hook(|| 42).unwrap(), 42);
    }
    #[test]
    #[ignore = "read-only integrity check against an explicitly supplied elevated window; uses a temporary normal child window"]
    fn elevated_target_rejection_does_not_poison_the_next_normal_gesture() {
        use super::*;
        let Some(text) = std::env::var_os("WINSIZEP_ELEVATED_TEST_HWND") else {
            eprintln!("SKIPPED: no elevated window handle supplied");
            return;
        };
        let elevated =
            usize::from_str_radix(text.to_string_lossy().trim_start_matches("0x"), 16).unwrap();
        assert!(!crate::security::can_control_window(elevated as HWND));
        assert!(!desktop::allowed(elevated as HWND, &[]));
        let cfg = crate::test_support::cfg();
        assert!(desktop::Drag::start(elevated as HWND, POINT::default(), false, &cfg).is_err());
        let request = crate::cli::Request {
            x: Some(0),
            ..Default::default()
        };
        assert!(
            desktop::apply(
                elevated as HWND,
                &request,
                &cfg,
                std::path::Path::new(""),
                POINT::default()
            )
            .is_err()
        );
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "resident::tests::target_window"])
            .env("WINSIZEP_PREVIEW_TARGET", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let result = (|| -> Result<(), String> {
            let until = std::time::Instant::now() + Duration::from_secs(5);
            let normal = loop {
                let hwnd = unsafe {
                    FindWindowW(
                        windows_sys::core::w!("STATIC"),
                        windows_sys::core::w!("WinSizeP preview test target"),
                    )
                };
                if !hwnd.is_null() {
                    break hwnd as usize;
                }
                if std::time::Instant::now() > until {
                    return Err("No temporary normal window".into());
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            assert!(desktop::allowed(normal as HWND, &[]));
            let _hooks = Hooks::start(&cfg)?;
            on_hook(move || unsafe {
                INPUT.with_borrow_mut(|input| {
                    input.select_target(elevated as HWND);
                    input.pressed = 1;
                    assert!(!input.target_allowed);
                });
                let mouse_event = MSLLHOOKSTRUCT::default();
                assert_eq!(
                    mouse(
                        HC_ACTION as i32,
                        WM_RBUTTONDOWN as usize,
                        (&mouse_event as *const _) as isize
                    ),
                    0
                );
                INPUT.with_borrow_mut(|input| {
                    assert!(input.drag.is_none());
                    input.keyboard(LEFT, false, 1);
                    assert_eq!(input.pressed, 0);
                    input.select_target(normal as HWND);
                    input.pressed = 1;
                    input.hold_active = true;
                    assert!(input.target_allowed);
                    assert!(input.keyboard(0x51, true, 2));
                    assert!(input.keyboard(0x51, false, 3));
                    input.keyboard(LEFT, false, 4);
                });
            })?;
            let mut saw_action = false;
            while let Some(event) = next_event() {
                saw_action |= event.message == ACTION && event.wParam == 0x51;
            }
            assert!(
                saw_action,
                "Normal window shortcut did not recover after access denial"
            );
            unsafe {
                PostMessageW(normal as HWND, WM_CLOSE, 0, 0);
            }
            Ok(())
        })();
        if child.try_wait().unwrap().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
        result.unwrap();
    }
    #[test]
    fn missing_alt_release_recovers_without_changing_bindings() {
        let mut input = super::Input {
            pressed: 1,
            alt_mask: 1,
            gesture_mask: 1,
            target_allowed: true,
            ..Default::default()
        };
        input.tap = Some((super::LEFT, 100));
        input.eaten[1] = 1 << 13;
        input.bound[1] = 1 << 13;
        assert!(input.reconcile_keys(|_| false));
        assert_eq!(input.pressed, 0);
        assert!(!input.second && !input.target_allowed);
        assert!(input.tap.is_none());
        assert_eq!(input.eaten[1], 1 << 13);
        assert_eq!(input.bound[1], 1 << 13);
        assert!(!input.keyboard(super::LEFT, true, 200));
        assert_eq!(
            input.pressed, 1,
            "A fresh Alt press remained stuck as a repeat"
        );
        input.keyboard(super::LEFT, false, 201);
    }
    #[test]
    fn reconciliation_does_not_cancel_intercepted_buttons_or_repeat_shortcuts() {
        use super::*;
        let mut input = Input {
            pressed: 1,
            second: true,
            cast_buttons: 1 << 2,
            drag: Some(PointerAction {
                start: POINT::default(),
                button: 2,
                resize: false,
                draggable: false,
                active: false,
                canceled: false,
            }),
            ..Default::default()
        };
        input.eaten[1] = 1 << 13;
        assert!(!input.reconcile_keys(|_| false));
        assert!(input.second && input.drag.is_some());
        assert_eq!(input.cast_buttons, 1 << 2);
        assert_eq!(input.eaten[1], 1 << 13);
        assert!(input.keyboard(77, true, 10));
        assert!(input.keyboard(77, false, 11));
        assert_eq!(input.eaten[1], 0);
    }
    #[test]
    fn stalled_worker_rearms_hooks_and_clears_incomplete_gestures() {
        use super::*;
        let _hooks = Hooks::escape_only().unwrap();
        on_hook(|| {
            INPUT.with_borrow_mut(|input| {
                input.cast_keys = true;
                input.alt_mask = 1;
                input.pressed = 1;
                input.second = true;
            })
        })
        .unwrap();
        let (send, receive) = mpsc::channel();
        DRIVER.with_borrow(|driver| {
            let driver = driver.as_ref().unwrap();
            driver
                .commands
                .send(Control::Run(Box::new(move || {
                    std::thread::sleep(Duration::from_millis(950));
                    let _ = send.send(());
                })))
                .unwrap();
            unsafe {
                SetEvent(driver.wake.0 as HANDLE);
            }
        });
        receive.recv_timeout(Duration::from_secs(2)).unwrap();
        let end = std::time::Instant::now() + Duration::from_secs(2);
        let recovered = loop {
            if let Some(event) = next_event()
                && event.message == RECOVERED
            {
                break event.wParam;
            }
            assert!(
                std::time::Instant::now() < end,
                "No recovery after worker stall"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(recovered, 1);
        on_hook(|| {
            INPUT.with_borrow(|input| {
                assert_eq!(input.pressed, 0);
                assert!(!input.second);
                assert!(input.cast_keys);
                assert_eq!(input.alt_mask, 1);
            })
        })
        .unwrap();
    }
    #[test]
    fn modal_annotation_drain_preserves_actions_and_release_order() {
        use super::*;
        let _hooks = Hooks::escape_only().unwrap();
        on_hook(|| {
            post(ACTION, 0x4d, 0);
            post_event(
                CAST_MOUSE,
                2,
                crate::mouse_cast::RELEASE as isize,
                Some(POINT { x: 30, y: 40 }),
            );
            post(ALT_RELEASE, 0, 0);
        })
        .unwrap();
        let event = next_annotation_event().unwrap();
        assert_eq!(
            (event.message, event.wParam, event.lParam),
            (CAST_MOUSE, 2, crate::mouse_cast::RELEASE as isize)
        );
        assert_eq!(cast_point().x, 30);
        assert!(next_annotation_event().is_none());
        assert_eq!(next_event().unwrap().message, ACTION);
        assert_eq!(next_event().unwrap().message, ALT_RELEASE);
    }
    #[test]
    fn moved_position_does_not_wait_for_stalled_hook_worker() {
        use super::*;
        let _hooks = Hooks::escape_only().unwrap();
        on_hook(|| post_move(POINT { x: -500, y: 250 })).unwrap();
        let event = next_event().unwrap();
        assert_eq!(event.message, MOVE);
        let (send, receive) = mpsc::channel();
        DRIVER.with_borrow(|driver| {
            let driver = driver.as_ref().unwrap();
            driver
                .commands
                .send(Control::Run(Box::new(move || {
                    send.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(300));
                })))
                .unwrap();
            unsafe {
                SetEvent(driver.wake.0 as HANDLE);
            }
        });
        receive.recv_timeout(Duration::from_secs(1)).unwrap();
        let start = std::time::Instant::now();
        let p = moved();
        assert_eq!((p.x, p.y), (-500, 250));
        assert!(start.elapsed() < Duration::from_millis(100));
    }
    #[test]
    fn blocked_worker_does_not_hold_shutdown_or_lose_event_ownership() {
        use super::*;
        let hooks = Hooks::escape_only().unwrap();
        let wake = Arc::downgrade(&hooks.wake);
        let (ready, started) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        DRIVER.with_borrow(|slot| {
            let driver = slot.as_ref().unwrap();
            driver
                .commands
                .send(Control::Run(Box::new(move || {
                    ready.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(900));
                    done.send(()).unwrap();
                })))
                .unwrap();
            unsafe {
                SetEvent(driver.wake.0 as HANDLE);
            }
        });
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        let start = std::time::Instant::now();
        drop(hooks);
        assert!(start.elapsed() < Duration::from_millis(750));
        assert!(
            wake.upgrade().is_some(),
            "A running worker lost its event handle"
        );
        let second = Hooks::escape_only().unwrap();
        finished.recv_timeout(Duration::from_secs(2)).unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(1);
        while wake.upgrade().is_some() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            wake.upgrade().is_none(),
            "Exited worker leaked the event handle"
        );
        drop(second);
    }
    #[test]
    fn reentrant_hook_callbacks_do_not_panic_or_consume_input() {
        let _hooks = super::Hooks::escape_only().unwrap();
        super::on_hook(|| {
            super::INPUT.with(|cell| {
                let mut input = cell.borrow_mut();
                input.cast_keys = true;
                input.cast_mouse = true;
                unsafe {
                    let key = super::KBDLLHOOKSTRUCT {
                        vkCode: 0x41,
                        ..Default::default()
                    };
                    let mouse = super::MSLLHOOKSTRUCT::default();
                    assert_eq!(
                        super::keyboard(
                            super::HC_ACTION as i32,
                            super::WM_KEYDOWN as usize,
                            (&key as *const _) as isize
                        ),
                        0
                    );
                    assert_eq!(
                        super::mouse(
                            super::HC_ACTION as i32,
                            super::WM_LBUTTONDOWN as usize,
                            (&mouse as *const _) as isize
                        ),
                        0
                    );
                }
            });
        })
        .unwrap();
        assert!(super::next_event().is_none());
    }
    #[test]
    fn scroll_bursts_keep_latest_axes_without_starving_button_release() {
        let _hooks = super::Hooks::escape_only().unwrap();
        super::on_hook(|| {
            for n in 0..10000 {
                super::post_event(
                    super::CAST_MOUSE,
                    if n % 2 == 0 { 8 } else { 9 },
                    0,
                    Some(super::POINT { x: n, y: -n }),
                );
                super::post_event(super::CAST_MOUSE, 11, 0, Some(super::POINT { x: n, y: 1 }));
            }
            super::post_event(
                super::CAST_MOUSE,
                1,
                crate::mouse_cast::RELEASE as isize,
                Some(super::POINT { x: 10000, y: 2 }),
            );
        })
        .unwrap();
        let first = super::next_event().unwrap();
        assert_eq!((first.message, first.wParam), (super::CAST_MOUSE, 9));
        assert_eq!(super::cast_point().x, 9999);
        let release = super::next_event().unwrap();
        assert_eq!(
            (release.wParam, release.lParam),
            (1, crate::mouse_cast::RELEASE as isize)
        );
        let horizontal = super::next_event().unwrap();
        assert_eq!(horizontal.wParam, 11);
        assert!(super::next_event().is_none());
    }
    #[test]
    fn annotation_preview_keeps_clicks_and_escape_independent() {
        let _hooks = super::Hooks::escape_only().unwrap();
        super::annotation_preview(true).unwrap();
        super::on_hook(|| unsafe {
            let mouse = super::MSLLHOOKSTRUCT::default();
            assert_eq!(
                super::mouse(
                    super::HC_ACTION as i32,
                    super::WM_LBUTTONDOWN as usize,
                    (&mouse as *const _) as isize
                ),
                0
            );
        })
        .unwrap();
        assert!(
            super::next_event().is_none(),
            "A normal click canceled annotation preview"
        );
        assert!(
            super::on_hook(|| super::INPUT.with_borrow_mut(|input| input.keyboard(27, true, 1)))
                .unwrap()
        );
        assert_eq!(super::next_event().unwrap().message, super::CANCEL);
    }
    #[test]
    fn mouse_annotations_preserve_release_and_os_scroll_direction() {
        use super::*;
        assert_eq!(cast_mouse_action(WM_LBUTTONDOWN, 0), Some((1, 0)));
        assert_eq!(
            cast_mouse_action(WM_LBUTTONUP, 0),
            Some((1, crate::mouse_cast::RELEASE))
        );
        assert_eq!(
            cast_mouse_action(WM_RBUTTONUP, 0),
            Some((2, crate::mouse_cast::RELEASE))
        );
        assert_eq!(
            cast_mouse_action(WM_MBUTTONUP, 0),
            Some((4, crate::mouse_cast::RELEASE))
        );
        let positive = 120u32 << 16;
        let negative = (-120i16 as u16 as u32) << 16;
        assert_eq!(cast_mouse_action(WM_MOUSEWHEEL, positive), Some((8, 0)));
        assert_eq!(cast_mouse_action(WM_MOUSEWHEEL, negative), Some((9, 0)));
        assert_eq!(cast_mouse_action(WM_MOUSEHWHEEL, positive), Some((11, 0)));
        assert_eq!(cast_mouse_action(WM_MOUSEHWHEEL, negative), Some((10, 0)));
        assert_eq!(cast_mouse_action(WM_MOUSEWHEEL, 0), None);
    }
    #[test]
    fn annotation_hotkeys_work_without_a_target_and_consume_only_their_keys() {
        let hooks = super::Hooks::escape_only().unwrap();
        super::configure(&crate::test_support::cfg()).unwrap();
        super::on_hook(|| {
            super::INPUT.with_borrow_mut(|input| {
                input.pressed = 1;
                input.target_allowed = false;
                input.hold_active = true;
                assert!(input.keyboard(0x4d, true, 1));
                assert!(input.keyboard(0x4d, true, 2));
                assert!(input.keyboard(0x4d, false, 3));
                assert!(!input.keyboard(0x4a, true, 4));
            })
        })
        .unwrap();
        let event = super::next_event().unwrap();
        assert_eq!((event.message, event.wParam), (super::ACTION, 0x4d));
        drop(hooks);
    }
    #[test]
    fn annotation_hooks_observe_keys_and_mouse_without_consuming_input() {
        let _hooks = super::Hooks::escape_only().unwrap();
        super::set_casting(true, true).unwrap();
        super::on_hook(|| unsafe {
            let key = super::KBDLLHOOKSTRUCT {
                vkCode: 0x41,
                scanCode: 0x1e,
                ..Default::default()
            };
            assert_eq!(
                super::keyboard(
                    super::HC_ACTION as i32,
                    super::WM_KEYDOWN as usize,
                    (&key as *const _) as isize
                ),
                0
            );
            let mouse = super::MSLLHOOKSTRUCT {
                pt: super::POINT { x: -100, y: 250 },
                ..Default::default()
            };
            assert_eq!(
                super::mouse(
                    super::HC_ACTION as i32,
                    super::WM_LBUTTONDOWN as usize,
                    (&mouse as *const _) as isize
                ),
                0
            );
        })
        .unwrap();
        let key = super::next_event().unwrap();
        assert_eq!((key.message, key.wParam), (super::CAST_KEY, 0x41));
        let mouse = super::next_event().unwrap();
        assert_eq!((mouse.message, mouse.wParam), (super::CAST_MOUSE, 1));
        let point = super::cast_point();
        assert_eq!((point.x, point.y), (-100, 250));
        super::set_casting(false, false).unwrap();
        super::on_hook(|| super::INPUT.with_borrow(|input| assert!(input.mouse.is_null())))
            .unwrap();
    }
    #[test]
    fn annotation_sample_preview_can_be_canceled_with_escape() {
        let _hooks = super::Hooks::escape_only().unwrap();
        super::track_preview(true).unwrap();
        let eaten =
            super::on_hook(|| super::INPUT.with_borrow_mut(|input| input.keyboard(27, true, 1)))
                .unwrap();
        assert!(eaten);
        assert_eq!(super::next_event().unwrap().message, super::CANCEL);
        super::track_preview(false).unwrap();
    }
    #[test]
    fn scrolling_keys_are_consumed_only_for_an_overflowing_tip() {
        let _hooks = super::Hooks::escape_only().unwrap();
        super::tip_scrollable(true, 0);
        let eaten =
            super::on_hook(|| super::INPUT.with_borrow_mut(|input| input.keyboard(0x22, true, 1)))
                .unwrap();
        assert!(eaten);
        let event = super::next_event().unwrap();
        assert_eq!((event.message, event.wParam), (super::TIP_SCROLL, 400));
        super::tip_visible(false);
        super::on_hook(|| {
            super::INPUT.with_borrow_mut(|input| {
                assert!(input.keyboard(0x22, false, 2));
                assert!(!input.keyboard(0x22, true, 3));
            })
        })
        .unwrap();
    }
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
