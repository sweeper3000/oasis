use super::{crypto, remote::Remote, validate_name};
use anyhow::{ensure, Result};
use rand::{rngs::OsRng, RngCore};
use rocket::serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Pool, Sqlite};
use std::collections::HashSet;
use zeroize::Zeroizing;

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(crate = "rocket::serde", deny_unknown_fields)]
pub struct KeyDerivation {
    pub version: u8,
    pub salt: [u8; 16],
}

#[derive(Clone)]
pub(crate) struct PreparedKey {
    source: [u8; 32],
    key: Zeroizing<[u8; 32]>,
}

#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(crate = "rocket::serde", rename_all = "snake_case")]
pub enum StorageMode {
    #[default]
    Mount,
    Main,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(crate = "rocket::serde", rename_all = "snake_case")]
pub enum Protocol {
    S3,
    B2,
    Sftp,
    Ftps,
    Ftp,
    Webdav,
    Smb,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(crate = "rocket::serde", default, deny_unknown_fields)]
pub struct StorageConfig {
    pub enabled: bool,
    pub mode: StorageMode,
    pub main_provider: String,
    pub providers: Vec<Provider>,
}

// Never derive Debug: these values contain credentials and encryption keys.
#[derive(Clone, Deserialize, Serialize)]
#[serde(crate = "rocket::serde", default, deny_unknown_fields)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    pub root: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub endpoint: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: String,
    pub key_file: String,
    pub known_hosts_file: String,
    pub domain: String,
    pub implicit_tls: bool,
    pub encryption: bool,
    pub encryption_key: String,
    pub key_fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_derivation: Option<KeyDerivation>,
    #[serde(skip)]
    pub(crate) prepared_key: Option<PreparedKey>,
    pub password_set: bool,
    pub secret_access_key_set: bool,
    pub session_token_set: bool,
    pub encryption_key_set: bool,
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            protocol: Protocol::S3,
            root: String::new(),
            host: String::new(),
            port: 0,
            username: String::new(),
            password: String::new(),
            endpoint: String::new(),
            region: "us-east-1".into(),
            access_key_id: String::new(),
            secret_access_key: String::new(),
            session_token: String::new(),
            key_file: String::new(),
            known_hosts_file: String::new(),
            domain: String::new(),
            implicit_tls: false,
            encryption: false,
            encryption_key: String::new(),
            key_fingerprint: String::new(),
            key_derivation: None,
            prepared_key: None,
            password_set: false,
            secret_access_key_set: false,
            session_token_set: false,
            encryption_key_set: false,
        }
    }
}

impl Provider {
    fn backblaze_s3(&self) -> bool {
        self.protocol == Protocol::S3
            && rocket::http::uri::Absolute::parse(&self.endpoint)
                .ok()
                .and_then(|url| url.authority().map(|a| a.host().to_ascii_lowercase()))
                .map(|host| host.starts_with("s3.") && host.ends_with(".backblazeb2.com"))
                .unwrap_or(false)
    }

    fn b2_namespace(&self) -> bool {
        self.protocol == Protocol::B2 || self.backblaze_s3()
    }

    fn same_namespace(&self, old: &Self) -> bool {
        self.root.trim_matches('/') == old.root.trim_matches('/')
            && ((self.b2_namespace() && old.b2_namespace())
                || (self.protocol == old.protocol
                    && self.host == old.host
                    && self.endpoint == old.endpoint
                    && self.port == old.port))
    }

    fn secret(&self) -> Zeroizing<String> {
        Zeroizing::new(
            std::env::var("OASIS_STORAGE_KEY").unwrap_or_else(|_| self.encryption_key.clone()),
        )
    }

    fn validate_secret(value: &str) -> Result<()> {
        ensure!(
            value.len() <= 1024,
            "Encryption secret must be at most 1,024 UTF-8 bytes"
        );
        if crypto::parse_key(value).is_err() {
            ensure!(value.chars().count() >= 12 && !value.trim().is_empty(),
                "Encryption passphrase must contain at least 12 characters, or use a 64-character hexadecimal key");
        }
        Ok(())
    }

