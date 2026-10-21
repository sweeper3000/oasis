use crate::{
    entity::upload_task::UploadTask,
    entity::{
        error::Error,
        request::{CancelUploadRequest, UploadRequest},
    },
    service::{app_state::AppState, auth::AuthAdmin},
    storage::{self, Storage},
    util,
};
use rocket::{fs::TempFile, serde::json::Json, Route, State};
use std::{path::Path, sync::atomic::Ordering};
use tokio::{fs, io::AsyncWriteExt};
const SLICE: u64 = 2 * 1024 * 1024;

pub fn route() -> Vec<Route> {
    routes![pre_upload, upload_file_slices, finish_upload, cancel_upload]
}

#[post("/pre-upload", data = "<req_body>")]
async fn pre_upload(
    state: &State<AppState>,
    req_body: Json<UploadRequest>,
    user: AuthAdmin,
) -> Result<String, Error> {
    let _settings_lock = state.storage_settings_lock.lock().await;
    storage::validate_name(&req_body.filename).map_err(|_| Error::BadRequest)?;
    let dir = super::files::encoded_path(&req_body.target)?;
    let storage = Storage::from_state(state)?;
    if !storage.stat(&dir).await?.map(|m| m.is_dir).unwrap_or(false) {
        return Err(Error::BadRequest);
    }
    let target = storage::join(&dir, &req_body.filename).map_err(|_| Error::BadRequest)?;
    if storage.protected_root(&target) {
        return Err(Error::BadRequest);
    }
    if state.get_upload_tasks()?.len() >= 32 {
        return Err(Error::Conflict);
    }
    let available =
        util::file_system::get_available_space(&util::get_temp_path().to_string_lossy());
    // Assembly/encryption can temporarily require a second copy.
    if available > 0 && req_body.size.saturating_mul(2) > available {
        return Err(Error::BadRequest);
    }
    let task = UploadTask::new(&req_body, user.uid, dir, storage)?;
    let uuid = task.uuid.clone();
    state.push_upload_task(task)?;
    Ok(uuid)
}

fn owned_task(state: &AppState, uuid: &str, user: &AuthAdmin) -> Result<UploadTask, Error> {
    let task = state.find_upload_uuid(uuid)?.ok_or(Error::NotFound)?;
    if task.userid != user.uid {
        return Err(Error::Unauthorized);
    }
    Ok(task)
}

#[post("/upload/<uuid>/<index>", data = "<file>")]
async fn upload_file_slices(
    state: &State<AppState>,
    uuid: &str,
    index: u64,
    mut file: TempFile<'_>,
    user: AuthAdmin,
) -> Result<(), Error> {
    let task = owned_task(state, uuid, &user)?;
    let _lock = task.lock.lock().await;
    if state.find_upload_uuid(uuid)?.is_none() {
        return Err(Error::NotFound);
    }
    let count = task.size / SLICE + u64::from(task.size % SLICE != 0);
    if index == 0 || index > count {
        return Err(Error::BadRequest);
    }
    let expected = (task.size - (index - 1) * SLICE).min(SLICE);
    if file.len() != expected {
        return Err(Error::BadRequest);
    }
    let target = task.workspace.path().join(index.to_string());
    file.copy_to(target).await?;
    task.touched_at
        .store(util::get_utc_seconds(), Ordering::Relaxed);
    Ok(())
}

#[post("/finish-upload/<uuid>")]
async fn finish_upload(state: &State<AppState>, uuid: &str, user: AuthAdmin) -> Result<(), Error> {
    let task = owned_task(state, uuid, &user)?;
    let _lock = task.lock.lock().await;
    if state.find_upload_uuid(uuid)?.is_none() {
        return Err(Error::NotFound);
    }
    let result = finish(&task).await;
    // Removing the final task reference removes all staging files on success or
    // error. A failed upload never starts by deleting the existing destination.
    state.remove_upload_task(task.clone())?;
    result.map_err(Error::from)
}

async fn finish(task: &UploadTask) -> anyhow::Result<()> {
    let assembled = task.workspace.path().join("assembled");
    combine_file_slices(&assembled, task.workspace.path(), task.size).await?;
    let target = storage::join(&task.dir, &task.filename)?;
    task.storage.write(&target, &assembled).await?;
    Ok(())
}

#[post("/cancel-upload", data = "<req_body>")]
async fn cancel_upload(
    state: &State<AppState>,
    req_body: Json<CancelUploadRequest>,
    user: AuthAdmin,
) -> Result<(), Error> {
    for uuid in &req_body.uuids {
        if state.find_upload_uuid(uuid)?.is_none() {
            continue;
        }
        let task = owned_task(state, uuid, &user)?;
        let _lock = task.lock.lock().await;
        state.remove_upload_task(task.clone())?;
    }
    Ok(())
}

async fn combine_file_slices(target: &Path, directory: &Path, size: u64) -> anyhow::Result<()> {
    let mut output = fs::File::create(target).await?;
    let count = size / SLICE + u64::from(size % SLICE != 0);
    let mut written = 0;
    for index in 1..=count {
        let path = directory.join(index.to_string());
        let mut input = fs::File::open(&path).await?;
        let expected = (size - (index - 1) * SLICE).min(SLICE);
        anyhow::ensure!(
            input.metadata().await?.len() == expected,
            "Upload slice size mismatch"
        );
        written += tokio::io::copy(&mut input, &mut output).await?;
        drop(input);
        fs::remove_file(path).await?;
    }
    anyhow::ensure!(written == size, "Upload size mismatch");
    output.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn assembly_detects_missing_slices_and_removes_consumed_slices() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("assembled");
        assert!(combine_file_slices(&output, dir.path(), 1).await.is_err());
        fs::write(dir.path().join("1"), b"a").await.unwrap();
        combine_file_slices(&output, dir.path(), 1).await.unwrap();
        assert!(!dir.path().join("1").exists());
        assert_eq!(fs::read(&output).await.unwrap(), b"a");
        combine_file_slices(&output, dir.path(), 0).await.unwrap();
        assert_eq!(fs::metadata(output).await.unwrap().len(), 0);
    }
}
