use rocket::serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::storage::progress::Phase;
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
    pub phase: Phase,
    pub current_file: Option<String>,
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
            phase: Phase::Preparing,
            current_file: None,
            user_id,
            is_copy,
            status: CopyMoveTaskStatus::Pending,
            overwrite,
        }
    }

    pub fn update_progress(&self, progress: f64, status: CopyMoveTaskStatus) {
        let mut current = COPY_MOVE_TASK.lock().unwrap();
        self.update(&mut current, progress, status);
    }

    fn update(&self, current: &mut Option<Self>, progress: f64, status: CopyMoveTaskStatus) {
        if let Some(task) = current.as_mut().filter(|task| {
            task.uuid == self.uuid
                && matches!(
                    task.status,
                    CopyMoveTaskStatus::Pending | CopyMoveTaskStatus::InProgress
                )
        }) {
            if status == CopyMoveTaskStatus::Success {
                task.progress = 1.0;
                task.phase = Phase::Completed;
                task.current_file = None;
            } else if status != CopyMoveTaskStatus::Failed && progress.is_finite() {
                task.progress = task.progress.max(progress.clamp(0.0, 0.99));
            }
            task.status = status;
        }
    }

    pub fn report(&self, progress: f64, phase: Phase, current_file: Option<String>) {
        let mut current = COPY_MOVE_TASK.lock().unwrap();
        self.report_to(&mut current, progress, phase, current_file);
    }

    fn report_to(
        &self,
        current: &mut Option<Self>,
        progress: f64,
        phase: Phase,
        current_file: Option<String>,
    ) {
        if let Some(task) = current.as_mut().filter(|task| {
            task.uuid == self.uuid
                && matches!(
                    task.status,
                    CopyMoveTaskStatus::Pending | CopyMoveTaskStatus::InProgress
                )
        }) {
            if progress.is_finite() {
                task.progress = task.progress.max(progress.clamp(0.0, 0.99));
            }
            task.phase = phase;
            task.current_file = current_file;
            task.status = CopyMoveTaskStatus::InProgress;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failures_retain_details_and_stale_workers_cannot_replace_new_tasks() {
        let task = CopyMoveTask::new("source".into(), "target".into(), 1, false, false);
        let mut current = Some(task.clone());
        task.report_to(
            &mut current,
            0.7,
            Phase::Uploading,
            Some("folder/file.txt".into()),
        );
        task.report_to(
            &mut current,
            0.2,
            Phase::Verifying,
            Some("folder/file.txt".into()),
        );
        task.update(&mut current, 0.0, CopyMoveTaskStatus::Failed);
        assert_eq!(current.as_ref().unwrap().progress, 0.7);
        assert_eq!(current.as_ref().unwrap().phase, Phase::Verifying);
        assert_eq!(
            current.as_ref().unwrap().current_file.as_deref(),
            Some("folder/file.txt")
        );
        task.update(&mut current, 1.0, CopyMoveTaskStatus::Success);
        assert_eq!(current.as_ref().unwrap().status, CopyMoveTaskStatus::Failed);
        let next = CopyMoveTask::new("different".into(), "target".into(), 1, true, false);
        current = Some(next.clone());
        task.report_to(&mut current, 0.9, Phase::RemovingSource, None);
        task.update(&mut current, 1.0, CopyMoveTaskStatus::Success);
        assert_eq!(current.as_ref().unwrap().uuid, next.uuid);
        assert_eq!(current.as_ref().unwrap().progress, 0.0);
        next.update(&mut current, 1.0, CopyMoveTaskStatus::Success);
        assert_eq!(current.as_ref().unwrap().progress, 1.0);
        assert_eq!(current.as_ref().unwrap().phase, Phase::Completed);
    }
}
