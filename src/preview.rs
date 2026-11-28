use crate::{
    config::Config,
    desktop::{self, Display, err},
    layout::Rect,
    overlay::{self, Label, Panel},
};

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
            (
                dx * dx + dy * dy,
                center(rect).1,
                center(rect).0,
                i64::from(rect.width) * i64::from(rect.height),
                &region.name,
            )
        })
        .map(|(index, _)| index)
}
fn center(rect: Rect) -> (i64, i64) {
    (
        i64::from(rect.x) * 2 + i64::from(rect.width),
        i64::from(rect.y) * 2 + i64::from(rect.height),
    )
}
fn overlapping(regions: &[PresetRegion], point: POINT) -> Vec<usize> {
    let mut candidates: Vec<_> = regions
        .iter()
        .enumerate()
        .filter_map(|(index, region)| contains(region.rect, point).then_some(index))
        .collect();
    candidates.sort_by_key(|&index| {
        let region = &regions[index];
        (
            i64::from(region.rect.width) * i64::from(region.rect.height),
            region.rect.y,
            region.rect.x,
            &region.name,
        )
    });
    candidates
}
fn coincident(regions: &[PresetRegion], selected: usize) -> Vec<usize> {
    let target = center(regions[selected].rect);
    regions
        .iter()
        .enumerate()
        .filter_map(|(index, r)| (center(r.rect) == target).then_some(index))
        .collect()
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Chosen {
    name: String,
    rect: Rect,
}

fn enclosing(rects: impl Iterator<Item = Rect>) -> Option<Rect> {
    let mut rects = rects;
    let first = rects.next()?;
    let (mut left, mut top) = (first.x, first.y);
    let (mut right, mut bottom) = (
        first.x.checked_add(first.width)?,
        first.y.checked_add(first.height)?,
    );
    for rect in rects {
        left = left.min(rect.x);
        top = top.min(rect.y);
        right = right.max(rect.x.checked_add(rect.width)?);
        bottom = bottom.max(rect.y.checked_add(rect.height)?);
    }
    Some(Rect {
        x: left,
        y: top,
        width: right.checked_sub(left)?,
        height: bottom.checked_sub(top)?,
    })
}

pub struct Preview {
    base: Panel,
    highlight: Panel,
    candidate_outline: Panel,
    overlaps: Vec<Panel>,
    label: Label,
    chosen: Vec<Chosen>,
    chosen_panels: Vec<Panel>,
    envelope: Panel,
    frame: Rect,
    painting: bool,
    interactive: bool,
    display: Option<(HMONITOR, Display)>,
    regions: Vec<PresetRegion>,
    selected: Option<usize>,
    candidates: Vec<usize>,
    cycled: Option<usize>,
    wheel_remainder: i32,
    defer_paint: bool,
    paint_pending: bool,
    locked: Option<String>,
    pointer: Option<(i32, i32)>,
    display_epoch: u64,
    warnings: Vec<String>,
}
impl Preview {
    pub fn active(&self) -> bool {
        self.display.is_some()
    }
    pub fn visible(&self) -> bool {
        self.active() && self.painting
    }

    pub fn new() -> Result<Self, String> {
        Ok(Self {
            base: Panel::new()?,
            highlight: Panel::new()?,
            candidate_outline: Panel::new()?,
            overlaps: Vec::new(),
            label: Label::new()?,
            chosen: Vec::new(),
            chosen_panels: Vec::new(),
            envelope: Panel::new()?,
            frame: Rect::default(),
            painting: true,
            interactive: false,
            display: None,
            regions: Vec::new(),
            selected: None,
            candidates: Vec::new(),
            cycled: None,
            wheel_remainder: 0,
            defer_paint: false,
            paint_pending: false,
            locked: None,
            pointer: None,
            display_epoch: desktop::display_epoch(),
            warnings: Vec::new(),
        })
    }
    pub fn start(&mut self, hwnd: HWND, cfg: &Config) -> Result<(), String> {
        self.begin(hwnd, cfg, None, false)
    }
    pub fn select(&mut self, hwnd: HWND, name: &str, cfg: &Config) -> Result<(), String> {
        self.begin(hwnd, cfg, Some(crate::config::command_name(name)), false)
    }
    pub fn interactive(&mut self, hwnd: HWND, cfg: &Config) -> Result<(), String> {
        self.begin(hwnd, cfg, None, true)
    }
    pub fn hit(&mut self, point: POINT, cfg: &Config) -> Result<Option<(String, Rect)>, String> {
        self.follow(point, cfg)?;
        Ok(self.selected.map(|index| {
            let region = &self.regions[index];
            (region.name.clone(), region.rect)
        }))
    }
    pub fn hovered_name(&self) -> Option<&str> {
        self.selected.map(|index| self.regions[index].name.as_str())
    }
    pub fn candidate_position(&self) -> Option<(usize, usize)> {
        let selected = self.selected?;
        let position = self
            .candidates
            .iter()
            .position(|&index| index == selected)?;
        (self.interactive && self.candidates.len() > 1)
            .then_some((position + 1, self.candidates.len()))
    }
    /// Returns whether the wheel browsed overlapping candidates, even when a
    /// smooth wheel has not yet accumulated a complete notch.
    pub fn cycle(&mut self, point: POINT, delta: i32, cfg: &Config) -> Result<bool, String> {
        if !self.active() || !self.interactive || !cfg.mouse_enabled || delta == 0 {
            return Ok(false);
        }
        self.follow(point, cfg)?;
        let Some((position, count)) = self.candidate_position() else {
            self.wheel_remainder = 0;
            return Ok(false);
        };
        self.wheel_remainder = self.wheel_remainder.saturating_add(delta);
        let steps = self.wheel_remainder / 120;
        self.wheel_remainder %= 120;
        if steps != 0 {
            // Windows reports upward scrolling as positive: down selects a larger region.
            let next = (position as i64 - 1 - i64::from(steps)).rem_euclid(count as i64) as usize;
            self.cycled = Some(self.candidates[next]);
            self.highlight(self.cycled, cfg)?;
        }
        Ok(true)
    }
    pub fn cycle_deferred(
        &mut self,
        point: POINT,
        delta: i32,
        cfg: &Config,
    ) -> Result<bool, String> {
        self.defer_paint = true;
        let result = self.cycle(point, delta, cfg);
        self.defer_paint = false;
        result
    }
    pub fn flush_paint(&mut self, cfg: &Config) -> Result<(), String> {
        if self.paint_pending {
            self.paint_pending = false;
            let selected = self.selected.take();
            self.highlight(selected, cfg)?;
        }
        Ok(())
    }
    pub fn count(&self) -> usize {
        self.chosen.len()
    }
    pub fn restyle(&mut self, cfg: &Config) -> Result<(), String> {
        let Some((monitor, _)) = self.display.as_ref() else {
            return Ok(());
        };
        let monitor = *monitor;
        let selected = self
            .selected
            .and_then(|i| self.regions.get(i))
            .map(|r| r.name.clone());
        let cycled = self
            .cycled
            .and_then(|i| self.regions.get(i))
            .map(|r| r.name.clone());
        let chosen = std::mem::take(&mut self.chosen);
        self.rebuild(monitor, cfg)?;
        for region in chosen {
            self.toggle(region.name, region.rect, cfg)?;
        }
        let index = selected.and_then(|name| self.regions.iter().position(|r| r.name == name));
        if let Some((x, y)) = self.pointer {
            self.candidates = overlapping(&self.regions, POINT { x, y });
            self.cycled = cycled.and_then(|name| self.regions.iter().position(|r| r.name == name));
        }
        self.highlight(index, cfg)
    }
    pub fn selection_rect(&self) -> Option<Rect> {
        enclosing(self.chosen.iter().map(|region| region.rect))
    }
    pub fn toggle(&mut self, name: String, rect: Rect, cfg: &Config) -> Result<(), String> {
        let chosen = Chosen { name, rect };
        if let Some(index) = self.chosen.iter().position(|r| *r == chosen) {
            self.chosen.remove(index);
        } else {
            self.chosen.push(chosen);
        }
        if self.interactive {
            self.painting = cfg.preview_mode == "always"
                || (cfg.preview_mode == "selected" && !self.chosen.is_empty());
        }
        if !self.painting {
            self.hide_layers();
            return Ok(());
        }
        self.base.show(desktop::desktop());
        for panel in &self.chosen_panels {
            panel.hide();
        }
        while self.chosen_panels.len() < self.chosen.len() {
            self.chosen_panels.push(Panel::new()?);
        }
        for (region, panel) in self.chosen.iter().zip(&self.chosen_panels) {
            panel.style(Self::color(cfg), cfg.highlight_opacity);
            panel.show(region.rect);
        }
        self.envelope.hide();
        if let Some(rect) = self.selection_rect() {
            let area = desktop::desktop();
            outline(
                &self.envelope,
                &[(rect, area, area, cfg.outline.width.max(2))],
                area,
            )?;
            self.envelope
                .style(unsafe { GetSysColor(COLOR_HIGHLIGHT) }, 255);
            self.envelope.show(area);
        }
        let hovered = self.selected.take();
        self.highlight(hovered, cfg)?;
        Ok(())
    }
    fn begin(
        &mut self,
        hwnd: HWND,
        cfg: &Config,
        locked: Option<String>,
        interactive: bool,
    ) -> Result<(), String> {
        self.hide();
        self.interactive = interactive;
        self.painting = !interactive || cfg.preview_mode == "always";
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
    fn hide_layers(&self) {
        self.base.hide();
        self.highlight.hide();
        self.candidate_outline.hide();
        for panel in &self.overlaps {
            panel.hide();
        }
        self.label.hide();
        for panel in &self.chosen_panels {
            panel.hide();
        }
        self.envelope.hide();
    }
    pub fn hide(&mut self) {
        self.hide_layers();
        self.chosen.clear();
        self.display = None;
        self.selected = None;
        self.candidates.clear();
        self.cycled = None;
        self.wheel_remainder = 0;
        self.pointer = None;
        self.paint_pending = false;
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
        self.candidates.clear();
        self.cycled = None;
        self.wheel_remainder = 0;
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
        let all: Vec<_> = self
            .regions
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
            .collect();
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
    fn show_base(&mut self, _cfg: &Config) -> Result<(), String> {
        self.highlight.hide();
        self.candidate_outline.hide();
        for panel in &self.overlaps {
            panel.hide();
        }
        self.label.hide();
        self.selected = None;
        if self.painting {
            self.base.show(desktop::desktop());
        }
        Ok(())
    }
    fn highlight(&mut self, selected: Option<usize>, cfg: &Config) -> Result<(), String> {
        if self.selected == selected {
            return Ok(());
        }
        self.selected = selected;
        if self.defer_paint {
            self.paint_pending = true;
            return Ok(());
        }
        self.paint_pending = false;
        self.highlight.hide();
        self.candidate_outline.hide();
        for panel in &self.overlaps {
            panel.hide();
        }
        self.label.hide();
        if !self.painting {
            return Ok(());
        }
        if let Some(index) = selected {
            self.highlight.style(
                unsafe { GetSysColor(COLOR_HIGHLIGHT) },
                cfg.highlight_opacity,
            );
            let group = if self.locked.is_some() {
                vec![index]
            } else {
                coincident(&self.regions, index)
            };
            while self.overlaps.len() + 1 < group.len() {
                self.overlaps.push(Panel::new()?);
            }
            for (index, panel) in group
                .iter()
                .copied()
                .filter(|other| *other != index)
                .zip(&self.overlaps)
            {
                panel.style(
                    unsafe { GetSysColor(COLOR_HIGHLIGHT) },
                    if self.interactive {
                        cfg.highlight_opacity / 2
                    } else {
                        cfg.highlight_opacity
                    },
                );
                panel.show(self.regions[index].rect);
            }
            let region = &self.regions[index];
            let dpi = region.dpi;
            // Selection uses a translucent interior fill; the ordinary outline stays unchanged.
            self.highlight.show(region.rect);
            if self.interactive {
                let desktop = desktop::desktop();
                let width = ((cfg.outline.width + 2) * dpi / 96).max(3);
                outline(
                    &self.candidate_outline,
                    &[(region.rect, region.area, region.area, width)],
                    desktop,
                )?;
                self.candidate_outline
                    .style(unsafe { GetSysColor(COLOR_HIGHLIGHT) }, 255);
                self.candidate_outline.show(desktop);
            }
            let label = if self.interactive {
                cfg.region_caption(&region.name)
            } else {
                group
                    .iter()
                    .map(|&i| cfg.region_caption(&self.regions[i].name))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            self.label
                .show(&label, region.rect, Self::color(cfg), dpi, cfg)?;
        }
        Ok(())
    }
    pub fn tick(&mut self, cfg: &Config) -> Result<(), String> {
        if self.active() {
            let point = desktop::cursor()?;
            // Mouse hooks can miss moves while a native UI operation is running. Polling
            // only changed coordinates avoids starting a fresh dwell at the old deadline.
            if self.pointer != Some((point.x, point.y))
                || self.display_epoch != desktop::display_epoch()
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
        if edge {
            self.candidates.clear();
            self.cycled = None;
            self.wheel_remainder = 0;
            self.highlight(None, cfg)?;
            return Ok(());
        }
        let candidates = overlapping(&self.regions, point);
        if self.candidates != candidates {
            self.wheel_remainder = 0;
            self.candidates = candidates;
        }
        self.cycled = self.cycled.filter(|index| self.candidates.contains(index));
        let selected = self.cycled.or_else(|| nearest(&self.regions, point));
        self.highlight(selected, cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wheel_candidates_include_only_pointer_overlaps_in_stable_size_order() {
        let mut regions = vec![
            region("large", -100, -100, 400, 400),
            region("small", 40, 40, 20, 20),
            region("same-size-right", 25, 0, 100, 100),
            region("same-size-left", 0, 0, 100, 100),
            region("outside", 500, 0, 10, 10),
        ];
        let point = POINT { x: 50, y: 50 };
        let names = |regions: &[PresetRegion]| {
            overlapping(regions, point)
                .iter()
                .map(|&i| regions[i].name.clone())
                .collect::<Vec<_>>()
        };
        let expected = ["small", "same-size-left", "same-size-right", "large"];
        assert_eq!(names(&regions), expected);
        regions.reverse();
        assert_eq!(names(&regions), expected);
        assert!(overlapping(&regions, POINT { x: 800, y: 800 }).is_empty());
    }
    #[test]
    #[ignore = "shows actual region overlays to verify wheel selection, smooth deltas, and reset boundaries"]
    fn wheel_browsing_keeps_center_until_exit_and_respects_display_modes() {
        let _dpi = crate::test_support::Dpi::new();
        let _overlay = overlay::Overlay::new().unwrap();
        let mut cfg = crate::test_support::cfg();
        let area = desktop::screen(desktop::cursor().unwrap(), 0).unwrap().work;
        let point = POINT {
            x: area.x + area.width / 2 + 3,
            y: area.y + area.height / 2 + 3,
        };
        for mode in ["always", "selected", "off"] {
            cfg.preview_mode = mode.into();
            let mut preview = Preview::new().unwrap();
            preview.interactive(std::ptr::null_mut(), &cfg).unwrap();
            preview.follow(point, &cfg).unwrap();
            assert_eq!(preview.hovered_name(), Some("5/9"));
            assert_eq!(preview.candidate_position(), Some((1, 3)));
            for _ in 0..3 {
                assert!(preview.cycle(point, -30, &cfg).unwrap());
                assert_eq!(preview.hovered_name(), Some("5/9"));
            }
            preview.cycle(point, -30, &cfg).unwrap();
            assert_eq!(preview.hovered_name(), Some("4/4"));
            preview.cycle(point, -120, &cfg).unwrap();
            assert_eq!(preview.hovered_name(), Some("center"));
            assert_eq!(preview.candidate_position(), Some((3, 3)));
            assert_eq!(preview.count(), 0);
            assert_eq!(preview.visible(), mode == "always");
            // Slight motion and even crossing another candidate's edge keep the browsed region.
            let inside = POINT {
                x: area.x + area.width / 4,
                y: area.y + area.height / 4,
            };
            let (name, rect) = preview.hit(inside, &cfg).unwrap().unwrap();
            assert_eq!(name, "center");
            preview.toggle(name, rect, &cfg).unwrap();
            assert_eq!(preview.visible(), mode != "off");
            assert_eq!(preview.selection_rect(), Some(rect));
            preview.restyle(&cfg).unwrap();
            assert_eq!(preview.hit(point, &cfg).unwrap().unwrap().0, "center");
            preview.cycle(point, -120, &cfg).unwrap();
            assert_eq!(preview.hovered_name(), Some("5/9"));
            preview.cycle(point, 120, &cfg).unwrap();
            assert_eq!(preview.hovered_name(), Some("center"));
            assert_eq!(
                preview.selection_rect(),
                Some(rect),
                "wheel must never change cumulative selection"
            );
            let first_number = preview.display.as_ref().unwrap().1.number;
            if let Some(display) = desktop::displays()
                .unwrap()
                .iter()
                .find(|d| d.number != first_number)
            {
                let second = POINT {
                    x: display.work.x + display.work.width / 2 + 3,
                    y: display.work.y + display.work.height / 2 + 3,
                };
                preview.follow(second, &cfg).unwrap();
                assert_eq!(preview.hovered_name(), Some("5/9"));
                assert!(preview.cycled.is_none());
                preview.follow(point, &cfg).unwrap();
            }
            let outside = POINT {
                x: area.x + 5,
                y: area.y + 5,
            };
            preview.follow(outside, &cfg).unwrap();
            assert_ne!(preview.hovered_name(), Some("center"));
            preview.follow(point, &cfg).unwrap();
            assert_eq!(preview.hovered_name(), Some("5/9"));
            preview.cycle(point, -60, &cfg).unwrap();
            desktop::invalidate_displays();
            preview.follow(point, &cfg).unwrap();
            preview.cycle(point, -60, &cfg).unwrap();
            assert_eq!(
                preview.hovered_name(),
                Some("5/9"),
                "display rebuild must reset wheel fractions"
            );
            preview.hide();
            assert!(!preview.cycle(point, -120, &cfg).unwrap());
            assert_eq!(
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible(
                        preview.candidate_outline.0,
                    )
                },
                0
            );
            preview.interactive(std::ptr::null_mut(), &cfg).unwrap();
            preview.cycle(point, -60, &cfg).unwrap();
            assert_eq!(preview.hovered_name(), Some("5/9"));
        }
        cfg.presets.retain(|name, _| name == "center");
        let mut only_one = Preview::new().unwrap();
        only_one.interactive(std::ptr::null_mut(), &cfg).unwrap();
        only_one.follow(point, &cfg).unwrap();
        assert_eq!(only_one.hovered_name(), Some("center"));
        assert!(only_one.candidate_position().is_none());
        assert!(!only_one.cycle(point, -120, &cfg).unwrap());
        cfg = crate::test_support::cfg();
        cfg.mouse_enabled = false;
        let mut preview = Preview::new().unwrap();
        preview.interactive(std::ptr::null_mut(), &cfg).unwrap();
        preview.follow(point, &cfg).unwrap();
        assert!(!preview.cycle(point, -120, &cfg).unwrap());
        assert_eq!(preview.hovered_name(), Some("5/9"));
    }
    #[test]
    #[ignore = "opens temporary previews to verify all three display modes across selection changes"]
    fn display_modes_apply_to_the_entire_selection_session() {
        let _overlay = overlay::Overlay::new().unwrap();
        let mut cfg = crate::test_support::cfg();
        for mode in ["always", "selected", "off"] {
            cfg.preview_mode = mode.into();
            let mut preview = Preview::new().unwrap();
            preview.interactive(std::ptr::null_mut(), &cfg).unwrap();
            assert!(preview.active());
            assert_eq!(preview.visible(), mode == "always");
            let rect = Rect {
                x: 100,
                y: 100,
                width: 300,
                height: 200,
            };
            preview.toggle("one".into(), rect, &cfg).unwrap();
            assert_eq!(preview.count(), 1);
            assert_eq!(preview.visible(), mode != "off");
            preview.toggle("one".into(), rect, &cfg).unwrap();
            assert_eq!(preview.count(), 0);
            assert_eq!(preview.visible(), mode == "always");
            preview.hide();
            assert!(!preview.active());
        }
    }
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
    fn coincident_centers_show_all_regions_but_choose_smallest_then_name() {
        let regions = [
            region("Big", 0, 0, 300, 300),
            region("Small", 100, 100, 100, 100),
            region("Alias", 100, 100, 100, 100),
            region("Other", 101, 100, 100, 100),
        ];
        assert_eq!(nearest(&regions, POINT { x: 150, y: 150 }), Some(2));
        assert_eq!(coincident(&regions, 2), [0, 1, 2]);
        assert_eq!(coincident(&regions, 3), [3]);
    }
    #[test]
    #[ignore = "shows layered highlights to verify coincident regions and their alpha"]
    fn coincident_highlights_use_separate_translucent_windows() {
        let _overlay = overlay::Overlay::new().unwrap();
        let cfg = crate::test_support::cfg();
        let mut preview = Preview::new().unwrap();
        preview.regions = vec![
            region("Big", 100, 100, 300, 300),
            region("Small", 200, 200, 100, 100),
        ];
        preview.highlight(Some(1), &cfg).unwrap();
        assert_eq!(preview.overlaps.len(), 1);
        for hwnd in [preview.highlight.0, preview.overlaps[0].0] {
            let mut alpha = 0;
            let mut flags = 0;
            unsafe {
                assert_ne!(
                    windows_sys::Win32::UI::WindowsAndMessaging::GetLayeredWindowAttributes(
                        hwnd,
                        std::ptr::null_mut(),
                        &mut alpha,
                        &mut flags
                    ),
                    0
                );
                assert_ne!(
                    windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd),
                    0
                );
            }
            assert_eq!(alpha, cfg.highlight_opacity);
        }
        preview.hide();
        assert_eq!(
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible(preview.overlaps[0].0)
            },
            0
        );
    }
    #[test]
    fn selection_encloses_disjoint_and_diagonal_regions() {
        let a = Rect {
            x: -900,
            y: 0,
            width: 300,
            height: 300,
        };
        let b = Rect {
            x: -600,
            y: 300,
            width: 300,
            height: 300,
        };
        assert_eq!(
            enclosing([a, b].into_iter()),
            Some(Rect {
                x: -900,
                y: 0,
                width: 600,
                height: 600
            })
        );
        assert_eq!(enclosing([].into_iter()), None);
        assert_eq!(enclosing([a].into_iter()), Some(a));
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
