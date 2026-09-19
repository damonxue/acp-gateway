//! Tracing setup.
//!
//! The daemon and the one-shot commands want different things: the daemon logs
//! structured lines with timestamps and targets, while a command the user just
//! typed should print warnings and nothing else, so its actual output is not
//! buried.

use tracing_subscriber::EnvFilter;

/// Full logging for the long-running daemon.
pub(crate) fn init_daemon(filter: &str) {
    // An empty AGENT_GATEWAY_LOG is commonly exported by shell setup files.
    // `try_from_default_env` accepts it as an empty filter, which silently
    // disables every log line and makes a running daemon look hung. Treat an
    // empty value as unset so the configured/default level still applies.
    let filter = std::env::var("AGENT_GATEWAY_LOG")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| EnvFilter::try_new(value).ok())
        .or_else(|| EnvFilter::try_new(filter).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}

/// Quiet logging for one-shot commands.
pub(crate) fn init_cli(filter: Option<&str>) {
    let filter = std::env::var("AGENT_GATEWAY_LOG")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| EnvFilter::try_new(value).ok())
        .or_else(|| EnvFilter::try_new(filter.unwrap_or("warn")).ok())
        .unwrap_or_else(|| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .without_time()
        .with_target(false)
        .init();
}
