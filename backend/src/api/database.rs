use crate::{
    service::{
        app_state::AppState,
        auth::AuthAdmin,
        backup::{self, JobKind},
    },
    storage::{response::StorageFile, Materialized, Workspace},
    util,
};
use rocket::{
    data::ToByteUnit,
    http::{Cookie, CookieJar, Status},
    response::{status::Custom, Responder},
    serde::{json::Json, Deserialize, Serialize},
    Data, Route, State,
};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
pub struct Failure {
    error: String,
}
type ApiError = Custom<Json<Failure>>;
fn fail(status: Status, message: impl ToString) -> ApiError {
    Custom(
        status,
        Json(Failure {
            error: message.to_string(),
        }),
    )
}
fn invalid(error: anyhow::Error) -> ApiError {
    fail(Status::BadRequest, error)
}
fn internal(_: impl std::fmt::Display) -> ApiError {
    fail(
        Status::InternalServerError,
        "Database operation failed. Check the server status before retrying",
    )
}
async fn password(state: &AppState, uid: i64, value: &str) -> Result<(), ApiError> {
    backup::verify_password(state, uid, value)
        .await
        .map_err(|_| fail(Status::Forbidden, "Password verification failed"))
}
fn operation(state: &AppState) -> Result<tokio::sync::OwnedMutexGuard<()>, ApiError> {
    state
        .backups
        .expensive
        .clone()
        .try_lock_owned()
        .map_err(|_| fail(Status::Conflict, "Another database operation is running"))
}

#[derive(Deserialize)]
#[serde(crate = "rocket::serde", deny_unknown_fields)]
pub struct Password {
    password: String,
}
#[derive(Deserialize)]
#[serde(crate = "rocket::serde", deny_unknown_fields)]
pub struct ExportConfirm {
    password: String,
    answers: Vec<String>,
}
#[derive(Deserialize)]
#[serde(crate = "rocket::serde", deny_unknown_fields)]
pub struct ImportPrepare {
    password: String,
    phrase: String,
}
#[derive(Deserialize)]
#[serde(crate = "rocket::serde", deny_unknown_fields)]
pub struct ImportConfirm {
    password: String,
    local_storage: String,
    confirm_replace: bool,
}
#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
pub struct ExportPrepared {
    id: String,
    phrase: String,
    positions: [usize; 3],
}
#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
pub struct ImportPrepared {
    id: String,
}
#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
pub struct Preview {
    id: String,
    site_name: String,
    accounts: usize,
    providers: usize,
    visibility_rules: usize,
    source_storage: String,
    local_storage: String,
}

// The existing response owns its workspace until download completion/disconnect.
pub struct Download(StorageFile);
impl<'r> Responder<'r, 'static> for Download {
    fn respond_to(self, request: &'r rocket::Request<'_>) -> rocket::response::Result<'static> {
        let mut response = self.0.respond_to(request)?;
        response.set_raw_header(
            "Content-Disposition",
            "attachment; filename=\"oasis-database.oasis-backup\"",
        );
        response.set_raw_header("Content-Type", "application/octet-stream");
        response.set_raw_header("Cache-Control", "no-store");
        Ok(response)
    }
}

pub fn route() -> Vec<Route> {
    routes![
        prepare_export,
        download_export,
        prepare_import,
        upload_import,
        restore_import,
        cancel
    ]
}

#[post("/sys/database/export/prepare", data = "<body>")]
async fn prepare_export(
    state: &State<AppState>,
    admin: AuthAdmin,
    body: Json<Password>,
) -> Result<Json<ExportPrepared>, ApiError> {
    password(state, admin.uid, &body.password).await?;
    let (phrase, positions) = backup::recovery_phrase().map_err(internal)?;
    let visible_phrase = phrase.to_string();
    let id = state
        .backups
        .insert(
            admin.uid,
            JobKind::Export {
                phrase,
                positions,
                attempts: 0,
            },
        )
        .map_err(invalid)?;
    Ok(Json(ExportPrepared {
        id,
        phrase: visible_phrase,
        positions: positions.map(|i| i + 1),
    }))
}

#[post("/sys/database/export/<id>/download", data = "<body>")]
async fn download_export(
    id: &str,
    state: &State<AppState>,
    admin: AuthAdmin,
    body: Json<ExportConfirm>,
) -> Result<Download, ApiError> {
    let _operation = operation(state)?;
    password(state, admin.uid, &body.password).await?;
    let phrase = state
        .backups
        .confirm_export(id, admin.uid, &body.answers)
        .map_err(invalid)?;
    let workspace = Workspace::new().map_err(internal)?;
    let snapshot = backup::snapshot(&state.pool).await.map_err(internal)?;
    let payload = Zeroizing::new(serde_json::to_vec(&snapshot).map_err(internal)?);
    // Leave room for the authenticated framing and outer header.
    if payload.len() as u64 > backup::MAX_BACKUP - 65536 {
        return Err(fail(
            Status::PayloadTooLarge,
            "Database backup exceeds 64 MiB",
        ));
    }
    tokio::fs::write(workspace.path().join("payload"), &*payload)
        .await
        .map_err(internal)?;
    drop(payload);
    backup::encrypt(phrase, &workspace)
        .await
        .map_err(internal)?;
    Ok(Download(StorageFile::new(
        Materialized {
            path: workspace.path().join("archive"),
            workspace: Some(workspace),
        },
        "oasis-database.oasis-backup".into(),
        None,
    )))
}

