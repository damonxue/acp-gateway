//! # gateway-config
//!
//! Loading, defaulting and **validating** the gateway's TOML configuration.
//!
//! Configuration is the gateway's main attack surface: it decides which port
//! is exposed, which executables are launched, and which secrets exist. So the
//! rule here is that a [`GatewayConfig`] value can only be obtained through
//! [`GatewayConfig::load`] or [`GatewayConfig::from_toml`], both of which run
//! [`GatewayConfig::validate`]. Downstream crates therefore never have to ask
//! "was this checked?".
//!
//! ```no_run
//! # fn main() -> Result<(), gateway_config::ConfigError> {
//! let path = gateway_config::default_config_path();
//! let config = gateway_config::GatewayConfig::load(&path)?;
//! println!("{} agents configured", config.agents.len());
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

mod secret;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gateway_core::AgentDescriptor;
use gateway_core::ids::AgentId;
use serde::{Deserialize, Serialize};

pub use secret::Secret;

/// Default TCP endpoint. Loopback only — remote access goes through a tunnel.
pub const DEFAULT_BIND: &str = "127.0.0.1:48100";
/// Directory (before `~` expansion) holding the database, identity and logs.
pub const DEFAULT_DATA_DIR: &str = "~/.agent-gateway";
/// File name of the configuration inside the data directory.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Everything that can go wrong while loading configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read or written.
    #[error("cannot access config file {path}: {source}")]
    Io {
        /// Offending path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },

    /// The file is not valid TOML, or does not match the schema.
    #[error("invalid TOML in {path}: {source}")]
    Parse {
        /// Offending path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: toml::de::Error,
    },

    /// The file parsed but the values are not usable.
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

impl ConfigError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

/// The whole configuration file.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    /// Local daemon settings.
    #[serde(default)]
    pub gateway: GatewaySection,
    /// Credential lifetimes and loopback policy.
    #[serde(default)]
    pub security: SecuritySection,
    /// Control-plane settings. Absent means "local only".
    #[serde(default)]
    pub relay: Option<RelaySection>,
    /// Cloudflare tunnel settings. Absent means "no tunnel".
    #[serde(default)]
    pub tunnel: Option<TunnelSection>,
    /// Outbound Agent Host Protocol channel. Absent means "no AHP channel".
    #[serde(default)]
    pub ahp: Option<AhpSection>,
    /// Embedded WeChat Bot adapter. Credentials are kept in the OS secret store.
    #[serde(default)]
    pub wechat: Option<WechatSection>,
    /// Agents this gateway may launch. Discovery is never implicit.
    #[serde(default, rename = "agents")]
    pub agents: Vec<AgentSection>,
}

/// `[gateway]`
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewaySection {
    /// Address the local HTTP/WS server binds to.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Directory for the database, identity material and logs.
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    /// `tracing` filter (`info`, `debug`, `gateway_acp=trace`, …).
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Display name reported to the relay. Defaults to the hostname.
    #[serde(default)]
    pub machine_name: Option<String>,
    /// Allow binding to a non-loopback address.
    ///
    /// Off by default: the supported way to reach the gateway from another
    /// network is a tunnel plus a ticket, not an open port.
    #[serde(default)]
    pub allow_public_bind: bool,
}

impl Default for GatewaySection {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            data_dir: default_data_dir(),
            log_level: default_log_level(),
            machine_name: None,
            allow_public_bind: false,
        }
    }
}

/// `[security]`
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecuritySection {
    /// How long a WebSocket ticket stays redeemable.
    #[serde(with = "humantime_serde", default = "default_ticket_ttl")]
    pub ticket_ttl: Duration,
    /// How long a pairing code stays redeemable.
    #[serde(with = "humantime_serde", default = "default_pairing_ttl")]
    pub pairing_ttl: Duration,
    /// Whether connections arriving from loopback may skip ticket checks.
    ///
    /// `true` keeps `curl 127.0.0.1` and local development usable. Traffic
    /// arriving through the tunnel is *not* loopback-exempt: `cloudflared`
    /// connects locally, so the HTTP layer distinguishes the two by requiring
    /// a ticket whenever the request carries tunnel headers.
    #[serde(default = "default_true")]
    pub trust_loopback: bool,
}

