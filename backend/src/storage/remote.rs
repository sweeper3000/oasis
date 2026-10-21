use super::{
    config::{Protocol, Provider},
    crypto, validate_name, Workspace,
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use rocket::serde::Deserialize;
use rocket::serde::Serialize;
use std::{collections::BTreeMap, path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};

pub const NAMESPACE: &str = ".oasis-storage-v1";
pub const DIR_MARKER: &str = ".oasis-dir";

#[derive(Deserialize, Serialize)]
#[serde(crate = "rocket::serde", deny_unknown_fields)]
pub struct PassphraseNamespace {
    version: u8,
    derivation: super::config::KeyDerivation,
    verification: String,
}

pub enum Namespace {
    Legacy(String),
    Passphrase(PassphraseNamespace),
}
impl Namespace {
    fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= 16384, "Invalid storage namespace marker");
        if bytes.first() == Some(&b'{') {
            let marker: PassphraseNamespace = serde_json::from_slice(bytes)
                .map_err(|_| anyhow!("Invalid storage namespace marker"))?;
            ensure!(
                marker.version == 2 && marker.derivation.version == 1,
                "Unsupported storage namespace version"
            );
            Ok(Self::Passphrase(marker))
        } else {
            Ok(Self::Legacy(String::from_utf8(bytes.to_vec())?))
        }
    }
    pub fn derivation(&self) -> Option<&super::config::KeyDerivation> {
        match self {
            Self::Legacy(_) => None,
            Self::Passphrase(m) => Some(&m.derivation),
        }
    }
    fn verify(&self, provider: &Provider, key: &[u8; 32]) -> Result<()> {
        let (sealed, expected): (&str, &[u8]) = match self {
            Self::Legacy(sealed) => {
                ensure!(
                    provider.key_derivation.is_none(),
                    "This storage requires its original hexadecimal key"
                );
                (sealed, b"oasis-encrypted-namespace-v1")
            }
            Self::Passphrase(m) => {
                ensure!(
                    provider.key_derivation.as_ref() == Some(&m.derivation),
                    "Incorrect encryption secret or derivation metadata"
                );
                (&m.verification, b"oasis-encrypted-namespace-v2")
            }
        };
        ensure!(
            crypto::unseal(key, sealed).map_err(|_| anyhow!("Incorrect encryption secret"))?
                == expected,
            "Incorrect encryption secret"
        );
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(crate = "rocket::serde", rename_all = "PascalCase")]
pub struct Entry {
    pub name: String,
    pub size: i64,
    pub is_dir: bool,
}

#[derive(Clone)]
pub struct Remote {
    pub provider: Provider,
    env: BTreeMap<String, String>,
}

fn command() -> Command {
    let executable = std::env::var_os("OASIS_RCLONE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let filename = if cfg!(windows) {
                "rclone.exe"
            } else {
                "rclone"
            };
            let sibling = crate::util::get_pwd().join(filename);
            if sibling.is_file() {
                sibling
            } else {
                filename.into()
            }
        });
    let mut cmd = Command::new(executable);
    // Do not inherit arbitrary rclone backend/configuration overrides.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("RCLONE_") {
            cmd.env_remove(key);
        }
    }
    cmd.env_remove("OASIS_STORAGE_KEY");
    cmd.kill_on_drop(true).stdin(Stdio::null());
    cmd
}

pub async fn verify_adapter() -> Result<()> {
    let output = tokio::time::timeout(Duration::from_secs(15), command().arg("version").output())
        .await
        .context("Checking rclone version timed out")?
        .context("Install rclone 1.71+ or set OASIS_RCLONE to its executable")?;
    let text = String::from_utf8(output.stdout)?;
    let version = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("rclone v"))
        .ok_or_else(|| anyhow!("Install rclone 1.71+ or set OASIS_RCLONE to its executable"))?;
    let mut parts = version.split('.');
    let major: u32 = parts.next().unwrap_or("").parse()?;
    let minor: u32 = parts.next().unwrap_or("").parse()?;
    ensure!(
        output.status.success() && (major > 1 || major == 1 && minor >= 71),
        "External storage requires rclone 1.71 or newer"
    );
    Ok(())
}

