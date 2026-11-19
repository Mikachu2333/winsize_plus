use crate::{cli::Switch, layout::Layout};
use std::{
    collections::HashMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};
use windows_sys::Win32::{
    Foundation::*, Globalization::GetUserDefaultUILanguage, System::Threading::*,
};

fn system_zh() -> bool {
    (unsafe { GetUserDefaultUILanguage() } & 0x3ff) == 4
}
pub fn help() -> &'static str {
    include_str!("example.en.toml")
}
pub fn path() -> Result<PathBuf, String> {
    Ok(std::env::current_exe()
        .map_err(|err| err.to_string())?
        .with_file_name("winsizep_config.toml"))
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub class: String,
    pub title: String,
}
impl Identity {
    pub fn matches(&self, window: &Self) -> bool {
        (self.name.is_empty() || self.name.eq_ignore_ascii_case(&window.name))
            && (self.class.is_empty() || self.class == window.class)
            && (self.title.is_empty() || self.title == window.title)
    }
}
#[derive(Clone, Copy)]
pub struct Line {
    pub width: i32,
    pub gap: i32,
    pub color: u32,
    pub opacity: u8,
}
pub struct Config {
    pub zh: bool,
    pub alt: String,
    pub gesture: String,
    pub double_ms: u32,
    pub elevate: bool,
    pub autostart: bool,
    pub preview: bool,
    pub show: String,
    pub duration: u32,
    pub theme: String,
    pub colors: [u32; 4],
    pub font: String,
    pub font_size: i32,
    pub font_fallbacks: Vec<String>,
    pub font_bold: bool,
    pub font_italic: bool,
    pub region_font: String,
    pub region_fallbacks: Vec<String>,
    pub region_font_size: i32,
    pub region_bold: bool,
    pub region_italic: bool,
    pub window_format: String,
    pub time_format: String,
    pub outline: Line,
    pub outline_dark: u32,
    pub highlight_opacity: u8,
    pub move_button: Option<u32>,
    pub resize_button: Option<u32>,
    pub warnings: Vec<String>,
    pub preset_names: HashMap<String, String>,
    pub lines: [Line; 2],
    pub fps: f64,
    pub presets: HashMap<String, Layout>,
    pub keys: HashMap<u32, String>,
    pub commands: HashMap<String, String>,
    pub grid: HashMap<u32, String>,
    pub exclusions: Vec<Identity>,
}
pub fn vk(text: &str) -> Option<u32> {
    let text = text.to_ascii_lowercase();
    if text.len() == 1 && text.as_bytes()[0].is_ascii_alphanumeric() {
        return Some(text.as_bytes()[0].to_ascii_uppercase() as u32);
    }
    if let Some(number) = text
        .strip_prefix('f')
        .and_then(|number| number.parse::<u32>().ok())
        .filter(|number| (1..=24).contains(number))
    {
        return Some(0x6f + number);
    }
    if let Some(number) = text.strip_prefix("vk:") {
        return u32::from_str_radix(number, 16)
            .ok()
            .filter(|number| ((7..255).contains(number) || *number == 3) && *number != 27);
    }
    Some(match text.as_str() {
        "grave" | "`" | "~" | "～" => 0xc0,
        "minus" | "-" | "－" | "—" => 0xbd,
        "equal" | "=" | "＝" => 0xbb,
        "backslash" | "\\" | "、" | "＼" => 0xdc,
        "space" => 0x20,
        "tab" => 9,
        "semicolon" | ";" | "；" | "：" => 0xba,
        "comma" | "," | "，" | "《" => 0xbc,
        "period" | "." | "。" | "》" => 0xbe,
        "slash" | "/" | "？" => 0xbf,
        "insert" => 0x2d,
        _ => return None,
    })
}

fn merge(table: &mut Table, defaults: &Table, named: bool) -> bool {
    if named {
        return false; // User-defined maps may intentionally be empty or omit defaults.
    }
    let mut changed = false;
    for (key, item) in defaults {
        if let Some(existing) = table.get_mut(key) {
            if let (Some(child), Some(default)) = (existing.as_table_mut(), item.as_table()) {
                changed |= merge(
                    child,
                    default,
                    matches!(key, "presets" | "keys" | "commands" | "grid"),
                )
            }
        } else if !named
            || !table
                .iter()
                .any(|(existing, _)| existing.eq_ignore_ascii_case(key))
        {
            table.insert(key, item.clone());
            changed = true
        }
    }
    changed
}

pub const ACTIONS: &[&str] = &[
    "top",
    "ignore",
    "help",
    "version",
    "command",
    "restart",
    "quit",
    "time",
    "monitor",
    "guide",
    "horizontal",
    "vertical",
    "cross",
    "reload",
    "settings",
    "borders",
];
pub const ALIASES: &[(&str, &str)] = &[
    ("ver", "version"),
    ("h", "help"),
    ("exit", "quit"),
    ("reboot", "restart"),
    ("pin", "top"),
    ("exclude", "ignore"),
    ("screen", "monitor"),
];

