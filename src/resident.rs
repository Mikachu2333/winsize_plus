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
        Foundation::*,
        Storage::FileSystem::*,
        System::{SystemInformation::GetLocalTime, Threading::*},
        UI::WindowsAndMessaging::*,
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
    window: HWND,
    command: String,
    help_line: usize,
    hide: Option<Instant>,
    wait: Option<Instant>,
    reload: Option<Instant>,
    snapshot: Option<Vec<u8>>,
    exit: Option<bool>,
}
impl App {
    fn refresh_preview(&mut self) -> Result<(), String> {
        self.preview.hide();
        input::track_preview(false)?;
        if self.cfg.preview
            && let Some(hwnd) = self.held_window.or(self.wait.map(|_| self.window))
        {
            self.preview.start(hwnd, &self.cfg)?;
            input::track_preview(true)?;
        }
        Ok(())
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
            "time" => {
                let mut time = SYSTEMTIME::default();
                unsafe { GetLocalTime(&mut time) };
                self.notice(&format!(
                    "{:02}:{:02}:{:02}",
                    time.wHour, time.wMinute, time.wSecond
                ))
            }
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
        let cfg = config::load(&self.path, None)?;
        self.snapshot = fs::read(&self.path).ok();
        let sync_task =
            !automatic || (cfg.autostart, cfg.elevate) != (self.cfg.autostart, self.cfg.elevate);
        let restart = automatic && (cfg.restart || cfg.elevate != self.cfg.elevate);
        input::configure(&cfg)?;
        self.frames.reset(cfg.fps);
        self.overlay.guides(self.overlay.mode(), &cfg)?;
        self.cfg = cfg;
        self.wait = None;
        self.hold = None;
        self.held_window = None;
        self.refresh_preview()?;
        if restart {
            self.exit = Some(true);
            return Ok(());
        }
        if sync_task {
            crate::logon::sync(self.cfg.autostart, self.cfg.elevate)?;
        }
        self.config_notice()
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
                let lines: Vec<_> = config::help()
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .collect();
                let pages = lines.len().div_ceil(8);
                let page = self.help_line % pages;
                self.help_line = page + 1;
                self.notice(&format!(
                    "{}  {}/{}\n{}",
                    self.cfg.tr("Help", "帮助"),
                    page + 1,
                    pages,
                    lines[page * 8..lines.len().min((page + 1) * 8)]
                        .iter()
                        .map(|line| line.trim_start_matches('#').trim())
                        .collect::<Vec<_>>()
                        .join("\n")
                ))
            }
            "command" => {
                input::command_mode();
                self.command.clear();
                self.hide = None;
                self.overlay.tip(": ▏", &self.cfg)
            }
            "reload" => self.load(false),
            "restart" | "quit" => {
                self.exit = Some(action == "restart");
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
                input::set_guides(mode != 0)
            }
            name if self.cfg.presets.contains_key(name) || matches!(name, "top" | "ignore") => {
                let req = match name {
                    "top" => Request {
                        top: Some(Switch::Toggle),
                        ..Default::default()
                    },
                    "ignore" => Request {
                        ignore: Some(Switch::Toggle),
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
                    })?
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
                let action = self
                    .cfg
                    .commands
                    .get(name)
                    .map(String::as_str)
                    .unwrap_or(name)
                    .to_string();
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
                self.window = msg.wParam as HWND;
                self.wait = Some(later(self.cfg.wait_ms));
                self.refresh_preview()?;
                self.status(&self.cfg.show.clone())
            }
            input::ALT_HOLD => {
                self.hold = Some((later(self.cfg.double_ms), msg.wParam as HWND));
                Ok(())
            }
            input::ALT_RELEASE => {
                self.hold = None;
                if self.held_window.take().is_some() {
                    self.refresh_preview()?;
                }
                Ok(())
            }
            input::ACTION => {
                self.wait = None;
                self.refresh_preview()?;
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
                if self.overlay.mode() != 0
                    && let Some(frame) = self.frames.moved(point)?
                {
                    self.overlay.follow(frame, &self.cfg)?;
                }
                Ok(())
            }
            input::CLICK => {
                let point = input::click_point(msg.lParam);
                let screen = desktop::screen(point, 0)?;
                let cell = screen.work.grid(point.x, point.y) as u32 + 1;
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
            input::CANCEL => {
                self.wait = None;
                self.refresh_preview()?;
                self.clear();
                Ok(())
            }
            WM_QUIT => {
                self.exit = Some(false);
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
    fn deadlines(&mut self) -> Result<(), String> {
        let now = Instant::now();
        if let Some((when, hwnd)) = self.hold
            && now >= when
        {
            self.hold = None;
            self.held_window = Some(hwnd);
            self.refresh_preview()?;
        }
        if self.wait.is_some_and(|when| now >= when) {
            self.wait = None;
            self.refresh_preview()?;
            if input::is_waiting() {
                input::cancel();
            }
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
            w!("Local\\WinSizeP.Resident.v1"),
        );
        if handle.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        CloseHandle(handle);
        true
    }
}
pub fn run(path: &Path) -> Result<bool, String> {
    unsafe {
        let handle = CreateMutexW(null(), 0, w!("Local\\WinSizeP.Resident.v1"));
        if handle.is_null() {
            return Err(err());
        }
        let status = GetLastError();
        let _instance = Instance(handle);
        if status == ERROR_ALREADY_EXISTS {
            return Ok(false);
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
        let snapshot = fs::read(path).ok();
        let cfg = config::load(path, None)?;
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
            window: null_mut(),
            command: String::new(),
            help_line: 0,
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
            if let Some(restart) = app.exit {
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
                        && let Err(problem) = app.overlay.follow(point, &app.cfg)
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
