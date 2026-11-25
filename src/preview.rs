use crate::{
    config::Config,
    desktop::{self, Display, err},
    layout::Rect,
    overlay::{self, Label, Panel},
};
use std::time::{Duration, Instant};
use windows_sys::Win32::{Foundation::*, Graphics::Gdi::*};

fn strokes(rect: Rect, area: Rect, width: i32) -> [Rect; 4] {
    let width = width.max(1).min(rect.width.max(1)).min(rect.height.max(1));
    let left = rect.x.max(area.x);
    let top = rect.y.max(area.y);
    let right = rect
        .x
        .saturating_add(rect.width)
        .min(area.x.saturating_add(area.width));
    let bottom = rect
        .y
        .saturating_add(rect.height)
        .min(area.y.saturating_add(area.height));
    // Interior edges share one centered stroke, instead of two inward strokes joined
    // across adjacent rectangles. Outer screen edges remain clipped inside the screen.
    let vx =
        |edge: i32| (edge - width / 2).clamp(area.x, (area.x + area.width - width).max(area.x));
    let hy =
        |edge: i32| (edge - width / 2).clamp(area.y, (area.y + area.height - width).max(area.y));
    [
        Rect {
            x: vx(left),
            y: top,
            width,
            height: bottom - top,
        },
        Rect {
            x: vx(right),
            y: top,
            width,
            height: bottom - top,
        },
        Rect {
            x: left,
            y: hy(top),
            width: right - left,
            height: width,
        },
        Rect {
            x: left,
            y: hy(bottom),
            width: right - left,
            height: width,
        },
    ]
}
struct Region(HRGN);
impl Region {
    fn new() -> Result<Self, String> {
        let handle = unsafe { CreateRectRgn(0, 0, 0, 0) };
        if handle.is_null() {
            Err(err())
        } else {
            Ok(Self(handle))
        }
    }
}
impl Drop for Region {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.0);
        }
    }
}

fn outline(
    panel: &Panel,
    rectangles: &[(Rect, Rect, Rect, i32)],
    desktop: Rect,
) -> Result<(), String> {
    let shape = Region::new()?;
    let strip = Region::new()?;
    unsafe {
        for &(rect, area, clip, width) in rectangles {
            if rect.width <= 0 || rect.height <= 0 {
                continue;
            }
            for line in strokes(rect, area, width) {
                let left = line.x.max(desktop.x).max(clip.x);
                let right = line
                    .x
                    .saturating_add(line.width)
                    .min(desktop.x.saturating_add(desktop.width))
                    .min(clip.x.saturating_add(clip.width));
                let top = line.y.max(desktop.y).max(clip.y);
                let bottom = line
                    .y
                    .saturating_add(line.height)
                    .min(desktop.y.saturating_add(desktop.height))
                    .min(clip.y.saturating_add(clip.height));
                if right > left && bottom > top {
                    desktop::check(SetRectRgn(
                        strip.0,
                        left - desktop.x,
                        top - desktop.y,
                        right - desktop.x,
                        bottom - desktop.y,
                    ))?;
                    desktop::check(CombineRgn(shape.0, shape.0, strip.0, RGN_OR))?;
                }
            }
        }
        desktop::check(SetWindowRgn(panel.0, shape.0, 1))?;
        std::mem::forget(shape); // SetWindowRgn owns the region after success.
    }
    Ok(())
}
struct PresetRegion {
    name: String,
    rect: Rect,
    area: Rect,
    dpi: i32,
    virtual_area: bool,
}

fn contains(rect: Rect, point: POINT) -> bool {
    i64::from(point.x) >= i64::from(rect.x)
        && i64::from(point.y) >= i64::from(rect.y)
        && i64::from(point.x) < i64::from(rect.x) + i64::from(rect.width)
        && i64::from(point.y) < i64::from(rect.y) + i64::from(rect.height)
}
fn nearest(regions: &[PresetRegion], point: POINT) -> Option<usize> {
    regions
        .iter()
        .enumerate()
        .filter(|(_, region)| contains(region.rect, point))
        .min_by_key(|(_, region)| {
            let rect = region.rect;
            // Double coordinates preserve half-pixel centers; i128 prevents overflow.
            let dx = i128::from(point.x) * 2 - (i128::from(rect.x) * 2 + i128::from(rect.width));
            let dy = i128::from(point.y) * 2 - (i128::from(rect.y) * 2 + i128::from(rect.height));
            (dx * dx + dy * dy, rect.y, rect.x, &region.name)
        })
        .map(|(index, _)| index)
}
fn grid_rect(area: Rect, cell: u32) -> Rect {
    let column = (cell - 1) as i32 % 3;
    let row = (cell - 1) as i32 / 3;
    let x = area.width * column / 3;
    let y = area.height * row / 3;
    Rect {
        x: area.x + x,
        y: area.y + y,
        width: area.width * (column + 1) / 3 - x,
        height: area.height * (row + 1) / 3 - y,
    }
}

