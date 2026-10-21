use crate::util::constants;
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::uri::Path;
use rocket::http::{Method, Status};
use rocket::{Data, Request, Response};

// Request handlers keep a shared permit until their response is ready. Restore
// takes the exclusive permit, so auth and database writes cannot straddle it.
pub struct DatabaseGate;
struct RequestPermit(std::sync::Mutex<Option<tokio::sync::OwnedRwLockReadGuard<()>>>);

#[rocket::async_trait]
impl Fairing for DatabaseGate {
    fn info(&self) -> Info {
        Info {
            name: "Database restore gate",
            kind: Kind::Request | Kind::Response,
        }
    }

    async fn on_request(&self, request: &mut Request<'_>, _: &mut Data<'_>) {
        // Decode individual segments so an encoded slash inside a job id cannot
        // make the restore handler acquire a shared permit and deadlock itself.
        let path = request.uri().path();
        let parts: Vec<_> = path.as_str().trim_matches('/').split('/').collect();
        let restoring = request.method() == Method::Post
            && parts.len() == 6
            && [0, 1, 2, 3, 5]
                .iter()
                .zip(["api", "sys", "database", "import", "restore"])
                .all(|(i, expected)| {
                    urlencoding::decode(parts[*i])
                        .map(|s| s == expected)
                        .unwrap_or(false)
                });
        if !restoring {
            if let Some(state) = request
                .rocket()
                .state::<crate::service::app_state::AppState>()
            {
                let permit = state.operations.clone().read_owned().await;
                request.local_cache(|| RequestPermit(std::sync::Mutex::new(Some(permit))));
            }
        }
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, _: &mut Response<'r>) {
        let permit = request.local_cache(|| RequestPermit(std::sync::Mutex::new(None)));
        permit.0.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

pub struct StaticFileCache;

#[rocket::async_trait]
impl Fairing for StaticFileCache {
    fn info(&self) -> Info {
        Info {
            name: "Static file cache",
            kind: Kind::Response,
        }
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        let req_path = request.uri().path();
        if urlencoding::decode(req_path.as_str())
            .map(|s| s.starts_with("/api/sys/database/"))
            .unwrap_or(false)
        {
            response.set_raw_header("Cache-Control", "no-store");
            response.set_raw_header("Pragma", "no-cache");
            return;
        }
        let external_enabled = request
            .rocket()
            .state::<crate::service::app_state::AppState>()
            .and_then(|state| state.storage.lock().ok().map(|config| config.enabled))
            .unwrap_or(false);
        if external_enabled && req_path.starts_with("/api/file/") {
            // Provider-side edits and visibility changes must be seen on the next
            // request, including previews, shared links and partial responses.
            response.set_raw_header("Cache-Control", "private, no-store");
            return;
        }
        if response.status() != Status::Ok {
            return;
        }

        if request.method() == Method::Get && req_static(&req_path) {
            let content = format!("private, max-age={}", constants::CACHE_MAX_AGE);
            response.set_raw_header("Cache-Control", content);
            response.set_raw_header("Accept-Ranges", "bytes");
        }
    }
}

// Do not cache development related files in debug mode.
#[cfg(debug_assertions)]
fn req_static<'r>(req_path: &Path) -> bool {
    req_path.starts_with("/api/file/") && !req_path.starts_with("/api/file/copy-move-status")
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use crate::service::app_state::AppState;
    use rocket::{local::asynchronous::Client, State};

    #[post("/api/sys/database/import/<_id>/restore")]
    async fn cutover(_id: &str, state: &State<AppState>) {
        let _exclusive = state.operations.write().await;
    }

    #[rocket::async_test]
    async fn restore_routes_do_not_deadlock_on_encoded_job_ids() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_lazy("sqlite::memory:")
            .unwrap();
        let client = Client::tracked(
            rocket::build()
                .manage(AppState::new(None, pool))
                .attach(DatabaseGate)
                .attach(StaticFileCache)
                .mount("/", routes![cutover]),
        )
        .await
        .unwrap();
        for path in [
            "/api/sys/database/import/job/restore",
            "/api/sys/database/import/job%2Fid/restore",
        ] {
            let response = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                client.post(path).dispatch(),
            )
            .await
            .expect("Restore handler deadlocked on its request permit");
            assert_eq!(response.status(), Status::Ok);
            assert_eq!(
                response.headers().get_one("Cache-Control"),
                Some("no-store")
            );
        }
    }
}

#[cfg(not(debug_assertions))]
fn req_static<'r>(req_path: &Path) -> bool {
    for ext in constants::CACHE_FILE_EXTS.iter() {
        if req_path.ends_with(ext) {
            return true;
        }
    }

    req_path.starts_with("/api/file/") && !req_path.starts_with("/api/file/copy-move-status")
}
