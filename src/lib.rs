use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use dialoguer::{theme::ColorfulTheme, Select};
use indicatif::{ProgressBar, ProgressStyle};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;
use serde_json::json;
use std::cmp::min;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

const DEFAULT_DELAY_TEST_URL: &str = "https://www.gstatic.com/generate_204";
const DEFAULT_DELAY_TIMEOUT_MS: u64 = 2_000;
const DEFAULT_CONCURRENCY: usize = 20;
const BLOCK_BEGIN: &str = "# >>> verge-proxy >>>";
const BLOCK_END: &str = "# <<< verge-proxy <<<";
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD_GREEN: &str = "\x1b[1;38;2;166;227;161m";
const ANSI_BOLD_RED: &str = "\x1b[1;38;2;243;139;168m";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusInfo {
    pub mode: Mode,
    pub group: String,
    pub node: String,
    pub delay: Option<u64>,
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
    configured_group: Option<&str>,
    proxies: &HashMap<String, Proxy>,
) -> (String, String) {
    if *mode == Mode::Direct {
        return ("DIRECT".to_string(), "DIRECT".to_string());
    }

    let global_now = proxies.get("GLOBAL").and_then(|proxy| proxy.now.as_deref());
    let starting_group = configured_group
        .filter(|name| proxies.contains_key(*name))
        .map(str::to_string)
        .or_else(|| {
            global_now
                .filter(|name| {
                    proxies
                        .get(*name)
                        .is_some_and(|proxy| !proxy.all.is_empty())
                })
                .map(str::to_string)
        })
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
    let delay = info
        .delay
        .map(|delay| format!("{delay}ms"))
        .unwrap_or_else(|| "timeout".to_string());
    [
        format!("mode: {}", info.mode.label()),
        format!("group: {}", info.group),
        format!("node: {}", info.node),
        format!("delay: {delay}"),
        format!("port: {}", info.port),
    ]
    .join("\n")
}

pub fn format_status_prompt(
    info: &StatusInfo,
    prompt: &PromptConfig,
    terminal_width: usize,
    initial_width: usize,
) -> String {
    let delay = info
        .delay
        .map(|delay| format!("{delay}ms"))
        .unwrap_or_else(|| "timeout".to_string());
    let segments = [
        PromptSegment::new(
            prompt.mode_icon.as_deref().unwrap_or("󰒓"),
            info.mode.label().to_string(),
            "#fab387",
        ),
        PromptSegment::new(
            prompt.group_icon.as_deref().unwrap_or("󰓹"),
            info.group.clone(),
            "#f9e2af",
        ),
        PromptSegment::new(
            prompt.node_icon.as_deref().unwrap_or("󰤨"),
            info.node.clone(),
            "#a6e3a1",
        ),
        PromptSegment::new(
            prompt.delay_icon.as_deref().unwrap_or("󱎫"),
            delay,
            "#74c7ec",
        ),
        PromptSegment::new(
            prompt.port_icon.as_deref().unwrap_or("󰍍"),
            info.port.to_string(),
            "#b4befe",
        ),
    ];

    let mut output = String::new();
    let mut current_width = initial_width;
    let terminal_width = terminal_width.max(20);
    for segment in segments {
        if current_width > 0 && current_width + segment.width > terminal_width {
            output.push('\n');
            current_width = 0;
        }
        output.push_str(&segment.render());
        output.push(' ');
        current_width += segment.width + 1;
    }
    output.pop();
    output
}

struct PromptSegment {
    icon: String,
    value: String,
    color: &'static str,
    width: usize,
}

impl PromptSegment {
    fn new(icon: &str, value: String, color: &'static str) -> Self {
        let plain = format!(" {icon} {value} ");
        Self {
            icon: icon.to_string(),
            value,
            color,
            width: display_width(&plain),
        }
    }

    fn render(&self) -> String {
        let mut output = String::new();
        output.push_str(&ansi_fg(self.color));
        output.push('');
        output.push_str(&ansi_bg_fg(self.color, "#11111b"));
        output.push(' ');
        output.push_str(&self.icon);
        output.push(' ');
        output.push_str(&self.value);
        output.push(' ');
        output.push_str(ANSI_RESET);
        output.push_str(&ansi_fg(self.color));
        output.push('');
        output.push_str(ANSI_RESET);
        output
    }
}