    async fn prepare_key(&mut self, recover: bool) -> Result<()> {
        if !self.encryption {
            return Ok(());
        }
        let value = self.secret();
        Self::validate_secret(&value)?;
        if crypto::parse_key(&value).is_ok() {
            ensure!(
                self.key_derivation.is_none(),
                "Restore the original encryption passphrase"
            );
            self.key()?;
            return Ok(());
        }
        if self.key_derivation.is_none() {
            ensure!(recover, "Missing passphrase derivation metadata; reconnect this provider in storage settings");
            self.key_derivation = match Remote::new(self.clone()).await?.read_namespace().await? {
                Some(marker) => Some(
                    marker
                        .derivation()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "This storage uses a raw hexadecimal key; restore its original key"
                            )
                        })?
                        .clone(),
                ),
                None => {
                    let mut salt = [0; 16];
                    OsRng.fill_bytes(&mut salt);
                    Some(KeyDerivation { version: 1, salt })
                }
            };
        }
        let derivation = self.key_derivation.as_ref().unwrap();
        ensure!(
            derivation.version == 1,
            "Unsupported storage key derivation version"
        );
        let salt = derivation.salt;
        let source = Sha256::digest(value.as_bytes()).into();
        let key = tokio::task::spawn_blocking(move || -> Result<Zeroizing<[u8; 32]>> {
            use argon2::{Algorithm, Argon2, Params, Version};
            let params = Params::new(65536, 3, 1, Some(32))
                .map_err(|_| anyhow::anyhow!("Invalid derivation parameters"))?;
            let mut key = Zeroizing::new([0; 32]);
            Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
                .hash_password_into(value.as_bytes(), &salt, &mut *key)
                .map_err(|_| anyhow::anyhow!("Could not derive storage key"))?;
            Ok(key)
        })
        .await??;
        self.prepared_key = Some(PreparedKey { source, key });
        self.key()?;
        Ok(())
    }

    pub fn key(&self) -> Result<Option<[u8; 32]>> {
        if !self.encryption {
            return Ok(None);
        }
        let value = self.secret();
        Self::validate_secret(&value)?;
        let key = if let Ok(key) = crypto::parse_key(&value) {
            ensure!(
                self.key_derivation.is_none(),
                "Restore the original encryption passphrase"
            );
            key
        } else {
            let prepared = self
                .prepared_key
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Encryption passphrase has not been prepared"))?;
            ensure!(
                prepared.source == <[u8; 32]>::from(Sha256::digest(value.as_bytes())),
                "Encryption secret changed; restore the original secret"
            );
            *prepared.key
        };
        ensure!(self.key_fingerprint.is_empty() || self.key_fingerprint == crypto::fingerprint(&key),
            "Encryption key changed. Restore the original key; changing it makes existing files unreadable");
        Ok(Some(key))
    }
}

impl StorageConfig {
    pub fn redacted(&self) -> Self {
        let mut config = self.clone();
        for provider in &mut config.providers {
            provider.password_set = !provider.password.is_empty();
            provider.secret_access_key_set = !provider.secret_access_key.is_empty();
            provider.session_token_set = !provider.session_token.is_empty();
            provider.encryption_key_set =
                !provider.encryption_key.is_empty() || std::env::var("OASIS_STORAGE_KEY").is_ok();
            provider.password.clear();
            provider.secret_access_key.clear();
            provider.session_token.clear();
            provider.encryption_key.clear();
            provider.key_fingerprint.clear();
            provider.key_derivation = None;
            provider.prepared_key = None;
        }
        config
    }

    pub fn merge_secrets(&mut self, previous: &Self) {
        for provider in &mut self.providers {
            // Fingerprints supplied by clients must never bypass key-change checks.
            provider.key_fingerprint.clear();
            provider.key_derivation = None;
            provider.prepared_key = None;
            if let Some(old) = previous.providers.iter().find(|p| p.id == provider.id) {
                if provider.password.is_empty() {
                    provider.password = old.password.clone();
                }
                if provider.secret_access_key.is_empty() {
                    provider.secret_access_key = old.secret_access_key.clone();
                }
                if provider.session_token.is_empty() {
                    provider.session_token = old.session_token.clone();
                }
                if provider.encryption_key.is_empty() {
                    provider.encryption_key = old.encryption_key.clone();
                }
                // Prevent accidental environment/settings key changes for this namespace.
                if provider.same_namespace(old) {
                    provider.key_fingerprint = old.key_fingerprint.clone();
                    provider.key_derivation = old.key_derivation.clone();
                }
                if provider.protocol == Protocol::B2
                    && old.protocol == Protocol::S3
                    && provider.endpoint == old.endpoint
                {
                    provider.endpoint.clear();
                }
            }
            if provider.protocol == Protocol::B2 {
                if provider.endpoint.starts_with("https://s3.")
                    && provider.endpoint.contains(".backblazeb2.com")
                {
                    provider.endpoint.clear();
                }
                provider.region.clear();
                provider.session_token.clear();
            }
        }
    }

