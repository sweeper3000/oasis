use crate::util::constants;
use rocket::fairing::{Fairing, Info, Kind};
use rocket::http::uri::Path;
use rocket::http::{Method, Status};
use rocket::{Request, Response};

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

#[cfg(not(debug_assertions))]
fn req_static<'r>(req_path: &Path) -> bool {
    for ext in constants::CACHE_FILE_EXTS.iter() {
        if req_path.ends_with(ext) {
            return true;
        }
    }

    req_path.starts_with("/api/file/") && !req_path.starts_with("/api/file/copy-move-status")
}
