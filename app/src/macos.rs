//! Native macOS status-bar companion implemented with the maintained objc2
//! bindings.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Cursor;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use image::{ImageFormat, Luma};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{
    AnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    sel,
};
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSControlStateValueOn, NSImage,
    NSImageView, NSMenu, NSMenuDelegate, NSMenuItem, NSOpenPanel, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSData, NSPoint, NSRect, NSSize, NSString};
use qrcode::QrCode;
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

fn owned_gateway() -> &'static Mutex<Option<Child>> {
    OWNED_GATEWAY.get_or_init(|| Mutex::new(None))
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
    let (health, error) = match client.get(format!("{base}/health")).send() {
        Ok(response) if response.status().is_success() => match response.json::<Health>() {
            Ok(value) => (Some(value), None),
            Err(error) => (None, Some(error.to_string())),
        },
        Ok(response) => (
            None,
            Some(format!("gateway returned {}", response.status())),
        ),
        Err(error) => (None, Some(format!("gateway offline: {error}"))),
    };
    let (sessions, session_error) = match client.get(format!("{base}/sessions")).send() {
        Ok(response) if response.status().is_success() => match response.json::<SessionList>() {
            Ok(list) => (list.sessions, None),
            Err(error) => (
                Vec::new(),
                Some(format!("cannot decode /sessions: {error}")),
            ),
        },
        Ok(response) => (
            Vec::new(),
            Some(format!("/sessions returned {}", response.status())),
        ),
        Err(error) => (Vec::new(), Some(format!("cannot read /sessions: {error}"))),
    };
    let (ahp, ahp_error) = match client.get(format!("{base}/ahp/status")).send() {
        Ok(response) if response.status().is_success() => match response.json::<AhpResponse>() {
            Ok(value) => (Some(value), None),
            Err(error) => (None, Some(format!("cannot decode /ahp/status: {error}"))),
        },
        Ok(response) => (
            None,
            Some(format!("/ahp/status returned {}", response.status())),
        ),
        Err(error) => (None, Some(format!("cannot read /ahp/status: {error}"))),
    };
    GatewaySnapshot {
        health,
        sessions,
        session_error,
        ahp,
        ahp_error,
        error,
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
    let path = PathBuf::from(home).join("Library/Application Support/Agent Gateway/project.path");
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
    snapshot
        .ahp
        .as_ref()
        .and_then(|ahp| ahp.status.as_ref())
        .and_then(|status| status.session_id.clone())
        .or_else(|| {
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
        })
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

struct MenuState {
    menu: RefCell<Option<Retained<NSMenu>>>,
}

impl Default for MenuState {
    fn default() -> Self {
        Self {
            menu: RefCell::new(None),
        }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = MenuState]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method_id(init))]
        fn init(this: Allocated<Self>) -> Retained<Self> {
            let this = this.set_ivars(MenuState::default());
            unsafe { msg_send![super(this), init] }
        }

        #[unsafe(method(noop:))]
        fn noop(&self, _sender: Option<&AnyObject>) {}

        #[unsafe(method(refresh:))]
        fn refresh(&self, _sender: Option<&AnyObject>) {
            if let Some(menu) = self.ivars().menu.borrow().as_ref() {
                // Replacing the items while AppKit is tracking the open menu
                // can leave the visible popup backed by the old item array.
                // Close that tracking pass first; the next opening then shows
                // the freshly fetched /sessions response.
                menu.cancelTrackingWithoutAnimation();
                self.rebuild_menu_with(menu);
            }
        }

        #[unsafe(method(toggleGateway:))]
        fn toggle_gateway(&self, _sender: Option<&AnyObject>) {
            if fetch_snapshot().health.is_some() {
                stop_gateway_process();
            } else if let Err(error) = start_gateway_process() {
                self.alert("Cannot start gateway", &error.to_string());
            }
            self.rebuild_menu();
        }

        #[unsafe(method(bindSession:))]
        fn bind_session(&self, sender: Option<&AnyObject>) {
            let Some(value) = sender
                .and_then(|sender| sender.downcast_ref::<NSMenuItem>())
                .and_then(|item| item.representedObject())
                .and_then(|value| value.downcast_ref::<NSString>().map(ToString::to_string))
            else {
                return;
            };
            if let Err(error) = post_json("/wechat/bind", serde_json::json!({"session_id": value})) {
                self.alert("Cannot bind WeChat", &error);
            }
            self.rebuild_menu();
        }

        #[unsafe(method(unbindSession:))]
        fn unbind_session(&self, _sender: Option<&AnyObject>) {
            if let Err(error) = post_json("/wechat/unbind", serde_json::json!({})) {
                self.alert("Cannot unbind WeChat", &error);
            }
            self.rebuild_menu();
        }

        #[unsafe(method(showQr:))]
        fn show_qr(&self, sender: Option<&AnyObject>) {
            let Some(payload) = sender
                .and_then(|sender| sender.downcast_ref::<NSMenuItem>())
                .and_then(|item| item.representedObject())
                .and_then(|value| value.downcast_ref::<NSString>().map(ToString::to_string))
            else {
                return;
            };
            self.show_qr_alert(
                "WeChat login QR",
                &payload,
                "Scan this QR in WeChat. The menu status will show whether the channel is ready and which session it is bound to; click Refresh after confirming the scan.",
            );
            self.rebuild_menu();
        }

        #[unsafe(method(showPairingQr:))]
        fn show_pairing_qr(&self, _sender: Option<&AnyObject>) {
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
                Ok(payload) => {
                    let details = serde_json::from_str::<serde_json::Value>(&payload)
                        .ok()
                        .map(|offer| {
                            format!(
                                "This is Agent Gateway phone/browser pairing, not WeChat login.\nPairing code: {}\nExpires: {}",
                                offer
                                    .get("pairing_code")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("unknown"),
                                offer
                                    .get("expires_at")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("unknown")
                            )
                        })
                        .unwrap_or_else(|| "This is Agent Gateway phone/browser pairing, not WeChat login.".to_owned());
                    self.show_qr_alert("Phone/browser pairing QR", &payload, &details);
                }
                Err(error) => self.alert("Pairing QR unavailable", &error),
            }
        }

        #[unsafe(method(openDashboard:))]
        fn open_dashboard(&self, _sender: Option<&AnyObject>) {
            let _ = Command::new("open").arg(format!("{}/app", base_url())).spawn();
        }

        #[unsafe(method(copyZed:))]
        fn copy_zed(&self, _sender: Option<&AnyObject>) {
            if let Err(error) = copy_zed_settings() {
                self.alert("Cannot copy Zed agent_servers", &error.to_string());
            }
        }

        #[unsafe(method(chooseGatewayProject:))]
        fn choose_gateway_project(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::new().expect("menu actions run on the main thread");
            let panel = NSOpenPanel::openPanel(mtm);
            panel.setCanChooseFiles(false);
            panel.setCanChooseDirectories(true);
            panel.setAllowsMultipleSelection(false);
            panel.setTitle(Some(&NSString::from_str("Choose acp-gw project directory")));
            if panel.runModal() != 1 {
                return;
            }
            let Some(url) = panel.URL() else { return };
            let Some(path) = url.path() else { return };
            let selected = path.to_string();
            if !PathBuf::from(&selected).join("Cargo.toml").is_file() {
                self.alert("Invalid project", "The selected directory has no Cargo.toml.");
                return;
            }
            if let Some(home) = std::env::var_os("HOME") {
                let path = PathBuf::from(home).join("Library/Application Support/Agent Gateway/project.path");
                if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
                let _ = std::fs::write(path, selected);
            }
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            stop_gateway_process();
            let mtm = MainThreadMarker::new().expect("menu actions run on the main thread");
            NSApplication::sharedApplication(mtm).terminate(None);
        }
    }

    unsafe impl NSObjectProtocol for MenuTarget {}

    unsafe impl NSMenuDelegate for MenuTarget {
        #[unsafe(method(menuWillOpen:))]
        #[allow(non_snake_case)]
        fn menuWillOpen(&self, menu: &NSMenu) {
            self.rebuild_menu_with(menu);
        }
    }
);

