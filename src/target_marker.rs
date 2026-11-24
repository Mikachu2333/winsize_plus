//! Four narrow layered strips mark the captured target without a full-window bitmap.
use crate::{
    desktop::{self, err},
    layout::Rect,
    overlay::Panel,
};
use std::ptr::null_mut;
use windows_sys::Win32::{Foundation::*, UI::WindowsAndMessaging::*};

fn edges(rect: Rect) -> [Rect; 4] {
    let width = 4.min(rect.width.max(0)).min(rect.height.max(0));
    let middle = (rect.height - width * 2).max(0);
    let bottom = width.min(rect.height - width);
    let right = width.min(rect.width - width);
    [
        Rect {
            height: width,
            ..rect
        },
        Rect {
            y: rect.y + rect.height - bottom,
            height: bottom,
            ..rect
        },
        Rect {
            y: rect.y + width,
            width,
            height: middle,
            ..rect
        },
        Rect {
            x: rect.x + rect.width - right,
            y: rect.y + width,
            width: right,
            height: middle,
            ..rect
        },
    ]
}
#[derive(Default)]
pub struct TargetMarker {
    panels: Option<[Panel; 4]>,
    target: HWND,
    rect: Option<Rect>,
    topmost: bool,
}
impl TargetMarker {
    pub fn hide(&mut self) {
        if self.rect.take().is_some()
            && let Some(panels) = &self.panels
        {
            for panel in panels {
                unsafe {
                    ShowWindow(panel.0, SW_HIDE);
                }
            }
        }
        self.target = null_mut();
    }
    pub fn show(&mut self, target: HWND) -> Result<(), String> {
        // SAFETY: window-manager queries validate external HWNDs; no foreign memory is read.
        if unsafe { IsWindowVisible(target) == 0 || IsIconic(target) != 0 } {
            self.hide();
            return Ok(());
        }
        let rect = desktop::bounds(target)?.1;
        if rect.width <= 0 || rect.height <= 0 {
            self.hide();
            return Ok(());
        }
        if self.panels.is_none() {
            let panels = [Panel::new()?, Panel::new()?, Panel::new()?, Panel::new()?];
            for panel in &panels {
                panel.style(crate::colors::colorref(0xffff3c), 204);
            }
            self.panels = Some(panels);
        }
        let panels = self.panels.as_ref().unwrap();
        let topmost = desktop::topmost(target)?;
        let prior = unsafe { GetWindow(target, GW_HWNDPREV) };
        if self.target == target
            && self.rect == Some(rect)
            && self.topmost == topmost
            && prior == panels[3].0
        {
            return Ok(());
        }
        // Match the target's normal/topmost group before showing, then insert the
        // strips immediately above it. Existing preview windows retain their order.
        unsafe {
            if self.rect.is_none() || self.topmost != topmost {
                for panel in panels {
                    desktop::check(SetWindowPos(
                        panel.0,
                        if topmost {
                            HWND_TOPMOST
                        } else {
                            HWND_NOTOPMOST
                        },
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                    ))?;
                }
            }
            let mut anchor = GetWindow(target, GW_HWNDPREV);
            while panels.iter().any(|p| p.0 == anchor) {
                anchor = GetWindow(anchor, GW_HWNDPREV);
            }
            let mut batch = BeginDeferWindowPos(4);
            if batch.is_null() {
                return Err(err());
            }
            for (panel, edge) in panels.iter().zip(edges(rect)) {
                batch = DeferWindowPos(
                    batch,
                    panel.0,
                    anchor,
                    edge.x,
                    edge.y,
                    edge.width.max(1),
                    edge.height.max(1),
                    SWP_NOACTIVATE
                        | SWP_NOOWNERZORDER
                        | if edge.width > 0 && edge.height > 0 {
                            SWP_SHOWWINDOW
                        } else {
                            SWP_HIDEWINDOW
                        },
                );
                if batch.is_null() {
                    return Err(err());
                }
                anchor = panel.0;
            }
            desktop::check(EndDeferWindowPos(batch))?;
        }
        self.target = target;
        self.rect = Some(rect);
        self.topmost = topmost;
        Ok(())
    }
    pub fn follow(&mut self) -> Result<(), String> {
        if !self.target.is_null() {
            self.show(self.target)?;
        }
        Ok(())
    }
    #[cfg(test)]
    pub fn visible(&self) -> bool {
        self.rect.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "opens temporary windows to verify marker layering, transparency and rendering cost"]
    fn marker_layering_transparency_and_cached_follow_cost() {
        use windows_sys::{Win32::System::Threading::*, core::w};
        let _dpi = crate::test_support::Dpi::new();
        let _overlay = crate::overlay::Overlay::new().unwrap();
        let work = desktop::screen(desktop::cursor().unwrap(), 0).unwrap().work;
        unsafe {
            let target = CreateWindowExW(
                0,
                w!("STATIC"),
                w!("WinSizeP temporary marker test"),
                WS_OVERLAPPEDWINDOW,
                work.x + 20,
                work.y + 20,
                600,
                400,
                null_mut(),
                null_mut(),
                null_mut(),
                std::ptr::null(),
            );
            assert!(!target.is_null());
            ShowWindow(target, SW_SHOWNOACTIVATE);
            SetWindowPos(
                target,
                HWND_TOP,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            let window = Panel::new().unwrap();
            window.style(0xff0000, 100);
            window.show(Rect {
                x: work.x + 10,
                y: work.y + 10,
                width: 10,
                height: 10,
            });
            let foreground = GetForegroundWindow();
            let mut marker = TargetMarker::default();
            for top in [false, true, false] {
                SetWindowPos(
                    target,
                    if top { HWND_TOPMOST } else { HWND_NOTOPMOST },
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                // Put the test preview above the target before adding its marker.
                SetWindowPos(
                    window.0,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                let frame = desktop::bounds(target).unwrap().1;
                let hit = POINT {
                    x: frame.x + frame.width / 2,
                    y: frame.y + 2,
                };
                let expected_hit = WindowFromPoint(hit);
                marker.show(target).unwrap();
                assert!(marker.visible());
                let panels = marker.panels.as_ref().unwrap();
                assert_eq!(GetWindow(target, GW_HWNDPREV), panels[3].0);
                for panel in panels {
                    assert_eq!(desktop::topmost(panel.0).unwrap(), top);
                    assert_eq!(
                        GetWindowLongW(panel.0, GWL_EXSTYLE) as u32
                            & (WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE),
                        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE
                    );
                    let mut key = 0;
                    let mut alpha = 0;
                    let mut flags = 0;
                    assert_ne!(
                        GetLayeredWindowAttributes(panel.0, &mut key, &mut alpha, &mut flags),
                        0
                    );
                    assert_eq!((alpha, flags), (204, LWA_ALPHA));
                }
                assert_eq!(GetForegroundWindow(), foreground);
                assert_eq!(WindowFromPoint(hit), expected_hit);
                assert!(!panels.iter().any(|p| p.0 == WindowFromPoint(hit)));
            }
            let resources = GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS);
            let cpu = || {
                let mut creation = FILETIME::default();
                let mut exit = FILETIME::default();
                let mut kernel = FILETIME::default();
                let mut user = FILETIME::default();
                GetThreadTimes(
                    GetCurrentThread(),
                    &mut creation,
                    &mut exit,
                    &mut kernel,
                    &mut user,
                );
                let ticks =
                    |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
                ticks(kernel) + ticks(user)
            };
            let cpu_start = cpu();
            let start = std::time::Instant::now();
            for _ in 0..20000 {
                marker.follow().unwrap();
            }
            let per_call = start.elapsed().as_secs_f64() / 20000.0;
            let cpu_per_call = (cpu() - cpu_start) as f64 / 10_000_000.0 / 20000.0;
            println!(
                "cached_follow_us={:.3}; CPU at 10 checks/s={:.5}% of one core; extra GDI objects={}",
                per_call * 1e6,
                cpu_per_call * 1000.0,
                GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) as i32 - resources as i32
            );
            assert_eq!(
                GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS),
                resources
            );
            marker.hide();
            assert!(!marker.visible());
            assert!(
                marker
                    .panels
                    .as_ref()
                    .unwrap()
                    .iter()
                    .all(|p| IsWindowVisible(p.0) == 0)
            );
            DestroyWindow(target);
        }
    }
    #[test]
    fn strips_stay_inside_and_corners_are_not_double_blended() {
        for (width, height) in [(800, 600), (4, 4), (2, 3), (10, 7)] {
            let rect = Rect {
                x: -100,
                y: 25,
                width,
                height,
            };
            let edges = edges(rect);
            for y in rect.y..rect.y + height {
                for x in rect.x..rect.x + width {
                    let count = edges
                        .iter()
                        .filter(|r| x >= r.x && y >= r.y && x < r.x + r.width && y < r.y + r.height)
                        .count();
                    assert_eq!(
                        count,
                        usize::from(
                            x < rect.x + 4
                                || x >= rect.x + width - 4
                                || y < rect.y + 4
                                || y >= rect.y + height - 4
                        )
                    );
                }
            }
        }
    }
}
