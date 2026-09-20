//! Zed integration helpers shared by the menu bar app and CLI.

#![allow(unreachable_pub)]

use std::collections::BTreeMap;
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use gateway_config::GatewayConfig;
use serde_json::{Map, json};

#[derive(Debug, Subcommand)]
pub(crate) enum ZedCommand {
    /// Print the `agent_servers` object to stdout.
    Config(ZedConfigArgs),
    /// Copy the `agent_servers` object to the macOS clipboard.
    Copy(ZedConfigArgs),
}

#[derive(Debug, Args)]
pub(crate) struct ZedConfigArgs {
    /// Agent id to include. Repeat the command for another agent, or omit to include all.
    #[arg(long)]
    pub agent: Option<String>,
    /// Command path written into Zed. Defaults to the current gateway executable.
    #[arg(long, env = "AGENT_GATEWAY_BIN")]
    pub command: Option<String>,
}

#[allow(dead_code)]
pub(crate) fn run(config: &GatewayConfig, command: ZedCommand, config_path: &Path) -> Result<()> {
    let args = match &command {
        ZedCommand::Config(args) | ZedCommand::Copy(args) => args,
    };
    let rendered = render(
        config,
        config_path,
        args.agent.as_deref(),
        args.command.as_deref(),
    )?;
    match command {
        ZedCommand::Config(_) => println!("{rendered}"),
        ZedCommand::Copy(_) => copy_to_clipboard(&rendered)?,
    }
    Ok(())
}

/// Render the Zed `agent_servers` object for an already loaded gateway config.
pub fn render(
    config: &GatewayConfig,
    config_path: &Path,
    selected: Option<&str>,
    command: Option<&str>,
) -> Result<String> {
    let executable = match command {
        Some(command) => PathBuf::from(command),
        None => discover_gateway_binary()?,
    };
    let mut output = Map::new();
    for agent in &config.agents {
        if selected.is_some_and(|selected| selected != agent.id) {
            continue;
        }
        let key = format!("{} via Agent Gateway", agent.name);
        let mut env = BTreeMap::new();
        env.insert(
            "AGENT_GATEWAY_CONFIG".to_owned(),
            config_path.to_string_lossy().to_string(),
        );
        output.insert(
            key,
            json!({
                "default_config_options": { "model": "gpt-6-astra" },
                "type": "custom",
                "command": executable,
                "args": ["acp-bridge", "--agent", agent.id],
                "env": env,
            }),
        );
    }
    if output.is_empty() {
        anyhow::bail!("no configured agent matched");
    }
    Ok(serde_json::to_string_pretty(
        &json!({"agent_servers": output}),
    )?)
}

/// Locate the gateway executable for both an installed App bundle and local development.
///
/// `AGENT_GATEWAY_BIN` is intentionally first so a user can pin an explicit build. In the
/// App bundle the wrapper is next to the status item executable. When running from a checkout,
/// the dependency's manifest directory lets us find the workspace `target` directory even
/// though the app has its own Cargo workspace.
pub fn discover_gateway_binary() -> Result<PathBuf> {
    if let Some(value) = env::var_os("AGENT_GATEWAY_BIN") {
        let path = PathBuf::from(value);
        return existing_binary(path, "AGENT_GATEWAY_BIN");
    }

    if let Some(path) = sibling_binary(env::current_exe().ok()) {
        return Ok(path);
    }

    if let Some(project_dir) = env::var_os("AGENT_GATEWAY_PROJECT") {
        return discover_gateway_binary_from_project(Path::new(&project_dir));
    }

    if let Some(manifest_dir) = option_env!("CARGO_MANIFEST_DIR") {
        let manifest = Path::new(manifest_dir);
        if let Some(path) = project_binary(manifest.parent().unwrap_or(manifest)) {
            return Ok(path);
        }
        if let Some(workspace_root) = manifest.parent().and_then(Path::parent)
            && let Some(path) = project_binary(workspace_root)
        {
            return Ok(path);
        }
    }

    if let Some(path) = env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|dir| dir.join(binary_name()))
            .find(|path| path.is_file())
    }) {
        return Ok(path);
    }

    anyhow::bail!(
        "cannot find agent-gateway; install the bundled CLI, set AGENT_GATEWAY_BIN, set AGENT_GATEWAY_PROJECT, or build `cargo build -p gateway-cli`"
    )
}

/// Locate a gateway binary below a user-selected Cargo project directory.
pub fn discover_gateway_binary_from_project(project_dir: &Path) -> Result<PathBuf> {
    let mut candidate_dir = Some(project_dir);
    while let Some(dir) = candidate_dir {
        if dir.join("Cargo.toml").is_file()
            && let Some(path) = project_binary(dir)
        {
            return Ok(path);
        }
        candidate_dir = dir.parent();
    }
    anyhow::bail!(
        "no agent-gateway binary found under {}; build it with `cargo build -p gateway-cli`",
        project_dir.display()
    )
}

fn project_binary(project_dir: &Path) -> Option<PathBuf> {
    for profile in ["debug", "release"] {
        let candidate = project_dir.join("target").join(profile).join(binary_name());
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn existing_binary(path: PathBuf, source: &str) -> Result<PathBuf> {
    if path.is_file() {
        Ok(path)
    } else {
        anyhow::bail!(
            "{source} points to a missing agent-gateway binary: {}",
            path.display()
        )
    }
}

fn sibling_binary(current_exe: Option<PathBuf>) -> Option<PathBuf> {
    current_exe
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .map(|dir| dir.join(binary_name()))
        .filter(|path| path.is_file())
}

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "agent-gateway.exe"
    } else {
        "agent-gateway"
    }
}

/// Copy rendered Zed settings to the platform clipboard.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    for program in ["pbcopy", "wl-copy", "xclip"] {
        let mut command = Command::new(program);
        if program == "xclip" {
            command.args(["-selection", "clipboard"]);
        }
        let mut child = match command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => continue,
        };
        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(text.as_bytes())
                .with_context(|| format!("cannot write to {program}"))?;
        }
        let status = child
            .wait()
            .with_context(|| format!("cannot wait for {program}"))?;
        if status.success() {
            println!("Zed agent_servers configuration copied to the clipboard.");
            return Ok(());
        }
    }
    anyhow::bail!(
        "no clipboard command available; use `agent-gateway zed config` and copy its output"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn generated_entry_matches_zed_custom_server_shape() {
        let config = GatewayConfig::from_toml(
            r#"[[agents]]
id = "codex"
name = "Codex"
command = "codex"
"#,
        )
        .unwrap();
        let path = Path::new("/tmp/gateway.toml");
        let rendered = render(&config, path, None, Some("/tmp/agent-gateway")).unwrap();
        let servers = serde_json::from_str::<Value>(&rendered).unwrap();
        let entry = &servers["agent_servers"]["Codex via Agent Gateway"];
        assert_eq!(entry["type"], "custom");
        assert_eq!(entry["args"], json!(["acp-bridge", "--agent", "codex"]));
        assert_eq!(entry["default_config_options"]["model"], "gpt-6-astra");
        assert_eq!(entry["env"]["AGENT_GATEWAY_CONFIG"], "/tmp/gateway.toml");
    }

    #[test]
    fn explicit_binary_must_exist() {
        let error = existing_binary(
            PathBuf::from("/definitely/missing/agent-gateway"),
            "AGENT_GATEWAY_BIN",
        )
        .unwrap_err();
        assert!(error.to_string().contains("AGENT_GATEWAY_BIN"));
    }
}
