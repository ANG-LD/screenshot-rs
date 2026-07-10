use screenshot_rs::app::AppState;
use screenshot_rs::error::AppResult;

fn main() -> AppResult<()> {
    tracing_subscriber::fmt::init();
    let state = AppState::new()?;
    state.run()
}
