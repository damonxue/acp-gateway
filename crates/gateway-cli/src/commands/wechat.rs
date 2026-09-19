use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;
use gateway_config::GatewayConfig;
use gateway_wechat::{CredentialStore, HttpWeixinApi, KeychainCredentialStore, login};
use qrcode::QrCode;
use qrcode::render::unicode;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Subcommand)]
pub(crate) enum WechatCommand {
    /// Display a QR code and save the confirmed bot credentials to Keychain.
    Login,
    /// Remove the saved WeChat credentials.
    Logout,
    /// Print redacted login status.
    Status,
    /// Bind one existing Gateway session and chat URI in config.toml.
    Bind {
        /// Existing Gateway session id.
        #[arg(long = "session")]
        session_id: String,
        /// Stable AHP chat URI associated with that session.
        #[arg(long = "chat")]
        chat_id: String,
    },
    /// Disable the configured binding while retaining its values for later.
    Unbind,
}

pub(crate) async fn run(config: &GatewayConfig, command: WechatCommand, path: &Path) -> Result<()> {
    let store = KeychainCredentialStore::default();
    match command {
        WechatCommand::Login => {
            let section = config.wechat.as_ref().ok_or_else(|| {
                anyhow::anyhow!("add [wechat] to config.toml before using this command")
            })?;
            let api = HttpWeixinApi::new(&section.api_base).context("invalid WeChat API base")?;
            let cancel = CancellationToken::new();
            let credentials = login(
                &api,
                cancel,
                |challenge| {
                    let code =
                        QrCode::new(challenge.image_content.as_bytes()).map_err(|error| {
                            gateway_wechat::WeixinError::Protocol(error.to_string())
                        })?;
                    println!(
                        "Scan this QR code in WeChat:\n{}",
                        code.render::<unicode::Dense1x2>().quiet_zone(true).build()
                    );
                    println!("QR payload: {}", challenge.code);
                    Ok(())
                },
                || {
                    print!("WeChat verification code: ");
                    io::stdout().flush().ok();
                    let mut line = String::new();
                    io::stdin().read_line(&mut line).map_err(|error| {
                        gateway_wechat::WeixinError::Protocol(error.to_string())
                    })?;
                    Ok(line.trim().to_owned())
                },
            )
            .await
            .context("WeChat QR login failed")?;
            store
                .save(&credentials)
                .map_err(|error| anyhow::anyhow!(error))?;
            println!(
                "WeChat login succeeded for owner {} (token stored in Keychain)",
                credentials.owner_id
            );
            Ok(())
        }
        WechatCommand::Logout => {
            store.clear().map_err(|error| anyhow::anyhow!(error))?;
            println!("WeChat credentials cleared");
            Ok(())
        }
        WechatCommand::Status => {
            match store.load().map_err(|error| anyhow::anyhow!(error))? {
                Some(credentials) => println!(
                    "logged in: bot={} owner={} base={}",
                    credentials.bot_id, credentials.owner_id, credentials.base_url
                ),
                None => println!("not logged in"),
            }
            Ok(())
        }
        WechatCommand::Bind {
            session_id,
            chat_id,
        } => {
            if session_id.trim().is_empty() || chat_id.trim().is_empty() {
                anyhow::bail!("session_id and chat_id must not be empty");
            }
            update_binding(path, Some((session_id, chat_id)))?;
            println!("WeChat binding saved; restart `agent-gateway run` to apply it");
            Ok(())
        }
        WechatCommand::Unbind => {
            update_binding(path, None)?;
            println!("WeChat binding disabled; restart `agent-gateway run` to apply it");
            Ok(())
        }
    }
}

fn update_binding(path: &Path, binding: Option<(String, String)>) -> Result<()> {
    let text = std::fs::read_to_string(path).context("cannot read config.toml")?;
    let mut document: toml::Value = toml::from_str(&text).context("cannot parse config.toml")?;
    let root = document
        .as_table_mut()
        .context("config root is not a table")?;
    let wechat = root
        .entry("wechat")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .context("wechat config is not a table")?;
    match binding {
        Some((session_id, chat_id)) => {
            wechat.insert("enabled".into(), toml::Value::Boolean(true));
            let mut table = toml::map::Map::new();
            table.insert("session_id".into(), toml::Value::String(session_id));
            table.insert("chat_id".into(), toml::Value::String(chat_id));
            wechat.insert("binding".into(), toml::Value::Table(table));
        }
        None => {
            wechat.insert("enabled".into(), toml::Value::Boolean(false));
        }
    }
    let rendered = toml::to_string_pretty(&document).context("cannot render config.toml")?;
    std::fs::write(path, rendered).context("cannot write config.toml")?;
    Ok(())
}
