use crate::entity::{site::Site, upload_task::UploadTask};
use anyhow::Result as AnyResult;
use sqlx::{pool::PoolConnection, Pool, Sqlite};
use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc, Mutex, MutexGuard};

#[derive(Clone)]
pub struct AppState {
    pub operations: Arc<tokio::sync::RwLock<()>>,
    pub backups: Arc<crate::service::backup::BackupManager>,
    pub storage: Arc<Mutex<crate::storage::config::StorageConfig>>,
    pub storage_settings_lock: Arc<tokio::sync::Mutex<()>>,
    pub first_run: Arc<AtomicBool>,
    pub site: Arc<Mutex<Site>>,
    pub pool: Pool<Sqlite>,
    pub uploads: Arc<Mutex<Vec<UploadTask>>>,
}

impl AppState {
    pub fn new(site_op: Option<Site>, pool: Pool<Sqlite>) -> Self {
        let first_run = site_op.is_none();
        let site = match site_op {
            Some(site) => site,
            None => Site::default(),
        };

        Self {
            operations: Arc::new(tokio::sync::RwLock::new(())),
            backups: Arc::new(crate::service::backup::BackupManager::default()),
            storage: Arc::new(Mutex::new(crate::storage::config::StorageConfig::default())),
            storage_settings_lock: Arc::new(tokio::sync::Mutex::new(())),
            first_run: Arc::new(AtomicBool::new(first_run)),
            site: Arc::new(Mutex::new(site)),
            pool,
            uploads: Arc::new(Mutex::new(vec![])),
        }
    }

    pub fn get_storage_config(&self) -> AnyResult<crate::storage::config::StorageConfig> {
        Ok(self
            .storage
            .lock()
            .map_err(|_| anyhow::anyhow!("Storage configuration lock failed"))?
            .clone())
    }

    pub fn set_storage_config(
        &self,
        config: crate::storage::config::StorageConfig,
    ) -> AnyResult<()> {
        *self
            .storage
            .lock()
            .map_err(|_| anyhow::anyhow!("Storage configuration lock failed"))? = config;
        Ok(())
    }

    pub fn start_upload_reaper(&self) {
        let uploads = self.uploads.clone();
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                timer.tick().await;
                if let Ok(mut tasks) = uploads.lock() {
                    let cutoff = crate::util::get_utc_seconds() - 3600;
                    tasks.retain(|task| {
                        task.touched_at.load(Ordering::Relaxed) > cutoff
                            || task.lock.try_lock().is_err()
                    });
                }
            }
        });
    }

    pub fn get_first_run(&self) -> bool {
        self.first_run.load(Ordering::Relaxed)
    }

    pub fn set_first_run(&self, new_first_run: bool) {
        self.first_run.store(new_first_run, Ordering::Relaxed);
    }

    pub fn get_secret(&self) -> AnyResult<String> {
        Ok(self.get_site()?.secret.to_owned())
    }

    pub fn get_allow_guest(&self) -> AnyResult<i8> {
        Ok(self.get_site()?.allow_guest)
    }

    pub async fn get_pool_conn(&self) -> Result<PoolConnection<Sqlite>, sqlx::Error> {
        Ok(self.pool.acquire().await?)
    }

    pub fn get_site(&self) -> AnyResult<MutexGuard<Site>> {
        match self.site.lock() {
            Ok(v) => Ok(v),
            Err(e) => return Err(anyhow::anyhow!("Cannot retrieve site from state: {}", e)),
        }
    }

    pub fn set_site(&self, new_site: Site) -> AnyResult<()> {
        let mut site = self.get_site()?;
        *site = new_site;

        Ok(())
    }

    pub fn get_upload_tasks(&self) -> AnyResult<MutexGuard<Vec<UploadTask>>> {
        match self.uploads.lock() {
            Ok(v) => Ok(v),
            Err(e) => return Err(anyhow::anyhow!("Cannot retrieve site from state: {}", e)),
        }
    }

    pub fn find_upload_uuid(&self, uuid: &str) -> AnyResult<Option<UploadTask>> {
        let uploads = self.get_upload_tasks()?;
        for task in uploads.iter() {
            if task.uuid == uuid {
                return Ok(Some(task.to_owned()));
            }
        }

        Ok(None)
    }

    pub fn push_upload_task(&self, target_task: UploadTask) -> AnyResult<()> {
        let mut uploads = self.get_upload_tasks()?;
        (*uploads).push(target_task);

        Ok(())
    }

    pub fn remove_upload_task(&self, target_task: UploadTask) -> AnyResult<()> {
        let mut uploads = self.get_upload_tasks()?;
        (*uploads).retain(|upload| upload.uuid != target_task.uuid);

        Ok(())
    }
}
