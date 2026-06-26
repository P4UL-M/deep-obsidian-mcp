use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use deep_obsidian_types::{AuthConfig, EmbeddingConfig, SecretRef};
use rand::rngs::OsRng;
use rand::RngCore;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::default_secrets_path;

const ENCRYPTED_FILE_VERSION: u32 = 1;
const ENCRYPTED_FILE_CIPHER: &str = "xchacha20poly1305";
const APP_SECRET_KEY: [u8; 32] = [
    0x44, 0x65, 0x65, 0x70, 0x4f, 0x62, 0x73, 0x69, 0x64, 0x69, 0x61, 0x6e, 0x4d, 0x43, 0x50, 0x2d,
    0x73, 0x74, 0x61, 0x74, 0x69, 0x63, 0x2d, 0x6b, 0x65, 0x79, 0x2d, 0x76, 0x30, 0x30, 0x31, 0x21,
];

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("OS keyring is unavailable: {0}")]
    OsKeyringUnavailable(String),
    #[error("secret not found")]
    MissingSecret,
    #[error("encrypted secret file is corrupt: {0}")]
    CorruptEncryptedFile(String),
    #[error("failed to decrypt secret")]
    DecryptFailed,
    #[error("secret I/O failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize secret file {path}: {source}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

pub trait SecretStore {
    fn get(&self, reference: &SecretRef) -> Result<Option<SecretString>, SecretError>;
    fn put(&self, reference: &SecretRef, value: SecretString) -> Result<(), SecretError>;
    fn delete(&self, reference: &SecretRef) -> Result<(), SecretError>;
}

#[derive(Debug, Clone, Default)]
pub struct OsKeyringStore;

impl OsKeyringStore {
    fn entry(service: &str, account: &str) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(service, account)
            .map_err(|error| SecretError::OsKeyringUnavailable(error.to_string()))
    }
}

impl SecretStore for OsKeyringStore {
    fn get(&self, reference: &SecretRef) -> Result<Option<SecretString>, SecretError> {
        let SecretRef::OsKeyring { service, account } = reference else {
            return Ok(None);
        };
        let entry = Self::entry(service, account)?;
        match entry.get_password() {
            Ok(value) => Ok(Some(SecretString::new(value))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(SecretError::OsKeyringUnavailable(error.to_string())),
        }
    }

    fn put(&self, reference: &SecretRef, value: SecretString) -> Result<(), SecretError> {
        let SecretRef::OsKeyring { service, account } = reference else {
            return Ok(());
        };
        Self::entry(service, account)?
            .set_password(value.expose_secret())
            .map_err(|error| SecretError::OsKeyringUnavailable(error.to_string()))
    }

    fn delete(&self, reference: &SecretRef) -> Result<(), SecretError> {
        let SecretRef::OsKeyring { service, account } = reference else {
            return Ok(());
        };
        match Self::entry(service, account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(SecretError::OsKeyringUnavailable(error.to_string())),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EncryptedFileStore {
    path: PathBuf,
}

impl Default for EncryptedFileStore {
    fn default() -> Self {
        Self {
            path: default_secrets_path(),
        }
    }
}

impl EncryptedFileStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read_file(&self) -> Result<EncryptedSecretsFile, SecretError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(EncryptedSecretsFile::default());
            }
            Err(source) => {
                return Err(SecretError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let file: EncryptedSecretsFile = serde_json::from_str(&text)
            .map_err(|error| SecretError::CorruptEncryptedFile(error.to_string()))?;
        if file.version != ENCRYPTED_FILE_VERSION || file.cipher != ENCRYPTED_FILE_CIPHER {
            return Err(SecretError::CorruptEncryptedFile(
                "unsupported encrypted secret file version or cipher".to_string(),
            ));
        }
        Ok(file)
    }

    fn write_file(&self, file: &EncryptedSecretsFile) -> Result<(), SecretError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| SecretError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let text = serde_json::to_string_pretty(file).map_err(|source| SecretError::Serialize {
            path: self.path.clone(),
            source,
        })?;
        let temp_path = self.path.with_extension("json.tmp");
        {
            let mut temp = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temp_path)
                .map_err(|source| SecretError::Io {
                    path: temp_path.clone(),
                    source,
                })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                temp.set_permissions(fs::Permissions::from_mode(0o600))
                    .map_err(|source| SecretError::Io {
                        path: temp_path.clone(),
                        source,
                    })?;
            }
            temp.write_all(text.as_bytes())
                .and_then(|_| temp.write_all(b"\n"))
                .and_then(|_| temp.sync_all())
                .map_err(|source| SecretError::Io {
                    path: temp_path.clone(),
                    source,
                })?;
        }
        fs::rename(&temp_path, &self.path).map_err(|source| SecretError::Io {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }

    fn encrypt(value: SecretString) -> Result<EncryptedSecretItem, SecretError> {
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&APP_SECRET_KEY));
        let mut nonce = [0_u8; 24];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = cipher
            .encrypt(XNonce::from_slice(&nonce), value.expose_secret().as_bytes())
            .map_err(|_| SecretError::DecryptFailed)?;
        Ok(EncryptedSecretItem {
            nonce: BASE64.encode(nonce),
            ciphertext: BASE64.encode(ciphertext),
        })
    }

