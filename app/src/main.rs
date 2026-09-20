//! Tiny macOS menu bar companion for `agent-gateway`.
//!
//! The daemon remains the CLI binary. This process only owns the status item
//! and starts the CLI daemon as a child. Pure configuration and Zed rendering
//! are shared through the `gateway-cli` library.

#[cfg(target_os = "macos")]
mod macos {
    use std::net::TcpStream;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use cocoa::appkit::NSSavePanel as _;
    use cocoa::appkit::{
        NSApp, NSApplication, NSApplicationActivationPolicyAccessory, NSButton, NSMenu, NSMenuItem,
        NSOpenPanel, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
    };
    use cocoa::base::{id, nil};
    use cocoa::foundation::{NSAutoreleasePool, NSString};
    use objc::declare::ClassDecl;
    use objc::runtime::{Class, NO, Object, Sel, YES};
    use objc::{class, msg_send, sel, sel_impl};
    use std::ffi::CStr;

    extern "C" fn copy_zed(_: &Object, _: Sel, _: id) {
        if let Err(error) = copy_zed_settings() {
            eprintln!("cannot copy Zed agent_servers: {error:#}");
        }
    }

    extern "C" fn start_gateway(_: &Object, _: Sel, _: id) {
        if let Err(error) = start_gateway_process() {
            eprintln!("cannot start agent-gateway: {error:#}");
        }
    }

    extern "C" fn choose_gateway_project(_: &Object, _: Sel, _: id) {
        if let Err(error) = choose_project_directory() {
            eprintln!("cannot choose gateway project: {error:#}");
        }
    }

    extern "C" fn quit(_: &Object, _: Sel, _: id) {
        stop_owned_gateway();
        unsafe {
            let _: () = msg_send![NSApp(), terminate: nil];
        }
    }

    fn target_class() -> &'static Class {
        let superclass = class!(NSObject);
        let mut declaration =
            ClassDecl::new("AgentGatewayMenuTarget", superclass).expect("menu target class");
        unsafe {
            declaration.add_method(sel!(copyZed:), copy_zed as extern "C" fn(&Object, Sel, id));
            declaration.add_method(
                sel!(startGateway:),
                start_gateway as extern "C" fn(&Object, Sel, id),
            );
            declaration.add_method(
                sel!(chooseGatewayProject:),
                choose_gateway_project as extern "C" fn(&Object, Sel, id),
            );
            declaration.add_method(sel!(quit:), quit as extern "C" fn(&Object, Sel, id));
        }
        declaration.register()
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

    fn selected_project_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        let path =
            PathBuf::from(home).join("Library/Application Support/Agent Gateway/project.path");
        std::fs::read_to_string(path)
            .ok()
            .map(|value| PathBuf::from(value.trim()))
            .filter(|path| path.is_dir())
    }

    fn copy_zed_settings() -> anyhow::Result<()> {
        let path = config_path();
        let config = gateway_config::GatewayConfig::load(&path)
            .map_err(|error| anyhow::anyhow!("cannot load {}: {error}", path.display()))?;
        let rendered = gateway_cli::zed::render(&config, &path, None, None)?;
        gateway_cli::zed::copy_to_clipboard(&rendered)
    }

    fn start_gateway_process() -> anyhow::Result<()> {
        let path = config_path();
        let config = gateway_config::GatewayConfig::load(&path)
            .map_err(|error| anyhow::anyhow!("cannot load {}: {error}", path.display()))?;
        if TcpStream::connect_timeout(&config.bind_addr(), Duration::from_millis(150)).is_ok() {
            return Ok(());
        }

        let mut owned = owned_gateway()
            .lock()
            .map_err(|_| anyhow::anyhow!("gateway process lock is poisoned"))?;
        if let Some(child) = owned.as_mut() {
            if child.try_wait()?.is_none() {
                return Ok(());
            }
        }
        *owned = None;

        let binary = daemon_binary()?;
        let child = Command::new(binary)
            .args(["--config", &path.to_string_lossy(), "run"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        *owned = Some(child);
        Ok(())
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

    fn stop_owned_gateway() {
        let Ok(mut owned) = owned_gateway().lock() else {
            return;
        };
        if let Some(mut child) = owned.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn choose_project_directory() -> anyhow::Result<()> {
        unsafe {
            let panel = NSOpenPanel::openPanel(nil);
            panel.setCanChooseFiles_(NO);
            panel.setCanChooseDirectories_(YES);
            panel.setAllowsMultipleSelection_(NO);
            let title = NSString::alloc(nil).init_str("Choose acp-gw project directory");
            let _: () = msg_send![panel, setTitle: title];
            if panel.runModal() as i64 != 1 {
                return Ok(());
            }
            let url = panel.URL();
            if url == nil {
                return Ok(());
            }
            let path: id = msg_send![url, path];
            let selected = CStr::from_ptr(NSString::UTF8String(path))
                .to_string_lossy()
                .into_owned();
            let cargo = PathBuf::from(&selected).join("Cargo.toml");
            if !cargo.is_file() {
                anyhow::bail!("selected directory does not contain Cargo.toml: {selected}");
            }
            let home =
                std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
            let path =
                PathBuf::from(home).join("Library/Application Support/Agent Gateway/project.path");
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, selected)?;
            eprintln!("selected gateway project; build with `cargo build -p gateway-cli`");
        }
        Ok(())
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

    pub fn run() {
        unsafe {
            let _pool = NSAutoreleasePool::new(nil);
            let app = NSApp();
            app.setActivationPolicy_(NSApplicationActivationPolicyAccessory);
            let target: id = msg_send![target_class(), new];
            let status =
                NSStatusBar::systemStatusBar(nil).statusItemWithLength_(NSVariableStatusItemLength);
            status
                .button()
                .setTitle_(NSString::alloc(nil).init_str("AG"));
            let menu = NSMenu::new(nil).autorelease();
            menu.addItem_(item("Start gateway", sel!(startGateway:), target));
            menu.addItem_(item("Copy Zed agent_servers", sel!(copyZed:), target));
            menu.addItem_(item(
                "Choose gateway project…",
                sel!(chooseGatewayProject:),
                target,
            ));
            menu.addItem_(NSMenuItem::separatorItem(nil));
            menu.addItem_(item("Quit Agent Gateway", sel!(quit:), target));
            status.setMenu_(menu);
            if let Err(error) = start_gateway_process() {
                eprintln!("agent-gateway was not started: {error:#}");
            }
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
    eprintln!("agent-gateway-app is a macOS menu bar helper");
}