pub fn command_name(text: &str) -> String {
    let text: String = text
        .chars()
        .map(|ch| match ch {
            'Ａ'..='Ｚ' | 'ａ'..='ｚ' | '０'..='９' => {
                char::from_u32(ch as u32 - 0xfee0).unwrap_or(ch)
            }
            '，' | '－' | '—' => '-',
            '＿' => '_',
            '　' => ' ',
            _ => ch,
        })
        .collect();
    text.trim().to_lowercase()
}

pub fn standard_name(name: &str) -> String {
    if let Some((_, action)) = ALIASES.iter().find(|(alias, _)| *alias == name) {
        return standard_name(action);
    }
    let mut chars = name.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}
fn is_builtin_action(name: &str) -> bool {
    ACTIONS.contains(&name)
}

fn canonicalize(doc: &mut DocumentMut) {
    let names: HashMap<String, String> = doc
        .get("presets")
        .and_then(Item::as_table)
        .into_iter()
        .flat_map(|table| table.iter())
        .map(|(key, _)| (key.to_ascii_lowercase(), key.to_owned()))
        .collect();
    // Preserve custom names as authored; normalize action references and built-in spelling.
    for section in ["keys", "commands", "grid"] {
        if let Some(table) = doc.get_mut(section).and_then(Item::as_table_mut) {
            for (_, item) in table.iter_mut() {
                if let Some(text) = item.as_str() {
                    let lower = text.to_ascii_lowercase();
                    let canonical = if is_builtin_action(&lower) {
                        Some(standard_name(&lower))
                    } else {
                        names.get(&lower).cloned()
                    };
                    if let Some(canonical) = canonical
                        && canonical != text
                    {
                        let decor = item.as_value().unwrap().decor().clone();
                        *item = value(canonical);
                        *item.as_value_mut().unwrap().decor_mut() = decor;
                    }
                }
            }
        }
    }
}

impl Config {
    pub fn cell(&self, monitor: u32, cell: u32) -> Option<&str> {
        [monitor * 10 + cell, cell]
            .iter()
            .find_map(|key| self.grid.get(key))
            .filter(|name| !name.is_empty())
            .map(String::as_str)
    }
    pub fn preset(&self, name_or_key: &str) -> Option<&Layout> {
        self.presets
            .get(&name_or_key.to_ascii_lowercase())
            .or_else(|| {
                let action = self.keys.get(&vk(name_or_key)?)?;
                if is_builtin_action(action) {
                    return None;
                }
                self.presets.get(action)
            })
    }

