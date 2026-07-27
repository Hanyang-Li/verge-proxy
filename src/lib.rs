use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use dialoguer::{theme::ColorfulTheme, Select};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;
use serde_json::json;
use std::cmp::min;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{IsTerminal, Write};
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const DEFAULT_DELAY_TEST_URL: &str = "https://www.gstatic.com/generate_204";
const DEFAULT_DELAY_TIMEOUT_MS: u64 = 2_000;
const DEFAULT_AUTO_NODE_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_CONCURRENCY: usize = 20;
const BLOCK_BEGIN: &str = "# >>> verge-proxy >>>";
const BLOCK_END: &str = "# <<< verge-proxy <<<";
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD_GREEN: &str = "\x1b[1;38;2;166;227;161m";
const ANSI_BOLD_RED: &str = "\x1b[1;38;2;243;139;168m";
const ANSI_SPINNER: &str = "\x1b[94m"; // 亮蓝
const ANSI_BAR_FILLED: &str = "\x1b[36m"; // 青色，已完成
const ANSI_BAR_EMPTY: &str = "\x1b[34m"; // 蓝色，未完成
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner_frame(i: usize) -> &'static str {
    SPINNER_FRAMES[i % SPINNER_FRAMES.len()]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Rule,
    Global,
    Direct,
    Unknown(String),
}

impl Mode {
    pub fn parse(value: &str) -> Self {
        match value {
            "rule" => Self::Rule,
            "global" => Self::Global,
            "direct" => Self::Direct,
            other => Self::Unknown(other.to_string()),
        }
    }