pub struct Preview {
    base: Panel,
    highlight: Panel,
    label: Label,
    grid_labels: Vec<Label>,
    frame: Rect,
    display: Option<(HMONITOR, Display)>,
    regions: Vec<PresetRegion>,
    selected: Option<usize>,
    locked: Option<String>,
    grid: bool,
    focused: bool,
    hover: Option<(u32, Instant)>,
    pointer: Option<(i32, i32)>,
    display_epoch: u64,
    warnings: Vec<String>,
}
impl Preview {
    pub fn visible(&self) -> bool {
        self.display.is_some()
    }
    #[cfg(test)]
    pub fn focused(&self) -> bool {
        self.focused
    }
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            base: Panel::new()?,
            highlight: Panel::new()?,
            label: Label::new()?,
            grid_labels: Vec::new(),
            frame: Rect::default(),
            display: None,
            regions: Vec::new(),
            selected: None,
            locked: None,
            grid: false,
            focused: false,
            hover: None,
            pointer: None,
            display_epoch: desktop::display_epoch(),
            warnings: Vec::new(),
        })
    }
    pub fn start(&mut self, hwnd: HWND, cfg: &Config) -> Result<(), String> {
        self.begin(hwnd, cfg, false, None)
    }
    pub fn select(&mut self, hwnd: HWND, name: &str, cfg: &Config) -> Result<(), String> {
        self.begin(hwnd, cfg, false, Some(crate::config::command_name(name)))
    }
    pub fn grid(&mut self, hwnd: HWND, cfg: &Config) -> Result<(), String> {
        self.begin(hwnd, cfg, true, None)
    }
    fn begin(
        &mut self,
        hwnd: HWND,
        cfg: &Config,
        grid: bool,
        locked: Option<String>,
    ) -> Result<(), String> {
        self.hide();
        self.grid = grid;
        self.locked = locked;
        let point = desktop::cursor()?;
        self.frame = desktop::bounds(hwnd)
            .map(|(_, frame)| frame)
            .unwrap_or(desktop::screen(point, 0)?.work);
        let result = self.follow(point, cfg);
        if result.is_err() {
            // The desktop-wide outline is shown before the labels are rendered. A failure
            // after that point has no timer to remove it, so take it down here.
            self.hide();
        }
        result
    }
    pub fn hide(&mut self) {
        self.base.hide();
        self.highlight.hide();
        self.label.hide();
        for label in &self.grid_labels {
            label.hide();
        }
        self.display = None;
        self.selected = None;
        self.hover = None;
        self.focused = false;
        self.pointer = None;
    }
    fn color(cfg: &Config) -> u32 {
        if overlay::dark() {
            cfg.outline_dark
        } else {
            cfg.outline.color
        }
    }
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
    fn rebuild(&mut self, monitor: HMONITOR, cfg: &Config) -> Result<(), String> {
        let display = desktop::display(monitor)?;
        let virtual_area = desktop::desktop();
        let displays = desktop::displays()?;
        self.regions.clear();
        self.warnings.clear();
        for (name, layout) in &cfg.presets {
            let result = (|| -> Result<PresetRegion, String> {
                let target = if layout.monitor == 0 {
                    &display
                } else {
                    displays
                        .iter()
                        .find(|d| d.number == layout.monitor)
                        .ok_or_else(|| {
                            format!(
                                "Monitor {} is unavailable / 屏幕 {} 不可用",
                                layout.monitor, layout.monitor
                            )
                        })?
                };
                let rect = layout.resolve(target.work, self.frame, virtual_area)?;
                let virtual_layout = layout.uses_virtual_desktop();
                Ok(PresetRegion {
                    name: name.clone(),
                    rect,
                    area: if virtual_layout {
                        virtual_area
                    } else {
                        target.work
                    },
                    dpi: target.dpi,
                    virtual_area: virtual_layout,
                })
            })();
            match result {
                Ok(region) => self.regions.push(region),
                Err(error) => self
                    .warnings
                    .push(format!("{}: {error}", cfg.display_name(name))),
            }
        }
        if self
            .locked
            .as_ref()
            .is_some_and(|name| !self.regions.iter().any(|r| &r.name == name))
        {
            return Err(self.warnings.join("\n"));
        }
        self.regions.sort_by(|a, b| a.name.cmp(&b.name));
        let width = |dpi: i32| (cfg.outline.width * dpi / 96).max(1);
        let all: Vec<_> = if self.grid {
            (1..=9)
                .map(|cell| {
                    (
                        grid_rect(display.full, cell),
                        display.full,
                        display.full,
                        width(display.dpi),
                    )
                })
                .collect()
        } else {
            self.regions
                .iter()
                .flat_map(|r| {
                    if r.virtual_area {
                        displays
                            .iter()
                            .map(|d| (r.rect, r.area, d.full, width(d.dpi)))
                            .collect::<Vec<_>>()
                    } else {
                        vec![(r.rect, r.area, r.area, width(r.dpi))]
                    }
                })
                .collect()
        };
        outline(&self.base, &all, virtual_area)?;
        self.base.style(Self::color(cfg), cfg.outline.opacity);
        self.highlight.style(
            unsafe { GetSysColor(COLOR_HIGHLIGHT) },
            cfg.highlight_opacity,
        );
        self.display = Some((monitor, display));
        self.display_epoch = desktop::display_epoch();
        self.show_base(cfg)?;
        Ok(())
    }
    fn show_base(&mut self, cfg: &Config) -> Result<(), String> {
        self.highlight.hide();
        self.label.hide();
        self.selected = None;
        self.focused = false;
        self.hover = None;
        self.base.show(desktop::desktop());
        if self.grid {
            while self.grid_labels.len() < 9 {
                self.grid_labels.push(Label::new()?);
            }
            let display = &self.display.as_ref().unwrap().1;
            for cell in 1..=9 {
                let name = cfg
                    .cell(display.number, cell)
                    .map(|name| cfg.region_caption(name))
                    .unwrap_or_else(|| "—".into());
                self.grid_labels[cell as usize - 1].show(
                    &format!("{cell}\n{name}"),
                    grid_rect(display.full, cell),
                    Self::color(cfg),
                    display.dpi,
                    cfg,
                )?;
            }
        }
        Ok(())
    }
    fn highlight(&mut self, selected: Option<usize>, cfg: &Config) -> Result<(), String> {
        if self.selected == selected {
            return Ok(());
        }
        self.selected = selected;
        self.highlight.hide();
        self.label.hide();
        if let Some(index) = selected {
            let region = &self.regions[index];
            let dpi = region.dpi;
            // Selection uses a translucent interior fill; the ordinary outline stays unchanged.
            self.highlight.show(region.rect);
            self.label.show(
                &cfg.region_caption(&region.name),
                region.rect,
                Self::color(cfg),
                dpi,
                cfg,
            )?;
        }
        Ok(())
    }
    pub fn reset(&mut self, cfg: &Config) -> Result<bool, String> {
        if self.visible() && self.locked.is_none() && (self.grid || self.focused) {
            self.show_base(cfg)?;
            return Ok(true);
        }
        Ok(false)
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.hover
            .map(|(_, since)| since + Duration::from_secs(2))
            .filter(|_| !self.focused)
    }
    pub fn tick(&mut self, cfg: &Config) -> Result<(), String> {
        if self.visible() {
            let point = desktop::cursor()?;
            // Mouse hooks can miss moves while a native UI operation is running. Polling
            // only changed coordinates avoids starting a fresh dwell at the old deadline.
            if self.pointer != Some((point.x, point.y))
                || self.display_epoch != desktop::display_epoch()
                || self.deadline().is_some_and(|when| Instant::now() >= when)
            {
                self.follow(point, cfg)?;
            }
        }
        Ok(())
    }
    pub fn follow(&mut self, point: POINT, cfg: &Config) -> Result<(), String> {
        self.pointer = Some((point.x, point.y));
        let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
        if self.display.as_ref().is_none_or(|(old, _)| *old != monitor)
            || self.display_epoch != desktop::display_epoch()
        {
            self.rebuild(monitor, cfg)?;
        }
        if let Some(name) = &self.locked {
            let selected = self.regions.iter().position(|region| &region.name == name);
            return self.highlight(selected, cfg);
        }
        let display = &self.display.as_ref().unwrap().1;
        let area = display.full;
        let edge = point.x <= area.x + 1
            || point.y <= area.y + 1
            || point.x >= area.x + area.width - 2
            || point.y >= area.y + area.height - 2;
        if self.focused {
            // A grid cell may map to a region elsewhere (another monitor, a custom
            // mapping, the taskbar strip), so there the pointer's cell decides.
            let inside = |index: usize| {
                if self.grid {
                    cfg.cell(display.number, area.grid(point.x, point.y) as u32 + 1)
                        .is_some_and(|name| self.regions[index].name == name)
                } else {
                    contains(self.regions[index].rect, point)
                }
            };
            if edge || self.selected.is_none_or(|index| !inside(index)) {
                self.show_base(cfg)?;
            } else {
                return Ok(());
            }
        }
        if edge {
            self.hover = None;
            self.highlight(None, cfg)?;
            return Ok(());
        }
        let (key, selected) = if self.grid {
            let cell = area.grid(point.x, point.y) as u32 + 1;
            let name = cfg.cell(self.display.as_ref().unwrap().1.number, cell);
            (
                cell,
                name.and_then(|name| self.regions.iter().position(|r| r.name == name)),
            )
        } else {
            let selected = nearest(&self.regions, point);
            (selected.map_or(u32::MAX, |index| index as u32), selected)
        };
        if !self.grid {
            self.highlight(selected, cfg)?;
        }
        if selected.is_none() {
            self.hover = None;
            return Ok(());
        }
        let now = Instant::now();
        if self.hover.is_none_or(|(old, _)| old != key) {
            self.hover = Some((key, now));
        }
        if self
            .hover
            .is_some_and(|(_, since)| now.duration_since(since) >= Duration::from_secs(2))
        {
            self.base.hide();
            for label in &self.grid_labels {
                label.hide();
            }
            self.highlight(selected, cfg)?;
            self.focused = true;
            self.hover = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adjacent_regions_share_the_same_stroke() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 1200,
            height: 900,
        };
        let left = strokes(Rect { width: 600, ..area }, area, 2);
        let right = strokes(
            Rect {
                x: 600,
                width: 600,
                ..area
            },
            area,
            2,
        );
        assert_eq!(left[1], right[0]);
        let top = strokes(
            Rect {
                height: 450,
                ..area
            },
            area,
            2,
        );
        let bottom = strokes(
            Rect {
                y: 450,
                height: 450,
                ..area
            },
            area,
            2,
        );
        assert_eq!(top[3], bottom[2]);
    }
    fn region(name: &str, x: i32, y: i32, width: i32, height: i32) -> PresetRegion {
        let rect = Rect {
            x,
            y,
            width,
            height,
        };
        PresetRegion {
            name: name.into(),
            rect,
            area: rect,
            dpi: 96,
            virtual_area: false,
        }
    }
    #[test]
    fn overlap_uses_distance_then_top_left_independent_of_order() {
        let regions = [
            region("Right", 50, 0, 100, 100),
            region("Left", 0, 0, 100, 100),
            region("Center", 25, 25, 100, 100),
        ];
        assert_eq!(nearest(&regions, POINT { x: 100, y: 50 }), Some(0));
        assert_eq!(nearest(&regions, POINT { x: 75, y: 50 }), Some(1));
        assert_eq!(nearest(&regions, POINT { x: 75, y: 75 }), Some(2));
        assert_eq!(nearest(&regions, POINT { x: -1, y: 20 }), None);
    }
    #[test]
    fn grid_covers_odd_sized_negative_coordinate_screen() {
        let area = Rect {
            x: -1921,
            y: -80,
            width: 1921,
            height: 1081,
        };
        assert_eq!(grid_rect(area, 1).x, -1921);
        let last = grid_rect(area, 9);
        assert_eq!(last.x + last.width, 0);
        assert_eq!(last.y + last.height, 1001);
        assert_eq!(
            (1..=9)
                .map(|cell| {
                    let r = grid_rect(area, cell);
                    r.width * r.height
                })
                .sum::<i32>(),
            area.width * area.height
        );
        for cell in 1..=9 {
            let rect = grid_rect(area, cell);
            assert_eq!(area.grid(rect.x, rect.y), (cell - 1) as usize);
            assert_eq!(
                area.grid(rect.x + rect.width - 1, rect.y + rect.height - 1),
                (cell - 1) as usize
            );
        }
    }
}

