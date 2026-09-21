mod daemon;
mod gateway_client;
mod state;
mod views;
mod zed_settings;

use anyhow::{Context, Result};
use gpui::{App, AppContext as _, Application, Bounds, WindowBounds, WindowOptions, px, size};
use gpui_component::{Root, init as init_components};
use state::{AppServices, AppState};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .without_time()
        .init();
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .thread_name("agent-gateway-app")
            .build()
            .context("cannot create the async runtime")?,
    );
    let services = Arc::new(AppServices::discover(runtime)?);
    Application::new().run(move |cx: &mut App| {
        init_components(cx);
        let bounds = Bounds::centered(None, size(px(1360.), px(900.)), cx);
        let services = Arc::clone(&services);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |window, cx| {
                let state = cx.new(|cx| AppState::new(Arc::clone(&services), window, cx));
                state.update(cx, |state: &mut AppState, cx| state.start(window, cx));
                cx.new(|cx| Root::new(state, window, cx))
            },
        )
        .expect("cannot open the main window");
        cx.activate(true);
    });
    Ok(())
}
