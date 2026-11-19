#[derive(Default)]
pub struct Selector {
    pub pid: Option<u32>,
    pub hwnd: Option<usize>,
    pub class: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
}

#[derive(Clone, Copy)]
pub enum Switch {
    On,
    Off,
    Toggle,
}
impl Switch {
    pub fn value(self, current: bool) -> bool {
        match self {
            Self::On => true,
            Self::Off => false,
            Self::Toggle => !current,
        }
    }
}

#[derive(Default)]
pub struct Request {
    pub target: Selector,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub size: Option<(i32, i32)>,
    pub preset: Option<String>,
    pub top: Option<Switch>,
    pub ignore: Option<Switch>,
}

impl Request {
    pub fn geometry(&self) -> bool {
        self.x.is_some() || self.y.is_some() || self.size.is_some() || self.preset.is_some()
    }
}

pub fn parse(args: &[String]) -> Result<Request, String> {
    let mut req = Request::default();
    let mut args: std::collections::VecDeque<_> = args.iter().map(String::as_str).collect();
    while let Some(argument) = args.pop_front() {
        // Only the token being consumed as an option is normalized/split.
        // Values may legitimately start with '-' or contain '='.
        let (key, inline) = argument
            .strip_prefix("--")
            .and_then(|_| argument.split_once('='))
            .map_or((argument, None), |(key, value)| (key, Some(value)));
        let normalized = key.to_ascii_lowercase();
        let key = normalized.as_str();
        if let Some(value) = inline {
            args.push_front(value);
        }
        let bad = || format!("Invalid or missing value / 参数错误: {key}");
        match key {
            "--top" | "--ignore" => {
                let switch = match args.front().copied() {
                    None => Switch::Toggle,
                    Some(next) if next.starts_with('-') && inline.is_none() => Switch::Toggle,
                    Some(_) => match args.pop_front().map(str::to_ascii_lowercase).as_deref() {
                        Some("on" | "1" | "true") => Switch::On,
                        Some("off" | "0" | "false") => Switch::Off,
                        Some("toggle") => Switch::Toggle,
                        _ => return Err(bad()),
                    },
                };
                if key == "--top" {
                    req.top = Some(switch)
                } else {
                    req.ignore = Some(switch)
                }
            }
            "--location" | "-l" | "--size" | "-s" => {
                let first = args.pop_front().ok_or_else(bad)?;
                let (first, second) = match first.split_once(',') {
                    Some(pair) => pair,
                    None => (first, args.pop_front().ok_or_else(bad)?),
                };
                let pair = (
                    first.trim().parse().map_err(|_| bad())?,
                    second.trim().parse().map_err(|_| bad())?,
                );
                if matches!(key, "--size" | "-s") {
                    if pair.0 <= 0 || pair.1 <= 0 {
                        return Err(bad());
                    }
                    req.size = Some(pair)
                } else {
                    (req.x, req.y) = (Some(pair.0), Some(pair.1))
                }
            }
            "--pid" => {
                let pid = args
                    .pop_front()
                    .ok_or_else(bad)?
                    .parse()
                    .map_err(|_| bad())?;
                req.target.pid.get_or_insert(pid);
            }
            "--hwnd" => {
                let text = args.pop_front().ok_or_else(bad)?;
                req.target.hwnd.get_or_insert(
                        if let Some(hex) =
                            text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))
                        {
                            usize::from_str_radix(hex, 16)
                        } else {
                            text.parse()
                        }
                        .map_err(|_| bad())?,
                    );
            }
            "--x" | "-x" => {
                req.x = Some(
                    args.pop_front()
                        .ok_or_else(bad)?
                        .parse()
                        .map_err(|_| bad())?,
                )
            }
            "--y" | "-y" => {
                req.y = Some(
                    args.pop_front()
                        .ok_or_else(bad)?
                        .parse()
                        .map_err(|_| bad())?,
                )
            }
            "--name" | "--class" | "--title" | "--preset" => {
                let value = args
                    .pop_front()
                    .filter(|value| {
                        !value.is_empty() && (inline.is_some() || !value.starts_with("--"))
                    })
                    .ok_or_else(bad)?
                    .to_string();
                match key {
                    "--name" => {
                        req.target.name.get_or_insert(value);
                    }
                    "--class" => {
                        req.target.class.get_or_insert(value);
                    }
                    "--title" => {
                        req.target.title.get_or_insert(value);
                    }
                    _ => req.preset = Some(value),
                }
            }
            _ => return Err(format!("Unknown option / 未知参数: {key}")),
        }
    }
    if !req.geometry() && req.top.is_none() && req.ignore.is_none() {
        return Err("Specify an action / 请提供操作参数 (--help)".into());
    }
    Ok(req)
}

/// A pipeline contains one argument list per line. Quotes group paths and titles; backslashes
/// are literal except before double quotes, following the Windows command-line convention.
pub fn split(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut started = false;
    let mut chars = line.trim_start_matches('\u{feff}').chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                let mut count = 1;
                while chars.peek() == Some(&'\\') {
                    chars.next();
                    count += 1;
                }
                if chars.peek() == Some(&'"') {
                    word.extend(std::iter::repeat_n('\\', count / 2));
                    chars.next();
                    if count % 2 == 0 {
                        quoted = !quoted;
                    } else {
                        word.push('"');
                    }
                } else {
                    word.extend(std::iter::repeat_n('\\', count));
                }
                started = true;
            }
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return Err("Unclosed quote in stdin / 管道输入引号未闭合".into());
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    #[test]
    fn option_normalization_never_changes_selector_values() {
        for values in [
            vec!["--TITLE", "-Secret=AbC", "--TOP"],
            vec!["--title=-Secret=AbC", "--top"],
        ] {
            let request =
                super::parse(&values.into_iter().map(str::to_owned).collect::<Vec<_>>()).unwrap();
            assert_eq!(request.target.title.as_deref(), Some("-Secret=AbC"));
        }
        let request =
            super::parse(&["--class=--Mixed=Name", "--X", "-100"].map(str::to_owned)).unwrap();
        assert_eq!(request.target.class.as_deref(), Some("--Mixed=Name"));
        assert_eq!(request.x, Some(-100));
        assert!(super::parse(&["--top=-invalid".into()]).is_err());
    }
    #[test]
    fn repeated_selectors_keep_the_first_and_continue_reading_actions() {
        let args = [
            "--pid=124",
            "--pid",
            "222",
            "--name=notepad.exe",
            "--SIZE",
            "900,600",
            "--TOP",
            "ON",
        ]
        .map(str::to_owned);
        let request = super::parse(&args).unwrap();
        assert_eq!(request.target.pid, Some(124));
        assert_eq!(request.target.name.as_deref(), Some("notepad.exe"));
        assert_eq!(request.size, Some((900, 600)));
        assert!(request.top.unwrap().value(false));
    }
    #[test]
    fn pipeline_preserves_quoted_unicode_and_windows_paths() {
        let args =
            super::split(r#"--title "中文 文档" --name "C:\Windows\notepad.exe" --preset Center"#)
                .unwrap();
        assert_eq!(args[1], "中文 文档");
        assert_eq!(args[3], r"C:\Windows\notepad.exe");
        assert!(super::split("--title \"broken").is_err());
        for text in [r"C:\path\", "a\"b", "中文 文档"] {
            assert_eq!(super::split(&crate::quote_argument(text)).unwrap(), [text]);
        }
    }
}