    pub fn validate(&mut self, local_root: &str) -> Result<()> {
        if !self.enabled || self.mode == StorageMode::Mount {
            ensure!(
                std::path::Path::new(local_root).is_dir(),
                "Select an existing local storage directory before using local or mounted storage"
            );
        }
        ensure!(
            self.providers.len() <= 16,
            "At most 16 external providers are allowed"
        );
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        for provider in &mut self.providers {
            ensure!(
                !provider.id.is_empty()
                    && provider
                        .id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                "Invalid provider id"
            );
            ensure!(ids.insert(provider.id.clone()), "Duplicate provider id");
            validate_name(&provider.name)?;
            ensure!(
                names.insert(provider.name.to_lowercase()),
                "Drive names must be unique"
            );
            if self.enabled && self.mode == StorageMode::Mount {
                let root = std::path::Path::new(local_root);
                if root.is_dir() {
                    for entry in std::fs::read_dir(root)? {
                        ensure!(
                            entry?.file_name().to_string_lossy().to_lowercase()
                                != provider.name.to_lowercase(),
                            "A local entry already uses this drive name"
                        );
                    }
                }
            }
            ensure!(
                !provider.root.contains('\\')
                    && !provider.root.contains('\0')
                    && !provider.root.split('/').any(|p| p == ".." || p == "."),
                "Invalid provider root"
            );
            ensure!(
                !provider.password.contains(['\r', '\n']),
                "Password cannot contain a newline"
            );
            match provider.protocol {
                Protocol::B2 => {
                    ensure!(
                        !provider.root.trim_matches('/').is_empty(),
                        "B2 root must include an existing bucket"
                    );
                    ensure!(
                        !provider.access_key_id.is_empty()
                            && !provider.secret_access_key.is_empty(),
                        "B2 Application Key ID and Application Key are required"
                    );
                    ensure!(
                        provider.endpoint.is_empty()
                            || provider.endpoint.starts_with("https://")
                            || provider.endpoint.starts_with("http://"),
                        "B2 API endpoint must be an HTTP(S) URL"
                    );
                }
                Protocol::S3 => {
                    ensure!(
                        !provider.root.trim_matches('/').is_empty(),
                        "S3 root must include a bucket"
                    );
                    ensure!(
                        provider.endpoint.is_empty()
                            || provider.endpoint.starts_with("https://")
                            || provider.endpoint.starts_with("http://"),
                        "S3 endpoint must be an HTTP(S) URL"
                    );
                }
                Protocol::Webdav => ensure!(
                    provider.endpoint.starts_with("https://")
                        || provider.endpoint.starts_with("http://"),
                    "WebDAV endpoint must be an HTTP(S) URL"
                ),
                Protocol::Sftp => {
                    ensure!(!provider.host.is_empty(), "SFTP host is required");
                    ensure!(
                        !provider.known_hosts_file.is_empty()
                            && provider.known_hosts_file != "none",
                        "SFTP requires a known_hosts file for host key verification"
                    );
                }
                _ => ensure!(!provider.host.is_empty(), "Provider host is required"),
            }
            if self.enabled
                && (self.mode == StorageMode::Mount || provider.id == self.main_provider)
            {
                if provider.encryption {
                    Provider::validate_secret(&provider.secret())?;
                }
                if provider.encryption
                    && provider
                        .key_derivation
                        .as_ref()
                        .map(|k| k.version != 1)
                        .unwrap_or(false)
                {
                    anyhow::bail!("Unsupported storage key derivation version");
                }
                // Preparation is asynchronous. Structural validation runs first;
                // the same validation runs again with a prepared passphrase key.
                if provider.encryption
                    && (crypto::parse_key(&provider.secret()).is_ok()
                        || provider.prepared_key.is_some())
                {
                    let key = provider.key()?.unwrap();
                    provider.key_fingerprint = crypto::fingerprint(&key);
                }
            }
        }
        for (index, provider) in self.providers.iter().enumerate() {
            for other in &self.providers[index + 1..] {
                let same_server = (provider.b2_namespace() && other.b2_namespace())
                    || (provider.protocol == other.protocol
                        && provider.host.eq_ignore_ascii_case(&other.host)
                        && provider.port == other.port
                        && provider.endpoint.trim_end_matches('/')
                            == other.endpoint.trim_end_matches('/')
                        && (provider.protocol == Protocol::S3
                            || provider.username == other.username));
                let root = provider.root.trim_matches('/');
                let other_root = other.root.trim_matches('/');
                if same_server {
                    ensure!(
                        !(root == other_root
                            || root.is_empty()
                            || other_root.is_empty()
                            || root.starts_with(&format!("{other_root}/"))
                            || other_root.starts_with(&format!("{root}/"))),
                        "Providers on the same server must use separate, non-overlapping roots"
                    );
                }
            }
        }
        if self.enabled {
            ensure!(!self.providers.is_empty(), "Add an external provider first");
            if self.mode == StorageMode::Main {
                ensure!(
                    self.providers.iter().any(|p| p.id == self.main_provider),
                    "Select the main provider"
                );
            }
        }
        Ok(())
    }

