use crate::config::{self, Config};
use toml_edit::DocumentMut;

pub const TABS: [(&str, &str); 9] = [
    ("General", "通用"),
    ("Tips", "提示框"),
    ("Screen regions", "屏幕分区"),
    ("Region outlines", "标识框线"),
    ("Quick actions", "快捷操作"),
    ("Crosshair", "十字线"),
    ("Right-click grid", "右键九宫格"),
    ("Mouse annotations", "鼠标标注"),
    ("Keyboard annotations", "键盘标注"),
];
#[derive(Clone)]
pub enum Kind {
    Bool,
    Number,
    Text,
    Choice(Vec<(String, String)>),
    Font,
    Fallbacks,
    Color(ColorChannels),
    Mapping,
    Add,
    OpenConfig,
    Preview,
    Note,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorChannels {
    Rgb,
    Rgba,
}
impl ColorChannels {
    pub fn count(self) -> usize {
        match self {
            Self::Rgb => 3,
            Self::Rgba => 4,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Rgb => "RGB",
            Self::Rgba => "RGBA",
        }
    }
}
#[derive(Clone)]
pub struct Field {
    pub section: String,
    pub key: String,
    pub label: String,
    pub hint: String,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceContext {
    Language,
    AltSide,
    MouseButton,
    TipContent,
    Theme,
    Font,
    Action,
    Position,
}

pub fn option(cfg: &Config, context: ChoiceContext, value: &str) -> String {
    use ChoiceContext::*;
    let normalized = value.to_ascii_lowercase();
    let (en, zh) = match (context, normalized.as_str()) {
        (Language, "auto") => ("System language", "跟随系统"),
        (Language, "zh-cn") => ("简体中文", "简体中文"),
        (Language, "en") => ("English", "English"),
        (AltSide, "left") => ("Left Alt", "左 Alt"),
        (AltSide, "right") => ("Right Alt", "右 Alt"),
        (AltSide, "both") => ("Either Alt", "两侧 Alt 均可"),
        (AltSide, "off") => ("Disabled", "关闭"),
        (MouseButton, "left") => ("Left button", "左键"),
        (MouseButton, "right") => ("Right button", "右键"),
        (MouseButton, "middle") => ("Middle button", "中键"),
        (MouseButton, "x1") => ("Side button 1", "侧键 1"),
        (MouseButton, "x2") => ("Side button 2", "侧键 2"),
        (Theme, "system") => ("System theme", "跟随系统"),
        (Theme, "light") => ("Light", "明亮"),
        (Theme, "dark") => ("Dark", "黑暗"),
        (TipContent, "window") => ("Window information", "窗口信息"),
        (TipContent, "time") => ("Time", "时间"),
        (TipContent, "monitor") => ("Monitor number", "屏幕编号"),
        (TipContent, "off") => ("No default tip", "不显示默认提示"),
        (Font, "systemfont") => ("System UI font", "系统默认 UI 字体"),
        (Action, "top") => ("Toggle always on top", "置顶 / 取消置顶"),
        (Action, "centerwindow") => ("Center on pointer's monitor", "在鼠标所在屏幕居中"),
        (Action, "ignore") => ("Exclude window", "忽略窗口"),
        (Action, "help") => ("Show help", "显示帮助"),
        (Action, "version") => ("Show version", "显示版本"),
        (Action, "command") => ("Enter command", "输入指令"),
        (Action, "restart") => ("Restart", "重启"),
        (Action, "quit") => ("Exit", "退出"),
        (Action, "time") => ("Show time", "显示时间"),
        (Action, "monitor") => ("Show monitor number", "显示屏幕编号"),
        (Action, "guide" | "cross") => ("Toggle crosshair", "显示 / 隐藏十字线"),
        (Action, "horizontal") => ("Horizontal crosshair", "水平十字线"),
        (Action, "vertical") => ("Vertical crosshair", "垂直十字线"),
        (Action, "borders") => ("Toggle region outlines", "显示 / 隐藏框线"),
        (Action, "reload") => ("Reload configuration", "重载配置"),
        (Action, "settings") => ("Open settings", "打开设置"),
        (Action, "mousecast") => ("Toggle mouse annotations", "开启 / 关闭鼠标标注"),
        (Action, "keycast") => ("Toggle keyboard annotations", "开启 / 关闭键盘标注"),
        (Position, "bottom-left") => ("Bottom left", "左下"),
        (Position, "bottom-right") => ("Bottom right", "右下"),
        (Position, "top-left") => ("Top left", "左上"),
        (Position, "top-right") => ("Top right", "右上"),
        _ => (value, value),
    };
    cfg.tr(en, zh).to_owned()
}
fn choices(cfg: &Config, context: ChoiceContext, values: &[&str]) -> Kind {
    Kind::Choice(
        values
            .iter()
            .map(|value| (value.to_string(), option(cfg, context, value)))
            .collect(),
    )
}
fn describe(section: &str, key: &str) -> (&'static str, &'static str, &'static str, &'static str) {
    if section == "mousecast" && key == "opacity" {
        return (
            "Opacity",
            "不透明度",
            "96–224. Accent-derived colors adapt to light and dark themes; shapes stay translucent.",
            "96–224。主题派生色适配明暗模式，图形保持半透明并可辨认。",
        );
    }
    if matches!(section, "mousecast" | "keycast") {
        return match key {
            "enabled" => (
                "Enable on startup",
                "启动时启用",
                "Hotkeys toggle the current session; this setting controls the initial state.",
                "快捷键切换本次运行的状态；此设置决定启动时的初始状态。",
            ),
            "swap_side_buttons" => (
                "Swap upper/lower side buttons",
                "对调上下侧键显示",
                "Enabled by default: X1 appears below X2. Turn off to restore X1 above X2; button actions do not change.",
                "默认开启：X1 显示在 X2 下方。关闭后恢复 X1 在上、X2 在下；实际按键功能不变。",
            ),
            "duration_ms" => (
                "Display duration (ms)",
                "显示时长（毫秒）",
                "Keep each annotation visible for 100–10000 ms before the shared 400 ms fade.",
                "每次标注保持 100–10000 毫秒，再统一用 400 毫秒淡出。",
            ),
            "radius" => (
                "Shape radius",
                "图形半径",
                "4–64 logical pixels, scaled per monitor. All shapes and offsets use this size.",
                "4–64 逻辑像素，按屏幕 DPI 缩放。所有图形及相对位置按比例调整。",
            ),
            "background" => (
                "Background",
                "背景颜色",
                "Annotation label background.",
                "标注标签的背景颜色。",
            ),
            "foreground" => (
                "Text color",
                "文字颜色",
                "Annotation label text.",
                "标注标签的文字颜色。",
            ),
            "opacity" => (
                "Opacity",
                "不透明度",
                "32–255; affects the label and click indicator.",
                "32–255，作用于标签及点击指示。",
            ),
            "font" => (
                "Font",
                "字体",
                "SystemFont uses the Windows UI font.",
                "SystemFont 使用 Windows 界面字体。",
            ),
            "font_size" => (
                "Font size (pt)",
                "字号（磅）",
                "6–72 points, scaled per monitor.",
                "6–72 磅，按屏幕 DPI 缩放。",
            ),
            "history" => (
                "Visible keystrokes",
                "显示条数",
                "1–8 labels; identical consecutive keys show a repeat count.",
                "1–8 条；连续相同按键显示重复次数。",
            ),
            "position" => (
                "Position",
                "显示位置",
                "Corner of the monitor containing the pointer when a key is pressed.",
                "按键时鼠标所在显示器的指定角落。",
            ),
            _ => (
                "Setting",
                "设置项",
                "Changes apply automatically.",
                "更改自动应用。",
            ),
        };
    }
    match (section, key) {
        ("shortcuts", "center_enabled") => (
            "Enable window centering",
            "启用窗口居中",
            "Alt + middle click centers on the pointer's monitor, scales for its DPI and clamps to its work area.",
            "Alt + 中键点击：按目标屏幕 DPI 换算大小，超出工作区的边缩窄后居中。",
        ),
        ("shortcuts", "center_button") => (
            "Center button",
            "居中按键",
            "Enabled mouse actions must use distinct buttons. Right click is reserved for the grid.",
            "已启用的鼠标操作不可使用重复按键；右键点击用于九宫格。",
        ),
        ("general", "language") => (
            "Interface language",
            "界面语言",
            "Applies to controls, choices and help.",
            "应用到控件、选项和帮助文本。",
        ),
        ("general", "autostart") => (
            "Start when signing in",
            "登录时自动启动",
            "The program creates or removes its scheduled task automatically.",
            "程序自动创建或移除计划任务。",
        ),
        ("general", "elevate") => (
            "Run as administrator",
            "以管理员身份运行",
            "Updates the logon task and restarts with the requested permission; Windows may request consent.",
            "更新自启任务，并按新权限重启；Windows 可能请求授权。",
        ),
        ("general", "preview") => (
            "Show region outlines after Alt",
            "Alt 触发后显示分区框线",
            "Show with the tip after Alt activation and hide together. Releasing held Alt closes both.",
            "Alt 触发后与提示框同步显示、隐藏；长按时松开 Alt 即关闭。",
        ),
        ("general", "alt_hold_ms") => (
            "Alt hold delay (milliseconds)",
            "Alt 长按延迟（毫秒）",
            "0–60000; default 2000. Keyboard and mouse actions activate after this delay. Earlier combinations go to the application and cancel activation until Alt is released. Double-tap Alt activates directly.",
            "0–60000，默认 2000。延迟结束后启用键盘和鼠标操作；提前输入组合键会交给应用，并取消本次长按触发。双击 Alt 可直接触发。",
        ),
        ("shortcuts", "alt") => (
            "Alt key for shortcuts",
            "快捷操作使用的 Alt",
            "Hold Alt until activation, then press a key; or double-tap Alt then press a key. Double-tap timing follows Windows.",
            "长按 Alt 触发后配合按键，或双击 Alt 后按键；双击时间跟随 Windows。",
        ),
        ("shortcuts", "gesture") => (
            "Alt key for mouse actions",
            "鼠标操作使用的 Alt",
            "Used for moving, resizing and right-click placement.",
            "用于拖动窗口、调整大小和右键九宫格。",
        ),
        ("shortcuts", "move_enabled") => (
            "Enable window dragging",
            "启用拖动窗口",
            "Moves the window selected when Alt was pressed.",
            "移动按下 Alt 时选中的窗口。",
        ),
        ("shortcuts", "resize_enabled") => (
            "Enable window resizing",
            "启用调整窗口大小",
            "Keeps the top-left corner fixed and moves the bottom-right corner.",
            "固定左上角，拖动右下角调整大小。",
        ),
        ("shortcuts", "move_button") => (
            "Button for moving",
            "移动窗口的鼠标按键",
            "Hold Alt and drag with this button.",
            "按住 Alt，用此按键拖动。",
        ),
        ("shortcuts", "resize_button") => (
            "Button for resizing",
            "调整大小的鼠标按键",
            "A right click without dragging still places by the grid.",
            "右键只点击、不拖动时，仍使用九宫格调整位置。",
        ),
        ("tip", "show") => (
            "Default tip content",
            "默认提示内容",
            "Choose window information, time, monitor number, or hide the default tip.",
            "选择窗口信息、时间、屏幕编号，或不显示默认提示。",
        ),
        ("tip", "duration_ms") => (
            "Display time (milliseconds)",
            "停留时间（毫秒）",
            "Starts when the tip appears, and also controls Alt region previews. Help stays until Esc.",
            "从提示框显示时开始计时，同时决定 Alt 分区预览的停留时间。帮助保持显示，直到按 Esc。",
        ),
        ("tip", "window_format") => (
            "Window information format",
            "窗口信息格式",
            "Tokens: {hwnd}, {title}, {pid}, {process}, {display_name}, {class}.",
            "可用：{hwnd}、{title}、{pid}、{process}、{display_name}、{class}。",
        ),
        ("tip", "time_format") => (
            "Time format",
            "时间格式",
            "Example: YYYY-MM-DD hh:mm:ss. The default is hh:mm.",
            "例如 YYYY-MM-DD hh:mm:ss，默认 hh:mm。",
        ),
        ("tip", "theme") => (
            "Tip theme",
            "提示框主题",
            "Follows the Windows app theme unless a mode is selected.",
            "默认跟随 Windows 应用主题。",
        ),
        (_, "font") => (
            "Font family",
            "字体",
            "Each item previews its font. SystemFont uses the system UI font.",
            "每项用对应字体显示；SystemFont 为系统默认 UI 字体。",
        ),
        (_, "font_fallbacks") => (
            "Fallback fonts",
            "回退字体",
            "Add up to three fonts. Drag to reorder; select an item to remove it.",
            "最多三项；拖动可调整顺序，选中后可删除。",
        ),
        (_, "font_size") => (
            "Font size (pt)",
            "字号（磅）",
            "The preview updates automatically.",
            "自动更新实际提示框或区域标签预览。",
        ),
        (_, "font_bold") => (
            "Bold",
            "粗体",
            "Uses the selected font's bold style.",
            "使用所选字体的粗体样式。",
        ),
        (_, "font_italic") => (
            "Italic",
            "斜体",
            "Uses the selected font's italic style.",
            "使用所选字体的斜体样式。",
        ),
        (_, "background") => (
            "Light background",
            "明亮模式背景",
            "Click the swatch or edit the color channels.",
            "点击色块选色，或直接修改颜色通道。",
        ),
        (_, "foreground") => (
            "Light text",
            "明亮模式文字",
            "Click the swatch or edit the color channels.",
            "点击色块选色，或直接修改颜色通道。",
        ),
        (_, "dark_background") => (
            "Dark background",
            "黑暗模式背景",
            "Click the swatch or edit the color channels.",
            "点击色块选色，或直接修改颜色通道。",
        ),
        (_, "dark_foreground") => (
            "Dark text",
            "黑暗模式文字",
            "Click the swatch or edit the color channels.",
            "点击色块选色，或直接修改颜色通道。",
        ),
        ("outline", "opacity") => (
            "Line opacity (both themes)",
            "线条不透明度（明暗主题共用）",
            "0–255. Shared by the light and dark outline colors.",
            "0–255，明亮与黑暗框线颜色共用此值。",
        ),
        ("outline", "color") => (
            "Light-mode line color (RGB)",
            "明亮模式线条颜色（RGB）",
            "Opacity is configured separately for both themes.",
            "不透明度在下方统一设置，两种主题共用。",
        ),
        ("outline", "dark_color") => (
            "Dark-mode line color (RGB)",
            "黑暗模式线条颜色（RGB）",
            "Opacity is configured separately for both themes.",
            "不透明度在下方统一设置，两种主题共用。",
        ),
        (_, "color") => (
            "Line color (RGBA)",
            "线条颜色（RGBA）",
            "RGB and opacity (A) range from 0 to 255; the preview is live.",
            "RGB 与不透明度 A 均为 0–255，实时预览。",
        ),
        (_, "dark_color") => (
            "Dark-mode line color (RGBA)",
            "黑暗模式线条颜色（RGBA）",
            "Used when Windows uses dark mode.",
            "Windows 使用黑暗模式时生效。",
        ),
        (_, "width") => (
            "Line width",
            "线宽",
            "Logical pixels, scaled with the display DPI.",
            "单位为逻辑像素，按屏幕 DPI 缩放。",
        ),
        (_, "gap") => (
            "Gap around the pointer",
            "鼠标周围的空隙",
            "Radius in logical pixels.",
            "空隙半径，单位为逻辑像素。",
        ),
        (_, "fill_opacity") => (
            "Selection fill opacity",
            "选区填充不透明度",
            "Translucent system accent fill; 0 is transparent and 255 is opaque.",
            "使用半透明系统强调色填充；0 为完全透明，255 为完全不透明。",
        ),
        ("crosshair", "fps") => (
            "Maximum frames per second",
            "每秒最大帧数",
            "Integer, 24–1000. Default: 30. Frames are only drawn when the pointer moves.",
            "正整数，范围 24–1000，默认 30；仅在鼠标移动时绘制。",
        ),
        _ => (
            "Setting",
            "设置项",
            "Changes are saved and applied automatically.",
            "更改后自动保存并应用。",
        ),
    }
}

pub fn fields(tab: usize, axis: usize, cfg: &Config, doc: &DocumentMut) -> Vec<Field> {
    static DEFAULTS: std::sync::LazyLock<DocumentMut> = std::sync::LazyLock::new(|| {
        config::help()
            .parse()
            .expect("valid built-in configuration")
    });
    let mut fields = Vec::new();
    let sections: &[&str] = match tab {
        0 => &["general"],
        1 => &["tip"],
        2 => &["presets", "regions"],
        3 => &["outline"],
        4 => &["shortcuts", "keys", "commands"],
        5 if cfg.crosshair_unified || axis != 1 => &["crosshair", "horizontal"],
        5 => &["crosshair", "vertical"],
        7 => &["mousecast"],
        8 => &["keycast"],
        _ => &["grid"],
    };
    if matches!(tab, 2 | 3 | 5 | 6 | 7 | 8) {
        fields.push(Field { section: String::new(), key: String::new(), label: cfg.tr("Live desktop preview", "实时桌面预览").into(), hint: cfg.tr("Esc closes or resets the preview. Region preview requires selecting a region below.", "Esc 关闭或重置预览；分区预览还需选中下方的分区。").into(), kind: Kind::Preview });
        if tab >= 7 {
            fields.last_mut().unwrap().hint=cfg.tr("Repeats a sample animation without capturing input. Esc stops it; previews pause while settings is inactive.","循环播放示例动画，不捕获输入。Esc 停止预览；设置窗口失焦时暂停。").into();
        }
    }
    if tab == 0 {
        fields.push(Field { section: String::new(), key: String::new(), label: cfg.tr("Changes are saved and applied automatically", "更改后自动保存并应用").into(), hint: cfg.tr("Manual file edits take precedence over conflicting GUI edits. To allow an excluded window again, remove its exclusion from the configuration file.", "文件中的手动修改优先于 GUI 的冲突修改。若要重新允许已排除的窗口，请从配置文件中移除对应排除项。").into(), kind: Kind::Note });
        fields.push(Field { section: String::new(), key: String::new(), label: cfg.tr("Open configuration file", "打开配置文件").into(), hint: cfg.tr("Open winsizep_config.toml in Notepad. Windows 10 and 11 system tray overflow menus are excluded by default.", "使用记事本打开 winsizep_config.toml。默认排除 Windows 10 和 11 的系统托盘弹出菜单。").into(), kind: Kind::OpenConfig });
    }
    if tab == 2 {
        fields.push(Field { section: String::new(), key: String::new(), label: cfg.tr("Region expressions", "分区表达式").into(), hint: cfg.tr("x | y | width | height [| monitor]. sw/sh: work-area width/height; ww/wh: current window; vw/vh: virtual desktop. Units are percentages: 50sw-8 = half the work-area width minus 8 px. Monitor 0 follows the pointer; vw/vh use the virtual-desktop origin.", "x | y | 宽 | 高 [| 屏号]。sw/sh：工作区宽高；ww/wh：当前窗口宽高；vw/vh：虚拟桌面宽高。单位均为百分比，例如 50sw-8 为半屏宽减 8 像素。屏号 0 跟随鼠标；含 vw/vh 时从虚拟桌面原点计算。").into(), kind: Kind::Note });
    }
    if tab == 6 {
        fields.push(Field { section: String::new(), key: String::new(), label: cfg.tr("Monitor-specific cells", "设置指定屏幕的区块").into(), hint: cfg.tr("Use monitor number × 10 + cell. For monitor 3, bottom-left: 3*10+7=37. Hover for 2 seconds to preview the mapped region.", "想设置某块屏幕上的区块，只需要将对应的屏幕编号乘10，再加上格子号即可，例如想设置屏幕3的左下角，编号就是 3*10+7=37。悬停2秒预览对应区域。").into(), kind: Kind::Note });
    }
    if tab == 7 {
        fields.push(Field{section:String::new(),key:String::new(),label:cfg.tr("Filled mouse indicators","填色鼠标指示").into(),
            hint:cfg.tr("Left/right: two halves of a circle; middle: a small wheel rectangle; scrolling: directional triangles. Held buttons and continuous scrolling stay visible. Fade timing is fixed at about 0.5 seconds.","左右键拼成完整圆形，中键为靠上的小长方形，滚动为方向三角形。长按和连续滚动保持显示；淡入淡出固定约 0.5 秒，不可调。").into(),kind:Kind::Note});
    }
    for section in sections {
        let Some(table) = doc.get(section).and_then(|item| item.as_table()) else {
            continue;
        };
        let mapping = matches!(*section, "presets" | "keys" | "commands" | "grid");
        let mut entries: Vec<_> = table.iter().collect();
        if *section == "shortcuts" {
            let order = [
                "move_enabled",
                "move_button",
                "resize_enabled",
                "resize_button",
                "center_enabled",
                "center_button",
                "gesture",
                "alt",
            ];
            entries
                .sort_by_key(|(key, _)| order.iter().position(|v| v == key).unwrap_or(order.len()));
        }
        for (key, item) in entries {
            if *section == "crosshair" && key == "mode" {
                continue;
            }
            if !mapping
                && ((key == "opacity" && !matches!(*section, "outline" | "mousecast" | "keycast"))
                    || DEFAULTS
                        .get(section)
                        .and_then(|table| table.get(key))
                        .is_none())
            {
                continue;
            }
            let (en, zh, hint_en, hint_zh) = describe(section, key);
            let mut kind = if item.is_bool() {
                Kind::Bool
            } else if item.is_integer() {
                Kind::Number
            } else {
                Kind::Text
            };
            if mapping {
                kind = Kind::Mapping;
            } else if key == "font" {
                kind = Kind::Font;
            } else if key == "font_fallbacks" {
                kind = Kind::Fallbacks;
            } else if key.contains("color")
                || matches!(
                    key,
                    "background" | "foreground" | "dark_background" | "dark_foreground"
                )
            {
                kind = Kind::Color(if matches!(*section, "horizontal" | "vertical") {
                    ColorChannels::Rgba
                } else {
                    ColorChannels::Rgb
                });
            } else {
                let values: &[&str] = match (*section, key) {
                    ("general", "language") => &["auto", "zh-CN", "en"],
                    ("shortcuts", "alt") => &["left", "right", "both"],
                    ("shortcuts", "gesture") => &["left", "right", "both", "off"],
                    ("shortcuts", "move_button" | "resize_button" | "center_button") => {
                        &["left", "right", "middle", "x1", "x2"]
                    }
                    ("tip", "show") => &["window", "time", "monitor", "off"],
                    ("tip", "theme") => &["system", "light", "dark"],
                    ("keycast", "position") => {
                        &["bottom-left", "bottom-right", "top-left", "top-right"]
                    }
                    _ => &[],
                };
                if !values.is_empty() {
                    let context = match (*section, key) {
                        ("general", "language") => ChoiceContext::Language,
                        ("shortcuts", "alt" | "gesture") => ChoiceContext::AltSide,
                        ("shortcuts", _) => ChoiceContext::MouseButton,
                        ("tip", "show") => ChoiceContext::TipContent,
                        ("keycast", "position") => ChoiceContext::Position,
                        _ => ChoiceContext::Theme,
                    };
                    kind = choices(cfg, context, values);
                }
            }
            let (label, hint) = if mapping {
                (if *section == "keys" { config::shortcut_name(key) } else { key.to_owned() }, match *section {
                "presets" => "",
                "keys" => cfg.tr("Key and action. System Alt shortcuts, modifiers, Esc and tip navigation keys are reserved. Clear the action to disable a binding.", "按键与动作；系统常用 Alt 快捷键、修饰键、Esc 和提示滚动键已保留。清空动作可禁用绑定。"),
                "commands" => cfg.tr("Command name and action. Names are case-insensitive and cannot shadow regions or redefine built-in actions/aliases.", "命令名称与动作；名称不区分大小写，不可与分区重名或改写内置动作及别名。"),
                _ => cfg.tr("Cell number and region. General cells are 1–9.", "格子编号与区域名称，通用格子编号为 1–9。"),
            }.to_owned())
            } else {
                (cfg.tr(en, zh).into(), cfg.tr(hint_en, hint_zh).into())
            };
            fields.push(Field {
                section: section.to_string(),
                key: key.into(),
                label,
                hint,
                kind,
            });
        }
        if mapping {
            fields.push(Field {
                section: section.to_string(),
                key: String::new(),
                label: cfg
                    .tr(
                        match *section {
                            "presets" => "Add region",
                            "keys" => "Add shortcut",
                            "commands" => "Add command",
                            _ => "Add grid mapping",
                        },
                        match *section {
                            "presets" => "添加分区",
                            "keys" => "添加快捷键",
                            "commands" => "添加命令",
                            _ => "添加九宫格映射",
                        },
                    )
                    .into(),
                hint: String::new(),
                kind: Kind::Add,
            });
        }
    }
    if tab == 4 {
        fields.push(Field { section: String::new(), key: String::new(), label: cfg.tr("Built-in command aliases", "内置命令别名").into(), hint: "Version = Ver; Help = H; Quit = Exit; Restart = Reboot; Top = Pin; Ignore = Exclude; Monitor = Screen".into(), kind: Kind::Note });
    }
    fields
}

pub fn group(field: &Field) -> (&str, u8) {
    let part = match field.key.as_str() {
        "font" | "font_fallbacks" | "font_size" | "font_bold" | "font_italic" => 2,
        "color" | "dark_color" | "background" | "foreground" | "dark_background"
        | "dark_foreground" | "opacity" | "fill_opacity" => 1,
        "alt" => 3,
        _ => 0,
    };
    (&field.section, part)
}
pub fn actions(cfg: &Config, presets_only: bool) -> Vec<(String, String)> {
    let mut values: Vec<_> = cfg
        .preset_names
        .values()
        .map(|name| (name.clone(), name.clone()))
        .collect();
    values.sort();
    if !presets_only {
        values.extend(config::ACTIONS.iter().map(|name| {
            let name = config::standard_name(name);
            let label = option(cfg, ChoiceContext::Action, &name);
            (name, label)
        }));
    }
    values.insert(0, (String::new(), cfg.tr("Disabled", "停用").into()));
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn color_fields_expose_only_supported_channels_and_known_options() {
        let mut doc = config::help().parse::<DocumentMut>().unwrap();
        doc["tip"]["opacity"] = toml_edit::value(120);
        doc["tip"]["font_opacity"] = toml_edit::value(120);
        doc["regions"]["font_opacity"] = toml_edit::value(120);
        doc["presets"]["opacity"] = toml_edit::value("0|0|50sw|50sh");
        let cfg = Config::parse(&doc).unwrap();
        let tips = fields(1, 0, &cfg, &doc);
        assert_eq!(
            tips.iter()
                .filter(|field| matches!(field.kind, Kind::Color(ColorChannels::Rgb)))
                .count(),
            4
        );
        assert!(!tips.iter().any(|field| field.key.contains("opacity")));
        let regions = fields(2, 0, &cfg, &doc);
        assert!(
            !regions
                .iter()
                .any(|field| field.section == "regions" && field.key == "font_opacity")
        );
        assert!(
            regions
                .iter()
                .any(|field| field.section == "presets" && field.key == "opacity")
        );
        let outline = fields(3, 0, &cfg, &doc);
        assert_eq!(
            outline
                .iter()
                .filter(|field| field.key == "opacity" && matches!(field.kind, Kind::Number))
                .count(),
            1
        );
        assert_eq!(
            outline
                .iter()
                .filter(|field| matches!(field.kind, Kind::Color(ColorChannels::Rgb)))
                .count(),
            2
        );
        for axis in 0..3 {
            let colors: Vec<_> = fields(5, axis, &cfg, &doc)
                .into_iter()
                .filter(|field| matches!(field.kind, Kind::Color(_)))
                .collect();
            assert!(!colors.is_empty());
            assert!(
                colors
                    .iter()
                    .all(|field| matches!(field.kind, Kind::Color(ColorChannels::Rgba)))
            );
        }
    }
    #[test]
    fn time_labels_depend_on_type_not_capitalization() {
        let mut doc = config::help().parse::<DocumentMut>().unwrap();
        doc["general"]["language"] = toml_edit::value("zh-CN");
        let cfg = Config::parse(&doc).unwrap();
        for spelling in ["time", "Time", "TIME"] {
            assert_eq!(option(&cfg, ChoiceContext::TipContent, spelling), "时间");
            assert_eq!(option(&cfg, ChoiceContext::Action, spelling), "显示时间");
        }
        assert_eq!(option(&cfg, ChoiceContext::Font, "Time"), "Time");
    }
}
