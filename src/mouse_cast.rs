//! Fixed-size input shapes. State is merged per button/axis; pixels and HWNDs are reused.
use crate::{
    cast::FADE_OUT,
    config::MouseStyle,
    desktop::{self, Display},
    label_text::Image,
    overlay::Label,
};
use std::time::{Duration, Instant};
use windows_sys::Win32::{
    Foundation::POINT,
    Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint},
};

pub const RELEASE: u32 = 1 << 24;
pub const FRAME: Duration = Duration::from_millis(16);
const FADE_IN: Duration = Duration::from_millis(60);
const SCROLL_GAP: Duration = Duration::from_millis(180);
const CLICK_HOLD: Duration = Duration::from_millis(100);
pub const PREVIEW_INTERVAL: Duration = Duration::from_millis(2200);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Left,
    Right,
    Middle,
    Side1,
    Side2,
    Up,
    Down,
    ScrollLeft,
    ScrollRight,
}
const SHAPES: [Shape; 9] = [
    Shape::Left,
    Shape::Right,
    Shape::Middle,
    Shape::Side1,
    Shape::Side2,
    Shape::Up,
    Shape::Down,
    Shape::ScrollLeft,
    Shape::ScrollRight,
];
impl Shape {
    fn input(button: u32) -> Option<(usize, Self)> {
        Some(match button {
            1 => (0, Self::Left),
            2 => (1, Self::Right),
            4 => (2, Self::Middle),
            5 => (3, Self::Side1),
            6 => (4, Self::Side2),
            8 => (5, Self::Up),
            9 => (5, Self::Down),
            10 => (6, Self::ScrollLeft),
            11 => (6, Self::ScrollRight),
            _ => return None,
        })
    }
    fn side_mapping(self, swap: bool) -> Self {
        match (self, swap) {
            (Self::Side1, true) => Self::Side2,
            (Self::Side2, true) => Self::Side1,
            _ => self,
        }
    }
    fn contains(self, x: f32, y: f32) -> bool {
        let circle = x * x + y * y <= 1.0;
        match self {
            Self::Left => circle && x < 0.0,
            Self::Right => circle && x >= 0.0,
            Self::Middle => x.abs() <= 0.22 && (-0.68..=-0.05).contains(&y),
            Self::Side1 => circle && x < -0.70 && y < 0.0,
            Self::Side2 => circle && x < -0.70 && y >= 0.0,
            Self::Up => triangle(x, y),
            Self::Down => triangle(x, -y),
            Self::ScrollLeft => triangle(y, x),
            Self::ScrollRight => triangle(y, -x),
        }
    }
}
fn triangle(x: f32, y: f32) -> bool {
    (-0.9..=0.55).contains(&y) && x.abs() <= (y + 0.9) / 1.45 * 0.72
}

