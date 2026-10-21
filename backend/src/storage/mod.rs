pub mod config;
pub mod crypto;
pub mod remote;
pub mod response;

use crate::{
    entity::file::{File, FileType},
    service::app_state::AppState,
};
use anyhow::{anyhow, ensure, Result};
use config::{StorageConfig, StorageMode};
use remote::{Remote, DIR_MARKER, NAMESPACE};
use std::path::{Path, PathBuf};

pub struct Workspace(tempfile::TempDir);
impl Workspace {
    pub fn new() -> Result<Self> {
        // The application removes this private staging root at startup after a crash.
        let root = crate::util::get_temp_path();
        std::fs::create_dir_all(&root)?;
        Ok(Self(
            tempfile::Builder::new().prefix("op-").tempdir_in(root)?,
        ))
    }
    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

pub struct Materialized {
    pub path: PathBuf,
    pub workspace: Option<Workspace>,
}

#[derive(Clone)]
pub struct Metadata {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}
impl Metadata {
    pub fn file(&self, need_dir: bool, permission: i8) -> File {
        let path = Path::new(&self.path);
        File {
            dir: if need_dir {
                Some(path.parent().unwrap_or(Path::new("")).into())
            } else {
                None
            },
            filename: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into(),
            file_type: if self.is_dir {
                FileType::Dir
            } else {
                FileType::infer_file_type(path.extension().and_then(|p| p.to_str()).unwrap_or(""))
            },
            size: if self.is_dir { 0 } else { self.size },
            least_permission: permission,
        }
    }
}

#[derive(Clone)]
pub struct Storage {
    pub local: String,
    pub config: StorageConfig,
}

enum Target {
    Local(PathBuf),
    Remote(config::Provider, String),
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name != "."
            && name != ".."
            && name.len() <= 255
            && !name.chars().any(|c| c.is_control()
                || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
            && name != DIR_MARKER
            && name != NAMESPACE
            && !name.starts_with(".oasis-tmp-")
            && !name.ends_with([' ', '.']),
        "Invalid or reserved file/folder name"
    );
    Ok(())
}

// The caller decodes the transport encoding exactly once before reaching here.
pub fn normalize(path: &str) -> Result<String> {
    ensure!(
        !path.starts_with('/') && !path.contains('\\'),
        "Storage path must be relative"
    );
    if path.is_empty() {
        return Ok(String::new());
    }
    for part in path.split('/') {
        validate_name(part)?;
    }
    Ok(path.to_owned())
}

pub fn join(parent: &str, name: &str) -> Result<String> {
    normalize(parent)?;
    validate_name(name)?;
    Ok(if parent.is_empty() {
        name.into()
    } else {
        format!("{parent}/{name}")
    })
}

impl Storage {
    pub fn from_state(state: &AppState) -> Result<Self> {
        Ok(Self {
            local: state.get_site()?.storage.clone(),
            config: state.get_storage_config()?,
        })
    }

    pub fn protected_root(&self, path: &str) -> bool {
        path.is_empty()
            || (self.config.enabled
                && self.config.mode == StorageMode::Mount
                && self.config.providers.iter().any(|p| p.name == path))
    }

