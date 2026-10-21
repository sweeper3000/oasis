use rocket::serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::COPY_MOVE_TASK;

#[derive(Deserialize, Clone)]
#[serde(crate = "rocket::serde")]
pub struct CopyMoveFileRequest {
    pub source: String,
    pub target: String,
    pub is_copy: bool,
    pub overwrite: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(crate = "rocket::serde")]
pub enum CopyMoveTaskStatus {
    Pending,
    InProgress,
    Success,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct CopyMoveTask {
    pub uuid: String,
    pub user_id: i64,
    pub status: CopyMoveTaskStatus,
    pub source: PathBuf,
    pub target: PathBuf,
    pub progress: f64,
    pub is_copy: bool,
    pub overwrite: bool,
}

impl CopyMoveTask {
    pub fn new(
        source: PathBuf,
        target: PathBuf,
        user_id: i64,
        is_copy: bool,
        overwrite: bool,
    ) -> Self {
        let uuid = uuid::Uuid::new_v4().to_string();
        CopyMoveTask {
            uuid,
            source,
            target,
            progress: 0.0,
            user_id,
            is_copy,
            status: CopyMoveTaskStatus::Pending,
            overwrite,
        }
    }

    pub fn update_progress(&self, progress: f64, status: CopyMoveTaskStatus) {
        let mut updated_task = self.clone();
        updated_task.progress = progress;
        updated_task.status = status;
        updated_task.set_static_value();
    }

    pub fn set_static_value(&self) {
        let mut copy_move_task = COPY_MOVE_TASK.lock().unwrap();
        *copy_move_task = Some(self.clone());
    }

    pub fn get_static_value() -> Option<Self> {
        let copy_move_task = COPY_MOVE_TASK.lock().unwrap();
        copy_move_task.clone()
    }

    pub fn allow_new_task() -> bool {
        let task = Self::get_static_value();
        if task.is_none() {
            return true;
        }

        let task = task.as_ref().unwrap();
        task.status == CopyMoveTaskStatus::Success || task.status == CopyMoveTaskStatus::Failed
    }
}
