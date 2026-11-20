//! Default Fluent neutrals and native Windows selection colors.
use windows_sys::Win32::Graphics::Gdi::{COLOR_WINDOW, COLOR_WINDOWTEXT, GetSysColor};

pub const TEXT_SECONDARY_LIGHT: u32 = 0x616161;
pub const TEXT_SECONDARY_DARK: u32 = 0xadadad;
pub const TEXT_PRIMARY_LIGHT: u32 = 0x242424;
pub const WHITE: u32 = 0xffffff;
pub const BLACK: u32 = 0;
pub const CHECKER_LIGHT: u32 = 0xf5f5f5;
pub const CHECKER_SHADE: u32 = 0xe0e0e0;
pub const SHADOW_LIGHT_ALPHA: u8 = 36; // Fluent colorNeutralShadowKey: 14%.
pub const SHADOW_DARK_ALPHA: u8 = 71; // Fluent colorNeutralShadowKey: 28%.

pub fn accent() -> u32 {
    let mut argb = 0;
    let mut opaque = 0;
    // SAFETY: DWM only writes two stack-owned scalar values.
    if unsafe { windows_sys::Win32::Graphics::Dwm::DwmGetColorizationColor(&mut argb, &mut opaque) }
        >= 0
    {
        colorref(argb & 0x00ff_ffff)
    } else {
        unsafe { GetSysColor(windows_sys::Win32::Graphics::Gdi::COLOR_HIGHLIGHT) }
    }
}

pub fn secondary_text() -> u32 {
    let background = unsafe { GetSysColor(COLOR_WINDOW) };
    let candidate = if luminance(background) < 0.5 {
        TEXT_SECONDARY_DARK
    } else {
        TEXT_SECONDARY_LIGHT
    };
    if contrast(candidate, background) >= 4.5 {
        candidate
    } else {
        unsafe { GetSysColor(COLOR_WINDOWTEXT) }
    }
}

// COLORREF is 0x00BBGGRR. Shared helpers keep channel order consistent with TOML #RRGGBB.
pub const fn colorref(rgb: u32) -> u32 {
    (rgb >> 16) | (rgb & 0xff00) | ((rgb & 255) << 16)
}
pub fn composite(foreground: u32, background: u32, alpha: u8) -> u32 {
    let channel = |shift: u32| {
        let foreground = (foreground >> shift) & 255u32;
        let background = (background >> shift) & 255u32;
        (foreground * u32::from(alpha) + background * (255 - u32::from(alpha)) + 127) / 255
    };
    channel(0) | (channel(8) << 8) | (channel(16) << 16)
}
fn luminance(color: u32) -> f64 {
    let channel = |shift: u32| {
        let value = f64::from((color >> shift) & 255u32) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    channel(0) * 0.2126 + channel(8) * 0.7152 + channel(16) * 0.0722
}
pub fn contrast(a: u32, b: u32) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}
pub fn tip_border(background: u32) -> u32 {
    let edge = if contrast(BLACK, background) > contrast(WHITE, background) {
        BLACK
    } else {
        WHITE
    };
    composite(edge, background, 72)
}
pub fn swatch_text(color: u32, alpha: u8) -> u32 {
    let backgrounds = [CHECKER_LIGHT, CHECKER_SHADE].map(|base| composite(color, base, alpha));
    [TEXT_PRIMARY_LIGHT, WHITE, BLACK]
        .into_iter()
        .max_by(|a, b| {
            let worst = |color| {
                backgrounds
                    .iter()
                    .map(|base| contrast(color, *base))
                    .fold(f64::INFINITY, f64::min)
            };
            worst(*a).total_cmp(&worst(*b))
        })
        .unwrap_or(BLACK)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn text_contrasts_with_actual_default_surfaces() {
        let config = crate::config::Config::parse(&crate::config::help().parse().unwrap()).unwrap();
        assert!(contrast(config.colors[0], config.colors[1]) >= 7.0);
        assert!(contrast(config.colors[2], config.colors[3]) >= 7.0);
        assert!(contrast(TEXT_SECONDARY_LIGHT, WHITE) >= 4.5);
        assert_eq!(colorref(0x0f6cbd), 0xbd6c0f);
        for color in [colorref(0x0f6cbd), 0xffffff, 0, 0x808080] {
            for alpha in [0, 120, 255] {
                let text = swatch_text(color, alpha);
                for background in [CHECKER_LIGHT, CHECKER_SHADE] {
                    assert!(contrast(text, composite(color, background, alpha)) >= 4.5);
                }
            }
        }
    }
}
