//! DirectWrite labels rendered directly to premultiplied BGRA for layered windows.
use crate::{config::Config, desktop, fonts};
use std::{
    cell::{Cell, RefCell},
    marker::PhantomData,
    ptr::null_mut,
    rc::Rc,
};
use windows::{
    Win32::{
        Foundation::RECT,
        Graphics::{
            Direct2D::{Common::*, *},
            DirectWrite::*,
            Dxgi::Common::*,
            Gdi::HDC,
        },
    },
    core::{Interface, PCWSTR},
};
use windows_numerics::Vector2;
use windows_sys::Win32::{Globalization::GetUserDefaultLocaleName, Graphics::Gdi::*};

thread_local! {
    static RENDERER: RefCell<Option<Renderer>> = const { RefCell::new(None) };
    static SESSIONS: Cell<usize> = const {Cell::new(0)};
}

/// Own graphics on the UI thread while its message loop is alive. Releasing WARP
/// from an FLS/TLS destructor during ExitProcess can schedule work after Windows
/// has already shut down its thread pool (TppRaiseInvalidParameter, 0xc000000d).
pub(crate) struct Session(PhantomData<Rc<()>>);
impl Session {
    pub(crate) fn new() -> Self {
        SESSIONS.with(|count| count.set(count.get() + 1));
        Self(PhantomData)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let last = SESSIONS.with(|count| {
            let remaining = count.get() - 1;
            count.set(remaining);
            remaining == 0
        });
        if last {
            let renderer = RENDERER.with_borrow_mut(Option::take);
            drop(renderer); // Drop COM resources outside the RefCell borrow and loader shutdown.
        }
    }
}
fn with_renderer<T>(draw: impl FnOnce(&mut Renderer) -> Result<T, String>) -> Result<T, String> {
    if SESSIONS.with(Cell::get) == 0 {
        // Standalone calls must not leave graphics resources for a TLS destructor.
        return draw(&mut Renderer::new().map_err(|e| e.to_string())?);
    }
    RENDERER.with(|cell| {
        let mut slot = cell
            .try_borrow_mut()
            .map_err(|_| "Renderer is already drawing / 正在绘制".to_owned())?;
        if slot.is_none() {
            *slot = Some(Renderer::new().map_err(|e| e.to_string())?);
        }
        draw(slot.as_mut().unwrap())
    })
}

#[derive(PartialEq, Eq)]
struct Style {
    families: Vec<String>,
    points: i32,
    bold: bool,
    italic: bool,
}
impl Style {
    fn new(cfg: &Config, dpi: i32) -> Self {
        Self::fonts(
            &cfg.font,
            &cfg.font_fallbacks,
            cfg.font_size,
            cfg.font_bold,
            cfg.font_italic,
            dpi,
        )
    }
    fn fonts(
        font: &str,
        fallbacks: &[String],
        points: i32,
        bold: bool,
        italic: bool,
        dpi: i32,
    ) -> Self {
        let face = fonts::system_face(dpi);
        let length = face
            .lfFaceName
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(32);
        let system = String::from_utf16_lossy(&face.lfFaceName[..length]);
        let mut families = Vec::<String>::new();
        for family in std::iter::once(font)
            .chain(fallbacks.iter().take(3).map(String::as_str))
            .chain(std::iter::once("SystemFont"))
        {
            let family = if family.eq_ignore_ascii_case("SystemFont") {
                &system
            } else {
                family
            };
            if !families.iter().any(|old| old.eq_ignore_ascii_case(family)) {
                families.push(family.to_owned());
            }
        }
        Self {
            families,
            points,
            bold,
            italic,
        }
    }
}

