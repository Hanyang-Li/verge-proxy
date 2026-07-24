fn main() {
    // run() 已按各命令的约定渲染并退出；这里只兜底极早期失败（如无法定位 HOME）。
    if let Err(error) = verge_proxy::run() {
        eprintln!(
            "{}",
            verge_proxy::error_line(
                &format!("错误：{error:#}"),
                None,
                &verge_proxy::PromptConfig::default(),
                &verge_proxy::TagDefaults::default(),
            )
        );
        std::process::exit(1);
    }
}
