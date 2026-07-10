use screenshot_rs::app::AppState;
use screenshot_rs::error::AppResult;

fn main() -> AppResult<()> {
    tracing_subscriber::fmt::init();
    println!("screenshot-rs starting...");
    let _state = AppState::new()?;
    Ok(())
}