    pub fn tr<'a>(&self, en: &'a str, zh: &'a str) -> &'a str {
        if self.zh { zh } else { en }
    }
    pub fn display_name<'a>(&'a self, name: &'a str) -> &'a str {
        self.preset_names
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
            .unwrap_or(name)
    }
    pub fn action(&self, text: &str) -> String {
        let name = command_name(text);
        self.commands.get(&name).cloned().unwrap_or_else(|| {
            ALIASES
                .iter()
                .find(|(alias, _)| *alias == name)
                .map(|(_, action)| (*action).to_owned())
                .unwrap_or(name)
        })
    }
    pub fn help_text(&self) -> String {
        let button = |button| match button {
            Some(1) => self.tr("left button", "左键"),
            Some(2) => self.tr("right button", "右键"),
            Some(4) => self.tr("middle button", "中键"),
            Some(5) => self.tr("side button 1", "侧键 1"),
            Some(6) => self.tr("side button 2", "侧键 2"),
            _ => "",
        };
        let mut lines = vec![
            self.tr(
                "WinSizeP — Esc closes this help",
                "WinSizeP 帮助：按 Esc 关闭",
            )
            .to_owned(),
        ];
        for (button_id, en, zh) in [
            (self.move_button, "move window", "移动窗口"),
            (
                self.resize_button,
                "resize from bottom right",
                "调整窗口大小（固定左上角）",
            ),
        ] {
            if button_id.is_some() {
                lines.push(format!(
                    "Alt + {} {} = {}",
                    button(button_id),
                    self.tr("drag", "拖动"),
                    self.tr(en, zh)
                ));
            }
        }
        lines.push(
            self.tr(
                "Alt + right click = place by the 3×3 grid",
                "Alt + 右键点击屏幕位置 = 九宫格位置调整",
            )
            .into(),
        );
        lines.push(
            self.tr(
                "Hold Alt + a key, or double-tap Alt then press a key:",
                "按住 Alt + 按键，或双击 Alt 后按键：",
            )
            .into(),
        );
        let mut bindings: Vec<_> = self.keys.iter().collect();
        bindings.sort_by_key(|(key, _)| **key);
        for (key, action) in bindings {
            let key = match *key {
                0xc0 => "`".into(),
                0xbd => "-".into(),
                0xbb => "=".into(),
                0xdc => "\\".into(),
                0x70..=0x87 => format!("F{}", key - 0x6f),
                _ => char::from_u32(*key)
                    .map(|c| c.to_string())
                    .unwrap_or_default(),
            };
            let title = match action.as_str() {
                "top" => self.tr("toggle topmost", "置顶 / 取消置顶"),
                "ignore" => self.tr("exclude window", "忽略窗口"),
                "guide" | "cross" => self.tr("toggle crosshair", "显示 / 隐藏十字线"),
                "horizontal" => self.tr("horizontal crosshair", "水平十字线"),
                "vertical" => self.tr("vertical crosshair", "垂直十字线"),
                "borders" => self.tr("toggle region outlines", "显示 / 隐藏框线"),
                "command" => self.tr("run command", "运行指令"),
                "help" => self.tr("help", "帮助"),
                _ => self.display_name(action),
            };
            lines.push(format!("Alt + {key} = {title}"));
        }
        lines.push(self.tr("Aliases: Version = Ver; Help = H; Quit = Exit; Restart = Reboot; Top = Pin; Ignore = Exclude; Monitor = Screen", "内置别名：Version = Ver；Help = H；Quit = Exit；Restart = Reboot；Top = Pin；Ignore = Exclude；Monitor = Screen").into());
        lines.join("\n")
    }
    pub fn parse(doc: &DocumentMut) -> Result<Self, String> {
        let field = |section: &str, key: &str| {
            doc.get(section)
                .and_then(|item| item.get(key))
                .ok_or_else(|| format!("{section}.{key}"))
        };
        let text = |section: &str, key: &str| {
            field(section, key)?
                .as_str()
                .ok_or_else(|| format!("{section}.{key}"))
        };
        let choice = |section: &str, key: &str, options: &[&str]| {
            let val = text(section, key)?;
            if options.contains(&val) {
                Ok(val.to_string())
            } else {
                Err(format!("{section}.{key}: {val}"))
            }
        };
        let number = |section: &str, key: &str, min: i64, max: i64| {
            field(section, key)?
                .as_integer()
                .filter(|val| (min..=max).contains(val))
                .ok_or_else(|| format!("{section}.{key} ({min}..{max})"))
        };
        let boolean = |section: &str, key: &str| {
            field(section, key)?
                .as_bool()
                .ok_or_else(|| format!("{section}.{key}"))
        };
        let color = |section: &str, key: &str| {
            let hex = text(section, key)?.trim_start_matches('#');
            let rgb = u32::from_str_radix(hex, 16)
                .ok()
                .filter(|_| hex.len() == 6)
                .ok_or_else(|| format!("{section}.{key}"))?;
            Ok::<_, String>(crate::colors::colorref(rgb))
        };
        let line = |section: &str| {
            Ok::<_, String>(Line {
                width: number(section, "width", 1, 256)? as i32,
                gap: number(section, "gap", 0, 4096)? as i32,
                color: color(section, "color")?,
                opacity: number(section, "opacity", 0, 255)? as u8,
            })
        };
        let entries = |section: &str| {
            doc.get(section)
                .and_then(Item::as_table)
                .ok_or_else(|| section.to_string())
        };
        let mut presets = HashMap::new();
        let mut preset_names = HashMap::new();
        for (key, item) in entries("presets")? {
            if key.eq_ignore_ascii_case("esc")
                || key.eq_ignore_ascii_case("escape")
                || key.is_empty()
            {
                return Err(format!("presets.{key}: reserved name"));
            }
            let layout = item
                .as_str()
                .ok_or_else(|| format!("presets.{key}"))?
                .parse()
                .map_err(|err: String| format!("presets.{key}: {err}"))?;
            if presets.insert(key.to_ascii_lowercase(), layout).is_some() {
                return Err(format!("presets.{key}: duplicate name"));
            }
            preset_names.insert(key.to_ascii_lowercase(), key.to_owned());
        }
        let action = |item: &Item| {
            let name = item
                .as_str()
                .ok_or_else(|| item.to_string())?
                .to_ascii_lowercase();
            if name.is_empty() || presets.contains_key(&name) || is_builtin_action(&name) {
                Ok(name)
            } else {
                Err(format!("Unknown action / 未知动作: {name}"))
            }
        };
        let mut keys = HashMap::new();
        for (key, item) in entries("keys")? {
            let name = action(item).map_err(|err| format!("keys.{key}: {err}"))?;
            let vk = vk(key).ok_or_else(|| format!("keys.{key}: invalid or reserved key"))?;
            if !name.is_empty() && keys.insert(vk, name).is_some() {
                return Err(format!("keys.{key}: duplicate key"));
            }
        }
        let mut commands = HashMap::new();
        for (key, item) in entries("commands")? {
            if !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b' '))
            {
                return Err(format!("commands.{key}"));
            }
            if key.eq_ignore_ascii_case("esc") || key.eq_ignore_ascii_case("escape") {
                return Err(format!("commands.{key}: reserved name"));
            }
            commands.insert(
                key.to_ascii_lowercase(),
                action(item).map_err(|err| format!("commands.{key}: {err}"))?,
            );
        }
        let mut grid = HashMap::new();
        for (key, item) in entries("grid")? {
            let name = item
                .as_str()
                .ok_or_else(|| format!("grid.{key}"))?
                .to_ascii_lowercase();
            if !name.is_empty() && !presets.contains_key(&name) {
                return Err(format!("grid.{key}: {name}"));
            }
            let number: u32 = key.parse().map_err(|_| format!("grid.{key}"))?;
            if number == 0 || number.is_multiple_of(10) || number > 9999 {
                return Err(format!("grid.{key}: expected monitor * 10 + cell"));
            }
            grid.insert(number, name);
        }
        let mut exclusions = Vec::new();
        if let Some(item) = doc.get("exclusions") {
            for table in item.as_array_of_tables().ok_or("[[exclusions]]")? {
                let get = |key: &str| {
                    table
                        .get(key)
                        .map(|item| item.as_str().ok_or_else(|| format!("exclusions.{key}")))
                        .transpose()
                        .map(|value| value.unwrap_or("").to_string())
                };
                let id = Identity {
                    name: get("name")?,
                    class: get("class")?,
                    title: get("title")?,
                };
                if id == Identity::default() {
                    return Err("empty exclusion / 空排除项".into());
                }
                exclusions.push(id);
            }
        }
        let language = choice("general", "language", &["auto", "zh-CN", "en"])?;
        let fallbacks = |section| -> Result<Vec<String>, String> {
            let items = field(section, "font_fallbacks")?
                .as_array()
                .ok_or_else(|| format!("{section}.font_fallbacks"))?;
            if items.len() > 3 {
                return Err(format!("{section}.font_fallbacks: at most three fonts"));
            }
            items
                .iter()
                .map(|v| {
                    v.as_str()
                        .filter(|v| !v.is_empty() && !v.starts_with('@'))
                        .map(str::to_owned)
                        .ok_or_else(|| format!("{section}.font_fallbacks"))
                })
                .collect()
        };
        let font = |section| -> Result<String, String> {
            let name = text(section, "font")?;
            if name.is_empty() || name.starts_with('@') {
                return Err(format!("{section}.font"));
            }
            Ok(name.to_owned())
        };
        let button = |key: &str| -> Result<Option<u32>, String> {
            let enabled = boolean("shortcuts", &format!("{key}_enabled"))?;
            let name = choice(
                "shortcuts",
                &format!("{key}_button"),
                &["left", "right", "middle", "x1", "x2"],
            )?;
            Ok(enabled.then_some(match name.as_str() {
                "left" => 1,
                "right" => 2,
                "middle" => 4,
                "x1" => 5,
                _ => 6,
            }))
        };
        Ok(Self {
            zh: language == "zh-CN" || (language == "auto" && system_zh()),
            alt: choice("shortcuts", "alt", &["left", "right", "both"])?,
            gesture: choice("shortcuts", "gesture", &["left", "right", "both", "off"])?,
            double_ms: unsafe {
                windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime()
            },
            elevate: boolean("general", "elevate")?,
            autostart: boolean("general", "autostart")?,
            preview: boolean("general", "preview")?,
            show: choice("tip", "show", &["off", "time", "monitor", "window"])?,
            duration: number("tip", "duration_ms", 100, 120000)? as u32,
            theme: choice("tip", "theme", &["system", "dark", "light"])?,
            colors: [
                color("tip", "background")?,
                color("tip", "foreground")?,
                color("tip", "dark_background")?,
                color("tip", "dark_foreground")?,
            ],
            font: font("tip")?,
            font_size: number("tip", "font_size", 6, 128)? as i32,
            font_fallbacks: fallbacks("tip")?,
            font_bold: boolean("tip", "font_bold")?,
            font_italic: boolean("tip", "font_italic")?,
            region_font: font("regions")?,
            region_fallbacks: fallbacks("regions")?,
            region_font_size: number("regions", "font_size", 6, 128)? as i32,
            region_bold: boolean("regions", "font_bold")?,
            region_italic: boolean("regions", "font_italic")?,
            window_format: text("tip", "window_format")?.into(),
            time_format: text("tip", "time_format")?.into(),
            outline: Line {
                width: number("outline", "width", 1, 16)? as i32,
                gap: 0,
                color: color("outline", "color")?,
                opacity: number("outline", "opacity", 0, 255)? as u8,
            },
            outline_dark: color("outline", "dark_color")?,
            highlight_opacity: number("outline", "highlight_opacity", 0, 255)? as u8,
            move_button: button("move")?,
            resize_button: button("resize")?,
            warnings: Vec::new(),
            preset_names,
            lines: [line("horizontal")?, line("vertical")?],
            fps: {
                let rate = field("crosshair", "fps")?;
                rate.as_integer()
                    .ok_or("crosshair.fps: expected integer")?
                    .clamp(24, 1000) as f64
            },
            presets,
            keys,
            commands,
            grid,
            exclusions,
        })
    }
}

