use crate::{
    config::{self, Config},
    settings_schema::{self as schema, Field, Kind, TABS},
};
use std::collections::HashMap;
use toml_edit::DocumentMut;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    Field {
        tab: usize,
        axis: usize,
        section: String,
        key: String,
        occurrence: usize,
    },
    Axis(usize),
}
impl Target {
    pub fn page(&self) -> (usize, usize) {
        match self {
            Self::Field { tab, axis, .. } => (*tab, *axis),
            Self::Axis(axis) => (5, if *axis < 2 { *axis } else { 0 }),
        }
    }
}
pub struct Entry {
    pub target: Target,
    pub label: String,
    terms: Vec<(i32, String)>,
}
impl Entry {
    fn term(&mut self, weight: i32, text: impl AsRef<str>) {
        let text = text.as_ref().trim().to_lowercase();
        if !text.is_empty() {
            self.terms.push((weight, text));
        }
    }
    fn region(&mut self, weight: i32, name: &str, cfg: &Config) {
        if cfg.preset(name).is_none() {
            return;
        }
        self.term(weight, cfg.region_caption(name));
        for (&key, action) in &cfg.keys {
            if cfg.action(action) == config::command_name(name) {
                self.term(weight, config::shortcut_name(&format!("VK:{key:02X}")));
            }
        }
    }
}
#[derive(Default)]
pub struct Index {
    pub entries: Vec<Entry>,
}
impl Index {
    pub fn build(doc: &DocumentMut) -> Result<Self, String> {
        let mut cfg = Config::parse(doc)?;
        let display_zh = cfg.zh;
        let mut result = Self::default();
        let mut positions = HashMap::new();
        for zh in [display_zh, !display_zh] {
            cfg.zh = zh;
            for (tab, &(page_en, page_zh)) in TABS.iter().enumerate() {
                let axes: &[usize] = if tab == 5 && !cfg.crosshair_unified {
                    &[0, 1]
                } else {
                    &[0]
                };
                for &axis in axes {
                    let mut occurrences = HashMap::new();
                    for field in schema::fields(tab, axis, &cfg, doc) {
                        let occurrence = occurrences
                            .entry((field.section.clone(), field.key.clone()))
                            .or_insert(0usize);
                        let target = Target::Field {
                            tab,
                            axis: if tab == 5 && field.section != "vertical" {
                                0
                            } else {
                                axis
                            },
                            section: field.section.clone(),
                            key: field.key.clone(),
                            occurrence: *occurrence,
                        };
                        *occurrence += 1;
                        if matches!(field.kind, Kind::Note) {
                            continue;
                        }
                        let at = *positions.entry(target.clone()).or_insert_with(|| {
                            let page = cfg.tr(page_en, page_zh);
                            let suffix = if tab == 5
                                && matches!(field.section.as_str(), "horizontal" | "vertical")
                            {
                                if axis == 0 {
                                    cfg.tr(" · Horizontal", " · 横线")
                                } else {
                                    cfg.tr(" · Vertical", " · 竖线")
                                }
                            } else {
                                ""
                            };
                            let label = if matches!(field.kind, Kind::Mapping)
                                && field.section == "presets"
                            {
                                cfg.region_caption(&field.key).replace('\n', " ")
                            } else {
                                field.label.clone()
                            };
                            result.entries.push(Entry {
                                target,
                                label: format!("{page}{suffix}  ›  {label}"),
                                terms: Vec::new(),
                            });
                            result.entries.len() - 1
                        });
                        result.entries[at].add(&field, tab, &cfg, doc);
                    }
                }
            }
        }
        cfg.zh = display_zh;
        for (axis, en, zh) in [
            (0, "Horizontal line", "横线"),
            (1, "Vertical line", "竖线"),
            (2, "Unified style", "统一样式"),
            (3, "Custom style", "自定义样式"),
        ] {
            let mut entry = Entry {
                target: Target::Axis(axis),
                label: format!("{}  ›  {}", cfg.tr(TABS[5].0, TABS[5].1), cfg.tr(en, zh)),
                terms: Vec::new(),
            };
            entry.term(100, en);
            entry.term(100, zh);
            entry.term(25, TABS[5].0);
            entry.term(25, TABS[5].1);
            for alias in aliases(5, "crosshair", "mode") {
                entry.term(75, alias);
            }
            for alias in group_aliases("crosshair") {
                entry.term(40, alias);
            }
            result.entries.push(entry);
        }
        for entry in &mut result.entries {
            entry
                .terms
                .sort_unstable_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)));
            entry.terms.dedup_by(|a, b| a.1 == b.1);
        }
        Ok(result)
    }
    pub fn find(&self, query: &str) -> Vec<usize> {
        let normalized = query.trim().to_lowercase();
        let tokens: Vec<_> = normalized.split_whitespace().collect();
        if tokens.is_empty() {
            return Vec::new();
        }
        let mut matches: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let score = tokens
                    .iter()
                    .map(|token| {
                        entry
                            .terms
                            .iter()
                            .filter_map(|(weight, term)| {
                                score(term, token, tokens.len() == 1).map(|score| score + weight)
                            })
                            .max()
                    })
                    .collect::<Option<Vec<_>>>()?
                    .iter()
                    .sum::<i32>();
                Some((score, index))
            })
            .collect();
        matches.sort_unstable_by_key(|&(score, index)| (-score, index));
        matches.into_iter().map(|(_, index)| index).collect()
    }
}
impl Entry {
    fn add(&mut self, field: &Field, tab: usize, cfg: &Config, doc: &DocumentMut) {
        self.term(if field.section == "keys" { 120 } else { 100 }, &field.key);
        self.term(100, &field.label);
        self.term(30, &field.hint);
        let page_weight = if matches!(field.kind, Kind::Preview | Kind::Add | Kind::OpenConfig) {
            10
        } else {
            25
        };
        self.term(page_weight, TABS[tab].0);
        self.term(page_weight, TABS[tab].1);
        for alias in group_aliases(&field.section) {
            self.term(40, alias);
        }
        if !matches!(field.kind, Kind::Mapping | Kind::Add) {
            for alias in aliases(tab, &field.section, &field.key) {
                self.term(75, alias);
            }
        }
        if let Some(item) = doc
            .get(&field.section)
            .and_then(|table| table.get(&field.key))
        {
            if let Some(value) = item.as_str() {
                self.term(85, value);
                self.region(85, value, cfg);
                if matches!(field.kind, Kind::Mapping) && field.section != "presets" {
                    let action = cfg.action(value);
                    self.term(
                        85,
                        schema::option(cfg, schema::ChoiceContext::Action, &action),
                    );
                    for alias in action_aliases(&action) {
                        self.term(85, alias);
                    }
                }
            } else if let Some(value) = item.as_integer() {
                self.term(85, value.to_string());
            } else if let Some(value) = item.as_bool() {
                self.term(85, value.to_string());
            } else if let Some(array) = item.as_array() {
                for value in array.iter().filter_map(|item| item.as_str()) {
                    self.term(85, value);
                }
            }
        }
        match &field.kind {
            Kind::Choice(options) => {
                let current = doc
                    .get(&field.section)
                    .and_then(|table| table.get(&field.key))
                    .and_then(|item| {
                        item.as_str()
                            .map(str::to_owned)
                            .or_else(|| item.as_integer().map(|value| value.to_string()))
                    });
                // Search only the selected choice and its translated label.
                if let Some((_, label)) = options.iter().find(|(value, _)| {
                    current
                        .as_ref()
                        .is_some_and(|current| value.eq_ignore_ascii_case(current))
                }) {
                    self.term(85, label);
                }
            }
            Kind::Font => {
                if let Some(current) = doc
                    .get(&field.section)
                    .and_then(|table| table.get(&field.key))
                    .and_then(|item| item.as_str())
                {
                    self.term(
                        85,
                        schema::option(cfg, schema::ChoiceContext::Font, current),
                    );
                }
            }
            Kind::Mapping if field.section == "presets" => self.region(110, &field.key, cfg),
            Kind::Color(schema::ColorChannels::Rgba) => {
                for alias in aliases(tab, &field.section, "opacity") {
                    self.term(75, alias);
                }
                if let Some(alpha) = doc
                    .get(&field.section)
                    .and_then(|table| table.get("opacity"))
                    .and_then(|item| item.as_integer())
                {
                    self.term(85, alpha.to_string());
                }
            }
            Kind::Preview => {
                for alias in ["实时预览", "预览效果", "示例", "样例", "preview"] {
                    self.term(75, alias);
                }
            }
            Kind::OpenConfig => {
                for alias in ["编辑配置", "打开配置", "TOML", "配置文件", "config file"]
                {
                    self.term(75, alias);
                }
            }
            Kind::Add => {
                for alias in ["新增", "新建", "添加", "add"] {
                    self.term(75, alias);
                }
            }
            _ => {}
        }
    }
}
fn group_aliases(section: &str) -> &'static [&'static str] {
    match section {
        "general" => &["常规", "基础设置", "preferences"],
        "topmost_marker" => &["置顶角标", "置顶标志", "角标", "pin marker", "topmost"],
        "tip" => &["提示气泡", "信息浮窗", "悬浮提示", "提示窗口", "tooltip"],
        "regions" | "presets" => &[
            "分屏",
            "区域",
            "布局",
            "平铺",
            "排列",
            "partition",
            "layout",
        ],
        "outline" => &["区域边框", "轮廓", "边线", "描边", "border", "outline"],
        "crosshair" | "horizontal" | "vertical" => {
            &["准星", "瞄准线", "瞄准点", "reticle", "crosshair"]
        }
        "shortcuts" | "keys" => &[
            "快捷键",
            "热键",
            "按键绑定",
            "键盘绑定",
            "hotkey",
            "keybinding",
        ],
        "commands" => &["指令", "命令", "快捷指令", "command"],
        "selection" => &["选择", "累计", "鼠标", "selection", "button"],
        "mousecast" => &["鼠标显示", "鼠标指示", "鼠标点击显示", "mouse annotations"],
        "keycast" => &["按键显示", "键盘显示", "输入显示", "keystrokes"],
        _ => &[],
    }
}
fn aliases(_: usize, section: &str, key: &str) -> &'static [&'static str] {
    match (section, key) {
        ("general", "language") => &["语言", "语种", "界面语言", "locale", "language"],
        ("general", "autostart") => &[
            "开机启动",
            "开机自启",
            "自动运行",
            "登录启动",
            "登陆启动",
            "startup",
            "autostart",
        ],
        ("general", "elevate") => &["提权", "管理员", "管理权限", "admin", "UAC", "elevation"],
        ("general", "alt_hold_ms") => {
            &["长按时间", "触发延迟", "等待时间", "按住时间", "hold delay"]
        }
        ("general", "preview_mode") => &["分区预览", "区域预览", "操作预览", "框线预览"],
        ("general", "target_marker") => {
            &["目标窗口", "待操作窗口", "目标边框", "选择标记", "目标高亮"]
        }
        ("shortcuts", "alt") => &["快捷键Alt", "左右Alt", "激活键", "keyboard activation"],
        ("shortcuts", "mouse_enabled") => &["启用鼠标", "鼠标总开关", "鼠标操作", "mouse actions"],
        ("shortcuts", "move_enabled") => &["拖动", "拖拽", "移动窗口", "窗口移动", "move"],
        ("shortcuts", "move_button") => &["拖动按键", "拖拽按键", "移动鼠标键"],
        ("shortcuts", "resize_enabled") => &["缩放窗口", "调整尺寸", "放大缩小", "resize"],
        ("shortcuts", "resize_button") => &["缩放按键", "调整大小按键", "缩放鼠标键"],
        ("shortcuts", "center_enabled") => &["居中", "屏幕中央", "对齐中间", "center"],
        ("shortcuts", "center_button") => &["居中按键", "居中鼠标键", "中键"],
        ("selection", "add_button") => &["累计选择", "多选", "选择按键"],
        ("selection", "apply_button") => &["立即应用", "直接选择", "应用按键"],
        ("tip", "show") => &["提示内容", "默认内容", "内容类型", "显示内容"],
        ("tip", "window_format") => &["窗口信息格式", "窗口标题", "进程名称", "占位符", "格式化"],
        ("tip", "time_format") => &[
            "时钟格式",
            "日期格式",
            "时间格式",
            "年月日",
            "小时分钟",
            "clock",
            "date",
        ],
        (_, "enabled") => &["开关", "启用", "开启", "启动显示", "enable", "toggle"],
        (_, "size") => &["大小", "尺寸", "边长", "像素", "size"],
        (_, "position") => &["显示位置", "角落", "方位", "摆放", "corner", "position"],
        (_, "duration_ms") => &[
            "持续时间",
            "停留时间",
            "显示时长",
            "超时",
            "消失时间",
            "duration",
            "timeout",
        ],
        (_, "theme") => &[
            "亮暗",
            "明暗",
            "配色模式",
            "主题",
            "light mode",
            "dark mode",
        ],
        (_, "font") => &[
            "字型",
            "字体",
            "字体名称",
            "字体家族",
            "font face",
            "typeface",
        ],
        (_, "font_size") => &["字号", "文字大小", "字体大小", "磅", "pt", "text size"],
        (_, "font_fallbacks") => &[
            "备用字体",
            "后备字体",
            "缺字",
            "替代字体",
            "字体回退",
            "fallback",
        ],
        (_, "font_bold") => &["加粗", "粗体", "粗字", "字重", "bold", "weight"],
        (_, "font_italic") => &["倾斜", "斜体", "斜字", "italic"],
        (_, "background") => &["底色", "背景色", "底板", "填充色", "background"],
        (_, "foreground") => &[
            "字色",
            "文本颜色",
            "文字颜色",
            "前景色",
            "foreground",
            "text color",
        ],
        (_, "dark_background") => &["夜间背景", "暗色背景", "深色底色"],
        (_, "dark_foreground") => &["夜间文字", "暗色文字", "深色字色"],
        (_, "color") => &[
            "线色",
            "线条颜色",
            "颜色",
            "线条配色",
            "line color",
            "stroke",
        ],
        (_, "dark_color") => &["暗色线条", "深色线条", "夜间线色", "dark stroke"],
        (_, "opacity") => &[
            "透明度",
            "不透明度",
            "透光",
            "透明",
            "alpha",
            "transparency",
            "opacity",
        ],
        (_, "fill_opacity") => &["填充透明度", "高亮透明度", "选区透明度", "选中透明度"],
        (_, "width") => &["线宽", "粗细", "厚度", "宽度", "thickness", "stroke width"],
        (_, "gap") => &[
            "间隔", "间断", "空隙", "空白", "缺口", "间距", "断开", "gap", "spacing", "break",
        ],
        (_, "fps") => &[
            "帧率",
            "刷新率",
            "流畅度",
            "每秒帧数",
            "刷新频率",
            "fps",
            "frame rate",
        ],
        (_, "mode") => &["样式模式", "统一", "独立", "自定义", "mode"],
        (_, "radius") => &["半径", "大小", "尺寸", "圆形大小", "指示大小", "radius"],
        (_, "swap_side_buttons") => &[
            "侧键",
            "前进后退",
            "上下侧键",
            "交换侧键",
            "对调",
            "X1 X2",
            "swap",
        ],
        (_, "history") => &[
            "显示数量",
            "记录数量",
            "按键条数",
            "历史",
            "条数",
            "history",
            "count",
        ],
        _ => &[],
    }
}
fn action_aliases(action: &str) -> &'static [&'static str] {
    match action {
        "top" => &["置顶", "固定窗口", "钉住", "最上层", "always on top", "pin"],
        "ignore" => &["忽略", "排除", "禁止操作", "exclude"],
        "guide" | "cross" => &["十字线开关", "显示准星", "准星开关"],
        "horizontal" => &["横线开关", "水平线", "横向准星"],
        "vertical" => &["竖线开关", "垂直线", "纵向准星"],
        "mousecast" => &["鼠标标注", "鼠标显示", "鼠标指示"],
        "keycast" => &["键盘标注", "按键显示", "输入显示"],
        "fullscreen" => &["最大化", "全屏", "铺满", "扩大窗口", "maximize"],
        "centerwindow" => &["居中", "中间", "中央", "center"],
        "command" => &["命令输入", "指令", "命令行"],
        "help" => &["帮助", "说明", "快捷键说明"],
        "version" => &["版本", "版本号", "about"],
        "restart" => &["重启", "重新运行", "重新启动", "relaunch"],
        "quit" => &["退出", "关闭程序", "结束", "exit"],
        "time" => &["时间", "时钟", "clock"],
        "monitor" => &["屏幕", "显示器", "屏幕编号"],
        "borders" => &["分区框线", "标识框线", "框线"],
        "reload" => &["重新加载", "配置重载", "读取配置"],
        "settings" => &["设置", "配置", "preferences"],
        _ => &[],
    }
}
fn score(term: &str, token: &str, single: bool) -> Option<i32> {
    if term == token {
        return Some(300);
    }
    if single && token.len() == 1 && token.as_bytes()[0].is_ascii_alphabetic() {
        return None;
    }
    if term.starts_with(token) {
        return Some(200);
    }
    if term.contains(token) {
        return Some(150);
    }
    if token.len() >= 3 && token.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        for word in term.split(|ch: char| !ch.is_alphanumeric()) {
            if word.len() < token.len() || word.len() > token.len() * 2 {
                continue;
            }
            let mut chars = word.chars();
            if token
                .chars()
                .all(|wanted| chars.by_ref().any(|next| next == wanted))
            {
                return Some(50);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn synonyms_cover_every_fixed_setting_and_resolve_to_related_options() {
        let doc = config::help().parse::<DocumentMut>().unwrap();
        let cfg = Config::parse(&doc).unwrap();
        for tab in 0..TABS.len() {
            for field in schema::fields(tab, 0, &cfg, &doc) {
                if matches!(
                    field.kind,
                    Kind::Mapping | Kind::Add | Kind::Note | Kind::Preview | Kind::OpenConfig
                ) {
                    continue;
                }
                assert!(
                    !aliases(tab, &field.section, &field.key).is_empty(),
                    "{}.{}",
                    field.section,
                    field.key
                );
            }
        }
        let index = Index::build(&doc).unwrap();
        for (query, tab, section, key) in [
            ("间隔", 5, "horizontal", "gap"),
            ("间断", 5, "horizontal", "gap"),
            ("厚度", 3, "outline", "width"),
            ("提权", 0, "general", "elevate"),
            ("开机自启", 0, "general", "autostart"),
            ("后备字体", 1, "tip", "font_fallbacks"),
            ("倾斜", 1, "tip", "font_italic"),
            ("配色模式", 1, "tip", "theme"),
            ("透光", 5, "horizontal", "color"),
            ("前进后退", 7, "mousecast", "swap_side_buttons"),
            ("条数", 8, "keycast", "history"),
            ("时钟格式", 1, "tip", "time_format"),
            ("线色", 3, "outline", "color"),
            ("最上层", 4, "keys", "P"),
            ("扩大窗口", 4, "keys", "F"),
        ] {
            assert!(matches(&index, query, tab, section, key), "{query}");
        }
        assert!(
            index
                .entries
                .iter()
                .all(|entry| !entry.label.contains("间断"))
        );
        assert!(
            matches!(&index.entries[index.find("间断")[0]].target, Target::Field { section, key, .. } if section == "horizontal" && key == "gap")
        );
    }
    fn matches(index: &Index, query: &str, tab: usize, section: &str, key: &str) -> bool {
        index.find(query).iter().any(|&i| matches!(&index.entries[i].target, Target::Field { tab: t, section: s, key: k, .. } if *t == tab && s == section && k == key))
    }
    #[test]
    fn search_matches_labels_hints_current_values_bindings_and_hidden_aliases() {
        let doc = config::help().parse::<DocumentMut>().unwrap();
        let index = Index::build(&doc).unwrap();
        for (query, tab, section, key) in [
            ("准星", 5, "horizontal", "width"),
            ("TIME_FORMAT", 1, "tip", "time_format"),
            ("{display_name}", 1, "tip", "window_format"),
            ("左上", 0, "topmost_marker", "position"),
            ("20", 0, "topmost_marker", "size"),
            ("crshair", 5, "horizontal", "color"),
            ("系统默认 UI 字体", 1, "tip", "font"),
        ] {
            assert!(matches(&index, query, tab, section, key), "{query}");
        }
        assert!(matches(&index, "Q", 2, "presets", "1/9"));
        assert!(
            matches!(&index.entries[index.find("准星")[0]].target, Target::Field { section, key, .. } if section == "crosshair" && key == "fps")
        );
        assert!(matches(&index, "q", 4, "keys", "Q"));
        assert!(
            matches!(&index.entries[index.find("q")[0]].target, Target::Field { section, key, .. } if section == "keys" && key == "Q")
        );
        assert!(index.find("   ").is_empty());
        assert!(index.find("不存在的设置关键词").is_empty());
    }
    #[test]
    fn unselected_candidates_do_not_match_and_value_changes_update_the_index() {
        let mut doc = config::help().parse::<DocumentMut>().unwrap();
        let index = Index::build(&doc).unwrap();
        assert!(matches(&index, "right", 6, "selection", "apply_button"));
        assert!(!matches(&index, "x2", 6, "selection", "apply_button"));
        assert!(!matches(
            &index,
            "bottom-right",
            0,
            "topmost_marker",
            "position"
        ));
        assert!(!matches(&index, "A Test Font", 1, "tip", "font"));
        assert!(!matches(&index, "退出", 4, "keys", "Q"));

        doc["selection"]["apply_button"] = toml_edit::value("x2");
        doc["topmost_marker"]["position"] = toml_edit::value("bottom-right");
        doc["topmost_marker"]["size"] = toml_edit::value(32);
        doc["tip"]["font"] = toml_edit::value("A Test Font");
        doc["tip"]["font_fallbacks"] = toml_edit::value(toml_edit::Array::from_iter([
            "Another Font",
            "Fallback Font",
        ]));
        doc["keys"]["Q"] = toml_edit::value("quit");
        doc["keys"]["T"] = toml_edit::value("1/9");
        let index = Index::build(&doc).unwrap();
        assert!(matches(&index, "x2", 6, "selection", "apply_button"));
        assert!(!matches(&index, "right", 6, "selection", "apply_button"));
        assert!(matches(&index, "右下", 0, "topmost_marker", "position"));
        assert!(!matches(
            &index,
            "top-left",
            0,
            "topmost_marker",
            "position"
        ));
        assert!(matches(&index, "32", 0, "topmost_marker", "size"));
        assert!(matches(&index, "A Test Font", 1, "tip", "font"));
        assert!(matches(&index, "Fallback Font", 1, "tip", "font_fallbacks"));
        assert!(matches(&index, "退出", 4, "keys", "Q"));
    }
    #[test]
    fn index_tracks_custom_regions_bindings_and_both_crosshair_axes() {
        let mut doc = config::help().parse::<DocumentMut>().unwrap();
        doc["presets"]["My Region"] = toml_edit::value("0|0|30sw|30sh");
        doc["keys"]["T"] = toml_edit::value("My Region");
        doc["crosshair"]["mode"] = toml_edit::value("custom");
        let index = Index::build(&doc).unwrap();
        assert!(matches(&index, "T", 2, "presets", "My Region"));
        assert!(matches(&index, "准星", 5, "vertical", "color"));
        assert_eq!(index.entries.iter().filter(|entry| matches!(&entry.target, Target::Field { section, key, .. } if section == "crosshair" && key == "fps")).count(), 1);
        assert!(
            index
                .find("自定义样式")
                .iter()
                .any(|&i| index.entries[i].target == Target::Axis(3))
        );
        assert!(matches(&index, "提示 字型", 1, "tip", "font"));
    }
}
