use screenshot_rs::app::AppState;
use screenshot_rs::error::AppResult;
use tracing_subscriber::EnvFilter;

// 全局 panic hook：让 GPUI 线程 crash 时 stack trace 不再静默丢失。
// 之前 Text 工具触发 blur → panic → 覆盖窗口闪退，看不到任何信息。
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("\n========== PANIC ==========");
        eprintln!("{}", info);
        if let Some(loc) = info.location() {
            eprintln!("at {}:{}", loc.file(), loc.line());
        }
        eprintln!("Backtrace:\n{}", std::backtrace::Backtrace::capture());
        eprintln!("============================\n");
    }));
}

fn main() -> AppResult<()> {
    install_panic_hook();

    // tracing subscriber 必须最先初始化，否则后续 tracing::info! 调用会被丢弃。
    // EnvFilter 默认从 RUST_LOG 读取；若未设置，会得到一个空 filter（拦截所有日志），
    // 所以必须提供默认 `info` 级别作为回退。
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Linux 托盘依赖 GTK（tray-icon 内部用 muda，muda 内部用 gtk-rs）。
    // tray-icon 不会自动调用 gtk::init()，所以我们必须先初始化。
    // Windows / macOS 不需要。
    #[cfg(target_os = "linux")]
    {
        gtk::init().expect("Failed to initialize GTK");
        tracing::info!("GTK 初始化完成");
    }

    let state = AppState::new()?;
    tracing::info!("服务启动完成，等待 alt+s 热键或托盘菜单事件...");
    state.run()
}