async fn obscure(password: &str) -> Result<String> {
    if password.is_empty() {
        return Ok(String::new());
    }
    let mut child = command()
        .args(["obscure", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("Install rclone 1.71+ or set OASIS_RCLONE to its executable")?;
    let mut input = child.stdin.take().unwrap();
    input.write_all(password.as_bytes()).await?;
    input.write_all(b"\n").await?;
    drop(input);
    let output = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output()).await??;
    ensure!(
        output.status.success(),
        "Could not prepare provider credentials"
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

impl Remote {
    pub async fn new(provider: Provider) -> Result<Self> {
        let mut env = BTreeMap::new();
        let backend = match provider.protocol {
            Protocol::S3 => "s3",
            Protocol::Sftp => "sftp",
            Protocol::Ftps | Protocol::Ftp => "ftp",
            Protocol::Webdav => "webdav",
            Protocol::Smb => "smb",
        };
        env.insert("TYPE".into(), backend.into());
        match provider.protocol {
            Protocol::S3 => {
                env.insert(
                    "PROVIDER".into(),
                    if provider.endpoint.is_empty() {
                        "AWS"
                    } else {
                        "Other"
                    }
                    .into(),
                );
                env.insert(
                    "ENV_AUTH".into(),
                    provider.access_key_id.is_empty().to_string(),
                );
                env.insert("REGION".into(), provider.region.clone());
                env.insert("ENDPOINT".into(), provider.endpoint.clone());
                env.insert("ACCESS_KEY_ID".into(), provider.access_key_id.clone());
                env.insert(
                    "SECRET_ACCESS_KEY".into(),
                    provider.secret_access_key.clone(),
                );
                env.insert("SESSION_TOKEN".into(), provider.session_token.clone());
            }
            _ => {
                env.insert("HOST".into(), provider.host.clone());
                env.insert("USER".into(), provider.username.clone());
                env.insert("PASS".into(), obscure(&provider.password).await?);
                if provider.port != 0 {
                    env.insert("PORT".into(), provider.port.to_string());
                }
                match provider.protocol {
                    Protocol::Sftp => {
                        env.insert("KEY_FILE".into(), provider.key_file.clone());
                        env.insert("KNOWN_HOSTS_FILE".into(), provider.known_hosts_file.clone());
                        env.insert("SHELL_TYPE".into(), "none".into());
                        env.insert("DISABLE_HASHCHECK".into(), "true".into());
                    }
                    Protocol::Ftps => {
                        env.insert("TLS".into(), provider.implicit_tls.to_string());
                        env.insert("EXPLICIT_TLS".into(), (!provider.implicit_tls).to_string());
                        env.insert("NO_CHECK_CERTIFICATE".into(), "false".into());
                        if provider.port == 0 && provider.implicit_tls {
                            env.insert("PORT".into(), "990".into());
                        }
                    }
                    Protocol::Ftp => {
                        env.insert("TLS".into(), "false".into());
                        env.insert("EXPLICIT_TLS".into(), "false".into());
                    }
                    Protocol::Webdav => {
                        env.insert("URL".into(), provider.endpoint.clone());
                        env.insert("VENDOR".into(), "other".into());
                    }
                    Protocol::Smb => {
                        env.insert("DOMAIN".into(), provider.domain.clone());
                    }
                    _ => (),
                }
            }
        }
        Ok(Self { provider, env })
    }

    pub fn path(&self, relative: &str) -> Result<String> {
        let mut parts = vec![];
        let key = if relative.trim_matches('/').is_empty() {
            None
        } else {
            self.provider.key()?
        };
        for part in relative.split('/').filter(|p| !p.is_empty()) {
            validate_name(part)?;
            parts.push(match key {
                Some(key) => crypto::encrypt_name(&key, part)?,
                None => part.to_owned(),
            });
        }
        let root = if self.provider.protocol == Protocol::Sftp {
            self.provider.root.as_str()
        } else {
            self.provider.root.trim_matches('/')
        };
        let suffix = parts.join("/");
        Ok(if suffix.is_empty() {
            format!("oasis:{root}")
        } else if root.is_empty() {
            format!("oasis:{suffix}")
        } else {
            append(&format!("oasis:{root}"), &suffix)
        })
    }

    pub async fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let mut cmd = command();
        cmd.args(args).args([
            "--config",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
            "--contimeout",
            "15s",
            "--timeout",
            "60s",
            "--retries",
            "1",
            "--low-level-retries",
            "2",
            "--log-level",
            "ERROR",
        ]);
        for (key, value) in &self.env {
            cmd.env(format!("RCLONE_CONFIG_OASIS_{key}"), value);
        }
        if let Some(certificate) = std::env::var_os("OASIS_STORAGE_CA_CERT") {
            cmd.arg("--ca-cert").arg(certificate);
        }
        let limit = if matches!(args[0], "lsjson" | "mkdir") {
            90
        } else {
            3600
        };
        let output = tokio::time::timeout(Duration::from_secs(limit), cmd.output())
            .await
            .context("External storage operation timed out")?
            .context("Install rclone 1.71+ or set OASIS_RCLONE to its executable")?;
        if !output.status.success() {
            // Remote stderr can include credentials/URLs. Never expose or log it.
            bail!("External provider '{}' failed {} (exit {}). Check connection, permissions, host keys and TLS certificates", self.provider.name, args[0], output.status.code().unwrap_or(-1));
        }
        Ok(output.stdout)
    }

    pub async fn list(&self, relative: &str) -> Result<Vec<Entry>> {
        let path = self.path(relative)?;
        let bytes = self
            .run(&["lsjson", &path, "--no-modtime", "--no-mimetype"])
            .await?;
        let entries: Vec<Entry> = serde_json::from_slice(&bytes)?;
        let key = self.provider.key()?;
        let mut result = vec![];
        for mut entry in entries {
            if entry.name == DIR_MARKER || entry.name == NAMESPACE {
                continue;
            }
            if let Some(key) = key {
                entry.name = crypto::decrypt_name(&key, &entry.name)?;
                if !entry.is_dir {
                    entry.size = crypto::plaintext_size(entry.size.try_into()?)?.try_into()?;
                }
            }
            validate_name(&entry.name)?;
            result.push(entry);
        }
        Ok(result)
    }

    pub async fn check_namespace(&self, initialize: bool) -> Result<()> {
        let root = self.path("")?;
        let entries: Vec<Entry> = serde_json::from_slice(
            &self
                .run(&["lsjson", &root, "--no-modtime", "--no-mimetype"])
                .await?,
        )?;
        let has_marker = entries.iter().any(|e| e.name == NAMESPACE);
        if let Some(key) = self.provider.key()? {
            if has_marker {
                self.read_namespace()
                    .await?
                    .ok_or_else(|| anyhow!("Storage namespace disappeared"))?
                    .verify(&self.provider, &key)?;
            } else {
                ensure!(entries.is_empty(), "Encryption requires an empty dedicated provider root. Existing files are never encrypted in place");
                if initialize {
                    let workspace = Workspace::new()?;
                    let path = workspace.path().join("marker");
                    let marker = match &self.provider.key_derivation {
                        Some(derivation) => serde_json::to_vec(&PassphraseNamespace {
                            version: 2,
                            derivation: derivation.clone(),
                            verification: crypto::seal(&key, b"oasis-encrypted-namespace-v2")?,
                        })?,
                        None => crypto::seal(&key, b"oasis-encrypted-namespace-v1")?.into_bytes(),
                    };
                    tokio::fs::write(&path, marker).await?;
                    let remote = append(&root, NAMESPACE);
                    self.upload_to(&path, &remote).await?;
                }
            }
        } else {
            ensure!(!has_marker, "This root contains encrypted Oasis storage. Restore its original encryption key and enable encryption");
        }
        Ok(())
    }

    pub async fn read_namespace(&self) -> Result<Option<Namespace>> {
        let root = self.path("")?;
        let entries: Vec<Entry> = serde_json::from_slice(
            &self
                .run(&["lsjson", &root, "--no-modtime", "--no-mimetype"])
                .await?,
        )?;
        if let Some(entry) = entries.iter().find(|e| e.name == NAMESPACE) {
            ensure!(
                !entry.is_dir && (0..=16384).contains(&entry.size),
                "Invalid storage namespace marker"
            );
            let bytes = self
                .run(&["cat", &append(&root, NAMESPACE), "--head", "16385"])
                .await?;
            Ok(Some(Namespace::parse(&bytes)?))
        } else {
            Ok(None)
        }
    }

    pub async fn mkdir(&self, relative: &str) -> Result<()> {
        let path = self.path(relative)?;
        self.run(&["mkdir", &path]).await?;
        // A tiny marker retains empty directories on object stores such as S3.
        let workspace = Workspace::new()?;
        let marker = workspace.path().join("marker");
        tokio::fs::write(&marker, []).await?;
        self.upload_to(&marker, &append(&path, DIR_MARKER)).await?;
        Ok(())
    }

    pub async fn download(&self, relative: &str, target: &Path) -> Result<()> {
        self.run(&[
            "copyto",
            &self.path(relative)?,
            target
                .to_str()
                .ok_or_else(|| anyhow!("Invalid staging path"))?,
            "--ignore-times",
        ])
        .await?;
        Ok(())
    }

    pub async fn upload(&self, relative: &str, source: &Path) -> Result<()> {
        self.upload_to(source, &self.path(relative)?).await
    }

    // All outgoing files, including internal markers, are checked using fresh
    // remote metadata. For encrypted files, source is the ciphertext staging file.
    async fn upload_to(&self, source: &Path, destination: &str) -> Result<()> {
        let metadata = tokio::fs::metadata(source).await?;
        ensure!(metadata.is_file(), "Upload source must be a file");
        let expected_size = metadata.len();
        self.run(&[
            "copyto",
            source
                .to_str()
                .ok_or_else(|| anyhow!("Invalid staging path"))?,
            destination,
            "--ignore-times",
        ])
        .await?;
        let metadata = self
            .run(&[
                "lsjson",
                destination,
                "--stat",
                "--no-modtime",
                "--no-mimetype",
            ])
            .await
            .context("Upload verification failed: could not read destination metadata")?;
        verify_uploaded_metadata(&metadata, expected_size)
    }
}

use std::convert::TryInto;

fn verify_uploaded_metadata(bytes: &[u8], expected_size: u64) -> Result<()> {
    // Do not include raw adapter output, paths or credentials in error messages.
    let entry: Entry = serde_json::from_slice(bytes)
        .map_err(|_| anyhow!("Upload verification failed: invalid destination metadata"))?;
    ensure!(
        !entry.is_dir,
        "Upload verification failed: destination is a directory"
    );
    ensure!(
        entry.size >= 0,
        "Upload verification failed: destination size is unknown"
    );
    ensure!(
        entry.size as u64 == expected_size,
        "Upload verification failed: destination size differs from uploaded file"
    );
    Ok(())
}

fn append(parent: &str, name: &str) -> String {
    format!(
        "{}{}{}",
        parent,
        if parent.ends_with(':') || parent.ends_with('/') {
            ""
        } else {
            "/"
        },
        name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_versions_authenticate_keys_and_derivation_metadata() {
        let key = [23; 32];
        let mut provider = Provider::default();
        let sealed = crypto::seal(&key, b"oasis-encrypted-namespace-v1").unwrap();
        Namespace::parse(sealed.as_bytes())
            .unwrap()
            .verify(&provider, &key)
            .unwrap();
        let derivation = super::super::config::KeyDerivation {
            version: 1,
            salt: [8; 16],
        };
        provider.key_derivation = Some(derivation.clone());
        let mut marker = PassphraseNamespace {
            version: 2,
            derivation,
            verification: crypto::seal(&key, b"oasis-encrypted-namespace-v2").unwrap(),
        };
        let parsed = Namespace::parse(&serde_json::to_vec(&marker).unwrap()).unwrap();
        parsed.verify(&provider, &key).unwrap();
        assert!(parsed.verify(&provider, &[24; 32]).is_err());
        provider.key_derivation.as_mut().unwrap().salt[0] ^= 1;
        assert!(parsed.verify(&provider, &key).is_err());
        marker.version = 3;
        assert!(Namespace::parse(&serde_json::to_vec(&marker).unwrap()).is_err());
        assert!(Namespace::parse(b"{bad").is_err());
        assert!(Namespace::parse(&vec![0; 16385]).is_err());
    }

    #[test]
    fn upload_verification_requires_a_file_with_the_exact_known_size() {
        for size in [0, 1, 65536, i64::MAX as u64] {
            let metadata = serde_json::json!({"Name": "file", "Size": size, "IsDir": false});
            assert!(
                verify_uploaded_metadata(&serde_json::to_vec(&metadata).unwrap(), size).is_ok()
            );
        }
        for metadata in [
            "null",
            "[]",
            "not JSON",
            r#"{"Name":"file","Size":10}"#,
            r#"{"Name":"file","Size":"10","IsDir":false}"#,
            r#"{"Name":"file","Size":10,"IsDir":true}"#,
            r#"{"Name":"file","Size":-1,"IsDir":false}"#,
            r#"{"Name":"file","Size":9,"IsDir":false}"#,
            r#"{"Name":"file","Size":11,"IsDir":false}"#,
        ] {
            assert!(
                verify_uploaded_metadata(metadata.as_bytes(), 10).is_err(),
                "{}",
                metadata
            );
        }
    }

    #[tokio::test]
    async fn maps_all_protocols_and_preserves_absolute_sftp_roots() {
        for protocol in [
            Protocol::S3,
            Protocol::Sftp,
            Protocol::Ftps,
            Protocol::Ftp,
            Protocol::Webdav,
            Protocol::Smb,
        ] {
            let provider = Provider {
                protocol: protocol.clone(),
                root: "/".into(),
                ..Provider::default()
            };
            let remote = Remote::new(provider).await.unwrap();
            let expected = match protocol {
                Protocol::S3 => "s3",
                Protocol::Sftp => "sftp",
                Protocol::Ftp | Protocol::Ftps => "ftp",
                Protocol::Webdav => "webdav",
                Protocol::Smb => "smb",
            };
            assert_eq!(remote.env["TYPE"], expected);
            assert_eq!(
                remote.path("dir/file").unwrap(),
                if protocol == Protocol::Sftp {
                    "oasis:/dir/file"
                } else {
                    "oasis:dir/file"
                }
            );
            if protocol == Protocol::Ftps {
                assert_eq!(remote.env["EXPLICIT_TLS"], "true");
                assert_eq!(remote.env["NO_CHECK_CERTIFICATE"], "false");
            }
        }
    }
}