impl MenuTarget {
    fn any_object(&self) -> &AnyObject {
        self.as_super().as_super()
    }

    fn item(&self, title: &str, action: objc2::runtime::Sel) -> Retained<NSMenuItem> {
        let mtm = MainThreadMarker::new().expect("menu actions run on the main thread");
        let empty = NSString::from_str("");
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(action),
                &empty,
            )
        };
        unsafe {
            item.setTarget(Some(self.any_object()));
        }
        let _ = mtm;
        item
    }

    fn disabled_item(&self, menu: &NSMenu, title: &str) {
        let item = self.item(title, sel!(noop:));
        item.setEnabled(false);
        menu.addItem(&item);
    }

    fn separator(&self, menu: &NSMenu) {
        let mtm = MainThreadMarker::new().expect("menu actions run on the main thread");
        menu.addItem(&NSMenuItem::separatorItem(mtm));
    }

    fn action_item(&self, menu: &NSMenu, title: &str, action: objc2::runtime::Sel) {
        menu.addItem(&self.item(title, action));
    }

    fn represented_item(
        &self,
        menu: &NSMenu,
        title: &str,
        action: objc2::runtime::Sel,
        value: &str,
    ) {
        let item = self.item(title, action);
        let value = NSString::from_str(value);
        unsafe {
            item.setRepresentedObject(Some(value.as_super().as_super()));
        }
        menu.addItem(&item);
    }

    fn set_symbol(&self, item: &NSMenuItem, symbol: &str) {
        if let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str(symbol),
            Some(&NSString::from_str(symbol)),
        ) {
            item.setImage(Some(&image));
        }
    }

    fn rebuild_menu(&self) {
        if let Some(menu) = self.ivars().menu.borrow().as_ref() {
            self.rebuild_menu_with(menu);
        }
    }

    fn rebuild_menu_with(&self, menu: &NSMenu) {
        let snapshot = fetch_snapshot();
        menu.removeAllItems();
        let online = snapshot.health.is_some();
        self.disabled_item(
            menu,
            if online {
                "● Gateway online"
            } else {
                "○ Gateway offline"
            },
        );
        if let Some(error) = snapshot.error.as_deref() {
            self.disabled_item(menu, &format!("  ⚠ {error}"));
        }
        if let Some(health) = snapshot.health.as_ref() {
            self.disabled_item(
                menu,
                &format!(
                    "  Sessions: active {} / total {}",
                    health.sessions.active, health.sessions.created_total
                ),
            );
        }
        self.action_item(
            menu,
            if online {
                "■ Stop gateway"
            } else {
                "▶ Start gateway"
            },
            sel!(toggleGateway:),
        );
        self.action_item(menu, "↻ Refresh", sel!(refresh:));
        self.action_item(menu, "Open local dashboard", sel!(openDashboard:));
        self.separator(menu);
        self.disabled_item(menu, &format!("Sessions ({})", snapshot.sessions.len()));
        if let Some(error) = snapshot.session_error.as_deref() {
            self.disabled_item(menu, &format!("  ⚠ {error}"));
        } else if snapshot.sessions.is_empty() {
            self.disabled_item(menu, "  No sessions found");
        } else {
            let bound = bound_session_id(&snapshot);
            let mut projects: BTreeMap<String, Vec<&SessionInfo>> = BTreeMap::new();
            for session in &snapshot.sessions {
                projects
                    .entry(session.workspace.clone())
                    .or_default()
                    .push(session);
            }
            for (workspace, sessions) in projects {
                self.add_project_group(menu, &workspace, &sessions, bound.as_deref());
            }
        }
        self.separator(menu);
        self.disabled_item(menu, &bound_session_summary(&snapshot));
        self.add_channels(menu, &snapshot);
        self.action_item(menu, "Show phone pairing QR…", sel!(showPairingQr:));
        if let Some(error) = snapshot.ahp_error.as_deref() {
            self.disabled_item(menu, &format!("  ⚠ {error}"));
        }
        self.separator(menu);
        self.action_item(menu, "Copy Zed agent_servers", sel!(copyZed:));
        self.action_item(menu, "Choose gateway project…", sel!(chooseGatewayProject:));
        self.separator(menu);
        self.action_item(menu, "⏻ Agent Gateway", sel!(quit:));
        menu.update();
    }

    fn add_project_group(
        &self,
        parent: &NSMenu,
        workspace: &str,
        sessions: &[&SessionInfo],
        bound: Option<&str>,
    ) {
        let project = if workspace.is_empty() {
            "unknown project"
        } else {
            project_name(workspace)
        };
        let group = self.item(
            &format!(
                "{project} · {} session{}",
                sessions.len(),
                if sessions.len() == 1 { "" } else { "s" }
            ),
            sel!(noop:),
        );
        self.set_symbol(
            &group,
            if sessions
                .iter()
                .any(|session| bound == Some(session.id.as_str()))
            {
                "folder.fill"
            } else {
                "folder"
            },
        );
        let mtm = MainThreadMarker::new().expect("menu actions run on the main thread");
        let submenu = NSMenu::new(mtm);
        self.disabled_item(
            &submenu,
            if workspace.is_empty() {
                "Workspace unavailable"
            } else {
                workspace
            },
        );
        self.separator(&submenu);
        for session in sessions {
            self.add_session_item(&submenu, session, bound);
        }
        group.setSubmenu(Some(&submenu));
        parent.addItem(&group);
    }

    fn add_session_item(&self, parent: &NSMenu, session: &SessionInfo, bound: Option<&str>) {
        let title = session
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or("Untitled session");
        let item = self.item(
            &format!(
                "{title}{}",
                if session.status.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", session.status)
                }
            ),
            sel!(noop:),
        );
        if bound == Some(session.id.as_str()) {
            item.setState(NSControlStateValueOn);
            self.set_symbol(&item, "checkmark.circle.fill");
        }
        let mtm = MainThreadMarker::new().expect("menu actions run on the main thread");
        let submenu = NSMenu::new(mtm);
        self.disabled_item(
            &submenu,
            &format!("{} · {}", session.status, session.origin),
        );
        if !session.agent_name.is_empty() || !session.agent_id.is_empty() {
            self.disabled_item(
                &submenu,
                &format!(
                    "Agent: {}",
                    if session.agent_name.is_empty() {
                        &session.agent_id
                    } else {
                        &session.agent_name
                    }
                ),
            );
        }
        self.disabled_item(
            &submenu,
            &format!(
                "Project: {}",
                if session.workspace.is_empty() {
                    "unknown"
                } else {
                    &session.workspace
                }
            ),
        );
        if !session.cwd.is_empty() && session.cwd != session.workspace {
            self.disabled_item(&submenu, &format!("Working directory: {}", session.cwd));
        }
        self.disabled_item(
            &submenu,
            &format!("Gateway session: {}", shortened(&session.id, 24)),
        );
        if let Some(acp) = session.acp_session_id.as_deref() {
            self.disabled_item(&submenu, &format!("ACP session: {}", shortened(acp, 24)));
        }
        self.disabled_item(
            &submenu,
            &format!(
                "Events: {} · updated {}",
                session.last_seq,
                if session.updated_at.is_empty() {
                    "unknown"
                } else {
                    &session.updated_at
                }
            ),
        );
        self.separator(&submenu);
        self.represented_item(
            &submenu,
            "Bind WeChat to this session",
            sel!(bindSession:),
            &session.id,
        );
        item.setSubmenu(Some(&submenu));
        parent.addItem(&item);
    }

    fn add_channels(&self, menu: &NSMenu, snapshot: &GatewaySnapshot) {
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
            self.disabled_item(
                menu,
                &format!("  WeChat/AHP · {state}{session}{channel}{reason}"),
            );
            if let Some(qr) = status.and_then(|status| status.qr_code.as_deref()) {
                self.represented_item(menu, "  Show WeChat QR…", sel!(showQr:), qr);
            }
            if status.is_some_and(|status| status.session_id.is_some()) {
                self.action_item(menu, "  Unbind WeChat", sel!(unbindSession:));
            }
            if !ahp.enabled {
                self.disabled_item(menu, "  AHP is not configured");
            }
        }
        if let Some(health) = snapshot.health.as_ref() {
            for component in &health.components {
                if matches!(component.name.as_str(), "wechat" | "lark" | "telegram") {
                    let label = if component.name == "wechat" {
                        "WeChat"
                    } else {
                        component.name.as_str()
                    };
                    let state = if component.name == "wechat"
                        && component
                            .detail
                            .as_deref()
                            .is_some_and(|detail| detail.starts_with("session_id="))
                    {
                        "bound"
                    } else {
                        component.state.as_str()
                    };
                    self.disabled_item(
                        menu,
                        &format!(
                            "  {} · {}{}",
                            label,
                            state,
                            component
                                .detail
                                .as_deref()
                                .map(|detail| format!(" · {detail}"))
                                .unwrap_or_default()
                        ),
                    );
                }
            }
        }
        let bound = bound_session_id(snapshot);
        for channel in configured_channels() {
            let binding = if channel.name == "WeChat" {
                bound.as_deref().or(channel.session_id.as_deref())
            } else {
                channel.session_id.as_deref()
            };
            self.disabled_item(
                menu,
                &format!(
                    "  {} · {}{}{}",
                    channel.name,
                    if channel.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    binding
                        .map(|id| format!(" · session {}", shortened(id, 18)))
                        .unwrap_or_default(),
                    channel
                        .chat_id
                        .as_deref()
                        .map(|chat| format!(" · chat {}", shortened(chat, 18)))
                        .unwrap_or_default()
                ),
            );
        }
    }

    fn show_qr_alert(&self, title: &str, payload: &str, details: &str) {
        let mtm = MainThreadMarker::new().expect("menu actions run on the main thread");
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(details));

        if let Ok(png) = qr_png(payload) {
            let data = unsafe {
                NSData::initWithBytes_length(NSData::alloc(), png.as_ptr().cast(), png.len())
            };
            if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
                image.setSize(NSSize {
                    width: 320.0,
                    height: 320.0,
                });
                let view = NSImageView::imageViewWithImage(&image, mtm);
                view.setFrame(NSRect {
                    origin: NSPoint { x: 0.0, y: 0.0 },
                    size: NSSize {
                        width: 320.0,
                        height: 320.0,
                    },
                });
                alert.setAccessoryView(Some(view.as_super().as_super()));
            }
        }

        alert.addButtonWithTitle(&NSString::from_str("OK"));
        let _ = alert.runModal();
    }

    fn alert(&self, title: &str, message: &str) {
        let mtm = MainThreadMarker::new().expect("menu actions run on the main thread");
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(message));
        alert.addButtonWithTitle(&NSString::from_str("OK"));
        let _ = alert.runModal();
    }
}