impl Default for SecuritySection {
    fn default() -> Self {
        Self {
            ticket_ttl: default_ticket_ttl(),
            pairing_ttl: default_pairing_ttl(),
            trust_loopback: true,
        }
    }
}

/// `[relay]`
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelaySection {
    /// Base URL of the relay control plane.
    pub endpoint: String,
    /// Bearer token identifying this machine's owner, if the relay requires one.
    #[serde(default)]
    pub token: Option<Secret>,
    /// Heartbeat interval.
    #[serde(with = "humantime_serde", default = "default_heartbeat")]
    pub heartbeat_interval: Duration,
}

/// How `cloudflared` is configured.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelMode {
    /// `cloudflared tunnel --no-autoupdate run --token <token>`
    Token,
    /// `cloudflared tunnel --config <config_path> run`
    Config,
}

/// `[tunnel]`
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelSection {
    /// Which launch mode to use.
    pub mode: TunnelMode,
    /// Tunnel token, required by [`TunnelMode::Token`]. Never logged.
    #[serde(default)]
    pub token: Option<Secret>,
    /// `cloudflared` config file, required by [`TunnelMode::Config`].
    #[serde(default)]
    pub config_path: Option<String>,
    /// Public hostname the tunnel exposes, reported to the relay.
    #[serde(default)]
    pub hostname: Option<String>,
    /// Local origin the tunnel forwards to. Defaults to `http://<bind>`.
    #[serde(default)]
    pub origin: Option<String>,
    /// Path to the `cloudflared` executable.
    #[serde(default = "default_cloudflared")]
    pub binary: String,
}

/// `[ahp]` — outbound channel to the AHP/WeChat service.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AhpSection {
    /// WebSocket endpoint of the AHP channel service.
    pub endpoint: String,
    /// Optional bearer token accepted by the channel service.
    #[serde(default)]
    pub token: Option<Secret>,
    /// Delay between reconnect attempts.
    #[serde(with = "humantime_serde", default = "default_ahp_reconnect")]
    pub reconnect_interval: Duration,
}

/// `[wechat]` — embedded Weixin Bot adapter settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WechatSection {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_wechat_api_base")]
    pub api_base: String,
    #[serde(with = "humantime_serde", default = "default_wechat_poll_timeout")]
    pub poll_timeout: Duration,
    #[serde(with = "humantime_serde", default = "default_wechat_qr_timeout")]
    pub qr_timeout: Duration,
    #[serde(default = "default_wechat_max_text_bytes")]
    pub max_text_bytes: usize,
    #[serde(default = "default_wechat_max_chunk_bytes")]
    pub max_chunk_bytes: usize,
    #[serde(default)]
    pub binding: Option<WechatBindingSection>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WechatBindingSection {
    pub session_id: String,
    pub chat_id: String,
}

/// One `[[agents]]` entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSection {
    /// Stable id used by the API.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Executable.
    pub command: String,
    /// Arguments that put the executable into ACP mode.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl From<&AgentSection> for AgentDescriptor {
    fn from(section: &AgentSection) -> Self {
        Self {
            id: AgentId::new(section.id.clone()),
            name: section.name.clone(),
            command: section.command.clone(),
            args: section.args.clone(),
            env: section.env.clone(),
        }
    }
}