    pub fn api_value(&self) -> &str {
        match self {
            Self::Rule => "rule",
            Self::Global => "global",
            Self::Direct => "direct",
            Self::Unknown(value) => value.as_str(),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Rule => "规则",
            Self::Global => "全局",
            Self::Direct => "直连",
            Self::Unknown(value) => value.as_str(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    pub name: String,
    pub proxy_type: String,
    pub now: Option<String>,
    pub all: Vec<String>,
}

/// 延时段的显示状态：Hidden 不渲染该段，Timeout 渲染 timeout。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delay {
    Hidden,
    Value(u64),
    Timeout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusInfo {
    pub mode: Mode,
    pub group: String,
    pub node: String,
    pub delay: Delay,
    pub port: u16,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PromptConfig {
    pub mode_icon: Option<String>,
    pub group_icon: Option<String>,
    pub node_icon: Option<String>,
    pub delay_icon: Option<String>,
    pub port_icon: Option<String>,
}

/// 当前 mode/group 等于默认值时隐藏对应 tag。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TagDefaults {
    pub mode: Option<String>,
    pub group: Option<String>,
}

pub fn port_from_configs_json(configs: &serde_json::Value) -> Option<u16> {
    configs
        .get("mixed-port")
        .and_then(|value| value.as_u64())
        .and_then(|value| u16::try_from(value).ok())
}

pub fn port_from_yaml_str(input: &str) -> Option<u16> {
    let yaml: serde_yaml::Value = serde_yaml::from_str(input).ok()?;
    yaml.get("mixed-port")
        .and_then(|value| value.as_u64())
        .and_then(|value| u16::try_from(value).ok())
}

pub fn choose_default_group(proxies: &HashMap<String, Proxy>) -> Option<String> {
    if proxies.contains_key("🔰 手动选择") {
        return Some("🔰 手动选择".to_string());
    }
    proxies
        .keys()
        .find(|name| name.contains("手动选择"))
        .cloned()
        .or_else(|| {
            proxies.get("GLOBAL").and_then(|global| {
                global
                    .all
                    .iter()
                    .find(|name| {
                        proxies
                            .get(*name)
                            .is_some_and(|proxy| !proxy.all.is_empty())
                    })
                    .cloned()
            })
        })
}

pub fn selector_groups(proxies: &HashMap<String, Proxy>) -> Vec<String> {
    let mut groups: Vec<_> = proxies
        .values()
        .filter(|proxy| !proxy.all.is_empty())
        .map(|proxy| proxy.name.clone())
        .collect();
    groups.sort();
    groups
}

pub fn resolve_active_group_and_node(
    mode: &Mode,
    proxies: &HashMap<String, Proxy>,
) -> (String, String) {
    if *mode == Mode::Direct {
        return ("DIRECT".to_string(), "DIRECT".to_string());
    }

    let global_now = proxies.get("GLOBAL").and_then(|proxy| proxy.now.as_deref());
    if let Some(now) = global_now {
        if proxies.get(now).is_none_or(|proxy| proxy.all.is_empty()) {
            return ("GLOBAL".to_string(), now.to_string());
        }
    }
    let starting_group = global_now
        .filter(|name| {
            proxies
                .get(*name)
                .is_some_and(|proxy| !proxy.all.is_empty())
        })
        .map(str::to_string)
        .or_else(|| choose_default_group(proxies))
        .unwrap_or_else(|| "GLOBAL".to_string());

    let mut visited = HashSet::new();
    let node = resolve_node_from(&starting_group, proxies, &mut visited)
        .or_else(|| global_now.map(str::to_string))
        .unwrap_or_else(|| "未知".to_string());

    let group = if proxies
        .get(global_now.unwrap_or(""))
        .is_some_and(|proxy| !proxy.all.is_empty())
    {
        global_now.unwrap().to_string()
    } else {
        starting_group
    };

    (group, node)
}

fn resolve_node_from(
    name: &str,
    proxies: &HashMap<String, Proxy>,
    visited: &mut HashSet<String>,
) -> Option<String> {
    if !visited.insert(name.to_string()) {
        return None;
    }

    let proxy = proxies.get(name)?;
    let now = proxy.now.as_deref()?;
    if proxies.get(now).is_some_and(|next| !next.all.is_empty()) {
        return resolve_node_from(now, proxies, visited);
    }
    Some(now.to_string())
}

pub fn leaf_nodes_for_group(group: &str, proxies: &HashMap<String, Proxy>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut nodes = Vec::new();
    collect_leaf_nodes(group, proxies, &mut seen, &mut nodes);
    nodes
        .into_iter()
        .filter(|name| name != "REJECT")
        .collect::<Vec<_>>()
}

fn collect_leaf_nodes(
    name: &str,
    proxies: &HashMap<String, Proxy>,
    seen: &mut HashSet<String>,
    nodes: &mut Vec<String>,
) {
    if !seen.insert(name.to_string()) {
        return;
    }
    let Some(proxy) = proxies.get(name) else {
        nodes.push(name.to_string());
        return;
    };
    if proxy.all.is_empty() {
        nodes.push(name.to_string());
        return;
    }
    for child in &proxy.all {
        if proxies
            .get(child)
            .is_some_and(|proxy| !proxy.all.is_empty())
        {
            collect_leaf_nodes(child, proxies, seen, nodes);
        } else {
            nodes.push(child.clone());
        }
    }
}

pub fn parse_filter_ranges(filter: Option<&str>, configured: &[String]) -> Vec<String> {
    if let Some(filter) = filter {
        return filter
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect();
    }
    configured
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn candidate_batches(nodes: &[String], ranges: &[String]) -> Vec<(String, Vec<String>)> {
    if ranges.is_empty() {
        return vec![("全部节点".to_string(), nodes.to_vec())];
    }

    let mut batches = Vec::new();
    let mut matched = HashSet::new();
    for range in ranges {
        let batch: Vec<String> = nodes
            .iter()
            .filter(|node| node.contains(range))
            .cloned()
            .collect();
        for node in &batch {
            matched.insert(node.clone());
        }
        batches.push((range.clone(), batch));
    }

    let fallback: Vec<String> = nodes
        .iter()
        .filter(|node| !matched.contains(*node))
        .cloned()
        .collect();
    if !fallback.is_empty() {
        batches.push(("其他节点".to_string(), fallback));
    }
    batches
}

pub fn fuzzy_filter_nodes(nodes: &[String], keyword: &str) -> Vec<String> {
    let keyword = keyword.trim().to_lowercase();
    if keyword.is_empty() {
        return nodes.to_vec();
    }
    nodes
        .iter()
        .filter(|node| node.to_lowercase().contains(&keyword))
        .cloned()
        .collect()
}

pub fn parse_proxies(value: serde_json::Value) -> anyhow::Result<HashMap<String, Proxy>> {
    let root = value
        .get("proxies")
        .and_then(|value| value.as_object())
        .ok_or_else(|| anyhow::anyhow!("controller response missing proxies object"))?;
    let mut proxies = HashMap::new();
    for (name, value) in root {
        let proxy_type = value
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let now = value
            .get("now")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let all = value
            .get("all")
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        proxies.insert(
            name.clone(),
            Proxy {
                name: name.clone(),
                proxy_type,
                now,
                all,
            },
        );
    }
    Ok(proxies)
}

pub fn format_status(info: &StatusInfo) -> String {
    let mut lines = vec![
        format!("mode: {}", info.mode.label()),
        format!("group: {}", info.group),
        format!("node: {}", info.node),
    ];
    match info.delay {
        Delay::Hidden => {}
        Delay::Value(delay) => lines.push(format!("delay: {delay}ms")),
        Delay::Timeout => lines.push("delay: timeout".to_string()),
    }
    lines.push(format!("port: {}", info.port));
    lines.join("\n")
}

pub fn format_status_prompt(
    info: &StatusInfo,
    prompt: &PromptConfig,
    defaults: &TagDefaults,
    terminal_width: usize,
    initial_width: usize,
) -> String {
    let hide_mode = defaults
        .mode
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| value == info.mode.api_value());
    let hide_group = defaults
        .group
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| value == info.group);

    let mut segments = Vec::with_capacity(5);
    if !hide_mode {
        segments.push(PromptSegment::new(
            prompt.mode_icon.as_deref().unwrap_or("󰒓"),
            info.mode.label().to_string(),
            "#fab387",
        ));
    }
    if !hide_group {
        segments.push(PromptSegment::new(
            prompt.group_icon.as_deref().unwrap_or("󰓹"),
            info.group.clone(),
            "#f9e2af",
        ));
    }
    segments.push(PromptSegment::new(
        prompt.node_icon.as_deref().unwrap_or("󰍍"),
        info.node.clone(),
        "#a6e3a1",
    ));
    let delay = match info.delay {
        Delay::Hidden => None,
        Delay::Value(delay) => Some(format!("{delay}ms")),
        Delay::Timeout => Some("timeout".to_string()),
    };
    if let Some(delay) = delay {
        segments.push(PromptSegment::new(
            prompt.delay_icon.as_deref().unwrap_or("󱎫"),
            delay,
            "#74c7ec",
        ));
    }
    segments.push(PromptSegment::new(
        prompt.port_icon.as_deref().unwrap_or("󰤨"),
        info.port.to_string(),
        "#b4befe",
    ));

    // 布局：按宽度把 segments 分行，行首 segment 占完整双胶囊宽度，其余少一个左胶囊
    let terminal_width = terminal_width.max(20);
    let mut lines: Vec<Vec<PromptSegment>> = Vec::new();
    let mut current: Vec<PromptSegment> = Vec::new();
    let mut current_width = initial_width;
    for segment in segments {
        let segment = segment.fit_to_width(terminal_width);
        let width = if current.is_empty() {
            segment.width
        } else {
            segment.width.saturating_sub(1)
        };
        if current_width > 0 && current_width + width >= terminal_width {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        let width = if current.is_empty() {
            segment.width
        } else {
            segment.width.saturating_sub(1)
        };
        current_width += width;
        current.push(segment);
    }
    if !current.is_empty() {
        lines.push(current);
    }

    lines
        .iter()
        .map(|line| render_prompt_line(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 渲染一行 tags：行首显示左胶囊，相邻 segment 的右胶囊画在下一段背景上平滑衔接。
fn render_prompt_line(segments: &[PromptSegment]) -> String {
    let mut output = String::new();
    for (index, segment) in segments.iter().enumerate() {
        if index == 0 {
            output.push_str(&ansi_fg(segment.color));
            output.push('\u{e0b6}');
        }
        output.push_str(&ansi_bg_fg(segment.color, "#11111b"));
        output.push(' ');
        output.push_str(&segment.icon);
        output.push(' ');
        output.push_str(&segment.value);
        output.push(' ');
        if let Some(next) = segments.get(index + 1) {
            output.push_str(&ansi_bg_fg(next.color, segment.color));
            output.push('\u{e0b4}');
        } else {
            output.push_str(ANSI_RESET);
            output.push_str(&ansi_fg(segment.color));
            output.push('\u{e0b4}');
            output.push_str(ANSI_RESET);
        }
    }
    output
}

#[derive(Clone)]
struct PromptSegment {
    icon: String,
    value: String,
    color: &'static str,
    width: usize,
}

impl PromptSegment {
    fn new(icon: &str, value: String, color: &'static str) -> Self {
        let icon = normalize_prompt_text(icon);
        let value = normalize_prompt_text(&value);
        let plain = format!(" {icon} {value} ");
        Self {
            icon,
            value,
            color,
            width: prompt_display_width(&plain),
        }
    }

    fn fit_to_width(&self, max_width: usize) -> Self {
        if self.width <= max_width {
            return self.clone();
        }

        let empty_segment_width = prompt_display_width("   ");
        let icon_budget = max_width.saturating_sub(empty_segment_width);
        let icon = truncate_to_width(&self.icon, icon_budget);
        let fixed_width = prompt_display_width(&format!(" {icon}  "));
        let value_budget = max_width.saturating_sub(fixed_width);
        Self::new(
            &icon,
            truncate_to_width(&self.value, value_budget),
            self.color,
        )
    }
}

fn normalize_prompt_text(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn prompt_display_width(input: &str) -> usize {
    input.chars().map(prompt_char_width).sum()
}

fn prompt_char_width(ch: char) -> usize {
    if is_private_use(ch) {
        return 2;
    }
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

fn is_private_use(ch: char) -> bool {
    matches!(
        ch as u32,
        0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD
    )
}

fn truncate_to_width(input: &str, max_width: usize) -> String {
    if prompt_display_width(input) <= max_width {
        return input.to_string();
    }
    if max_width == 0 {
        return String::new();
    }

    let marker = "…";
    let marker_width = display_width(marker);
    if max_width <= marker_width {
        return marker.to_string();
    }

    let content_width = max_width - marker_width;
    let mut output = String::new();
    let mut width = 0;
    for ch in input.chars() {
        let ch_width = prompt_char_width(ch);
        if width + ch_width > content_width {
            break;
        }
        output.push(ch);
        width += ch_width;
    }
    output.push_str(marker);
    output
}

pub fn success_line(
    message: &str,
    status: Option<&StatusInfo>,
    prompt: &PromptConfig,
    defaults: &TagDefaults,
) -> String {
    let mut output = format!("{}✔{} {}", ANSI_BOLD_GREEN, ANSI_RESET, message);
    if let Some(status) = status {
        output.push(' ');
        output.push_str(&format_status_prompt(
            status,
            prompt,
            defaults,
            terminal_width(),
            display_width(message) + 3,
        ));
    }
    output
}

pub fn error_line(
    message: &str,
    status: Option<&StatusInfo>,
    prompt: &PromptConfig,
    defaults: &TagDefaults,
) -> String {
    let mut output = format!("{}✘{} {}", ANSI_BOLD_RED, ANSI_RESET, message);
    if let Some(status) = status {
        output.push(' ');
        output.push_str(&format_status_prompt(
            status,
            prompt,
            defaults,
            terminal_width(),
            display_width(message) + 3,
        ));
    }
    output
}

#[derive(Parser)]
#[command(name = "verge-proxy", version, about = "Clash Verge proxy CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 开启命令行代理：读取端口并导出 http/https/all_proxy 环境变量
    Start,
    /// 关闭命令行代理：移除代理相关环境变量
    Stop {
        /// 跳过 proxy_name 检查强制执行
        #[arg(long, short)]
        force: bool,
    },
    /// 重启命令行代理：重新读取端口并刷新环境变量
    Restart {
        /// 跳过 proxy_name 检查强制执行
        #[arg(long, short)]
        force: bool,
    },
    /// 查看当前状态：模式 / 代理组 / 节点 / 延迟 / 端口
    Status,
    /// 交互切换代理模式：规则 / 全局 / 直连
    Mode,
    /// 交互切换代理组
    Group,
    /// 交互切换当前代理组下的节点
    Node {
        /// 按关键字预筛节点（逗号分隔，任一命中即保留）
        #[arg(long)]
        filter: Option<String>,
    },
    /// 输出当前 mixed-port 端口号
    Port,
    /// 自动测速并切换到当前代理组中延迟最低的节点
    AutoNode {
        /// 限制测速范围（逗号分隔，按顺序分批测试；测速时按 Esc 可跳过当前批次）
        #[arg(long)]
        filter: Option<String>,
    },
    /// 安装 / 更新 shell 集成、命令补全与配置文件
    Install,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AppConfig {
    filter: Option<String>,
    concurrency: Option<usize>,
    timeout_ms: Option<u64>,
    mode: Option<String>,
    group: Option<String>,
    #[serde(default)]
    prompt: PromptConfig,
}

impl AppConfig {
    fn tag_defaults(&self) -> TagDefaults {
        TagDefaults {
            mode: self.mode.clone(),
            group: self.group.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct Paths {
    clash_config: PathBuf,
    clash_runtime_config: PathBuf,
    app_config_dir: PathBuf,
    app_config: PathBuf,
    data_dir: PathBuf,
    zsh_functions_dir: PathBuf,
    zshrc: PathBuf,
}

#[derive(Debug, Clone)]
struct Controller {
    socket: Option<PathBuf>,
    base: String,
    secret: Option<String>,
}

#[derive(Debug, Clone)]
struct DelayResult {
    node: String,
    delay: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallAction {
    Set,
    Updated,
}

impl InstallAction {
    fn label(self) -> &'static str {
        match self {
            Self::Set => "已设置",
            Self::Updated => "已更新",
        }
    }
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::new()?;
    let app_config = read_app_config(&paths).unwrap_or_default();

    // start/stop/restart 的 stdout 会被 shell wrapper `eval` 执行，错误须以 `echo … / return 1`
    // 形式输出；其余命令直接展示，错误统一按 error_line 风格（红 ✘）打到 stdout。两类都以退出码 1 结束。
    let evaled = matches!(
        cli.command,
        Commands::Start | Commands::Stop { .. } | Commands::Restart { .. }
    );
    let result = match cli.command {
        Commands::Start => cmd_start(&paths, &app_config),
        Commands::Stop { force } => cmd_stop(force),
        Commands::Restart { force } => cmd_restart(&paths, &app_config, force),
        Commands::Status => cmd_status(&paths, &app_config),
        Commands::Mode => cmd_mode(&paths, &app_config),
        Commands::Group => cmd_group(&paths, &app_config),
        Commands::Node { filter } => cmd_node(&paths, &app_config, filter.as_deref()),
        Commands::Port => cmd_port(&paths),
        Commands::AutoNode { filter } => cmd_auto_node(&paths, &app_config, filter.as_deref()),
        Commands::Install => cmd_install(&paths),
    };

    if let Err(error) = result {
        let message = format!("错误：{error:#}");
        if evaled {
            emit_shell_error(&message, &app_config.prompt, &app_config.tag_defaults());
        } else {
            println!(
                "{}",
                error_line(&message, None, &app_config.prompt, &app_config.tag_defaults())
            );
        }
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_status(paths: &Paths, app_config: &AppConfig) -> Result<()> {
    let info = collect_status(paths, true)?;
    println!(
        "{}",
        format_status_prompt(
            &info,
            &app_config.prompt,
            &app_config.tag_defaults(),
            terminal_width(),
            0,
        )
    );
    Ok(())
}

fn cmd_port(paths: &Paths) -> Result<()> {
    println!("{}", read_port(paths)?);
    Ok(())
}

impl Paths {
    fn new() -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("无法定位 HOME"))?;
        let clash_dir =
            home.join("Library/Application Support/io.github.clash-verge-rev.clash-verge-rev");
        let app_config_dir = home.join(".config/verge-proxy");
        Ok(Self {
            clash_config: clash_dir.join("config.yaml"),
            clash_runtime_config: clash_dir.join("clash-verge.yaml"),
            app_config: app_config_dir.join("config.yaml"),
            app_config_dir,
            data_dir: home.join(".local/share/verge-proxy"),
            zsh_functions_dir: home.join(".local/share/zsh/site-functions"),
            zshrc: home.join(".zshrc"),
        })
    }
}

/// 以 shell 可 eval 的形式输出一条错误：`echo '✘ …'` + `return 1`。
/// 供 start/restart/stop 使用——它们的 stdout 会被 shell wrapper `eval` 执行。
fn emit_shell_error(message: &str, prompt: &PromptConfig, defaults: &TagDefaults) {
    println!(
        "echo {}",
        shell_single_quote(&error_line(message, None, prompt, defaults))
    );
    println!("return 1 2>/dev/null || exit 1");
}

/// Emits an error and returns `true` when `proxy_name` is set to a value other
/// than `verge`. An unset or empty `proxy_name` is allowed (returns `false`).
fn proxy_name_conflict(prompt: &PromptConfig, defaults: &TagDefaults) -> bool {
    match env::var("proxy_name") {
        Ok(value) if !value.is_empty() && value != "verge" => {
            emit_shell_error(
                &format!("proxy_name 当前为 {value}，非 verge，拒绝操作"),
                prompt,
                defaults,
            );
            true
        }
        _ => false,
    }
}

fn cmd_start(paths: &Paths, app_config: &AppConfig) -> Result<()> {
    if proxy_name_conflict(&app_config.prompt, &app_config.tag_defaults()) {
        return Ok(());
    }
    let occupied: Vec<_> = ["http_proxy", "https_proxy", "all_proxy"]
        .into_iter()
        .filter(|name| env::var_os(name).is_some())
        .collect();
    if !occupied.is_empty() {
        emit_shell_error(
            "环境变量被占用，请执行 verge-proxy stop 后再次尝试",
            &app_config.prompt,
            &app_config.tag_defaults(),
        );
        return Ok(());
    }
    emit_proxy_exports(paths, app_config, "命令行代理已开启")
}

fn cmd_stop(force: bool) -> Result<()> {
    if !force && proxy_name_conflict(&PromptConfig::default(), &TagDefaults::default()) {
        return Ok(());
    }
    println!("unset http_proxy https_proxy all_proxy no_proxy proxy_name");
    println!(
        "echo {}",
        shell_single_quote(&success_line(
            "命令行代理已关闭，环境变量已移除",
            None,
            &PromptConfig::default(),
            &TagDefaults::default()
        ))
    );
    Ok(())
}

fn cmd_restart(paths: &Paths, app_config: &AppConfig, force: bool) -> Result<()> {
    if !force && proxy_name_conflict(&app_config.prompt, &app_config.tag_defaults()) {
        return Ok(());
    }
    emit_proxy_exports(paths, app_config, "命令行代理已重启")
}

fn emit_proxy_exports(paths: &Paths, app_config: &AppConfig, message: &str) -> Result<()> {
    let port = read_port(paths)?;
    println!("export http_proxy=http://127.0.0.1:{port}");
    println!("export https_proxy=http://127.0.0.1:{port}");
    println!("export all_proxy=socks5://127.0.0.1:{port}");
    println!("export no_proxy=localhost,127.0.0.1");
    println!("export proxy_name=verge");
    let status = collect_status(paths, false).ok();
    println!(
        "echo {}",
        shell_single_quote(&success_line(
            message,
            status.as_ref(),
            &app_config.prompt,
            &app_config.tag_defaults()
        ))
    );
    Ok(())
}

fn cmd_mode(paths: &Paths, app_config: &AppConfig) -> Result<()> {
    let controller = Controller::discover(paths)?;
    let current = controller.mode()?;
    let options = [Mode::Rule, Mode::Global, Mode::Direct];
    let selected = match choose_from_list(
        "切换模式",
        options
            .iter()
            .position(|mode| *mode == current)
            .unwrap_or(0),
        &options
            .iter()
            .map(|mode| mode.label().to_string())
            .collect::<Vec<_>>(),
        10,
    ) {
        Ok(selected) => selected,
        Err(error) => return print_interactive_error(paths, app_config, error),
    };
    controller.set_mode(&options[selected])?;
    Ok(())
}

fn cmd_group(paths: &Paths, app_config: &AppConfig) -> Result<()> {
    let controller = Controller::discover(paths)?;
    let mode = controller.mode()?;
    if mode == Mode::Direct {
        let status = collect_status(paths, false).ok();
        println!(
            "{}",
            success_line(
                "设置直连成功",
                status.as_ref(),
                &app_config.prompt,
                &app_config.tag_defaults()
            )
        );
        return Ok(());
    }
    let proxies = controller.proxies()?;
    let groups = selector_groups(&proxies)
        .into_iter()
        .filter(|name| name != "GLOBAL")
        .filter(|name| name != "🛑 Block")
        .collect::<Vec<_>>();
    if groups.is_empty() {
        return Err(anyhow!("没有可切换的代理组"));
    }
    let (current_group, _) = resolve_active_group_and_node(&mode, &proxies);
    let selected = match choose_from_list(
        "切换代理组",
        groups
            .iter()
            .position(|name| name == &current_group)
            .unwrap_or(0),
        &groups,
        10,
    ) {
        Ok(selected) => selected,
        Err(error) => return print_interactive_error(paths, app_config, error),
    };
    controller.select_proxy("GLOBAL", &groups[selected])?;
    Ok(())
}

fn cmd_node(paths: &Paths, app_config: &AppConfig, filter: Option<&str>) -> Result<()> {
    let controller = Controller::discover(paths)?;
    let mode = controller.mode()?;
    if mode == Mode::Direct {
        let status = collect_status(paths, false).ok();
        println!(
            "{}",
            success_line(
                "设置直连成功",
                status.as_ref(),
                &app_config.prompt,
                &app_config.tag_defaults()
            )
        );
        return Ok(());
    }
    let proxies = controller.proxies()?;
    let (group, current_node) = resolve_active_group_and_node(&mode, &proxies);
    let all_nodes = leaf_nodes_for_group(&group, &proxies);
    let nodes = filter_nodes_by_keyword(&all_nodes, filter);
    if nodes.is_empty() {
        println!(
            "{}",
            error_line(
                "错误：没有匹配节点",
                collect_status(paths, false).ok().as_ref(),
                &app_config.prompt,
                &app_config.tag_defaults()
            )
        );
        return Ok(());
    }
    let selected = match choose_from_list(
        &format!("切换节点: {group}"),
        nodes
            .iter()
            .position(|name| name == &current_node)
            .unwrap_or(0),
        &nodes,
        10,
    ) {
        Ok(selected) => selected,
        Err(error) => return print_interactive_error(paths, app_config, error),
    };
    controller.select_proxy(&group, &nodes[selected])?;
    Ok(())
}

fn cmd_auto_node(paths: &Paths, app_config: &AppConfig, filter: Option<&str>) -> Result<()> {
    let controller = Controller::discover(paths)?;
    let mode = controller.mode()?;
    if mode == Mode::Direct {
        let status = collect_status(paths, false).ok();
        println!(
            "{}",
            success_line(
                "设置直连成功",
                status.as_ref(),
                &app_config.prompt,
                &app_config.tag_defaults()
            )
        );
        return Ok(());
    }

    let proxies = controller.proxies()?;
    let (group, _) = resolve_active_group_and_node(&mode, &proxies);
    let nodes = leaf_nodes_for_group(&group, &proxies)
        .into_iter()
        .filter(|name| name != "DIRECT" && name != "REJECT")
        .collect::<Vec<_>>();
    if nodes.is_empty() {
        return Err(anyhow!("代理组 {group} 没有可测速节点"));
    }

    let configured_ranges: Vec<String> = app_config
        .filter
        .as_deref()
        .map(|value| value.split(',').map(str::to_string).collect())
        .unwrap_or_default();
    let ranges = parse_filter_ranges(filter, &configured_ranges);
    let concurrency = app_config
        .concurrency
        .unwrap_or(DEFAULT_CONCURRENCY)
        .clamp(1, 256);
    // 单节点预算放宽到 5s：冷连接拨号+TLS 握手实测可达数秒
    let timeout_ms = app_config
        .timeout_ms
        .unwrap_or(DEFAULT_AUTO_NODE_TIMEOUT_MS)
        .max(1);

    // 整个测速过程只占一行：每批次共用同一行，Esc 跳过当前批次进入下一批次
    for (label, batch) in candidate_batches(&nodes, &ranges) {
        if batch.is_empty() {
            continue;
        }
        if let BatchOutcome::Best(best) =
            test_batch(&controller, batch, concurrency, timeout_ms, &label)
        {
            controller.select_proxy(&group, &best.node)?;
            // 直接复用测速阶段刚测得的延迟，避免用更短的预算重测导致误报 timeout
            let status = collect_status(paths, false).ok().map(|mut info| {
                info.delay = Delay::Value(best.delay);
                info
            });
            println!(
                "{}",
                success_line(
                    "已自动选择",
                    status.as_ref(),
                    &app_config.prompt,
                    &app_config.tag_defaults()
                )
            );
            return Ok(());
        }
    }

    Err(anyhow!("没有找到可连通节点"))
}

fn cmd_install(paths: &Paths) -> Result<()> {
    fs::create_dir_all(&paths.app_config_dir)
        .with_context(|| format!("无法创建 {}", paths.app_config_dir.display()))?;
    let config_action = ensure_config_file(paths)?;

    let (completion_action, completion_path) = write_completion_file(paths)?;
    let zshrc_action = update_zshrc(paths)?;
    let prompt = PromptConfig::default();
    println!(
        "{}",
        install_line(zshrc_action, "环境配置", &paths.zshrc, &prompt)
    );
    println!(
        "{}",
        install_line(completion_action, "补全配置", &completion_path, &prompt)
    );
    println!(
        "{}",
        install_line(config_action, "自定义配置", &paths.app_config, &prompt)
    );
    Ok(())
}

fn collect_status(paths: &Paths, probe_delay: bool) -> Result<StatusInfo> {
    let port = read_port(paths)?;
    let controller = Controller::discover(paths)?;
    let mode = controller.mode()?;
    let proxies = controller.proxies()?;
    let (group, node) = resolve_active_group_and_node(&mode, &proxies);
    let delay = if probe_delay {
        let delay_target = if mode == Mode::Direct {
            "DIRECT"
        } else {
            &node
        };
        // 冷连接首次探测（拨号+TLS 握手）容易超过 2s 预算，失败后立即重试一次
        controller
            .delay(delay_target, DEFAULT_DELAY_TIMEOUT_MS)
            .or_else(|_| controller.delay(delay_target, DEFAULT_DELAY_TIMEOUT_MS))
            .map(Delay::Value)
            .unwrap_or(Delay::Timeout)
    } else {
        Delay::Hidden
    };
    Ok(StatusInfo {
        mode,
        group,
        node,
        delay,
        port,
    })
}

fn read_port(paths: &Paths) -> Result<u16> {
    if let Ok(controller) = Controller::discover(paths) {
        if let Ok(configs) = controller.configs() {
            if let Some(port) = port_from_configs_json(&configs) {
                return Ok(port);
            }
        }
    }
    for path in [&paths.clash_runtime_config, &paths.clash_config] {
        if let Ok(input) = fs::read_to_string(path) {
            if let Some(port) = port_from_yaml_str(&input) {
                return Ok(port);
            }
        }
    }
    Err(anyhow!(
        "无法从 Clash Verge controller 或配置文件读取 mixed-port"
    ))
}

impl Controller {
    fn discover(paths: &Paths) -> Result<Self> {
        let runtime = read_yaml_file(&paths.clash_runtime_config).unwrap_or_default();
        let config = read_yaml_file(&paths.clash_config).unwrap_or_default();
        let secret = yaml_string(&runtime, "secret").or_else(|| yaml_string(&config, "secret"));
        if let Some(socket) = yaml_string(&runtime, "external-controller-unix")
            .or_else(|| yaml_string(&config, "external-controller-unix"))
        {
            let socket_path = PathBuf::from(socket);
            if socket_path.exists() {
                return Ok(Self {
                    socket: Some(socket_path),
                    base: "http://localhost".to_string(),
                    secret,
                });
            }
        }

        let mut base = yaml_string(&runtime, "external-controller")
            .or_else(|| yaml_string(&config, "external-controller"))
            .ok_or_else(|| anyhow!("找不到 Clash controller"))?;
        if base.is_empty() {
            return Err(anyhow!("找不到 Clash controller"));
        }
        if !base.starts_with("http://") && !base.starts_with("https://") {
            base = format!("http://{base}");
        }
        base = base
            .trim_end_matches('/')
            .replace("http://0.0.0.0:", "http://127.0.0.1:");
        Ok(Self {
            socket: None,
            base,
            secret,
        })
    }

    fn configs(&self) -> Result<serde_json::Value> {
        self.request_json("GET", "/configs", None, Duration::from_secs(5))
    }

    fn mode(&self) -> Result<Mode> {
        let configs = self.configs()?;
        let mode = configs
            .get("mode")
            .and_then(|value| value.as_str())
            .ok_or_else(|| anyhow!("controller /configs 缺少 mode"))?;
        Ok(Mode::parse(mode))
    }

    fn set_mode(&self, mode: &Mode) -> Result<()> {
        self.request_empty(
            "PATCH",
            "/configs",
            Some(json!({ "mode": mode.api_value() })),
            Duration::from_secs(5),
        )?;
        Ok(())
    }

    fn proxies(&self) -> Result<HashMap<String, Proxy>> {
        parse_proxies(self.request_json("GET", "/proxies", None, Duration::from_secs(10))?)
    }

    fn select_proxy(&self, group: &str, node: &str) -> Result<()> {
        self.request_empty(
            "PUT",
            &format!("/proxies/{}", encode(group)),
            Some(json!({ "name": node })),
            Duration::from_secs(10),
        )?;
        Ok(())
    }

    fn delay(&self, node: &str, timeout_ms: u64) -> Result<u64> {
        let path = format!(
            "/proxies/{}/delay?url={}&timeout={}",
            encode(node),
            encode(DEFAULT_DELAY_TEST_URL),
            timeout_ms
        );
        let curl_timeout = Duration::from_millis(timeout_ms + 3_000);
        let value = self.request_json("GET", &path, None, curl_timeout)?;
        value
            .get("delay")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| anyhow!("delay timeout"))
    }

    fn request_json(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        let output = self.request(method, path, body, timeout)?;
        parse_controller_json_response(&output)?.ok_or_else(|| anyhow!("controller 返回空 JSON"))
    }

    fn request_empty(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
        timeout: Duration,
    ) -> Result<()> {
        let output = self.request(method, path, body, timeout)?;
        if output.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Ok(());
        }
        parse_controller_json_response(&output)?;
        Ok(())
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        let mut command = Command::new("/usr/bin/curl");
        command
            .arg("-sS")
            .arg("--fail")
            .arg("--max-time")
            .arg(timeout.as_secs_f64().max(1.0).to_string());
        if let Some(socket) = &self.socket {
            command.arg("--unix-socket").arg(socket);
        }
        if let Some(secret) = &self.secret {
            if !secret.is_empty() {
                command
                    .arg("-H")
                    .arg(format!("Authorization: Bearer {secret}"));
            }
        }
        if method != "GET" {
            command.arg("-X").arg(method);
        }
        if let Some(body) = body {
            command
                .arg("-H")
                .arg("Content-Type: application/json")
                .arg("-d")
                .arg(body.to_string());
        }
        command.arg(format!("{}{}", self.base, path));
        let output = command.output().context("无法执行 curl")?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(anyhow!(
                "controller 请求失败: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

/// 一个批次的测速结果：找到最快节点 / 全部不可用 / 被 Esc 跳过。
enum BatchOutcome {
    Best(DelayResult),
    Empty,
    Cancelled,
}

/// 并发测速一批节点。工作线程后台拨测，spinner 在独立线程渲染，主线程只轮询键盘并
/// 收集结果：Esc 取消当前批次立即返回（不等待在途拨测），Ctrl+C 恢复终端后退出。
fn test_batch(
    controller: &Controller,
    nodes: Vec<String>,
    concurrency: usize,
    timeout_ms: u64,
    label: &str,
) -> BatchOutcome {
    let total = nodes.len() as u64;
    let controller = Arc::new(controller.clone());
    let queue = Arc::new(Mutex::new(nodes.into_iter()));
    let cancel = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicU64::new(0));
    let (tx, rx) = mpsc::channel();
    let worker_count = min(concurrency, total as usize).max(1);
    for _ in 0..worker_count {
        let controller = Arc::clone(&controller);
        let queue = Arc::clone(&queue);
        let cancel = Arc::clone(&cancel);
        let done = Arc::clone(&done);
        let tx = tx.clone();
        thread::spawn(move || loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let node = {
                let mut queue = queue.lock().unwrap();
                queue.next()
            };
            let Some(node) = node else {
                break;
            };
            if let Ok(delay) = controller.delay(&node, timeout_ms) {
                let _ = tx.send(DelayResult { node, delay });
            }
            done.fetch_add(1, Ordering::Relaxed);
        });
    }
    drop(tx);

    let mut probe = Probe::start(label, total, Arc::clone(&done));
    // Esc/Ctrl+C 需要在原始模式下逐键读取；退出作用域自动恢复终端。
    let raw_ok = std::io::stdin().is_terminal() && enable_raw_mode().is_ok();
    let _guard = raw_ok.then_some(RawModeGuard);

    let mut best: Option<DelayResult> = None;
    let mut cancelled = false;
    let collect = |best: &mut Option<DelayResult>| {
        while let Ok(result) = rx.try_recv() {
            if best.as_ref().is_none_or(|b| result.delay < b.delay) {
                *best = Some(result);
            }
        }
    };
    loop {
        if raw_ok && event::poll(Duration::from_millis(120)).unwrap_or(false) {
            if let Ok(Event::Key(k)) = event::read() {
                if k.kind != KeyEventKind::Release {
                    if k.code == KeyCode::Esc {
                        cancel.store(true, Ordering::Relaxed);
                        cancelled = true;
                    } else if k.modifiers.contains(KeyModifiers::CONTROL)
                        && k.code == KeyCode::Char('c')
                    {
                        cancel.store(true, Ordering::Relaxed);
                        probe.finish();
                        let _ = disable_raw_mode();
                        eprintln!();
                        std::process::exit(130);
                    }
                }
            }
        } else if !raw_ok {
            thread::sleep(Duration::from_millis(120));
        }

        collect(&mut best);
        if cancelled || done.load(Ordering::Relaxed) >= total {
            collect(&mut best);
            break;
        }
    }
    probe.finish();

    if cancelled {
        BatchOutcome::Cancelled
    } else if let Some(best) = best {
        BatchOutcome::Best(best)
    } else {
        BatchOutcome::Empty
    }
}

/// 退出作用域时恢复终端 cooked 模式，即使发生 panic 也不会把终端留在原始模式。
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// 单行测速进度指示器：spinner 在独立后台线程按 ~80ms 节奏刷新，因此主线程无论
/// 阻塞与否都不会让进度看起来卡死；仅当 stderr 是 tty 时才渲染。
struct Probe {
    finished: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    animate: bool,
}

impl Probe {
    fn start(label: &str, total: u64, done: Arc<AtomicU64>) -> Probe {
        let animate = std::io::stderr().is_terminal();
        let finished = Arc::new(AtomicBool::new(false));
        let handle = if animate {
            let finished = Arc::clone(&finished);
            let label = label.to_string();
            let width = terminal_width();
            Some(thread::spawn(move || {
                let mut frame = 0usize;
                while !finished.load(Ordering::Relaxed) {
                    let d = done.load(Ordering::Relaxed).min(total);
                    let line = render_probe_line(spinner_frame(frame), &label, d, total, width);
                    let mut stderr = std::io::stderr();
                    let _ = write!(stderr, "\r\x1b[K{line}");
                    let _ = stderr.flush();
                    frame += 1;
                    thread::sleep(Duration::from_millis(80));
                }
            }))
        } else {
            None
        };
        Probe {
            finished,
            handle,
            animate,
        }
    }

    /// 停止动画线程并清掉当前进度行，让下一批次或结果从干净的行开始。
    fn finish(&mut self) {
        self.finished.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        if self.animate {
            let mut stderr = std::io::stderr();
            let _ = write!(stderr, "\r\x1b[K");
            let _ = stderr.flush();
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        if self.handle.is_some() {
            self.finish();
        }
    }
}

/// 渲染一行测速进度：`<spinner> 节点延迟测试: {label} {bar} {done}/{total}`。
/// 进度条宽度随终端宽度自适应，先收缩进度条，实在放不下才截断标签，保证不换行。
fn render_probe_line(spinner: &str, label: &str, done: u64, total: u64, width: usize) -> String {
    let width = width.max(10);
    let count = format!("{done}/{total}");
    let count_w = display_width(&count);
    let prefix = format!("节点延迟测试: {label}");
    // "{spinner} {prefix} {bar} {count}" 中除 prefix/bar 外的固定宽度：spinner + 3 空格 + count
    let scaffold = 4 + count_w;
    let budget = width.saturating_sub(scaffold);
    let prefix_w = display_width(&prefix);
    let (prefix, bar_w) = if prefix_w <= budget {
        (prefix, (budget - prefix_w).min(36))
    } else {
        (truncate_to_width(&prefix, budget), 0)
    };
    let head = format!("{ANSI_SPINNER}{spinner}{ANSI_RESET} {prefix}");
    if bar_w == 0 {
        format!("{head} {count}")
    } else {
        format!("{head} {} {count}", render_bar(bar_w, done, total))
    }
}

fn render_bar(width: usize, done: u64, total: u64) -> String {
    if width == 0 {
        return String::new();
    }
    let filled = if total == 0 {
        width
    } else {
        ((done as u128 * width as u128) / total as u128) as usize
    }
    .min(width);
    format!(
        "{ANSI_BAR_FILLED}{}{ANSI_BAR_EMPTY}{}{ANSI_RESET}",
        "━".repeat(filled),
        "─".repeat(width - filled)
    )
}

fn choose_from_list(
    title: &str,
    initial: usize,
    options: &[String],
    max_rows: usize,
) -> Result<usize> {
    if options.is_empty() {
        return Err(anyhow!("没有可选择项"));
    }
    let theme = ColorfulTheme::default();
    let selected = Select::with_theme(&theme)
        .with_prompt(title)
        .items(options)
        .default(initial.min(options.len() - 1))
        .max_length(max_rows)
        .interact_opt()?;
    selected.ok_or_else(|| anyhow!("已取消"))
}

pub fn filter_nodes_by_keyword(nodes: &[String], filter: Option<&str>) -> Vec<String> {
    let Some(filter) = filter.map(str::trim).filter(|value| !value.is_empty()) else {
        return nodes.to_vec();
    };
    let keywords: Vec<String> = filter
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase)
        .collect();
    if keywords.is_empty() {
        return nodes.to_vec();
    }
    nodes
        .iter()
        .filter(|node| {
            let node = node.to_lowercase();
            keywords.iter().any(|keyword| node.contains(keyword))
        })
        .cloned()
        .collect()
}

fn print_interactive_error(
    paths: &Paths,
    app_config: &AppConfig,
    error: anyhow::Error,
) -> Result<()> {
    let status = collect_status(paths, false).ok();
    let message = if error.to_string().contains("已取消") {
        "错误：已取消".to_string()
    } else {
        format!("错误：{error:#}")
    };
    println!(
        "{}",
        error_line(
            &message,
            status.as_ref(),
            &app_config.prompt,
            &app_config.tag_defaults()
        )
    );
    Ok(())
}

pub fn parse_controller_json_response(output: &[u8]) -> Result<Option<serde_json::Value>> {
    if output.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(None);
    }
    serde_json::from_slice(output)
        .map(Some)
        .context("无法解析 controller JSON")
}

fn read_yaml_file(path: &Path) -> Result<serde_yaml::Value> {
    let input = fs::read_to_string(path).with_context(|| format!("无法读取 {}", path.display()))?;
    serde_yaml::from_str(&input).with_context(|| format!("无法解析 {}", path.display()))
}

fn yaml_string(value: &serde_yaml::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.trim_matches(['"', '\'']).to_string())
}

fn read_app_config(paths: &Paths) -> Result<AppConfig> {
    let input = fs::read_to_string(&paths.app_config)
        .with_context(|| format!("无法读取 {}", paths.app_config.display()))?;
    serde_yaml::from_str(&input).with_context(|| format!("无法解析 {}", paths.app_config.display()))
}

fn ensure_config_file(paths: &Paths) -> Result<InstallAction> {
    if !paths.app_config.exists() {
        fs::write(&paths.app_config, default_config_file())
            .with_context(|| format!("无法写入 {}", paths.app_config.display()))?;
        return Ok(InstallAction::Set);
    }

    let input = fs::read_to_string(&paths.app_config)
        .with_context(|| format!("无法读取 {}", paths.app_config.display()))?;
    let updated = ensure_config_defaults(&input);
    if updated != input {
        fs::write(&paths.app_config, updated)
            .with_context(|| format!("无法写入 {}", paths.app_config.display()))?;
    }
    Ok(InstallAction::Updated)
}

/// 补齐缺失的默认配置：mode/group 默认值、prompt 图标，并移除废弃的 active_group。
pub fn ensure_config_defaults(input: &str) -> String {
    let mut output = remove_active_group_config(input);
    if !config_has_key(&output, "mode:") {
        push_config_line(&mut output, "mode: \"rule\"");
    }
    if !config_has_key(&output, "group:") {
        push_config_line(&mut output, "group: \"🔰 手动选择\"");
    }
    if !config_has_key(&output, "prompt:") {
        push_config_line(
            &mut output,
            r#"prompt:
  mode_icon: "󰒓"
  group_icon: "󰓹"
  node_icon: "󰍍"
  delay_icon: "󱎫"
  port_icon: "󰤨"
"#,
        );
    }
    output
}

fn config_has_key(input: &str, key: &str) -> bool {
    input
        .lines()
        .any(|line| line.trim_start().starts_with(key))
}

fn push_config_line(output: &mut String, line: &str) {
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(line);
    if !output.ends_with('\n') {
        output.push('\n');
    }
}

pub fn remove_active_group_config(input: &str) -> String {
    let mut output = input
        .lines()
        .filter(|line| !line.trim_start().starts_with("active_group:"))
        .collect::<Vec<_>>()
        .join("\n");
    if input.ends_with('\n') && !output.is_empty() {
        output.push('\n');
    }
    output
}

fn write_completion_file(paths: &Paths) -> Result<(InstallAction, PathBuf)> {
    let source_file = paths.data_dir.join("_verge-proxy");
    let target_file = paths.zsh_functions_dir.join("_verge-proxy");
    let action = if target_file.exists() {
        InstallAction::Updated
    } else {
        InstallAction::Set
    };
    fs::create_dir_all(&paths.data_dir)?;
    fs::write(
        &source_file,
        r#"#compdef verge-proxy

_verge-proxy() {
  local -a commands
  commands=(
    'start:读取端口并设置当前 zsh 代理环境变量'
    'stop:移除当前 zsh 代理环境变量'
    'restart:重新读取端口并更新代理环境变量'
    'status:显示 mode/group/node/delay/port'
    'mode:交互切换规则/全局/直连'
    'group:交互切换代理组'
    'node:交互切换节点'
    'port:输出当前 mixed-port'
    'auto-node:自动测速并选择最快节点'
    'install:配置 verge-proxy'
  )
  _arguments -s \
    '(-h --help)'{-h,--help}'[显示帮助信息]' \
    '1:command:->cmds' \
    '*::arg:->args'
  case "$state" in
    cmds) _describe 'command' commands ;;
    args)
      case "$words[1]" in
        stop|restart) _arguments '(-f --force)'{-f,--force}'[跳过 proxy_name 检查强制执行]' ;;
        node) _arguments '--filter=[按关键字预筛节点]' ;;
        auto-node) _arguments '--filter=[限制测速范围，逗号分隔]' ;;
      esac
      ;;
  esac
}

_verge-proxy "$@"
"#,
    )?;
    fs::create_dir_all(&paths.zsh_functions_dir)?;
    if target_file.exists() || fs::symlink_metadata(&target_file).is_ok() {
        fs::remove_file(&target_file)
            .with_context(|| format!("无法移除旧补全配置 {}", target_file.display()))?;
    }
    unix_fs::symlink(&source_file, &target_file).with_context(|| {
        format!(
            "无法创建补全软链接 {} -> {}",
            target_file.display(),
            source_file.display()
        )
    })?;
    Ok((action, target_file))
}

fn update_zshrc(paths: &Paths) -> Result<InstallAction> {
    let exe = env::current_exe()
        .ok()
        .and_then(|path| path.canonicalize().ok())
        .unwrap_or_else(|| PathBuf::from("verge-proxy"));
    let exe = exe.display().to_string();
    let block = zsh_wrapper_block(&exe);
    let original = fs::read_to_string(&paths.zshrc).unwrap_or_default();
    let action = if original.contains(BLOCK_BEGIN) && original.contains(BLOCK_END) {
        InstallAction::Updated
    } else {
        InstallAction::Set
    };
    let updated = replace_managed_block(&original, &block);
    fs::write(&paths.zshrc, updated)?;
    Ok(action)
}

pub fn install_line(
    action: InstallAction,
    name: &str,
    path: &Path,
    prompt: &PromptConfig,
) -> String {
    success_line(
        &format!("{}{}: {}", action.label(), name, path.display()),
        None,
        prompt,
        &TagDefaults::default(),
    )
}

fn default_config_file() -> &'static str {
    r#"filter: ""
concurrency: 20
timeout_ms: 5000
mode: "rule"
group: "🔰 手动选择"
prompt:
  mode_icon: "󰒓"
  group_icon: "󰓹"
  node_icon: "󰍍"
  delay_icon: "󱎫"
  port_icon: "󰤨"
"#
}

pub fn zsh_wrapper_block(exe: &str) -> String {
    format!(
        r#"{BLOCK_BEGIN}
# verge-proxy wrapper (added by verge-proxy install)
verge-proxy() {{
  case "$1" in
    start|stop|restart) eval "$(COLUMNS=${{COLUMNS:-80}} "{exe}" "$@")" ;;
    *) COLUMNS=${{COLUMNS:-80}} "{exe}" "$@" ;;
  esac
}}
vp() {{
  (
    eval "$(COLUMNS=${{COLUMNS:-80}} "{exe}" restart -f)" >&2 || exit
    if [[ -n ${{aliases[$1]}} ]]; then
      eval "${{aliases[$1]}} ${{(j: :)${{(@q)@[2,-1]}}}}"
    else
      "$@"
    fi
  )
}}
{BLOCK_END}
"#
    )
}

pub fn replace_managed_block(original: &str, block: &str) -> String {
    let Some(begin) = original.find(BLOCK_BEGIN) else {
        let mut output = original.to_string();
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push('\n');
        output.push_str(block);
        return output;
    };
    let Some(relative_end) = original[begin..].find(BLOCK_END) else {
        let mut output = original.to_string();
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push('\n');
        output.push_str(block);
        return output;
    };
    let mut end = begin + relative_end + BLOCK_END.len();
    if original[end..].starts_with('\n') {
        end += 1;
    }
    format!("{}{}{}", &original[..begin], block, &original[end..])
}

fn encode(input: &str) -> String {
    utf8_percent_encode(input, NON_ALPHANUMERIC).to_string()
}

fn ansi_bg_fg(bg: &str, fg: &str) -> String {
    let (br, bgc, bb) = hex_to_rgb(bg).unwrap_or((49, 50, 68));
    let (fr, fgc, fb) = hex_to_rgb(fg).unwrap_or((17, 17, 27));
    format!("\x1b[48;2;{br};{bgc};{bb}m\x1b[38;2;{fr};{fgc};{fb}m")
}

fn ansi_fg(fg: &str) -> String {
    let (fr, fgc, fb) = hex_to_rgb(fg).unwrap_or((205, 214, 244));
    format!("\x1b[38;2;{fr};{fgc};{fb}m")
}

fn terminal_width() -> usize {
    env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .or_else(|| {
            Command::new("tput")
                .arg("cols")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| {
                    String::from_utf8_lossy(&output.stdout)
                        .trim()
                        .parse::<usize>()
                        .ok()
                })
        })
        .unwrap_or(80)
}