fn qr_png(payload: &str) -> Result<Vec<u8>, String> {
    let code = QrCode::new(payload.as_bytes()).map_err(|error| error.to_string())?;
    let image = code
        .render::<Luma<u8>>()
        .quiet_zone(true)
        .min_dimensions(320, 320)
        .build();
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(image)
        .write_to(&mut output, ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok(output.into_inner())
}

pub fn run() {
    let mtm = MainThreadMarker::new().expect("macOS UI must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let target: Retained<MenuTarget> = unsafe { msg_send![MenuTarget::class(), new] };
    let menu = NSMenu::new(mtm);
    *target.ivars().menu.borrow_mut() = Some(menu.clone());
    menu.setDelegate(Some(ProtocolObject::from_ref(&*target)));
    target.rebuild_menu_with(&menu);
    let status_bar = NSStatusBar::systemStatusBar();
    let status: Retained<NSStatusItem> =
        status_bar.statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = status.button(mtm) {
        if let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str("network"),
            Some(&NSString::from_str("Agent Gateway")),
        ) {
            button.setImage(Some(&image));
        } else {
            button.setTitle(&NSString::from_str("AG"));
        }
    }
    status.setMenu(Some(&menu));
    app.run();
}

#[cfg(test)]
mod tests {
    use super::{SessionList, qr_png};

    #[test]
    fn decodes_gateway_sessions() {
        let list: SessionList = serde_json::from_value(serde_json::json!({
            "sessions": [{"id": "sess_1", "workspace": "/tmp/project", "status": "idle"}]
        }))
        .expect("session list should match the gateway response");
        assert_eq!(list.sessions[0].id, "sess_1");
        assert_eq!(list.sessions[0].status, "idle");
    }

    #[test]
    fn renders_a_scannable_png_for_qr_payloads() {
        let png =
            qr_png(r#"{"pairing_code":"123456","nonce":"n"}"#).expect("QR payload should render");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }
}
