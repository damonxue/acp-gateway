//! JSONC-preserving Zed settings editor.
//!
//! 保留 JSONC 注释与格式的 Zed 配置编辑器。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use directories::BaseDirs;
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstObject, CstObjectProp, CstRootNode};
use serde::{Deserialize, Serialize};

use gateway_core::agent::AgentDescriptor;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ZedAgentServerSpec {
    pub key: String,
    pub command: String,
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ZedMutationRecord {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub agent_servers_existed: bool,
    #[serde(default)]
    pub inserted: Vec<String>,
    #[serde(default)]
    pub overwritten: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ZedAgentServerSummary {
    pub key: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ZedSettingsSnapshot {
    pub settings_path: PathBuf,
    pub backup_path: PathBuf,
    pub record_path: PathBuf,
    pub has_backup: bool,
    pub has_record: bool,
    pub enabled: bool,
    pub agent_servers: Vec<ZedAgentServerSummary>,
}

#[derive(Clone, Debug)]
pub struct ZedSettingsManager {
    settings_path: PathBuf,
    backup_path: PathBuf,
    record_path: PathBuf,
}

impl ZedSettingsManager {
    #[must_use]
    pub fn discover() -> Result<Self> {
        let home = BaseDirs::new()
            .map(|dirs| dirs.home_dir().to_path_buf())
            .ok_or_else(|| anyhow!("cannot determine the home directory"))?;
        let settings_path = home.join(".config").join("zed").join("settings.json");
        let support_dir = home.join(".agent-gateway");
        let backup_path = support_dir.join("zed-settings.backup.json");
        let record_path = support_dir.join("zed-integration.json");
        Ok(Self {
            settings_path,
            backup_path,
            record_path,
        })
    }

    #[must_use]
    pub fn settings_path(&self) -> &Path {
        &self.settings_path
    }

    #[must_use]
    pub fn backup_path(&self) -> &Path {
        &self.backup_path
    }

    #[must_use]
    pub fn record_path(&self) -> &Path {
        &self.record_path
    }

    #[must_use]
    pub fn snapshot(&self) -> Result<ZedSettingsSnapshot> {
        let text = self.read_settings_text()?;
        let settings = parse_snapshot(&text)?;
        let record = self.read_record().ok();
        Ok(ZedSettingsSnapshot {
            settings_path: self.settings_path.clone(),
            backup_path: self.backup_path.clone(),
            record_path: self.record_path.clone(),
            has_backup: self.backup_path.exists(),
            has_record: self.record_path.exists(),
            enabled: record.as_ref().is_some_and(|record| record.enabled),
            agent_servers: settings,
        })
    }

    pub fn enable_for_agents(
        &self,
        agents: &[AgentDescriptor],
        bridge_command: &Path,
    ) -> Result<()> {
        self.ensure_backup()?;
        let text = self.read_settings_text()?;
        let mut record = self.read_record().unwrap_or_default();
        record.enabled = true;

        let root = CstRootNode::parse(&text, &ParseOptions::default())
            .context("cannot parse Zed settings as JSONC")?;
        let root_obj = root.object_value_or_set();
        let agent_servers_existed = root_obj.get("agent_servers").is_some();
        let agent_servers = root_obj.object_value_or_set("agent_servers");
        record.agent_servers_existed = agent_servers_existed;

        for agent in agents {
            let key = format!("{} via Gateway", agent.name);
            let spec = ZedAgentServerSpec {
                key: key.clone(),
                command: bridge_command.to_string_lossy().to_string(),
                args: vec![
                    "acp-bridge".to_owned(),
                    "--agent".to_owned(),
                    agent.id.to_string(),
                ],
                env: BTreeMap::new(),
            };
            self.upsert_agent_server(&agent_servers, &key, &spec, &mut record)?;
        }

        self.write_settings_text(&root.to_string())?;
        self.write_record(&record)?;
        Ok(())
    }

    pub fn disable(&self) -> Result<()> {
        let record = match self.read_record() {
            Ok(record) if record.enabled => record,
            Ok(record) => record,
            Err(_) => return Ok(()),
        };

        let text = self.read_settings_text()?;
        let root = CstRootNode::parse(&text, &ParseOptions::default())
            .context("cannot parse Zed settings as JSONC")?;
        let root_obj = root.object_value_or_set();
        let Some(agent_servers_prop) = root_obj.get("agent_servers") else {
            return Ok(());
        };
        let Some(agent_servers) = agent_servers_prop.value().and_then(|node| node.as_object())
        else {
            return Ok(());
        };

        for key in &record.inserted {
            if let Some(prop) = agent_servers.get(key) {
                prop.remove();
            }
        }

        for (key, value) in &record.overwritten {
            if let Some(prop) = agent_servers.get(key) {
                prop.set_value(json_to_cst(value.clone()));
            } else {
                agent_servers.append(key, json_to_cst(value.clone()));
            }
        }

        if !record.agent_servers_existed && agent_servers.properties().is_empty() {
            if let Some(prop) = root_obj.get("agent_servers") {
                prop.remove();
            }
        }

        self.write_settings_text(&root.to_string())?;
        let mut updated = record;
        updated.enabled = false;
        self.write_record(&updated)?;
        Ok(())
    }

    pub fn restore_backup(&self) -> Result<()> {
        if !self.backup_path.exists() {
            return Ok(());
        }
        let text = fs::read_to_string(&self.backup_path)
            .with_context(|| format!("cannot read {}", self.backup_path.display()))?;
        self.write_settings_text(&text)?;
        if self.record_path.exists() {
            fs::remove_file(&self.record_path)
                .with_context(|| format!("cannot remove {}", self.record_path.display()))?;
        }
        Ok(())
    }

    fn ensure_backup(&self) -> Result<()> {
        if self.backup_path.exists() {
            return Ok(());
        }
        if let Some(parent) = self.backup_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let text = self
            .read_settings_text()
            .unwrap_or_else(|_| "{}\n".to_owned());
        fs::write(&self.backup_path, text)
            .with_context(|| format!("cannot write {}", self.backup_path.display()))?;
        Ok(())
    }

    fn read_settings_text(&self) -> Result<String> {
        if self.settings_path.exists() {
            fs::read_to_string(&self.settings_path)
                .with_context(|| format!("cannot read {}", self.settings_path.display()))
        } else {
            Ok("{}\n".to_owned())
        }
    }

    fn write_settings_text(&self, text: &str) -> Result<()> {
        if let Some(parent) = self.settings_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        fs::write(&self.settings_path, text)
            .with_context(|| format!("cannot write {}", self.settings_path.display()))?;
        Ok(())
    }

    fn read_record(&self) -> Result<ZedMutationRecord> {
        if !self.record_path.exists() {
            return Ok(ZedMutationRecord::default());
        }
        let text = fs::read_to_string(&self.record_path)
            .with_context(|| format!("cannot read {}", self.record_path.display()))?;
        Ok(serde_json::from_str(&text)?)
    }

    fn write_record(&self, record: &ZedMutationRecord) -> Result<()> {
        if let Some(parent) = self.record_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        fs::write(&self.record_path, serde_json::to_string_pretty(record)?)
            .with_context(|| format!("cannot write {}", self.record_path.display()))?;
        Ok(())
    }

    fn upsert_agent_server(
        &self,
        agent_servers: &CstObject,
        key: &str,
        spec: &ZedAgentServerSpec,
        record: &mut ZedMutationRecord,
    ) -> Result<()> {
        let payload = agent_server_spec_to_cst(spec.clone());
        match agent_servers.get(key) {
            Some(existing) => {
                if !record.overwritten.contains_key(key) {
                    let value = existing
                        .value()
                        .and_then(|value| value.to_serde_value())
                        .ok_or_else(|| anyhow!("cannot capture existing Zed agent server"))?;
                    record.overwritten.insert(key.to_owned(), value);
                }
                existing.set_value(payload);
            }
            None => {
                agent_servers.append(key, payload);
                if !record.inserted.iter().any(|entry| entry == key) {
                    record.inserted.push(key.to_owned());
                }
            }
        }
        Ok(())
    }
}

fn parse_snapshot(text: &str) -> Result<Vec<ZedAgentServerSummary>> {
    let value: serde_json::Value =
        jsonc_parser::parse_to_serde_value(text, &ParseOptions::default())
            .context("cannot parse settings.json as JSONC")?;
    let mut items = Vec::new();
    let Some(agent_servers) = value
        .get("agent_servers")
        .and_then(|value| value.as_object())
    else {
        return Ok(items);
    };

    for (key, value) in agent_servers {
        let mut summary = ZedAgentServerSummary {
            key: key.clone(),
            kind: value
                .get("type")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_owned(),
            command: value
                .get("command")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned),
            ..Default::default()
        };
        if let Some(args) = value.get("args").and_then(|value| value.as_array()) {
            summary.args = args
                .iter()
                .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                .collect();
        }
        if let Some(env) = value.get("env").and_then(|value| value.as_object()) {
            summary.env = env
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_owned()))
                })
                .collect();
        }
        items.push(summary);
    }

    Ok(items)
}