fn display_width(input: &str) -> usize {
    UnicodeWidthStr::width(input)
}

fn hex_to_rgb(input: &str) -> Option<(u8, u8, u8)> {
    let input = input.strip_prefix('#').unwrap_or(input);
    if input.len() != 6 {
        return None;
    }
    let red = u8::from_str_radix(&input[0..2], 16).ok()?;
    let green = u8::from_str_radix(&input[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&input[4..6], 16).ok()?;
    Some((red, green, blue))
}

fn shell_single_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

pub fn proxies_from_pairs(pairs: &[(&str, &str, &[&str])]) -> HashMap<String, Proxy> {
    pairs
        .iter()
        .map(|(name, now, all)| {
            (
                (*name).to_string(),
                Proxy {
                    name: (*name).to_string(),
                    proxy_type: "Selector".to_string(),
                    now: if now.is_empty() {
                        None
                    } else {
                        Some((*now).to_string())
                    },
                    all: all.iter().map(|value| (*value).to_string()).collect(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_ansi(input: &str) -> String {
        let mut output = String::new();
        let mut chars = input.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            output.push(ch);
        }
        output
    }

    #[test]
    fn stop_and_restart_accept_force_flag() {
        assert!(matches!(
            Cli::try_parse_from(["verge-proxy", "stop"]).unwrap().command,
            Commands::Stop { force: false }
        ));
        assert!(matches!(
            Cli::try_parse_from(["verge-proxy", "stop", "--force"])
                .unwrap()
                .command,
            Commands::Stop { force: true }
        ));
        assert!(matches!(
            Cli::try_parse_from(["verge-proxy", "restart", "-f"])
                .unwrap()
                .command,
            Commands::Restart { force: true }
        ));
        assert!(Cli::try_parse_from(["verge-proxy", "start", "--force"]).is_err());
    }

    #[test]
    fn spinner_frame_cycles() {
        assert_eq!(spinner_frame(0), SPINNER_FRAMES[0]);
        assert_eq!(spinner_frame(10), SPINNER_FRAMES[0]);
        assert_eq!(spinner_frame(11), SPINNER_FRAMES[1]);
    }

    #[test]
    fn probe_line_shows_spinner_label_and_count() {
        let line = render_probe_line("⠋", "日本", 12, 36, 80);
        let plain = strip_ansi(&line);
        assert!(plain.starts_with("⠋ "));
        assert!(plain.contains("节点延迟测试: 日本"));
        assert!(plain.contains("12/36"));
        assert!(plain.contains('━') || plain.contains('─'));
    }

    #[test]
    fn probe_line_bar_shrinks_and_never_overflows_width() {
        for width in [20usize, 24, 30, 40, 60, 80, 120] {
            let line = render_probe_line("⠹", "新加坡 IPLC 专线中转", 3, 50, width);
            let plain = strip_ansi(&line);
            assert!(
                display_width(&plain) <= width,
                "width={width} rendered={} line={plain:?}",
                display_width(&plain)
            );
        }
    }

    #[test]
    fn probe_line_bar_fills_proportionally() {
        let none = strip_ansi(&render_probe_line("⠋", "日本", 0, 10, 80));
        let full = strip_ansi(&render_probe_line("⠋", "日本", 10, 10, 80));
        assert!(!none.contains('━'));
        assert!(!full.contains('─'));
        assert!(full.contains('━'));
    }

    #[test]
    fn reads_port_from_controller_configs_without_hardcoding() {
        let configs = serde_json::json!({"mixed-port": 7899, "mode": "rule"});
        assert_eq!(port_from_configs_json(&configs), Some(7899));
    }

    #[test]
    fn reads_port_from_yaml_fallback() {
        assert_eq!(
            port_from_yaml_str("mode: direct\nmixed-port: 7898\n"),
            Some(7898)
        );
    }

    #[test]
    fn displays_modes_in_chinese() {
        assert_eq!(Mode::parse("rule").label(), "规则");
        assert_eq!(Mode::parse("global").label(), "全局");
        assert_eq!(Mode::parse("direct").label(), "直连");
    }

    #[test]
    fn resolves_global_group_to_nested_manual_node() {
        let proxies = proxies_from_pairs(&[
            ("GLOBAL", "🔰 手动选择", &["DIRECT", "🔰 手动选择"]),
            ("🔰 手动选择", "日本 A19", &["日本 A19", "新加坡 B01"]),
        ]);
        assert_eq!(
            resolve_active_group_and_node(&Mode::Rule, &proxies),
            ("🔰 手动选择".to_string(), "日本 A19".to_string())
        );
    }

    #[test]
    fn resolves_leaf_global_now_as_global_group() {
        let proxies = proxies_from_pairs(&[
            ("GLOBAL", "日本 A19", &["DIRECT", "🔰 手动选择", "日本 A19"]),
            ("🔰 手动选择", "新加坡 B01", &["日本 A19", "新加坡 B01"]),
        ]);
        assert_eq!(
            resolve_active_group_and_node(&Mode::Global, &proxies),
            ("GLOBAL".to_string(), "日本 A19".to_string())
        );
    }

    #[test]
    fn direct_mode_status_is_direct_even_if_selectors_have_other_state() {
        let proxies = proxies_from_pairs(&[
            ("GLOBAL", "🔰 手动选择", &["DIRECT", "🔰 手动选择"]),
            ("🔰 手动选择", "日本 A19", &["日本 A19"]),
        ]);
        assert_eq!(
            resolve_active_group_and_node(&Mode::Direct, &proxies),
            ("DIRECT".to_string(), "DIRECT".to_string())
        );
    }

    #[test]
    fn filter_ranges_match_in_order_with_fallback() {
        let nodes = vec![
            "日本 A".to_string(),
            "新加坡 B".to_string(),
            "美国 C".to_string(),
        ];
        let batches = candidate_batches(&nodes, &["新加坡".to_string(), "日本".to_string()]);
        assert_eq!(
            batches[0],
            ("新加坡".to_string(), vec!["新加坡 B".to_string()])
        );
        assert_eq!(batches[1], ("日本".to_string(), vec!["日本 A".to_string()]));
        assert_eq!(
            batches[2],
            ("其他节点".to_string(), vec!["美国 C".to_string()])
        );
    }

    #[test]
    fn install_block_replaces_existing_managed_region_only() {
        let original = "before\n# >>> verge-proxy >>>\nold\n# <<< verge-proxy <<<\nafter\n";
        let block = "# >>> verge-proxy >>>\nnew\n# <<< verge-proxy <<<\n";
        assert_eq!(
            replace_managed_block(original, block),
            "before\n# >>> verge-proxy >>>\nnew\n# <<< verge-proxy <<<\nafter\n"
        );
    }

    #[test]
    fn empty_controller_success_body_is_not_json_error() {
        assert_eq!(parse_controller_json_response(b"").unwrap(), None);
        assert_eq!(
            parse_controller_json_response(br#"{"delay": 10}"#)
                .unwrap()
                .unwrap()["delay"],
            10
        );
    }

    #[test]
    fn generated_zsh_wrapper_evals_environment_commands() {
        let block = zsh_wrapper_block("/usr/local/bin/verge-proxy");
        assert!(
            block.contains(
                r#"start|stop|restart) eval "$(COLUMNS=${COLUMNS:-80} "/usr/local/bin/verge-proxy" "$@")" ;;"#
            )
        );
        assert!(block.contains(
            r#"eval "$(COLUMNS=${COLUMNS:-80} "/usr/local/bin/verge-proxy" restart -f)" >&2 || exit"#
        ));
        assert!(block.contains(r#"*) COLUMNS=${COLUMNS:-80} "/usr/local/bin/verge-proxy" "$@" ;;"#));
        assert!(!block.contains("compinit"));
        assert!(!block.contains("site-functions"));
    }

    #[test]
    fn status_prompt_is_single_line_without_field_names() {
        let info = StatusInfo {
            mode: Mode::Rule,
            group: "🔰 手动选择".to_string(),
            node: "日本 A19".to_string(),
            delay: Delay::Value(108),
            port: 7897,
        };
        let prompt = format_status_prompt(&info, &PromptConfig::default(), &TagDefaults::default(), 240, 0);
        assert!(!prompt.contains("mode"));
        assert!(!prompt.contains("group"));
        assert!(!prompt.contains("node"));
        assert!(!prompt.contains("port"));
        assert!(prompt.contains("规则"));
        assert!(prompt.contains("🔰 手动选择"));
        assert!(prompt.contains("日本 A19"));
        assert!(prompt.contains("108ms"));
        assert!(prompt.contains("7897"));
    }

    #[test]
    fn status_prompt_wraps_between_complete_segments() {
        let info = StatusInfo {
            mode: Mode::Rule,
            group: "🔰 手动选择".to_string(),
            node: "日本 A19 IPV6双栈本地路由".to_string(),
            delay: Delay::Value(108),
            port: 7897,
        };
        let prompt = format_status_prompt(&info, &PromptConfig::default(), &TagDefaults::default(), 24, 0);
        assert!(prompt.contains('\n'));
        for line in prompt.lines() {
            assert!(line.contains(''));
            assert!(line.contains(''));
        }
    }

    #[test]
    fn status_prompt_truncates_long_segments_to_terminal_width() {
        let info = StatusInfo {
            mode: Mode::Rule,
            group: "🔰 手动选择".to_string(),
            node: "日本 A19 IPV6双栈本地路由-超长节点名称-用于验证不会段内折行".to_string(),
            delay: Delay::Value(108),
            port: 7897,
        };
        let prompt = format_status_prompt(&info, &PromptConfig::default(), &TagDefaults::default(), 24, 0);
        let plain = strip_ansi(&prompt);

        assert!(plain.contains('…'));
        for line in plain.lines() {
            assert!(
                display_width(line) <= 24,
                "line width {} exceeded limit: {line}",
                display_width(line)
            );
            assert!(line.contains(''));
            assert!(line.contains(''));
        }
    }

    #[test]
    fn status_prompt_normalizes_embedded_newlines_in_segments() {
        let info = StatusInfo {
            mode: Mode::Rule,
            group: "🔰 手动选择".to_string(),
            node: "日本 A13\n电信优化路由".to_string(),
            delay: Delay::Value(108),
            port: 7897,
        };
        let prompt = format_status_prompt(&info, &PromptConfig::default(), &TagDefaults::default(), 80, 0);
        let plain = strip_ansi(&prompt);

        assert!(plain.contains("日本 A13 电信优化路由"));
        for line in plain.lines() {
            assert!(line.contains(''));
            assert!(line.contains(''));
        }
    }

    #[test]
    fn status_prompt_wraps_before_node_when_icon_widths_are_ambiguous() {
        let info = StatusInfo {
            mode: Mode::Rule,
            group: "🔰 手动选择".to_string(),
            node: "日本 A13 电信优化路由".to_string(),
            delay: Delay::Value(208),
            port: 7897,
        };
        let prompt = format_status_prompt(
            &info,
            &PromptConfig::default(),
            &TagDefaults::default(),
            60,
            display_width("错误：已取消") + 3,
        );
        let plain = strip_ansi(&prompt);
        let lines = plain.lines().collect::<Vec<_>>();

        assert!(lines.len() >= 2, "plain prompt: {plain:?}");
        assert!(
            !lines[0].contains("日本 A13"),
            "first line width {}, prompt width {}, line: {:?}",
            display_width(lines[0]),
            prompt_display_width(lines[0]),
            lines[0]
        );
        assert!(lines[1].contains("日本 A13 电信优化路由"));
    }

    #[test]
    fn success_and_error_lines_use_bold_colored_prefixes() {
        let info = StatusInfo {
            mode: Mode::Direct,
            group: "DIRECT".to_string(),
            node: "DIRECT".to_string(),
            delay: Delay::Timeout,
            port: 7897,
        };
        let prompt = PromptConfig::default();
        assert!(success_line("设置直连成功", Some(&info), &prompt, &TagDefaults::default()).starts_with(ANSI_BOLD_GREEN));
        assert!(error_line("错误：已取消", Some(&info), &prompt, &TagDefaults::default()).starts_with(ANSI_BOLD_RED));
    }

    #[test]
    fn node_filter_is_applied_before_dialoguer_select_with_comma_or() {
        let items = vec![
            "日本 A".to_string(),
            "新加坡 B".to_string(),
            "美国 C".to_string(),
        ];
        assert_eq!(filter_nodes_by_keyword(&items, None), items);
        assert_eq!(
            filter_nodes_by_keyword(&items, Some("日本")),
            vec!["日本 A".to_string()]
        );
        assert_eq!(
            filter_nodes_by_keyword(&items, Some("日本, 美国")),
            vec!["日本 A".to_string(), "美国 C".to_string()]
        );
        assert!(filter_nodes_by_keyword(&items, Some("德国,法国")).is_empty());
    }

    #[test]
    fn install_line_names_action_item_and_path() {
        let line = install_line(
            InstallAction::Updated,
            "补全配置",
            Path::new("/tmp/_verge-proxy"),
            &PromptConfig::default(),
        );
        assert!(line.contains("已更新补全配置: /tmp/_verge-proxy"));
        assert!(line.starts_with(ANSI_BOLD_GREEN));
    }

    #[test]
    fn install_adds_prompt_defaults_to_existing_config_without_overwriting() {
        let existing = "active_group: 🔰 手动选择\nfilter: ''\n";
        let updated = ensure_config_defaults(existing);
        assert!(!updated.contains("active_group:"));
        assert!(updated.contains("prompt:"));
        assert!(updated.contains("mode_icon"));

        assert!(updated.contains("mode: \"rule\""));
        assert!(updated.contains("group: \"🔰 手动选择\""));

        let custom = "mode: global\ngroup: 其他\nprompt:\n  mode_icon: X\n";
        assert_eq!(ensure_config_defaults(custom), custom);
    }

    #[test]
    fn existing_prompt_config_still_drops_active_group() {
        let existing = "active_group: old\nfilter: 日本\nprompt:\n  mode_icon: X\n";
        let updated = ensure_config_defaults(existing);
        assert!(!updated.contains("active_group:"));
        assert!(updated.contains("filter: 日本"));
        assert!(updated.contains("mode_icon: X"));
        assert!(updated.contains("mode: \"rule\""));
        assert!(updated.contains("group: \"🔰 手动选择\""));
    }

    #[test]
    fn status_prompt_hides_default_mode_and_group_tags() {
        let info = StatusInfo {
            mode: Mode::Rule,
            group: "🔰 手动选择".to_string(),
            node: "日本 A19".to_string(),
            delay: Delay::Value(108),
            port: 7897,
        };
        let defaults = TagDefaults {
            mode: Some("rule".to_string()),
            group: Some("🔰 手动选择".to_string()),
        };
        let prompt = format_status_prompt(&info, &PromptConfig::default(), &defaults, 240, 0);
        let plain = strip_ansi(&prompt);

        assert!(!plain.contains("规则"));
        assert!(!plain.contains("手动选择"));
        assert!(plain.contains("日本 A19"));
        assert!(plain.contains("108ms"));
        assert!(plain.contains("7897"));
        // 首个可见 tag 仍显示左胶囊
        assert!(plain.starts_with('\u{e0b6}'));
    }

    #[test]
    fn status_prompt_keeps_non_default_mode_and_group_tags() {
        let info = StatusInfo {
            mode: Mode::Global,
            group: "🚀 节点选择".to_string(),
            node: "日本 A19".to_string(),
            delay: Delay::Value(108),
            port: 7897,
        };
        let defaults = TagDefaults {
            mode: Some("rule".to_string()),
            group: Some("🔰 手动选择".to_string()),
        };
        let prompt = format_status_prompt(&info, &PromptConfig::default(), &defaults, 240, 0);
        let plain = strip_ansi(&prompt);

        assert!(plain.contains("全局"));
        assert!(plain.contains("🚀 节点选择"));
    }

    #[test]
    fn status_prompt_joins_adjacent_tags_without_gap() {
        let info = StatusInfo {
            mode: Mode::Rule,
            group: "🔰 手动选择".to_string(),
            node: "日本 A19".to_string(),
            delay: Delay::Value(108),
            port: 7897,
        };
        let prompt =
            format_status_prompt(&info, &PromptConfig::default(), &TagDefaults::default(), 240, 0);
        let plain = strip_ansi(&prompt);

        // 单行内只有第一个 tag 显示左胶囊，其余 tag 紧挨左侧 tag
        assert_eq!(plain.matches('\u{e0b6}').count(), 1);
        assert_eq!(plain.matches('\u{e0b4}').count(), 5);
        assert!(!plain.contains("\u{e0b4} \u{e0b6}"));
    }

    #[test]
    fn status_prompt_omits_delay_segment_when_hidden() {
        let info = StatusInfo {
            mode: Mode::Rule,
            group: "🔰 手动选择".to_string(),
            node: "日本 A19".to_string(),
            delay: Delay::Hidden,
            port: 7897,
        };
        let prompt =
            format_status_prompt(&info, &PromptConfig::default(), &TagDefaults::default(), 240, 0);
        let plain = strip_ansi(&prompt);

        assert!(!plain.contains("ms"));
        assert!(!plain.contains("timeout"));
        assert!(plain.contains("日本 A19"));
        assert!(plain.contains("7897"));
    }

    #[test]
    fn status_prompt_blends_right_cap_into_next_segment_background() {
        let info = StatusInfo {
            mode: Mode::Rule,
            group: "🔰 手动选择".to_string(),
            node: "日本 A19".to_string(),
            delay: Delay::Value(108),
            port: 7897,
        };
        let prompt =
            format_status_prompt(&info, &PromptConfig::default(), &TagDefaults::default(), 240, 0);

        // 相邻 segment 的右胶囊：fg=当前段颜色，bg=下一段颜色，衔接处不 reset
        assert!(prompt.contains(&format!("{}\u{e0b4}", ansi_bg_fg("#f9e2af", "#fab387"))));
        assert!(prompt.contains(&format!("{}\u{e0b4}", ansi_bg_fg("#a6e3a1", "#f9e2af"))));
        assert!(prompt.contains(&format!("{}\u{e0b4}", ansi_bg_fg("#74c7ec", "#a6e3a1"))));
        // 行尾右胶囊画在默认背景上
        assert!(prompt.ends_with(&format!("{}\u{e0b4}{ANSI_RESET}", ansi_fg("#b4befe"))));
    }

    #[test]
    fn status_prompt_wrapped_lines_restart_with_left_capsule() {
        let info = StatusInfo {
            mode: Mode::Rule,
            group: "🔰 手动选择".to_string(),
            node: "日本 A19".to_string(),
            delay: Delay::Value(108),
            port: 7897,
        };
        let prompt =
            format_status_prompt(&info, &PromptConfig::default(), &TagDefaults::default(), 24, 0);
        let plain = strip_ansi(&prompt);

        assert!(plain.lines().count() > 1);
        for line in plain.lines() {
            assert_eq!(line.matches('\u{e0b6}').count(), 1, "line: {line}");
            assert!(line.contains('\u{e0b4}'));
        }
    }
}