    pub async fn check_connections(&self, initialize: bool) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        super::remote::verify_adapter().await?;
        for provider in self
            .providers
            .iter()
            .filter(|p| self.mode == StorageMode::Mount || p.id == self.main_provider)
        {
            Remote::new(provider.clone())
                .await?
                .check_namespace(initialize)
                .await?;
        }
        Ok(())
    }

    pub async fn prepare_keys(&mut self, recover: bool) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mode = &self.mode;
        let main = &self.main_provider;
        for provider in self
            .providers
            .iter_mut()
            .filter(|p| *mode == StorageMode::Mount || p.id == *main)
        {
            provider.prepare_key(recover).await?;
        }
        Ok(())
    }

    pub async fn load(pool: &Pool<Sqlite>, secret: &str) -> Result<Self> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT sealed_config FROM storage_config WHERE id = 1")
                .fetch_optional(pool)
                .await?;
        let mut config: Self = match row {
            None => Self::default(),
            Some((sealed,)) => {
                serde_json::from_slice(&crypto::unseal(&settings_key(secret), &sealed)?)?
            }
        };
        config.prepare_keys(false).await?;
        Ok(config)
    }

    pub async fn save(&self, pool: &Pool<Sqlite>, secret: &str) -> Result<()> {
        ensure!(
            !secret.is_empty(),
            "Finish site setup before configuring storage"
        );
        let sealed = crypto::seal(&settings_key(secret), &serde_json::to_vec(self)?)?;
        sqlx::query("INSERT INTO storage_config (id, sealed_config) VALUES (1, ?) ON CONFLICT(id) DO UPDATE SET sealed_config = excluded.sealed_config")
            .bind(sealed).execute(pool).await?;
        Ok(())
    }
}

pub(crate) fn settings_key(secret: &str) -> [u8; 32] {
    Sha256::digest(&[b"oasis-storage-settings-v1".as_ref(), secret.as_bytes()].concat())
        .as_slice()
        .try_into()
        .unwrap()
}

