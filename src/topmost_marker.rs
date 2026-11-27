use crate::{
    config::{Config, TopmostStyle},
    desktop::{self, err},
    fonts::Font,
    layout::Rect,
};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ptr::{null, null_mut},
};
use windows_sys::{
    Win32::{
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*},
        System::LibraryLoader::GetModuleHandleW,
        UI::{Accessibility::*, WindowsAndMessaging::*},
    },
    core::w,
};

#[cfg(not(test))]
const CLASS: *const u16 = w!("WinSizeP.TopmostMarker");
#[cfg(test)]
const CLASS: *const u16 = w!("WinSizeP.Test.TopmostMarker");
#[cfg(not(test))]
const MANAGED: *const u16 = w!("WinSizeP.TopmostTarget");
#[cfg(test)]
const MANAGED: *const u16 = w!("WinSizeP.Test.TopmostTarget");
const LETTER: *const u16 = w!("WinSizeP.TopmostLetter");
const FONT: *const u16 = w!("WinSizeP.TopmostFont");
const REFRESH: u32 = WM_APP + 110;
const TRACK: u32 = WM_APP + 111;
thread_local! {
    static RECEIVER: Cell<HWND> = const { Cell::new(null_mut()) };
    static QUEUED: Cell<bool> = const { Cell::new(false) };
    static DIRTY: Cell<bool> = const { Cell::new(false) };
    static INCOMING: RefCell<Vec<HWND>> = const { RefCell::new(Vec::new()) };
}

fn letter(cfg: &Config) -> u16 {
    cfg.keys
        .iter()
        .filter_map(|(&key, action)| {
            (cfg.action(action) == "top" && (0x41..=0x5a).contains(&key)).then_some(key as u16)
        })
        .min()
        .unwrap_or(b'P' as u16)
}

fn position(frame: Rect, style: &TopmostStyle) -> Rect {
    Rect {
        x: frame.x
            + if style.position == "bottom-right" {
                (frame.width - style.size).max(0)
            } else {
                0
            },
        y: frame.y
            + if style.position == "top-left" {
                0
            } else {
                (frame.height - style.size).max(0)
            },
        width: style.size,
        height: style.size,
    }
}

pub fn record(target: HWND, topmost: bool) -> Result<(), String> {
    // SAFETY: window properties store a value, never a pointer to foreign memory.
    unsafe {
        if topmost {
            desktop::check(SetPropW(target, MANAGED, 1usize as HANDLE))?;
        } else {
            RemovePropW(target, MANAGED);
        }
        let receiver = FindWindowExW(HWND_MESSAGE, null_mut(), CLASS, null());
        if !receiver.is_null() {
            PostMessageW(receiver, TRACK, target as usize, 0);
        }
    }
    Ok(())
}

fn incoming(target: HWND) {
    INCOMING.with_borrow_mut(|targets| {
        if !targets.contains(&target) {
            targets.push(target);
        }
    });
}

