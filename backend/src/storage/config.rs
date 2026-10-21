use super::{crypto, remote::Remote, validate_name};
use anyhow::{ensure, Result};
use rocket::serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Pool, Sqlite};
use std::collections::HashSet;

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
            password_set: false,
            secret_access_key_set: false,
            session_token_set: false,
            encryption_key_set: false,
        }
    }
}

impl Provider {
    pub fn key(&self) -> Result<Option<[u8; 32]>> {
        if !self.encryption {
            return Ok(None);
        }
        let value =
            std::env::var("OASIS_STORAGE_KEY").unwrap_or_else(|_| self.encryption_key.clone());
        let key = crypto::parse_key(&value)?;
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
        }
        config
    }

    pub fn merge_secrets(&mut self, previous: &Self) {
        for provider in &mut self.providers {
            // Fingerprints supplied by clients must never bypass key-change checks.
            provider.key_fingerprint.clear();
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
                if provider.protocol == old.protocol
                    && provider.host == old.host
                    && provider.endpoint == old.endpoint
                    && provider.root == old.root
                {
                    provider.key_fingerprint = old.key_fingerprint.clone();
                }
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
                if let Some(key) = provider.key()? {
                    provider.key_fingerprint = crypto::fingerprint(&key);
                }
            }
        }
        for (index, provider) in self.providers.iter().enumerate() {
            for other in &self.providers[index + 1..] {
                let same_server = provider.protocol == other.protocol
                    && provider.host.eq_ignore_ascii_case(&other.host)
                    && provider.port == other.port
                    && provider.endpoint.trim_end_matches('/')
                        == other.endpoint.trim_end_matches('/')
                    && (provider.protocol == Protocol::S3 || provider.username == other.username);
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

    pub async fn load(pool: &Pool<Sqlite>, secret: &str) -> Result<Self> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT sealed_config FROM storage_config WHERE id = 1")
                .fetch_optional(pool)
                .await?;
        match row {
            None => Ok(Self::default()),
            Some((sealed,)) => Ok(serde_json::from_slice(&crypto::unseal(
                &settings_key(secret),
                &sealed,
            )?)?),
        }
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