use std::convert::TryInto;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_b2_conversion_preserves_secrets_keys_and_rejects_alias_overlap() {
        let local = tempfile::tempdir().unwrap();
        let mut old = config();
        old.providers[0].endpoint = "https://s3.ca-east-006.backblazeb2.com".into();
        old.providers[0].access_key_id = "application-key-id".into();
        old.providers[0].encryption_key = "old passphrase with spaces".into();
        old.providers[0].key_derivation = Some(KeyDerivation {
            version: 1,
            salt: [19; 16],
        });
        old.prepare_keys(false).await.unwrap();
        old.validate(local.path().to_str().unwrap()).unwrap();
        let key = old.providers[0].key().unwrap().unwrap();
        let mut updated = old.redacted();
        updated.providers[0].protocol = Protocol::B2;
        updated.merge_secrets(&old);
        assert!(updated.providers[0].endpoint.is_empty());
        assert!(updated.providers[0].region.is_empty());
        assert_eq!(
            updated.providers[0].secret_access_key,
            old.providers[0].secret_access_key
        );
        assert_eq!(
            updated.providers[0].key_fingerprint,
            old.providers[0].key_fingerprint
        );
        assert!(updated.providers[0].key_derivation == old.providers[0].key_derivation);
        updated.prepare_keys(false).await.unwrap();
        assert_eq!(updated.providers[0].key().unwrap().unwrap(), key);
        let mut alias = old.providers[0].clone();
        alias.id = "alias".into();
        alias.name = "Alias".into();
        alias.root += "/nested";
        updated.providers.push(alias);
        assert!(updated.validate(local.path().to_str().unwrap()).is_err());
        updated.providers[1].root = "other-bucket".into();
        updated.validate(local.path().to_str().unwrap()).unwrap();
        updated.providers[0].secret_access_key.clear();
        assert!(updated.validate(local.path().to_str().unwrap()).is_err());
    }

    fn config() -> StorageConfig {
        StorageConfig {
            enabled: true,
            main_provider: "drive-1".into(),
            providers: vec![Provider {
                id: "drive-1".into(),
                name: "Archive".into(),
                root: "bucket/archive".into(),
                password: "test-password".into(),
                secret_access_key: "test-secret".into(),
                encryption: true,
                encryption_key: "17".repeat(32),
                ..Provider::default()
            }],
            ..StorageConfig::default()
        }
    }

    #[tokio::test]
    async fn passphrases_preserve_bytes_validate_limits_and_never_serialize_cached_keys() {
        for invalid in [
            "",
            "short",
            "            ",
            &"x".repeat(1025),
            &"界".repeat(342),
        ] {
            assert!(Provider::validate_secret(invalid).is_err());
        }
        for value in [
            "Twelve chars!",
            "  spaces matter  ",
            "日本語の暗号化パスフレーズ",
            &"A7".repeat(32),
        ] {
            assert!(Provider::validate_secret(value).is_ok());
        }
        let mut p = config().providers.remove(0);
        p.encryption_key = "a7".repeat(32);
        assert!(!serde_json::to_string(&p)
            .unwrap()
            .contains("key_derivation"));
        let lower = p.key().unwrap().unwrap();
        p.encryption_key = p.encryption_key.to_uppercase();
        assert_eq!(p.key().unwrap().unwrap(), lower);
        p.encryption_key = "A long passphrase 日本語!".into();
        p.key_derivation = Some(KeyDerivation {
            version: 1,
            salt: [5; 16],
        });
        p.prepare_key(false).await.unwrap();
        let key = p.key().unwrap().unwrap();
        p.key_fingerprint = crypto::fingerprint(&key);
        let mut other = p.clone();
        other.key_fingerprint.clear();
        other.key_derivation.as_mut().unwrap().salt[0] ^= 1;
        other.prepare_key(false).await.unwrap();
        assert_ne!(other.key().unwrap().unwrap(), key);
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("prepared_key") && !json.contains("source"));
        let mut restored: Provider = serde_json::from_str(&json).unwrap();
        assert!(restored.key().is_err());
        restored.prepare_key(false).await.unwrap();
        assert_eq!(restored.key().unwrap().unwrap(), key);
        restored.encryption_key.push(' ');
        assert!(restored.key().is_err());
        assert!(restored.prepare_key(false).await.is_err());
        restored.key_derivation.as_mut().unwrap().version = 2;
        assert!(restored.prepare_key(false).await.is_err());
        p.key_derivation = None;
        p.prepared_key = None;
        assert!(p.prepare_key(false).await.is_err());
    }

    #[test]
    fn redacts_preserves_secrets_and_rejects_key_change_and_collisions() {
        let root = tempfile::tempdir().unwrap();
        let local = root.path().to_str().unwrap();
        let mut config = config();
        config.validate(local).unwrap();
        let previous = config.clone();
        let mut edited = config.redacted();
        assert!(edited.providers[0].password.is_empty());
        assert!(edited.providers[0].secret_access_key.is_empty());
        assert!(edited.providers[0].encryption_key.is_empty());
        assert!(edited.providers[0].password_set && edited.providers[0].encryption_key_set);
        edited.merge_secrets(&previous);
        assert_eq!(edited.providers[0].password, "test-password");
        assert_eq!(edited.providers[0].encryption_key, "17".repeat(32));
        edited.providers[0].encryption_key = "18".repeat(32);
        assert!(edited.validate(local).is_err());
        std::fs::create_dir(root.path().join("archive")).unwrap();
        assert!(config.validate(local).is_err());
    }

    #[tokio::test]
    async fn settings_survive_restart_without_plaintext_credentials() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE storage_config (id INTEGER PRIMARY KEY, sealed_config TEXT NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        config().save(&pool, "site-test-secret").await.unwrap();
        let (stored,): (String,) = sqlx::query_as("SELECT sealed_config FROM storage_config")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!stored.contains("test-password") && !stored.contains("test-secret"));
        let restored = StorageConfig::load(&pool, "site-test-secret")
            .await
            .unwrap();
        assert_eq!(restored.providers[0].password, "test-password");
        assert!(StorageConfig::load(&pool, "wrong-secret").await.is_err());
    }
}
