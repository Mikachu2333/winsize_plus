use crate::{
    cli::{Request, Switch},
    config::{self, Config},
    desktop::{self, err, wide},
    framerate::FrameClock,
    input,
    overlay::Overlay,
    preview::Preview,
};
use std::{
    fs,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use windows_sys::{
    Win32::{
        Foundation::*, Storage::FileSystem::*, System::Threading::*, UI::WindowsAndMessaging::*,
    },
    core::w,
};

struct Watch(HANDLE);
impl Drop for Watch {
    fn drop(&mut self) {
        unsafe {
            FindCloseChangeNotification(self.0);
        }
    }
}
struct Instance(HANDLE);
impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn later(ms: u32) -> Instant {
    Instant::now() + Duration::from_millis(ms.into())
}
struct App {
    cfg: Config,
    path: PathBuf,
    overlay: Overlay,
    tray: crate::tray::Tray,
    frames: FrameClock,
    preview: Preview,
    hold: Option<(Instant, HWND)>,
    held_window: Option<HWND>,
    preview_latched: bool,
    preview_until: Option<Instant>,
    preview_check: Option<Instant>,
    window: HWND,
    drag: Option<desktop::Drag>,
    command: String,
    hide: Option<Instant>,
    wait: Option<Instant>,
    reload: Option<Instant>,
    snapshot: Option<Vec<u8>>,
    exit: Option<crate::ResidentExit>,
}
impl App {
    fn refresh_preview(&mut self) -> Result<(), String> {
        self.preview.hide();
        input::track_preview(false)?;
        if self.cfg.preview
            && let Some(hwnd) = self.held_window
        {
            self.preview.start(hwnd, &self.cfg)?;
            input::track_preview(true)?;
        }
        Ok(())
    }
    fn close_preview(&mut self) -> Result<(), String> {
        self.hold = None;
        self.wait = None;
        self.held_window = None;
        self.preview_until = None;
        self.preview_check = None;
        self.preview_latched = false;
        self.preview.hide();
        input::track_preview(false)
    }
    fn notice(&mut self, text: &str) -> Result<(), String> {
        self.hide = Some(later(self.cfg.duration));
        self.overlay.tip(text, &self.cfg)
    }
    fn status(&mut self, kind: &str) -> Result<(), String> {
        match kind {
            "monitor" => {
                let number = desktop::screen(desktop::cursor()?, 0)?.number;
                self.notice(&format!("{} {number}", self.cfg.tr("Monitor", "屏幕")))
            }
            "time" => self.notice(&desktop::time_text(&self.cfg.time_format)),
            "window" => self.notice(&desktop::window_text(self.window, &self.cfg.window_format)),
            _ => {
                self.clear();
                Ok(())
            }
        }
    }
    fn clear(&mut self) {
        self.hide = None;
        self.overlay.hide_tip()
    }
    fn load(&mut self, automatic: bool) -> Result<(), String> {
        let bytes = fs::read(&self.path).ok();
        if automatic && bytes.is_some() && bytes == self.snapshot {
            return Ok(());
        }
        let mut cfg = config::load(&self.path, None)?;
        self.snapshot = fs::read(&self.path).ok();
        let sync_task =
            !automatic || (cfg.autostart, cfg.elevate) != (self.cfg.autostart, self.cfg.elevate);
        let mut restart = automatic && cfg.elevate != self.cfg.elevate;
        let mut startup_error = None;
        if sync_task && let Err(error) = crate::logon::sync(cfg.autostart, cfg.elevate) {
            // Keep independent settings working when consent is canceled; retry startup on
            // the next explicit reload/edit instead of repeatedly prompting for UAC.
            cfg.autostart = self.cfg.autostart;
            cfg.elevate = self.cfg.elevate;
            restart = false;
            startup_error = Some(error);
        }
        input::configure(&cfg)?;
        self.frames.reset(cfg.fps);
        self.overlay.guides(self.overlay.mode(), &cfg)?;
        self.cfg = cfg;
        self.close_preview()?;
        if restart {
            self.exit = Some(crate::ResidentExit::Restart(Some(self.cfg.elevate)));
            return Ok(());
        }
        if let Some(error) = startup_error {
            self.notice(&error)
        } else {
            self.config_notice()
        }
    }
    fn config_notice(&mut self) -> Result<(), String> {
        if self.cfg.warnings.is_empty() {
            self.notice(self.cfg.tr("Configuration reloaded", "配置已重载"))
        } else {
            self.notice(&format!(
                "{}: {}",
                self.cfg.tr(
                    "Invalid fields restored to defaults",
                    "以下字段有误，已恢复默认值"
                ),
                self.cfg.warnings.join(", ")
            ))
        }
    }
    fn act(&mut self, action: &str) -> Result<(), String> {
        match action {
            "settings" => crate::settings::open(),
            "time" | "monitor" => self.status(action),
            "version" => self.notice(concat!("WinSizeP ", env!("CARGO_PKG_VERSION"))),
            "help" => {
                self.hide = None;
                self.overlay.tip(&self.cfg.help_text(), &self.cfg)
            }
            "command" => {
                input::command_mode();
                self.command.clear();
                self.hide = None;
                self.overlay.tip(": ▏", &self.cfg)
            }
            "reload" => self.load(false),
            "restart" | "quit" => {
                self.exit = Some(if action == "restart" {
                    crate::ResidentExit::Restart(None)
                } else {
                    crate::ResidentExit::Quit
                });
                Ok(())
            }
            "guide" | "cross" | "horizontal" | "vertical" => {
                let current = self.overlay.mode();
                let wanted = match action {
                    "horizontal" => 1,
                    "vertical" => 2,
                    _ => 3,
                };
                let mode = if wanted == current || (action == "guide" && current != 0) {
                    0
                } else {
                    wanted
                };
                self.overlay.guides(mode, &self.cfg)?;
                self.frames.reset(self.cfg.fps);
                input::set_guides(mode != 0)?;
                self.notice(if mode == 0 {
                    self.cfg.tr("Crosshair hidden", "十字线已隐藏")
                } else {
                    self.cfg.tr("Crosshair shown", "十字线已显示")
                })
            }
            "borders" => {
                if self.preview.visible() {
                    self.held_window = None;
                    self.preview.hide();
                    input::track_preview(false)?;
                    self.notice(self.cfg.tr("Outlines hidden", "框线已隐藏"))
                } else {
                    self.held_window = Some(self.window);
                    self.preview_latched = true;
                    self.refresh_preview()?;
                    self.notice(self.cfg.tr("Outlines shown", "框线已显示"))
                }
            }
            name if self.cfg.presets.contains_key(name) || matches!(name, "top" | "ignore") => {
                let req = match name {
                    "top" => Request {
                        top: Some(Switch::Toggle),
                        ..Default::default()
                    },
                    "ignore" => Request {
                        ignore: Some(Switch::On),
                        ..Default::default()
                    },
                    _ => Request {
                        preset: Some(name.into()),
                        ..Default::default()
                    },
                };
                desktop::apply(self.window, &req, &self.cfg, &self.path, desktop::cursor()?)?;
                if name == "ignore" {
                    self.cfg = config::load(&self.path, None)?;
                    let id = desktop::identity(self.window);
                    let excluded = self.cfg.exclusions.iter().any(|rule| rule.matches(&id));
                    self.notice(if excluded {
                        self.cfg.tr("Excluded", "已排除")
                    } else {
                        self.cfg.tr("Included", "已取消排除")
                    })?;
                    input::configure(&self.cfg)?;
                } else if name == "top" {
                    let top = unsafe {
                        GetWindowLongW(self.window, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST != 0
                    };
                    self.notice(if top {
                        self.cfg.tr("Always on top enabled", "已置顶")
                    } else {
                        self.cfg.tr("Always on top disabled", "已取消置顶")
                    })?;
                }
                Ok(())
            }
            _ => Err(format!(
                "{}: {action}",
                self.cfg.tr("Unknown command", "未知命令")
            )),
        }
    }
    fn typed(&mut self, key: u32) -> Result<(), String> {
        match key {
            13 => {
                let text = std::mem::take(&mut self.command);
                let name = text.trim();
                self.clear();
                if name.is_empty() {
                    return Ok(());
                }
                let action = self.cfg.action(name);
                return self.act(&action);
            }
            27 => {
                self.command.clear();
                self.clear();
                return Ok(());
            }
            8 => {
                self.command.pop();
            }
            _ if self.command.len() < 80 => {
                if let Some(ch) = char::from_u32(key) {
                    self.command.push(ch)
                }
            }
            _ => {}
        }
        self.overlay.tip(&format!(": {}▏", self.command), &self.cfg)
    }
    fn event(&mut self, msg: MSG) -> Result<(), String> {
        match msg.message {
            crate::tray::MENU => {
                if let Some(action) = self.tray.menu(&self.cfg, msg.lParam)? {
                    self.act(action)?;
                }
                Ok(())
            }
            input::ARMED => {
                self.close_preview()?;
                self.window = msg.wParam as HWND;
                self.wait = Some(later(self.cfg.duration));
                self.hold = None;
                self.held_window = None;
                self.preview_latched = true;
                self.refresh_preview()?;
                self.status(&self.cfg.show.clone())
            }
            input::ALT_HOLD => {
                self.close_preview()?;
                self.window = msg.wParam as HWND;
                self.hold = Some((later(self.cfg.duration), self.window));
                self.wait = None;
                self.held_window = None;
                self.preview_latched = false;
                self.refresh_preview()?;
                self.status(&self.cfg.show.clone())
            }
            input::ALT_RELEASE => {
                self.hold = None;
                if !self.preview_latched {
                    self.close_preview()?;
                }
                Ok(())
            }
            input::ACTION => {
                self.close_preview()?;
                self.clear();
                if let Some(action) = self.cfg.keys.get(&(msg.wParam as u32)).cloned() {
                    self.act(&action)?
                }
                Ok(())
            }
            input::TYPE => self.typed(msg.wParam as u32),
            input::MOVE => {
                let point = input::moved();
                if self.preview.visible() {
                    self.preview.follow(point, &self.cfg)?;
                }
                if (self.overlay.mode() != 0 || self.drag.is_some())
                    && let Some(frame) = self.frames.moved(point)?
                {
                    self.follow_frame(frame)?;
                }
                Ok(())
            }
            input::CLICK => {
                self.close_preview()?;
                let point = input::click_point(msg.lParam);
                let screen = desktop::screen(point, 0)?;
                let cell = screen.full.grid(point.x, point.y) as u32 + 1;
                if let Some(preset) = self.cfg.cell(screen.number, cell) {
                    desktop::apply(
                        msg.wParam as HWND,
                        &Request {
                            preset: Some(preset.to_owned()),
                            ..Default::default()
                        },
                        &self.cfg,
                        &self.path,
                        point,
                    )?
                }
                Ok(())
            }
            input::GRID_SHOW => {
                self.close_preview()?;
                self.window = msg.wParam as HWND;
                self.hold = None;
                self.preview.grid(self.window, &self.cfg)?;
                self.preview_check = Some(later(100));
                input::track_preview(true)
            }
            input::DRAG_START => {
                self.drag = Some(desktop::Drag::start(
                    msg.wParam as HWND,
                    input::drag_origin(),
                    msg.lParam != 0,
                    &self.cfg,
                )?);
                self.hold = None;
                self.held_window = None;
                self.wait = None;
                self.preview.hide();
                self.preview_until = None;
                self.preview_check = None;
                input::track_preview(false)?;
                self.clear();
                self.frames.reset(self.cfg.fps);
                self.follow_frame(desktop::cursor()?)
            }
            input::DRAG_END => {
                self.follow_frame(input::moved())?;
                self.drag = None;
                self.frames.reset(self.cfg.fps);
                Ok(())
            }
            input::CANCEL => {
                input::cancel();
                self.drag = None;
                self.clear();
                self.wait = None;
                self.hold = None;
                if self.preview.reset(&self.cfg)? {
                    self.preview_until = Some(later(self.cfg.duration));
                    return Ok(());
                }
                self.close_preview()?;
                Ok(())
            }
            WM_QUIT => {
                self.exit = Some(crate::ResidentExit::Quit);
                Ok(())
            }
            _ => {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                Ok(())
            }
        }
    }
    fn follow_frame(&mut self, point: POINT) -> Result<(), String> {
        if let Some(drag) = &self.drag {
            drag.follow(point)?;
        }
        self.overlay.follow(point, &self.cfg)
    }
    fn deadlines(&mut self) -> Result<(), String> {
        let now = Instant::now();
        if let Some((when, hwnd)) = self.hold
            && now >= when
        {
            self.hold = None;
            self.held_window = Some(hwnd);
            self.refresh_preview()?;
            self.preview_check = Some(later(100));
        }
        if self.wait.is_some_and(|when| now >= when) {
            self.wait = None;
            self.held_window = Some(self.window);
            self.refresh_preview()?;
            self.preview_until = Some(later(self.cfg.duration));
            self.preview_check = Some(later(100));
        }
        if self.preview_until.is_some_and(|when| now >= when) {
            input::cancel();
            self.close_preview()?;
        }
        if self.preview_check.is_some_and(|when| now >= when) {
            if unsafe { IsWindow(self.window) } == 0
                || (!self.preview_latched && !input::alt_held())
                || unsafe { GetForegroundWindow() } != self.window
            {
                input::cancel();
                self.close_preview()?;
            } else {
                self.preview_check = Some(later(100));
            }
        }
        if self.preview.deadline().is_some_and(|when| now >= when) {
            self.preview.tick(&self.cfg)?;
        }
        if self.hide.is_some_and(|when| now >= when) {
            self.clear()
        }
        if self.reload.is_some_and(|when| now >= when) {
            self.reload = None;
            self.load(true)?
        }
        Ok(())
    }
    fn timeout(&self) -> u32 {
        [
            self.wait,
            self.hide,
            self.reload,
            self.hold.map(|(when, _)| when),
            self.preview.deadline(),
            self.preview_until,
            self.preview_check,
        ]
        .into_iter()
        .flatten()
        .min()
        .map_or(INFINITE, |when| {
            when.saturating_duration_since(Instant::now()).as_millis() as u32 + 1
        })
    }
}

pub fn running() -> bool {
    unsafe {
        let handle = OpenMutexW(
            SYNCHRONIZATION_SYNCHRONIZE,
            0,
            w!("Local\\D5C9A59069D907909B153C76CEBDC023"),
        );
        if handle.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        CloseHandle(handle);
        true
    }
}
pub fn run(path: &Path) -> Result<crate::ResidentExit, String> {
    unsafe {
        let handle = CreateMutexW(null(), 0, w!("Local\\D5C9A59069D907909B153C76CEBDC023"));
        if handle.is_null() {
            if GetLastError() == ERROR_ACCESS_DENIED && running() {
                return Ok(crate::ResidentExit::Quit);
            }
            return Err(err());
        }
        let status = GetLastError();
        let _instance = Instance(handle);
        if status == ERROR_ALREADY_EXISTS {
            return Ok(crate::ResidentExit::Quit);
        }
        let watch = FindFirstChangeNotificationW(
            wide(path.parent().ok_or("Config directory")?).as_ptr(),
            0,
            FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_SIZE,
        );
        if watch == INVALID_HANDLE_VALUE {
            return Err(err());
        }
        let watch = Watch(watch);
        let cfg = config::load(path, None)?;
        let snapshot = fs::read(path).ok();
        let overlay = Overlay::new()?;
        let _hooks = input::Hooks::start(&cfg)?;
        let mut app = App {
            frames: FrameClock::new(cfg.fps)?,
            cfg,
            path: path.into(),
            overlay,
            tray: crate::tray::Tray::new()?,
            preview: Preview::new()?,
            hold: None,
            held_window: None,
            preview_latched: false,
            preview_until: None,
            preview_check: None,
            window: null_mut(),
            drag: None,
            command: String::new(),
            hide: None,
            wait: None,
            reload: None,
            snapshot,
            exit: None,
        };
        let mut msg = MSG::default();
        if let Err(problem) = crate::logon::sync(app.cfg.autostart, app.cfg.elevate) {
            app.notice(&problem)?;
        } else if !app.cfg.warnings.is_empty() {
            app.config_notice()?;
        }
        loop {
            if let Err(problem) = app.deadlines() {
                let _ = app.notice(&problem);
            }
            if let Some(restart) = app.exit.take() {
                return Ok(restart);
            }
            match MsgWaitForMultipleObjectsEx(
                2,
                [watch.0, app.frames.handle()].as_ptr(),
                app.timeout(),
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            ) {
                WAIT_OBJECT_0 => {
                    desktop::check(FindNextChangeNotification(watch.0))?;
                    app.reload = Some(later(160));
                }
                value if value == WAIT_OBJECT_0 + 1 => {
                    if let Some(point) = app.frames.tick()
                        && let Err(problem) = app.follow_frame(point)
                    {
                        let _ = app.notice(&problem);
                    }
                }
                value if value == WAIT_OBJECT_0 + 2 => {
                    for _ in 0..64 {
                        if PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) == 0 {
                            break;
                        }
                        if let Err(problem) = app.event(msg) {
                            let _ = app.notice(&problem);
                        }
                        if app.exit.is_some() {
                            break;
                        }
                    }
                }
                WAIT_TIMEOUT => {}
                _ => return Err(err()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::{Command, Stdio},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };
    use windows_sys::Win32::{
        System::LibraryLoader::GetModuleHandleW, UI::Input::KeyboardAndMouse::*,
    };
    const TARGET: &str = "WinSizeP preview test target";

    #[test]
    #[ignore = "helper process for the global input integration test"]
    fn target_window() {
        if std::env::var_os("WINSIZEP_PREVIEW_TARGET").is_none() {
            return;
        }
        unsafe {
            let hwnd = CreateWindowExW(
                0,
                w!("STATIC"),
                wide(TARGET).as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                200,
                150,
                800,
                550,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!hwnd.is_null());
            SetForegroundWindow(hwnd);
            let until = Instant::now() + Duration::from_secs(20);
            while Instant::now() < until && IsWindow(hwnd) != 0 {
                MsgWaitForMultipleObjectsEx(0, null(), 50, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            DestroyWindow(hwnd);
        }
    }
    fn key(vk: u16, down: bool) {
        unsafe {
            let event = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: vk,
                        dwFlags: if down { 0 } else { KEYEVENTF_KEYUP },
                        ..Default::default()
                    },
                },
            };
            assert_eq!(SendInput(1, &event, size_of::<INPUT>() as i32), 1);
        }
    }
    fn visible(state: &AtomicBool, wanted: bool) -> Result<(), String> {
        let until = Instant::now() + Duration::from_secs(2);
        while state.load(Ordering::SeqCst) != wanted {
            if Instant::now() >= until {
                return Err(format!("Preview did not become {wanted}"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
    #[test]
    #[ignore = "sends Alt/Esc to a temporary test window; needs an interactive desktop"]
    fn alt_preview_lifecycle() {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "resident::tests::target_window",
                "--nocapture",
            ])
            .env("WINSIZEP_PREVIEW_TARGET", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut doc = config::help().parse::<toml_edit::DocumentMut>().unwrap();
        doc["tip"]["duration_ms"] = toml_edit::value(300);
        let cfg = Config::parse(&doc).unwrap();
        let overlay = Overlay::new().unwrap();
        let _hooks = input::Hooks::start(&cfg).unwrap();
        let mut app = App {
            frames: FrameClock::new(cfg.fps).unwrap(),
            cfg,
            path: PathBuf::new(),
            overlay,
            tray: crate::tray::Tray::new().unwrap(),
            preview: Preview::new().unwrap(),
            hold: None,
            held_window: None,
            preview_latched: false,
            preview_until: None,
            preview_check: None,
            window: null_mut(),
            drag: None,
            command: String::new(),
            hide: None,
            wait: None,
            reload: None,
            snapshot: None,
            exit: None,
        };
        let state = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let observed = state.clone();
        let finished = done.clone();
        let controller = std::thread::spawn(move || {
            let result = (|| {
                let until = Instant::now() + Duration::from_secs(5);
                let target = loop {
                    let hwnd = unsafe { FindWindowW(w!("STATIC"), wide(TARGET).as_ptr()) };
                    if !hwnd.is_null() {
                        break hwnd;
                    }
                    if Instant::now() > until {
                        return Err("Target window missing".into());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                };
                unsafe {
                    // Tests share a desktop with the terminal. Attach only for this explicit
                    // activation so foreground-lock policy does not invalidate the hook test.
                    let current = GetCurrentThreadId();
                    let foreground = GetWindowThreadProcessId(GetForegroundWindow(), null_mut());
                    let target_thread = GetWindowThreadProcessId(target, null_mut());
                    windows_sys::Win32::System::Threading::AttachThreadInput(
                        current, foreground, 1,
                    );
                    windows_sys::Win32::System::Threading::AttachThreadInput(
                        current,
                        target_thread,
                        1,
                    );
                    SetForegroundWindow(target);
                    windows_sys::Win32::System::Threading::AttachThreadInput(
                        current,
                        target_thread,
                        0,
                    );
                    windows_sys::Win32::System::Threading::AttachThreadInput(
                        current, foreground, 0,
                    );
                    SetCursorPos(600, 400);
                }
                std::thread::sleep(Duration::from_millis(100));
                if unsafe { GetForegroundWindow() } != target {
                    unsafe {
                        let mut rect = RECT::default();
                        GetWindowRect(target, &mut rect);
                        SetWindowPos(target, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
                        SetCursorPos(rect.left + 100, rect.top + 12);
                        let down = INPUT {
                            r#type: INPUT_MOUSE,
                            Anonymous: INPUT_0 {
                                mi: MOUSEINPUT {
                                    dwFlags: MOUSEEVENTF_LEFTDOWN,
                                    ..Default::default()
                                },
                            },
                        };
                        let up = INPUT {
                            r#type: INPUT_MOUSE,
                            Anonymous: INPUT_0 {
                                mi: MOUSEINPUT {
                                    dwFlags: MOUSEEVENTF_LEFTUP,
                                    ..Default::default()
                                },
                            },
                        };
                        SendInput(2, [down, up].as_ptr(), size_of::<INPUT>() as i32);
                        SetWindowPos(target, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
                        SetCursorPos(600, 400);
                    }
                    std::thread::sleep(Duration::from_millis(150));
                    if unsafe { GetForegroundWindow() } != target {
                        return Err(format!(
                            "Cannot focus test target {target:?}; foreground={:?}",
                            unsafe { GetForegroundWindow() }
                        ));
                    }
                }
                key(VK_LMENU, true);
                visible(&observed, true)?;
                key(VK_LMENU, false);
                visible(&observed, false)?;
                println!("held Alt preview appeared and released cleanly");
                std::thread::sleep(Duration::from_millis(650));
                key(VK_LMENU, true);
                std::thread::sleep(Duration::from_millis(30));
                key(VK_LMENU, false);
                std::thread::sleep(Duration::from_millis(50));
                key(VK_LMENU, true);
                std::thread::sleep(Duration::from_millis(30));
                key(VK_LMENU, false);
                visible(&observed, true)?;
                visible(&observed, false)?;
                println!("double Alt preview appeared and expired without Escape");
                std::thread::sleep(Duration::from_millis(650));
                key(VK_LMENU, true);
                visible(&observed, true)?;
                key(VK_ESCAPE, true);
                key(VK_ESCAPE, false);
                visible(&observed, false)?;
                key(VK_LMENU, false);
                println!("Escape canceled held preview");
                Ok::<_, String>(())
            })();
            key(VK_LMENU, false);
            finished.store(true, Ordering::SeqCst);
            result
        });
        let until = Instant::now() + Duration::from_secs(15);
        let mut failure = None;
        while !done.load(Ordering::SeqCst) && Instant::now() < until {
            if let Err(err) = app.deadlines() {
                failure = Some(err);
                break;
            }
            unsafe {
                MsgWaitForMultipleObjectsEx(
                    0,
                    null(),
                    app.timeout().min(50),
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                );
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    if let Err(err) = app.event(msg) {
                        failure = Some(err);
                        break;
                    }
                }
            }
            state.store(app.preview.visible(), Ordering::SeqCst);
        }
        let result = controller.join().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        assert!(failure.is_none(), "{failure:?}");
        result.unwrap();
    }
}
