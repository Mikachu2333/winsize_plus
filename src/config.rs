use crate::{cli::Switch, layout::Layout};
use std::{
    collections::HashMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    ptr::null,
};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};
use windows_sys::Win32::{
    Foundation::*, Globalization::GetUserDefaultUILanguage, System::Threading::*,
};
use windows_sys::core::w;

fn system_zh() -> bool {
    (unsafe { GetUserDefaultUILanguage() } & 0x3ff) == 4
}
pub fn help() -> &'static str {
    if system_zh() {
        include_str!("example.zh.toml")
    } else {
        include_str!("example.en.toml")
    }
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
    pub wait_ms: u32,
    pub elevate: bool,
    pub autostart: bool,
    pub preview: bool,
    pub restart: bool,
    pub show: String,
    pub duration: u32,
    pub theme: String,
    pub colors: [u32; 4],
    pub font: String,
    pub font_size: i32,
    pub lines: [Line; 2],
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
            .filter(|number| (7..255).contains(number) || *number == 3);
    }
    Some(match text.as_str() {
        "grave" | "`" | "~" | "～" => 0xc0,
        "minus" | "-" => 0xbd,
        "equal" | "=" => 0xbb,
        "backslash" | "\\" => 0xdc,
        "space" => 0x20,
        "tab" => 9,
        "semicolon" => 0xba,
        "comma" => 0xbc,
        "period" => 0xbe,
        "slash" => 0xbf,
        "insert" => 0x2d,
        _ => return None,
    })
}

fn merge(table: &mut Table, defaults: &Table, named: bool) -> bool {
    let mut changed = false;
    for (key, item) in defaults {
        if let Some(existing) = table.get_mut(key) {
            if let (Some(child), Some(default)) = (existing.as_table_mut(), item.as_table()) {
                changed |= merge(
                    child,
                    default,
                    matches!(key, "presets" | "keys" | "commands"),
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

fn is_builtin_action(name: &str) -> bool {
    matches!(
        name,
        "top"
            | "ignore"
            | "help"
            | "version"
            | "command"
            | "restart"
            | "quit"
            | "time"
            | "monitor"
            | "guide"
            | "horizontal"
            | "vertical"
            | "cross"
            | "reload"
    )
}

impl Config {
    pub fn cell(&self, monitor: u32, cell: u32) -> Option<&str> {
        [monitor * 10, monitor * 10 + cell, cell]
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
    fn parse(doc: &DocumentMut) -> Result<Self, String> {
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
        let boolean = |key: &str| {
            field("general", key)?
                .as_bool()
                .ok_or_else(|| format!("general.{key}"))
        };
        let color = |section: &str, key: &str| {
            let hex = text(section, key)?.trim_start_matches('#');
            let rgb = u32::from_str_radix(hex, 16)
                .ok()
                .filter(|_| hex.len() == 6)
                .ok_or_else(|| format!("{section}.{key}"))?;
            Ok::<_, String>((rgb >> 16) | (rgb & 0xff00) | ((rgb & 255) << 16))
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
        for (key, item) in entries("presets")? {
            let layout = item
                .as_str()
                .ok_or(key)?
                .parse()
                .map_err(|err: String| format!("presets.{key}: {err}"))?;
            if presets.insert(key.to_ascii_lowercase(), layout).is_some() {
                return Err(key.into());
            }
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
            let name = action(item)?;
            if !name.is_empty() && keys.insert(vk(key).ok_or(key)?, name).is_some() {
                return Err(key.into());
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
            commands.insert(key.to_ascii_lowercase(), action(item)?);
        }
        let mut grid = HashMap::new();
        for (key, item) in entries("grid")? {
            let name = item.as_str().ok_or(key)?.to_ascii_lowercase();
            if !name.is_empty() && !presets.contains_key(&name) {
                return Err(format!("grid.{key}: {name}"));
            }
            grid.insert(key.parse().map_err(|_| key.to_string())?, name);
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
        Ok(Self {
            zh: language == "zh-CN" || (language == "auto" && system_zh()),
            alt: choice("general", "alt", &["left", "right", "both"])?,
            gesture: choice("general", "gesture", &["left", "right", "both", "off"])?,
            double_ms: number("general", "double_ms", 50, 2000)? as u32,
            wait_ms: number("general", "wait_ms", 100, 60000)? as u32,
            elevate: boolean("elevate")?,
            autostart: boolean("autostart")?,
            preview: boolean("preview")?,
            restart: boolean("restart_on_change")?,
            show: choice("tip", "show", &["off", "time", "monitor"])?,
            duration: number("tip", "duration_ms", 100, 120000)? as u32,
            theme: choice("tip", "theme", &["system", "dark", "light"])?,
            colors: [
                color("tip", "background")?,
                color("tip", "foreground")?,
                color("tip", "dark_background")?,
                color("tip", "dark_foreground")?,
            ],
            font: text("tip", "font")?.into(),
            font_size: number("tip", "font_size", 6, 128)? as i32,
            lines: [line("horizontal")?, line("vertical")?],
            presets,
            keys,
            commands,
            grid,
            exclusions,
        })
    }
}

pub fn encode(text: &str) -> Vec<u8> {
    std::iter::once(0xfeffu16)
        .chain(text.encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect()
}
fn decode(bytes: &[u8]) -> Result<String, String> {
    let rest = bytes
        .strip_prefix(&[255, 254])
        .filter(|rest| rest.len() % 2 == 0)
        .ok_or("Expected UTF-16LE BOM / 需要 UTF-16LE BOM")?;
    String::from_utf16(
        &rest
            .as_chunks::<2>()
            .0
            .iter()
            .copied()
            .map(u16::from_le_bytes)
            .collect::<Vec<_>>(),
    )
    .map_err(|err| err.to_string())
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
        let handle = CreateMutexW(null(), 0, w!("Local\\WinSizeP.Config.v1"));
        if handle.is_null() {
            return Err(io::Error::last_os_error().to_string());
        }
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
        file.write_all(&encode(text))?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
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
    let mut doc = text.parse::<DocumentMut>().map_err(|err| err.to_string())?;
    let defaults = help()
        .parse::<DocumentMut>()
        .map_err(|err| err.to_string())?;
    let mut changed = merge(doc.as_table_mut(), defaults.as_table(), false) || new;
    if let Some(general) = doc.get_mut("general").and_then(Item::as_table_mut) {
        changed |= general.remove("gesture_button").is_some();
    }
    let mut config = Config::parse(&doc)?;
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
