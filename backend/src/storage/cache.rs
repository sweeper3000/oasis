//! A disposable plaintext response cache. Authorization always happens upstream.
use super::{config::Provider, crypto, remote::Remote, Materialized, Workspace};
use anyhow::{ensure, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

const TTL: u64 = 24 * 60 * 60;
fn now() -> u64 {
    crate::util::get_utc_seconds().max(0) as u64
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct Key {
    namespace: String,
    path: String,
}
struct Entry {
    path: PathBuf,
    validator: String,
    touched: AtomicU64,
    _workspace: Workspace,
}

pub struct Lease(Arc<Entry>);
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.touched.store(now(), Ordering::Relaxed);
    }
}

#[derive(Default)]
struct State {
    entries: HashMap<Key, Arc<Entry>>,
    fills: HashMap<Key, std::sync::Weak<tokio::sync::Mutex<()>>>,
    generation: u64,
}

#[derive(Default)]
pub struct Cache {
    state: Mutex<State>,
}

pub fn namespace(provider: &Provider) -> Result<String> {
    // Secrets only enter the digest, never cache filenames or manifests. Include
    // the effective key so environment changes cannot reuse readable old bytes.
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(provider)?);
    if let Some(key) = provider.key()? {
        digest.update(key);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub struct Invalidation<'a> {
    cache: &'a Cache,
    namespace: String,
    path: String,
}
impl Drop for Invalidation<'_> {
    fn drop(&mut self) {
        self.cache.invalidate(&self.namespace, &self.path);
    }
}