impl GatewayConfig {
    /// Read and validate a configuration file.
    ///
    /// # Errors
    /// Fails if the file cannot be read, is not valid TOML, or does not pass
    /// [`Self::validate`].
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Parse and validate configuration from a string.
    ///
    /// # Errors
    /// Fails on malformed TOML or invalid values.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let mut config: Self = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: PathBuf::from("<memory>"),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Check every rule the daemon relies on, and normalise paths.
    ///
    /// Called by the constructors; public so `agent-gateway config check` can
    /// run it against a file the user is about to install.
    ///
    /// # Errors
    /// Returns [`ConfigError::Invalid`] describing the first broken rule.
    pub fn validate(&mut self) -> Result<(), ConfigError> {
        self.gateway.data_dir = expand_tilde(&self.gateway.data_dir);

        let bind: SocketAddr = self.gateway.bind.parse().map_err(|_| {
            ConfigError::invalid(format!(
                "bind `{}` is not a socket address",
                self.gateway.bind
            ))
        })?;
        if !bind.ip().is_loopback() && !self.gateway.allow_public_bind {
            return Err(ConfigError::invalid(format!(
                "bind `{bind}` is not loopback; set gateway.allow_public_bind = true only if you \
                 really want the gateway reachable without a tunnel"
            )));
        }

        if self.agents.is_empty() {
            return Err(ConfigError::invalid(
                "no [[agents]] configured; the gateway never discovers agents on its own",
            ));
        }
        let mut seen = BTreeMap::new();
        for agent in &self.agents {
            if agent.id.trim().is_empty() {
                return Err(ConfigError::invalid("agent id must not be empty"));
            }
            if agent.command.trim().is_empty() {
                return Err(ConfigError::invalid(format!(
                    "agent `{}` has an empty command",
                    agent.id
                )));
            }
            if seen.insert(agent.id.clone(), ()).is_some() {
                return Err(ConfigError::invalid(format!(
                    "agent id `{}` is used twice",
                    agent.id
                )));
            }
        }

        if let Some(relay) = &self.relay {
            let url = url::Url::parse(&relay.endpoint).map_err(|error| {
                ConfigError::invalid(format!("relay.endpoint is not a URL: {error}"))
            })?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(ConfigError::invalid(
                    "relay.endpoint must be http:// or https://",
                ));
            }
        }

        if let Some(ahp) = &self.ahp {
            let url = url::Url::parse(&ahp.endpoint).map_err(|error| {
                ConfigError::invalid(format!("ahp.endpoint is not a URL: {error}"))
            })?;
            if !matches!(url.scheme(), "ws" | "wss") {
                return Err(ConfigError::invalid("ahp.endpoint must be ws:// or wss://"));
            }
        }
        if let Some(wechat) = &self.wechat {
            let url = url::Url::parse(&wechat.api_base).map_err(|error| {
                ConfigError::invalid(format!("wechat.api_base is not a URL: {error}"))
            })?;
            let authority = wechat
                .api_base
                .strip_prefix("https://")
                .and_then(|rest| rest.split('/').next())
                .unwrap_or_default();
            if url.scheme() != "https"
                || url.username() != ""
                || url.password().is_some()
                || url.port().is_some()
                || url.path() != "/"
                || url.query().is_some()
                || url.fragment().is_some()
                || authority.contains(':')
                || !url.host_str().is_some_and(valid_wechat_host)
            {
                return Err(ConfigError::invalid(
                    "wechat.api_base must be an https ilink*.weixin.qq.com origin",
                ));
            }
            if wechat.max_text_bytes == 0 || wechat.max_text_bytes > 16 * 1024 {
                return Err(ConfigError::invalid(
                    "wechat.max_text_bytes must be between 1 and 16384",
                ));
            }
            if wechat.max_chunk_bytes < 4 || wechat.max_chunk_bytes > 3500 {
                return Err(ConfigError::invalid(
                    "wechat.max_chunk_bytes must be between 4 and 3500",
                ));
            }
            if let Some(binding) = &wechat.binding
                && (binding.session_id.trim().is_empty() || binding.chat_id.trim().is_empty())
            {
                return Err(ConfigError::invalid(
                    "wechat.binding.session_id and chat_id must not be empty",
                ));
            }
        }