pub fn success_line(message: &str, status: Option<&StatusInfo>, prompt: &PromptConfig) -> String {
    let mut output = format!("{}✔{} {}", ANSI_BOLD_GREEN, ANSI_RESET, message);
    if let Some(status) = status {
        output.push(' ');
        output.push_str(&format_status_prompt(
            status,
            prompt,
            terminal_width(),
            display_width(message) + 2,
        ));
    }
    output
}

pub fn error_line(message: &str, status: Option<&StatusInfo>, prompt: &PromptConfig) -> String {
    let mut output = format!("{}✘{} {}", ANSI_BOLD_RED, ANSI_RESET, message);
    if let Some(status) = status {
        output.push(' ');
        output.push_str(&format_status_prompt(
            status,
            prompt,
            terminal_width(),
            display_width(message) + 2,
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
    Start,
    Stop,
    Restart,
    Status,
    Mode,
    Group,
    Node {
        #[arg(long)]
        filter: Option<String>,
    },
    Port,
    AutoNode {
        #[arg(long)]
        filter: Option<String>,
    },
    Install,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AppConfig {
    active_group: Option<String>,
    filter: Option<String>,
    concurrency: Option<usize>,
    timeout_ms: Option<u64>,
    #[serde(default)]
    prompt: PromptConfig,
}

#[derive(Debug, Clone)]
struct Paths {
    clash_config: PathBuf,
    clash_runtime_config: PathBuf,
    app_config_dir: PathBuf,
    app_config: PathBuf,
    completion_dir: PathBuf,
    completion_file: PathBuf,
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

    match cli.command {
        Commands::Start => cmd_start(&paths, &app_config),
        Commands::Stop => cmd_stop(),
        Commands::Restart => cmd_restart(&paths, &app_config),
        Commands::Status => {
            let info = collect_status(&paths, &app_config)?;
            println!(
                "{}",
                format_status_prompt(&info, &app_config.prompt, terminal_width(), 0)
            );
            Ok(())
        }
        Commands::Mode => cmd_mode(&paths, &app_config),
        Commands::Group => cmd_group(&paths, &app_config),
        Commands::Node { filter } => cmd_node(&paths, &app_config, filter.as_deref()),
        Commands::Port => {
            println!("{}", read_port(&paths)?);
            Ok(())
        }
        Commands::AutoNode { filter } => cmd_auto_node(&paths, &app_config, filter.as_deref()),
        Commands::Install => cmd_install(&paths),
    }
}

impl Paths {
    fn new() -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("无法定位 HOME"))?;
        let clash_dir =
            home.join("Library/Application Support/io.github.clash-verge-rev.clash-verge-rev");
        let app_config_dir = home.join(".config/verge-proxy");
        let completion_dir = app_config_dir.join("completions");
        Ok(Self {
            clash_config: clash_dir.join("config.yaml"),
            clash_runtime_config: clash_dir.join("clash-verge.yaml"),
            app_config: app_config_dir.join("config.yaml"),
            completion_file: completion_dir.join("_verge-proxy"),
            completion_dir,
            app_config_dir,
            zshrc: home.join(".zshrc"),
        })
    }
}

fn cmd_start(paths: &Paths, app_config: &AppConfig) -> Result<()> {
    let occupied: Vec<_> = ["http_proxy", "https_proxy", "all_proxy"]
        .into_iter()
        .filter(|name| env::var_os(name).is_some())
        .collect();
    if !occupied.is_empty() {
        println!(
            "echo {}",
            shell_single_quote(&error_line(
                "环境变量被占用，请执行 verge-proxy stop 后再次尝试",
                None,
                &app_config.prompt
            ))
        );
        println!("return 1 2>/dev/null || exit 1");
        return Ok(());
    }
    emit_proxy_exports(paths, app_config, "命令行代理已开启")
}

fn cmd_stop() -> Result<()> {
    println!("unset http_proxy https_proxy all_proxy no_proxy");
    println!(
        "echo {}",
        shell_single_quote(&success_line(
            "命令行代理已关闭，环境变量已移除",
            None,
            &PromptConfig::default()
        ))
    );
    Ok(())
}

fn cmd_restart(paths: &Paths, app_config: &AppConfig) -> Result<()> {
    emit_proxy_exports(paths, app_config, "命令行代理已重启")
}

fn emit_proxy_exports(paths: &Paths, app_config: &AppConfig, message: &str) -> Result<()> {
    let port = read_port(paths)?;
    println!("export http_proxy=http://127.0.0.1:{port}");
    println!("export https_proxy=http://127.0.0.1:{port}");
    println!("export all_proxy=socks5://127.0.0.1:{port}");
    println!("export no_proxy=localhost,127.0.0.1");
    let status = collect_status(paths, app_config).ok();
    println!(
        "echo {}",
        shell_single_quote(&success_line(message, status.as_ref(), &app_config.prompt))
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
        let status = collect_status(paths, app_config).ok();
        println!(
            "{}",
            success_line("设置直连成功", status.as_ref(), &app_config.prompt)
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
    let (current_group, _) =
        resolve_active_group_and_node(&mode, app_config.active_group.as_deref(), &proxies);
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
    write_active_group(paths, &groups[selected])?;
    Ok(())
}

fn cmd_node(paths: &Paths, app_config: &AppConfig, filter: Option<&str>) -> Result<()> {
    let controller = Controller::discover(paths)?;
    let mode = controller.mode()?;
    if mode == Mode::Direct {
        let status = collect_status(paths, app_config).ok();
        println!(
            "{}",
            success_line("设置直连成功", status.as_ref(), &app_config.prompt)
        );
        return Ok(());
    }
    let proxies = controller.proxies()?;
    let (group, current_node) =
        resolve_active_group_and_node(&mode, app_config.active_group.as_deref(), &proxies);
    let all_nodes = leaf_nodes_for_group(&group, &proxies);
    let nodes = filter_nodes_by_keyword(&all_nodes, filter);
    if nodes.is_empty() {
        println!(
            "{}",
            error_line(
                "错误：没有匹配节点",
                collect_status(paths, app_config).ok().as_ref(),
                &app_config.prompt
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
        let status = collect_status(paths, app_config).ok();
        println!(
            "{}",
            success_line("设置直连成功", status.as_ref(), &app_config.prompt)
        );
        return Ok(());
    }

    let proxies = controller.proxies()?;
    let (group, _) =
        resolve_active_group_and_node(&mode, app_config.active_group.as_deref(), &proxies);
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
    let timeout_ms = app_config
        .timeout_ms
        .unwrap_or(DEFAULT_DELAY_TIMEOUT_MS)
        .max(1);

    for (label, batch) in candidate_batches(&nodes, &ranges) {
        if batch.is_empty() {
            continue;
        }
        let progress = progress_bar(&label, batch.len() as u64);
        if let Some(best) = test_batch(
            &controller,
            batch,
            concurrency,
            timeout_ms,
            Some(progress.clone()),
        )? {
            progress.finish();
            controller.select_proxy(&group, &best.node)?;
            let status = collect_status(paths, app_config).ok();
            println!(
                "{}",
                success_line("已自动选择", status.as_ref(), &app_config.prompt)
            );
            return Ok(());
        }
        progress.finish();
    }

    Err(anyhow!("没有找到可连通节点"))
}

fn cmd_install(paths: &Paths) -> Result<()> {
    fs::create_dir_all(&paths.app_config_dir)
        .with_context(|| format!("无法创建 {}", paths.app_config_dir.display()))?;
    let config_action = ensure_config_file(paths)?;

    let completion_action = write_completion_file(paths)?;
    let zshrc_action = update_zshrc(paths)?;
    let prompt = PromptConfig::default();
    println!(
        "{}",
        install_line(zshrc_action, "环境配置", &paths.zshrc, &prompt)
    );
    println!(
        "{}",
        install_line(
            completion_action,
            "补全配置",
            &paths.completion_file,
            &prompt
        )
    );
    println!(
        "{}",
        install_line(config_action, "自定义配置", &paths.app_config, &prompt)
    );
    Ok(())
}

fn collect_status(paths: &Paths, app_config: &AppConfig) -> Result<StatusInfo> {
    let port = read_port(paths)?;
    let controller = Controller::discover(paths)?;
    let mode = controller.mode()?;
    let proxies = controller.proxies()?;
    let (group, node) =
        resolve_active_group_and_node(&mode, app_config.active_group.as_deref(), &proxies);
    let delay_target = if mode == Mode::Direct {
        "DIRECT"
    } else {
        &node
    };
    let delay = controller
        .delay(delay_target, DEFAULT_DELAY_TIMEOUT_MS)
        .ok();
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

fn test_batch(
    controller: &Controller,
    nodes: Vec<String>,
    concurrency: usize,
    timeout_ms: u64,
    progress: Option<ProgressBar>,
) -> Result<Option<DelayResult>> {
    let controller = Arc::new(controller.clone());
    let queue = Arc::new(Mutex::new(nodes.into_iter()));
    let progress = progress.map(Arc::new);
    let (tx, rx) = mpsc::channel();
    let worker_count = min(concurrency, queue.lock().unwrap().len()).max(1);
    for _ in 0..worker_count {
        let controller = Arc::clone(&controller);
        let queue = Arc::clone(&queue);
        let tx = tx.clone();
        let progress = progress.clone();
        thread::spawn(move || loop {
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
            if let Some(progress) = &progress {
                progress.inc(1);
            }
        });
    }
    drop(tx);
    Ok(rx.into_iter().min_by_key(|result| result.delay))
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
    let status = collect_status(paths, app_config).ok();
    let message = if error.to_string().contains("已取消") {
        "错误：已取消".to_string()
    } else {
        format!("错误：{error:#}")
    };
    println!(
        "{}",
        error_line(&message, status.as_ref(), &app_config.prompt)
    );
    Ok(())
}

fn progress_bar(label: &str, len: u64) -> ProgressBar {
    let progress = ProgressBar::new(len);
    let style = ProgressStyle::with_template("{prefix:.bold} {bar:36.cyan/blue} {pos}/{len}")
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("━━╾─");
    progress.set_style(style);
    progress.set_prefix(format!("节点延迟测试：{label}"));
    progress
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
    let updated = ensure_prompt_defaults(&input);
    if updated != input {
        fs::write(&paths.app_config, updated)
            .with_context(|| format!("无法写入 {}", paths.app_config.display()))?;
    }
    Ok(InstallAction::Updated)
}

pub fn ensure_prompt_defaults(input: &str) -> String {
    if input
        .lines()
        .any(|line| line.trim_start().starts_with("prompt:"))
    {
        return input.to_string();
    }
    let mut output = input.to_string();
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(
        r#"prompt:
  mode_icon: "󰒓"
  group_icon: "󰓹"
  node_icon: "󰤨"
  delay_icon: "󱎫"
  port_icon: "󰍍"
"#,
    );
    output
}

fn write_active_group(paths: &Paths, group: &str) -> Result<()> {
    fs::create_dir_all(&paths.app_config_dir)?;
    let mut config = read_app_config(paths).unwrap_or_default();
    config.active_group = Some(group.to_string());
    let filter = config.filter.unwrap_or_default();
    let input = format!(
        "active_group: {}\nfilter: {}\nconcurrency: {}\ntimeout_ms: {}\n",
        serde_yaml::to_string(group)?.trim(),
        serde_yaml::to_string(&filter)?.trim(),
        config.concurrency.unwrap_or(DEFAULT_CONCURRENCY),
        config.timeout_ms.unwrap_or(DEFAULT_DELAY_TIMEOUT_MS)
    );
    fs::write(&paths.app_config, input)?;
    Ok(())
}

fn write_completion_file(paths: &Paths) -> Result<InstallAction> {
    let action = if paths.completion_file.exists() {
        InstallAction::Updated
    } else {
        InstallAction::Set
    };
    fs::create_dir_all(&paths.completion_dir)?;
    fs::write(
        &paths.completion_file,
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
        node) _arguments '--filter=[按关键字预筛节点]' ;;
        auto-node) _arguments '--filter=[限制测速范围，逗号分隔]' ;;
      esac
      ;;
  esac
}

_verge-proxy "$@"
"#,
    )?;
    Ok(action)
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
    )
}

fn default_config_file() -> &'static str {
    r#"active_group: "🔰 手动选择"
filter: ""
concurrency: 20
timeout_ms: 2000
prompt:
  mode_icon: "󰒓"
  group_icon: "󰓹"
  node_icon: "󰤨"
  delay_icon: "󱎫"
  port_icon: "󰍍"
"#
}

pub fn zsh_wrapper_block(exe: &str) -> String {
    format!(
        r#"{BLOCK_BEGIN}
# verge-proxy wrapper (added by verge-proxy install)
verge-proxy() {{
  case "$1" in
    start|stop|restart) eval "$("{exe}" "$@")" ;;
    *) "{exe}" "$@" ;;
  esac
}}
vp() {{
  (
    eval "$("{exe}" restart)" >&2 || exit
    if [[ -n ${{aliases[$1]}} ]]; then
      eval "${{aliases[$1]}} ${{(j: :)${{(@q)@[2,-1]}}}}"
    else
      "$@"
    fi
  )
}}
if [[ -d "$HOME/.config/verge-proxy/completions" ]]; then
  fpath=("$HOME/.config/verge-proxy/completions" $fpath)
fi
autoload -Uz compinit
compinit
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
            resolve_active_group_and_node(&Mode::Rule, None, &proxies),
            ("🔰 手动选择".to_string(), "日本 A19".to_string())
        );
    }

    #[test]
    fn direct_mode_status_is_direct_even_if_selectors_have_other_state() {
        let proxies = proxies_from_pairs(&[
            ("GLOBAL", "🔰 手动选择", &["DIRECT", "🔰 手动选择"]),
            ("🔰 手动选择", "日本 A19", &["日本 A19"]),
        ]);
        assert_eq!(
            resolve_active_group_and_node(&Mode::Direct, Some("🔰 手动选择"), &proxies),
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
            block.contains(r#"start|stop|restart) eval "$("/usr/local/bin/verge-proxy" "$@")" ;;"#)
        );
        assert!(block.contains(r#"eval "$("/usr/local/bin/verge-proxy" restart)" >&2 || exit"#));
    }

    #[test]
    fn status_prompt_is_single_line_without_field_names() {
        let info = StatusInfo {
            mode: Mode::Rule,
            group: "🔰 手动选择".to_string(),
            node: "日本 A19".to_string(),
            delay: Some(108),
            port: 7897,
        };
        let prompt = format_status_prompt(&info, &PromptConfig::default(), 240, 0);
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
            delay: Some(108),
            port: 7897,
        };
        let prompt = format_status_prompt(&info, &PromptConfig::default(), 24, 0);
        assert!(prompt.contains('\n'));
        for line in prompt.lines() {
            assert!(line.contains(''));
            assert!(line.contains(''));
        }
    }

    #[test]
    fn success_and_error_lines_use_bold_colored_prefixes() {
        let info = StatusInfo {
            mode: Mode::Direct,
            group: "DIRECT".to_string(),
            node: "DIRECT".to_string(),
            delay: None,
            port: 7897,
        };
        let prompt = PromptConfig::default();
        assert!(success_line("设置直连成功", Some(&info), &prompt).starts_with(ANSI_BOLD_GREEN));
        assert!(error_line("错误：已取消", Some(&info), &prompt).starts_with(ANSI_BOLD_RED));
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
        let updated = ensure_prompt_defaults(existing);
        assert!(updated.contains("active_group: 🔰 手动选择"));
        assert!(updated.contains("prompt:"));
        assert!(updated.contains("mode_icon"));

        let custom = "prompt:\n  mode_icon: X\n";
        assert_eq!(ensure_prompt_defaults(custom), custom);
    }
}