fn json_to_cst(value: serde_json::Value) -> CstInputValue {
    match value {
        serde_json::Value::Null => CstInputValue::Null,
        serde_json::Value::Bool(value) => CstInputValue::Bool(value),
        serde_json::Value::Number(value) => CstInputValue::Number(value.to_string()),
        serde_json::Value::String(value) => CstInputValue::String(value),
        serde_json::Value::Array(items) => {
            CstInputValue::Array(items.into_iter().map(json_to_cst).collect())
        }
        serde_json::Value::Object(map) => CstInputValue::Object(
            map.into_iter()
                .map(|(key, value)| (key, json_to_cst(value)))
                .collect(),
        ),
    }
}

fn agent_server_spec_to_cst(spec: ZedAgentServerSpec) -> CstInputValue {
    let env = spec
        .env
        .into_iter()
        .map(|(key, value)| (key, CstInputValue::String(value)))
        .collect::<Vec<_>>();

    CstInputValue::Object(vec![
        (
            "type".to_owned(),
            CstInputValue::String("custom".to_owned()),
        ),
        ("command".to_owned(), CstInputValue::String(spec.command)),
        (
            "args".to_owned(),
            CstInputValue::Array(spec.args.into_iter().map(CstInputValue::String).collect()),
        ),
        ("env".to_owned(), CstInputValue::Object(env)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn manager(root: &Path) -> ZedSettingsManager {
        ZedSettingsManager {
            settings_path: root.join("settings.json"),
            backup_path: root.join("backup.json"),
            record_path: root.join("record.json"),
        }
    }

    fn agent_descriptor() -> AgentDescriptor {
        AgentDescriptor {
            id: gateway_core::ids::AgentId::new("codex"),
            name: "Codex".to_owned(),
            command: "/bin/codex".to_owned(),
            args: vec!["--acp".to_owned()],
            env: BTreeMap::new(),
        }
    }

    fn test_gateway_binary() -> &'static Path {
        Path::new("/tmp/agent-gateway-test")
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    #[test]
    fn parses_agent_servers_from_jsonc() {
        let text = r#"
        {
          // keep this comment
          "agent_servers": {
            "codex via Gateway": {
              "type": "custom",
              "command": "/tmp/agent-gateway-test",
              "args": ["acp-bridge", "--agent", "codex"],
              "env": {}
            }
          }
        }
        "#;
        let items = parse_snapshot(text).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key, "codex via Gateway");
        assert_eq!(items[0].kind, "custom");
    }

    #[test]
    fn converts_agent_server_spec_to_cst() {
        let spec = ZedAgentServerSpec {
            key: "codex".into(),
            command: "/tmp/agent-gateway-test".into(),
            args: vec!["acp-bridge".into(), "--agent".into(), "codex".into()],
            env: BTreeMap::new(),
        };
        let value = agent_server_spec_to_cst(spec);
        let text = format!("{value:?}");
        assert!(text.contains("\"custom\""));
        assert!(text.contains("\"acp-bridge\""));
    }

    #[test]
    fn enable_disable_round_trip_inserts_and_removes_agent_servers() {
        let dir = tempdir().unwrap();
        let manager = manager(dir.path());
        let original = r#"
{
  // keep this comment
  "theme": "dark"
}
"#;
        fs::write(manager.settings_path(), original).unwrap();

        manager
            .enable_for_agents(&[agent_descriptor()], test_gateway_binary())
            .unwrap();

        let enabled = read(manager.settings_path());
        assert!(enabled.contains("// keep this comment"));
        assert!(enabled.contains("\"agent_servers\""));
        assert!(enabled.contains("\"Codex via Gateway\""));
        assert!(enabled.contains("\"acp-bridge\""));
        assert!(manager.backup_path().exists());
        assert!(manager.record_path().exists());

        manager.disable().unwrap();
        let disabled = read(manager.settings_path());
        assert!(disabled.contains("// keep this comment"));
        assert!(!disabled.contains("\"agent_servers\""));
        assert!(manager.record_path().exists());
        let snapshot = manager.snapshot().unwrap();
        assert!(!snapshot.enabled);
    }

    #[test]
    fn enable_disable_round_trip_restores_existing_entry() {
        let dir = tempdir().unwrap();
        let manager = manager(dir.path());
        let original = r#"
{
  "agent_servers": {
    "Codex via Gateway": {
      "type": "custom",
      "command": "/bin/original",
      "args": ["old"],
      "env": {}
    }
  }
}
"#;
        fs::write(manager.settings_path(), original).unwrap();

        manager
            .enable_for_agents(&[agent_descriptor()], test_gateway_binary())
            .unwrap();
        let enabled = read(manager.settings_path());
        assert!(enabled.contains("\"/tmp/agent-gateway-test\""));
        assert!(enabled.contains("\"acp-bridge\""));

        manager.disable().unwrap();
        let disabled = read(manager.settings_path());
        assert!(disabled.contains("\"/bin/original\""));
        assert!(disabled.contains("\"old\""));
        assert!(!disabled.contains("\"acp-bridge\""));
        assert!(parse_snapshot(&disabled).unwrap()[0].command.as_deref() == Some("/bin/original"));
    }

    #[test]
    fn restore_backup_clears_the_integration_record() {
        let dir = tempdir().unwrap();
        let manager = manager(dir.path());
        let original = r#"
{
  "agent_servers": {
    "Codex via Gateway": {
      "type": "custom",
      "command": "/bin/original",
      "args": ["old"],
      "env": {}
    }
  }
}
"#;
        fs::write(manager.settings_path(), original).unwrap();

        manager
            .enable_for_agents(&[agent_descriptor()], test_gateway_binary())
            .unwrap();
        manager.restore_backup().unwrap();

        assert_eq!(read(manager.settings_path()), original);
        assert!(!manager.record_path().exists());
        let snapshot = manager.snapshot().unwrap();
        assert!(!snapshot.enabled);
        assert_eq!(snapshot.agent_servers.len(), 1);
        assert_eq!(
            snapshot.agent_servers[0].command.as_deref(),
            Some("/bin/original")
        );
    }
}