    fn decrypt(item: &EncryptedSecretItem) -> Result<SecretString, SecretError> {
        let nonce = BASE64
            .decode(&item.nonce)
            .map_err(|error| SecretError::CorruptEncryptedFile(error.to_string()))?;
        let ciphertext = BASE64
            .decode(&item.ciphertext)
            .map_err(|error| SecretError::CorruptEncryptedFile(error.to_string()))?;
        if nonce.len() != 24 {
            return Err(SecretError::CorruptEncryptedFile(
                "invalid XChaCha20Poly1305 nonce length".to_string(),
            ));
        }
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&APP_SECRET_KEY));
        let plaintext = cipher
            .decrypt(XNonce::from_slice(&nonce), ciphertext.as_ref())
            .map_err(|_| SecretError::DecryptFailed)?;
        let secret = String::from_utf8(Zeroizing::new(plaintext).to_vec())
            .map_err(|error| SecretError::CorruptEncryptedFile(error.to_string()))?;
        Ok(SecretString::new(secret))
    }
}

impl SecretStore for EncryptedFileStore {
    fn get(&self, reference: &SecretRef) -> Result<Option<SecretString>, SecretError> {
        let SecretRef::EncryptedFile { id } = reference else {
            return Ok(None);
        };
        let file = self.read_file()?;
        file.items.get(id).map(Self::decrypt).transpose()
    }

    fn put(&self, reference: &SecretRef, value: SecretString) -> Result<(), SecretError> {
        let SecretRef::EncryptedFile { id } = reference else {
            return Ok(());
        };
        let mut file = self.read_file()?;
        file.items.insert(id.clone(), Self::encrypt(value)?);
        self.write_file(&file)
    }

    fn delete(&self, reference: &SecretRef) -> Result<(), SecretError> {
        let SecretRef::EncryptedFile { id } = reference else {
            return Ok(());
        };
        let mut file = self.read_file()?;
        file.items.remove(id);
        self.write_file(&file)
    }
}

#[derive(Debug, Clone, Default)]
pub struct SecretResolver {
    os_keyring: OsKeyringStore,
    encrypted_file: EncryptedFileStore,
}

