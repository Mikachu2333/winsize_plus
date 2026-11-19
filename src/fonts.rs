use crate::desktop::{err, wide};
use std::ptr::{from_mut, null_mut};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    UI::{HiDpi::SystemParametersInfoForDpi, WindowsAndMessaging::*},
};

pub struct Font(pub HFONT);
impl Drop for Font {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.0);
        }
    }
}

pub fn system_face(dpi: i32) -> LOGFONTW {
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    // SAFETY: metrics has the size and lifetime required by the synchronous Win32 call.
    unsafe {
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS,
            metrics.cbSize,
            from_mut(&mut metrics).cast(),
            0,
            dpi as u32,
        );
    }
    if metrics.lfMessageFont.lfFaceName[0] == 0 {
        let name = wide("Segoe UI");
        metrics.lfMessageFont.lfFaceName[..name.len()].copy_from_slice(&name);
        metrics.lfMessageFont.lfHeight = -((12 * dpi + 48) / 96);
    }
    metrics.lfMessageFont
}

impl Font {
    pub fn new(
        family: &str,
        points: i32,
        dpi: i32,
        bold: bool,
        italic: bool,
    ) -> Result<Self, String> {
        Self::with_quality(family, points, dpi, bold, italic, CLEARTYPE_QUALITY)
    }
    fn with_quality(
        family: &str,
        points: i32,
        dpi: i32,
        bold: bool,
        italic: bool,
        quality: u8,
    ) -> Result<Self, String> {
        let mut face = system_face(dpi);
        if !family.eq_ignore_ascii_case("SystemFont") {
            face.lfFaceName = [0; 32];
            let name = wide(family);
            let len = (name.len() - 1).min(31);
            face.lfFaceName[..len].copy_from_slice(&name[..len]);
        }
        if points > 0 {
            face.lfHeight = -((points * dpi + 36) / 72);
        }
        face.lfWeight = if bold {
            FW_BOLD as i32
        } else {
            FW_NORMAL as i32
        };
        face.lfItalic = u8::from(italic);
        face.lfQuality = quality;
        let handle = unsafe { CreateFontIndirectW(&face) };
        if handle.is_null() {
            Err(err())
        } else {
            Ok(Self(handle))
        }
    }
    pub fn height(&self) -> i32 {
        unsafe {
            let dc = GetDC(null_mut());
            let old = SelectObject(dc, self.0);
            let mut metrics = TEXTMETRICW::default();
            GetTextMetricsW(dc, &mut metrics);
            SelectObject(dc, old);
            ReleaseDC(null_mut(), dc);
            (metrics.tmHeight + metrics.tmExternalLeading).max(1)
        }
    }
}

pub fn families() -> Vec<String> {
    unsafe extern "system" fn collect(
        face: *const LOGFONTW,
        _: *const TEXTMETRICW,
        _: u32,
        param: LPARAM,
    ) -> i32 {
        // SAFETY: GDI calls synchronously with a valid LOGFONTW and our live Vec.
        unsafe {
            let face = &*face;
            let len = face.lfFaceName.iter().position(|c| *c == 0).unwrap_or(32);
            let name = String::from_utf16_lossy(&face.lfFaceName[..len]);
            if !name.starts_with('@') && !name.is_empty() {
                (&mut *(param as *mut Vec<String>)).push(name);
            }
        }
        1
    }
    let mut names = Vec::<String>::new();
    unsafe {
        let dc = GetDC(null_mut());
        let face = LOGFONTW {
            lfCharSet: DEFAULT_CHARSET,
            ..Default::default()
        };
        EnumFontFamiliesExW(dc, &face, Some(collect), from_mut(&mut names) as isize, 0);
        ReleaseDC(null_mut(), dc);
    }
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup();
    names.retain(|name| name != "SystemFont");
    names.insert(0, "SystemFont".into());
    names
}

