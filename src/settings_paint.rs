//! Owner drawing must not borrow the editor: Windows can request painting during layout.
use crate::{desktop::wide, fonts::Font};
use std::{cell::RefCell, collections::HashMap};
use windows_sys::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        UI::{
            Controls::*,
            HiDpi::GetDpiForWindow,
            Input::KeyboardAndMouse::{GetFocus, IsWindowEnabled},
            WindowsAndMessaging::*,
        },
    },
    core::w,
};
thread_local! { static FONTS: RefCell<HashMap<(i32, String), Font>> = RefCell::new(HashMap::new()); }
fn text(hwnd: HWND) -> Vec<u16> {
    unsafe {
        let mut text = vec![0; GetWindowTextLengthW(hwnd).max(0) as usize + 1];
        GetWindowTextW(hwnd, text.as_mut_ptr(), text.len() as i32);
        text
    }
}
pub fn draw(root: HWND, draw: &DRAWITEMSTRUCT) {
    unsafe {
        let dc = draw.hDC;
        let dpi = GetDpiForWindow(root).max(96) as i32;
        let scale = |v| (v * dpi + 48) / 96;
        let mut rect = draw.rcItem;
        let font = SendMessageW(draw.hwndItem, WM_GETFONT, 0, 0) as HFONT;
        let old = SelectObject(dc, font);
        let saved = SaveDC(dc);
        SetBkMode(dc, TRANSPARENT as i32);
        if draw.CtlID == 703 {
            FillRect(dc, &rect, crate::settings_theme::control());
            FrameRect(
                dc,
                &rect,
                if GetFocus() == GetDlgItem(root, 700) {
                    GetSysColorBrush(COLOR_HIGHLIGHT)
                } else {
                    crate::settings_theme::edge()
                },
            );
        } else if draw.CtlID == 600 {
            // Owner drawing can run while the window's resize region is changing.
            // Clip this paint to its current bounds as well as the window region.
            let diameter = scale(10)
                .min(rect.right - rect.left)
                .min(rect.bottom - rect.top)
                .max(1);
            let region = CreateRoundRectRgn(
                rect.left,
                rect.top,
                rect.right + 1,
                rect.bottom + 1,
                diameter,
                diameter,
            );
            if !region.is_null() {
                ExtSelectClipRgn(dc, region, RGN_AND);
                DeleteObject(region);
            }
            FillRect(dc, &rect, crate::settings_theme::notice());
            SetTextColor(dc, crate::settings_theme::notice_text());
            rect.left += scale(6);
            rect.right -= scale(6);
            rect.top += scale(4);
            let label = text(draw.hwndItem);
            DrawTextW(
                dc,
                label.as_ptr(),
                (label.len() - 1) as i32,
                &mut rect,
                DT_LEFT | DT_WORDBREAK | DT_NOPREFIX,
            );
        } else if !GetPropW(draw.hwndItem, w!("Separator")).is_null() {
            FillRect(dc, &rect, crate::settings_theme::edge());
        } else if matches!(draw.CtlType, ODT_COMBOBOX | ODT_LISTBOX) {
            let selected = draw.itemState & ODS_SELECTED != 0;
            FillRect(
                dc,
                &rect,
                GetSysColorBrush(if selected {
                    COLOR_HIGHLIGHT
                } else {
                    COLOR_WINDOW
                }),
            );
            SetTextColor(
                dc,
                GetSysColor(if selected {
                    COLOR_HIGHLIGHTTEXT
                } else {
                    COLOR_WINDOWTEXT
                }),
            );
            if !selected {
                FillRect(dc, &rect, crate::settings_theme::control());
                SetTextColor(dc, crate::settings_theme::text());
            }
            if draw.itemState & ODS_DISABLED != 0 || IsWindowEnabled(draw.hwndItem) == 0 {
                SetTextColor(dc, crate::settings_theme::hint());
            }
            if draw.itemID != u32::MAX {
                let (get_length, get_text) = if draw.CtlType == ODT_LISTBOX {
                    (LB_GETTEXTLEN, LB_GETTEXT)
                } else {
                    (CB_GETLBTEXTLEN, CB_GETLBTEXT)
                };
                let length = SendMessageW(draw.hwndItem, get_length, draw.itemID as usize, 0);
                if length >= 0 {
                    let mut label = vec![0u16; length as usize + 1];
                    SendMessageW(
                        draw.hwndItem,
                        get_text,
                        draw.itemID as usize,
                        label.as_mut_ptr() as isize,
                    );
                    if !GetPropW(draw.hwndItem, w!("FontPicker")).is_null() {
                        let family = if draw.itemID == 0 {
                            "SystemFont".into()
                        } else {
                            String::from_utf16_lossy(&label[..length as usize])
                        };
                        FONTS.with_borrow_mut(|fonts| {
                            if let std::collections::hash_map::Entry::Vacant(entry) =
                                fonts.entry((dpi, family.clone()))
                                && let Ok(font) =
                                    label_font(&family, &label[..length as usize], dpi)
                            {
                                entry.insert(font);
                            }
                            if let Some(font) = fonts.get(&(dpi, family)) {
                                SelectObject(dc, font.0);
                            }
                        });
                    }
                    rect.left += scale(6);
                    rect.right -= scale(4);
                    DrawTextW(
                        dc,
                        label.as_ptr(),
                        length as i32,
                        &mut rect,
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
                    );
                }
            }
        } else if (100..100 + crate::settings_schema::TABS.len() as u32).contains(&draw.CtlID) {
            let selected = GetPropW(draw.hwndItem, w!("SelectedTab")) as usize != 0;
            FillRect(dc, &rect, crate::settings_theme::surface());
            SelectObject(dc, GetStockObject(DC_BRUSH));
            SelectObject(dc, GetStockObject(NULL_PEN));
            SetDCBrushColor(
                dc,
                GetSysColor(if selected {
                    COLOR_HIGHLIGHT
                } else {
                    COLOR_BTNFACE
                }),
            );
            if !selected {
                SetDCBrushColor(
                    dc,
                    if crate::settings_theme::dark() {
                        0x303030
                    } else {
                        0xf5f5f5
                    },
                );
            }
            RoundRect(
                dc,
                rect.left,
                rect.top,
                rect.right,
                rect.bottom,
                scale(12),
                scale(12),
            );
            SetTextColor(
                dc,
                GetSysColor(if selected {
                    COLOR_HIGHLIGHTTEXT
                } else {
                    COLOR_BTNTEXT
                }),
            );
            if !selected {
                SetTextColor(dc, crate::settings_theme::text());
            }
            let label = text(draw.hwndItem);
            rect.left += scale(16);
            rect.right -= scale(16);
            let mut measured = rect;
            DrawTextW(
                dc,
                label.as_ptr(),
                (label.len() - 1) as i32,
                &mut measured,
                DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX,
            );
            rect.top += ((rect.bottom - rect.top - (measured.bottom - measured.top)) / 2).max(0);
            DrawTextW(
                dc,
                label.as_ptr(),
                (label.len() - 1) as i32,
                &mut rect,
                DT_LEFT | DT_WORDBREAK | DT_NOPREFIX,
            );
        } else {
            let id = draw.CtlID as i32;
            let parent = GetParent(draw.hwndItem);
            let channel = |slot| {
                String::from_utf16_lossy(&text(GetDlgItem(parent, id + slot)))
                    .trim_end_matches('\0')
                    .parse::<u32>()
                    .unwrap_or(255)
                    .min(255)
            };
            let has_alpha = !GetDlgItem(parent, id + 6).is_null();
            let channels = [
                channel(3),
                channel(4),
                channel(5),
                if has_alpha { channel(6) } else { 255 },
            ];
            let foreground = channels[0] | (channels[1] << 8) | (channels[2] << 16);
            let tile = scale(10).max(1);
            for y in (rect.top..rect.bottom).step_by(tile as usize) {
                for x in (rect.left..rect.right).step_by(tile as usize) {
                    let base = if (x / tile + y / tile) % 2 == 0 {
                        crate::colors::CHECKER_LIGHT
                    } else {
                        crate::colors::CHECKER_SHADE
                    };
                    SetDCBrushColor(
                        dc,
                        crate::colors::composite(foreground, base, channels[3] as u8),
                    );
                    FillRect(
                        dc,
                        &RECT {
                            left: x,
                            top: y,
                            right: (x + tile).min(rect.right),
                            bottom: (y + tile).min(rect.bottom),
                        },
                        GetStockObject(DC_BRUSH),
                    );
                }
            }
            let mut label = format!("#{:02X}{:02X}{:02X}", channels[0], channels[1], channels[2]);
            if has_alpha {
                label.push_str(&format!("   A: {}", channels[3]));
            }
            let label = wide(label);
            SetTextColor(
                dc,
                crate::colors::swatch_text(foreground, channels[3] as u8),
            );
            DrawTextW(
                dc,
                label.as_ptr(),
                (label.len() - 1) as i32,
                &mut rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            FrameRect(dc, &draw.rcItem, crate::settings_theme::edge());
        }
        RestoreDC(dc, saved);
        SelectObject(dc, old);
    }
}
pub fn reset_fonts() {
    FONTS.with_borrow_mut(HashMap::clear);
}
fn label_font(family: &str, label: &[u16], dpi: i32) -> Result<Font, String> {
    let font = Font::new(family, 0, dpi, false, false)?;
    if font.supports_label(label) {
        Ok(font)
    } else {
        Font::new("SystemFont", 0, dpi, false, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_and_emoji_font_names_remain_readable() {
        let names = crate::fonts::families();
        for family in ["Wingdings", "Noto Color Emoji", "Noto Emoji"] {
            if !names.iter().any(|name| name == family) {
                continue;
            }
            let label: Vec<u16> = family.encode_utf16().collect();
            assert!(
                label_font(family, &label, 96)
                    .unwrap()
                    .supports_label(&label),
                "Unreadable font name: {family}"
            );
        }
    }
}