struct Renderer {
    write: IDWriteFactory2,
    draw: ID2D1Factory,
    target: Option<ID2D1DCRenderTarget>,
    format: Option<(Style, IDWriteTextFormat)>,
}
impl Drop for Renderer {
    fn drop(&mut self) {
        // Release device resources before their factories, during Session teardown.
        self.target.take();
        self.format.take();
    }
}
impl Renderer {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: the factories and their resources remain on the creating UI thread.
        unsafe {
            Ok(Self {
                write: DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?,
                draw: D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?,
                target: None,
                format: None,
            })
        }
    }
    fn format(&mut self, style: Style) -> windows::core::Result<&IDWriteTextFormat> {
        if self.format.as_ref().is_none_or(|(old, _)| *old != style) {
            let family = desktop::wide(&style.families[0]);
            let mut locale = [0u16; 85];
            // SAFETY: null-terminated UTF-16 buffers and COM objects outlive synchronous calls.
            unsafe {
                GetUserDefaultLocaleName(locale.as_mut_ptr(), locale.len() as i32);
                let format = self.write.CreateTextFormat(
                    PCWSTR(family.as_ptr()),
                    None,
                    if style.bold {
                        DWRITE_FONT_WEIGHT_BOLD
                    } else {
                        DWRITE_FONT_WEIGHT_NORMAL
                    },
                    if style.italic {
                        DWRITE_FONT_STYLE_ITALIC
                    } else {
                        DWRITE_FONT_STYLE_NORMAL
                    },
                    DWRITE_FONT_STRETCH_NORMAL,
                    style.points as f32 * 96.0 / 72.0,
                    PCWSTR(locale.as_ptr()),
                )?;
                let builder = self.write.CreateFontFallbackBuilder()?;
                let fallbacks: Vec<_> = style.families.iter().skip(1).map(desktop::wide).collect();
                if !fallbacks.is_empty() {
                    let names: Vec<_> = fallbacks.iter().map(|name| name.as_ptr()).collect();
                    builder.AddMapping(
                        &[DWRITE_UNICODE_RANGE {
                            first: 0,
                            last: 0x10ffff,
                        }],
                        &names,
                        None,
                        PCWSTR::null(),
                        PCWSTR::null(),
                        1.0,
                    )?;
                }
                builder.AddMappings(&self.write.GetSystemFontFallback()?)?;
                format
                    .cast::<IDWriteTextFormat1>()?
                    .SetFontFallback(&builder.CreateFontFallback()?)?;
                format.SetWordWrapping(DWRITE_WORD_WRAPPING_CHARACTER)?;
                self.format = Some((style, format));
            }
        }
        Ok(&self.format.as_ref().unwrap().1)
    }
    fn render(
        &mut self,
        caption: &str,
        width: i32,
        dpi: i32,
        color: u32,
        style: Style,
    ) -> Result<Image, String> {
        let scale = dpi.max(1) as f32 / 96.0;
        let format = self.format(style).map_err(|e| e.to_string())?.clone();
        let units: Vec<u16> = caption.encode_utf16().collect();
        // SAFETY: layout and drawing use owned buffers and resources on this UI thread.
        unsafe {
            let layout = self
                .write
                .CreateTextLayout(&units, &format, width.max(1) as f32 / scale, 100_000.0)
                .map_err(|e| e.to_string())?;
            let mut metrics = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut metrics).map_err(|e| e.to_string())?;
            // Round outward only when allocating pixels, keeping fractional glyph advances.
            let content_width = (metrics.widthIncludingTrailingWhitespace * scale)
                .ceil()
                .max(1.0);
            layout
                .SetMaxWidth(content_width / scale)
                .map_err(|e| e.to_string())?;
            layout
                .SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)
                .map_err(|e| e.to_string())?;
            layout.GetMetrics(&mut metrics).map_err(|e| e.to_string())?;
            layout
                .SetMaxHeight(metrics.height)
                .map_err(|e| e.to_string())?;
            let overhang = layout.GetOverhangMetrics().map_err(|e| e.to_string())?;
            let inset = |amount: f32| (amount.max(0.0) * scale).ceil() as i32 + 2;
            let left = inset(overhang.left);
            let top = inset(overhang.top);
            let image = Image::new(
                content_width as i32 + left + inset(overhang.right),
                (metrics.height * scale).ceil() as i32 + top + inset(overhang.bottom),
            )?;
            let bounds = RECT {
                left: 0,
                top: 0,
                right: image.width,
                bottom: image.height,
            };
            if self.target.is_none() {
                self.target = Some(
                    self.draw
                        .CreateDCRenderTarget(&D2D1_RENDER_TARGET_PROPERTIES {
                            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                            pixelFormat: D2D1_PIXEL_FORMAT {
                                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                            },
                            dpiX: dpi as f32,
                            dpiY: dpi as f32,
                            ..Default::default()
                        })
                        .map_err(|e| e.to_string())?,
                );
            }
            let target = self.target.as_ref().unwrap();
            let result = (|| -> windows::core::Result<()> {
                target.BindDC(HDC(image.dc), &bounds)?;
                target.SetDpi(dpi as f32, dpi as f32);
                target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
                let brush = target.CreateSolidColorBrush(
                    &D2D1_COLOR_F {
                        r: (color & 255) as f32 / 255.0,
                        g: ((color >> 8) & 255) as f32 / 255.0,
                        b: ((color >> 16) & 255) as f32 / 255.0,
                        a: 1.0,
                    },
                    None,
                )?;
                target.BeginDraw();
                target.Clear(Some(&D2D1_COLOR_F::default()));
                target.DrawTextLayout(
                    Vector2::new(left as f32 / scale, top as f32 / scale),
                    &layout,
                    &brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                );
                target.EndDraw(None, None)
            })();
            if let Err(error) = result {
                self.target = None; // Discard device resources; the next render can recreate them.
                return Err(error.to_string());
            }
            GdiFlush();
            Ok(image)
        }
    }
}

