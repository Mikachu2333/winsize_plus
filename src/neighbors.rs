use crate::{config::Config, desktop, layout::Rect, overlay::Panel};
mod updates;
use std::{
    cell::Cell,
    ptr::from_mut,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*, Graphics::Dwm::*, System::Threading::GetCurrentProcessId,
    UI::WindowsAndMessaging::*,
};

const GAP: i64 = 5;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
}
#[derive(Clone)]
struct Window {
    hwnd: HWND,
    rect: Rect,
    work: Rect,
    movable: bool,
}
#[derive(Clone, Debug)]
struct Link {
    axis: Axis,
    before: usize,
    after: usize,
    coordinate: i64,
    start: i64,
    end: i64,
}
fn axis_rect(rect: Rect, axis: Axis) -> (i64, i64, i64, i64) {
    match axis {
        Axis::X => (
            i64::from(rect.x),
            i64::from(rect.x) + i64::from(rect.width),
            i64::from(rect.y),
            i64::from(rect.y) + i64::from(rect.height),
        ),
        Axis::Y => (
            i64::from(rect.y),
            i64::from(rect.y) + i64::from(rect.height),
            i64::from(rect.x),
            i64::from(rect.x) + i64::from(rect.width),
        ),
    }
}
fn visible_segments(
    windows: &[Window],
    before: usize,
    after: usize,
    axis: Axis,
    span: (i64, i64),
) -> Vec<(i64, i64)> {
    let mut spans = vec![span];
    for (target, far_edge) in [(before, true), (after, false)] {
        let r = axis_rect(windows[target].rect, axis);
        let sample = if far_edge { r.1 - 1 } else { r.0 };
        for (index, blocker) in windows[..target].iter().enumerate() {
            if index == before || index == after {
                continue;
            }
            let b = axis_rect(blocker.rect, axis);
            if sample < b.0 || sample >= b.1 {
                continue;
            }
            spans = spans
                .into_iter()
                .flat_map(|(a, z)| {
                    let mut result = Vec::new();
                    if b.3 <= a || b.2 >= z {
                        result.push((a, z));
                    } else {
                        if a < b.2 {
                            result.push((a, b.2));
                        }
                        if z > b.3 {
                            result.push((b.3, z));
                        }
                    }
                    result
                })
                .collect();
        }
    }
    spans
}
fn links(windows: &[Window]) -> Vec<Link> {
    let mut result = Vec::new();
    for (before, a) in windows.iter().enumerate().filter(|(_, w)| w.movable) {
        for (after, b) in windows
            .iter()
            .enumerate()
            .filter(|(i, w)| *i != before && w.movable)
        {
            for axis in [Axis::X, Axis::Y] {
                let ar = axis_rect(a.rect, axis);
                let br = axis_rect(b.rect, axis);
                if (ar.1 - br.0).abs() > GAP {
                    continue;
                }
                let start = ar.2.max(br.2);
                let end = ar.3.min(br.3);
                let length = match axis {
                    Axis::X => a.work.height.min(b.work.height),
                    Axis::Y => a.work.width.min(b.work.width),
                };
                if (end - start) * 16 <= i64::from(length) {
                    continue;
                }
                // Insets from either side make tiny permitted overlaps count as a shared seam.
                for (start, end) in visible_segments(windows, before, after, axis, (start, end)) {
                    if (end - start) * 16 > i64::from(length) {
                        result.push(Link {
                            axis,
                            before,
                            after,
                            coordinate: (ar.1 + br.0) / 2,
                            start,
                            end,
                        });
                    }
                }
            }
        }
    }
    result
}
fn hit(links: &[Link], point: POINT, radius: i64, movement: Option<POINT>) -> Option<usize> {
    let preferred = movement.map(|d| {
        if i64::from(d.x).abs() >= i64::from(d.y).abs() {
            Axis::X
        } else {
            Axis::Y
        }
    });
    links
        .iter()
        .enumerate()
        .filter_map(|(i, l)| {
            let (across, along) = match l.axis {
                Axis::X => (point.x, point.y),
                Axis::Y => (point.y, point.x),
            };
            let distance = (i64::from(across) - l.coordinate).abs();
            (distance <= radius
                && i64::from(along) >= l.start - radius
                && i64::from(along) <= l.end + radius)
                .then_some((
                    i,
                    (
                        usize::from(preferred.is_some_and(|axis| axis != l.axis)),
                        distance,
                    ),
                ))
        })
        .min_by_key(|(_, key)| *key)
        .map(|(i, _)| i)
}
fn connected(links: &[Link], seed: usize) -> Vec<usize> {
    let anchor = &links[seed];
    let mut result = vec![seed];
    loop {
        let prior = result.len();
        for (i, l) in links.iter().enumerate() {
            if result.contains(&i)
                || l.axis != anchor.axis
                || (l.coordinate - anchor.coordinate).abs() > GAP
            {
                continue;
            }
            if result
                .iter()
                .any(|&j| l.start <= links[j].end + GAP && l.end + GAP >= links[j].start)
            {
                result.push(i);
            }
        }
        if result.len() == prior {
            return result;
        }
    }
}
fn snapshot(cfg: &Config) -> Result<Vec<Window>, String> {
    unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> i32 {
        // SAFETY: EnumWindows keeps this owned vector alive for its synchronous callbacks.
        unsafe {
            (&mut *(data as *mut Vec<HWND>)).push(hwnd);
        }
        1
    }
    let mut handles = Vec::<HWND>::new();
    unsafe {
        desktop::check(EnumWindows(Some(collect), from_mut(&mut handles) as isize))?;
    }
    let mut result = Vec::new();
    for hwnd in handles {
        // SAFETY: window queries validate foreign handles; only copied geometry leaves this loop.
        unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            let mut cloaked = 0u32;
            DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, from_mut(&mut cloaked).cast(), 4);
            if IsWindowVisible(hwnd) == 0
                || IsIconic(hwnd) != 0
                || cloaked != 0
                || pid == GetCurrentProcessId()
                || GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT != 0
            {
                continue;
            }
            let Ok((_, rect)) = desktop::bounds(hwnd) else {
                continue;
            };
            let point = POINT {
                x: rect.x.saturating_add(rect.width / 2),
                y: rect.y.saturating_add(rect.height / 2),
            };
            let work = desktop::screen(point, 0)?.work;
            let movable = IsZoomed(hwnd) == 0
                && GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_THICKFRAME != 0
                && desktop::allowed(hwnd, &cfg.exclusions);
            result.push(Window {
                hwnd,
                rect,
                work,
                movable,
            });
        }
    }
    Ok(result)
}
fn radius(point: POINT) -> i64 {
    desktop::screen(point, 0).map_or(6, |d| i64::from((6 * d.dpi / 96).max(5)))
}