#[cfg(test)]
mod multi_monitor_regressions {
    use super::*;
    use crate::test_support::{Dpi, displays, point};

    #[test]
    #[ignore = "requires an interactive desktop; some cases need mixed DPI monitors"]
    fn fixed_monitor_label_uses_destination_dpi() {
        let _dpi = Dpi::new();
        let Some(displays) = crate::test_support::require_displays(true) else {
            return;
        };
        let low = &displays.first().unwrap().1;
        let high = &displays.last().unwrap().1;
        let _overlay = overlay::Overlay::new().unwrap();
        let mut cfg = crate::test_support::cfg();
        cfg.presets.clear();
        cfg.presets.insert(
            "remote".into(),
            format!("0|0|100sw|100sh|{}", high.number).parse().unwrap(),
        );
        let mut preview = Preview::new().unwrap();
        preview.locked = Some("remote".into());
        preview.follow(point(low), &cfg).unwrap();
        let actual = overlay::label_rect(&preview.label);
        let expected = crate::label_text::render(
            "remote",
            high.work.width,
            high.dpi,
            Preview::color(&cfg),
            &cfg,
        )
        .unwrap();
        println!(
            "pointer dpi={} destination dpi={} label={}x{} expected={}x{}",
            low.dpi, high.dpi, actual.width, actual.height, expected.width, expected.height
        );
        preview.hide();
        assert_eq!(
            (actual.width, actual.height),
            (expected.width, expected.height),
            "Fixed-monitor labels must use that monitor's DPI"
        );
    }