/// Owns the bitmap selected into its memory DC, including cleanup on a failed render.
pub struct Image {
    pub dc: windows_sys::Win32::Graphics::Gdi::HDC,
    pub width: i32,
    pub height: i32,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u32,
}
impl Image {
    fn new(width: i32, height: i32) -> Result<Self, String> {
        // SAFETY: the DIB allocation is owned here and deselected before deletion in Drop.
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
            if dc.is_null() {
                return Err(desktop::err());
            }
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = null_mut();
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bitmap.is_null() {
                let error = desktop::err();
                DeleteDC(dc);
                return Err(error);
            }
            let previous = SelectObject(dc, bitmap);
            if previous.is_null() || previous as isize == -1 {
                let error = desktop::err();
                DeleteObject(bitmap);
                DeleteDC(dc);
                return Err(error);
            }
            Ok(Self {
                dc,
                width,
                height,
                bitmap,
                previous,
                bits: bits.cast(),
            })
        }
    }
    pub(crate) fn pixels(width: i32, height: i32, pixels: &[u32]) -> Result<Self, String> {
        if width <= 0
            || height <= 0
            || usize::try_from(i64::from(width) * i64::from(height)).ok() != Some(pixels.len())
        {
            return Err("Invalid bitmap dimensions".into());
        }
        let image = Self::new(width, height)?;
        // SAFETY: the checked dimensions equal the owned 32-bit DIB allocation.
        unsafe {
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), image.bits, pixels.len());
        }
        Ok(image)
    }
}
impl Drop for Image {
    fn drop(&mut self) {
        // SAFETY: DC and bitmap belong to this value; no drawing is pending after EndDraw.
        unsafe {
            SelectObject(self.dc, self.previous);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}
pub fn render(
    caption: &str,
    width: i32,
    dpi: i32,
    color: u32,
    cfg: &Config,
) -> Result<Image, String> {
    with_renderer(|renderer| renderer.render(caption, width, dpi, color, Style::new(cfg, dpi)))
}
pub fn annotation(
    caption: &str,
    width: i32,
    dpi: i32,
    cfg: &crate::config::CastStyle,
) -> Result<Image, String> {
    with_renderer(|renderer| {
        renderer.render(
            caption,
            width,
            dpi,
            cfg.foreground,
            Style::fonts(&cfg.font, &[], cfg.font_size, false, false, dpi),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn region_name_and_shortcut_lines_are_centered_together() {
        let mut cfg = crate::test_support::cfg();
        cfg.font = "Arial".into();
        for dpi in [96, 144, 192] {
            let image = render(&cfg.region_caption("2/9"), 600, dpi, 0xffffff, &cfg).unwrap();
            let mut bands = Vec::new();
            let mut bounds: Option<(usize, usize)> = None;
            for row in pixels(&image).chunks_exact(image.width as usize) {
                let first = row.iter().position(|p| p >> 24 != 0);
                let last = row.iter().rposition(|p| p >> 24 != 0);
                if let (Some(first), Some(last)) = (first, last) {
                    bounds = Some(bounds.map_or((first, last), |(left, right)| {
                        (left.min(first), right.max(last))
                    }));
                } else if let Some(bounds) = bounds.take() {
                    bands.push(bounds);
                }
            }
            if let Some(bounds) = bounds {
                bands.push(bounds);
            }
            assert_eq!(bands.len(), 2, "Expected a region line and a shortcut line");
            for (left, right) in bands {
                assert!((left as i32 + right as i32 - image.width).abs() <= 4 * dpi / 96);
            }
        }
    }
    #[test]
    #[ignore = "child process for the graphics shutdown regression"]
    fn graphics_shutdown_child() {
        if std::env::var_os("WINSIZEP_GRAPHICS_EXIT_TEST").is_none() {
            return;
        }
        let cfg = crate::test_support::cfg();
        {
            let _overlay = crate::overlay::Overlay::new().unwrap();
            for n in 0..25 {
                render("Exit lifetime 中文", 400, 96 + n % 2 * 144, 0, &cfg).unwrap();
            }
        }
        assert!(RENDERER.with_borrow(Option::is_none));
    }
    #[test]
    #[ignore = "starts isolated processes to verify Direct2D releases before ExitProcess"]
    fn graphics_resources_survive_repeated_process_shutdown() {
        use std::os::windows::process::CommandExt;
        for _ in 0..12 {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "--ignored",
                    "label_text::tests::graphics_shutdown_child",
                ])
                .env("WINSIZEP_GRAPHICS_EXIT_TEST", "1")
                .creation_flags(0x08000000)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    #[test]
    fn last_graphics_session_releases_renderer_before_thread_exit() {
        let cfg = crate::test_support::cfg();
        let first = Session::new();
        let second = Session::new();
        render("lifetime", 200, 96, 0, &cfg).unwrap();
        assert!(RENDERER.with_borrow(Option::is_some));
        drop(first);
        assert!(RENDERER.with_borrow(Option::is_some));
        drop(second);
        assert!(RENDERER.with_borrow(Option::is_none));
        render("standalone", 200, 96, 0, &cfg).unwrap();
        assert!(RENDERER.with_borrow(Option::is_none));
    }
    fn pixels(image: &Image) -> &[u32] {
        // SAFETY: the bitmap owns these pixels until image is dropped; EndDraw has completed.
        unsafe { std::slice::from_raw_parts(image.bits, (image.width * image.height) as usize) }
    }
    fn validate(image: &Image) {
        let pixels = pixels(image);
        assert!(
            pixels
                .iter()
                .any(|pixel| pixel >> 24 > 0 && pixel >> 24 < 255)
        );
        for (index, pixel) in pixels.iter().enumerate() {
            let alpha = pixel >> 24;
            for shift in [0, 8, 16] {
                assert!((pixel >> shift) & 255 <= alpha);
            }
            let x = index % image.width as usize;
            let y = index / image.width as usize;
            if x == 0 || y == 0 || x + 1 == image.width as usize || y + 1 == image.height as usize {
                assert_eq!(alpha, 0, "Clipped glyph at {x},{y}");
            }
        }
    }
    #[test]
    fn transparent_glyphs_have_smooth_alpha_and_unclipped_overhangs() {
        let mut cfg = Config::parse(&crate::config::help().parse().unwrap()).unwrap();
        cfg.font = "Arial".into();
        cfg.font_fallbacks = vec!["Microsoft YaHei UI".into(), "Segoe UI Emoji".into()];
        for dpi in [96, 120, 144, 192] {
            for italic in [false, true] {
                cfg.font_italic = italic;
                let image = render(
                    "Center 中心区域 Aa 0123\n🙂 e\u{301}",
                    400,
                    dpi,
                    0xbd6c0f,
                    &cfg,
                )
                .unwrap();
                validate(&image);
                assert!(
                    pixels(&image).contains(&0xff0f6cbd),
                    "COLORREF channel order changed"
                );
                let wrapped = render("LeftBottom 左下角", 40, dpi, 0xffffff, &cfg).unwrap();
                validate(&wrapped);
                assert!(wrapped.height > image.height);
            }
        }
        let image = render("", 1, 96, 0, &cfg).unwrap();
        assert!(pixels(&image).iter().all(|pixel| *pixel == 0));
    }
    #[test]
    fn configured_fallback_order_changes_cjk_glyphs() {
        let installed = fonts::families();
        let families = ["Microsoft YaHei UI", "SimSun"];
        if !families
            .iter()
            .all(|family| installed.iter().any(|name| name == family))
        {
            return; // These optional Windows language-pack fonts are needed for this comparison.
        }
        let mut cfg = Config::parse(&crate::config::help().parse().unwrap()).unwrap();
        cfg.font = "Arial".into();
        cfg.font_fallbacks = families.iter().map(|name| (*name).into()).collect();
        let first = render("中心区域", 500, 96, 0xffffff, &cfg).unwrap();
        cfg.font_fallbacks.reverse();
        let second = render("中心区域", 500, 96, 0xffffff, &cfg).unwrap();
        assert_ne!(
            pixels(&first),
            pixels(&second),
            "Configured font order was ignored"
        );
        cfg.font_fallbacks
            .insert(0, "WinSizeP nonexistent font".into());
        let missing = render("中心区域", 500, 96, 0xffffff, &cfg).unwrap();
        assert_eq!(
            pixels(&second),
            pixels(&missing),
            "Missing font blocked the next fallback"
        );
    }
    #[test]
    #[ignore = "writes DirectWrite glyph samples for visual comparison"]
    fn capture_labels() {
        let folder = std::env::current_dir().unwrap().join("target/text-render");
        std::fs::create_dir_all(&folder).unwrap();
        let mut cfg = Config::parse(&crate::config::help().parse().unwrap()).unwrap();
        for dpi in [96, 120, 144, 192] {
            for (points, italic) in [(12, false), (14, false), (12, true)] {
                cfg.font_size = points;
                cfg.font_italic = italic;
                let image = render("Center 中心区域 Aa 0123", 1000, dpi, 0, &cfg).unwrap();
                validate(&image);
                let mut bytes = vec![0u8; 54];
                bytes[..2].copy_from_slice(b"BM");
                bytes[2..6].copy_from_slice(&(54 + image.width * image.height * 4).to_le_bytes());
                bytes[10..14].copy_from_slice(&54u32.to_le_bytes());
                bytes[14..18].copy_from_slice(&40u32.to_le_bytes());
                bytes[18..22].copy_from_slice(&image.width.to_le_bytes());
                bytes[22..26].copy_from_slice(&(-image.height).to_le_bytes());
                bytes[26..28].copy_from_slice(&1u16.to_le_bytes());
                bytes[28..30].copy_from_slice(&32u16.to_le_bytes());
                for pixel in pixels(&image) {
                    bytes.extend_from_slice(&pixel.to_le_bytes());
                }
                std::fs::write(
                    folder.join(format!("dwrite-{dpi}-{points}-{italic}.bmp")),
                    bytes,
                )
                .unwrap();
            }
        }
    }
}
