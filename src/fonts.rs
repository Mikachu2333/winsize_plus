use crate::desktop::{err, wide};
use std::ptr::{from_mut, null_mut};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    UI::{HiDpi::SystemParametersInfoForDpi, WindowsAndMessaging::*},
};

pub struct Font(pub HFONT, i32);
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
        let quality = CLEARTYPE_QUALITY;
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
            let height = unsafe {
                let dc = GetDC(null_mut());
                let old = SelectObject(dc, handle);
                let mut metrics = TEXTMETRICW::default();
                GetTextMetricsW(dc, &mut metrics);
                SelectObject(dc, old);
                ReleaseDC(null_mut(), dc);
                (metrics.tmHeight + metrics.tmExternalLeading).max(1)
            };
            Ok(Self(handle, height))
        }
    }
    pub fn height(&self) -> i32 {
        self.1
    }
    pub fn supports_label(&self, text: &[u16]) -> bool {
        // SAFETY: the font stays alive, buffers match the UTF-16 length, and the DC is restored.
        unsafe {
            let dc = GetDC(null_mut());
            if dc.is_null() {
                return false;
            }
            let old = SelectObject(dc, self.0);
            let mut metrics = TEXTMETRICW::default();
            let mut glyphs = vec![0u16; text.len()];
            let supported = GetTextMetricsW(dc, &mut metrics) != 0
                && metrics.tmCharSet != SYMBOL_CHARSET
                && GetGlyphIndicesW(
                    dc,
                    text.as_ptr(),
                    text.len() as i32,
                    glyphs.as_mut_ptr(),
                    GGI_MARK_NONEXISTING_GLYPHS,
                ) != GDI_ERROR as u32
                && glyphs.iter().all(|glyph| *glyph != 0xffff && *glyph != 0);
            SelectObject(dc, old);
            ReleaseDC(null_mut(), dc);
            supported
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
    pub fn opaque(
        primary: &str,
        fallbacks: &[String],
        size: i32,
        dpi: i32,
        bold: bool,
        italic: bool,
    ) -> Result<Self, String> {
        let mut fonts = Vec::new();
        let mut families = Vec::<&str>::new();
        for name in std::iter::once(primary)
            .chain(fallbacks.iter().take(3).map(String::as_str))
            .chain(std::iter::once("SystemFont"))
        {
            if families
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(name))
            {
                continue;
            }
            families.push(name);
            fonts.push(Font::new(name, size, dpi, bold, italic)?);
        }
        Ok(Self(fonts))
    }
}
struct Run {
    font: usize,
    text: Vec<u16>,
    width: i32,
}
impl Run {
    fn is_tab(&self) -> bool {
        self.text == [b'\t' as u16]
    }
}
struct Line {
    runs: Vec<Run>,
    width: i32,
}
pub struct TextLayout {
    fonts: Fonts,
    lines: Vec<Line>,
    line_height: i32,
    ascents: Vec<i32>,
    baseline: i32,
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
            ascents: Vec::new(),
            baseline: 0,
            width: 0,
            height: 0,
        };
        let mut line = Line {
            runs: Vec::new(),
            width: 0,
        };
        let mut previous = null_mut();
        let mut descent = 0;
        let text = text.replace('\r', "");
        let tabbed_lines: Vec<_> = text.split('\n').map(|line| line.contains('\t')).collect();
        let mut line_number = 0;
        let mut gap = 1;
        // SAFETY: the owned fonts and UTF-16 buffers remain alive during synchronous GDI calls.
        unsafe {
            for (index, font) in layout.fonts.0.iter().enumerate() {
                let old = SelectObject(dc, font.0);
                if index == 0 {
                    previous = old;
                    let mut size = SIZE::default();
                    GetTextExtentPoint32W(dc, [32u16, 32].as_ptr(), 2, &mut size);
                    gap = size.cx.max(1);
                }
                let mut metrics = TEXTMETRICW::default();
                GetTextMetricsW(dc, &mut metrics);
                layout.line_height = layout
                    .line_height
                    .max(metrics.tmHeight + metrics.tmExternalLeading);
                layout.ascents.push(metrics.tmAscent);
                layout.baseline = layout.baseline.max(metrics.tmAscent);
                descent = descent.max(metrics.tmDescent + metrics.tmExternalLeading);
            }
            layout.line_height = layout.line_height.max(layout.baseline + descent);
            for ch in text.chars() {
                if ch == '\n' {
                    layout.lines.push(line);
                    line = Line {
                        runs: Vec::new(),
                        width: 0,
                    };
                    line_number += 1;
                    continue;
                }
                if ch == '\t' {
                    line.runs.push(Run {
                        font: 0,
                        text: vec![9],
                        width: 0,
                    });
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
                if !tabbed_lines[line_number]
                    && line.width > 0
                    && line.width + size.cx > max_width.max(1)
                {
                    layout.lines.push(line);
                    line = Line {
                        runs: Vec::new(),
                        width: 0,
                    };
                }
                if let Some(run) = line
                    .runs
                    .last_mut()
                    .filter(|run| run.font == selected && !run.is_tab())
                {
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
        // Measure complete runs using the same shaping/kerning as TextOutW.
        unsafe {
            let dc = GetDC(null_mut());
            if dc.is_null() {
                return Err(err());
            }
            let old = SelectObject(dc, layout.fonts.0[0].0);
            for line in &mut layout.lines {
                line.width = 0;
                for run in &mut line.runs {
                    if run.is_tab() {
                        continue;
                    }
                    SelectObject(dc, layout.fonts.0[run.font].0);
                    let mut size = SIZE::default();
                    GetTextExtentPoint32W(dc, run.text.as_ptr(), run.text.len() as i32, &mut size);
                    run.width = size.cx;
                    line.width += size.cx;
                }
            }
            // Each consecutive group of tabbed lines is a table. Measure columns independently,
            // so short key names never inherit the wider spacing needed by region names.
            let mut start = 0;
            while start < layout.lines.len() {
                if !layout.lines[start].runs.iter().any(Run::is_tab) {
                    start += 1;
                    continue;
                }
                let end = start
                    + layout.lines[start..]
                        .iter()
                        .take_while(|line| line.runs.iter().any(Run::is_tab))
                        .count();
                let mut widths = Vec::<i32>::new();
                for line in &layout.lines[start..end] {
                    let (mut column, mut width) = (0, 0);
                    for run in &line.runs {
                        if run.is_tab() {
                            if column == widths.len() {
                                widths.push(0);
                            }
                            widths[column] = widths[column].max(width);
                            column += 1;
                            width = 0;
                        } else {
                            width += run.width;
                        }
                    }
                    if column == widths.len() {
                        widths.push(width);
                    } else {
                        widths[column] = widths[column].max(width);
                    }
                }
                for line in &mut layout.lines[start..end] {
                    let (mut column, mut width) = (0, 0);
                    for run in &mut line.runs {
                        if run.is_tab() {
                            run.width = widths[column] - width + gap;
                            column += 1;
                            width = 0;
                        } else {
                            width += run.width;
                        }
                    }
                    line.width = line.runs.iter().map(|run| run.width).sum();
                }
                start = end;
            }
            // Very wide tables must remain readable on small screens: wrap their text and
            // reduce tab padding on overflow, instead of clipping the region names.
            let max_width = max_width.max(1);
            let mut wrapped = Vec::new();
            for line in std::mem::take(&mut layout.lines) {
                if line.width <= max_width || !line.runs.iter().any(Run::is_tab) {
                    wrapped.push(line);
                    continue;
                }
                let mut current = Line {
                    runs: Vec::new(),
                    width: 0,
                };
                for run in line.runs {
                    if run.is_tab() {
                        if current.width > 0 && current.width + gap <= max_width {
                            current.runs.push(Run { width: gap, ..run });
                            current.width += gap;
                        }
                        continue;
                    }
                    SelectObject(dc, layout.fonts.0[run.font].0);
                    for ch in String::from_utf16_lossy(&run.text).chars() {
                        let mut units = [0; 2];
                        let units = ch.encode_utf16(&mut units);
                        let mut size = SIZE::default();
                        GetTextExtentPoint32W(dc, units.as_ptr(), units.len() as i32, &mut size);
                        if current.width > 0 && current.width + size.cx > max_width {
                            wrapped.push(current);
                            current = Line {
                                runs: Vec::new(),
                                width: 0,
                            };
                        }
                        current.runs.push(Run {
                            font: run.font,
                            text: units.to_vec(),
                            width: size.cx,
                        });
                        current.width += size.cx;
                    }
                }
                wrapped.push(current);
            }
            layout.lines = wrapped;
            SelectObject(dc, old);
            ReleaseDC(null_mut(), dc);
        }
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
        // SAFETY: the caller supplies a live DC; all selected fonts remain owned by the layout.
        unsafe {
            let mut clip = RECT::default();
            if matches!(GetClipBox(dc, &mut clip), 0 | 1) {
                return;
            }
            let visible = self.visible_lines(y, clip.top, clip.bottom);
            let old = SelectObject(dc, self.fonts.0[0].0);
            let old_color = SetTextColor(dc, color);
            let old_mode = SetBkMode(dc, TRANSPARENT as i32);
            for index in visible {
                let line = &self.lines[index];
                let mut left = x + if centered {
                    (self.width - line.width) / 2
                } else {
                    0
                };
                for run in &line.runs {
                    if run.is_tab() {
                        left += run.width;
                        continue;
                    }
                    SelectObject(dc, self.fonts.0[run.font].0);
                    TextOutW(
                        dc,
                        left,
                        y + index as i32 * self.line_height + self.baseline
                            - self.ascents[run.font],
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
    fn visible_lines(&self, y: i32, top: i32, bottom: i32) -> std::ops::Range<usize> {
        // Include one adjacent line for glyph overhang, but never visit the full document.
        let start = (i64::from(top) - i64::from(y))
            .div_euclid(i64::from(self.line_height))
            .saturating_sub(1)
            .max(0) as usize;
        let end = (i64::from(bottom) - i64::from(y))
            .div_euclid(i64::from(self.line_height))
            .saturating_add(2)
            .max(0) as usize;
        start.min(self.lines.len())..end.max(start).min(self.lines.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(text: &str, width: i32, dpi: i32) -> TextLayout {
        TextLayout::new(
            text,
            Fonts::opaque("SystemFont", &[], 12, dpi, false, false).unwrap(),
            width,
        )
        .unwrap()
    }

    fn stops(line: &Line) -> Vec<i32> {
        let mut x = 0;
        line.runs
            .iter()
            .filter_map(|run| {
                x += run.width;
                run.is_tab().then_some(x)
            })
            .collect()
    }

    #[test]
    fn tab_columns_align_using_actual_font_widths_and_reset_between_tables() {
        for dpi in [96, 144, 192] {
            let table = layout(
                "F12\t\t中文长区域名\tRight\nQ\t->\tLeft\tR\n\nI\tX",
                4000,
                dpi,
            );
            assert_eq!(stops(&table.lines[0]), stops(&table.lines[1]));
            let columns = stops(&table.lines[0]);
            assert!(columns[2] - columns[1] > columns[0]);
            assert!(stops(&table.lines[3])[0] < columns[0]);
            let longer = layout(
                "F12\t\tEvenLongerRegionName\tRight\nQ\t->\tLeft\tR",
                4000,
                dpi,
            );
            assert_eq!(&stops(&longer.lines[0])[..2], &columns[..2]);
            assert!(stops(&longer.lines[0])[2] > columns[2]);
        }
    }

    #[test]
    fn shorter_rows_still_contribute_their_last_cell_to_column_widths() {
        let table = layout("Q\tLongRegionName\nF12\tX\tY", 4000, 96);
        let first_stops = stops(&table.lines[0]);
        let second_stops = stops(&table.lines[1]);
        assert_eq!(first_stops[0], second_stops[0]);
        assert!(second_stops[1] > table.lines[0].width);
    }

    #[test]
    fn narrow_tables_wrap_without_losing_text() {
        let text = "Q\tW\t->\t这是很长的区域名称\tRightBottom";
        let table = layout(text, 80, 96);
        assert!(table.lines.len() > 1);
        assert!(table.width <= 80);
        let displayed: Vec<u16> = table
            .lines
            .iter()
            .flat_map(|line| &line.runs)
            .filter(|run| !run.is_tab())
            .flat_map(|run| run.text.iter().copied())
            .collect();
        assert_eq!(String::from_utf16_lossy(&displayed), text.replace('\t', ""));
    }
    #[test]
    fn scrolling_draws_only_the_viewport_and_adjacent_glyph_overhang() {
        let text = layout(&"line\n".repeat(4500), 300, 96);
        let line_height = text.line_height;
        for scroll in [
            0,
            line_height - 1,
            line_height * 2000 + 3,
            text.height - 300,
        ] {
            let range = text.visible_lines(-scroll, 0, 300);
            assert!(range.len() <= (300 / line_height + 4) as usize);
            for line in 0..text.lines.len() {
                let y = line as i32 * line_height - scroll;
                if y < 300 && y + line_height > 0 {
                    assert!(
                        range.contains(&line),
                        "missing visible line {line} at scroll {scroll}"
                    );
                }
            }
        }
        assert!(text.visible_lines(text.height, 0, 300).is_empty());
        assert!(text.visible_lines(-text.height - 300, 0, 300).is_empty());
    }
}
