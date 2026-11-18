use crate::{
    config::Config,
    desktop::{self, Display, err},
    layout::Rect,
    overlay::Panel,
};
use windows_sys::Win32::{Foundation::*, Graphics::Gdi::*};

fn strokes(rect: Rect, area: Rect) -> [Rect; 4] {
    let left = rect.x;
    let top = rect.y;
    let right = left.saturating_add(rect.width);
    let bottom = top.saturating_add(rect.height);
    let stroke = |position: i32, end: i32| {
        if position == end {
            position.saturating_sub(2)
        } else {
            position.saturating_sub(1)
        }
    };
    let vertical = |edge| Rect {
        x: stroke(edge, area.x + area.width).max(area.x),
        y: top,
        width: 2,
        height: rect.height,
    };
    let horizontal = |edge| Rect {
        x: left,
        y: stroke(edge, area.y + area.height).max(area.y),
        width: rect.width,
        height: 2,
    };
    [
        vertical(left),
        vertical(right),
        horizontal(top),
        horizontal(bottom),
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

fn outline(panel: &Panel, rectangles: &[(Rect, Rect)], desktop: Rect) -> Result<(), String> {
    let shape = Region::new()?;
    let strip = Region::new()?;
    unsafe {
        for &(rect, area) in rectangles {
            if rect.width <= 0 || rect.height <= 0 {
                continue;
            }
            for line in strokes(rect, area) {
                let left = line.x.max(desktop.x);
                let right = line
                    .x
                    .saturating_add(line.width)
                    .min(desktop.x + desktop.width);
                let top = line.y.max(desktop.y);
                let bottom = line
                    .y
                    .saturating_add(line.height)
                    .min(desktop.y + desktop.height);
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
        std::mem::forget(shape);
    }
    Ok(())
}

pub struct Preview {
    base: Panel,
    highlight: Panel,
    frame: Rect,
    display: Option<(HMONITOR, Display)>,
    highlighted: bool,
}
impl Preview {
    pub fn visible(&self) -> bool {
        self.display.is_some()
    }
    pub fn new() -> Result<Self, String> {
        let base = Panel::new()?;
        let highlight = Panel::new()?;
        base.style(0x887766, 175);
        highlight.style(0xe09015, 255);
        Ok(Self {
            base,
            highlight,
            frame: Rect::default(),
            display: None,
            highlighted: false,
        })
    }
    pub fn start(&mut self, hwnd: HWND, cfg: &Config) -> Result<(), String> {
        self.hide();
        let point = desktop::cursor()?;
        self.frame = desktop::bounds(hwnd)
            .map(|(_, frame)| frame)
            .unwrap_or(desktop::screen(point, 0)?.work);
        self.follow(point, cfg)
    }
    pub fn hide(&mut self) {
        self.base.hide();
        self.highlight.hide();
        self.display = None;
        self.highlighted = false;
    }
    pub fn follow(&mut self, point: POINT, cfg: &Config) -> Result<(), String> {
        let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
        if self.display.as_ref().is_none_or(|(old, _)| *old != monitor) {
            let display = desktop::display(monitor)?;
            let virtual_area = desktop::desktop();
            let mut all = Vec::with_capacity(cfg.presets.len());
            let mut center = Vec::new();
            for (name, layout) in &cfg.presets {
                let area = if layout.monitor == 0 || layout.monitor == display.number {
                    display.work
                } else {
                    desktop::screen(point, layout.monitor)?.work
                };
                let rect = layout.resolve(area, self.frame, virtual_area)?;
                all.push((rect, area));
                if name == "center" {
                    center.push((rect, area));
                }
            }
            outline(&self.base, &all, virtual_area)?;
            outline(&self.highlight, &center, virtual_area)?;
            self.highlight.hide();
            self.highlighted = false;
            self.base.show(virtual_area);
            self.display = Some((monitor, display));
        }
        if let Some((_, screen)) = &self.display {
            let center = cfg.cell(screen.number, screen.work.grid(point.x, point.y) as u32 + 1)
                == Some("center");
            if center != self.highlighted {
                if center {
                    self.highlight.show(desktop::desktop());
                } else {
                    self.highlight.hide();
                }
                self.highlighted = center;
            }
        }
        Ok(())
    }
}