#[cfg(test)]
pub fn encode(text: &str) -> Vec<u8> {
    std::iter::once(0xfeffu16)
        .chain(text.encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect()
}
fn decode(bytes: &[u8]) -> Result<String, String> {
    if let Some(rest) = bytes
        .strip_prefix(&[255, 254])
        .or_else(|| bytes.strip_prefix(&[254, 255]))
    {
        if rest.len() % 2 != 0 {
            return Err("Incomplete UTF-16 configuration / UTF-16 配置不完整".into());
        }
        let big = bytes[0] == 254;
        let units: Vec<_> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .copied()
            .map(|unit| {
                if big {
                    u16::from_be_bytes(unit)
                } else {
                    u16::from_le_bytes(unit)
                }
            })
            .collect();
        return String::from_utf16(&units).map_err(|err| err.to_string());
    }
    String::from_utf8(
        bytes
            .strip_prefix(&[239, 187, 191])
            .unwrap_or(bytes)
            .to_vec(),
    )
    .map_err(|err| err.to_string())
}

// Only syntax outside strings/comments is repaired. A successfully parsed file never enters here.
fn repair_punctuation(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut result = String::with_capacity(text.len());
    let mut index = 0;
    let mut quote = None;
    let mut triple = false;
    let mut escaped = false;
    let mut comment = false;
    while index < chars.len() {
        let ch = chars[index];
        if comment {
            result.push(ch);
            comment = ch != '\n';
        } else if let Some((end, ascii)) = quote {
            if ch == end && !escaped {
                if !triple || chars.get(index..index + 3) == Some(&[end, end, end]) {
                    result.push(ascii);
                    if triple {
                        result.push(ascii);
                        result.push(ascii);
                        index += 2;
                    }
                    quote = None;
                } else {
                    result.push(ch);
                }
            } else {
                result.push(ch);
            }
            escaped = ascii == '"' && ch == '\\' && !escaped;
        } else {
            match ch {
                '#' => {
                    result.push(ch);
                    comment = true;
                }
                '"' | '\'' | '“' | '‘' => {
                    let (end, ascii) = match ch {
                        '“' => ('”', '"'),
                        '‘' => ('’', '\''),
                        _ => (ch, ch),
                    };
                    triple = ch == ascii && chars.get(index..index + 3) == Some(&[ch, ch, ch]);
                    quote = Some((end, ascii));
                    escaped = false;
                    result.push(ascii);
                    if triple {
                        result.push(ascii);
                        result.push(ascii);
                        index += 2;
                    }
                }
                _ => result.push(match ch {
                    '【' | '「' | '『' => '[',
                    '】' | '」' | '』' => ']',
                    '：' => '=',
                    '；' | '，' => ',',
                    '。' => '.',
                    '～' => '~',
                    '（' => '(',
                    '）' => ')',
                    '！' => '!',
                    '《' => '<',
                    '》' => '>',
                    '？' => '?',
                    '＝' => '=',
                    '　' => ' ',
                    _ => ch,
                }),
            }
        }
        index += 1;
    }
    result
}

fn document(text: &str) -> Result<(DocumentMut, bool), String> {
    let spaces = text.replace('\t', "    ");
    let source = match spaces.parse::<DocumentMut>() {
        Ok(_) => spaces,
        Err(original) => {
            let repaired = repair_punctuation(&spaces);
            repaired
                .parse::<DocumentMut>()
                .map_err(|_| original.to_string())?;
            repaired
        }
    };
    let clean = english_comments(&source);
    let doc = clean
        .parse::<DocumentMut>()
        .map_err(|err| err.to_string())?;
    Ok((doc, clean != text))
}

fn english_comments(text: &str) -> String {
    let chars: Vec<_> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    let mut quote = None;
    let mut triple = false;
    let mut escaped = false;
    while index < chars.len() {
        let ch = chars[index];
        if let Some(end) = quote {
            out.push(ch);
            if ch == end
                && !escaped
                && (!triple || chars.get(index..index + 3) == Some(&[end, end, end]))
            {
                if triple {
                    out.push(end);
                    out.push(end);
                    index += 2;
                }
                quote = None;
            }
            escaped = end == '"' && ch == '\\' && !escaped;
        } else if matches!(ch, '"' | '\'') {
            quote = Some(ch);
            triple = chars.get(index..index + 3) == Some(&[ch, ch, ch]);
            escaped = false;
            out.push(ch);
            if triple {
                out.push(ch);
                out.push(ch);
                index += 2;
            }
        } else if ch == '#' {
            let end = chars[index..]
                .iter()
                .position(|ch| *ch == '\n')
                .map_or(chars.len(), |n| index + n);
            if chars[index..end].iter().all(char::is_ascii) {
                out.extend(&chars[index..end]);
            }
            index = end;
            continue;
        } else {
            out.push(ch);
        }
        index += 1;
    }
    out
}

fn migrate(doc: &mut DocumentMut) {
    if let Some(keys) = doc.get_mut("keys").and_then(Item::as_table_mut) {
        let old = keys
            .iter()
            .find(|(key, item)| {
                key.eq_ignore_ascii_case("f1")
                    && item
                        .as_str()
                        .is_some_and(|name| name.eq_ignore_ascii_case("help"))
            })
            .map(|(key, _)| key.to_owned());
        if let Some(old) = old
            && !keys.iter().any(|(key, _)| key.eq_ignore_ascii_case("h"))
        {
            keys.remove(&old);
            keys.insert("H", value("Help"));
        }
    }
    for key in ["alt", "gesture"] {
        if let Some(item) = doc
            .get_mut("general")
            .and_then(Item::as_table_mut)
            .and_then(|table| table.remove(key))
        {
            if !doc.contains_key("shortcuts") {
                doc["shortcuts"] = Item::Table(Table::new());
            }
            if doc["shortcuts"].get(key).is_none() {
                doc["shortcuts"][key] = item;
            }
        }
    }
    if let Some(table) = doc.get_mut("general").and_then(Item::as_table_mut) {
        for key in [
            "double_ms",
            "wait_ms",
            "restart_on_change",
            "gesture_button",
            "auto_save",
        ] {
            table.remove(key);
        }
    }
}

fn recover(doc: &mut DocumentMut) -> Result<Config, String> {
    let defaults = help()
        .parse::<DocumentMut>()
        .map_err(|err| err.to_string())?;
    let mut warnings = Vec::new();
    // Fixed sections have schemas; user maps are preserved, including intentional deletions.
    for (key, default) in defaults.as_table() {
        if doc.get(key).is_some_and(|v| !v.is_table()) {
            doc[key] = default.clone();
            warnings.push(key.to_owned());
        }
    }
    merge(doc.as_table_mut(), defaults.as_table(), false);
    canonicalize(doc);
    if let Some(rate) = doc
        .get("crosshair")
        .and_then(|s| s.get("fps"))
        .and_then(Item::as_integer)
        && rate != rate.clamp(24, 1000)
    {
        doc["crosshair"]["fps"] = value(rate.clamp(24, 1000));
    }
    // One invalid field cannot prevent independent settings from being applied.
    for _ in 0..1024 {
        match Config::parse(doc) {
            Ok(mut cfg) => {
                cfg.warnings = warnings;
                return Ok(cfg);
            }
            Err(error) => {
                let matched = doc
                    .as_table()
                    .iter()
                    .flat_map(|(section, item)| {
                        item.as_table().into_iter().flat_map(move |table| {
                            table.iter().map(move |(key, _)| format!("{section}.{key}"))
                        })
                    })
                    .filter(|field| {
                        error.strip_prefix(field).is_some_and(|rest| {
                            rest.is_empty() || rest.starts_with([':', ' ', '('])
                        })
                    })
                    .max_by_key(String::len);
                let field = matched
                    .as_deref()
                    .unwrap_or_else(|| error.split([':', ' ', '(']).next().unwrap_or_default());
                if field.starts_with("exclusions") || field == "[[exclusions]]" || field == "empty"
                {
                    doc.remove("exclusions");
                    warnings.push("exclusions".into());
                    continue;
                }
                let Some((section, key)) = field.split_once('.') else {
                    return Err(error);
                };
                if error.contains("duplicate")
                    && let Some(table) = doc.get_mut(section).and_then(Item::as_table_mut)
                {
                    table.remove(key);
                    warnings.push(field.to_owned());
                    continue;
                }
                let replacement = defaults.get(section).and_then(|s| s.get(key)).cloned();
                if let Some(default) = replacement {
                    if doc
                        .get(section)
                        .and_then(|s| s.get(key))
                        .is_some_and(|old| old.to_string() == default.to_string())
                    {
                        // A removed preset can invalidate an otherwise default binding.
                        if matches!(section, "keys" | "commands" | "grid") {
                            doc[section][key] = value("");
                        } else {
                            return Err(error);
                        }
                    } else {
                        doc[section][key] = default;
                    }
                } else if let Some(table) = doc.get_mut(section).and_then(Item::as_table_mut) {
                    table.remove(key);
                } else {
                    return Err(error);
                }
                warnings.push(field.to_owned());
            }
        }
    }
    Err("Too many invalid settings / 配置错误过多".into())
}

fn semantic(item: Option<&Item>) -> String {
    match item {
        Some(Item::Value(v)) => v.clone().decorated("", "").to_string(),
        Some(other) => other.to_string(),
        None => "<absent>".into(),
    }
}

fn merge_edits(base: &Table, draft: &Table, disk: &mut Table) {
    let keys: std::collections::BTreeSet<_> = base
        .iter()
        .chain(draft.iter())
        .map(|(key, _)| key.to_owned())
        .collect();
    for key in keys {
        let before = base.get(&key);
        let wanted = draft.get(&key);
        if semantic(before) == semantic(wanted) {
            continue;
        }
        if let (Some(a), Some(b), Some(c)) = (
            before.and_then(Item::as_table),
            wanted.and_then(Item::as_table),
            disk.get_mut(&key).and_then(Item::as_table_mut),
        ) {
            merge_edits(a, b, c);
        } else if semantic(disk.get(&key)) == semantic(before) {
            match wanted {
                Some(item) => {
                    disk.insert(&key, item.clone());
                }
                None => {
                    disk.remove(&key);
                }
            }
        }
    }
}

struct Lock(HANDLE);
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}
fn lock() -> Result<Lock, String> {
    unsafe {
        let handle = crate::security::config_mutex()?;
        if matches!(
            WaitForSingleObject(handle, 5000),
            WAIT_OBJECT_0 | WAIT_ABANDONED
        ) {
            Ok(Lock(handle))
        } else {
            CloseHandle(handle);
            Err("Config busy / 配置正忙".into())
        }
    }
}

