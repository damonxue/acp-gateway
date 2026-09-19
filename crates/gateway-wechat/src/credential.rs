use std::process::Command;
use std::sync::Mutex;

use crate::types::Credentials;

/// Errors returned by a credential backend. The token is deliberately never
/// included in the error text.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("credential store is unavailable: {0}")]
    Unavailable(String),
    #[error("credential store operation failed: {0}")]
    Operation(String),
}

/// Persistent secret boundary used by the adapter. Implementations must store
/// the token in an OS secret service; TOML and SQLite are metadata stores only.
pub trait CredentialStore: Send + Sync {
    fn load(&self) -> Result<Option<Credentials>, CredentialError>;
    fn save(&self, credentials: &Credentials) -> Result<(), CredentialError>;
    fn clear(&self) -> Result<(), CredentialError>;
}

/// In-memory store for tests and short-lived CLI login flows.
#[derive(Debug, Default)]
pub struct MemoryCredentialStore {
    value: Mutex<Option<Credentials>>,
}

impl MemoryCredentialStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn load(&self) -> Result<Option<Credentials>, CredentialError> {
        self.value
            .lock()
            .map_err(|_| CredentialError::Operation("credential lock poisoned".into()))
            .map(|value| value.clone())
    }

    fn save(&self, credentials: &Credentials) -> Result<(), CredentialError> {
        let mut value = self
            .value
            .lock()
            .map_err(|_| CredentialError::Operation("credential lock poisoned".into()))?;
        *value = Some(credentials.clone());
        Ok(())
    }

    fn clear(&self) -> Result<(), CredentialError> {
        let mut value = self
            .value
            .lock()
            .map_err(|_| CredentialError::Operation("credential lock poisoned".into()))?;
        *value = None;
        Ok(())
    }
}

/// macOS Keychain-backed credentials. Other operating systems return a clear
/// unavailable error until their native secret-service adapter is installed;
/// no plaintext fallback is provided.
#[derive(Clone, Debug)]
pub struct KeychainCredentialStore {
    service: String,
    account: String,
}

impl Default for KeychainCredentialStore {
    fn default() -> Self {
        Self {
            service: "agent-gateway/wechat".into(),
            account: "default".into(),
        }
    }
}

impl KeychainCredentialStore {
    fn unavailable() -> CredentialError {
        CredentialError::Unavailable(
            "no native credential backend is implemented for this platform".into(),
        )
    }
}

impl CredentialStore for KeychainCredentialStore {
    fn load(&self) -> Result<Option<Credentials>, CredentialError> {
        if !cfg!(target_os = "macos") {
            return Err(Self::unavailable());
        }
        let output = Command::new("security")
            .args([
                "find-generic-password",
                "-s",
                &self.service,
                "-a",
                &self.account,
                "-w",
            ])
            .output()
            .map_err(|error| CredentialError::Unavailable(error.to_string()))?;
        if !output.status.success() {
            return Ok(None);
        }
        serde_json::from_slice(&output.stdout)
            .map(Some)
            .map_err(|error| {
                CredentialError::Operation(format!("invalid keychain record: {error}"))
            })
    }

    fn save(&self, credentials: &Credentials) -> Result<(), CredentialError> {
        if !cfg!(target_os = "macos") {
            return Err(Self::unavailable());
        }
        let value = serde_json::to_string(credentials)
            .map_err(|error| CredentialError::Operation(error.to_string()))?;
        let output = Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-s",
                &self.service,
                "-a",
                &self.account,
                "-w",
                &value,
            ])
            .output()
            .map_err(|error| CredentialError::Unavailable(error.to_string()))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(CredentialError::Operation(
                "security keychain save failed".into(),
            ))
        }
    }

    fn clear(&self) -> Result<(), CredentialError> {
        if !cfg!(target_os = "macos") {
            return Err(Self::unavailable());
        }
        let output = Command::new("security")
            .args([
                "delete-generic-password",
                "-s",
                &self.service,
                "-a",
                &self.account,
            ])
            .output()
            .map_err(|error| CredentialError::Unavailable(error.to_string()))?;
        if output.status.success() {
            Ok(())
        } else {
            // Deleting an already absent item is idempotent for logout.
            Ok(())
        }
    }
}