#[post("/sys/database/import/prepare", data = "<body>")]
async fn prepare_import(
    state: &State<AppState>,
    admin: AuthAdmin,
    body: Json<ImportPrepare>,
) -> Result<Json<ImportPrepared>, ApiError> {
    password(state, admin.uid, &body.password).await?;
    let phrase = backup::normalize_phrase(&body.phrase).map_err(invalid)?;
    let workspace = Arc::new(Workspace::new().map_err(internal)?);
    let id = state
        .backups
        .insert(admin.uid, JobKind::Import { phrase, workspace })
        .map_err(invalid)?;
    Ok(Json(ImportPrepared { id }))
}

#[put("/sys/database/import/<id>", data = "<data>")]
async fn upload_import(
    id: &str,
    state: &State<AppState>,
    admin: AuthAdmin,
    data: Data<'_>,
) -> Result<Json<Preview>, ApiError> {
    let _operation = operation(state)?;
    let job = state.backups.start_import(id, admin.uid).map_err(invalid)?;
    let expires = job.expires();
    let result = async {
        let (phrase, workspace) = match job.kind {
            JobKind::Import { phrase, workspace } => (phrase, workspace),
            _ => return Err(fail(Status::BadRequest, "Not an import operation")),
        };
        let archive = workspace.path().join("archive");
        let mut output = tokio::fs::File::create(&archive).await.map_err(internal)?;
        let count = data
            .open((backup::MAX_BACKUP + 1).bytes())
            .stream_to(&mut output)
            .await
            .map_err(internal)?;
        if !count.complete || count.written > backup::MAX_BACKUP {
            return Err(fail(
                Status::PayloadTooLarge,
                "Database backup exceeds 64 MiB",
            ));
        }
        output.flush().await.map_err(internal)?;
        drop(output);
        backup::decrypt(phrase, &workspace).await.map_err(invalid)?;
        let snapshot = backup::read_snapshot(&workspace).await.map_err(invalid)?;
        let preview = Preview {
            id: id.into(),
            site_name: snapshot.site.name,
            accounts: snapshot.users.len(),
            providers: snapshot.storage.providers.len(),
            visibility_rules: snapshot.hidden.len(),
            source_storage: snapshot.site.storage,
            local_storage: state.get_site().map_err(internal)?.storage.clone(),
        };
        state
            .backups
            .ready(id, admin.uid, workspace, expires)
            .map_err(invalid)?;
        Ok(Json(preview))
    }
    .await;
    if result.is_err() {
        state.backups.cancel(id, admin.uid);
    }
    result
}

#[post("/sys/database/import/<id>/restore", data = "<body>")]
async fn restore_import(
    id: &str,
    state: &State<AppState>,
    admin: AuthAdmin,
    body: Json<ImportConfirm>,
    jar: &CookieJar<'_>,
) -> Result<(), ApiError> {
    if !body.confirm_replace {
        return Err(fail(
            Status::BadRequest,
            "Confirm replacement of the current installation",
        ));
    }
    state
        .backups
        .ready_workspace(id, admin.uid)
        .map_err(invalid)?;
    let permit = operation(state)?;
    password(state, admin.uid, &body.password).await?;
    let access_token = jar
        .get(util::constants::ACCESS_TOKEN)
        .ok_or_else(|| fail(Status::Unauthorized, "Sign in again"))?
        .value()
        .to_owned();
    let state = state.inner().clone();
    let id = id.to_owned();
    let body = body.into_inner();
    // Detach on HTTP disconnect: once commit begins, database and caches must
    // finish switching together even if the browser leaves.
    tokio::spawn(async move {
        let _permit = permit;
        backup::restore(
            state,
            admin.uid,
            id,
            Zeroizing::new(body.password),
            body.local_storage,
            access_token,
        )
        .await
    })
    .await
    .map_err(internal)?
    .map_err(invalid)?;
    jar.remove(
        Cookie::build(util::constants::ACCESS_TOKEN, "")
            .path("/")
            .finish(),
    );
    jar.remove(
        Cookie::build(util::constants::REFRESH_TOKEN, "")
            .path("/api/user")
            .finish(),
    );
    Ok(())
}

#[delete("/sys/database/jobs/<id>")]
async fn cancel(id: &str, state: &State<AppState>, admin: AuthAdmin) {
    state.backups.cancel(id, admin.uid);
}
