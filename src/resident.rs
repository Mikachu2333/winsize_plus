use crate::{
    cli::{Request, Switch},
    config::{self, Config},
    desktop::{self, err},
    framerate::FrameClock,
    input,
    overlay::Overlay,
    preview::Preview,
};
use std::{
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*, Storage::FileSystem::*, System::Threading::*, UI::WindowsAndMessaging::*,
};
#[cfg(test)]
use windows_sys::core::w;
#[cfg(test)]
use {crate::desktop::wide, std::fs};

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
    topmost: crate::topmost_marker::Markers,
    paused: bool,
    cast: crate::cast::Visuals,
    mousecast: bool,
    keycast: bool,
    cfg: Config,
    path: PathBuf,
    // Command controls borrow the tip HWND, so release them before the overlay.
    prompt: Option<crate::command_prompt::Prompt>,
    overlay: Overlay,
    tray: crate::tray::Tray,
    frames: FrameClock,
    drag_frames: FrameClock,
    operations: desktop::operations::Operations,
    preview: Preview,
    neighbors: crate::neighbors::Hint,
    held_window: Option<HWND>,
    preview_latched: bool,
    expired_selection: bool,
    selection_suspended: Option<Duration>,
    selection_hint: String,
    selection_paint: Option<Instant>,
    preview_until: Option<Instant>,
    preview_check: Option<Instant>,
    window: HWND,
    drag: Option<desktop::operations::Drag>,
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
        if let Some(hwnd) = self.held_window {
            self.preview.interactive(hwnd, &self.cfg)?;
            input::track_preview(true)?;
        }
        Ok(())
    }
    fn close_preview(&mut self) -> Result<(), String> {
        self.neighbors.hide();
        self.marker.hide();
        self.held_window = None;
        self.preview_until = None;
        self.preview_check = None;
        self.preview_latched = false;
        self.selection_suspended = None;
        self.selection_hint.clear();
        self.selection_paint = None;
        self.preview.hide();
        input::track_preview(false)
    }
    fn begin_alt_preview(&mut self, window: HWND, _latched: bool) -> Result<(), String> {
        self.operations.cancel();
        self.close_prompt_without_focus();
        self.close_preview()?;
        self.clear();
        self.window = window;
        self.held_window = Some(window);
        self.preview_latched = true;
        self.expired_selection = false;
        self.refresh_preview()?;
        if self.cfg.target_marker {
            self.marker.show(window)?;
        }
        self.preview_until = Some(later(10_000));
        self.preview_check = Some(later(100));
        self.selection_tip()
    }
    fn selection_tip(&mut self) -> Result<(), String> {
        if self.held_window.is_none() {
            return Ok(());
        }
        let count = self.preview.count();
        let candidate = self.preview.hovered_name().map(|name| {
            let name = self.cfg.display_name(name);
            match self
                .preview
                .candidate_position()
                .filter(|_| self.cfg.mouse_enabled)
            {
                Some((position, total)) => format!(
                    "{name} · {position}/{total} · {}",
                    self.cfg.tr("Wheel to switch", "滚轮切换")
                ),
                None => name.to_owned(),
            }
        });
        let text = if count == 0 {
            let mut parts = Vec::new();
            if let Some(candidate) = candidate {
                parts.push(candidate);
            }
            if self.cfg.mouse_enabled {
                for (button, en, zh) in [
                    (self.cfg.add_button, "select", "累计选择"),
                    (self.cfg.apply_button, "apply", "立即应用"),
                ] {
                    if button.is_some() {
                        parts.push(format!(
                            "{} {}",
                            self.cfg.button_name(button),
                            self.cfg.tr(en, zh)
                        ));
                    }
                }
            }
            parts.push(self.cfg.tr("Esc to exit", "Esc 退出").into());
            parts.join(" · ")
        } else {
            let remaining = self.selection_suspended.unwrap_or_else(|| {
                self.preview_until.map_or(Duration::ZERO, |end| {
                    end.saturating_duration_since(Instant::now())
                })
            });
            let seconds = remaining.as_millis().div_ceil(1000).max(1);
            let mut text = if self.cfg.zh {
                format!("已选 {count} 个分区 · Enter 确认 · Esc 取消")
            } else {
                format!("{count} selected · Enter to apply · Esc to cancel")
            };
            if self.selection_suspended.is_some() {
                text.push_str(self.cfg.tr(
                    " · Timer paused; release to continue",
                    " · 计时已暂停，松开后继续",
                ));
            } else {
                text.push_str(&if self.cfg.zh {
                    format!(" · {seconds} 秒后应用")
                } else {
                    format!(" · Apply in {seconds}s")
                });
            }
            if self.cfg.mouse_enabled {
                let mut help = Vec::new();
                if self.cfg.add_button.is_some() {
                    help.push(format!(
                        "{} {}",
                        self.cfg.button_name(self.cfg.add_button),
                        self.cfg.tr("toggles a region", "添加／取消分区")
                    ));
                }
                if self.cfg.apply_button.is_some() {
                    help.push(format!(
                        "{} {}",
                        self.cfg.button_name(self.cfg.apply_button),
                        self.cfg
                            .tr("applies only the hovered region", "仅应用悬停分区")
                    ));
                }
                if !help.is_empty() {
                    text.push('\n');
                    text.push_str(&help.join(" · "));
                }
                if let Some(candidate) = candidate {
                    text.push('\n');
                    text.push_str(&candidate);
                }
            }
            text
        };
        if text != self.selection_hint {
            self.overlay.tip(&text, &self.cfg)?;
            self.selection_hint = text;
        }
        self.hide = None;
        Ok(())
    }
    fn finish_selection(&mut self, rect: Option<crate::layout::Rect>) -> Result<(), String> {
        let window = self.window;
        self.close_preview()?;
        self.clear();
        input::cancel();
        if let Some(rect) = rect {
            if unsafe { GetForegroundWindow() } != window
                || !desktop::allowed(window, &self.cfg.exclusions)
            {
                return Ok(());
            }
            self.apply_geometry(
                window,
                Request {
                    x: Some(rect.x),
                    y: Some(rect.y),
                    size: Some((rect.width, rect.height)),
                    ..Default::default()
                },
                desktop::cursor()?,
            )?;
        }
        Ok(())
    }
    fn notice(&mut self, text: &str) -> Result<(), String> {
        self.close_prompt_without_focus();
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
        let bytes = crate::config_access::read(&self.path).ok();
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
        let preserve_session = !restart && self.cfg.same_interaction(&cfg);
        if !preserve_session {
            self.operations.cancel();
            input::configure(&cfg)?;
        }
        input::set_casting(self.mousecast, self.keycast)?;
        self.frames.reset(cfg.fps);
        self.overlay.guides(self.overlay.mode(), &cfg)?;
        self.topmost.configure(&cfg)?;
        self.cfg = cfg;
        if preserve_session {
            self.preview.restyle(&self.cfg)?;
            self.selection_hint.clear();
            if self.cfg.target_marker {
                if let Some(hwnd) = self.held_window {
                    self.marker.show(hwnd)?;
                }
            } else {
                self.marker.hide();
            }
        } else {
            self.close_preview()?;
        }
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
        } else if preserve_session && self.held_window.is_some() {
            self.selection_tip()
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
                self.operations.cancel();
                self.close_prompt_without_focus();
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
            "version" => self.notice(crate::identity::VERSION_TEXT),
            "help" => {
                self.hide = None;
                self.overlay.tip(&self.cfg.help_text(), &self.cfg)
            }
            "command" => {
                self.close_prompt_without_focus();
                self.command.clear();
                self.hide = None;
                let surface = self.overlay.command_surface(&self.cfg)?;
                self.prompt =
                    match crate::command_prompt::Prompt::attach(surface, &self.cfg, self.window) {
                        Ok(prompt) => {
                            input::native_text(prompt.window);
                            Some(prompt)
                        }
                        Err(error) => {
                            self.overlay.hide_tip();
                            input::native_text(null_mut());
                            return Err(error);
                        }
                    };
                Ok(())
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
                input::arm(self.window)?;
                self.begin_alt_preview(self.window, true)
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
                if req.geometry() {
                    self.apply_geometry(self.window, req, desktop::cursor()?)?;
                } else {
                    desktop::apply(self.window, &req, &self.cfg, &self.path, desktop::cursor()?)?;
                }
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
    fn close_prompt(&mut self) {
        self.close_prompt_without_focus();
        unsafe {
            if IsWindow(self.window) != 0 {
                SetForegroundWindow(self.window);
            }
        }
    }
    fn close_prompt_without_focus(&mut self) {
        if self.prompt.take().is_some() {
            self.overlay.hide_tip();
            input::native_text(null_mut());
        }
    }
    fn submit_prompt(&mut self) -> Result<(), String> {
        let Some(prompt) = &self.prompt else {
            return Ok(());
        };
        let command = match crate::command_prompt::parse(&prompt.text(), &self.cfg) {
            Ok(command) => command,
            Err(error) => {
                prompt.error(&error);
                return Ok(());
            }
        };
        let rect = if let crate::command_prompt::Command::Layout(layout) = &command {
            let resolved = (|| {
                let screen = desktop::screen(desktop::cursor()?, layout.monitor)?;
                layout.resolve(
                    screen.work,
                    desktop::bounds(self.window)?.1,
                    desktop::desktop(),
                )
            })();
            match resolved {
                Ok(rect) => Some(rect),
                Err(error) => {
                    prompt.error(&error);
                    return Ok(());
                }
            }
        } else {
            None
        };
        self.close_prompt();
        match command {
            crate::command_prompt::Command::Action(action) => self.act(&action),
            crate::command_prompt::Command::Layout(_) => self.finish_selection(rect),
        }
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
        let occurred = input::take_event_time();
        if matches!(msg.message, input::COMMAND_SUBMIT | input::COMMAND_CANCEL)
            && msg.wParam != 0
            && self.prompt.as_ref().is_none_or(|prompt| {
                prompt.window as usize != msg.wParam || prompt.token != (msg.lParam as usize >> 1)
            })
        {
            return Ok(());
        }
        let selection_action = matches!(
            msg.message,
            input::CLICK
                | input::POINTER_DOWN
                | input::POINTER_UP
                | input::CONFIRM
                | input::ACTION
                | input::DRAG_START
                | input::CENTER
                | input::REGION_WHEEL
        ) && !(msg.message == input::ACTION
            && self
                .cfg
                .keys
                .get(&(msg.wParam as u32))
                .is_some_and(|a| matches!(a.as_str(), "mousecast" | "keycast")));
        if selection_action
            && self.held_window.is_some()
            && self.preview_until.is_some_and(|end| occurred >= end)
        {
            self.finish_selection(self.preview.selection_rect())?;
            self.expired_selection = true;
        }
        if selection_action && self.expired_selection {
            return Ok(());
        }
        if self.paused
            && (input::ARMED..=input::REGION_WHEEL).contains(&msg.message)
            && msg.message != input::DISPLAY_CHANGED
        {
            return Ok(());
        }
        match msg.message {
            input::COMMAND_SUBMIT => self.submit_prompt(),
            input::COMMAND_CANCEL => {
                if msg.lParam & 1 != 0 {
                    self.close_prompt_without_focus();
                } else {
                    self.close_prompt();
                }
                Ok(())
            }
            input::RECOVERED => {
                self.operations.cancel();
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
                Ok(())
            }
            input::REGION_WHEEL => {
                if self.held_window.is_some()
                    && self.cfg.mouse_enabled
                    && self.drag.is_none()
                    && self.selection_suspended.is_none()
                    && self.preview.cycle_deferred(
                        input::wheel_point(),
                        msg.wParam as i32,
                        &self.cfg,
                    )?
                {
                    self.preview_until = Some(
                        occurred
                            + Duration::from_millis(if self.preview.count() == 0 {
                                10_000
                            } else {
                                3000
                            }),
                    );
                    self.selection_paint.get_or_insert_with(|| later(16));
                }
                Ok(())
            }
            input::DISPLAY_CHANGED => {
                if self.held_window.is_some() {
                    self.finish_selection(None)?;
                }
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
                let topmost = &mut self.topmost;
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
                    let _ = topmost.poll();
                })? {
                    self.act(action)?;
                }
                Ok(())
            }
            input::ARMED => self.begin_alt_preview(msg.wParam as HWND, true),
            input::ALT_HOLD => self.begin_alt_preview(msg.wParam as HWND, false),
            input::ALT_RELEASE => Ok(()),
            input::CONFIRM => {
                if self.preview.count() > 0 {
                    self.finish_selection(self.preview.selection_rect())?;
                }
                Ok(())
            }
            input::ACTION => {
                self.marker.hide();
                let action = self.cfg.keys.get(&(msg.wParam as u32)).cloned();
                self.close_preview()?;
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
                if self.preview.active() {
                    self.preview.follow(point, &self.cfg)?;
                    self.selection_tip()?;
                }
                if self.cfg.mouse_enabled
                    && self.held_window.is_some()
                    && self.selection_suspended.is_none()
                {
                    self.neighbors.follow(point, &self.cfg)?;
                }
                if self.drag.is_some()
                    && let Some(frame) = self.drag_frames.moved(point)?
                {
                    self.follow_drag(frame)?;
                }
                if self.overlay.mode() != 0
                    && let Some(frame) = self.frames.moved(point)?
                {
                    self.follow_frame(frame)?;
                }
                Ok(())
            }
            input::CLICK => {
                if self.held_window.is_none() {
                    return Ok(());
                }
                let point = input::click_point();
                if !self.preview.active() {
                    self.preview.interactive(self.window, &self.cfg)?;
                }
                if let Some((name, rect)) = self.preview.hit(point, &self.cfg)? {
                    if msg.lParam != 0 {
                        self.preview.toggle(name, rect, &self.cfg)?;
                        self.selection_suspended = None;
                        self.preview_until = Some(
                            occurred
                                + Duration::from_millis(if self.preview.count() == 0 {
                                    10_000
                                } else {
                                    3000
                                }),
                        );
                        self.selection_tip()?;
                    } else {
                        self.finish_selection(Some(rect))?;
                    }
                } else if msg.lParam == 0 {
                    self.finish_selection(None)?;
                }
                Ok(())
            }
            input::CENTER => {
                self.close_preview()?;
                self.clear();
                self.apply_geometry(
                    msg.wParam as HWND,
                    Request {
                        center: true,
                        ..Default::default()
                    },
                    input::click_point(),
                )
            }
            input::POINTER_DOWN => {
                self.selection_suspended = self
                    .preview_until
                    .take()
                    .map(|end| end.saturating_duration_since(occurred));
                self.selection_tip()
            }
            input::POINTER_UP => {
                if let Some(remaining) = self.selection_suspended.take() {
                    self.preview_until = Some(occurred + remaining);
                }
                self.selection_tip()
            }
            input::DRAG_START => {
                let drag = self.operations.start_drag(
                    msg.wParam as HWND,
                    input::drag_origin(),
                    msg.lParam != 0,
                    self.cfg.clone(),
                );
                self.drag = Some(drag);
                self.close_preview()?;
                self.clear();
                self.drag_frames.reset(120.0);
                self.follow_drag(desktop::cursor()?)
            }
            input::DRAG_END => {
                let drag = self.drag.take();
                self.drag_frames.reset(120.0);
                let point = input::moved();
                // End the gesture even if its final move fails (closed target,
                // denied operation or drawing error). A failure must not re-arm it.
                let moved = drag.as_ref().map_or(Ok(()), |drag| drag.follow(point));
                let drawn = self.overlay.follow(point, &self.cfg);
                moved.and(drawn)
            }
            input::CANCEL => {
                self.operations.cancel();
                input::cancel();
                self.drag_frames.reset(120.0);
                if let Some(drag) = self.drag.take() {
                    drag.cancel()?;
                }
                self.clear();
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
        self.overlay.follow(point, &self.cfg)
    }
    fn apply_geometry(
        &mut self,
        window: HWND,
        request: Request,
        point: POINT,
    ) -> Result<(), String> {
        self.operations
            .apply(window, request, self.cfg.clone(), self.path.clone(), point);
        input::operation_pending(true);
        Ok(())
    }
    fn follow_drag(&mut self, point: POINT) -> Result<(), String> {
        if let Some(drag) = &self.drag {
            drag.follow(point)?;
        }
        Ok(())
    }
    fn deadlines(&mut self) -> Result<(), String> {
        input::operation_pending(self.operations.pending());
        self.operations.poll()?;
        self.topmost.poll()?;
        let now = Instant::now();
        if self.selection_paint.is_some_and(|when| now >= when) {
            self.flush_selection_paint()?;
        }
        if self.cast.deadline().is_some_and(|when| now >= when) {
            self.cast.tick(&self.cfg)?;
        }
        if self.preview_until.is_some_and(|when| now >= when) && !input::has_pending_events() {
            self.finish_selection(self.preview.selection_rect())?;
            self.expired_selection = true;
        }
        if self.preview_check.is_some_and(|when| now >= when) {
            if unsafe { IsWindow(self.window) } == 0
                || unsafe { GetForegroundWindow() } != self.window
            {
                self.clear();
                input::cancel();
                self.close_preview()?;
            } else {
                self.preview.tick(&self.cfg)?;
                if self.cfg.mouse_enabled
                    && self.held_window.is_some()
                    && self.selection_suspended.is_none()
                {
                    self.neighbors.follow(desktop::cursor()?, &self.cfg)?;
                }
                self.selection_tip()?;
                self.marker.follow()?;
                self.preview_check = Some(later(100));
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
            self.cast.deadline(),
            self.hide,
            self.reload,
            self.preview_until,
            self.preview_check,
            self.selection_paint,
        ]
        .into_iter()
        .flatten()
        .min()
        .map_or(INFINITE, |when| {
            when.saturating_duration_since(Instant::now()).as_millis() as u32 + 1
        })
        .min(if self.operations.pending() {
            16
        } else {
            INFINITE
        })
    }
    fn flush_selection_paint(&mut self) -> Result<(), String> {
        if self.selection_paint.take().is_some() {
            self.preview.flush_paint(&self.cfg)?;
            self.selection_tip()?;
        }
        Ok(())
    }
}

pub fn running() -> bool {
    unsafe {
        let handle = OpenMutexW(
            SYNCHRONIZATION_SYNCHRONIZE,
            0,
            crate::identity::mutex(false).as_ptr(),
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
    startup: Option<&crate::handoff::Child>,
    fallback: &mut Option<Config>,
) -> Result<crate::ResidentExit, String> {
    unsafe {
        let handle = CreateMutexW(null(), 0, crate::identity::mutex(false).as_ptr());
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
            crate::config_access::watch(path.parent()?)
                .ok()
                .map(|handle| Watch(handle as HANDLE))
        };
        let mut watch = open_watch();
        let mut rewatch = watch.is_none().then(|| later(5000));
        let (mut cfg, snapshot, write_error) = match config::load_snapshot(path, None) {
            Ok(loaded) => (loaded.config, Some(loaded.bytes), loaded.write_error),
            Err(error) if recovery.is_some() => (
                fallback.take().ok_or_else(|| error.clone())?,
                None,
                Some(error),
            ),
            Err(error) => return Err(error),
        };
        if recovery.is_some() {
            // A failed handoff resumes this process at its original integrity level.
            // Keep the requested file untouched so an explicit reload can retry.
            cfg.elevate = crate::security::elevated();
        }
        let overlay = Overlay::new()?;
        let _hooks = input::Hooks::start(&cfg)?;
        input::set_casting(cfg.mousecast.enabled, cfg.keycast.enabled)?;
        let mut app = App {
            marker: crate::target_marker::TargetMarker::default(),
            topmost: crate::topmost_marker::Markers::new(&cfg)?,
            paused: false,
            cast: crate::cast::Visuals::new(),
            mousecast: cfg.mousecast.enabled,
            keycast: cfg.keycast.enabled,
            frames: FrameClock::new(cfg.fps)?,
            drag_frames: FrameClock::new(120.0)?,
            operations: desktop::operations::Operations::new()?,
            cfg,
            path: path.into(),
            overlay,
            tray: crate::tray::Tray::new()?,
            preview: Preview::new()?,
            neighbors: crate::neighbors::Hint::default(),
            held_window: None,
            preview_latched: false,
            expired_selection: false,
            selection_suspended: None,
            selection_hint: String::new(),
            selection_paint: None,
            preview_until: None,
            preview_check: None,
            window: null_mut(),
            drag: None,
            command: String::new(),
            prompt: None,
            hide: None,
            reload: None,
            snapshot,
            exit: None,
        };
        if app.mousecast {
            app.cast.prepare_mouse(desktop::cursor()?, &app.cfg)?;
        }
        // Configuration, hooks, renderers and the tray are now usable. Startup task
        // synchronization may prompt the user and is not part of resident readiness.
        if let Some(startup) = startup {
            startup.confirm()?;
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
        } else if let Some(problem) = write_error {
            app.notice(&problem)?;
        } else if !app.cfg.warnings.is_empty() {
            app.config_notice()?;
        }
        if reopen_settings && let Err(problem) = crate::settings::open() {
            let _ = app.notice(&problem);
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
                *fallback = Some(app.cfg);
                return Ok(restart);
            }
            if rewatch.is_some_and(|when| Instant::now() >= when) {
                watch = open_watch();
                rewatch = watch.is_none().then(|| later(5000));
                // Changes made while nothing was watching are picked up here. Without a
                // watcher this doubles as a slow poll, as long as the file is reachable.
                app.reload = Some(Instant::now());
            }
            let timeout = rewatch.map_or(app.timeout(), |when| {
                app.timeout()
                    .min(when.saturating_duration_since(Instant::now()).as_millis() as u32 + 1)
            });
            // The watcher, when present, is the first handle; later results shift by one.
            let handles = [
                watch.as_ref().map_or(null_mut(), |watch| watch.0),
                app.frames.handle(),
                app.drag_frames.handle(),
            ];
            let skipped = usize::from(watch.is_none());
            let signaled = MsgWaitForMultipleObjectsEx(
                (3 - skipped) as u32,
                handles[skipped..].as_ptr(),
                timeout,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
            let signaled = if signaled < WAIT_OBJECT_0 + 4 {
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
                    if let Some(point) = app.drag_frames.tick()
                        && let Err(problem) = app.follow_drag(point)
                    {
                        let _ = app.notice(&problem);
                    }
                }
                value if value == WAIT_OBJECT_0 + 3 => {
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
    use std::process::{Command, Stdio};
    use windows_sys::Win32::{
        System::LibraryLoader::GetModuleHandleW,
        UI::Controls::{EM_REPLACESEL, EM_SETSEL},
    };
    const TARGET: &str = "WinSizeP preview test target";

    fn test_app() -> App {
        let cfg = crate::test_support::cfg();
        App {
            marker: crate::target_marker::TargetMarker::default(),
            topmost: crate::topmost_marker::Markers::default(),
            paused: false,
            cast: crate::cast::Visuals::new(),
            mousecast: false,
            keycast: false,
            frames: FrameClock::new(cfg.fps).unwrap(),
            drag_frames: FrameClock::new(120.0).unwrap(),
            operations: desktop::operations::Operations::new().unwrap(),
            cfg,
            path: PathBuf::new(),
            overlay: Overlay::new().unwrap(),
            tray: crate::tray::Tray::new().unwrap(),
            preview: Preview::new().unwrap(),
            neighbors: crate::neighbors::Hint::default(),
            held_window: None,
            preview_latched: false,
            expired_selection: false,
            selection_suspended: None,
            selection_hint: String::new(),
            selection_paint: None,
            preview_until: None,
            preview_check: None,
            window: null_mut(),
            drag: None,
            command: String::new(),
            prompt: None,
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
    #[ignore = "checks selection hints while pausing and resuming the apply timer"]
    fn selection_hints_explain_paused_timer_and_click_actions() {
        let _hooks = input::Hooks::start(&crate::test_support::cfg()).unwrap();
        let mut app = test_app();
        app.cfg.zh = true;
        app.begin_alt_preview(unsafe { GetForegroundWindow() }, false)
            .unwrap();
        app.preview
            .toggle(
                "1/4".into(),
                crate::layout::Rect {
                    x: 0,
                    y: 0,
                    width: 300,
                    height: 200,
                },
                &app.cfg,
            )
            .unwrap();
        app.preview_until = Some(later(3000));
        app.handle_event(MSG {
            message: input::POINTER_DOWN,
            ..Default::default()
        })
        .unwrap();
        assert!(app.overlay.tip_text().contains("计时已暂停"));
        assert!(!app.overlay.tip_text().contains("秒后应用"));
        assert!(app.overlay.tip_text().contains("添加／取消"));
        assert!(app.overlay.tip_text().contains("仅应用悬停"));
        app.handle_event(MSG {
            message: input::POINTER_UP,
            ..Default::default()
        })
        .unwrap();
        assert!(app.overlay.tip_text().contains("秒后应用"));
    }

    #[test]
    #[ignore = "opens a temporary native prompt to verify blur cancels without focus restoration"]
    fn command_blur_cancels_and_stale_close_does_not_affect_new_prompt() {
        let _hooks = input::Hooks::start(&crate::test_support::cfg()).unwrap();
        let mut app = test_app();
        app.window = unsafe { GetForegroundWindow() };
        app.act("command").unwrap();
        let old = app.prompt.as_ref().unwrap().window;
        let old_token = app.prompt.as_ref().unwrap().token;
        unsafe {
            SendMessageW(old, WM_ACTIVATE, WA_INACTIVE as usize, 0);
        }
        while let Some(event) = input::next_event() {
            app.handle_event(event).unwrap();
        }
        assert!(app.prompt.is_none());
        app.act("command").unwrap();
        let current = app.prompt.as_ref().unwrap().window;
        assert_eq!(old, current, "command input must reuse the tip HWND");
        for message in [input::COMMAND_CANCEL, input::COMMAND_SUBMIT] {
            app.handle_event(MSG {
                message,
                wParam: old as usize,
                lParam: (old_token << 1) as isize,
                ..Default::default()
            })
            .unwrap();
            assert!(
                app.prompt.is_some(),
                "stale events must not affect a new command on the same tip"
            );
        }
        // A handle from another window must never close this prompt.
        app.handle_event(MSG {
            message: input::COMMAND_CANCEL,
            wParam: usize::MAX,
            lParam: 1,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(app.prompt.as_ref().unwrap().window, current);
        app.close_prompt_without_focus();
    }

    #[test]
    #[ignore = "opens a temporary command prompt and verifies pause cancels it and blocks queued submissions"]
    fn pausing_closes_command_prompt_and_rejects_pending_submission() {
        let _hooks = input::Hooks::start(&crate::test_support::cfg()).unwrap();
        let mut app = test_app();
        app.window = unsafe { GetForegroundWindow() };
        app.act("command").unwrap();
        let hwnd = app.prompt.as_ref().unwrap().window;
        unsafe {
            SetWindowTextW(GetDlgItem(hwnd, 1), wide("version").as_ptr());
        }
        app.act("pause").unwrap();
        assert!(app.prompt.is_none());
        assert_ne!(unsafe { IsWindow(hwnd) }, 0);
        assert_eq!(unsafe { GetPropW(hwnd, w!("CommandToken")) } as usize, 0);
        assert!(unsafe { GetDlgItem(hwnd, 1) }.is_null());
        let notice = app.overlay.tip_text().to_owned();
        app.handle_event(MSG {
            message: input::COMMAND_SUBMIT,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(app.overlay.tip_text(), notice);
        app.act("pause").unwrap();
    }

    #[test]
    #[ignore = "replays timestamped wheel events against selection deadlines using temporary overlays"]
    fn wheel_refreshes_deadlines_without_selecting_and_cannot_revive_expired_session() {
        let _dpi = crate::test_support::Dpi::new();
        let _hooks = input::Hooks::start(&crate::test_support::cfg()).unwrap();
        let mut app = test_app();
        app.begin_alt_preview(unsafe { GetForegroundWindow() }, false)
            .unwrap();
        let area = desktop::screen(desktop::cursor().unwrap(), 0).unwrap().work;
        let point = POINT {
            x: area.x + area.width / 2 + 3,
            y: area.y + area.height / 2 + 3,
        };
        let now = Instant::now();
        let first = now - Duration::from_millis(20);
        app.preview_until = Some(now - Duration::from_millis(10));
        input::tests::enqueue_pointer_at(input::REGION_WHEEL, (-30i32) as usize, point, first);
        let event = input::next_event().unwrap();
        app.handle_event(event).unwrap();
        assert_eq!(app.preview_until, Some(first + Duration::from_secs(10)));
        assert_eq!(app.preview.hovered_name(), Some("5/9"));
        assert_eq!(app.preview.count(), 0);
        app.flush_selection_paint().unwrap();
        assert!(app.overlay.tip_text().contains("1/3"));
        let (name, rect) = app.preview.hit(point, &app.cfg).unwrap().unwrap();
        app.preview.toggle(name, rect, &app.cfg).unwrap();
        input::tests::enqueue_pointer_at(input::REGION_WHEEL, (-90i32) as usize, point, now);
        let event = input::next_event().unwrap();
        app.handle_event(event).unwrap();
        assert_eq!(app.preview_until, Some(now + Duration::from_secs(3)));
        assert_eq!(app.preview.hovered_name(), Some("4/4"));
        assert_eq!(app.preview.selection_rect(), Some(rect));
        let deadline = app.preview_until;
        input::tests::enqueue_at(input::TIP_SCROLL, now);
        let event = input::next_event().unwrap();
        app.handle_event(event).unwrap();
        assert_eq!(app.preview_until, deadline);
        app.selection_suspended = Some(Duration::from_millis(500));
        app.preview_until = None;
        input::tests::enqueue_pointer_at(input::REGION_WHEEL, (-120i32) as usize, point, now);
        let event = input::next_event().unwrap();
        app.handle_event(event).unwrap();
        assert_eq!(app.preview.hovered_name(), Some("4/4"));
        assert_eq!(app.selection_suspended, Some(Duration::from_millis(500)));
        assert!(app.preview_until.is_none());
        // Use an empty new session so expiry cannot move an unrelated desktop window.
        app.begin_alt_preview(unsafe { GetForegroundWindow() }, false)
            .unwrap();
        app.preview_until = Some(now);
        input::tests::enqueue_pointer_at(
            input::REGION_WHEEL,
            (-120i32) as usize,
            point,
            now + Duration::from_millis(1),
        );
        let event = input::next_event().unwrap();
        app.handle_event(event).unwrap();
        assert!(app.expired_selection && !app.preview.active());
        assert!(app.preview_until.is_none());
    }

    #[test]
    #[ignore = "opens temporary previews and replays a backlog spanning the selection deadline"]
    fn queued_pointer_down_uses_occurrence_time_and_late_input_cannot_reopen_session() {
        let _hooks = input::Hooks::start(&crate::test_support::cfg()).unwrap();
        let mut app = test_app();
        app.begin_alt_preview(unsafe { GetForegroundWindow() }, false)
            .unwrap();
        let now = Instant::now();
        let deadline = now - Duration::from_millis(20);
        app.preview_until = Some(deadline);
        for _ in 0..70 {
            input::tests::enqueue_at(input::TIP_SCROLL, deadline - Duration::from_millis(10));
        }
        input::tests::enqueue_at(input::POINTER_DOWN, deadline - Duration::from_millis(5));
        for _ in 0..64 {
            app.handle_event(input::next_event().unwrap()).unwrap();
        }
        app.deadlines().unwrap();
        assert!(
            app.held_window.is_some(),
            "backlogged pointer down was overtaken by expiry"
        );
        while let Some(event) = input::next_event() {
            app.handle_event(event).unwrap();
        }
        assert_eq!(app.selection_suspended, Some(Duration::from_millis(5)));
        assert!(app.preview_until.is_none());
        input::tests::enqueue_at(input::POINTER_UP, now);
        app.handle_event(input::next_event().unwrap()).unwrap();
        assert_eq!(app.preview_until, Some(now + Duration::from_millis(5)));
        input::tests::enqueue_at(input::POINTER_DOWN, now + Duration::from_millis(6));
        app.handle_event(input::next_event().unwrap()).unwrap();
        assert!(app.held_window.is_none() && app.expired_selection);
        input::tests::enqueue_at(input::CONFIRM, now + Duration::from_millis(7));
        app.handle_event(input::next_event().unwrap()).unwrap();
        assert!(app.held_window.is_none());
    }

    #[test]
    #[ignore = "reloads an isolated configuration while a temporary selection is active"]
    fn appearance_reload_preserves_selection_and_deadline_but_bindings_cancel() {
        let _hooks = input::Hooks::start(&crate::test_support::cfg()).unwrap();
        let mut app = test_app();
        let path = std::env::temp_dir().join(format!(
            "winsizep-session-reload-{}.toml",
            std::process::id()
        ));
        app.path = path.clone();
        let mut doc = config::help().parse::<toml_edit::DocumentMut>().unwrap();
        std::fs::write(&path, doc.to_string()).unwrap();
        app.begin_alt_preview(unsafe { GetForegroundWindow() }, false)
            .unwrap();
        let rect = crate::layout::Rect {
            x: 0,
            y: 0,
            width: 300,
            height: 200,
        };
        app.preview.toggle("1/4".into(), rect, &app.cfg).unwrap();
        app.preview_until = Some(later(3000));
        let deadline = app.preview_until;
        doc["ui"]["font_size"] = toml_edit::value(20);
        doc["outline"]["opacity"] = toml_edit::value(180);
        std::fs::write(&path, doc.to_string()).unwrap();
        app.load(true).unwrap();
        assert_eq!(app.preview.count(), 1);
        assert_eq!(app.preview.selection_rect(), Some(rect));
        assert_eq!(app.preview_until, deadline);
        assert_eq!(app.cfg.font_size, 20);
        doc["keys"]["Q"] = toml_edit::value("2/4");
        std::fs::write(&path, doc.to_string()).unwrap();
        app.load(true).unwrap();
        assert!(app.held_window.is_none());
        assert!(!app.preview.visible());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    #[ignore = "opens temporary selection tips to verify scrolling preserves active and paused deadlines"]
    fn scrolling_selection_tips_never_changes_selection_deadlines() {
        let _hooks = input::Hooks::start(&crate::test_support::cfg()).unwrap();
        let mut app = test_app();
        app.begin_alt_preview(unsafe { GetForegroundWindow() }, false)
            .unwrap();
        let deadline = app.preview_until;
        app.handle_event(MSG {
            message: input::TIP_SCROLL,
            wParam: 400,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(app.preview_until, deadline);
        app.handle_event(MSG {
            message: input::POINTER_DOWN,
            ..Default::default()
        })
        .unwrap();
        let remaining = app.selection_suspended;
        app.handle_event(MSG {
            message: input::TIP_SCROLL,
            wParam: 400,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(app.selection_suspended, remaining);
        assert!(app.preview_until.is_none());
    }

    #[test]
    #[ignore = "opens temporary selection overlays to exercise confirmation and pointer timers"]
    fn selection_session_survives_alt_release_and_pauses_only_during_pointer_hold() {
        let _hooks = input::Hooks::start(&crate::test_support::cfg()).unwrap();
        let mut app = test_app();
        app.cfg.zh = true;
        app.begin_alt_preview(unsafe { GetForegroundWindow() }, false)
            .unwrap();
        assert!(
            app.preview_until
                .unwrap()
                .saturating_duration_since(Instant::now())
                > Duration::from_secs(9)
        );
        assert!(app.overlay.tip_text().contains("Esc"));
        assert!(!app.overlay.tip_text().contains("Q /"));
        app.handle_event(MSG {
            message: input::ALT_RELEASE,
            ..Default::default()
        })
        .unwrap();
        assert!(app.preview.visible());
        app.handle_event(MSG {
            message: input::POINTER_DOWN,
            ..Default::default()
        })
        .unwrap();
        assert!(app.preview_until.is_none() && app.selection_suspended.is_some());
        app.handle_event(MSG {
            message: input::POINTER_UP,
            ..Default::default()
        })
        .unwrap();
        assert!(app.preview_until.is_some() && app.selection_suspended.is_none());
        app.preview
            .toggle(
                "one".into(),
                crate::layout::Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
                &app.cfg,
            )
            .unwrap();
        app.preview_until = Some(later(3000));
        app.selection_tip().unwrap();
        assert!(app.overlay.tip_text().contains("Enter"));
        app.handle_event(MSG {
            message: input::CANCEL,
            ..Default::default()
        })
        .unwrap();
        assert!(!app.preview.visible() && !app.overlay.tip_visible());
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
            app.begin_alt_preview(unsafe { GetForegroundWindow() }, false)
                .unwrap();
            app.preview_until = expiry.then(Instant::now);
            if !expiry {
                app.window = null_mut();
            }
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
    #[ignore = "opens a temporary preview to check the legacy borders action uses the normal session"]
    fn borders_action_uses_the_normal_selection_session() {
        let _hooks = input::Hooks::start(&crate::test_support::cfg()).unwrap();
        let mut app = test_app();
        app.window = unsafe { GetForegroundWindow() };
        if !desktop::allowed(app.window, &app.cfg.exclusions) {
            return;
        }
        for mode in ["always", "off"] {
            app.cfg.preview_mode = mode.into();
            app.act("borders").unwrap();
            assert!(app.held_window.is_some());
            assert!(app.preview_until.is_some());
            assert!(app.preview_check.is_some());
            assert_eq!(app.preview.visible(), mode == "always");
            app.handle_event(MSG {
                message: input::CANCEL,
                ..Default::default()
            })
            .unwrap();
        }
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
            topmost: crate::topmost_marker::Markers::default(),
            paused: false,
            cast: crate::cast::Visuals::new(),
            mousecast: false,
            keycast: false,
            frames: FrameClock::new(cfg.fps).unwrap(),
            drag_frames: FrameClock::new(120.0).unwrap(),
            operations: desktop::operations::Operations::new().unwrap(),
            cfg,
            path: PathBuf::new(),
            overlay,
            tray: crate::tray::Tray::new().unwrap(),
            preview: Preview::new().unwrap(),
            neighbors: crate::neighbors::Hint::default(),
            held_window: None,
            preview_latched: false,
            expired_selection: false,
            selection_suspended: None,
            selection_hint: String::new(),
            selection_paint: None,
            preview_until: None,
            preview_check: None,
            window: null_mut(),
            drag: None,
            command: String::new(),
            prompt: None,
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
        let class = crate::identity::SETTINGS_CLASS;
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
            topmost: crate::topmost_marker::Markers::default(),
            paused: false,
            cast: crate::cast::Visuals::new(),
            mousecast: false,
            keycast: false,
            frames: FrameClock::new(cfg.fps).unwrap(),
            drag_frames: FrameClock::new(120.0).unwrap(),
            operations: desktop::operations::Operations::new().unwrap(),
            cfg,
            path: path.clone(),
            overlay,
            tray: crate::tray::Tray::new().unwrap(),
            preview: Preview::new().unwrap(),
            neighbors: crate::neighbors::Hint::default(),
            held_window: None,
            preview_latched: false,
            expired_selection: false,
            selection_suspended: None,
            selection_hint: String::new(),
            selection_paint: None,
            preview_until: None,
            preview_check: None,
            window: null_mut(),
            drag: None,
            command: String::new(),
            prompt: None,
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
                            preset: Some("5/9".into()),
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
                    let index = crate::settings::schema::fields(1, 0, &app.cfg, &doc)
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
    #[test]
    #[ignore = "uses a temporary foreign window and hook callbacks to verify unified selection and application"]
    fn alt_preview_lifecycle() {
        let _dpi = crate::test_support::Dpi::new();
        let original_cursor = desktop::cursor().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "resident::tests::target_window"])
            .env("WINSIZEP_PREVIEW_TARGET", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let result = std::panic::catch_unwind(|| {
            let until = Instant::now() + Duration::from_secs(5);
            let target = loop {
                let hwnd = unsafe { FindWindowW(null(), wide(TARGET).as_ptr()) };
                if !hwnd.is_null() {
                    break hwnd;
                }
                assert!(Instant::now() < until);
                std::thread::sleep(Duration::from_millis(10));
            };
            unsafe {
                SetForegroundWindow(target);
            }
            let mut app = test_app();
            let _hooks = input::Hooks::start(&app.cfg).unwrap();
            let drain = |app: &mut App| {
                while let Some(event) = input::next_event() {
                    app.handle_event(event).unwrap();
                }
                let until = Instant::now() + Duration::from_secs(3);
                while app.operations.pending() && app.drag.is_none() {
                    assert!(Instant::now() < until, "background geometry did not finish");
                    app.operations.poll().unwrap();
                    crate::test_support::pump();
                    std::thread::sleep(Duration::from_millis(5));
                }
                app.operations.poll().unwrap();
            };
            for double in [false, true] {
                unsafe {
                    SetForegroundWindow(target);
                }
                input::tests::activate_session(double);
                drain(&mut app);
                assert!(app.preview.visible() && app.held_window == Some(target));
                assert!(
                    app.preview_until
                        .unwrap()
                        .saturating_duration_since(Instant::now())
                        > Duration::from_secs(9)
                );
                // Both activation methods accept top-row keyboard bindings after Alt release.
                input::tests::session_key(0x31);
                drain(&mut app);
                assert!(!app.preview.visible());
                let point = desktop::cursor().unwrap();
                let work = desktop::screen(point, 0).unwrap().work;
                let actual = desktop::bounds(target).unwrap().1;
                let expected = app.cfg.presets["1/4"]
                    .resolve(work, actual, desktop::desktop())
                    .unwrap();
                assert_eq!(actual, expected);
                input::tests::activate_session(double);
                drain(&mut app);
                let a = POINT {
                    x: work.x + work.width / 4,
                    y: work.y + work.height / 4,
                };
                let b = POINT {
                    x: work.x + work.width * 3 / 4,
                    y: work.y + work.height / 4,
                };
                input::tests::session_click(a, false);
                drain(&mut app);
                input::tests::session_click(b, false);
                drain(&mut app);
                assert_eq!(app.preview.count(), 2);
                input::tests::session_key(13);
                drain(&mut app);
                assert_eq!(
                    desktop::bounds(target).unwrap().1,
                    crate::layout::Rect {
                        x: work.x,
                        y: work.y,
                        width: work.width,
                        height: (work.height as f64 / 2.0).round() as i32
                    }
                );
                input::tests::activate_session(double);
                drain(&mut app);
                input::tests::session_click(a, false);
                drain(&mut app);
                let before = desktop::bounds(target).unwrap().1;
                input::tests::session_key(27);
                drain(&mut app);
                assert!(!app.preview.visible());
                assert_eq!(desktop::bounds(target).unwrap().1, before);

                // Browsing keeps the cumulative set; the right click still applies
                // only the current candidate, for both hold and double-tap activation.
                input::tests::activate_session(double);
                drain(&mut app);
                input::tests::session_click(a, false);
                drain(&mut app);
                let chosen = app.preview.selection_rect();
                let center = POINT {
                    x: work.x + work.width / 2 + 3,
                    y: work.y + work.height / 2 + 3,
                };
                unsafe {
                    SetCursorPos(center.x, center.y);
                }
                std::thread::sleep(Duration::from_millis(40));
                drain(&mut app);
                input::tests::session_wheel(center, -120);
                drain(&mut app);
                assert_eq!(
                    app.preview.hovered_name(),
                    Some("4/4"),
                    "double={double}, candidate={:?}, cursor={:?}",
                    app.preview.candidate_position(),
                    desktop::cursor().map(|p| (p.x, p.y))
                );
                input::tests::session_wheel(center, -120);
                drain(&mut app);
                assert_eq!(app.preview.hovered_name(), Some("center"));
                assert_eq!(app.preview.selection_rect(), chosen);
                app.flush_selection_paint().unwrap();
                assert!(app.overlay.tip_text().contains("3/3"));
                assert_eq!(desktop::bounds(target).unwrap().1, before);
                input::tests::session_click(center, false);
                drain(&mut app);
                assert_eq!(app.preview.count(), 2);
                assert_eq!(app.preview.hovered_name(), Some("center"));
                input::tests::session_click(
                    POINT {
                        x: center.x + 1,
                        y: center.y + 1,
                    },
                    true,
                );
                drain(&mut app);
                let expected = app.cfg.presets["center"]
                    .resolve(work, before, desktop::desktop())
                    .unwrap();
                assert_eq!(desktop::bounds(target).unwrap().1, expected);
                assert!(!app.preview.active());
                // The same tip hosts native command entry and returns focus before
                // applying a mixed-separator expression to the captured target.
                input::tests::activate_session(double);
                drain(&mut app);
                input::tests::session_key(0xc0);
                drain(&mut app);
                let surface = app.prompt.as_ref().unwrap().window;
                unsafe {
                    let edit = GetDlgItem(surface, 1);
                    SetWindowTextW(edit, wide(r"0\0、100sw｜50sh").as_ptr());
                    SendMessageW(edit, WM_KEYDOWN, 13, 1);
                }
                drain(&mut app);
                assert!(app.prompt.is_none());
                assert_ne!(unsafe { IsWindow(surface) }, 0);
                assert_eq!(
                    desktop::bounds(target).unwrap().1,
                    crate::layout::Rect {
                        height: (work.height as f64 / 2.0).round() as i32,
                        ..work
                    }
                );
            }
        });
        let _ = child.kill();
        let _ = child.wait();
        unsafe {
            SetCursorPos(original_cursor.x, original_cursor.y);
        }
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
    #[test]
    #[ignore]
    fn wheel_backlog_cancels_without_rendering_intermediate_candidates() {
        let _hooks = input::Hooks::escape_only().unwrap();
        let mut app = test_app();
        app.begin_alt_preview(unsafe { GetForegroundWindow() }, false)
            .unwrap();
        let area = desktop::screen(desktop::cursor().unwrap(), 0).unwrap().work;
        let p = POINT {
            x: area.x + area.width / 2 + 3,
            y: area.y + area.height / 2 + 3,
        };
        let now = Instant::now();
        for _ in 0..30 {
            input::tests::enqueue_pointer_at(input::REGION_WHEEL, (-120i32) as usize, p, now);
        }
        input::tests::enqueue_at(input::CANCEL, now);
        let t = Instant::now();
        let mut count = 0;
        while let Some(event) = input::next_event() {
            count += 1;
            let cancel = event.message == input::CANCEL;
            app.handle_event(event).unwrap();
            if cancel {
                break;
            }
        }
        println!(
            "PERF wheel_backlog events_before_cancel={} cancel_latency_ms={:.3}",
            count - 1,
            t.elapsed().as_secs_f64() * 1000.0
        );
        assert!(app.held_window.is_none());
        assert!(
            t.elapsed() < Duration::from_millis(100),
            "cancel waited for wheel repaint backlog"
        );
        assert!(app.selection_paint.is_none());
    }
}
