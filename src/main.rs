fn main() {
    if let Err(error) = verge_proxy::run() {
        eprintln!("错误: {error:#}");
        std::process::exit(1);
    }
}
