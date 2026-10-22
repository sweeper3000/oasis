use crate::{
    entity::{
        copy_move_task::{CopyMoveFileRequest, CopyMoveTask, CopyMoveTaskStatus},
        error::Error,
        file::{File, FileType},
        hidden::Hidden,
        request::{
            CreateDirRequest, GenerateLinkRequest, RenameFileRequest, SetFileVisibilityRequest,
        },
        response::FileResponse,
    },
    service::{
        app_state::AppState,
        auth::{AuthAdmin, AuthUser},
        range::Range,
        track,
    },
    storage::{self, response::StorageFile, Storage},
    util,
};
use anyhow::{anyhow, Result as AnyResult};
use async_zip::{write::ZipFileWriter, Compression, ZipEntryBuilder};
use rocket::{response::stream::ByteStream, serde::json::Json, Route, State};
use sqlx::Acquire;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWrite};

pub fn route() -> Vec<Route> {
    routes![
        dir_content,
        create_dir,
        update_file_name,
        delete_file,
        file_content,
        file_head,
        video_track,
        generate_share_link,
        get_share_link,
        share_head,
        search_files,
        update_file_visibility,
        copy_move_file,
        get_copy_move_status,
        download_dir
    ]
}

pub fn encoded_path(path: &str) -> Result<String, Error> {
    let decoded = urlencoding::decode(path).map_err(|_| Error::BadRequest)?;
    virtual_path(&decoded)
}
fn virtual_path(path: &str) -> Result<String, Error> {
    storage::normalize(path).map_err(|_| Error::BadRequest)
}

fn permission(path: &str, hiddens: &[Hidden]) -> i8 {
    hiddens
        .iter()
        .filter(|hidden| path == hidden.path || path.starts_with(&format!("{}/", hidden.path)))
        .map(|hidden| hidden.least_permission)
        .max()
        .unwrap_or(0)
}
async fn authorize(state: &AppState, path: &str, user: &AuthUser) -> Result<(), Error> {
    let mut conn = state.get_pool_conn().await?;
    if permission(path, &Hidden::find_all(&mut conn).await?) > user.permission {
        return Err(Error::Unauthorized);
    }
    Ok(())
}

#[get("/dir?<path>")]
async fn dir_content(
    path: Option<&str>,
    user: AuthUser,
    state: &State<AppState>,
) -> Result<Json<Vec<File>>, Error> {
    let path = virtual_path(path.unwrap_or(""))?;
    let storage = Storage::from_state(state)?;
    let mut conn = state.get_pool_conn().await?;
    let hiddens = Hidden::find_all(&mut conn).await?;
    if permission(&path, &hiddens) > user.permission {
        return Err(Error::Unauthorized);
    }
    let content = storage
        .list(&path)
        .await?
        .into_iter()
        .filter_map(|entry| {
            let required = permission(&entry.path, &hiddens);
            if required <= user.permission {
                Some(entry.file(false, required))
            } else {
                None
            }
        })
        .collect();
    Ok(Json(content))
}

#[post("/dir", data = "<req_body>")]
async fn create_dir(
    state: &State<AppState>,
    req_body: Json<CreateDirRequest>,
    _user: AuthAdmin,
) -> Result<(), Error> {
    let parent = encoded_path(&req_body.parent)?;
    let target = storage::join(&parent, &req_body.name).map_err(|_| Error::BadRequest)?;
    Ok(Storage::from_state(state)?.mkdir(&target).await?)
}

