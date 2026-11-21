use crate::{cli::Switch, layout::Layout};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};
use unicode_width::UnicodeWidthStr;
use windows_sys::Win32::{
    Foundation::*, Globalization::GetUserDefaultUILanguage, System::Threading::*,
};

fn system_zh() -> bool {
    (unsafe { GetUserDefaultUILanguage() } & 0x3ff) == 4
}
pub fn help() -> &'static str {
    include_str!("example.toml")
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
#[derive(Clone, Copy, PartialEq)]
pub struct Line {
    pub width: i32,
    pub gap: i32,
    pub color: u32,
    pub opacity: u8,
}
#[derive(Clone, Debug, PartialEq)]
pub struct CastStyle {
    pub enabled: bool,
    pub duration: u32,
    pub fade: u32,
    pub font: String,
    pub font_size: i32,
    pub foreground: u32,
    pub background: u32,
    pub opacity: u8,
    pub history: usize,
    pub position: String,
}
#[derive(Clone, Debug, PartialEq)]
pub struct MouseStyle {
    pub enabled: bool,
    pub swap_side_buttons: bool,
    pub radius: i32,
    pub opacity: u8,
    pub color: u32,
    pub dark: bool,
}
#[derive(PartialEq)]
pub struct Config {
    pub mousecast: MouseStyle,
    pub keycast: CastStyle,
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
    let text = text.trim().to_ascii_lowercase();
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
        "leftbracket" | "[" | "【" => 0xdb,
        "rightbracket" | "]" | "】" => 0xdd,
        "quote" | "'" => 0xde,
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

pub(crate) fn reserved_key(key: u32) -> bool {
    matches!(key,
        // System Alt shortcuts, modifiers, and the tip's navigation/cancel keys.
        0x09 | 0x0d | 0x10..=0x12 | 0x1b | 0x20..=0x28 | 0x2c
        | 0x5b..=0x5c | 0x73 | 0xa0..=0xa5
        // Mouse/gamepad input, undefined/reserved codes, and synthetic IME/Unicode input.
        | 0x00..=0x07 | 0x0a..=0x0b | 0x0e..=0x0f | 0x3a..=0x40
        | 0x5e | 0x88..=0x8f | 0x97..=0x9f | 0xb8..=0xb9
        | 0xc1..=0xda | 0xe0 | 0xe5..=0xe8 | 0xfc | 0xff..=u32::MAX
    )
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
        } else {
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
    "mousecast",
    "keycast",
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
    match name {
        "mousecast" => return "MouseCast".into(),
        "keycast" => return "KeyCast".into(),
        _ => {}
    }
    if let Some((_, action)) = ALIASES.iter().find(|(alias, _)| *alias == name) {
        return standard_name(action);
    }
    let mut chars = name.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}
fn key_name(key: u32) -> String {
    match key {
        9 => "Tab".into(),
        32 => "Space".into(),
        0x2d => "Insert".into(),
        0xc0 => "`".into(),
        0xbd => "-".into(),
        0xbb => "=".into(),
        0xdc => "\\".into(),
        0xdb => "[".into(),
        0xdd => "]".into(),
        0xde => "'".into(),
        0xba => ";".into(),
        0xbc => ",".into(),
        0xbe => ".".into(),
        0xbf => "/".into(),
        0x70..=0x87 => format!("F{}", key - 0x6f),
        0x30..=0x39 | 0x41..=0x5a => char::from_u32(key).unwrap().to_string(),
        _ => format!("VK:{key:02X}"),
    }
}
fn is_builtin_action(name: &str) -> bool {
    ACTIONS.contains(&name)
}
pub fn shortcut_name(text: &str) -> String {
    vk(text).map(key_name).unwrap_or_else(|| text.to_owned())
}
fn reserved_preset_name(name: &str) -> bool {
    let name = command_name(name);
    is_builtin_action(&name) || ALIASES.iter().any(|(alias, _)| *alias == name)
}

fn validate_command_names(doc: &DocumentMut) -> Result<(), String> {
    let presets: HashSet<_> = doc
        .get("presets")
        .and_then(Item::as_table)
        .into_iter()
        .flat_map(|table| table.iter().map(|(key, _)| command_name(key)))
        .collect();
    let mut names = HashSet::new();
    if let Some(table) = doc.get("commands").and_then(Item::as_table) {
        for (key, item) in table {
            let name = command_name(key);
            if !names.insert(name.clone()) {
                return Err(format!("commands.{key}: duplicate name / 命令名称重复"));
            }
            if presets.contains(&name) {
                return Err(format!(
                    "commands.{key}: name conflicts with a preset / 命令名与分区名冲突"
                ));
            }
            let builtin = ALIASES
                .iter()
                .find(|(alias, _)| *alias == name)
                .map(|(_, action)| *action)
                .or_else(|| is_builtin_action(&name).then_some(name.as_str()));
            if let Some(builtin) = builtin
                && item.as_str().map(command_name).as_deref() != Some(builtin)
            {
                return Err(format!(
                    "commands.{key}: reserved action name / 不可覆盖内置动作或别名"
                ));
            }
        }
    }
    Ok(())
}

fn canonicalize(doc: &mut DocumentMut) {
    let names: HashMap<String, String> = doc
        .get("presets")
        .and_then(Item::as_table)
        .into_iter()
        .flat_map(|table| table.iter())
        .map(|(key, _)| (command_name(key), key.to_owned()))
        .collect();
    // Preserve custom names as authored; normalize action references and built-in spelling.
    for section in ["keys", "commands", "grid"] {
        if let Some(table) = doc.get_mut(section).and_then(Item::as_table_mut) {
            for (_, item) in table.iter_mut() {
                if let Some(text) = item.as_str() {
                    let lower = command_name(text);
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
        self.presets.get(&command_name(name_or_key)).or_else(|| {
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
            .get(&command_name(name))
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
    pub fn region_caption(&self, name: &str) -> String {
        let normalized = command_name(name);
        let mut keys: Vec<_> = self
            .keys
            .iter()
            .filter_map(|(key, action)| (action == &normalized).then_some(*key))
            .collect();
        keys.sort_unstable();
        let title = self.display_name(name);
        if keys.is_empty() {
            return title.to_owned();
        }
        let shortcuts = keys
            .into_iter()
            .map(|key| format!("[ {} ]", key_name(key)))
            .collect::<Vec<_>>()
            .join(" ");
        format!("{title}\n{shortcuts}")
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
        // Content is organized as sections of lines; only the final join adds spacing.
        let mut sections = vec![vec![
            self.tr(
                "WinSizeP HELP: (Esc to close)",
                "WinSizeP 帮助：（按 Esc 关闭）",
            )
            .to_owned(),
        ]];
        let mut mouse = Vec::new();
        for (button_id, en, zh) in [
            (self.move_button, "move window", "移动窗口"),
            (
                self.resize_button,
                "resize from bottom right",
                "调整窗口大小（固定左上角）",
            ),
        ] {
            if button_id.is_some() && self.gesture != "off" {
                mouse.push(format!(
                    "Alt + {} {} = {}",
                    button(button_id),
                    self.tr("drag", "拖动"),
                    self.tr(en, zh)
                ));
            }
        }
        if self.gesture != "off" {
            mouse.push(
                self.tr(
                    "Alt + right click = place by the 3×3 grid",
                    "Alt + 右键点击屏幕位置 = 九宫格位置调整",
                )
                .into(),
            );
        }
        sections.push(mouse);
        let mut regions = vec![
            self.tr(
                "Hold Alt + a key, or double-tap Alt then press a key:",
                "按住 Alt + 按键，或双击 Alt 后按键：",
            )
            .into(),
        ];
        // Only region bindings form the keyboard diagram. Empty physical rows take no space.
        let keyboard_rows: [Vec<u32>; 5] = [
            (0x70..=0x87).collect(),
            "`1234567890-="
                .chars()
                .filter_map(|ch| vk(&ch.to_string()))
                .collect(),
            "QWERTYUIOP[]\\"
                .chars()
                .filter_map(|ch| vk(&ch.to_string()))
                .collect(),
            "ASDFGHJKL;'"
                .chars()
                .filter_map(|ch| vk(&ch.to_string()))
                .collect(),
            "ZXCVBNM,./"
                .chars()
                .filter_map(|ch| vk(&ch.to_string()))
                .collect(),
        ];
        let rows: Vec<Vec<_>> = keyboard_rows
            .iter()
            .map(|row| {
                row.iter()
                    .filter_map(|key| {
                        let action = self.keys.get(key)?;
                        self.presets.contains_key(action).then_some((*key, action))
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|row| !row.is_empty())
            .collect();
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut shown = HashSet::new();
        for (index, row) in rows.iter().enumerate() {
            let mut cells: Vec<_> = row.iter().map(|(key, _)| key_name(*key)).collect();
            cells.resize(columns, String::new());
            cells.push(if index == rows.len() / 2 { "->" } else { "" }.into());
            for (key, action) in row {
                shown.insert(*key);
                cells.push(self.display_name(action).to_owned());
            }
            regions.push(cells.join("\t"));
        }
        sections.push(regions);
        let mut bindings: Vec<_> = self.keys.iter().collect();
        bindings.sort_by_key(|(key, _)| **key);
        let mut crosshair = Vec::new();
        for (name, en, zh) in [
            ("guide", "toggle crosshair", "显示或隐藏十字线"),
            ("cross", "both axes", "双线"),
            ("vertical", "vertical", "竖线"),
            ("horizontal", "horizontal", "横线"),
        ] {
            let keys: Vec<_> = bindings
                .iter()
                .filter(|(_, action)| action.as_str() == name)
                .map(|(key, _)| {
                    shown.insert(**key);
                    key_name(**key)
                })
                .collect();
            if !keys.is_empty() {
                crosshair.push(format!(
                    "{} {}",
                    keys.join(self.tr(" / ", "、")),
                    self.tr(en, zh)
                ));
            }
        }
        let mut visual = Vec::new();
        if !crosshair.is_empty() {
            visual.push(crosshair.join(self.tr(";  ", "；")));
        }
        let mut casts = Vec::new();
        for (name, en, zh) in [
            ("mousecast", "mouse annotations", "鼠标标注"),
            ("keycast", "keyboard annotations", "键盘标注"),
        ] {
            for (key, action) in &bindings {
                if action.as_str() == name {
                    shown.insert(**key);
                    casts.push(format!("Alt + {} = {}", key_name(**key), self.tr(en, zh)));
                }
            }
        }
        if !casts.is_empty() {
            visual.push(casts.join(self.tr(";  ", "；")));
        }
        sections.push(visual);
        let mut other = Vec::new();
        for (key, action) in bindings {
            if shown.contains(key) {
                continue;
            }
            let key = key_name(*key);
            let title = match action.as_str() {
                "top" => self.tr("toggle topmost", "置顶或取消置顶"),
                "ignore" => self.tr("exclude window", "忽略窗口"),
                "borders" => self.tr("toggle region outlines", "显示或隐藏框线"),
                "command" => self.tr("run command", "运行指令"),
                "help" => self.tr("help", "帮助"),
                _ => self.display_name(action),
            };
            other.push(format!("Alt + {key} = {title}"));
        }
        sections.push(other);
        let mut aliases = vec![self.tr("Aliases:", "内置别名：").to_owned()];
        for row in [
            &["Version = Ver", "Help = H"][..],
            &["Quit = Exit", "Restart = Reboot"][..],
            &["Top = Pin", "Ignore = Exclude", "Monitor = Screen"][..],
        ] {
            aliases.push(row.join(self.tr("; ", "；")));
        }
        sections.push(aliases);
        sections
            .into_iter()
            .filter(|section| !section.is_empty())
            .map(|section| section.join("\n"))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
    pub fn console_help_text(&self) -> String {
        // Console tabs have fixed stops. Expand the same cells using Unicode display widths.
        let text = self.help_text();
        let lines: Vec<_> = text.lines().collect();
        let mut output = Vec::new();
        let mut start = 0;
        while start < lines.len() {
            if !lines[start].contains('\t') {
                output.push(lines[start].to_owned());
                start += 1;
                continue;
            }
            let end = start
                + lines[start..]
                    .iter()
                    .take_while(|line| line.contains('\t'))
                    .count();
            let rows: Vec<Vec<_>> = lines[start..end]
                .iter()
                .map(|line| line.split('\t').collect())
                .collect();
            let mut widths = vec![0; rows.iter().map(Vec::len).max().unwrap_or(0)];
            for row in &rows {
                for (column, cell) in row.iter().enumerate() {
                    widths[column] = widths[column].max(cell.width());
                }
            }
            for row in rows {
                let mut line = String::new();
                for (column, cell) in row.iter().enumerate() {
                    line.push_str(cell);
                    if column + 1 < row.len() {
                        line.extend(std::iter::repeat_n(' ', widths[column] - cell.width() + 2));
                    }
                }
                output.push(line);
            }
            start = end;
        }
        output.join("\n")
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
            options
                .iter()
                .find(|option| option.eq_ignore_ascii_case(val.trim()))
                .map(|option| (*option).to_owned())
                .ok_or_else(|| format!("{section}.{key}: {val}"))
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
            let name = command_name(key);
            if reserved_preset_name(key) {
                return Err(format!(
                    "presets.{key}: reserved action name / 分区名与内置动作冲突"
                ));
            }
            if matches!(name.as_str(), "esc" | "escape" | "") || key.chars().any(char::is_control) {
                return Err(format!("presets.{key}: reserved name"));
            }
            let layout = item
                .as_str()
                .ok_or_else(|| format!("presets.{key}"))?
                .parse()
                .map_err(|err: String| format!("presets.{key}: {err}"))?;
            if presets.insert(name.clone(), layout).is_some() {
                return Err(format!("presets.{key}: duplicate name"));
            }
            preset_names.insert(name, key.to_owned());
        }
        validate_command_names(doc)?;
        let action = |item: &Item| {
            let name = command_name(item.as_str().ok_or_else(|| item.to_string())?);
            if name.is_empty() || presets.contains_key(&name) || is_builtin_action(&name) {
                Ok(name)
            } else {
                Err(format!("Unknown action / 未知动作: {name}"))
            }
        };
        let mut keys = HashMap::new();
        let mut used_keys = HashSet::new();
        for (key, item) in entries("keys")? {
            let name = action(item).map_err(|err| format!("keys.{key}: {err}"))?;
            let vk = vk(key).ok_or_else(|| format!("keys.{key}: invalid or reserved key"))?;
            if reserved_key(vk) {
                return Err(format!(
                    "keys.{key}: system or internal reserved key / 系统或程序保留键"
                ));
            }
            if !used_keys.insert(vk) {
                return Err(format!("keys.{key}: duplicate key"));
            }
            if !name.is_empty() {
                keys.insert(vk, name);
            }
        }
        let mut commands = HashMap::new();
        for (key, item) in entries("commands")? {
            let name = command_name(key);
            if name.is_empty()
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b' '))
            {
                return Err(format!("commands.{key}"));
            }
            if matches!(name.as_str(), "esc" | "escape") {
                return Err(format!("commands.{key}: reserved name"));
            }
            commands.insert(
                name,
                action(item).map_err(|err| format!("commands.{key}: {err}"))?,
            );
        }
        let mut grid = HashMap::new();
        for (key, item) in entries("grid")? {
            let name = command_name(item.as_str().ok_or_else(|| format!("grid.{key}"))?);
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
        let cast = |section: &'static str| -> Result<CastStyle, String> {
            Ok(CastStyle {
                enabled: boolean(section, "enabled")?,
                duration: number(section, "duration_ms", 100, 10000)? as u32,
                fade: number(section, "fade_ms", 0, 3000)? as u32,
                font: font(section)?,
                font_size: number(section, "font_size", 6, 72)? as i32,
                foreground: color(section, "foreground")?,
                background: color(section, "background")?,
                opacity: number(section, "opacity", 32, 255)? as u8,
                history: if section == "keycast" {
                    number(section, "history", 1, 8)? as usize
                } else {
                    5
                },
                position: if section == "keycast" {
                    choice(
                        section,
                        "position",
                        &["bottom-left", "bottom-right", "top-left", "top-right"],
                    )?
                } else {
                    "pointer".into()
                },
            })
        };
        Ok(Self {
            mousecast: MouseStyle {
                enabled: boolean("mousecast", "enabled")?,
                swap_side_buttons: boolean("mousecast", "swap_side_buttons")?,
                radius: number("mousecast", "radius", 4, 64)? as i32,
                opacity: number("mousecast", "opacity", 96, 224)? as u8,
                color: crate::colors::accent(),
                dark: crate::overlay::dark(),
            },
            keycast: cast("keycast")?,
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
            highlight_opacity: number("outline", "fill_opacity", 0, 255)? as u8,
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
    match text.parse::<DocumentMut>() {
        Ok(doc) => Ok((doc, false)),
        Err(original) => {
            let repaired = repair_punctuation(text);
            let doc = repaired
                .parse::<DocumentMut>()
                .map_err(|_| original.to_string())?;
            Ok((doc, repaired != text))
        }
    }
}

fn migrate(doc: &mut DocumentMut) -> Result<(), String> {
    if let Some(mouse) = doc.get_mut("mousecast").and_then(Item::as_table_mut) {
        if (mouse.contains_key("ring_width") || mouse.contains_key("duration_ms"))
            && let Some(radius) = mouse.get("radius").and_then(Item::as_integer)
        {
            mouse["radius"] = value((radius / 2).clamp(4, 64));
        }
        for old in [
            "duration_ms",
            "fade_ms",
            "ring_width",
            "color",
            "background",
            "foreground",
            "font",
            "font_size",
        ] {
            mouse.remove(old);
        }
        if !mouse.contains_key("swap_side_buttons") {
            if let Some(radius) = mouse.get("radius").and_then(Item::as_integer) {
                mouse["radius"] = value(((radius.clamp(4, 64) * 2 + 1) / 3).max(4));
            }
            mouse["swap_side_buttons"] = value(true);
        }
        if let Some(alpha) = mouse.get("opacity").and_then(Item::as_integer)
            && !(96..=224).contains(&alpha)
        {
            mouse["opacity"] = value(alpha.clamp(96, 224));
        }
    }
    // Introduce each shortcut only once, preserving occupied/disabled virtual-key aliases
    // and intentionally empty maps. The new fixed sections mark the completed migration.
    for (section, key, action) in [("mousecast", "M", "MouseCast"), ("keycast", "K", "KeyCast")] {
        if doc.get(section).is_none()
            && let Some(keys) = doc.get_mut("keys").and_then(Item::as_table_mut)
            && !keys.is_empty()
            && !keys.iter().any(|(name, item)| {
                vk(name) == vk(key)
                    || item
                        .as_str()
                        .is_some_and(|v| v.eq_ignore_ascii_case(action))
            })
        {
            keys.insert(key, value(action));
        }
    }
    // Validate the destination before removing any legacy fields. A malformed
    // document must remain available for correction instead of panicking in a callback.
    if doc
        .get("general")
        .and_then(Item::as_table)
        .is_some_and(|general| general.contains_key("alt") || general.contains_key("gesture"))
        && doc.get("shortcuts").is_some_and(|item| !item.is_table())
    {
        return Err("shortcuts: expected a table / 快捷操作配置必须为表".into());
    }
    // Convert the old edge-highlight schema once, so later deliberate font/alpha edits survive.
    if let Some(legacy) = doc
        .get_mut("outline")
        .and_then(Item::as_table_mut)
        .and_then(|table| table.remove("highlight_opacity"))
    {
        if let Some(commands) = doc.get_mut("commands").and_then(Item::as_table_mut) {
            let example = commands
                .iter()
                .find(|(key, item)| {
                    key.eq_ignore_ascii_case("asdf")
                        && item
                            .as_str()
                            .is_some_and(|value| value.eq_ignore_ascii_case("center"))
                })
                .map(|(key, _)| key.to_owned());
            if let Some(example) = example {
                commands.remove(&example);
            }
        }
        if doc["outline"].get("fill_opacity").is_none() {
            let alpha = legacy
                .as_integer()
                .filter(|n| *n >= 0 && *n < 255)
                .unwrap_or(48);
            doc["outline"]["fill_opacity"] = value(alpha);
        }
        if doc
            .get("tip")
            .and_then(|v| v.get("font_size"))
            .and_then(Item::as_integer)
            == Some(16)
        {
            doc["tip"]["font_size"] = value(12);
        }
    }
    // Key bindings are user data. Historical defaults cannot distinguish an old
    // template from a deliberate F1 binding; only new files get the current H default.
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
    Ok(())
}

fn recover(doc: &mut DocumentMut) -> Result<Config, String> {
    // Dropping a conflicting preset would reinterpret its bindings as actions.
    // Reject before canonicalization or any other document mutation.
    if let Some((key, _)) = doc
        .get("presets")
        .and_then(Item::as_table)
        .and_then(|table| table.iter().find(|(key, _)| reserved_preset_name(key)))
    {
        return Err(format!(
            "presets.{key}: reserved action name / 分区名与内置动作冲突"
        ));
    }
    // Removing a conflicting command could expose an unintended built-in action.
    validate_command_names(doc)?;
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
                    // Exclusions restrict which windows may be changed. Guessing at
                    // invalid rules can silently remove those restrictions. Reject this
                    // update so callers keep their last valid configuration and the file.
                    return Err(format!(
                        "{error}: exclusion rules were preserved; correct the invalid rule / 排除规则已保留，请修正无效项"
                    ));
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
    // GUI renames keep the row's original position. Respect manual reordering if it changed.
    let disk_existing: Vec<_> = disk
        .iter()
        .filter(|(key, _)| base.contains_key(key))
        .map(|(key, _)| key.to_owned())
        .collect();
    let base_existing: Vec<_> = base
        .iter()
        .filter(|(key, _)| disk.contains_key(key))
        .map(|(key, _)| key.to_owned())
        .collect();
    if disk_existing == base_existing {
        let ranks: HashMap<_, _> = draft
            .iter()
            .enumerate()
            .map(|(index, (key, _))| (key.to_owned(), index))
            .collect();
        disk.sort_values_by(|a, _, b, _| {
            ranks
                .get(a.get())
                .copied()
                .unwrap_or(usize::MAX)
                .cmp(&ranks.get(b.get()).copied().unwrap_or(usize::MAX))
        });
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

fn write(path: &Path, text: &str, expected: Option<&[u8]>) -> io::Result<()> {
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let result = (|| {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        // Editors outside WinSizeP do not acquire our mutex. Refuse an observed
        // intervening edit rather than replacing a version we never merged.
        let current = match fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if current.as_deref() != expected {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Configuration changed during save; retry / 保存期间配置已变化，请重试",
            ));
        }
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
    migrate(&mut doc)?;
    Ok(recover(&mut doc)?.elevate)
}

pub fn save_settings(path: &Path, original: &str, doc: &DocumentMut) -> Result<String, String> {
    Config::parse(doc)?;
    let _lock = lock()?;
    let bytes = fs::read(path).map_err(|err| err.to_string())?;
    let (base, _) = document(original)?;
    let (mut disk, _) = document(&decode(&bytes)?)?;
    merge_edits(base.as_table(), doc.as_table(), disk.as_table_mut());
    migrate(&mut disk)?;
    recover(&mut disk)?;
    let saved = disk.to_string();
    write(path, &saved, Some(&bytes)).map_err(|err| err.to_string())?;
    Ok(saved)
}

pub struct Loaded {
    pub config: Config,
    pub document: DocumentMut,
    pub text: String,
    pub bytes: Vec<u8>,
}

pub fn load(path: &Path, edit: Option<(&Identity, Switch)>) -> Result<Config, String> {
    load_snapshot(path, edit).map(|loaded| loaded.config)
}

/// All returned values describe the same read (or the same committed write).
pub fn load_snapshot(path: &Path, edit: Option<(&Identity, Switch)>) -> Result<Loaded, String> {
    let _lock = lock()?;
    let (bytes, new) = match fs::read(path) {
        Ok(bytes) => (bytes, false),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let legacy = path.with_file_name("winsizep_config");
            match fs::read(legacy) {
                Ok(bytes) => (bytes, true),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    (help().as_bytes().to_vec(), true)
                }
                Err(err) => return Err(err.to_string()),
            }
        }
        Err(err) => return Err(err.to_string()),
    };
    let text = decode(&bytes)?;
    let (mut doc, repaired) = document(&text)?;
    migrate(&mut doc)?;
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
    let (text, bytes) = if changed {
        let text = doc.to_string();
        write(path, &text, (!new).then_some(bytes.as_slice())).map_err(|err| err.to_string())?;
        let bytes = text.as_bytes().to_vec();
        (text, bytes)
    } else {
        (text, bytes)
    };
    Ok(Loaded {
        config,
        document: doc,
        text,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn region_captions_follow_case_insensitive_user_bindings() {
        let mut doc = help().parse::<DocumentMut>().unwrap();
        doc["keys"] = Item::Table(Table::new());
        doc["keys"]["w"] = value("fUlLtOp");
        doc["shortcuts"]["alt"] = value("LEFT");
        doc["shortcuts"]["gesture"] = value("Both");
        doc["shortcuts"]["move_button"] = value("Left");
        let cfg = Config::parse(&doc).unwrap();
        assert_eq!(cfg.alt, "left");
        assert_eq!(cfg.gesture, "both");
        assert_eq!(cfg.move_button, Some(1));
        let fields = crate::settings_schema::fields(4, 0, &cfg, &doc);
        assert_eq!(
            fields
                .iter()
                .find(|field| field.section == "keys" && field.key == "w")
                .unwrap()
                .label,
            "W"
        );
        assert_eq!(cfg.region_caption("FULLTOP"), "FullTop\n[ W ]");
        assert_eq!(cfg.region_caption("center"), "Center");
        assert_eq!(shortcut_name("iNsErT"), "Insert");
        assert_eq!(shortcut_name("f12"), "F12");
        assert_eq!(shortcut_name("vk:57"), "W");
        doc["keys"].as_table_mut().unwrap().remove("w");
        doc["keys"]["f12"] = value("FULLTOP");
        doc["keys"]["insert"] = value("fulltop");
        let cfg = Config::parse(&doc).unwrap();
        assert_eq!(cfg.region_caption("fulltop"), "FullTop\n[ Insert ] [ F12 ]");
        doc["keys"]["F12"] = value("Center");
        assert!(Config::parse(&doc).is_err());
    }
    #[test]
    fn side_button_setting_defaults_to_swapped_and_migrates_size_only_once() {
        let mut doc = help().parse::<DocumentMut>().unwrap();
        assert!(Config::parse(&doc).unwrap().mousecast.swap_side_buttons);
        doc["mousecast"]
            .as_table_mut()
            .unwrap()
            .remove("swap_side_buttons");
        doc["mousecast"]["radius"] = value(14);
        migrate(&mut doc).unwrap();
        assert_eq!(doc["mousecast"]["radius"].as_integer(), Some(9));
        assert_eq!(doc["mousecast"]["swap_side_buttons"].as_bool(), Some(true));
        doc["mousecast"]["swap_side_buttons"] = value(false);
        migrate(&mut doc).unwrap();
        let cfg = Config::parse(&doc).unwrap();
        assert_eq!(cfg.mousecast.radius, 9);
        assert!(!cfg.mousecast.swap_side_buttons);
    }
    #[test]
    fn old_mouse_animation_settings_migrate_once_to_compact_shapes() {
        let mut doc = help().parse::<DocumentMut>().unwrap();
        doc["mousecast"]
            .as_table_mut()
            .unwrap()
            .remove("swap_side_buttons");
        doc["mousecast"]["radius"] = value(28);
        doc["mousecast"]["duration_ms"] = value(450);
        doc["mousecast"]["fade_ms"] = value(250);
        doc["mousecast"]["ring_width"] = value(3);
        doc["mousecast"]["font"] = value("Arial");
        migrate(&mut doc).unwrap();
        assert_eq!(doc["mousecast"]["radius"].as_integer(), Some(9));
        for key in ["duration_ms", "fade_ms", "ring_width", "font"] {
            assert!(doc["mousecast"].get(key).is_none());
        }
        migrate(&mut doc).unwrap();
        assert_eq!(doc["mousecast"]["radius"].as_integer(), Some(9));
        Config::parse(&doc).unwrap();
    }
    #[test]
    fn annotation_migration_preserves_existing_shortcuts_and_runs_once() {
        let mut doc = help().parse::<DocumentMut>().unwrap();
        doc.as_table_mut().remove("mousecast");
        doc.as_table_mut().remove("keycast");
        doc["keys"].as_table_mut().unwrap().remove("M");
        doc["keys"].as_table_mut().unwrap().remove("K");
        doc["keys"]["VK:4D"] = value("Help");
        migrate(&mut doc).unwrap();
        assert!(doc["keys"].get("M").is_none());
        assert_eq!(doc["keys"]["VK:4D"].as_str(), Some("Help"));
        assert_eq!(doc["keys"]["K"].as_str(), Some("KeyCast"));
        recover(&mut doc).unwrap();
        doc["keys"].as_table_mut().unwrap().remove("K");
        migrate(&mut doc).unwrap();
        assert!(doc["keys"].get("K").is_none());
        doc.as_table_mut().remove("mousecast");
        doc.as_table_mut().remove("keycast");
        doc["keys"] = Item::Table(Table::new());
        migrate(&mut doc).unwrap();
        assert!(doc["keys"].as_table().unwrap().is_empty());
    }
    #[test]
    fn invalid_annotation_fields_recover_without_losing_independent_styles() {
        let mut doc = help().parse::<DocumentMut>().unwrap();
        doc["mousecast"]["radius"] = value(-1);
        doc["keycast"]["font_size"] = value(32);
        doc["keycast"]["history"] = value(999);
        let cfg = recover(&mut doc).unwrap();
        assert_eq!(cfg.mousecast.radius, 9);
        assert_eq!(cfg.keycast.font_size, 32);
        assert_eq!(cfg.keycast.history, 4);
        assert!(cfg.warnings.iter().any(|w| w.contains("mousecast.radius")));
        assert!(cfg.warnings.iter().any(|w| w.contains("keycast.history")));
    }
    #[test]
    fn help_omits_disabled_mouse_actions_and_uses_readable_key_names() {
        let mut doc = help().parse::<DocumentMut>().unwrap();
        doc["general"]["language"] = value("en");
        doc["shortcuts"]["gesture"] = value("off");
        doc["keys"]["F12"] = value("Help");
        doc["keys"]["Insert"] = value("Version");
        doc["keys"]["Comma"] = value("Help");
        let text = Config::parse(&doc).unwrap().help_text();
        assert!(!text.contains("drag =") && !text.contains("right click"));
        for key in ["F12", "Insert", ","] {
            assert!(text.contains(&format!("Alt + {key} =")));
        }
    }
    #[test]
    fn help_groups_regions_in_keyboard_order_then_crosshair_then_other_actions() {
        let mut doc = help().parse::<DocumentMut>().unwrap();
        doc["general"]["language"] = value("en");
        let text = Config::parse(&doc).unwrap().help_text();
        assert!(
            text.contains(concat!(
                "Q\tW\tE\t\tLeftTop\tFullTop\tRightTop\n",
                "A\tS\tD\t->\tFullLeft\tCenter\tFullRight\n",
                "Z\tX\tC\t\tLeftBottom\tFullBottom\tRightBottom\n\n",
                "L toggle crosshair;  = both axes;  \\ vertical;  - horizontal\n",
                "Alt + M = mouse annotations;  Alt + K = keyboard annotations\n\n",
            )),
            "{text}"
        );
        assert!(text.find("horizontal\n").unwrap() < text.find("Alt + H = help").unwrap());
        assert!(!text.contains("Alt + Q =") && !text.contains("Alt + L ="));
    }
    #[test]
    fn help_sections_have_consistent_spacing_and_localized_punctuation() {
        for language in ["en", "zh-CN"] {
            let mut doc = help().parse::<DocumentMut>().unwrap();
            doc["general"]["language"] = value(language);
            let text = Config::parse(&doc).unwrap().help_text();
            let sections: Vec<_> = text.split("\n\n").collect();
            assert_eq!(sections.len(), 6, "{text}");
            assert!(sections.iter().all(|section| !section.is_empty()
                && !section.starts_with('\n')
                && !section.ends_with('\n')));
            assert_eq!(sections[3].lines().count(), 2);
            let casts = sections[3].lines().nth(1).unwrap();
            assert!(casts.contains("Alt + M =") && casts.contains("Alt + K ="));
            assert_eq!(text.matches("Alt + M =").count(), 1);
            assert_eq!(text.matches("Alt + K =").count(), 1);
            assert!(!sections[3].contains([':', '：']));
            if language == "zh-CN" {
                assert!(sections[3].contains("L 显示或隐藏十字线；= 双线；\\ 竖线；- 横线"));
                assert!(sections[5].starts_with("内置别名：\n"));
                assert!(!text.contains([':', ';', ',', '(', ')']));
            } else {
                assert!(
                    sections[3]
                        .contains("L toggle crosshair;  = both axes;  \\ vertical;  - horizontal")
                );
                assert!(sections[5].starts_with("Aliases:\n"));
                assert!(!text.contains(['：', '；', '，', '（', '）', '、']));
            }
            doc["shortcuts"]["gesture"] = value("off");
            doc["keys"] = Item::Table(Table::new());
            let text = Config::parse(&doc).unwrap().help_text();
            assert_eq!(text.split("\n\n").count(), 3);
            assert!(!text.contains("\n\n\n"));
        }
    }
    #[test]
    fn help_includes_all_five_keyboard_rows_and_unusual_region_keys_once() {
        let mut doc = help().parse::<DocumentMut>().unwrap();
        doc["keys"] = Item::Table(Table::new());
        for key in [
            "F12", "F2", "0", "1", "Grave", "Equal", "P", "Q", "Quote", "A", "Slash", "Z", "Insert",
        ] {
            doc["keys"][key] = value("Center");
        }
        let text = Config::parse(&doc).unwrap().help_text();
        let rows: Vec<_> = text.lines().filter(|line| line.contains('\t')).collect();
        assert_eq!(rows.len(), 5);
        for (row, expected) in
            rows.iter()
                .zip(["F2\tF12\t", "`\t1\t0\t=\t", "Q\tP\t", "A\t'\t", "Z\t/\t"])
        {
            assert!(row.starts_with(expected), "{row}");
        }
        assert_eq!(text.matches("Alt + Insert = Center").count(), 1);
        doc["keys"] = Item::Table(Table::new());
        assert!(!Config::parse(&doc).unwrap().help_text().contains('\t'));
    }
    #[test]
    fn console_help_aligns_wide_region_names_without_widening_key_columns() {
        let mut doc = help().parse::<DocumentMut>().unwrap();
        doc["presets"]["左上角区域名称"] = value("0|0|100|100");
        doc["keys"]["Q"] = value("左上角区域名称");
        let text = Config::parse(&doc).unwrap().console_help_text();
        assert!(!text.contains('\t'));
        let rows: Vec<_> = text
            .lines()
            .filter(|line| {
                line.starts_with("Q  W  E")
                    || line.starts_with("A  S  D")
                    || line.starts_with("Z  X  C")
            })
            .collect();
        assert_eq!(rows.len(), 3);
        for names in [
            ["左上角区域名称", "FullLeft", "LeftBottom"],
            ["FullTop", "Center", "FullBottom"],
            ["RightTop", "FullRight", "RightBottom"],
        ] {
            let starts: Vec<_> = rows
                .iter()
                .zip(names)
                .map(|(row, name)| row[..row.find(name).unwrap()].width())
                .collect();
            assert_eq!(starts[0], starts[1]);
            assert_eq!(starts[0], starts[2]);
        }
    }
    #[test]
    fn system_and_internal_keys_cannot_be_bound_even_by_virtual_key_code() {
        for key in [
            "Tab", "F4", "Space", "Esc", "VK:09", "VK:0D", "VK:10", "VK:11", "VK:12", "VK:1B",
            "VK:20", "VK:21", "VK:22", "VK:23", "VK:24", "VK:25", "VK:26", "VK:27", "VK:28",
            "VK:2C", "VK:5B", "VK:5C", "VK:73", "VK:A0", "VK:A1", "VK:A2", "VK:A3", "VK:A4",
            "VK:A5", "VK:E5", "VK:E7",
        ] {
            let mut doc = help().parse::<DocumentMut>().unwrap();
            doc["keys"][key] = value("Help");
            let error = Config::parse(&doc)
                .err()
                .unwrap_or_else(|| panic!("accepted {key}"));
            assert!(error.starts_with(&format!("keys.{key}:")), "{error}");
        }
        let mut doc = help().parse::<DocumentMut>().unwrap();
        for key in [
            "F1",
            "F12",
            "F24",
            "Insert",
            "VK:60",
            "LeftBracket",
            "RightBracket",
            "Quote",
        ] {
            doc["keys"][key] = value("Help");
        }
        assert!(Config::parse(&doc).is_ok());
    }
    #[test]
    fn command_conflicts_are_rejected_without_rewriting_the_document() {
        for (key, target) in [
            ("Help", "Quit"),
            ("Pin", "Quit"),
            ("LeftTop", "Help"),
            ("reboot", "Restart"),
            (" G ", "Help"),
        ] {
            let mut doc = help().parse::<DocumentMut>().unwrap();
            doc["commands"][key] = value(target);
            let before = doc.to_string();
            assert!(Config::parse(&doc).is_err(), "{key}");
            assert!(recover(&mut doc).is_err(), "{key}");
            assert_eq!(doc.to_string(), before);
        }
        let mut doc = help().parse::<DocumentMut>().unwrap();
        doc["commands"]["Help"] = value("Help");
        doc["commands"][" My Command "] = value("Center");
        let cfg = Config::parse(&doc).unwrap();
        assert_eq!(cfg.action("MY COMMAND"), "center");
        assert_eq!(cfg.action("Pin"), "top");
    }
    #[test]
    fn normalized_names_and_disabled_key_aliases_cannot_hide_duplicates() {
        let mut doc = help().parse::<DocumentMut>().unwrap();
        doc["presets"][" Ｃｅｎｔｅｒ "] = value("0|0|100|100");
        assert!(
            Config::parse(&doc)
                .err()
                .unwrap()
                .contains("duplicate name")
        );
        let mut doc = help().parse::<DocumentMut>().unwrap();
        doc["keys"]["Q"] = value("");
        doc["keys"]["VK:51"] = value("Center");
        assert!(Config::parse(&doc).err().unwrap().contains("duplicate key"));
        let mut doc = help().parse::<DocumentMut>().unwrap();
        doc["presets"]["大区域"] = value("0|0|100|100");
        doc["keys"]["Q"] = value("大区域");
        assert!(Config::parse(&doc).unwrap().help_text().contains("大区域"));
        for name in ["", "   ", " Escape ", "Ｅｓｃ", "two\nlines", "a\tb"] {
            let mut doc = help().parse::<DocumentMut>().unwrap();
            doc["presets"][name] = value("0|0|100|100");
            assert!(Config::parse(&doc).is_err(), "{name:?}");
        }
    }
    #[test]
    fn built_in_actions_and_aliases_cannot_be_reinterpreted_as_presets() {
        for name in ACTIONS
            .iter()
            .copied()
            .chain(ALIASES.iter().map(|(alias, _)| *alias))
            .chain([" Quit ", "Ｑｕｉｔ"])
        {
            let mut doc = help().parse::<DocumentMut>().unwrap();
            doc["presets"][name] = value("0|0|100|100");
            doc["keys"]["Q"] = value(name);
            assert!(
                Config::parse(&doc)
                    .err()
                    .unwrap()
                    .contains("reserved action name")
            );
            assert!(recover(&mut doc).is_err());
            assert!(doc["presets"].get(name).is_some());
            assert_eq!(doc["keys"]["Q"].as_str(), Some(name));
        }
    }
    #[test]
    fn snapshot_keeps_the_version_that_was_actually_parsed() {
        let folder = std::env::temp_dir().join(format!("winsizep-snapshot-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join("config.toml");
        fs::write(&path, encode(help())).unwrap();
        let first = load_snapshot(&path, None).unwrap();
        let mut newer = first.document.clone();
        newer["crosshair"]["fps"] = value(120);
        fs::write(&path, newer.to_string()).unwrap();
        assert_eq!(
            write(&path, "unmerged", Some(&first.bytes))
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(first.config.fps, 30.0);
        assert_eq!(decode(&first.bytes).unwrap(), first.text);
        assert_ne!(first.bytes, fs::read(&path).unwrap());
        let second = load_snapshot(&path, None).unwrap();
        assert_eq!(second.config.fps, 120.0);
        assert_eq!(second.document["crosshair"]["fps"].as_integer(), Some(120));
        fs::remove_file(path).unwrap();
        fs::remove_dir(folder).unwrap();
    }
    #[test]
    fn invalid_exclusion_never_removes_valid_protection() {
        for invalid in ["name = 123", "class = false", "title = []", ""] {
            let source = format!(
                "{}\n[[exclusions]]\nname = 'protected.exe'\n[[exclusions]]\n{invalid}\n",
                help()
            );
            let mut doc = source.parse::<DocumentMut>().unwrap();
            let before = doc["exclusions"].to_string();
            assert!(recover(&mut doc).is_err());
            assert_eq!(doc["exclusions"].to_string(), before);
        }
        let mut doc = help().parse::<DocumentMut>().unwrap();
        doc["exclusions"] = value("invalid");
        assert!(recover(&mut doc).is_err());
        assert_eq!(doc["exclusions"].as_str(), Some("invalid"));
    }
    #[test]
    fn malformed_migration_destination_returns_an_error_without_changing_input() {
        for key in ["alt", "gesture"] {
            for malformed in [
                value(123),
                value("left"),
                value(false),
                value(toml_edit::Array::new()),
            ] {
                let mut doc = help().parse::<DocumentMut>().unwrap();
                doc["general"][key] = value("left");
                doc["shortcuts"] = malformed;
                let original = doc.to_string();
                assert!(migrate(&mut doc).unwrap_err().starts_with("shortcuts:"));
                assert_eq!(doc.to_string(), original);
            }
        }
        let mut doc = help().parse::<DocumentMut>().unwrap();
        doc.as_table_mut().remove("shortcuts");
        doc["general"]["alt"] = value("right");
        migrate(&mut doc).unwrap();
        assert_eq!(recover(&mut doc).unwrap().alt, "right");
    }
    #[test]
    fn new_defaults_and_migration_do_not_override_later_user_edits() {
        let mut doc = help().parse::<DocumentMut>().unwrap();
        assert_eq!(Config::parse(&doc).unwrap().font_size, 12);
        assert_eq!(doc["keys"]["L"].as_str(), Some("Guide"));
        assert!(doc["keys"].get("G").is_none());
        assert!(doc["commands"].get("ASDF").is_none());
        doc["outline"]
            .as_table_mut()
            .unwrap()
            .remove("fill_opacity");
        doc["outline"]["highlight_opacity"] = value(255);
        doc["tip"]["font_size"] = value(16);
        migrate(&mut doc).unwrap();
        assert_eq!(doc["outline"]["fill_opacity"].as_integer(), Some(48));
        doc["tip"]["font_size"] = value(16);
        doc["outline"]["fill_opacity"] = value(255);
        migrate(&mut doc).unwrap();
        assert_eq!(doc["tip"]["font_size"].as_integer(), Some(16));
        assert_eq!(doc["outline"]["fill_opacity"].as_integer(), Some(255));
    }
    #[test]
    fn new_help_default_is_h_but_migration_preserves_explicit_f1() {
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
        migrate(&mut doc).unwrap();
        assert_eq!(doc["keys"]["F1"].as_str(), Some("Help"));
        assert!(doc["keys"].get("H").is_none());
        migrate(&mut doc).unwrap();
        assert_eq!(doc["keys"]["F1"].as_str(), Some("Help"));
        assert!(doc["keys"].get("H").is_none());
        doc["keys"]["H"] = value("Horizontal");
        doc["keys"]["F1"] = value("Help");
        migrate(&mut doc).unwrap();
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
        assert!(doc.to_string().contains("# 注释：，"));
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
        assert_eq!(cfg.font_size, 12);
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
    fn merging_renames_preserves_positions_and_manual_order() {
        let base = "[keys]\nQ='LeftTop'\nG='Guide'\nH='Help'\n"
            .parse::<DocumentMut>()
            .unwrap();
        let draft = "[keys]\nQ='LeftTop'\nF12='Guide'\nH='Help'\n"
            .parse::<DocumentMut>()
            .unwrap();
        let mut disk = base.clone();
        merge_edits(base.as_table(), draft.as_table(), disk.as_table_mut());
        assert_eq!(
            disk["keys"]
                .as_table()
                .unwrap()
                .iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>(),
            ["Q", "F12", "H"]
        );
        let mut reordered = "[keys]\nH='Help'\nG='Guide'\nQ='LeftTop'\n"
            .parse::<DocumentMut>()
            .unwrap();
        let mut draft = base.clone();
        draft["keys"]["Q"] = value("Center");
        merge_edits(base.as_table(), draft.as_table(), reordered.as_table_mut());
        assert_eq!(
            reordered["keys"]
                .as_table()
                .unwrap()
                .iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>(),
            ["H", "G", "Q"]
        );
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
    fn parsing_preserves_comments_hashes_and_multiline_strings() {
        let source = "# 旧注释\n[tip]\nwindow_format = '''中文 # 注释不是注释\n下一行''' # 删除\nfont = \"SystemFont\" # English\n";
        let (doc, _) = document(source).unwrap();
        assert_eq!(
            doc["tip"]["window_format"].as_str(),
            Some("中文 # 注释不是注释\n下一行")
        );
        assert!(doc.to_string().contains("# English"));
        assert_eq!(doc.to_string(), source);
    }
    #[test]
    fn saving_an_unrelated_setting_preserves_tabs_and_unicode_comments() {
        let folder = std::env::temp_dir().join(format!("winsizep-preserve-{}", std::process::id()));
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join("config.toml");
        let original = format!(
            "{}\n# 保留用户说明\n[[exclusions]]\ntitle = 'before\tafter'\n",
            help()
        );
        fs::write(&path, &original).unwrap();
        let loaded = load_snapshot(&path, None).unwrap();
        assert_eq!(
            loaded.text.replace("\r\n", "\n"),
            original.replace("\r\n", "\n")
        );
        let mut draft = loaded.document;
        draft["general"]["preview"] = value(false);
        let saved = save_settings(&path, &original, &draft).unwrap();
        assert!(saved.contains("# 保留用户说明"));
        assert!(saved.contains("'before\tafter'"));
        assert_eq!(
            load(&path, None).unwrap().exclusions[0].title,
            "before\tafter"
        );
        fs::remove_file(path).unwrap();
        fs::remove_dir(folder).unwrap();
    }
}