    fn target(&self, path: &str) -> Result<Target> {
        let path = normalize(path)?;
        if self.config.enabled {
            if self.config.mode == StorageMode::Main {
                let provider = self
                    .config
                    .providers
                    .iter()
                    .find(|p| p.id == self.config.main_provider)
                    .ok_or_else(|| anyhow!("Main external provider is missing"))?;
                return Ok(Target::Remote(provider.clone(), path));
            }
            for provider in &self.config.providers {
                if path == provider.name {
                    return Ok(Target::Remote(provider.clone(), String::new()));
                }
                if let Some(relative) = path.strip_prefix(&format!("{}/", provider.name)) {
                    return Ok(Target::Remote(provider.clone(), relative.into()));
                }
            }
        }
        let root = Path::new(&self.local).canonicalize()?;
        let mut target = root.clone();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            target.push(part);
            match std::fs::symlink_metadata(&target) {
                Ok(meta) => ensure!(
                    !meta.file_type().is_symlink(),
                    "Symbolic links are not accessible through storage"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
        ensure!(target.starts_with(root), "Path is outside storage");
        Ok(Target::Local(target))
    }

    pub async fn stat(&self, path: &str) -> Result<Option<Metadata>> {
        let path = normalize(path)?;
        if self.protected_root(&path) {
            // Ensure a missing local root never masquerades as a working drive.
            self.target(&path)?;
            return Ok(Some(Metadata {
                path,
                is_dir: true,
                size: 0,
            }));
        }
        match self.target(&path)? {
            Target::Local(target) => match tokio::fs::metadata(target).await {
                Ok(meta) => Ok(Some(Metadata {
                    path,
                    is_dir: meta.is_dir(),
                    size: meta.len(),
                })),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            },
            Target::Remote(provider, relative) => {
                let remote = Remote::new(provider).await?;
                let mut parent = String::new();
                let parts: Vec<&str> = relative.split('/').collect();
                for (index, name) in parts.iter().enumerate() {
                    let entry = match remote
                        .list(&parent)
                        .await?
                        .into_iter()
                        .find(|e| e.name == *name)
                    {
                        Some(entry) => entry,
                        None => return Ok(None),
                    };
                    if index + 1 == parts.len() {
                        return Ok(Some(Metadata {
                            path,
                            is_dir: entry.is_dir,
                            size: entry.size.max(0) as u64,
                        }));
                    }
                    if !entry.is_dir {
                        return Ok(None);
                    }
                    parent = join(&parent, name)?;
                }
                Ok(None)
            }
        }
    }

    pub async fn list(&self, path: &str) -> Result<Vec<Metadata>> {
        let path = normalize(path)?;
        let mut entries = vec![];
        match self.target(&path)? {
            Target::Local(target) => {
                let mut dir = tokio::fs::read_dir(target).await?;
                while let Some(entry) = dir.next_entry().await? {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if validate_name(&name).is_err() || entry.file_type().await?.is_symlink() {
                        continue;
                    }
                    let meta = entry.metadata().await?;
                    entries.push(Metadata {
                        path: join(&path, &name)?,
                        is_dir: meta.is_dir(),
                        size: meta.len(),
                    });
                }
            }
            Target::Remote(provider, relative) => {
                for entry in Remote::new(provider).await?.list(&relative).await? {
                    entries.push(Metadata {
                        path: join(&path, &entry.name)?,
                        is_dir: entry.is_dir,
                        size: entry.size.max(0) as u64,
                    });
                }
            }
        }
        if path.is_empty() && self.config.enabled && self.config.mode == StorageMode::Mount {
            for provider in &self.config.providers {
                ensure!(!entries.iter().any(|e| e.path.eq_ignore_ascii_case(&provider.name)), "A local entry conflicts with an external drive name; rename it on disk or in settings");
                entries.push(Metadata {
                    path: provider.name.clone(),
                    is_dir: true,
                    size: 0,
                });
            }
        }
        Ok(entries)
    }

    pub async fn mkdir(&self, path: &str) -> Result<()> {
        ensure!(!self.protected_root(path), "Cannot create a drive root");
        ensure!(self.stat(path).await?.is_none(), "An entry already exists");
        let parent = Path::new(path)
            .parent()
            .unwrap_or(Path::new(""))
            .to_string_lossy();
        ensure!(
            self.stat(&parent).await?.map(|m| m.is_dir).unwrap_or(false),
            "Parent folder does not exist"
        );
        match self.target(path)? {
            Target::Local(target) => tokio::fs::create_dir(target).await?,
            Target::Remote(provider, relative) => {
                Remote::new(provider).await?.mkdir(&relative).await?
            }
        }
        Ok(())
    }

    pub async fn materialize(&self, path: &str) -> Result<Materialized> {
        ensure!(
            self.stat(path).await?.map(|m| !m.is_dir).unwrap_or(false),
            "File does not exist"
        );
        match self.target(path)? {
            Target::Local(target) => Ok(Materialized {
                path: target,
                workspace: None,
            }),
            Target::Remote(provider, relative) => {
                let workspace = Workspace::new()?;
                let raw = workspace.path().join("remote");
                let remote = Remote::new(provider.clone()).await?;
                remote.download(&relative, &raw).await?;
                let path = if let Some(key) = provider.key()? {
                    let plain = workspace.path().join("plain");
                    crypto::decrypt_file(&key, &raw, &plain).await?;
                    tokio::fs::remove_file(raw).await?;
                    plain
                } else {
                    raw
                };
                Ok(Materialized {
                    path,
                    workspace: Some(workspace),
                })
            }
        }
    }

    pub async fn write(&self, path: &str, source: &Path) -> Result<()> {
        ensure!(!self.protected_root(path), "Cannot replace a drive root");
        ensure!(
            !self.stat(path).await?.map(|m| m.is_dir).unwrap_or(false),
            "Cannot replace a folder with a file"
        );
        match self.target(path)? {
            Target::Local(target) => {
                // Stage on the destination filesystem, then rename atomically.
                let staging = tempfile::Builder::new()
                    .prefix(".oasis-tmp-")
                    .tempfile_in(target.parent().unwrap())?;
                tokio::fs::copy(source, staging.path()).await?;
                staging
                    .persist(&target)
                    .map_err(|e| anyhow!("Could not commit upload: {}", e.error))?;
            }
            Target::Remote(provider, relative) => {
                let remote = Remote::new(provider.clone()).await?;
                if let Some(key) = provider.key()? {
                    let workspace = Workspace::new()?;
                    let encrypted = workspace.path().join("encrypted");
                    crypto::encrypt_file(&key, source, &encrypted).await?;
                    remote.upload(&relative, &encrypted).await?;
                } else {
                    remote.upload(&relative, source).await?;
                }
            }
        }
        Ok(())
    }

    pub async fn remove(&self, path: &str) -> Result<()> {
        ensure!(
            !self.protected_root(path),
            "Cannot delete a drive root; remove it in settings"
        );
        let meta = self
            .stat(path)
            .await?
            .ok_or_else(|| anyhow!("File does not exist"))?;
        match self.target(path)? {
            Target::Local(target) => {
                if meta.is_dir {
                    tokio::fs::remove_dir_all(target).await?;
                } else {
                    tokio::fs::remove_file(target).await?;
                }
            }
            Target::Remote(provider, relative) => {
                let remote = Remote::new(provider).await?;
                remote
                    .run(&[
                        if meta.is_dir { "purge" } else { "deletefile" },
                        &remote.path(&relative)?,
                    ])
                    .await?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub async fn transfer(
        &self,
        source: &str,
        destination: &str,
        is_copy: bool,
        overwrite: bool,
    ) -> Result<()> {
        self.transfer_with_hook(source, destination, is_copy, overwrite, || async { Ok(()) })
            .await
    }

    pub async fn transfer_with_hook<F, Fut>(
        &self,
        source: &str,
        destination: &str,
        is_copy: bool,
        overwrite: bool,
        before_write: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        normalize(source)?;
        normalize(destination)?;
        ensure!(
            !self.protected_root(source) && !self.protected_root(destination),
            "Drive roots cannot be copied, moved or renamed"
        );
        ensure!(
            source != destination && !destination.starts_with(&format!("{source}/")),
            "Cannot transfer an entry into itself"
        );
        // Preflight every conflict before writing anything. A failed move never deletes its source.
        let mut pending = vec![(source.to_owned(), destination.to_owned())];
        let mut files = vec![];
        let mut directories = vec![];
        while let Some((src, dest)) = pending.pop() {
            let entry = self
                .stat(&src)
                .await?
                .ok_or_else(|| anyhow!("Source no longer exists"))?;
            let existing = self.stat(&dest).await?;
            if let Some(existing) = existing {
                ensure!(
                    overwrite && entry.is_dir == existing.is_dir,
                    "Destination already exists or has a different type"
                );
            }
            if entry.is_dir {
                directories.push(dest.clone());
                for child in self.list(&src).await? {
                    let name = Path::new(&child.path)
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string();
                    pending.push((child.path, join(&dest, &name)?));
                }
            } else {
                files.push((src, dest));
            }
        }
        // Install destination access rules before any file becomes visible.
        before_write().await?;
        for directory in directories {
            if self.stat(&directory).await?.is_none() {
                self.mkdir(&directory).await?;
            }
        }
        for (source, destination) in files {
            let file = self.materialize(&source).await?;
            self.write(&destination, &file.path).await?;
        }
        if !is_copy {
            self.remove(source).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escape_paths_and_reserved_names() {
        for path in [
            "../outside",
            "/etc/passwd",
            "a/../b",
            "a\\b",
            "C:secret",
            "a//b",
            "a/./b",
            ".oasis-storage-v1",
            ".oasis-dir",
        ] {
            assert!(normalize(path).is_err(), "{path}");
        }
        assert!(normalize("照片/a%20b.txt").is_ok());
    }

    #[tokio::test]
    async fn local_operations_and_failed_moves_preserve_source() {
        let root = tempfile::tempdir().unwrap();
        let storage = Storage {
            local: root.path().to_string_lossy().into(),
            config: StorageConfig::default(),
        };
        let input = root.path().join("input.txt");
        tokio::fs::write(&input, b"hello").await.unwrap();
        storage.mkdir("folder").await.unwrap();
        storage
            .transfer("input.txt", "folder/copy.txt", true, false)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read(root.path().join("folder/copy.txt"))
                .await
                .unwrap(),
            b"hello"
        );
        assert!(storage
            .transfer("input.txt", "folder/copy.txt", false, false)
            .await
            .is_err());
        assert!(input.exists());
        assert!(storage
            .transfer("folder", "folder/subfolder", true, true)
            .await
            .is_err());
        storage.mkdir("folder/empty").await.unwrap();
        storage
            .transfer("folder", "copied-folder", true, false)
            .await
            .unwrap();
        assert!(root.path().join("copied-folder/empty").is_dir());
        assert_eq!(
            tokio::fs::read(root.path().join("copied-folder/copy.txt"))
                .await
                .unwrap(),
            b"hello"
        );
        storage
            .transfer("copied-folder", "moved-folder", false, false)
            .await
            .unwrap();
        assert!(!root.path().join("copied-folder").exists());
        storage
            .transfer("input.txt", "renamed.txt", false, false)
            .await
            .unwrap();
        assert!(!input.exists());
        assert!(root.path().join("renamed.txt").exists());
        assert!(storage.remove("").await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_local_symlink_escapes() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let storage = Storage {
            local: root.path().to_string_lossy().into(),
            config: StorageConfig::default(),
        };
        assert!(storage.stat("escape/secret").await.is_err());
        assert!(storage.list("escape").await.is_err());
    }
}
