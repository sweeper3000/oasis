use super::request::UploadRequest;
use crate::storage::{Storage, Workspace};
use std::sync::{atomic::AtomicI64, Arc};

#[derive(Clone)]
pub struct UploadTask {
    pub uuid: String,
    pub userid: i64,
    pub filename: String,
    pub size: u64,
    pub dir: String,
    pub storage: Storage,
    pub workspace: Arc<Workspace>,
    pub lock: Arc<tokio::sync::Mutex<()>>,
    pub touched_at: Arc<AtomicI64>,
}

impl UploadTask {
    pub fn new(
        req: &UploadRequest,
        userid: i64,
        dir: String,
        storage: Storage,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            uuid: uuid::Uuid::new_v4().to_string(),
            userid,
            filename: req.filename.clone(),
            size: req.size,
            dir,
            storage,
            workspace: Arc::new(Workspace::new()?),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            touched_at: Arc::new(AtomicI64::new(crate::util::get_utc_seconds())),
        })
    }
}
