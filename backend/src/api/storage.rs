use crate::{
    entity::error::Error,
    service::{app_state::AppState, auth::AuthAdmin},
    storage::config::StorageConfig,
};
use rocket::{
    http::Status,
    response::status::Custom,
    serde::{json::Json, Serialize},
    Route, State,
};

#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
pub struct StorageError {
    error: String,
}
type ConfigError = Custom<Json<StorageError>>;
fn invalid(error: anyhow::Error) -> ConfigError {
    Custom(
        Status::BadRequest,
        Json(StorageError {
            error: error.to_string(),
        }),
    )
}

pub fn route() -> Vec<Route> {
    routes![get_storage, update_storage, test_storage]
}

#[get("/sys/storage")]
async fn get_storage(
    state: &State<AppState>,
    _admin: AuthAdmin,
) -> Result<Json<StorageConfig>, Error> {
    Ok(Json(state.get_storage_config()?.redacted()))
}

#[put("/sys/storage", data = "<body>")]
async fn update_storage(
    state: &State<AppState>,
    _admin: AuthAdmin,
    body: Json<StorageConfig>,
) -> Result<Json<StorageConfig>, ConfigError> {
    let _lock = state.storage_settings_lock.lock().await;
    let previous = state.get_storage_config().map_err(invalid)?;
    let mut config = body.into_inner();
    config.merge_secrets(&previous);
    config
        .validate(&state.get_site().map_err(invalid)?.storage)
        .map_err(invalid)?;
    if !state.get_upload_tasks().map_err(invalid)?.is_empty()
        || !crate::entity::copy_move_task::CopyMoveTask::allow_new_task()
    {
        return Err(invalid(anyhow::anyhow!(
            "Wait for uploads and copy/move operations to finish before changing storage"
        )));
    }
    config.prepare_keys(true).await.map_err(invalid)?;
    config
        .validate(&state.get_site().map_err(invalid)?.storage)
        .map_err(invalid)?;
    config.check_connections(false).await.map_err(invalid)?;
    config.check_connections(true).await.map_err(invalid)?;
    config
        .save(&state.pool, &state.get_secret().map_err(invalid)?)
        .await
        .map_err(invalid)?;
    let redacted = config.redacted();
    state.set_storage_config(config).map_err(invalid)?;
    Ok(Json(redacted))
}

#[post("/sys/storage/test", data = "<body>")]
async fn test_storage(
    state: &State<AppState>,
    _admin: AuthAdmin,
    body: Json<StorageConfig>,
) -> Result<(), ConfigError> {
    let _lock = state.storage_settings_lock.lock().await;
    let mut config = body.into_inner();
    config.merge_secrets(&state.get_storage_config().map_err(invalid)?);
    config.enabled = true;
    config
        .validate(&state.get_site().map_err(invalid)?.storage)
        .map_err(invalid)?;
    config.prepare_keys(true).await.map_err(invalid)?;
    config
        .validate(&state.get_site().map_err(invalid)?.storage)
        .map_err(invalid)?;
    config.check_connections(false).await.map_err(invalid)
}
