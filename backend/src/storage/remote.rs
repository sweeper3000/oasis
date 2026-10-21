use super::{
    config::{Protocol, Provider},
    crypto, validate_name, Workspace,
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use rocket::serde::Deserialize;
use std::{collections::BTreeMap, path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};

pub const NAMESPACE: &str = ".oasis-storage-v1";
pub const DIR_MARKER: &str = ".oasis-dir";

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
        let key = self.provider.key()?;
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
                let workspace = Workspace::new()?;
                let path = workspace.path().join("marker");
                let remote = append(&root, NAMESPACE);
                self.run(&[
                    "copyto",
                    &remote,
                    path.to_str()
                        .ok_or_else(|| anyhow!("Invalid staging path"))?,
                    "--ignore-times",
                ])
                .await?;
                let marker = tokio::fs::read_to_string(&path).await?;
                ensure!(
                    crypto::unseal(&key, &marker)? == b"oasis-encrypted-namespace-v1",
                    "Incorrect encryption key"
                );
            } else {
                ensure!(entries.is_empty(), "Encryption requires an empty dedicated provider root. Existing files are never encrypted in place");
                if initialize {
                    let workspace = Workspace::new()?;
                    let path = workspace.path().join("marker");
                    tokio::fs::write(&path, crypto::seal(&key, b"oasis-encrypted-namespace-v1")?)
                        .await?;
                    let remote = append(&root, NAMESPACE);
                    self.run(&["copyto", path.to_str().unwrap(), &remote, "--ignore-times"])
                        .await?;
                }
            }
        } else {
            ensure!(!has_marker, "This root contains encrypted Oasis storage. Restore its original encryption key and enable encryption");
        }
        Ok(())
    }

    pub async fn mkdir(&self, relative: &str) -> Result<()> {
        let path = self.path(relative)?;
        self.run(&["mkdir", &path]).await?;
        // A tiny marker retains empty directories on object stores such as S3.
        let workspace = Workspace::new()?;
        let marker = workspace.path().join("marker");
        tokio::fs::write(&marker, []).await?;
        self.run(&[
            "copyto",
            marker.to_str().unwrap(),
            &append(&path, DIR_MARKER),
            "--ignore-times",
        ])
        .await?;
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
        self.run(&[
            "copyto",
            source
                .to_str()
                .ok_or_else(|| anyhow!("Invalid staging path"))?,
            &self.path(relative)?,
            "--ignore-times",
        ])
        .await?;
        Ok(())
    }
}

use std::convert::TryInto;

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
