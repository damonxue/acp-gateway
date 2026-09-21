//! A small, native macOS status bar controller for Agent Gateway.
//!
//! The menu is deliberately AppKit based: it starts quickly, has no window or
//! UI runtime, and reads the gateway's local HTTP API whenever it opens.

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::BTreeMap;
    use std::ffi::CStr;
    use std::net::TcpStream;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use cocoa::appkit::{
        NSApp, NSApplication, NSApplicationActivationPolicyAccessory, NSButton, NSMenu, NSMenuItem,
        NSOpenPanel, NSSavePanel as _, NSStatusBar, NSStatusItem as _, NSVariableStatusItemLength,
    };
    use cocoa::base::{YES, id, nil};
    use cocoa::foundation::{NSAutoreleasePool, NSPoint, NSRect, NSRectEdge, NSSize, NSString};
    use objc::declare::ClassDecl;
    use objc::runtime::{Class, NO, Object, Sel};
    use objc::{class, msg_send, sel, sel_impl};
    use qrcode::QrCode;
    use qrcode::render::unicode;
    use serde::Deserialize;

    #[derive(Clone, Debug, Default, Deserialize)]
    struct GatewaySnapshot {
        #[serde(default)]
        health: Option<Health>,
        #[serde(default)]
        sessions: Vec<SessionInfo>,
        #[serde(default)]
        session_error: Option<String>,
        #[serde(default)]
        ahp: Option<AhpResponse>,
        #[serde(default)]
        ahp_error: Option<String>,
        #[serde(default)]
        error: Option<String>,
    }

    #[derive(Clone, Debug, Default, Deserialize)]
    struct Health {
        #[serde(default)]
        pid: Option<u32>,
        #[serde(default)]
        sessions: SessionCounts,
        #[serde(default)]
        components: Vec<Component>,
    }

    #[derive(Clone, Debug, Default, Deserialize)]
    struct SessionCounts {
        active: usize,
        created_total: usize,
    }

    #[derive(Clone, Debug, Default, Deserialize)]
    struct Component {
        name: String,
        state: String,
        #[serde(default)]
        detail: Option<String>,
    }

    #[derive(Clone, Debug, Default, Deserialize)]
    struct SessionInfo {
        id: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        workspace: String,
        #[serde(default)]
        cwd: String,
        #[serde(default)]
        agent_name: String,
        #[serde(default)]
        agent_id: String,
        #[serde(default)]
        acp_session_id: Option<String>,
        #[serde(default)]
        status: String,
        #[serde(default)]
        origin: String,
        #[serde(default)]
        last_seq: u64,
        #[serde(default)]
        updated_at: String,
    }

    #[derive(Clone, Debug, Default, Deserialize)]
    struct SessionList {
        #[serde(default)]
        sessions: Vec<SessionInfo>,
    }

    #[derive(Clone, Debug, Default, Deserialize)]
    struct AhpResponse {
        #[serde(default)]
        enabled: bool,
        #[serde(default)]
        status: Option<AhpInfo>,
    }

    #[derive(Clone, Debug, Default, Deserialize)]
    struct AhpInfo {
        #[serde(default)]
        state: String,
        #[serde(default)]
        qr_code: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        channel_id: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    }

    #[derive(Clone, Debug)]
    struct ConfiguredChannel {
        name: &'static str,
        enabled: bool,
        session_id: Option<String>,
        chat_id: Option<String>,
    }

    static OWNED_GATEWAY: OnceLock<Mutex<Option<Child>>> = OnceLock::new();
    static STATUS_BUTTON: OnceLock<usize> = OnceLock::new();
    static MENU_TARGET: OnceLock<usize> = OnceLock::new();
    static QR_POPOVER: OnceLock<Mutex<Option<usize>>> = OnceLock::new();

    fn owned_gateway() -> &'static Mutex<Option<Child>> {
        OWNED_GATEWAY.get_or_init(|| Mutex::new(None))
    }

    fn status_button() -> id {
        STATUS_BUTTON
            .get()
            .copied()
            .map(|value| value as id)
            .unwrap_or(nil)
    }

    fn qr_popover() -> &'static Mutex<Option<usize>> {
        QR_POPOVER.get_or_init(|| Mutex::new(None))
    }

    fn config_path() -> PathBuf {
        std::env::var_os("AGENT_GATEWAY_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(gateway_config::default_config_path)
    }

    fn base_url() -> String {
        gateway_config::GatewayConfig::load(&config_path())
            .map(|config| format!("http://{}", config.bind_addr()))
            .unwrap_or_else(|_| "http://127.0.0.1:48100".to_owned())
    }

    fn fetch_snapshot() -> GatewaySnapshot {
        let base = base_url();
        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                return GatewaySnapshot {
                    error: Some(error.to_string()),
                    ..Default::default()
                };
            }
        };
        let health = match client.get(format!("{base}/health")).send() {
            Ok(response) if response.status().is_success() => match response.json::<Health>() {
                Ok(value) => value,
                Err(error) => {
                    return GatewaySnapshot {
                        error: Some(error.to_string()),
                        ..Default::default()
                    };
                }
            },
            Ok(response) => {
                return GatewaySnapshot {
                    error: Some(format!("gateway returned {}", response.status())),
                    ..Default::default()
                };
            }
            Err(error) => {
                return GatewaySnapshot {
                    error: Some(format!("gateway offline: {error}")),
                    ..Default::default()
                };
            }
        };
        let (sessions, session_error) = match client.get(format!("{base}/sessions")).send() {
            Ok(response) if response.status().is_success() => {
                match response.json::<SessionList>() {
                    Ok(list) => (list.sessions, None),
                    Err(error) => (
                        Vec::new(),
                        Some(format!("cannot decode /sessions: {error}")),
                    ),
                }
            }
            Ok(response) => (
                Vec::new(),
                Some(format!("/sessions returned {}", response.status())),
            ),
            Err(error) => (Vec::new(), Some(format!("cannot read /sessions: {error}"))),
        };
        let (ahp, ahp_error) = match client.get(format!("{base}/ahp/status")).send() {
            Ok(response) if response.status().is_success() => {
                match response.json::<AhpResponse>() {
                    Ok(value) => (Some(value), None),
                    Err(error) => (None, Some(format!("cannot decode /ahp/status: {error}"))),
                }
            }
            Ok(response) => (
                None,
                Some(format!("/ahp/status returned {}", response.status())),
            ),
            Err(error) => (None, Some(format!("cannot read /ahp/status: {error}"))),
        };
        GatewaySnapshot {
            health: Some(health),
            sessions,
            session_error,
            ahp,
            ahp_error,
            error: None,
        }
    }

    fn configured_channels() -> Vec<ConfiguredChannel> {
        let Ok(config) = gateway_config::GatewayConfig::load(&config_path()) else {
            return Vec::new();
        };
        let mut channels = Vec::new();
        if let Some(section) = config.wechat {
            channels.push(ConfiguredChannel {
                name: "WeChat",
                enabled: section.enabled,
                session_id: section
                    .binding
                    .as_ref()
                    .map(|binding| binding.session_id.clone()),
                chat_id: section.binding.map(|binding| binding.chat_id),
            });
        }
        if let Some(section) = config.lark {
            channels.push(ConfiguredChannel {
                name: "Lark",
                enabled: section.enabled,
                session_id: section
                    .binding
                    .as_ref()
                    .map(|binding| binding.session_id.clone()),
                chat_id: section.binding.map(|binding| binding.chat_id),
            });
        }
        if let Some(section) = config.telegram {
            channels.push(ConfiguredChannel {
                name: "Telegram",
                enabled: section.enabled,
                session_id: section
                    .binding
                    .as_ref()
                    .map(|binding| binding.session_id.clone()),
                chat_id: section.binding.map(|binding| binding.chat_id),
            });
        }
        channels
    }

    fn post_json(path: &str, body: serde_json::Value) -> Result<(), String> {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(700))
            .build()
            .map_err(|error| error.to_string())?
            .post(format!("{}{path}", base_url()))
            .json(&body)
            .send()
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn selected_project_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        let path =
            PathBuf::from(home).join("Library/Application Support/Agent Gateway/project.path");
        std::fs::read_to_string(path)
            .ok()
            .map(|value| PathBuf::from(value.trim()))
            .filter(|path| path.is_dir())
    }

    fn daemon_binary() -> anyhow::Result<PathBuf> {
        if let Some(path) = std::env::current_exe()?
            .parent()
            .map(|dir| dir.join("agent-gateway-daemon"))
            .filter(|path| path.is_file())
        {
            return Ok(path);
        }
        match selected_project_path() {
            Some(project) => gateway_cli::zed::discover_gateway_binary_from_project(&project),
            None => gateway_cli::zed::discover_gateway_binary(),
        }
    }

    fn start_gateway_process() -> anyhow::Result<()> {
        let config = gateway_config::GatewayConfig::load(&config_path())?;
        if TcpStream::connect_timeout(&config.bind_addr(), Duration::from_millis(150)).is_ok() {
            return Ok(());
        }
        let mut owned = owned_gateway()
            .lock()
            .map_err(|_| anyhow::anyhow!("gateway process lock is poisoned"))?;
        if let Some(child) = owned.as_mut()
            && child.try_wait()?.is_none()
        {
            return Ok(());
        }
        *owned = Some(
            Command::new(daemon_binary()?)
                .args(["--config", &config_path().to_string_lossy(), "run"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        Ok(())
    }

    fn stop_gateway_process() {
        if let Ok(mut owned) = owned_gateway().lock()
            && let Some(mut child) = owned.take()
        {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        if let Some(pid) = fetch_snapshot().health.and_then(|health| health.pid) {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
    }

    fn copy_zed_settings() -> anyhow::Result<()> {
        let path = config_path();
        let config = gateway_config::GatewayConfig::load(&path)?;
        let rendered = gateway_cli::zed::render(&config, &path, None, None)?;
        gateway_cli::zed::copy_to_clipboard(&rendered)
    }

    fn item(title: &str, action: Sel, target: id) -> id {
        unsafe {
            let title = NSString::alloc(nil).init_str(title);
            let item = NSMenuItem::alloc(nil).initWithTitle_action_keyEquivalent_(
                title,
                action,
                NSString::alloc(nil).init_str(""),
            );
            NSMenuItem::setTarget_(item, target);
            item
        }
    }

    fn system_symbol(name: &str) -> id {
        unsafe {
            let symbol_name = NSString::alloc(nil).init_str(name);
            let accessibility = NSString::alloc(nil).init_str(name);
            let image: id = msg_send![
                class!(NSImage),
                imageWithSystemSymbolName: symbol_name
                accessibilityDescription: accessibility
            ];
            if image != nil {
                let _: () = msg_send![image, setTemplate: YES];
            }
            image
        }
    }

    fn set_menu_image(item: id, symbol: &str) {
        unsafe {
            let image = system_symbol(symbol);
            if image != nil {
                let _: () = msg_send![item, setImage: image];
            }
        }
    }

    fn disabled_item(title: &str, target: id) -> id {
        unsafe {
            let item = item(title, sel!(noop:), target);
            let _: () = msg_send![item, setEnabled: NO];
            item
        }
    }

    fn represented_item(title: &str, action: Sel, target: id, value: &str) -> id {
        unsafe {
            let item = item(title, action, target);
            let value = NSString::alloc(nil).init_str(value);
            let _: () = msg_send![item, setRepresentedObject: value];
            item
        }
    }

    fn clear_menu(menu: id) {
        unsafe {
            let _: () = msg_send![menu, removeAllItems];
        }
    }

    fn add(menu: id, item: id) {
        unsafe {
            NSMenu::addItem_(menu, item);
        }
    }

    fn separator() -> id {
        unsafe { NSMenuItem::separatorItem(nil) }
    }

    fn shortened(value: &str, max_chars: usize) -> String {
        let mut chars = value.chars();
        let shortened: String = chars.by_ref().take(max_chars).collect();
        if chars.next().is_some() {
            format!("{shortened}…")
        } else {
            shortened
        }
    }

    fn project_name(path: &str) -> &str {
        Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path)
    }

    fn bound_session_id(snapshot: &GatewaySnapshot) -> Option<String> {
        if let Some(id) = snapshot
            .ahp
            .as_ref()
            .and_then(|ahp| ahp.status.as_ref())
            .and_then(|status| status.session_id.as_deref())
        {
            return Some(id.to_owned());
        }
        snapshot
            .health
            .as_ref()
            .and_then(|health| {
                health
                    .components
                    .iter()
                    .find(|component| component.name == "wechat")
            })
            .and_then(|component| component.detail.as_deref())
            .and_then(|detail| detail.strip_prefix("session_id="))
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    }

    fn bound_session_summary(snapshot: &GatewaySnapshot) -> String {
        let Some(id) = bound_session_id(snapshot) else {
            return "Channels · WeChat 未绑定 session".to_owned();
        };
        let Some(session) = snapshot.sessions.iter().find(|session| session.id == id) else {
            return format!("Channels · bound session {}", shortened(&id, 18));
        };
        let title = session
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or("Untitled session");
        let project = if session.workspace.is_empty() {
            "unknown project"
        } else {
            project_name(&session.workspace)
        };
        format!(
            "Channels · ● {} / {} · {}",
            project,
            title,
            shortened(&id, 18)
        )
    }

    fn add_session_menu_item(
        session: &SessionInfo,
        target: id,
        bound_session: Option<&str>,
        parent_menu: id,
    ) {
        let title = session
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or("Untitled session");
        let is_bound = bound_session == Some(session.id.as_str());
        let status = if session.status.trim().is_empty() {
            String::new()
        } else {
            format!(" · {}", session.status)
        };
        let item = item(&format!("{title}{status}"), sel!(noop:), target);
        unsafe {
            if is_bound {
                let _: () = msg_send![item, setState: 1i64];
                set_menu_image(item, "checkmark.circle.fill");
            }
            let submenu = NSMenu::new(nil).autorelease();
            add(
                submenu,
                disabled_item(&format!("{} · {}", session.status, session.origin), target),
            );
            if !session.agent_name.is_empty() || !session.agent_id.is_empty() {
                let agent = if session.agent_name.is_empty() {
                    &session.agent_id
                } else {
                    &session.agent_name
                };
                add(submenu, disabled_item(&format!("Agent: {agent}"), target));
            }
            add(
                submenu,
                disabled_item(
                    &format!(
                        "Project: {}",
                        if session.workspace.is_empty() {
                            "unknown"
                        } else {
                            &session.workspace
                        }
                    ),
                    target,
                ),
            );
            if !session.cwd.is_empty() && session.cwd != session.workspace {
                add(
                    submenu,
                    disabled_item(&format!("Working directory: {}", session.cwd), target),
                );
            }
            add(
                submenu,
                disabled_item(
                    &format!("Gateway session: {}", shortened(&session.id, 24)),
                    target,
                ),
            );
            if let Some(acp_session_id) = session.acp_session_id.as_deref() {
                add(
                    submenu,
                    disabled_item(
                        &format!("ACP session: {}", shortened(acp_session_id, 24)),
                        target,
                    ),
                );
            }
            add(
                submenu,
                disabled_item(
                    &format!(
                        "Events: {} · updated {}",
                        session.last_seq,
                        if session.updated_at.is_empty() {
                            "unknown"
                        } else {
                            &session.updated_at
                        }
                    ),
                    target,
                ),
            );
            add(submenu, separator());
            add(
                submenu,
                represented_item(
                    "Bind WeChat to this session",
                    sel!(bindSession:),
                    target,
                    &session.id,
                ),
            );
            add(parent_menu, item);
            let _: () = msg_send![parent_menu, setSubmenu: submenu forItem: item];
        }
    }

    fn add_project_group(
        parent_menu: id,
        workspace: &str,
        sessions: &[&SessionInfo],
        target: id,
        bound_session: Option<&str>,
    ) {
        let project = if workspace.is_empty() {
            "unknown project"
        } else {
            project_name(workspace)
        };
        let bound = sessions
            .iter()
            .any(|session| bound_session == Some(session.id.as_str()));
        let group_item = item(
            &format!(
                "{project} · {} session{}",
                sessions.len(),
                if sessions.len() == 1 { "" } else { "s" }
            ),
            sel!(noop:),
            target,
        );
        set_menu_image(group_item, if bound { "folder.fill" } else { "folder" });

        unsafe {
            let submenu = NSMenu::new(nil).autorelease();
            if workspace.is_empty() {
                add(submenu, disabled_item("Workspace unavailable", target));
            } else {
                add(submenu, disabled_item(workspace, target));
            }
            add(submenu, separator());
            for session in sessions {
                add_session_menu_item(session, target, bound_session, submenu);
            }
            add(parent_menu, group_item);
            let _: () = msg_send![parent_menu, setSubmenu: submenu forItem: group_item];
        }
    }

    fn refresh_menu(menu: id) {
        let _pool = unsafe { NSAutoreleasePool::new(nil) };
        let target: id = unsafe { msg_send![menu, delegate] };
        if target == nil {
            return;
        }
        let snapshot = fetch_snapshot();
        clear_menu(menu);
        let online = snapshot.health.is_some();
        let gateway_title = if online {
            "● Gateway online"
        } else {
            "○ Gateway offline"
        };
        add(menu, disabled_item(gateway_title, target));
        if let Some(error) = snapshot.error.as_deref() {
            add(menu, disabled_item(&format!("  ⚠ {error}"), target));
        }
        if let Some(health) = snapshot.health.as_ref() {
            add(
                menu,
                disabled_item(
                    &format!(
                        "  Sessions: active {} / total {}",
                        health.sessions.active, health.sessions.created_total
                    ),
                    target,
                ),
            );
        }
        let gateway_action = if online {
            "■ Stop gateway"
        } else {
            "▶ Start gateway"
        };
        add(menu, item(gateway_action, sel!(toggleGateway:), target));
        add(menu, item("↻ Refresh", sel!(refresh:), target));
        add(
            menu,
            item("Open local dashboard", sel!(openDashboard:), target),
        );
        add(menu, separator());
        add(
            menu,
            disabled_item(&format!("Sessions ({})", snapshot.sessions.len()), target),
        );
        if let Some(error) = snapshot.session_error.as_deref() {
            add(menu, disabled_item(&format!("  ⚠ {error}"), target));
        } else if snapshot.sessions.is_empty() {
            add(menu, disabled_item("  No sessions found", target));
        } else {
            let bound_session = bound_session_id(&snapshot);
            let mut projects: BTreeMap<String, Vec<&SessionInfo>> = BTreeMap::new();
            for session in &snapshot.sessions {
                projects
                    .entry(session.workspace.clone())
                    .or_default()
                    .push(session);
            }
            for (workspace, sessions) in projects {
                add_project_group(
                    menu,
                    &workspace,
                    &sessions,
                    target,
                    bound_session.as_deref(),
                );
            }
        }
        add(menu, separator());
        add(
            menu,
            disabled_item(&bound_session_summary(&snapshot), target),
        );
        add_channels(menu, target, &snapshot);
        add(
            menu,
            item("Show phone pairing QR…", sel!(showPairingQr:), target),
        );
        if let Some(error) = snapshot.ahp_error.as_deref() {
            add(menu, disabled_item(&format!("  ⚠ {error}"), target));
        }
        add(menu, separator());
        add(menu, item("Copy Zed agent_servers", sel!(copyZed:), target));
        add(
            menu,
            item(
                "Choose gateway project…",
                sel!(chooseGatewayProject:),
                target,
            ),
        );
        add(menu, separator());
        add(menu, item("⏻ Agent Gateway", sel!(quit:), target));
    }

    fn add_channels(menu: id, target: id, snapshot: &GatewaySnapshot) {
        if let Some(ahp) = snapshot.ahp.as_ref() {
            let status = ahp.status.as_ref();
            let state = status
                .map(|status| status.state.as_str())
                .unwrap_or("disabled");
            let session = status
                .and_then(|status| status.session_id.as_ref())
                .map(|id| format!(" · session {}", shortened(id, 18)))
                .unwrap_or_default();
            let channel = status
                .and_then(|status| status.channel_id.as_ref())
                .map(|id| format!(" · channel {}", shortened(id, 18)))
                .unwrap_or_default();
            let reason = status
                .and_then(|status| status.reason.as_ref())
                .map(|reason| format!(" · {reason}"))
                .unwrap_or_default();
            add(
                menu,
                disabled_item(
                    &format!("  WeChat/AHP · {state}{session}{channel}{reason}"),
                    target,
                ),
            );
            if let Some(status) = status
                && let Some(qr) = status.qr_code.as_deref()
            {
                add(
                    menu,
                    represented_item("  Show WeChat QR…", sel!(showQr:), target, qr),
                );
            }
            if let Some(status) = status
                && status.session_id.is_some()
            {
                add(menu, item("  Unbind WeChat", sel!(unbindSession:), target));
            }
            if !ahp.enabled {
                add(menu, disabled_item("  AHP is not configured", target));
            }
        }
        if let Some(health) = snapshot.health.as_ref() {
            for component in &health.components {
                if matches!(component.name.as_str(), "wechat" | "lark" | "telegram") {
                    let detail = component
                        .detail
                        .as_deref()
                        .map(|detail| format!(" · {detail}"))
                        .unwrap_or_default();
                    add(
                        menu,
                        disabled_item(
                            &format!("  {} · {}{}", component.name, component.state, detail),
                            target,
                        ),
                    );
                }
            }
        }
        let current_bound_session = bound_session_id(snapshot);
        for channel in configured_channels() {
            let enabled = if channel.enabled {
                "enabled"
            } else {
                "disabled"
            };
            let channel_session_id = if channel.name == "WeChat" {
                current_bound_session
                    .as_deref()
                    .or(channel.session_id.as_deref())
            } else {
                channel.session_id.as_deref()
            };
            let binding = channel_session_id
                .as_deref()
                .map(|session| format!(" · session {}", shortened(session, 18)))
                .unwrap_or_default();
            let chat = channel
                .chat_id
                .as_deref()
                .map(|chat| format!(" · chat {}", shortened(chat, 18)))
                .unwrap_or_default();
            add(
                menu,
                disabled_item(
                    &format!("  {} · {}{}{}", channel.name, enabled, binding, chat),
                    target,
                ),
            );
        }
    }

    fn refresh_sender(sender: id) {
        unsafe {
            let mut menu: id = msg_send![sender, menu];
            while menu != nil {
                let parent: id = msg_send![menu, supermenu];
                if parent == nil {
                    break;
                }
                menu = parent;
            }
            if menu != nil {
                refresh_menu(menu);
            }
        }
    }

    extern "C" fn noop(_: &Object, _: Sel, _: id) {}

    extern "C" fn menu_will_open(_: &Object, _: Sel, menu: id) {
        refresh_menu(menu);
    }

    extern "C" fn toggle_gateway(_: &Object, _: Sel, sender: id) {
        if fetch_snapshot().health.is_some() {
            stop_gateway_process();
            for _ in 0..10 {
                if fetch_snapshot().health.is_none() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        } else if let Err(error) = start_gateway_process() {
            eprintln!("cannot start gateway: {error:#}");
        } else {
            for _ in 0..10 {
                if fetch_snapshot().health.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        refresh_sender(sender);
    }

    extern "C" fn refresh(_: &Object, _: Sel, sender: id) {
        refresh_sender(sender);
    }

    extern "C" fn bind_session(_: &Object, _: Sel, sender: id) {
        unsafe {
            let value: id = msg_send![sender, representedObject];
            if value == nil {
                return;
            }
            let session_id = CStr::from_ptr(NSString::UTF8String(value))
                .to_string_lossy()
                .into_owned();
            if let Err(error) = post_json(
                "/wechat/bind",
                serde_json::json!({"session_id": session_id}),
            ) {
                eprintln!("cannot bind WeChat: {error}");
            }
            refresh_sender(sender);
        }
    }

    extern "C" fn unbind_session(_: &Object, _: Sel, sender: id) {
        if let Err(error) = post_json("/wechat/unbind", serde_json::json!({})) {
            eprintln!("cannot unbind WeChat: {error}");
        }
        refresh_sender(sender);
    }

    fn show_qr_popover(title: &str, payload: &str) {
        let rendered = QrCode::new(payload.as_bytes())
            .ok()
            .map(|code| code.render::<unicode::Dense1x2>().quiet_zone(true).build())
            .unwrap_or_else(|| payload.to_owned());
        let content = format!("{title}\n\n{rendered}\n\n二维码内容可选中复制");

        unsafe {
            // Keep the QR in an editable-looking, fixed-width text surface so
            // whitespace is preserved and the payload can be copied.
            let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(480.0, 600.0));
            let text_view: id = msg_send![class!(NSTextView), alloc];
            let text_view_frame = NSRect::new(
                NSPoint::new(0.0, 48.0),
                NSSize::new(frame.size.width, frame.size.height - 48.0),
            );
            let text_view: id = msg_send![text_view, initWithFrame: text_view_frame];
            let _: () = msg_send![text_view, setString: NSString::alloc(nil).init_str(&content)];
            let _: () = msg_send![text_view, setEditable: NO];
            let _: () = msg_send![text_view, setSelectable: YES];
            let _: () = msg_send![text_view, setDrawsBackground: NO];
            let _: () = msg_send![text_view, setTextContainerInset: NSSize::new(16.0, 16.0)];
            let font: id = msg_send![
                class!(NSFont),
                monospacedSystemFontOfSize: 8.0f64
                weight: 0.0f64
            ];
            if font != nil {
                let _: () = msg_send![text_view, setFont: font];
            }

            let container: id = msg_send![class!(NSView), alloc];
            let container: id = msg_send![container, initWithFrame: frame];
            let _: () = msg_send![container, addSubview: text_view];

            let close_button_frame = NSRect::new(NSPoint::new(16.0, 10.0), NSSize::new(72.0, 28.0));
            let close_button: id = msg_send![class!(NSButton), alloc];
            let close_button: id = msg_send![close_button, initWithFrame: close_button_frame];
            let _: () = msg_send![close_button, setTitle: NSString::alloc(nil).init_str("关闭")];
            let _: () = msg_send![close_button, setBezelStyle: 1i64];
            if let Some(target) = MENU_TARGET.get().copied() {
                let target = target as id;
                let _: () = msg_send![close_button, setTarget: target];
                let _: () = msg_send![close_button, setAction: sel!(closeQrPopover:)];
            }
            let _: () = msg_send![container, addSubview: close_button];

            let controller: id = msg_send![class!(NSViewController), new];
            let _: () = msg_send![controller, setView: container];
            let popover: id = msg_send![class!(NSPopover), new];
            if popover == nil {
                return;
            }
            let _: () = msg_send![popover, setContentViewController: controller];
            // Keep the popover open after the menu closes, but dismiss it when
            // the user leaves the app.
            let _: () = msg_send![popover, setBehavior: 2u64];
            let _: () = msg_send![popover, setAnimates: YES];
            let _: () = msg_send![popover, setContentSize: NSSize::new(480.0, 600.0)];
            if let Ok(mut current) = qr_popover().lock() {
                *current = Some(popover as usize);
            }

            // Keep the popover anchored to the actual status-bar button. This
            // remains valid after the menu dismisses, so the QR stays beside
            // the menu icon instead of becoming a standalone alert window.
            let anchor = status_button();
            if anchor != nil {
                let bounds: NSRect = msg_send![anchor, bounds];
                let _: () = msg_send![
                    popover,
                    showRelativeToRect: bounds
                    ofView: anchor
                    preferredEdge: NSRectEdge::NSRectMaxYEdge
                ];
            }
        }
    }

    extern "C" fn close_qr_popover(_: &Object, _: Sel, _: id) {
        let popover = qr_popover()
            .lock()
            .ok()
            .and_then(|mut current| current.take())
            .map(|value| value as id)
            .unwrap_or(nil);
        if popover != nil {
            unsafe {
                let _: () = msg_send![popover, performClose: nil];
            }
        }
    }

    extern "C" fn show_qr(_: &Object, _: Sel, sender: id) {
        unsafe {
            let value: id = msg_send![sender, representedObject];
            if value == nil {
                return;
            }
            let payload = CStr::from_ptr(NSString::UTF8String(value)).to_string_lossy();
            show_qr_popover("Scan WeChat QR code", &payload);
        }
    }

    extern "C" fn show_pairing_qr(_: &Object, _: Sel, _sender: id) {
        let result = (|| -> Result<String, String> {
            let response = reqwest::blocking::Client::builder()
                .timeout(Duration::from_millis(700))
                .build()
                .map_err(|error| error.to_string())?
                .post(format!("{}/pairing/begin", base_url()))
                .json(&serde_json::json!({}))
                .send()
                .map_err(|error| error.to_string())?
                .error_for_status()
                .map_err(|error| error.to_string())?;
            let offer: serde_json::Value = response.json().map_err(|error| error.to_string())?;
            serde_json::to_string(&offer).map_err(|error| error.to_string())
        })();
        match result {
            Ok(payload) => show_qr_popover("Pair a phone or browser", &payload),
            Err(error) => show_qr_popover(
                "Pairing QR unavailable",
                &format!("Unable to create pairing QR:\n{error}"),
            ),
        }
    }

    extern "C" fn copy_zed(_: &Object, _: Sel, _: id) {
        if let Err(error) = copy_zed_settings() {
            eprintln!("cannot copy Zed agent_servers: {error:#}");
        }
    }

    extern "C" fn choose_gateway_project(_: &Object, _: Sel, _: id) {
        unsafe {
            let panel = NSOpenPanel::openPanel(nil);
            panel.setCanChooseFiles_(NO);
            panel.setCanChooseDirectories_(YES);
            panel.setAllowsMultipleSelection_(NO);
            let _: () = msg_send![panel, setTitle: NSString::alloc(nil).init_str("Choose acp-gw project directory")];
            if panel.runModal() as i64 != 1 {
                return;
            }
            let url = panel.URL();
            if url == nil {
                return;
            }
            let path: id = msg_send![url, path];
            let selected = CStr::from_ptr(NSString::UTF8String(path))
                .to_string_lossy()
                .into_owned();
            if !PathBuf::from(&selected).join("Cargo.toml").is_file() {
                eprintln!("selected directory has no Cargo.toml: {selected}");
                return;
            }
            if let Some(home) = std::env::var_os("HOME") {
                let path = PathBuf::from(home)
                    .join("Library/Application Support/Agent Gateway/project.path");
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(path, selected);
            }
        }
    }

    extern "C" fn quit(_: &Object, _: Sel, _: id) {
        stop_gateway_process();
        unsafe {
            let _: () = msg_send![NSApp(), terminate: nil];
        }
    }

    fn target_class() -> &'static Class {
        let mut declaration =
            ClassDecl::new("AgentGatewayMenuTarget", class!(NSObject)).expect("menu target class");
        unsafe {
            declaration.add_method(sel!(noop:), noop as extern "C" fn(&Object, Sel, id));
            declaration.add_method(
                sel!(closeQrPopover:),
                close_qr_popover as extern "C" fn(&Object, Sel, id),
            );
            declaration.add_method(
                sel!(menuWillOpen:),
                menu_will_open as extern "C" fn(&Object, Sel, id),
            );
            declaration.add_method(
                sel!(toggleGateway:),
                toggle_gateway as extern "C" fn(&Object, Sel, id),
            );
            declaration.add_method(sel!(refresh:), refresh as extern "C" fn(&Object, Sel, id));
            declaration.add_method(
                sel!(bindSession:),
                bind_session as extern "C" fn(&Object, Sel, id),
            );
            declaration.add_method(
                sel!(unbindSession:),
                unbind_session as extern "C" fn(&Object, Sel, id),
            );
            declaration.add_method(sel!(showQr:), show_qr as extern "C" fn(&Object, Sel, id));
            declaration.add_method(
                sel!(showPairingQr:),
                show_pairing_qr as extern "C" fn(&Object, Sel, id),
            );
            declaration.add_method(sel!(copyZed:), copy_zed as extern "C" fn(&Object, Sel, id));
            declaration.add_method(
                sel!(chooseGatewayProject:),
                choose_gateway_project as extern "C" fn(&Object, Sel, id),
            );
            declaration.add_method(sel!(quit:), quit as extern "C" fn(&Object, Sel, id));
        }
        declaration.register()
    }

    #[cfg(test)]
    mod tests {
        use super::SessionList;

        #[test]
        fn decodes_gateway_sessions_with_project_and_title() {
            let list: SessionList = serde_json::from_value(serde_json::json!({
                "sessions": [{
                    "id": "sess_1",
                    "agent_id": "codex",
                    "agent_name": "Codex",
                    "workspace": "/Users/test/project",
                    "cwd": "/Users/test/project/src",
                    "title": "Fix login",
                    "origin": "ide_bridge",
                    "status": "idle",
                    "last_seq": 12,
                    "updated_at": "2026-09-21T10:00:00Z"
                }]
            }))
            .expect("session list should match the gateway response");
            let session = &list.sessions[0];
            assert_eq!(session.title.as_deref(), Some("Fix login"));
            assert_eq!(session.workspace, "/Users/test/project");
            assert_eq!(session.cwd, "/Users/test/project/src");
            assert_eq!(session.status, "idle");
        }
    }

    pub fn run() {
        unsafe {
            let _pool = NSAutoreleasePool::new(nil);
            let app = NSApp();
            app.setActivationPolicy_(NSApplicationActivationPolicyAccessory);
            let target: id = msg_send![target_class(), new];
            let _ = MENU_TARGET.set(target as usize);
            let status: id =
                NSStatusBar::systemStatusBar(nil).statusItemWithLength_(NSVariableStatusItemLength);
            // `NSStatusItem` owns the slot, but the actual image is set on its
            // `NSStatusBarButton`. `imageWithSystemSymbolName:...` is not
            // exposed by cocoa 0.25, so call the AppKit selector directly.
            let symbol_name = NSString::alloc(nil).init_str("network");
            let accessibility = NSString::alloc(nil).init_str("Agent Gateway");
            let symbol_image: id = msg_send![
                class!(NSImage),
                imageWithSystemSymbolName: symbol_name
                accessibilityDescription: accessibility
            ];
            let button = status.button();
            let _ = STATUS_BUTTON.set(button as usize);
            if symbol_image != nil {
                let _: () = msg_send![symbol_image, setTemplate: YES];
                button.setImage_(symbol_image);
            } else {
                // Older macOS versions may not have SF Symbols available.
                button.setTitle_(NSString::alloc(nil).init_str("AG"));
            }
            let menu = NSMenu::new(nil).autorelease();
            let _: () = msg_send![menu, setDelegate: target];
            refresh_menu(menu);
            status.setMenu_(menu);
            app.run();
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    macos::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("agent-gateway-app is a macOS status bar helper");
}
