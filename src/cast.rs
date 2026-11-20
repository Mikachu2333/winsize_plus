//! Transient, click-through input annotations. No input is written to disk.
use crate::{
    config::{CastStyle, Config},
    desktop::{self, Display},
    label_text,
    layout::Rect,
    overlay::{Label, Panel},
};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use windows_sys::Win32::{Foundation::*, UI::Input::KeyboardAndMouse::*};

pub const CTRL: u32 = 1;
pub const ALT: u32 = 2;
pub const SHIFT: u32 = 4;
pub const WIN: u32 = 8;

fn modifiers(text: &str, flags: u32) -> String {
    let mut parts = Vec::new();
    for (flag, name) in [(CTRL, "Ctrl"), (ALT, "Alt"), (SHIFT, "Shift"), (WIN, "Win")] {
        if flags & flag != 0 {
            parts.push(name);
        }
    }
    parts.push(text);
    parts.join(" + ")
}

pub fn key_caption(key: u32, detail: u32) -> Option<String> {
    if matches!(key, 0x10..=0x12 | 0x5b..=0x5c | 0xa0..=0xa5) {
        return None;
    }
    let special = match key {
        8 => Some("Backspace"),
        9 => Some("Tab"),
        13 => Some("Enter"),
        27 => Some("Esc"),
        32 => Some("Space"),
        33 => Some("PageUp"),
        34 => Some("PageDown"),
        35 => Some("End"),
        36 => Some("Home"),
        37 => Some("←"),
        38 => Some("↑"),
        39 => Some("→"),
        40 => Some("↓"),
        44 => Some("PrintScreen"),
        45 => Some("Insert"),
        46 => Some("Delete"),
        _ => None,
    };
    let name = if let Some(name) = special {
        name.into()
    } else if (0x70..=0x87).contains(&key) {
        format!("F{}", key - 0x6f)
    } else {
        let scan = (detail >> 8) & 255;
        let scan = if scan == 0 {
            unsafe { MapVirtualKeyW(key, MAPVK_VK_TO_VSC) }
        } else {
            scan
        };
        let mut name = [0u16; 128];
        // SAFETY: GetKeyNameTextW writes only into this stack buffer. Scan-code names
        // do not compose IME text or change the target application's keyboard state.
        let len = unsafe {
            GetKeyNameTextW(
                ((scan << 16) | ((detail & (1 << 16)) << 8)) as i32,
                name.as_mut_ptr(),
                name.len() as i32,
            )
        };
        if len > 0 {
            String::from_utf16_lossy(&name[..len as usize])
        } else if (0x30..=0x5a).contains(&key) {
            char::from_u32(key)?.to_string()
        } else {
            format!("VK:{key:02X}")
        }
    };
    Some(modifiers(&name, detail & 15))
}

fn opacity(age: Duration, style: &CastStyle) -> u8 {
    let age = age.as_millis();
    let linger = u128::from(style.duration);
    let fade = u128::from(style.fade);
    if age <= linger {
        style.opacity
    } else if fade == 0 || age >= linger + fade {
        0
    } else {
        (u128::from(style.opacity) * (linger + fade - age) / fade) as u8
    }
}

struct Badge {
    background: Panel,
    label: Label,
    image: label_text::Image,
    padding: i32,
}
impl Badge {
    fn new(text: &str, display: &Display, style: &CastStyle) -> Result<Self, String> {
        let padding = (8 * display.dpi / 96).max(1);
        let width = (display.work.width - 4 * padding).clamp(1, 600 * display.dpi / 96);
        Ok(Self {
            background: Panel::new()?,
            label: Label::new()?,
            image: label_text::annotation(text, width, display.dpi, style)?,
            padding,
        })
    }
    fn height(&self) -> i32 {
        self.image.height + 2 * self.padding
    }
    fn draw(
        &self,
        x: i32,
        y: i32,
        display: &Display,
        style: &CastStyle,
        alpha: u8,
    ) -> Result<(), String> {
        let width = self.image.width + 2 * self.padding;
        let height = self.height();
        let area = display.work;
        let x = x.clamp(area.x, (area.x + area.width - width).max(area.x));
        let y = y.clamp(area.y, (area.y + area.height - height).max(area.y));
        self.background.round(width, height, self.padding * 2)?;
        self.background.style(style.background, alpha);
        self.background.show(Rect {
            x,
            y,
            width,
            height,
        });
        self.label
            .image(&self.image, x + self.padding, y + self.padding, alpha)
    }
}