async fn serve_file(
    storage: Storage,
    path: &str,
    range: Range,
    text_preview: bool,
    head: bool,
) -> Result<FileResponse, Error> {
    let kind = FileType::infer_file_type(
        Path::new(path)
            .extension()
            .and_then(|p| p.to_str())
            .unwrap_or(""),
    );
    if kind == FileType::Video {
        if let Some(response) = storage.remote_video(path, range.range, head).await? {
            return Ok(FileResponse::Video(response));
        }
    }
    if head {
        let metadata = storage.stat(path).await?.ok_or(Error::NotFound)?;
        if metadata.is_dir {
            return Err(Error::BadRequest);
        }
        return Ok(FileResponse::Video(
            storage::stream::VideoResponse::metadata(metadata.size, path, range.range),
        ));
    }
    let file = storage.cached_file(path).await?;
    if range.range.is_none() && text_preview && kind == FileType::Text {
        let text = util::file_system::read_text_file(file.path.clone()).await?;
        return Ok(FileResponse::Text(text));
    }
    Ok(FileResponse::Storage(StorageFile::new(
        file,
        Path::new(path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into(),
        range.range,
    )))
}

#[get("/file/<path>")]
async fn file_content(
    path: &str,
    user: AuthUser,
    state: &State<AppState>,
    range_header: Range,
) -> Result<FileResponse, Error> {
    let path = virtual_path(path)?;
    authorize(state, &path, &user).await?;
    serve_file(
        Storage::from_state(state)?,
        &path,
        range_header,
        true,
        false,
    )
    .await
}

#[head("/file/<path>")]
async fn file_head(
    path: &str,
    user: AuthUser,
    state: &State<AppState>,
    range_header: Range,
) -> Result<FileResponse, Error> {
    let path = virtual_path(path)?;
    authorize(state, &path, &user).await?;
    serve_file(Storage::from_state(state)?, &path, range_header, true, true).await
}

#[put("/file/<path>/name", data = "<req_body>")]
async fn update_file_name(
    state: &State<AppState>,
    path: &str,
    req_body: Json<RenameFileRequest>,
    _user: AuthAdmin,
) -> Result<(), Error> {
    let path = virtual_path(path)?;
    let parent = Path::new(&path)
        .parent()
        .unwrap_or(Path::new(""))
        .to_string_lossy();
    let target = storage::join(&parent, &req_body.new_name).map_err(|_| Error::BadRequest)?;
    let storage = Storage::from_state(state)?;
    storage
        .transfer_with_hook(&path, &target, false, false, || {
            sync_visibility(&state.pool, &path, &target)
        })
        .await?;
    remove_source_visibility(&state.pool, &path).await?;
    Ok(())
}

#[put("/file/<path>/visibility", data = "<req_body>")]
async fn update_file_visibility(
    state: &State<AppState>,
    path: &str,
    req_body: Json<SetFileVisibilityRequest>,
    _user: AuthAdmin,
) -> Result<(), Error> {
    let path = virtual_path(path)?;
    if Storage::from_state(state)?.stat(&path).await?.is_none() {
        return Err(Error::NotFound);
    }
    let mut conn = state.get_pool_conn().await?;
    let mut tx = conn.begin().await?;
    Hidden::delete_query(&path, &mut tx).await?;
    if !req_body.visible {
        Hidden::new(&path, 1).insert_query(&mut tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

#[delete("/file/<path>")]
async fn delete_file(state: &State<AppState>, path: &str, _user: AuthAdmin) -> Result<(), Error> {
    let path = virtual_path(path)?;
    Storage::from_state(state)?.remove(&path).await?;
    let mut conn = state.get_pool_conn().await?;
    let mut tx = conn.begin().await?;
    // instr uses a literal prefix; '%' and '_' in filenames are not wildcards.
    sqlx::query("DELETE FROM hidden WHERE path = ? OR instr(path, ?) = 1")
        .bind(&path)
        .bind(format!("{path}/"))
        .execute(&mut tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[get("/file/track/<path>")]
async fn video_track(path: &str, user: AuthUser, state: &State<AppState>) -> Result<String, Error> {
    let path = virtual_path(path)?;
    let storage = Storage::from_state(state)?;
    let mut target = path.clone();
    if storage.stat(&target).await?.is_none() {
        target = Path::new(&path)
            .with_extension("srt")
            .to_string_lossy()
            .into();
    }
    authorize(state, &target, &user).await?;
    let meta = storage.stat(&target).await?.ok_or(Error::NotFound)?;
    if meta.is_dir || meta.size > 1024 * 1024 {
        return Err(Error::BadRequest);
    }
    let file = storage.cached_file(&target).await?;
    let text = util::file_system::read_text_file(file.path.clone()).await?;
    if target.ends_with(".srt") {
        Ok(track::srt_to_vtt(&text).await?)
    } else {
        Ok(text)
    }
}

#[post("/file/share", data = "<req_body>")]
async fn generate_share_link(
    state: &State<AppState>,
    req_body: Json<GenerateLinkRequest>,
    user: AuthUser,
) -> Result<String, Error> {
    let path = encoded_path(&req_body.path)?;
    authorize(state, &path, &user).await?;
    if req_body.expire <= util::get_utc_seconds()
        || !Storage::from_state(state)?
            .stat(&path)
            .await?
            .map(|m| !m.is_dir)
            .unwrap_or(false)
    {
        return Err(Error::BadRequest);
    }
    let path = urlencoding::encode(&path);
    let input = format!("expire={}&path={}", req_body.expire, path);
    let hash = util::sha256(&input, &state.get_secret()?);
    Ok(format!("hash={}&{}", hash, input))
}

#[get("/file/share?<path>&<expire>&<hash>")]
async fn get_share_link(
    state: &State<AppState>,
    path: &str,
    expire: i64,
    hash: &str,
    range_header: Range,
) -> Result<FileResponse, Error> {
    serve_share(state, path, expire, hash, range_header, false).await
}

#[head("/file/share?<path>&<expire>&<hash>")]
async fn share_head(
    state: &State<AppState>,
    path: &str,
    expire: i64,
    hash: &str,
    range_header: Range,
) -> Result<FileResponse, Error> {
    serve_share(state, path, expire, hash, range_header, true).await
}

async fn serve_share(
    state: &State<AppState>,
    path: &str,
    expire: i64,
    hash: &str,
    range_header: Range,
    head: bool,
) -> Result<FileResponse, Error> {
    let path = virtual_path(path)?;
    if expire <= util::get_utc_seconds() {
        return Err(Error::Forbidden);
    }
    let input = format!("expire={}&path={}", expire, urlencoding::encode(&path));
    if hash != util::sha256(&input, &state.get_secret()?) {
        return Err(Error::Forbidden);
    }
    serve_file(
        Storage::from_state(state)?,
        &path,
        range_header,
        false,
        head,
    )
    .await
}

#[get("/file/search?<keywords>")]
async fn search_files(
    state: &State<AppState>,
    keywords: &str,
    user: AuthUser,
) -> Result<Json<Vec<File>>, Error> {
    let storage = Storage::from_state(state)?;
    let mut conn = state.get_pool_conn().await?;
    let hiddens = Hidden::find_all(&mut conn).await?;
    let keywords: Vec<String> = keywords.split('+').map(str::to_lowercase).collect();
    let mut directories = vec![String::new()];
    let mut files = vec![];
    while let Some(directory) = directories.pop() {
        for entry in storage.list(&directory).await? {
            let required = permission(&entry.path, &hiddens);
            if required > user.permission {
                continue;
            }
            if entry.is_dir {
                directories.push(entry.path.clone());
            }
            let name = Path::new(&entry.path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            if keywords.iter().all(|word| {
                name.contains(word)
                    && (!word.starts_with('.')
                        || (!entry.is_dir
                            && name
                                .rsplit_once('.')
                                .map(|(_, ext)| format!(".{ext}") == *word)
                                .unwrap_or(false)))
            }) {
                files.push(entry.file(true, required));
            }
        }
    }
    Ok(Json(files))
}

#[post("/file/copy-move", data = "<req_body>")]
async fn copy_move_file(
    state: &State<AppState>,
    admin: AuthAdmin,
    req_body: Json<CopyMoveFileRequest>,
) -> Result<String, Error> {
    let _settings_lock = state.storage_settings_lock.lock().await;
    let source = encoded_path(&req_body.source)?;
    let target = encoded_path(&req_body.target)?;
    let storage = Storage::from_state(state)?;
    if !storage
        .stat(&target)
        .await?
        .map(|m| m.is_dir)
        .unwrap_or(false)
    {
        return Err(Error::BadRequest);
    }
    let name = Path::new(&source)
        .file_name()
        .ok_or(Error::BadRequest)?
        .to_string_lossy();
    let destination = storage::join(&target, &name).map_err(|_| Error::BadRequest)?;
    let task = CopyMoveTask::new(
        source.clone().into(),
        destination.clone().into(),
        admin.uid,
        req_body.is_copy,
        req_body.overwrite,
    );
    {
        let mut current = crate::COPY_MOVE_TASK
            .lock()
            .map_err(|_| Error::InternalServerError)?;
        if current
            .as_ref()
            .map(|t| {
                t.status == CopyMoveTaskStatus::Pending
                    || t.status == CopyMoveTaskStatus::InProgress
            })
            .unwrap_or(false)
        {
            return Err(Error::Conflict);
        }
        *current = Some(task.clone());
    }
    let uuid = task.uuid.clone();
    let pool = state.pool.clone();
    tokio::spawn(async move {
        task.update_progress(0.0, CopyMoveTaskStatus::InProgress);
        let observed = task.clone();
        let progress = storage::progress::Progress::new(move |fraction, phase, file| {
            observed.report(fraction, phase, file)
        });
        let result: AnyResult<()> = async {
            storage
                .transfer_with_progress(
                    &source,
                    &destination,
                    task.is_copy,
                    task.overwrite,
                    || sync_visibility(&pool, &source, &destination),
                    Some(&progress),
                )
                .await?;
            if !task.is_copy {
                remove_source_visibility(&pool, &source).await?;
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => task.update_progress(1.0, CopyMoveTaskStatus::Success),
            Err(_) => {
                eprintln!(
                    "Storage copy/move failed; source is retained unless the transfer completed"
                );
                task.update_progress(0.0, CopyMoveTaskStatus::Failed);
            }
        }
    });
    Ok(uuid)
}

async fn sync_visibility(
    pool: &sqlx::Pool<sqlx::Sqlite>,
    source: &str,
    destination: &str,
) -> AnyResult<()> {
    let mut conn = pool.acquire().await?;
    let hidden = Hidden::find_all(&mut conn).await?;
    let inherited = permission(source, &hidden);
    let mut tx = conn.begin().await?;
    if inherited > 0 {
        Hidden::new(destination, inherited)
            .insert_query(&mut tx)
            .await?;
    }
    for hidden in hidden
        .iter()
        .filter(|h| h.path == source || h.path.starts_with(&format!("{source}/")))
    {
        let new_path = format!("{}{}", destination, &hidden.path[source.len()..]);
        Hidden::new(&new_path, hidden.least_permission)
            .insert_query(&mut tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn remove_source_visibility(pool: &sqlx::Pool<sqlx::Sqlite>, source: &str) -> AnyResult<()> {
    sqlx::query("DELETE FROM hidden WHERE path = ? OR instr(path, ?) = 1")
        .bind(source)
        .bind(format!("{source}/"))
        .execute(pool)
        .await?;
    Ok(())
}

#[get("/file/copy-move-status/<uuid>")]
async fn get_copy_move_status(uuid: String, admin: AuthAdmin) -> Result<Json<CopyMoveTask>, Error> {
    let mut task = CopyMoveTask::get_static_value()
        .filter(|t| t.uuid == uuid)
        .ok_or(Error::NotFound)?;
    if task.user_id != admin.uid {
        return Err(Error::Unauthorized);
    }
    task.user_id = 0;
    Ok(Json(task))
}

#[get("/download/dir?<path>")]
async fn download_dir(
    path: &str,
    _user: AuthAdmin,
    state: &State<AppState>,
) -> Result<ByteStream![Vec<u8>], Error> {
    let path = virtual_path(path)?;
    let storage = Storage::from_state(state)?;
    if !storage
        .stat(&path)
        .await?
        .map(|m| m.is_dir)
        .unwrap_or(false)
    {
        return Err(Error::BadRequest);
    }
    let (mut writer, mut reader) = tokio::io::duplex(util::constants::ZIP_BUFFER_SIZE);
    let task = tokio::spawn(async move {
        if zip_dir(&mut writer, storage, &path).await.is_err() {
            eprintln!("Directory download failed or was disconnected");
        }
    });
    struct AbortOnDrop(tokio::task::JoinHandle<()>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let cleanup = AbortOnDrop(task);
    Ok(ByteStream! {
        let _cleanup = cleanup;
        loop {
            let mut buffer = vec![0; util::constants::ZIP_BUFFER_SIZE];
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(size) => { buffer.truncate(size); yield buffer; }
            }
        }
    })
}

async fn zip_dir<W: AsyncWrite + Unpin>(
    writer: &mut W,
    storage: Storage,
    path: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut zip = ZipFileWriter::new(writer);
    let prefix = Path::new(path)
        .parent()
        .unwrap_or(Path::new(""))
        .to_string_lossy()
        .to_string();
    let mut pending = vec![path.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in storage.list(&directory).await? {
            let filename = if prefix.is_empty() {
                entry.path.clone()
            } else {
                entry
                    .path
                    .strip_prefix(&format!("{prefix}/"))
                    .ok_or_else(|| anyhow!("Invalid archive path"))?
                    .to_owned()
            };
            if entry.is_dir {
                let directory_entry =
                    ZipEntryBuilder::new(format!("{filename}/"), Compression::Stored)
                        .unix_permissions(0o755);
                zip.write_entry_whole(directory_entry, &[]).await?;
                pending.push(entry.path);
            } else {
                let file = storage.materialize(&entry.path).await?;
                let mut source = tokio::fs::File::open(&file.path).await?;
                let mut sink = zip
                    .write_entry_stream(
                        ZipEntryBuilder::new(filename, Compression::Stored).unix_permissions(0o644),
                    )
                    .await?;
                tokio::io::copy(&mut source, &mut sink).await?;
                sink.close().await?;
            }
        }
    }
    zip.close().await?;
    Ok(())
}
