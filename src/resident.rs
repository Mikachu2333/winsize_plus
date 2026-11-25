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
    marker: crate::target_marker::TargetMarker,
    paused: bool,
    cast: crate::cast::Visuals,
    mousecast: bool,
    keycast: bool,
    cfg: Config,
    path: PathBuf,
    overlay: Overlay,
    tray: crate::tray::Tray,
    frames: FrameClock,
    preview: Preview,
    held_window: Option<HWND>,
    preview_latched: bool,
    preview_until: Option<Instant>,
    preview_check: Option<Instant>,
    window: HWND,
    drag: Option<desktop::Drag>,
    command: String,
    hide: Option<Instant>,
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
        self.marker.hide();
        self.held_window = None;
        self.preview_until = None;
        self.preview_check = None;
        self.preview_latched = false;
        self.preview.hide();
        input::track_preview(false)
    }
    fn begin_alt_preview(&mut self, window: HWND, latched: bool) -> Result<(), String> {
        self.close_preview()?;
        self.clear();
        self.window = window;
        self.held_window = Some(window);
        self.preview_latched = latched;
        self.refresh_preview()?;
        if self.cfg.target_marker {
            self.marker.show(window)?;
        }
        if self.preview.warnings().is_empty() {
            self.status(&self.cfg.show.clone())?;
        } else {
            self.notice(&self.preview.warnings().join("\n"))?;
        }
        // The default tip and outlines share one deadline, including a hidden default tip.
        self.preview_until = (!self.overlay.tip_scrollable())
            .then(|| self.hide.unwrap_or_else(|| later(self.cfg.duration)));
        self.preview_check = Some(later(100));
        Ok(())
    }
    fn notice(&mut self, text: &str) -> Result<(), String> {
        self.overlay.tip(text, &self.cfg)?;
        self.hide = (!self.overlay.tip_scrollable()).then(|| later(self.cfg.duration));
        Ok(())
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
        let loaded = config::load_snapshot(&self.path, None)?;
        let mut cfg = loaded.config;
        let snapshot = loaded.bytes;
        let unsaved = loaded.write_error;
        let current = crate::security::elevated();
        if !cfg.elevate {
            crate::security::raise_declined(false);
        }
        if !current && cfg.elevate && automatic && crate::security::raise_was_declined() {
            // Consent was refused for this request. Keep running normally instead of
            // prompting again after every edit; an explicit reload or restart retries.
            cfg.elevate = false;
        }
        let sync_task =
            !automatic || (cfg.autostart, cfg.elevate) != (self.cfg.autostart, self.cfg.elevate);
        // Only a split token has a normal half to return to: with UAC disabled or the
        // built-in Administrator account every instance is elevated.
        let lowering = current && !cfg.elevate && crate::security::can_lower();
        let raising = !current && cfg.elevate;
        let mut restart = lowering || raising;
        let mut startup_error = None;
        let task = if raising {
            // The elevated successor registers the task itself; doing it here as well
            // would ask for consent twice.
            None
        } else if lowering {
            // Updating RunLevel does not remove an administrator-owned task's protected
            // ACL. Delete while still elevated; the normal successor creates it again.
            Some(false)
        } else {
            sync_task.then_some(cfg.autostart)
        };
        if let Some(enabled) = task
            && let Err(error) = crate::logon::sync(enabled, cfg.elevate)
        {
            // Keep independent settings working when consent is canceled; retry startup on
            // the next explicit reload/edit instead of repeatedly prompting for UAC.
            cfg.autostart = self.cfg.autostart;
            cfg.elevate = self.cfg.elevate;
            restart = false;
            startup_error = Some(error);
        }
        if cfg.mousecast.enabled != self.cfg.mousecast.enabled {
            self.mousecast = cfg.mousecast.enabled;
        }
        if cfg.keycast.enabled != self.cfg.keycast.enabled {
            self.keycast = cfg.keycast.enabled;
        }
        self.cast.clear();
        if self.mousecast {
            self.cast.prepare_mouse(desktop::cursor()?, &cfg)?;
        }
        input::configure(&cfg)?;
        input::set_casting(self.mousecast, self.keycast)?;
        self.frames.reset(cfg.fps);
        self.overlay.guides(self.overlay.mode(), &cfg)?;
        self.cfg = cfg;
        self.close_preview()?;
        self.snapshot = Some(snapshot);
        if restart {
            self.exit = Some(crate::ResidentExit::Restart(Some(self.cfg.elevate)));
            return Ok(());
        }
        if let Some(error) = startup_error {
            self.notice(&error)
        } else if let Some(error) = unsaved {
            self.notice(&format!(
                "{}: {error}",
                self.cfg.tr(
                    "Configuration applied, but the file could not be updated",
                    "配置已应用，但无法更新配置文件"
                )
            ))
        } else if sync_task && let Some(warning) = crate::logon::elevation_warning(&self.cfg) {
            self.notice(&warning)
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
            "pause" => {
                let paused = !self.paused;
                input::set_paused(paused)?;
                self.paused = paused;
                self.drag = None;
                self.command.clear();
                self.cast.clear();
                self.close_preview()?;
                self.overlay.guides(0, &self.cfg)?;
                self.clear();
                self.notice(if paused {
                    self.cfg.tr("Paused", "已暂停")
                } else {
                    self.cfg.tr("Capture started", "开始捕获")
                })
            }
            "mousecast" | "keycast" => {
                let mouse = if action == "mousecast" {
                    !self.mousecast
                } else {
                    self.mousecast
                };
                let keys = if action == "keycast" {
                    !self.keycast
                } else {
                    self.keycast
                };
                if mouse && !self.mousecast {
                    self.cast.prepare_mouse(desktop::cursor()?, &self.cfg)?;
                }
                input::set_casting(mouse, keys)?;
                self.mousecast = mouse;
                self.keycast = keys;
                if !mouse {
                    self.cast.clear_mouse();
                }
                if !keys {
                    self.cast.clear_keys();
                }
                let enabled = if action == "mousecast" { mouse } else { keys };
                let name = if action == "mousecast" {
                    self.cfg.tr("Mouse annotations", "鼠标标注")
                } else {
                    self.cfg.tr("Keyboard annotations", "键盘标注")
                };
                self.notice(&format!(
                    "{name}: {}",
                    if enabled {
                        self.cfg.tr("On", "开启")
                    } else {
                        self.cfg.tr("Off", "关闭")
                    }
                ))
            }
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
                if action == "quit" {
                    // Only a refused save keeps the resident alive. An unresponsive
                    // settings window must not make quitting impossible.
                    crate::settings::close_for_exit()?;
                }
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
                    self.close_preview()?;
                    self.notice(self.cfg.tr("Outlines hidden", "框线已隐藏"))
                } else if !self.cfg.preview {
                    self.close_preview()?;
                    self.notice(self.cfg.tr(
                        "Region outlines disabled in settings",
                        "分区框线已在设置中禁用",
                    ))
                } else {
                    self.held_window = Some(self.window);
                    self.preview_latched = true;
                    self.refresh_preview()?;
                    self.notice(self.cfg.tr("Outlines shown", "框线已显示"))
                }
            }
            name if self.cfg.presets.contains_key(name)
                || matches!(name, "top" | "ignore" | "centerwindow" | "fullscreen") =>
            {
                let req = match name {
                    "fullscreen" => Request {
                        fullscreen: true,
                        ..Default::default()
                    },
                    "centerwindow" => Request {
                        center: true,
                        ..Default::default()
                    },
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
                    // Adopt our own write so the folder watcher does not reload it again
                    // and replace this notice.
                    let loaded = config::load_snapshot(&self.path, None)?;
                    self.cfg = loaded.config;
                    self.snapshot = Some(loaded.bytes);
                    self.notice(self.cfg.tr("Excluded", "已排除"))?;
                    input::configure(&self.cfg)?;
                } else if name == "top" {
                    let top = desktop::topmost(self.window)?;
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
        self.handle_event(msg)?;
        for _ in 0..64 {
            let Some(event) = input::next_event() else {
                break;
            };
            self.handle_event(event)?;
        }
        Ok(())
    }
    fn handle_event(&mut self, msg: MSG) -> Result<(), String> {
        if self.paused
            && (input::ARMED..=input::CENTER).contains(&msg.message)
            && msg.message != input::DISPLAY_CHANGED
        {
            return Ok(());
        }
        match msg.message {
            input::RECOVERED => {
                self.drag = None;
                self.cast.clear();
                self.close_preview()?;
                self.clear();
                if msg.wParam == 0 {
                    return Err("Cannot restore input hooks / 无法恢复输入钩子".into());
                }
                Ok(())
            }
            input::CAST_KEY if self.keycast => {
                if let Some(text) = crate::cast::key_caption(msg.wParam as u32, msg.lParam as u32) {
                    self.cast.key(text, input::cast_point(), &self.cfg)?;
                }
                Ok(())
            }
            input::CAST_MOUSE if self.mousecast => self.cast.mouse(
                msg.wParam as u32,
                msg.lParam as u32,
                input::cast_point(),
                &self.cfg,
            ),
            input::TIP_SCROLL => {
                self.overlay.scroll_tip(msg.wParam as i32);
                self.hide = None;
                self.preview_until = None;
                Ok(())
            }
            input::DISPLAY_CHANGED => {
                self.cfg.mousecast.color = crate::colors::accent();
                self.cfg.mousecast.dark = crate::overlay::dark();
                self.cast.tick(&self.cfg)?;
                self.overlay.follow(desktop::cursor()?, &self.cfg)?;
                self.preview.tick(&self.cfg)
            }
            crate::tray::MENU => {
                // Opening the menu changes focus and ends transient Alt previews.
                if self.preview_until.is_some() || self.preview_check.is_some() {
                    self.clear();
                    self.close_preview()?;
                }
                let cast = &mut self.cast;
                let hide = &mut self.hide;
                let overlay = &self.overlay;
                let cfg = &self.cfg;
                let mouse = self.mousecast;
                let keys = self.keycast;
                if let Some(action) = self.tray.menu(cfg, self.paused, msg.lParam, || {
                    // TrackPopupMenu owns the message loop until the menu closes.
                    if hide.is_some_and(|when| Instant::now() >= when) {
                        *hide = None;
                        overlay.hide_tip();
                    }
                    for _ in 0..64 {
                        let Some(event) = input::next_annotation_event() else {
                            break;
                        };
                        let result = match event.message {
                            input::CAST_MOUSE if mouse => cast.mouse(
                                event.wParam as u32,
                                event.lParam as u32,
                                input::cast_point(),
                                cfg,
                            ),
                            input::CAST_KEY if keys => {
                                crate::cast::key_caption(event.wParam as u32, event.lParam as u32)
                                    .map_or(Ok(()), |text| cast.key(text, input::cast_point(), cfg))
                            }
                            _ => Ok(()),
                        };
                        if result.is_err() {
                            cast.clear();
                        }
                    }
                    if let Ok(point) = desktop::cursor() {
                        let _ = cast.follow_mouse(point, cfg);
                    }
                    let _ = cast.tick(cfg);
                })? {
                    self.act(action)?;
                }
                Ok(())
            }
            input::ARMED => self.begin_alt_preview(msg.wParam as HWND, true),
            input::ALT_HOLD => self.begin_alt_preview(msg.wParam as HWND, false),
            input::ALT_RELEASE => {
                if !self.preview_latched
                    && (self.preview_until.is_some() || self.preview_check.is_some())
                {
                    self.close_preview()?;
                    self.clear();
                }
                Ok(())
            }
            input::ACTION => {
                self.marker.hide();
                let action = self.cfg.keys.get(&(msg.wParam as u32)).cloned();
                // Borders must inspect the visible preview before deciding
                // whether to show or hide it.
                if action.as_deref() != Some("borders") {
                    self.close_preview()?;
                }
                self.clear();
                if let Some(action) = action {
                    self.act(&action)?
                }
                Ok(())
            }
            input::TYPE => self.typed(msg.wParam as u32),
            input::MOVE => {
                let point = input::moved();
                if self.mousecast {
                    self.cast.follow_mouse(point, &self.cfg)?;
                }
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
                self.clear();
                let point = input::click_point();
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
            input::CENTER => {
                self.close_preview()?;
                self.clear();
                desktop::apply(
                    msg.wParam as HWND,
                    &Request {
                        center: true,
                        ..Default::default()
                    },
                    &self.cfg,
                    &self.path,
                    input::click_point(),
                )
            }
            input::GRID_SHOW => {
                self.close_preview()?;
                self.clear();
                self.window = msg.wParam as HWND;
                self.preview.grid(self.window, &self.cfg)?;
                self.preview_check = Some(later(100));
                input::track_preview(true)
            }
            input::DRAG_START => {
                let drag = desktop::Drag::start(
                    msg.wParam as HWND,
                    input::drag_origin(),
                    msg.lParam != 0,
                    &self.cfg,
                );
                self.drag = match drag {
                    Ok(drag) => Some(drag),
                    Err(error) => {
                        input::cancel();
                        self.drag = None;
                        return Err(error);
                    }
                };
                self.close_preview()?;
                self.clear();
                self.frames.reset(self.cfg.fps);
                self.follow_frame(desktop::cursor()?)
            }
            input::DRAG_END => {
                let drag = self.drag.take();
                self.frames.reset(self.cfg.fps);
                let point = input::moved();
                // End the gesture even if its final move fails (closed target,
                // denied operation or drawing error). A failure must not re-arm it.
                let moved = drag.as_ref().map_or(Ok(()), |drag| drag.follow(point));
                let drawn = self.overlay.follow(point, &self.cfg);
                moved.and(drawn)
            }
            input::CANCEL => {
                input::cancel();
                self.drag = None;
                self.clear();
                if self.preview_until.is_none() && self.preview.reset(&self.cfg)? {
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
        if self.cast.deadline().is_some_and(|when| now >= when) {
            self.cast.tick(&self.cfg)?;
        }
        if self.preview_until.is_some_and(|when| now >= when) {
            self.clear();
            input::cancel();
            self.close_preview()?;
        }
        if self.preview_check.is_some_and(|when| now >= when) {
            if unsafe { IsWindow(self.window) } == 0
                || (!self.preview_latched && !input::alt_held())
                || unsafe { GetForegroundWindow() } != self.window
            {
                self.clear();
                input::cancel();
                self.close_preview()?;
            } else {
                self.preview.tick(&self.cfg)?;
                self.marker.follow()?;
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
            self.cast.deadline(),
            self.hide,
            self.reload,
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
pub fn run(
    path: &Path,
    recovery: Option<&str>,
    reopen_settings: bool,
) -> Result<crate::ResidentExit, String> {
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
        // Losing the folder watcher (network share, removable drive) must not end the
        // resident: reopen it periodically and fall back to polling meanwhile.
        let open_watch = || {
            let handle = FindFirstChangeNotificationW(
                wide(path.parent()?).as_ptr(),
                0,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_LAST_WRITE
                    | FILE_NOTIFY_CHANGE_SIZE,
            );
            (handle != INVALID_HANDLE_VALUE).then(|| Watch(handle))
        };
        let mut watch = open_watch();
        let mut rewatch = watch.is_none().then(|| later(5000));
        let loaded = config::load_snapshot(path, None)?;
        let mut cfg = loaded.config;
        if recovery.is_some() {
            // A failed handoff resumes this process at its original integrity level.
            // Keep the requested file untouched so an explicit reload can retry.
            cfg.elevate = crate::security::elevated();
        }
        let snapshot = Some(loaded.bytes);
        let overlay = Overlay::new()?;
        let _hooks = input::Hooks::start(&cfg)?;
        input::set_casting(cfg.mousecast.enabled, cfg.keycast.enabled)?;
        let mut app = App {
            marker: crate::target_marker::TargetMarker::default(),
            paused: false,
            cast: crate::cast::Visuals::new(),
            mousecast: cfg.mousecast.enabled,
            keycast: cfg.keycast.enabled,
            frames: FrameClock::new(cfg.fps)?,
            cfg,
            path: path.into(),
            overlay,
            tray: crate::tray::Tray::new()?,
            preview: Preview::new()?,
            held_window: None,
            preview_latched: false,
            preview_until: None,
            preview_check: None,
            window: null_mut(),
            drag: None,
            command: String::new(),
            hide: None,
            reload: None,
            snapshot,
            exit: None,
        };
        if app.mousecast {
            app.cast.prepare_mouse(desktop::cursor()?, &app.cfg)?;
        }
        let mut msg = MSG::default();
        if let Some(problem) = recovery {
            // A failed lowering removed the task while still elevated: register it again,
            // which needs no consent here. After a refused elevation nothing was changed,
            // and touching an administrator-owned task would only ask for consent again.
            if crate::security::elevated()
                && let Err(task_error) = crate::logon::sync(app.cfg.autostart, app.cfg.elevate)
            {
                app.notice(&format!("{problem}\n{task_error}"))?;
            } else {
                app.notice(problem)?;
            }
        } else if let Err(problem) = crate::logon::sync(app.cfg.autostart, app.cfg.elevate) {
            app.notice(&problem)?;
        } else if !app.cfg.warnings.is_empty() {
            app.config_notice()?;
        }
        if reopen_settings {
            crate::settings::open()?;
        }
        loop {
            for _ in 0..64 {
                let Some(event) = input::next_event() else {
                    break;
                };
                if let Err(problem) = app.handle_event(event) {
                    let _ = app.notice(&problem);
                }
            }
            if let Err(problem) = app.deadlines() {
                let _ = app.notice(&problem);
            }
            if let Some(restart) = app.exit.take() {
                return Ok(restart);
            }
            if rewatch.is_some_and(|when| Instant::now() >= when) {
                watch = open_watch();
                rewatch = watch.is_none().then(|| later(5000));
                // Changes made while nothing was watching are picked up here. Without a
                // watcher this doubles as a slow poll, as long as the file is reachable.
                if watch.is_some() || path.exists() {
                    app.reload = Some(Instant::now());
                }
            }
            let timeout = rewatch.map_or(app.timeout(), |when| {
                app.timeout()
                    .min(when.saturating_duration_since(Instant::now()).as_millis() as u32 + 1)
            });
            // The watcher, when present, is the first handle; later results shift by one.
            let handles = [
                watch.as_ref().map_or(null_mut(), |watch| watch.0),
                app.frames.handle(),
            ];
            let skipped = usize::from(watch.is_none());
            let signaled = MsgWaitForMultipleObjectsEx(
                (2 - skipped) as u32,
                handles[skipped..].as_ptr(),
                timeout,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
            let signaled = if signaled < WAIT_OBJECT_0 + 3 {
                signaled + skipped as u32
            } else {
                signaled
            };
            match signaled {
                WAIT_OBJECT_0 => {
                    if watch
                        .as_ref()
                        .is_some_and(|watch| FindNextChangeNotification(watch.0) == 0)
                    {
                        watch = None;
                        rewatch = Some(later(5000));
                        let _ = app.notice(app.cfg.tr(
                            "Configuration folder is unavailable; retrying",
                            "配置文件夹暂不可用，将自动重试",
                        ));
                    }
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
                // A watcher whose folder disappeared can fail the whole wait.
                _ if watch.is_some() => {
                    watch = None;
                    rewatch = Some(later(5000));
                }
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
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::{EM_REPLACESEL, EM_SETSEL},
            Input::KeyboardAndMouse::*,
        },
    };
    const TARGET: &str = "WinSizeP preview test target";

    fn test_app() -> App {
        let cfg = crate::test_support::cfg();
        App {
            marker: crate::target_marker::TargetMarker::default(),
            paused: false,
            cast: crate::cast::Visuals::new(),
            mousecast: false,
            keycast: false,
            frames: FrameClock::new(cfg.fps).unwrap(),
            cfg,
            path: PathBuf::new(),
            overlay: Overlay::new().unwrap(),
            tray: crate::tray::Tray::new().unwrap(),
            preview: Preview::new().unwrap(),
            held_window: None,
            preview_latched: false,
            preview_until: None,
            preview_check: None,
            window: null_mut(),
            drag: None,
            command: String::new(),
            hide: None,
            reload: None,
            snapshot: None,
            exit: None,
        }
    }

    #[test]
    #[ignore = "opens temporary overlays to verify pause notices and pending-action cancellation"]
    fn pause_hides_marker_and_cancels_actions_until_resumed() {
        let _dpi = crate::test_support::Dpi::new();
        let _hooks = input::Hooks::start(&crate::test_support::cfg()).unwrap();
        let mut app = test_app();
        app.cfg.zh = true;
        let target = crate::overlay::Panel::new().unwrap();
        target.style(0xffffff, 100);
        target.show(crate::layout::Rect {
            x: 100,
            y: 100,
            width: 600,
            height: 400,
        });
        app.marker.show(target.0).unwrap();
        app.command = "pending command".into();
        app.act("pause").unwrap();
        assert!(app.paused);
        assert!(!app.marker.visible());
        assert!(app.command.is_empty());
        assert_eq!(app.overlay.tip_text(), "已暂停");
        app.handle_event(MSG {
            message: input::ACTION,
            wParam: 0x46,
            ..Default::default()
        })
        .unwrap();
        app.handle_event(MSG {
            message: input::CENTER,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(app.overlay.tip_text(), "已暂停");
        app.act("pause").unwrap();
        assert!(!app.paused);
        assert_eq!(app.overlay.tip_text(), "开始捕获");
    }

    #[test]
    #[ignore = "shows a temporary tip and closes its test tray menu automatically"]
    fn tip_expires_while_tray_menu_is_open() {
        unsafe extern "system" fn close_menu(_: HWND, _: u32, _: usize, _: u32) {
            // SAFETY: this callback runs on the thread that opened the test menu.
            unsafe {
                EndMenu();
            }
        }
        let _hooks = input::Hooks::escape_only().unwrap();
        let mut app = test_app();
        app.cfg.duration = 100;
        app.notice("Temporary expiry test").unwrap();
        // SAFETY: the timer stays on this thread and is removed after the menu returns.
        let timer = unsafe { SetTimer(null_mut(), 0, 350, Some(close_menu)) };
        assert_ne!(timer, 0);
        let result = app.handle_event(MSG {
            message: crate::tray::MENU,
            lParam: -1,
            ..Default::default()
        });
        unsafe {
            KillTimer(null_mut(), timer);
        }
        result.unwrap();
        assert!(app.hide.is_none());
        assert!(!app.overlay.tip_visible());
    }

    #[test]
    #[ignore = "shows temporary previews and a tray icon; pauses only the test's input worker"]
    fn preview_deadlines_hide_immediately_with_a_paused_worker() {
        let _hooks = input::Hooks::escape_only().unwrap();
        let mut app = test_app();
        for expiry in [true, false] {
            app.cfg.show = "time".into();
            app.begin_alt_preview(unsafe { GetForegroundWindow() }, false)
                .unwrap();
            app.preview_until = expiry.then(Instant::now);
            app.preview_check = Some(Instant::now());
            let resume = input::tests::pause_worker();
            let started = Instant::now();
            let result = app.deadlines();
            let elapsed = started.elapsed();
            drop(resume);
            result.unwrap();
            assert!(elapsed < Duration::from_millis(250), "{elapsed:?}");
            assert!(!app.overlay.tip_visible() && !app.preview.visible());
            assert!(app.preview_until.is_none() && app.preview_check.is_none());
        }
    }

    #[test]
    #[ignore = "opens temporary outlines, tips and a tray icon; no config is changed"]
    fn borders_notice_reflects_visibility_and_clears_old_deadlines() {
        let _dpi = crate::test_support::Dpi::new();
        let mut cfg = crate::test_support::cfg();
        cfg.preview = false;
        let overlay = Overlay::new().unwrap();
        let _hooks = input::Hooks::start(&cfg).unwrap();
        let mut app = App {
            marker: crate::target_marker::TargetMarker::default(),
            paused: false,
            cast: crate::cast::Visuals::new(),
            mousecast: false,
            keycast: false,
            frames: FrameClock::new(cfg.fps).unwrap(),
            cfg,
            path: PathBuf::new(),
            overlay,
            tray: crate::tray::Tray::new().unwrap(),
            preview: Preview::new().unwrap(),
            held_window: None,
            preview_latched: false,
            preview_until: Some(Instant::now()),
            preview_check: Some(Instant::now()),
            window: null_mut(),
            drag: None,
            command: String::new(),
            hide: None,
            reload: None,
            snapshot: None,
            exit: None,
        };
        app.act("borders").unwrap();
        assert!(!app.preview.visible());
        assert!(app.held_window.is_none());
        assert!(app.preview_until.is_none() && app.preview_check.is_none());
        assert_eq!(
            app.overlay.tip_text(),
            app.cfg.tr(
                "Region outlines disabled in settings",
                "分区框线已在设置中禁用"
            )
        );
        app.cfg.preview = true;
        app.cfg.keys.insert(0x42, "borders".into());
        app.act("borders").unwrap();
        assert!(app.preview.visible());
        assert_eq!(
            app.overlay.tip_text(),
            app.cfg.tr("Outlines shown", "框线已显示")
        );
        app.preview_until = Some(Instant::now());
        app.preview_check = Some(Instant::now());
        app.handle_event(MSG {
            message: input::ACTION,
            wParam: 0x42,
            ..Default::default()
        })
        .unwrap();
        assert!(!app.preview.visible());
        assert!(app.held_window.is_none() && !app.preview_latched);
        assert!(app.preview_until.is_none() && app.preview_check.is_none());
        app.deadlines().unwrap();
        assert!(app.overlay.tip_visible());
        assert_eq!(
            app.overlay.tip_text(),
            app.cfg.tr("Outlines hidden", "框线已隐藏")
        );
    }

    #[test]
    #[ignore = "opens temporary overlays and a tray icon; no startup task or config is changed"]
    fn annotation_actions_toggle_independently_and_dispatch_input() {
        let _dpi = crate::test_support::Dpi::new();
        let cfg = crate::test_support::cfg();
        let overlay = Overlay::new().unwrap();
        let _hooks = input::Hooks::start(&cfg).unwrap();
        let mut app = App {
            marker: crate::target_marker::TargetMarker::default(),
            paused: false,
            cast: crate::cast::Visuals::new(),
            mousecast: false,
            keycast: false,
            frames: FrameClock::new(cfg.fps).unwrap(),
            cfg,
            path: PathBuf::new(),
            overlay,
            tray: crate::tray::Tray::new().unwrap(),
            preview: Preview::new().unwrap(),
            held_window: None,
            preview_latched: false,
            preview_until: None,
            preview_check: None,
            window: null_mut(),
            drag: None,
            command: String::new(),
            hide: None,
            reload: None,
            snapshot: None,
            exit: None,
        };
        app.handle_event(MSG {
            message: input::ACTION,
            wParam: 0x4b,
            ..Default::default()
        })
        .unwrap();
        assert!(app.keycast && !app.mousecast);
        app.handle_event(MSG {
            message: input::CAST_KEY,
            wParam: 0x41,
            lParam: crate::cast::CTRL as isize,
            ..Default::default()
        })
        .unwrap();
        assert!(app.cast.deadline().is_some());
        app.handle_event(MSG {
            message: input::ACTION,
            wParam: 0x4d,
            ..Default::default()
        })
        .unwrap();
        assert!(app.keycast && app.mousecast);
        app.handle_event(MSG {
            message: input::CAST_MOUSE,
            wParam: 1,
            ..Default::default()
        })
        .unwrap();
        app.handle_event(MSG {
            message: input::ACTION,
            wParam: 0x4b,
            ..Default::default()
        })
        .unwrap();
        assert!(!app.keycast && app.mousecast);
        app.handle_event(MSG {
            message: input::ACTION,
            wParam: 0x4d,
            ..Default::default()
        })
        .unwrap();
        assert!(!app.keycast && !app.mousecast);
        app.cast.tick(&app.cfg).unwrap();
        assert!(app.cast.deadline().is_none());
    }

    #[test]
    #[ignore = "opens a temporary settings process and exercises the tray Quit action"]
    fn tray_quit_closes_settings() {
        let class = w!("WinSizeP.Settings");
        assert!(
            unsafe { FindWindowW(class, null()) }.is_null(),
            "Close existing settings before running this integration test"
        );
        let folder = std::env::current_dir()
            .unwrap()
            .join("target/tray-exit-smoke");
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join("config.toml");
        fs::write(&path, config::help()).unwrap();
        let cfg = Config::parse(&config::help().parse().unwrap()).unwrap();
        let overlay = Overlay::new().unwrap();
        let mut app = App {
            marker: crate::target_marker::TargetMarker::default(),
            paused: false,
            cast: crate::cast::Visuals::new(),
            mousecast: false,
            keycast: false,
            frames: FrameClock::new(cfg.fps).unwrap(),
            cfg,
            path: path.clone(),
            overlay,
            tray: crate::tray::Tray::new().unwrap(),
            preview: Preview::new().unwrap(),
            held_window: None,
            preview_latched: false,
            preview_until: None,
            preview_check: None,
            window: null_mut(),
            drag: None,
            command: String::new(),
            hide: None,
            reload: None,
            snapshot: None,
            exit: None,
        };
        // Cover both ordinary editing and a native modal color dialog. No scheduled task is touched.
        for modal in [false, true] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "settings::tests::exit_settings_host",
                    "--nocapture",
                ])
                .env("WINSIZEP_SETTINGS_TEST_CONFIG", &path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let checks = (|| -> Result<(), String> {
                let until = Instant::now() + Duration::from_secs(8);
                let hwnd = loop {
                    let hwnd = unsafe { FindWindowW(class, null()) };
                    if !hwnd.is_null() && unsafe { GetPropW(hwnd, w!("ReadyTab")) } as usize == 1 {
                        break hwnd;
                    }
                    if Instant::now() > until {
                        return Err("Settings did not open".into());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                };
                let page = if modal { 1 } else { 5 };
                if desktop::allowed(hwnd, &[]) {
                    return Err("Settings in another process were not excluded".into());
                }
                for selector in [
                    crate::cli::Selector {
                        hwnd: Some(hwnd as usize),
                        ..Default::default()
                    },
                    crate::cli::Selector {
                        pid: Some(child.id()),
                        ..Default::default()
                    },
                ] {
                    if desktop::target(&selector).is_ok() {
                        return Err("Command-line selection accepted settings".into());
                    }
                }
                if desktop::Drag::start(hwnd, POINT::default(), false, &app.cfg).is_ok()
                    || desktop::apply(
                        hwnd,
                        &Request {
                            preset: Some("center".into()),
                            ..Default::default()
                        },
                        &app.cfg,
                        &path,
                        POINT::default(),
                    )
                    .is_ok()
                {
                    return Err("Settings accepted a window action".into());
                }
                unsafe {
                    PostMessageW(hwnd, WM_COMMAND, 100 + page, 0);
                }
                while unsafe { GetPropW(hwnd, w!("ReadyTab")) } as usize != page + 1 {
                    if Instant::now() > until {
                        return Err("Settings page did not load".into());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                let pane = unsafe { GetDlgItem(hwnd, 2) };
                if modal {
                    let doc = config::read_settings(&path)?
                        .parse::<toml_edit::DocumentMut>()
                        .map_err(|error| error.to_string())?;
                    let index = crate::settings_schema::fields(1, 0, &app.cfg, &doc)
                        .iter()
                        .position(|field| field.key == "foreground")
                        .ok_or("Text color setting missing")?;
                    let color = unsafe { GetDlgItem(pane, 2000 + index as i32 * 16) };
                    unsafe {
                        PostMessageW(
                            hwnd,
                            WM_COMMAND,
                            (2000 + index * 16) as usize,
                            color as isize,
                        );
                    }
                    loop {
                        let popup = unsafe { GetLastActivePopup(hwnd) };
                        if popup != hwnd && unsafe { IsWindowVisible(popup) } != 0 {
                            if desktop::allowed(popup, &[])
                                || desktop::target(&crate::cli::Selector {
                                    hwnd: Some(popup as usize),
                                    ..Default::default()
                                })
                                .is_ok()
                            {
                                return Err("Settings color dialog was not excluded".into());
                            }
                            break;
                        }
                        if Instant::now() > until {
                            return Err("Color dialog did not open".into());
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                } else {
                    let fps = unsafe { GetDlgItem(pane, 2016) };
                    unsafe {
                        SendMessageW(fps, EM_SETSEL, 0, -1);
                        SendMessageW(fps, EM_REPLACESEL, 1, w!("") as isize);
                    }
                    if app.act("quit").is_ok()
                        || app.exit.is_some()
                        || child
                            .try_wait()
                            .map_err(|error| error.to_string())?
                            .is_some()
                    {
                        return Err("Invalid pending input was discarded during Quit".into());
                    }
                    unsafe {
                        SendMessageW(fps, EM_SETSEL, 0, -1);
                        SendMessageW(fps, EM_REPLACESEL, 1, w!("60") as isize);
                    }
                    // Quit immediately, before the usual 450ms save debounce expires.
                }
                app.act("quit")?;
                if !matches!(app.exit.take(), Some(crate::ResidentExit::Quit)) {
                    return Err("Resident did not accept Quit".into());
                }
                let until = Instant::now() + Duration::from_secs(5);
                loop {
                    if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                        if !status.success() {
                            return Err(format!("Settings exited with {status}"));
                        }
                        break;
                    }
                    if Instant::now() > until {
                        return Err("Settings process survived tray Quit".into());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                if unsafe { IsWindow(hwnd) } != 0 {
                    return Err("Settings window survived tray Quit".into());
                }
                let saved = config::read_settings(&path)?
                    .parse::<toml_edit::DocumentMut>()
                    .map_err(|error| error.to_string())?;
                if saved["crosshair"]["fps"].as_integer() != Some(60) {
                    return Err("Pending setting was not saved on exit".into());
                }
                println!("tray quit: settings closed; pending edits saved; modal={modal}");
                Ok(())
            })();
            if child.try_wait().unwrap().is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
            checks.unwrap();
        }
    }

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
        doc["tip"]["duration_ms"] = toml_edit::value(900);
        doc["general"]["alt_hold_ms"] = toml_edit::value(200);
        doc["shortcuts"]["resize_enabled"] = toml_edit::value(false);
        let cfg = Config::parse(&doc).unwrap();
        let overlay = Overlay::new().unwrap();
        let _hooks = input::Hooks::start(&cfg).unwrap();
        let mut app = App {
            marker: crate::target_marker::TargetMarker::default(),
            paused: false,
            cast: crate::cast::Visuals::new(),
            mousecast: false,
            keycast: false,
            frames: FrameClock::new(cfg.fps).unwrap(),
            cfg,
            path: PathBuf::new(),
            overlay,
            tray: crate::tray::Tray::new().unwrap(),
            preview: Preview::new().unwrap(),
            held_window: None,
            preview_latched: false,
            preview_until: None,
            preview_check: None,
            window: null_mut(),
            drag: None,
            command: String::new(),
            hide: None,
            reload: None,
            snapshot: None,
            exit: None,
        };
        let state = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let observed = state.clone();
        let tips = Arc::new(AtomicBool::new(false));
        let observed_tips = tips.clone();
        let focused = Arc::new(AtomicBool::new(false));
        let observed_focus = focused.clone();
        let finished = done.clone();
        let physical_alt = input::tests::physical_alt_sender();
        let controller = std::thread::spawn(move || {
            let key = |vk, down| {
                key(vk, down);
                if matches!(vk, VK_LMENU | VK_RMENU) {
                    physical_alt(vk, down);
                }
            };
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
                for vk in [VK_LMENU, VK_RMENU] {
                    for down in [true, false, true, false] {
                        self::key(vk, down);
                        std::thread::sleep(Duration::from_millis(30));
                    }
                    std::thread::sleep(Duration::from_millis(150));
                    if observed.load(Ordering::SeqCst) || observed_tips.load(Ordering::SeqCst) {
                        return Err("Injected Alt opened a preview or tip".into());
                    }
                }
                println!("injected Alt did not open a preview or tip");
                let started = Instant::now();
                key(VK_LMENU, true);
                visible(&observed, true)?;
                println!(
                    "held Alt first_preview_ms={}",
                    started.elapsed().as_millis()
                );
                assert!(
                    (Duration::from_millis(180)..Duration::from_millis(450))
                        .contains(&started.elapsed()),
                    "Alt preview did not respect the configured hold delay"
                );
                key(VK_LMENU, false);
                visible(&observed, false)?;
                visible(&observed_tips, false)?;
                println!("held Alt preview appeared and released cleanly");
                std::thread::sleep(Duration::from_millis(650));
                let started = Instant::now();
                key(VK_LMENU, true);
                visible(&observed, true)?;
                visible(&observed, false)?;
                visible(&observed_tips, false)?;
                // 200 ms hold + 900 ms tip, plus scheduling and the 20 ms polling above.
                assert!(
                    started.elapsed() < Duration::from_millis(1500),
                    "Held preview outlived the tip: {:?}",
                    started.elapsed()
                );
                key(VK_LMENU, false);
                println!("held Alt preview and tip expired together");
                std::thread::sleep(Duration::from_millis(650));
                key(VK_LMENU, true);
                std::thread::sleep(Duration::from_millis(30));
                key(VK_LMENU, false);
                std::thread::sleep(Duration::from_millis(50));
                key(VK_LMENU, true);
                std::thread::sleep(Duration::from_millis(30));
                let started = Instant::now();
                key(VK_LMENU, false);
                visible(&observed, true)?;
                println!(
                    "double Alt first_preview_ms={}",
                    started.elapsed().as_millis()
                );
                assert!(
                    started.elapsed() < Duration::from_millis(450),
                    "Double Alt preview was delayed"
                );
                visible(&observed, false)?;
                visible(&observed_tips, false)?;
                assert!(
                    started.elapsed() < Duration::from_millis(1200),
                    "Double Alt preview outlived the tip"
                );
                println!("double Alt preview appeared and expired without Escape");
                std::thread::sleep(Duration::from_millis(650));
                key(VK_LMENU, true);
                visible(&observed, true)?;
                key(VK_ESCAPE, true);
                key(VK_ESCAPE, false);
                visible(&observed, false)?;
                key(VK_LMENU, false);
                println!("Escape canceled held preview");
                std::thread::sleep(Duration::from_millis(650));
                key(VK_LMENU, true);
                visible(&observed, true)?;
                key(b'H' as u16, true);
                key(b'H' as u16, false);
                key(VK_LMENU, false);
                visible(&observed, false)?;
                visible(&observed_tips, true)?;
                std::thread::sleep(Duration::from_millis(1100));
                if !observed_tips.load(Ordering::SeqCst) {
                    return Err("Releasing Alt or the default timer dismissed Help".into());
                }
                key(VK_ESCAPE, true);
                key(VK_ESCAPE, false);
                visible(&observed_tips, false)?;
                println!("Help stays visible after Alt release until Escape");
                std::thread::sleep(Duration::from_millis(650));
                key(VK_LMENU, true);
                visible(&observed, true)?;
                let mouse = |flags| unsafe {
                    let event = INPUT {
                        r#type: INPUT_MOUSE,
                        Anonymous: INPUT_0 {
                            mi: MOUSEINPUT {
                                dwFlags: flags,
                                ..Default::default()
                            },
                        },
                    };
                    SendInput(1, &event, size_of::<INPUT>() as i32);
                };
                mouse(MOUSEEVENTF_RIGHTDOWN);
                visible(&observed_tips, false)?;
                let area = desktop::screen(desktop::cursor()?, 0)?.full;
                for cell in [1, 9] {
                    let started = Instant::now();
                    // SetCursorPos deliberately exercises the polling path even when no
                    // low-level mouse notification reaches the preview.
                    unsafe {
                        SetCursorPos(
                            area.x + area.width * if cell == 1 { 1 } else { 5 } / 6,
                            area.y + area.height * if cell == 1 { 1 } else { 5 } / 6,
                        );
                    }
                    visible(&observed_focus, false)?;
                    while !observed_focus.load(Ordering::SeqCst)
                        && started.elapsed() < Duration::from_secs(5)
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let elapsed = started.elapsed();
                    println!("resident grid cell={cell} dwell_ms={}", elapsed.as_millis());
                    if elapsed < Duration::from_millis(1900)
                        || elapsed > Duration::from_millis(2300)
                    {
                        return Err(format!("Resident grid dwell took {elapsed:?}"));
                    }
                }
                key(VK_ESCAPE, true);
                key(VK_ESCAPE, false);
                mouse(MOUSEEVENTF_RIGHTUP);
                key(VK_LMENU, false);
                visible(&observed, false)?;
                Ok::<_, String>(())
            })();
            key(VK_LMENU, false);
            finished.store(true, Ordering::SeqCst);
            result
        });
        let until = Instant::now() + Duration::from_secs(20);
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
            if app.held_window.is_some()
                && app.preview_until.is_some()
                && app.preview.visible() != app.overlay.tip_visible()
            {
                failure = Some("Alt preview and tip visibility differed".into());
                break;
            }
            tips.store(app.overlay.tip_visible(), Ordering::SeqCst);
            focused.store(app.preview.focused(), Ordering::SeqCst);
            state.store(app.preview.visible(), Ordering::SeqCst);
        }
        let result = controller.join().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        assert!(failure.is_none(), "{failure:?}");
        result.unwrap();
    }
}