        if let Some(tunnel) = &mut self.tunnel {
            match tunnel.mode {
                TunnelMode::Token => {
                    if tunnel.token.as_ref().is_none_or(Secret::is_empty) {
                        return Err(ConfigError::invalid(
                            "tunnel.mode = \"token\" requires a non-empty tunnel.token",
                        ));
                    }
                }
                TunnelMode::Config => {
                    let path = tunnel.config_path.as_ref().ok_or_else(|| {
                        ConfigError::invalid("tunnel.mode = \"config\" requires tunnel.config_path")
                    })?;
                    tunnel.config_path = Some(expand_tilde(path));
                }
            }
            if tunnel.origin.is_none() {
                tunnel.origin = Some(format!("http://{bind}"));
            }
        }

        Ok(())
    }

    /// The parsed bind address. Valid by construction.
    ///
    /// # Panics
    /// Never: [`Self::validate`] rejects an unparsable `bind`.
    #[must_use]
    pub fn bind_addr(&self) -> SocketAddr {
        self.gateway
            .bind
            .parse()
            .expect("validate() guarantees bind parses")
    }

    /// Expanded data directory.
    #[must_use]
    pub fn data_dir(&self) -> PathBuf {
        PathBuf::from(&self.gateway.data_dir)
    }

    /// `<data_dir>/gateway.db`
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.data_dir().join("gateway.db")
    }

    /// `<data_dir>/identity`
    #[must_use]
    pub fn identity_dir(&self) -> PathBuf {
        self.data_dir().join("identity")
    }

    /// The configured agents as domain descriptors.
    #[must_use]
    pub fn agent_descriptors(&self) -> Vec<AgentDescriptor> {
        self.agents.iter().map(AgentDescriptor::from).collect()
    }

    /// Create the data directory and prove it is writable.
    ///
    /// # Errors
    /// Fails if the directory cannot be created or written to.
    pub fn ensure_data_dir(&self) -> Result<PathBuf, ConfigError> {
        let dir = self.data_dir();
        std::fs::create_dir_all(&dir).map_err(|source| ConfigError::Io {
            path: dir.clone(),
            source,
        })?;
        let probe = dir.join(".write-probe");
        std::fs::write(&probe, b"").map_err(|source| ConfigError::Io {
            path: probe.clone(),
            source,
        })?;
        std::fs::remove_file(&probe).ok();
        Ok(dir)
    }
}

/// Where the configuration lives when the user did not say otherwise.
///
/// `$AGENT_GATEWAY_CONFIG` wins, then `<data_dir>/config.toml` under the
/// user's home directory.
#[must_use]
pub fn default_config_path() -> PathBuf {
    if let Ok(path) = std::env::var("AGENT_GATEWAY_CONFIG") {
        return PathBuf::from(expand_tilde(&path));
    }
    PathBuf::from(expand_tilde(DEFAULT_DATA_DIR)).join(CONFIG_FILE_NAME)
}

/// A commented starter configuration, written by `agent-gateway config init`.
#[must_use]
pub fn config_template() -> String {
    include_str!("../template.toml").to_owned()
}

fn expand_tilde(value: &str) -> String {
    shellexpand::tilde(value).into_owned()
}

fn default_bind() -> String {
    DEFAULT_BIND.to_owned()
}

fn default_data_dir() -> String {
    DEFAULT_DATA_DIR.to_owned()
}

fn default_log_level() -> String {
    "info".to_owned()
}

fn default_cloudflared() -> String {
    "cloudflared".to_owned()
}

fn default_ticket_ttl() -> Duration {
    Duration::from_secs(60)
}

fn default_pairing_ttl() -> Duration {
    Duration::from_secs(300)
}

fn default_heartbeat() -> Duration {
    Duration::from_secs(30)
}

fn default_ahp_reconnect() -> Duration {
    Duration::from_secs(3)
}

fn default_wechat_api_base() -> String {
    "https://ilinkai.weixin.qq.com".to_owned()
}

fn default_wechat_poll_timeout() -> Duration {
    Duration::from_secs(40)
}

fn default_wechat_qr_timeout() -> Duration {
    Duration::from_secs(300)
}

fn default_wechat_max_text_bytes() -> usize {
    16 * 1024
}

