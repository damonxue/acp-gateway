//! Small native Windows companion built on Microsoft's official `windows`
//! projection. It keeps the gateway daemon beside the app and exposes a
//! refreshable session list in a normal Win32 window.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result as WindowsResult};

const REFRESH_ID: usize = 1001;
const LIST_ID: usize = 1002;
const WINDOW_CLASS: &str = "AgentGatewayDesktopWindow";

static DAEMON: OnceLock<Mutex<Option<Child>>> = OnceLock::new();
static mut SESSION_LIST: Option<HWND> = None;

#[derive(Debug, Default, Deserialize)]
struct SessionList {
    #[serde(default)]
    sessions: Vec<SessionInfo>,
}

#[derive(Debug, Default, Deserialize)]
struct SessionInfo {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    workspace: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    agent_name: String,
}

fn daemon_slot() -> &'static Mutex<Option<Child>> {
    DAEMON.get_or_init(|| Mutex::new(None))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
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

fn start_daemon() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(parent) = exe.parent() else { return };
    let daemon = parent.join("agent-gateway-daemon.exe");
    let Ok(mut slot) = daemon_slot().lock() else {
        return;
    };
    if let Some(child) = slot.as_mut() {
        match child.try_wait() {
            Ok(None) => return,
            Ok(Some(_)) | Err(_) => *slot = None,
        }
    }
    if let Ok(child) = Command::new(daemon)
        .args(["--config", &config_path().to_string_lossy(), "run"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        *slot = Some(child);
    }
}

fn sessions() -> Vec<String> {
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(client) => client,
        Err(error) => return vec![format!("Gateway error: {error}")],
    };
    let base = base_url();
    let refresh = match client.post(format!("{base}/sessions/refresh")).send() {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => return vec![format!("Gateway refresh returned {}", response.status())],
        Err(error) => return vec![format!("Gateway refresh failed: {error}")],
    };
    drop(refresh);
    let response = match client.get(format!("{base}/sessions")).send() {
        Ok(response) => response,
        Err(error) => return vec![format!("Gateway offline: {error}")],
    };
    if !response.status().is_success() {
        return vec![format!("Gateway returned {}", response.status())];
    }
    match response.json::<SessionList>() {
        Ok(list) if list.sessions.is_empty() => vec!["No sessions found".to_owned()],
        Ok(list) => list
            .sessions
            .into_iter()
            .map(|session| {
                let title = session
                    .title
                    .unwrap_or_else(|| "Untitled session".to_owned());
                let agent = if session.agent_name.is_empty() {
                    "unknown agent"
                } else {
                    &session.agent_name
                };
                format!(
                    "{title} · {} · {agent} · {} · {}",
                    session.status, session.workspace, session.id
                )
            })
            .collect(),
        Err(error) => vec![format!("Cannot decode /sessions: {error}")],
    }
}

unsafe fn refresh_list() {
    let Some(list) = (unsafe { SESSION_LIST }) else {
        return;
    };
    let _ = unsafe { SendMessageW(list, LB_RESETCONTENT, None, None) };
    for session in sessions() {
        let value = wide(&session);
        let _ = unsafe {
            SendMessageW(
                list,
                LB_ADDSTRING,
                None,
                Some(LPARAM(value.as_ptr() as isize)),
            )
        };
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CREATE => {
            let button_class = wide("BUTTON");
            let button_title = wide("Refresh sessions");
            let list_class = wide("LISTBOX");
            let empty = wide("");
            let button = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    PCWSTR(button_class.as_ptr()),
                    PCWSTR(button_title.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                    16,
                    16,
                    160,
                    30,
                    Some(hwnd),
                    Some(HMENU(REFRESH_ID as *mut _)),
                    None,
                    None,
                )
            };
            let _ = button;
            unsafe {
                SESSION_LIST = CreateWindowExW(
                    WS_EX_CLIENTEDGE,
                    PCWSTR(list_class.as_ptr()),
                    PCWSTR(empty.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WS_VSCROLL | WINDOW_STYLE(LBS_NOTIFY as u32),
                    16,
                    60,
                    740,
                    450,
                    Some(hwnd),
                    Some(HMENU(LIST_ID as *mut _)),
                    None,
                    None,
                )
                .ok();
                refresh_list();
            }
            LRESULT(0)
        }
        WM_COMMAND if (wparam.0 & 0xffff) == REFRESH_ID => {
            unsafe { refresh_list() };
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { SESSION_LIST = None };
            if let Ok(mut slot) = daemon_slot().lock()
                && let Some(mut child) = slot.take()
            {
                let _ = child.kill();
                let _ = child.wait();
            }
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

pub fn run() {
    start_daemon();
    let _ = run_inner();
}

fn run_inner() -> WindowsResult<()> {
    unsafe {
        let module = GetModuleHandleW(None)?;
        let instance = HINSTANCE(module.0);
        let class_name = wide(WINDOW_CLASS);
        let window_name = wide("Agent Gateway");
        let class = WNDCLASSW {
            hInstance: instance,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            lpfnWndProc: Some(window_proc),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        let _ = RegisterClassW(&class);
        let window = CreateWindowExW(
            WS_EX_APPWINDOW,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(window_name.as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            800,
            560,
            None,
            None,
            Some(instance),
            None,
        )?;
        ShowWindow(window, SW_SHOW);
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}