// COLORREF in, COLORREF out. Hue rotation preserves a readable saturation and value
// even for an achromatic custom theme, so buttons cannot collapse to the same color.
fn color(base: u32, shape: Shape) -> u32 {
    let r = (base & 255) as f32 / 255.0;
    let g = ((base >> 8) & 255) as f32 / 255.0;
    let b = ((base >> 16) & 255) as f32 / 255.0;
    let high = r.max(g).max(b);
    let low = r.min(g).min(b);
    let delta = high - low;
    let hue = if delta < 0.001 {
        210.0
    } else if high == r {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if high == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let offset = match shape {
        Shape::Left => 0.0,
        Shape::Right => 180.0,
        Shape::Middle => 120.0,
        Shape::Side1 => 240.0,
        Shape::Side2 => 300.0,
        Shape::Up => 60.0,
        Shape::Down => 270.0,
        Shape::ScrollLeft => 30.0,
        Shape::ScrollRight => 150.0,
    };
    let h = (hue + offset).rem_euclid(360.0) / 60.0;
    let v = high.clamp(0.65, 0.95);
    let s = if high > 0.0 {
        (delta / high).clamp(0.65, 0.9)
    } else {
        0.8
    };
    let chroma = v * s;
    let second = chroma * (1.0 - (h.rem_euclid(2.0) - 1.0).abs());
    let m = v - chroma;
    let (r, g, b) = match h as u8 {
        0 => (chroma, second, 0.0),
        1 => (second, chroma, 0.0),
        2 => (0.0, chroma, second),
        3 => (0.0, second, chroma),
        4 => (second, 0.0, chroma),
        _ => (chroma, 0.0, second),
    };
    ((r + m) * 255.0).round() as u32
        | (((g + m) * 255.0).round() as u32) << 8
        | (((b + m) * 255.0).round() as u32) << 16
}
fn visible_color(base: u32, shape: Shape, dark: bool, alpha: u8) -> u32 {
    let hue = color(base, shape);
    let surface = if dark { 0x202020 } else { 0xfafafa };
    let limit = if dark { 0xffffff } else { 0 };
    // Keep hue differences at ordinary opacity; progressively tune value only as
    // needed for the actual alpha-composited contrast against the active theme.
    let maximum = if dark { 3.0 } else { 2.5 };
    let target = (1.0 + f64::from(alpha) / 160.0 * (maximum - 1.0)).min(maximum);
    for amount in (0..=224).step_by(8) {
        let candidate = crate::colors::composite(limit, hue, amount as u8);
        let blended = crate::colors::composite(candidate, surface, alpha);
        if crate::colors::contrast(blended, surface) >= target {
            return candidate;
        }
    }
    crate::colors::composite(limit, hue, 224)
}
fn pixels(shape: Shape, radius: i32, color: u32) -> Vec<u32> {
    let size = radius * 2 + 2;
    let mut result = vec![0; (size * size) as usize];
    for y in 0..size {
        for x in 0..size {
            let mut inside = 0;
            // Four samples give stable antialiased edges; this runs only when the cache changes.
            for sy in [0.25, 0.75] {
                for sx in [0.25, 0.75] {
                    let px = (x as f32 + sx - size as f32 / 2.0) / radius as f32;
                    let py = (y as f32 + sy - size as f32 / 2.0) / radius as f32;
                    inside += u32::from(shape.contains(px, py));
                }
            }
            let a = inside * 255 / 4;
            let r = (color & 255) * a / 255;
            let g = ((color >> 8) & 255) * a / 255;
            let b = ((color >> 16) & 255) * a / 255;
            result[(y * size + x) as usize] = (a << 24) | (r << 16) | (g << 8) | b;
        }
    }
    result
}

#[derive(Clone, Copy)]
struct Envelope {
    since: Instant,
    from: f32,
    to: f32,
    span: Duration,
    release_at: Option<Instant>,
}
impl Envelope {
    fn new(now: Instant) -> Self {
        Self {
            since: now,
            from: 0.35,
            to: 1.0,
            span: FADE_IN,
            release_at: None,
        }
    }
    fn level(&self, now: Instant) -> f32 {
        let progress = (now.saturating_duration_since(self.since).as_secs_f32()
            / self.span.as_secs_f32())
        .clamp(0.0, 1.0);
        self.from + (self.to - self.from) * progress
    }
    fn press(&mut self, now: Instant) {
        if self.to == 0.0 {
            self.from = self.level(now);
            self.to = 1.0;
            self.since = now;
            self.span = FADE_IN;
        }
        self.release_at = None;
    }
    fn advance(&mut self, now: Instant) {
        if let Some(at) = self.release_at
            && now >= at
        {
            self.from = self.level(at);
            self.to = 0.0;
            self.since = at;
            self.span = FADE_OUT;
            self.release_at = None;
        }
    }
    fn deadline(&self, now: Instant) -> Option<Instant> {
        let end = self.since + self.span;
        if now < end {
            Some((now + FRAME).min(end))
        } else {
            self.release_at
        }
    }
}
struct Slot {
    surface: Label,
    shape: Shape,
    envelope: Option<Envelope>,
    images: Vec<(Shape, i32, u32, Image)>,
    sent: Option<(i32, i32, u8)>,
}
impl Slot {
    fn new(shape: Shape) -> Result<Self, String> {
        Ok(Self {
            surface: Label::new()?,
            shape,
            envelope: None,
            images: Vec::new(),
            sent: None,
        })
    }
    fn hide(&mut self) {
        self.surface.hide();
        self.envelope = None;
        self.sent = None;
    }
}

pub struct MouseVisuals {
    slots: Vec<Slot>,
    point: POINT,
    display: Option<(usize, Display, u64)>,
    last_draw: Option<Instant>,
    next: Option<Instant>,
    dirty: bool,
    palette: Option<((u32, bool, u8), [u32; 9])>,
    preview_refresh: Option<(u32, Instant, Instant)>,
    #[cfg(test)]
    uploads: usize,
    #[cfg(test)]
    builds: usize,
}
impl MouseVisuals {
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            point: POINT::default(),
            display: None,
            last_draw: None,
            next: None,
            dirty: false,
            palette: None,
            preview_refresh: None,
            #[cfg(test)]
            uploads: 0,
            #[cfg(test)]
            builds: 0,
        }
    }
    pub fn clear(&mut self) {
        for slot in &mut self.slots {
            slot.hide();
        }
        self.next = None;
        self.dirty = false;
        self.preview_refresh = None;
    }
    pub fn active(&self) -> bool {
        self.slots.iter().any(|s| s.envelope.is_some())
    }
    pub fn deadline(&self) -> Option<Instant> {
        [self.next, self.preview_refresh.map(|(_, at, _)| at)]
            .into_iter()
            .flatten()
            .min()
    }
    pub fn prepare(&mut self, point: POINT, style: &MouseStyle) -> Result<(), String> {
        if self.slots.is_empty() {
            self.slots = [
                Shape::Left,
                Shape::Right,
                Shape::Middle,
                Shape::Side1,
                Shape::Side2,
                Shape::Up,
                Shape::ScrollLeft,
            ]
            .into_iter()
            .map(Slot::new)
            .collect::<Result<_, _>>()?;
        }
        self.point = point;
        self.refresh_display()?;
        for index in 0..self.slots.len() {
            self.prepare_image(index, style)?;
        }
        Ok(())
    }
    fn refresh_display(&mut self) -> Result<(), String> {
        let monitor = unsafe { MonitorFromPoint(self.point, MONITOR_DEFAULTTONEAREST) };
        let epoch = desktop::display_epoch();
        if self
            .display
            .as_ref()
            .is_none_or(|(old, _, version)| *old != monitor as usize || *version != epoch)
        {
            self.display = Some((monitor as usize, desktop::display(monitor)?, epoch));
        }
        Ok(())
    }
    fn prepare_image(&mut self, index: usize, style: &MouseStyle) -> Result<(), String> {
        let palette_key = (style.color, style.dark, style.opacity);
        if self
            .palette
            .as_ref()
            .is_none_or(|(key, _)| *key != palette_key)
        {
            self.palette = Some((
                palette_key,
                SHAPES.map(|shape| visible_color(style.color, shape, style.dark, style.opacity)),
            ));
        }
        let palette = &self.palette.as_ref().unwrap().1;
        let dpi = self.display.as_ref().unwrap().1.dpi;
        let radius = (style.radius * dpi / 96).max(2);
        let slot = &mut self.slots[index];
        let rgb = palette[slot.shape as usize];
        slot.images
            .retain(|(shape, r, c, _)| *r == radius && *c == palette[*shape as usize]);
        if !slot
            .images
            .iter()
            .any(|(shape, _, _, _)| *shape == slot.shape)
        {
            let size = 2 * radius + 2;
            slot.images.push((
                slot.shape,
                radius,
                rgb,
                Image::pixels(size, size, &pixels(slot.shape, radius, rgb))?,
            ));
            slot.sent = None;
            #[cfg(test)]
            {
                self.builds += 1;
            }
        }
        Ok(())
    }
    pub fn event(
        &mut self,
        button: u32,
        flags: u32,
        point: POINT,
        style: &MouseStyle,
    ) -> Result<(), String> {
        let Some((index, shape)) = Shape::input(button) else {
            return Ok(());
        };
        let shape = shape.side_mapping(style.swap_side_buttons);
        if self.slots.is_empty() {
            self.prepare(point, style)?;
        }
        self.point = point;
        let now = Instant::now();
        let was_active = self.active();
        let slot = &mut self.slots[index];
        if flags & RELEASE != 0 {
            if let Some(envelope) = &mut slot.envelope {
                envelope.release_at = Some(now.max(envelope.since + CLICK_HOLD));
            } else {
                return Ok(());
            }
        } else {
            let envelope = slot.envelope.get_or_insert_with(|| Envelope::new(now));
            envelope.advance(now);
            envelope.press(now);
            if index >= 5 {
                envelope.release_at = Some(now + SCROLL_GAP);
            }
            if slot.shape != shape {
                slot.shape = shape;
                slot.sent = None;
            }
        }
        self.dirty = true;
        // First visibility is immediate; repeated events only merge state until the next frame.
        if !was_active
            || self
                .last_draw
                .is_none_or(|last| now.duration_since(last) >= FRAME)
        {
            self.draw(now, style)
        } else {
            self.schedule(self.last_draw.unwrap() + FRAME);
            Ok(())
        }
    }
    pub fn follow(&mut self, point: POINT, style: &MouseStyle) -> Result<(), String> {
        if !self.active() || (self.point.x, self.point.y) == (point.x, point.y) {
            return Ok(());
        }
        self.point = point;
        self.dirty = true;
        self.tick(style)
    }
    pub fn tick(&mut self, style: &MouseStyle) -> Result<(), String> {
        let now = Instant::now();
        if let Some((button, at, until)) = self.preview_refresh
            && now >= at
        {
            if now < until {
                self.event(button, 0, self.point, style)?;
                self.preview_refresh = Some((button, now + Duration::from_millis(80), until));
            } else {
                self.preview_refresh = None;
            }
        }
        let changed = self
            .display
            .as_ref()
            .is_some_and(|(_, _, epoch)| *epoch != desktop::display_epoch());
        if self.active() && (self.dirty || changed || self.next.is_some_and(|at| now >= at)) {
            if let Some(last) = self.last_draw
                && now.duration_since(last) < FRAME
            {
                self.schedule(last + FRAME);
                return Ok(());
            }
            let result = self.draw(now, style);
            if result.is_err() {
                self.clear();
            }
            result
        } else {
            Ok(())
        }
    }
    fn schedule(&mut self, at: Instant) {
        let now = Instant::now();
        self.next = Some(
            self.next
                .filter(|old| *old > now)
                .map_or(at, |old| old.min(at)),
        );
    }
    fn draw(&mut self, now: Instant, style: &MouseStyle) -> Result<(), String> {
        self.refresh_display()?;
        self.next = None;
        self.dirty = false;
        self.last_draw = Some(now);
        for index in 0..self.slots.len() {
            let Some(mut envelope) = self.slots[index].envelope else {
                continue;
            };
            envelope.advance(now);
            let alpha = (envelope.level(now) * f32::from(style.opacity)).round() as u8;
            if alpha == 0 && envelope.to == 0.0 {
                self.slots[index].hide();
                continue;
            }
            self.slots[index].envelope = Some(envelope);
            self.prepare_image(index, style)?;
            let slot = &mut self.slots[index];
            let image = &slot
                .images
                .iter()
                .find(|(shape, _, _, _)| *shape == slot.shape)
                .unwrap()
                .3;
            let position = (
                self.point.x - image.width / 2,
                self.point.y - image.height / 2,
                alpha,
            );
            if slot.sent != Some(position) {
                slot.surface.image(image, position.0, position.1, alpha)?;
                slot.sent = Some(position);
                #[cfg(test)]
                {
                    self.uploads += 1;
                }
            }
            if let Some(at) = envelope.deadline(now) {
                self.schedule(at);
            }
        }
        Ok(())
    }
    pub fn preview(&mut self, step: usize, point: POINT, style: &MouseStyle) -> Result<(), String> {
        self.clear();
        self.prepare(point, style)?;
        let sequence = [1, 2, 4, 5, 6, 8, 9];
        let button = sequence[step % sequence.len()];
        self.event(button, 0, point, style)?;
        if button < 8 {
            let index = Shape::input(button).unwrap().0;
            self.slots[index].envelope.as_mut().unwrap().release_at =
                Some(Instant::now() + Duration::from_millis(1400));
        } else {
            self.preview_refresh = Some((
                button,
                Instant::now() + Duration::from_millis(80),
                Instant::now() + Duration::from_millis(1400),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn swapping_side_indicators_keeps_button_slots_and_other_shapes() {
        let (x1_slot, x1) = Shape::input(5).unwrap();
        let (x2_slot, x2) = Shape::input(6).unwrap();
        assert_eq!((x1_slot, x2_slot), (3, 4));
        assert!(x1.side_mapping(true).contains(-0.8, 0.1));
        assert!(x2.side_mapping(true).contains(-0.8, -0.1));
        assert!(x1.side_mapping(false).contains(-0.8, -0.1));
        assert!(x2.side_mapping(false).contains(-0.8, 0.1));
        for button in [1, 2, 4, 8, 9, 10, 11] {
            let (_, shape) = Shape::input(button).unwrap();
            assert_eq!(shape.side_mapping(true), shape);
        }
    }
    #[test]
    fn derived_fills_remain_distinct_on_light_and_dark_surfaces() {
        for base in [
            0, 0xffffff, 0x808080, 0xd47800, 0x00ff00, 0xff00ff, 0x00ffff,
        ] {
            for dark in [false, true] {
                let surface = if dark { 0x202020 } else { 0xfafafa };
                for alpha in [96, 128, 160, 224] {
                    let palette = SHAPES.map(|shape| visible_color(base, shape, dark, alpha));
                    for (i, foreground) in palette.into_iter().enumerate() {
                        let composed = crate::colors::composite(foreground, surface, alpha);
                        let contrast = crate::colors::contrast(composed, surface);
                        if alpha >= 160 {
                            assert!(
                                contrast >= 1.99,
                                "base={base:x} dark={dark} alpha={alpha} shape={i} contrast={contrast}"
                            );
                        } else {
                            assert!(contrast > 1.05);
                        }
                    }
                    for pair in [(0, 1), (0, 2), (1, 2), (5, 6)] {
                        let a = crate::colors::composite(palette[pair.0], surface, alpha);
                        let b = crate::colors::composite(palette[pair.1], surface, alpha);
                        let distance = [0, 8, 16]
                            .into_iter()
                            .map(|shift| {
                                (((a >> shift) & 255) as i32 - ((b >> shift) & 255) as i32).abs()
                            })
                            .sum::<i32>();
                        assert!(distance >= 12, "Indistinguishable colors: {a:x}/{b:x}");
                    }
                }
            }
        }
    }
    #[test]
    #[ignore = "exports the actual compact shape pixels over light and dark backgrounds"]
    fn capture_mouse_palettes() {
        let width = 400i32;
        let height = 160i32;
        let mut canvas = vec![0u32; (width * height) as usize];
        for (row, dark) in [false, true].into_iter().enumerate() {
            let background = if dark { 0x202020 } else { 0xfafafa };
            for y in row * 80..(row + 1) * 80 {
                for x in 0..width as usize {
                    canvas[y * width as usize + x] = background;
                }
            }
            for (col, shape) in [
                Shape::Left,
                Shape::Right,
                Shape::Middle,
                Shape::Up,
                Shape::Down,
            ]
            .into_iter()
            .enumerate()
            {
                let rgb = visible_color(crate::colors::accent(), shape, dark, 160);
                let bitmap = pixels(shape, 14, rgb);
                for y in 0..30 {
                    for x in 0..30 {
                        let pixel = bitmap[y * 30 + x];
                        let alpha = pixel >> 24;
                        let channel = |shift: u32| {
                            let foreground = (pixel >> shift) & 255;
                            let base = (background >> shift) & 255;
                            (foreground * 160 / 255 + base * (255 - alpha * 160 / 255) / 255)
                                .min(255)
                        };
                        canvas[(row * 80 + 25 + y) * width as usize + col * 80 + 25 + x] =
                            channel(0) | (channel(8) << 8) | (channel(16) << 16);
                    }
                }
            }
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"BM");
        bytes.extend_from_slice(&(54 + width * height * 4).to_le_bytes());
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&54u32.to_le_bytes());
        bytes.extend_from_slice(&40u32.to_le_bytes());
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&(-height).to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&32u16.to_le_bytes());
        bytes.extend_from_slice(&[0; 24]);
        for pixel in canvas {
            bytes.extend_from_slice(&pixel.to_le_bytes());
        }
        std::fs::create_dir_all("target").unwrap();
        std::fs::write("target/mouse-cast-palettes.bmp", bytes).unwrap();
    }
    #[test]
    fn held_buttons_and_repeated_scroll_do_not_restart_the_fade() {
        let start = Instant::now();
        let mut hold = Envelope::new(start);
        hold.advance(start + Duration::from_secs(3600));
        assert_eq!(hold.level(start + Duration::from_secs(3600)), 1.0);
        assert_eq!(hold.deadline(start + Duration::from_secs(3600)), None);
        let mut wheel = Envelope::new(start);
        for step in 1..1000 {
            let now = start + Duration::from_millis(step * 20);
            wheel.advance(now);
            wheel.press(now);
            wheel.release_at = Some(now + SCROLL_GAP);
            assert_eq!(
                wheel.since, start,
                "Continuous scrolling restarted the fade"
            );
            if step >= 3 {
                assert_eq!(wheel.level(now), 1.0);
            }
        }
        let stop = wheel.release_at.unwrap();
        wheel.advance(stop);
        assert_eq!(wheel.level(stop), 1.0);
        assert!(wheel.level(stop + FADE_OUT / 2) > 0.0);
        assert_eq!(wheel.level(stop + FADE_OUT), 0.0);
        // A new event during fading resumes from the current alpha, never from zero.
        let mut resumed = Envelope {
            since: stop,
            from: 1.0,
            to: 0.0,
            span: FADE_OUT,
            release_at: None,
        };
        let now = stop + FADE_OUT / 2;
        let before = resumed.level(now);
        resumed.press(now);
        assert_eq!(resumed.level(now), before);
        assert_eq!(resumed.level(now + FADE_IN), 1.0);
    }
    #[test]
    fn mouse_shapes_match_buttons_and_have_distinct_theme_colors() {
        for y in -20..20 {
            for x in -20..20 {
                let x = x as f32 / 20.0 + 0.01;
                let y = y as f32 / 20.0 + 0.01;
                assert_eq!(
                    Shape::Left.contains(x, y) || Shape::Right.contains(x, y),
                    x * x + y * y <= 1.0
                );
                assert!(!(Shape::Left.contains(x, y) && Shape::Right.contains(x, y)));
            }
        }
        assert!(Shape::Middle.contains(0.0, -0.3));
        assert!(!Shape::Middle.contains(0.5, -0.3));
        assert!(!Shape::Middle.contains(0.0, 0.3));
        assert!(Shape::Up.contains(0.0, -0.85));
        assert!(!Shape::Down.contains(0.0, -0.85));
        assert!(Shape::Down.contains(0.0, 0.85));
        for base in [0, 0xffffff, 0x808080, 0xd47800] {
            let palette = [
                Shape::Left,
                Shape::Right,
                Shape::Middle,
                Shape::Up,
                Shape::Down,
            ]
            .map(|s| color(base, s));
            for i in 0..palette.len() {
                for j in i + 1..palette.len() {
                    assert_ne!(palette[i], palette[j]);
                }
            }
        }
        for pixel in pixels(Shape::Left, 28, 0xd47800) {
            let a = pixel >> 24;
            for shift in [0, 8, 16] {
                assert!((pixel >> shift) & 255 <= a, "Invalid premultiplied alpha");
            }
        }
    }
    #[test]
    #[ignore = "shows temporary filled indicators; works with one or more monitors"]
    fn mouse_shapes_throttle_uploads_and_reuse_surfaces_while_held() {
        let _dpi = crate::test_support::Dpi::new();
        let _overlay = crate::overlay::Overlay::new().unwrap();
        let style = crate::test_support::cfg().mousecast;
        let mut visuals = MouseVisuals::new();
        for (_, display) in crate::test_support::displays() {
            let point = crate::test_support::point(&display);
            visuals.clear();
            visuals.prepare(point, &style).unwrap();
            visuals.event(1, 0, point, &style).unwrap();
            visuals.event(2, 0, point, &style).unwrap();
            let now = Instant::now() + FADE_IN;
            visuals.draw(now, &style).unwrap();
            assert!(visuals.slots[0].envelope.is_some() && visuals.slots[1].envelope.is_some());
            let uploads = visuals.uploads;
            let builds = visuals.builds;
            for n in 1..1000 {
                visuals
                    .draw(now + Duration::from_millis(n * 16), &style)
                    .unwrap();
            }
            assert_eq!(
                visuals.uploads, uploads,
                "Stable holds must not upload pixels"
            );
            assert_eq!(
                visuals.builds, builds,
                "Stable holds must not allocate bitmaps"
            );
            assert!(visuals.deadline().is_none());
            visuals.clear();
            visuals.last_draw = None;
            visuals.event(8, 0, point, &style).unwrap();
            let first = visuals.slots[5].envelope.unwrap().since;
            let uploads = visuals.uploads;
            let builds = visuals.builds;
            let started = Instant::now();
            for _ in 0..10000 {
                visuals.event(8, 0, point, &style).unwrap();
            }
            let elapsed = started.elapsed();
            assert_eq!(visuals.slots[5].envelope.unwrap().since, first);
            assert_eq!(visuals.builds, builds);
            assert!(visuals.uploads - uploads <= (elapsed.as_millis() / 16 + 1) as usize);
            println!(
                "dpi={} 10000 events={}us uploads={} bitmap_rebuilds={}",
                display.dpi,
                elapsed.as_micros(),
                visuals.uploads - uploads,
                visuals.builds - builds
            );
            let release = visuals.slots[5].envelope.unwrap().release_at.unwrap();
            visuals.draw(release + FADE_OUT, &style).unwrap();
            assert!(!visuals.active() && visuals.deadline().is_none());
            // Cache both wheel directions once, then reverse repeatedly without allocating.
            for button in [8, 9] {
                visuals.last_draw = None;
                visuals.event(button, 0, point, &style).unwrap();
            }
            let builds = visuals.builds;
            let since = visuals.slots[5].envelope.unwrap().since;
            for n in 0..200 {
                visuals.last_draw = None;
                visuals
                    .event(if n % 2 == 0 { 8 } else { 9 }, 0, point, &style)
                    .unwrap();
            }
            assert_eq!(
                visuals.builds, builds,
                "Direction changes recreated cached bitmaps"
            );
            assert_eq!(
                visuals.slots[5].envelope.unwrap().since,
                since,
                "Direction changes restarted fade-in"
            );
        }
    }
}