fn default_wechat_max_chunk_bytes() -> usize {
    3500
}

fn valid_wechat_host(host: &str) -> bool {
    let Some(prefix) = host.strip_suffix(".weixin.qq.com") else {
        return false;
    };
    let Some(label) = prefix.strip_prefix("ilink") else {
        return false;
    };
    label.chars().all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
    })
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [[agents]]
        id = "codex"
        name = "Codex"
        command = "codex"
        args = ["--acp"]
    "#;

    #[test]
    fn defaults_are_loopback_and_agents_are_explicit() {
        let config = GatewayConfig::from_toml(MINIMAL).unwrap();
        assert_eq!(config.gateway.bind, DEFAULT_BIND);
        assert!(config.bind_addr().ip().is_loopback());
        assert_eq!(config.agent_descriptors().len(), 1);
        assert!(config.tunnel.is_none());
        assert!(config.ahp.is_none());
    }

    #[test]
    fn ahp_requires_a_websocket_endpoint() {
        let toml = format!("[ahp]\nendpoint = \"https://ahp.example\"\n{MINIMAL}");
        let error = GatewayConfig::from_toml(&toml).unwrap_err();
        assert!(error.to_string().contains("ahp.endpoint must be ws://"));

        let toml = format!(
            "[ahp]\nendpoint = \"wss://ahp.example/channel\"\nreconnect_interval = \"5s\"\n{MINIMAL}"
        );
        let config = GatewayConfig::from_toml(&toml).unwrap();
        assert_eq!(
            config.ahp.unwrap().reconnect_interval,
            Duration::from_secs(5)
        );
    }

    #[test]
    fn a_config_without_agents_is_rejected() {
        let error = GatewayConfig::from_toml("[gateway]\n").unwrap_err();
        assert!(error.to_string().contains("no [[agents]]"));
    }

    #[test]
    fn public_bind_needs_an_explicit_opt_in() {
        let toml = format!("[gateway]\nbind = \"0.0.0.0:48100\"\n{MINIMAL}");
        let error = GatewayConfig::from_toml(&toml).unwrap_err();
        assert!(error.to_string().contains("allow_public_bind"));

        let toml =
            format!("[gateway]\nbind = \"0.0.0.0:48100\"\nallow_public_bind = true\n{MINIMAL}");
        assert!(GatewayConfig::from_toml(&toml).is_ok());
    }

    #[test]
    fn duplicate_agent_ids_are_rejected() {
        let toml = format!("{MINIMAL}{MINIMAL}");
        let error = GatewayConfig::from_toml(&toml).unwrap_err();
        assert!(error.to_string().contains("used twice"));
    }

    #[test]
    fn token_mode_requires_a_token_and_defaults_the_origin() {
        let toml = format!("[tunnel]\nmode = \"token\"\n{MINIMAL}");
        let error = GatewayConfig::from_toml(&toml).unwrap_err();
        assert!(error.to_string().contains("tunnel.token"));

        let toml = format!("[tunnel]\nmode = \"token\"\ntoken = \"abc\"\n{MINIMAL}");
        let config = GatewayConfig::from_toml(&toml).unwrap();
        let tunnel = config.tunnel.unwrap();
        assert_eq!(tunnel.origin.as_deref(), Some("http://127.0.0.1:48100"));
        // The token survives round-tripping but never shows up in a log line.
        assert!(!format!("{tunnel:?}").contains("abc"));
    }

    #[test]
    fn the_shipped_template_is_valid() {
        let mut config: GatewayConfig = toml::from_str(&config_template()).unwrap();
        config.validate().unwrap();
        assert!(!config.agents.is_empty());
    }

    #[test]
    fn unknown_keys_are_reported_instead_of_ignored() {
        let toml = format!("[gateway]\nbnid = \"oops\"\n{MINIMAL}");
        assert!(matches!(
            GatewayConfig::from_toml(&toml),
            Err(ConfigError::Parse { .. })
        ));
    }
}