impl SecretResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_encrypted_file_path(encrypted_file_path: PathBuf) -> Self {
        Self {
            os_keyring: OsKeyringStore,
            encrypted_file: EncryptedFileStore::new(encrypted_file_path),
        }
    }

    pub fn get(&self, reference: &SecretRef) -> Result<Option<SecretString>, SecretError> {
        match reference {
            SecretRef::OsKeyring { .. } => self.os_keyring.get(reference),
            SecretRef::EncryptedFile { .. } => self.encrypted_file.get(reference),
        }
    }

    pub fn put(&self, reference: &SecretRef, value: SecretString) -> Result<(), SecretError> {
        match reference {
            SecretRef::OsKeyring { .. } => self.os_keyring.put(reference, value),
            SecretRef::EncryptedFile { .. } => self.encrypted_file.put(reference, value),
        }
    }

    pub fn delete(&self, reference: &SecretRef) -> Result<(), SecretError> {
        match reference {
            SecretRef::OsKeyring { .. } => self.os_keyring.delete(reference),
            SecretRef::EncryptedFile { .. } => self.encrypted_file.delete(reference),
        }
    }

    pub fn resolve_embedding_api_key(
        &self,
        embedding: &EmbeddingConfig,
    ) -> Result<Option<SecretString>, SecretError> {
        let Some(reference) = &embedding.api_key_ref else {
            return Ok(None);
        };
        self.get(reference)?
            .ok_or(SecretError::MissingSecret)
            .map(Some)
    }

    /// Resolve the HTTP bearer token referenced by [`AuthConfig::token_ref`].
    /// Returns `Ok(None)` when no reference is configured; errors when a
    /// reference is set but the underlying secret is missing.
    pub fn resolve_auth_token(
        &self,
        auth: &AuthConfig,
    ) -> Result<Option<SecretString>, SecretError> {
        let Some(reference) = &auth.token_ref else {
            return Ok(None);
        };
        self.get(reference)?
            .ok_or(SecretError::MissingSecret)
            .map(Some)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EncryptedSecretsFile {
    version: u32,
    cipher: String,
    items: BTreeMap<String, EncryptedSecretItem>,
}

impl Default for EncryptedSecretsFile {
    fn default() -> Self {
        Self {
            version: ENCRYPTED_FILE_VERSION,
            cipher: ENCRYPTED_FILE_CIPHER.to_string(),
            items: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EncryptedSecretItem {
    nonce: String,
    ciphertext: String,
}

#[cfg(test)]
mod tests {
    use super::{EncryptedFileStore, SecretError, SecretStore};
    use deep_obsidian_types::SecretRef;
    use secrecy::{ExposeSecret, SecretString};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "deep-obsidian-{name}-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        ))
    }

    fn reference() -> SecretRef {
        SecretRef::EncryptedFile {
            id: "openai-embedding".to_string(),
        }
    }

    #[test]
    fn encrypted_file_can_put_get_delete() {
        let path = temp_path("secret-roundtrip");
        let store = EncryptedFileStore::new(path.clone());
        let reference = reference();

        store
            .put(&reference, SecretString::new("super-secret".to_string()))
            .expect("put secret");
        let loaded = store.get(&reference).expect("get secret").expect("secret");
        assert_eq!(loaded.expose_secret(), "super-secret");

        store.delete(&reference).expect("delete secret");
        assert!(store.get(&reference).expect("get missing").is_none());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn encrypted_file_does_not_store_plaintext() {
        let path = temp_path("secret-plaintext");
        let store = EncryptedFileStore::new(path.clone());
        store
            .put(&reference(), SecretString::new("super-secret".to_string()))
            .expect("put secret");
        let text = fs::read_to_string(&path).expect("read secret file");
        assert!(!text.contains("super-secret"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn encrypted_file_uses_unique_nonce_per_write() {
        let path = temp_path("secret-nonce");
        let store = EncryptedFileStore::new(path.clone());
        let reference = reference();
        store
            .put(&reference, SecretString::new("same-secret".to_string()))
            .expect("first put");
        let first = fs::read_to_string(&path).expect("read first");
        store
            .put(&reference, SecretString::new("same-secret".to_string()))
            .expect("second put");
        let second = fs::read_to_string(&path).expect("read second");
        assert_ne!(first, second);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn encrypted_file_rejects_corrupted_ciphertext() {
        let path = temp_path("secret-corrupt");
        fs::write(
            &path,
            r#"{"version":1,"cipher":"xchacha20poly1305","items":{"openai-embedding":{"nonce":"AAAA","ciphertext":"BBBB"}}}"#,
        )
        .expect("write corrupt file");
        let store = EncryptedFileStore::new(path.clone());
        let error = store.get(&reference()).expect_err("expected corrupt file");
        assert!(matches!(
            error,
            SecretError::CorruptEncryptedFile(_) | SecretError::DecryptFailed
        ));
        let _ = fs::remove_file(path);
    }
}
