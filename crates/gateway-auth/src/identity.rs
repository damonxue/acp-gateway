//! The machine's long-lived Ed25519 identity.
//!
//! The private key never leaves the developer's computer: the relay only ever
//! sees the public key and signatures. Everything that authenticates "this
//! machine" — relay registration, heartbeats, ticket binding — is derived from
//! the key material loaded here.

use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use gateway_core::error::{GatewayError, Result};
use gateway_core::ids::MachineId;
use sha2::{Digest, Sha256};
use tracing::info;

/// File holding the base64-encoded 32-byte private key, mode `0600`.
const PRIVATE_KEY_FILE: &str = "machine.key";
/// File holding the base64-encoded public key, for the user's convenience.
const PUBLIC_KEY_FILE: &str = "machine.pub";

/// A loaded machine identity.
#[derive(Clone)]
pub struct MachineIdentity {
    machine_id: MachineId,
    signing_key: SigningKey,
}

impl std::fmt::Debug for MachineIdentity {
    /// Deliberately prints no key material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MachineIdentity")
            .field("machine_id", &self.machine_id)
            .finish_non_exhaustive()
    }
}

impl MachineIdentity {
    /// Load the identity from `dir`, generating one on first run.
    ///
    /// # Errors
    /// Fails if the directory is not usable or the stored key is malformed.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .map_err(|error| GatewayError::internal(format!("cannot create {dir:?}: {error}")))?;
        let key_path = dir.join(PRIVATE_KEY_FILE);
        if key_path.exists() {
            return Self::load(&key_path);
        }

        let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
        write_private_key(&key_path, &signing_key)?;
        std::fs::write(
            dir.join(PUBLIC_KEY_FILE),
            BASE64.encode(signing_key.verifying_key().as_bytes()),
        )
        .map_err(|error| GatewayError::internal(format!("cannot write public key: {error}")))?;

        let identity = Self::from_signing_key(signing_key);
        info!(machine_id = %identity.machine_id, "generated machine identity");
        Ok(identity)
    }

    fn load(key_path: &PathBuf) -> Result<Self> {
        let encoded = std::fs::read_to_string(key_path)
            .map_err(|error| GatewayError::internal(format!("cannot read machine key: {error}")))?;
        let bytes = BASE64.decode(encoded.trim()).map_err(|error| {
            GatewayError::internal(format!("machine key is not base64: {error}"))
        })?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
            GatewayError::internal("machine key must be exactly 32 bytes".to_owned())
        })?;
        Ok(Self::from_signing_key(SigningKey::from_bytes(&bytes)))
    }

    /// Build an identity from an in-memory key. Used by tests.
    #[must_use]
    pub fn from_signing_key(signing_key: SigningKey) -> Self {
        let machine_id = derive_machine_id(&signing_key.verifying_key());
        Self {
            machine_id,
            signing_key,
        }
    }

    /// Generate a throwaway identity that is never written to disk.
    #[must_use]
    pub fn ephemeral() -> Self {
        Self::from_signing_key(SigningKey::generate(&mut rand::rngs::OsRng))
    }

    /// This machine's id, derived from its public key.
    #[must_use]
    pub fn machine_id(&self) -> &MachineId {
        &self.machine_id
    }

    /// Base64 public key, safe to publish.
    #[must_use]
    pub fn public_key_base64(&self) -> String {
        BASE64.encode(self.signing_key.verifying_key().as_bytes())
    }

    /// Sign a message, e.g. a relay registration challenge.
    #[must_use]
    pub fn sign_base64(&self, message: &[u8]) -> String {
        BASE64.encode(self.signing_key.sign(message).to_bytes())
    }
}

/// Derive a stable, human-readable machine id from a public key.
///
/// Truncating the digest keeps ids short enough to show in a phone UI while
/// staying collision-free in practice; the id is an identifier, not a
/// credential, so 64 bits is ample.
fn derive_machine_id(public_key: &VerifyingKey) -> MachineId {
    let digest = Sha256::digest(public_key.as_bytes());
    let mut id = String::from(MachineId::PREFIX);
    for byte in &digest[..8] {
        id.push_str(&format!("{byte:02x}"));
    }
    MachineId::new(id)
}

#[cfg(unix)]
fn write_private_key(path: &Path, key: &SigningKey) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        // 0600 at creation time: never exists on disk world-readable, not even
        // for the instant between `create` and `set_permissions`.
        .mode(0o600)
        .open(path)
        .map_err(|error| GatewayError::internal(format!("cannot create machine key: {error}")))?;
    file.write_all(BASE64.encode(key.to_bytes()).as_bytes())
        .map_err(|error| GatewayError::internal(format!("cannot write machine key: {error}")))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_key(path: &Path, key: &SigningKey) -> Result<()> {
    std::fs::write(path, BASE64.encode(key.to_bytes()))
        .map_err(|error| GatewayError::internal(format!("cannot write machine key: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identity_is_stable_across_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let first = MachineIdentity::load_or_create(dir.path()).unwrap();
        let second = MachineIdentity::load_or_create(dir.path()).unwrap();
        assert_eq!(first.machine_id(), second.machine_id());
        assert_eq!(first.public_key_base64(), second.public_key_base64());
        assert!(first.machine_id().as_str().starts_with("machine_"));
    }

    #[cfg(unix)]
    #[test]
    fn the_private_key_is_not_readable_by_anyone_else() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        MachineIdentity::load_or_create(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join(PRIVATE_KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn signatures_verify_against_the_published_public_key() {
        use ed25519_dalek::{Signature, Verifier};

        let identity = MachineIdentity::ephemeral();
        let signature = identity.sign_base64(b"register");
        let signature = Signature::from_slice(&BASE64.decode(signature).unwrap()).unwrap();
        let public = VerifyingKey::from_bytes(
            &BASE64
                .decode(identity.public_key_base64())
                .unwrap()
                .try_into()
                .unwrap(),
        )
        .unwrap();
        assert!(public.verify(b"register", &signature).is_ok());
    }

    #[test]
    fn the_debug_output_never_contains_key_material() {
        let identity = MachineIdentity::ephemeral();
        let rendered = format!("{identity:?}");
        assert!(rendered.contains("machine_"));
        assert!(!rendered.contains(&BASE64.encode(identity.signing_key.to_bytes())));
    }
}
