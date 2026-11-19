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
    [
        Rect {
            x: left,
            y: top,
            width,
            height: bottom - top,
        },
        Rect {
            x: right - width,
            y: top,
            width,
            height: bottom - top,
        },
        Rect {
            x: left,
            y: top,
            width: right - left,
            height: width,
        },
        Rect {
            x: left,
            y: bottom - width,
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
    rectangles: &[(Rect, Rect)],
    desktop: Rect,
    width: i32,
) -> Result<(), String> {
    let shape = Region::new()?;
    let strip = Region::new()?;
    unsafe {
        for &(rect, area) in rectangles {
            if rect.width <= 0 || rect.height <= 0 {
                continue;
            }
            for line in strokes(rect, area, width) {
                let left = line.x.max(desktop.x);
                let right = line
                    .x
                    .saturating_add(line.width)
                    .min(desktop.x.saturating_add(desktop.width));
                let top = line.y.max(desktop.y);
                let bottom = line
                    .y
                    .saturating_add(line.height)
                    .min(desktop.y.saturating_add(desktop.height));
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
}
impl Preview {
    pub fn visible(&self) -> bool {
        self.display.is_some()
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
        })
    }
    pub fn start(&mut self, hwnd: HWND, cfg: &Config) -> Result<(), String> {
        self.begin(hwnd, cfg, false, None)
    }
    pub fn select(&mut self, hwnd: HWND, name: &str, cfg: &Config) -> Result<(), String> {
        self.begin(hwnd, cfg, false, Some(name.to_lowercase()))
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
        self.follow(point, cfg)
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
    }
    fn color(cfg: &Config) -> u32 {
        if overlay::dark() {
            cfg.outline_dark
        } else {
            cfg.outline.color
        }
    }
    fn rebuild(&mut self, monitor: HMONITOR, point: POINT, cfg: &Config) -> Result<(), String> {
        let display = desktop::display(monitor)?;
        let virtual_area = desktop::desktop();
        self.regions.clear();
        for (name, layout) in &cfg.presets {
            let area = if layout.monitor == 0 || layout.monitor == display.number {
                display.work
            } else {
                desktop::screen(point, layout.monitor)?.work
            };
            let rect = layout.resolve(area, self.frame, virtual_area)?;
            if rect.width > 0 && rect.height > 0 {
                self.regions.push(PresetRegion {
                    name: name.clone(),
                    rect,
                    area,
                });
            }
        }
        self.regions.sort_by(|a, b| a.name.cmp(&b.name));
        let all: Vec<_> = if self.grid {
            (1..=9)
                .map(|cell| (grid_rect(display.full, cell), display.full))
                .collect()
        } else {
            self.regions.iter().map(|r| (r.rect, r.area)).collect()
        };
        let width = (cfg.outline.width * display.dpi / 96).max(1);
        outline(&self.base, &all, virtual_area, width)?;
        self.base.style(Self::color(cfg), cfg.outline.opacity);
        self.highlight
            .style(Self::color(cfg), cfg.highlight_opacity);
        self.display = Some((monitor, display));
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
                    .map(|name| cfg.display_name(name))
                    .unwrap_or("—");
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
            let dpi = self.display.as_ref().unwrap().1.dpi;
            outline(
                &self.highlight,
                &[(region.rect, region.area)],
                desktop::desktop(),
                (cfg.outline.width * dpi / 96).max(1),
            )?;
            // Raise all four edges after the base; overlapping base strokes cannot cover the selection.
            self.highlight.show(desktop::desktop());
            self.label.show(
                cfg.display_name(&region.name),
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
            self.follow(desktop::cursor()?, cfg)?;
        }
        Ok(())
    }
    pub fn follow(&mut self, point: POINT, cfg: &Config) -> Result<(), String> {
        let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
        if self.display.as_ref().is_none_or(|(old, _)| *old != monitor) {
            self.rebuild(monitor, point, cfg)?;
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
            if edge
                || self
                    .selected
                    .is_none_or(|index| !contains(self.regions[index].rect, point))
            {
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