    #[test]
    #[ignore = "requires an interactive desktop; some cases need mixed DPI monitors"]
    fn virtual_desktop_outline_reaches_second_monitor() {
        let _dpi = Dpi::new();
        let Some(displays) = crate::test_support::require_displays(false) else {
            return;
        };
        let primary = &displays[0].1;
        let virtual_area = desktop::desktop();
        let _overlay = overlay::Overlay::new().unwrap();
        let mut cfg = crate::test_support::cfg();
        cfg.presets.clear();
        cfg.presets
            .insert("span".into(), "0|0|100vw|100vh".parse().unwrap());
        let mut preview = Preview::new().unwrap();
        preview.follow(point(primary), &cfg).unwrap();
        let r = &preview.regions[0];
        assert_eq!(r.rect, virtual_area);
        let shape = Region::new().unwrap();
        assert_ne!(unsafe { GetWindowRgn(preview.base.0, shape.0) }, 0);
        let rightmost = &displays
            .iter()
            .max_by_key(|(_, d)| d.full.x + d.full.width)
            .unwrap()
            .1;
        let x = rightmost.full.x + rightmost.full.width - 1 - virtual_area.x;
        let y = rightmost.full.y + rightmost.full.height / 2 - virtual_area.y;
        let contains = unsafe { PtInRegion(shape.0, x, y) } != 0;
        println!(
            "virtual={virtual_area:?} clipping={:?} far edge present={contains}",
            r.area
        );
        preview.hide();
        assert!(
            contains,
            "Virtual-desktop preset outline is clipped to the pointer monitor"
        );
    }

    #[test]
    #[ignore = "requires an interactive desktop; some cases need mixed DPI monitors"]
    fn one_overflowing_preset_does_not_disable_other_previews() {
        let _dpi = Dpi::new();
        let displays = displays();
        let d = &displays[0].1;
        let _overlay = overlay::Overlay::new().unwrap();
        let mut doc = crate::config::help()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        doc["presets"]["Huge"] = toml_edit::value("0|0|999999999999sw|100sh");
        let cfg = Config::parse(&doc).unwrap();
        let mut preview = Preview::new().unwrap();
        let result = preview.follow(point(d), &cfg);
        println!("parse accepted=true preview result={result:?}");
        preview.hide();
        assert!(
            result.is_ok(),
            "One invalid resolved preset disables every valid preset preview"
        );
    }
}