impl Cache {
    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.generation = state.generation.wrapping_add(1);
        state.entries.clear();
    }

    fn invalidate(&self, namespace: &str, path: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.generation = state.generation.wrapping_add(1);
        state.entries.retain(|k, _| {
            k.namespace != namespace || !(k.path == path || k.path.starts_with(&format!("{path}/")))
        });
    }

    pub fn mutation<'a>(&'a self, provider: &Provider, path: &str) -> Result<Invalidation<'a>> {
        let namespace = namespace(provider)?;
        self.invalidate(&namespace, path);
        Ok(Invalidation {
            cache: self,
            namespace,
            path: path.into(),
        })
    }

    pub fn reap(&self) {
        self.reap_at(now());
    }
    fn reap_at(&self, time: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.entries.retain(|_, entry| {
            Arc::strong_count(entry) > 1
                || time.saturating_sub(entry.touched.load(Ordering::Relaxed)) < TTL
        });
        state.fills.retain(|_, lock| lock.strong_count() > 0);
    }

    pub async fn get(&self, provider: Provider, relative: &str) -> Result<Materialized> {
        let key = Key {
            namespace: namespace(&provider)?,
            path: relative.into(),
        };
        let lock = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let lock = state
                .fills
                .get(&key)
                .and_then(|weak| weak.upgrade())
                .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
            state.fills.insert(key.clone(), Arc::downgrade(&lock));
            lock
        };
        let _fill = lock.lock().await;
        let remote = Remote::new(provider.clone()).await?;
        // Do not serve cached bytes when the remote check fails or disappears.
        let metadata = match remote.metadata(relative).await {
            Ok(meta) => meta,
            Err(e) => {
                self.invalidate(&key.namespace, relative);
                return Err(e);
            }
        };
        let generation = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = state.entries.get(&key) {
                if entry.validator == metadata.validator
                    && entry.path.is_file()
                    && now().saturating_sub(entry.touched.load(Ordering::Relaxed)) < TTL
                {
                    entry.touched.store(now(), Ordering::Relaxed);
                    return Ok(Self::materialized(entry.clone()));
                }
            }
            state.entries.remove(&key);
            state.generation
        };
        let workspace = Workspace::cached()?;
        let raw = workspace.path().join("remote");
        remote.download(relative, &raw).await?;
        ensure!(
            tokio::fs::metadata(&raw).await?.len() == metadata.size,
            "Remote file changed during cache fill"
        );
        let path = if let Some(key) = provider.key()? {
            let path = workspace.path().join("plain");
            crypto::decrypt_file(&key, &raw, &path).await?;
            tokio::fs::remove_file(raw).await?;
            path
        } else {
            raw
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
        }
        ensure!(
            remote.metadata(relative).await? == metadata,
            "Remote file changed during cache fill; retry"
        );
        let entry = Arc::new(Entry {
            path,
            validator: metadata.validator,
            touched: AtomicU64::new(now()),
            _workspace: workspace,
        });
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.generation == generation {
                state.entries.insert(key, entry.clone());
            }
        }
        Ok(Self::materialized(entry))
    }

    fn materialized(entry: Arc<Entry>) -> Materialized {
        Materialized {
            path: entry.path.clone(),
            workspace: None,
            cache_lease: Some(Lease(entry)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(cache: &Cache, namespace: &str, name: &str) -> PathBuf {
        let workspace = Workspace::cached().unwrap();
        let path = workspace.path().join("plain");
        std::fs::write(&path, b"cached bytes").unwrap();
        cache.state.lock().unwrap().entries.insert(
            Key {
                namespace: namespace.into(),
                path: name.into(),
            },
            Arc::new(Entry {
                path: path.clone(),
                validator: "old".into(),
                touched: AtomicU64::new(now()),
                _workspace: workspace,
            }),
        );
        path
    }

    #[test]
    fn mutations_invalidate_descendants_and_late_fills_without_touching_other_paths() {
        let cache = Cache::default();
        let provider = Provider::default();
        let namespace = namespace(&provider).unwrap();
        let nested = seed(&cache, &namespace, "folder/nested/file.txt");
        let neighbor = seed(&cache, &namespace, "folder-other/file.txt");
        let other_provider = seed(&cache, "other-provider", "folder/nested/file.txt");
        let mutation = cache.mutation(&provider, "folder").unwrap();
        assert!(!nested.exists());
        assert!(neighbor.exists() && other_provider.exists());
        let late = seed(&cache, &namespace, "folder/new.txt");
        drop(mutation);
        assert!(!late.exists());
        assert!(neighbor.exists() && other_provider.exists());
        cache.clear();
    }
    #[test]
    fn expiry_pins_and_invalidation_remove_bytes_only_after_last_reader() {
        let cache = Cache::default();
        let workspace = Workspace::cached().unwrap();
        let path = workspace.path().join("plain");
        std::fs::write(&path, b"private").unwrap();
        let entry = Arc::new(Entry {
            path: path.clone(),
            validator: "v1".into(),
            touched: AtomicU64::new(100),
            _workspace: workspace,
        });
        let key = Key {
            namespace: "provider".into(),
            path: "folder/file.txt".into(),
        };
        cache
            .state
            .lock()
            .unwrap()
            .entries
            .insert(key.clone(), entry.clone());
        let lease = Lease(entry);
        cache.reap_at(100 + TTL);
        assert!(path.exists());
        drop(lease);
        let touched = cache.state.lock().unwrap().entries[&key]
            .touched
            .load(Ordering::Relaxed);
        cache.reap_at(touched + TTL - 1);
        assert!(path.exists());
        cache.reap_at(touched + TTL);
        assert!(!path.exists());
        assert!(cache.state.lock().unwrap().entries.is_empty());
    }

    #[test]
    fn clearing_a_cache_does_not_remove_an_active_response() {
        let cache = Cache::default();
        let workspace = Workspace::cached().unwrap();
        let path = workspace.path().join("plain");
        std::fs::write(&path, b"in flight").unwrap();
        let entry = Arc::new(Entry {
            path: path.clone(),
            validator: "v1".into(),
            touched: AtomicU64::new(now()),
            _workspace: workspace,
        });
        let key = Key {
            namespace: "one".into(),
            path: "same.txt".into(),
        };
        cache
            .state
            .lock()
            .unwrap()
            .entries
            .insert(key, entry.clone());
        let lease = Lease(entry);
        cache.clear();
        assert!(path.exists());
        drop(lease);
        assert!(!path.exists());
    }
}