pub struct Fonts(Vec<Font>);
impl Fonts {
    pub fn new(
        primary: &str,
        fallbacks: &[String],
        size: i32,
        dpi: i32,
        bold: bool,
        italic: bool,
    ) -> Result<Self, String> {
        let mut fonts = Vec::new();
        for name in std::iter::once(primary)
            .chain(fallbacks.iter().take(3).map(String::as_str))
            .chain(std::iter::once("SystemFont"))
        {
            // Transparent overlay text needs grayscale edges; opaque settings use ClearType.
            fonts.push(Font::with_quality(
                name,
                size,
                dpi,
                bold,
                italic,
                ANTIALIASED_QUALITY,
            )?);
        }
        Ok(Self(fonts))
    }
}
struct Run {
    font: usize,
    text: Vec<u16>,
    width: i32,
}
struct Line {
    runs: Vec<Run>,
    width: i32,
}
pub struct TextLayout {
    fonts: Fonts,
    lines: Vec<Line>,
    line_height: i32,
    pub width: i32,
    pub height: i32,
}
impl TextLayout {
    pub fn new(text: &str, fonts: Fonts, max_width: i32) -> Result<Self, String> {
        let dc = unsafe { GetDC(null_mut()) };
        if dc.is_null() {
            return Err(err());
        }
        let mut layout = Self {
            fonts,
            lines: Vec::new(),
            line_height: 1,
            width: 0,
            height: 0,
        };
        let mut line = Line {
            runs: Vec::new(),
            width: 0,
        };
        let mut previous = null_mut();
        unsafe {
            for (index, font) in layout.fonts.0.iter().enumerate() {
                let old = SelectObject(dc, font.0);
                if index == 0 {
                    previous = old;
                }
                let mut metrics = TEXTMETRICW::default();
                GetTextMetricsW(dc, &mut metrics);
                layout.line_height = layout
                    .line_height
                    .max(metrics.tmHeight + metrics.tmExternalLeading);
            }
            for ch in text.replace('\r', "").chars() {
                if ch == '\n' {
                    layout.lines.push(line);
                    line = Line {
                        runs: Vec::new(),
                        width: 0,
                    };
                    continue;
                }
                let mut units = [0u16; 2];
                let units = ch.encode_utf16(&mut units);
                let mut selected = layout.fonts.0.len() - 1;
                for (index, font) in layout.fonts.0.iter().enumerate() {
                    SelectObject(dc, font.0);
                    let mut glyphs = [0u16; 2];
                    if GetGlyphIndicesW(
                        dc,
                        units.as_ptr(),
                        units.len() as i32,
                        glyphs.as_mut_ptr(),
                        GGI_MARK_NONEXISTING_GLYPHS,
                    ) != GDI_ERROR as u32
                        && glyphs[..units.len()].iter().all(|glyph| *glyph != 0xffff)
                    {
                        selected = index;
                        break;
                    }
                }
                SelectObject(dc, layout.fonts.0[selected].0);
                let mut size = SIZE::default();
                GetTextExtentPoint32W(dc, units.as_ptr(), units.len() as i32, &mut size);
                if line.width > 0 && line.width + size.cx > max_width.max(1) {
                    layout.lines.push(line);
                    line = Line {
                        runs: Vec::new(),
                        width: 0,
                    };
                }
                if let Some(run) = line.runs.last_mut().filter(|run| run.font == selected) {
                    run.text.extend_from_slice(units);
                    run.width += size.cx;
                } else {
                    line.runs.push(Run {
                        font: selected,
                        text: units.to_vec(),
                        width: size.cx,
                    });
                }
                line.width += size.cx;
            }
            SelectObject(dc, previous);
            ReleaseDC(null_mut(), dc);
        }
        layout.lines.push(line);
        layout.width = layout
            .lines
            .iter()
            .map(|line| line.width)
            .max()
            .unwrap_or(1)
            .max(1);
        layout.height = layout.line_height * layout.lines.len() as i32;
        Ok(layout)
    }
    pub fn draw(&self, dc: HDC, x: i32, y: i32, color: u32, centered: bool) {
        unsafe {
            let old = SelectObject(dc, self.fonts.0[0].0);
            let old_color = SetTextColor(dc, color);
            let old_mode = SetBkMode(dc, TRANSPARENT as i32);
            for (index, line) in self.lines.iter().enumerate() {
                let mut left = x + if centered {
                    (self.width - line.width) / 2
                } else {
                    0
                };
                for run in &line.runs {
                    SelectObject(dc, self.fonts.0[run.font].0);
                    TextOutW(
                        dc,
                        left,
                        y + index as i32 * self.line_height,
                        run.text.as_ptr(),
                        run.text.len() as i32,
                    );
                    left += run.width;
                }
            }
            SetTextColor(dc, old_color);
            SetBkMode(dc, old_mode);
            SelectObject(dc, old);
        }
    }
}