#[derive(Default)]
pub struct Hint {
    windows: Vec<Window>,
    links: Vec<Link>,
    panels: Vec<Panel>,
    refreshed: Option<Instant>,
    shown: Vec<Rect>,
}
impl Hint {
    pub fn hide(&mut self) {
        for p in &self.panels {
            p.hide();
        }
        self.refreshed = None;
        self.shown.clear();
        crate::input::edge_cursor(None);
    }
    pub fn follow(&mut self, point: POINT, cfg: &Config) -> Result<(), String> {
        if !cfg.mouse_enabled || !cfg.linked_resize {
            self.hide();
            return Ok(());
        }
        if self
            .refreshed
            .is_none_or(|t| t.elapsed() >= Duration::from_millis(100))
        {
            self.windows = snapshot(cfg)?;
            self.links = links(&self.windows);
            self.refreshed = Some(Instant::now());
        }
        let Some(seed) = hit(&self.links, point, radius(point), None) else {
            self.show(&[])?;
            crate::input::edge_cursor(None);
            return Ok(());
        };
        let group = connected(&self.links, seed);
        let mut rectangles = group
            .iter()
            .map(|&i| {
                let l = &self.links[i];
                match l.axis {
                    Axis::X => Rect {
                        x: l.coordinate as i32 - 2,
                        y: l.start as i32,
                        width: 4,
                        height: (l.end - l.start) as i32,
                    },
                    Axis::Y => Rect {
                        x: l.start as i32,
                        y: l.coordinate as i32 - 2,
                        width: (l.end - l.start) as i32,
                        height: 4,
                    },
                }
            })
            .collect::<Vec<_>>();
        let mut members = Vec::new();
        for &edge in &group {
            for index in [self.links[edge].before, self.links[edge].after] {
                if !members.contains(&index) {
                    members.push(index);
                }
            }
        }
        for index in members {
            let r = self.windows[index].rect;
            let stroke = 2;
            rectangles.extend([
                Rect {
                    x: r.x,
                    y: r.y,
                    width: r.width,
                    height: stroke,
                },
                Rect {
                    x: r.x,
                    y: r.y + r.height - stroke,
                    width: r.width,
                    height: stroke,
                },
                Rect {
                    x: r.x,
                    y: r.y,
                    width: stroke,
                    height: r.height,
                },
                Rect {
                    x: r.x + r.width - stroke,
                    y: r.y,
                    width: stroke,
                    height: r.height,
                },
            ]);
        }
        self.show(&rectangles)?;
        crate::input::edge_cursor(Some(self.links[seed].axis));
        Ok(())
    }
    fn show(&mut self, rectangles: &[Rect]) -> Result<(), String> {
        if self.shown == rectangles {
            return Ok(());
        }
        while self.panels.len() < rectangles.len() {
            let panel = Panel::new()?;
            panel.style(0xd77800, 220);
            self.panels.push(panel);
        }
        for (index, &rect) in rectangles.iter().enumerate() {
            self.panels[index].show(rect);
        }
        for panel in &self.panels[rectangles.len()..] {
            panel.hide();
        }
        self.shown = rectangles.to_vec();
        Ok(())
    }
}
struct Member {
    window: Window,
    outer: Rect,
    before: bool,
    min: i64,
    max: i64,
}
pub struct Drag {
    axis: Axis,
    members: Vec<Member>,
    origin: POINT,
    last_delta: Cell<Option<i64>>,
    updates: updates::Updates,
}
impl Drag {
    pub fn start(origin: POINT, point: POINT, cfg: &Config) -> Result<Option<Self>, String> {
        let windows = snapshot(cfg)?;
        let links = links(&windows);
        let movement = POINT {
            x: point.x.saturating_sub(origin.x),
            y: point.y.saturating_sub(origin.y),
        };
        let Some(seed) = hit(&links, origin, radius(origin), Some(movement)) else {
            return Ok(None);
        };
        let axis = links[seed].axis;
        let mut sides = Vec::new();
        for i in connected(&links, seed) {
            for pair in [(links[i].before, true), (links[i].after, false)] {
                if !sides.contains(&pair) {
                    sides.push(pair);
                }
            }
        }
        let mut members = Vec::new();
        let budget = Instant::now() + Duration::from_millis(200);
        for (index, before) in sides {
            desktop::operations::check_cancelled()?;
            let remaining = budget.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("Resize limit queries timed out / 尺寸约束查询超时".into());
            }
            let window = windows[index].clone();
            let (outer, frame) = desktop::bounds(window.hwnd)?;
            let border = match axis {
                Axis::X => outer.width - frame.width,
                Axis::Y => outer.height - frame.height,
            };
            let mut info = MINMAXINFO::default();
            let mut reply = 0;
            // SAFETY: WM_GETMINMAXINFO is system-marshalled; the stack buffer lives until the bounded send returns.
            unsafe {
                if SendMessageTimeoutW(
                    window.hwnd,
                    WM_GETMINMAXINFO,
                    0,
                    from_mut(&mut info) as isize,
                    SMTO_ABORTIFHUNG,
                    remaining.as_millis().clamp(1, 100) as u32,
                    &mut reply,
                ) == 0
                {
                    return Err(cfg
                        .tr(
                            "Window did not provide resize limits",
                            "窗口未响应尺寸约束查询",
                        )
                        .into());
                }
            }
            let (min, max) = match axis {
                Axis::X => (info.ptMinTrackSize.x, info.ptMaxTrackSize.x),
                Axis::Y => (info.ptMinTrackSize.y, info.ptMaxTrackSize.y),
            };
            members.push(Member {
                window,
                outer,
                before,
                min: i64::from((min - border).max(1)),
                max: if max > 0 {
                    i64::from((max - border).max(1))
                } else {
                    i64::from(i32::MAX)
                },
            });
        }
        Ok(Some(Self {
            axis,
            members,
            origin,
            last_delta: Cell::new(None),
            updates: updates::Updates::new()?,
        }))
    }
    fn delta(&self, point: POINT) -> i64 {
        let wanted = match self.axis {
            Axis::X => i64::from(point.x) - i64::from(self.origin.x),
            Axis::Y => i64::from(point.y) - i64::from(self.origin.y),
        };
        let (mut low, mut high) = (i64::from(i32::MIN), i64::from(i32::MAX));
        for member in &self.members {
            let rect = axis_rect(member.window.rect, self.axis);
            let size = rect.1 - rect.0;
            let (a, b) = if member.before {
                (member.min - size, member.max - size)
            } else {
                (size - member.max, size - member.min)
            };
            low = low.max(a);
            high = high.min(b);
        }
        if low > high {
            0
        } else {
            wanted.clamp(low, high)
        }
    }
    pub fn follow(&self, point: POINT) -> Result<(), String> {
        self.updates.status()?;
        let delta = self.delta(point);
        if self.last_delta.get() == Some(delta) {
            return Ok(());
        }
        let wanted = self
            .members
            .iter()
            .map(|m| {
                desktop::frame_position(
                    resize(m.window.rect, self.axis, m.before, delta),
                    m.outer,
                    m.window.rect,
                )
                .map(|rect| (m.window.hwnd as usize, rect))
            })
            .collect::<Result<Vec<_>, String>>()?;
        self.updates.submit(wanted)?;
        self.last_delta.set(Some(delta));
        Ok(())
    }
    pub fn cancel(&self) -> Result<(), String> {
        self.last_delta.set(None);
        self.updates.replace(
            self.members
                .iter()
                .map(|m| (m.window.hwnd as usize, m.outer))
                .collect::<Vec<_>>(),
        )
    }
    pub fn pending(&self) -> bool {
        self.updates.pending()
    }
    pub fn status(&self) -> Result<(), String> {
        self.updates.status()
    }
}
fn resize(mut rect: Rect, axis: Axis, before: bool, delta: i64) -> Rect {
    let change = if before { delta } else { -delta };
    match axis {
        Axis::X => {
            rect.width = (i64::from(rect.width) + change) as i32;
            if !before {
                rect.x = (i64::from(rect.x) + delta) as i32;
            }
        }
        Axis::Y => {
            rect.height = (i64::from(rect.height) + change) as i32;
            if !before {
                rect.y = (i64::from(rect.y) + delta) as i32;
            }
        }
    }
    rect
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::null_mut;
    use windows_sys::Win32::Graphics::Gdi::{GetUpdateRect, UpdateWindow, ValidateRect};
    fn window(x: i32, y: i32, width: i32, height: i32) -> Window {
        Window {
            hwnd: null_mut(),
            rect: Rect {
                x,
                y,
                width,
                height,
            },
            work: Rect {
                x: 0,
                y: 0,
                width: 1600,
                height: 900,
            },
            movable: true,
        }
    }
    #[test]
    #[ignore = "shows temporary shared-edge highlights to verify unchanged geometry does not invalidate windows"]
    fn unchanged_shared_edge_does_not_repaint() {
        let _overlay = crate::overlay::Overlay::new().unwrap();
        let mut hint = Hint::default();
        let rect = Rect {
            x: 100,
            y: 100,
            width: 4,
            height: 300,
        };
        hint.show(&[rect]).unwrap();
        unsafe {
            UpdateWindow(hint.panels[0].0);
            ValidateRect(hint.panels[0].0, std::ptr::null());
        }
        hint.show(&[rect]).unwrap();
        assert_eq!(
            unsafe { GetUpdateRect(hint.panels[0].0, std::ptr::null_mut(), 0) },
            0
        );
        hint.show(&[]).unwrap();
        assert_eq!(unsafe { IsWindowVisible(hint.panels[0].0) }, 0);
    }

    #[test]
    fn t_junction_expands_only_along_chosen_axis() {
        let windows = [
            window(0, 0, 1000, 400),
            window(0, 400, 500, 400),
            window(500, 400, 500, 400),
        ];
        let edges = links(&windows);
        let point = POINT { x: 500, y: 400 };
        let horizontal = hit(&edges, point, 6, Some(POINT { x: 1, y: 20 })).unwrap();
        let vertical = hit(&edges, point, 6, Some(POINT { x: 20, y: 1 })).unwrap();
        assert_eq!(edges[horizontal].axis, Axis::Y);
        assert_eq!(connected(&edges, horizontal).len(), 2);
        assert_eq!(edges[vertical].axis, Axis::X);
        assert_eq!(connected(&edges, vertical).len(), 1);
    }
    #[test]
    fn adjacency_accepts_five_pixel_gaps_but_rejects_short_contacts_and_occlusion() {
        for gap in -6i32..=6 {
            let windows = [window(0, 0, 500, 400), window(500 + gap, 0, 500, 400)];
            assert_eq!(links(&windows).len(), usize::from(gap.abs() <= 5));
        }
        assert!(links(&[window(0, 0, 500, 400), window(500, 350, 500, 400)]).is_empty());
        let mut cover = window(450, 0, 100, 400);
        cover.movable = false;
        assert!(links(&[cover, window(0, 0, 500, 400), window(500, 0, 500, 400)]).is_empty());
    }
    #[test]
    fn connected_edges_do_not_absorb_disconnected_collinear_windows() {
        let windows = [
            window(0, 0, 400, 200),
            window(400, 0, 400, 200),
            window(0, 300, 400, 200),
            window(400, 300, 400, 200),
        ];
        let edges = links(&windows);
        let seed = hit(&edges, POINT { x: 400, y: 100 }, 6, None).unwrap();
        assert_eq!(connected(&edges, seed).len(), 1);
        // More than 1/16, not greater than or equal: 900/16 = 56.25.
        assert!(links(&[window(0, 0, 400, 56), window(400, 0, 400, 56)]).is_empty());
        assert_eq!(
            links(&[window(0, 0, 400, 57), window(400, 0, 400, 57)]).len(),
            1
        );
    }

    #[test]
    #[ignore = "helper process owns temporary windows for linked resizing"]
    fn linked_target_process() {
        let Some(id) = std::env::var_os("WINSIZEP_LINKED_TEST") else {
            return;
        };
        let _dpi = crate::test_support::Dpi::new();
        unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
            unsafe {
                if msg == WM_APP + 71 {
                    SetPropW(hwnd, windows_sys::core::w!("TestResizeDelay"), wp as HANDLE);
                    SetPropW(
                        hwnd,
                        windows_sys::core::w!("TestResizeCount"),
                        std::ptr::null_mut(),
                    );
                    return 0;
                }
                if msg == WM_WINDOWPOSCHANGING
                    && (*(lp as *const WINDOWPOS)).flags & SWP_NOSIZE == 0
                {
                    let count = GetPropW(hwnd, windows_sys::core::w!("TestResizeCount")) as usize;
                    SetPropW(
                        hwnd,
                        windows_sys::core::w!("TestResizeCount"),
                        (count + 1) as HANDLE,
                    );
                    let delay = GetPropW(hwnd, windows_sys::core::w!("TestResizeDelay")) as usize;
                    if delay != 0 {
                        std::thread::sleep(Duration::from_millis(delay as u64));
                    }
                }
                if msg == WM_GETMINMAXINFO {
                    let delay = GetPropW(hwnd, windows_sys::core::w!("TestLimitsDelay")) as usize;
                    if delay != 0 {
                        std::thread::sleep(Duration::from_millis(delay as u64));
                    }
                    let info = &mut *(lp as *mut MINMAXINFO);
                    info.ptMinTrackSize = POINT { x: 100, y: 100 };
                    info.ptMaxTrackSize = POINT { x: 10000, y: 10000 };
                    return 0;
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        let class = windows_sys::core::w!("WinSizeP.LinkedTest");
        let mut windows = Vec::new();
        unsafe {
            let instance =
                windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: instance,
                lpszClassName: class,
                ..Default::default()
            });
            for n in 0..3 {
                let title = desktop::wide(format!("{}-{n}", id.to_string_lossy()));
                let hwnd = CreateWindowExW(
                    WS_EX_TOPMOST,
                    class,
                    title.as_ptr(),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    100 + n * 100,
                    100,
                    300,
                    200,
                    null_mut(),
                    null_mut(),
                    instance,
                    std::ptr::null(),
                );
                assert!(!hwnd.is_null());
                let delay = std::env::var("WINSIZEP_LINKED_DELAY")
                    .ok()
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                SetPropW(
                    hwnd,
                    windows_sys::core::w!("TestLimitsDelay"),
                    delay as HANDLE,
                );
                windows.push(hwnd);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline && windows.iter().any(|&h| unsafe { IsWindow(h) != 0 }) {
            crate::test_support::pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        for hwnd in windows {
            unsafe {
                if IsWindow(hwnd) != 0 {
                    DestroyWindow(hwnd);
                }
            }
        }
    }

    #[test]
    #[ignore = "creates three temporary foreign windows and verifies linked resizing and Escape restoration"]
    fn real_t_junction_resizes_three_windows_and_restores_them() {
        use std::{
            os::windows::process::CommandExt,
            process::{Command, Stdio},
        };
        let _dpi = crate::test_support::Dpi::new();
        let id = format!("winsizep-linked-{}", std::process::id());
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "neighbors::tests::linked_target_process",
            ])
            .env("WINSIZEP_LINKED_TEST", &id)
            .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let result = std::panic::catch_unwind(|| {
            let mut handles = Vec::new();
            let until = Instant::now() + Duration::from_secs(5);
            for n in 0..3 {
                loop {
                    let hwnd = unsafe {
                        FindWindowW(
                            windows_sys::core::w!("WinSizeP.LinkedTest"),
                            desktop::wide(format!("{id}-{n}")).as_ptr(),
                        )
                    };
                    if !hwnd.is_null() {
                        handles.push(hwnd);
                        break;
                    }
                    assert!(Instant::now() < until, "target did not start");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            let work = desktop::screen(desktop::cursor().unwrap(), 0).unwrap().work;
            let x = work.x + 40;
            let y = work.y + 40;
            let w = (work.width * 2 / 3) / 2 * 2;
            let h = work.height / 3;
            let original = [
                Rect {
                    x,
                    y,
                    width: w,
                    height: h,
                },
                Rect {
                    x,
                    y: y + h,
                    width: w / 2,
                    height: h,
                },
                Rect {
                    x: x + w / 2,
                    y: y + h,
                    width: w / 2,
                    height: h,
                },
            ];
            let placements: Vec<_> = handles.iter().copied().zip(original).collect();
            fn settle(placements: &[(HWND, Rect)]) {
                let end = Instant::now() + Duration::from_secs(3);
                while !placements
                    .iter()
                    .all(|&(h, r)| desktop::bounds(h).is_ok_and(|(_, actual)| actual == r))
                {
                    assert!(
                        Instant::now() < end,
                        "geometry did not settle: {:?}",
                        placements
                            .iter()
                            .map(|&(h, r)| (r, desktop::bounds(h)))
                            .collect::<Vec<_>>()
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            desktop::set_frames(&placements).unwrap();
            settle(&placements);
            let cfg = crate::test_support::cfg();
            let origin = POINT {
                x: x + w / 4,
                y: y + h,
            };
            let point = POINT {
                x: origin.x,
                y: origin.y + 20,
            };
            let started = Instant::now();
            let drag = Drag::start(origin, point, &cfg)
                .unwrap()
                .expect("horizontal seam");
            println!(
                "linked start members={} elapsed_ms={}",
                drag.members.len(),
                started.elapsed().as_millis()
            );
            assert_eq!(drag.members.len(), 3);
            drag.follow(point).unwrap();
            let expected: Vec<_> = handles
                .iter()
                .copied()
                .enumerate()
                .map(|(i, hwnd)| (hwnd, resize(original[i], Axis::Y, i == 0, 20)))
                .collect();
            settle(&expected);
            drag.cancel().unwrap();
            settle(&placements);
            let origin = POINT {
                x: x + w / 2,
                y: y + h + h / 2,
            };
            let point = POINT {
                x: origin.x + 20,
                y: origin.y,
            };
            let drag = Drag::start(origin, point, &cfg)
                .unwrap()
                .expect("vertical seam");
            assert_eq!(drag.members.len(), 2);
            drag.follow(point).unwrap();
            settle(&[
                (handles[0], original[0]),
                (handles[1], resize(original[1], Axis::X, true, 20)),
                (handles[2], resize(original[2], Axis::X, false, 20)),
            ]);
            drag.cancel().unwrap();
            settle(&placements);
            let mut single_cfg = crate::test_support::cfg();
            // A slow passive window must see the newest rectangle, not a backlog
            // of every intermediate mouse position queued by SWP_ASYNCWINDOWPOS.
            unsafe {
                SendMessageW(handles[2], WM_APP + 71, 40, 0);
            }
            let start = Instant::now();
            for delta in 1..=40 {
                drag.follow(POINT {
                    x: origin.x + delta,
                    y: origin.y,
                })
                .unwrap();
                std::thread::sleep(Duration::from_millis(5));
            }
            let issued = start.elapsed();
            let latest = [
                (handles[0], original[0]),
                (handles[1], resize(original[1], Axis::X, true, 40)),
                (handles[2], resize(original[2], Axis::X, false, 40)),
            ];
            let final_started = Instant::now();
            settle(&latest);
            let catchup = final_started.elapsed();
            let count =
                unsafe { GetPropW(handles[2], windows_sys::core::w!("TestResizeCount")) as usize };
            println!(
                "slow linked drag: issued_ms={} catchup_ms={} passive_resize_count={count}",
                issued.as_millis(),
                catchup.as_millis()
            );
            assert!(
                count < 20,
                "outdated rectangles were not coalesced: {count}"
            );
            // Cancellation replaces an in-flight backlog, and dropping the drag
            // still lets its final restore complete without a later stale move.
            for delta in 41..=65 {
                drag.follow(POINT {
                    x: origin.x + delta,
                    y: origin.y,
                })
                .unwrap();
            }
            drag.cancel().unwrap();
            drop(drag);
            settle(&placements);
            unsafe {
                SendMessageW(handles[2], WM_APP + 71, 0, 0);
            }
            assert!(
                catchup < Duration::from_millis(600),
                "passive window drained stale resize requests: {catchup:?}"
            );
            single_cfg.linked_resize = false;
            for resizing in [false, true] {
                let origin = POINT {
                    x: x + w / 2,
                    y: y + h + h / 2,
                };
                let single =
                    desktop::Drag::start(handles[1], origin, resizing, &single_cfg).unwrap();
                single
                    .follow(POINT {
                        x: origin.x + 20,
                        y: origin.y + 20,
                    })
                    .unwrap();
                let mut expected = original[1];
                if resizing {
                    expected.width += 20;
                    expected.height += 20;
                } else {
                    expected.x += 20;
                    expected.y += 20;
                }
                settle(&[(handles[1], expected), (handles[2], original[2])]);
                single.cancel().unwrap();
                settle(&placements);
            }
            // The resident returns immediately while the foreign UI thread queries
            // its constraints, and still applies the last point when released early.
            let operations = desktop::operations::Operations::new().unwrap();
            let start = Instant::now();
            let pending = operations.start_drag(handles[1], origin, false, cfg.clone());
            assert!(start.elapsed() < Duration::from_millis(40));
            pending
                .follow(POINT {
                    x: origin.x + 30,
                    y: origin.y,
                })
                .unwrap();
            drop(pending);
            let end = Instant::now() + Duration::from_secs(3);
            while operations.pending() {
                assert!(Instant::now() < end);
                operations.poll().unwrap();
                std::thread::sleep(Duration::from_millis(5));
            }
            operations.poll().unwrap();
            settle(&[
                (handles[0], original[0]),
                (handles[1], resize(original[1], Axis::X, true, 30)),
                (handles[2], resize(original[2], Axis::X, false, 30)),
            ]);
            desktop::set_frames(&placements).unwrap();
            settle(&placements);
            let pending = operations.start_drag(handles[1], origin, false, cfg.clone());
            pending
                .follow(POINT {
                    x: origin.x + 60,
                    y: origin.y,
                })
                .unwrap();
            pending.cancel().unwrap();
            drop(pending);
            std::thread::sleep(Duration::from_millis(250));
            settle(&placements);
            unsafe {
                SendMessageW(handles[2], WM_APP + 71, 300, 0);
            }
            let started = Instant::now();
            operations.apply(
                handles[2],
                crate::cli::Request {
                    x: Some(original[2].x),
                    y: Some(original[2].y),
                    size: Some((original[2].width - 10, original[2].height)),
                    ..Default::default()
                },
                cfg.clone(),
                std::path::PathBuf::new(),
                origin,
            );
            let dispatch = started.elapsed();
            assert!(
                dispatch < Duration::from_millis(40),
                "UI blocked during apply: {dispatch:?}"
            );
            let until = Instant::now() + Duration::from_secs(3);
            while operations.pending() {
                assert!(Instant::now() < until);
                operations.poll().unwrap();
                std::thread::sleep(Duration::from_millis(5));
            }
            operations.poll().unwrap();
            println!(
                "async apply dispatch_us={} completion_ms={}",
                dispatch.as_micros(),
                started.elapsed().as_millis()
            );
            assert_eq!(
                desktop::bounds(handles[2]).unwrap().1.width,
                original[2].width - 10
            );
            unsafe {
                SendMessageW(handles[2], WM_APP + 71, 0, 0);
            }
            desktop::set_frames(&placements).unwrap();
            settle(&placements);
            for hwnd in handles {
                unsafe {
                    PostMessageW(hwnd, WM_CLOSE, 0, 0);
                }
            }
        });
        let _ = child.kill();
        let _ = child.wait();
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
    #[test]
    fn group_clamps_at_smallest_available_range_and_keeps_external_edges() {
        let drag = Drag {
            axis: Axis::X,
            origin: POINT { x: 500, y: 0 },
            last_delta: Cell::new(None),
            updates: updates::Updates::new().unwrap(),
            members: vec![
                Member {
                    outer: Rect::default(),
                    window: window(0, 0, 500, 400),
                    before: true,
                    min: 100,
                    max: 1000,
                },
                Member {
                    outer: Rect::default(),
                    window: window(503, 0, 500, 400),
                    before: false,
                    min: 300,
                    max: 800,
                },
            ],
        };
        let delta = drag.delta(POINT { x: 900, y: 900 });
        assert_eq!(delta, 200);
        let left = resize(drag.members[0].window.rect, Axis::X, true, delta);
        let right = resize(drag.members[1].window.rect, Axis::X, false, delta);
        assert_eq!(left.x, 0);
        assert_eq!(right.x + right.width, 1003);
        assert_eq!(right.x - left.x - left.width, 3);
        assert_eq!(drag.delta(POINT { x: 0, y: 0 }), -300);
    }
    #[test]
    #[ignore]
    fn slow_process_does_not_block_other_windows_or_supersede_new_sessions() {
        use std::{
            os::windows::process::CommandExt,
            process::{Command, Stdio},
        };
        let spawn = |id: &str| {
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "neighbors::tests::linked_target_process",
                ])
                .env("WINSIZEP_LINKED_TEST", id)
                .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        };
        let slow_id = format!("audit-slow-{}", std::process::id());
        let fast_id = format!("audit-fast-{}", std::process::id());
        let mut slow = spawn(&slow_id);
        let mut fast = spawn(&fast_id);
        let result = std::panic::catch_unwind(|| {
            let lookup = |id: &str| {
                let end = Instant::now() + Duration::from_secs(5);
                loop {
                    let h = unsafe {
                        FindWindowW(
                            windows_sys::core::w!("WinSizeP.LinkedTest"),
                            desktop::wide(format!("{id}-0")).as_ptr(),
                        )
                    };
                    if !h.is_null() {
                        break h;
                    }
                    assert!(Instant::now() < end);
                    std::thread::sleep(Duration::from_millis(10));
                }
            };
            let slow_hwnd = lookup(&slow_id);
            let fast_hwnd = lookup(&fast_id);
            let slow_rect = desktop::bounds(slow_hwnd).unwrap().0;
            let fast_rect = desktop::bounds(fast_hwnd).unwrap().0;
            unsafe {
                SendMessageW(slow_hwnd, WM_APP + 71, 1500, 0);
            }
            let updates = updates::Updates::new().unwrap();
            updates
                .submit(vec![
                    (
                        slow_hwnd as usize,
                        Rect {
                            width: slow_rect.width + 20,
                            ..slow_rect
                        },
                    ),
                    (
                        fast_hwnd as usize,
                        Rect {
                            width: fast_rect.width + 20,
                            ..fast_rect
                        },
                    ),
                ])
                .unwrap();
            std::thread::sleep(Duration::from_millis(400));
            let fast_after = desktop::bounds(fast_hwnd).unwrap().0;
            assert_eq!(
                fast_after.width,
                fast_rect.width + 20,
                "responsive process waited behind a slow process"
            );
            let newer = updates::Updates::new().unwrap();
            newer
                .submit(vec![(
                    slow_hwnd as usize,
                    Rect {
                        width: slow_rect.width + 80,
                        ..slow_rect
                    },
                )])
                .unwrap();
            updates
                .submit(vec![(slow_hwnd as usize, slow_rect)])
                .unwrap();
            drop(newer);
            drop(updates);
            let end = Instant::now() + Duration::from_secs(3);
            while desktop::bounds(slow_hwnd).unwrap().0.width != slow_rect.width + 80
                && Instant::now() < end
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                desktop::bounds(slow_hwnd).unwrap().0.width,
                slow_rect.width + 80,
                "old session overwrote the newer target"
            );
        });
        let _ = slow.kill();
        let _ = slow.wait();
        let _ = fast.kill();
        let _ = fast.wait();
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }
}
