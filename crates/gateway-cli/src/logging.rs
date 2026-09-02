//! Tracing setup.
//!
//! The daemon and the one-shot commands want different things: the daemon logs
//! structured lines with timestamps and targets, while a command the user just
//! typed should print warnings and nothing else, so its actual output is not
//! buried.

use tracing_subscriber::EnvFilter;

/// Full logging for the long-running daemon.
pub(crate) fn init_daemon(filter: &str) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(filter))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}

/// Quiet logging for one-shot commands.
pub(crate) fn init_cli(filter: Option<&str>) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(filter.unwrap_or("warn")))
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .without_time()
        .with_target(false)
        .init();
}