fn write(path: &Path, text: &str) -> io::Result<()> {
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let result = (|| {
        file.write_all(text.replace('\t', "    ").as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
pub fn read_settings(path: &Path) -> Result<String, String> {
    let _lock = lock()?;
    decode(&fs::read(path).map_err(|err| err.to_string())?)
}
pub fn requested_elevation(path: &Path) -> Result<bool, String> {
    let text = match fs::read(path) {
        Ok(bytes) => decode(&bytes)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => help().to_owned(),
        Err(error) => return Err(error.to_string()),
    };
    let (mut doc, _) = document(&text)?;
    migrate(&mut doc);
    Ok(recover(&mut doc)?.elevate)
}

pub fn save_settings(path: &Path, original: &str, doc: &DocumentMut) -> Result<String, String> {
    Config::parse(doc)?;
    let _lock = lock()?;
    let bytes = fs::read(path).map_err(|err| err.to_string())?;
    let (base, _) = document(original)?;
    let (mut disk, _) = document(&decode(&bytes)?)?;
    merge_edits(base.as_table(), doc.as_table(), disk.as_table_mut());
    migrate(&mut disk);
    recover(&mut disk)?;
    let saved = disk.to_string().replace('\t', "    ");
    write(path, &saved).map_err(|err| err.to_string())?;
    Ok(saved)
}

pub fn load(path: &Path, edit: Option<(&Identity, Switch)>) -> Result<Config, String> {
    let _lock = lock()?;
    let (text, new) = match fs::read(path) {
        Ok(bytes) => (decode(&bytes)?, false),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let legacy = path.with_file_name("winsizep_config");
            match fs::read(legacy) {
                Ok(bytes) => (decode(&bytes)?, true),
                Err(err) if err.kind() == io::ErrorKind::NotFound => (help().into(), true),
                Err(err) => return Err(err.to_string()),
            }
        }
        Err(err) => return Err(err.to_string()),
    };
    let (mut doc, repaired) = document(&text)?;
    migrate(&mut doc);
    let mut config = recover(&mut doc)?;
    let mut changed = new || repaired || doc.to_string() != text;
    if let Some((id, switch)) = edit {
        let matches: Vec<_> = config
            .exclusions
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.matches(id))
            .map(|(index, _)| index)
            .collect();
        let before = !matches.is_empty();
        let after = switch.value(before);
        if before != after {
            if !doc.contains_key("exclusions") {
                doc["exclusions"] = Item::ArrayOfTables(ArrayOfTables::new());
            }
            let rules = doc["exclusions"]
                .as_array_of_tables_mut()
                .ok_or("exclusions")?;
            if after {
                let mut table = Table::new();
                table.insert("name", value(&id.name));
                table.insert("class", value(&id.class));
                table.insert("title", value(&id.title));
                rules.push(table)
            } else {
                for index in matches.into_iter().rev() {
                    rules.remove(index);
                }
            }
            changed = true;
            config = Config::parse(&doc)?;
        }
    }
    if changed {
        write(path, &doc.to_string()).map_err(|err| err.to_string())?
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_help_key_is_h_and_legacy_migration_respects_custom_bindings() {
        let mut doc = help().parse::<DocumentMut>().unwrap();
        assert_eq!(
            Config::parse(&doc)
                .unwrap()
                .keys
                .get(&u32::from(b'H'))
                .map(String::as_str),
            Some("help")
        );
        doc["keys"].as_table_mut().unwrap().remove("H");
        doc["keys"]["F1"] = value("Help");
        migrate(&mut doc);
        assert!(doc["keys"].get("F1").is_none());
        assert_eq!(doc["keys"]["H"].as_str(), Some("Help"));
        doc["keys"]["H"] = value("Horizontal");
        doc["keys"]["F1"] = value("Help");
        migrate(&mut doc);
        assert_eq!(doc["keys"]["H"].as_str(), Some("Horizontal"));
        assert_eq!(doc["keys"]["F1"].as_str(), Some("Help"));
    }

    #[test]
    fn accepts_utf8_and_old_unicode_configs() {
        let text = "[tip]\nfont = \"微软雅黑\"\n";
        assert_eq!(decode(text.as_bytes()).unwrap(), text);
        assert_eq!(decode(&encode(text)).unwrap(), text);
        assert!(decode(&[255, 254, 1]).is_err());
    }

    #[test]
    fn repairs_syntax_without_touching_string_or_comment_punctuation() {
        let text = "【tip】\nfont ： “微软雅黑”\nwindow_format = \"中文，。‘’【】\" # 注释：，\n";
        let (doc, repaired) = document(text).unwrap();
        assert!(repaired);
        assert_eq!(doc["tip"]["font"].as_str(), Some("微软雅黑"));
        assert_eq!(doc["tip"]["window_format"].as_str(), Some("中文，。‘’【】"));
        assert!(!doc.to_string().contains("# 注释：，"));
        assert!(repair_punctuation(text).contains("# 注释：，"));
        let valid = "[tip]\nfont = '“微软雅黑”'\n";
        assert!(!document(valid).unwrap().1);
    }

    #[test]
    fn invalid_field_falls_back_without_discarding_other_edits() {
        let mut doc = help().parse::<DocumentMut>().unwrap();
        doc["tip"]["font_size"] = value(-2);
        doc["general"]["autostart"] = value(true);
        doc["crosshair"]["fps"] = value(2000);
        let cfg = recover(&mut doc).unwrap();
        assert_eq!(cfg.font_size, 16);
        assert!(cfg.autostart);
        assert_eq!(cfg.fps, 1000.0);
        assert_eq!(cfg.warnings, ["tip.font_size"]);
        doc["crosshair"]["fps"] = value(59.94);
        assert_eq!(recover(&mut doc).unwrap().fps, 30.0);
    }

    #[test]
    fn disk_wins_only_conflicting_fields_and_preserves_deletions() {
        let base = "[tip]\nfont_size = 16\nshow = 'window'\n[commands]\nCustom = 'Help'\n"
            .parse::<DocumentMut>()
            .unwrap();
        let mut draft = base.clone();
        draft["tip"]["font_size"] = value(20);
        draft["tip"]["show"] = value("time");
        draft["commands"]["Custom"] = value("Version");
        let mut disk = base.clone();
        disk["tip"]["font_size"] = value(24);
        disk["commands"].as_table_mut().unwrap().remove("Custom");
        merge_edits(base.as_table(), draft.as_table(), disk.as_table_mut());
        assert_eq!(disk["tip"]["font_size"].as_integer(), Some(24));
        assert_eq!(disk["tip"]["show"].as_str(), Some("time"));
        assert!(disk["commands"].get("Custom").is_none());
    }

    #[test]
    fn removed_presets_are_not_readded_and_escape_is_reserved() {
        let mut doc = help().parse::<DocumentMut>().unwrap();
        doc["presets"].as_table_mut().unwrap().remove("Center");
        doc["keys"]["Esc"] = value("Help");
        doc["presets"]["Escape"] = value("0|0|100sw|100sh");
        let cfg = recover(&mut doc).unwrap();
        assert!(!cfg.presets.contains_key("center"));
        assert!(!cfg.presets.contains_key("escape"));
        assert!(!cfg.keys.contains_key(&27));
        assert_eq!(vk("vk:1B"), None);
    }
    #[test]
    fn recovery_handles_names_with_spaces_and_normalizes_action_spelling() {
        let mut doc = help().parse::<DocumentMut>().unwrap();
        doc["presets"]["My Region"] = value("broken");
        doc["keys"]["P"] = value("tOP");
        let cfg = recover(&mut doc).unwrap();
        assert!(!cfg.presets.contains_key("my region"));
        assert_eq!(doc["keys"]["P"].as_str(), Some("Top"));
        assert_eq!(cfg.warnings, ["presets.My Region"]);
    }
    #[test]
    fn comment_migration_preserves_hashes_and_multiline_strings() {
        let source = "# 旧注释\n[tip]\nwindow_format = '''中文 # 注释不是注释\n下一行''' # 删除\nfont = \"SystemFont\" # English\n";
        let (doc, _) = document(source).unwrap();
        assert_eq!(
            doc["tip"]["window_format"].as_str(),
            Some("中文 # 注释不是注释\n下一行")
        );
        assert!(doc.to_string().contains("# English"));
        assert!(!doc.to_string().contains("# 旧注释"));
    }
}