unsafe extern "system" fn event(
    _: HWINEVENTHOOK,
    event: u32,
    target: HWND,
    object: i32,
    child: i32,
    _: u32,
    _: u32,
) {
    let desktop_reorder = event == EVENT_OBJECT_REORDER
        && object == OBJID_CLIENT
        && target == unsafe { GetDesktopWindow() };
    if (!desktop_reorder && object != OBJID_WINDOW) || child != CHILDID_SELF as i32 {
        return;
    }
    if !matches!(
        event,
        EVENT_OBJECT_DESTROY
            | EVENT_OBJECT_SHOW
            | EVENT_OBJECT_HIDE
            | EVENT_OBJECT_REORDER
            | EVENT_OBJECT_LOCATIONCHANGE
            | EVENT_OBJECT_STATECHANGE
            | EVENT_OBJECT_CLOAKED
            | EVENT_OBJECT_UNCLOAKED
            | EVENT_SYSTEM_MINIMIZESTART
            | EVENT_SYSTEM_MINIMIZEEND
    ) {
        return;
    }
    let marked = !unsafe { GetPropW(target, MANAGED) }.is_null();
    if marked {
        incoming(target);
    }
    if marked || matches!(event, EVENT_OBJECT_DESTROY | EVENT_OBJECT_REORDER) {
        RECEIVER.with(|receiver| {
            if !receiver.get().is_null() && !QUEUED.replace(true) {
                unsafe {
                    PostMessageW(receiver.get(), REFRESH, 0, 0);
                }
            }
        });
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match message {
        TRACK => {
            incoming(wp as HWND);
            DIRTY.set(true);
            0
        }
        REFRESH => {
            QUEUED.set(false);
            DIRTY.set(true);
            0
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_NCHITTEST => HTTRANSPARENT as LRESULT,
        WM_ERASEBKGND => 1,
        WM_DPICHANGED => 0,
        WM_PAINT => {
            // SAFETY: painting uses live window properties and restores the selected font.
            unsafe {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut paint);
                let mut area = RECT::default();
                GetClientRect(hwnd, &mut area);
                SetDCBrushColor(dc, 0x00ffff);
                FillRect(dc, &area, GetStockObject(DC_BRUSH));
                let font = GetPropW(hwnd, FONT);
                if !font.is_null() {
                    let prior = SelectObject(dc, font);
                    let text = [GetPropW(hwnd, LETTER) as usize as u16, 0];
                    SetBkMode(dc, TRANSPARENT as i32);
                    SetTextColor(dc, 0);
                    DrawTextW(
                        dc,
                        text.as_ptr(),
                        1,
                        &mut area,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                    );
                    SelectObject(dc, prior);
                }
                EndPaint(hwnd, &paint);
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wp, lp) },
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
struct Badge {
    window: Window,
    position: Option<Rect>,
    letter: u16,
    blocked_order: Option<(HWND, HWND)>,
}
fn place_badge(mut place: impl FnMut(bool) -> Result<(), u32>) -> Result<bool, String> {
    match place(false) {
        Ok(()) => Ok(false),
        Err(ERROR_ACCESS_DENIED) => {
            // UIPI can reject hWndInsertAfter even when the badge and target are
            // accessible. Retain the badge's order while updating its geometry.
            place(true)
                .map(|()| true)
                .map_err(|code| std::io::Error::from_raw_os_error(code as i32).to_string())
        }
        Err(code) => Err(std::io::Error::from_raw_os_error(code as i32).to_string()),
    }
}
impl Badge {
    fn new(font: &Font, letter: u16, size: i32) -> Result<Self, String> {
        // SAFETY: the shared font outlives every badge window.
        unsafe {
            let window = CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE,
                CLASS,
                null(),
                WS_POPUP,
                0,
                0,
                size,
                size,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            if window.is_null() {
                return Err(err());
            }
            let window = Window(window);
            desktop::check(SetPropW(window.0, FONT, font.0))?;
            desktop::check(SetPropW(window.0, LETTER, letter as usize as HANDLE))?;
            desktop::check(SetLayeredWindowAttributes(window.0, 0, 255, LWA_ALPHA))?;
            Ok(Self {
                window,
                position: None,
                letter,
                blocked_order: None,
            })
        }
    }
    fn sync(&mut self, target: HWND, letter: u16, style: &TopmostStyle) -> Result<bool, String> {
        unsafe {
            if IsWindow(target) == 0 || GetPropW(target, MANAGED).is_null() {
                return Ok(false);
            }
            if !desktop::topmost(target)? {
                RemovePropW(target, MANAGED);
                return Ok(false);
            }
            if self.letter != letter {
                desktop::check(SetPropW(self.window.0, LETTER, letter as usize as HANDLE))?;
                InvalidateRect(self.window.0, null(), 0);
                self.letter = letter;
            }
            let mut cloaked = 0u32;
            DwmGetWindowAttribute(
                target,
                DWMWA_CLOAKED as u32,
                (&mut cloaked as *mut u32).cast(),
                size_of::<u32>() as u32,
            );
            if IsWindowVisible(target) == 0 || IsIconic(target) != 0 || cloaked != 0 {
                if self.position.take().is_some() {
                    ShowWindow(self.window.0, SW_HIDE);
                }
                return Ok(true);
            }
            let frame = desktop::bounds(target)?.1;
            if frame.width <= 0 || frame.height <= 0 {
                ShowWindow(self.window.0, SW_HIDE);
                self.position = None;
                return Ok(true);
            }
            let position = position(frame, style);
            let mut anchor = GetWindow(target, GW_HWNDPREV);
            let order = (anchor, GetWindow(self.window.0, GW_HWNDPREV));
            if self.position == Some(position)
                && (anchor == self.window.0 || self.blocked_order == Some(order))
            {
                return Ok(true);
            }
            let keep_order = anchor == self.window.0 || self.blocked_order == Some(order);
            if anchor == self.window.0 {
                anchor = GetWindow(self.window.0, GW_HWNDPREV);
            }
            let blocked = place_badge(|fallback| {
                if SetWindowPos(
                    self.window.0,
                    if keep_order || fallback {
                        null_mut()
                    } else {
                        anchor
                    },
                    position.x,
                    position.y,
                    style.size,
                    style.size,
                    SWP_NOACTIVATE
                        | SWP_NOOWNERZORDER
                        | SWP_SHOWWINDOW
                        | SWP_NOSENDCHANGING
                        | if keep_order || fallback {
                            SWP_NOZORDER
                        } else {
                            0
                        },
                ) != 0
                {
                    Ok(())
                } else {
                    Err(GetLastError())
                }
            })?;
            self.blocked_order =
                (blocked || (keep_order && self.blocked_order.is_some())).then(|| {
                    (
                        GetWindow(target, GW_HWNDPREV),
                        GetWindow(self.window.0, GW_HWNDPREV),
                    )
                });
            self.position = Some(position);
            Ok(true)
        }
    }
}

#[derive(Default)]
pub struct Markers {
    badges: HashMap<HWND, Badge>,
    font: Option<Font>,
    window: Option<Window>,
    hooks: Vec<HWINEVENTHOOK>,
    letter: u16,
    style: TopmostStyle,
}
impl Drop for Markers {
    fn drop(&mut self) {
        for hook in self.hooks.drain(..) {
            unsafe {
                UnhookWinEvent(hook);
            }
        }
        if self
            .window
            .as_ref()
            .is_some_and(|window| RECEIVER.get() == window.0)
        {
            RECEIVER.set(null_mut());
            QUEUED.set(false);
            DIRTY.set(false);
            INCOMING.with_borrow_mut(Vec::clear);
        }
    }
}
impl Markers {
    fn start(cfg: &Config) -> Result<Self, String> {
        // SAFETY: callbacks and their receiver stay on this thread until the hooks are removed.
        unsafe {
            let instance = GetModuleHandleW(null());
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: CLASS,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                return Err(err());
            }
            let receiver = CreateWindowExW(
                0,
                CLASS,
                null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                instance,
                null(),
            );
            if receiver.is_null() {
                return Err(err());
            }
            let window = Window(receiver);
            let mut markers = Self {
                badges: HashMap::new(),
                hooks: Vec::new(),
                font: cfg
                    .topmost_marker
                    .enabled
                    .then(|| Font::new("SystemFont", cfg.topmost_marker.size / 2, 96, true, false))
                    .transpose()?,
                window: Some(window),
                letter: letter(cfg),
                style: cfg.topmost_marker.clone(),
            };
            desktop::check(ChangeWindowMessageFilterEx(
                receiver,
                TRACK,
                MSGFLT_ALLOW,
                null_mut(),
            ))?;
            RECEIVER.set(receiver);
            if markers.style.enabled {
                markers.listen()?;
            }
            Ok(markers)
        }
    }
    fn listen(&mut self) -> Result<(), String> {
        for (first, last) in [
            (EVENT_OBJECT_DESTROY, EVENT_OBJECT_UNCLOAKED),
            (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
        ] {
            let hook = unsafe {
                SetWinEventHook(
                    first,
                    last,
                    null_mut(),
                    Some(event),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            if hook.is_null() {
                return Err(err());
            }
            self.hooks.push(hook);
        }
        Ok(())
    }
    pub fn new(cfg: &Config) -> Result<Self, String> {
        let mut markers = Self::start(cfg)?;
        markers.discover()?;
        markers.poll()?;
        Ok(markers)
    }
    fn discover(&mut self) -> Result<(), String> {
        unsafe extern "system" fn visit(target: HWND, _: LPARAM) -> i32 {
            if !unsafe { GetPropW(target, MANAGED) }.is_null() {
                incoming(target);
            }
            1
        }
        unsafe {
            desktop::check(EnumWindows(Some(visit), 0))?;
        }
        DIRTY.set(true);
        Ok(())
    }
    pub fn configure(&mut self, cfg: &Config) -> Result<(), String> {
        if self.window.is_none() {
            return Ok(());
        }
        if self.style != cfg.topmost_marker {
            let font = cfg
                .topmost_marker
                .enabled
                .then(|| Font::new("SystemFont", cfg.topmost_marker.size / 2, 96, true, false))
                .transpose()?;
            self.badges.clear();
            self.font = font;
            self.style = cfg.topmost_marker.clone();
            if !self.style.enabled {
                for hook in self.hooks.drain(..) {
                    unsafe {
                        UnhookWinEvent(hook);
                    }
                }
            } else if self.hooks.is_empty() {
                self.listen()?;
            }
            self.discover()?;
        }
        let next = letter(cfg);
        if self.letter != next {
            self.letter = next;
            DIRTY.set(true);
        }
        self.poll()
    }
    pub fn poll(&mut self) -> Result<(), String> {
        if self.window.is_none() || !DIRTY.replace(false) {
            return Ok(());
        }
        let targets = INCOMING.with_borrow_mut(std::mem::take);
        if !self.style.enabled {
            return Ok(());
        }
        for target in targets {
            if !target.is_null()
                && unsafe { IsWindow(target) != 0 && !GetPropW(target, MANAGED).is_null() }
            {
                if !desktop::topmost(target)? {
                    unsafe {
                        RemovePropW(target, MANAGED);
                    }
                } else if let std::collections::hash_map::Entry::Vacant(entry) =
                    self.badges.entry(target)
                {
                    entry.insert(Badge::new(
                        self.font.as_ref().unwrap(),
                        self.letter,
                        self.style.size,
                    )?);
                }
            }
        }
        let mut error = None;
        self.badges.retain(
            |&target, badge| match badge.sync(target, self.letter, &self.style) {
                Ok(keep) => keep,
                Err(problem) => {
                    error = Some(problem);
                    // Keep ownership so a later window event can retry. Hide stale
                    // geometry until that succeeds; only a dead/unmarked target is removed.
                    if badge.position.take().is_some() {
                        unsafe {
                            ShowWindow(badge.window.0, SW_HIDE);
                        }
                    }
                    badge.blocked_order = None;
                    true
                }
            },
        );
        error.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "creates temporary windows to verify geometry-only positioning beside a foreign anchor"]
    fn geometry_only_position_keeps_order_while_following_target() {
        let _dpi = crate::test_support::Dpi::new();
        let cfg = crate::test_support::cfg();
        let markers = Markers::start(&cfg).unwrap();
        let target = Target::with_topmost(true);
        unsafe {
            desktop::check(SetPropW(target.hwnd, MANAGED, 1usize as HANDLE)).unwrap();
        }
        let mut badge = Badge::new(markers.font.as_ref().unwrap(), markers.letter, 20).unwrap();
        badge
            .sync(target.hwnd, markers.letter, &cfg.topmost_marker)
            .unwrap();
        // Any existing window directly above the target must not be passed to
        // SetWindowPos when geometry alone changes. A normal foreign window also
        // exercises this path without requiring elevation on the test machine.
        let blocker = Target::with_topmost(true);
        unsafe {
            desktop::check(SetWindowPos(
                blocker.hwnd,
                badge.window.0,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            ))
            .unwrap();
        }
        badge.blocked_order = Some(unsafe {
            (
                GetWindow(target.hwnd, GW_HWNDPREV),
                GetWindow(badge.window.0, GW_HWNDPREV),
            )
        });
        let mut style = cfg.topmost_marker.clone();
        style.position = "bottom-right".into();
        badge.sync(target.hwnd, markers.letter, &style).unwrap();
        assert_eq!(
            desktop::bounds(badge.window.0).unwrap().0,
            position(desktop::bounds(target.hwnd).unwrap().1, &style)
        );
        assert_ne!(unsafe { IsWindowVisible(badge.window.0) }, 0);
    }
    #[test]
    fn denied_z_order_retries_geometry_without_hiding_or_reordering() {
        let mut requests = Vec::new();
        assert!(
            place_badge(|keep_order| {
                requests.push(keep_order);
                if keep_order {
                    Ok(())
                } else {
                    Err(ERROR_ACCESS_DENIED)
                }
            })
            .unwrap()
        );
        assert_eq!(requests, [false, true]);
        requests.clear();
        assert!(
            place_badge(|keep_order| {
                requests.push(keep_order);
                Err(ERROR_INVALID_WINDOW_HANDLE)
            })
            .is_err()
        );
        assert_eq!(requests, [false]);
        assert!(place_badge(|_| Err(ERROR_ACCESS_DENIED)).is_err());
    }
    #[test]
    fn marker_letter_follows_custom_bindings_and_command_aliases() {
        let mut cfg = crate::test_support::cfg();
        cfg.keys.clear();
        cfg.keys.insert(b'T' as u32, "Pin".into());
        assert_eq!(letter(&cfg), b'T' as u16);
        cfg.keys.clear();
        cfg.commands.insert("mypin".into(), "top".into());
        cfg.keys.insert(b'K' as u32, "mypin".into());
        assert_eq!(letter(&cfg), b'K' as u16);
    }
    use std::{
        os::windows::{io::AsRawHandle, process::CommandExt},
        path::Path,
        process::{Child, Command, Stdio},
        time::{Duration, Instant},
    };
    use windows_sys::Win32::System::Threading::*;

    struct Target {
        child: Child,
        hwnd: HWND,
    }
    impl Target {
        fn new() -> Self {
            Self::with_topmost(false)
        }
        fn with_topmost(topmost: bool) -> Self {
            let child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "topmost_marker::tests::target_probe",
                    "--ignored",
                ])
                .env("WINSIZEP_TOPMOST_PROBE", "1")
                .env("WINSIZEP_TOPMOST_INITIAL", if topmost { "1" } else { "0" })
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let caption = desktop::wide(format!("WinSizeP.TopmostTest.{}", child.id()));
            let mut target = Self {
                child,
                hwnd: null_mut(),
            };
            let until = Instant::now() + Duration::from_secs(5);
            while target.hwnd.is_null() && Instant::now() < until {
                target.hwnd = unsafe { FindWindowW(w!("STATIC"), caption.as_ptr()) };
                if !target.hwnd.is_null() && unsafe { IsWindowVisible(target.hwnd) } == 0 {
                    target.hwnd = null_mut();
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(!target.hwnd.is_null(), "Temporary target did not start");
            target
        }
    }
    impl Drop for Target {
        fn drop(&mut self) {
            unsafe {
                PostMessageW(self.hwnd, WM_CLOSE, 0, 0);
            }
            if crate::wait_process(self.child.as_raw_handle(), Duration::from_millis(500)).is_err()
            {
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
        }
    }
    #[test]
    #[ignore = "isolated child window for the topmost marker test"]
    fn target_probe() {
        if std::env::var_os("WINSIZEP_TOPMOST_PROBE").is_none() {
            return;
        }
        let _dpi = crate::test_support::Dpi::new();
        let work = desktop::screen(desktop::cursor().unwrap(), 0).unwrap().work;
        let caption = desktop::wide(format!("WinSizeP.TopmostTest.{}", std::process::id()));
        let target = unsafe {
            CreateWindowExW(
                if std::env::var("WINSIZEP_TOPMOST_INITIAL").as_deref() == Ok("1") {
                    WS_EX_TOPMOST
                } else {
                    0
                },
                w!("STATIC"),
                caption.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                work.x + 40,
                work.y + 40,
                600,
                400,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            )
        };
        assert!(!target.is_null());
        unsafe {
            ShowWindow(target, SW_SHOWNOACTIVATE);
        }
        while unsafe { IsWindow(target) } != 0 {
            crate::test_support::pump();
            unsafe {
                MsgWaitForMultipleObjectsEx(0, null(), 50, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            }
        }
    }
    #[track_caller]
    fn settle(markers: &mut Markers, ready: impl Fn(&Markers) -> bool) {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            crate::test_support::pump();
            markers.poll().unwrap();
            if ready(markers) {
                return;
            }
            assert!(
                Instant::now() < until,
                "Window event did not update the marker"
            );
            unsafe {
                MsgWaitForMultipleObjectsEx(0, null(), 10, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            }
        }
    }
    fn toggle(target: HWND, cfg: &Config) {
        desktop::apply(
            target,
            &crate::cli::Request {
                top: Some(crate::cli::Switch::Toggle),
                ..Default::default()
            },
            cfg,
            Path::new(""),
            desktop::cursor().unwrap(),
        )
        .unwrap();
    }
    fn resources() -> (u32, u32) {
        unsafe {
            (
                GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS),
                GetGuiResources(GetCurrentProcess(), GR_USEROBJECTS),
            )
        }
    }
    #[test]
    #[ignore = "opens temporary child windows to verify topmost markers and their resource lifetime"]
    fn markers_follow_actual_state_position_and_config_without_leaking() {
        let _dpi = crate::test_support::Dpi::new();
        let mut cfg = crate::test_support::cfg();
        let displays = crate::test_support::displays();
        let target = Target::with_topmost(true);
        for _ in 0..2 {
            let warm = Markers::start(&cfg).unwrap();
            let badge = Badge::new(warm.font.as_ref().unwrap(), warm.letter, 20).unwrap();
            unsafe {
                ShowWindow(badge.window.0, SW_SHOWNOACTIVATE);
                UpdateWindow(badge.window.0);
            }
            drop(badge);
            drop(warm);
            crate::test_support::pump();
        }
        let before = resources();
        let mut markers = Markers::start(&cfg).unwrap();
        settle(&mut markers, |_| desktop::topmost(target.hwnd).unwrap());
        toggle(target.hwnd, &cfg);
        settle(&mut markers, |markers| {
            !desktop::topmost(target.hwnd).unwrap() && markers.badges.is_empty()
        });
        assert!(unsafe { GetPropW(target.hwnd, MANAGED) }.is_null());

        let foreground = unsafe { GetForegroundWindow() };
        toggle(target.hwnd, &cfg);
        settle(&mut markers, |markers| {
            markers.badges.contains_key(&target.hwnd)
        });
        let badge = &markers.badges[&target.hwnd];
        let mut hwnd = badge.window.0;
        let frame = desktop::bounds(target.hwnd).unwrap().1;
        let rect = desktop::bounds(hwnd).unwrap().0;
        assert_eq!(
            (rect.x, rect.y, rect.width, rect.height),
            (frame.x, frame.y, 20, 20)
        );
        assert_eq!(unsafe { GetForegroundWindow() }, foreground);
        assert_eq!(
            unsafe {
                WindowFromPoint(POINT {
                    x: rect.x + 10,
                    y: rect.y + 10,
                })
            },
            target.hwnd
        );
        assert_eq!(
            unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 }
                & (WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE),
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE
        );
        assert_eq!(unsafe { GetPropW(hwnd, LETTER) as usize }, b'P' as usize);
        unsafe {
            UpdateWindow(hwnd);
            let dc = GetDC(hwnd);
            let mut yellow = 0;
            let mut black = 0;
            for y in 0..20 {
                for x in 0..20 {
                    match GetPixel(dc, x, y) {
                        0x00ffff => yellow += 1,
                        0 => black += 1,
                        _ => {}
                    }
                }
            }
            ReleaseDC(hwnd, dc);
            assert!(yellow > 200 && black > 0, "yellow={yellow}, black={black}");
        }

        for (_, display) in displays {
            unsafe {
                assert_ne!(
                    SetWindowPos(
                        target.hwnd,
                        null_mut(),
                        display.work.x + 80,
                        display.work.y + 80,
                        650,
                        450,
                        SWP_NOACTIVATE | SWP_NOZORDER | SWP_ASYNCWINDOWPOS
                    ),
                    0
                );
            }
            settle(&mut markers, |markers| {
                let outer = desktop::bounds(target.hwnd).unwrap().0;
                let frame = desktop::bounds(target.hwnd).unwrap().1;
                outer.x == display.work.x + 80
                    && outer.y == display.work.y + 80
                    && markers.badges[&target.hwnd]
                        .position
                        .is_some_and(|rect| (rect.x, rect.y) == (frame.x, frame.y))
            });
            let rect = desktop::bounds(hwnd).unwrap().0;
            assert_eq!((rect.width, rect.height), (20, 20));
        }
        cfg.keys.retain(|_, action| action != "top");
        cfg.keys.insert(b'T' as u32, "top".into());
        markers.configure(&cfg).unwrap();
        assert_eq!(unsafe { GetPropW(hwnd, LETTER) as usize }, b'T' as usize);

        for size in [16, 20, 24, 32] {
            for corner in ["top-left", "bottom-left", "bottom-right"] {
                cfg.topmost_marker.size = size;
                cfg.topmost_marker.position = corner.into();
                markers.configure(&cfg).unwrap();
                hwnd = markers.badges[&target.hwnd].window.0;
                let frame = desktop::bounds(target.hwnd).unwrap().1;
                let rect = desktop::bounds(hwnd).unwrap().0;
                let expected_x = frame.x
                    + if corner == "bottom-right" {
                        frame.width - size
                    } else {
                        0
                    };
                let expected_y = frame.y
                    + if corner == "top-left" {
                        0
                    } else {
                        frame.height - size
                    };
                assert_eq!(
                    (rect.x, rect.y, rect.width, rect.height),
                    (expected_x, expected_y, size, size)
                );
            }
        }
        cfg.topmost_marker.enabled = false;
        markers.configure(&cfg).unwrap();
        assert!(markers.badges.is_empty() && markers.hooks.is_empty() && markers.font.is_none());
        assert!(desktop::topmost(target.hwnd).unwrap());
        let disabled_target = Target::new();
        toggle(disabled_target.hwnd, &cfg);
        settle(&mut markers, |markers| markers.badges.is_empty());
        unsafe {
            SetWindowPos(
                disabled_target.hwnd,
                HWND_NOTOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
            );
        }
        settle(&mut markers, |_| {
            !desktop::topmost(disabled_target.hwnd).unwrap()
        });
        cfg.topmost_marker.enabled = true;
        cfg.topmost_marker.size = 20;
        cfg.topmost_marker.position = "top-left".into();
        markers.configure(&cfg).unwrap();
        assert_eq!(markers.badges.len(), 1);
        assert!(unsafe { GetPropW(disabled_target.hwnd, MANAGED) }.is_null());
        drop(disabled_target);
        hwnd = markers.badges[&target.hwnd].window.0;

        unsafe {
            ShowWindowAsync(target.hwnd, SW_MINIMIZE);
        }
        settle(&mut markers, |_| unsafe {
            IsIconic(target.hwnd) != 0 && IsWindowVisible(hwnd) == 0
        });
        unsafe {
            ShowWindowAsync(target.hwnd, SW_SHOWNOACTIVATE);
        }
        settle(&mut markers, |_| unsafe {
            IsIconic(target.hwnd) == 0 && IsWindowVisible(hwnd) != 0
        });
        unsafe {
            ShowWindowAsync(target.hwnd, SW_HIDE);
        }
        settle(&mut markers, |_| unsafe {
            IsWindowVisible(target.hwnd) == 0 && IsWindowVisible(hwnd) == 0
        });
        unsafe {
            ShowWindowAsync(target.hwnd, SW_SHOWNOACTIVATE);
        }
        settle(&mut markers, |_| unsafe {
            IsWindowVisible(target.hwnd) != 0 && IsWindowVisible(hwnd) != 0
        });

        let stable = resources();
        let start = Instant::now();
        for _ in 0..20000 {
            DIRTY.set(true);
            markers.poll().unwrap();
        }
        println!(
            "cached_follow_us={:.3}; GDI/USER objects={stable:?}",
            start.elapsed().as_secs_f64() * 1e6 / 20000.0
        );
        assert_eq!(resources(), stable);
        unsafe {
            SetWindowPos(
                target.hwnd,
                HWND_NOTOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
            );
        }
        settle(&mut markers, |markers| markers.badges.is_empty());
        assert!(unsafe { GetPropW(target.hwnd, MANAGED) }.is_null());

        toggle(target.hwnd, &cfg);
        settle(&mut markers, |markers| {
            markers.badges.contains_key(&target.hwnd)
        });
        let second = Target::new();
        toggle(second.hwnd, &cfg);
        settle(&mut markers, |markers| markers.badges.len() == 2);
        drop(second);
        settle(&mut markers, |markers| markers.badges.len() == 1);
        let font = markers.font.as_ref().unwrap().0;
        let receiver = markers.window.as_ref().unwrap().0;
        drop(markers);
        unsafe {
            GdiFlush();
        }
        let mut face = LOGFONTW::default();
        assert_eq!(
            unsafe {
                GetObjectW(
                    font,
                    size_of::<LOGFONTW>() as i32,
                    (&mut face as *mut LOGFONTW).cast(),
                )
            },
            0
        );
        assert_eq!(unsafe { IsWindow(receiver) }, 0);
        let mut restored = Markers::new(&cfg).unwrap();
        assert!(restored.badges.contains_key(&target.hwnd));
        toggle(target.hwnd, &cfg);
        settle(&mut restored, |markers| markers.badges.is_empty());
        drop(restored);
        crate::test_support::pump();
        assert_eq!(resources(), before);
    }
}