struct Entry {
    text: String,
    count: u32,
    started: Instant,
    badge: Badge,
}
pub struct Visuals {
    keys: VecDeque<Entry>,
    mouse: crate::mouse_cast::MouseVisuals,
    preview_step: usize,
    display: Option<Display>,
    next: Option<Instant>,
    epoch: u64,
}
impl Visuals {
    pub fn new() -> Self {
        Self {
            keys: VecDeque::new(),
            mouse: crate::mouse_cast::MouseVisuals::new(),
            preview_step: 0,
            display: None,
            next: None,
            epoch: desktop::display_epoch(),
        }
    }
    pub fn clear(&mut self) {
        self.keys.clear();
        self.mouse.clear();
        self.display = None;
        self.next = None;
    }
    pub fn clear_keys(&mut self) {
        self.keys.clear();
    }
    pub fn clear_mouse(&mut self) {
        self.mouse.clear();
    }
    #[cfg(test)]
    pub(crate) fn mouse_active(&self) -> bool {
        self.mouse.active()
    }
    pub fn deadline(&self) -> Option<Instant> {
        [self.next, self.mouse.deadline()]
            .into_iter()
            .flatten()
            .min()
    }
    pub fn prepare_mouse(&mut self, point: POINT, cfg: &Config) -> Result<(), String> {
        self.mouse.prepare(point, &cfg.mousecast)
    }
    pub fn follow_mouse(&mut self, point: POINT, cfg: &Config) -> Result<(), String> {
        self.mouse.follow(point, &cfg.mousecast)
    }
    pub fn key(&mut self, text: String, point: POINT, cfg: &Config) -> Result<(), String> {
        let now = Instant::now();
        let display = desktop::screen(point, 0)?;
        if self
            .display
            .as_ref()
            .is_some_and(|d| d.number != display.number || d.dpi != display.dpi)
        {
            self.keys.clear();
        }
        self.display = Some(display.clone());
        let count = if self.keys.back().is_some_and(|e| {
            e.text == text && opacity(now.saturating_duration_since(e.started), &cfg.keycast) > 0
        }) {
            self.keys.pop_back().unwrap().count.saturating_add(1)
        } else {
            1
        };
        let caption = if count > 1 {
            format!("{text} ×{count}")
        } else {
            text.clone()
        };
        let badge = Badge::new(&caption, &display, &cfg.keycast)?;
        self.keys.push_back(Entry {
            text,
            count,
            started: now,
            badge,
        });
        while self.keys.len() > cfg.keycast.history {
            self.keys.pop_front();
        }
        self.tick(cfg)
    }
    pub fn mouse(
        &mut self,
        button: u32,
        flags: u32,
        point: POINT,
        cfg: &Config,
    ) -> Result<(), String> {
        let result = self.mouse.event(button, flags, point, &cfg.mousecast);
        if result.is_err() {
            self.mouse.clear();
        }
        result
    }
    pub fn tick(&mut self, cfg: &Config) -> Result<(), String> {
        let result = self.draw(cfg);
        if result.is_err() {
            self.clear();
        }
        result
    }
    fn draw(&mut self, cfg: &Config) -> Result<(), String> {
        if self.epoch != desktop::display_epoch() {
            // New input selects the new display topology; stale animation coordinates
            // must never keep invisible windows or timers alive on a removed monitor.
            self.epoch = desktop::display_epoch();
            if let Some(d) = &self.display {
                let current = desktop::screen(
                    POINT {
                        x: d.work.x,
                        y: d.work.y,
                    },
                    d.number,
                );
                if !current
                    .as_ref()
                    .is_ok_and(|new| new.work == d.work && new.dpi == d.dpi)
                {
                    self.keys.clear();
                }
            }
        }
        let now = Instant::now();
        self.keys
            .retain(|e| opacity(now.saturating_duration_since(e.started), &cfg.keycast) > 0);
        if let Some(display) = &self.display {
            let margin = 16 * display.dpi / 96;
            let bottom = cfg.keycast.position.starts_with("bottom");
            let right = cfg.keycast.position.ends_with("right");
            let area = display.work;
            let mut edge = if bottom {
                area.y + area.height - margin
            } else {
                area.y + margin
            };
            for entry in self.keys.iter().rev() {
                let h = entry.badge.height();
                if bottom {
                    edge -= h;
                }
                if edge < area.y || edge + h > area.y + area.height {
                    entry.badge.background.hide();
                    entry.badge.label.hide();
                    continue;
                }
                let x = if right {
                    area.x + area.width - margin - entry.badge.image.width - 2 * entry.badge.padding
                } else {
                    area.x + margin
                };
                entry.badge.draw(
                    x,
                    edge,
                    display,
                    &cfg.keycast,
                    opacity(now.saturating_duration_since(entry.started), &cfg.keycast),
                )?;
                edge += if bottom { -margin / 2 } else { h + margin / 2 };
            }
        }
        self.mouse.tick(&cfg.mousecast)?;
        self.next = (!self.keys.is_empty()).then(|| now + Duration::from_millis(16));
        Ok(())
    }
    pub fn preview(&mut self, mouse: bool, point: POINT, cfg: &Config) -> Result<(), String> {
        self.clear();
        if mouse {
            let step = self.preview_step;
            self.preview_step = self.preview_step.wrapping_add(1);
            self.mouse.preview(step, point, &cfg.mousecast)
        } else {
            self.key("Ctrl + Shift + K".into(), point, cfg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    #[test]
    #[ignore = "measures real mouse rendering CPU and latency for three seconds"]
    fn mouse_rendering_cost() {
        let _dpi = crate::test_support::Dpi::new();
        let _overlay = crate::overlay::Overlay::new().unwrap();
        let cfg = crate::test_support::cfg();
        let p = desktop::cursor().unwrap();
        let mut visuals = Visuals::new();
        let cpu = || unsafe {
            use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
            let mut times = [FILETIME::default(); 4];
            let [create, exit, kernel, user] = &mut times;
            assert_ne!(
                GetProcessTimes(GetCurrentProcess(), create, exit, kernel, user),
                0
            );
            let value = |v: FILETIME| {
                ((u64::from(v.dwHighDateTime) << 32) | u64::from(v.dwLowDateTime)) as f64 / 10000.0
            };
            value(times[2]) + value(times[3])
        };
        let start = Instant::now();
        visuals.mouse(8, 0, p, &cfg).unwrap();
        let cold = start.elapsed();
        let cpu_before = cpu();
        let began = Instant::now();
        let mut costs = Vec::new();
        for n in 0..360 {
            let start = Instant::now();
            visuals.mouse(8, 0, p, &cfg).unwrap();
            costs.push(start.elapsed().as_micros());
            if n % 2 == 0 {
                visuals.tick(&cfg).unwrap();
                crate::test_support::pump();
            }
            let deadline = began + Duration::from_micros((n + 1) * 8333);
            if let Some(wait) = deadline.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
        costs.sort();
        println!(
            "cold_us={} warm_p50_us={} warm_p95_us={} cpu_ms={:.2} wall_ms={}",
            cold.as_micros(),
            costs[180],
            costs[342],
            cpu() - cpu_before,
            began.elapsed().as_millis()
        );
        visuals.clear();
    }
    #[test]
    fn combinations_keep_modifiers_and_special_keys() {
        assert_eq!(
            key_caption(13, CTRL | SHIFT).as_deref(),
            Some("Ctrl + Shift + Enter")
        );
        assert_eq!(key_caption(0x7b, ALT).as_deref(), Some("Alt + F12"));
        assert_eq!(key_caption(0xa2, CTRL), None);
    }
    #[test]
    fn fade_has_exact_linger_and_expiry_boundaries() {
        let cfg = crate::test_support::cfg();
        let style = &cfg.keycast;
        assert_eq!(
            opacity(Duration::from_millis(style.duration.into()), style),
            style.opacity
        );
        assert_eq!(
            opacity(
                Duration::from_millis((style.duration + style.fade).into()),
                style
            ),
            0
        );
        assert!(
            opacity(
                Duration::from_millis((style.duration + style.fade / 2).into()),
                style
            ) < style.opacity
        );
    }
    #[test]
    #[ignore = "opens temporary annotation windows on each available monitor"]
    fn annotations_bound_history_expire_and_preserve_focus() {
        let _dpi = crate::test_support::Dpi::new();
        let _overlay = crate::overlay::Overlay::new().unwrap();
        let cfg = crate::test_support::cfg();
        let foreground = unsafe { GetForegroundWindow() };
        let mut visuals = Visuals::new();
        for (_, display) in crate::test_support::displays() {
            let p = crate::test_support::point(&display);
            visuals.preview(false, p, &cfg).unwrap();
            visuals.key("Ctrl + Shift + K".into(), p, &cfg).unwrap();
            assert_eq!(visuals.keys.len(), 1);
            assert_eq!(visuals.keys[0].count, 2);
            for n in 0..12 {
                visuals.key(format!("F{n}"), p, &cfg).unwrap();
            }
            assert_eq!(visuals.keys.len(), cfg.keycast.history);
            for entry in &visuals.keys {
                let rect = desktop::bounds(entry.badge.background.0).unwrap().0;
                assert!(
                    rect.x >= display.work.x
                        && rect.x + rect.width <= display.work.x + display.work.width
                );
                assert!(
                    rect.y >= display.work.y
                        && rect.y + rect.height <= display.work.y + display.work.height
                );
            }
            for n in 0..10 {
                visuals
                    .mouse(if n % 2 == 0 { 1 } else { 8 }, CTRL, p, &cfg)
                    .unwrap();
            }
            assert!(visuals.mouse.active());
            assert_eq!(unsafe { GetForegroundWindow() }, foreground);
            for entry in &mut visuals.keys {
                entry.started = Instant::now() - Duration::from_secs(20);
            }
            visuals.clear_mouse();
            visuals.tick(&cfg).unwrap();
            assert!(visuals.keys.is_empty() && !visuals.mouse.active());
            assert!(visuals.deadline().is_none());
        }
    }
}
