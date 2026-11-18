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
    let mut args = args.iter().map(String::as_str).peekable();
    while let Some(key) = args.next() {
        let bad = || format!("Invalid or missing value / 参数错误: {key}");
        match key {
            "--top" | "--ignore" => {
                let switch = match args.peek().copied() {
                    None => Switch::Toggle,
                    Some(next) if next.starts_with('-') => Switch::Toggle,
                    Some(_) => match args.next() {
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
                let first = args.next().ok_or_else(bad)?;
                let (first, second) = match first.split_once(',') {
                    Some(pair) => pair,
                    None => (first, args.next().ok_or_else(bad)?),
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
                req.target.pid = Some(args.next().ok_or_else(bad)?.parse().map_err(|_| bad())?)
            }
            "--hwnd" => {
                let text = args.next().ok_or_else(bad)?;
                req.target.hwnd =
                    Some(
                        if let Some(hex) =
                            text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))
                        {
                            usize::from_str_radix(hex, 16)
                        } else {
                            text.parse()
                        }
                        .map_err(|_| bad())?,
                    )
            }
            "--x" | "-x" => req.x = Some(args.next().ok_or_else(bad)?.parse().map_err(|_| bad())?),
            "--y" | "-y" => req.y = Some(args.next().ok_or_else(bad)?.parse().map_err(|_| bad())?),
            "--name" | "--class" | "--title" | "--preset" => {
                let value = args
                    .next()
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .ok_or_else(bad)?
                    .to_string();
                match key {
                    "--name" => req.target.name = Some(value),
                    "--class" => req.target.class = Some(value),
                    "--title" => req.target.title = Some(value),
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
